//! The Wave domain model.
//!
//! The hierarchy is the one Google Wave introduced, and it is what separates a
//! wave from a chat room:
//!
//! ```text
//! Wave                       a conversation, the unit that appears in your inbox
//! └── Wavelet                a participant set + a threaded document
//!     ├── conv+root          the main conversation everyone in it can see
//!     └── conv+<id>          a private reply: fewer participants, anchored to a blip
//!         └── Blip           one message, itself a live collaborative document
//! ```
//!
//! Access control lives on the *wavelet*, not the wave. That is what makes
//! private replies work: a wave can hold a public thread and a side conversation
//! that only two of its participants can see, and the server simply never sends
//! the latter to anyone else.

use serde::{Deserialize, Serialize};

/// Milliseconds since the Unix epoch.
///
/// Chosen over a richer time type because it round-trips through JSON into a
/// JavaScript `Date` with no conversion and no precision surprises.
pub type Timestamp = i64;

/// Current wall-clock time in milliseconds since the epoch.
pub fn now() -> Timestamp {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Defines a string newtype so the different kinds of identifier cannot be
/// silently swapped for one another at a call site.
macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// Mint a fresh, globally unique identifier.
            ///
            /// Deliberately not a `Default` impl: `Default` reads as "empty",
            /// and an identifier that silently invents itself where a caller
            /// expected a placeholder is a bug waiting to happen.
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self {
                $name(format!("{}{}", $prefix, uuid::Uuid::new_v4().simple()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                $name(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                $name(s.to_string())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

id_type!(/// Identifies a person.
    UserId, "u-");
id_type!(/// Identifies a wave — the thing that appears in an inbox.
    WaveId, "w-");
id_type!(/// Identifies a wavelet — a participant set plus its threaded blips.
    WaveletId, "s-");
id_type!(/// Identifies a blip — a single message document.
    BlipId, "b-");

// --- users --------------------------------------------------------------

/// A registered account, including material never sent to clients.
#[derive(Clone, Debug)]
pub struct User {
    pub id: UserId,
    /// Unique login handle, lowercased.
    pub name: String,
    pub display_name: String,
    pub email: String,
    pub password_hash: String,
    /// Stable hue (0–360) used to colour avatars and cursors consistently.
    pub color: u16,
    pub created_at: Timestamp,
}

impl User {
    /// The projection safe to send to other participants.
    pub fn public(&self) -> PublicUser {
        PublicUser {
            id: self.id.clone(),
            name: self.name.clone(),
            display_name: self.display_name.clone(),
            color: self.color,
        }
    }
}

/// The subset of a user that is safe to share. Notably excludes the email
/// address and password hash.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicUser {
    pub id: UserId,
    pub name: String,
    pub display_name: String,
    pub color: u16,
}

/// Derive a stable colour from a name, so a user looks the same to everyone
/// without needing to store a preference.
pub fn color_for(name: &str) -> u16 {
    // FNV-1a, folded into the hue circle.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    (hash % 360) as u16
}

// --- waves --------------------------------------------------------------

/// A conversation. Holds no content itself; it groups wavelets.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Wave {
    pub id: WaveId,
    pub creator: UserId,
    pub created_at: Timestamp,
}

/// Why a wavelet exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WaveletKind {
    /// The main thread. Every wave has exactly one.
    Conversation,
    /// A side conversation visible only to its own participants, anchored to
    /// the blip it was started from.
    PrivateReply,
}

/// A participant set plus the threaded blips they share.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Wavelet {
    pub id: WaveletId,
    pub wave_id: WaveId,
    pub kind: WaveletKind,
    pub title: String,
    /// Everyone who can see this wavelet. Membership *is* the access rule.
    pub participants: Vec<UserId>,
    /// For a private reply, the blip in the parent wavelet it hangs off.
    pub anchor_blip: Option<BlipId>,
    pub created_at: Timestamp,
    pub last_modified: Timestamp,
}

impl Wavelet {
    pub fn has_participant(&self, user: &UserId) -> bool {
        self.participants.iter().any(|p| p == user)
    }
}

/// A single message: an independently versioned collaborative document.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Blip {
    pub id: BlipId,
    pub wavelet_id: WaveletId,
    pub wave_id: WaveId,
    /// The blip this one replies to. `None` makes it a root-level blip.
    pub parent: Option<BlipId>,
    /// Ordering among siblings; monotonic within a wavelet.
    pub seq: i64,
    pub author: UserId,
    /// Everyone who has edited this blip, in first-contribution order. Wave
    /// showed these as stacked avatars, which is how you could tell a message
    /// had been jointly written.
    pub contributors: Vec<UserId>,
    pub created_at: Timestamp,
    pub last_modified: Timestamp,
    /// The document itself.
    pub content: gal_ot::Delta,
    /// How many ops have been applied. Clients submit against this.
    pub revision: u64,
    pub deleted: bool,
}

impl Blip {
    pub fn new(
        wave_id: WaveId,
        wavelet_id: WaveletId,
        author: UserId,
        parent: Option<BlipId>,
        seq: i64,
    ) -> Self {
        let ts = now();
        Blip {
            id: BlipId::new(),
            wavelet_id,
            wave_id,
            parent,
            seq,
            contributors: vec![author.clone()],
            author,
            created_at: ts,
            last_modified: ts,
            content: gal_ot::Delta::new(),
            revision: 0,
            deleted: false,
        }
    }

    /// Record an edit, keeping contributors in first-contribution order.
    pub fn record_contributor(&mut self, user: &UserId) {
        if !self.contributors.iter().any(|c| c == user) {
            self.contributors.push(user.clone());
        }
        self.last_modified = now();
    }

    /// First line of the document, for inbox previews and search snippets.
    pub fn preview(&self, max_chars: usize) -> String {
        let text = self.content.to_plain_text();
        let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        let trimmed = line.trim();
        if trimmed.chars().count() <= max_chars {
            trimmed.to_string()
        } else {
            let cut: String = trimmed.chars().take(max_chars).collect();
            format!("{}…", cut.trim_end())
        }
    }
}

/// Tracks how far a user has read into a blip, so unread counts survive edits:
/// a blip becomes unread again when someone revises it past this revision.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadMark {
    pub user_id: UserId,
    pub blip_id: BlipId,
    pub revision: u64,
}

/// Per-user, per-wave flags that are not shared with other participants.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaveFlags {
    pub archived: bool,
    pub muted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_unique_and_prefixed() {
        let a = WaveId::new();
        let b = WaveId::new();
        assert_ne!(a, b);
        assert!(a.as_str().starts_with("w-"));
        assert!(BlipId::new().as_str().starts_with("b-"));
    }

    #[test]
    fn colour_is_stable_and_in_range() {
        assert_eq!(color_for("alice"), color_for("alice"));
        assert_ne!(color_for("alice"), color_for("bob"));
        for name in ["alice", "bob", "carol", "dave", "erin"] {
            assert!(color_for(name) < 360);
        }
    }

    #[test]
    fn public_projection_omits_credentials() {
        let user = User {
            id: UserId::new(),
            name: "alice".into(),
            display_name: "Alice".into(),
            email: "alice@example.com".into(),
            password_hash: "$argon2id$secret".into(),
            color: 42,
            created_at: now(),
        };
        let json = serde_json::to_string(&user.public()).unwrap();
        assert!(
            !json.contains("argon2"),
            "password hash must never be serialised"
        );
        assert!(
            !json.contains("example.com"),
            "email must not leak to peers"
        );
        assert!(json.contains("Alice"));
    }

    #[test]
    fn contributors_keep_first_contribution_order_without_duplicates() {
        let alice = UserId::from("u-alice");
        let bob = UserId::from("u-bob");
        let mut blip = Blip::new(WaveId::new(), WaveletId::new(), alice.clone(), None, 0);

        blip.record_contributor(&bob);
        blip.record_contributor(&alice);
        blip.record_contributor(&bob);
        assert_eq!(blip.contributors, vec![alice, bob]);
    }

    #[test]
    fn preview_takes_the_first_non_empty_line_and_truncates() {
        let mut blip = Blip::new(WaveId::new(), WaveletId::new(), UserId::new(), None, 0);
        blip.content =
            gal_ot::Delta::document("\n\nHello there, this is a long message\nsecond line");
        assert_eq!(blip.preview(80), "Hello there, this is a long message");
        assert_eq!(blip.preview(11), "Hello there…");
    }

    #[test]
    fn preview_of_empty_document_is_empty() {
        let blip = Blip::new(WaveId::new(), WaveletId::new(), UserId::new(), None, 0);
        assert_eq!(blip.preview(20), "");
    }
}
