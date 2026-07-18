---
paths:
  - "crates/kyzo-bin/**/*.rs"
---

# Zone: Host, Native — CLI, REPL, and marshal-only network adapters

A way to reach the engine. Never a place engine meaning lives.

Max purity: one sealed typed door. Hosts only marshal. Network carriage is the Kyzo
envelope over the fabric (NATS subject grammar / request-reply). HTTP/gRPC as a
second public product protocol is deleted — optional thin white-label skin may wrap
the same envelope for a density; it never invents verbs, authz, or meaning.

## Required

- Consumes the sealed public contract only; results are rendered and routed
  through the envelope vocabulary.
- No panic escapes a request handler or REPL command: every failure renders
  as a typed error to the caller with the engine's reason and span intact.
  Panic containment is at the typed envelope on every host — not incidental to
  one transport.
- Every entry path passes the auth/capability gate; the surface is enumerable
  and each path is deliberate.
- Streaming and subscribe surfaces preserve the engine's delivery guarantees —
  the host never reorders, dedups, or drops. Delivery, fan-out, and durable
  resume are the fabric's (NATS/JetStream), never a second delivery mechanism
  built here.

## Forbidden

- Importing engine internals — a host that needs a private door is evidence
  the public contract is missing something; fix the contract.
- Interpreting or transforming results beyond envelope rendering — no
  host-side semantics, filtering, or "helpful" coercions.
- `unwrap`/`expect` on any request path.
- State of its own that the engine cannot account for (caches of results,
  shadow catalogs).
- Treating HTTP/REST/gRPC as architecture or a second meaning door beside the
  sealed envelope / NATS carriage.
