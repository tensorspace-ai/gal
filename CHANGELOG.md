# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project uses
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added — comments on a notepad

A notepad admits no new messages by design, so the only way to disagree with a
sentence was to rewrite it. Select some words and comment on them instead: the
remark goes in the margin, level with the text it is about, and can be replied
to and resolved.

Where a comment points is not stored as a position. The range is marked in the
document with an attribute, so the anchor is transformed by the same code that
transforms bold — type above a commented sentence and the highlight moves with
the sentence on every client. Deleting the sentence leaves the thread
*detached*, keeping the remarks rather than pointing them at unrelated words.

The remarks are ordinary blips, so they are co-edited live and carry
contributors, unread marks, search and playback — though they stay out of the
inbox preview and message count, so a notepad still reads as the one shared
page it is. A comment is never deleted on its own; resolving takes it out of
the margin and can be undone. Deleting the message a thread annotates does
remove it, since there would be nothing left to read the remark against.
Existing threads stay
visible and settleable in every mode, so switching a commented page to another
mode still destroys nothing — only *starting* a thread is a notepad's
privilege, and a frozen wave refuses even to resolve.

Schema v5 adds a `comments` table and a `blips.comment` column. **Take a backup
before upgrading**; a database this migration has touched cannot be read by
0.2.0.

## [0.2.0] — 2026-08-02

### Changed — a field the server does not define is now refused

An unknown message *type* was already rejected; an unknown *field* was dropped
in silence. Nearly every optional field in the protocol has a default, so a
misspelling succeeded and did something other than what was asked:
`partcipants` created a wave with nobody in it, `moed: "frozen"` created an
editable one, a mistyped `opId` turned idempotency off so a reconnect applied
the edit twice, and a `Delta` whose `ops` key was misspelled parsed as empty —
the server acked a revision for an edit that applied nothing, so the user's
typing vanished while their client believed it had landed.

The signup, login, password-change, lookup and upload requests are covered too.
The shipped browser client sends the correct spellings, so this costs it
nothing; it is third-party and API clients that were being misled.

A boolean environment variable that is neither on nor off now refuses to start.
`GAL_SECURE_COOKIES=ture` and `GAL_HSTS=enabled` used to read as *off*, starting
a server configured as the opposite of what the operator asked for.

### Fixed

- A blip whose stored content will not parse is reported instead of being served
  as an empty document. An empty `Delta` is valid, so the message merely looked
  blank — and the next keystroke composed against that blank and wrote it back,
  losing the original permanently with nothing logged. Contributors and the op
  log had the same fallback, the latter making playback show a history that
  never happened.
- A database error while loading the inbox no longer greets the user with zero
  waves, which reads as "my conversations are gone".

### Packaging

- Every crate ships its `LICENSE` and a `README.md`; `gal-server` had neither.
- `keywords`, `categories`, `rust-version` and `homepage` are inherited by the
  crates rather than declared only on the workspace, where crates.io never saw
  them.

## [0.1.0] — 2026-07-29

First public release.

Versioned `0.x` deliberately. Gal is complete and tested, but the wire protocol
and the database schema may still change in ways that need a migration, and
there is no password reset yet. The `0.x` series signals that; breaking changes
bump the minor version and are listed here.

### Added

- Waves, wavelets, and threaded blips, with live collaborative editing backed by
  operational transformation.
- Wave modes — Document, Chat, Announcement, Notepad and Frozen — changing both
  what participants may do and how the wave is presented. Only the creator can
  switch, and switching never alters stored content.
- Private replies: side conversations scoped to a subset of a wave's
  participants, enforced server-side.
- Playback of a wave's entire edit history, replayed from the op log.
- Presence and remote cursors, scoped to what each viewer may see.
- Rich text (bold, italic, underline, strikethrough, code, links) carried as
  document attributes, so formatting survives concurrent edits. The formatting
  controls dock to whichever input holds the caret.
- Attachments, by drag and drop, paste, or the paperclip. They are embedded
  objects in the document rather than metadata beside it, so they transform
  against concurrent edits; they belong to a wavelet, so a file in a private
  reply is as private as its text; and they are stored in the database, so a
  backup includes them. Images are identified by their magic bytes and
  everything else is served as an opaque download.
- Full-text search with highlighting, scoped to waves you participate in.
- Per-revision unread tracking: editing a message you had read makes it unread
  again.
- Offline tolerance: the client reconnects with backoff and replays unsent work.
- Schema versioning. Gal refuses to start against a database it does not
  recognise rather than appearing to lose data.
- `GET /healthz` and `gal-server --healthcheck` for container health checks.
- Rate limiting on login, registration, and username lookup.
- Idempotent op submission: each op carries a durable id, so a client replaying
  unacknowledged work after a reconnect cannot apply the same edit twice.
- Password change, which revokes every other session.
- `Origin` validation on WebSocket upgrades, plus CSP and related security
  headers.
- `Dockerfile`, CI, and deployment documentation covering TLS, backups, and
  upgrades.

### Security

- Passwords hashed with Argon2id, off the async reactor.
- Session tokens are 256 bits of OS randomness; only their SHA-256 hash is
  stored.
- Search snippets are escaped server-side before the `<mark>` highlights are
  restored, so message content cannot inject markup.
- Link targets restricted to `http`, `https`, and `mailto`.
- The member directory is not enumerable.

[Unreleased]: https://github.com/tensorspace-ai/gal/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/tensorspace-ai/gal/releases/tag/v0.2.0
[0.1.0]: https://github.com/tensorspace-ai/gal/releases/tag/v0.1.0
