//! The WebSocket endpoint: one task per connection, dispatching client commands.
//!
//! A connection owns a bounded outbound queue drained by a dedicated writer
//! task, so a slow client applies backpressure to itself rather than to the
//! waves it is watching.

use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use futures::{FutureExt, SinkExt, StreamExt};
use gal_core::model::*;
use gal_core::protocol::*;
use gal_ot::{Delta, Insert, OpKind};
use std::sync::Arc as StdArc;
use tokio::sync::{mpsc, Mutex, Notify};

use crate::auth::Identity;
use crate::state::{blip_view, Action, AppState, ConnId, LiveWave, OpSubmission, Subscriber};

/// Outbound queue depth per connection.
const OUTBOUND_CAPACITY: usize = 512;

/// Largest accepted incoming frame. Generous for an op, far below anything that
/// could be used to exhaust server memory.
const MAX_FRAME_BYTES: usize = 1 << 20;

use crate::state::{MAX_BLIP_RUNS, MAX_BLIP_UNITS};

pub async fn handler(
    upgrade: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    identity: Identity,
) -> Response {
    // Reject cross-origin upgrades. Cookies are attached automatically by the
    // browser, so without this the only thing stopping a hostile page from
    // opening an authenticated socket — and reading the victim's entire inbox —
    // is the cookie's SameSite attribute, which is a browser default rather than
    // something this server controls, and which is same-site, not same-origin.
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if !crate::auth::origin_allowed(
        header("origin"),
        header("host"),
        &state.config.allowed_origins,
    ) {
        tracing::warn!(origin = ?header("origin"), "rejected a cross-origin WebSocket upgrade");
        return (
            axum::http::StatusCode::FORBIDDEN,
            "This origin is not allowed to connect.",
        )
            .into_response();
    }

    upgrade
        .max_message_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| connection(socket, state, identity.user))
}

/// Per-connection state that only the reader task touches.
struct Session {
    conn_id: ConnId,
    user: User,
    /// Waves this connection is currently watching.
    subscribed: HashSet<WaveId>,
    tx: mpsc::Sender<ServerMessage>,
    /// Fired by the server when this connection must be torn down — currently
    /// only when its outbound queue overflowed, meaning it has missed messages
    /// and must resynchronise from scratch.
    kill: StdArc<Notify>,
}

async fn connection(socket: WebSocket, state: Arc<AppState>, user: User) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::channel::<ServerMessage>(OUTBOUND_CAPACITY);
    let conn_id = state.register_conn(user.id.clone(), tx.clone());

    // Writer task: the only place that touches the socket's send half.
    let writer = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            let Ok(json) = serde_json::to_string(&message) else {
                continue;
            };
            if sink.send(Message::Text(json.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let kill = StdArc::new(Notify::new());
    let mut session = Session {
        conn_id,
        user: user.clone(),
        subscribed: HashSet::new(),
        tx,
        kill: kill.clone(),
    };

    // Greet with the user's identity and inbox.
    //
    // An empty inbox on a database error reads to the user as "my conversations
    // are gone" — the exact failure `migrate` refuses to start rather than
    // cause. Say the inbox could not be loaded instead, and let them retry.
    let inbox = match state.db.inbox(&user.id).await {
        Ok(inbox) => inbox,
        Err(err) => {
            tracing::error!(user = %user.id, error = %err, "loading inbox for welcome");
            let _ = session.tx.try_send(ServerMessage::error(
                ErrorCode::Internal,
                "could not load your inbox; reconnect to try again",
            ));
            return;
        }
    };
    let _ = session.tx.try_send(ServerMessage::Welcome {
        user: user.public(),
        inbox,
    });

    loop {
        let incoming = tokio::select! {
            biased;
            // A client that overflowed its queue has already missed messages;
            // closing is what makes it reconnect and resynchronise.
            _ = kill.notified() => break,
            frame = stream.next() => frame,
        };
        let Some(Ok(message)) = incoming else { break };

        let text = match message {
            Message::Text(text) => text,
            Message::Close(_) => break,
            // Ping/Pong are handled by axum; binary frames are not part of the
            // protocol.
            _ => continue,
        };

        let parsed: Result<ClientMessage, _> = serde_json::from_str(&text);
        match parsed {
            Ok(command) => {
                let name = command.name();
                // A panic in here is a bug, and it used to take the whole
                // process with it — `panic = "abort"` made one malformed edit
                // in one wave an outage for every other connection on the box,
                // which is also why the JoinError arms around the blocking pool
                // were unreachable. Contain it to this connection instead.
                let outcome = std::panic::AssertUnwindSafe(dispatch(&state, &mut session, command))
                    .catch_unwind()
                    .await;
                match outcome {
                    Ok(Ok(())) => {}
                    Ok(Err(reply)) => {
                        let _ = session.tx.try_send(reply);
                    }
                    Err(payload) => {
                        tracing::error!(
                            command = name,
                            user = %session.user.id,
                            conn = session.conn_id,
                            panic = panic_message(&payload),
                            "a command panicked; dropping the connection and its waves"
                        );
                        // The panic may have landed between mutating a resident
                        // document and persisting the op, so the memory cannot
                        // be trusted. Throw those waves away and let everyone
                        // watching reload from storage.
                        for wave_id in session.subscribed.clone() {
                            state.evict_after_panic(&wave_id).await;
                        }
                        let _ = session.tx.try_send(ServerMessage::error(
                            ErrorCode::Internal,
                            "Something went wrong handling that. Reconnecting.",
                        ));
                        break;
                    }
                }
            }
            Err(e) => {
                let _ = session.tx.try_send(ServerMessage::error(
                    ErrorCode::BadRequest,
                    format!("Could not understand that message: {e}"),
                ));
            }
        }
    }

    // Clean up: leave every wave, then drop the connection.
    for wave_id in session.subscribed.clone() {
        leave_wave(&state, &mut session, &wave_id).await;
    }
    state.unregister_conn(conn_id);
    writer.abort();
}

/// The human-readable half of a panic payload, which is a `&str` or a `String`
/// for every panic raised by `panic!`, `unwrap` or an index out of bounds.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(non-string panic payload)")
}

/// Handle one client command. `Err` carries a message to send back.
async fn dispatch(
    state: &Arc<AppState>,
    session: &mut Session,
    command: ClientMessage,
) -> Result<(), ServerMessage> {
    #[cfg(test)]
    if state
        .panic_next_command
        .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        panic!("deliberate panic from a test");
    }

    match command {
        ClientMessage::Ping => {
            let _ = session.tx.try_send(ServerMessage::Pong);
            Ok(())
        }

        ClientMessage::Open { wave_id } => open_wave(state, session, wave_id).await,

        ClientMessage::Close { wave_id } => {
            leave_wave(state, session, &wave_id).await;
            Ok(())
        }

        ClientMessage::CreateWave {
            title,
            participants,
            content,
            mode,
        } => create_wave(state, session, title, participants, content, mode).await,

        ClientMessage::Submit {
            blip_id,
            revision,
            delta,
            op_id,
        } => {
            // Named, like every other refusal of an op: a bare one leaves the
            // client holding work it retries forever, and everything typed
            // afterwards piles up behind it.
            if let Err(refusal) = check_embeds(&delta).and_then(|_| check_attributes(&delta)) {
                return Err(name_blip(*refusal, &blip_id));
            }
            let (_, wave) = find_blip(state, session, &blip_id).await?;
            let mut live = wave.lock().await;
            state
                .apply_op(
                    &mut live,
                    session.conn_id,
                    &session.user.id,
                    OpSubmission {
                        blip_id,
                        revision,
                        delta,
                        op_id,
                    },
                )
                .await
        }

        ClientMessage::CreateBlip {
            wavelet_id,
            parent,
            content,
        } => create_blip(state, session, wavelet_id, parent, content).await,

        ClientMessage::DeleteBlip { blip_id } => delete_blip(state, session, blip_id).await,

        ClientMessage::SetTitle { wavelet_id, title } => {
            set_title(state, session, wavelet_id, title).await
        }

        ClientMessage::SetMode { wave_id, mode } => set_mode(state, session, wave_id, mode).await,

        ClientMessage::AddParticipant { wavelet_id, name } => {
            add_participant(state, session, wavelet_id, name).await
        }

        ClientMessage::RemoveParticipant {
            wavelet_id,
            user_id,
        } => remove_participant(state, session, wavelet_id, user_id).await,

        ClientMessage::PrivateReply {
            wavelet_id,
            anchor,
            participants,
        } => private_reply(state, session, wavelet_id, anchor, participants).await,

        ClientMessage::CreateComment {
            wavelet_id,
            blip_id,
            comment_id,
            content,
        } => create_comment(state, session, wavelet_id, blip_id, comment_id, content).await,

        ClientMessage::ReplyToComment {
            comment_id,
            content,
        } => reply_to_comment(state, session, comment_id, content).await,

        ClientMessage::ResolveComment {
            comment_id,
            resolved,
        } => resolve_comment(state, session, comment_id, resolved).await,

        ClientMessage::Cursor {
            wave_id,
            blip_id,
            index,
            length,
        } => cursor(state, session, wave_id, blip_id, index, length).await,

        ClientMessage::MarkRead { wave_id } => {
            require_participant(state, session, &wave_id).await?;
            state
                .db
                .mark_wave_read(&session.user.id, &wave_id)
                .await
                .map_err(internal)?;
            send_inbox_row(state, session, &wave_id).await;
            Ok(())
        }

        ClientMessage::SetFlags { wave_id, flags } => {
            require_participant(state, session, &wave_id).await?;
            state
                .db
                .set_flags(&session.user.id, &wave_id, flags)
                .await
                .map_err(internal)?;
            send_inbox_row(state, session, &wave_id).await;
            Ok(())
        }

        ClientMessage::RequestPlayback { wave_id } => {
            require_participant(state, session, &wave_id).await?;
            let frames = state
                .db
                .playback(&session.user.id, &wave_id)
                .await
                .map_err(internal)?;
            let _ = session
                .tx
                .try_send(ServerMessage::Playback { wave_id, frames });
            Ok(())
        }

        ClientMessage::Search { query } => {
            let hits = state
                .db
                .search(&session.user.id, &query)
                .await
                .map_err(internal)?;
            let _ = session
                .tx
                .try_send(ServerMessage::SearchResults { query, hits });
            Ok(())
        }
    }
}

// --- command implementations -------------------------------------------

async fn open_wave(
    state: &Arc<AppState>,
    session: &mut Session,
    wave_id: WaveId,
) -> Result<(), ServerMessage> {
    // Fetch per-user state before taking the wave lock, so a disk read never
    // blocks other participants' edits.
    let read_marks = state.read_marks(&session.user.id, &wave_id).await;
    let flags = state
        .db
        .flags(&session.user.id, &wave_id)
        .await
        .unwrap_or_default();

    // Retry once: the wave may have been evicted between us taking the Arc and
    // acquiring its lock, in which case attaching here would subscribe us to a
    // copy that is about to be dropped.
    let mut wave = state
        .open_wave(&wave_id)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    let mut guard = wave.lock().await;
    if guard.evicted {
        drop(guard);
        wave = state
            .open_wave(&wave_id)
            .await
            .map_err(internal)?
            .ok_or_else(not_found)?;
        guard = wave.lock().await;
    }
    let live = &mut *guard;

    if !live.may_view(&session.user.id) {
        return Err(not_found());
    }

    live.subscribers.insert(
        session.conn_id,
        Subscriber {
            user_id: session.user.id.clone(),
            tx: session.tx.clone(),
            kill: session.kill.clone(),
            editing: None,
        },
    );
    session.subscribed.insert(wave_id.clone());

    let view = live.view(&session.user.id, &read_marks, flags);
    // Everything that follows is relative to this snapshot, so losing it would
    // leave the client subscribed to a wave it never received.
    live.send_to(session.conn_id, ServerMessage::WaveState { wave: view });
    broadcast_presence(live, &wave_id);
    Ok(())
}

async fn leave_wave(state: &Arc<AppState>, session: &mut Session, wave_id: &WaveId) {
    if !session.subscribed.remove(wave_id) {
        return;
    }
    if let Ok(Some(wave)) = state.open_wave(wave_id).await {
        let mut live = wave.lock().await;
        live.subscribers.remove(&session.conn_id);
        broadcast_presence(&mut live, wave_id);
    }
    state.maybe_evict(wave_id).await;
}

async fn create_wave(
    state: &Arc<AppState>,
    session: &mut Session,
    title: String,
    participants: Vec<String>,
    content: Option<Delta>,
    mode: Option<WaveMode>,
) -> Result<(), ServerMessage> {
    let (mut ids, missing) = state
        .db
        .resolve_names(participants)
        .await
        .map_err(internal)?;
    if !missing.is_empty() {
        return Err(ServerMessage::error(
            ErrorCode::BadRequest,
            format!("No such user: {}", missing.join(", ")),
        ));
    }
    // The creator is always a participant, and never duplicated.
    ids.retain(|id| id != &session.user.id);
    ids.insert(0, session.user.id.clone());

    let title = normalise_title(&title);
    let (wave, wavelet) = state
        .db
        .create_wave(
            session.user.id.clone(),
            title,
            ids,
            mode.unwrap_or_default(),
        )
        .await
        .map_err(internal)?;

    // Seed the first blip so a new wave is immediately writable.
    let mut blip = Blip::new(
        wave.id.clone(),
        wavelet.id.clone(),
        session.user.id.clone(),
        None,
        0,
    );
    if let Some(content) = seed_content(content).map_err(|e| *e)? {
        blip.content = content;
        blip.revision = 1;
    }
    state.db.insert_blip(blip.clone()).await.map_err(internal)?;
    if blip.revision > 0 {
        // Record the seed text in the op log so playback starts from empty.
        state
            .db
            .commit_op(
                blip.clone(),
                blip.content.clone(),
                session.user.id.clone(),
                blip.created_at,
                // Seeded at creation; there is no client op to be idempotent about.
                None,
            )
            .await
            .map_err(internal)?;
    }

    open_wave(state, session, wave.id.clone()).await?;
    state.schedule_inbox_update(&wave.id);
    Ok(())
}

async fn create_blip(
    state: &Arc<AppState>,
    session: &mut Session,
    wavelet_id: WaveletId,
    parent: Option<BlipId>,
    content: Option<Delta>,
) -> Result<(), ServerMessage> {
    let (wave_id, wave) = find_wavelet(state, session, &wavelet_id).await?;

    // The lock is held for the whole operation, including the writes. Releasing
    // it between the checks and the insert left two holes: two concurrent
    // creates could be handed the same ordering position, and a rule change
    // could land in the gap and be applied to a blip that had already passed its
    // checks.
    let mut live = wave.lock().await;
    if !live.may_access(&session.user.id, &wavelet_id) {
        return Err(not_found());
    }
    live.permit(
        &session.user.id,
        if parent.is_some() {
            Action::Reply
        } else {
            Action::NewMessage
        },
    )?;
    // A reply must attach to a blip in the same wavelet.
    if let Some(parent_id) = &parent {
        let ok = live
            .blips
            .get(parent_id)
            .is_some_and(|b| b.meta.wavelet_id == wavelet_id);
        if !ok {
            return Err(ServerMessage::error(
                ErrorCode::BadRequest,
                "Cannot reply to that blip.",
            ));
        }
    }

    let mut blip = Blip::new(
        wave_id.clone(),
        wavelet_id.clone(),
        session.user.id.clone(),
        parent,
        live.next_seq(&wavelet_id),
    );
    if let Some(content) = seed_content(content).map_err(|e| *e)? {
        blip.content = content;
        blip.revision = 1;
    }

    state.db.insert_blip(blip.clone()).await.map_err(internal)?;
    if blip.revision > 0 {
        if let Err(e) = state
            .db
            .commit_op(
                blip.clone(),
                blip.content.clone(),
                session.user.id.clone(),
                blip.created_at,
                // Seeded at creation; there is no client op to be idempotent about.
                None,
            )
            .await
        {
            // The row exists but its seed op does not, which would make playback
            // reconstruct the wrong document. Remove it rather than leave the
            // two disagreeing.
            let _ = state.db.delete_blip(&blip.id).await;
            return Err(internal(e));
        }
    }

    live.blips
        .insert(blip.id.clone(), crate::state::LiveBlip::new(blip.clone()));
    if let Some(wavelet) = live.wavelet_mut(&wavelet_id) {
        wavelet.last_modified = blip.last_modified;
    }
    // Everyone including the author: the author needs the id to focus it.
    let empty = Default::default();
    live.broadcast(
        &wavelet_id,
        None,
        ServerMessage::BlipAdded {
            wave_id: wave_id.clone(),
            blip: blip_view(&blip, &empty),
        },
    );
    drop(live);

    state.schedule_inbox_update(&wave_id);
    Ok(())
}

/// Open a comment thread on a range of a blip's text.
///
/// The range is not named here and is not stored: the client marks it by
/// applying a [`COMMENT_ATTRIBUTE`] run in an ordinary `Submit`, so the anchor
/// is part of the document and rides along with every edit. This creates the
/// thread that run points at, together with its first remark.
async fn create_comment(
    state: &Arc<AppState>,
    session: &mut Session,
    wavelet_id: WaveletId,
    blip_id: BlipId,
    comment_id: CommentId,
    content: Option<Delta>,
) -> Result<(), ServerMessage> {
    // A client mints this id, so it is checked before it reaches storage, the
    // wire, or anyone's DOM.
    if !comment_id.is_well_formed() {
        return Err(ServerMessage::error(
            ErrorCode::BadRequest,
            "That is not a usable comment id.",
        ));
    }
    let (wave_id, wave) = find_wavelet(state, session, &wavelet_id).await?;
    let content = seed_content(content).map_err(|e| *e)?;

    // Held across the writes, as in `create_blip`: releasing it between the
    // checks and the insert would let two clients claim the same id, and would
    // let the mode change in the gap.
    let mut live = wave.lock().await;
    if !live.may_access(&session.user.id, &wavelet_id) {
        return Err(not_found());
    }
    live.permit(&session.user.id, Action::Comment)?;

    if live.comments.contains_key(&comment_id) {
        return Err(ServerMessage::error(
            ErrorCode::BadRequest,
            "That comment already exists.",
        ));
    }
    let Some(target) = live.blips.get(&blip_id) else {
        return Err(not_found());
    };
    if target.meta.wavelet_id != wavelet_id {
        return Err(not_found());
    }
    // Comments annotate the page, not each other. Allowing a thread on a remark
    // would give a comment its own comments and no sensible place to draw them.
    if target.meta.comment.is_some() {
        return Err(ServerMessage::error(
            ErrorCode::BadRequest,
            "You cannot comment on a comment.",
        ));
    }

    let thread = CommentThread {
        id: comment_id.clone(),
        wavelet_id: wavelet_id.clone(),
        blip_id: blip_id.clone(),
        author: session.user.id.clone(),
        created_at: now(),
        resolved_by: None,
        resolved_at: None,
    };
    // The remark hangs off the blip it annotates, so the existing rule that a
    // blip with replies cannot be deleted already protects a commented page.
    let mut blip = Blip::new(
        wave_id.clone(),
        wavelet_id.clone(),
        session.user.id.clone(),
        Some(blip_id),
        live.next_seq(&wavelet_id),
    );
    blip.comment = Some(comment_id);
    if let Some(content) = content {
        blip.content = content;
        blip.revision = 1;
    }

    // Thread, remark and its seed op in one transaction. Unlike `create_blip`
    // there is no useful compensating action here: `delete_blip` is a soft
    // delete and leaves the `comments` row, so a half-written thread would
    // survive as a card with an author, a timestamp and nothing in it.
    state
        .db
        .create_comment(thread.clone(), blip.clone())
        .await
        .map_err(internal)?;

    live.comments.insert(thread.id.clone(), thread.clone());
    live.blips
        .insert(blip.id.clone(), crate::state::LiveBlip::new(blip.clone()));
    if let Some(wavelet) = live.wavelet_mut(&wavelet_id) {
        wavelet.last_modified = blip.last_modified;
    }

    let empty = Default::default();
    // Everyone including the author, who needs the ids to focus the new remark.
    live.broadcast(
        &wavelet_id,
        None,
        ServerMessage::CommentAdded {
            wave_id: wave_id.clone(),
            comment: thread,
            blip: blip_view(&blip, &empty),
        },
    );
    drop(live);

    state.schedule_inbox_update(&wave_id);
    Ok(())
}

async fn reply_to_comment(
    state: &Arc<AppState>,
    session: &mut Session,
    comment_id: CommentId,
    content: Option<Delta>,
) -> Result<(), ServerMessage> {
    let (wave_id, wave) = find_comment(state, session, &comment_id).await?;
    let content = seed_content(content).map_err(|e| *e)?;

    let mut live = wave.lock().await;
    let Some(thread) = live.comments.get(&comment_id).cloned() else {
        return Err(not_found());
    };
    if !live.may_access(&session.user.id, &thread.wavelet_id) {
        return Err(not_found());
    }
    live.permit(&session.user.id, Action::Comment)?;
    // A resolved thread is drawn collapsed, so a remark added to one would be
    // written and then not shown. Reopening is one click and says what happened.
    if thread.resolved() {
        return Err(ServerMessage::error(
            ErrorCode::BadRequest,
            "This comment is resolved. Reopen it to reply.",
        ));
    }

    let mut blip = Blip::new(
        wave_id.clone(),
        thread.wavelet_id.clone(),
        session.user.id.clone(),
        Some(thread.blip_id.clone()),
        live.next_seq(&thread.wavelet_id),
    );
    blip.comment = Some(comment_id);
    if let Some(content) = content {
        blip.content = content;
        blip.revision = 1;
    }

    state.db.insert_blip(blip.clone()).await.map_err(internal)?;
    if blip.revision > 0 {
        if let Err(e) = state
            .db
            .commit_op(
                blip.clone(),
                blip.content.clone(),
                session.user.id.clone(),
                blip.created_at,
                None,
            )
            .await
        {
            let _ = state.db.delete_blip(&blip.id).await;
            return Err(internal(e));
        }
    }

    live.blips
        .insert(blip.id.clone(), crate::state::LiveBlip::new(blip.clone()));
    if let Some(wavelet) = live.wavelet_mut(&thread.wavelet_id) {
        wavelet.last_modified = blip.last_modified;
    }
    let empty = Default::default();
    live.broadcast(
        &thread.wavelet_id,
        None,
        ServerMessage::BlipAdded {
            wave_id: wave_id.clone(),
            blip: blip_view(&blip, &empty),
        },
    );
    drop(live);

    state.schedule_inbox_update(&wave_id);
    Ok(())
}

/// Close a thread, or reopen it.
///
/// Any participant may, which matches the mode this exists for: a notepad is a
/// page everyone edits, so a thread about it is everyone's to close. Who did it
/// is recorded rather than restricted.
async fn resolve_comment(
    state: &Arc<AppState>,
    session: &mut Session,
    comment_id: CommentId,
    resolved: bool,
) -> Result<(), ServerMessage> {
    let (wave_id, wave) = find_comment(state, session, &comment_id).await?;

    let mut live = wave.lock().await;
    let Some(thread) = live.comments.get(&comment_id).cloned() else {
        return Err(not_found());
    };
    if !live.may_access(&session.user.id, &thread.wavelet_id) {
        return Err(not_found());
    }
    live.permit(&session.user.id, Action::ResolveComment)?;
    if thread.resolved() == resolved {
        return Ok(()); // already how the caller wants it
    }

    let (resolved_by, resolved_at) = if resolved {
        (Some(session.user.id.clone()), Some(now()))
    } else {
        (None, None)
    };

    // Written before anyone is told, and before the resident copy moves. The
    // in-memory flip is also what the idempotent short-circuit above reads, so
    // announcing first and failing to write would leave every client showing a
    // settled thread, the database showing an open one, and a retry returning
    // `Ok` without ever reaching storage. The lock is held across the write, as
    // it is in `create_blip`, so nothing can race between the two.
    state
        .db
        .set_comment_resolved(&comment_id, resolved_by.clone(), resolved_at)
        .await
        .map_err(internal)?;

    if let Some(thread) = live.comments.get_mut(&comment_id) {
        thread.resolved_by = resolved_by.clone();
        thread.resolved_at = resolved_at;
    }
    live.broadcast(
        &thread.wavelet_id,
        None,
        ServerMessage::CommentResolved {
            wave_id,
            comment_id,
            resolved_by,
            resolved_at,
        },
    );
    Ok(())
}

async fn delete_blip(
    state: &Arc<AppState>,
    session: &mut Session,
    blip_id: BlipId,
) -> Result<(), ServerMessage> {
    let (wave_id, wave) = find_blip(state, session, &blip_id).await?;
    let mut live = wave.lock().await;

    let Some(blip) = live.blips.get(&blip_id) else {
        return Err(not_found());
    };
    let wavelet_id = blip.meta.wavelet_id.clone();
    if !live.may_access(&session.user.id, &wavelet_id) {
        return Err(not_found());
    }
    // A remark is not deletable on its own, in any mode. Deleting the first one
    // would leave a thread with nothing in it — a state the client cannot draw
    // and the protocol has no way to repair — and the wave may since have left
    // Notepad, where `allows_delete` would otherwise wave this through.
    // Resolving retracts a comment and keeps the record; deleting the message it
    // is about takes the whole thread with it, below.
    if blip.meta.comment.is_some() {
        return Err(ServerMessage::error(
            ErrorCode::Forbidden,
            "A comment is not deleted on its own. Resolve the thread instead.",
        ));
    }
    if blip.meta.author != session.user.id {
        return Err(ServerMessage::error(
            ErrorCode::Forbidden,
            "Only the author can delete a message.",
        ));
    }
    live.permit(&session.user.id, Action::Delete)?;
    // Keep the thread intact: a blip with replies would orphan them. Remarks are
    // not replies for this purpose — they are annotations *on* this message, and
    // counting them here made a commented message undeletable for good, since
    // the refusal above means the remarks could never be cleared either.
    if live
        .blips
        .values()
        .any(|b| b.meta.parent.as_ref() == Some(&blip_id) && b.meta.comment.is_none())
    {
        return Err(ServerMessage::error(
            ErrorCode::BadRequest,
            "Delete the replies to this message first.",
        ));
    }

    // Comments go with the text they were about. A thread that outlived the
    // message it annotated would be a remark about nothing, and unlike a
    // detached anchor — where the message is still there and the words are not —
    // there would be no way left to read what it referred to.
    let threads: Vec<CommentId> = live
        .comments
        .values()
        .filter(|c| c.blip_id == blip_id)
        .map(|c| c.id.clone())
        .collect();
    let remarks: Vec<BlipId> = live
        .blips
        .values()
        .filter(|b| {
            b.meta
                .comment
                .as_ref()
                .is_some_and(|id| threads.contains(id))
        })
        .map(|b| b.meta.id.clone())
        .collect();

    live.blips.remove(&blip_id);
    for remark in &remarks {
        live.blips.remove(remark);
    }
    for thread in &threads {
        live.comments.remove(thread);
    }
    // Remarks first, so no client is briefly holding a thread whose anchor blip
    // has gone but whose remarks have not.
    for remark in &remarks {
        live.broadcast(
            &wavelet_id,
            None,
            ServerMessage::BlipRemoved {
                wave_id: wave_id.clone(),
                blip_id: remark.clone(),
            },
        );
    }
    live.broadcast(
        &wavelet_id,
        None,
        ServerMessage::BlipRemoved {
            wave_id: wave_id.clone(),
            blip_id: blip_id.clone(),
        },
    );
    drop(live);

    for remark in &remarks {
        state.db.delete_blip(remark).await.map_err(internal)?;
    }
    for thread in &threads {
        state.db.delete_comment(thread).await.map_err(internal)?;
    }
    state.db.delete_blip(&blip_id).await.map_err(internal)?;
    state.schedule_inbox_update(&wave_id);
    Ok(())
}

async fn set_title(
    state: &Arc<AppState>,
    session: &mut Session,
    wavelet_id: WaveletId,
    title: String,
) -> Result<(), ServerMessage> {
    let (wave_id, wave) = find_wavelet(state, session, &wavelet_id).await?;
    let title = normalise_title(&title);

    let mut live = wave.lock().await;
    if !live.may_access(&session.user.id, &wavelet_id) {
        return Err(not_found());
    }
    live.permit(&session.user.id, Action::Retitle)?;
    if let Some(wavelet) = live.wavelet_mut(&wavelet_id) {
        wavelet.title = title.clone();
    }
    live.broadcast(
        &wavelet_id,
        None,
        ServerMessage::TitleChanged {
            wave_id: wave_id.clone(),
            wavelet_id: wavelet_id.clone(),
            title: title.clone(),
        },
    );
    drop(live);

    state
        .db
        .set_title(&wavelet_id, title)
        .await
        .map_err(internal)?;
    state.schedule_inbox_update(&wave_id);
    Ok(())
}

/// Change how a wave behaves.
///
/// Applies to the whole wave rather than a single wavelet, so a private reply
/// cannot keep the old rules after the wave is frozen.
async fn set_mode(
    state: &Arc<AppState>,
    session: &mut Session,
    wave_id: WaveId,
    mode: WaveMode,
) -> Result<(), ServerMessage> {
    if !session.subscribed.contains(&wave_id) {
        return Err(not_found());
    }
    let wave = state
        .open_wave(&wave_id)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;

    let mut live = wave.lock().await;
    if !live.may_view(&session.user.id) {
        return Err(not_found());
    }
    live.permit(&session.user.id, Action::SetMode)?;
    if live.wave.mode == mode {
        return Ok(());
    }
    live.wave.mode = mode;

    // Reaches every participant of the wave, not just one wavelet: the mode
    // governs private replies too.
    let wavelet_ids: Vec<WaveletId> = live.wavelets.iter().map(|w| w.id.clone()).collect();
    for wavelet_id in wavelet_ids {
        live.broadcast(
            &wavelet_id,
            None,
            ServerMessage::ModeChanged {
                wave_id: wave_id.clone(),
                mode,
            },
        );
    }
    drop(live);

    state.db.set_mode(&wave_id, mode).await.map_err(internal)?;
    state.schedule_inbox_update(&wave_id);
    Ok(())
}

async fn add_participant(
    state: &Arc<AppState>,
    session: &mut Session,
    wavelet_id: WaveletId,
    name: String,
) -> Result<(), ServerMessage> {
    let (wave_id, wave) = find_wavelet(state, session, &wavelet_id).await?;
    let Some(user) = state.db.user_by_name(&name).await.map_err(internal)? else {
        return Err(ServerMessage::error(
            ErrorCode::BadRequest,
            format!("No user called {name}."),
        ));
    };

    let mut live = wave.lock().await;
    if !live.may_access(&session.user.id, &wavelet_id) {
        return Err(not_found());
    }
    // A private reply may only contain people who are already in the wave it
    // hangs off. Without this check, adding an outsider straight to a private
    // wavelet bypassed the very restriction `private_reply` enforces, and leaked
    // the parent wave's title and roster to them.
    let is_private = live
        .wavelet(&wavelet_id)
        .is_some_and(|w| w.kind == WaveletKind::PrivateReply);
    if is_private {
        let in_wave = live
            .wavelets
            .iter()
            .any(|w| w.kind == WaveletKind::Conversation && w.has_participant(&user.id));
        if !in_wave {
            return Err(ServerMessage::error(
                ErrorCode::Forbidden,
                format!(
                    "{} is not in this wave yet — add them to the wave first.",
                    user.display_name
                ),
            ));
        }
    }

    let Some(wavelet) = live.wavelet_mut(&wavelet_id) else {
        return Err(not_found());
    };
    if wavelet.has_participant(&user.id) {
        return Ok(()); // already there; nothing to do
    }
    wavelet.participants.push(user.id.clone());
    live.user_cache.insert(user.id.clone(), user.public());
    live.broadcast(
        &wavelet_id,
        None,
        ServerMessage::ParticipantAdded {
            wave_id: wave_id.clone(),
            wavelet_id: wavelet_id.clone(),
            user: user.public(),
        },
    );
    drop(live);

    state
        .db
        .add_participant(&wavelet_id, &user.id)
        .await
        .map_err(internal)?;
    state.schedule_inbox_update(&wave_id);
    Ok(())
}

async fn remove_participant(
    state: &Arc<AppState>,
    session: &mut Session,
    wavelet_id: WaveletId,
    user_id: UserId,
) -> Result<(), ServerMessage> {
    let (wave_id, wave) = find_wavelet(state, session, &wavelet_id).await?;

    let mut live = wave.lock().await;
    if !live.may_access(&session.user.id, &wavelet_id) {
        return Err(not_found());
    }
    // Removal is either leaving yourself, or the wave's creator removing someone.
    // Letting any participant evict any other made a hostile takeover trivial:
    // whoever moved first could remove everyone else, including the creator,
    // with no undo and no way back in.
    let is_self = user_id == session.user.id;
    let is_creator = live.wave.creator == session.user.id;
    if !is_self && !is_creator {
        return Err(ServerMessage::error(
            ErrorCode::Forbidden,
            "Only the person who started this wave can remove someone else.              You can always remove yourself.",
        ));
    }

    let kind = live.wavelet(&wavelet_id).map(|w| w.kind);
    let Some(wavelet) = live.wavelet_mut(&wavelet_id) else {
        return Err(not_found());
    };
    if !wavelet.participants.iter().any(|p| p == &user_id) {
        return Ok(()); // already gone
    }
    // Never strand a wavelet with no one in it.
    if wavelet.participants.len() <= 1 {
        return Err(ServerMessage::error(
            ErrorCode::BadRequest,
            "A wave needs at least one participant.",
        ));
    }
    wavelet.participants.retain(|p| p != &user_id);
    live.broadcast(
        &wavelet_id,
        None,
        ServerMessage::ParticipantRemoved {
            wave_id: wave_id.clone(),
            wavelet_id: wavelet_id.clone(),
            user_id: user_id.clone(),
        },
    );

    // Being in the wave *is* being in its conversation: `add_participant`
    // refuses to put anyone into a private reply who is not in the conversation
    // first. Leaving the conversation therefore has to take the private replies
    // with it, or that invariant holds in one direction only.
    //
    // It did. `may_view` asks whether the user is in *any* wavelet, so someone
    // removed from the conversation kept every side conversation they had been
    // in — its live updates, its search hits, its playback and its attachments
    // — and kept them silently, since the roster they disappeared from was the
    // only one anybody was looking at. The creator generally could not repair
    // it by hand either: removing someone from a private reply requires being
    // in that private reply, and by construction they may not be.
    let mut cascaded = Vec::new();
    if kind == Some(WaveletKind::Conversation) {
        for wavelet in live.wavelets.iter_mut() {
            if wavelet.kind == WaveletKind::PrivateReply && wavelet.has_participant(&user_id) {
                wavelet.participants.retain(|p| p != &user_id);
                cascaded.push(wavelet.id.clone());
            }
        }
        // No minimum-participant guard here, unlike the conversation above. A
        // private reply whose last member leaves the wave is left empty and
        // therefore unreadable, which is the right outcome: the alternative is
        // keeping them in it to avoid the empty state, and that is the bug.
        for id in &cascaded {
            live.broadcast(
                id,
                None,
                ServerMessage::ParticipantRemoved {
                    wave_id: wave_id.clone(),
                    wavelet_id: id.clone(),
                    user_id: user_id.clone(),
                },
            );
        }
    }

    // If the removed user can no longer see the wave at all, evict their
    // subscriptions so they stop receiving updates immediately.
    if !live.may_view(&user_id) {
        let stale: Vec<ConnId> = live
            .subscribers
            .iter()
            .filter(|(_, sub)| sub.user_id == user_id)
            .map(|(id, _)| *id)
            .collect();
        for conn_id in stale {
            live.subscribers.remove(&conn_id);
        }
        state.send_to_user(
            &user_id,
            &ServerMessage::WaveRemoved {
                wave_id: wave_id.clone(),
            },
        );
    }
    drop(live);

    state
        .db
        .remove_participant(&wavelet_id, &user_id)
        .await
        .map_err(internal)?;
    for id in &cascaded {
        state
            .db
            .remove_participant(id, &user_id)
            .await
            .map_err(internal)?;
    }
    state.schedule_inbox_update(&wave_id);
    Ok(())
}

async fn private_reply(
    state: &Arc<AppState>,
    session: &mut Session,
    wavelet_id: WaveletId,
    anchor: BlipId,
    participants: Vec<String>,
) -> Result<(), ServerMessage> {
    let (wave_id, wave) = find_wavelet(state, session, &wavelet_id).await?;
    let (mut ids, missing) = state
        .db
        .resolve_names(participants)
        .await
        .map_err(internal)?;
    if !missing.is_empty() {
        return Err(ServerMessage::error(
            ErrorCode::BadRequest,
            format!("No such user: {}", missing.join(", ")),
        ));
    }

    let live = wave.lock().await;

    if !live.may_access(&session.user.id, &wavelet_id) {
        return Err(not_found());
    }
    live.permit(&session.user.id, Action::PrivateReply)?;
    if !live.blips.contains_key(&anchor) {
        return Err(not_found());
    }
    // A private reply may only include people who are already in the parent
    // wavelet — otherwise it would leak the wave to an outsider.
    let parent_members: HashSet<UserId> = live
        .wavelet(&wavelet_id)
        .map(|w| w.participants.iter().cloned().collect())
        .unwrap_or_default();
    if let Some(outsider) = ids.iter().find(|id| !parent_members.contains(id)) {
        let name = live
            .user_cache
            .get(outsider)
            .map(|u| u.display_name.clone())
            .unwrap_or_else(|| outsider.to_string());
        return Err(ServerMessage::error(
            ErrorCode::Forbidden,
            format!("{name} is not in this wave yet — add them to the wave first."),
        ));
    }
    let title = live.title();
    drop(live);

    ids.retain(|id| id != &session.user.id);
    ids.insert(0, session.user.id.clone());

    let wavelet = Wavelet {
        id: WaveletId::new(),
        wave_id: wave_id.clone(),
        kind: WaveletKind::PrivateReply,
        title,
        participants: ids.clone(),
        anchor_blip: Some(anchor),
        created_at: now(),
        last_modified: now(),
    };
    state
        .db
        .create_wavelet(wavelet.clone())
        .await
        .map_err(internal)?;

    let seq = {
        let live = wave.lock().await;
        live.next_seq(&wavelet.id)
    };
    let blip = Blip::new(
        wave_id.clone(),
        wavelet.id.clone(),
        session.user.id.clone(),
        None,
        seq,
    );
    state.db.insert_blip(blip.clone()).await.map_err(internal)?;

    let mut live = wave.lock().await;
    live.wavelets.push(wavelet.clone());
    live.blips
        .insert(blip.id.clone(), crate::state::LiveBlip::new(blip.clone()));

    let empty = Default::default();
    let users = live.user_cache.clone();
    let view = WaveletView {
        id: wavelet.id.clone(),
        wave_id: wave_id.clone(),
        kind: wavelet.kind,
        title: wavelet.title.clone(),
        participants: ids.iter().filter_map(|id| users.get(id).cloned()).collect(),
        anchor_blip: wavelet.anchor_blip.clone(),
        created_at: wavelet.created_at,
        last_modified: wavelet.last_modified,
        blips: vec![blip_view(&blip, &empty)],
        // A wavelet created this instant has nothing anchored in it yet.
        comments: Vec::new(),
    };
    live.broadcast(
        &wavelet.id,
        None,
        ServerMessage::WaveletAdded {
            wave_id: wave_id.clone(),
            wavelet: view,
        },
    );
    drop(live);

    state.schedule_inbox_update(&wave_id);
    Ok(())
}

async fn cursor(
    state: &Arc<AppState>,
    session: &mut Session,
    wave_id: WaveId,
    blip_id: BlipId,
    index: usize,
    length: usize,
) -> Result<(), ServerMessage> {
    if !session.subscribed.contains(&wave_id) {
        return Ok(()); // stale message from a wave we just closed
    }
    let Ok(Some(wave)) = state.open_wave(&wave_id).await else {
        return Ok(());
    };
    let mut live = wave.lock().await;

    let Some(wavelet_id) = live.blips.get(&blip_id).map(|b| b.meta.wavelet_id.clone()) else {
        return Ok(());
    };
    if !live.may_access(&session.user.id, &wavelet_id) {
        return Ok(());
    }
    if let Some(sub) = live.subscribers.get_mut(&session.conn_id) {
        sub.editing = Some(blip_id.clone());
    }
    live.broadcast(
        &wavelet_id,
        Some(session.conn_id),
        ServerMessage::Cursor {
            wave_id: wave_id.clone(),
            blip_id,
            user_id: session.user.id.clone(),
            index,
            length,
        },
    );
    broadcast_presence(&mut live, &wave_id);
    Ok(())
}

// --- helpers ------------------------------------------------------------

/// Locate the resident wave holding `wavelet_id`, restricted to waves this
/// connection has open. Searching rather than trusting a client-supplied wave id
/// means access is checked against state the server established.
async fn find_wavelet(
    state: &Arc<AppState>,
    session: &Session,
    wavelet_id: &WaveletId,
) -> Result<(WaveId, Arc<Mutex<LiveWave>>), ServerMessage> {
    for wave_id in &session.subscribed {
        if let Ok(Some(wave)) = state.open_wave(wave_id).await {
            if wave.lock().await.wavelet(wavelet_id).is_some() {
                return Ok((wave_id.clone(), wave));
            }
        }
    }
    Err(not_found())
}

/// Same, for a blip.
async fn find_blip(
    state: &Arc<AppState>,
    session: &Session,
    blip_id: &BlipId,
) -> Result<(WaveId, Arc<Mutex<LiveWave>>), ServerMessage> {
    for wave_id in &session.subscribed {
        if let Ok(Some(wave)) = state.open_wave(wave_id).await {
            if wave.lock().await.blips.contains_key(blip_id) {
                return Ok((wave_id.clone(), wave));
            }
        }
    }
    Err(not_found())
}

/// Same, for a comment thread.
async fn find_comment(
    state: &Arc<AppState>,
    session: &Session,
    comment_id: &CommentId,
) -> Result<(WaveId, Arc<Mutex<LiveWave>>), ServerMessage> {
    for wave_id in &session.subscribed {
        if let Ok(Some(wave)) = state.open_wave(wave_id).await {
            if wave.lock().await.comments.contains_key(comment_id) {
                return Ok((wave_id.clone(), wave));
            }
        }
    }
    Err(not_found())
}

/// Send each watcher the presence list scoped to what they may see.
///
/// Built per recipient rather than once, because an unscoped list reveals that a
/// private reply exists: `editing` would name a blip in a wavelet the recipient
/// is not part of.
fn broadcast_presence(live: &mut LiveWave, wave_id: &WaveId) {
    let recipients: Vec<(ConnId, UserId)> = live
        .subscribers
        .iter()
        .map(|(id, sub)| (*id, sub.user_id.clone()))
        .collect();

    let mut failed = Vec::new();
    for (conn_id, user_id) in recipients {
        let users = live.presence_for(&user_id);
        let message = ServerMessage::Presence {
            wave_id: wave_id.clone(),
            users,
        };
        if let Some(sub) = live.subscribers.get(&conn_id) {
            if sub.tx.try_send(message).is_err() {
                failed.push(conn_id);
            }
        }
    }
    live.disconnect(&failed);
}

/// Push one refreshed inbox row to the requesting connection.
async fn send_inbox_row(state: &Arc<AppState>, session: &Session, wave_id: &WaveId) {
    if let Ok(Some(summary)) = state.db.wave_summary(&session.user.id, wave_id).await {
        let _ = session.tx.try_send(ServerMessage::InboxUpdated { summary });
    }
}

/// Validate client-supplied seed content before it becomes a blip.
///
/// A document may only contain inserts. Anything else cannot be loaded back as a
/// `ServerDoc`, and the failure was previously swallowed into an empty document
/// whose revision disagreed with the stored metadata — permanently bricking the
/// blip, since every later edit was then rejected as out of date.
fn seed_content(content: Option<Delta>) -> Result<Option<Delta>, Box<ServerMessage>> {
    let Some(content) = content.filter(|c| !c.is_empty()) else {
        return Ok(None);
    };
    if !content.is_document() {
        return Err(Box::new(ServerMessage::error(
            ErrorCode::BadRequest,
            "Initial content must consist only of insertions.",
        )));
    }
    if content.len() > MAX_BLIP_UNITS || content.ops.len() > MAX_BLIP_RUNS {
        return Err(Box::new(ServerMessage::error(
            ErrorCode::BadRequest,
            "That message is too large.",
        )));
    }
    check_embeds(&content)?;
    check_attributes(&content)?;
    Ok(Some(content))
}

/// Largest embed payload, serialised.
///
/// An embed counts as one unit of a document however much JSON it carries, so
/// the length limits above say nothing about its size. An attachment reference
/// is a few hundred bytes; this leaves room for that and no room for using a
/// document as general-purpose storage.
const MAX_EMBED_BYTES: usize = 1024;

/// Attach a blip id to a refusal, so the client knows which document to drop.
fn name_blip(mut refusal: ServerMessage, blip_id: &BlipId) -> ServerMessage {
    if let ServerMessage::Error {
        blip_id: ref mut target,
        ..
    } = refusal
    {
        *target = Some(blip_id.clone());
    }
    refusal
}

/// Longest `link` value a document may carry.
///
/// Browsers stop honouring URLs well before this. It is here to bound what a
/// run can hold, not to have an opinion about addresses.
const MAX_LINK_BYTES: usize = 1024;

/// The boolean attributes this application's documents use.
const FLAG_ATTRIBUTES: [&str; 5] = ["bold", "italic", "underline", "strike", "code"];

/// Reject attributes that are not part of the document model.
///
/// A delta's attribute map is `String -> serde_json::Value`, so without this a
/// participant could hang arbitrary JSON of arbitrary size off every run of a
/// document. Nothing else bounded it: `check_embeds` covers embeds only, and the
/// length limits count *units*, of which an attribute map costs none however
/// much it carries.
///
/// Unknown keys are refused rather than ignored, which is the same choice this
/// protocol already makes for unknown message fields — silently keeping
/// something the server does not understand is how a document ends up holding
/// data no version of this program will ever read again.
///
/// Note what this deliberately does not do: it bounds a link's *size*, not its
/// scheme. Which URLs are safe to turn into anchors is the renderer's judgement
/// (`safeUrl` in `web/editor.js`), and a second copy of that rule here would be
/// one that could drift out of step with the one that actually protects anyone.
fn check_attributes(delta: &Delta) -> Result<(), Box<ServerMessage>> {
    let refuse = |what: &str| {
        Err(Box::new(ServerMessage::error(
            ErrorCode::BadRequest,
            format!("That formatting is not something this server accepts: {what}."),
        )))
    };

    for op in &delta.ops {
        for (key, value) in &op.attributes {
            // A null value *removes* an attribute, which is how a retain strips
            // formatting. It carries nothing, so there is nothing to check.
            if value.is_null() {
                continue;
            }
            let ok = match key.as_str() {
                k if FLAG_ATTRIBUTES.contains(&k) => value.is_boolean(),
                "link" => value
                    .as_str()
                    .is_some_and(|url| url.len() <= MAX_LINK_BYTES),
                COMMENT_ATTRIBUTE => value
                    .as_str()
                    .is_some_and(|id| CommentId::from(id).is_well_formed()),
                _ => false,
            };
            if !ok {
                return refuse(key);
            }
        }
    }
    Ok(())
}

/// Reject embeds that are not small JSON objects.
///
/// Deltas accept an arbitrary JSON value as an embed, which is what lets one
/// carry an attachment reference — and, unchecked, anything else a client cares
/// to invent.
fn check_embeds(delta: &Delta) -> Result<(), Box<ServerMessage>> {
    for op in &delta.ops {
        let OpKind::Insert(Insert::Embed(value)) = &op.kind else {
            continue;
        };
        let too_big = !value.is_object()
            || serde_json::to_string(value)
                .map(|s| s.len())
                .unwrap_or(usize::MAX)
                > MAX_EMBED_BYTES;
        if too_big {
            return Err(Box::new(ServerMessage::error(
                ErrorCode::BadRequest,
                "That embedded object is not something this server accepts.",
            )));
        }
    }
    Ok(())
}

/// Reject commands naming a wave the caller does not participate in.
///
/// Used for the per-user operations that touch storage directly rather than
/// going through a resident wave, where the participant check would otherwise be
/// missing entirely.
async fn require_participant(
    state: &Arc<AppState>,
    session: &Session,
    wave_id: &WaveId,
) -> Result<(), ServerMessage> {
    match state.db.is_participant(&session.user.id, wave_id).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(not_found()),
        Err(e) => Err(internal(e)),
    }
}

/// Titles are single-line and bounded; the UI renders them in one line.
fn normalise_title(title: &str) -> String {
    let cleaned: String = title.trim().chars().filter(|c| !c.is_control()).collect();
    let cleaned = if cleaned.is_empty() {
        "Untitled wave".to_string()
    } else {
        cleaned
    };
    cleaned.chars().take(200).collect()
}

fn not_found() -> ServerMessage {
    ServerMessage::error(ErrorCode::NotFound, "That wave is not available.")
}

fn internal(e: anyhow::Error) -> ServerMessage {
    tracing::error!(error = %e, "request failed");
    ServerMessage::error(ErrorCode::Internal, "Something went wrong on the server.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_trimmed_bounded_and_never_empty() {
        assert_eq!(normalise_title("  Launch plan  "), "Launch plan");
        assert_eq!(normalise_title(""), "Untitled wave");
        assert_eq!(normalise_title("   "), "Untitled wave");
        assert_eq!(
            normalise_title("a\nb\tc"),
            "abc",
            "control characters are stripped"
        );
        assert_eq!(normalise_title(&"x".repeat(500)).chars().count(), 200);
    }

    #[test]
    fn embeds_must_be_small_objects() {
        let attachment = serde_json::json!({
            "attachment": { "id": "a-1", "name": "plan.png", "mime": "image/png", "size": 4096 }
        });
        let ok = Delta::new().embed(attachment);
        assert!(check_embeds(&ok).is_ok());

        // An embed is one unit of a document however much it carries, so
        // nothing else bounds this.
        let huge = serde_json::json!({ "blob": "x".repeat(MAX_EMBED_BYTES) });
        let too_big = Delta::new().embed(huge);
        assert!(check_embeds(&too_big).is_err());

        // A bare scalar is not something any client of this server produces.
        let scalar = Delta::new().embed(serde_json::json!("hello"));
        assert!(check_embeds(&scalar).is_err());
    }

    #[test]
    fn ordinary_text_passes_the_embed_check() {
        assert!(check_embeds(&Delta::document("just words")).is_ok());
    }

    /// Everything the shipped client actually sends must survive the check, or
    /// the limit is a bug rather than a bound.
    #[test]
    fn the_formatting_the_client_uses_is_accepted() {
        let mut attrs = gal_ot::Attributes::new();
        for flag in FLAG_ATTRIBUTES {
            attrs.insert(flag.to_string(), serde_json::json!(true));
        }
        attrs.insert("link".into(), serde_json::json!("https://example.com/x"));
        attrs.insert(
            COMMENT_ATTRIBUTE.into(),
            serde_json::json!(CommentId::new().as_str()),
        );
        assert!(check_attributes(&Delta::new().insert_with("hello", attrs)).is_ok());

        // A bare domain: the client stores what was typed and normalises it at
        // render time, so the server must not insist on a scheme.
        let mut bare = gal_ot::Attributes::new();
        bare.insert("link".into(), serde_json::json!("example.com/plan"));
        assert!(check_attributes(&Delta::new().insert_with("x", bare)).is_ok());

        // A retain that strips formatting carries nulls, which remove rather
        // than store and so have nothing to bound.
        let mut removals = gal_ot::Attributes::new();
        removals.insert("bold".into(), serde_json::Value::Null);
        removals.insert(COMMENT_ATTRIBUTE.into(), serde_json::Value::Null);
        assert!(check_attributes(&Delta::new().retain_with(3, removals)).is_ok());

        assert!(check_attributes(&Delta::document("plain")).is_ok());
    }

    /// An attribute map is `String -> Value` and costs no document *units*, so
    /// nothing else stood between a participant and hanging arbitrary JSON off
    /// every run of a document.
    #[test]
    fn attributes_outside_the_document_model_are_refused() {
        let case = |key: &str, value: serde_json::Value| {
            let mut attrs = gal_ot::Attributes::new();
            attrs.insert(key.to_string(), value);
            check_attributes(&Delta::new().insert_with("x", attrs))
        };

        // A key this server has never heard of, holding as much as it likes.
        assert!(case("payload", serde_json::json!("x".repeat(100_000))).is_err());
        assert!(case("payload", serde_json::json!({ "nested": [1, 2, 3] })).is_err());
        // Real keys, wrong shapes — a "flag" is a boolean, not a place to put
        // half a megabyte.
        assert!(case("bold", serde_json::json!("x".repeat(500_000))).is_err());
        assert!(case("bold", serde_json::json!({ "on": true })).is_err());
        assert!(case("link", serde_json::json!("x".repeat(MAX_LINK_BYTES + 1))).is_err());
        assert!(case("link", serde_json::json!(["https://example.com"])).is_err());
        // A comment id reaches SQL, the wire and the DOM, so the same rule
        // applies to it here as when a thread is opened.
        assert!(case(COMMENT_ATTRIBUTE, serde_json::json!("not-a-comment-id")).is_err());
        assert!(case(COMMENT_ATTRIBUTE, serde_json::json!("c-<script>")).is_err());
        assert!(case(COMMENT_ATTRIBUTE, serde_json::json!(true)).is_err());
    }
}
