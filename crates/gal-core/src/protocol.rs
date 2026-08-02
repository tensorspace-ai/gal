//! The WebSocket wire protocol.
//!
//! Messages are JSON tagged with `type`, using camelCase field names so the
//! browser client can consume them without a translation layer.
//!
//! # The concurrency contract
//!
//! Each blip is an independently versioned document. A client submits
//! [`ClientMessage::Submit`] carrying the revision its edit was written
//! against. The server transforms that op over anything committed since,
//! applies it, then:
//!
//! - sends [`ServerMessage::Ack`] to the author, and
//! - sends [`ServerMessage::Op`] with the *transformed* op to everyone else.
//!
//! An author never receives an echo of their own op. Both messages advance the
//! recipient's revision by exactly one, and the server emits them in commit
//! order, so every client's revision counter tracks the server's.

use serde::{Deserialize, Serialize};

use crate::model::*;
use gal_ot::Delta;

// --- views sent to clients ---------------------------------------------

/// A wavelet plus its blips, as delivered on subscribe.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaveletView {
    pub id: WaveletId,
    pub wave_id: WaveId,
    pub kind: WaveletKind,
    pub title: String,
    pub participants: Vec<PublicUser>,
    pub anchor_blip: Option<BlipId>,
    pub created_at: Timestamp,
    pub last_modified: Timestamp,
    pub blips: Vec<BlipView>,
    /// Comment threads anchored in this wavelet's blips. Their remarks are in
    /// `blips` like any other, tagged with the thread they belong to; this list
    /// only carries the thread state that has nowhere else to live.
    #[serde(default)]
    pub comments: Vec<CommentThread>,
}

/// A blip document with everything needed to render and edit it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlipView {
    pub id: BlipId,
    pub wavelet_id: WaveletId,
    pub parent: Option<BlipId>,
    /// Set when this blip is a remark in a comment thread rather than part of
    /// the conversation.
    #[serde(default)]
    pub comment: Option<CommentId>,
    pub seq: i64,
    pub author: UserId,
    pub contributors: Vec<UserId>,
    pub created_at: Timestamp,
    pub last_modified: Timestamp,
    pub content: Delta,
    pub revision: u64,
    /// Whether the viewer has seen the blip at its current revision.
    pub unread: bool,
}

/// A full wave, scoped to what the requesting user may see.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaveView {
    pub id: WaveId,
    pub creator: UserId,
    pub created_at: Timestamp,
    /// Only wavelets the viewer participates in. Private replies belonging to
    /// others are filtered out server-side and never reach the client.
    pub wavelets: Vec<WaveletView>,
    pub flags: WaveFlags,
    /// What participants may do here, and how the client should present it.
    #[serde(default)]
    pub mode: WaveMode,
}

/// One row in the inbox.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaveSummary {
    pub id: WaveId,
    pub title: String,
    pub participants: Vec<PublicUser>,
    pub last_modified: Timestamp,
    /// Author and first line of the most recent blip.
    pub snippet: String,
    pub snippet_author: Option<UserId>,
    pub blip_count: usize,
    pub unread_count: usize,
    pub flags: WaveFlags,
}

/// Someone currently viewing a wave.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PresenceEntry {
    pub user: PublicUser,
    /// The blip they are editing, if any.
    pub editing: Option<BlipId>,
}

/// One step in a wave's history, for playback.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackFrame {
    pub blip_id: BlipId,
    pub revision: u64,
    pub author: UserId,
    pub timestamp: Timestamp,
    pub delta: Delta,
    /// Set when this frame is the blip's creation rather than an edit.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub created: bool,
}

/// A search hit.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub wave_id: WaveId,
    pub blip_id: BlipId,
    pub title: String,
    pub snippet: String,
    pub author: UserId,
    pub timestamp: Timestamp,
}

// --- client to server ---------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum ClientMessage {
    /// Start receiving live updates for a wave, and get its current state.
    #[serde(rename_all = "camelCase")]
    Open {
        wave_id: WaveId,
    },
    /// Stop receiving updates.
    #[serde(rename_all = "camelCase")]
    Close {
        wave_id: WaveId,
    },

    /// Start a new wave. `participants` are usernames; the creator is implicit.
    #[serde(rename_all = "camelCase")]
    CreateWave {
        title: String,
        #[serde(default)]
        participants: Vec<String>,
        /// Optional text for the first blip, so "reply and send" is one round trip.
        #[serde(default)]
        content: Option<Delta>,
        /// Defaults to `Document`, which is how every wave behaved before modes
        /// existed.
        #[serde(default)]
        mode: Option<WaveMode>,
    },

    /// Submit an edit to a blip, written against `revision`.
    #[serde(rename_all = "camelCase")]
    Submit {
        blip_id: BlipId,
        revision: u64,
        delta: Delta,
        /// Unique per submitted op. Makes submission idempotent: after a
        /// reconnect a client replays work it never saw acknowledged, and
        /// without an id the server cannot tell a genuine retry from a new edit,
        /// so the same text would be applied twice. Optional for compatibility;
        /// omitting it restores the old at-least-once behaviour.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        op_id: Option<String>,
    },

    /// Add a blip. `parent` nests it as a reply; `None` appends at root level.
    #[serde(rename_all = "camelCase")]
    CreateBlip {
        wavelet_id: WaveletId,
        #[serde(default)]
        parent: Option<BlipId>,
        #[serde(default)]
        content: Option<Delta>,
    },

    #[serde(rename_all = "camelCase")]
    DeleteBlip {
        blip_id: BlipId,
    },

    #[serde(rename_all = "camelCase")]
    SetTitle {
        wavelet_id: WaveletId,
        title: String,
    },

    /// Change how a wave behaves. Only its creator may do this.
    #[serde(rename_all = "camelCase")]
    SetMode {
        wave_id: WaveId,
        mode: WaveMode,
    },

    /// Add someone by username.
    #[serde(rename_all = "camelCase")]
    AddParticipant {
        wavelet_id: WaveletId,
        name: String,
    },

    #[serde(rename_all = "camelCase")]
    RemoveParticipant {
        wavelet_id: WaveletId,
        user_id: UserId,
    },

    /// Open a comment thread on a range of a blip's text.
    ///
    /// The range itself is not named here. The client applies a
    /// [`COMMENT_ATTRIBUTE`] run over the text in an ordinary
    /// [`Submit`](Self::Submit), which is what makes the anchor move as the page
    /// is edited; this message only creates the thread that run points at. The
    /// client picks `comment_id` so it can do both against one revision.
    #[serde(rename_all = "camelCase")]
    CreateComment {
        wavelet_id: WaveletId,
        /// The blip the anchored range lives in.
        blip_id: BlipId,
        comment_id: CommentId,
        /// Text of the first remark, so opening a thread is one round trip.
        #[serde(default)]
        content: Option<Delta>,
    },

    /// Add a further remark to an open thread.
    #[serde(rename_all = "camelCase")]
    ReplyToComment {
        comment_id: CommentId,
        #[serde(default)]
        content: Option<Delta>,
    },

    /// Close a thread, or reopen a closed one.
    ///
    /// Nothing is deleted either way: the anchor and every remark stay exactly
    /// where they are, so this only changes how the thread is drawn.
    #[serde(rename_all = "camelCase")]
    ResolveComment {
        comment_id: CommentId,
        resolved: bool,
    },

    /// Branch a private side conversation off a blip.
    #[serde(rename_all = "camelCase")]
    PrivateReply {
        wavelet_id: WaveletId,
        anchor: BlipId,
        #[serde(default)]
        participants: Vec<String>,
    },

    /// Report caret position so peers can render remote cursors.
    #[serde(rename_all = "camelCase")]
    Cursor {
        wave_id: WaveId,
        blip_id: BlipId,
        index: usize,
        length: usize,
    },

    /// Mark every blip in a wave as read up to its current revision.
    #[serde(rename_all = "camelCase")]
    MarkRead {
        wave_id: WaveId,
    },

    #[serde(rename_all = "camelCase")]
    SetFlags {
        wave_id: WaveId,
        flags: WaveFlags,
    },

    /// Ask for the full edit history of a wave.
    #[serde(rename_all = "camelCase")]
    RequestPlayback {
        wave_id: WaveId,
    },

    /// Full-text search across every wave the user participates in.
    Search {
        query: String,
    },

    Ping,
}

impl ClientMessage {
    /// The command's name on the wire, for logging, metrics and rate limiting.
    ///
    /// Deliberately not `Debug`: that would print the message's *contents*, and
    /// these carry the text of private conversations.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Open { .. } => "open",
            Self::Close { .. } => "close",
            Self::CreateWave { .. } => "createWave",
            Self::Submit { .. } => "submit",
            Self::CreateBlip { .. } => "createBlip",
            Self::DeleteBlip { .. } => "deleteBlip",
            Self::SetTitle { .. } => "setTitle",
            Self::SetMode { .. } => "setMode",
            Self::AddParticipant { .. } => "addParticipant",
            Self::RemoveParticipant { .. } => "removeParticipant",
            Self::CreateComment { .. } => "createComment",
            Self::ReplyToComment { .. } => "replyToComment",
            Self::ResolveComment { .. } => "resolveComment",
            Self::PrivateReply { .. } => "privateReply",
            Self::Cursor { .. } => "cursor",
            Self::MarkRead { .. } => "markRead",
            Self::SetFlags { .. } => "setFlags",
            Self::RequestPlayback { .. } => "requestPlayback",
            Self::Search { .. } => "search",
            Self::Ping => "ping",
        }
    }
}

// --- server to client ---------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServerMessage {
    /// First message on every connection.
    #[serde(rename_all = "camelCase")]
    Welcome {
        user: PublicUser,
        inbox: Vec<WaveSummary>,
    },

    /// Full state of a wave, in response to [`ClientMessage::Open`].
    #[serde(rename_all = "camelCase")]
    WaveState {
        wave: WaveView,
    },

    /// A committed op from another participant, already transformed into the
    /// recipient's frame of reference. Advances the recipient's revision by one.
    #[serde(rename_all = "camelCase")]
    Op {
        wave_id: WaveId,
        blip_id: BlipId,
        revision: u64,
        author: UserId,
        delta: Delta,
    },

    /// Confirmation of the recipient's own op. Also advances revision by one.
    /// Carries the transformed form, which the client needs when the server
    /// rebased the op onto concurrent edits.
    #[serde(rename_all = "camelCase")]
    Ack {
        wave_id: WaveId,
        blip_id: BlipId,
        revision: u64,
        delta: Delta,
        /// Echoes the submitted `op_id`, so a client can match an
        /// acknowledgement to the op it sent — including a replayed one the
        /// server recognised as already applied.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        op_id: Option<String>,
    },

    #[serde(rename_all = "camelCase")]
    BlipAdded {
        wave_id: WaveId,
        blip: BlipView,
    },

    #[serde(rename_all = "camelCase")]
    BlipRemoved {
        wave_id: WaveId,
        blip_id: BlipId,
    },

    /// A comment thread was opened.
    ///
    /// Carries its first remark rather than leaving that to a following
    /// [`BlipAdded`](Self::BlipAdded): a thread with nothing in it is a state no
    /// recipient should ever have to draw. Later remarks do arrive as ordinary
    /// `BlipAdded`, tagged with the thread.
    #[serde(rename_all = "camelCase")]
    CommentAdded {
        wave_id: WaveId,
        comment: CommentThread,
        blip: BlipView,
    },

    /// A thread was closed, or reopened. `resolved_by` is `None` when reopened.
    #[serde(rename_all = "camelCase")]
    CommentResolved {
        wave_id: WaveId,
        comment_id: CommentId,
        resolved_by: Option<UserId>,
        resolved_at: Option<Timestamp>,
    },

    /// The wave's mode changed. Recipients must re-render, and drop any local
    /// edits the new mode no longer permits.
    #[serde(rename_all = "camelCase")]
    ModeChanged {
        wave_id: WaveId,
        mode: WaveMode,
    },

    #[serde(rename_all = "camelCase")]
    TitleChanged {
        wave_id: WaveId,
        wavelet_id: WaveletId,
        title: String,
    },

    #[serde(rename_all = "camelCase")]
    ParticipantAdded {
        wave_id: WaveId,
        wavelet_id: WaveletId,
        user: PublicUser,
    },

    #[serde(rename_all = "camelCase")]
    ParticipantRemoved {
        wave_id: WaveId,
        wavelet_id: WaveletId,
        user_id: UserId,
    },

    /// A new wavelet became visible — in practice, a private reply the
    /// recipient was included in.
    #[serde(rename_all = "camelCase")]
    WaveletAdded {
        wave_id: WaveId,
        wavelet: WaveletView,
    },

    /// An inbox row changed. Sent to participants who are not currently viewing
    /// the wave, so their inbox stays live.
    #[serde(rename_all = "camelCase")]
    InboxUpdated {
        summary: WaveSummary,
    },

    #[serde(rename_all = "camelCase")]
    WaveRemoved {
        wave_id: WaveId,
    },

    #[serde(rename_all = "camelCase")]
    Presence {
        wave_id: WaveId,
        users: Vec<PresenceEntry>,
    },

    #[serde(rename_all = "camelCase")]
    Cursor {
        wave_id: WaveId,
        blip_id: BlipId,
        user_id: UserId,
        index: usize,
        length: usize,
    },

    #[serde(rename_all = "camelCase")]
    Playback {
        wave_id: WaveId,
        frames: Vec<PlaybackFrame>,
    },

    #[serde(rename_all = "camelCase")]
    SearchResults {
        query: String,
        hits: Vec<SearchHit>,
    },

    /// Something went wrong. `blip_id` is set when the failure was a rejected
    /// op, which tells the client which document to resynchronise.
    #[serde(rename_all = "camelCase")]
    Error {
        code: ErrorCode,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        blip_id: Option<BlipId>,
    },

    Pong,
}

/// Machine-readable failure reasons, so the client can react rather than just
/// display a string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCode {
    /// The user is not a participant, or the object does not exist. These are
    /// deliberately the same code: distinguishing them would leak the existence
    /// of waves the user cannot see.
    NotFound,
    /// The request was malformed.
    BadRequest,
    /// The client's op could not be transformed; it must reopen the wave to
    /// resynchronise.
    Resync,
    /// The action is not allowed for this user.
    Forbidden,
    Internal,
}

impl ServerMessage {
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        ServerMessage::Error {
            code,
            message: message.into(),
            blip_id: None,
        }
    }

    pub fn resync(blip_id: BlipId, message: impl Into<String>) -> Self {
        ServerMessage::Error {
            code: ErrorCode::Resync,
            message: message.into(),
            blip_id: Some(blip_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_messages_use_tagged_camel_case() {
        let msg = ClientMessage::Submit {
            blip_id: BlipId::from("b-1"),
            revision: 7,
            delta: Delta::new().retain(3).insert("hi"),
            op_id: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"type":"submit","blipId":"b-1","revision":7,"delta":{"ops":[{"retain":3},{"insert":"hi"}]}}"#
        );
    }

    #[test]
    fn every_client_message_round_trips() {
        let cases = vec![
            ClientMessage::Open {
                wave_id: WaveId::from("w-1"),
            },
            ClientMessage::Close {
                wave_id: WaveId::from("w-1"),
            },
            ClientMessage::CreateWave {
                title: "Launch plan".into(),
                participants: vec!["bob".into()],
                content: Some(Delta::document("hello")),
                mode: Some(WaveMode::Chat),
            },
            ClientMessage::CreateBlip {
                wavelet_id: WaveletId::from("s-1"),
                parent: Some(BlipId::from("b-1")),
                content: None,
            },
            ClientMessage::DeleteBlip {
                blip_id: BlipId::from("b-2"),
            },
            ClientMessage::SetTitle {
                wavelet_id: WaveletId::from("s-1"),
                title: "T".into(),
            },
            ClientMessage::SetMode {
                wave_id: WaveId::from("w-1"),
                mode: WaveMode::Chat,
            },
            ClientMessage::AddParticipant {
                wavelet_id: WaveletId::from("s-1"),
                name: "carol".into(),
            },
            ClientMessage::PrivateReply {
                wavelet_id: WaveletId::from("s-1"),
                anchor: BlipId::from("b-1"),
                participants: vec!["dave".into()],
            },
            ClientMessage::CreateComment {
                wavelet_id: WaveletId::from("s-1"),
                blip_id: BlipId::from("b-1"),
                comment_id: CommentId::from("c-1"),
                content: Some(Delta::document("this line reads oddly")),
            },
            ClientMessage::ReplyToComment {
                comment_id: CommentId::from("c-1"),
                content: None,
            },
            ClientMessage::ResolveComment {
                comment_id: CommentId::from("c-1"),
                resolved: true,
            },
            ClientMessage::Cursor {
                wave_id: WaveId::from("w-1"),
                blip_id: BlipId::from("b-1"),
                index: 4,
                length: 2,
            },
            ClientMessage::MarkRead {
                wave_id: WaveId::from("w-1"),
            },
            ClientMessage::Search {
                query: "launch".into(),
            },
            ClientMessage::Submit {
                blip_id: BlipId::from("b-1"),
                revision: 3,
                delta: Delta::new().retain(1).insert("x"),
                op_id: Some("alice-1".into()),
            },
            ClientMessage::RemoveParticipant {
                wavelet_id: WaveletId::from("s-1"),
                user_id: UserId::from("u-1"),
            },
            ClientMessage::SetFlags {
                wave_id: WaveId::from("w-1"),
                flags: WaveFlags::default(),
            },
            ClientMessage::RequestPlayback {
                wave_id: WaveId::from("w-1"),
            },
            ClientMessage::Ping,
        ];
        // Every variant must appear above, or the two assertions below only
        // cover the ones somebody remembered.
        let mut seen = std::collections::HashSet::new();
        for case in cases {
            let json = serde_json::to_string(&case).unwrap();
            let back: ClientMessage = serde_json::from_str(&json).unwrap();
            assert_eq!(json, serde_json::to_string(&back).unwrap());

            // `name()` is what logs, metrics and rate limits are keyed on. If it
            // drifts from the tag the client actually sends, every one of those
            // silently starts describing the wrong command.
            let tag = serde_json::from_str::<serde_json::Value>(&json).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string();
            assert_eq!(case.name(), tag, "name() disagrees with the wire tag");
            seen.insert(tag);
        }
        assert_eq!(
            seen.len(),
            20,
            "a ClientMessage variant is missing from this test: {seen:?}"
        );
    }

    #[test]
    fn optional_fields_may_be_omitted_by_the_client() {
        // Keeps the browser client from having to send explicit nulls.
        let msg: ClientMessage =
            serde_json::from_str(r#"{"type":"createWave","title":"Hi"}"#).unwrap();
        match msg {
            ClientMessage::CreateWave {
                participants,
                content,
                ..
            } => {
                assert!(participants.is_empty());
                assert!(content.is_none());
            }
            _ => panic!("wrong variant"),
        }

        let msg: ClientMessage =
            serde_json::from_str(r#"{"type":"createBlip","waveletId":"s-1"}"#).unwrap();
        assert!(matches!(
            msg,
            ClientMessage::CreateBlip { parent: None, .. }
        ));
    }

    #[test]
    fn server_messages_round_trip() {
        let msg = ServerMessage::Op {
            wave_id: WaveId::from("w-1"),
            blip_id: BlipId::from("b-1"),
            revision: 3,
            author: UserId::from("u-a"),
            delta: Delta::new().retain(1).delete(2),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: ServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(json, serde_json::to_string(&back).unwrap());
    }

    #[test]
    fn unknown_message_types_are_rejected_not_ignored() {
        assert!(serde_json::from_str::<ClientMessage>(r#"{"type":"dropTables"}"#).is_err());
    }

    /// An unknown *field* was dropped where an unknown *type* was refused, and
    /// every one of these fields defaults — so the message succeeded and did
    /// something other than what was asked. Nothing tells the sender.
    #[test]
    fn unknown_message_fields_are_rejected_too() {
        for json in [
            // Was: a wave created with only its creator in it.
            r#"{"type":"createWave","title":"t","partcipants":["bob"]}"#,
            // Was: a Document wave — fully editable — where Frozen was asked for.
            r#"{"type":"createWave","title":"t","moed":"frozen"}"#,
            // Was: idempotency silently off, so a reconnect applied the edit twice.
            r#"{"type":"submit","waveId":"w-1","blipId":"b-1","revision":1,"delta":{"ops":[]},"op_id":"x"}"#,
            // Was: a reply at root level instead of nested under its parent.
            r#"{"type":"addBlip","waveId":"w-1","parnet":"b-1","content":{"ops":[]}}"#,
        ] {
            assert!(
                serde_json::from_str::<ClientMessage>(json).is_err(),
                "should be refused: {json}"
            );
        }
    }

    /// The shipped client sends these, so the strictness must not cost the
    /// spellings that were always right.
    #[test]
    fn correctly_spelled_messages_still_parse() {
        for json in [
            r#"{"type":"createWave","title":"t","participants":["bob"],"mode":"frozen"}"#,
            r#"{"type":"createWave","title":"t"}"#,
            r#"{"type":"open","waveId":"w-1"}"#,
        ] {
            assert!(
                serde_json::from_str::<ClientMessage>(json).is_ok(),
                "should parse: {json}"
            );
        }
    }

    /// Collect every JSON object key appearing anywhere in a value.
    fn all_keys(value: &serde_json::Value, into: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    into.push(key.clone());
                    all_keys(child, into);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    all_keys(item, into);
                }
            }
            _ => {}
        }
    }

    /// Every field the browser sees must be camelCase.
    ///
    /// The container-level `rename_all` only renames *variants*; fields need the
    /// attribute repeated on each variant. Forgetting it is invisible to a Rust
    /// round-trip test — both sides agree on the wrong name — and only shows up
    /// as a rejected message from the real client. This asserts the wire shape
    /// directly instead.
    #[test]
    fn no_message_field_is_snake_case() {
        let mut json = Vec::new();

        for message in sample_client_messages() {
            json.push(serde_json::to_value(&message).unwrap());
        }
        for message in sample_server_messages() {
            json.push(serde_json::to_value(&message).unwrap());
        }

        let mut offenders = Vec::new();
        for value in &json {
            let mut keys = Vec::new();
            all_keys(value, &mut keys);
            for key in keys {
                // Attribute maps carry user-defined keys; only protocol field
                // names are covered by this rule.
                if key.contains('_') && !matches!(key.as_str(), "bold" | "italic") {
                    offenders.push(format!("{key} in {value}"));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "snake_case leaked onto the wire:\n{}",
            offenders.join("\n")
        );
    }

    fn sample_client_messages() -> Vec<ClientMessage> {
        vec![
            ClientMessage::Open {
                wave_id: WaveId::from("w-1"),
            },
            ClientMessage::Close {
                wave_id: WaveId::from("w-1"),
            },
            ClientMessage::CreateWave {
                title: "T".into(),
                participants: vec!["bob".into()],
                content: Some(Delta::document("hi")),
                mode: None,
            },
            ClientMessage::Submit {
                blip_id: BlipId::from("b-1"),
                revision: 1,
                delta: Delta::new(),
                op_id: Some("op-1".into()),
            },
            ClientMessage::CreateBlip {
                wavelet_id: WaveletId::from("s-1"),
                parent: Some(BlipId::from("b-1")),
                content: None,
            },
            ClientMessage::DeleteBlip {
                blip_id: BlipId::from("b-1"),
            },
            ClientMessage::SetTitle {
                wavelet_id: WaveletId::from("s-1"),
                title: "T".into(),
            },
            ClientMessage::SetMode {
                wave_id: WaveId::from("w-1"),
                mode: WaveMode::Chat,
            },
            ClientMessage::AddParticipant {
                wavelet_id: WaveletId::from("s-1"),
                name: "bob".into(),
            },
            ClientMessage::RemoveParticipant {
                wavelet_id: WaveletId::from("s-1"),
                user_id: UserId::from("u-1"),
            },
            ClientMessage::PrivateReply {
                wavelet_id: WaveletId::from("s-1"),
                anchor: BlipId::from("b-1"),
                participants: vec![],
            },
            ClientMessage::CreateComment {
                wavelet_id: WaveletId::from("s-1"),
                blip_id: BlipId::from("b-1"),
                comment_id: CommentId::from("c-1"),
                content: None,
            },
            ClientMessage::ReplyToComment {
                comment_id: CommentId::from("c-1"),
                content: None,
            },
            ClientMessage::ResolveComment {
                comment_id: CommentId::from("c-1"),
                resolved: true,
            },
            ClientMessage::Cursor {
                wave_id: WaveId::from("w-1"),
                blip_id: BlipId::from("b-1"),
                index: 0,
                length: 0,
            },
            ClientMessage::MarkRead {
                wave_id: WaveId::from("w-1"),
            },
            ClientMessage::SetFlags {
                wave_id: WaveId::from("w-1"),
                flags: WaveFlags::default(),
            },
            ClientMessage::RequestPlayback {
                wave_id: WaveId::from("w-1"),
            },
            ClientMessage::Search { query: "x".into() },
            ClientMessage::Ping,
        ]
    }

    fn sample_server_messages() -> Vec<ServerMessage> {
        let user = PublicUser {
            id: UserId::from("u-1"),
            name: "bob".into(),
            display_name: "Bob".into(),
            color: 1,
        };
        let blip = BlipView {
            id: BlipId::from("b-1"),
            wavelet_id: WaveletId::from("s-1"),
            parent: None,
            comment: None,
            seq: 0,
            author: UserId::from("u-1"),
            contributors: vec![],
            created_at: 0,
            last_modified: 0,
            content: Delta::new(),
            revision: 0,
            unread: false,
        };
        let thread = CommentThread {
            id: CommentId::from("c-1"),
            wavelet_id: WaveletId::from("s-1"),
            blip_id: BlipId::from("b-1"),
            author: UserId::from("u-1"),
            created_at: 0,
            resolved_by: None,
            resolved_at: None,
        };
        let wavelet = WaveletView {
            id: WaveletId::from("s-1"),
            wave_id: WaveId::from("w-1"),
            kind: WaveletKind::Conversation,
            title: "T".into(),
            participants: vec![user.clone()],
            anchor_blip: None,
            created_at: 0,
            last_modified: 0,
            blips: vec![blip.clone()],
            comments: vec![thread.clone()],
        };
        let summary = WaveSummary {
            id: WaveId::from("w-1"),
            title: "T".into(),
            participants: vec![user.clone()],
            last_modified: 0,
            snippet: String::new(),
            snippet_author: None,
            blip_count: 0,
            unread_count: 0,
            flags: WaveFlags::default(),
        };

        vec![
            ServerMessage::Welcome {
                user: user.clone(),
                inbox: vec![summary.clone()],
            },
            ServerMessage::WaveState {
                wave: WaveView {
                    id: WaveId::from("w-1"),
                    creator: UserId::from("u-1"),
                    created_at: 0,
                    wavelets: vec![wavelet.clone()],
                    flags: WaveFlags::default(),
                    mode: WaveMode::Chat,
                },
            },
            ServerMessage::Op {
                wave_id: WaveId::from("w-1"),
                blip_id: BlipId::from("b-1"),
                revision: 1,
                author: UserId::from("u-1"),
                delta: Delta::new(),
            },
            ServerMessage::Ack {
                wave_id: WaveId::from("w-1"),
                blip_id: BlipId::from("b-1"),
                revision: 1,
                delta: Delta::new(),
                op_id: Some("op-1".into()),
            },
            ServerMessage::BlipAdded {
                wave_id: WaveId::from("w-1"),
                blip: blip.clone(),
            },
            ServerMessage::BlipRemoved {
                wave_id: WaveId::from("w-1"),
                blip_id: BlipId::from("b-1"),
            },
            ServerMessage::CommentAdded {
                wave_id: WaveId::from("w-1"),
                comment: thread,
                blip: blip.clone(),
            },
            ServerMessage::CommentResolved {
                wave_id: WaveId::from("w-1"),
                comment_id: CommentId::from("c-1"),
                resolved_by: Some(UserId::from("u-1")),
                resolved_at: Some(0),
            },
            ServerMessage::TitleChanged {
                wave_id: WaveId::from("w-1"),
                wavelet_id: WaveletId::from("s-1"),
                title: "T".into(),
            },
            ServerMessage::ModeChanged {
                wave_id: WaveId::from("w-1"),
                mode: WaveMode::Frozen,
            },
            ServerMessage::ParticipantAdded {
                wave_id: WaveId::from("w-1"),
                wavelet_id: WaveletId::from("s-1"),
                user: user.clone(),
            },
            ServerMessage::ParticipantRemoved {
                wave_id: WaveId::from("w-1"),
                wavelet_id: WaveletId::from("s-1"),
                user_id: UserId::from("u-1"),
            },
            ServerMessage::WaveletAdded {
                wave_id: WaveId::from("w-1"),
                wavelet,
            },
            ServerMessage::InboxUpdated { summary },
            ServerMessage::WaveRemoved {
                wave_id: WaveId::from("w-1"),
            },
            ServerMessage::Presence {
                wave_id: WaveId::from("w-1"),
                users: vec![PresenceEntry {
                    user,
                    editing: None,
                }],
            },
            ServerMessage::Cursor {
                wave_id: WaveId::from("w-1"),
                blip_id: BlipId::from("b-1"),
                user_id: UserId::from("u-1"),
                index: 0,
                length: 0,
            },
            ServerMessage::Playback {
                wave_id: WaveId::from("w-1"),
                frames: vec![PlaybackFrame {
                    blip_id: BlipId::from("b-1"),
                    revision: 1,
                    author: UserId::from("u-1"),
                    timestamp: 0,
                    delta: Delta::new(),
                    created: true,
                }],
            },
            ServerMessage::SearchResults {
                query: "x".into(),
                hits: vec![SearchHit {
                    wave_id: WaveId::from("w-1"),
                    blip_id: BlipId::from("b-1"),
                    title: "T".into(),
                    snippet: String::new(),
                    author: UserId::from("u-1"),
                    timestamp: 0,
                }],
            },
            ServerMessage::error(ErrorCode::NotFound, "nope"),
            ServerMessage::Pong,
        ]
    }

    #[test]
    fn error_codes_serialise_as_stable_strings() {
        let json =
            serde_json::to_string(&ServerMessage::error(ErrorCode::NotFound, "nope")).unwrap();
        assert!(json.contains(r#""code":"notFound""#), "got {json}");
    }
}
