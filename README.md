# Gal

An Apache Wave–style collaboration server, in Rust.

Wave's idea was that a conversation is a *shared document*, not a log of messages
you send at each other. Everyone in a wave can edit every message in it, live,
character by character — and the history of how it got written is part of the
document. Gal rebuilds that on modern foundations: WebSockets instead of long
polling, a single Rust binary instead of a Java stack, and no plugins in the
browser.

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
cargo install gal-server     # or from a checkout: cargo run --release -p gal-server
gal-server
```

Binaries for Linux and macOS are attached to each
[release](https://github.com/tensorspace-ai/gal/releases), and images are at
`ghcr.io/tensorspace-ai/gal`. Building from source needs Rust 1.87 or newer and
a C compiler, since SQLite is compiled in.

Then open <http://127.0.0.1:8080> and sign up. The first account you create is
just a normal account — there is no separate admin setup. Everything lives in one
SQLite file (`gal.db` by default, plus its WAL sidecar — see
[Backups](#backups)), and the web client is compiled into the binary, so
deploying is copying one file.

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
| `GAL_LOG`                  | `gal_server=info,tower_http=warn` | `tracing` filter                                      |
| `GAL_LOG_JSON`             | `0`         | Set to `1` to emit one JSON object per line instead of the human format |
| `GAL_METRICS_TOKEN`        | *(none)*    | Bearer token for `GET /metrics`. Unset disables the endpoint; must be at least 16 characters |

Boolean variables accept `1`, `true`, `yes`, `on` and their opposites `0`,
`false`, `no`, `off`. An empty value is false, so `GAL_SECURE_COOKIES=` means
*off*. Anything else refuses to start rather than guessing: `GAL_HSTS=enabled`
would otherwise have quietly meant *off*.

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

### Stopping and restarting it

On `SIGTERM` or Ctrl-C, Gal stops accepting requests, asks every open socket to
close, and waits up to ten seconds for them to go before exiting. It logs
whether they all went. This matters because axum's own graceful shutdown does
not cover WebSockets — an upgraded socket runs in a task it no longer tracks —
so a stop used to exit the process with every connection still mid-frame.

Clients reconnect with backoff and replay what they had not sent, and ops are
idempotent, so a restart costs a reconnection rather than anybody's typing.

The server pings a silent socket every 30 seconds and drops one that has said
nothing at all for two minutes. Browsers answer ping frames themselves, so this
needs nothing from the client. It is there because a connection whose other end
vanished — a laptop closed, a NAT that forgot — is otherwise indistinguishable
from an idle one, and holds a task, a queue and every wave it was watching in
memory indefinitely.

### Watching it run

Every request writes one log line — method, path, status, duration, and a
request id — at `info`. The id comes from an inbound `X-Request-Id` when a proxy
set one, and is echoed on the response either way, so a report of a slow or
failed request can be found in the log. Paths are logged **without** their query
string, so `/api/lookup?name=alice` does not put usernames in your logs.

`GET /metrics` serves Prometheus text. It is **off until you set
`GAL_METRICS_TOKEN`**, and returns 404 rather than 401 until you do — what it
exposes is how many people use this server and when, which is not something to
publish by default and then hope a proxy rule catches. With a token set, scrape
it with `Authorization: Bearer <token>`.

```sh
curl -H "Authorization: Bearer $GAL_METRICS_TOKEN" http://127.0.0.1:8080/metrics
```

Counters cover commands by name, HTTP responses by status, ops applied, refused
and rolled back, rate-limit refusals by limiter, panics, and unparseable frames;
gauges cover open connections and resident waves. The one to alert on is
`gal_ws_slow_client_disconnects_total`: a client whose outbound queue overflows
has already missed messages and is disconnected so it resynchronises, and that
is invisible to everyone but the person it happens to. `gal_ops_persist_failures_total`
should be flat at zero — it counts edits that were applied in memory, would not
write, and were rolled back.

There are no latency histograms. Per-request duration is in the access log,
which answers "what is slow" at this size; buckets are worth adding when
somebody needs a quantile across more than one machine.

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
- **Threaded conversations.** Reply under any message; threads nest arbitrarily.
- **Modes.** A wave has a mode that decides what people can do in it, and its
  creator can change it at any time:

  | Mode | Who posts | Who edits | Shape |
  |---|---|---|---|
  | **Document** *(default)* | anyone | anyone edits anything | threaded |
  | **Chat** | anyone | only your own messages | a channel: flat, with a composer |
  | **Announcement** | the creator | only the author | a notice with replies |
  | **Notepad** | nobody | anyone edits everything | one shared page, with comments |
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
- **Comments on a notepad.** A notepad admits no new messages by design, which
  left nowhere to disagree with a sentence except by rewriting it. Select some
  words and comment on them: the remark goes in the margin, level with the text
  it is about.

  ![A notepad with two phrases highlighted and a comment card beside each in the
  right-hand margin, one of them holding a reply and a reply
  box](docs/screenshots/mode-notepad.png)

  Where a comment points is not stored as a position. The range is marked in the
  document itself, so the anchor is transformed by the same code that transforms
  bold: type a paragraph above a commented sentence and the highlight moves with
  the sentence on every client. Delete the sentence and the comment is left
  *detached* rather than pointing at whatever words moved into those offsets —
  the remarks are kept, because losing the discussion of why a line was wrong,
  at the moment somebody acted on it, is the opposite of the point.

  Comments are ordinary messages underneath, so they are co-edited live, carry
  contributors and unread marks, are searchable, and appear in playback.
  Resolving one takes it out of the margin without deleting anything, and it can
  be reopened. Threads stay visible if the wave is switched to another mode;
  only starting a new one is a notepad's privilege.
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
- **Undo that knows other people exist.** ⌘Z / Ctrl-Z takes back *your* last
  edit, not the last edit — and it keeps working after somebody else has
  written in the same message. The stack holds operations rather than saved
  states, because restoring a state would silently overwrite whatever anyone
  else had typed meanwhile; each stored step is rebased over every change that
  arrives, so it still means what it meant when you made it. A run of typing
  undoes as one step.
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

  **Uploads are not deleted when they stop being referenced.** Deleting the
  message that held a file, or editing the file out of it, leaves the bytes in
  the database and the URL still fetchable by the wavelet's participants. That
  is deliberate — playback replays the op log, so a file that was in a wave
  yesterday has to still resolve when you scrub back to yesterday — but it does
  mean attachment storage only grows. **There is no way to delete a wave**, so
  there is currently no path that reclaims the space: the foreign keys cascade
  correctly, but nothing in the server or the client issues the delete. Until
  one exists, `gal.db` only grows, and reclaiming space means removing rows by
  hand with `sqlite3`. If you need a retraction to be a deletion, it is not one
  yet.
- **Unread tracking that understands editing.** Read state is per *revision*, so
  a message you have already read becomes unread again when somebody revises it.
  A wave you **mute** keeps arriving and stops counting: it holds its place in
  the list, without a badge and out of the Unread filter. Archiving is the one
  that puts a wave away, and anything written in it brings it back.
- **Leaving.** Remove yourself from a wave from its header, or by clicking your
  own face in the participant list. It takes any private replies you were in
  with it, since those are part of the wave.
- **Full-text search** across every wave you participate in, with highlights —
  and opening a result scrolls to the message that matched and marks it, rather
  than dropping you at the top of a conversation to find the line by eye.
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
    ├── conversation  the main thread, which every participant can see
    └── privateReply  fewer participants, anchored to a blip
        └── Blip      one message, itself a live collaborative document
```

Ids are opaque and prefixed by kind of *object*, not kind of wavelet: `w-` for a
wave, `s-` for a wavelet, `b-` for a blip, `c-` for a comment thread. Which sort
of wavelet it is lives in its `kind` column.

A comment thread hangs off this rather than extending it. It holds no text: its
remarks are blips tagged with its id, and where it points is a run of the blip
it annotates carrying that id as a document attribute — so the anchor is
transformed along with the text instead of being an offset kept beside it. The
thread row is keyed to the wavelet, for the same reason attachments are.

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
- **Cross-language conformance.** `tests/vectors.json` is a checked-in golden
  file of 1,505 randomised cases with the expected results attached, and *both*
  engines are replayed against it — `cargo test -p gal-ot` for Rust,
  `tests/ot.test.js` for JavaScript. Both engines transform the same ops, so a
  divergence between them is silent data corruption; this catches it, and
  because the file is frozen rather than regenerated on each run, it also
  catches either engine changing what it does. It has already caught two real
  bugs — a surrogate pair sliced in half, and an empty op that one engine
  dropped and the other did not.
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
- **Passwords are judged on more than length**: at least 12 characters, at most
  1024 — Argon2's cost grows with its input, so an unbounded one is a cheap way
  to spend a server's CPU — not one of the passwords everyone picks, and not
  one containing your own username, which is the weak password a length rule
  cannot see. The same rules apply to changing a password as to choosing one,
  which was not previously true.
- **Repeated wrong guesses lock the account, not just the address.** The
  per-address limiter throttles one attacker and does nothing about a thousand
  of them, or one with a list of addresses, all guessing at the same account.
  Ten failures, then one more every two minutes; only failures are charged, so
  signing in normally never touches it, and a locked account is refused before
  the hash so it costs no CPU either.
- **Sessions can be revoked without changing your password** — "Sessions" in
  the sidebar ends every other browser and device. Previously the only way to
  sign out a lost laptop was to change your password.
- **No email address is collected.** One used to be accepted at registration
  and written to the database, where nothing ever read it: unverified personal
  data held for no purpose, with no way to delete it. The client never sent one.
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
- **The WebSocket is metered too**, which for a long time it was not: every
  limiter was on an HTTP endpoint while the socket carried twenty commands,
  including the two that read the database hardest. Commands are now charged
  against a per-*account* allowance — so opening more sockets buys none — and
  priced by what they cost, because one flat price would have to be set for
  replaying a wave's entire op log and would then be no defence against a flood
  of cursor positions. A refusal costs nothing, so an expensive call a client
  cannot afford does not also drain what it needs to keep typing. One account
  may hold 24 sockets open at once.
- **The member directory is not enumerable.** `/api/users` returns only people
  you already share a wave with; adding anyone else requires their exact
  username.
- **Only the wave's creator can remove someone else**, and anyone may remove
  themselves. Letting any participant evict any other made a hostile takeover of
  a wave trivial and irreversible. Removing someone from the wave removes them
  from its private replies as well — access is per wavelet, so an eviction that
  stopped at the main thread left every side conversation they were in intact,
  and the creator usually could not reach into those to finish the job.
- **A document is bounded in what it can hold**, and the bound is applied to
  every edit rather than only to a message's first draft. Three things are
  capped, because any one alone leaves the other two free: its length
  (256K UTF-16 units), the number of separately-formatted runs it is split into
  (16K), and what a run's formatting may be — the attributes a document carries
  are the ones this application defines, with values of the shape it defines.
  Without the last of those, a delta's attribute map is arbitrary JSON of
  arbitrary size hung off text that costs nothing against a length limit.

### Not implemented

Disclosed rather than hidden, because some of these matter for how you deploy it:

- **No password reset.** You can change your password, and you can sign out
  every other session without changing it, but there is no email-based
  recovery — Gal sends no mail and holds no address — so a forgotten password
  still needs an operator to intervene in the database.
- **One factor, and no single sign-on.** Username and password is the only way
  in. No TOTP, no passkeys, no OIDC or SAML.
- **No account deletion or data export.**
- **No moderation surface.** No way to suspend an account or audit participant
  changes.
- **Comments do not overlap.** A range carries one anchor, so a second comment
  cannot cover words a first already covers — including a wider selection that
  swallows one; the client says so rather than quietly detaching the older
  thread. A comment is also never deleted on its own: resolving is how one is
  retracted, and it keeps the record. Deleting the message a thread is about
  does take the thread with it, since a remark that outlived what it referred
  to could no longer be read against anything.
- **No federation** between servers.
- **No end-to-end encryption.** The server can read everything; private replies
  are enforced by the server, not by cryptography.
- **Quotas are rates, not totals.** Every WebSocket command is charged against
  a per-account allowance priced by what it costs the server, and a single
  document is bounded in length, runs and formatting — but nothing caps the
  *total* a patient account can accumulate. Someone willing to stay under the
  rate can still make a great many waves over a great many days, and there is
  no per-user storage quota.

Gal suits a team or community that broadly trusts its members. Close
registration (`GAL_OPEN_REGISTRATION=0`) on anything internet-facing.

## License

MIT — Copyright (c) 2026 TensorSpace, Inc. See [LICENSE](LICENSE).

Provided as is, without warranty of any kind, as the license sets out. It is
pre-1.0 software that stores other people's conversations — keep your own
backups.

Third-party components and the notices required when redistributing binaries are
listed in [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md). SQLite is compiled in
and is public domain.
