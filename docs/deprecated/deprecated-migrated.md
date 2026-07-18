---
paths:
  - "crates/kyzo-core/src/query/laws.rs"
  - "crates/kyzo-core/src/query/gauntlet.rs"
  - "crates/kyzo-core/src/query/dst_query.rs"
  - "crates/kyzo-core/src/query/provenance.rs"
  - "crates/kyzo-core/src/query/trials.rs"
  - "crates/kyzo-core/src/jepsen_trials.rs"
  - "crates/kyzo-core/src/storage/conformance.rs"
  - "crates/kyzo-core/src/storage/crash_matrix.rs"
  - "crates/kyzo-core/src/storage/sim.rs"
  - "crates/kyzo-core/src/parse/fuzz_tests.rs"
  - "crates/kyzo-core/src/data/bitemporal.rs"
  - "crates/kyzo-core/src/data/aggr.rs"
  - "crates/kyzo-core/src/data/sketch/**"
  - "crates/kyzo-core/src/data/json.rs"
  - "crates/kyzo-core/src/data/arrow_ipc.rs"
  - "crates/kyzo-core/src/data/span.rs"
  - "crates/kyzo-core/src/data/symb.rs"
  - "crates/kyzo-core/src/data/relation.rs"
  - "crates/kyzo-core/src/data/value/tag.rs"
  - "crates/kyzo-core/src/data/value/canonical.rs"
  - "crates/kyzo-core/src/data/value/cell.rs"
  - "crates/kyzo-core/src/data/value/number.rs"
  - "crates/kyzo-core/src/data/value/string.rs"
  - "crates/kyzo-core/src/data/value/prefix.rs"
  - "crates/kyzo-core/src/data/value/proofs.rs"
  - "crates/kyzo-core/src/data/value/arena.rs"
  - "crates/kyzo-core/src/data/value/code.rs"
  - "crates/kyzo-core/src/data/value/column.rs"
  - "crates/kyzo-core/src/data/value/exec.rs"
  - "crates/kyzo-core/src/parse/**"
  - "crates/kyzo-core/src/format.rs"
  - "crates/kyzo-core/src/format/tests.rs"
  - "crates/kyzo-core/src/capacity.rs"
  - "crates/kyzo-core/src/typestate.rs"
  - "crates/kyzo-core/src/kyzoscript.pest"
  - "crates/kyzo-core/src/query/compile.rs"
  - "crates/kyzo-core/src/query/stratify.rs"
  - "crates/kyzo-core/src/query/magic.rs"
  - "crates/kyzo-core/src/query/graph.rs"
  - "crates/kyzo-core/src/query/eval.rs"
  - "crates/kyzo-core/src/query/sort.rs"
  - "crates/kyzo-core/src/query/search.rs"
  - "crates/kyzo-core/src/query/semiring.rs"
  - "crates/kyzo-core/src/query/incremental.rs"
  - "crates/kyzo-core/src/query/standing.rs"
  - "crates/kyzo-core/src/engines/**"
  - "crates/kyzo-core/src/runtime/**"
  - "crates/kyzo-core/src/storage/**"
  - "crates/kyzo-core/src/fixed_rule/**"
  - "crates/kyzo-core/src/lib.rs"
  - "crates/kyzo-core/tests/**"
  - "crates/kyzo-core/benches/**"
  - "crates/kyzo-core/examples/language_tour.rs"
  - "crates/kyzo-bin/**"
  - "crates/kyzo-crashfs/**"
  - "crates/kyzo-arrow-interop/**"
  - "crates/kyzo-lsp/**"
---

# Migrated — files with a 1:1 successor that move whole to their target home

A migration is never a bare `mv`: the construct is reforged to the
DESTINATION zone's law on arrival. Files also listed in `split.md` or
`absorbed.md` migrate only the parts those rules don't claim.

Entries below are census-verified: each file's construct inventory was
enumerated to closure before its verdicts landed.

## data/aggr.rs (1928 lines; inventory: fork-deviation header, module doc
(meet-vs-normal law), `NormalAggrObj`/`MeetAggrObj` traits, the two
factory types, `AggrKind`, `Aggregation` (+is_meet/meet_op/normal_op,
name-only identity, Debug), `meet_aggr!`/`normal_aggr!` macros, 25
aggregation consts with their fold structs (and/or/unique/group_count/
count_unique/union/intersection/collect+factory/choice_rand/count/
variance/std_dev/mean/sum/product over `NumAccum`/min/max/latest_by/
smallest_by/min_cost/shortest/choice/bit_and/bit_or/bit_xor),
`parse_aggr` with sketch fallthrough, and the test battery
(update_checked flag-pinning harness, semilattice laws per meet, the
inversion/exactness/beyond-2^53/exact-Int regressions, F1/F2
mutation-proven holes, kind-agreement, collect-limit) — closed)
- **L1:** NOT a 1:1 move — the file welds declaration to implementation.
  The meaning half (names, the meet-vs-normal kind as data — what
  stratify rules on and the oracle implements independently) →
  `model/program/aggregate.rs` (seat exists). The fold objects, factories
  and `NumAccum` → `exec/fold/aggr.rs` (seat exists). Tests move with
  their halves; the flag-contract battery goes with exec.
- **L2:**
  - **gold:** `AggrKind`'s unrepresentable kind/impl disagreement (rebuild the proof at the new seam); the changed-flag contract with the fixed upstream inversions (the flag gates delta propagation — a false "unchanged" is a premature fixpoint); exact-`Num`-order min/max; `NumAccum` exact-Int sum/product; the whole test battery incl. F1/F2.
  - **Condemned:** `choice_rand` folds UNSEEDED `rand::rng()` — nondeterminism in the answer path with no determinism-as-data field on `Aggregation` to even declare it; it takes the `rules/rng.rs` seeded discipline or is refused — it does not migrate as-is.
  - **Watch:** `Null` doubles as "no value yet" in the meet accumulators (min/max document null-skipping; intersection silently conflates a real Null row with its identity) — destination law wants a typed Option-shaped accumulator.
  - **NEW-SEAT:** none needed.
## data/relation.rs (501 lines; inventory: fork header, module doc
(coerce as contract-at-the-boundary), `VecElementType`,
`NullableColType` + Display, `ColType` (11 kinds), `ColumnDef`,
`StoredRelationMetadata` + satisfied_by_required_col/compatible_with_col,
and `coerce` (null gate, per-kind arms: Any/Bool/Int/Float/String/Bytes-
with-base64/Uuid-from-string/List/Vec{list,vector,base64-LE}/Tuple/
Validity{value,ASSERT-RETRACT-~strings,pair}/Json-recursive) — closed)
- **L1:** → `model/schema/`, split as the map draws it:
  `StoredRelationMetadata` + column-compat checks → `relation.rs`;
  `NullableColType`/`ColType`/`ColumnDef`/`VecElementType` + `coerce` →
  `column.rs`.
- **L2:**
  - **gold:** `coerce` is parse-don't-validate stated as law ("fallible parsing, not validation — downstream never re-checks what coercion proved"); the byte conventions (base64 vectors little-endian BY DEFINITION, exact element count or refuse — replacing upstream's unsafe native-endian pointer cast); F32-as-precision-constraint semantics (declared width, values stay f64-canonical, F32 claim checked f32-exact with NaN exempt); the reserved-tick refusals (`i64::MAX`/`MIN` validity refuse at coercion); validity coercion floors shared with `str2vld` so coercion and parse agree on the containing microsecond. Note for the successor doc: `compatible_with_col` treats nullable `Any?` as a wildcard — a deliberate subtlety, state it, don't rediscover it.
  - **Condemned:** N/A
  - **Watch:** N/A
  - **NEW-SEAT:** N/A
## data/value/exec.rs (510 lines; inventory: module doc (the two-form law
made operational: durable = canonical bytes only at boundaries, execution
= raw codes under a proven Domain; the narrow door: admitted rows in,
admitted rows out, no constructor from arbitrary u32),
`#![allow(dead_code)]` (#119 foundation / naive oracle note; #120 wires
the RA engine), `Side`, `ExecRows` with its @authority block (THE DOOR in:
`admit` copying a stamp-verified `Rows`'s codes; row-major accessors; THE
DOOR recombine: `join_project` — hash-join on code equality, cross-arena/
epoch panics, output domain = the wider input extent so copied codes stay
provably in-domain; `raw`; THE DOOR out: `resolve_cell`, the only place a
code becomes bytes), `ExecDedup` with its @authority block (packed
u32-tuple identity: new/contains/insert/absorb/to_exec, insertion-ordered
distinct rows), and the test battery (TC-step vs hand oracle; u32-tuple
dedup identity; THE FOUNDATIONAL GUARANTEE — a full join+dedup pass
leaves the arena's intern count unchanged and its compare-deref counter
at zero, measured by the arena's own instruments; the no-raw-constructor
structural witness; the differential — join_project equals a naive
value-level nested-loop join over seven adversarial edge sets;
determinism — byte-identical left-row-major output, the probe is a
lookup never a hash-order iteration; cross-arena join panic) — closed)
- **L1:** two seats, both existing: `Side` + `ExecRows` and its doors →
  `exec/currency/admitted.rs` ("admitted rows under a proven domain —
  unforgeable" is this type's exact description); `ExecDedup` →
  `exec/fixpoint/delta_store.rs` ("working memory keyed on packed-code
  identity" — this type IS that seed). Tests split with their types; the
  foundational-guarantee test spans both and lands with the fixpoint,
  which is the law it protects.
- **L2:**
  - **gold:** the zero-canonical-encode-in-fixpoint law as an EXECUTABLE test (the arena's own counters prove zero intern/zero deref — verify-never-assert realized); the narrow-door construction (private field, no from_raw; forge vectors proven absent in proofs.rs); the value-oracle differential and the determinism pin (schedule-independence is a stated engine law); both @authority blocks migrate intact. Arrival notes: when #120 lands the production RA join (`exec/op/join.rs`), `join_project`'s naive HashMap probe becomes the law-grade ORACLE the verify battery differentials against — the engine arriving must not delete the oracle. Watch for the destination doc: an empty `out` projection yields `arity.max(1)` with zero codes, so a zero-column projection (semijoin/count shape) silently reports zero rows however many matches occurred — the door has no match-count-without-columns form yet.
  - **Condemned:** N/A
  - **Watch:** N/A
  - **NEW-SEAT:** N/A
## data/value/row.rs (820 lines; inventory: module doc (the code-lifetime
law — codes never persist across a seal, the durable form is canonical
bytes, held by TYPE SURFACE: `Rows` has no serialization surface,
`EncodedKey` has no code accessors, one door each way; the fixpoint
choreography — read phase/mint phase alternation enforced by `intern`
taking `&mut Arena`), `#![allow(dead_code)]` (#119/#120 note), `Rows`
(new_in ≥1-arity, push_row stamp door, push_encoded bytes→execution with
validate-FIRST-then-intern, admit, gather), `AdmittedRows` (raw/row
identity currency, resolve_cell, cmp_rows elementwise, encode_row — the
only execution→bytes mint), `PushError` (Decode/ForeignArena/
StaleDomain — typed, never panics), `split_key` (exactly-arity lawful
encodings, refuses trailing bytes), `EncodedKey` with its @authority
block (layout consts RELATION_PREFIX_LEN/VALIDITY_TAIL_LEN/
BITEMPORAL_TAIL_LEN, from_values, from_stored — the ONLY public byte
constructor, validating), `RelationId` (SYSTEM, checked `new`, CAP =
1<<48 with the 0xFF-headroom rationale, raw_encode, raw_decode exhaustion
door, next, Display), `TupleT` + blanket impl, `encode_key_with_suffix`
(bitemporal slots), `append_bounds` (the scan sentinel law:
Least/Greatest/0xFF upper tail) + scan_key_lower/upper + projected
variants, and the test battery (durable-across-seals with a VACUITY GUARD
proving codes actually moved; key byte order == tuple semantic order over
all pairs; push_encoded round-trip + total refusal, no partial tuples;
THE FIXPOINT CHOREOGRAPHY LAW test; from_stored as validating door; typed
stale/foreign refusals; RelationId cap enforced at decode AND
constructor; scan-key bracket law + projected==materialized; validity
slot widths pinned against the codec; arity panic at the write door) —
closed)
- **L1:** two destinations. The execution half — `Rows`, `AdmittedRows`,
  `PushError`, `split_key` — → `exec/currency/row.rs` (seat exists:
  "interned tuple rows; cell views only at boundaries" is this type).
  The written half — `EncodedKey` + layout consts, `RelationId`,
  `TupleT`, `encode_key_with_suffix`, the scan-key builders and sentinel
  law — is the storage keyspace law, and the store zone (which owns key
  laws: `store/time.rs` is the precedent) has no named file for it.
  NEW-SEAT proposal (operator ratification required): `store/keys.rs` —
  storage key layout v1: the relation-id keyspace with its cap, the
  relation-prefixed canonical tuple form, the validity tails, and the
  scan-bound sentinel law; consumed by `exec/op/stored.rs`. The
  schema-tier RelationId serde (see the json.rs entry) must agree with
  `raw_encode` — one layout, stated once.
- **L2:**
  - **gold:** the code-lifetime law held by type surface, not convention ("you cannot write codes down; you cannot smuggle execution currency out of stored bytes"); the fixpoint choreography as a LAW TEST with the borrow checker as its enforcement mechanism; the deliberate refusal asymmetry (stamp doors PANIC — programmer error; the bytes door returns typed `PushError` — stored bytes are data, "storage ingestion is a refusal surface, not a panic surface"); validate-then-intern so refusal leaves no partial tuple; the vacuity guard in the durability test (a test that proves itself non-vacuous is house standard); `RelationId::CAP`'s 0xFF-headroom rationale (every assignable prefix stays below the sentinel byte every storage consumer assumes). Finding for the destination law: EncodedKey is ONE type holding TWO shapes with no discriminant — the bare written tuple (encode_row/from_values/from_stored, no prefix; split_key is lawful only here) and the relation-prefixed storage key (encode_key_with_suffix, TupleT), on which `from_stored`'s arity split would REFUSE because the 8-byte prefix is not a canonical encoding. The split at migration resolves it (bare form with the currency, prefixed form in store/keys.rs as its own type) — do not carry the conflation across.
  - **Condemned:** N/A
  - **Watch:** N/A
  - **NEW-SEAT:** N/A
## engines/mod.rs (115 lines; inventory: module doc (one shared concept:
the index-read corruption doctrine), the eight module declarations with
their per-engine liveness notes (fts/hnsw/lsh live through the db.rs
surface; gazetteer/sparse/spatial lib-dead awaiting theirs; segments
wired except one helper; text carrying future surface; the two hostile
batteries `#[cfg(test)]`), `IndexRowCorrupt` typed Diagnostic error +
`new`/`from_decode`, and `index_rows` (the scan-wrapping boundary:
codec refusals become the index's OWN typed corruption by downcast;
storage/IO errors pass through unchanged) — closed)
- **L1:** the module tree dissolves into `project/` (each engine to its
  own subtree — see their entries); the one owned concept —
  `IndexRowCorrupt` + `index_rows` — → `project/contract.rs` (seat
  exists): its help text ("the index can be dropped and re-created from
  its base relation") IS the projection law that file states, and every
  projection read consumes scans through this boundary.
- **L2:**
  - **gold:** corruption-is-an-error-never-a-panic extended to every index read path, defined ONCE because all engines name it; the downcast discipline separating codec corruption from storage/IO errors (a raw `DecodeError` cannot leak out of an engine as its contract).
  - **Condemned:** the per-module `#[allow(dead_code)]` liveness ledger — in the target, each projection lands with its surface or doesn't land; the mod-file-as-status-board pattern dies with the monolith crate layout.
  - **Watch:** N/A
  - **NEW-SEAT:** N/A
## engines/segments.rs (486 lines; inventory: module doc (rebuildable
index never a second truth; validity TYPED not sequenced — the
bump-before-commit / witness-after-snapshot pairing, adopted after a
hostile review proved the documented-ordering version racy; the gated
rebuild closing issue #82), `Watermark` (monotone, process-local; fresh
process = zero + empty cache so cross-process staleness cannot arise),
`Segments<'a>` Copy context + `OFF`, `REBUILD_AFTER_STABLE_MISSES`,
`SegmentEngine` (marks/segments/misses maps; `slot`;
`witness_after_snapshot` taking the open snapshot BY SIGNATURE so the
racy order is unrepresentable; `bump_before_commit` with the
harmless-early-orphan rollback story; `get` serving on witness equality
alone; `should_build` — N consecutive misses at the SAME witness;
`install`; `evict`), `Segment` (dense row-major values + u32 END
offsets; `build` declining past u32; `row`; `cmp_prefix`;
`prefix_range`; `partition` binary search), `checked_row_end` (the F7
total cast, factored out to be testable without 4.3 billion values),
and the test battery (prefix ranges vs linear-scan oracle across mixed
types; witness equality governs service incl. orphan + evict + held
Arc; the stable-miss-streak gate; the #82 alternating-writes shape
never crossing the gate; miss-map loss only delays rebuild, never
corrupts serving — the "never a source of truth" doc claim PROVEN;
empty segment; the u32 boundary) — closed)
- **L1:** two seats, both existing. The validity discipline —
  `Watermark`, `witness_after_snapshot`, `bump_before_commit`,
  `should_build` + miss map — → `project/residency.rs` ("the
  rebuild/validity discipline (generations, invalidation)": this IS
  that discipline's first realization, and every other projection needs
  it). The segment structure and its cache/serving —
  `Segment`/`SegmentEngine`'s segments map/`Segments`
  context/`checked_row_end` — → `project/current.rs`. Tests split with
  their halves; the #82 regression battery travels with the gate.
- **L2:**
  - **gold:** soundness by SIGNATURE, not calling convention (the enforcement-ladder ruling — same mechanism as the storage layer's `stamp_after_snapshot`); witness equality as the entire serving criterion; declining-is-always-sound (the u32 decline and the gate decline are one doctrine: a projection is optional speed, the fallback pays no more than the build would have); the miss map's never-a-source-of-truth claim proven by a loss test; Arc-held orphans serving mid-scan readers to completion. Arrival notes: `Segments::OFF` threading is door plumbing the #120 operator wiring replaces (see bench_api's entry); the process-local watermark is sound ONLY while segments are memory-only — if projections ever persist, the generation vocabulary must become durable (residency.rs's business, name it there on day one).
  - **Condemned:** N/A
  - **Watch:** N/A
  - **NEW-SEAT:** N/A
## engines/text/ast.rs (367 lines; inventory: dual MPL header (the
permanent home of the FTS query AST; bounds-checked `remove(0)`
replacing unwraps on the user-text path), module doc, `FtsLiteral`
(+ tokenize — PREFIX literals pass through whole: filtering or
stemming a prefix pattern would change what it means), `FtsNear`,
`FtsExpr` with THE DEPTH INVARIANT doc (the parser is the only
non-test constructor and bounds depth/operator count, so every
recursive walk INCLUDING the compiler-derived Drop/Clone/PartialEq/
Hash is stack-safe BECAUSE the bound holds; "a new constructor must
either enforce the same bound or make every walk, including Drop,
iterative"), tokenize/is_empty (shallow BY DESIGN; flatten is the
normalizer)/flatten/do_tokenize, and tests (is_empty edges incl.
zero-booster; flatten collapse laws; analyzer rewrites incl.
stopword-vanishing and Near distance preservation) — closed)
- **L1:** SPLIT, compelled by the crate wall (correction found at
  parse/fts.rs's read): `model/parse/search.rs` owns the FTS
  mini-language and must NAME the AST it produces, and kyzo-model
  cannot depend on the engine — so the pure-data half (`FtsExpr`/
  `FtsLiteral`/`FtsNear` + flatten/is_empty + the depth invariant) →
  `kyzo-model` beside `parse/search.rs`, while the analyzer-coupled
  `tokenize` rewrite stays engine-side in `project/text/` as an
  extension over the model type.
- **L2:**
  - **gold:** the depth-invariant doc (the sharpest derived-Drop stack-safety analysis in the tree — bounding at the parser is proven STRONGER than an iterative rewrite); prefix-literals-pass-whole as a meaning argument; shallow-is_empty with flatten-as-normalizer stated as a design pair.
  - **Condemned:** N/A
  - **Watch:** N/A
  - **NEW-SEAT:** N/A
## query/mod.rs (186 lines; inventory: THE ENGINE LAWS module doc —
seven laws, each with its enforcement site named (answer correctness
via the deliberately-unoptimized oracle in laws.rs, itself
cross-checked by a second strategy; stratification-refusal;
termination; rule safety; total input handling; concurrency liveness;
operator coherence — "clever execution must be invisible") — and the
module-declaration LEDGER: every `#[allow(dead_code)]` carries its
justification (the oracle no longer test-only since #80's `::verify`
ships it; gauntlet pub(crate) so #80's whole-corpus proof REUSES its
renderer "rather than re-deriving a second one"), and three attributes
are recorded as REMOVED after verifying zero warnings — the ratchet
discipline executed in prose — closed)
- **L1:** the laws doc → `exec/`'s module root as its constitution;
  the declarations dissolve into the target tree (each module's
  liveness note travels to its file). The trials/laws/gauntlet/dst
  declarations re-home with their files to kyzo-trials per those
  entries.
- **L2:**
  - **gold:** the seven laws with enforcement sites (the engine's contract stated where its parts are declared); per-attribute justification and the removed-once-proven notes.
  - **Condemned:** N/A
  - **Watch:** N/A
  - **NEW-SEAT:** N/A
## query/normalize.rs (757 lines; inventory: header (THREE PARTS, each
with its own landing note: the normalizer — faithful ports of
upstream logical.rs NNF→DNF and reorder.rs well-ordering, "nothing
about them is interim ... the logic is final", re-homing when the
tier lands; the SessionView read surface; the SessionFixedRule
adapter; an earlier interim nested-loop interpreter RECORDED as
superseded and deleted; law-5 notes — every upstream `unreachable!`
a typed invariant), the search-atoms-at-the-catalog-boundary ruling
(NormalFormAtom carries no search variant "because search atoms join
the plan at the catalog boundary, not through normalization"),
`SessionView` (Copy BY DESIGN — two references; `_`-prefix
namespace-routed catalog; as-of-routed scans; serving the magic
tier's schema seam and the fixed-rule payload's stored-input seam),
the NNF/DNF machinery (negated Unification = UnsafeNegation, negated
Search = the shared NegatedSearchUnsupported; two lookup closures —
schema for named fields, full handles for search), `normalize_args`
(expressions → fresh bindings + unifications; REPEATED variables →
fresh binding + equality unification; ignored → generated), the
TWO-ROUND well-ordering (positive applications bind; pending atoms
insert as soon as their inputs are bound; still-pending at the end
refused with the upstream-verbatim UnsafeNegation/UnboundVariable),
and `SessionFixedRule` (payload from epoch stores + the view; output
BRANDED with the manifest arity, "never a caller-supplied one"; the
budget's kill flag shared so a cancelled query stops the rule; the
budgeted output armed with the TRUE GLOBAL admitted total "counting
every prior admission, not just this writer's own rows — so a
row-amplifying algorithm refuses mid-run") — closed)
- **L1:** SPLIT along its own three-part structure: the normalizer →
  `exec/plan/` (the NNF/DNF/well-ordering passes feed compile.rs; the
  file's own landing note names this re-homing); `SessionView` →
  `session/` (it IS the session's read surface, serving plan and
  rules); `SessionFixedRule` → the `rules/contract.rs` boundary
  (where a fixed rule's payload, branding, and cancellation are the
  contract's substance).
- **L2:**
  - **gold:** parts that know their own destinations; the catalog-boundary ruling for search atoms; brand-with-manifest-arity; the global-admission budget arming; superseded code deleted and its deletion recorded. Nothing condemned.
  - **Condemned:** N/A
  - **Watch:** N/A
  - **NEW-SEAT:** N/A
## query/trials.rs (2964 lines; inventory: MPL header, module doc (two
README claims under "The engine keeps its word" demonstrated AT SCALE
against the sealed oracle; test-only over the pub(crate) eval seams;
determinism-as-a-law — seed-reproducible generator, finite Budget so
"a ceiling turns explosion into a typed refusal", answers + witness
tables + refusals byte-identical across 1/2/4/8 threads; answers-that-
show-their-work — proof trees reconstructed from the witness table and
verified by an INDEPENDENT checker; and the STATED OPEN GAP: "the
demand rewriter has no end-to-end differential anywhere today...
Closing it is scheduled at the session tier... Boundary stated, not
smuggled"), `#![cfg(test)]`, the splitmix64 `Rng` (+one_of), the
ModelBody/ModelFixed RuleBody/FixedRuleEval harness (shared laws
helpers per issue #89 — "this harness used to hand-copy them"; the
occurrence-map ruling repeated), the transcribed Bellman stratum
assignment ("this scaffolding must not lean on the judge's internals —
any valid stratification yields the oracle's fixpoint"), `compile_for`
(with fixed rules), `model_arities` (from the MODEL alone — "an
oracle-empty relation must still carry a real arity, or an
over-derivation into it would be an invisible vacuous pass"),
`real_eval`, THE GENERATOR (five meet lattices; `GenParams`'s eleven
dimensions incl. `cross_join` — a non-self-healing two-delta join
built to discriminate "a delta-discipline mutant that threads only the
first contained store's delta (which pj's repair rule masks)";
`meet_pos0` and `meet_interleaved` exercising the positional
MeetAggrStore that retired MeetNotSuffix; safe-by-construction strata;
EDB "sized into the thousands"; opaque `fixed_endpoints`), CAPABILITY
1 (the pool-width guard — "a 1-thread 8-thread run proves nothing";
`differential` over every IDB relation; `run_seed`'s four claims incl.
the empty-witness-table vacuity guard and refusal determinism on BOTH
budget dimensions with exact spends; KYZO_TRIALS_SEEDS/BASE env knobs;
the named regression-pin placeholder), CAPABILITY 2 (the `Proof` tree;
`reconstruct` with the witness-table boundaries documented —
derivation-less admissions return None; `index_witnesses` first-
witness-wins; the independent `verify` — "imports no EVALUATOR
symbol... re-derives each step's binding from scratch, so a corrupted
proof cannot pass by echoing eval's own reasoning", with the #89 note
on the REMOVED hand-rolled check_unify — "drift, not a deliberate
second algorithm"; the negated-premise boundary refused rather than
pretended; the fixture; every-derived-fact reconstruction incl.
intermediates; the FOUR-corruption negative control — premise tuple,
conclusion, out-of-range rule index, and the sibling-rule
mis-attribution via flip_interior_rule; generator seed
reproducibility with a substance floor), CAPABILITY 3 (the temporal
generator twin, with the section doc's precise EPISTEMICS — (a)
resolve vs derive_intervals are "genuinely TWO independent
algorithms", (b) diff/compose is "a mathematical identity... not an
independence claim", (c) the pushdown check proves WIRING not
algebra; `gen_temporal_history` with same-valid-instant corrections
at even odds; HIST_RELS in fixed order so reproducibility never
depends on map iteration; `shuffle_body` carrying the Mutant-C
lineage — the body-order invariance that survived every campaign
because generators "happened to always emit positives before
negatives", now hunted at scale; `program_grid` ±1-and-extremes with
the i64::MAX−1 sentinel reasoning; the >5000-case grid differential
over generated PROGRAMS plus union/join wiring; randomized-bounds
composition on both axes; the pushdown differential; and THREE
HAND-MUTANT pairs, each proving a weakened generator/grid is
structurally BLIND to a companion sabotaged oracle twin that the real
one catches — no-Erase vs erase-as-retract, non-negative-only vs
abs-value sort, and stored-coordinates-only vs the short-end boundary
bug with the honest COUNTED comparative claim: "a coordinates-only
grid CAN still catch this... the honest claim is comparative"),
CAPABILITY 4 (the refusal-lift coverage: an existential-payload
temporal generator, `neg_lit_at`, the ReachabilityFixture running
recursion, THE LIFTED negation-over-as-of shape, and both aggregation
families over the SAME historical relations at the SAME coordinates,
bodies shuffled; four independently written references — brute-force
closure, set complement, group-and-count, and meet propagation whose
FOLD reuses the real landed Aggregation ops "per this module's own
header doc: a bug in an aggregation must never hide behind a parallel
test-only reimplementation" while the LOOP stays independent, with a
termination guard; ≥800 cases; the section doc stating precisely what
is and is not proven) — closed)
- **L1:** preserve-and-move with a NAMED SPLIT across the trials
  lanes: the generator + Capability 1 → `crates/kyzo-trials/src/gauntlet.rs`
  (generated-program hunting) with its determinism assertions feeding
  `determinism.rs`'s lane; Capability 2 → the proposed
  `crates/kyzo-trials/src/provenance.rs` (NEW-SEAT, shared with
  query/provenance.rs's entry); Capabilities 3–4 →
  `crates/kyzo-trials/src/time_travel.rs` beside time_travel_trials.rs's
  material. Same crate-wall rewire as its siblings (pub(crate) eval
  seams → public surface or a sanctioned deeper seam; the oracle side
  is already kyzo-oracle vocabulary). OPERATOR-VISIBLE STANDING ITEM:
  the module's own stated open gap — no end-to-end demand-rewriter
  differential — is scheduled at the session tier (runtime/db.rs
  wave); the migration must carry that obligation forward, not lose
  it in the move.
- **L2:**
  - **gold:** the stated-boundary discipline (open gaps named in the doc, never smuggled); generator dimensions justified by the exact mutant each discriminates (cross_join's masking argument); hand-mutant pairs that prove the CAMPAIGN's own eyes (a weakened generator shown blind, the real one shown to catch); the counted comparative claim over a boolean where the boolean would overclaim; the epistemics sections stating what each oracle-vs-oracle check does and does not prove; model-derived arities against vacuous passes; the real-landed-ops fold rule for references; fixed-order generator vocabularies for seed reproducibility. Nothing condemned.
  - **Condemned:** N/A
  - **Watch:** N/A
  - **NEW-SEAT:** N/A
## query/eval.rs (5015 lines; inventory: dual fork header with FIVE
story-#3 transformations (BUDGET IS A REQUIRED PARAMETER — the
original's only controls were a Poison flag set by a sleeper thread
and an unbounded `for epoch in 0u32..`; deterministic dimensions
checked at epoch barriers ONLY "so a refusal is a pure function of
program+facts+budget", the deadline read inside rule iteration closing
the unkillable-scan gap, Poison surviving only as the user-kill flag;
NO UNBOUNDED FIXPOINT EXISTS; provenance hooks at the admission seam
with zero-cost-off by monomorphization; THE EVALUATOR CONSUMES A SEAM,
generic over RuleBody/FixedRuleEval), the THIRTEEN-site Law-5 panic
audit (every unwrap/unreachable accounted: constructor proofs,
store_of/store_of_mut, the checked all-aggregated flatten, the landed
per-group aggregation API), deviations D1–D5 (dead skip-flags removed;
the D2 intra-epoch limiter dedup fixing upstream's double-count; the
D3 non-suffix refusal SINCE RETIRED by positional grouping; delta
iteration over contained_rules keys with typed missing-store; one
execution closure for all epochs) and notes N1–N2 (N1: "the
load-bearing re-derivation dedup is merge_in's... removing the filter
is survivable, removing merge_in's dedup is not"; N2: the preserved
limiter cross-epoch overshoot, pinned by name), module doc (THE
DETERMINISM LAW: same program+facts+budget ⇒ identical results AND
identical refusals at any thread count, with its four supports —
immutable epoch reads, the sequential canonical-order merge barrier,
barrier-only deterministic dimensions ("a mid-epoch check would
observe a schedule-dependent partial count and is therefore a
determinism bug"), and the sequential entry-under-limit carve-out;
the three seams incl. the WASM clock note), the budget tier
(`BudgetDimension` — DerivedTuples/InFlightDerivations/Epochs/
Deadline with the deterministic-vs-interrupt split; `LimitExceeded`
carrying rule+span for the mid-epoch dimension only; `Killed`;
`Budget` with epoch_ceiling/check_interrupt pub(crate) per story #80
for the oracle's own barrier loop; INTERRUPT_STRIDE=64;
`InterruptTicker` carrying the FULL mid-epoch spend-guard theory in
its doc — the determinism law (baseline+in_flight a deterministic
UNDER-approximation with bounded slack), the NON-PERTURBATION THEOREM
(plain out-stores hold only admissions so len IS the count; meet
epochs fold unchanged re-derivations so len OVERCOUNTS — "the refuted
theorem" — repaired by meet_put_admission_faithful's monotone
transition counting: "it refuses ONLY queries the barrier would also
refuse — earlier, before the OOM"), and the BOUNDEDNESS LAW (peak
resident O(P·(ceiling+STRIDE)), "independent of the input relations'
product size — that is the guarantee the incident violated")), the
SEAM (`AtomOccurrence` — positional not name-keyed, with the
self-join rewrite Δ(P⋈P)=(ΔP⋈P)∪(P⋈ΔP) and the predecessor Many
collapse named; `Premises`/`PremiseSource`; `RuleBody` with its
five-clause contract incl. the Cow slice-consuming economy —
"re-derived and rejected rows — the bulk of every recursive fixpoint
— therefore allocate nothing on either path"; `FixedRuleEval`
run-once with the baseline handoff), the evaluable tier
(`HeadAggrKind` — one concept one name vs data::aggr's;
`EvalRuleSet::new` classifying with POSITIONAL meet keys and the
retired-MeetNotSuffix history; `EvalProgram::from_execution_order`'s
entry proof), the limiter (`RowLimit`/`QueryLimiter`), the witness
machinery (`Witness` with derivation None for fold/fixed/identity
rows; `WitnessTable` append-only in admission order;
`WitnessBinder` recovering meet group keys by projection),
`stratified_evaluate(_with_stores)` (lifetime-dropped stores; the
clobbered-store debug_assert with the exactly-one-stratum argument),
`evaluate_stratum` (the epoch loop, the epoch-fixed baseline, the
sequential-entry carve-out, rayon-vs-wasm dispatch, THE MERGE
BARRIER, both barrier refusals), `note_pending` (first-writer-wins),
`provenance_graph` (`ProvenanceUnsupported`; the COLLAPSE BOUNDARY —
"aggregation folds and opaque algorithms are not semiring
operations", their tuples ground out; premise rows VERIFIED against
their attributed stores; the enumeration ceiling; limiter-blind
noted), `project_positions`, and the four eval functions
(initial/incremental plain — the slice-probe prev_store filter;
initial meet — the identity row inserted ONLY if empty and
all-aggregated "so the identity can never leak alongside real
derivations"; incremental meet — the admission-faithful `effective`
counter with its full justification; initial normal-aggr with the
empty-fold row), and the ~3200-line test battery: the STREAMING
ModelBody harness (stream_join replacing the frontier Vec that "grew
to the whole cross product below the budget's tick seam" — reviewer
finding F3 — with leaf order proven byte-identical); the transcribed
stratum assignment; model_arities (the vacuous-pass repair: "an
oracle-empty relation used to default to arity 1... any
over-derivation into such a relation was invisible");
assert_matches_oracle with arity-drift check; fifteen fixed
differentials (TC, self-join Many, stratified negation,
normal-aggregation over recursion + the empty fold, meet min on
cycle, the and/or premature-fixpoint end-to-end pins at BOTH suffix
and pos0 layouts, interleaved split columns AND interleaved
recursion, identity-row-feeds-recursion, negation over completed
meet, fixed rules on boundaries, mutual recursion, two delta-carrying
deps killing per-delta truncation, the meet self-join Many
multiplicity reviving dead branch M6, and
two-recursions-converge-independently pinning `changed |=` against
last-store-wins); the randomized proptest differential over five
lattices with the HONEST enumerated list of shapes the generator
still cannot produce, each cross-referenced to its fixed pin;
thread-count determinism for results+witnesses, non-suffix meet, and
refusals; exact-spend budget refusals; the mid-epoch in-flight
section (refuses BEFORE the barrier with THE BOUNDEDNESS PROOF
asserted on the emitted counter — "remove the mid-epoch check and
emitted becomes 160_000"; byte-identical across threads; the
canonically-first tripping rule); the REFUTED-THEOREM counterexample
landed as a differential (the 500-cycle equal-seed sweep:
binary-searched true spend asserted = 502, the barrier refusal's
spent proven to be TRUE ADMITTED SPEND, and the entire old divergence
window swept to byte-identical completion); five mutation-hardening
kills (exact-at-ceiling completes killing `>=`; the stride pinned BY
LITERAL because "a bound written in terms of the symbol moves with
the mutant", with the deliberate-change escape hatch named; the
nonzero-baseline pin at spent=163; its fixed-rule twin proving the
true-global-baseline handoff both refusing and completing; the F3
harness killer — a 100M-row cross product refusing typed and
stride-bounded under the memory cap); deadline-zero; the kill flag
observed MID-iteration with a promptness bound; the limiter pair
(take-minus-skip; the TRACED incremental entry recursion pinning D2's
dedup and N2's overshoot epoch by epoch, expecting exactly take+1
rows); witness pins (canonical order with exact derivations; meet
identity None; the non-suffix binding attack; the per-group premises
KILLER — two groups folding to the same value, where prefix-keyed
witnessing binds one group's witness to the other's derivation); the
adopted rev_* attacks (Nulls in group and value, shared key/val
variable, all-aggregated recursive, negation-below determinism at
scale, the randomized NON-SUFFIX proptest); and the construction
refusals (empty set; the retired D3 shape now constructing AND
answering; missing store typed; entry-less refused; epoch-ceiling-1
refuses any deriving program while 2 suffices) — closed)
- **L1:** preserve-and-move with a NAMED SPLIT inside exec/: the
  fixpoint engine (Budget, InterruptTicker, the seam traits, the
  evaluable tier, the limiter, evaluate_stratum and the four eval
  functions) → `exec/fixpoint/eval.rs` ("the loop: recursion over
  admitted currency" — Admitted IS that currency); the parallel-epoch
  dispatch and merge-barrier determinism machinery align with
  `exec/fixpoint/parallel.rs` ("deterministic sharded parallelism");
  the provenance constructs (Witness/WitnessTable/WitnessBinder/
  PendingWitnesses/provenance_graph/ProvNode/PremiseSource) →
  `exec/provenance/` ("derivations that explain themselves"). The
  differential/determinism test batteries migrate with their zone as
  module tests; kyzo-trials grows public-surface twins per the
  sibling entries.
- **L2:**
  - **gold:** the determinism law with its four supports stated as an invariant system (barrier-only checks named as a determinism REQUIREMENT, not a style choice); the non-perturbation theorem WITH its refutation history and the landed counterexample-as-differential; the boundedness law tied to the incident it forecloses; N1's do-not-strip warning on the load-bearing dedup; refusals as first-class deterministic outputs (byte-identical across threads, exact spends, honest admitted-not- materialized accounting); mutants killed by LITERALS where a symbol-relative bound would move with the mutant; the honest generator-gap list cross-referenced to fixed pins; the traced limiter semantics (D2/N2) preserved as documented behavior rather than silently "fixed". Nothing condemned. Carried obligations: D3's full retirement is complete (the tests prove construct AND answer); the story-#80 pub(crate) widenings (epoch_ceiling, check_interrupt) are the sanctioned oracle seam — the kyzo-oracle split must give the oracle its own budget vocabulary or keep this seam deliberate.
  - **Condemned:** N/A
  - **Watch:** N/A
  - **NEW-SEAT:** N/A
## query/laws.rs (5058 lines; inventory: MPL header, module doc (THE
REFERENCE SEMANTICS as executable law — "deliberately naive... written
to be OBVIOUSLY correct"; the oracle is judge, never production; the
abstract program form minimal "so it can outlive any concrete AST"; the
REAL-LANDED-AGGREGATIONS rule — "a bug in an aggregation cannot hide
behind a parallel test-only reimplementation"; aggregation semantics AS
LAW — normal folds once at the fixpoint beneath, all-meet heads recurse,
the all-aggregated identity row inserted only when round one derives
nothing "exposing it alongside real rows would... derive facts outside
the least fixpoint", fixed rules on boundaries; THREE deliberate
upstream divergences all in the oracle's favor — the non-suffix meet
demotion upstream froze into wrong answers, order-dependent
aggregations deterministic here but arrival-order artifacts so
"differential harnesses must avoid or canonicalize them", and
no-entry-symbol whole-program judging vs upstream's dead-rule pruning;
THE TIME-TRAVEL NEGATION LIFT — the engine's former refusal "was always
an operator-implementation gap, not a semantic one", the oracle's
structural never-gap argument (a negated literal's as_of is a TERM-FREE
AsOf constructible only as a constant; a historical relation is always
a SINK in the stratification graph, "exactly like an EDB fact
relation"), check_time_travel_negation "deleted whole" because general
safety and stratifiability already prove the lift sound; THE SHARED
REFERENCE-TIER HELPERS — issue #89's consolidation of three
byte-for-byte hand copies with the soundness argument ("all three
modules are reference tier — they judge the ENGINE... never each
other") and the ONE deliberately-independent copy NOT touched,
stratify.rs's aggregation_character), the #119/#120 target-split
dead_code note, the temporal vocabulary (`AsOf` with THE EXACT
CORRESPONDENCE to the Reverse-wrapped real type — "the two types encode
the identical total order through inverted representations", proven by
the kernel cross-check "rather than leaving that claim as an assertion
in a doc comment"; `Event` with the TERMINAL-TICK RESERVATION at
construction (the hostile-review ruling keeping a zero-width
[MAX, MAX) interval UNREPRESENTABLE) and the `untimed` embedding as "a
real, callable function rather than a comment"; `resolve_events` — the
brute-force twin of check_key_for_bitemporal with
Assert-holds/Retract-settles/Erase-transparent; `resolve`/
`resolve_relation`; `Axis`/`Interval`/`OPEN_END`/`derive_intervals`
(coalescing definitional; the sys-axis breakpoint filtering rationale);
`SignedFact`/`diff` (on resolved snapshots, never intervals)/`compose`),
the program model (`Literal` + four ONE-SEAM constructors carrying "the
lesson of story #62's compiler-forced fallout across five files";
`neg_at`'s legality argument at the constructor; `Rule`/`FixedRule`/
`Program::untimed`; `Rejection`'s five variants), the checkers
(`check_safety` law 4; `HeadClass`/`head_classes` shared;
`dependency_edges` with the poisoned-edge rules; `check_stratifiable`
law 2; the `NameIntroduction` refactor — "one predicate applied
uniformly, not three separately-argued refusal loops that could drift"
(issue #85 sharpening #62); `check_wellformed`'s eight refusal families
incl. facts-XOR-histories and the three-failure-modes comment on the
historical-namespace law; `strata` Bellman-Ford with its convergence
argument), the shared `Bindings`/`unify`/`ground`, the evaluator
(`literal_rows` — per-literal AsOf "pushed down to the stored leaf the
literal names, never precomputed above it", zero behavior change for
untimed programs; `body_bindings(_from)` positives-then-negatives;
`derived_rows` preserving fold multiplicity; `eval_normal_aggr_head`;
`MeetState`; `naive_eval`/`naive_eval_at`/`naive_eval_at_budgeted` —
story #80, ADDITIVE never a replacement: "the naive oracle's whole
reason to exist is the TRUE answer, and a mandatory ceiling would put
a second, lesser claim in its place", barrier-granularity only with
the no-per-rule-ticker rationale; `check_oracle_budget` carrying the
REAL LimitExceeded messages; `naive_eval_at_impl` with the
identity-row-at-round-one rule and the 100k loud non-termination
bound), the STORY-#61 INCREMENTAL LAW (the scope doc: recursion
refused unconditionally — DRed territory, with the DAG-makes-it-sound
argument; aggregation FULLY covered by group re-derivation because
"min/max under retraction is the classic case with no such formula";
fixed rules refused with the empty-delta wrong-answer argument; the
two-phase candidates-then-verify algorithm with the MULTISET-VS-SET
pitfall "the generative differential caught the first time it ran";
`edb_relations` zero-rows-still-EDB; Kahn's `topological_order`;
`has_any_cycle`; the sign-agnostic subset-expansion candidate
collectors; `head_is_derivable`; the aggregation extension;
`incremental_eval` with the redundant-patch filter and its
caught-phantom-delta lineage), the SHARED `unstratifiable_corpus`
(eight named refusal programs "shared between the reference checker's
self-tests and the real compiler's"), and the ~2570-line test battery:
the five law pins; budgeted-additive proofs; the identity-row REVIEW
FINDING pair (feeds recursion; INVISIBLE when derivations exist, with
the larger-lattice argument that and/or cannot tell but min's Null
can); `FlagMode`/`semi_naive_meet_reach` (upstream's semi-naive
transcribed) and the inverted-flag differential pinning BOTH the
honest match and the stranded-at-seed premature fixpoint; the
naive-vs-semi-naive proptest over five lattices; the unified temporal
oracle's NINE degenerate pins; the terminal-tick pair; the KERNEL
CROSS-CHECK (a from-scratch skip walk over the real
check_key_for_bitemporal, negative valid AND sys coordinates folded
into the STORED fixture per the hostile-review pin — "sign-boundary
coverage belongs in the fixture, not only in the probe grid"); the
three name-collision refusals (rule head with the arity-isolation
note, fixed head, fixed INPUT closing #85's silent-empty-read); the
untimed-embedding byte-identity; per-literal pushdown inside
naive_eval; negation-without-own-as-of not refused; the lift's
HAND-TRACED fixture (coordinates A, B=A-swapped, and current, chosen
mutually distinct so axis-swap AND silent-default-fallback mutants
each fail, with the what-this-proves epistemics doc); the SOURCE-ORDER
regression (negated literal written FIRST — a deleted reorder panics
loudly via ground's missing key, "fails loudly, not silently"); the
two >5000-case grid campaigns (interval-vs-resolve, negation WIRING
with its full does-not-re-prove-resolution doc); and the incremental
battery (recompute_patch ground truth; ten fixed pins incl. the
two-stratum double sign flip, subset expansion at |varying|=2, the
min rescan, group-vanishes vs global-identity-revert, and both
refusals; the 4-shape × 80-seed campaign) — closed)
- **L1:** preserve-and-move with a NAMED SPLIT into kyzo-oracle (THE
  REFERENCE SEMANTICS — "deliberately slow, small enough to
  hostile-review line by line... the crate wall makes independence
  physics"): the program model + checkers + naive evaluator →
  `crates/kyzo-oracle/src/eval.rs`; the temporal vocabulary (AsOf, Event,
  resolve*, derive_intervals, diff/compose, Axis/Interval/OPEN_END) →
  `crates/kyzo-oracle/src/temporal.rs`; the story-#61 incremental reference →
  NEW-SEAT `crates/kyzo-oracle/src/incremental.rs` (operator ratification;
  alternatively folds into eval.rs — the react/incremental.rs
  production twin needs its judge either way); the shared
  unstratifiable_corpus rides with eval.rs as the judge's contract
  surface (lib.rs). ARRIVAL QUESTIONS FOR THE OPERATOR, both created
  by the crate wall's "depends ONLY on kyzo-model": (1) the #80
  budgeted door (`naive_eval_at_budgeted`) consumes kyzo-core's
  `eval::Budget` — the oracle must grow its own bounds vocabulary (or
  model must carry one), and the verify door's direction of dependency
  (core → oracle for ::verify) must be ratified against the map's
  dependency arrows; (2) the real-landed-aggregations rule requires
  `Aggregation`'s FOLD implementations to be model vocabulary the
  oracle can reach — reconcile with the map's oracle-owns-its-OWN-expr
  stance (expr.rs): expressions deliberately independent, aggregations
  deliberately shared, both on the record as rulings. DOC CORRECTION
  on arrival: the module doc's "cfg(test) only" claim is already
  superseded by #80's production ::verify consumer — the target
  formulation is the map's own ("the judge's contract: same question,
  independent answer"), not test-only.
- **L2:**
  - **gold:** obviously-correct-by-inspection as the design criterion (optimizing the oracle is a defect); the three upstream divergences recorded WITH their directions; the lift's structural never-gap argument and its deleted-check ruling; the one-seam constructor discipline with its five-file-fallout lesson; the #89 sharing-soundness argument paired with the one deliberately-independent copy; the terminal-tick reservation making the zero-width interval unrepresentable; the exact-correspondence doc PROVEN by the kernel cross-check; additive-budgeting so the true answer stays the oracle's claim; the multiset-vs-set lineage; review findings landed as paired positive/negative pins; loud-failure regressions over silent ones. Nothing condemned.
  - **Condemned:** N/A
  - **Watch:** N/A
  - **NEW-SEAT:** N/A
## runtime/relation.rs (2070 lines; inventory: dual fork header with TEN
named re-architectures (the system keyspace TYPED — `SystemKey` a
closed enum, "no fourth shape can appear by accident", with the
Where-STORAGE_VERSION-went record: merged into FormatVersion which
refuses "strictly earlier and stricter"; indices BY REFERENCE — "the
index relation's own catalog row is the single authority"; the
original's `lock: bool` DIES — "the kernel is full SSI... there is no
unlocked read to ask for"; the id counter a transactional RMW replacing
a process-wide AtomicU64 "which leaked ids on abort and could not
survive a second process"; destroy's del_range in-transaction so
"an aborted transaction now rolls the destruction back"; law-5
fallibility throughout; `amend_key_prefix` DELETED — "splicing a
different relation id into an EncodedKey's bytes would launder
unproven provenance into the typed key"; TWO fixes-on-port —
ensure_compatible's trivially-true self-comparison that never
type-checked input dependents, and create_relation's dead store +
opposite-store name check; rename REFUSING with indices attached —
the original stranded index rows under the old name), module doc (a
handle is "a decoded catalog row — knowledge, not authority... the
store's bytes remain the truth"; SSI concurrency with "no catalog
locks and no process-wide atomics"; SEAMS documented not hidden —
temp routing owned by the session tier, triggers-as-source with the
Phase C parsed-substances end state, the landed index manifests,
IndexPositionUse's provisional home; the WIRE FORMAT law — msgpack
struct maps "IS an on-disk format... changing it is a migration
conversation, not a refactor"), `SystemKey`/`RelationIdSpaceExhausted`/
`next_relation_id` (routed through raw_decode's single bounds check),
`AccessLevel` ("the Ord derive IS the semantics... Do not reorder the
variants") + `InsufficientAccessLevel`, `IndexRef` (relation_name —
"the one place the {base}:{index} convention is spelled"),
`IndexKind` (Plain; TEMPORAL with its full design doc — the valid
instant promoted ahead of the base key "answering 'what changed
at/near instant t' with a contiguous scan", scan-shaped-not-search-
shaped, "a Plain mapper cannot express this kind... the leading
column is the WRITE'S OWN coordinate"; Hnsw/Fts/Lsh with the
MIGRATION RECORD — unit variants grew manifests, "decode-compatible
with every store ever written" because the seam refused attachment
before), `TEMPORAL_POSTING_LEADING_COLUMN`, `ConstraintRef`,
`IndexPositionUse` (provisional home, compile.rs imports it),
`KeyspaceKind` (Facts vs AlgorithmState — "versioning them would
corrupt its invariants"), `RelationHandle` (+RelationDeserError with
its version-mismatch-ruled-out help), THE CATALOG SERIALIZATION
BOUNDARY (RULED: row values are the value plane's canonical
encodings, catalog METADATA is structured configuration — msgpack's
ONE door, SEALED by a private supertrait so "no row value can ever
be routed through msgpack", with the compile-time ABSENCE PROOF —
an ambiguity-trick that fails the build if DataValue ever implements
CatalogRecord), the handle impl (constructors; the encode family
with zero-clone bitemporal key encoding and its bulk-write
rationale; put_fact/retract_fact; the polarity-byte value format
with the id-not-repeated note; ensure_compatible;
choose_index — longest bound prefix, back-join coverage, law-5
edges degrading to no-index; and the SCAN SURFACE — "every method
takes the transaction to read; none of them routes... is_temp is
the routing DATUM, not the router", skip scans stripping the time
slots to LOGICAL rows, the zero-clone projected probes with the
every-mutated-row rationale), the catalog operations (five typed
errors incl. `TempRelationNotRoutable` — "refused here rather than
routed" and `RelationHasConstraints`; `allocate_relation_id` with
THE CONCURRENCY STORY — "uniqueness is isolation's theorem, not an
atomic's side effect"; `write_relation_row` the single row-update
funnel; `destroy_relation` gated on indices/constraints/access;
`set_access_level` DELIBERATELY ungated — "gating it on itself
would wedge them shut"; rename with four gates), and thirteen
tests (system-key shapes with Null<Str proven; the full catalog
lifecycle; the SHADOW-STRUCT corruption test — a hand-serialized
out-of-range id must refuse decode; destroy-in-one-tx killing own
writes; counter persistence; the racing-creates CONCURRENCY PROOF
with typed ConflictError and successful retry; the access ladder;
typed id exhaustion; temp/duplicate refusals; rename keeping id and
keyspace; skip-scan time travel; choose_index edges; the
ensure_compatible fix pin; and the WIRE ROUND-TRIP + PINNED-BYTES
test — PINNED_HANDLE_HEX, "this test failing is that conversation
starting") — closed)
- **L1:** preserve-and-move with a NAMED SPLIT inside session/: the
  catalog (SystemKey, RelationHandle, the sealed serialization
  boundary, all catalog operations, the wire-format law) →
  `session/catalog.rs` ("the store's knowledge of its relations;
  coherent multi-row moves"); `AccessLevel`/`InsufficientAccessLevel`
  → `session/access.rs` ("per-relation protection tiers");
  `IndexPositionUse` relocates to `exec/plan/compile.rs` exactly as
  its own provisional-home note promises; the handle's bitemporal
  scan surface consumes store/skip_walk.rs's one walk on arrival
  (the temporal.rs entry's unfreeze obligation lands HERE: this file
  is where keyspace bounds and the raw multi-version scan gain their
  official accessors). `IndexKind`'s manifests remain the projection
  zones' vocabulary, referenced by the catalog.
- **L2:**
  - **gold:** knowledge-not-authority; the closed SystemKey with the STORAGE_VERSION merge record; the sealed one-door serialization boundary WITH its compile-time absence proof (the house pattern for two-format discipline); Ord-IS-the- semantics on the access ladder; uniqueness-is-isolation's-theorem; the deleted-amend_key_prefix provenance argument; migration records written where the format changed; the pinned-bytes conversation- starter; refused-rather-than-routed seams; the deliberately ungated access setter with its reason. Nothing condemned. The two fixes-on-port are silent-wrong-answer classes upstream shipped — keep their pins forever.
  - **Condemned:** N/A
  - **Watch:** N/A
  - **NEW-SEAT:** N/A
## runtime/mutate.rs (2741 lines; inventory: dual fork header with six
re-architectures (mutation on `SessionTx<T: WriteTx>` — "running it
against a read session does not compile"; the CLEANUPS MACHINERY GONE
— del_range in-transaction so ":replace and ::remove are atomic with
the query and an abort rolls them back"; triggers PARSED ONCE PER
SESSION — sound because a session has one cur_vld, with the
FLAG(catalog tier) Phase C parsed-substances end state carried; index
maintenance a typed BY-REFERENCE seam; law-5 fallible decode + typed
invariant), module doc, the trigger-cascade law
(MAX_TRIGGER_CASCADE_DEPTH=32, `TriggerCascadeTooDeep` — "never silent
truncation... and never an unbounded loop"), `execute_relation` (the
:replace gates — in-trigger, with-indices, below-Normal; old triggers
carried across the replace; `note_constraints` + `touched_relations`
noted for the SEGMENT WATERMARK before commit; the seven-op dispatch),
`put_into_relation` (the SYSTEM-coordinate doc — one transaction, one
stamp; the VALID-coordinate default doc — the stamp not wall-clock,
"snapshot-monotone, so a retrying writer can never land its update at
an instant an already-committed writer has shadowed"; THE LOAD-BEARING
UNCONDITIONAL SSI PROBE — "bitemporal version keys are distinct per
transaction stamp, so two writers of the same fact never collide on
written keys — the fact-range READ this probe conflict-tracks is the
ONLY thing that makes a same-fact race abort one racer instead of
losing an update", resolved AT THIS WRITE'S OWN valid "never an
unrelated later instant"; the :insert duplicate refusal),
`update_in_relation` (must-exist at the write's own valid; the
CARRY-FORWARD of omitted non-key columns with a typed
short-stored-row error), `remove_from_relation` ("retraction is
revision, not erasure: a Retract row at the coordinate, never a
physical delete"; the preserved _new-carries-key-columns-only
asymmetry), ensure/ensure_not (ReadOnly rung; ":ensure can never
carry a @ clause... 'current' always means the newest instant ever
recorded"), `collect_mutations` (triggers in-transaction, callbacks
collected for post-commit), `update_indices` (pub(crate) as the ONE
write-side seam cross-module tests drive; Plain fires both sides
because its mirror is payload-mapped; the TEMPORAL SINGLE-FIRE ruling
with the full hostile-review argument — old and new "compose to the
IDENTICAL posting key at the IDENTICAL coordinate", dual-fire "would
silently let the Assert clobber the Retract... a wasted, SSI-tracked
write, not two events", and the honest epistemics: the invariant is
"content-equivalent to the old dual-fire shape... so no byte-content
test can guard it: the guard is the write-count law test"),
`index_write_row` (the shared scan-shaped seam writing at the base
write's EXACT coordinate; the index's own watermark bump with its
demonstrated-stale-read lineage), `temporal_posting_tuple` (+ the
typed `ShortTemporalIndexRow` for a state "nothing today can
produce"), `project_mapper` (StaleIndexMapper), the extractor tier
(DataExtractor; make_update_extractors' None-means-carry-forward),
`make_const_rule` (the _new/_old Constant injection through
init_options "so the injected options are in the proven form"), the
MANIFEST-INDEX tier (`IndexCtx` resolved once per session and cached
— "a manifest that no longer parses, builds, or decodes is a typed
refusal at first touch, never mid-scan corruption";
`apply_manifest_index` per-engine put/del hooks;
`attach_and_backfill` — temp and duplicate refusals, the
KeyspaceKind dispatch ("a posting IS a bitemporal fact"), the
TEMPORAL BACKFILL as a raw whole-history walk — "resolution is
exactly what would collapse the history this backfill must reproduce
whole" — versus the plain/manifest current-rows backfill with the
0xFF group-clearing resume bound and the re-mints-now note; the five
::create ops incl. HNSW's documented standard derivations and LSH's
pinned-seed byte-identical builds; remove_index), the
bulk_write_tests (the STORE-BYTES-UNCHANGED pin: the 802-row
append-only arithmetic, the MEANING ANCHOR decoding the store back
through the public path BEFORE the SHA-256 whole-store fingerprint —
"a witness over format-CORRECT bytes, not an implementation
snapshot"; the per-row terminal-tick refusal proving whole-mutation
abort with the no-partial-write property located at run_script's
never-committing; and THREE story-#88 coverage-gap pins — the
:insert duplicate branch "ran zero times in every suite run", the
:update missing-key refusal, the carry-forward branch), and the
temporal_index_tests (the direct-SessionTx rationale documented —
no parsed surface for ::temporal index create, Erase has no scripted
surface, "every function called here is the exact same code the
eventual parsed surface would call"; the posting-rows-match-history
fixture WITH the literal hand-encoded first-key-on-disk byte claim;
BACKFILL-EQUALS-INCREMENTAL across two universes with id alignment
asserted and raw-byte identity; the base↔posting bijection; the
production-pipeline both-Some branch test; and THE WRITE-COUNT LAW —
the confirmation reviewer proved the dual-fire mutant is
BYTE-IDENTICAL on committed disk, "no scan of the committed keyspace,
however thorough, can tell the two shapes apart", so the law is a
COUNT claim guarded by SimStorage's put_call_count oracle: exactly 2
puts per mutation kind, 0 dels ever) — closed)
- **L1:** preserve-and-move with a NAMED SPLIT inside session/: the
  mutation pipeline (execute_relation and its op family, extractors,
  triggers, make_const_rule, update_indices and the scan-shaped write
  seam) → `session/admit.rs` ("the write admission path: mutation
  enters here only" — the zone law's ALL-writes-one-path clause is
  this file); the index LIFECYCLE (attach_and_backfill, the five
  ::create ops, remove_index, IndexCtx) → `session/ops.rs` ("operator
  surface"), with the per-engine put/del hooks remaining project/
  zone vocabulary the ops call through. The temporal-index write
  seam's cross-module test contract (ra/temporal.rs drives it)
  survives the move as an admit-path pub(crate).
- **L2:**
  - **gold:** the unconditional SSI probe with its lost-update argument (deleting it is a silent-wrong-answer class); the snapshot-monotone valid-default reasoning; resolved-at- this-write's-own-valid discipline on all three mutation kinds; retraction-is-revision; the bounded cascade as typed whole-abort; the temporal single-fire ruling WITH its no-byte-test-can-guard-it epistemics and the count-oracle guard; backfill-equals-incremental as the rebuildability law; the meaning-anchored byte fingerprint pattern; refusal-at-first-touch manifest contexts; coverage-gap pins named by the branch that never ran. Nothing condemned. Carried obligations: the Phase C parsed-substances FLAG; the unparsed `::temporal index create` surface (the tests' own documented gap) — both operator-visible.
  - **Condemned:** N/A
  - **Watch:** N/A
  - **NEW-SEAT:** N/A
## runtime/db.rs (3076 lines; inventory: dual fork header with six
re-architectures (session SPECIES — "the read/write distinction is a
type, not a convention", the session owns its transaction and is Send;
conflict retry rebuilding "a fresh transaction AND a fresh callback
collector" so "a conflicted attempt leaks no phantom events"; the
cleanups machinery gone; BUDGET REQUIRED BY PARAMETER — "there is no
cooperative-poison thread and nothing sleeps to enforce a limit"; the
typed catalog; fixed rules RUN) and the INTERIM section carrying a
stale-comment mea culpa — "an external audit read the stale claim as
ground truth, which is exactly the failure a comment in this codebase
must never cause"; only ::explain and ::running/::kill remain
deferred), the constants (DEFAULT_EPOCH_CEILING 1M;
MAX_COMMIT_ATTEMPTS 128 with its measured liveness reasoning — "at 32
it was reachable by three writers under a loaded machine, which is
contention working, not failing"; and DEFAULT_DERIVED_TUPLE_CEILING
50M with its extraordinary justification: "not a round guess" —
bench_api's own ceiling REUSED, verified against kyzo-bench's actual
recorded real-graph results with headroom arithmetic
(tc/snap-p2p-Gnutella08's 13.1M rows ≈ 26.3M true spend after the
entry-copy doubling), the rejected fast-refusing alternative that
"would have silently regressed these exact already-recorded
benchmarks", and the structural guarantee that a fixpoint-less query
ALWAYS crosses it), seven typed refusals (incl.
`TempRelationNotReachableError` — review finding F2, "without this
refusal the read path would silently drop the mutation";
`InvalidTimeout` — "the last line of defense before
Duration::from_secs_f64 would panic"), `ScriptOptions`, `Db` (+Clone
— "the handle is a shared view of one universe") and the fixed-rule
registry surface, `run_script(_with)`, `execute_single` (the F2 temp
refusal; the write path's ordered ceremony — fresh tx+collector,
run_query, enforce_constraints, segment bumps BEFORE commit,
retirement evictions AFTER, callbacks after durable),
`compile_and_eval` (the shared read-only heart; ONE kill flag shared
by the budget, every fixed rule's CancelFlag, and every search atom;
the ONE-MACHINE note — "the row-at-a-time twin was deleted; criterion
on a loaded 32-core box had it losing or tying everywhere it was
measured"; the sorted-query limit rule), `finalize_rows`, `run_query`
(mutation preconditions; Segments::OFF in write sessions — "typed
dirty-read protection, pinned by the constraint suite"; :returning),
`run_query_readonly` (segments ON), `run_sys_op` (the full dispatch
incl. the bounded ::merkle_root scan ceiling and SysOp::Verify),
`sys_write`, `build_budget` (tighter-of-two deadlines; typed
InvalidTimeout), the helpers, and `SessionTx` (the routed session
core: the per-session trigger-parse and index-ctx caches;
`current_row_routed` with the full valid-is-the-write's-own-instant
doc AND the SSI range-tracking note; `system_stamp_routed` — "a
transaction's writes are one instant of recorded history";
`destroy_relation`'s retired-id FUNNEL — "three sibling destroy
sites leaked one engine entry per cycle, forever"), and the
~1900-line test battery: the SEGMENTS LAW end to end
(fresh-never-dirty through build/orphan/rollback/index-segment, plus
the issue-#75 join-probe twin); the fixed-rule ceiling and the
BASELINE-FORWARDING pin (exact spent=73 arithmetic from empirically
confirmed counts, with the doc explaining why Ok/Err-only tests can
never catch the regression); the timeout panics closed on both
paths; the named fuzz-artifact overflow regression; the search
pipeline end to end (HNSW with exact squared-L2 distances on both
backends, FTS with post-delete and 1200-hit batch-boundary
resumption, LSH with drop-then-typed-refusal); the first end-to-end
query on both backends; :replace atomicity; RETRY UNDER CONTENTION
losing no update; the reviewers' RETRACTION-GOVERNS pin (the shipped
defect: retractions keyed off script wall time while asserts used
the stamp — "on the sim's logical clock the domains were
incomparable"); the @-clause coordinate-ORDER pin with a
discriminating history; index-as-of = base; the guard idiom through
scripts (with the caught vacuous-division earlier version and the
unguarded mirror proving teeth) and through conjunction pushdown;
5000-row backfill resumption; the runaway-recursion pin (explicit
ceiling names DerivedTuples, not Epochs) and the WIDENING recursion
under the pure DEFAULT budget ("the one deliberately expensive test
in this file", measured ~30s/~2.4GB to the typed refusal); the
bracketing test proving a raised ceiling admits a larger TERMINATING
query (true spend ~999_000 confirmed empirically); obligation 11 —
the magic-sets END-TO-END differential (symbol introspection proves
the rewrite FIRED, answers match naive_eval on both demand
patterns, the disconnected component making demand selective); the
two #68 unadorned-symbols diagnostics; and obligation 12 — the
bench-internals-gated magic-vs-bypass differential (byte-identical
answers AND symbol shape for TC and pointsto, plus the
hostile-review corpus: mutual bf/ff recursion, negation beside an
ff sibling, repeated-variable adornment, and the reviewer's orphan
shape reconstructed WITH the honest disclosure — "included as a
verified-correct adjacent case, not a positive reproduction of the
reviewer's exact finding", the sweep's necessity located in the
tests that DO demonstrate it) — closed)
- **L1:** preserve-and-move with a NAMED SPLIT inside session/: the
  entrypoint (Db, run_script*, execute_single, compile_and_eval,
  finalize_rows, build_budget, the SessionTx core and routing) →
  `session/db.rs` ("the entrypoint: script string to result rows");
  the sys-op dispatch splits by op family — jobs (::running/::kill,
  today's stubs/refusals) → `session/jobs.rs`, operator surface
  (Compact, MerkleRoot, the index DDL dispatch) → `session/ops.rs`,
  catalog ops route through session/catalog.rs, Verify through
  session/verify.rs. The eval.rs entry's carried D3 obligation is
  DISCHARGED here: fixed rules share the budget's kill flag as
  CancelFlag with no Poison revival. The trials.rs entry's standing
  demand-rewriter gap is PARTIALLY discharged by obligations 11–12
  (an end-to-end demand differential now exists at this seam); the
  remaining breadth (a generative corpus through the public path)
  stays open and named.
- **L2:**
  - **gold:** the stale-comment mea culpa as standing doctrine (rule #20 in its own words); the 50M ceiling's evidence-backed justification (rule #19 exemplary — a default defended by recorded benchmarks and a rejected alternative); the ordered commit ceremony (bumps before, evictions after, callbacks after durable); the one-kill-flag design; the one-machine ruling with its measurement; the retired-id funnel; discriminating- history pins over agreeable fixtures; honest reconstruction disclosures in tests. Nothing condemned. The `#[allow (clippy::collapsible_if)]` toolchain-drift note is a dated workaround — re-check on the next toolchain bump.
  - **Condemned:** N/A
  - **Watch:** N/A
  - **NEW-SEAT:** N/A
## storage/mod.rs (549 lines; inventory: MPL header, module doc (THE
STORAGE CONTRACT — "written for that machine... not for any historical
backend's shape"; the two-species genus with consuming commit — "a
committed transaction is not an invalid state to guard against but a
value that no longer exists"; CONCURRENCY ECONOMICS STATED PLAINLY —
reads AND writes are the conflict surface, first-committer-wins,
"uniqueness is enforced by the write itself... a blind put cannot
silently swallow a concurrent insert", batch_put outside the surface,
parallel preparation with serial commit application as the throughput
ceiling, long-reader GC delay, coarse as-of ranges in write
transactions; and the SEALED CONTRACT HISTORY — v2's write-set
validation ruling with the industry comparison on record
("FoundationDB- and badger-class oracles validate reads only...
PostgreSQL SSI and TiKV/Percolator abort write-write races. KyzoDB
sides with the latter"); v3's mandatory bitemporality with the
SNAPSHOT-THEN-MINT proof written in full ("READS-FROM ORDER AGREES
WITH STAMP ORDER... the mint takes the open snapshot as an argument,
so the reverse order is unrepresentable") and the shadowed-forever
lost-update lineage "found live and pinned"; v4's one-cursor-per-walk
skip scan with no caller-visible semantic change), the module decls
each carrying its reason (conformance as the reusable trait-surface
kit; crash_matrix "a separate mechanism on purpose"; sim's
bench-internals cfg gymnastics justified), `SystemClock`
(max(now,last+1) CAS mint; floor/raise_floor for restore),
`FormatVersion` (CURRENT=5 with the whole version history in its doc
— the v4 BUMP-ANYWAY ruling: "the decodable tag space is part of the
format's identity same as the tags already in it"; canonical-spelling
parse refusing non-canonical stamps), `ConflictError` ("retry-on-
conflict is a control-flow decision, not a string match"), the
`sealed` module (one backend by decree; the simulator admitted as
"the contract's own test double... not a second backend"), `Storage`
(concurrent writes "a core requirement, not an option"; the
durability levels stated precisely on sync), `ReadTx` (Slice as
refcount currency; degenerate ranges EMPTY never an error;
whole-range conflict tracking on open — "the conservative choice,
and the one phantom protection needs"; the MANDATORY two-axis
`range_skip_scan_tuple` with the corrupt-key
error-without-advancing rule — "a scan cannot silently step over
bytes it could not judge"; keys-only scans so counts never pay value
I/O), and `WriteTx` (system_stamp minted at snapshot creation;
put/del joining the conflict surface; del_range's
degenerate-tracks-nothing caveat; consuming commit; commit_durable's
honest failure semantics — "the transaction IS committed... the
error reports the durability shortfall, not a rollback") — closed)
- **L1:** preserve-and-move with a NAMED SPLIT inside store/: the
  contract prose, Storage trait, FormatVersion, SystemClock, and the
  seal → `store/contract.rs` ("the storage contract: ordered scans,
  SSI, consuming commits"); ReadTx/WriteTx and ConflictError →
  `store/tx.rs` ("transactions: snapshot isolation, typed
  conflicts"); the module decls are structural glue dying with the
  directory; conformance/crash_matrix/sim/tests migrate to their
  trials/crashfs seats per their own entries.
- **L2:**
  - **gold:** the sealed-history discipline (every contract change recorded WITH its ruling and industry context); the snapshot-then-mint proof and its unrepresentable-by- signature enforcement; the concurrency economics as caller-facing contract prose; the bump-anyway format ruling; honest durability semantics on fsync failure; degenerate-range laws stated at the trait. Nothing condemned.
  - **Condemned:** N/A
  - **Watch:** N/A
  - **NEW-SEAT:** N/A
## storage/tests.rs (3313 lines; inventory: dual MPL header, module doc
(THE LAWS-NOT-SCENARIOS DOCTRINE — "each is a universal property
quantified over all values, because the failure modes that matter here
(cross-type tag disorder, non-monotone float encodings, NaN order
divergence) are invisible to example-based tests"; the three encoding
laws — round-trip, order embedding over ALL pairs, no-panic-on-corrupt;
the oracle discipline for the storage half), the ENCODING-LAW battery
(`corpus` with its rules doc — every variant, ≥2 members each so
cross-type AND within-type pairs exist, the tricky regions enumerated,
"adding a case is one line", nested collections bound by name;
law1/law2 corpus arms — law2 EXHAUSTIVE PAIRWISE so "cross-type
disagreements cannot hide behind sort stability, and a failure names
the exact offending pair"; `arb_value` with regex excluded FOR A REASON;
the generative law1/law2/law3 proptests; `law2_order_embedding_shared_
boundary_generative` — a targeted arm justified by the exact bug shape:
independent i64 draws "almost never share a boundary... so that generic
generator has no power against a comparison that drops one field",
forced same-start and same-end pairs with checked-overflow prop_assume
guards; the vector signed-zero canonicalization law with its
Num-vs-OrderedFloat contrast; the scalar -0.0-collapses pin;
`law3_byte_flip_harness` — every single-byte mutation × three flip
masks over every corpus encoding, "the structured-corruption space the
random Law 3 generator misses"; the value-side law-3 pair incl. the
14-byte hostile rmp payload), the FJALL CONTRACT scenarios
(kv-vs-BTreeMap model; MVCC conflict/discard; RYOW + snapshot
isolation; del_range-kills-own-writes; del_range chunk boundaries;
phantom protection; live-iterator snapshot stability; degenerate
ranges; `inverted_ranges_under_contention_commit_clean` — the
store-poisoning regression driven through EVERY range entry point with
the contention that arms commit-time validation, then proving the
write-serialize lock unpoisoned; `write_write_race_aborts_second_
committer` — the contract-v2 pin carrying the v1 history "re-pinned
KNOWINGLY under the story #3 ruling", put-vs-del races, disjoint
writers, and the empty-write-set-certifies-nothing arm, with its sim
twin named — "the two must stay together"; typed-conflict + options +
stats; 8-thread concurrent writers; compile-time Send+Sync bounds),
the TIME-TRAVEL oracle arm (bitemp_key/vld_row/pol_val helpers;
`as_of_oracle` — "slow and obviously correct"; the full-history
differential at every instant 0..=10; own-writes visibility; the two
MIN-ts termination pins), the BACKUP battery (round trip;
`dump_refuses_a_row_stamped_above_its_own_floor` — layer-3
sabotage-verify with a hand-minted future stamp and the
legitimate-row control; `dumps_never_advertise_a_floor_below_their_
own_rows_under_concurrent_writers` — 8 real writer threads × 200 dump
cycles, each dump's FILE BYTES independently re-parsed "not the
in-process values dump_storage itself computed", with the
wait-for-writers scheduling-artifact guard; restore refuses non-empty;
edge cases — huge length prefix is "an error, not an allocation
abort", truncation mid-pair, truncated floor field;
`restore_raises_clock_floor_past_imported_stamps`), FORMAT VERSION
(stamp + tamper refusal; `pre_value_plane_stores_v4_refuse_to_open` —
the #119 migration boundary "made explicit and executable", pinning
that v4 SPECIFICALLY is refused and CURRENT parses as exactly 5;
canonical-spelling refusals with older-stamps-still-parse "so the
mismatch refusal can NAME it"), CRASH CONSISTENCY
(`crash_consistency_process_abort` — a child process commits, stages,
and `abort()`s, with the SCOPE HONESTY doc: abort simulates a process
crash, "a power cut is a stronger event... testing THAT honestly
requires fault-injection infrastructure (e.g. dm-flakey), not a unit
test that lies about what it simulates"), INTEGRITY (verify_storage
on a REAL kernel-written store — "the catalog-aware verifier needs
the real entry taxonomy... not raw puts below the kernel"; corruption
injected below the kernel surfaces as BadTag and THE WALK CONTINUES
past the wound; `verify_storage_catches_a_corrupt_value` — a
still-decodable key over a corrupt polarity byte, "proof that
catalog-aware per-format value verification is real, not decorative"),
retry-under-contention, THE DST SECTION (sim KV-vs-model;
`sim_write_tx_range_scan_overlay_matches_model` — every merge case
mid-transaction against an overlay model; `sim_mvcc_semantics_smoke`
— the fjall v2 pin's designated twin; spurious-conflict typed +
discards; sim time travel vs the same oracle; batch_put clean-prefix
with the torn-chunk-at-2500 → exactly-2048 assertion;
`sim_read_faults_transient_and_deterministic`;
`sim_interleaving_seed_deterministic_and_diverse` — the log key whose
"final content IS the commit order, and its length proves no update
was lost"; and the SEVEN CAMPAIGNS: (a) 1000-seed retry survival
under spurious storms + interleavings vs the model; (b) 150-seed
crash-at-every-point clean prefix with an uncommitted transaction
staged at the crash; (c) durability tiers distinct — deterministic
arm, failed-fsync arm ("committed, crash-survivable,
power-cut-lost"), 200-seed mixed arm; (d) 200-seed time travel under
interleaved retrying history writers; (e) WRITE SKEW — overlapping
snapshots with crossed read/write sets "must abort at least one side
in EVERY seed", final state one of the two SERIAL outcomes; (f) NO
LOST PHANTOM — commit order observed through the serialized
scheduler, the ["B","A"] branch asserting summary=4 AND an abort
happened, the ["A","B"] branch asserting zero aborts; (g) write-write
first-committer-wins with EXACTLY-ONE abort per seed), the HARDENING
SENTINEL (`sim_fault_plan_identical_at_any_thread_count` — fixed
logical work partitioned across 1/2/4/8 FREE-RUNNING OS threads, no
scheduler, the (op, attempt)→outcome matrix byte-identical per seed
and different across seeds, with both-arms anti-vacuity; "a
positional (global op-counter) fault plan fails this test"),
`sim_retry_liveness_escapes_injected_faults` (90% storms, the
missing-attempt-component mutant named as "mutation-verified"),
`bitemporal_fact_race_aborts_second_committer`, and the CLOCK
batteries (`system_stamps_survive_reopen_strictly_monotone` — an
ABANDONED transaction's mint still raises the floor, "a too-high
floor is safe, a reused stamp is not"; sim stamps through
crash/powercut; `concurrent_increments_lose_nothing_at_the_storage_
layer` — THE NAMED REPRODUCER fjall.rs's snapshot-then-mint proof
cites, 2×200 racing skip-scan increments where "a lost update here
is a conflict-oracle hole, not a query-tier bug"; the Linux RAM-floor
pin with its sane-band assertion "catches a unit mixup... without
pinning an exact host-dependent value") — closed)
- **L1:** NAMED SPLIT — one tier-wide test file the target decomposes
  beside its subjects. The encoding-law battery (corpus, laws 1–3,
  byte-flip, signed-zero, shared-boundary) → kyzo-model beside
  `model/value/canonical.rs` (it tests the codec, not the store; the
  corpus doubles as fuzz-seed material for crates/kyzo-trials/fuzz.rs). The
  fjall-specific pins (inverted-range poisoning, options/stats,
  format-version stamps incl. the v4 boundary, clock/watermark
  batteries, the snapshot-then-mint reproducer, the RAM-floor pin) →
  `store/fjall.rs` + `store/contract.rs` module tests. The backup
  battery incl. both floor pins → `store/backup.rs`. The verify pair
  → `store/verify_walk.rs` (note: they drive Db to build a REAL
  cataloged fixture — on the split that fixture builds through the
  session crate's public surface). The time-travel-vs-oracle arm and
  MIN-ts pins → per-backend module tests beside their cursors (the
  skip-walk theorem already owns the generic proof). The
  process-abort crash test → `crates/kyzo-trials/src/crash.rs` (real-process
  territory, same lane as the FUSE matrix). The DST semantics tests +
  seven campaigns + hardening sentinel + retry-liveness → move with
  the sim instrument to its ratified seat (they are the instrument's
  own proof battery), with the campaign SHAPES feeding
  crates/kyzo-trials/src/dst.rs. CONDEMNED AS SUPERSEDED: the generic
  KV/MVCC/RYOW/del_range/phantom/concurrent-writers/chunk-boundary/
  send-sync scenarios here are the story-#79 kit's EXTRACTION SOURCE
  and are now verbatim-duplicated by conformance.rs's generic laws,
  which fjall and sim already pass via `run_full_battery` — on
  migration these per-backend copies die in favor of the kit call,
  keeping only what the kit deliberately excludes (the per-backend
  pins above).
- **L2:**
  - **gold:** laws-not-scenarios with the invisible-to-examples argument; exhaustive-pairwise order checking; generators justified by the exact mutant/bug shape they catch (the shared-boundary arm, the byte-flip harness, the hardening sentinel's positional-plan kill, the retry-liveness mutant); scope honesty on what a test does NOT simulate (the abort-vs-power-cut paragraph); independently-re-parsed artifacts (the dump-file concurrency pin reads bytes, not the writer's own values); commit-order-observed campaign dispatch with per-branch assertions (write skew's serial-outcomes check, the lost-phantom order match); sabotage-verify with a legitimate control; the abandoned-mint-raises-floor ruling; real-fixture-over-raw-puts for catalog-aware verification; walk-continues-past-the-wound; re-pinned-KNOWINGLY contract history in the test doc; sane-band assertions over host-pinned values. The one condemned class is the superseded kit-source duplicates named in L1; everything else crosses.
  - **Condemned:** N/A
  - **Watch:** N/A
  - **NEW-SEAT:** N/A
## fixed_rule/mod.rs (1965 lines; inventory: dual fork header with the
load-bearing changes list (the STORED-INPUT SEAM — the original payload
held a live &SessionTx; `StoredInputSource` abstracts it, SessionView
implements it in production, `NoStoredInputs` is "the pre-runtime
placeholder it superseded, kept only for its own regression test",
algorithms never see the seam "so their code is final now"; the
ARITY-BRANDED OUTPUT WRITER — "SimpleFixedRule's check made universal";
Poison → `CancelFlag` re-homed with the story-#3 budget integration
named; the `graph` crate replaced by the inline CSR with errors flowing
by straightforward Result "instead of smuggling a captured
Option<Report> out of a filter_map closure"; std rendezvous channel
over crossbeam; LazyLock; the graph-algo feature GONE — "the
algorithms are dependency-free pure Rust, so they are always compiled";
Arc<dyn FixedRule> over Arc<Box<dyn>>; the re-homing ledger; the dead
`InvalidInverseTripleUse` identified and dropped), module doc (a fixed
rule is "an opaque computation the Datalog engine treats as a single
stratum-bounded rule... it never participates in recursion"), the five
module decls, the `TupleIter` SEAM (re-homes note), `CancelFlag` +
`QueryCancelledError` (the polling doctrine — "at least once per unit
of unbounded work — a loop that never checks is a loop that cannot be
killed"; the one-flag integration point: kill switch, timeouts, and the
deadline half of Budget all set it, "so a rule that honors check honors
all of them for free"), the `StoredInputSource` trait +
`StoredInputUnavailable` + `NoStoredInputs`, `FixedRulePayload` /
`FixedRuleInputRelation` (arity, `ensure_min_len` — a SCHEMA-level
guard on declared bindings, binding map, iter/prefix_iter over both
arms, `intern_edges` — the shared skeleton with `checked_node_id`
minting at the intern site: the original's `indices.len() as u32`
"silently truncated... aliasing the 2^32-th node onto id 0"; the cap
is u32::MAX−1 because u32::MAX "stays reserved as the Dijkstra core's
no-back-pointer sentinel"; "the bound is untestable at scale... it is
factored into this function precisely so a unit test can pin the
boundary arithmetic without the allocation"; the two graph builders
with finite/non-negative weight validation and default weight 1.0),
the nine option extractors (expr/string/span/integer/pos/non-neg/
float/unit-interval/bool, each refusing typed with teaching help),
`FixedRuleOutput` (branded arity, every put width-checked) +
`OutputSpendGuard` (THE FIXED-RULE TWIN of eval's mid-epoch
InterruptTicker: a fixed rule "runs to completion INSIDE ONE EPOCH,
filling its output store before the barrier... ever checks the
ceiling", so a near-cross-product output "can materialize an unbounded
intermediate before any ceiling fires — the same hole this story
closes for ordinary rules"; determinism argued — the count is the
store's own distinct size "a function of the algorithm's deterministic
output alone", refusal byte-identical; boundedness ceiling +
OUTPUT_STRIDE, the stride harmonized with eval's) + its two budget
tests (refusal bounded by ceiling+stride, never materializing the
flood; small/unbudgeted never perturbed), the `FixedRule` trait, the
`NamedRows` SEAM (+`to_arrow_ipc` — "the actual production call site
of story #77's encoder", refusing heterogeneous columns),
`SimpleFixedRule` (+`rule_with_channel` rendezvous round trip;
DisconnectedChannelRule), the handle/registry/error tier
(`FixedRuleHandle`, `FixedRuleNotFoundError`, `DEFAULT_FIXED_RULES` —
28 registrations incl. aliases; NotAnEdge/BadEdgeWeight/RuleNotFound/
NodeNotFound/BadExprValue), `MagicFixedRuleRuleArg::arity`, the
`tests_support` harness (`PreparedFixedRule` splitting store-build
from run "so a test can pay that cost once and then time only the
algorithm body"; the `NeverRun` placeholder), and the test battery:
arity-brand refusals (writer, lying rule, SimpleFixedRule riding the
universal check); the stored-seam refusal regression; cancellation
honored mid-run through BFS's per-edge poll; the registry pins; the
channel round trip; `intern_site_refuses_at_u32_bound` (F3 — the
boundary arithmetic pinned at 0 / u32::MAX−1 / u32::MAX); the
graph-builder battery (interning, undirected doubling, default
weight, typed not-an-edge and NaN-weight refusals); THE SYSTEMIC
FINDING TEST `nullary_node_relation_refuses_not_panics_across_algos`
— ten algorithms read a node relation's first column unguarded, "a
NULLARY relation (zero columns)... made every one of them panic
instead of refusing cleanly"; each case built as "a complete, valid,
non-empty setup — options included — so removing just the one guard
under test lets execution reach the real indexing site"; and the two
arrow-ipc wiring tests — closed)
- **L1:** NAMED SPLIT inside `rules/`: the contract substance —
  `FixedRule` trait, payload + input-relation surface, the option
  extractors, `FixedRuleOutput` + `OutputSpendGuard`, `CancelFlag`,
  handle/registry/errors, `SimpleFixedRule`, the tests_support
  harness and the trait-level tests → `rules/contract.rs` ("what a
  fixed rule promises"); the graph-builder half — `intern_edges`,
  `checked_node_id`, the two `as_directed_*` builders, NotAnEdge/
  BadEdgeWeight and the builder tests → `rules/graph_view.rs` beside
  the CSR they build. The two declared seams land as their own notes
  say: `TupleIter` to the tuple tier's iterator species,
  `NamedRows` (+to_arrow_ipc) to the session tier's public result
  type with the encoder call at the envelope boundary. `CancelFlag`
  is engine-wide cancellation vocabulary (budget deadline, kill,
  search atoms all share it per db.rs's one-flag design) — it seats
  at the contract but is consumed across zones; the map's
  contract.rs line ("determinism, seeded randomness") should name
  cancellation explicitly. CROSS-REFERENCE the standing
  parse-tier blocker (data/program.rs, parse/query.rs entries): the
  registry's `Arc<dyn FixedRule>` is resolved and CALLED at parse
  time — the model crate can only bind a declaration-shaped handle
  (name/arity), the live impl attaching at the engine boundary; the
  registry stays engine-side and the LSP arrival question
  (deprecated-sealed lsp_api entry) hangs on the same vocabulary cut.
- **L2:**
  - **gold:** the arity brand as a universal contract ("refused at the first wrong row instead of feeding mis-shaped tuples into downstream joins"); the OutputSpendGuard's hole analysis and its determinism/boundedness laws (the mid-epoch guard doctrine extended to the one place it didn't reach); the checked-at-the-intern-site fix with its reserved-sentinel cap and factored-for-testability honesty; the systemic-finding test's design discipline (every case complete except the guard under test — failure isolation built into the fixture); the superseded-placeholder-kept-for-its-regression pattern; the cancellation polling doctrine; dead code identified-and-dropped in the header ledger; seams declared with named landing sites so "this draft must not reshape a landed file". Nothing condemned.
  - **Condemned:** N/A
  - **Watch:** N/A
  - **NEW-SEAT:** N/A
## fixed_rule/utilities/mod.rs (24 lines; inventory: dual header ("module
docs added; contents unchanged"), module doc, four decls + re-exports —
closed)
- **L1:** structural glue — dies with the directory; the readers and
  constant land in `rules/io/`, reorder_sort in `rules/algo/`.
- **L2:**
  - **gold:** nothing beyond the glue.
  - **Condemned:** N/A
  - **Watch:** N/A
  - **NEW-SEAT:** N/A
## crates/kyzo-core/tests/ — the story-#88 external public-surface suite
The fourteen files below are EXTERNAL integration crates: by construction
of where they live, they reach only the public `kyzo::` façade — the exact
boundary the target architecture SEALS. Their collective verdict is
PRESERVE-IN-PLACE: `crates/kyzo-core/tests/` remains the sealed contract's own
integration suite. The one cross-cutting rewire: on the crate split, the
model-vocabulary re-exports they import (`DataValue`, `Tuple`, `Validity`,
`UuidWrapper`, …) resolve per lib.rs's entry (kyzo-model's public surface
re-exported through the façade), which changes no test text if the façade
keeps the names — the suite is the drift detector for exactly that
decision ("if the public API reshapes and breaks a consumer, this test
breaks first, at the contract").

## crates/kyzo-bin/ — the native host against the map's door layout
The map keeps the crate and renames doors: client.rs → repl/fetch.rs,
relations.rs → bulk.rs (the shared codec), repl/output.rs →
repl/render.rs, server/changes.rs + server/standing.rs → server/feeds.rs,
server/pages.rs → server/console.rs; main/auth/query/bulk/rules keep
their names, engine.rs is a lawful unlisted file (zones are stable,
files grow). Entries below are per-file; the zone law (zone-bin.md)
adjudicates quality.

## crates/kyzo-bin/src/server/mod.rs (304 lines; inventory: dual header — the
richest behavior-change ledger in the crate: no per-request mutability
override (the engine reads mutability "off the parsed program itself...
not a caller-supplied claim"; reopening it "is a runtime-tier design
decision past this story's scope... not a difficulty deferral"); no
/transact ("dropped, not stubbed; the fix is new kyzo-core runtime-tier
API, not a bin-crate workaround"); no /import-from-backup
(restore_storage is whole-store-into-empty by contract, the two real
entry points named); text_query is one line because the envelope lives
in kyzo-core "shared with every future binding"; std::mpsc for /rules;
x-kyzo-auth + the simplified token check with the
column-this-port-cannot-honor reasoning; CompressionLayer gzip+brotli
ONLY with the zstd-sys C-dependency exclusion and the cargo-tree
verification recorded; and the BODY-LIMIT FIX — upstream disabled the
2 MiB cap router-wide, "one oversized request to any endpoint would
buffer unbounded... (a one-connection memory-exhaustion DoS)"; only
/import raises its limit, via --max-import-body-mb — plus module doc,
ServerArgs, `DbState`, `server_main` (startup: open/restore with
panic-on-fail; localhost skips auth; the persisted-or-minted 64-char
token file; CORS; the route table wiring every door; the
security-warning banner off-localhost), `wrap_json` (ok → 200/400),
and `internal_error` (JoinError only — "everything the engine itself
can refuse comes back as Ok(Err(_))") — closed)
- **L1:** NAMED SPLIT per the map: the route table + state + startup
  stay as server/ root; changes.rs + standing.rs merge → server/
  feeds.rs; pages.rs → server/console.rs; the rest keep seats.
- **L2:**
  - **gold:** the dropped-not-stubbed ledger (each upstream feature's absence argued from a contract, with the fix's real seat named); the per-route body-limit DoS fix; the C-dependency exclusion verified not asserted. Zone-law NOTE: server_main's startup unwraps/panics (bind parse, listener, token file write) are process-entry failures, not request paths — lawful under the zone's no-panic-escapes-a-HANDLER clause, but the map's "malformed config is a typed refusal" line wants the bind/port parse lifted to a typed refusal on arrival. Nothing condemned.
  - **Condemned:** N/A
  - **Watch:** N/A
  - **NEW-SEAT:** N/A
## crates/kyzo-lsp/src/main.rs (837 lines; inventory: header, module doc (story
#92 — "the delivery of story #73's designed diagnostics, live in the
editor instead of only after a real run"; every didOpen/didChange
re-validates through kyzo::lsp_api::check_script; the TRANSPORT ruling —
hand-rolled Content-Length framing, no async runtime, "message SHAPES
are not hand-rolled — every request/notification param... are
lsp-types, the same crate rust-analyzer uses, so this server speaks the
real protocol, not an approximation of it"; the CATALOG scope —
initializationOptions.dbPath opens a real Db "so hover-over-a-relation
and completion can answer from the connected store's actual catalog...
not a separately-maintained shadow of it", degrading catalog-free
without one; the DELIBERATELY-LEXICAL go-to-definition argument — "the
document being edited is often mid-keystroke and does not parse at all,
and a feature that only works on valid documents reads as broken to an
editor user"), the WIRE TRANSPORT (`read_message` — clean-EOF vs
mid-message-EOF distinguished "so a truncated stream is diagnosable
instead of read as a quiet shutdown"; `write_message` with the flush
note; notification/response builders), `LineIndex` (LSP positions are
UTF-16 CODE UNITS with the rationale naming exactly which diagnostics
would render at the wrong column — "the escape/codepoint diagnostics
that carry non-ASCII spans"; `position` CLAMPS past-the-end — "a clamp
is a wrong-looking diagnostic, never a crashed server"; `offset` the
honest inverse with the spec's own over-long-position rule), `word_at`,
the DIAGNOSTICS TRANSLATION (`diagnostics_from_report`/`collect_labels`
— one Diagnostic per label across the error tree, "the same shape
parse::fuzz_tests::walk_labels proves every parser error satisfies,
just walked here from the public Diagnostic trait"; Display + #[help]
in one message "so the mechanical fix #73 wrote for a SQL-shaped
mistake shows up in the editor"; the defensive no-label fallback with
its reason), `validate` (empty-on-success because "clearing any
previously-published diagnostics... is just as important as reporting
new ones"), the CATALOG TIER (the `AGGREGATIONS` const with its
drift-honesty note — mirrors the crate-internal list "in spirit...
a drift between the two would weaken a hint or a completion
suggestion, never misreport what the engine actually accepts";
`KEYWORDS`; `open_catalog_db` — dbPath ONLY, "no guessing from
rootUri, since pointing an LSP session at the wrong on-disk store (or
silently creating one nobody asked for) is a worse failure mode";
list_relations/columns_for_relation with the word-shaped-never-
interpolated note; completion_items; hover_at — None over guessing),
the GO-TO-DEFINITION tier (`matching_bracket` — a string/comment-
skipping mini-lexer, "the same class... reject_excessive_nesting uses
on the engine side for the identical reason (this file has no access
to that crate-internal scanner, so it earns its own narrow copy)";
None on unbalanced — "don't guess"; `rule_definitions` — sigil
exclusion by construction, the := / <- / <~ confirmation, spans at
the identifier; `definition_at`), and the SERVER LOOP (initialize
capabilities: FULL sync/hover/completion/definition; didOpen/
didChange with the full-sync-only invariant stated; didClose clearing
stale squiggles; shutdown/exit; unhandled methods ignored; `publish`
— an unparseable URI "is the client's bug, not ours to crash over")
— closed)
- **L1:** SPLIT per the map's kyzo-lsp (main.rs + translate.rs): the
  transport, server loop, and feature handlers stay `main.rs`; the
  diagnostics translation (LineIndex, word_at,
  diagnostics_from_report/collect_labels) → `translate.rs` ("miette →
  LSP: spans to ranges, errors to diagnostics" is that seat's
  substance). REWIRE per deprecated-sealed.md's lsp_api entry: the
  `kyzo::lsp_api::check_script` door dies; validation speaks
  kyzo-model's public parse surface. TWO OPERATOR QUESTIONS, both
  already on the record and now given their third citation: (1) the
  fixed-rule vocabulary cut (lsp_api's entry) — full resolution needs
  name/arity vocabulary in the model or stays engine-coupled; (2) NEW
  and sharper — zone-lsp FORBIDS "depending on the engine (storage,
  execution, session)... the language server needs the language, not
  the database", yet the catalog features (hover columns, relation
  completion) open a real fjall Db and run ::relations/::columns
  through the session tier. The zone law and the shipped feature
  contradict; either the law gains a sanctioned catalog door (a
  read-only introspection contract the LSP may consume) or the
  catalog features move out of the LSP — the operator must rule; the
  census records, it does not choose.
- **L2:**
  - **gold:** real-protocol-shapes over hand-rolled ones with the transport-only exception argued; the UTF-16 position law with its which-diagnostics-break rationale; clamp-never-crash and None-never-guess as the server's posture everywhere; the mid-keystroke argument for lexical navigation (a design choice argued from the user's experience of brokenness); worse-failure-mode reasoning on dbPath; drift-honest hand copies with their blast radius stated (weakened hint, never a misreport). Watch items for the split: the AGGREGATIONS/KEYWORDS consts become model vocabulary when the fixed-rule/aggregation name cut lands (three hand-kept lists — parse's, the LSP's, the grammar's keywords — one concept each); zone-lsp's formatter clause is honored today only by absence — when formatting lands it must call kyzo-model's format.rs, never a local one. Nothing condemned.
  - **Condemned:** N/A
  - **Watch:** N/A
  - **NEW-SEAT:** N/A
