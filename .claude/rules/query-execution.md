---
paths:
  - "kyzo-core/src/query/**/*.rs"
  - "kyzo-core/src/runtime/**/*.rs"
  - "kyzo-core/src/data/functions.rs"
---

# Query and Execution Law

Execution is ONE vectorized machine (semi-naive fixpoint over the columnar evaluator `vm.rs`); the
naive oracle (`query/laws.rs`) is the semantic judge. No row-at-a-time twin exists.

## Invariants that must never regress

- Stratification REJECTS unstratifiable negation/aggregation (a miss yields wrong answers, not an
  error).
- Magic-sets change only *demand*, never result semantics.
- Semi-naive delta eval reaches the same fixpoint as naive eval; recursion terminates. Admissions are
  canonical-order and schedule-independent.
- The columnar evaluator is observationally identical to row-by-row `Expr::eval`: same values, same
  presence, same error IDENTITY (first failing row, first failing subexpression). Lazy connectives
  short-circuit in both.

## Execution uses the execution form

- Recursive recombination carries EXISTING codes when values are already interned.
- Dedup inside one admitted domain uses packed `u32` tuple identity (`ExecDedup`), not durable
  canonical bytes.
- Durable canonical tuple bytes are for storage/scan/persistence/output boundaries only.
- Rules constructing NEW values enter the mint phase — do not generalize a recombination fast path to
  all recursive rules.
- Raw code execution requires admitted same-domain proof. No unchecked raw-code constructor, no
  arbitrary code injection into `Rows`, no re-interning existing values in a hot recombination loop.

If a benchmark regresses because canonical encoding is in a hot loop, the format is not slow — the
hot loop is using the wrong FORM (`benchmarks.md`).

## A change here requires

A Datalog-level (query-result) test; a differential against the naive oracle for anything touching
evaluation (and `vm.rs`'s row-vs-columnar differential for expression evaluation); the refusal corpus
still refused; an explicit stratification-safety and fixpoint-termination argument.
