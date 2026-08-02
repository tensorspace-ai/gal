//! Live server state: resident waves, connections, and op routing.
//!
//! # Concurrency
//!
//! Each open wave is an actor: an `Arc<Mutex<LiveWave>>` holding its documents
//! and its subscribers. Every mutation of a wave — applying an op, adding a
//! blip, changing participants — happens while holding that one lock, which
//! gives a total order per wave. Different waves never contend, so throughput
//! scales with the number of active conversations.
//!
//! An op is persisted *before* it is acknowledged, so a client is never told
//! its edit landed when it might not survive a crash.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use dashmap::DashMap;
use gal_core::model::*;
use gal_core::protocol::*;
use gal_ot::{Delta, OtError, ServerDoc};
use tokio::sync::{mpsc, Mutex, Notify};

use crate::config::Config;
use crate::db::Storage;
use crate::limit::{client_key, RateLimiter};

/// Identifies one WebSocket connection.
pub type ConnId = u64;

/// How long to coalesce inbox notifications for a wave.
///
/// Typing generates an op per keystroke, but the inbox only shows a snippet and
/// an unread count. Recomputing that per keystroke for every non-viewing
/// participant would dominate the server's work, so updates are batched.
const INBOX_DEBOUNCE_MS: u64 = 600;

/// Largest document a single blip may hold, in UTF-16 code units.
///
/// A resident wave keeps every blip's document *and* the history needed to
/// rebase against it, so an unbounded blip is an unbounded allocation in the
/// server, not merely a large row.
pub const MAX_BLIP_UNITS: usize = 256 * 1024;

/// Most separately-formatted runs a document may be split into.
///
/// Length alone does not bound a document's cost. Every run carries its own
/// attribute map, so a document within the length limit can still be made
/// arbitrarily large by alternating formatting character by character —
/// `MAX_BLIP_UNITS` runs, each holding an attribute map, is megabytes of JSON
/// for a message of a quarter-million characters. Bounding the runs as well as
/// the characters is what makes the pair of limits actually bound the memory.
///
/// Far above anything written by a person: a message split into sixteen
/// thousand differently-formatted pieces is not formatting, it is a payload.
pub const MAX_BLIP_RUNS: usize = 16 * 1024;

/// A blip's document plus the OT history needed to rebase concurrent edits.
pub struct LiveBlip {
    pub meta: Blip,
    pub doc: ServerDoc,
}

impl LiveBlip {
    /// Wrap a stored blip, seeding its OT history from the persisted snapshot.
    pub fn new(blip: Blip) -> Self {
        let doc = ServerDoc::from_snapshot(blip.content.clone(), blip.revision)
            .unwrap_or_else(|_| ServerDoc::new());
        LiveBlip { meta: blip, doc }
    }

    /// Copy the authoritative document back onto the metadata that gets persisted.
    fn sync(&mut self) {
        self.meta.content = self.doc.content().clone();
        self.meta.revision = self.doc.revision();
    }
}

/// Someone currently watching a wave.
pub struct Subscriber {
    pub user_id: UserId,
    pub tx: mpsc::Sender<ServerMessage>,
    /// Fires when this connection must be torn down. A client whose outbound
    /// queue overflowed has missed ops, so the only safe thing to do is close
    /// the socket and let it reconnect and resynchronise.
    pub kill: Arc<Notify>,
    /// Blip the subscriber's caret is in, used for the presence display.
    pub editing: Option<BlipId>,
}

/// A wave resident in memory, with its documents and watchers.
pub struct LiveWave {
    pub wave: Wave,
    pub wavelets: Vec<Wavelet>,
    pub blips: HashMap<BlipId, LiveBlip>,
    /// Comment threads, keyed by id. Their remarks are ordinary entries in
    /// `blips`, tagged with the thread they belong to.
    pub comments: HashMap<CommentId, CommentThread>,
    pub subscribers: HashMap<ConnId, Subscriber>,
    /// Public profiles of everyone who might appear in this wave, so rendering
    /// a view never has to go back to the database.
    pub user_cache: HashMap<UserId, PublicUser>,
    /// Set while holding this lock, immediately before the wave is removed from
    /// the residency map. A task that obtained the `Arc` before removal is
    /// blocked on this very lock, so it observes the flag and reloads instead of
    /// attaching itself to an orphaned copy.
    pub evicted: bool,
    /// Shared with `AppState`, so the places that drop a subscriber can say so.
    /// Those are the ones worth counting and the ones with no other voice.
    pub metrics: Arc<crate::metrics::Metrics>,
}

impl LiveWave {
    pub fn wavelet(&self, id: &WaveletId) -> Option<&Wavelet> {
        self.wavelets.iter().find(|w| &w.id == id)
    }

    pub fn wavelet_mut(&mut self, id: &WaveletId) -> Option<&mut Wavelet> {
        self.wavelets.iter_mut().find(|w| &w.id == id)
    }

    /// The wavelet every wave has.
    pub fn root(&self) -> Option<&Wavelet> {
        self.wavelets
            .iter()
            .find(|w| w.kind == WaveletKind::Conversation)
    }

    pub fn title(&self) -> String {
        self.root().map(|w| w.title.clone()).unwrap_or_default()
    }

    /// Is this action allowed here, by this user, in this wave's mode?
    ///
    /// Every content mutation goes through this one function rather than
    /// matching on the mode at each call site. That is deliberate: the rules are
    /// then in a single place that can be tested exhaustively, and a handler
    /// added later cannot quietly skip them.
    // The error *is* the reply sent to the client, which is the convention every
    // handler in this crate follows. Boxing it here to save stack would force an
    // unbox at each of the six call sites for no benefit.
    #[allow(clippy::result_large_err)]
    pub fn permit(&self, user: &UserId, action: Action) -> Result<(), ServerMessage> {
        let mode = self.wave.mode;
        let is_creator = self.wave.creator == *user;

        let allowed = match action {
            Action::NewMessage => mode.allows_new_message(is_creator),
            Action::Reply => mode.allows_replies(),
            Action::Edit { is_author } => mode.allows_edit(is_author),
            Action::Delete => mode.allows_delete(),
            Action::Retitle => mode.allows_retitle(is_creator),
            Action::PrivateReply => mode.allows_private_reply(),
            Action::Comment => mode.allows_comments(),
            Action::ResolveComment => mode.allows_resolve(),
            // Changing the rules is the creator's alone, in every mode —
            // including Frozen, or a wave could never be thawed.
            Action::SetMode => is_creator,
        };
        if allowed {
            return Ok(());
        }

        Err(ServerMessage::error(
            ErrorCode::Forbidden,
            match (mode, action) {
                (_, Action::SetMode) => {
                    "Only the person who started this wave can change its mode.".to_string()
                }
                (m, _) if m.is_frozen() => {
                    "This wave is frozen. Unfreeze it to make changes.".to_string()
                }
                // Every other writable mode has a reply or a composer, which is
                // why it has no comments: there is already somewhere to say this.
                (_, Action::Comment) => {
                    "Comments belong to a notepad. Here you can reply instead.".to_string()
                }
                (WaveMode::Chat, Action::Edit { .. }) => {
                    "In a chat you can only edit your own messages.".to_string()
                }
                (WaveMode::Announcement, Action::Edit { .. }) => {
                    "Only the author can edit this.".to_string()
                }
                (WaveMode::Announcement, Action::NewMessage) => {
                    "Only the person who started this wave can post here. You can reply."
                        .to_string()
                }
                (WaveMode::Chat, Action::Reply) => {
                    "This wave is a chat, so messages are not threaded.".to_string()
                }
                (WaveMode::Notepad, _) => {
                    "This wave is a single shared page — edit it directly.".to_string()
                }
                _ => format!("That is not allowed in {} mode.", mode.label()),
            },
        ))
    }

    /// The next ordering position for a new blip in this wavelet.
    ///
    /// Derived from resident state rather than a query, so it can be allocated
    /// while holding this wave's lock. Reading it from storage beforehand let
    /// two concurrent creates receive the same position, and blips that tie
    /// have no defined order.
    pub fn next_seq(&self, wavelet_id: &WaveletId) -> i64 {
        self.blips
            .values()
            .filter(|b| &b.meta.wavelet_id == wavelet_id)
            .map(|b| b.meta.seq)
            .max()
            .map_or(0, |max| max + 1)
    }

    /// Can `user` see this wavelet?
    pub fn may_access(&self, user: &UserId, wavelet_id: &WaveletId) -> bool {
        self.wavelet(wavelet_id)
            .is_some_and(|w| w.has_participant(user))
    }

    /// Every wavelet `user` can see.
    pub fn visible_wavelets(&self, user: &UserId) -> Vec<&Wavelet> {
        self.wavelets
            .iter()
            .filter(|w| w.has_participant(user))
            .collect()
    }

    /// Does `user` participate in this wave at all?
    pub fn may_view(&self, user: &UserId) -> bool {
        self.wavelets.iter().any(|w| w.has_participant(user))
    }

    /// Send to every subscriber who may see `wavelet_id`, optionally skipping one
    /// connection (used so an author gets an ack instead of an echo).
    ///
    /// A subscriber whose queue is full has already missed messages, so it is
    /// disconnected rather than merely unsubscribed. Silently dropping it would
    /// leave a client that still believes it is watching the wave, still able to
    /// submit, and never again acknowledged — its edits would accumulate locally
    /// and never be sent.
    pub fn broadcast(
        &mut self,
        wavelet_id: &WaveletId,
        skip: Option<ConnId>,
        message: ServerMessage,
    ) {
        let allowed: HashSet<UserId> = self
            .wavelet(wavelet_id)
            .map(|w| w.participants.iter().cloned().collect())
            .unwrap_or_default();

        let mut failed = Vec::new();
        for (conn_id, sub) in &self.subscribers {
            if Some(*conn_id) == skip || !allowed.contains(&sub.user_id) {
                continue;
            }
            if sub.tx.try_send(message.clone()).is_err() {
                failed.push(*conn_id);
            }
        }
        self.disconnect(&failed);
    }

    /// Drop these subscribers and tear their connections down.
    pub fn disconnect(&mut self, conn_ids: &[ConnId]) {
        for conn_id in conn_ids {
            if let Some(sub) = self.subscribers.remove(conn_id) {
                // Wakes the connection task, which closes the socket. The client
                // reconnects and re-opens, which is a full resynchronisation.
                sub.kill.notify_waiters();
                // Counted and logged because it is otherwise perfectly silent:
                // the user sees a reconnect, the operator sees nothing, and a
                // server that is quietly resynchronising everybody looks
                // healthy from outside.
                self.metrics
                    .ws_slow_client_disconnects
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(
                    conn = conn_id,
                    user = %sub.user_id,
                    wave = %self.wave.id,
                    "outbound queue overflowed; disconnecting so the client resynchronises"
                );
            }
        }
    }

    /// Send to one subscriber, disconnecting it if its queue has overflowed.
    ///
    /// Used for messages that are meaningless to lose — an `Ack` the author is
    /// waiting on, or the `WaveState` snapshot everything else is relative to.
    pub fn send_to(&mut self, conn_id: ConnId, message: ServerMessage) {
        let failed = match self.subscribers.get(&conn_id) {
            Some(sub) => sub.tx.try_send(message).is_err(),
            None => false,
        };
        if failed {
            self.disconnect(&[conn_id]);
        }
    }

    /// Build the view of this wave for one user, hiding wavelets they are not in.
    pub fn view(
        &self,
        user: &UserId,
        read_marks: &HashMap<BlipId, u64>,
        flags: WaveFlags,
    ) -> WaveView {
        let users = &self.user_cache;
        let wavelets = self
            .visible_wavelets(user)
            .into_iter()
            .map(|w| {
                let mut blips: Vec<&LiveBlip> = self
                    .blips
                    .values()
                    .filter(|b| b.meta.wavelet_id == w.id)
                    .collect();
                // Total order. `seq` alone is not enough: it is only unique per
                // wavelet by convention, and ties would otherwise resolve to
                // HashMap iteration order — different for every client, and
                // different again after a reload.
                blips.sort_by(|a, b| {
                    (a.meta.seq, a.meta.created_at, &a.meta.id).cmp(&(
                        b.meta.seq,
                        b.meta.created_at,
                        &b.meta.id,
                    ))
                });
                WaveletView {
                    id: w.id.clone(),
                    wave_id: w.wave_id.clone(),
                    kind: w.kind,
                    title: w.title.clone(),
                    participants: w
                        .participants
                        .iter()
                        .filter_map(|id| users.get(id).cloned())
                        .collect(),
                    anchor_blip: w.anchor_blip.clone(),
                    created_at: w.created_at,
                    last_modified: w.last_modified,
                    blips: blips
                        .into_iter()
                        .map(|b| blip_view(&b.meta, read_marks))
                        .collect(),
                    // Scoped by wavelet like everything else here: a thread is
                    // only ever visible to people who can read the blip it
                    // annotates, because that is the wavelet it belongs to.
                    comments: {
                        let mut threads: Vec<CommentThread> = self
                            .comments
                            .values()
                            .filter(|c| c.wavelet_id == w.id)
                            .cloned()
                            .collect();
                        threads.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
                        threads
                    },
                }
            })
            .collect();

        WaveView {
            id: self.wave.id.clone(),
            creator: self.wave.creator.clone(),
            created_at: self.wave.created_at,
            wavelets,
            flags,
            mode: self.wave.mode,
        }
    }

    /// Presence entries as `viewer` is allowed to see them.
    ///
    /// Scoped deliberately: an unscoped list leaks the *existence* of private
    /// replies, because `editing` would name a blip in a wavelet the viewer is
    /// not part of. Content stayed private but "who is talking privately, and
    /// when" did not, which contradicts the isolation this server promises.
    pub fn presence_for(&self, viewer: &UserId) -> Vec<PresenceEntry> {
        let visible: HashSet<&WaveletId> = self
            .visible_wavelets(viewer)
            .into_iter()
            .map(|w| &w.id)
            .collect();
        // Everyone the viewer shares at least one wavelet with.
        let co_participants: HashSet<&UserId> = self
            .visible_wavelets(viewer)
            .into_iter()
            .flat_map(|w| w.participants.iter())
            .collect();

        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        for sub in self.subscribers.values() {
            if !co_participants.contains(&sub.user_id) || !seen.insert(sub.user_id.clone()) {
                continue;
            }
            let Some(user) = self.user_cache.get(&sub.user_id) else {
                continue;
            };
            // Only reveal what they are editing if the viewer can see that blip.
            let editing = sub.editing.as_ref().filter(|blip_id| {
                self.blips
                    .get(*blip_id)
                    .is_some_and(|b| visible.contains(&b.meta.wavelet_id))
            });
            entries.push(PresenceEntry {
                user: user.clone(),
                editing: editing.cloned(),
            });
        }
        entries.sort_by(|a, b| a.user.display_name.cmp(&b.user.display_name));
        entries
    }
}

/// Something a participant is trying to do to a wave's content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    NewMessage,
    Reply,
    /// Editing a message, and whether the actor wrote it.
    Edit {
        is_author: bool,
    },
    Delete,
    Retitle,
    PrivateReply,
    /// Opening a comment thread on a range of text, or adding to one.
    Comment,
    /// Closing or reopening a thread that already exists.
    ResolveComment,
    SetMode,
}

/// One client's edit, as submitted.
pub struct OpSubmission {
    pub blip_id: BlipId,
    /// The revision the edit was written against.
    pub revision: u64,
    pub delta: Delta,
    /// Unique per submitted op, so a replay after a reconnect is recognisable.
    pub op_id: Option<String>,
}

/// Render a blip for the wire, resolving the viewer's unread state.
pub fn blip_view(blip: &Blip, read_marks: &HashMap<BlipId, u64>) -> BlipView {
    let unread = read_marks.get(&blip.id).copied().unwrap_or(0) < blip.revision;
    BlipView {
        id: blip.id.clone(),
        wavelet_id: blip.wavelet_id.clone(),
        parent: blip.parent.clone(),
        comment: blip.comment.clone(),
        seq: blip.seq,
        author: blip.author.clone(),
        contributors: blip.contributors.clone(),
        created_at: blip.created_at,
        last_modified: blip.last_modified,
        content: blip.content.clone(),
        revision: blip.revision,
        unread,
    }
}

/// A live connection's outbound handle.
#[derive(Clone)]
pub struct ConnHandle {
    pub user_id: UserId,
    pub tx: mpsc::Sender<ServerMessage>,
}

/// Bucket key for sign-in failures. Prefixed so it can never collide with the
/// address keys the other limiters use — `"10.0.0.1"` is a legal username shape
/// as far as a string map is concerned.
fn account_key(name: &str) -> String {
    format!("account:{}", name.trim().to_lowercase())
}

pub struct AppState {
    pub db: Storage,
    pub config: Config,
    pub metrics: Arc<crate::metrics::Metrics>,
    waves: DashMap<WaveId, Arc<Mutex<LiveWave>>>,
    conns: DashMap<ConnId, ConnHandle>,
    user_conns: DashMap<UserId, HashSet<ConnId>>,
    /// Waves with a pending debounced inbox notification.
    inbox_pending: DashMap<WaveId, ()>,
    /// Serialises loading and eviction so a wave can never be resident twice.
    /// Two copies would each accept ops and silently fork the document.
    residency: Mutex<()>,
    next_conn_id: AtomicU64,
    /// Login and registration. Tight, because each attempt costs an Argon2 hash.
    auth_limiter: RateLimiter,
    /// Username lookup, which is an existence oracle by nature.
    lookup_limiter: RateLimiter,
    /// Failed sign-ins, keyed by the *account* rather than the caller.
    ///
    /// `auth_limiter` is keyed by address, which throttles one attacker and
    /// does nothing about a thousand of them — or one with a list of addresses
    /// — all guessing at the same account. This is the other axis, and it is
    /// the one that matters for a targeted attempt.
    account_limiter: RateLimiter,
    /// Every WebSocket command, priced by what it costs the server. Keyed by
    /// *user* rather than by address or connection: the socket is authenticated,
    /// so the account is the thing to hold to an allowance, and opening more
    /// connections must not buy more of it.
    command_limiter: RateLimiter,
    /// Flipped once on shutdown. A watch rather than a `Notify` because a
    /// connection busy inside a command when the signal arrives must still see
    /// it afterwards; `notify_waiters` only wakes whoever is already waiting.
    shutdown: tokio::sync::watch::Sender<bool>,
    /// Makes the next dispatched command panic, so the containment around it can
    /// be tested with a real panic on a real connection rather than by trusting
    /// that it would work. There is no reachable panic to provoke on purpose,
    /// and a bug that produced one would be fixed rather than kept as a fixture.
    #[cfg(test)]
    pub panic_next_command: std::sync::atomic::AtomicBool,
}

impl AppState {
    pub fn new(db: Storage, config: Config) -> Arc<Self> {
        Arc::new(AppState {
            db,
            config,
            metrics: Arc::new(crate::metrics::Metrics::default()),
            waves: DashMap::new(),
            conns: DashMap::new(),
            user_conns: DashMap::new(),
            inbox_pending: DashMap::new(),
            residency: Mutex::new(()),
            next_conn_id: AtomicU64::new(1),
            // A person signing in mistypes a few times; nobody legitimately
            // makes ten attempts a second.
            auth_limiter: RateLimiter::new(10.0, 0.5),
            lookup_limiter: RateLimiter::new(30.0, 2.0),
            // Only *failures* are charged, so somebody signing in normally
            // never touches it however often they do it. Ten wrong guesses,
            // then one more every two minutes.
            account_limiter: RateLimiter::new(10.0, 1.0 / 120.0),
            // Sized so that ordinary use never reaches it and abuse does.
            // Typing is one op per acknowledgement per message, so a fast
            // typist with several messages open runs at tens a second; this
            // allows 200 a second sustained and a burst of six seconds' worth.
            // At those prices it is 3 playbacks a second, or 10 searches.
            command_limiter: RateLimiter::new(1200.0, 200.0),
            shutdown: tokio::sync::watch::channel(false).0,
            #[cfg(test)]
            panic_next_command: std::sync::atomic::AtomicBool::new(false),
        })
    }

    // --- rate limiting --------------------------------------------------

    /// `Some(response)` when the caller is over their allowance.
    pub fn check_auth_rate(
        &self,
        headers: &axum::http::HeaderMap,
        peer: Option<std::net::IpAddr>,
    ) -> Option<axum::response::Response> {
        self.check(
            &self.auth_limiter,
            headers,
            peer,
            "Too many attempts. Wait a moment and try again.",
            "auth",
        )
    }

    pub fn check_lookup_rate(
        &self,
        headers: &axum::http::HeaderMap,
        peer: Option<std::net::IpAddr>,
    ) -> Option<axum::response::Response> {
        self.check(
            &self.lookup_limiter,
            headers,
            peer,
            "Too many lookups. Slow down.",
            "lookup",
        )
    }

    fn check(
        &self,
        limiter: &RateLimiter,
        headers: &axum::http::HeaderMap,
        peer: Option<std::net::IpAddr>,
        message: &str,
        name: &'static str,
    ) -> Option<axum::response::Response> {
        use axum::response::IntoResponse;

        let forwarded = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok());
        // Only honour the forwarded header when the operator has said they are
        // behind a proxy; otherwise any client could spoof it to evade limits.
        let key = client_key(peer, forwarded, self.config.trust_forwarded_for);

        if limiter.check(&key) {
            return None;
        }
        // A limiter that is doing its job and a limiter that is refusing real
        // people look identical from inside the process.
        self.metrics.rate_limit(name);
        Some(
            (
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                [(axum::http::header::RETRY_AFTER, "10")],
                axum::Json(serde_json::json!({ "error": message })),
            )
                .into_response(),
        )
    }

    /// Is this account currently locked out by repeated failed sign-ins?
    ///
    /// Checked *before* the password is verified, so a locked account costs no
    /// Argon2 hash either.
    pub fn account_is_throttled(&self, name: &str) -> bool {
        !self.account_limiter.would_allow(&account_key(name))
    }

    /// Charge one wrong guess against an account.
    pub fn note_failed_signin(&self, name: &str) {
        self.account_limiter.check(&account_key(name));
        self.metrics.rate_limit("signin_failures");
    }

    /// Charge a command against its sender's allowance.
    ///
    /// `false` means refuse it. The whole WebSocket surface was unmetered:
    /// every limiter this server had was on an HTTP endpoint, while the socket
    /// carried twenty commands including the two that read the database
    /// hardest, and an authenticated client could call any of them as fast as
    /// it could write frames.
    pub fn check_command_rate(&self, user_id: &UserId, command: &ClientMessage) -> bool {
        if self
            .command_limiter
            .check_cost(user_id.as_str(), command.cost())
        {
            return true;
        }
        self.metrics.rate_limit("command");
        tracing::warn!(
            user = %user_id,
            command = command.name(),
            "over the command allowance"
        );
        false
    }

    // --- shutdown -------------------------------------------------------

    /// Watch that flips to `true` when the server is winding down.
    pub fn shutdown_signal(&self) -> tokio::sync::watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    /// Tell every connection to finish and close.
    pub fn begin_shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Wait for the open sockets to go away, up to `grace`.
    ///
    /// A WebSocket upgraded by axum runs in a task that `axum::serve` does not
    /// track, so its graceful shutdown returns while every socket is still
    /// live and the process then exits from under them mid-frame. Ops are
    /// idempotent and clients replay on reconnect, so the damage was bounded —
    /// but "bounded" is not the same as "none", and every deploy was a hard
    /// disconnect for everyone with no notice.
    ///
    /// Returns how many were still open when the wait ended.
    pub async fn drain_connections(&self, grace: std::time::Duration) -> usize {
        let deadline = std::time::Instant::now() + grace;
        loop {
            let open = self.conns.len();
            if open == 0 || std::time::Instant::now() >= deadline {
                return open;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    // --- connections ----------------------------------------------------

    /// How many sockets this user currently has open.
    pub fn connections_for(&self, user_id: &UserId) -> usize {
        self.user_conns.get(user_id).map(|s| s.len()).unwrap_or(0)
    }

    pub fn register_conn(&self, user_id: UserId, tx: mpsc::Sender<ServerMessage>) -> ConnId {
        let id = self.next_conn_id.fetch_add(1, Ordering::Relaxed);
        self.conns.insert(
            id,
            ConnHandle {
                user_id: user_id.clone(),
                tx,
            },
        );
        self.user_conns.entry(user_id).or_default().insert(id);
        id
    }

    pub fn unregister_conn(&self, conn_id: ConnId) {
        if let Some((_, handle)) = self.conns.remove(&conn_id) {
            if let Some(mut set) = self.user_conns.get_mut(&handle.user_id) {
                set.remove(&conn_id);
            }
            self.user_conns
                .remove_if(&handle.user_id, |_, set| set.is_empty());
        }
    }

    /// Deliver a message to every connection of a user.
    pub fn send_to_user(&self, user_id: &UserId, message: &ServerMessage) {
        let Some(conn_ids) = self.user_conns.get(user_id).map(|s| s.clone()) else {
            return;
        };
        for conn_id in conn_ids {
            if let Some(handle) = self.conns.get(&conn_id) {
                let _ = handle.tx.try_send(message.clone());
            }
        }
    }

    // --- wave residency -------------------------------------------------

    /// Get the resident copy of a wave, loading it from storage if needed.
    ///
    /// Guarded by the residency lock so concurrent openers share one instance.
    pub async fn open_wave(&self, wave_id: &WaveId) -> Result<Option<Arc<Mutex<LiveWave>>>> {
        if let Some(existing) = self.waves.get(wave_id) {
            return Ok(Some(existing.clone()));
        }
        let _guard = self.residency.lock().await;
        // Re-check: another task may have loaded it while we waited.
        if let Some(existing) = self.waves.get(wave_id) {
            return Ok(Some(existing.clone()));
        }

        let Some(wave) = self.db.wave(wave_id).await? else {
            return Ok(None);
        };
        let wavelets = self.db.wavelets_of_wave(wave_id).await?;
        let blips = self.db.blips_of_wave(wave_id).await?;
        let comments = self.db.comments_of_wave(wave_id).await?;
        let all_users = self.db.all_users().await?;

        let mut live = LiveWave {
            wave,
            wavelets,
            blips: blips
                .into_iter()
                .map(|b| (b.id.clone(), LiveBlip::new(b)))
                .collect(),
            comments: comments.into_iter().map(|c| (c.id.clone(), c)).collect(),
            subscribers: HashMap::new(),
            user_cache: HashMap::new(),
            evicted: false,
            metrics: self.metrics.clone(),
        };
        live.user_cache = all_users.into_iter().map(|u| (u.id.clone(), u)).collect();

        let arc = Arc::new(Mutex::new(live));
        self.waves.insert(wave_id.clone(), arc.clone());
        self.metrics.wave_loaded();
        Ok(Some(arc))
    }

    /// Drop a wave from memory once nobody is watching it.
    ///
    /// The tombstone is what makes this safe. `open_wave` hands out the `Arc`
    /// without taking the residency lock (it is on the hot path of every
    /// submit), so a task can be holding the `Arc` and waiting on the wave lock
    /// at the moment we decide to evict. Marking `evicted` while we still hold
    /// that lock guarantees such a task sees the flag and reloads, instead of
    /// attaching a subscriber to a copy that is about to be dropped.
    pub async fn maybe_evict(&self, wave_id: &WaveId) {
        let _guard = self.residency.lock().await;
        // Clone the Arc out of the map first: holding a DashMap guard across an
        // await is a deadlock waiting to happen.
        let Some(entry) = self.waves.get(wave_id).map(|e| e.clone()) else {
            return;
        };
        let mut live = entry.lock().await;
        if live.subscribers.is_empty() {
            live.evicted = true;
            self.waves.remove(wave_id);
            self.metrics.wave_evicted();
        }
    }

    /// Drop a wave from memory after a command panicked partway through it,
    /// disconnecting everyone watching so they reload from storage.
    ///
    /// A panic can land between mutating the resident document and writing the
    /// op, which is the state `apply_op`'s rollback exists to prevent: leave the
    /// wave resident and every later op transforms over one that exists nowhere
    /// else. Storage is the authority, so the repair is to throw the memory away
    /// and let it be read again.
    ///
    /// This is what aborting the process used to do, minus everyone else's
    /// waves. Unlike `maybe_evict` it does not wait for the wave to be idle —
    /// the subscribers are exactly who must not keep editing it — and it reuses
    /// the same eviction tombstone, so a task already holding the `Arc` and
    /// waiting on the lock observes the flag and reloads.
    pub async fn evict_after_panic(&self, wave_id: &WaveId) {
        let _guard = self.residency.lock().await;
        let Some(entry) = self.waves.get(wave_id).map(|e| e.clone()) else {
            return;
        };
        let mut live = entry.lock().await;
        let watching: Vec<ConnId> = live.subscribers.keys().copied().collect();
        // Closing the socket is what forces a resynchronisation, exactly as it
        // does for a client whose outbound queue overflowed.
        live.disconnect(&watching);
        live.evicted = true;
        self.waves.remove(wave_id);
        self.metrics.wave_evicted();
        self.metrics
            .waves_evicted_after_panic
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Refresh the cached profile of a user in every resident wave, so a newly
    /// registered account renders correctly in waves already in memory.
    pub async fn cache_user(&self, user: PublicUser) {
        let waves: Vec<_> = self.waves.iter().map(|e| e.value().clone()).collect();
        for wave in waves {
            wave.lock()
                .await
                .user_cache
                .insert(user.id.clone(), user.clone());
        }
    }

    // --- op application -------------------------------------------------

    /// Apply a client op to a blip and fan the result out.
    ///
    /// Returns the committed revision. The caller already holds the wave lock,
    /// which is what serialises concurrent submissions.
    pub async fn apply_op(
        self: &Arc<Self>,
        live: &mut LiveWave,
        conn_id: ConnId,
        author: &UserId,
        submission: OpSubmission,
    ) -> std::result::Result<(), ServerMessage> {
        let OpSubmission {
            blip_id,
            revision: client_revision,
            delta,
            op_id,
        } = submission;
        let blip_id = &blip_id;
        let Some(blip) = live.blips.get(blip_id) else {
            return Err(ServerMessage::error(ErrorCode::NotFound, "No such blip."));
        };
        let wavelet_id = blip.meta.wavelet_id.clone();
        if !live.may_access(author, &wavelet_id) {
            // Same response as a missing blip: distinguishing them would reveal
            // that a private reply exists.
            return Err(ServerMessage::error(ErrorCode::NotFound, "No such blip."));
        }

        // A reconnecting client replays work it never saw acknowledged. Without
        // this check the same edit is applied twice — and because SIGTERM closes
        // every socket, that happens on an ordinary deploy, not just a crash.
        if let Some(op_id) = op_id.as_deref() {
            match self.db.revision_for_op(blip_id, op_id).await {
                Ok(Some(revision)) => {
                    let content = live
                        .blips
                        .get(blip_id)
                        .map(|b| b.doc.content().clone())
                        .unwrap_or_default();
                    live.send_to(
                        conn_id,
                        ServerMessage::Ack {
                            wave_id: live.wave.id.clone(),
                            blip_id: blip_id.clone(),
                            revision,
                            delta: Delta::new(),
                            op_id: Some(op_id.to_string()),
                        },
                    );
                    let _ = content;
                    return Ok(());
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "op-id lookup failed; applying anyway"),
            }
        }

        // The mode check goes *after* the idempotency lookup, and the ordering is
        // load-bearing. A reconnecting client replays work it never saw
        // acknowledged; if the wave was frozen in the meantime and this ran
        // first, an op the server had already committed would be refused. The
        // client would never get its acknowledgement, its outstanding op would
        // never clear, and every later keystroke would pile up locally and never
        // be sent. Checking after the lookup only ever gates genuinely new work.
        let is_author = live
            .blips
            .get(blip_id)
            .is_some_and(|b| b.meta.author == *author);
        if let Err(mut refusal) = live.permit(author, Action::Edit { is_author }) {
            // Carry the blip id so the client knows which document to reset. A
            // bare refusal leaves it holding an op it will retry forever.
            if let ServerMessage::Error {
                blip_id: ref mut target,
                ..
            } = refusal
            {
                *target = Some(blip_id.clone());
            }
            return Err(refusal);
        }

        let blip = live.blips.get_mut(blip_id).expect("checked above");
        let committed = match blip.doc.apply(client_revision, &delta, author.as_str()) {
            Ok(rev) => rev,
            Err(OtError::RevisionTooOld { .. }) | Err(OtError::RevisionInFuture { .. }) => {
                return Err(ServerMessage::resync(
                    blip_id.clone(),
                    "Your edit was too far out of date; reloading this wave.",
                ));
            }
            Err(e) => {
                return Err(ServerMessage::resync(blip_id.clone(), e.to_string()));
            }
        };

        // Growth is checked here, on the *result*, and not on the submitted op.
        // The limits used to be applied only to a blip's initial content, so a
        // document could be grown without bound one edit at a time — which is
        // the way documents are actually written, and the only way that matters
        // for a limit whose stated purpose is bounding what the server holds in
        // memory. Checking the op instead would be inexact: it arrives written
        // against an older revision and is rebased before it lands.
        let content = blip.doc.content();
        let (units, runs) = (content.len(), content.ops.len());
        if units > MAX_BLIP_UNITS || runs > MAX_BLIP_RUNS {
            // Same rollback the persistence failure below uses: the op has
            // already been applied to the resident document, and leaving it
            // there would let later ops transform over an op the log never
            // receives.
            blip.doc.rollback_last();
            blip.sync();
            return Err(ServerMessage::resync(
                blip_id.clone(),
                "This message has reached its maximum size.",
            ));
        }

        blip.sync();
        blip.meta.record_contributor(author);
        let meta = blip.meta.clone();
        let wave_id = meta.wave_id.clone();

        // Durable before acknowledged. If the write fails the in-memory document
        // must go back: leaving it ahead of storage would mean later ops
        // transform over an op that exists nowhere else, and the next successful
        // commit would skip a revision, leaving a permanent hole in the op log
        // that corrupts playback from that point on.
        if let Err(e) = self
            .db
            .commit_op(
                meta.clone(),
                committed.delta.clone(),
                author.clone(),
                meta.last_modified,
                op_id.clone(),
            )
            .await
        {
            self.metrics
                .ops_persist_failures
                .fetch_add(1, Ordering::Relaxed);
            tracing::error!(error = %e, blip = %blip_id, "failed to persist op; rolling back");
            if let Some(blip) = live.blips.get_mut(blip_id) {
                blip.doc.rollback_last();
                blip.sync();
            }
            return Err(ServerMessage::resync(
                blip_id.clone(),
                "The server could not save your edit; reloading this wave.",
            ));
        }

        self.metrics.ops_applied.fetch_add(1, Ordering::Relaxed);

        if let Some(wavelet) = live.wavelet_mut(&wavelet_id) {
            wavelet.last_modified = meta.last_modified;
        }

        // The author gets an ack; everyone else gets the transformed op.
        live.broadcast(
            &wavelet_id,
            Some(conn_id),
            ServerMessage::Op {
                wave_id: wave_id.clone(),
                blip_id: blip_id.clone(),
                revision: committed.revision,
                author: author.clone(),
                delta: committed.delta.clone(),
            },
        );
        // An ack is never droppable: the author's client holds the op as
        // outstanding until it arrives, and stops sending anything further for
        // that blip. If the queue is full, disconnect so it resynchronises.
        live.send_to(
            conn_id,
            ServerMessage::Ack {
                wave_id: wave_id.clone(),
                blip_id: blip_id.clone(),
                revision: committed.revision,
                delta: committed.delta,
                op_id,
            },
        );

        self.schedule_inbox_update(&wave_id);
        Ok(())
    }

    // --- inbox notifications --------------------------------------------

    /// Queue a debounced inbox refresh for everyone in a wave.
    pub fn schedule_inbox_update(self: &Arc<Self>, wave_id: &WaveId) {
        if self.inbox_pending.insert(wave_id.clone(), ()).is_some() {
            return; // already queued
        }
        let state = self.clone();
        let wave_id = wave_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(INBOX_DEBOUNCE_MS)).await;
            state.inbox_pending.remove(&wave_id);
            state.push_inbox_update(&wave_id).await;
        });
    }

    /// Send a fresh inbox row for `wave_id` to every participant who is
    /// connected. Viewers get it too: their inbox list is on screen beside the
    /// wave they are reading.
    async fn push_inbox_update(&self, wave_id: &WaveId) {
        // Read the participant set from storage rather than via `open_wave`.
        // Going through residency here would re-load a wave that nobody is
        // watching, and nothing would ever evict it again — an unbounded leak,
        // since each resident wave pins every blip's history plus a copy of the
        // user directory.
        let participants = match self.db.wave_participants(wave_id).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "failed to list participants for inbox update");
                return;
            }
        };

        for user_id in participants {
            if !self.user_conns.contains_key(&user_id) {
                continue;
            }
            match self.db.wave_summary(&user_id, wave_id).await {
                Ok(Some(summary)) => {
                    self.send_to_user(&user_id, &ServerMessage::InboxUpdated { summary });
                }
                Ok(None) => {
                    self.send_to_user(
                        &user_id,
                        &ServerMessage::WaveRemoved {
                            wave_id: wave_id.clone(),
                        },
                    );
                }
                Err(e) => tracing::warn!(error = %e, "failed to build inbox summary"),
            }
        }
    }

    /// Read marks for a user in a wave, as a lookup table.
    pub async fn read_marks(&self, user_id: &UserId, wave_id: &WaveId) -> HashMap<BlipId, u64> {
        self.db
            .read_marks(user_id, wave_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect()
    }
}
