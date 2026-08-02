//! The Wave domain model.
//!
//! The hierarchy is the one Apache Wave introduced, and it is what separates a
//! wave from a chat room:
//!
//! ```text
//! Wave                       a conversation, the unit that appears in your inbox
//! └── Wavelet                a participant set + a threaded document
//!     ├── conversation       the main thread everyone in it can see
//!     └── privateReply       fewer participants, anchored to a blip
//!         └── Blip           one message, itself a live collaborative document
//! ```
//!
//! Ids are prefixed by kind of *object* — `w-`, `s-`, `b-` — and which sort of
//! wavelet it is lives in [`Wavelet::kind`], not in its identifier.
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
id_type!(/// Identifies a comment thread anchored to a range of text.
    CommentId, "c-");

impl UserId {
    /// Longest identifier accepted from a client.
    pub const MAX_LEN: usize = 64;

    /// Is this the shape of an id this server mints?
    ///
    /// A user id is never *created* by a client, but one arrives inside a
    /// document as the value of a [`MENTION_ATTRIBUTE`], and from there it
    /// reaches the wire and the DOM. Checking the shape is what keeps a
    /// mention from being a way to store arbitrary strings in a message, and
    /// costs nothing.
    pub fn is_well_formed(&self) -> bool {
        let Some(rest) = self.0.strip_prefix("u-") else {
            return false;
        };
        !rest.is_empty()
            && self.0.len() <= Self::MAX_LEN
            && rest.chars().all(|c| c.is_ascii_alphanumeric())
    }
}

impl CommentId {
    /// Longest identifier accepted from a client.
    ///
    /// A comment id is one of the few identifiers a *client* mints (see
    /// [`CommentId::is_well_formed`]), and it is stored, indexed, and written
    /// into the document as an attribute value. Bounding it keeps a hostile
    /// client from using an anchor as free storage.
    pub const MAX_LEN: usize = 64;

    /// Whether this is an identifier the server is willing to accept.
    ///
    /// Unlike every other id in this model, a comment id is chosen by the
    /// client. It has to be: anchoring a comment means applying an attribute to
    /// a range of text *and* creating the thread it names, and a client that had
    /// to wait for a server-minted id could not do both against the same
    /// revision — someone typing in the page would have moved the range out from
    /// under it.
    ///
    /// Accepting a client's id is therefore fine, but accepting a client's
    /// *string* is not: it reaches SQL, the wire, and the DOM.
    pub fn is_well_formed(&self) -> bool {
        let Some(rest) = self.0.strip_prefix("c-") else {
            return false;
        };
        !rest.is_empty()
            && self.0.len() <= Self::MAX_LEN
            && rest.chars().all(|c| c.is_ascii_alphanumeric())
    }
}

/// The document attribute that anchors a range of text to a comment thread.
///
/// Anchoring this way rather than storing an offset is what makes a comment
/// survive editing: the attribute is part of the document, so every transform
/// the OT engines already perform carries it along. Type a paragraph above a
/// commented sentence and the highlight moves with the sentence, on every
/// client, with no separate index to keep in step. Delete the sentence and the
/// anchor goes with it, which is what leaves the thread detached rather than
/// pointing at unrelated words.
pub const COMMENT_ATTRIBUTE: &str = "comment";

/// The document attribute that marks a run of text as naming someone.
///
/// An attribute rather than an embed, for exactly the reason above: a mention
/// is carried along by the transforms the engines already perform, so it
/// survives whatever anyone types around it, and it splits and merges with the
/// text it covers instead of needing a position of its own.
///
/// Its value is the mentioned user's id, not their name. Names are display
/// text and change; the id is what makes "does this name me?" still true after
/// someone edits the words or changes what they are called.
pub const MENTION_ATTRIBUTE: &str = "mention";

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

/// How a wave behaves: what participants may do, and how a client presents it.
///
/// Deliberately a property of the *wave* rather than of each wavelet. If each
/// wavelet carried its own, a private reply started before a wave was frozen
/// would keep its old permissions and stay editable — so "frozen" would not
/// mean frozen. One value for the whole wave makes the guarantee real and
/// removes any question of what a new private reply inherits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WaveMode {
    /// Everyone edits everything, replies nest. The original Wave behaviour.
    #[default]
    Document,
    /// A channel. Anyone posts, nobody edits anyone else's message, no threading.
    Chat,
    /// Only the creator posts at the top level; anyone may reply.
    Announcement,
    /// One shared page: everyone edits what is there, nothing new is added —
    /// except comments, which hang off a range of the page rather than extend it.
    Notepad,
    /// Read-only. Reversible.
    Frozen,
}

impl WaveMode {
    /// Every mode, for iteration in tests and for the client's picker.
    pub const ALL: [WaveMode; 5] = [
        WaveMode::Document,
        WaveMode::Chat,
        WaveMode::Announcement,
        WaveMode::Notepad,
        WaveMode::Frozen,
    ];

    /// The value stored in the database and sent on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            WaveMode::Document => "document",
            WaveMode::Chat => "chat",
            WaveMode::Announcement => "announcement",
            WaveMode::Notepad => "notepad",
            WaveMode::Frozen => "frozen",
        }
    }

    /// Parse a stored value.
    ///
    /// Returns `None` rather than falling back to a default: an unreadable mode
    /// must not silently become the most permissive one, which would quietly
    /// unfreeze a frozen wave.
    pub fn parse(value: &str) -> Option<Self> {
        WaveMode::ALL.into_iter().find(|m| m.as_str() == value)
    }

    /// A short human label for the picker.
    pub fn label(self) -> &'static str {
        match self {
            WaveMode::Document => "Document",
            WaveMode::Chat => "Chat",
            WaveMode::Announcement => "Announcement",
            WaveMode::Notepad => "Notepad",
            WaveMode::Frozen => "Frozen",
        }
    }

    /// Nothing at all may change.
    pub fn is_frozen(self) -> bool {
        self == WaveMode::Frozen
    }

    /// May a new top-level message be posted, and by whom?
    pub fn allows_new_message(self, is_creator: bool) -> bool {
        match self {
            WaveMode::Document | WaveMode::Chat => true,
            WaveMode::Announcement => is_creator,
            WaveMode::Notepad | WaveMode::Frozen => false,
        }
    }

    /// May a message be replied to?
    pub fn allows_replies(self) -> bool {
        matches!(self, WaveMode::Document | WaveMode::Announcement)
    }

    /// May `editor` edit a message written by `author`?
    ///
    /// Note that even the author-owned modes let an author edit their own
    /// message. Anything stricter would be unwritable: a message is created
    /// empty and its text arrives as edits, so locking a message on creation
    /// would mean it could never be written in the first place.
    pub fn allows_edit(self, is_author: bool) -> bool {
        match self {
            WaveMode::Document | WaveMode::Notepad => true,
            WaveMode::Chat | WaveMode::Announcement => is_author,
            WaveMode::Frozen => false,
        }
    }

    /// May a message be deleted? Authorship is checked separately.
    pub fn allows_delete(self) -> bool {
        matches!(
            self,
            WaveMode::Document | WaveMode::Chat | WaveMode::Announcement
        )
    }

    /// May the title be changed, and by whom?
    pub fn allows_retitle(self, is_creator: bool) -> bool {
        match self {
            WaveMode::Document | WaveMode::Chat | WaveMode::Notepad => true,
            WaveMode::Announcement => is_creator,
            WaveMode::Frozen => false,
        }
    }

    /// May a comment be anchored to a range of text?
    ///
    /// Notepad alone, and that is the whole point of it. Every other writable
    /// mode already has somewhere to put a remark — a reply, or another message
    /// in the channel — but a notepad admits no new messages at all, so the only
    /// way to say "this sentence is wrong" was to edit the sentence. A comment
    /// is the missing way to say something *about* the page without changing it.
    pub fn allows_comments(self) -> bool {
        matches!(self, WaveMode::Notepad)
    }

    /// May an existing comment thread be resolved or reopened?
    ///
    /// Deliberately wider than [`allows_comments`](Self::allows_comments):
    /// resolving is housekeeping on a thread that already exists, not a new
    /// contribution. Gating it the same way would strand every open thread the
    /// moment a notepad was switched to another mode, and mode changes are
    /// supposed to be reversible. Frozen still refuses, because frozen means
    /// nothing changes.
    pub fn allows_resolve(self) -> bool {
        !self.is_frozen()
    }

    /// May a private side conversation be started?
    pub fn allows_private_reply(self) -> bool {
        matches!(
            self,
            WaveMode::Document | WaveMode::Chat | WaveMode::Announcement
        )
    }

    /// Does the client render messages as a flat list rather than a tree?
    pub fn is_flat(self) -> bool {
        matches!(self, WaveMode::Chat | WaveMode::Notepad)
    }

    /// One line explaining the mode, shown in the picker.
    pub fn description(self) -> &'static str {
        match self {
            WaveMode::Document => "Everyone can edit every message. Replies nest.",
            WaveMode::Chat => "A channel. Only you can edit your own messages.",
            WaveMode::Announcement => "Only you can post; anyone can reply.",
            WaveMode::Notepad => {
                "One shared page that everyone edits, with comments in the margin."
            }
            WaveMode::Frozen => "Read-only. Nothing can change until you unfreeze it.",
        }
    }
}

/// A conversation. Holds no content itself; it groups wavelets.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Wave {
    pub id: WaveId,
    pub creator: UserId,
    pub created_at: Timestamp,
    /// Applies to the whole wave, private replies included.
    #[serde(default)]
    pub mode: WaveMode,
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
    /// Set when this blip is a remark in a comment thread rather than part of
    /// the conversation itself. Its [`parent`](Self::parent) is the blip the
    /// thread is anchored in, so a comment cannot be orphaned by the ordinary
    /// threading rules; this field is what tells the two apart.
    #[serde(default)]
    pub comment: Option<CommentId>,
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
            comment: None,
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

/// A comment thread: a remark about a range of text rather than about the wave.
///
/// The thread carries no text of its own. Its remarks are ordinary blips
/// carrying its [`id`](Self::id), which is what gives a comment everything a
/// message already has — live co-editing, contributor avatars, unread marks,
/// search, and a place in playback — instead of a second, poorer kind of text.
///
/// Where the thread points is *not* stored here. The anchor lives in the
/// document as a [`COMMENT_ATTRIBUTE`] run, so it moves as the page is edited.
/// This row only records that the thread exists, which page blip it belongs to,
/// and whether anyone has closed it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommentThread {
    pub id: CommentId,
    /// Kept alongside `blip_id` because access control is a *wavelet* test, and
    /// every path that returns a comment has to be able to make that test
    /// without first looking up the blip it hangs from.
    pub wavelet_id: WaveletId,
    /// The blip whose text carries the anchor.
    pub blip_id: BlipId,
    pub author: UserId,
    pub created_at: Timestamp,
    /// Who closed the thread, if anyone. A resolved thread keeps its anchor and
    /// its remarks; only the way it is drawn changes, so reopening restores it
    /// exactly.
    #[serde(default)]
    pub resolved_by: Option<UserId>,
    #[serde(default)]
    pub resolved_at: Option<Timestamp>,
}

impl CommentThread {
    pub fn resolved(&self) -> bool {
        self.resolved_by.is_some()
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

// --- attachments --------------------------------------------------------

id_type!(/// Identifies an uploaded file.
    AttachmentId, "a-");

/// An uploaded file, without its bytes.
///
/// Attachments belong to a *wavelet*, not to a wave, and that is what makes
/// them safe: the same membership row that decides who may read a private
/// reply decides who may fetch a file uploaded into it. A file dropped into a
/// private thread is no more visible than the sentence next to it.
///
/// The whole of this struct is what a client embeds in a document, so it must
/// stay small — an embed is one unit of a delta no matter how much JSON it
/// carries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    pub id: AttachmentId,
    pub wavelet_id: WaveletId,
    /// The name the uploader's file had, sanitised for display.
    pub name: String,
    /// What the server will actually serve this back as — never what the
    /// uploader claimed. See `http::sniff_image`.
    pub mime: String,
    pub size: u64,
    pub uploader: UserId,
    pub created_at: Timestamp,
}

impl Attachment {
    /// Whether this is an image the server is willing to serve inline, and a
    /// client should therefore render rather than list.
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_round_trips_through_its_stored_value() {
        for mode in WaveMode::ALL {
            assert_eq!(WaveMode::parse(mode.as_str()), Some(mode));
            assert!(!mode.label().is_empty());
            assert!(!mode.description().is_empty());
        }
    }

    #[test]
    fn an_unreadable_mode_is_rejected_rather_than_defaulted() {
        // Falling back to a default would silently unfreeze a frozen wave.
        assert_eq!(WaveMode::parse("Document"), None, "parsing is exact");
        assert_eq!(WaveMode::parse("nonsense"), None);
        assert_eq!(WaveMode::parse(""), None);
    }

    #[test]
    fn document_mode_is_the_default_and_permits_everything() {
        let m = WaveMode::default();
        assert_eq!(m, WaveMode::Document);
        assert!(m.allows_new_message(false) && m.allows_replies());
        assert!(m.allows_edit(false) && m.allows_edit(true));
        assert!(m.allows_delete() && m.allows_retitle(false) && m.allows_private_reply());
        assert!(!m.is_frozen() && !m.is_flat());
    }

    #[test]
    fn chat_lets_you_edit_only_your_own_messages() {
        let m = WaveMode::Chat;
        assert!(m.allows_edit(true), "your own message");
        assert!(!m.allows_edit(false), "someone else's message");
        assert!(m.allows_new_message(false), "anyone may post");
        assert!(!m.allows_replies(), "chat is flat");
        assert!(m.is_flat());
    }

    #[test]
    fn announcement_restricts_posting_to_the_creator_but_stays_writable() {
        let m = WaveMode::Announcement;
        assert!(m.allows_new_message(true));
        assert!(!m.allows_new_message(false));
        assert!(m.allows_replies(), "anyone may reply");
        // A message is created empty and filled by editing, so the author must
        // be able to edit it or the announcement could never be written.
        assert!(m.allows_edit(true));
        assert!(!m.allows_edit(false));
        assert!(m.allows_retitle(true) && !m.allows_retitle(false));
    }

    #[test]
    fn notepad_is_a_single_editable_page() {
        let m = WaveMode::Notepad;
        assert!(m.allows_edit(false), "everyone edits the page");
        assert!(
            !m.allows_new_message(true),
            "not even the creator adds messages"
        );
        assert!(!m.allows_replies());
        // Deleting is off, which is what keeps the page from being emptied and
        // leaving a wave nobody can add to.
        assert!(!m.allows_delete());
        assert!(!m.allows_private_reply());
        assert!(m.is_flat());
        // The one way to say something without changing the page.
        assert!(m.allows_comments() && m.allows_resolve());
    }

    #[test]
    fn frozen_permits_nothing() {
        let m = WaveMode::Frozen;
        assert!(m.is_frozen());
        for is_privileged in [true, false] {
            assert!(!m.allows_new_message(is_privileged));
            assert!(!m.allows_edit(is_privileged));
            assert!(!m.allows_retitle(is_privileged));
        }
        assert!(!m.allows_replies() && !m.allows_delete() && !m.allows_private_reply());
        assert!(!m.allows_comments() && !m.allows_resolve());
    }

    #[test]
    fn commenting_is_notepads_alone() {
        for mode in WaveMode::ALL {
            assert_eq!(
                mode.allows_comments(),
                mode == WaveMode::Notepad,
                "{mode:?} should not offer comments: every other writable mode \
                 already has a reply or a composer to put a remark in"
            );
        }
    }

    #[test]
    fn any_mode_that_can_open_a_thread_can_also_close_one() {
        // Otherwise a notepad would accumulate threads nobody could ever clear.
        for mode in WaveMode::ALL {
            assert!(
                !mode.allows_comments() || mode.allows_resolve(),
                "{mode:?} opens threads it cannot close"
            );
        }
    }

    #[test]
    fn resolving_outlives_the_mode_that_allowed_commenting() {
        // Switching a notepad to another mode must not strand its open threads;
        // mode changes are reversible and are not supposed to destroy anything.
        assert!(WaveMode::Document.allows_resolve());
        assert!(WaveMode::Chat.allows_resolve());
        assert!(WaveMode::Announcement.allows_resolve());
        assert!(!WaveMode::Frozen.allows_resolve(), "frozen means frozen");
    }

    #[test]
    fn a_client_minted_comment_id_must_look_like_one() {
        assert!(CommentId::new().is_well_formed(), "our own minting");
        assert!(CommentId::from("c-a1b2").is_well_formed());

        for bad in [
            "",
            "c-",                       // prefix but no body
            "b-abc",                    // someone else's namespace
            "abc",                      // no prefix at all
            "c-../../etc",              // path-ish
            "c-<script>",               // markup
            "c-a b",                    // whitespace
            "c-héllo",                  // non-ASCII
            "c-a'; DROP TABLE blips--", // the classic
        ] {
            assert!(
                !CommentId::from(bad).is_well_formed(),
                "should be refused: {bad:?}"
            );
        }

        // An id is stored, indexed, and written into the document as an
        // attribute value, so length is bounded too.
        let long = format!("c-{}", "a".repeat(CommentId::MAX_LEN));
        assert!(!CommentId::from(long.as_str()).is_well_formed());
        let limit = format!("c-{}", "a".repeat(CommentId::MAX_LEN - 2));
        assert!(CommentId::from(limit.as_str()).is_well_formed());
    }

    #[test]
    fn a_thread_is_open_until_someone_is_recorded_as_closing_it() {
        let mut thread = CommentThread {
            id: CommentId::new(),
            wavelet_id: WaveletId::new(),
            blip_id: BlipId::new(),
            author: UserId::from("u-alice"),
            created_at: now(),
            resolved_by: None,
            resolved_at: None,
        };
        assert!(!thread.resolved());
        thread.resolved_by = Some(UserId::from("u-bob"));
        assert!(thread.resolved());
    }

    #[test]
    fn only_document_and_announcement_thread() {
        // Replies and flat rendering are opposites; nothing should claim both.
        for mode in WaveMode::ALL {
            assert!(
                !(mode.allows_replies() && mode.is_flat()),
                "{mode:?} both threads and renders flat"
            );
        }
    }

    #[test]
    fn a_frozen_wave_permits_nothing_any_other_mode_permits() {
        // Frozen must be the strict floor: any action allowed in Frozen would be
        // a hole, since Frozen is what people reach for to stop all change.
        let frozen = WaveMode::Frozen;
        for other in WaveMode::ALL.into_iter().filter(|m| !m.is_frozen()) {
            for privileged in [true, false] {
                assert!(
                    !frozen.allows_new_message(privileged) || other.allows_new_message(privileged)
                );
                assert!(!frozen.allows_edit(privileged) || other.allows_edit(privileged));
            }
        }
    }

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
