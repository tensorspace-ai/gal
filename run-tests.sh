#!/usr/bin/env bash
# Run every test in the project.
#
# Five layers, cheapest first:
#   1. Lint
#   2. Rust unit + property tests (OT algebra, model, storage)
#   3. Rust end-to-end tests (real server, real WebSockets, real concurrency)
#   4. Cross-language conformance (the JS OT engine must match the Rust one)
#   5. Browser tests (the editor, the layout, surviving an outage) — optional,
#      since they need a headless browser
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
echo "==> regenerating OT conformance vectors from the Rust engine"
cargo run -q -p gal-ot --example gen_vectors > tests/vectors.json

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
