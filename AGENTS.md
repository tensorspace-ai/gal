# Agent instructions

Rules for any AI agent working in this repository. Read this before changing
anything.

## Quality gate — required before every commit

```sh
./run-tests.sh
```

**Do not commit if it fails.** It runs, in order: `cargo fmt --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`,
the cross-language OT conformance vectors, and the browser suite when a browser
is available.

There are no exceptions for "small" or "obvious" changes. Several of the worst
bugs this codebase has had looked small: a one-line `try_send` that silently
unsubscribed a client and lost its work, an off-by-one in a length sum that
overflowed, a serde attribute that renamed a variant but not its fields. Each
was caught by a test, not by reading the diff.

If a test fails and you believe the test is wrong, say so explicitly and explain
why. Do not delete, skip, or weaken a test to make a change pass.

## Commit policy

Human contributors should follow [CONTRIBUTING.md](CONTRIBUTING.md), which
describes the pull request route.

- **Commit as you go**, at each coherent unit of work, rather than one batch at
  the end. A commit should leave the tree green.
- **Do not push.** Committing is the agent's job; publishing is the
  maintainer's, and it is theirs to time.
- **Commit to `main`.** No feature branches for routine work.
- Write a message that explains *why*, not just what changed. Match the style of
  the existing history.
- Never force-push, amend or squash unless asked.

## Verify behaviour, not just compilation

A green build is not evidence that a feature works. For anything with a runtime
surface, exercise it:

```sh
cargo run --release -p gal-server      # then drive it
node tests/browser.mjs                 # two real browsers, real WebSockets
```

When you measure something, measure it correctly. Two examples from this
project's history where a plausible-looking test proved nothing:

- A browser caps ~6 concurrent connections per origin, so "200 concurrent
  requests" from a page is really 6. Use a real client with a connection pool.
- The server drains one WebSocket sequentially, so flooding a single socket
  measures the socket, not the server. Use many sockets.

## The README's screenshots are part of the UI

`docs/screenshots/` holds five images the README leans on to describe the client.
They are captures of a real client driven through three browser sessions, not
drawings, which is the only reason they are worth anything — and it also means a
UI change can quietly falsify them. If you change how the client looks,
regenerate them and reread the prose around them:

```sh
cargo build --release -p gal-server   # the web client is compiled in
node tools/screenshots.mjs
```

The frame numbers in the playback caption come from that run's op log, so they
change with it. Update the text to whatever the script printed.

## Invariants that must not be broken

These are load-bearing. Breaking one corrupts user data, usually silently.

### 1. The two OT engines must agree exactly

`crates/gal-ot/src/` (Rust) and `crates/gal-server/src/web/ot.js` (JavaScript)
implement the same algorithms and transform the same operations. Any
disagreement is silent document corruption, not a crash you would notice.

If you touch either, touch both. `tests/vectors.json` is a **checked-in golden
file**, and both engines are replayed against it: `cargo test -p gal-ot` for the
Rust half, `node tests/ot.test.js` for the JavaScript half. Neither test
regenerates it.

That is deliberate, and it is the whole reason the file is committed. The
vectors used to be regenerated from the Rust engine immediately before being
replayed, which meant they could only detect JavaScript drifting away from Rust.
A change to Rust's own transform semantics rewrote its expectations on the way
past and the suite stayed green — the file was decorative.

So: **do not regenerate the vectors to make a test pass.** If a change to the
algebra is intended, regenerate them as a deliberate step and commit the diff
with the change, so the new behaviour is reviewed rather than absorbed:

```sh
cargo run -p gal-ot --example gen_vectors > tests/vectors.json
```

A diff that changes vectors and is not accompanied by an explanation of *why*
the algebra changed is a bug report.

This has already caught real divergences that every other test missed — a
surrogate pair sliced in half, and an empty op one engine dropped and the other
kept.

### 2. All offsets are UTF-16 code units

Not bytes, not Unicode scalar values. The browser's selection API is defined over
UTF-16. Counting `char`s in Rust desynchronises the engines the first time
someone types an emoji.

### 3. Access control lives on the wavelet, not the wave

A wave can contain a private reply that only some participants may see. Any code
path that returns content must filter by *wavelet* participation — live updates,
snapshots, search, inbox summaries, playback, presence, **and attachments**.

Attachments are the reason `attachments.wavelet_id` exists rather than a
`wave_id`: `Storage::attachment_for` joins through `participants`, so there is no
way to get the bytes without the membership test. A wave-scoped check would hand
a file uploaded into a private reply to everyone else in the wave.

Attachment rows are never deleted when an embed is edited out of a message or
its blip is removed. That is not an oversight: playback replays the op log, so
a file that was in the wave at frame 40 must still resolve when someone scrubs
there. Deleting the wavelet row would cascade, but nothing issues that delete —
there is no wave-deletion path in the server or the client — so there is no
collection at all today and the database only grows. The README says so; if you
add a retraction or a deletion, say so there too.

Presence is the easy one to miss: an unscoped presence list names the blip each
person is editing, which reveals that a private thread exists and when it is
active, even though the content stays hidden. There is a test for this; keep it
passing.

### 4. Persist before acknowledging, and roll back on failure

An op is written to storage before the author is told it landed. If the write
fails, the in-memory document must be rolled back (`ServerDoc::rollback_last`).
Leaving it ahead of storage means later ops transform over an op that exists
nowhere else, and the op log gains a permanent hole that corrupts playback.

### 5. Never silently drop a client

If a client's outbound queue is full it has already missed messages. Close the
connection so it reconnects and resynchronises. Merely removing it from the
subscriber list leaves it connected, believing it is still watching, still
accepting edits, and never acknowledged again — its work accumulates locally and
is never sent.

`Ack` and `WaveState` in particular must never be dropped.

### 6. Mode rules go through `LiveWave::permit`, and only there

A wave's mode decides what participants may do. Every content mutation must ask
`permit` rather than matching on the mode at the call site — adding the checks by
hand was already enough to miss one.

Two orderings are load-bearing:

- In `apply_op` the mode check runs **after** the idempotency lookup. A
  reconnecting client replays work it never saw acknowledged; checking first
  would refuse an op the server already holds, the acknowledgement would never
  arrive, and everything typed afterwards would pile up locally and never be
  sent.
- A refusal must carry the blip id, so the client knows which document to drop.
  A bare refusal leaves it retrying forever.

Never reconstruct a document from `state.wave...blips[].content` in the client.
That is the snapshot from when the wave was opened; every edit since arrived as
an operation. Reopen the wave for authoritative content instead.

### 7. A comment's anchor is a document attribute, never an offset

A comment thread points at a range of text by marking that range with
`COMMENT_ATTRIBUTE` in the blip's own document. Nothing anywhere stores an
index for it, and nothing should start: an offset kept beside a document that
other people are editing is wrong from the first keystroke, and wrong silently —
the highlight lands on the wrong words rather than failing.

Because the anchor is part of the document, it is transformed by the code that
already transforms every other attribute, in both engines, for free. Two
consequences follow and both are load-bearing:

- Positions are *derived* on the way past, never cached. `commentRanges` reads
  them out of the delta and the client's layout pass re-reads them each time.
  Cards did cache their anchor at first, and the highlight then moved with its
  sentence while the card stayed where the sentence used to be.
- Typed text must not inherit it *at the edges*, and must inherit it *inside*.
  Formatting is inherited from the character before the caret, which keeps
  typing at the end of a bold word bold; applied to a comment at the end of a
  phrase it drags the next words into somebody else's thread, and withheld in
  the middle of one it punches a hole through the anchor and splits it in two.
  `inheritable()` strips it and `interiorComment()` puts it back when the
  characters on both sides belong to the same thread; every path that inherits
  attributes goes through the pair.

Testing this from the client's own DOM proves nothing. The typing fast path
deliberately leaves the browser's edit in place without re-rendering, so the
author's markup can show one unbroken highlight over a model that has already
been split. Assert on a *second* client, whose DOM is built from the operation.

A thread always has at least one remark, which is why a remark cannot be deleted
on its own in any mode. Deleting the first would leave a thread nothing can draw
and the protocol cannot repair. Resolving is the retraction, and it keeps the
record. Deleting the blip a thread annotates removes the whole thread, remarks
included — and the "blip with replies" rule must ignore remarks, or a commented
message becomes undeletable for ever: the parent is refused for having replies
and the replies are refused for being comments.

### 8. Schema changes need a version bump and a migration

`schema.sql` is a **frozen v1 baseline** — do not add columns to it. Every change
since is a migration step: bump `SCHEMA_VERSION` in `crates/gal-server/src/db.rs`
and add a step to `migrate()`, bumping `user_version` inside the same transaction
as the DDL. Fresh databases run every migration too, so they end up identical to
upgraded ones.

The server refuses to start against a schema it does not recognise. That is
deliberate: starting cleanly and then serving empty inboxes looks like data loss
to the user.

### 9. Nothing reaches the wire in snake_case

Serde's container-level `rename_all` renames *variants*, not fields. Each variant
needs its own `#[serde(rename_all = "camelCase")]`. Forgetting it is invisible to
a Rust round-trip test — both sides agree on the wrong name — and fatal to the
browser client. `no_message_field_is_snake_case` guards this.

### 10. Keep expensive work off the async reactor

Password hashing runs in `spawn_blocking`. Argon2 costs ~19 MiB and tens of
milliseconds; running it inline lets a handful of concurrent logins stall every
other request, WebSockets included.

## Security expectations

- All user-supplied text reaches the DOM via `textContent`. The single
  `innerHTML` sink is the search snippet, which is escaped server-side before its
  `<mark>` tags are restored. Do not add another.
- WebSocket upgrades are checked against `Origin`. Do not remove that check on
  the assumption that `SameSite` cookies are sufficient — `SameSite` is
  same-*site*, so a sibling subdomain still qualifies.
- Do not expose a full user directory. `/api/users` returns only people the
  caller already shares a wave with.
- An uploaded file is served inline only when its own bytes identify it as a
  PNG, JPEG, GIF or WebP. Everything else — including a file *named* `.png`, and
  including SVG, which can carry script — goes out as
  `application/octet-stream` with `Content-Disposition: attachment`. Do not
  widen this to trust the uploader's `Content-Type` or the extension: `script-src
  'self'` allows same-origin scripts, so a stored file served inline as HTML runs
  in every participant's session.
- A document's limits apply to every *edit*, not only to the content a blip is
  created with. `MAX_BLIP_UNITS` was enforced in `seed_content` alone for a
  long time, which bounded nothing: documents are grown a keystroke at a time.
  The check lives in `apply_op`, on the result, with a rollback — the same one
  the persistence-failure path uses, because an op left in the resident
  document but absent from the log corrupts everything that transforms over it.
- Length is not a bound on its own. An attribute map costs no document *units*
  however much JSON it holds, so `MAX_BLIP_RUNS` and `check_attributes` are the
  other two thirds of the limit: cap the runs, and accept only the attributes
  this application defines, in the shapes it defines. Adding a new attribute to
  the client means adding it to `check_attributes` too.
- `check_attributes` bounds a link's size, not its scheme. Which URLs are safe
  to turn into anchors is `safeUrl`'s judgement in `web/editor.js`; a second
  copy of that rule on the server would be one that could drift out of step
  with the one that actually protects anyone.
- Report security-relevant findings rather than quietly fixing them in a large
  unrelated change.

## Repository layout

```
crates/gal-ot/       operational transformation — the concurrency core
crates/gal-core/     domain model (Wave → Wavelet → Blip) and wire protocol
crates/gal-server/   axum + WebSockets + SQLite; web client under src/web/
tests/               cross-language OT conformance + browser tests
tools/               regenerates the README's screenshots
```

## Style

- Formatting is whatever `cargo fmt` produces. There is no custom config.
- Comments explain *why*. Match the surrounding density; do not narrate the code.
- Keep the README honest. Every quantitative claim in it is checked against the
  source — if you change an iteration count or a guarantee, update the text.
