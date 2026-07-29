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
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn start_server() -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open(dir.path().join("e2e.db")).unwrap();
    let config = Config {
        database: dir.path().join("e2e.db"),
        ..Config::default()
    };
    let state = AppState::new(storage, config);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = crate::http::router(state);

    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    TestServer {
        base: format!("http://{addr}"),
        _dir: dir,
        handle,
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
        let url = self.base.replace("http://", "ws://") + "/ws";
        let mut request = url.into_client_request().unwrap();
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());

        let (socket, _) = connect_async(request)
            .await
            .expect("websocket upgrade failed");
        let mut client = TestClient {
            socket,
            user: PublicUser {
                id: UserId::from(""),
                name: String::new(),
                display_name: String::new(),
                color: 0,
            },
            docs: Default::default(),
            presence: Vec::new(),
            op_counter: 0,
        };

        // The first message is always the greeting.
        match client.recv().await {
            ServerMessage::Welcome { user, .. } => client.user = user,
            other => panic!("expected welcome, got {other:?}"),
        }
        client
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
            self.absorb(&message);
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
                    self.absorb(&parsed);
                }
            }
        }
    }

    /// Apply protocol messages to the local mirror, exactly as the browser does.
    fn absorb(&mut self, message: &ServerMessage) {
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
            ServerMessage::BlipAdded { blip, .. } => {
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
                let Some(state) = self.docs.get_mut(blip_id) else {
                    return;
                };
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
                if let Some(state) = self.docs.get_mut(blip_id) {
                    state.revision = *revision;
                    state.outstanding = state.buffer.take();
                }
            }
            _ => {}
        }
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

    fn text(&self, blip_id: &BlipId) -> String {
        self.docs
            .get(blip_id)
            .map(|d| d.doc.to_plain_text())
            .unwrap_or_default()
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
    client
        .send(ClientMessage::CreateWave {
            title: title.into(),
            participants,
            content: None,
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
    for _ in 0..6 {
        alice.drain().await;
        bob.drain().await;
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
