---
paths:
  - "kyzo-core/src/data/value/wide/validity.rs"
  - "kyzo-core/src/data/value/wide/interval.rs"
  - "kyzo-core/src/data/bitemporal.rs"
  - "kyzo-core/src/query/ra/temporal.rs"
  - "kyzo-core/tests/time_travel.rs"
---

# Time and Interval Semantics

Unboundedness is a real SHAPE, not a sentinel. `i64::MAX` is a finite instant, not infinity, and the
two must stay distinguishable everywhere — `[300, i64::MAX]` (has a finite end) and `[300, ∞)` (no
end) are different values.

## Rules

- `Hi::PosUnbounded` means no finite upper endpoint; `Lo::NegUnbounded` means no finite lower one.
  They are byte-distinct and round-trip-distinct from any finite bound.
- `interval_end([start, ∞))` returns Null; `interval_start((-∞, end])` returns Null. The empty
  interval is neither bounded nor unbounded (all four `interval_has_*`/`interval_is_*_unbounded`
  predicates false).
- Intervals are CLOSED on the discrete i64 grid: `interval_end` is the last included instant, not an
  exclusive upper bound.
- The legacy `@'END'` adapter may reserve `i64::MAX` as the open sentinel ONLY because every
  user-facing validity construction path (`ValidityTs::for_assertion`, reached by the `@ <ts>` parser
  coordinate and the per-row mutation loop) REJECTS `i64::MAX` as a real event timestamp. The adapter
  maps the reserved END sentinel to `Hi::PosUnbounded`. Public interval APIs must never leak the
  sentinel.

## Required tests

- validity construction refuses `i64::MAX` (the reservation), at every user-facing path
- the unbounded form encode/decodes as unbounded (no sentinel)
- `interval_end` of an unbounded interval returns Null
- tests assert `Hi::PosUnbounded` / Null directly, NEVER papering over with `unwrap_or(i64::MAX)`
