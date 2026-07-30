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

- **Commit directly to `main`.** No feature branches, no pull requests for
  routine work.
- **Commit regularly** — at each coherent unit of work, not in one large batch at
  the end. A commit should leave the tree green.
- Write a message that explains *why*, not just what changed. Match the style of
  the existing history.
- Push after committing unless asked not to.

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

`docs/screenshots/` holds four images the README leans on to describe the client.
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

If you touch either, touch both, then regenerate and replay the vectors:

```sh
cargo run -p gal-ot --example gen_vectors > tests/vectors.json
node tests/ot.test.js
```

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

### 7. Schema changes need a version bump and a migration

`schema.sql` is a **frozen v1 baseline** — do not add columns to it. Every change
since is a migration step: bump `SCHEMA_VERSION` in `crates/gal-server/src/db.rs`
and add a step to `migrate()`, bumping `user_version` inside the same transaction
as the DDL. Fresh databases run every migration too, so they end up identical to
upgraded ones.

The server refuses to start against a schema it does not recognise. That is
deliberate: starting cleanly and then serving empty inboxes looks like data loss
to the user.

### 8. Nothing reaches the wire in snake_case

Serde's container-level `rename_all` renames *variants*, not fields. Each variant
needs its own `#[serde(rename_all = "camelCase")]`. Forgetting it is invisible to
a Rust round-trip test — both sides agree on the wrong name — and fatal to the
browser client. `no_message_field_is_snake_case` guards this.

### 9. Keep expensive work off the async reactor

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
