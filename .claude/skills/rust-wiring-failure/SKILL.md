---
name: rust-wiring-failure
description: Fires when kyzo plumbing is about to be written wrong instead of as the matching construct in rust-wiring-success — an env var read outside the config struct, a runner/orchestrator/step-list function sequencing domain work by hand in an entrypoint, a host crate importing engine internals, a host-side cache or shadow state the engine can't account for, or unwrap/expect on a request path.
---

# Wiring — failure patterns

Ways plumbing gets written wrong instead of built as a composition root, config-once injection, or sealed public door (`rust-wiring-success`).

## Scattered env reads

`std::env::var` called from inside business-logic modules re-litigates the environment read at every call site instead of proving it once at startup.

```rust
fn connect() -> Client {
    let url = std::env::var("VENUE_URL").unwrap(); // read scattered outside config: EngineConfig::from_env() is the one place this belongs
    Client::new(url)
}
```

## Orchestrator entrypoint

A runner/pipeline/step-list function in `main` calling domain operations in a hand-kept sequence duplicates an order the construction graph, or the store's transaction/consuming-verb chain, already determines. Wiring `main` to construct config, open storage, build the engine, and `serve` is composition-root success — the failure is domain step-lists beside that wiring.

```rust
fn main() {
    let engine = build_engine();
    ingest(&engine);      // domain work sequenced by hand in the entrypoint —
    reconcile(&engine);   // not composition-root wiring. Name the terminal
                          // operation the graph already orders; don't step-list it here.
    // serve(engine) alone after wiring is fine — see rust-wiring-success
}
```

## Host importing internals

A host crate (`kyzo-bin`, `kyzo-wasm`) reaching into an engine crate's private module is evidence the public contract is missing a capability, not a shortcut to take.

```rust
use kyzo_core::exec::internal::raw_codes; // zone-bin/zone-wasm forbid this outright: extend the public envelope instead
```

## Host-side shadow state

A cache of results, a duplicated catalog, or any state a host keeps that the engine itself doesn't know about drifts from the engine's own truth the moment either side changes independently.

```rust
struct HostCache {
    last_results: HashMap<QueryId, EngineResponse>, // the engine can't account for this: state of its own, forbidden by zone-bin/zone-wasm
}
```

## Panic on request path

`unwrap`/`expect` inside a request handler turns a typed engine refusal, or a malformed request, into a crash that takes the whole host process down.

```rust
fn handle(req: RawRequest) -> Response {
    let parsed: EngineRequest = req.try_into().unwrap(); // forbidden on any request path: render every failure as a typed error to the caller
}
```

## Second envelope shape

A bespoke response shape built for one consumer, alongside the engine's one sealed envelope, means playground, demo, and production are no longer provably the same build.

```rust
// a "simplified" JSON shape for the demo UI, separate from EngineResponse:
// zone-wasm forbids a second request/response shape beside the envelope, for any consumer
```

## Standing ban: `unsafe`

`#![forbid(unsafe_code)]` applies repo-wide across every `rust-*` group. `unsafe` is never a legal shortcut for any construct here. If wiring seems to need `unsafe` to exist, the construct is wrong, not the ban.
