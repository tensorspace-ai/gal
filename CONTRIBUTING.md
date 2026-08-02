# Contributing to Gal

Thanks for taking a look. This is a small, focused codebase and contributions
are welcome.

## Getting set up

You need a Rust toolchain and a C compiler (the SQLite amalgamation is compiled
from source, so `cc`/`clang` or `build-essential` must be available).

```sh
cargo run --release -p gal-server      # http://127.0.0.1:8080
./run-tests.sh                         # everything
```

Browser tests are optional and need a headless Chromium:

```sh
npm install --prefix tests
npx playwright install chromium
node tests/browser.mjs
```

## Before you open a pull request

`./run-tests.sh` must pass. It runs, in order: `cargo fmt --check`,
`cargo clippy -- -D warnings`, `cargo test --workspace`, the cross-language OT
conformance vectors, and the browser tests if a browser is available.

## The one rule that matters

**The Rust and JavaScript OT engines must stay in exact agreement.**

`crates/gal-ot/src/` and `crates/gal-server/src/web/ot.js` implement the same
algorithms. Both transform the same operations, so any disagreement between them
is silent data corruption in a user's document — not a crash you would notice.

If you touch either one, you must touch both. `tests/vectors.json` holds 1,505
randomised cases with the expected results attached, and **both** engines are
replayed against it — `cargo test -p gal-ot` for Rust, `node tests/ot.test.js`
for JavaScript. It has caught real bugs that every other test missed.

The file is checked in and neither test regenerates it, so a change to either
engine that alters what the algebra does will fail. **Do not regenerate the
vectors to make a test pass**; that is how the check was quietly disabled
before. If you have deliberately changed the algebra, regenerate them and
explain why in the commit message:

```sh
cargo run -p gal-ot --example gen_vectors > tests/vectors.json
```

Related invariants worth knowing before you change the core:

- **All offsets are UTF-16 code units**, not bytes and not Unicode scalar
  values, because the browser's selection API is defined over UTF-16. Counting
  `char`s in Rust desynchronises the engines the first time someone types an
  emoji.
- **Access control lives on the wavelet**, not the wave. If you add a message
  type that returns content, it must filter by wavelet participation — see the
  existing tests for private-reply isolation.
- **Ops are persisted before they are acknowledged.** Do not reorder that.

## Style

Formatting is whatever `cargo fmt` produces; there is no custom config. Comments
should explain *why*, not restate the code — match the surrounding density.

## Reporting bugs

Include the version, what you expected, what happened, and steps to reproduce.
For security issues, see [SECURITY.md](SECURITY.md) — please do not open a
public issue.
