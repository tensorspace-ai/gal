# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project uses
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] — 2026-08-05

### Security

- **The CSP no longer permits a socket to anywhere.** `connect-src` read
  `'self' ws: wss:`, and a bare scheme in a source list is a *scheme source*
  that matches every host — so the one directive between a script and the
  network allowed streaming a participant's inbox off the machine.
- **Removing someone from a wave now removes them from its private replies.**
  Access is per wavelet and `may_view` asks about *any* wavelet, so an eviction
  from the main thread left every side conversation they were in — and the
  creator usually could not reach into those to finish the job.
- **The WebSocket is rate limited.** Every limiter was on an HTTP endpoint
  while the socket carried twenty commands, including `requestPlayback`, which
  reads up to twenty thousand op rows per call. Commands are charged against a
  per-account allowance priced by what they cost. One account may hold 24
  sockets open.
- **Passwords are judged on more than length** — at least 12 characters, at
  most 1024 (Argon2's cost grows with its input), not a common password, and
  not one containing your username. The same rules now apply to changing a
  password, which had a laxer check of its own.
- **Repeated wrong guesses lock the account, not just the address**, which does
  nothing about an attacker with a list of addresses.
- **Registration no longer accepts an email address.** One was stored and never
  read: unverified personal data held for no purpose. Values written by older
  builds are left alone; clear them with `UPDATE users SET email = ''`.

### Added — single sign-on

Set `GAL_OIDC_ISSUER` and the three values beside it, and the sign-in screen
grows a "Sign in with …" button. The endpoints come from the issuer's discovery
document, so any conforming OpenID Connect provider works and nothing in Gal
names one. Leave it unset and the server behaves exactly as before.

Accounts are created on first sign-in, taking their username from the
provider's `preferred_username` and numbering it if it is taken. Identities are
keyed on the issuer and subject rather than on an email address, so controlling
an address is not a way to claim an existing account — and there is no way to
attach a provider to an account that already has a password. Passwords keep
working alongside it; pair it with `GAL_OPEN_REGISTRATION=0` for a
provider-only server.

Schema v6 adds `oauth_identities`.

### Added — mentions

Type `@` to name someone in the wave. The mention is a document attribute
carrying their id, so it transforms like bold does and still means the same
person after the words are edited. Only participants are offered. There are no
notifications yet, so it is a highlight rather than a summons.

### Added — undo

⌘Z takes back *your* last edit, not the last edit, and keeps working after
somebody else has written in the same message. The stack holds operations and
rebases them over every change that arrives; restoring a saved state would
discard whatever had been typed meanwhile.

### Added — an operator can see what the server is doing

`GET /metrics` serves Prometheus text, behind `GAL_METRICS_TOKEN` and off until
one is set. Every request logs once with a request id, and `GAL_LOG_JSON=1`
emits JSON. Until now every safety valve in the server — disconnecting a slow
client, rolling back an edit that would not persist — fired silently.

### Changed — stopping the server closes its sockets

axum's graceful shutdown does not cover WebSockets, so a stop exited the
process with every connection mid-frame. Sockets are now asked to close and
given ten seconds. The server also pings a silent socket every 30 seconds and
drops one that has said nothing for two minutes, which is what keeps a
connection whose far end vanished from holding a wave in memory for ever.

### Fixed — a panic no longer takes the whole server down

`panic = "abort"` turned one bad edit in one wave into an outage for everyone
on the machine, and made dead code of the `JoinError` handling written to
recover from exactly that. A panicking command now closes its own connection;
the waves it touched are evicted and reloaded from storage.

### Fixed — the server has a 404

Every unrouted path returned the app shell with a 200, so a `/metrics` scrape
recorded a healthy scrape of HTML and a mistyped endpoint looked like it worked.

### Fixed — the client can be used without a mouse, and read

Dialogs declare themselves, trap Tab, close on Escape and restore focus.
Renaming a wave and managing participants were click handlers on elements that
cannot be focused. The message holding the caret is now visibly marked — the
old indication was a two-per-cent shift in border luminance. `--text-faint`
was 2.6:1 on white and is now 4.8:1; it draws timestamps and the
Reply/Privately/Delete labels, not decoration. Errors are announced, and stay
until dismissed instead of fading after six seconds from a container that could
not be clicked. Being offline is a banner that says what it means for what you
are typing, visible on a phone, where the old indicator was hidden entirely.

### Fixed — the OT conformance vectors were decorative

`tests/vectors.json` was gitignored and regenerated from the Rust engine
immediately before being replayed against the JavaScript one, by both
`run-tests.sh` and CI. A change to Rust's transform semantics rewrote its own
expectations on the way past. The file is now checked in, both engines are
replayed against it, and neither test regenerates it.

### Added — muting, leaving, and search that takes you to the message

Three things the server already did and the client never offered.

**Mute** a wave to keep it and stop being counted at: no badge, and out of the
Unread filter, while it stays where it is in the list. The flag has been in the
schema and on the wire since waves had flags at all, and nothing read it.

**Leave** a wave from its header, or by clicking your own face among the
participants — which used to be the one avatar that did nothing when clicked,
though the server has always allowed anyone to remove themselves. Leaving takes
the private replies you were in with it.

**Search results now open at the message that matched**, scroll it into view and
mark it briefly. Every hit has carried its `blipId` all along; the client threw
it away and opened the wave at the top.

### Fixed — the wave header no longer shows stale flags

Archiving a wave left its button saying "Archive". The open wave keeps its own
copy of the flags from the snapshot it was opened with, and an inbox update
refreshed the list without refreshing that copy, so the header went on
describing the state from before the change until the wave was closed and
reopened.

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

### Fixed — a document's size limits now apply to editing it

`MAX_BLIP_UNITS` was checked only against the content a blip was *created*
with. Documents are grown a keystroke at a time, so the limit bounded nothing:
any participant could take a single message past it, and a resident wave holds
every document plus the history needed to rebase against it. The check now runs
on the result of each edit, rolling the operation back if it would not fit.

Length alone was not a bound either. A delta's attribute map is arbitrary JSON
and costs no document *units* however much it carries, so a message inside the
length limit could still hold unlimited data — nothing validated attributes at
all. Two limits close that: the number of separately-formatted runs a document
may be split into, and a check that its attributes are the ones this
application defines, with the values it defines. Unknown attributes are refused
rather than stored, as unknown message fields already are.

This is invisible to the shipped client, which sends only the formatting it has
always sent. A third-party client that invented its own attributes will now be
refused by name.

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
