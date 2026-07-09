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
- **L2:** gold, preserve through the cut: `AggrKind`'s unrepresentable
  kind/impl disagreement (rebuild the proof at the new seam); the
  changed-flag contract with the fixed upstream inversions (the flag
  gates delta propagation — a false "unchanged" is a premature fixpoint);
  exact-`Num`-order min/max; `NumAccum` exact-Int sum/product; the whole
  test battery incl. F1/F2. Condemned: `choice_rand` folds UNSEEDED
  `rand::rng()` — nondeterminism in the answer path with no
  determinism-as-data field on `Aggregation` to even declare it; it takes
  the `rules/rng.rs` seeded discipline or is refused — it does not
  migrate as-is. Watch: `Null` doubles as "no value yet" in the meet
  accumulators (min/max document null-skipping; intersection silently
  conflates a real Null row with its identity) — destination law wants a
  typed Option-shaped accumulator. NEW-SEAT: none needed.

## data/arrow_ipc.rs (732 lines; inventory: module doc (purity-verified
dependency argument + scope + two-sided correctness), `ColumnVec` +
`from_values` fit-detection, `ColumnBatch`, the hand-kept flatbuffer
constants, `align8`, `push_struct_vector` (the no-unsafe-Push
workaround), `PlannedColumn`, `validity_bitmap`, le/bitpack/offsets
helpers, `plan_column`, `plan_mixed_column`, `frame_message`,
`write_eos`, `build_field` (with the WIPOffset lesson), 
`write_schema_message`, `write_record_batch_message`, `encode_stream`,
and 8 unit tests — closed)
- **L1:** preserve-and-move whole → `model/envelope/arrow.rs` (seat
  exists). Already model-law: imports value vocabulary only;
  `ColumnVec`/`ColumnBatch` are self-declared export-boundary planning
  types, not currency. One concept (the dependency-free Arrow IPC stream
  encoding) at its natural size.
- **L2:** encode-only BY DESIGN — never let "round-trip" into its
  contract. Typed refusals for heterogeneous/unmapped columns; the
  `push_struct_vector` comment documents WHY no unsafe `Push` impl
  exists — keep it. Paired external judge: `kyzo-arrow-interop` (real
  `arrow`, deliberately OUTSIDE the purity-gated trees) proves a real
  reader decodes the output — the move repoints that crate, never
  orphans it. Keep `build_field`'s absolute-vs-relative-offset lesson
  (first draft's bug, caught by the interop reader).

## data/bitemporal.rs (648 lines; inventory: module doc (kernel vs key
format ownership), `VALUE_HEADER_LEN`/`DEFAULT_SIZE_HINT`,
`ClaimPolarity` + polarity bytes + encode, `claim_polarity_of_value`,
`check_key_for_bitemporal` (slot proofs, claimed-bytes bounds via
splice_both/splice_sys, the three-polarity resolution),
`system_stamp_of_key`, `extend_tuple_from_bitemporal_v`, and the test
module (vts/slot/bikey/skip_walk/oracle fixtures, order-pin test,
2000-case skip-scan-vs-oracle differential, polarity-flip governance,
corruption refusals incl. garbage fuzz loop, value round-trips, and the
2000-case laws-mirror differential with negative timestamps) — closed)
- **L1:** → `store/time.rs` (seat exists), preserve whole. Tests split by
  nature: order-pin, corruption-refusal and in-file-oracle batteries stay
  beside `store/time.rs`; the laws-mirror differential drives kernel AND
  judge, so it crosses to `kyzo-trials`' differential campaign when the
  crate wall goes up — it cannot live beside either party.
- **L2:** gold: `ClaimPolarity`'s polarity-in-value law (one system
  lineage per instant; the assert-vs-retract-at-same-instant
  contradiction unrepresentable); the claimed-bytes discipline ("blessing
  the prefix into `EncodedKey` would launder unproven bytes into a type
  whose possession means provenance" — quote it in the destination doc);
  `TERMINAL_VALIDITY` as bound-never-storable with its refusal test;
  `system_stamp_of_key`'s allocation-free single-slot decode for
  integrity checks. `VALUE_HEADER_LEN = 0` is the fact-payload v1
  versioning seam — keep it named.

## data/json.rs (491 lines; inventory: fork header (one-home rationale +
Bot ruling), module doc (asymmetry law), `JsonData` bridge, `DataValue`
serde impls over canonical bytes (`CanonicalVisitor`), `Diagnostic` for
DecodeError, `RelationId` serde, `json_from_serde`/`serde_from_json`,
`From<JsonValue> for DataValue` ×2, `From<&DataValue> for JsonValue` +
owned twin, `NamedRows::{into_json,from_json}`, `format_error_as_json` +
the two LazyLock report handlers, and 8 tests — closed)
- **L1:** splits three ways: the envelope (From impls, NamedRows codecs,
  `format_error_as_json` + handlers, `JsonData`) → `model/envelope/
  json.rs`; `DataValue`'s serde impls (canonical bytes as the ONE wire
  form — "no second serialization truth to drift") → beside
  `model/value/canonical.rs`; `RelationId` serde → the schema tier.
- **L2:** total both ways but deliberately NOT a round trip — the
  asymmetry is documented law (Bytes/Uuid/Regex/Set/Vec/Validity/Interval
  render one-way; a two-element array never reconstructs an Interval) and
  tests pin the one-way-ness. Gold: the Bot→Null totality ruling (an
  engine bug must not crash whichever binding hits it first); non-finite
  conventions (NaN→null, ±inf→named strings); the ok/message/display
  error envelope. Defect: `bot_renders_as_null_never_panics` has an EMPTY
  body — the `Bot` variant it once tested no longer exists in the value
  plane, so delete the hollow test and the header's stale Bot prose with
  it.

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
- **L2:** gold: `coerce` is parse-don't-validate stated as law ("fallible
  parsing, not validation — downstream never re-checks what coercion
  proved"); the byte conventions (base64 vectors little-endian BY
  DEFINITION, exact element count or refuse — replacing upstream's
  unsafe native-endian pointer cast); F32-as-precision-constraint
  semantics (declared width, values stay f64-canonical, F32 claim
  checked f32-exact with NaN exempt); the reserved-tick refusals
  (`i64::MAX`/`MIN` validity refuse at coercion); validity coercion
  floors shared with `str2vld` so coercion and parse agree on the
  containing microsecond. Note for the successor doc:
  `compatible_with_col` treats nullable `Any?` as a wildcard — a
  deliberate subtlety, state it, don't rediscover it.

## data/sketch/ (mod.rs 239, aggr.rs 455, count_min.rs 400, hll.rs 487,
tdigest.rs 492 — each read whole; inventories: mod (determinism law doc,
xxHash64 primes + round/merge_round/read_le helpers + `xxh64`,
`encode_value`, `hash_value`, golden-vector tests), aggr (lattice-law
table doc, six wrappers + hll_union meet/normal pair, `parse_sketch_aggr`,
9 tests incl. meet/normal agreement), count_min (monoid-not-lattice doc
with the documented-not-implemented max-merge variant, ROW_SEEDS, dims,
add/estimate/merge/to_bytes/from_bytes, 7 tests incl. PINNED
non-idempotence), hll (semilattice doc, HASH_SEED/precision/FORMAT_TAG,
add_hash/add/merge/estimate/serde, alpha, 8 tests incl. 24-permutation
byte-identity and INPUT-ANCHORED fingerprint), tdigest (sorted-fold
determinism doc, k1 scale fns, from_values/from_sorted_weighted/
quantile/merge/serde, `sort_floats` via exact Num order, 9 tests incl.
the honesty test that deliberately does NOT assert associativity) —
closed)
- **L1:** preserve-and-move whole → `exec/fold/sketch/` (seats exist:
  hll.rs, count_min.rs, tdigest.rs; aggr wrappers fold into
  `exec/fold/aggr.rs`'s registry at the cut). This subtree is the house
  standard realized.
- **L2:** arrival check: everything survives verbatim — the pinned
  portable xxh64 with published golden vectors; per-sketch fold-order
  honesty; lattice laws deciding exposure (only hll_union is a meet);
  count_min's non-idempotence PINNED so no refactor promotes it;
  tdigest's non-associativity documented by test; INPUT ANCHORS keeping
  fingerprints functions of format law, not implementation snapshots;
  format tags with bump-on-change discipline. Placement note: `xxh64`
  lives with the sketches because it is part of their stored format — a
  second consumer elsewhere makes it a shared-vocabulary candidate; do
  not let one appear silently.

## data/span.rs (65 lines; inventory: fork header, module doc,
`SourceSpan` + Debug/Display/miette conversions/merge — closed) and
## data/symb.rs (230 lines; inventory: fork header, module doc
(two-namespace law), `PROG_ENTRY`, `SymbolKind`, `Symbol` +
new/prog_entry/kind/is_temp_relation_name/ensure_valid_field +
Deref/Hash/Eq/Ord/Display/Debug, 4 tests — closed)
- **L1:** → `model/program/span.rs` and `model/program/symbol.rs` (symb
  gains its full name). Both model-ready as they stand; preserve whole.
- **L2:** preserve: spans never persisted (serde deliberately absent);
  "errors that cannot say where are not finished errors"; identity is
  the name alone, span rides for diagnostics; the TWO-namespace doctrine
  with exactly one classifier per namespace (variable kind vs
  relation-name temp prefix — they disagree about `_` by design, tests
  pin the disagreement); generated prefixes (`~`,`*`) not valid user
  identifiers so collision is impossible. Watch: the relation-namespace
  classifier living on `Symbol` is mild vocabulary bleed — if the schema
  tier grows its own name rules, it moves there rather than gaining
  siblings.

## data/value/ core (tag.rs 192, canonical.rs 1666, cell.rs 419,
number.rs 841, string.rs 185, prefix.rs 156, proofs.rs 133 — each read
whole; inventories: tag (v1 table doc with reserved ranges + activation
rule, STRUCT bytes, `Tag` + byte/from_byte/ALL, 3 pin tests), canonical
(format doc per kind, `CanonicalBytes` witness, `Datum`, encode family +
skip/decode families + ts-key helpers, 9 tests incl. the independent
semantic comparator differential and format_v1_golden_vectors), cell
(word layout doc + authority discipline, INLINE_MAX, `Value` +
repr-transparent asserts, `Minted`, mint/tag/inline accessors/code/
prefix4/try_cmp_storage/gathered/same_word, 6 tests incl. the pinned
per-kind residency table and the same_word-is-physical trap), number
(identity law + key format docs, class/repr consts, `Num` +
int/float/cmp_numeric + key encode/decode + property tests against an
independent exact comparator), string (GermanStr as a kind-proven Value,
MintedStr, from_str/from_bytes/from_value/inline accessors, 4 tests),
prefix (PREFIX_LEN, prefix4, PrefixCmp, cmp_prefixed, exhaustive +
seeded soundness tests), proofs (`assert_not_impl!` + the absence
proofs: Code no Ord; Value no Eq/Ord; StampedCode no Default;
BulkSpendAuthority/Minted/MintedStr no Clone/Copy/Default;
CanonicalBytes no From/Default; RelationId no From/Default + positive
companions) — closed)
- **L1:** → `model/value/` (seats exist: tag.rs, canonical.rs, cell.rs,
  number.rs, string.rs, prefix.rs, proofs.rs). ONE cut to draw at the
  crate wall: `Value::mint` and the string mints take `&mut Arena` — the
  word layout, tag/prefix/inline laws, and `try_cmp_storage` are model;
  the out-of-line mint path IS the currency door (the `CanonicalBytes`
  witness is what crosses). Arrival check: no execution import rides
  along.
- **L2:** already the house standard — preserve verbatim: the pinned v1
  tag table with reserved-range evolution and the store-level activation
  rule (FormatVersion never inside comparable bytes); `CanonicalBytes`
  witness-not-costume (mint-and-type share one file; the token pattern
  becomes mandatory if the mint ever moves); the independent semantic
  comparator law-locking codec order; Num's identity law (Int(1) !=
  Float(1.0) as query semantics forever; one NaN, no -0.0; the closed v1
  numeric domain — decimal/bigint are NEW kinds, never key extensions);
  the cell's no-Eq/no-Ord discipline with named exact alternatives; the
  ONE prefix doctrine ("two lookalike implementations whose divergence
  would be an undetectable ordering anomaly are structurally impossible
  because there is exactly one"); deref-only-on-tie measured by counter;
  compile-time ABSENCE proofs running in every build. Two closure-read
  finds to keep loud: `Num::to_int_coerced` bounds by the EXACT 2^63
  (i64::MAX as f64 rounds UP and would admit one-past-the-boundary,
  silently fabricating a different index key — the comment is the law);
  and canonical's JSON objects tag every entry key `JSTR` because a
  NUL-leading key once sorted BELOW a shorter object's terminator,
  splitting the two order authorities (adversarial-storage-review
  regression, pinned by `json_object_byte_order_matches_structural_
  order_with_nul_key`). encode_owned sorts sets by their ENCODED bytes,
  deliberately not by `Ord`, so the codec stays the independent authority
  the Ord mirror is law-locked against — no circularity.

## data/value/arena.rs (2372 lines; inventory: module doc (execution-
currency doctrine: codes are epoch-scoped RANKS; sealed code order == byte
order; seal/remap/observer discipline), `CHUNK_SIZE` + chunked `Heap`
(append-only byte storage, freeze-on-rollover), `Span`, `FrozenStore`,
`Entry`, `Run`, `Epoch`, `ArenaId`, `StampedCode` minting discipline +
`stamp`, `Arena` (new/intern/seal/snapshot/frame/epoch/len +
`compare_derefs` counter), `Remap` (apply/tail_len, epoch-crossing carry),
the shared View algorithms + `Frame` (resolve/rank/select/cmp_codes) +
`Snapshot` (owned, Send+Sync), liveness + foreign-arena + stale-epoch
asserts, `Default`, and the test battery (seeded xorshift Rng, the
`Naive` oracle + check_laws/check_snapshot sweeps, the drive harness,
exhaustive seal/snapshot placement sweeps, the stale-stamp exploit pin,
cross-arena stamp refusals, same-epoch observer agreement, tail
arrival-stability, seal-remap carry with post-seal dense byte order, the
deref-counting trio (distinct-prefix zero-deref / tie-must-deref /
all-sealed CodeColumn sort with ZERO derefs), the `#[ignore]`d
bench_code_dedup_vs_byte_dedup micro-benchmark, three 100k multi-epoch
stress shapes with a pinned early snapshot, and contract edges: Snapshot
Send+Sync, empty seal is identity yet advances the epoch, empty string is
a value, chunk-boundary round-trips, snapshot survival across writer
seals/cascades, forged-stamp and out-of-range panics) — closed)
- **L1:** preserve-and-move whole → `exec/currency/arena.rs` (seat
  exists). This is the currency side of the model/currency wall the value
  core entry names: `Value::mint` reaches ACROSS to it via `&mut Arena`,
  and the `CanonicalBytes` witness is what crosses. One concept — the
  interning arena with epoch-sealed rank codes — at its natural size: the
  ~1000 lines of test battery are the oracle-differential and law sweeps
  the currency layer's severity demands, not cohabiting concepts. Tests
  move with it.
- **L2:** gold, preserve verbatim: sealed-codes-ARE-ranks (the zero-deref
  sort fast lane, MEASURED by `compare_derefs` in tests — no
  durable-encoding work in the ordered-iteration hot path);
  prefix-first-then-deref-only-on-tie for unsealed codes, also
  counter-measured; the stamp discipline (epoch + arena identity carried
  in every code; stale stamps and foreign-arena stamps PANIC — the
  reviewer's stale-stamp exploit is pinned as a test); `Remap` as the
  only lawful epoch crossing; snapshot immutability proven against 90k+
  values of writer progress; the `Naive` oracle differential with
  exhaustive placement sweeps; the seeded no-clock Rng. Arrival check:
  `bench_code_dedup_vs_byte_dedup` is a micro-benchmark riding as an
  `#[ignore]`d unit test — on migration it graduates to the bench lane
  (`benches/`) rather than surviving as an ignored test (rule #11
  ledger item, pre-existing not new).

## data/value/code.rs (99 lines; inventory: module doc, module-level
`#![allow(dead_code)]` (#119 foundation note), `Code(u32)` (identity-only
doc + `raw()`), `StampedCode` (code+epoch+arena; `mint` gated on
`StampMintAuthority`; arena()/code()/epoch() accessors) — closed)
- **L1:** preserve-and-move whole → `exec/currency/code.rs` (seat
  exists). One concept (the dense interned handle and its stamped
  spendable form) at its natural size.
- **L2:** gold, preserve verbatim: the no-read-authority doctrine (by
  design NO read API anywhere accepts a bare `Code` — spending requires a
  Frame or Snapshot that verifies arena identity and epoch exactly);
  deliberately no `Ord` (order is the arena's to answer inside a frame;
  `raw()` claims identity order, never value order — the proofs.rs
  absence proof pins this); `StampedCode::mint` requiring
  `StampMintAuthority`, whose only constructor is private to arena.rs —
  authority as a per-concept COMPILE fact, not a module-prefix
  convention (the @authority pattern realized). Arrival check: the
  module-level `#![allow(dead_code)]` is #119-foundation scaffolding
  ("#120 wires it") — it comes OFF at migration when the target split
  wires real consumers; it must not cross silently.

## data/value/column.rs (761 lines; inventory: module doc (the admission
theorem — one container-domain check amortizes a million zero-per-code
spends, sound only because write doors verify every entering stamp; the
gather law — epoch crossing only through the consuming gather doors,
monotone over sealed codes; native arrays as the stamp-free vectorizable
lane), `#![allow(dead_code)]` (#119/#120 note), `Domain` with its
@authority block (arena+epoch+extent; for_observer/absorb_stamp/
admit/admit_to minting `BulkSpendAuthority`; extent ≤ observer visibility
including a snapshot's cut), `CodeColumn` (new_in, stamp-verifying push,
admit → `AdmittedCodes`, consuming gather), `AdmittedCodes` (raw
identity-only view, all_sealed, `raw_sealed` Option gate, resolve,
cmp_at, sort_permutation), `WordColumn` (uniform-container-law doc; push
consumes `Minted` — inline free, wide stamp-verified; admit; gather via
`Value::gathered`), `AdmittedWords` (get/canonical/cmp_at
local-knowledge-then-tie), `Column` enum (Ints/Floats/Bools/Codes/Words),
and the test battery (write-door stale + foreign refusals; admission
refusals for stale containers and contents beyond a snapshot cut;
one-check-then-free spends; sealed fast lane agrees with byte order;
tail-bearing columns leave the lane and still order correctly; gather
preserves values + sortedness and readmits, wrong-remap refusal; word
columns hold mixed residency, gather rewrites handles only, stale
wide-word refusal) — closed)
- **L1:** preserve-and-move whole → `exec/currency/column.rs` (seat
  exists). One concept — the stamped batch container and its admission
  discipline — at its natural size; tests move with it. The `Domain`
  @authority declaration migrates INTACT and the committed authority
  artifacts re-point at the new path.
- **L2:** gold, preserve verbatim: the admission theorem stated as doc
  law with its test pinning the exact snapshot-cut case the theorem
  names; the gather law (a consuming door is the ONLY mint of a
  new-epoch container — stale containers aren't fixed, they're
  inadmissible; monotone remap keeps sorted containers sorted, proven);
  the uniform container law (an all-inline WordColumn is still an
  epoch-domain container — "one law, no special cases");
  `BulkSpendAuthority` minted only by `admit_to` after the three-part
  proof; `raw()` documented as an identity surface, never an ordering
  surface; `raw_sealed` returning `Option` so the fast lane is a typed
  claim, not a caller's guess. Arrival check: the `#![allow(dead_code)]`
  #119 scaffolding comes off when #120 wires the RA engine — same
  discipline as code.rs.

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
- **L2:** gold, preserve verbatim: the zero-canonical-encode-in-fixpoint
  law as an EXECUTABLE test (the arena's own counters prove zero
  intern/zero deref — verify-never-assert realized); the narrow-door
  construction (private field, no from_raw; forge vectors proven absent
  in proofs.rs); the value-oracle differential and the determinism pin
  (schedule-independence is a stated engine law); both @authority blocks
  migrate intact. Arrival notes: when #120 lands the production RA join
  (`exec/op/join.rs`), `join_project`'s naive HashMap probe becomes the
  law-grade ORACLE the verify battery differentials against — the engine
  arriving must not delete the oracle. Watch for the destination doc: an
  empty `out` projection yields `arity.max(1)` with zero codes, so a
  zero-column projection (semijoin/count shape) silently reports zero
  rows however many matches occurred — the door has no
  match-count-without-columns form yet.

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
- **L2:** gold, preserve verbatim: the code-lifetime law held by type
  surface, not convention ("you cannot write codes down; you cannot
  smuggle execution currency out of stored bytes"); the fixpoint
  choreography as a LAW TEST with the borrow checker as its enforcement
  mechanism; the deliberate refusal asymmetry (stamp doors PANIC —
  programmer error; the bytes door returns typed `PushError` — stored
  bytes are data, "storage ingestion is a refusal surface, not a panic
  surface"); validate-then-intern so refusal leaves no partial tuple;
  the vacuity guard in the durability test (a test that proves itself
  non-vacuous is house standard); `RelationId::CAP`'s 0xFF-headroom
  rationale (every assignable prefix stays below the sentinel byte every
  storage consumer assumes). Finding for the destination law: EncodedKey
  is ONE type holding TWO shapes with no discriminant — the bare written
  tuple (encode_row/from_values/from_stored, no prefix; split_key is
  lawful only here) and the relation-prefixed storage key
  (encode_key_with_suffix, TupleT), on which `from_stored`'s arity split
  would REFUSE because the 8-byte prefix is not a canonical encoding.
  The split at migration resolves it (bare form with the currency,
  prefixed form in store/keys.rs as its own type) — do not carry the
  conflation across.

## data/value/wide/ (mod.rs 21, collection.rs 43, uuid.rs 14, vector.rs
38, json.rs 161, regex.rs 241, validity.rs 207, interval.rs 415 — each
read whole; inventories: mod (the faces doctrine: identity law before
bytes, payload encodings live in canonical.rs, residency is the cell's
threshold law never a per-kind decision), collection (doc law — Set
canonicalized at encode, REFUSED not repaired at decode, no separate wide
encoding — + nested round-trip test), uuid (doc law only: sixteen raw
bytes, no variant/version interpretation), vector (doc law: identity =
dimensionality + canonical elements through Num's float law; metrics
never identity; storage order ≠ semantic order — + Num-law component
test), json (`Json`, `JsonNum` finite-proven + `NonFiniteJsonNumber`,
`JsonObj` sorted-unique + `DuplicateKey` typed refusal, `fnv1a64` pinned
v1, 3 tests incl. independent FNV vectors), regex (three-law doc,
`RegexFlags` closed bitset with total `from_bits`, `RegexSource` two
mints — `validated` writer door parsing WITH flags, plane-internal
`from_stored` deliberately NOT re-proving — `compile` the only execution
mint, `CompiledRegexV1` witness + match/replace/find surface, 5 tests
incl. flags-change-the-grammar and flag-vs-inline distinct identities),
validity (`ValidityTs` with `Reverse` in the FIELDS so derived Ord IS the
imported as-of law, `for_assertion` refusing the terminal tick,
`MAX_VALIDITY_TS`, `Validity` + `cmp_as_of_order` named alias,
`TERMINAL_VALIDITY` as max slot ENCODING not magic timestamp,
`StoredValiditySlot` pinned-assert, `AsOf` clock-free coordinate pair, 2
law tests), interval (closed-normal-form doc, `Bound`/`Lo`/`Hi`,
`Interval` with canonicalizing `new`/`range`, i128-widened `wide_ends` so
successor arithmetic never overflows, six Allen primitives + intersects,
boundary-topology predicates, 5 tests incl. the finite-max-vs-unbounded
distinctness ruling, sentinel-free round-trip, and the Allen PARTITION
law — exactly one of 13 relations over an exhaustive grid) — closed)
- **L1:** preserve-and-move each file whole → `model/value/kind/` (seats
  exist for all seven: collection.rs, json.rs, uuid.rs, regex.rs,
  vector.rs, interval.rs, validity.rs); mod.rs's doctrine paragraph
  becomes the kind/ module root. All are already model-law pure values.
  One seam to name at the cut: regex.rs's `CompiledRegexV1` +
  `compile` bring the `regex`/`regex_syntax` crates (pure Rust) into the
  model tree and its match methods ARE evaluation — kept with the type
  because a witness minted anywhere else would need a raw door, and the
  map's own line seats the kind "under one execution contract"; if the
  operator rules model must stay evaluation-free, the witness and mint
  move to `exec/stdlib/text.rs` behind a plane-internal authority token.
- **L2:** gold, preserve verbatim: refusal-over-repair everywhere
  (non-canonical Set bytes, duplicate JSON keys, non-finite JSON
  numbers, reserved regex flag bits, empty-denoting interval bytes all
  REFUSE typed at a door — unlawful values cannot be written down); the
  order-by-shape doctrine in validity (`Reverse` in the fields makes the
  derived Ord unmisreadable); the JSON hash law (FNV-1a trailing,
  accelerator-never-equality-authority, decode verifies, algorithm
  pinned against independent vectors); regex's
  decode-does-not-re-parse ruling ("a decode-side re-check against an
  evolving parser would turn parser drift into format drift");
  interval's discrete-grid identity (closed normal form, one empty
  value, finite `i64::MAX` DISTINCT from unbounded — with the
  sentinel-free round-trip test) and the Allen partition law test; the
  no-unicode-normalization choice stated as deliberate. Cosmetic defect
  to fix on migration: interval.rs's boundary-topology doc block has a
  stray inline `///` (before `has_start`) fusing two doc paragraphs into
  one line.

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
- **L2:** gold: corruption-is-an-error-never-a-panic extended to every
  index read path, defined ONCE because all engines name it; the
  downcast discipline separating codec corruption from storage/IO
  errors (a raw `DecodeError` cannot leak out of an engine as its
  contract). Condemned with the tree: the per-module `#[allow(dead_code)]`
  liveness ledger — in the target, each projection lands with its
  surface or doesn't land; the mod-file-as-status-board pattern dies
  with the monolith crate layout.

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
- **L2:** gold, preserve verbatim: soundness by SIGNATURE, not calling
  convention (the enforcement-ladder ruling — same mechanism as the
  storage layer's `stamp_after_snapshot`); witness equality as the
  entire serving criterion; declining-is-always-sound (the u32 decline
  and the gate decline are one doctrine: a projection is optional
  speed, the fallback pays no more than the build would have); the
  miss map's never-a-source-of-truth claim proven by a loss test;
  Arc-held orphans serving mid-scan readers to completion. Arrival
  notes: `Segments::OFF` threading is door plumbing the #120 operator
  wiring replaces (see bench_api's entry); the process-local watermark
  is sound ONLY while segments are memory-only — if projections ever
  persist, the generation vocabulary must become durable
  (residency.rs's business, name it there on day one).

## engines/gazetteer.rs (889 lines; inventory: header (wholly new KyzoDB
work, no Cozo antecedent, built to the ported kernel doctrine), module
doc (telos: text-to-graph AS A RELATION — tag() yields join-ready tuples;
the dictionary relation as the one truth, automaton rebuilt per compile;
leftmost-longest with ALL entities at the winning span; ASCII-only case
folding derived from the span-truthfulness law — full-Unicode folding is
length-changing, and the FTS Lowercase filter is full-Unicode CORRECTLY
because FTS never owes an offset back; three laws; the fixed-rule
exposure seam), `GazetteerConfig`, typed errors `GazetteerEmptySurface`
(zero-width pattern = definition error) + `GazetteerBuildFailed`,
`gazetteer_dict_metadata`, `Gazetteer` (Option<AhoCorasick> so
correctness never rides on the library's zero-pattern behavior;
entities_by_pattern sorted/deduped; retained config), `Tag`
(document-cased surface), `compile_dictionary` (sorted BTreeMap collector
= pattern order a pure function of relation contents; folded keying
matches the automaton's own ASCII folding so automaton-equivalent
patterns can never tie; typed refusals for arity/non-list/non-string/
empty), `tag` (canonical (start, entity) order), `pattern_count`, and
the test battery (the naive greedy oracle + agreement across documents;
the overlap policy pinned; shared-surface ambiguity emits every entity;
adjacent/repeated; folding keeps document casing; case-variant collapse;
exact mode; multibyte byte-offset pins; no-match-inside-a-char; zalgo/
RTL/ZWJ vs oracle; empty dict/doc; two-compile determinism; the three
typed-corruption cases) — closed)
- **L1:** preserve-and-move whole. The compiled automaton is a resident
  rebuildable structure whose source relation stays the truth — the
  project/ zone's exact kind ("rebuildable speed, never truth") — but no
  tree line names it. NEW-SEAT proposal (operator ratification
  required): `project/gazetteer.rs` — dictionary entity tagging: the
  compiled leftmost-longest automaton over a surface-forms relation.
  The exposure seam the module doc names (a `GazetteerTag` fixed rule)
  lands in `rules/` and CONSUMES this seat; it is chartered by the doc,
  not yet built.
- **L2:** gold, preserve verbatim: the span-truthfulness law and the
  ASCII-only folding DERIVATION from it (a law forcing a design choice,
  stated with the FTS contrast — this is the house's best doc-law
  writing); spans-on-boundaries BY CONSTRUCTION (a non-empty valid-UTF-8
  pattern cannot align inside a char — impossibility, not a check);
  keep-all-ambiguous-entities (dropping a candidate silently is the
  ambiguity a downstream join exists to resolve); determinism as a pure
  function of relation contents via sorted collection; corruption typed
  through the shared `IndexRowCorrupt`. Watch: `compile_dictionary`
  scans the dictionary on every compile with no residency discipline —
  when it arrives in project/, it must adopt residency.rs's
  witness/generation vocabulary instead of growing its own.
