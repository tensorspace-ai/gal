# Gal

An Apache Wave–style collaboration server, in Rust.

Wave's idea was that a conversation is a *shared document*, not a log of messages
you send at each other. Everyone in a wave can edit every message in it, live,
character by character — and the history of how it got written is part of the
document. Gal rebuilds that on modern foundations: WebSockets instead of long
polling, one static binary instead of a Java stack, and no plugins in the browser.

![A Gal wave: three participants, one message being co-edited, a threaded reply,
and a private reply visible only to two of
them](docs/screenshots/wave.png)

Alice's view of a wave she shares with Bob and Carol. The top message has been
edited by both Alice and Bob; Bob's caret is visible inside the reply he is
typing. The dashed block is a private reply — Carol is a full participant in this
wave and never receives it. The formatting controls are at the bottom, dimmed
because nothing has the caret; click into a message and they move into it.

## Running it

```sh
cargo run --release -p gal-server
```

Then open <http://127.0.0.1:8080> and sign up. The first account you create is
just a normal account — there is no separate admin setup. Everything lives in one
SQLite file (`gal.db` by default), and the web client is compiled into the
binary, so deploying is copying one file.

| Variable                   | Default     | Meaning                                              |
| -------------------------- | ----------- | ---------------------------------------------------- |
| `GAL_HOST`                 | `127.0.0.1` | IP address to bind (must be an IP, not a hostname)    |
| `GAL_PORT`                 | `8080`      | Port to bind                                          |
| `GAL_DB`                   | `gal.db`    | SQLite database path                                  |
| `GAL_OPEN_REGISTRATION`    | `true`      | Set to `0` to close sign-ups — **do this if the server is reachable from the internet** |
| `GAL_SECURE_COOKIES`       | `0`         | Set to `1` when serving over HTTPS                    |
| `GAL_HSTS`                 | `0`         | Set to `1` to send HSTS. HTTPS only — a browser that caches it cannot reach a plain-HTTP deployment afterwards |
| `GAL_ALLOWED_ORIGINS`      | *(none)*    | Extra origins allowed to open a WebSocket, comma-separated. Same-origin always works |
| `GAL_TRUST_FORWARDED_FOR`  | `0`         | Set to `1` **only** behind a proxy that overwrites `X-Forwarded-For`; otherwise clients can spoof it and evade rate limits |
| `GAL_LOG`                  | `gal_server=info,tower_http=warn` | `tracing` filter. `GAL_LOG=tower_http=info` adds an access log |

Boolean variables accept `1`, `true`, `yes`, or `on`. Anything else is false —
including an empty value, so `GAL_SECURE_COOKIES=` means *off*.

`gal-server --healthcheck` probes a running instance and exits 0 or 1, for
container health checks. `GET /healthz` does the same over HTTP and touches the
database, so it fails when the server cannot actually serve.

## Deploying it

Gal speaks plain HTTP and has no TLS of its own, so **put it behind a
TLS-terminating reverse proxy**. With nginx:

```nginx
location / {
    proxy_pass http://127.0.0.1:8080;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;      # required for WebSockets
    proxy_set_header Connection "upgrade";
    proxy_set_header Host $host;                 # the Origin check compares against this
    proxy_set_header X-Forwarded-For $remote_addr;
}
```

Then set `GAL_SECURE_COOKIES=1`, `GAL_TRUST_FORWARDED_FOR=1`, and — once you are
sure HTTPS works — `GAL_HSTS=1`. If you set `GAL_SECURE_COOKIES=1` while still
serving plain HTTP, the browser discards the session cookie and login silently
appears to do nothing.

A `Dockerfile` is included. Building needs a C compiler, because SQLite is
compiled from source and linked in:

```sh
docker build -t gal .
docker run -p 8080:8080 -v gal-data:/data -e GAL_OPEN_REGISTRATION=0 gal
```

### Backups

The database runs in WAL mode, so **copying `gal.db` on its own does not back it
up** — recent data lives in `gal.db-wal` and you will restore an empty or stale
database. Use SQLite's own backup, which is safe against a running server:

```sh
sqlite3 gal.db ".backup /backups/gal-$(date +%F).db"
```

Durability is `synchronous = NORMAL`: a committed edit survives the process
crashing, but the last few may be lost if the machine loses power. That is a
deliberate trade for write throughput on a conversation server.

### Upgrading

The schema is versioned. Gal refuses to start against a database written by a
newer release, or one whose layout it does not recognise, rather than starting
cleanly and behaving as though your data vanished. Take a backup before
upgrading.

## What it does

- **Live co-editing.** Any participant can edit any message. Multiple people can
  type into the same message at once and everyone converges, with no locking and
  no last-write-wins.
- **Threaded conversations.** Reply under a message, or reply beside it. Threads
  nest arbitrarily.
- **Modes.** A wave has a mode that decides what people can do in it, and its
  creator can change it at any time:

  | Mode | Who posts | Who edits | Shape |
  |---|---|---|---|
  | **Document** *(default)* | anyone | anyone edits anything | threaded |
  | **Chat** | anyone | only your own messages | a channel: flat, with a composer |
  | **Announcement** | the creator | only the author | a notice with replies |
  | **Notepad** | nobody | anyone edits everything | one shared page |
  | **Frozen** | nobody | nobody | read-only |

  Switching is non-destructive and reversible: moving a threaded wave to Chat
  hides the nesting rather than flattening it in storage, and switching back
  restores the thread exactly. Every rule is enforced by the server, so hiding a
  button is a convenience and never the actual protection.

  ![A wave in Chat mode, laid out as a channel: an avatar gutter, a run of
  messages from one person under a single header, and a composer pinned below
  the thread](docs/screenshots/mode-chat.png)

  A wave in **Chat**, seen by a participant who did not create it. Chat is drawn
  as a channel rather than a stack of cards: the day is marked off, consecutive
  messages from one person collapse under a single header and show their send
  time on hover, and the composer stays below the thread instead of scrolling
  away with it, carrying the formatting and attachment controls in its own box.
  The nesting is gone, the other people's messages are not editable, and the
  mode shows as a badge rather than a picker, because only the creator may
  change it.

  ![The same wave in Frozen mode: read-only, with a note explaining
  why](docs/screenshots/mode-frozen.png)

  And in **Frozen**, seen by the creator: no composer, nothing editable, and a
  line saying why. Every message written in Chat is still there, and switching
  back to Document restores the threading it was hiding.
- **Private replies.** Branch a side conversation off any message with a subset
  of the wave's participants. The server never sends it to anyone else — not in
  live updates, not in snapshots, not in search results, not in the inbox
  snippet, and not in presence. That last one is easy to get wrong: an unscoped
  presence list names the blip each person is editing, which reveals that a
  private thread exists and when it is active, even though its content stays
  hidden. There is a test for it.
- **Playback.** Scrub through a wave's entire edit history and watch it get
  written. This is a real replay of the op log, not a diff of saved versions.

  ![Playback scrubbed to frame 102 of 291, showing the wave mid-sentence with
  later replies not yet written](docs/screenshots/playback.png)

  Frame 102 of 291 for the wave above. The first message stops mid-word, and the
  replies that had not been written yet are empty — this is the op log replayed,
  so the granularity is a keystroke, not a save.
- **Presence and remote cursors.** See who is in a wave and where their caret is.
- **Rich text** — bold, italic, underline, strikethrough, code, links — carried
  as attributes on the document, so formatting transforms correctly against
  concurrent edits instead of being clobbered by them. The controls travel to
  whichever message you are writing in rather than sitting in a header.
- **Attachments.** Drop a file into a message, paste a screenshot, or use the
  paperclip. Images render in place; anything else becomes a download. Up to
  10 MB a file and 200 MB per person per day.

  An attachment is part of the *document*, not metadata beside it: it is an
  embedded object occupying exactly one position in the text, so it transforms
  against concurrent edits like everything else — two people typing on either
  side of a photograph both keep it, in the right place. Files belong to the
  **wavelet** they were uploaded into, so one dropped in a private reply is
  exactly as private as the sentence next to it, and the bytes live in the
  database, so a backup of `gal.db` includes them.

  Images are recognised by their magic bytes and nothing else. A file is served
  back inline only if it really is a PNG, JPEG, GIF or WebP; everything else,
  including anything merely *named* `.png`, is handed over as
  `application/octet-stream` with `Content-Disposition: attachment`. That is
  what keeps an upload endpoint from becoming a way to serve script from your
  own origin.
- **Unread tracking that understands editing.** Read state is per *revision*, so
  a message you have already read becomes unread again when somebody revises it.
- **Full-text search** across every wave you participate in, with highlights.
- **Offline tolerance.** Kill the server mid-sentence and keep typing. The client
  reconnects with backoff, resynchronises, and replays what you wrote while it was
  gone — exactly once, even if the original op had actually been applied. There is
  a browser test that does exactly this.

## How it works

```
crates/
  gal-ot/       operational transformation — the concurrency core
  gal-core/     domain model (Wave → Wavelet → Blip) and wire protocol
  gal-server/   axum + WebSockets + SQLite, and the embedded web client
```

### The data model

```
Wave                  a conversation; the unit that appears in your inbox
└── Wavelet           a participant set + a threaded document
    ├── conv+root     the main conversation
    └── conv+<id>     a private reply: fewer participants, anchored to a blip
        └── Blip      one message, itself a live collaborative document
```

Access control lives on the **wavelet**, not the wave. That is the whole trick
behind private replies: one wave can hold a public thread and a side conversation
only two people can see, and the server simply never sends the latter to anyone
outside it.

### Operational transformation

Concurrency control is real OT, not CRDTs and not locking. A document is a
sequence of `insert` / `retain` / `delete` ops carrying formatting attributes —
the same shape as `quill-delta`, so the Rust and JavaScript engines share a wire
format.

Two operations do the work:

- `compose(a, b)` — sequential: the single delta equivalent to doing `a` then `b`.
  Applying a change to a document is just `compose(document, change)`, because a
  document *is* a delta of inserts.
- `transform(a, b, priority)` — concurrent: rewrite `b`, which was written
  against the same base as `a`, so it can be applied after `a`.

Together they satisfy the transformation property, which is what guarantees
everyone converges:

```
compose(a, transform(a, b, true)) == compose(b, transform(b, a, false))
```

The server holds the authoritative document plus a log of applied ops. A client
submits an op tagged with the revision it was written against; the server
transforms it forward over everything committed since, applies it, then sends an
**ack** to the author and the **transformed op** to everyone else. Authors never
receive an echo of their own op, and both messages advance the recipient's
revision by exactly one.

Clients edit optimistically. Only one op is ever in flight; further keystrokes
compose into a single buffered op sent on ack. That keeps the server's transform
history short no matter how fast anyone types.

Each submitted op carries a unique id, and the id is stored with the op. That
makes submission idempotent: after a reconnect a client replays work it never saw
acknowledged, and without an id the server cannot tell a genuine retry from a new
edit — so the same sentence would be inserted twice. Since a restart closes every
socket, that is an ordinary deploy, not a rare crash.

**All offsets are UTF-16 code units.** The browser's selection API is defined
over UTF-16, so counting Unicode scalar values in Rust would desynchronise the
two engines the first time anyone typed an emoji.

### Concurrency and durability

Each open wave is an actor — one lock covering its documents and its
subscribers — so every mutation of a wave is totally ordered while different
waves never contend. Ops are persisted **before** they are acknowledged, in the
same transaction that updates the materialised snapshot, so a client is never
told an edit landed when it might not survive a crash.

Slow clients apply backpressure to themselves: each connection has a bounded
outbound queue, and one that overflows is **disconnected** — not merely
unsubscribed. That distinction matters. A client that is silently dropped from a
wave's subscriber list stays connected, believes it is still watching, keeps
accepting edits, and never receives another acknowledgement; its work then
accumulates locally and is never sent. Closing the socket instead forces a
reconnect, which resynchronises from a fresh snapshot.

Inbox notifications are debounced, because typing produces an op per keystroke
but the inbox only shows a snippet and a count.

A reconnected socket is a brand new session to the server, so ops written against
the old connection's revisions cannot simply be flushed at it. Instead the client
holds unacknowledged work in the document itself, re-opens each wave to get a
fresh snapshot, and replays that work on top — declining, with a visible warning,
only when the message was rewritten underneath it so badly that the op no longer
fits. Silently dropping someone's typing is the one outcome worth engineering
against.

## Testing

```sh
./run-tests.sh
```

The interesting parts:

- **Property tests over the OT algebra.** Convergence is checked across 4,000
  randomised operation pairs, invertibility across 2,000, and the server's
  revision log across 500 rounds of clients submitting against deliberately
  stale revisions. (That last one exercises `ServerDoc` directly; the full
  request path is covered by the end-to-end tests below.)
- **Cross-language conformance.** `cargo run -p gal-ot --example gen_vectors`
  emits 1,500 randomised cases with the Rust engine's results attached, and
  `tests/ot.test.js` asserts the JavaScript engine reproduces every one exactly.
  Both engines transform the same ops, so a divergence between them is silent
  data corruption; this catches it. It has already caught two real bugs — a
  surrogate pair sliced in half, and an empty op that one engine dropped and the
  other did not.
- **End-to-end tests** drive a real server over real WebSockets with a test
  client that runs the same OT state machine as the browser, so a protocol change
  that would break the client breaks the tests too.
- **Browser tests** drive the real UI in
  real browser sessions: two people typing into one message and converging,
  formatting, threading, private replies staying private, playback, the phone
  layout — and killing the server mid-sentence to confirm that what you typed
  while it was down is still delivered when it comes back. They need a real
  browser, which `playwright-core` does not download for you:

  ```sh
  npm install --prefix tests
  npx playwright install chromium
  node tests/browser.mjs
  ```
- **A wire-shape test** asserts no field reaches the browser in `snake_case`.
  Serde's container-level `rename_all` renames variants but not fields, and
  forgetting the per-variant attribute is invisible to a Rust round-trip test —
  both sides agree on the wrong name — but fatal to the real client.

## Security notes

- Passwords are hashed with Argon2id. Session tokens are 256 bits of OS
  randomness, and only their SHA-256 hash is stored, so a database leak cannot be
  replayed as live logins.
- Login timing is equalised between existing and unknown accounts.
- Cookies are `HttpOnly` and `SameSite=Lax`; set `GAL_SECURE_COOKIES` behind
  HTTPS.
- All user-supplied text is inserted via `textContent`. The one exception is the
  search snippet, which needs `<mark>` — so FTS5 is asked to delimit matches with
  control characters, the server escapes the text, and only then are the markers
  turned back into tags. (Emitting FTS5's `<mark>` output directly is a stored
  XSS; there is a regression test.)
- Link targets are restricted to `http`, `https` and `mailto`, so a participant
  cannot plant a `javascript:` URL that runs in everyone else's session.
- "Not a participant" and "does not exist" return the same error, so the API does
  not reveal the existence of waves you cannot see.
- **WebSocket upgrades are checked against `Origin`.** Browsers attach cookies
  automatically, so without this the only thing stopping a hostile page from
  opening an authenticated socket is the cookie's `SameSite` attribute — a
  browser default rather than a server control, and one that is same-*site*, so
  a sibling subdomain would still qualify.
- **Security headers** on every response: a CSP with no `unsafe-inline`,
  `frame-ancestors 'none'` (the confirmation dialogs are otherwise clickjackable),
  `nosniff`, and a same-origin referrer policy.
- **Rate limiting** on login, registration, and username lookup, keyed per
  client. Password hashing is deliberately expensive, which makes an unthrottled
  login endpoint a denial-of-service amplifier: before this, 500 concurrent login
  attempts drove unrelated request latency from 0.2 ms to over a second.
  Hashing also runs off the async reactor so it cannot stall live editing.
- **The member directory is not enumerable.** `/api/users` returns only people
  you already share a wave with; adding anyone else requires their exact
  username.
- **Only the wave's creator can remove someone else**, and anyone may remove
  themselves. Letting any participant evict any other made a hostile takeover of
  a wave trivial and irreversible.

### Not implemented

Disclosed rather than hidden, because some of these matter for how you deploy it:

- **No password reset.** You can change your password (which signs out every
  other session), but there is no email-based recovery, so a forgotten password
  still needs an operator to intervene in the database.
- **No account deletion or data export.**
- **No moderation surface.** No way to suspend an account or audit participant
  changes.
- **No federation** between servers.
- **No end-to-end encryption.** The server can read everything; private replies
  are enforced by the server, not by cryptography.
- **Quotas are coarse.** Login, registration, and username lookup are rate
  limited per client, and message size is capped, but an authenticated user can
  still create a large number of waves.

Gal suits a team or community that broadly trusts its members. Close
registration (`GAL_OPEN_REGISTRATION=0`) on anything internet-facing.

## License

MIT — Copyright (c) 2026 TensorSpace, Inc. See [LICENSE](LICENSE).

Third-party components and the notices required when redistributing binaries are
listed in [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md). SQLite is compiled in
and is public domain.
