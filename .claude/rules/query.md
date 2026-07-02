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

The laws are executable: `query/laws.rs` holds the naive reference evaluator (the oracle every
optimized evaluation must equal) and the refusal corpus (unstratifiable and unsafe programs the
compiler must reject); the seven engine laws and their enforcement rungs are documented in
`query/mod.rs`. Pipeline stages should be typestate — a stage's output type is proof its checks passed
(an unstratifiable program never becomes an evaluable value).

**A change here requires:** a Datalog-level (query-result) test, a differential run against the naive
oracle for anything touching evaluation, the refusal corpus still refused, and an explicit argument for
stratification safety and fixpoint termination (resource bounds for the value-inventing fragment).
