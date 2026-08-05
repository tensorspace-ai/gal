//! End-to-end tests driving a real server over real WebSockets.
//!
//! These exercise the contract the browser client depends on: registration,
//! cookie auth, the op protocol, access control, and — most importantly —
//! that two clients editing the same blip at the same time converge.
//!
//! The test client runs the same OT state machine as `web/client.js`, so a
//! protocol change that would break the browser breaks these tests too.

use gal_core::model::*;
use gal_core::protocol::*;
use gal_ot::{compose, transform, Delta};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use futures::{SinkExt, StreamExt};

use crate::config::Config;
use crate::db::Storage;
use crate::state::AppState;

/// A server bound to an ephemeral port, torn down when dropped.
struct TestServer {
    base: String,
    _dir: TempDir,
    handle: tokio::task::JoinHandle<()>,
    /// The same state the router is serving from, so a test can reach in for
    /// what is not expressible over the wire.
    state: std::sync::Arc<AppState>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn start_server() -> TestServer {
    start_server_with(|_| {}).await
}

async fn start_server_with(adjust: impl FnOnce(&mut Config)) -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open(dir.path().join("e2e.db")).unwrap();
    let mut config = Config {
        database: dir.path().join("e2e.db"),
        ..Config::default()
    };
    adjust(&mut config);
    let state = AppState::new(storage, config);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = crate::http::router(state.clone());

    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    TestServer {
        base: format!("http://{addr}"),
        _dir: dir,
        handle,
        state,
    }
}

/// A connected client that mirrors the browser's OT state machine.
struct TestClient {
    socket: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    user: PublicUser,
    /// Per-blip: document, revision, and the in-flight/queued ops.
    docs: std::collections::HashMap<BlipId, ClientDoc>,
    /// Every presence entry received, so tests can assert on what leaked.
    presence: Vec<PresenceEntry>,
    /// The inbox this connection was greeted with.
    inbox: Vec<WaveSummary>,
    op_counter: u64,
}

#[derive(Default)]
struct ClientDoc {
    doc: Delta,
    revision: u64,
    outstanding: Option<Delta>,
    buffer: Option<Delta>,
}

impl TestServer {
    /// Register an account and return its session cookie.
    async fn register(&self, name: &str) -> String {
        let http = reqwest::Client::new();
        let response = http
            .post(format!("{}/api/register", self.base))
            .json(&serde_json::json!({
                "name": name,
                "displayName": name,
                "password": "correct horse battery",
            }))
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "registration failed for {name}"
        );

        response
            .headers()
            .get("set-cookie")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .expect("no session cookie issued")
            .to_string()
    }

    async fn connect(&self, cookie: &str) -> TestClient {
        let mut client = self.connect_raw(cookie).await;

        // The first message is always the greeting.
        match client.recv().await {
            ServerMessage::Welcome { user, inbox } => {
                client.user = user;
                client.inbox = inbox;
            }
            other => panic!("expected welcome, got {other:?}"),
        }
        client
    }

    /// Open a socket without insisting on a greeting, for the cases where the
    /// server is expected to say no instead.
    async fn connect_raw(&self, cookie: &str) -> TestClient {
        let url = self.base.replace("http://", "ws://") + "/ws";
        let mut request = url.into_client_request().unwrap();
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());

        let (socket, _) = connect_async(request)
            .await
            .expect("websocket upgrade failed");
        TestClient {
            socket,
            user: PublicUser {
                id: UserId::from(""),
                name: String::new(),
                display_name: String::new(),
                color: 0,
            },
            docs: Default::default(),
            presence: Vec::new(),
            inbox: Vec::new(),
            op_counter: 0,
        }
    }
}

impl TestClient {
    async fn send(&mut self, message: ClientMessage) {
        let json = serde_json::to_string(&message).unwrap();
        self.socket.send(Message::Text(json.into())).await.unwrap();
    }

    /// Receive the next message, failing rather than hanging if none arrives.
    async fn recv(&mut self) -> ServerMessage {
        let next = tokio::time::timeout(std::time::Duration::from_secs(5), self.socket.next())
            .await
            .expect("timed out waiting for a server message")
            .expect("connection closed")
            .expect("websocket error");

        match next {
            Message::Text(text) => serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("undecodable message {text}: {e}")),
            other => panic!("unexpected frame {other:?}"),
        }
    }

    /// Receive until a message matches, folding op traffic into local state.
    async fn recv_until<T>(&mut self, mut f: impl FnMut(&ServerMessage) -> Option<T>) -> T {
        for _ in 0..80 {
            let message = self.recv().await;
            if let Some(queued) = self.absorb(&message) {
                self.send(queued).await;
            }
            if let Some(value) = f(&message) {
                return value;
            }
        }
        panic!("expected message never arrived");
    }

    /// Drain anything already buffered, without blocking for new traffic.
    async fn drain(&mut self) {
        while let Ok(Some(Ok(message))) =
            tokio::time::timeout(std::time::Duration::from_millis(120), self.socket.next()).await
        {
            if let Message::Text(text) = message {
                if let Ok(parsed) = serde_json::from_str::<ServerMessage>(&text) {
                    if let Some(queued) = self.absorb(&parsed) {
                        self.send(queued).await;
                    }
                }
            }
        }
    }

    /// Apply protocol messages to the local mirror, exactly as the browser does.
    ///
    /// Returns the op the caller must now send, if an acknowledgement released
    /// work that had queued up behind it.
    fn absorb(&mut self, message: &ServerMessage) -> Option<ClientMessage> {
        match message {
            ServerMessage::WaveState { wave } => {
                for wavelet in &wave.wavelets {
                    for blip in &wavelet.blips {
                        self.docs.insert(
                            blip.id.clone(),
                            ClientDoc {
                                doc: blip.content.clone(),
                                revision: blip.revision,
                                outstanding: None,
                                buffer: None,
                            },
                        );
                    }
                }
            }
            ServerMessage::CommentAdded { blip, .. } | ServerMessage::BlipAdded { blip, .. } => {
                self.docs
                    .entry(blip.id.clone())
                    .or_insert_with(|| ClientDoc {
                        doc: blip.content.clone(),
                        revision: blip.revision,
                        outstanding: None,
                        buffer: None,
                    });
            }
            // A newly visible wavelet — in practice a private reply we were
            // included in — arrives with its blips attached.
            ServerMessage::WaveletAdded { wavelet, .. } => {
                for blip in &wavelet.blips {
                    self.docs
                        .entry(blip.id.clone())
                        .or_insert_with(|| ClientDoc {
                            doc: blip.content.clone(),
                            revision: blip.revision,
                            outstanding: None,
                            buffer: None,
                        });
                }
            }
            ServerMessage::Op {
                blip_id,
                revision,
                delta,
                ..
            } => {
                let state = self.docs.get_mut(blip_id)?;
                state.revision = *revision;
                match state.outstanding.clone() {
                    None => state.doc = compose(&state.doc, delta),
                    Some(outstanding) => {
                        // Same rebasing rule the server uses, so both agree.
                        let new_outstanding = transform(delta, &outstanding, true);
                        let mut rebased = transform(&outstanding, delta, false);
                        let new_buffer = match state.buffer.clone() {
                            Some(buffer) => {
                                let b = transform(&rebased, &buffer, true);
                                rebased = transform(&buffer, &rebased, false);
                                Some(b)
                            }
                            None => None,
                        };
                        state.outstanding = Some(new_outstanding);
                        state.buffer = new_buffer;
                        state.doc = compose(&state.doc, &rebased);
                    }
                }
            }
            ServerMessage::Presence { users, .. } => {
                self.presence.extend(users.iter().cloned());
            }
            ServerMessage::Ack {
                blip_id, revision, ..
            } => {
                let released = self.docs.get_mut(blip_id).and_then(|state| {
                    state.revision = *revision;
                    state.outstanding = state.buffer.take();
                    state
                        .outstanding
                        .clone()
                        .map(|delta| (state.revision, delta))
                });
                // Whatever queued up behind the acknowledged op goes on the wire
                // now, exactly as `web/client.js` does. Merely promoting it to
                // `outstanding` leaves the work stranded: every later edit sees
                // something in flight and queues behind an op that is never
                // sent, so the client silently stops contributing.
                if let Some((revision, delta)) = released {
                    let op_id = self.next_op_id();
                    return Some(ClientMessage::Submit {
                        blip_id: blip_id.clone(),
                        revision,
                        delta,
                        op_id: Some(op_id),
                    });
                }
            }
            _ => {}
        }
        None
    }

    /// Make a local edit and send it if nothing is in flight.
    async fn edit(&mut self, blip_id: &BlipId, delta: Delta) {
        let (revision, to_send) = {
            let state = self.docs.get_mut(blip_id).expect("unknown blip");
            state.doc = compose(&state.doc, &delta);
            let to_send = if state.outstanding.is_none() {
                state.outstanding = Some(delta.clone());
                Some(delta)
            } else {
                state.buffer = Some(match state.buffer.take() {
                    Some(buffer) => compose(&buffer, &delta),
                    None => delta,
                });
                None
            };
            (state.revision, to_send)
        };
        if let Some(delta) = to_send {
            let op_id = self.next_op_id();
            self.send(ClientMessage::Submit {
                blip_id: blip_id.clone(),
                revision,
                delta,
                op_id: Some(op_id),
            })
            .await;
        }
    }

    /// A fresh op id, unique within this client.
    fn next_op_id(&mut self) -> String {
        self.op_counter += 1;
        format!("{}-{}", self.user.id, self.op_counter)
    }

    /// Did any presence frame we have seen name this blip as being edited?
    fn presence_mentions(&self, blip_id: &BlipId) -> bool {
        self.presence
            .iter()
            .any(|entry| entry.editing.as_ref() == Some(blip_id))
    }

    /// Is an op still in flight, or queued behind one, for this blip?
    fn in_flight(&self, blip_id: &BlipId) -> bool {
        self.docs
            .get(blip_id)
            .is_some_and(|d| d.outstanding.is_some() || d.buffer.is_some())
    }

    fn text(&self, blip_id: &BlipId) -> String {
        self.docs
            .get(blip_id)
            .map(|d| d.doc.to_plain_text())
            .unwrap_or_default()
    }

    /// Open a wave and return the snapshot itself.
    ///
    /// Distinct from [`open`](Self::open) because that one consumes the
    /// `WaveState` on the way past; a caller that wants to inspect the snapshot
    /// cannot then wait for a second one that will never come.
    async fn open_state(&mut self, wave_id: &WaveId) -> WaveView {
        self.send(ClientMessage::Open {
            wave_id: wave_id.clone(),
        })
        .await;
        self.recv_until(|m| match m {
            ServerMessage::WaveState { wave } => Some(wave.clone()),
            _ => None,
        })
        .await
    }

    /// Open a wave and return its first blip.
    async fn open(&mut self, wave_id: &WaveId) -> (WaveletId, BlipId) {
        self.send(ClientMessage::Open {
            wave_id: wave_id.clone(),
        })
        .await;
        self.recv_until(|m| match m {
            ServerMessage::WaveState { wave } => {
                let wavelet = wave.wavelets.first()?;
                Some((wavelet.id.clone(), wavelet.blips.first()?.id.clone()))
            }
            _ => None,
        })
        .await
    }
}

/// Create a wave owned by `client`, returning its ids.
async fn create_wave(
    client: &mut TestClient,
    title: &str,
    participants: Vec<String>,
) -> (WaveId, WaveletId, BlipId) {
    create_wave_in(client, title, participants, None).await
}

/// Create a wave in a specific mode.
async fn create_wave_in(
    client: &mut TestClient,
    title: &str,
    participants: Vec<String>,
    mode: Option<WaveMode>,
) -> (WaveId, WaveletId, BlipId) {
    client
        .send(ClientMessage::CreateWave {
            title: title.into(),
            participants,
            content: None,
            mode,
        })
        .await;

    client
        .recv_until(|m| match m {
            ServerMessage::WaveState { wave } => {
                let wavelet = wave.wavelets.first()?;
                Some((
                    wave.id.clone(),
                    wavelet.id.clone(),
                    wavelet.blips.first()?.id.clone(),
                ))
            }
            _ => None,
        })
        .await
}

/// Every range of `delta` carrying `comment_id` as its anchor attribute, as
/// `(start, length)` in UTF-16 code units.
///
/// This is the client's job in the browser, and doing it here is the point of
/// these tests: the anchor is *derived from the document*, so if OT moves the
/// text it must move the range too, with nothing else keeping them in step.
fn anchored_ranges(delta: &Delta, comment_id: &CommentId) -> Vec<(usize, usize)> {
    use gal_ot::{Insert, OpKind};

    let mut ranges = Vec::new();
    let mut index = 0usize;
    for op in &delta.ops {
        let len = match &op.kind {
            OpKind::Insert(Insert::Text(text)) => text.encode_utf16().count(),
            OpKind::Insert(Insert::Embed(_)) => 1,
            _ => continue,
        };
        let anchored = op
            .attributes
            .get(COMMENT_ATTRIBUTE)
            .and_then(|v| v.as_str())
            == Some(comment_id.as_str());
        if anchored {
            // Adjacent runs are one range: an edit inside a commented sentence
            // splits the op without splitting the comment.
            match ranges.last_mut() {
                Some((start, length)) if *start + *length == index => *length += len,
                _ => ranges.push((index, len)),
            }
        }
        index += len;
    }
    ranges
}

/// Anchor `comment_id` over `[start, start + length)` of a blip, and open the
/// thread. Mirrors what the browser does when you select text and comment on it.
async fn comment_on(
    client: &mut TestClient,
    wavelet_id: &WaveletId,
    blip_id: &BlipId,
    comment_id: &CommentId,
    start: usize,
    length: usize,
    text: &str,
) {
    // The thread first, so the anchor never names something that does not exist.
    client
        .send(ClientMessage::CreateComment {
            wavelet_id: wavelet_id.clone(),
            blip_id: blip_id.clone(),
            comment_id: comment_id.clone(),
            content: Some(Delta::document(text)),
        })
        .await;
    client
        .recv_until(|m| match m {
            ServerMessage::CommentAdded { comment, .. } if &comment.id == comment_id => Some(()),
            _ => None,
        })
        .await;

    let mut attrs = gal_ot::Attributes::new();
    attrs.insert(
        COMMENT_ATTRIBUTE.to_string(),
        serde_json::json!(comment_id.as_str()),
    );
    client
        .edit(
            blip_id,
            Delta::new().retain(start).retain_with(length, attrs),
        )
        .await;
}

// --- tests --------------------------------------------------------------

#[tokio::test]
async fn a_wave_can_be_created_and_read_by_both_participants() {
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let bob_cookie = server.register("bob").await;

    let mut alice = server.connect(&alice_cookie).await;
    let mut bob = server.connect(&bob_cookie).await;
    assert_eq!(alice.user.name, "alice");

    let (wave_id, _, blip_id) = create_wave(&mut alice, "Launch plan", vec!["bob".into()]).await;

    alice
        .edit(&blip_id, Delta::new().insert("Ship on Friday"))
        .await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;

    let (_, bob_blip) = bob.open(&wave_id).await;
    assert_eq!(bob_blip, blip_id);
    assert_eq!(bob.text(&blip_id), "Ship on Friday");
}

#[tokio::test]
async fn concurrent_edits_to_one_blip_converge() {
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let bob_cookie = server.register("bob").await;
    let mut alice = server.connect(&alice_cookie).await;
    let mut bob = server.connect(&bob_cookie).await;

    let (wave_id, _, blip_id) = create_wave(&mut alice, "Draft", vec!["bob".into()]).await;
    bob.open(&wave_id).await;

    // Seed some text and let both settle on it.
    alice
        .edit(&blip_id, Delta::new().insert("The quick brown fox"))
        .await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;
    bob.recv_until(|m| matches!(m, ServerMessage::Op { .. }).then_some(()))
        .await;
    assert_eq!(bob.text(&blip_id), "The quick brown fox");

    // Now both edit at the same revision, without seeing each other.
    alice
        .edit(&blip_id, Delta::new().retain(4).insert("very "))
        .await;
    bob.edit(&blip_id, Delta::new().retain(19).insert(" jumps"))
        .await;

    // Let every op land on both sides.
    for _ in 0..3 {
        alice.drain().await;
        bob.drain().await;
    }

    assert_eq!(
        alice.text(&blip_id),
        bob.text(&blip_id),
        "clients diverged after concurrent edits"
    );

    // A fresh client must see the same thing the others converged on.
    let mut carol = server.connect(&alice_cookie).await;
    carol.open(&wave_id).await;
    assert_eq!(
        carol.text(&blip_id),
        alice.text(&blip_id),
        "server disagrees with clients"
    );
    assert!(alice.text(&blip_id).contains("very"));
    assert!(alice.text(&blip_id).contains("jumps"));
}

#[tokio::test]
async fn rapid_interleaved_typing_converges() {
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let bob_cookie = server.register("bob").await;
    let mut alice = server.connect(&alice_cookie).await;
    let mut bob = server.connect(&bob_cookie).await;

    let (wave_id, _, blip_id) = create_wave(&mut alice, "Race", vec!["bob".into()]).await;
    bob.open(&wave_id).await;

    // Both type at the front repeatedly without waiting for acks, which is what
    // real typing looks like and what exercises the outstanding/buffer path.
    for i in 0..12 {
        alice
            .edit(&blip_id, Delta::new().insert(format!("a{i}")))
            .await;
        bob.edit(&blip_id, Delta::new().insert(format!("b{i}")))
            .await;
        alice.drain().await;
        bob.drain().await;
    }
    // Settle: a queued op only goes out once its predecessor is acknowledged,
    // so this takes as many rounds as there are queued ops. Wait for that to
    // finish rather than for a fixed number of rounds, which on a loaded runner
    // stopped early and read as a divergence.
    for _ in 0..40 {
        alice.drain().await;
        bob.drain().await;
        if !alice.in_flight(&blip_id)
            && !bob.in_flight(&blip_id)
            && alice.text(&blip_id) == bob.text(&blip_id)
        {
            break;
        }
    }

    let final_text = alice.text(&blip_id);
    assert_eq!(
        final_text,
        bob.text(&blip_id),
        "clients diverged under interleaved typing"
    );

    let mut fresh = server.connect(&bob_cookie).await;
    fresh.open(&wave_id).await;
    assert_eq!(
        fresh.text(&blip_id),
        final_text,
        "persisted state diverged from clients"
    );

    // Nothing was lost: every keystroke is present.
    for i in 0..12 {
        assert!(
            final_text.contains(&format!("a{i}")),
            "lost alice's edit {i}"
        );
        assert!(final_text.contains(&format!("b{i}")), "lost bob's edit {i}");
    }
}

#[tokio::test]
async fn edits_survive_a_reconnect() {
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;

    let (wave_id, _, blip_id) = create_wave(&mut alice, "Persistent", vec![]).await;
    alice
        .edit(&blip_id, Delta::new().insert("written before the drop"))
        .await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;
    drop(alice);

    // A brand new connection reads the persisted document.
    let mut again = server.connect(&cookie).await;
    again.open(&wave_id).await;
    assert_eq!(again.text(&blip_id), "written before the drop");
}

#[tokio::test]
async fn threaded_replies_are_delivered_live() {
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let bob_cookie = server.register("bob").await;
    let mut alice = server.connect(&alice_cookie).await;
    let mut bob = server.connect(&bob_cookie).await;

    let (wave_id, wavelet_id, root_blip) =
        create_wave(&mut alice, "Thread", vec!["bob".into()]).await;
    bob.open(&wave_id).await;

    alice
        .send(ClientMessage::CreateBlip {
            wavelet_id: wavelet_id.clone(),
            parent: Some(root_blip.clone()),
            content: Some(Delta::document("a reply")),
        })
        .await;

    let reply = bob
        .recv_until(|m| match m {
            ServerMessage::BlipAdded { blip, .. } => Some(blip.clone()),
            _ => None,
        })
        .await;

    assert_eq!(
        reply.parent,
        Some(root_blip),
        "reply should nest under its parent"
    );
    assert_eq!(reply.content.to_plain_text(), "a reply");
    assert_eq!(bob.text(&reply.id), "a reply");
}

#[tokio::test]
async fn a_private_reply_is_never_sent_to_outsiders() {
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let bob_cookie = server.register("bob").await;
    let carol_cookie = server.register("carol").await;

    let mut alice = server.connect(&alice_cookie).await;
    let mut bob = server.connect(&bob_cookie).await;
    let mut carol = server.connect(&carol_cookie).await;

    let (wave_id, wavelet_id, root_blip) =
        create_wave(&mut alice, "Team", vec!["bob".into(), "carol".into()]).await;
    bob.open(&wave_id).await;
    carol.open(&wave_id).await;

    // Alice branches a private thread with Bob only.
    alice
        .send(ClientMessage::PrivateReply {
            wavelet_id,
            anchor: root_blip,
            participants: vec!["bob".into()],
        })
        .await;

    let private = bob
        .recv_until(|m| match m {
            ServerMessage::WaveletAdded { wavelet, .. } => Some(wavelet.clone()),
            _ => None,
        })
        .await;
    assert_eq!(private.kind, WaveletKind::PrivateReply);

    let secret_blip = private.blips[0].id.clone();
    bob.edit(&secret_blip, Delta::new().insert("just between us"))
        .await;
    bob.recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;

    // Carol must not have received the wavelet, the blip, or the op.
    carol.drain().await;
    assert!(
        !carol.docs.contains_key(&secret_blip),
        "carol received a private reply she is not part of"
    );

    // And a fresh snapshot for Carol still excludes it.
    let mut carol2 = server.connect(&carol_cookie).await;
    carol2.open(&wave_id).await;
    assert!(
        !carol2.docs.contains_key(&secret_blip),
        "private reply leaked in a snapshot"
    );

    // Bob, who is a participant, sees it on reconnect.
    let mut bob2 = server.connect(&bob_cookie).await;
    bob2.open(&wave_id).await;
    assert_eq!(bob2.text(&secret_blip), "just between us");
}

/// Removing someone from the conversation has to remove them from the wave's
/// private replies too. It did not: `may_view` asks whether the user is in any
/// wavelet, so an evicted participant kept every side conversation they were in
/// — and the creator could not clean it up by hand, because removing someone
/// from a private reply requires being in that private reply.
#[tokio::test]
async fn removing_someone_from_a_wave_takes_their_private_replies_with_them() {
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let bob_cookie = server.register("bob").await;
    let carol_cookie = server.register("carol").await;

    let mut alice = server.connect(&alice_cookie).await;
    let mut bob = server.connect(&bob_cookie).await;
    let mut carol = server.connect(&carol_cookie).await;

    let (wave_id, wavelet_id, root_blip) =
        create_wave(&mut alice, "Team", vec!["bob".into(), "carol".into()]).await;
    bob.open(&wave_id).await;
    carol.open(&wave_id).await;

    // Bob and Carol have a side conversation Alice is not in — so Alice, the
    // creator, has no way to reach into it herself.
    bob.send(ClientMessage::PrivateReply {
        wavelet_id: wavelet_id.clone(),
        anchor: root_blip,
        participants: vec!["carol".into()],
    })
    .await;

    let private = carol
        .recv_until(|m| match m {
            ServerMessage::WaveletAdded { wavelet, .. } => Some(wavelet.clone()),
            _ => None,
        })
        .await;
    assert_eq!(private.kind, WaveletKind::PrivateReply);
    let secret_blip = private.blips[0].id.clone();
    carol
        .edit(&secret_blip, Delta::new().insert("side channel"))
        .await;
    carol
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;

    // Alice evicts Bob from the conversation.
    let bob_id = bob.user.id.clone();
    alice
        .send(ClientMessage::RemoveParticipant {
            wavelet_id: wavelet_id.clone(),
            user_id: bob_id.clone(),
        })
        .await;
    alice
        .recv_until(|m| {
            matches!(m, ServerMessage::ParticipantRemoved { user_id, .. } if user_id == &bob_id)
                .then_some(())
        })
        .await;

    // A fresh session is the honest test: it asks the server what Bob may see
    // now, rather than what his old connection happens to still hold.
    let mut bob2 = server.connect(&bob_cookie).await;
    bob2.send(ClientMessage::Open {
        wave_id: wave_id.clone(),
    })
    .await;
    let code = bob2
        .recv_until(|m| match m {
            ServerMessage::Error { code, .. } => Some(*code),
            ServerMessage::WaveState { .. } => {
                panic!("a removed participant still opened the wave through his private reply")
            }
            _ => None,
        })
        .await;
    assert_eq!(code, ErrorCode::NotFound);

    // Carol, who was not removed, keeps the thread and its content.
    let mut carol2 = server.connect(&carol_cookie).await;
    carol2.open(&wave_id).await;
    assert_eq!(
        carol2.text(&secret_blip),
        "side channel",
        "the remaining participant lost the private reply"
    );
}

/// A panic used to abort the process, so one bad edit in one wave disconnected
/// everybody on the server and lost whatever they had not yet sent. It is now
/// contained to the connection that caused it.
#[tokio::test]
async fn a_panicking_command_takes_down_one_connection_and_no_more() {
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let bob_cookie = server.register("bob").await;

    let mut alice = server.connect(&alice_cookie).await;
    let mut bob = server.connect(&bob_cookie).await;

    let (wave_id, _, root_blip) = create_wave(&mut alice, "Team", vec!["bob".into()]).await;
    bob.open(&wave_id).await;

    alice.edit(&root_blip, Delta::new().insert("before")).await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;

    // Alice's next command panics partway through.
    server
        .state
        .panic_next_command
        .store(true, std::sync::atomic::Ordering::SeqCst);
    alice.send(ClientMessage::Ping).await;

    // The server is still serving: a brand new connection works, and the wave
    // reloads from storage with the edit that was committed before the panic.
    //
    // Bob writes the follow-up rather than a second Alice connection. Op ids
    // here are `{user}-{counter}` and each client counts from zero, so two
    // connections for one user reissue each other's ids — the server correctly
    // treats the second as the replay it is indistinguishable from, acks it,
    // and applies nothing.
    let mut bob2 = server.connect(&bob_cookie).await;
    bob2.open(&wave_id).await;
    assert_eq!(
        bob2.text(&root_blip),
        "before",
        "the wave did not come back from storage after the panic"
    );

    // And the wave still works — the eviction did not leave a wreck behind.
    bob2.edit(&root_blip, Delta::new().retain(6).insert(" and after"))
        .await;
    bob2.recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;

    let mut alice2 = server.connect(&alice_cookie).await;
    alice2.open(&wave_id).await;
    assert_eq!(alice2.text(&root_blip), "before and after");
}

#[tokio::test]
async fn metrics_are_off_until_a_token_is_configured() {
    let server = start_server().await;
    let http = reqwest::Client::new();

    // Not "401", which would confirm the endpoint exists on a server whose
    // operator has not opted into publishing anything about its traffic.
    let response = http
        .get(format!("{}/metrics", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn metrics_need_the_token_and_then_report_real_activity() {
    let server = start_server_with(|config| {
        config.metrics_token = Some("a-token-of-sufficient-length".into());
    })
    .await;
    let http = reqwest::Client::new();

    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;
    let (_, _, blip) = create_wave(&mut alice, "Instrumented", vec![]).await;
    alice.edit(&blip, Delta::new().insert("counted")).await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;

    let unauthorised = http
        .get(format!("{}/metrics", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorised.status(), 401);

    let wrong = http
        .get(format!("{}/metrics", server.base))
        .header("authorization", "Bearer not-the-token-but-long-enough")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);

    let response = http
        .get(format!("{}/metrics", server.base))
        .header("authorization", "Bearer a-token-of-sufficient-length")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();

    // The numbers have to come from what actually happened, or this is a test
    // that the string constants are spelled right.
    assert!(
        body.contains("gal_ws_commands_total{command=\"submit\"} 1"),
        "the submitted op was not counted:\n{body}"
    );
    assert!(
        body.contains("gal_ws_commands_total{command=\"createWave\"} 1"),
        "wave creation was not counted:\n{body}"
    );
    assert!(
        body.contains("gal_ops_applied_total 1"),
        "the applied op was not counted:\n{body}"
    );
    assert!(
        body.contains("gal_ws_connections_active 1"),
        "the open connection was not counted:\n{body}"
    );
    assert!(
        body.contains("gal_waves_resident 1"),
        "the resident wave was not counted:\n{body}"
    );

    // A counter nothing ever increments reports zero for ever and reads as
    // "this never happens". Refuse an op and watch it move.
    assert!(body.contains("gal_ops_refused_total 0"));
    alice
        .send(ClientMessage::Submit {
            blip_id: blip.clone(),
            revision: 9_000,
            delta: Delta::new().insert("written against a revision that never was"),
            op_id: Some("alice-refused".into()),
        })
        .await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Error { .. }).then_some(()))
        .await;

    let after = http
        .get(format!("{}/metrics", server.base))
        .header("authorization", "Bearer a-token-of-sufficient-length")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        after.contains("gal_ops_refused_total 1"),
        "a refused op was not counted:\n{after}"
    );
}

#[tokio::test]
async fn every_response_carries_a_request_id() {
    let server = start_server().await;
    let http = reqwest::Client::new();

    let fresh = http
        .get(format!("{}/healthz", server.base))
        .send()
        .await
        .unwrap();
    assert!(fresh.headers().contains_key("x-request-id"));

    // A proxy in front of us has already given the request an id; adopt it, so
    // one request reads as one request across every hop that logged it.
    let passed_through = http
        .get(format!("{}/healthz", server.base))
        .header("x-request-id", "from-the-proxy")
        .send()
        .await
        .unwrap();
    assert_eq!(
        passed_through.headers().get("x-request-id").unwrap(),
        "from-the-proxy"
    );
}

/// The expensive commands are the point of this: playback reads the whole op
/// log for a wave, and it was callable as fast as a client could write frames.
#[tokio::test]
async fn a_flood_of_expensive_commands_is_refused() {
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;
    let (wave_id, _, _) = create_wave(&mut alice, "Replayed", vec![]).await;

    // The allowance is 1200 with playback at 60, so this is comfortably past it
    // even accounting for what creating the wave already spent.
    for _ in 0..40 {
        alice
            .send(ClientMessage::RequestPlayback {
                wave_id: wave_id.clone(),
            })
            .await;
    }

    let code = alice
        .recv_until(|m| match m {
            ServerMessage::Error { code, .. } => Some(*code),
            _ => None,
        })
        .await;
    assert_eq!(
        code,
        ErrorCode::TooManyRequests,
        "playback should be refused once the allowance is gone"
    );
}

/// The limit is only worth having if it never touches anyone real. Typing is
/// one op per acknowledgement, so this is what a fast typist looks like.
#[tokio::test]
async fn ordinary_typing_never_reaches_the_allowance() {
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;
    let (_, _, blip) = create_wave(&mut alice, "Typed", vec![]).await;

    for i in 0..300 {
        alice.edit(&blip, Delta::new().retain(i).insert("x")).await;
        alice
            .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
            .await;
    }

    assert_eq!(
        alice.text(&blip),
        "x".repeat(300),
        "an edit was refused that should not have been"
    );
}

/// axum's graceful shutdown covers HTTP requests and not WebSockets: an
/// upgraded socket runs in a task it spawned and stopped tracking. So every
/// deploy exited the process with every socket still live and mid-frame.
#[tokio::test]
async fn shutting_down_closes_the_sockets_instead_of_dropping_them() {
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;
    let (wave_id, _, blip) = create_wave(&mut alice, "Deployed", vec![]).await;

    alice
        .edit(&blip, Delta::new().insert("committed before the deploy"))
        .await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;
    assert_eq!(server.state.connections_for(&alice.user.id), 1);

    server.state.begin_shutdown();

    // The socket goes away on its own, without the process having to exit
    // underneath it.
    let left_open = server
        .state
        .drain_connections(std::time::Duration::from_secs(5))
        .await;
    assert_eq!(
        left_open, 0,
        "a socket was still open after the grace period"
    );

    // And the edit that was acknowledged before the shutdown is still there.
    let mut after = server.connect(&cookie).await;
    after.open(&wave_id).await;
    assert_eq!(after.text(&blip), "committed before the deploy");
}

#[tokio::test]
async fn one_account_cannot_hold_unlimited_sockets() {
    let server = start_server().await;
    let cookie = server.register("alice").await;

    // Held in a vec: dropping them would close the sockets and free the slots.
    let mut held = Vec::new();
    for _ in 0..24 {
        held.push(server.connect(&cookie).await);
    }

    let mut refused = server.connect_raw(&cookie).await;
    let code = refused
        .recv_until(|m| match m {
            ServerMessage::Error { code, .. } => Some(*code),
            ServerMessage::Welcome { .. } => panic!("the 25th connection was greeted"),
            _ => None,
        })
        .await;
    assert_eq!(code, ErrorCode::TooManyRequests);
}

/// The address limiter throttles one attacker and does nothing about many of
/// them, or one with a list of addresses, all guessing at the same account.
#[tokio::test]
async fn repeated_wrong_guesses_lock_the_account_not_just_the_address() {
    // Trusting the forwarded header is what lets this test present itself as a
    // different client each time, which is the situation being defended against.
    let server = start_server_with(|config| config.trust_forwarded_for = true).await;
    server.register("alice").await;
    let http = reqwest::Client::new();

    let attempt = |password: &'static str, from: &'static str| {
        let http = http.clone();
        let base = server.base.clone();
        async move {
            http.post(format!("{base}/api/login"))
                // A different source address each time, which is what defeats
                // the per-address limiter. Honoured only because this server is
                // configured to trust the header.
                .header("x-forwarded-for", from)
                .json(&serde_json::json!({ "name": "alice", "password": password }))
                .send()
                .await
                .unwrap()
                .status()
        }
    };

    for i in 0..10 {
        let from: &'static str = Box::leak(format!("10.0.0.{i}").into_boxed_str());
        assert_eq!(attempt("wrong guess here", from).await, 400);
    }

    // Now the right password, from an address that has never been seen.
    assert_eq!(
        attempt("correct horse battery", "203.0.113.7").await,
        400,
        "the account should be locked regardless of who is asking"
    );
}

/// Signing out of a device you no longer have used to mean changing your
/// password, which is a strange thing to have to do about a lost laptop.
#[tokio::test]
async fn sessions_can_be_revoked_without_changing_the_password() {
    let server = start_server().await;
    let first = server.register("alice").await;
    let http = reqwest::Client::new();

    // A second and third sign-in: other devices.
    let sign_in = || async {
        http.post(format!("{}/api/login", server.base))
            .json(&serde_json::json!({ "name": "alice", "password": "correct horse battery" }))
            .send()
            .await
            .unwrap()
            .headers()
            .get("set-cookie")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .expect("no session cookie")
            .to_string()
    };
    let second = sign_in().await;
    let third = sign_in().await;

    let count = |cookie: String| {
        let http = http.clone();
        let base = server.base.clone();
        async move {
            http.get(format!("{base}/api/sessions"))
                .header("cookie", cookie)
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()["sessions"]
                .as_u64()
                .unwrap()
        }
    };
    assert_eq!(count(first.clone()).await, 3);

    let revoked = http
        .post(format!("{}/api/sessions/revoke", server.base))
        .header("cookie", first.clone())
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["revoked"]
        .as_u64()
        .unwrap();
    assert_eq!(revoked, 2);

    // The one that asked survives; the others are gone.
    assert_eq!(count(first).await, 1);
    for gone in [second, third] {
        let status = http
            .get(format!("{}/api/me", server.base))
            .header("cookie", gone)
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, 401, "a revoked session still worked");
    }
}

#[tokio::test]
async fn a_non_participant_cannot_open_a_wave() {
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let mallory_cookie = server.register("mallory").await;

    let mut alice = server.connect(&alice_cookie).await;
    let (wave_id, _, _) = create_wave(&mut alice, "Private", vec![]).await;

    let mut mallory = server.connect(&mallory_cookie).await;
    mallory.send(ClientMessage::Open { wave_id }).await;

    let code = mallory
        .recv_until(|m| match m {
            ServerMessage::Error { code, .. } => Some(*code),
            ServerMessage::WaveState { .. } => panic!("outsider received wave state"),
            _ => None,
        })
        .await;
    assert_eq!(code, ErrorCode::NotFound);
}

#[tokio::test]
async fn the_websocket_requires_authentication() {
    let server = start_server().await;
    let url = server.base.replace("http://", "ws://") + "/ws";
    let request = url.into_client_request().unwrap();
    assert!(
        connect_async(request).await.is_err(),
        "an unauthenticated socket must be rejected"
    );
}

#[tokio::test]
async fn adding_a_participant_grants_access_and_updates_their_inbox() {
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let bob_cookie = server.register("bob").await;
    let mut alice = server.connect(&alice_cookie).await;
    let mut bob = server.connect(&bob_cookie).await;

    let (wave_id, wavelet_id, blip_id) = create_wave(&mut alice, "Later", vec![]).await;
    alice.edit(&blip_id, Delta::new().insert("come join")).await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;

    // Bob cannot see it yet.
    bob.send(ClientMessage::Open {
        wave_id: wave_id.clone(),
    })
    .await;
    bob.recv_until(|m| matches!(m, ServerMessage::Error { .. }).then_some(()))
        .await;

    alice
        .send(ClientMessage::AddParticipant {
            wavelet_id,
            name: "bob".into(),
        })
        .await;

    // The debounced inbox push tells Bob about the wave.
    let summary = bob
        .recv_until(|m| match m {
            ServerMessage::InboxUpdated { summary } => Some(summary.clone()),
            _ => None,
        })
        .await;
    assert_eq!(summary.id, wave_id);
    assert_eq!(summary.title, "Later");

    bob.open(&wave_id).await;
    assert_eq!(bob.text(&blip_id), "come join");
}

#[tokio::test]
async fn search_finds_a_wave_by_its_contents() {
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;

    let (wave_id, _, blip_id) = create_wave(&mut alice, "Notes", vec![]).await;
    alice
        .edit(&blip_id, Delta::new().insert("the deployment checklist"))
        .await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;

    alice
        .send(ClientMessage::Search {
            query: "deployment".into(),
        })
        .await;
    let hits = alice
        .recv_until(|m| match m {
            ServerMessage::SearchResults { hits, .. } => Some(hits.clone()),
            _ => None,
        })
        .await;

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].wave_id, wave_id);
    assert!(
        hits[0].snippet.contains("deployment"),
        "got {:?}",
        hits[0].snippet
    );
}

#[tokio::test]
async fn playback_replays_the_whole_history() {
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;

    let (wave_id, _, blip_id) = create_wave(&mut alice, "History", vec![]).await;
    for text in ["Hello", " world", "!"] {
        let at = alice.docs[&blip_id].doc.len();
        alice
            .edit(&blip_id, Delta::new().retain(at).insert(text))
            .await;
        alice
            .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
            .await;
    }

    alice.send(ClientMessage::RequestPlayback { wave_id }).await;
    let frames = alice
        .recv_until(|m| match m {
            ServerMessage::Playback { frames, .. } => Some(frames.clone()),
            _ => None,
        })
        .await;

    assert_eq!(frames.len(), 3);
    // Replaying the log rebuilds the document at each point in time.
    let deltas: Vec<Delta> = frames.iter().map(|f| f.delta.clone()).collect();
    assert_eq!(gal_ot::replay(&deltas, 1).to_plain_text(), "Hello");
    assert_eq!(gal_ot::replay(&deltas, 2).to_plain_text(), "Hello world");
    assert_eq!(gal_ot::replay(&deltas, 3).to_plain_text(), "Hello world!");
}

#[tokio::test]
async fn the_greeting_carries_an_inbox_with_unread_counts() {
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let bob_cookie = server.register("bob").await;
    let mut alice = server.connect(&alice_cookie).await;

    let (wave_id, _, blip_id) = create_wave(&mut alice, "Unread", vec!["bob".into()]).await;
    alice.edit(&blip_id, Delta::new().insert("first")).await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;

    // A fresh connection greets with the inbox.
    let url = server.base.replace("http://", "ws://") + "/ws";
    let mut request = url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert("cookie", bob_cookie.parse().unwrap());
    let (mut socket, _) = connect_async(request).await.unwrap();

    let message = socket.next().await.unwrap().unwrap();
    let Message::Text(text) = message else {
        panic!("expected text")
    };
    let parsed: ServerMessage = serde_json::from_str(&text).unwrap();

    let ServerMessage::Welcome { inbox, .. } = parsed else {
        panic!("expected welcome")
    };
    let row = inbox
        .iter()
        .find(|r| r.id == wave_id)
        .expect("wave missing from inbox");
    assert_eq!(row.title, "Unread");
    assert_eq!(row.snippet, "first");
    assert_eq!(row.unread_count, 1, "bob has not read it yet");
    assert_eq!(row.blip_count, 1);
}

/// Switch a wave's mode and wait for the change to land.
async fn set_mode(client: &mut TestClient, wave_id: &WaveId, mode: WaveMode) {
    client
        .send(ClientMessage::SetMode {
            wave_id: wave_id.clone(),
            mode,
        })
        .await;
    client
        .recv_until(|m| matches!(m, ServerMessage::ModeChanged { .. }).then_some(()))
        .await;
}

/// The code an attempted action came back with, or None if it was allowed.
async fn refusal(client: &mut TestClient, message: ClientMessage) -> Option<ErrorCode> {
    client.send(message).await;
    client
        .recv_until(|m| match m {
            ServerMessage::Error { code, .. } => Some(Some(*code)),
            // Any of these means the action went through.
            ServerMessage::BlipAdded { .. }
            | ServerMessage::Ack { .. }
            | ServerMessage::TitleChanged { .. }
            | ServerMessage::BlipRemoved { .. }
            | ServerMessage::WaveletAdded { .. } => Some(None),
            _ => None,
        })
        .await
}

#[tokio::test]
async fn chat_mode_refuses_edits_to_someone_elses_message() {
    // Sent straight over the socket, bypassing the UI entirely: the mode must be
    // a server rule, not a hidden button.
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let bob_cookie = server.register("bob").await;
    let mut alice = server.connect(&alice_cookie).await;
    let mut bob = server.connect(&bob_cookie).await;

    let (wave_id, wavelet_id, blip_id) =
        create_wave(&mut alice, "Channel", vec!["bob".into()]).await;
    alice
        .edit(&blip_id, Delta::new().insert("alice wrote this"))
        .await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;
    bob.open(&wave_id).await;

    // In Document mode Bob may edit Alice's message — that is the default.
    bob.edit(&blip_id, Delta::new().retain(5).insert("EDIT"))
        .await;
    bob.recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;

    set_mode(&mut alice, &wave_id, WaveMode::Chat).await;
    bob.drain().await;

    let revision = bob.docs[&blip_id].revision;
    let code = refusal(
        &mut bob,
        ClientMessage::Submit {
            blip_id: blip_id.clone(),
            revision,
            delta: Delta::new().insert("bob again"),
            op_id: Some("bob-op".into()),
        },
    )
    .await;
    assert_eq!(
        code,
        Some(ErrorCode::Forbidden),
        "chat must protect others' messages"
    );

    // But Bob may still write his own message.
    bob.send(ClientMessage::CreateBlip {
        wavelet_id,
        parent: None,
        content: Some(Delta::document("bob's own")),
    })
    .await;
    let own = bob
        .recv_until(|m| match m {
            ServerMessage::BlipAdded { blip, .. } => Some(blip.id.clone()),
            _ => None,
        })
        .await;
    bob.edit(&own, Delta::new().retain(9).insert("!")).await;
    bob.recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;
}

#[tokio::test]
async fn chat_mode_refuses_threaded_replies() {
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;
    let (wave_id, wavelet_id, blip_id) = create_wave(&mut alice, "Channel", vec![]).await;
    set_mode(&mut alice, &wave_id, WaveMode::Chat).await;

    let code = refusal(
        &mut alice,
        ClientMessage::CreateBlip {
            wavelet_id: wavelet_id.clone(),
            parent: Some(blip_id),
            content: None,
        },
    )
    .await;
    assert_eq!(code, Some(ErrorCode::Forbidden), "chat is flat");

    // A top-level message is still fine.
    let code = refusal(
        &mut alice,
        ClientMessage::CreateBlip {
            wavelet_id,
            parent: None,
            content: Some(Delta::document("hello")),
        },
    )
    .await;
    assert_eq!(code, None);
}

#[tokio::test]
async fn frozen_mode_refuses_every_content_change() {
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let bob_cookie = server.register("bob").await;
    let mut alice = server.connect(&alice_cookie).await;
    let mut bob = server.connect(&bob_cookie).await;

    let (wave_id, wavelet_id, blip_id) =
        create_wave(&mut alice, "Decided", vec!["bob".into()]).await;
    alice.edit(&blip_id, Delta::new().insert("final")).await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;
    bob.open(&wave_id).await;
    set_mode(&mut alice, &wave_id, WaveMode::Frozen).await;
    bob.drain().await;
    let frozen_revision = bob.docs[&blip_id].revision;

    // Every mutation, from a participant who is not the creator and from the
    // creator alike.
    for (label, message) in [
        (
            "edit",
            ClientMessage::Submit {
                blip_id: blip_id.clone(),
                revision: frozen_revision,
                delta: Delta::new().insert("x"),
                op_id: Some("frozen-op".into()),
            },
        ),
        (
            "new message",
            ClientMessage::CreateBlip {
                wavelet_id: wavelet_id.clone(),
                parent: None,
                content: None,
            },
        ),
        (
            "retitle",
            ClientMessage::SetTitle {
                wavelet_id: wavelet_id.clone(),
                title: "changed".into(),
            },
        ),
        (
            "private reply",
            ClientMessage::PrivateReply {
                wavelet_id: wavelet_id.clone(),
                anchor: blip_id.clone(),
                participants: vec![],
            },
        ),
    ] {
        let code = refusal(&mut bob, message).await;
        assert_eq!(
            code,
            Some(ErrorCode::Forbidden),
            "frozen should refuse: {label}"
        );
    }

    // Even the creator cannot edit a frozen wave — but can unfreeze it.
    let code = refusal(
        &mut alice,
        ClientMessage::SetTitle {
            wavelet_id,
            title: "creator".into(),
        },
    )
    .await;
    assert_eq!(code, Some(ErrorCode::Forbidden));

    set_mode(&mut alice, &wave_id, WaveMode::Document).await;
    alice.drain().await;
    alice
        .edit(&blip_id, Delta::new().retain(5).insert(" again"))
        .await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;
    assert!(
        alice.text(&blip_id).contains("again"),
        "unfreezing must restore editing"
    );
}

#[tokio::test]
async fn every_content_command_is_refused_by_a_frozen_wave() {
    // A catch-all for the handler somebody adds in six months. Every command
    // that can change content must be refused; anything that only reads, or is
    // per-user, is listed explicitly so adding to that list is a deliberate act.
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;
    let (wave_id, wavelet_id, blip_id) = create_wave(&mut alice, "Frozen", vec![]).await;
    set_mode(&mut alice, &wave_id, WaveMode::Frozen).await;

    let mutations = vec![
        ClientMessage::Submit {
            blip_id: blip_id.clone(),
            revision: 0,
            delta: Delta::new().insert("x"),
            op_id: Some("guard".into()),
        },
        ClientMessage::CreateBlip {
            wavelet_id: wavelet_id.clone(),
            parent: None,
            content: None,
        },
        ClientMessage::CreateBlip {
            wavelet_id: wavelet_id.clone(),
            parent: Some(blip_id.clone()),
            content: None,
        },
        ClientMessage::DeleteBlip {
            blip_id: blip_id.clone(),
        },
        ClientMessage::SetTitle {
            wavelet_id: wavelet_id.clone(),
            title: "no".into(),
        },
        ClientMessage::PrivateReply {
            wavelet_id: wavelet_id.clone(),
            anchor: blip_id.clone(),
            participants: vec![],
        },
    ];
    for message in mutations {
        let label = format!("{message:?}");
        let code = refusal(&mut alice, message).await;
        assert_eq!(
            code,
            Some(ErrorCode::Forbidden),
            "a frozen wave accepted a content change: {label}"
        );
    }

    // Deliberately still allowed while frozen: reading, per-user state, and
    // membership. Freezing stops content changing; it does not lock people out.
    for message in [
        ClientMessage::MarkRead {
            wave_id: wave_id.clone(),
        },
        ClientMessage::RequestPlayback {
            wave_id: wave_id.clone(),
        },
        ClientMessage::Search {
            query: "anything".into(),
        },
    ] {
        alice.send(message).await;
        alice.drain().await;
    }
    // Still reachable and unchanged afterwards.
    let mut fresh = server.connect(&cookie).await;
    fresh.open(&wave_id).await;
}

#[tokio::test]
async fn only_the_creator_can_change_the_mode() {
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let bob_cookie = server.register("bob").await;
    let mut alice = server.connect(&alice_cookie).await;
    let mut bob = server.connect(&bob_cookie).await;

    let (wave_id, _, _) = create_wave(&mut alice, "Mine", vec!["bob".into()]).await;
    bob.open(&wave_id).await;

    bob.send(ClientMessage::SetMode {
        wave_id: wave_id.clone(),
        mode: WaveMode::Frozen,
    })
    .await;
    let code = bob
        .recv_until(|m| match m {
            ServerMessage::Error { code, .. } => Some(*code),
            ServerMessage::ModeChanged { .. } => panic!("a non-creator changed the mode"),
            _ => None,
        })
        .await;
    assert_eq!(code, ErrorCode::Forbidden);
}

#[tokio::test]
async fn a_mode_change_is_never_destructive() {
    // Switching to Chat hides threading; switching back must bring it back
    // exactly, because nothing was rewritten.
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;
    let (wave_id, wavelet_id, root) = create_wave(&mut alice, "Thread", vec![]).await;

    for i in 0..3 {
        alice
            .send(ClientMessage::CreateBlip {
                wavelet_id: wavelet_id.clone(),
                parent: Some(root.clone()),
                content: Some(Delta::document(format!("reply {i}"))),
            })
            .await;
        alice
            .recv_until(|m| matches!(m, ServerMessage::BlipAdded { .. }).then_some(()))
            .await;
    }

    let snapshot = |client: &mut TestClient| async {
        let mut fresh = server.connect(&cookie).await;
        let _ = client;
        fresh
            .send(ClientMessage::Open {
                wave_id: wave_id.clone(),
            })
            .await;
        fresh
            .recv_until(|m| match m {
                ServerMessage::WaveState { wave } => Some(
                    wave.wavelets
                        .iter()
                        .flat_map(|w| w.blips.iter())
                        .map(|b| (b.id.clone(), b.parent.clone(), b.content.to_plain_text()))
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .await
    };

    let before = snapshot(&mut alice).await;
    set_mode(&mut alice, &wave_id, WaveMode::Chat).await;
    let during = snapshot(&mut alice).await;
    set_mode(&mut alice, &wave_id, WaveMode::Document).await;
    let after = snapshot(&mut alice).await;

    assert_eq!(
        before, after,
        "round-tripping the mode changed stored content"
    );
    assert_eq!(
        before, during,
        "chat mode should hide threading in the client, not discard it on the server"
    );
}

#[tokio::test]
async fn a_frozen_wave_stays_frozen_inside_a_private_reply() {
    // The reason mode belongs to the wave and not to each wavelet: a private
    // reply created before the freeze must not keep the old permissions.
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let bob_cookie = server.register("bob").await;
    let mut alice = server.connect(&alice_cookie).await;
    let mut bob = server.connect(&bob_cookie).await;

    let (wave_id, wavelet_id, root) = create_wave(&mut alice, "Team", vec!["bob".into()]).await;
    bob.open(&wave_id).await;

    alice
        .send(ClientMessage::PrivateReply {
            wavelet_id,
            anchor: root,
            participants: vec!["bob".into()],
        })
        .await;
    let private = bob
        .recv_until(|m| match m {
            ServerMessage::WaveletAdded { wavelet, .. } => Some(wavelet.clone()),
            _ => None,
        })
        .await;
    let secret = private.blips[0].id.clone();
    bob.edit(&secret, Delta::new().insert("before")).await;
    bob.recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;

    set_mode(&mut alice, &wave_id, WaveMode::Frozen).await;
    bob.drain().await;

    let revision = bob.docs[&secret].revision;
    let code = refusal(
        &mut bob,
        ClientMessage::Submit {
            blip_id: secret.clone(),
            revision,
            delta: Delta::new().insert("after"),
            op_id: Some("private-op".into()),
        },
    )
    .await;
    assert_eq!(
        code,
        Some(ErrorCode::Forbidden),
        "a private reply must be frozen with the rest of the wave"
    );
}

#[tokio::test]
async fn a_replayed_op_is_still_acknowledged_after_the_wave_freezes() {
    // Ordering matters: the mode check runs after the idempotency lookup. If it
    // ran first, an op the server had already committed would be refused on
    // replay, the client would never see its acknowledgement, and everything it
    // typed afterwards would pile up locally and never be sent.
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;
    let (wave_id, _, blip_id) = create_wave(&mut alice, "Race", vec![]).await;

    let op_id = "replayed-op".to_string();
    alice
        .send(ClientMessage::Submit {
            blip_id: blip_id.clone(),
            revision: 0,
            delta: Delta::new().insert("committed"),
            op_id: Some(op_id.clone()),
        })
        .await;
    let first = alice
        .recv_until(|m| match m {
            ServerMessage::Ack { revision, .. } => Some(*revision),
            _ => None,
        })
        .await;

    set_mode(&mut alice, &wave_id, WaveMode::Frozen).await;

    // Replay exactly what a reconnecting client would.
    alice
        .send(ClientMessage::Submit {
            blip_id: blip_id.clone(),
            revision: 0,
            delta: Delta::new().insert("committed"),
            op_id: Some(op_id),
        })
        .await;
    let second = alice
        .recv_until(|m| match m {
            ServerMessage::Ack { revision, .. } => Some(Some(*revision)),
            ServerMessage::Error { .. } => Some(None),
            _ => None,
        })
        .await;
    assert_eq!(
        second,
        Some(first),
        "a replay of an already-committed op must still be acknowledged"
    );
}

#[tokio::test]
async fn a_rejected_edit_tells_the_client_which_message_to_reset() {
    // A bare refusal leaves the client holding an op it retries forever.
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let bob_cookie = server.register("bob").await;
    let mut alice = server.connect(&alice_cookie).await;
    let mut bob = server.connect(&bob_cookie).await;

    let (wave_id, _, blip_id) = create_wave(&mut alice, "Channel", vec!["bob".into()]).await;
    bob.open(&wave_id).await;
    set_mode(&mut alice, &wave_id, WaveMode::Chat).await;
    bob.drain().await;

    bob.send(ClientMessage::Submit {
        blip_id: blip_id.clone(),
        revision: 0,
        delta: Delta::new().insert("nope"),
        op_id: Some("bob-1".into()),
    })
    .await;
    let carried = bob
        .recv_until(|m| match m {
            ServerMessage::Error { blip_id, .. } => Some(blip_id.clone()),
            _ => None,
        })
        .await;
    assert_eq!(
        carried,
        Some(blip_id),
        "the refusal must name the message so the client can drop its pending edit"
    );
}

#[tokio::test]
async fn concurrent_replies_get_distinct_positions_and_a_stable_order() {
    // Ordering positions used to be read from storage before the wave lock was
    // taken, so two creates racing in the same wavelet could be handed the same
    // one. Ties then resolved to hash iteration order — a different order for
    // every client, and a different one again after a reload.
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let bob_cookie = server.register("bob").await;
    let mut alice = server.connect(&alice_cookie).await;
    let mut bob = server.connect(&bob_cookie).await;

    let (wave_id, wavelet_id, _) = create_wave(&mut alice, "Race", vec!["bob".into()]).await;
    bob.open(&wave_id).await;

    // Fire creates from both connections without waiting for each other.
    for i in 0..8 {
        alice
            .send(ClientMessage::CreateBlip {
                wavelet_id: wavelet_id.clone(),
                parent: None,
                content: Some(Delta::document(format!("a{i}"))),
            })
            .await;
        bob.send(ClientMessage::CreateBlip {
            wavelet_id: wavelet_id.clone(),
            parent: None,
            content: Some(Delta::document(format!("b{i}"))),
        })
        .await;
    }
    for _ in 0..4 {
        alice.drain().await;
        bob.drain().await;
    }

    // Read the wave back from two independent connections and compare order.
    async fn order_seen_by(
        server: &TestServer,
        cookie: &str,
        wave_id: &WaveId,
    ) -> Vec<(i64, String)> {
        let mut client = server.connect(cookie).await;
        client
            .send(ClientMessage::Open {
                wave_id: wave_id.clone(),
            })
            .await;
        client
            .recv_until(|m| match m {
                ServerMessage::WaveState { wave } => Some(
                    wave.wavelets
                        .iter()
                        .flat_map(|w| w.blips.iter())
                        .map(|b| (b.seq, b.content.to_plain_text()))
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .await
    }
    let first = order_seen_by(&server, &alice_cookie, &wave_id).await;
    let second = order_seen_by(&server, &bob_cookie, &wave_id).await;

    assert_eq!(first, second, "two clients disagreed about message order");

    let seqs: Vec<i64> = first.iter().map(|(s, _)| *s).collect();
    let mut unique = seqs.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        seqs.len(),
        unique.len(),
        "two blips share an ordering position: {seqs:?}"
    );
    assert!(
        seqs.windows(2).all(|w| w[0] <= w[1]),
        "not sorted: {seqs:?}"
    );
}

#[tokio::test]
async fn changing_a_password_revokes_other_sessions() {
    let server = start_server().await;
    let first = server.register("alice").await;
    // A second session for the same account, as if signed in elsewhere.
    let http = reqwest::Client::new();
    let response = http
        .post(format!("{}/api/login", server.base))
        .json(&serde_json::json!({ "name": "alice", "password": "correct horse battery" }))
        .send()
        .await
        .unwrap();
    let second = response
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .unwrap()
        .to_string();

    // Wrong current password is refused.
    let refused = http
        .post(format!("{}/api/password", server.base))
        .header("cookie", &first)
        .json(&serde_json::json!({ "currentPassword": "wrong", "newPassword": "a-new-password" }))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 400);

    let changed = http
        .post(format!("{}/api/password", server.base))
        .header("cookie", &first)
        .json(&serde_json::json!({
            "currentPassword": "correct horse battery",
            "newPassword": "a-new-password",
        }))
        .send()
        .await
        .unwrap();
    assert!(changed.status().is_success(), "password change failed");

    // The session that made the change survives; the other is revoked.
    let still_ok = http
        .get(format!("{}/api/me", server.base))
        .header("cookie", &first)
        .send()
        .await
        .unwrap();
    assert!(
        still_ok.status().is_success(),
        "the current session should survive"
    );

    let revoked = http
        .get(format!("{}/api/me", server.base))
        .header("cookie", &second)
        .send()
        .await
        .unwrap();
    assert_eq!(revoked.status(), 401, "other sessions must be signed out");

    // And the new password works.
    let login = http
        .post(format!("{}/api/login", server.base))
        .json(&serde_json::json!({ "name": "alice", "password": "a-new-password" }))
        .send()
        .await
        .unwrap();
    assert!(login.status().is_success());
}

#[tokio::test]
async fn replaying_an_op_after_a_reconnect_does_not_apply_it_twice() {
    // A client replays work it never saw acknowledged. Because SIGTERM closes
    // every socket, this happens on an ordinary deploy, not just a crash — so
    // without an op id the text would be duplicated routinely.
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;
    let (wave_id, _, blip_id) = create_wave(&mut alice, "Retry", vec![]).await;

    let delta = Delta::new().insert("hello");
    let op_id = "stable-op-1".to_string();
    alice
        .send(ClientMessage::Submit {
            blip_id: blip_id.clone(),
            revision: 0,
            delta: delta.clone(),
            op_id: Some(op_id.clone()),
        })
        .await;
    let first = alice
        .recv_until(|m| match m {
            ServerMessage::Ack { revision, .. } => Some(*revision),
            _ => None,
        })
        .await;

    // Reconnect and replay the identical op, exactly as the client does when it
    // never received the acknowledgement.
    drop(alice);
    let mut again = server.connect(&cookie).await;
    again.open(&wave_id).await;
    again
        .send(ClientMessage::Submit {
            blip_id: blip_id.clone(),
            revision: 0,
            delta,
            op_id: Some(op_id),
        })
        .await;
    let second = again
        .recv_until(|m| match m {
            ServerMessage::Ack { revision, .. } => Some(*revision),
            _ => None,
        })
        .await;

    assert_eq!(
        second, first,
        "the replay should be acknowledged at the original revision"
    );

    let mut fresh = server.connect(&cookie).await;
    fresh.open(&wave_id).await;
    assert_eq!(
        fresh.text(&blip_id),
        "hello",
        "the text must not be duplicated"
    );
}

#[tokio::test]
async fn distinct_ops_with_different_ids_both_apply() {
    // The dedup must not swallow genuinely different edits.
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;
    let (wave_id, _, blip_id) = create_wave(&mut alice, "Distinct", vec![]).await;

    for (i, text) in ["a", "b", "c"].iter().enumerate() {
        alice
            .send(ClientMessage::Submit {
                blip_id: blip_id.clone(),
                revision: i as u64,
                delta: Delta::new().retain(i).insert(*text),
                op_id: Some(format!("op-{i}")),
            })
            .await;
        alice
            .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
            .await;
    }

    // Read back from a fresh client: these submits bypassed `edit()`, so the
    // sender's own mirror was never updated.
    let mut fresh = server.connect(&cookie).await;
    fresh.open(&wave_id).await;
    assert_eq!(fresh.text(&blip_id), "abc");
}

#[tokio::test]
async fn presence_does_not_reveal_that_a_private_reply_exists() {
    // Content isolation was never the problem; metadata was. An unscoped
    // presence list named the blip a user was editing, so an excluded
    // participant learned a private side conversation existed and was active.
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let bob_cookie = server.register("bob").await;
    let carol_cookie = server.register("carol").await;

    let mut alice = server.connect(&alice_cookie).await;
    let mut bob = server.connect(&bob_cookie).await;
    let mut carol = server.connect(&carol_cookie).await;

    let (wave_id, wavelet_id, root_blip) =
        create_wave(&mut alice, "Team", vec!["bob".into(), "carol".into()]).await;
    bob.open(&wave_id).await;
    carol.open(&wave_id).await;

    alice
        .send(ClientMessage::PrivateReply {
            wavelet_id,
            anchor: root_blip,
            participants: vec!["bob".into()],
        })
        .await;
    let private = bob
        .recv_until(|m| match m {
            ServerMessage::WaveletAdded { wavelet, .. } => Some(wavelet.clone()),
            _ => None,
        })
        .await;
    let secret_blip = private.blips[0].id.clone();

    // Alice puts her caret in the private blip.
    alice
        .send(ClientMessage::Cursor {
            wave_id: wave_id.clone(),
            blip_id: secret_blip.clone(),
            index: 0,
            length: 0,
        })
        .await;

    carol.drain().await;
    let leaked = carol.presence_mentions(&secret_blip);
    assert!(
        !leaked,
        "presence exposed a private-reply blip id to an excluded participant"
    );

    // Bob, who is in the private reply, still gets useful presence.
    bob.drain().await;
    assert!(
        bob.presence_mentions(&secret_blip),
        "a participant of the private reply should still see the caret"
    );
}

#[tokio::test]
async fn seed_content_that_is_not_a_document_is_rejected() {
    // A retain-only delta cannot be loaded back as a document. It used to be
    // accepted and stored, leaving meta.revision=1 against doc.revision=0, so
    // every later edit was refused as out of date — the blip was bricked, and
    // the corruption survived a restart.
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;
    let (wave_id, wavelet_id, _) = create_wave(&mut alice, "Guard", vec![]).await;

    alice
        .send(ClientMessage::CreateBlip {
            wavelet_id,
            parent: None,
            content: Some(Delta::new().retain(5)),
        })
        .await;
    let code = alice
        .recv_until(|m| match m {
            ServerMessage::Error { code, .. } => Some(*code),
            ServerMessage::BlipAdded { .. } => panic!("a non-document was accepted"),
            _ => None,
        })
        .await;
    assert_eq!(code, ErrorCode::BadRequest);

    // The wave is still usable afterwards.
    let mut fresh = server.connect(&cookie).await;
    let (_, blip_id) = fresh.open(&wave_id).await;
    fresh
        .edit(&blip_id, Delta::new().insert("still works"))
        .await;
    fresh
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;
    assert_eq!(fresh.text(&blip_id), "still works");
}

#[tokio::test]
async fn an_oversized_embed_is_refused_by_name_and_leaves_the_blip_usable() {
    // An embed is one unit of a document however much JSON it carries, so the
    // length limits say nothing about its size. The refusal has to name the
    // blip: a bare one leaves the client retrying an op the server will never
    // take, and everything typed afterwards queues behind it forever.
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;
    let (wave_id, _, blip_id) = create_wave(&mut alice, "Embeds", vec![]).await;

    alice
        .send(ClientMessage::Submit {
            blip_id: blip_id.clone(),
            revision: 0,
            delta: Delta::new().embed(serde_json::json!({ "junk": "x".repeat(4096) })),
            op_id: Some("op-1".into()),
        })
        .await;

    let named = alice
        .recv_until(|m| match m {
            ServerMessage::Error { code, blip_id, .. } => Some((*code, blip_id.clone())),
            ServerMessage::Ack { .. } => panic!("an oversized embed was accepted"),
            _ => None,
        })
        .await;
    assert_eq!(named.0, ErrorCode::BadRequest);
    assert_eq!(named.1.as_ref(), Some(&blip_id));

    // An ordinary attachment reference is fine, and the blip still works.
    let mut fresh = server.connect(&cookie).await;
    let (_, blip_id) = fresh.open(&wave_id).await;
    fresh
        .edit(
            &blip_id,
            Delta::new().embed(serde_json::json!({
                "attachment": { "id": "a-1", "name": "plan.png", "mime": "image/png", "size": 12 }
            })),
        )
        .await;
    fresh
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;
    assert_eq!(fresh.text(&blip_id), "\u{fffc}");
}

#[tokio::test]
async fn only_the_creator_can_remove_someone_else() {
    // Any participant removing any other made a hostile takeover trivial.
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let bob_cookie = server.register("bob").await;
    let mut alice = server.connect(&alice_cookie).await;
    let mut bob = server.connect(&bob_cookie).await;

    let (wave_id, wavelet_id, _) = create_wave(&mut alice, "Mine", vec!["bob".into()]).await;
    bob.open(&wave_id).await;

    // Bob tries to evict the creator.
    bob.send(ClientMessage::RemoveParticipant {
        wavelet_id: wavelet_id.clone(),
        user_id: alice.user.id.clone(),
    })
    .await;
    let code = bob
        .recv_until(|m| match m {
            ServerMessage::Error { code, .. } => Some(*code),
            _ => None,
        })
        .await;
    assert_eq!(code, ErrorCode::Forbidden);

    // Alice still has access.
    let mut again = server.connect(&alice_cookie).await;
    again.open(&wave_id).await;

    // But Bob may always remove himself. He is no longer a participant of the
    // wavelet, so he is told the wave is gone rather than getting the
    // participant-change broadcast he is no longer a recipient of.
    bob.send(ClientMessage::RemoveParticipant {
        wavelet_id,
        user_id: bob.user.id.clone(),
    })
    .await;
    bob.recv_until(|m| matches!(m, ServerMessage::WaveRemoved { .. }).then_some(()))
        .await;

    // And it really took effect: he cannot reopen it.
    let mut bob_again = server.connect(&bob_cookie).await;
    bob_again.send(ClientMessage::Open { wave_id }).await;
    let code = bob_again
        .recv_until(|m| match m {
            ServerMessage::Error { code, .. } => Some(*code),
            ServerMessage::WaveState { .. } => panic!("removed user still has access"),
            _ => None,
        })
        .await;
    assert_eq!(code, ErrorCode::NotFound);
}

#[tokio::test]
async fn a_non_participant_cannot_mark_read_or_flag_a_wave() {
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let mallory_cookie = server.register("mallory").await;
    let mut alice = server.connect(&alice_cookie).await;
    let (wave_id, _, _) = create_wave(&mut alice, "Private", vec![]).await;

    let mut mallory = server.connect(&mallory_cookie).await;
    for message in [
        ClientMessage::MarkRead {
            wave_id: wave_id.clone(),
        },
        ClientMessage::SetFlags {
            wave_id: wave_id.clone(),
            flags: WaveFlags {
                archived: true,
                muted: false,
            },
        },
        ClientMessage::RequestPlayback {
            wave_id: wave_id.clone(),
        },
    ] {
        mallory.send(message).await;
        let code = mallory
            .recv_until(|m| match m {
                ServerMessage::Error { code, .. } => Some(*code),
                _ => None,
            })
            .await;
        assert_eq!(
            code,
            ErrorCode::NotFound,
            "an outsider reached a per-user wave operation"
        );
    }
}

#[tokio::test]
async fn playback_does_not_replay_a_deleted_blip() {
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;
    let (wave_id, wavelet_id, _) = create_wave(&mut alice, "History", vec![]).await;

    alice
        .send(ClientMessage::CreateBlip {
            wavelet_id,
            parent: None,
            content: Some(Delta::document("TOP SECRET")),
        })
        .await;
    let blip = alice
        .recv_until(|m| match m {
            ServerMessage::BlipAdded { blip, .. } => Some(blip.clone()),
            _ => None,
        })
        .await;

    alice
        .send(ClientMessage::DeleteBlip {
            blip_id: blip.id.clone(),
        })
        .await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::BlipRemoved { .. }).then_some(()))
        .await;

    alice.send(ClientMessage::RequestPlayback { wave_id }).await;
    let frames = alice
        .recv_until(|m| match m {
            ServerMessage::Playback { frames, .. } => Some(frames.clone()),
            _ => None,
        })
        .await;
    let replayed: String = frames.iter().map(|f| f.delta.to_plain_text()).collect();
    assert!(
        !replayed.contains("TOP SECRET"),
        "playback resurrected a deleted blip: {replayed}"
    );
}

#[tokio::test]
async fn malformed_messages_do_not_kill_the_connection() {
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;

    alice
        .socket
        .send(Message::Text("not json at all".into()))
        .await
        .unwrap();
    let code = alice
        .recv_until(|m| match m {
            ServerMessage::Error { code, .. } => Some(*code),
            _ => None,
        })
        .await;
    assert_eq!(code, ErrorCode::BadRequest);

    // The connection still works afterwards.
    alice.send(ClientMessage::Ping).await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Pong).then_some(()))
        .await;
}

#[tokio::test]
async fn an_op_against_an_impossible_revision_is_refused_without_corrupting_the_document() {
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;

    let (wave_id, _, blip_id) = create_wave(&mut alice, "Guard", vec![]).await;
    alice.edit(&blip_id, Delta::new().insert("safe")).await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;

    // Claim a revision the server has never reached.
    alice
        .send(ClientMessage::Submit {
            blip_id: blip_id.clone(),
            revision: 9999,
            delta: Delta::new().insert("corrupt"),
            op_id: None,
        })
        .await;
    let code = alice
        .recv_until(|m| match m {
            ServerMessage::Error { code, .. } => Some(*code),
            _ => None,
        })
        .await;
    assert_eq!(code, ErrorCode::Resync);

    let mut fresh = server.connect(&cookie).await;
    fresh.open(&wave_id).await;
    assert_eq!(fresh.text(&blip_id), "safe", "document must be untouched");
}

// --- comments ------------------------------------------------------------

/// The whole reason the anchor is an attribute rather than an offset.
///
/// Alice comments on a phrase; Bob then types *before* it. Nothing tells the
/// comment about that edit, and nothing has to: the anchor is part of the
/// document, so the same transform that moves the text moves the highlight, and
/// both clients land on the same range.
#[tokio::test]
async fn an_anchor_moves_with_its_text_when_someone_types_in_front_of_it() {
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let bob_cookie = server.register("bob").await;
    let mut alice = server.connect(&alice_cookie).await;
    let mut bob = server.connect(&bob_cookie).await;

    let (wave_id, wavelet_id, page) = create_wave_in(
        &mut alice,
        "Release notes",
        vec!["bob".into()],
        Some(WaveMode::Notepad),
    )
    .await;
    bob.open(&wave_id).await;

    alice
        .edit(&page, Delta::new().insert("Ship it on Friday."))
        .await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;
    bob.drain().await;

    // Anchor "Friday" — offset 11, six units in.
    let thread = CommentId::new();
    comment_on(
        &mut alice,
        &wavelet_id,
        &page,
        &thread,
        11,
        6,
        "Friday is too soon.",
    )
    .await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;
    bob.drain().await;

    let before = anchored_ranges(&alice.docs[&page].doc, &thread);
    assert_eq!(before, vec![(11, 6)], "the anchor starts on 'Friday'");
    assert_eq!(
        anchored_ranges(&bob.docs[&page].doc, &thread),
        before,
        "both clients see the same anchor"
    );

    // Bob prepends eight units. He knows nothing about the comment.
    bob.edit(&page, Delta::new().insert("Reminder")).await;
    bob.recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;
    alice.drain().await;
    bob.drain().await;

    assert_eq!(
        alice.text(&page),
        "ReminderShip it on Friday.",
        "the text converged"
    );
    assert_eq!(
        anchored_ranges(&alice.docs[&page].doc, &thread),
        vec![(19, 6)],
        "the anchor followed 'Friday' rather than staying at offset 11"
    );
    assert_eq!(
        anchored_ranges(&bob.docs[&page].doc, &thread),
        vec![(19, 6)],
        "and every client agrees on where it landed"
    );

    // A reopened wave rebuilds from the stored snapshot, so the anchor has to
    // survive the round trip through the database too.
    let mut fresh = server.connect(&alice_cookie).await;
    fresh.open(&wave_id).await;
    assert_eq!(
        anchored_ranges(&fresh.docs[&page].doc, &thread),
        vec![(19, 6)]
    );
}

/// Deleting the commented words detaches the thread rather than leaving it
/// pointing at whatever text happens to occupy those offsets afterwards.
#[tokio::test]
async fn deleting_the_anchored_text_detaches_the_thread_but_keeps_it() {
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;

    let (wave_id, wavelet_id, page) =
        create_wave_in(&mut alice, "Page", vec![], Some(WaveMode::Notepad)).await;
    alice
        .edit(&page, Delta::new().insert("keep cut keep"))
        .await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;

    let thread = CommentId::new();
    comment_on(&mut alice, &wavelet_id, &page, &thread, 5, 3, "why this?").await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;
    assert_eq!(
        anchored_ranges(&alice.docs[&page].doc, &thread),
        vec![(5, 3)]
    );

    alice.edit(&page, Delta::new().retain(4).delete(4)).await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;

    assert_eq!(alice.text(&page), "keep keep");
    assert!(
        anchored_ranges(&alice.docs[&page].doc, &thread).is_empty(),
        "the anchor went with the words it marked"
    );

    // The remarks are still there. Losing them because someone edited a
    // sentence would destroy the discussion about why it was edited.
    let mut fresh = server.connect(&cookie).await;
    let threads = fresh.open_state(&wave_id).await.wavelets[0]
        .comments
        .clone();
    assert_eq!(threads.len(), 1, "the thread outlives its anchor");
    assert_eq!(threads[0].id, thread);
}

#[tokio::test]
async fn only_a_notepad_takes_comments() {
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;

    for mode in WaveMode::ALL {
        let (_, wavelet_id, page) =
            create_wave_in(&mut alice, mode.label(), vec![], Some(mode)).await;
        alice.drain().await;
        alice
            .send(ClientMessage::CreateComment {
                wavelet_id,
                blip_id: page,
                comment_id: CommentId::new(),
                content: Some(Delta::document("a remark")),
            })
            .await;

        let accepted = alice
            .recv_until(|m| match m {
                ServerMessage::CommentAdded { .. } => Some(true),
                ServerMessage::Error { .. } => Some(false),
                _ => None,
            })
            .await;
        assert_eq!(
            accepted,
            mode.allows_comments(),
            "{mode:?} disagreed with its own rule"
        );
    }
}

#[tokio::test]
async fn a_comment_reaches_the_other_participant_live() {
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let bob_cookie = server.register("bob").await;
    let mut alice = server.connect(&alice_cookie).await;
    let mut bob = server.connect(&bob_cookie).await;

    let (wave_id, wavelet_id, page) = create_wave_in(
        &mut alice,
        "Shared page",
        vec!["bob".into()],
        Some(WaveMode::Notepad),
    )
    .await;
    bob.open(&wave_id).await;

    let thread = CommentId::new();
    alice
        .send(ClientMessage::CreateComment {
            wavelet_id,
            blip_id: page.clone(),
            comment_id: thread.clone(),
            content: Some(Delta::document("is this right?")),
        })
        .await;

    let (comment, blip) = bob
        .recv_until(|m| match m {
            ServerMessage::CommentAdded { comment, blip, .. } => {
                Some((comment.clone(), blip.clone()))
            }
            _ => None,
        })
        .await;
    assert_eq!(comment.id, thread);
    assert_eq!(comment.blip_id, page, "the thread names the page it is on");
    assert_eq!(comment.author, alice.user.id);
    assert!(!comment.resolved());
    // The remark arrives with the thread, not in a second message: a thread
    // with nothing in it is a state no client should have to draw.
    assert_eq!(blip.comment.as_ref(), Some(&thread));
    assert_eq!(blip.parent.as_ref(), Some(&page));
    assert_eq!(blip.content.to_plain_text(), "is this right?");

    // Bob replies; Alice sees it as an ordinary blip tagged with the thread.
    bob.send(ClientMessage::ReplyToComment {
        comment_id: thread.clone(),
        content: Some(Delta::document("no, fixing it")),
    })
    .await;
    let reply = alice
        .recv_until(|m| match m {
            ServerMessage::BlipAdded { blip, .. } if blip.comment.as_ref() == Some(&thread) => {
                Some(blip.clone())
            }
            _ => None,
        })
        .await;
    assert_eq!(reply.author, bob.user.id);
    assert_eq!(reply.content.to_plain_text(), "no, fixing it");
}

#[tokio::test]
async fn a_thread_can_be_closed_and_reopened_without_losing_anything() {
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let bob_cookie = server.register("bob").await;
    let mut alice = server.connect(&alice_cookie).await;
    let mut bob = server.connect(&bob_cookie).await;

    let (wave_id, wavelet_id, page) = create_wave_in(
        &mut alice,
        "Page",
        vec!["bob".into()],
        Some(WaveMode::Notepad),
    )
    .await;
    bob.open(&wave_id).await;

    let thread = CommentId::new();
    comment_on(&mut alice, &wavelet_id, &page, &thread, 0, 0, "typo here").await;
    bob.drain().await;

    // Bob did not open the thread, and closes it anyway: a notepad is a page
    // everyone edits, so a remark about it is everyone's to settle.
    bob.send(ClientMessage::ResolveComment {
        comment_id: thread.clone(),
        resolved: true,
    })
    .await;
    let by = alice
        .recv_until(|m| match m {
            ServerMessage::CommentResolved {
                comment_id,
                resolved_by,
                ..
            } if comment_id == &thread => Some(resolved_by.clone()),
            _ => None,
        })
        .await;
    assert_eq!(by, Some(bob.user.id.clone()), "who closed it is recorded");

    // Nothing was destroyed: the remark and the anchor are both still there.
    let mut fresh = server.connect(&alice_cookie).await;
    let state = fresh.open_state(&wave_id).await.wavelets[0].clone();
    let stored = state.comments.iter().find(|c| c.id == thread).unwrap();
    assert!(stored.resolved());
    assert_eq!(stored.resolved_by.as_ref(), Some(&bob.user.id));
    assert!(stored.resolved_at.is_some());
    assert_eq!(
        anchored_ranges(&fresh.docs[&page].doc, &thread).len(),
        0,
        "this thread was anchored to an empty range, so there is nothing to find"
    );
    assert!(
        state
            .blips
            .iter()
            .any(|b| b.comment.as_ref() == Some(&thread)),
        "the remark survives being resolved"
    );

    // And reopening restores it exactly. Bob is drained first: he was sent the
    // broadcast of his own resolve, and matching that one would prove nothing.
    bob.drain().await;
    alice
        .send(ClientMessage::ResolveComment {
            comment_id: thread.clone(),
            resolved: false,
        })
        .await;
    let by = bob
        .recv_until(|m| match m {
            ServerMessage::CommentResolved {
                comment_id,
                resolved_by,
                ..
            } if comment_id == &thread => Some(resolved_by.clone()),
            _ => None,
        })
        .await;
    assert_eq!(by, None);
}

#[tokio::test]
async fn a_resolved_thread_refuses_a_remark_rather_than_hiding_it() {
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;

    let (_, wavelet_id, page) =
        create_wave_in(&mut alice, "Page", vec![], Some(WaveMode::Notepad)).await;
    let thread = CommentId::new();
    comment_on(&mut alice, &wavelet_id, &page, &thread, 0, 0, "a point").await;
    alice.drain().await;

    alice
        .send(ClientMessage::ResolveComment {
            comment_id: thread.clone(),
            resolved: true,
        })
        .await;
    alice.drain().await;

    alice
        .send(ClientMessage::ReplyToComment {
            comment_id: thread,
            content: Some(Delta::document("one more thing")),
        })
        .await;
    // Accepting this would write a remark into a thread drawn collapsed, so the
    // author would never see what they had just written.
    let code = alice
        .recv_until(|m| match m {
            ServerMessage::Error { code, .. } => Some(*code),
            ServerMessage::BlipAdded { .. } => Some(ErrorCode::Internal),
            _ => None,
        })
        .await;
    assert_eq!(code, ErrorCode::BadRequest);
}

#[tokio::test]
async fn a_comment_id_the_server_would_not_mint_is_refused() {
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;

    let (_, wavelet_id, page) =
        create_wave_in(&mut alice, "Page", vec![], Some(WaveMode::Notepad)).await;
    alice.drain().await;

    // The id is the client's to choose, which is exactly why its shape is not.
    for bad in ["", "b-stolen", "c-a'; DROP TABLE blips--", "c-<script>"] {
        alice
            .send(ClientMessage::CreateComment {
                wavelet_id: wavelet_id.clone(),
                blip_id: page.clone(),
                comment_id: CommentId::from(bad),
                content: Some(Delta::document("x")),
            })
            .await;
        let code = alice
            .recv_until(|m| match m {
                ServerMessage::Error { code, .. } => Some(*code),
                ServerMessage::CommentAdded { .. } => Some(ErrorCode::Internal),
                _ => None,
            })
            .await;
        assert_eq!(code, ErrorCode::BadRequest, "should be refused: {bad:?}");
    }

    // Reusing an id would merge two people's threads into one.
    let thread = CommentId::new();
    comment_on(&mut alice, &wavelet_id, &page, &thread, 0, 0, "first").await;
    alice.drain().await;
    alice
        .send(ClientMessage::CreateComment {
            wavelet_id,
            blip_id: page,
            comment_id: thread,
            content: Some(Delta::document("second")),
        })
        .await;
    let code = alice
        .recv_until(|m| match m {
            ServerMessage::Error { code, .. } => Some(*code),
            ServerMessage::CommentAdded { .. } => Some(ErrorCode::Internal),
            _ => None,
        })
        .await;
    assert_eq!(code, ErrorCode::BadRequest);
}

#[tokio::test]
async fn a_remark_cannot_be_deleted_out_from_under_its_thread() {
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;

    let (wave_id, wavelet_id, page) =
        create_wave_in(&mut alice, "Page", vec![], Some(WaveMode::Notepad)).await;
    let thread = CommentId::new();
    comment_on(&mut alice, &wavelet_id, &page, &thread, 0, 0, "a point").await;
    let remark = alice
        .docs
        .keys()
        .find(|id| *id != &page)
        .cloned()
        .expect("the remark is a blip");
    alice.drain().await;

    // Notepad forbids deletion anyway; switch to a mode that allows it, which is
    // the case that would otherwise slip through.
    alice
        .send(ClientMessage::SetMode {
            wave_id: wave_id.clone(),
            mode: WaveMode::Document,
        })
        .await;
    alice.drain().await;

    alice
        .send(ClientMessage::DeleteBlip {
            blip_id: remark.clone(),
        })
        .await;
    let code = alice
        .recv_until(|m| match m {
            ServerMessage::Error { code, .. } => Some(*code),
            ServerMessage::BlipRemoved { .. } => Some(ErrorCode::Internal),
            _ => None,
        })
        .await;
    assert_eq!(
        code,
        ErrorCode::Forbidden,
        "deleting the only remark would leave a thread nothing can draw"
    );

    // The message it annotates *can* still be deleted, and takes the thread with
    // it. Counting remarks as replies made a commented message undeletable for
    // ever: the reply rule refused the parent and the rule above refused the
    // only thing that would have satisfied it.
    alice
        .send(ClientMessage::DeleteBlip {
            blip_id: page.clone(),
        })
        .await;
    alice
        .recv_until(|m| match m {
            ServerMessage::BlipRemoved { blip_id, .. } if blip_id == &page => Some(()),
            _ => None,
        })
        .await;

    let mut fresh = server.connect(&cookie).await;
    let state = fresh.open_state(&wave_id).await;
    let wavelet = &state.wavelets[0];
    assert!(
        wavelet.comments.is_empty(),
        "a thread about a message that no longer exists is a remark about nothing"
    );
    assert!(
        !wavelet.blips.iter().any(|b| b.comment.is_some()),
        "and its remarks go with it"
    );
}

#[tokio::test]
async fn a_remark_does_not_become_the_inbox_preview() {
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let bob_cookie = server.register("bob").await;
    let mut alice = server.connect(&alice_cookie).await;
    let mut bob = server.connect(&bob_cookie).await;

    let (wave_id, wavelet_id, page) = create_wave_in(
        &mut alice,
        "Release notes",
        vec!["bob".into()],
        Some(WaveMode::Notepad),
    )
    .await;
    alice
        .edit(&page, Delta::new().insert("Q3 launch plan"))
        .await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;

    let thread = CommentId::new();
    comment_on(
        &mut alice,
        &wavelet_id,
        &page,
        &thread,
        0,
        2,
        "this line reads oddly",
    )
    .await;
    alice.drain().await;
    bob.drain().await;

    // The margin is a note *about* the page, so it must not replace the page in
    // Bob's inbox — nor make a wave whose whole premise is one shared document
    // report a growing message count.
    let fresh = server.connect(&bob_cookie).await;
    let summary = fresh
        .inbox
        .iter()
        .find(|s| s.id == wave_id)
        .expect("wave missing from inbox");
    assert_eq!(summary.snippet, "Q3 launch plan");
    assert_eq!(summary.snippet_author.as_ref(), Some(&alice.user.id));
    assert_eq!(summary.blip_count, 1, "one shared page, one blip");
}

/// Leaving Notepad must not strand open threads, and freezing must stop them.
#[tokio::test]
async fn a_thread_can_still_be_settled_after_the_mode_moves_on() {
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;

    let (wave_id, wavelet_id, page) =
        create_wave_in(&mut alice, "Page", vec![], Some(WaveMode::Notepad)).await;
    let thread = CommentId::new();
    comment_on(&mut alice, &wavelet_id, &page, &thread, 0, 0, "a point").await;
    alice.drain().await;

    alice
        .send(ClientMessage::SetMode {
            wave_id: wave_id.clone(),
            mode: WaveMode::Document,
        })
        .await;
    alice.drain().await;

    // Still settleable: mode changes are reversible and must not leave rubbish
    // behind that nothing can ever clear.
    alice
        .send(ClientMessage::ResolveComment {
            comment_id: thread.clone(),
            resolved: true,
        })
        .await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::CommentResolved { .. }).then_some(()))
        .await;

    // But a frozen wave is frozen, resolving included.
    alice
        .send(ClientMessage::SetMode {
            wave_id,
            mode: WaveMode::Frozen,
        })
        .await;
    alice.drain().await;
    alice
        .send(ClientMessage::ResolveComment {
            comment_id: thread,
            resolved: false,
        })
        .await;
    let code = alice
        .recv_until(|m| match m {
            ServerMessage::Error { code, .. } => Some(*code),
            ServerMessage::CommentResolved { .. } => Some(ErrorCode::Internal),
            _ => None,
        })
        .await;
    assert_eq!(code, ErrorCode::Forbidden);
}

/// Access control lives on the wavelet, and comments are no exception.
#[tokio::test]
async fn a_comment_in_a_private_reply_stays_inside_it() {
    let server = start_server().await;
    let alice_cookie = server.register("alice").await;
    let bob_cookie = server.register("bob").await;
    let carol_cookie = server.register("carol").await;
    let mut alice = server.connect(&alice_cookie).await;
    let mut bob = server.connect(&bob_cookie).await;
    let mut carol = server.connect(&carol_cookie).await;

    // Start in Document so a private reply may be branched, then switch to
    // Notepad, which is the mode that takes comments.
    let (wave_id, wavelet_id, page) =
        create_wave(&mut alice, "Plan", vec!["bob".into(), "carol".into()]).await;
    bob.open(&wave_id).await;
    carol.open(&wave_id).await;

    alice
        .send(ClientMessage::PrivateReply {
            wavelet_id: wavelet_id.clone(),
            anchor: page,
            participants: vec!["bob".into()],
        })
        .await;
    let private = alice
        .recv_until(|m| match m {
            ServerMessage::WaveletAdded { wavelet, .. } => {
                Some((wavelet.id.clone(), wavelet.blips[0].id.clone()))
            }
            _ => None,
        })
        .await;
    alice
        .send(ClientMessage::SetMode {
            wave_id: wave_id.clone(),
            mode: WaveMode::Notepad,
        })
        .await;
    alice.drain().await;
    bob.drain().await;
    carol.drain().await;

    let thread = CommentId::new();
    alice
        .send(ClientMessage::CreateComment {
            wavelet_id: private.0,
            blip_id: private.1.clone(),
            comment_id: thread.clone(),
            content: Some(Delta::document("just between us")),
        })
        .await;
    bob.recv_until(|m| match m {
        ServerMessage::CommentAdded { comment, .. } if comment.id == thread => Some(()),
        _ => None,
    })
    .await;

    // Carol is in the wave but not in that wavelet, so nothing about the thread
    // may reach her — including the fact that it exists.
    carol.drain().await;
    assert!(
        !carol
            .docs
            .keys()
            .any(|id| Some(id) == alice.docs.keys().find(|k| **k == private.1)),
        "carol never received the private blip"
    );
    carol
        .send(ClientMessage::ResolveComment {
            comment_id: thread.clone(),
            resolved: true,
        })
        .await;
    let code = carol
        .recv_until(|m| match m {
            ServerMessage::Error { code, .. } => Some(*code),
            ServerMessage::CommentResolved { .. } => Some(ErrorCode::Internal),
            _ => None,
        })
        .await;
    assert_eq!(
        code,
        ErrorCode::NotFound,
        "and she is told no more than that"
    );

    // A reopened wave must not hand her the thread either.
    let mut fresh = server.connect(&carol_cookie).await;
    let wavelets = fresh.open_state(&wave_id).await.wavelets;
    assert!(
        wavelets.iter().all(|w| w.comments.is_empty()),
        "carol's snapshot must not carry a thread from a wavelet she is not in"
    );
}

#[tokio::test]
async fn a_comment_cannot_be_hung_on_a_comment() {
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;

    let (_, wavelet_id, page) =
        create_wave_in(&mut alice, "Page", vec![], Some(WaveMode::Notepad)).await;
    let thread = CommentId::new();
    comment_on(&mut alice, &wavelet_id, &page, &thread, 0, 0, "a point").await;
    let remark = alice
        .docs
        .keys()
        .find(|id| *id != &page)
        .cloned()
        .expect("the remark is a blip");
    alice.drain().await;

    alice
        .send(ClientMessage::CreateComment {
            wavelet_id,
            blip_id: remark,
            comment_id: CommentId::new(),
            content: Some(Delta::document("meta")),
        })
        .await;
    let code = alice
        .recv_until(|m| match m {
            ServerMessage::Error { code, .. } => Some(*code),
            ServerMessage::CommentAdded { .. } => Some(ErrorCode::Internal),
            _ => None,
        })
        .await;
    assert_eq!(code, ErrorCode::BadRequest);
}

// --- document limits -----------------------------------------------------

/// The limits existed but were only ever applied to a blip's *initial* content,
/// which is not how documents are written.
#[tokio::test]
async fn a_blip_cannot_be_grown_past_its_limit_one_edit_at_a_time() {
    use crate::state::MAX_BLIP_UNITS;

    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;

    let (wave_id, _, blip_id) = create_wave(&mut alice, "Big", vec![]).await;
    alice.edit(&blip_id, Delta::new().insert("keep")).await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;
    alice.drain().await;

    // Sent raw rather than through `edit`, so the client mirror is not left
    // holding work the server was always going to refuse.
    alice
        .send(ClientMessage::Submit {
            blip_id: blip_id.clone(),
            revision: 1,
            delta: Delta::new()
                .retain(4)
                .insert("a".repeat(MAX_BLIP_UNITS + 10)),
            op_id: Some("too-big".into()),
        })
        .await;
    let (code, named) = alice
        .recv_until(|m| match m {
            ServerMessage::Error { code, blip_id, .. } => Some((*code, blip_id.clone())),
            ServerMessage::Ack { .. } => Some((ErrorCode::Internal, None)),
            _ => None,
        })
        .await;
    assert_eq!(code, ErrorCode::Resync, "an oversized edit must not land");
    assert_eq!(
        named.as_ref(),
        Some(&blip_id),
        "a refusal must name the document to reset, or the client retries forever"
    );

    // The rollback is the point: an op left in the resident document but absent
    // from the log would have later ops transforming over something that exists
    // nowhere else.
    let mut fresh = server.connect(&cookie).await;
    fresh.open(&wave_id).await;
    assert_eq!(fresh.text(&blip_id), "keep", "the document is untouched");

    // And the blip still works afterwards.
    fresh
        .edit(&blip_id, Delta::new().retain(4).insert(" going"))
        .await;
    fresh
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;
    assert_eq!(fresh.text(&blip_id), "keep going");
}

/// Length counts units; an attribute map costs none however much it carries, so
/// the run count is the other half of the bound.
#[tokio::test]
async fn a_document_cannot_be_shattered_into_unlimited_runs() {
    use crate::state::MAX_BLIP_RUNS;

    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;

    let (wave_id, _, blip_id) = create_wave(&mut alice, "Runs", vec![]).await;

    // One character per run, alternating so nothing merges them back together.
    let mut delta = Delta::new();
    for i in 0..=MAX_BLIP_RUNS {
        let mut attrs = gal_ot::Attributes::new();
        attrs.insert("bold".into(), serde_json::json!(i % 2 == 0));
        delta = delta.insert_with("x", attrs);
    }
    alice
        .send(ClientMessage::Submit {
            blip_id: blip_id.clone(),
            revision: 0,
            delta,
            op_id: Some("shatter".into()),
        })
        .await;

    let code = alice
        .recv_until(|m| match m {
            ServerMessage::Error { code, .. } => Some(*code),
            ServerMessage::Ack { .. } => Some(ErrorCode::Internal),
            _ => None,
        })
        .await;
    assert_eq!(code, ErrorCode::Resync);

    let mut fresh = server.connect(&cookie).await;
    fresh.open(&wave_id).await;
    assert_eq!(fresh.text(&blip_id), "", "nothing was kept");
}

#[tokio::test]
async fn an_attribute_the_document_model_does_not_define_is_refused_by_name() {
    let server = start_server().await;
    let cookie = server.register("alice").await;
    let mut alice = server.connect(&cookie).await;

    let (wave_id, _, blip_id) = create_wave(&mut alice, "Attrs", vec![]).await;
    alice.edit(&blip_id, Delta::new().insert("hello")).await;
    alice
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;
    alice.drain().await;

    let mut attrs = gal_ot::Attributes::new();
    attrs.insert("payload".into(), serde_json::json!("x".repeat(200_000)));
    alice
        .send(ClientMessage::Submit {
            blip_id: blip_id.clone(),
            revision: 1,
            delta: Delta::new().retain_with(5, attrs),
            op_id: Some("junk".into()),
        })
        .await;

    let (code, named) = alice
        .recv_until(|m| match m {
            ServerMessage::Error { code, blip_id, .. } => Some((*code, blip_id.clone())),
            ServerMessage::Ack { .. } => Some((ErrorCode::Internal, None)),
            _ => None,
        })
        .await;
    assert_eq!(code, ErrorCode::BadRequest);
    assert_eq!(
        named.as_ref(),
        Some(&blip_id),
        "named, like every op refusal"
    );

    let mut fresh = server.connect(&cookie).await;
    fresh.open(&wave_id).await;
    assert_eq!(fresh.text(&blip_id), "hello");

    // Formatting the client really does send still works, so the check bounds
    // rather than breaks.
    let mut bold = gal_ot::Attributes::new();
    bold.insert("bold".into(), serde_json::json!(true));
    fresh
        .edit(&blip_id, Delta::new().retain_with(5, bold))
        .await;
    fresh
        .recv_until(|m| matches!(m, ServerMessage::Ack { .. }).then_some(()))
        .await;
    assert_eq!(fresh.text(&blip_id), "hello");
}

// --- signing in through an identity provider -----------------------------
//
// A real provider is stood up on a loopback port rather than mocked behind a
// trait. The failures worth catching in this flow are about what actually
// crosses the wire — the client authentication on the token request, the PKCE
// verifier, the shape of the discovery document — and a mock built from our own
// assumptions would agree with those assumptions. The server reaches it over
// plain HTTP because it is loopback, which is the exception `config` makes.

/// What the fake provider was asked for, so a test can assert on the request
/// and not only on the outcome.
#[derive(Default)]
struct ProviderSeen {
    token_form: Vec<(String, String)>,
    token_auth: Option<String>,
    userinfo_bearer: Option<String>,
}

#[derive(Clone)]
struct FakeProvider {
    base: String,
    /// Overrides the issuer the discovery document claims. Defaults to `base`.
    issuer_claim: Option<String>,
    sub: String,
    preferred_username: Option<String>,
    name: Option<String>,
    seen: std::sync::Arc<std::sync::Mutex<ProviderSeen>>,
}

impl FakeProvider {
    async fn start() -> FakeProvider {
        FakeProvider::start_with(|_| {}).await
    }

    async fn start_with(adjust: impl FnOnce(&mut FakeProvider)) -> FakeProvider {
        use axum::routing::{get, post};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut provider = FakeProvider {
            base: format!("http://127.0.0.1:{}", addr.port()),
            issuer_claim: None,
            sub: "provider-subject-1".to_string(),
            preferred_username: Some("alice".to_string()),
            name: Some("Alice Example".to_string()),
            seen: std::sync::Arc::new(std::sync::Mutex::new(ProviderSeen::default())),
        };
        adjust(&mut provider);

        let app = axum::Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(
                    |axum::extract::State(p): axum::extract::State<FakeProvider>| async move {
                        let b = &p.base;
                        axum::Json(serde_json::json!({
                            "issuer": p.issuer_claim.clone().unwrap_or_else(|| b.clone()),
                            "authorization_endpoint": format!("{b}/authorize"),
                            "token_endpoint": format!("{b}/token"),
                            "userinfo_endpoint": format!("{b}/userinfo"),
                        }))
                    },
                ),
            )
            .route("/token", post(fake_token))
            .route("/userinfo", get(fake_userinfo))
            .with_state(provider.clone());

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        provider
    }
}

async fn fake_token(
    axum::extract::State(provider): axum::extract::State<FakeProvider>,
    headers: axum::http::HeaderMap,
    body: String,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let form: Vec<(String, String)> = body
        .split('&')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (percent_decode(k), percent_decode(v)))
        .collect();
    {
        let mut seen = provider.seen.lock().unwrap();
        seen.token_form = form.clone();
        seen.token_auth = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
    }

    let code = form
        .iter()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.as_str());
    if code != Some("the-code") {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({ "error": "invalid_grant" })),
        )
            .into_response();
    }
    axum::Json(serde_json::json!({
        "access_token": "the-access-token",
        "token_type": "bearer",
        // Present and deliberately unparseable. Nothing reads it, and a test
        // that passed only because this was a well-formed JWT would be
        // testing the wrong thing.
        "id_token": "not.a.jwt",
    }))
    .into_response()
}

async fn fake_userinfo(
    axum::extract::State(provider): axum::extract::State<FakeProvider>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    provider.seen.lock().unwrap().userinfo_bearer = bearer.clone();
    if bearer.as_deref() != Some("the-access-token") {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    let mut claims = serde_json::json!({ "sub": provider.sub });
    if let Some(u) = &provider.preferred_username {
        claims["preferred_username"] = serde_json::json!(u);
    }
    if let Some(n) = &provider.name {
        claims["name"] = serde_json::json!(n);
    }
    axum::Json(claims).into_response()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(
                    std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                    16,
                ) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// A client that does not chase redirects, so a test can read the `Location`
/// and the cookies off the hop itself.
fn no_redirects() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// Start a Gal server pointed at `provider`.
async fn server_with_provider(provider: &FakeProvider) -> TestServer {
    let issuer = provider.base.clone();
    start_server_with(move |config| {
        config.oidc = Some(crate::config::OidcConfig {
            issuer,
            client_id: "the-client".to_string(),
            client_secret: "the-secret".to_string(),
            redirect_url: "http://127.0.0.1:8080/api/oauth/callback".to_string(),
            scopes: "openid profile".to_string(),
            label: "Example".to_string(),
        });
    })
    .await
}

/// The `name=value` pair of a `Set-Cookie` that actually sets something. A
/// cleared cookie has an empty value and is not a credential.
fn cookie_named(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| {
            let pair = v.split(';').next()?;
            let (key, value) = pair.split_once('=')?;
            (key == name && !value.is_empty()).then(|| pair.to_string())
        })
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    query.split('&').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

impl TestServer {
    /// Walk the flow the way a browser would: start, then come back to the
    /// callback with a code. Returns the callback's response.
    async fn sign_in_with_provider(&self, code: &str) -> reqwest::Response {
        let http = no_redirects();
        let started = http
            .get(format!("{}/api/oauth/start", self.base))
            .send()
            .await
            .unwrap();
        assert_eq!(
            started.status(),
            303,
            "start should redirect to the provider"
        );
        let location = started
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let state = query_param(&location, "state").expect("no state in the authorize URL");
        let flow = cookie_named(&started, "gal_oauth").expect("no flow cookie set");

        http.get(format!(
            "{}/api/oauth/callback?code={code}&state={state}",
            self.base
        ))
        .header("cookie", flow)
        .send()
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn a_first_provider_sign_in_creates_an_account_and_a_working_session() {
    let provider = FakeProvider::start().await;
    let server = server_with_provider(&provider).await;

    let done = server.sign_in_with_provider("the-code").await;
    assert_eq!(done.status(), 303, "the callback sends the browser back");
    assert_eq!(done.headers().get("location").unwrap(), "/");

    let session = cookie_named(&done, "gal_session").expect("no session cookie issued");
    // The flow cookie is cleared on the way out, so it cannot be replayed.
    assert!(
        cookie_named(&done, "gal_oauth").is_none(),
        "the flow cookie should be spent"
    );

    // The cookie on its own is a working credential — the client is handed no
    // token, exactly as with a password sign-in.
    let http = reqwest::Client::new();
    let me = http
        .get(format!("{}/api/me", server.base))
        .header("cookie", &session)
        .send()
        .await
        .unwrap();
    assert!(me.status().is_success());
    let body: serde_json::Value = me.json().await.unwrap();
    assert_eq!(
        body["user"]["name"], "alice",
        "the name comes from preferred_username: {body}"
    );
    assert_eq!(body["user"]["displayName"], "Alice Example");
}

#[tokio::test]
async fn the_token_request_authenticates_the_client_and_carries_the_verifier() {
    let provider = FakeProvider::start().await;
    let server = server_with_provider(&provider).await;
    server.sign_in_with_provider("the-code").await;

    let seen = provider.seen.lock().unwrap();
    let field = |k: &str| {
        seen.token_form
            .iter()
            .find(|(f, _)| f == k)
            .map(|(_, v)| v.clone())
    };

    assert_eq!(field("grant_type").as_deref(), Some("authorization_code"));
    assert_eq!(field("code").as_deref(), Some("the-code"));
    assert_eq!(
        field("redirect_uri").as_deref(),
        Some("http://127.0.0.1:8080/api/oauth/callback"),
        "the redirect URI has to be repeated exactly or a conforming provider refuses"
    );
    let verifier = field("code_verifier").expect("no PKCE verifier sent");
    assert!(
        verifier.len() >= 43,
        "expected 256 bits of verifier, got {verifier:?}"
    );

    // client_secret_basic, and the secret never rides in the body.
    let auth = seen.token_auth.clone().expect("no client authentication");
    assert!(auth.starts_with("Basic "), "got {auth:?}");
    assert!(
        !seen.token_form.iter().any(|(k, _)| k == "client_secret"),
        "the secret should authenticate the request, not travel as a form field"
    );
    assert_eq!(seen.userinfo_bearer.as_deref(), Some("the-access-token"));
}

#[tokio::test]
async fn the_same_subject_comes_back_to_the_same_account() {
    let provider = FakeProvider::start().await;
    let server = server_with_provider(&provider).await;

    let first = server.sign_in_with_provider("the-code").await;
    let first_session = cookie_named(&first, "gal_session").unwrap();
    let second = server.sign_in_with_provider("the-code").await;
    let second_session = cookie_named(&second, "gal_session").unwrap();
    assert_ne!(
        first_session, second_session,
        "a distinct session each time"
    );

    assert_eq!(
        server.state.db.user_count().await.unwrap(),
        1,
        "a second sign-in must not create a second account"
    );
}

#[tokio::test]
async fn a_callback_that_did_not_start_here_is_refused() {
    let provider = FakeProvider::start().await;
    let server = server_with_provider(&provider).await;
    let http = no_redirects();

    // No flow cookie at all: somebody pasted a callback URL.
    let bare = http
        .get(format!(
            "{}/api/oauth/callback?code=the-code&state=anything",
            server.base
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(bare.status(), 400);
    assert!(cookie_named(&bare, "gal_session").is_none());

    // A real flow, but a state that is not the one this browser was given —
    // which is what feeding somebody else's authorization code looks like.
    let started = http
        .get(format!("{}/api/oauth/start", server.base))
        .send()
        .await
        .unwrap();
    let flow = cookie_named(&started, "gal_oauth").unwrap();
    let forged = http
        .get(format!(
            "{}/api/oauth/callback?code=the-code&state=not-the-state",
            server.base
        ))
        .header("cookie", flow)
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), 400, "a mismatched state signs nobody in");
    assert!(cookie_named(&forged, "gal_session").is_none());
    assert_eq!(server.state.db.user_count().await.unwrap(), 0);
}

#[tokio::test]
async fn a_callback_cannot_be_replayed() {
    let provider = FakeProvider::start().await;
    let server = server_with_provider(&provider).await;
    let http = no_redirects();

    let started = http
        .get(format!("{}/api/oauth/start", server.base))
        .send()
        .await
        .unwrap();
    let location = started
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let state = query_param(&location, "state").unwrap();
    let flow = cookie_named(&started, "gal_oauth").unwrap();
    let url = format!(
        "{}/api/oauth/callback?code=the-code&state={state}",
        server.base
    );

    let first = http.get(&url).header("cookie", &flow).send().await.unwrap();
    assert_eq!(first.status(), 303);

    // The same URL and cookie again. The flow was consumed, so there is
    // nothing left to match.
    let again = http.get(&url).header("cookie", &flow).send().await.unwrap();
    assert_eq!(again.status(), 400);
    assert!(cookie_named(&again, "gal_session").is_none());
}

#[tokio::test]
async fn a_provider_that_refuses_sends_the_browser_home_without_a_session() {
    let provider = FakeProvider::start().await;
    let server = server_with_provider(&provider).await;
    let http = no_redirects();

    let started = http
        .get(format!("{}/api/oauth/start", server.base))
        .send()
        .await
        .unwrap();
    let flow = cookie_named(&started, "gal_oauth").unwrap();
    // What a provider sends when somebody presses cancel.
    let denied = http
        .get(format!(
            "{}/api/oauth/callback?error=access_denied&error_description=nope",
            server.base
        ))
        .header("cookie", flow)
        .send()
        .await
        .unwrap();

    assert_eq!(denied.status(), 303);
    assert!(
        cookie_named(&denied, "gal_session").is_none(),
        "a refusal must not issue a session"
    );
}

#[tokio::test]
async fn a_bad_code_fails_without_repeating_the_provider_s_complaint() {
    let provider = FakeProvider::start().await;
    let server = server_with_provider(&provider).await;

    let response = server.sign_in_with_provider("wrong-code").await;
    assert_eq!(response.status(), 500);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body["error"], "Something went wrong.",
        "the token endpoint's answer is logged, not forwarded: {body}"
    );
}

#[tokio::test]
async fn a_discovery_document_naming_another_issuer_is_refused() {
    // The issuer is half the primary key of every identity row. If the document
    // disagrees with what was configured, the two would file one subject under
    // two keys.
    let provider =
        FakeProvider::start_with(|p| p.issuer_claim = Some("https://somewhere.else".to_string()))
            .await;
    let server = server_with_provider(&provider).await;

    let started = no_redirects()
        .get(format!("{}/api/oauth/start", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(started.status(), 500);
    assert!(cookie_named(&started, "gal_oauth").is_none());
}

#[tokio::test]
async fn a_provider_name_that_is_not_a_username_still_yields_an_account() {
    let provider = FakeProvider::start_with(|p| {
        p.preferred_username = Some("Ünüsable ✨".to_string());
        p.name = None;
    })
    .await;
    let server = server_with_provider(&provider).await;

    let done = server.sign_in_with_provider("the-code").await;
    assert_eq!(done.status(), 303);
    let session = cookie_named(&done, "gal_session").unwrap();
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/api/me", server.base))
        .header("cookie", session)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["user"]["name"], "nsable",
        "what survives the character class, rather than a refusal: {body}"
    );
}

#[tokio::test]
async fn a_provider_name_already_taken_is_numbered() {
    let provider = FakeProvider::start().await;
    let server = server_with_provider(&provider).await;
    // Somebody already registered `alice` with a password.
    server.register("alice").await;

    let done = server.sign_in_with_provider("the-code").await;
    let session = cookie_named(&done, "gal_session").unwrap();
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/api/me", server.base))
        .header("cookie", session)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["user"]["name"], "alice2");
}

#[tokio::test]
async fn the_login_screen_is_told_whether_a_provider_exists() {
    let provider = FakeProvider::start().await;
    let server = server_with_provider(&provider).await;
    let info: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/api/server", server.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["oidc"]["label"], "Example");

    // With nothing configured the routes are absent rather than disabled, and
    // the client has nothing to draw.
    let plain = start_server().await;
    let info: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/api/server", plain.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(info["oidc"].is_null(), "{info}");

    let started = reqwest::Client::new()
        .get(format!("{}/api/oauth/start", plain.base))
        .send()
        .await
        .unwrap();
    assert_eq!(started.status(), 404);
}
