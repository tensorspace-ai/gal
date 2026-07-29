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
snapshots, search, inbox summaries, playback, **and presence**.

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

### 6. Schema changes need a version bump and a migration

`CREATE TABLE IF NOT EXISTS` is a no-op against an existing table, so a new
column would simply never be added. Bump `SCHEMA_VERSION` in
`crates/gal-server/src/db.rs` and add the corresponding step to `migrate()`,
bumping `user_version` inside the same transaction as the DDL.

The server refuses to start against a schema it does not recognise. That is
deliberate: starting cleanly and then serving empty inboxes looks like data loss
to the user.

### 7. Nothing reaches the wire in snake_case

Serde's container-level `rename_all` renames *variants*, not fields. Each variant
needs its own `#[serde(rename_all = "camelCase")]`. Forgetting it is invisible to
a Rust round-trip test — both sides agree on the wrong name — and fatal to the
browser client. `no_message_field_is_snake_case` guards this.

### 8. Keep expensive work off the async reactor

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
- Report security-relevant findings rather than quietly fixing them in a large
  unrelated change.

## Repository layout

```
crates/gal-ot/       operational transformation — the concurrency core
crates/gal-core/     domain model (Wave → Wavelet → Blip) and wire protocol
crates/gal-server/   axum + WebSockets + SQLite; web client under src/web/
tests/               cross-language OT conformance + browser tests
```

## Style

- Formatting is whatever `cargo fmt` produces. There is no custom config.
- Comments explain *why*. Match the surrounding density; do not narrate the code.
- Keep the README honest. Every quantitative claim in it is checked against the
  source — if you change an iteration count or a guarantee, update the text.
