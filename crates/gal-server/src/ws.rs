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
use futures::{SinkExt, StreamExt};
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

/// Largest document a single blip may hold, in UTF-16 code units.
///
/// Documents are held in memory with their edit history, so an unbounded blip is
/// an unbounded allocation.
const MAX_BLIP_UNITS: usize = 256 * 1024;

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
                if let Err(reply) = dispatch(&state, &mut session, command).await {
                    let _ = session.tx.try_send(reply);
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

/// Handle one client command. `Err` carries a message to send back.
async fn dispatch(
    state: &Arc<AppState>,
    session: &mut Session,
    command: ClientMessage,
) -> Result<(), ServerMessage> {
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
            if let Err(refusal) = check_embeds(&delta) {
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
    if blip.meta.author != session.user.id {
        return Err(ServerMessage::error(
            ErrorCode::Forbidden,
            "Only the author can delete a message.",
        ));
    }
    live.permit(&session.user.id, Action::Delete)?;
    // Keep the thread intact: a blip with replies would orphan them.
    if live
        .blips
        .values()
        .any(|b| b.meta.parent.as_ref() == Some(&blip_id))
    {
        return Err(ServerMessage::error(
            ErrorCode::BadRequest,
            "Delete the replies to this message first.",
        ));
    }

    live.blips.remove(&blip_id);
    live.broadcast(
        &wavelet_id,
        None,
        ServerMessage::BlipRemoved {
            wave_id: wave_id.clone(),
            blip_id: blip_id.clone(),
        },
    );
    drop(live);

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
    if content.len() > MAX_BLIP_UNITS {
        return Err(Box::new(ServerMessage::error(
            ErrorCode::BadRequest,
            "That message is too large.",
        )));
    }
    check_embeds(&content)?;
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
}
