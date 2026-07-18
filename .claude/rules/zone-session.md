---
paths:
  - "crates/kyzo-core/src/session/**/*.rs"
  - "crates/kyzo-core/src/lib.rs"
---

# Zone: Session — the one door

Everything between a caller and the truth: entry, admission, catalog,
observers, operations, the verify summons.

## Required

- ALL writes pass through the one admission path; there is no second way for
  data to reach storage from outside.
- Constraints gate admission as denial rules with typed witnesses — a refusal
  is a value naming the constraint and the offending rows, never an error string.
- Every catalog mutation is atomic across all its rows, with validity
  generations bumped in the same transaction.
- Public doors are enumerable and each is deliberate: adding a public surface
  (a `::` op, an API method, an envelope field) is an operator ruling.
- Every failure that reaches a caller is typed. The parsed grammar surface and
  the executable surface agree: parsed-but-unowned operations are explicitly
  owned typed refusals.
- `::verify` summons the oracle crate; it never reimplements any semantics.
- Relation triggers and their semantics are ours; post-commit external
  NOTIFICATION delivery is a publish-on-commit to the fabric (NATS), never an
  outbound delivery mechanism built in this zone.
- CompositionId is client-rooted; OperationKey + OperationOutcome is the one
  idempotency organ — Engine-minted CompositionId is Unconstructible.
- Catalog generations are the one meaning-clock for projection freshness.

## Forbidden

- `unwrap`/`expect` on any path reachable from a caller.
- Engine semantics implemented here (evaluation, encoding, projection logic
  belong below; this zone routes, admits, and administers).
- A bypass door for tests, tools, or convenience — corruption/bypass tests
  construct through storage directly and are named as such.
