---
paths:
  - "kyzo-core/src/data/expr.rs"
  - "kyzo-core/src/data/functions.rs"
  - "kyzo-core/src/data/program.rs"
  - "kyzo-core/src/data/value/row.rs"
  - "kyzo-core/src/query/batch.rs"
  - "kyzo-core/src/query/batch_ops.rs"
  - "kyzo-core/src/query/temp_store.rs"
  - "kyzo-core/src/query/vm.rs"
  - "kyzo-core/src/query/levels.rs"
  - "kyzo-core/src/query/normalize.rs"
  - "kyzo-core/src/data/mod.rs"
  - "kyzo-core/src/data/value/mod.rs"
  - "kyzo-core/src/data/tests/**"
---

# Split — files whose constructs scatter; the file itself dies

Guidance grade: high-level review by smell/feel against the target purity
state. Layer 1 = where the contents go and why. Layer 2 = what is condemned
vs reforge-worthy, judged by the DESTINATION zone's law. Not a refactor plan.

## data/expr.rs (~1650 lines)
- **L1:** the `Expr` tree (with spans) → `kyzo-model/program/expr.rs` (meaning
  as data); op *declarations* (name, arity, determinism-as-data) → model;
  op *bodies* → `exec/stdlib/`; the `Bytecode` stack machine → **dies** —
  it is the second production evaluator the one-evaluator law abolishes.
- **L2:** gold: errors-are-values totality, determinism declared as data
  (licenses constant folding), span carriage. Condemned: the bytecode
  machine and everything that exists only to keep two evaluators
  observationally identical. Watch: `Op` currently welds declaration to
  implementation — reforge as model-side declaration consumed by exec-side
  kernels.

## data/functions.rs (~2930 lines)
- **L1:** splits by domain into `exec/stdlib/{math,text,collection,time,geo}`.
- **L2:** gold: the total-function law (never panics, errors are values) and
  `define_op!` welding name/arity/determinism so facts cannot drift.
  Condemned: the monolith itself. Watch: caller-proves-arity is admission-
  shaped and good, but indexing `args[0..]` on faith deserves a typed arity
  proof at the new seam.

## data/program.rs (~2400 lines)
- **L1:** splits into `kyzo-model/program/{rule,query,aggregate}.rs`.
- **L2:** gold — the typestate tiers ("a value of a tier type is proof its
  stage's checks passed") are exactly the house discipline; preserve that
  law through the split intact. The only disease is one file holding every
  tier.

## data/value/row.rs (~820 lines)
- **L1:** splits along the line its own doc draws: `Rows` (execution form,
  no serialization surface) → `exec/currency/row.rs`; `EncodedKey` (written
  form) → the model-canonical/store boundary.
- **L2:** gold: the two-form law and codes-never-persist — already zone law.
  The split is clean because the file already refuses to blur the forms.

## query/batch.rs (~160 lines)
- **L1:** dies into `exec/currency/` per its own doc ("values-based v1 …
  this module is the seam it swaps behind").
- **L2:** condemned: `DataValue`-owned columns. Must survive the swap: the
  row-ordered minimum-error keeper (error identity is semantics, not lane
  detail).

## query/batch_ops.rs (~315 lines)
- **L1:** currency handling merges into `exec/currency/` + `exec/op/`.
- **L2:** condemned: `fjall::Slice` imported into query code — a storage
  type leaked across the zone wall; the store serves bytes only at its
  contract. Reforge the chunker and accumulate-then-refine filter sources
  over code columns.

## query/temp_store.rs (~1520 lines)
- **L1:** reborn as `exec/fixpoint/delta_store.rs`.
- **L2:** gold: the total/delta epoch discipline (it IS semi-naive
  evaluation) and the meet-store semantics. Condemned by counter-law: the
  `BTreeMap<Box<[u8]>, bool>` canonical-byte identity — packed-code
  identity replaces it; nothing byte-keyed survives in the loop.

## query/vm.rs (~520 lines)
- **L1:** becomes `exec/expr/eval.rs` — the ONE production expression
  evaluator.
- **L2:** gold: selection-partitioning as control flow (the DuckDB/Velox
  shape, chosen for the right reason). Dies with the row lane: every
  clause and test that exists to prove parity with `Expr::eval` — the
  independent check becomes the oracle's own evaluator.

## query/levels.rs (~860 lines)
- **L1:** merges into `exec/plan/graph.rs`.
- **L2:** the file has NO module doc at 860 lines — it cannot state its own
  law, which is itself the finding. Adjudicate its contents construct by
  construct during the merge; nothing enters the plan zone undocumented.

## query/normalize.rs (~760 lines)
- **L1:** splits on the zone boundary it currently straddles: the
  normalizer → `exec/plan/`; the session read-surface and fixed-rule
  adapter → `session/`.
- **L2:** the smell is the file's own description — "the session's
  query-side seam" is two zones in one file. Cut on the boundary; neither
  half is condemned, only their cohabitation.

## data/mod.rs, data/value/mod.rs, data/tests/**
- **L1:** structural glue; dies with the directory in the crate split.
- **L2:** nothing to salvage as code; any law stated only in a mod doc must
  land in the successor zone's docs before deletion.
