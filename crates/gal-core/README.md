# gal-core

Domain model and wire protocol for [Gal](https://github.com/tensorspace-ai/gal), a
Google Wave-style collaboration server.

```text
Wave                  a conversation; the unit that appears in your inbox
└── Wavelet           a participant set + a threaded document
    ├── conversation  the main thread
    └── privateReply  a side conversation: fewer participants, anchored to a blip
        └── Blip      one message, itself a live collaborative document
```

Access control lives on the **wavelet**, not the wave. That is what makes private
replies work: one wave can hold a public thread and a side conversation only some
of its participants can see.

This crate contains the model types and the `ClientMessage` / `ServerMessage`
enums that define the WebSocket protocol. Messages are JSON tagged with `type`
and use camelCase field names, so a browser client consumes them without a
translation layer.

Documents are [`gal-ot`](https://crates.io/crates/gal-ot) deltas.

## License

MIT — Copyright (c) 2026 TensorSpace, Inc.
