---
paths:
  - "crates/kyzo-core/src/data/expr.rs"
  - "crates/kyzo-core/src/data/functions.rs"
  - "crates/kyzo-core/src/data/program.rs"
  - "crates/kyzo-core/src/query/batch.rs"
  - "crates/kyzo-core/src/query/batch_ops.rs"
  - "crates/kyzo-core/src/query/temp_store.rs"
  - "crates/kyzo-core/src/query/vm.rs"
  - "crates/kyzo-core/src/query/levels.rs"
  - "crates/kyzo-core/src/query/normalize.rs"
  - "crates/kyzo-core/src/data/mod.rs"
  - "crates/kyzo-core/src/data/tests/**"
---

# Split — files whose constructs scatter; the file itself dies

Layer 1 = where each construct goes and why. Layer 2 = what is condemned
vs reforge-worthy, judged by the DESTINATION zone's law.

Entries below are census-verified: each file's construct inventory was
enumerated to closure before its verdicts landed.

## data/expr.rs (1644 lines; inventory: fork header, module doc (three
essences + the arity law), `Bytecode` + `BytecodeDe` wire twin + checked
Deserialize, errors (UnboundVariable, TupleTooShort, CorruptBytecode,
ArityMismatch, NoImplementation, PredicateType, EvalRaised),
`expr2bytecode`, `eval_bytecode_pred`/`eval_bytecode`, `Expr` (6
variants) + `ExprDe` twin + checked Deserialize + Display, `LazyOp` +
`Decision` + decide/identity/deciding_bool, Expr methods (compile, span,
get_binding, get_const, build_equate/and/is_in, negate, to_conjunction,
fill_binding_indices, binding_indices, eval_to_const, partial_eval,
bindings, collect_bindings, eval, extract_bound, get_variables,
to_var_list), `compute_bounds` + `ValueRange`, `Op` struct +
op_display_name + `apply_op` NaN checkpoint + `CustomOp` + serde impls +
`get_op` registry + arity_matches/arity_requirement/post_process_args —
closed)
- **L1:** four destinations. `Expr` tree with spans, `LazyOp`/`Decision`
  (THE single truth table both evaluators derive from), op DECLARATIONS
  (name/arity/determinism as data), and the serde wire-twin discipline →
  `model/program/expr.rs`. Op BODIES' registry (`get_op` and the `Op`
  weld's implementation half) → `exec/stdlib/`. `extract_bound`/
  `compute_bounds`/`ValueRange` are PLANNING → `exec/plan/`. The
  `Bytecode` machine (enum, compiler, evaluator, its errors, its wire
  twin) → REMOVE: the second production evaluator the one-evaluator law
  abolishes; the oracle becomes the independent check.
- **L2:** gold: errors-are-values totality; determinism-as-data with
  `partial_eval`'s law (a nondeterministic application is NOT a constant
  — upstream froze `rand_float()` per query); `eval_to_const` as a
  distinct one-evaluation-now request; deserialized-is-claimed-not-proven
  (arity re-proven at the boundary); the `apply_op` NaN backstop (order-
  contract protection that must survive in BOTH the exec kernel and the
  oracle's own evaluator). Reforge on arrival: `Op` welds declaration to
  implementation — cut as model declaration + exec kernels;
  `post_process_args` (self-flagged HIDDEN AST REWRITE hoisting regex
  compilation) becomes an explicit parse-tier desugaring;
  `Binding.tuple_pos: Option<usize>` carries its own invitation ("a
  typestate split would put that law in the types... deferred to the
  program-tier port") — the port IS the model migration, honor it. Watch
  on the plan-zone merge: `extract_bound`'s GE/GT arm coerces the
  lower-bound constant via `get_int` but the upper via `get_float` (and
  mirrored in LE/LT) — adjudicate whether that asymmetry is deliberate
  rounding direction or a latent bounds bug before it re-lands.

## data/functions.rs (2932 lines; inventory: fork header, module doc
(total-function law, caller-proves-arity, determinism-as-data),
`define_op!`, `unix_now`, `ensure_same_value_type`, the typed-error
catalog (IntegerOverflow / DivisionByZero / DomainError) + the NaN
discipline (`no_nan`, `no_nan_vec`, `result_has_nan`), and ~130 ops in
domains: json (list/json/paths/object/parse/dump + `json_array_index`,
`get_json_path[_immutable]`, `to_json`, `interval_to_json`, `json2val`),
comparison (eq..le over `ensure_same_value_type`), arithmetic
(add/sub/mul/div/minus/abs/signum/floor/ceil/round + `add_vecs`/
`mul_vecs`), transcendentals with per-op domain guards (exp/exp2/ln/
log2/log10/trig/hyperbolic/sqrt/pow + `pow_out_of_domain`/mod/negate),
bit ops + pack/unpack, concat + `deep_merge_json`, string ops
(includes/case/trim/starts/ends), regex (parser-injected OP_REGEX +
`compile_regex_value` + matches/replace/replace_all/extract/
extract_first), t2s, type predicates (is_*), list ops (append/prepend/
length/sorted/reverse/first/last/chunks/chunks_exact/windows +
`get_index`/get/maybe_get/slice/chars/slice_string/from_substrings),
base64, conversions (to_bool/to_unity/to_int/to_float/to_string +
`val2str`), vector ops (`vec_element_type`, vec, rand_vec, l2_normalize,
l2_dist, ip_dist, cos_dist), int_range, the rand ops, timestamp/validity
section (autosi_precision, format_rfc3339, format/parse_timestamp,
uuid ops, and the interval section
(validity coerce ALREADY CUT → kyzo-model `value/validity_coerce.rs`),
(`two_intervals`, make/start/end, boundary-shape predicates, six Allen
primitives + intersects) — closed)
- **L1:** destinies ruled in `docs/deprecated/move_plan.json` v9 (OpDecl /
  BoundOp / bind::resolve_op / errors / ten kernel seats incl. minted `geo.rs`;
  model `program/expr.rs` + `value/json_convert.rs`; condemned currents are
  Delete evidence only). ALREADY SEATED outside this bag: ValidityCoerce →
  `crates/kyzo-model/src/value/validity_coerce.rs`. Cut names from this
  inventory as each seat's meter passes — inventory cut is part of the seal.
  Settled: `StdlibRefuse::NanAnswer` at `BoundOp::apply` is the law for the
  offered-but-unreachable NaN surface (offers may stay). Carried:
  per-row `compile_regex_value` until bench-gated hoist (see plan
  `carried_obligations`).
- **L2:** gold: the total-function law; `define_op!`'s five-facts-zero-
  drift weld; the refusal catalog's policies (overflow never wraps to a
  wrong answer; div/mod refuse zero; one DomainError shape; infinity
  legitimate, NaN never); the NaN triple defense; determinism-as-data on
  every rand/clock op with the upstream folding bugs documented; the time
  laws (host-clock pre-epoch is an ERROR, parsed pre-epoch is a NEGATIVE
  value; `timestamp_to_micros` FLOORS because validity feeds the
  time-travel key; jiff/chrono deltas pinned); `vec`'s
  quantize-through-f32 meaning and exact-length base64 law; the Allen
  doctrine (six asymmetric primitives + intersects, inverses by argument
  swap; `i64::MAX`-finite vs unbounded distinguishable; start>end is the
  lawful EMPTY value). Oracle note: comparisons REFUSE cross-type
  operands (`ensure_same_value_type`) even though storage order is total
  cross-type — deliberate language semantics the naive evaluator must
  mirror. Condemned: the monolith itself. Findings for the cut:
  `to_float('NAN')` and `signum`'s NaN arm mint NaN VALUES the
  checkpoints then unconditionally refuse — offered-but-unreachable
  surface; either the offers go or the semantics get ruled (operator
  question). `compile_regex_value` recompiles per ROW (validation is
  hoisted, compilation is not — its own doc defers the hoist to the
  operator layer, bench-gated; carry the obligation). Watch:
  caller-proves-arity is good, but indexing `args[0..]` on faith deserves
  a typed arity proof at the new seam.

## data/mod.rs (structural glue; module decls + cfg(test) tests — closed)
- **L1:** structural glue for remaining `data/` modules; dies with the
  directory as the last `data/*` peels leave. `data/value/` is already
  gone (plane seated at `crates/kyzo-model/src/value/`).
- **L2:** gold: dead-code EXPECTATIONS fire as consumers land. Note:
  `program` still carries `#[allow(dead_code)]` — exempt from the
  self-removing ratchet the comment promises; the successor tree uses
  `expect` only.

## data/program.rs (2405 lines; inventory: fork header (typestate
transformations + port constraints), module doc (the four tiers as
typestate), query-output options (QueryAssertion, ReturnMutation,
SortDir, RelationOp, WriteValidity + per-row resolve,
InputRelationHandle, QueryOutOptions + Display + num_to_take),
TempSymbGen, the program-shape errors (NoEntry with spanned/spanless,
EmptyRuleSet, TierInvariant, EntryHeadNotExplicitlyDefined), the input
tier (Comment/Trivia, InputInlineRule, InputInlineRulesOrFixed,
FixedRuleApply + arity, FixedRuleArg, InputAtom + Display, SearchInput,
rule/relation apply atoms, DeltaAxis, ValidityClause + extra_var,
Unification, InputProgram + new/attach_comment_trivia with the
pest-span-quirk note/accessors/needs_write_lock/entry-head/
into_normalized_program/Display, collect_trivia_anchors,
shares_a_line_with_preceding_content), the BodyNormalizer seam +
normalize_ruleset (head dedup via ***n), the normal tier, the stratified
tier + StoreLifetimes, the magic tier (Adornment, MagicSymbol +Debug,
MagicInlineRule, MagicRulesOrFixed, MagicFixedRuleApply + errors,
MagicFixedRuleRuleArg, MagicAtom, magic apply atoms, MagicProgram,
StratifiedMagicProgram), and the 14-test battery — closed)
- **L1:** splits on the tier line: the INPUT tier + options vocabulary +
  Trivia/Comment + WriteValidity/ValidityClause/DeltaAxis →
  `crates/kyzo-model/program/{rule,query}.rs` (what engine, oracle, and hosts
  agree a query IS). The NORMAL/STRATIFIED/MAGIC tiers + StoreLifetimes +
  the BodyNormalizer seam are plan artifacts minted and consumed inside
  compilation → `exec/plan/` (the oracle never sees a magic program).
  Tests move with their tiers.
- **L2:** gold, preserve intact: the typestate law (tiers minted only by
  their transformations; possession is proof); entry-as-a-FIELD
  (construction refusal, never mid-pipeline discovery);
  execution-order-in-the-type (upstream's three-file `.rev()` convention
  reversed exactly once); `StoreLifetimes` as a documented unit type;
  `TierInvariantError` returned-never-panicked; `WriteValidity`'s
  no-system-time-by-design law AND its per-row terminal-tick re-proof
  (parse proved the column, not the value — re-prove per row through the
  same smart constructor); `ValidityClause`'s one-grammar-seat doctrine
  (Spans/Delta bind one EXTRA trailing column, arity checks untouched);
  `DeltaAxis` mirrored by the oracle's own `laws::Axis` — wall discipline
  already practiced; the trivia attachment authority (one place decides
  leading/trailing, with the verified pest span quirk documented).
  BLOCKER for the model migration: `FixedRuleApply.fixed_impl:
  Arc<dyn FixedRule>` welds parse-time program data to a live runtime
  implementation — model depends on nothing, so this becomes a resolved
  NAME (with arity proof) in model and the impl binds at exec/plan time;
  resolve the self-flagged duplicate-arity smell (parse-recorded `arity`
  field vs `arity()` recomputed from the impl) at the same cut — one
  authority.

## data/tests/ (mod.rs 15, exprs.rs 394, functions.rs 2281 — each read
whole; inventories: mod (2 decls + port note), exprs (conditional eval
through both machines, no-catch-all depanic pin, the synthetic
POISONED_SCALAR/VECTOR ops proving the NaN checkpoint structural,
fold/no-fold laws, serde arity re-proof both types, the short-circuit
law battery via `eval_both_ways`, refusal-span agreement between
machines), functions (op value tables for every domain; the
div-mod-zero, domain-error, vector-lane-domain, boundary-lock,
zero-vector-distance batteries; overflow boundaries incl. the pinned
fuzz artifact; mul_vecs regression; de-panic regressions (vec trailing
bytes, non-array JSON, negative JSON index); pre-epoch pins; and the
chrono→jiff fixture suite: four `*_agreed` batteries + the
floor-uniformity pin + `datetime_deltas_vs_chrono` pinning FIVE
divergence classes with chrono's former output in comments) — closed)
- **L1:** suites scatter with their subjects: functions' domain tables →
  the matching `exec/stdlib/*` test submodules; the time fixtures travel
  with stdlib/time + the model's validity coercion; exprs' language-law
  tests (short-circuit past errors, fold laws, serde re-proof, the
  poisoned-op checkpoint proofs, spans-point-at-the-argument) survive
  reforged against the ONE evaluator; every `eval_both_ways`
  tree-vs-bytecode parity harness dies with the bytecode machine — the
  independent check becomes the oracle.
- **L2:** never weaken: the pinned fuzz-artifact overflow (named crash
  hash), the boundary-lock test (exact domain edges guards must NOT
  refuse, so a `<`→`<=` slip is caught), the de-panic regressions, and
  the jiff-migration fixtures — permanent behavior-pinning of the
  pure-Rust time migration (each delta asserts jiff's CURRENT output so
  an upgrade shifts loudly, never silently). The poisoned-op tests prove
  the NaN class unrepresentable AT THE BOUNDARY, not per-op — keep that
  structural framing at the destination.
