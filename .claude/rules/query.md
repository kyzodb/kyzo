---
paths:
  - "kyzo-core/src/query/**"
---
# Rule: Datalog query engine semantics

Pipeline (`runtime/db.rs::run_query`): parse → normalize/DNF (`logical.rs`) → reorder for binding safety
(`reorder.rs`) → **stratify** via Tarjan SCC + Kahn (`stratify.rs`, `graph.rs`) → **magic-sets** rewrite
(`magic.rs`) → compile to relational algebra (`compile.rs`, `ra.rs`) → **semi-naive fixpoint eval**
(`eval.rs`).

Invariants that must never regress:

- Stratification must reject unstratifiable negation/aggregation. A miss yields *wrong answers*, not an
  error.
- Magic-sets may change only *demand* (which facts get computed), never *result semantics*.
- Semi-naive delta evaluation must reach the same fixpoint as naive evaluation, and recursion must
  terminate.

**A change here requires:** a Datalog-level (query-result) test, not just a unit test, plus an explicit
argument for stratification safety and fixpoint termination.
