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
  op *bodies* → `exec/stdlib/`; `extract_bound`/`compute_bounds`/
  `ValueRange` (filter→scan-bound extraction) are PLANNING, not expression
  semantics → `exec/plan/`; the `Bytecode` stack machine → **dies** —
  it is the second production evaluator the one-evaluator law abolishes.
- **L2:** gold: errors-are-values totality; determinism declared as data
  (and the `partial_eval` law that a nondeterministic application is NOT a
  constant — upstream froze `rand_float()` per query by folding it);
  `eval_to_const` as a distinct "one evaluation, now" request; the serde
  wire-twin discipline (`ExprDe`/`BytecodeDe`/`OpVisitor`: deserialized
  data is claimed, not proven — arity re-proven at the boundary); the
  `apply_op` NaN backstop (no op can hand a poison value past it — order-
  contract protection that must survive in BOTH evaluators, exec's and the
  oracle's own); `LazyOp::decide` as THE single truth table every
  short-circuit machine derives from — that declaration is exactly what
  model owns and both evaluators consume. Condemned: the bytecode machine
  and everything that exists only to keep two evaluators observationally
  identical. Reforge on arrival: `Op` welds declaration to implementation
  — cut as model-side declaration + exec-side kernels; `post_process_args`
  (the self-flagged HIDDEN AST REWRITE hoisting regex compilation) becomes
  an explicit parse-tier desugaring, not a method an op runs on its own
  arguments; `Binding.tuple_pos: Option<usize>` carries its own invitation
  ("a typestate split would put that law in the types... deferred to the
  program-tier port") — the port IS the model migration, honor it. Watch
  on merge: `extract_bound`'s GE/LE arms call `get_int` on one side and
  `get_float` on the other — adjudicate whether that asymmetry is a
  rounding-direction subtlety or a latent bounds bug before it re-lands.

## data/functions.rs (~2930 lines)
- **L1:** splits by domain into `exec/stdlib/{math,text,collection,time,geo}`
  (vector/distance ops land in math; json ops in collection; interval ops in
  time). Two constructs are NOT stdlib: `data_value_to_vld_spec`/`str2vld`
  (the one coercion law for what an `@`-clause validity expression may mean,
  shared by the parse-time and per-row-mutate evaluations) belong beside the
  Validity vocabulary at the model tier; `result_has_nan` is the predicate
  BOTH structural op-application checkpoints test — it travels with the
  checkpoint contract, not with any one domain.
- **L2:** gold: the total-function law (never panics, errors are values);
  `define_op!` welding name/arity/determinism/impl in one invocation ("five
  facts, zero drift"); the typed-refusal catalog with its policies stated as
  law (IntegerOverflow — never wraparound serving a wrong answer;
  DivisionByZero — no silent Infinity/NaN poison; one DomainError shape for
  every partial math op; infinity legitimate, NaN never); the NaN triple
  defense (per-op guards for targeted diagnostics + the structural backstop
  no op can bypass); determinism-as-data on every rand/clock op with the
  upstream folding bugs documented (`rand_uuid_v4` frozen per query,
  `now()` per-query by accident); the time laws (pre-epoch is an error for
  the host clock but a NEGATIVE value for parsed data; `timestamp_to_micros`
  FLOORS toward -∞ because validity feeds the time-travel key; the jiff
  leap-second delta from chrono documented); `vec`'s quantize-through-f32
  meaning and the exact-length base64 law; the Allen-relations doctrine
  (six asymmetric primitives + intersects, inverses by argument swap — one
  op per relation, not twelve; boundary-shape predicates keep `i64::MAX`
  finite distinct from unbounded; start>end collapses to the lawful EMPTY
  interval). Note for the oracle: comparisons REFUSE cross-type operands
  (`ensure_same_value_type`) even though the memcmp order is total
  cross-type — a deliberate language-surface semantics the naive evaluator
  must mirror exactly. Condemned: the monolith itself. Watch:
  caller-proves-arity is admission-shaped and good, but indexing
  `args[0..]` on faith deserves a typed arity proof at the new seam.

## data/program.rs (~2400 lines)
- **L1:** splits on a finer line than "all to model": the INPUT tier
  (`InputProgram`, atoms, `Unification`, `WriteValidity`,
  `ValidityClause`, Trivia/Comment) and the query-options vocabulary →
  `kyzo-model/program/{rule,query}.rs` — this is what engine, oracle, and
  hosts must agree a query IS. The NORMAL/STRATIFIED/MAGIC tiers are plan
  artifacts minted and consumed inside compilation — they go to
  `exec/plan/` with the transforms that mint them (the oracle never sees a
  magic program). The `BodyNormalizer` seam already prefigures this wall:
  "nothing in the program tier touches a transaction" becomes crate
  physics.
- **L2:** gold, preserve through the split intact: the typestate law ("a
  value of a tier type is proof its stage's checks passed"; tiers minted
  only by their transformations); entry-as-a-FIELD (an entry-less program
  cannot exist — construction refusal, not mid-pipeline discovery);
  execution-order-in-the-type (the upstream three-file `.rev()` convention
  reversed exactly once inside `from_reverse_execution_order`);
  `StoreLifetimes` as a documented unit type; `TierInvariantError`
  returned-never-panicked; `WriteValidity`'s no-system-time-by-design law
  (a script has no syntax to forge "when the database learned this");
  `ValidityClause`'s one-grammar-seat doctrine (Spans/Delta bind one EXTRA
  trailing column, arity checks untouched); `DeltaAxis` deliberately
  mirrored by the oracle's own `laws::Axis` — wall discipline already
  practiced. BLOCKER for the model migration: `FixedRuleApply.fixed_impl:
  Arc<dyn FixedRule>` welds parse-time program data to a live runtime
  implementation — model depends on nothing, so this becomes a resolved
  NAME (with arity proof) in model, and the impl binds at exec/plan time;
  the file also carries the duplicate-arity smell (parse-recorded
  `arity` field vs `arity()` recomputed from the impl, self-flagged as an
  unreconciled decision) — resolve it at the cut, one authority.

## data/value/row.rs (~820 lines)
- **L1:** splits along the line its own doc draws: `Rows` (execution form,
  no serialization surface) → `exec/currency/row.rs`; `EncodedKey` (written
  form, NO code accessors) → the model-canonical/store boundary.
- **L2:** gold: the two-form law and codes-never-persist — already zone
  law; the two conversion doors each way (`encode_row` through an admitted
  observer, `push_encoded` with TYPED refusals for foreign/stale
  containers, total element validation, then re-intern); and the fixpoint
  choreography stated and law-tested (read phase on admitted codes
  alternating with mint phase, the borrow checker enforcing the
  alternation). The split is clean because the file already refuses to
  blur the forms.

## query/batch.rs (~160 lines)
- **L1:** dies into `exec/currency/` per its own doc ("values-based v1 …
  this module is the seam it swaps behind").
- **L2:** condemned: `DataValue`-owned columns. Must survive the swap: the
  row-ordered minimum-error keeper (error identity is semantics, not lane
  detail).

## query/batch_ops.rs (~315 lines)
- **L1:** currency handling merges into `exec/currency/` + `exec/op/`.
- **L2:** condemned: `fjall::Slice` imported into query code (line 17,
  `BatchScanFilter` consuming a raw `(Slice, Slice)` stream) — a storage
  type leaked across the zone wall; the store serves bytes only at its
  contract. Reforge the chunker and accumulate-then-refine filter sources
  over code columns. Gold that must survive the reforge: the error-order
  identity discipline (`pending_err` held BEHIND the refined prefix so
  the batched path reports the first failure in stream order, byte-
  identical to the row path); order-is-load-bearing stated as law
  (batching regroups, never reorders — determinism rides on it); the
  torn-row guarantee (nothing lands in the arena unless the whole decode
  succeeds); and the measured-not-guessed allocation note (eager
  BATCH_ROWS reservation was a 3x recursive-workload regression).

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
- **L1:** CORRECTED by the read — this is not plan-graph material at all:
  it is the LEVEL TIER of the fixpoint's working memory (`EpochStore`,
  sealed sorted runs) and merges into `exec/fixpoint/delta_store.rs`
  alongside temp_store.rs.
- **L2:** the original finding narrows: the law IS stated (immutable
  sorted levels sealed per epoch barrier; THE DELTA IS THE NEWEST LEVEL;
  newest-wins shadowing; meet folds at the barrier so a group's value is
  never split across levels) but as a plain `//` section comment rustdoc
  never renders — promote it to a real module doc at the merge. Its
  representation is story #77's memcmp-byte arenas (`encode_tuple_bare`
  rows, byte-comparison probes) — exactly the byte-keyed working memory
  the #120 packed-code currency replaces; the STRUCTURE (runs, shadowing,
  barrier folds — the same architecture the value-plane arena uses)
  survives, the byte identity does not.

## query/normalize.rs (~760 lines)
- **L1:** splits on the zone boundary it currently straddles: the
  normalizer → `exec/plan/`; the session read-surface and fixed-rule
  adapter → `session/`.
- **L2:** the smell is the file's own description — "the session's
  query-side seam" is two zones in one file. Cut on the boundary; neither
  half is condemned, only their cohabitation.

## data/mod.rs, data/value/mod.rs
- **L1:** structural glue; dies with the directory in the crate split.
- **L2:** nothing to salvage as code; any law stated only in a mod doc must
  land in the successor zone's docs before deletion. data/mod.rs read note:
  its own discipline says dead-code EXPECTATIONS fire as consumers land,
  but `program` carries `#[allow(dead_code)]` — exempt from the
  self-removing ratchet the comment promises; the successor tree uses
  `expect` only.

## data/tests/** (mod.rs, exprs.rs ~395, functions.rs ~2280)
- **L1:** the suites scatter with their subjects. functions.rs's 74-test
  battery splits by domain into the `exec/stdlib/*` test submodules.
  exprs.rs splits by nature: the language-law tests (short-circuit
  semantics with erroring tails, fold laws, serde arity re-proof, the
  synthetic poisoned-op NaN-checkpoint proof, refusal-spans-point-at-the-
  argument) survive reforged against the ONE evaluator; every
  `eval_both_ways` tree-vs-bytecode parity harness dies with the bytecode
  machine — the independent check becomes the oracle.
- **L2:** gold to preserve exactly: the pinned fuzz-artifact regressions
  (overflow products named by crash hash), the upstream-bug regressions
  (`mul_vecs` multiplying instead of adding its prefix), the de-panic
  regressions (non-array JSON `vec`, negative JSON indices, pre-epoch
  parses), and the chrono-agreement battery (format/parse/validity-floor
  pinned against the former chrono behavior — these are the permanent
  fixtures of the pure-Rust time migration and must never be weakened).
  The poisoned-op tests prove the NaN class unrepresentable AT THE
  BOUNDARY, not per-op — keep that structural framing at the destination.
