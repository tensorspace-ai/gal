# gal-ot

Operational transformation for collaborative rich-text editing.

This is the concurrency-control core of [Gal](https://github.com/tensorspace-ai/gal),
an Apache Wave-style collaboration server, published separately because it is
useful on its own.

A `Delta` is a sequence of `insert` / `retain` / `delete` operations carrying
optional formatting attributes. A document is simply a delta containing only
inserts, so applying a change is `compose(document, change)`.

```rust
use gal_ot::{Delta, ServerDoc};

let mut doc = ServerDoc::new();
doc.apply(0, &Delta::new().insert("Hello world"), "alice")?;

// Two clients edit concurrently, both against revision 1.
doc.apply(1, &Delta::new().retain(5).insert(" there"), "bob")?;
doc.apply(1, &Delta::new().insert("Say: "), "carol")?;   // rebased automatically

assert_eq!(doc.to_plain_text(), "Say: Hello there world");
# Ok::<(), gal_ot::OtError>(())
```

Two operations do the work:

- `compose(a, b)` — sequential: the single delta equivalent to doing `a` then `b`.
- `transform(a, b, priority)` — concurrent: rewrite `b`, which was written
  against the same base as `a`, so it can be applied after `a`.

Together they satisfy the transformation property, which is what guarantees
everyone converges:

```text
compose(a, transform(a, b, true)) == compose(b, transform(b, a, false))
```

## Notes

- **All offsets are UTF-16 code units**, not bytes and not Unicode scalar values,
  so that offsets agree exactly with a browser's DOM selection APIs. Counting
  `char`s would desynchronise a Rust server from a JavaScript client the first
  time anyone typed an emoji.
- The JSON encoding matches the widely-used `quill-delta` format, so a browser
  client can share the wire format directly.
- `ServerDoc` keeps a bounded op history plus each revision's inverse, which
  gives exact rollback and exact playback.

Convergence is property-tested across 4,000 randomised operation pairs and
invertibility across 2,000, including astral characters and formatting
attributes.

## License

MIT — Copyright (c) 2026 TensorSpace, Inc.
