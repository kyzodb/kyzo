---
paths:
  - "kyzo-core/src/query/laws.rs"
  - "kyzo-core/src/query/gauntlet.rs"
  - "kyzo-core/src/query/dst_query.rs"
  - "kyzo-core/src/query/provenance.rs"
  - "kyzo-core/src/query/trials.rs"
  - "kyzo-core/src/query/time_travel_script_laws.rs"
  - "kyzo-core/src/query/time_travel_trials.rs"
  - "kyzo-core/src/jepsen_trials.rs"
  - "kyzo-core/src/storage/conformance.rs"
  - "kyzo-core/src/storage/crash_matrix.rs"
  - "kyzo-core/src/storage/sim.rs"
  - "kyzo-core/src/parse/fuzz_tests.rs"
  - "kyzo-core/src/data/bitemporal.rs"
  - "kyzo-core/src/data/aggr.rs"
  - "kyzo-core/src/data/sketch/**"
  - "kyzo-core/src/data/json.rs"
  - "kyzo-core/src/data/arrow_ipc.rs"
  - "kyzo-core/src/data/span.rs"
  - "kyzo-core/src/data/symb.rs"
  - "kyzo-core/src/data/relation.rs"
  - "kyzo-core/src/data/value/tag.rs"
  - "kyzo-core/src/data/value/canonical.rs"
  - "kyzo-core/src/data/value/cell.rs"
  - "kyzo-core/src/data/value/number.rs"
  - "kyzo-core/src/data/value/string.rs"
  - "kyzo-core/src/data/value/prefix.rs"
  - "kyzo-core/src/data/value/proofs.rs"
  - "kyzo-core/src/data/value/wide/**"
  - "kyzo-core/src/data/value/arena.rs"
  - "kyzo-core/src/data/value/code.rs"
  - "kyzo-core/src/data/value/column.rs"
  - "kyzo-core/src/data/value/exec.rs"
  - "kyzo-core/src/parse/**"
  - "kyzo-core/src/format.rs"
  - "kyzo-core/src/format/tests.rs"
  - "kyzo-core/src/kyzoscript.pest"
  - "kyzo-core/src/query/compile.rs"
  - "kyzo-core/src/query/stratify.rs"
  - "kyzo-core/src/query/magic.rs"
  - "kyzo-core/src/query/graph.rs"
  - "kyzo-core/src/query/eval.rs"
  - "kyzo-core/src/query/sort.rs"
  - "kyzo-core/src/query/search.rs"
  - "kyzo-core/src/query/semiring.rs"
  - "kyzo-core/src/query/ra/**"
  - "kyzo-core/src/query/incremental.rs"
  - "kyzo-core/src/query/standing.rs"
  - "kyzo-core/src/engines/**"
  - "kyzo-core/src/runtime/**"
  - "kyzo-core/src/storage/**"
---

# Migrated — files with a 1:1 successor that move whole to their target home

Guidance grade: high-level review by smell/feel against the target purity
state. A migration is never a bare `mv`: the construct is reforged to the
DESTINATION zone's law on arrival. Files also listed in `split.md` or
`absorbed.md` migrate only the parts those rules don't claim.

## To kyzo-oracle (the judge's law inverts: naive is correct)
- **query/laws.rs** (~5060 lines) → splits across the oracle crate, not a
  1:1 move to `eval.rs`: the naive stratified fixpoint + the oracle's OWN
  admission checkers (`check_safety`/`check_stratifiable`/
  `check_wellformed` + `Rejection` — the refusals the real compiler must
  mirror) → `oracle/eval.rs`; the temporal resolution algebra
  (`resolve`/`resolve_relation`/`derive_intervals` with definitional
  coalescing, `diff`/`compose` with the compositionality law) →
  `oracle/temporal.rs`; `incremental_eval` (the reference IVM law:
  candidates-then-verify with the documented SET-vs-Z-set lesson, honest
  refusals of recursion and fixed rules) is the judge of
  `react/incremental.rs` and stays oracle-side; `unstratifiable_corpus`
  (the shared refusal corpus) feeds the trials refusal campaign. On
  arrival it depends on the model ONLY — with one adjudication forced by
  the wall: it deliberately folds through the REAL landed `Aggregation`
  values ("a bug cannot hide behind a parallel reimplementation") and
  reuses production's `Budget`/`LimitExceeded` — under the map those live
  in exec, so either aggregation-meaning moves to model (already required
  by the aggr.rs cut) or the oracle grows its own folds; decide, don't
  drift. Gains its own expression evaluator (confirmed: `Term` is
  Var/Const only — no expression coverage today). Preserve verbatim: the
  three documented upstream divergences (meet-suffix demotion,
  order-dependent tie-breaks, whole-program-no-entry judging) as
  differential-harness constraints; the shared reference-tier helpers
  (`unify`/`ground`/`head_classes`, issue #89) with their
  soundness argument (consumers judge the ENGINE, never each other), and
  stratify.rs's deliberately-independent `aggregation_character` copy
  staying the engine's; the time-travel negation lift argument with its
  structural proof; `Event`'s reserved-terminal-tick construction refusal
  and the untimed embedding; the budgeted variant's ADDITIVE-never-
  replacement law (unbudgeted callers stay unbounded — the oracle's
  reason to exist is the TRUE answer).

## To kyzo-trials (campaigns: public claims, published seeds)
- **query/gauntlet.rs** → metamorphic campaign; reforge to run through the
  public surface only.
- **query/dst_query.rs** + **storage/sim.rs** → the DST drivers, unified
  under one seed discipline.
- **query/provenance.rs** → the provenance trials; its independent-reference
  checkers must arrive sharing nothing with exec's semiring code.
- **query/trials.rs**, **time_travel_script_laws.rs**,
  **time_travel_trials.rs** → the claim campaigns and temporal law
  batteries. Read note: the three are a deliberate LAYERED-surface
  strategy — trials/time_travel_trials reconstruct the compile→eval
  harness from `pub(crate)` seams (full path, real fjall, in-test naive
  references), while time_travel_script_laws drives the PUBLIC
  `Db::run_script` with real `@`-clause text against an oracle; on
  arrival in kyzo-trials the public-surface layer is the campaign norm
  and the seam-driving layers must either go through the public surface
  or be justified per the jepsen precedent (a public contract-level
  claim). time_travel_script_laws also records that the write-side `@`
  gap it once documented is FIXED — carry the fixed-status note, not the
  stale gap.
- **jepsen_trials.rs** → `serializability.rs`. Read note: it deliberately
  drives the `Storage` trait raw, NOT `run_script` — the script mutation
  path retries whole scripts on conflict and never surfaces a raw abort,
  which a history checker needs. That is lawful under trials-law because
  the storage CONTRACT is a public claim (the conformance kit exists for
  strangers); keep the justification with the code. Its two named
  out-of-scope legs (distributed rig post-replication; crash-fault leg
  sequenced after #31's injector) are honest scope statements — carry
  them into the campaign's doc, not silently.
- **storage/conformance.rs** → the public kit; reforge so a stranger's
  backend runs it unmodified.
- **storage/crash_matrix.rs** → the crash campaign, driving kyzo-crashfs.
- **parse/fuzz_tests.rs** → the fuzz drivers and corpus; generative
  machinery becomes trials property.

## To kyzo-model (pure vocabulary: no IO, no evaluation)
- **data/value/{tag,canonical,cell,number,string,prefix,proofs}.rs**,
  **value/mod.rs**, and **wide/** → `model/value/` (+ `value/kind/`).
  Already the house standard (pinned v1 tag table with reserved-range
  evolution; `CanonicalBytes` as witness-not-costume; the independent
  semantic comparator differentially law-locking codec order to `Ord`;
  compile-time ABSENCE proofs; identity laws stated per kind before
  bytes). ONE real cut to draw at the crate split: `Value::mint` and the
  string mints take `&mut Arena` — the 16-byte word layout, tag/prefix/
  inline laws, and `try_cmp_storage` are model, but the out-of-line mint
  path IS the currency door; at the wall, model owns the word and its
  inline laws, exec/currency owns minting-through-the-arena (the
  `CanonicalBytes` witness is what crosses). Arrival check otherwise: no
  execution import rides along, and the per-kind residency table, the
  `same_word`-is-physical trap test, and the `for_assertion` terminal-tick
  refusal survive verbatim.
- **data/relation.rs** → `model/schema/`, split as the map draws it:
  `StoredRelationMetadata` + its column-compat checks → `relation.rs`;
  `NullableColType`/`ColType`/`ColumnDef`/`VecElementType` and `coerce` →
  `column.rs`. Gold: `coerce` is parse-don't-validate stated as law
  ("fallible parsing, not validation — downstream never re-checks what
  coercion proved"); the byte conventions (base64 vectors little-endian by
  definition, exact-length or refuse — replacing upstream's unsafe
  native-endian pointer cast); F32-as-precision-constraint semantics
  (declared width, values stay f64-canonical); the reserved-tick refusals
  (`i64::MAX`/`MIN` validity timestamps refuse at coercion). Note:
  `compatible_with_col` treats nullable `Any?` as a wildcard — a deliberate
  subtlety the successor doc must state, not rediscover.
- **data/span.rs**, **data/symb.rs** → `model/program/span.rs`,
  `model/program/symbol.rs` (symb gains its full name). Both are
  model-ready as they stand. Preserve: spans are never persisted (serde
  deliberately absent) and "errors that cannot say where are not finished
  errors"; the TWO-namespace doctrine with exactly one classifier per
  namespace (variable kind vs relation-name temp prefix — they disagree
  about `_` by design, and the tests pin the disagreement). Watch: the
  relation-namespace classifier living on `Symbol` is mild vocabulary
  bleed — if the schema tier grows its own name rules, that classifier
  moves there rather than gaining siblings here.
- **data/json.rs** → mostly `model/envelope/json.rs`, but three constructs
  are NOT envelope: `DataValue`'s serde impls (the canonical-bytes wire
  form — "a thin skin over the one codec authority, no second
  serialization truth to drift") belong beside `model/value/canonical.rs`;
  `RelationId`'s serde belongs to the schema tier; `JsonData` (the
  serde_json bridge, kept out of the plane so the plane never depends on
  serde) rides with the envelope. The envelope itself is TOTAL both ways
  but deliberately NOT a round trip — the asymmetry is documented law
  (Bytes/Uuid/Regex/Set/Vec/Validity/Interval render one-way; a
  two-element array never reconstructs an Interval) and the tests pin the
  one-way-ness explicitly; do not let "round-trip" into its contract.
  Gold: the Bot→Null totality ruling (an engine bug must not crash
  whichever binding hits it first), the non-finite float conventions
  (NaN→null, ±inf→named strings), `format_error_as_json`'s
  ok/message/display error envelope. Defect found by read:
  `bot_renders_as_null_never_panics` is an EMPTY test body — a hollow
  test asserting nothing; write the real assertion or delete the name.
- **data/arrow_ipc.rs** → `model/envelope/arrow.rs`, preserve-and-move
  whole: already model-law (imports value vocabulary only; `ColumnVec`/
  `ColumnBatch` are self-declared export-boundary planning types, not
  currency; typed refusals for heterogeneous/unmapped columns; the
  `push_struct_vector` workaround documents WHY no unsafe `Push` impl
  exists). Encode-only by design — do not let "round-trip" creep into its
  contract. It has a paired external judge: `kyzo-arrow-interop` (real
  `arrow` crate, deliberately OUTSIDE the purity-gated trees) proves a real
  reader decodes the output — the move must repoint that crate, not orphan
  it. Keep the `build_field` doc's WIPOffset lesson (absolute-vs-relative
  offsets: the first draft's bug, caught by the interop reader).
- **parse/** (minus fuzz_tests) → `model/parse/` (fts.rs arrives as
  `search.rs`, imperative.rs inside `script.rs`); **kyzoscript.pest** →
  `model/parse/grammar.pest`; on arrival, every parsed-but-unowned grammar
  rule gets an owner or an owned typed refusal. Read-verified: the whole
  tier is claimed-text-becomes-proof, each module stating its proofs in
  its doc (expr: params/arity/literals proven at construction; fts: the
  runtime value-grammar with bounded depth; query: `InputProgram::new`
  called exactly once after the map completes; schema: unique columns and
  real types; sys: configs constructed only from constant-folded,
  range-checked options). Arrival items: parse/sys.rs is where the
  duplicated `FtsIndexConfig` (lsh.rs's recorded obligation) gets its one
  home; and fixed-rule resolution inside parse/query.rs currently binds
  live impls — see the program.rs blocker, resolution becomes name+arity
  proof at the model tier.
- **format.rs** (+ its tests) → `model/format.rs`. Read-verified
  house-standard: proof-becomes-one-true-text (every equivalent spelling
  collapses to one), total over any parseable program, never inspects
  source text or spans; the property suite proves idempotence and
  meaning-preserving round-trip against an oracle the formatter never
  touches. One coupled invariant to keep loud at the destination: its
  precedence tables are hand-transcribed from the Pratt parser's — a
  grammar precedence change must edit both or the formatter emits
  wrong-meaning text; the arrival wants a shared table or a
  cross-checking test, not two copies. The grammar itself
  (**kyzoscript.pest**) carries its own law header: the backtracking-free
  separated-sequence rewrite with per-rule equivalence proofs (the
  O(2^depth) upstream shape was a remote DoS from query text) — those
  proofs are format law and cross with the file.

## To exec (one currency, one evaluator, deterministic everything)
- **data/value/{arena,code,column,exec}.rs** → `exec/currency/` —
  preserve-and-move whole; this quartet is #119's target-quality output
  and much of zone-exec law was derived from it (epoch-scoped observers,
  `StampMintAuthority`'s uncallable-not-just-unnamed minting, the
  Domain admission theorem — one container check amortizing a million
  spends, the gather-door-only epoch crossing, deref-only-on-tie measured
  by counter, exhaustive/differential law batteries incl. all-seal-
  placement enumerations). The model/currency cut is already drawn IN the
  code: the arena interns raw bytes plane-internally and the value layer's
  one door is `Value::mint` spending a `CanonicalBytes` witness — at the
  crate split that witness spend IS the model→exec wall crossing; verify
  nothing else crosses. Interim note that dies with #120: all four carry
  module-level `#![allow(dead_code)]` justified as "target-split" — the
  wiring story removes them; they must not survive into the destination.
- **query/{compile,stratify,magic,graph}.rs** → `exec/plan/`, each
  read-verified 1:1 with its law already in-file: compile.rs's
  transformation catalog (free functions over the read species; the
  occurrence-keyed `contained_rules` fix with #68's measured 18-43×
  evidence; constructor-proof rule sets mirroring the parser's
  head-aggr check; `bind_for_eval` implementing eval's seam; premises
  deliberately NotRequested until operators grow provenance); stratify.rs
  (poison-span diagnostics pointing at the ESTABLISHING atom; the
  deliberately-independent `aggregation_character` engine copy per issue
  #89; two-index-space enumeration killing reversal arithmetic; the
  oracle's refusal corpus run through the REAL stratifier); magic.rs (the
  demand-never-semantics law; `AdornedHead` turning an "impossible
  options" comment into structure; the checked u16 narrowing whose silent
  wrap would merge supplementary relations — changed RESULTS, not just
  demand); graph.rs (explicit work stacks because rule graphs are
  user-shaped; the release-skipped `debug_assert` upgraded to a typed
  invariant so cyclic input refuses instead of silently truncating; an
  independent naive SCC reference in its tests). Stale pointer found by
  read: magic.rs's law citation names `.claude/rules/query.md`, which no
  longer exists — repoint to the zone rule at the move.
- **query/eval.rs** (~5000 lines) → `exec/fixpoint/eval.rs`; the
  provenance seams it carries are load-bearing and must survive the move
  proven (the semiring trials stay green). Read-verified gold that crosses
  intact: the determinism law with its stated mechanism (schedule-
  independent rule evaluation, ONE sequential merge barrier in canonical
  order, deterministic budget dimensions checked at the barrier ONLY —
  "a mid-epoch check would observe a schedule-dependent partial count and
  is therefore a determinism bug" — with the mid-epoch InFlightDerivations
  guard engineered to stay deterministic anyway); the 13-site upstream
  panic audit and D1–D5 deviation catalog; N1's warning (eval's
  `prev_store.exists` filter is an optimization — merge_in's dedup is the
  enforcement; strip the wrong one and recursion double-counts); N2's
  preserved limiter overshoot; the seam design (`RuleBody`/
  `FixedRuleEval`/`AdmissionSink` — the compile tier plugs in, nothing in
  the fixpoint changes); `provenance_graph`'s collapse-boundary honesty
  (aggregations and fixed rules enter as ground facts; full provenance is
  claimed only for the positive plain-rule fragment). The test battery is
  a two-front differential (in-file oracle mirror + thread-count
  determinism incl. byte-identical REFUSALS) and moves whole. Doc drift
  found by read: header deviation D3 still says non-suffix meet heads are
  refused pending positional grouping — the tests show that grouping has
  LANDED (`non_suffix_meet_head_constructs_with_positional_grouping` and
  its rev-differentials); fix the header at the move.
- **query/ra/** → `exec/op/` (`temp.rs` arrives as `delta.rs`, `fixed.rs`
  as `literal.rs`). Read-verified: mod.rs centralizes the transformation
  record (11-site panic audit; `NegRight` making an unlawful negation RHS
  unrepresentable; the retired-WITHOUT-successor validity check — the
  universal bitemporal format left nothing to inspect; positions-not-names
  after compilation) and the positional delta discipline (only the ONE
  named `AtomOccurrence` reads its delta; negation always reads totals).
  temporal.rs is an independently-written production twin of the oracle's
  interval/diff algebra (wall discipline held) with the one-fact-buffering
  law (memory O(one key's history), never O(relation)) — and carries a
  NAMED debt the merge must collect: it duplicates the relation keyspace
  bounds computation because `runtime/relation.rs` was frozen under
  another builder's fix at the time ("a small, named duplication, not a
  design choice") — unify at the cut. stored.rs already imports segments
  and the batch machinery — its arrival must route those through
  `project/` contracts, not sibling imports.
- **query/sort.rs**, **query/search.rs** → `exec/`.
- **query/semiring.rs** → `exec/provenance/semiring.rs`.
- **data/aggr.rs** (~1930 lines) → NOT a 1:1 move: it welds declaration to
  implementation, the same weld split.md condemns in expr.rs's `Op`. The
  meaning half (aggregation names, the meet-vs-normal kind as data — what
  stratify needs to rule on recursion legality, what the oracle needs to
  implement independently) → `model/program/aggregate.rs`; the fold objects
  and factories → `exec/fold/aggr.rs`. Gold, preserve through the cut: the
  `AggrKind` unrepresentability (kind and impl cannot disagree — rebuild
  that proof at the new seam), the changed-flag contract with its fixed
  upstream inversions (the flag gates delta propagation; a false
  "unchanged" is a premature fixpoint), exact-`Num`-order min/max, the
  `NumAccum` exact-Int sum/product, and the whole test battery
  (semilattice laws with flag pinned both directions, beyond-2^53
  regressions, the F1/F2 mutation-proven holes). Condemned: `choice_rand`
  folds via UNSEEDED `rand::rng()` — nondeterminism inside the answer path;
  it either takes the `rules/rng.rs` seeded discipline or is refused, it
  does not migrate as-is. Watch: `Null` doubles as the "no value yet"
  identity in the meet accumulators (min/max document null-skipping;
  intersection silently conflates a real Null row with its identity) — a
  sentinel the destination's illegal-states law wants as a typed
  Option-shaped accumulator. **data/sketch/** → `exec/fold/sketch/` —
  preserve-and-move whole; this subtree is the house standard realized.
  The mod doc IS the determinism law (pinned portable xxh64 hand-rolled
  against published vectors; per-sketch fold-order honesty; lattice laws
  deciding exposure: only hll_union is a meet). The tests carry the full
  discipline: count_min's NON-idempotence is pinned so no future refactor
  promotes it to a meet; tdigest's non-associativity is documented by a
  test that deliberately does NOT assert it; the pinned fingerprints use
  INPUT ANCHORS (hand-pinned canonical encodings) so goldens are functions
  of format law, not implementation snapshots. Arrival check: all of that
  survives verbatim. Placement note: `xxh64` lives with the sketches
  because it is part of their stored format; a second consumer elsewhere
  would make it a shared-vocabulary candidate — do not let one appear
  silently.

## To react (continuity: provably equal to recompute)
- **query/incremental.rs** → `react/incremental.rs`.
- **query/standing.rs** → `react/standing.rs`.

## To project (rebuildable speed, never truth)
- **engines/{hnsw,fts,lsh,sparse,spatial,gazetteer}.rs** →
  `project/{vector,text,dedup,sparse,spatial,text}/` — each arrives into
  the uniform per-engine shape (maintenance, search, law) and the
  projection contract. Read-verified: every engine already carries its law
  in its module doc and these must survive verbatim — hnsw's loud distance
  semantics (L2 is SQUARED) and the exact `min(k, matches)` filtered
  contract with strategy selection; fts's post-filter semantics (`k`
  counts MATCHING rows) and TF-IDF-not-BM25 honesty; lsh's candidate-SET-
  not-ranking contract (smallest-k-by-key, filter-invariant); sparse's
  admission gate (NaN/negative/repeated-dimension unrepresentable, stored
  weights re-checked so a corrupt store can't poison a score) and FIXED
  summation order; spatial's curve-order-equals-byte-order law, pinned
  CURVE_BITS as a format decision, and the typed antimeridian refusal;
  gazetteer's leftmost-longest all-entities-at-winning-span policy with
  truthful byte spans. The typed-corruption doctrine is ALREADY one
  authority (`engines/mod.rs`: `IndexRowCorrupt` + the `index_rows`
  boundary that distinguishes decode corruption from IO) — it becomes part
  of `project/contract.rs`. Recorded obligation carried in lsh.rs: the
  `FtsIndexConfig` type is duplicated between `parse/sys.rs` and fts —
  one concept, one name at the cut.
- **engines/segments.rs** → `project/current.rs`, reconciled with the one
  residency/generation discipline. Read note: the #82 rebuild-storm fix
  has LANDED here (writers bump before commit, readers witness after
  snapshot — both enforced BY SIGNATURE after a hostile review proved the
  documented-ordering version racy — plus the consecutive-miss rebuild
  gate). `project/current.rs` inherits a working discipline, not a to-do.
- **engines/text/** splits on the ownership line the read confirmed:
  `mod.rs` (TokenizerConfig as pure data with two moments of truth —
  definition-time validate NEW over upstream, use-time build still
  fallible because stored data is never trusted) and `ast.rs` (FtsExpr +
  the tokenize-through-the-index's-own-analyzer law) are OURS,
  house-standard → `project/text/`. The `tokenizer/` (16 files,
  tantivy-derived — mod.rs still carries tantivy's own schema examples in
  its doc) and `cangjie/` (4 files, MIT, attribution preserved) subtrees
  are vendored foreign code: provenance headers are present throughout
  and there is no unsafe, but the docs are not ours and the code keeps
  foreign habits (split_compound_words.rs alone has ~36 unwrap/expect
  sites) — the owned-or-replaced ruling is still the operator's; they do
  not cross to the target tree as-is.

## To store (persistence and nothing else)
- **storage/fjall.rs** → `store/fjall.rs`; **storage/mod.rs** contract →
  `store/contract.rs`.
- **storage/skip_walk.rs**, **storage/retry.rs**, **storage/backup.rs**,
  **storage/temp.rs** (→ `scratch.rs`), **storage/verify.rs**
  (→ `verify_walk.rs`), **storage/merkle.rs**. Read-verified laws that
  cross verbatim: skip_walk's single-positioned-cursor design (the
  stateless seek seam re-derived fjall's whole read path per version
  step — the fix is the module's reason to exist); backup's
  ascending-order format contract (restore's `batch_put` requires it);
  temp's entertaining-vs-committed distinction and its
  typed-refusal-until-the-router-lands posture; verify_walk's
  CATALOG-AWARE three-codec taxonomy ("a partial verifier reported as
  storage verification is worse than none"); merkle's cold-scan purity
  (content-addressed root independent of write history, deliberately
  non-incremental).
- **storage/tests.rs** (~3300 lines) splits by what it tests: the
  encoding LAW battery (round-trip, order embedding over ALL pairs
  cross-type included, no-panic-on-corrupt-bytes) belongs beside
  `model/value/canonical.rs` as its test submodule; the kernel
  contract/transaction tests stay beside `store/`; anything quantified
  over `S: Storage` folds into the conformance kit.
- **data/bitemporal.rs** → `store/time.rs` — the key law lives with the
  keys. Preserve whole: `ClaimPolarity` with its polarity-in-value law
  (one system lineage per instant; the assert-vs-retract-at-same-instant
  contradiction is unrepresentable), the skip-scan decision kernel, and
  the claimed-bytes discipline note ("blessing the prefix into
  `EncodedKey` would launder unproven bytes into a type whose possession
  means provenance") — quote that law in the destination doc. Tests split
  by their nature: the order-pin, corruption-refusal, and in-file-oracle
  batteries stay beside `store/time.rs`; the 2000-case differential
  against `query::laws::resolve_relation` drives kernel AND judge, so it
  crosses to `kyzo-trials`' differential campaign when the crate wall
  goes up — it cannot live beside either party.

## To session (the one door)
- **runtime/db.rs** → `session/db.rs`; **runtime/json.rs** →
  `session/json.rs`.
- **runtime/mutate.rs** → `session/admit.rs` — the one admission path,
  named for what it is.
- **runtime/relation.rs** → `session/catalog.rs`; its typed refusals stand
  until the coherent-move story replaces them. Read notes: handles are
  "knowledge, not authority" (decoded catalog rows; the store's bytes stay
  truth) — keep that law verbatim; catalog rows are MSGPACK-serialized
  islands (the very targets fuzz_api's decode fuzzers attack) — a second
  wire format beside the canonical codec that the session/catalog arrival
  must own as an explicit format decision, not inherit silently;
  concurrency is deliberately lock-free (catalog races resolve through
  SSI conflict + retry, no process-wide atomics).
- **runtime/constraint.rs** → `session/constraint.rs`.
- **runtime/verify.rs** → `session/verify.rs` — reforged to summon
  kyzo-oracle instead of an in-crate twin. Read note: today it bridges the
  oracle's `&'static str` names by deliberately LEAKING each distinct name
  once (documented, bounded by catalog vocabulary) — the crate split kills
  the leak by giving the oracle model-owned names; do not port the interner
  as if it were architecture.
- **runtime/callback.rs** → `session/observe.rs`; its feed-shaped parts
  belong to `react/feed.rs`.
