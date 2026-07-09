---
paths:
  - "kyzo-core/src/exec/**/*.rs"
---

# Zone: Exec — derived truth

One machine that turns proven programs into answers, deterministically.

## Required

- ONE execution currency: packed interned codes. Dedup and identity inside a
  fixpoint key on codes, never durable canonical bytes.
- Every raw-code compare or spend happens under same-domain admission; codes
  are unforgeable (no public raw constructors).
- ONE production expression evaluator. Expression semantics are defined in the
  model; they are evaluated here and in the oracle, nowhere else.
- Deterministic everything: no randomized-iteration collections, no unseeded
  randomness, no wall clock; parallel evaluation produces byte-identical
  results (rows AND order) at any thread count.
- Stdlib functions are pure and total over typed values; partiality is a typed
  error. Aggregations and sketches are deterministic folds — merge order must
  not change the result.
- Stratification is proven before evaluation. Termination arguments are
  recorded where recursion or iteration is unbounded by construction.
- Provenance seams are load-bearing: an operator that cannot attribute its
  output to its inputs is incomplete. Non-idempotent annotations never run on
  the idempotent fixpoint.
- Every operator's semantics must be expressible naively — if the oracle
  cannot state it, it does not land.
- Plan transforms (magic sets, reordering, pushdown) change demand and cost,
  never result semantics — proven by the oracle differential.
- The execution currency never persists: codes, rows, and columns die with
  their epoch; nothing serializes them and no code leaves an encoded key.

## Forbidden

- Canonical encoding in a hot loop (the counter law gates this).
- Allocation in the per-row hot path.
- A second batch container, expression evaluator, or dedup identity.
- Reaching into projections' internals — candidates come through the search
  operator seam.
