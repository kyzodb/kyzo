---
paths:
  - "kyzo-core/src/react/**/*.rs"
---

# Zone: React — derived truth kept current

Standing queries, incremental maintenance, change feeds.

## Required

- Every incremental result is provably equal to full recomputation — the
  differential proof ships with the feature, not as an afterthought test.
- The ordered write log is the ONLY feed source; there is no second change
  authority.
- The engine owns what an event IS and snapshot-consistency and emits the
  ordered record-event log; subscriber fan-out, backpressure, and durable-
  resume DELIVERY are the fabric's (NATS/JetStream), never built in this zone.
- Delivery is snapshot-consistent: a subscriber can never observe a state the
  log never contained, and delivery order is deterministic.
- Registration, cancellation, and failure of a standing query are typed
  lifecycle states.

## Forbidden

- Recompute-and-diff masquerading as incremental maintenance (except as the
  proof's reference side).
- Callbacks or triggers that can mutate state outside the one admission path.
- Buffering that silently drops or reorders events — backpressure is typed
  and visible.
