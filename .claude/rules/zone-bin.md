---
paths:
  - "crates/kyzo-bin/**/*.rs"
---

# Zone: Host, Native — the CLI, REPL, and HTTP server

A way to reach the engine. Never a place engine meaning lives.

## Required

- Consumes the sealed public contract only; results are rendered and routed
  through the envelope vocabulary.
- No panic escapes a request handler or REPL command: every failure renders
  as a typed error to the caller with the engine's reason and span intact.
- Every route passes the auth gate; the route table is enumerable and each
  route is deliberate.
- Streaming surfaces (SSE feeds, standing queries) preserve the engine's
  delivery guarantees — the host never reorders, dedups, or drops. Subscribe
  surfaces carry only the guarantee-preserving shape; delivery, fan-out, and
  durable resume are the fabric's (NATS/JetStream), never a second delivery
  mechanism built here.

## Forbidden

- Importing engine internals — a host that needs a private door is evidence
  the public contract is missing something; fix the contract.
- Interpreting or transforming results beyond envelope rendering — no
  host-side semantics, filtering, or "helpful" coercions.
- `unwrap`/`expect` on any request path.
- State of its own that the engine cannot account for (caches of results,
  shadow catalogs).
