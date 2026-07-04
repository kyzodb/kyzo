---
paths:
  - "kyzo-core/src/query/**"
---
# Rule: Datalog query engine semantics

Pipeline (`runtime/db.rs::run_query`): parse → normalize (NNF/DNF + binding-safety reorder, all in
`normalize.rs`) → **stratify** via Tarjan SCC + Kahn (`stratify.rs`, `graph.rs`) → **magic-sets**
rewrite (`magic.rs`) → compile to relational algebra (`compile.rs`, `ra.rs`) → **semi-naive fixpoint
eval** (`eval.rs`), with fixpoint state as level-merged sorted runs (`levels.rs`; the epoch
accumulators live in `temp_store.rs`), rows flowing as batches (`batch_ops.rs`) through the columnar
expression evaluator (`vm.rs`). Execution is ONE vectorized machine — no row-at-a-time twin exists; the
naive oracle (`laws.rs`) is the semantic judge, and batch-machinery tests
assert against independently computed expected answers, never against a
second run of the same machine.

Invariants that must never regress:

- Stratification must reject unstratifiable negation/aggregation. A miss yields *wrong answers*, not an
  error.
- Magic-sets may change only *demand* (which facts get computed), never *result semantics*.
- Semi-naive delta evaluation must reach the same fixpoint as naive evaluation, and recursion must
  terminate. The delta is the newest level; admissions are canonical-order and schedule-independent.
- The columnar evaluator (`vm.rs`) stays **observationally identical** to row-by-row `Expr::eval`:
  same values, same presence, and the same error IDENTITY (first failing row in row order, first
  failing subexpression within a row) — not merely the same pass/fail. The lazy connectives
  (`&&`/`||`/`~`, `Expr::Lazy`) short-circuit in BOTH machines; strict boolean ops do not exist.

The laws are executable: `query/laws.rs` holds the naive reference evaluator (the oracle every
optimized evaluation must equal) and the refusal corpus (unstratifiable and unsafe programs the
compiler must reject); the seven engine laws and their enforcement rungs are documented in
`query/mod.rs`. Pipeline stages should be typestate — a stage's output type is proof its checks passed
(an unstratifiable program never becomes an evaluable value).

**A change here requires:** a Datalog-level (query-result) test, a differential run against the naive
oracle for anything touching evaluation (and against `vm.rs`'s row-vs-columnar differential for
anything touching expression evaluation), the refusal corpus still refused, and an explicit argument
for stratification safety and fixpoint termination (resource bounds for the value-inventing fragment).
