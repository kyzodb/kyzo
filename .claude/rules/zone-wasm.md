---
paths:
  - "crates/kyzo-wasm/**/*.rs"
---

# Zone: Host, WASM — the runtime envelope

The real engine inside foreign hosts: browser, edge, embedded runtimes.

## Required

- ONE envelope: typed request in, typed result or typed error out. Every
  foreign host consumes this same surface — playground, demo, and production
  are the same build.
- The panic boundary is typed by construction: an engine panic crosses only
  as the typed error, never as host corruption.
- Determinism is the product: the same input produces serialized results
  byte-identical to a native build — this is a standing proof, not a hope.
- No clock, no randomness, no IO enters the engine except through typed,
  deterministic envelope inputs.

## Forbidden

- A toy or reduced engine build — what runs here is the engine.
- A second request/response shape beside the envelope, for any consumer.
- Host-JS logic that interprets results beyond envelope decoding.
- `unwrap`/`expect` anywhere the host can reach.
