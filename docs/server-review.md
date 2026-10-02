# Server review — 2026-10-02

The review focused on request authentication, SQLite queries and transactions,
wave residency, operation persistence, membership changes, outbound WebSocket
queues, connection lifecycle, and rate limiting. Fixes were committed as separate
units on `main`; nothing was pushed.

## Findings and repairs

| Finding | Repair and evidence |
| --- | --- |
| The unchanged checkout failed Clippy with 19 large-error diagnostics. | Box internal command errors. This keeps large protocol views out of the success/error result storage and restores the required gate without suppressing the lint. Wire messages are unchanged. |
| `notify_waiters` lost a disconnect when the socket reader was inside a command. Several reply paths silently dropped full-queue messages. | Use a retained notification permit and close on failed reply enqueue. A real socket test blocks a command, overflows its queue, and verifies that it disconnects afterwards. |
| A failed welcome leaked its connection registration and detached writer. Concurrent upgrades could bypass the per-account socket limit. | Run cleanup after failed greetings, bound writer shutdown, and claim socket slots atomically. Tests cover failed storage reads, late shutdown upgrades, and 48 concurrent upgrades sharing a 24-socket limit. Shutdown includes writers finishing their flush. |
| Every edit scanned the complete FTS table to remove its previous search entry. | Schema v7 gives search entries stable integer keys and preserves existing row IDs. A migration test checks old results, replacement, targeted deletion, and the bundled SQLite query plan. The retained load probe verifies every final document in storage. |
| Inbox updates and resident waves loaded the full user directory, including password-hash columns they did not need. Inbox result assembly searched vectors repeatedly. Registration locked every resident wave to cache an unrelated account. | Read only participating public profiles, join them directly into inbox results, use a hash lookup for wave rows, and remove registration-wide cache writes. Read transactions make summaries consistent. Regression tests compare bulk and single-wave summaries across 32 waves and exercise adding an account after a wave is resident. |
| Database requests spawned blocking workers before waiting for a pooled connection. | Wait asynchronously for one of 16 slots. Hold the permit inside the worker until its connection is returned, including when the caller is cancelled. A cancellation test verifies that the running worker retains its slot. |
| Title, mode, participant, and deletion broadcasts could precede failed storage writes. Private creation released its permission lock before writing; membership removal wrote private revocations separately. | Persist before resident changes and broadcasts; keep permission checks and writes under the wave lock. Create private replies and cascade participant removal transactionally. Failure injection verifies unchanged resident state, and a blocked-write test orders private creation against eviction. |
| Initial message snapshots and playback seeds were separate writes, with best-effort compensating deletes on failure. Deleting an annotated blip also used several independent writes. | Store the message, seed, and search entry together; store a new wave and its first blip together; delete a parent, its comments, remarks, and search entries together. Triggered failures leave no partial wave/message and restore an entire annotated thread. |
| Failed edit persistence restored the document but retained the new contributor and modification timestamp. | Restore those metadata fields too. A real client test injects an op-log failure and then confirms a subsequent edit commits at the correct revision. |
| Revoking a session did not revoke its already-open WebSocket, and an open socket could outlive its session expiry. | Track token hashes on connection handles, close affected sockets after successful logout/revocation/password changes, recheck after registration, and enforce expiry with a timer. Tests check actual closed sockets, unaffected sessions and users, and expiry. A command already in progress finishes before the reader closes, preserving the durable-write ordering. |
| Once a rate-limit map exceeded 4,096 entries, every request scanned it under a reactor-thread mutex. The ten-minute eviction also refilled a slow account bucket before its twenty-minute refill period ended. | Prune at most once per minute and retain buckets for at least their full refill period. Deterministic tests cover a large active map and a partially refilled account. |

## Validation and performance

`./run-tests.sh` was required and run before every commit: formatting, Clippy with
warnings denied, workspace tests (including real HTTP/WebSocket end-to-end tests),
frozen cross-language OT conformance, a release build, and the browser suite.
Every completed gate passed all 166 browser checks. No test was deleted,
weakened, or skipped to accept a fix. The golden OT vectors were not regenerated.

[Performance measurements and reproduction](server-performance.md) include two
matched eight-socket runs against 100,000 search entries. Throughput rose from
100.7–102.2 to 2,355.2–2,842.0 acknowledged edits per second after removing the FTS
scan. The larger new-path probe verified 1,280 persisted edits across 16 sockets.
These measurements isolate small-document edits with warm local caches.

The server still serializes mutations within a wave, and SQLite still has a
single writer. Large-document composition, hot-wave fanout, and sustained disk
workloads need their own measurements before further architectural changes.
