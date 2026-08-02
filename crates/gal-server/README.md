# gal-server

An Apache (formerly Google) Wave-style collaboration server in Rust.

Conversations you write together — every participant edits every message live,
with real operational transformation, threaded blips, private replies, and full
history playback.

```sh
cargo install gal-server
gal-server
```

It listens on `127.0.0.1:8080` and keeps everything in one SQLite file
(`gal.db`) beside it. There is no separate database to run, no message broker,
and no build step for the client — the web app is plain ES modules served by the
same binary.

Put it behind a TLS-terminating reverse proxy and set `GAL_SECURE_COOKIES=1`.
Close registration with `GAL_OPEN_REGISTRATION=0` on anything internet-facing:
Gal suits a team or community that broadly trusts its members, since any
participant of a wave can edit and delete content in that wave.

The [repository](https://github.com/tensorspace-ai/gal) has the full README —
configuration, the threat model, backups, and what is deliberately not
implemented. The transformation engine is published separately as
[`gal-ot`](https://crates.io/crates/gal-ot).

## License

MIT — Copyright (c) 2026 TensorSpace, Inc. See [LICENSE](LICENSE).

Provided as is, without warranty of any kind, as the license sets out.
