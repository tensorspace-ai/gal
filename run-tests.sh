#!/usr/bin/env bash
# Run every test in the project.
#
# Five layers, cheapest first:
#   1. Lint
#   2. Rust unit + property tests (OT algebra, model, storage), including the
#      Rust half of the conformance replay
#   3. Rust end-to-end tests (real server, real WebSockets, real concurrency)
#   4. Cross-language conformance (the JS OT engine must match the frozen
#      vectors the Rust engine is also held to)
#   5. Browser tests (the editor, the layout, surviving an outage) — optional,
#      since they need a headless browser
#
# tests/vectors.json is a checked-in golden file and is deliberately NOT
# regenerated here. Regenerating before replaying would let a change to the Rust
# engine rewrite its own expectations and still pass. When a change to the
# algebra is intended, regenerate it explicitly and commit the diff:
#   cargo run -p gal-ot --example gen_vectors > tests/vectors.json
set -euo pipefail

cd "$(dirname "$0")"

echo "==> rustfmt"
cargo fmt --all -- --check

echo
echo "==> clippy"
cargo clippy --workspace --all-targets -- -D warnings

echo
echo "==> rust tests"
cargo test --workspace

echo
echo "==> javascript OT engine"
node tests/ot.test.js

if [ -d tests/node_modules ]; then
  echo
  echo "==> browser tests"
  cargo build --release -p gal-server
  node tests/browser.mjs
else
  echo
  echo "==> browser tests skipped (run 'npm install --prefix tests' to enable)"
fi

echo
echo "All tests passed."
