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
  - "kyzo-core/src/fixed_rule/**"
  - "kyzo-core/src/lib.rs"
  - "kyzo-core/tests/**"
  - "kyzo-core/benches/**"
  - "kyzo-core/examples/language_tour.rs"
  - "kyzo-bin/**"
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

## engines/sparse.rs (966 lines; inventory: header (new to KyzoDB),
module doc (the SPLADE/BM25-family shape; one inverted relation `[dim,
src_key…] → weight`; EXACT dot product with FIXED ascending-dimension
summation order; the three tier laws; RA + mutation seams with the
del-before-put discipline; the Qdrant design reference — independent
implementation, no code copied — with the ONE deliberate divergence:
negative weights REFUSED at admission so future WAND pruning is
unconditionally sound, lifted only by explicit scope decision), typed
errors `SparseWeightInvalid` + `SparseDuplicateDimension`,
`admit_sparse` (the single gate: finite non-negative, sorted ascending,
duplicates refused; both put and search pass through it),
`sparse_index_metadata` (dim leading so a posting list is one prefix
scan), `posting_key_scaffold`, `sparse_put`, `sparse_del`,
`sparse_total_docs` (hoisted; unused by dot scoring, provided for the
BM25/df-idf seam), `decode_posting` (typed corruption + RE-CHECKING the
admission invariant on read so a corrupted store cannot poison a score),
`SparseSearchParams`, `sparse_search` (positive-only hits; score-desc +
memcmp-key tiebreak; no-filter truncation up front; `with_capacity`
capped at `min(k, len)` so a caller-controlled k cannot abort the
allocator; the check-BEFORE-push k discipline with its cross-engine fix
note), and the test battery (naive full-scan reference + agreement;
`summation_order_is_pinned` via the 2^24 ULP construction;
tie-determinism + truncation; put/del round trip; byte-identical
two-fresh-builds at BIT level; three corrupt-posting typed refusals
incl. a stored negative weight; empty/all-zero edges; four admission
refusals; filter-counts-matching-rows) — closed)
- **L1:** preserve-and-move whole → `project/sparse/` (seat exists:
  "sparse-vector inverted lists"). The hostile battery joins it as its
  named hostile section (see the absorbed entry). The maintenance seam
  (del-before-put from the mutation tier) rewires to `session/admit.rs`'s
  write path when the tree lands.
- **L2:** gold, preserve verbatim: the single admission gate WITH the
  read-side re-check (unrepresentable downstream AND un-poisonable from
  storage — belt and suspenders each carrying a different threat); the
  Qdrant divergence written as a scope ruling with its trade stated;
  the fixed summation order pinned by a test whose construction (2^24)
  makes order-sensitivity observable; bit-level cross-build identity;
  the allocator-abort guard; `sparse_total_docs` documented as
  present-for-a-named-seam rather than silently unused. Defect to fix
  at migration: `posting_key_scaffold`'s doc comment says the dimension
  slot is "left as `Bot`" but the code pushes `DataValue::Null` — a
  stale reference to the deleted `Bot` variant (the json.rs entry
  records its other residue); the code is right, the words are wrong.

## engines/fts.rs (1242 lines; inventory: dual MPL header carrying the
re-architecture LEDGER (each Cozo defect named with its fix: `SessionTx`
methods → pure functions over the tx species; `unwrap` on every posting
decode → law-5 typed `IndexRowCorrupt`; `l_iter.next().unwrap()` on
And/Near → empty nodes contribute nothing, "the engine never trusts the
shape of an AST it did not itself build"; scoring UNCHANGED and loudly
NOT BM25; `N` hoisted to `fts_total_docs`), module doc (one relation
`[word, src_key…]` → parallel occurrence arrays + token count; query
pipeline; exact scoring formulae; post-filter semantics — k counts
MATCHING rows; RA/mutation/lifecycle seams incl. TokenizerCache keyed by
full index handle name), `FtsScoreKind` (the engine's own vocabulary),
`FtsExtractorType`, `fts_index_metadata`, `extract_text` (Null = not
indexed), `posting_key_scaffold`, `fts_put` (per-distinct-term
collector; del-before-put contract), `fts_del`, `fts_total_docs`,
`LiteralPostings` + `literal_postings` (prefix literals scan
`[value, value·LARGEST_UTF_CHAR]`; three typed corruption refusals),
`compute_score`, `eval_ast` (recursion bounded BY THE PARSER's nesting
limit; And intersects summing, Or maxes, Not subtracts), `eval_near`
(the original's first-literal re-scan PRESERVED "so the semantics match
exactly"; live-position chaining; booster = sum), `FtsSearchParams`,
`fts_search` (CancelFlag checked per fetched row; deterministic
score-desc + memcmp tiebreak where the original left ties to hash-map
order; allocator-abort guard; check-before-push), and the test battery
(naive TF reference; AND/OR/NOT/prefix; NEAR at two distances; the
TF-IDF formula pinned to its exact closed form; delete withdraws;
typed extractor + corrupt-posting errors; stopword-only query
early-returns empty; the k=0 filter-path regression pin — the twin the
sparse_hostile entry demanded, CONFIRMED present) — closed)
- **L1:** preserve-and-move whole → `project/text/` (seat exists: "full
  text: inverted index, analyzers, tokenizers (owned)") as the subtree's
  index/search file; the analyzer plumbing it consumes arrives from
  `engines/text/` (its own entries). Lifecycle wiring (`::fts
  create/drop`) rewires to `session/` when the tree lands.
- **L2:** gold, preserve verbatim: the header's defect-by-defect
  fork ledger (the house's best fork-attribution writing — keep the
  form); NOT-BM25 stated at every seam a reader could confuse it;
  determinism as an IMPROVEMENT over upstream documented as such;
  never-trust-unbuilt-ASTs; recursion bounded by the parser so the
  engine needs no depth guard of its own; byte-compat over elegance in
  `eval_near` (the redundant first-literal re-scan is a documented
  semantics pin — reforge it only with a differential proving equality).
  Question for the trials lane (recorded, not guessed): whether a
  stored term of the form `prefix·U+10FFFF·tail` can escape the prefix
  literal's inclusive upper bound `[value, value·LARGEST_UTF_CHAR]` —
  reachable only if a tokenizer can emit U+10FFFF in a term; the
  hostile battery for this engine should decide it.

## engines/lsh.rs (1323 lines; inventory: dual header + rust-minhash
credit, the re-architecture ledger (TWO ratified on-disk format fixes:
the original's `unsafe` native-endian `Vec<u32>` reinterpretation —
non-portable AND UB — became explicit little-endian, forced by
`#![forbid(unsafe_code)]`; and signatures now hash memcmp-ENCODED
element bytes through seeded portable xxHash32, replacing
`std::hash::Hash`'s native-endian unpinned writes; permutation seeds
drawn deterministically via splitmix64 replacing OS entropy; law 5
replacing `unreachable!()`s; deterministic candidate order replacing
hash-set truncation of an unspecified subset; the
tokenizer-cache-keyed-by-FULL-index-name contract), module doc (two
relations + a persisted manifest; candidate SET not ranking — "fusion
and ranking are joins and score expressions, not API surface"),
`MinHashLshIndexManifest` (wire form IS an on-disk format) +
`get_hash_perms`, the two metadata minters (inverse declared `Bytes`
upstream but always stored a List — the declaration now truthful),
`DEFAULT_PERM_SEED`, `splitmix64`, `HashPermutations`
(seeded new / LE to_bytes / fallible from_bytes), `HashValues`
(element_bytes + ngram_bytes canonical portable encodings; min-fold;
`band_chunks` with an EQUALITY arithmetic guard and u16 band tags),
`LshParams`/`Weights`/`find_optimal_params` (ported intact),
`decode_inv_chunks` (decode BEFORE delete — the original deleted first
then hit unreachable), `lsh_del`, `lsh_put` (re-put removes first;
valueless postings), `LshSearchParams` (unranked-truncation-is-a-
decision doc), `lsh_search` (BTreeSet candidates; smallest-k-by-key on
BOTH filter paths), and the test battery (LE pins + round trips; the
band guard tested against BOTH weakenings of its equality; parameter
search determinism; the MinHash Jaccard law; seeded-draw determinism;
`signature_bytes_are_pinned_and_portable` with an INDEPENDENT ANCHOR
hand-derived from the format law and chained to number.rs's golden
vectors so drift isolates to encoder vs hash; whole-index
two-fresh-builds byte-identity; put/search/del round trip; corrupt
inverse row typed on del AND re-put paths; the manifest wire bytes
pinned as hex) — closed)
- **L1:** preserve-and-move whole → `project/dedup/` (seat exists:
  "MinHash-LSH near-duplicate signatures"). Lifecycle wiring (`::lsh
  create/drop`) rewires to `session/`; the recorded `FtsIndexConfig`
  dedup obligation (duplicated between `parse/sys.rs` and the fts tier
  — one concept, one name) lands with that lifecycle move.
- **L2:** gold, preserve verbatim: both format fixes with their
  rationale prose (the UB-and-portability argument is the teaching
  document for why `forbid(unsafe_code)` is a format-correctness tool);
  the anchored signature pin (three independent derivations chained so
  a failure NAMES its layer); unranked truncation defended as a
  decision (ranking a probabilistic candidate set on signature noise
  "dresses the result as a ranking the structure cannot honor");
  decode-before-delete; the equality guard's both-direction tests
  (mutation-hardened); smallest-k-by-key making the subset
  filter-invariant and platform-invariant. The `jaccard` helper is
  `#[cfg(test)]` — keep it test-only; a production similarity claim
  belongs to the relational tier's explicit expression.

## engines/spatial.rs (1529 lines; inventory: header (wholly new,
capability #44; the inherited `haversine` scalar is the exact re-scoring
primitive; the curve encoding lives INSIDE the memcmp law), module doc
(index = `[curve: 8 BE Morton bytes, src_key…] → (lat, lon)` with the
exact coordinates DENORMALIZED so the re-check needs no base fetch;
`CURVE_BITS = 32` as a pinned format decision; the boundary policy —
non-wrapping boxes, typed `AntimeridianBoxRefused` with the two-box
recipe, kNN over-scanning full longitude at seams; distances angular
radians, "the engine takes no stance on the figure of the Earth"),
format constants incl. `SPLIT_BUDGET`, four typed errors, `GeoPoint`
(`admit` the ONLY constructor — NaN/range unrepresentable past it),
monotone `quantize`, the Morton codec (spread32/compact32/encode/decode),
`SpatialIndexManifest` + `spatial_index_metadata`, `extract_point`/
`posting_key`/`spatial_put`/`spatial_del`, `BoundingBox` (admit/contains/
quantized), the quadtree decomposition (`cell_range`/`decompose_box`
with range merging/`decompose_cell` — a cell is dropped ONLY when
provably disjoint; budget exhaustion coarsens, never under-approximates),
`decode_posting` (curve must be exactly 8 bytes)/`fetch_base`/`scan_box`/
`spatial_range_query`, and the kNN machinery (`KnnParams`,
`angular_distance` identical to the scalar, `RingBox`/`ring_box`
pole/antimeridian snap, `inner_radius` — a SAFE UNDER-estimate so the
stop rule can only be stricter, `Candidate` max-heap with deterministic
tie-break, `spatial_knn` doubling ring stopping when the kth distance ≤
the scanned box's inner radius or the box spans the globe), and the
test battery (memcmp-order == curve-order over 5000 random points —
THE law; ATTACK R5: the quantization ROUNDING MODE pinned via city
fixtures with ≥0.5 fractional parts, "hostile-review F1, killer adopted
verbatim"; pinned curve codes incl. ASYMMETRIC fixtures that kill a
coherent lat/lon axis-swap mutant symmetric fixtures cannot; -0.0/0.0
one cell one key; typed admission refusals asserting concrete error
types; range vs naive full scan 2000×300 + determinism; inclusive
boundary points; the degenerate point-query box as the sharpest
boundary adversary; duplicate coordinates distinct; kNN vs exact sort
1500×120×3; exact ascending distances; antimeridian + over-the-pole
neighbours found; corruption typed; del withdraws; manifest wire
round-trip) — closed)
- **L1:** preserve-and-move whole → `project/spatial/` (seat exists:
  "the space-filling-curve access path"). Companion RA/lifecycle wiring
  lands in `exec/op/search.rs` and `session/` as its own staged patch —
  the file already names that seam.
- **L2:** gold, preserve verbatim: the safe-direction doctrine (every
  approximation — monotone quantization, budget coarsening, inner-radius
  under-estimate, seam over-scan — biased so error costs re-checking,
  never a missed row, and each bias documented where it lives); the
  curve-inside-the-memcmp-law test; the asymmetric axis-ownership pins
  and the rounding-mode pin (mutant-killing tests justified by the
  exact mutant); the kNN stop rule as a proof, not a heuristic; the
  admit-only construction. One gap to close at migration: the manifest
  round-trips but its wire bytes are NOT hex-pinned — bring it to the
  LSH manifest's pinned-hex standard (it is equally an on-disk format).

## engines/hnsw.rs (3948 lines; inventory: dual header with a NINE-point
re-architecture ledger (pure functions over the tx species; `HnswRow`
sum type replacing three-row-kinds-by-convention and positional offset
arithmetic; `VectorId` replacing the -1-sentinel CompoundKey — the wire
keeps Int(-1), only the codec spells it; NaN UNREPRESENTABLE via
`IndexVec::admit` — the original's zero-vector NaN silently PASSED the
radius filter; Int degree replacing a float degree in a Float column;
entry-point scans bounded at layer ≤ 0, killing the canary-as-entry
bugs; saturating degree decrement; full-VectorId neighbour comparison —
the original silently skipped same-row different-field edges; law 5
throughout), module doc (index IS a stored relation; layer convention;
distance semantics LOUD — L2 is SQUARED, "this surprises people";
the filtered min(k, M) contract), the test-only `probe` counter module,
`HnswIndexManifest` (wire hex-pinned) + `HNSW_LEVEL_SEED` + seeded
identity-folded `random_level` (clamped -64; u=0→1.0 so ln stays
finite — the original drew from thread_rng, so every rebuild differed),
`hnsw_index_metadata` (the `dist` column declared `Any` because it IS
sum-typed — metadata matching reality over claiming Float), three typed
errors, `VectorId`, `HnswRow` (Node/Edge with the Boxed-`to` size
rationale/Canary kept as a DELIBERATE belt-and-braces under SSI — "its
removal is a concurrency-semantics decision, not a port decision") +
key builders + write + closed-set typed `decode`, `IndexVec` (admit:
dim/finite/zero-refusal + unit normalization + the subnormal-overflow
re-check; SHA-256 content hash over canonical bytes; `dist` with a
per-metric NaN analysis, the InnerProduct edge "recorded honestly
rather than guarded"), `VectorCache` (loads RE-PROVE through admit),
`entry_point`/`neighbours` (eager, bounded, probe-instrumented), `Beam`
(total order — no two priorities ever equal) + `search_layer` (the
hnswlib-parity termination-guard fix, its non-effect MEASURED
bit-identically and documented), `select_neighbours_heuristic`
(Malkov & Yashunin alg. 4), the write half (put_fresh_at_levels,
read_node_row, neighbours_tagged, `shrink_neighbour` — the
tombstone-reclaim doctrine: a tombstoned edge is unfinished business,
resurrected or finally retired, plus the story-#76 investigation doc:
two candidate mechanisms DISPROVED by direct experiment, the
v_dist ≤ neighbours_calls·m_max0 ceiling argument, the 16k decay
signal, and `fit_power_law` left as the reusable instrument),
`put_vector`/`remove_vec` (inherited disconnect REMARK; graph healing
a recorded ceiling)/`hnsw_put` (admit everything BEFORE writing
anything)/`hnsw_remove`/`HnswKnnParams`/`hnsw_knn` (total-order rank;
appended-column order as a stated contract), the filter-aware block
(Qdrant learning-from credit with the named divergence — full-graph
routing + exact-scan fallback instead of payload-aware edges; pinned
reservoir estimator that only picks WHICH strategy, never correctness;
`build_cand_tuple` defined ONCE so the estimator and the results see
byte-identical filter semantics; Design-V routing-vs-visibility split;
`hnsw_knn_filtered` with the load-bearing fallback; test-only
plan-exposure and fallback-disabling doors), and the test battery
(per_insert_search_cost_is_bounded_by_construction — story #76's
ceiling as a MACHINE-CHECKED LAW; three `#[ignore]`d probes: graph
shape, build-time complexity to n=32000 with global power-law fit,
transaction-lifetime discriminator; tombstone-fix recall at 10k against
an independent brute-force oracle; hand-computed L2-SQUARED layout
pins; cosine ingest normalization; typed refusals at both doors;
canary retirement; content-hash re-put; proptest NaN-impossibility;
row-kind round-trips with wire-shape pins; corruption typed; manifest
hex pin; deterministic geometric levels; byte-identical builds;
equidistant tie-break totality; the `#[path]`-included filter harness)
— closed)
- **L1:** preserve-and-move whole → `project/vector/` (seat exists:
  "dense proximity: graph index, quantized search, filtering"). The
  filter harness moves with it as its test module (see the absorbed
  entry). Lifecycle (`::hnsw create/drop`) rewires to `session/`;
  quantized search (RaBitQ line, #122) lands BESIDE it in the same
  subtree, not inside this file.
- **L2:** gold, preserve verbatim: the nine-point fork ledger (with
  lsh.rs and fts.rs, the house form for fork attribution); LOUD
  distance semantics at every surface a reader could mistake them;
  NaN-unrepresentable-by-construction with the per-metric analysis and
  the honestly-recorded InnerProduct edge; the min(k, M) result-set
  guarantee argued from the relational tier ("a silently short result
  would be a wrong ANSWER, not a recall miss") with its fallback
  PROVEN load-bearing by fallback-disabled tests; the story-#76 method
  — mechanisms disproved by experiment, a ceiling proved by structure,
  then converted into a permanent law test; the determinism ladder
  (seeded levels → total-order beams → byte-identical builds →
  equidistant tie-breaks); metadata-matches-reality (`Any` for the
  sum-typed column); the deliberate canary retention with its ruling
  recorded. The three `#[ignore]`d probes are rule-#11 ledger items —
  on migration they graduate to the bench lane (they are measurement
  rigs, not tests); `fit_power_law` graduates with them.

## engines/text/cangjie/ (mod.rs 16, options.rs 29, tokenizer.rs 56,
stream.rs 62 — each read whole; inventories: mod (the dual attribution:
Cang-jie MIT via CozoDB, preserved verbatim, with per-file provenance
headers and `KYZO DEVIATION` marks as KyzoDB additions; three module
decls), options (`TokenizerOption`: All / Default{hmm} /
ForSearch{hmm} / Unicode), tokenizer (`CangJieTokenizer` over
`Arc<Jieba>`; Default = empty jieba, no HMM; the Tokenizer impl
dispatching to cut/cut_all/cut_for_search/char-fold; KYZO DEVIATION:
the vendored `log::trace!` of every user token REMOVED — kyzo-core
carries no `log` dependency and never echoes user text to logs),
stream (`CangjieTokenStream`: cumulative byte-offset advance over
jieba's contiguous segments) — closed)
- **L1:** preserve-and-move whole → `project/text/` as its vendored
  CJK-segmentation subtree, MIT headers intact. Standing obligation
  (the target-state ruling on the foreign body): this lineage is
  carried only until it is OWNED — documented and typed to house law —
  or replaced; the move is not an adoption.
- **L2:** gold: the attribution discipline (original notice verbatim,
  deviations marked at the exact line, licenses never blended) and the
  no-user-text-in-logs deviation. Watch for the owning rework:
  `CangjieTokenStream` sets `position_length` to the TOTAL token count
  — nonstandard against the tantivy convention (span length, normally
  1); harmless today because the FTS engine consumes only
  offsets/position, but a future consumer of `position_length` would
  inherit a quiet lie. The offset arithmetic assumes jieba segments
  tile the input contiguously — true for every current mode; state it
  as an invariant when the subtree is owned.

## engines/text/tokenizer/ (mod.rs 348, tokenizer_impl.rs 349,
empty_tokenizer.rs 51, raw_tokenizer.rs 78, simple_tokenizer.rs 96,
whitespace_tokenizer.rs 96, lower_caser.rs 96, alphanum_only.rs 103,
remove_long.rs 106, tokenized_string.rs 110, stemmer.rs 135,
split_compound_words.rs 259, ngram_tokenizer.rs 484,
ascii_folding_filter.rs 4062, stop_word_filter/mod.rs 188,
stop_word_filter/stopwords.rs 21891 — code files read whole; the two
data giants closed by construct-boundary enumeration:
ascii_folding_filter is header + filter/stream +
`fold_non_ascii_char` (the Lucene-derived char match table, lines
62–1537, per-char commented, source link kept) + `to_ascii` + a
2500-line vendored test block ending in the exhaustive
`test_all_foldings` fixture; stopwords.rs is a dual-attribution header
(stopwords-iso MIT via CozoDB, content unchanged) + 58 language `const`
arrays tiling lines 11–21891, exactly matching `for_lang`'s 58 arms —
closed)
- Inventory highlights: tokenizer_impl (Token — "offsets shall not be
  modified by token filters", default position usize::MAX; TextAnalyzer
  + `unique_ngrams` for the LSH engine; the trait trio + box-clone
  machinery), the four base tokenizers (whitespace splits on ASCII
  whitespace ONLY — U+3000 stays inside tokens, vendored behavior),
  five filters (lower_caser's no-sigma-special-case note; alphanum
  drops non-ASCII-alphanumeric tokens entirely; remove_long is a BYTE
  limit), tokenized_string (PreTokenizedString/Stream — its re-export
  is COMMENTED OUT: carried but unwired), stemmer (18 languages),
  split_compound_words (full-decomposition-only aho-corasick), ngram
  (StutteringIterator with THE KYZO DEVIATION: the vendored
  `max_gram -= 1` underflowed on post-exhaustion advance — debug panic,
  release wrap that RESURRECTED the stream to emit garbage —
  saturating_sub covers both; a large block of vendored tests commented
  out), and mod.rs carrying the owned LAW-5 HOSTILE SWEEP: every
  tokenizer × a maximal filter stack × zalgo/RTL-override/NUL/1-MiB
  token/UTF-8-lossy soup/combining flood/ZWJ emoji/CJK/empty inputs,
  run to exhaustion AND held exhausted (the underflow deviation's
  regression), plus a bare-stemmer sweep — "vendored code is not
  exempt: a panic is a panic wherever it was written".
- **L1:** preserve-and-move whole → `project/text/`'s tokenizer subtree,
  MIT attribution intact, under the same own-or-replace foreign-body
  obligation as cangjie/. The hostile sweep is OWNED KyzoDB law and
  moves as the subtree's gate.
- **L2:** gold: the hostile sweep's doctrine and its
  exhaustion-stability check; the ngram deviation's failure-mode prose
  (names BOTH the debug and the worse release behavior); the per-file
  attribution form. Cleanup at migration: `tokenized_string.rs` has no
  consumer and its re-export is commented out — Remove unless the
  pre-tokenized door lands with the session tier; the commented-out
  vendored ngram tests either revive against the owned API or go;
  whitespace/alphanum ASCII-only semantics get stated in the owned
  docs (they surprise exactly the users FTS serves).

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
- **L2:** gold, preserve verbatim: the depth-invariant doc (the
  sharpest derived-Drop stack-safety analysis in the tree — bounding
  at the parser is proven STRONGER than an iterative rewrite);
  prefix-literals-pass-whole as a meaning argument; shallow-is_empty
  with flatten-as-normalizer stated as a design pair.

## engines/text/mod.rs (584 lines; inventory: dual MPL header
(`validate` new over Cozo; poisoning recovery; the `indexing` SEAM
comment for the operator tier), module doc (TWO MOMENTS OF TRUTH:
definition-time `validate` before a manifest is written — the Cozo
original stored unknown names and failed at first use — and use-time
`build` staying lazily fallible because "data is never trusted to be
well-formed just because it was once stored"), `FtsIndexManifest`,
`TokenizerConfig` (pure data; `config_hash` = sha256 over names +
memcmp-encoded args, a STABILITY CONTRACT with the fork-divergence
note recorded; `validate`; `build`; the two constructor registries
with typed `NonPositiveRemoveLong` — the Cozo original's `as usize`
cast silently wrapped a negative into a filter that removed nothing),
`FtsIndexConfig` (the dedup obligation lsh.rs records — duplicated
with `parse/sys.rs`, one concept one name when the lifecycle lands),
`TokenizerCache` (name + config-hash two-level cache; KYZO DEVIATION:
lock acquisition recovers from poisoning — sound because entries are
inserted whole, and a panicking thread elsewhere no longer cascades
into every later FTS query), and tests (`config_hash_is_stable` with
an INDEPENDENTLY DERIVED expected input — hand-built canonical bytes
chained to number.rs's golden vectors, hashed with a stock Sha256,
never the production encoder, plus a `printf | sha256sum`-checkable
zero-arg vector; cache determinism by `Arc::ptr_eq`; RemoveLong
refused at BOTH moments; unknown names lazy at construction; the
validate proof sweep) — closed)
- **L1:** preserve-and-move whole → `project/text/` as the subtree's
  config/lifecycle seam; `validate` is what `session/`'s `::fts
  create` calls; the `TokenizerCache` full-index-name keying contract
  (recorded in fts.rs and lsh.rs) lives here.
- **L2:** gold: the two-moments-of-truth doctrine; the config hash's
  independent-derivation pin (the same anchored-pin form as lsh.rs's
  signature test — a failure names its layer); the typed refusal for
  the silent-wrap defect; poisoning recovery argued from the
  invariant, not asserted. The `FtsIndexConfig`/`parse/sys.rs`
  duplication remains an open obligation — the lifecycle move is where
  it dies.

## kyzoscript.pest (383 lines; inventory: dual header with TWO
documented divergences from upstream — (1) backtracking-free separated
sequences: the upstream `(x ~ ",")* ~ x?` shape parses every item TWICE
in a non-memoizing PEG, O(2^depth) on recursive rules (a 20-deep nested
list took ~20 s, 25-deep ~52 s — "a remote DoS from query text"), every
such rule rewritten to a parse-once shape with the [SEQ]/[SEQ1]
LANGUAGE-EQUIVALENCE PROOFS in the header and each rewritten rule citing
its shape inline; (2) compound-atomic strings: `raw_string` (and both
quoted forms) are `${…}` so implicit whitespace/comments can never open
inside a token, with the story-#93 fence law — the fence is `"_"+`
NEVER `"_"*`, because a zero-length fence would make every double-quoted
string match `raw_string` first and silently switch off ALL escape
processing ("live in every KyzoScript double-quoted string literal ever
written against this grammar until the fence was widened") — then the
full grammar: script alternation, sys ops (:: relations/columns/
indices/remove/rename/access_level/triggers/constraints-as-denial-rules/
index+hnsw+fts+lsh create/drop/compact/merkle_root/running/kill/
explain/verify/fixed_rules), nested block comments, the identifier
family, the three rule forms (:= / <- / <~), the validity seat with the
story-#62 keyword block (`@spans`/`@delta_sys`/`@delta` as ATOMIC
keyword tokens with the ordering rationale — delta_sys before delta or
a prefix match splits it — and the atomicity rationale for the
boundary guard), atoms/apply forms/search_apply, the expression grammar,
options (incl. the write-time `@` note deferring restriction to
parse.rs), the string/number literal family (PUSH/POP fence,
underscore digits, hex/octal/binary), table schema and the column type
grammar, the imperative %-statement family, the FTS mini-language, and
the expression/param-list entry points — closed)
- **L1:** preserve-and-move whole → `kyzo-model/parse/grammar.pest`
  (seat exists: "the KyzoScript grammar — advertises nothing unowned").
- **L2:** gold, preserve verbatim: the equivalence proofs living IN the
  grammar file with per-rule citations (a grammar that carries its own
  correctness argument); the DoS measurements kept as the rewrite's
  justification; the #93 fence law with its blast radius stated; the
  keyword-seat block's double rationale. Finding for parse/'s census to
  settle: `describe_relation_op`, `from_clause`, and `to_clause` are
  defined but referenced by NO other rule and absent from the
  `sys_script` alternation — either they are direct pest entry points
  somewhere in parse/, or they are dead rules to Remove at migration;
  the parse-tier read decides which.

## format.rs (1107 lines) + format/tests.rs (679 lines; both read whole;
inventories: format (module doc — "parse is text-becomes-proof; this is
proof-becomes-one-true-text", every same-meaning spelling collapses to
ONE; the precedence WARNING — this grammar's table is not the textbook
one (`%` looser than `+`, `~` tighter than `^`, `->` tighter than unary
prefixes), transcribed from parse/expr.rs's PRATT_PARSER, "a grammar
precedence change must edit both tables"; std::fmt-free hand building
because several Display impls are debug-oriented dumps; the ONE hidden
AST rewrite (`OP_REGEX_*` args) reversed by `unwrap_hidden_regex_arg`;
the comments limitation STATED then solved via trivia — `TriviaMode`
Bare/WithComments where Bare "must never read a trivia field"),
format_program(+_with_comments)/format_expr, ruleset/rule/fixed-rule
writers, the AND/OR nesting law block (Disjunction bare, Conjunction
parenthesized only as a disjunct member, same-kind nesting FLATTENED —
associative, so meaning-preserving and strictly more canonical),
plain-atom/unification/search writers, validity-clause writers matching
parse's coordinate order, out-options + relation-option writers, the
precedence machinery (`infix_form`/`prefix_form`/`write_operand` with
the associativity-side equal-precedence rule; prefix chains never
parenthesize because `unary_op*` is repetition, not a climb),
`write_const` (constructor-function round-trips for non-literal kinds;
the Set honesty note), `write_float` (forces a decimal point so a
whole float cannot silently reparse as Int; NaN/±INF via `to_float`),
`write_str_literal` (astral-plane characters written literally —
`\uXXXX` cannot represent them and `ANY` permits them); tests (the two
laws — idempotence and meaning-preserving round-trip — as a property
suite: 500 expressions + 300 programs + 300 commented programs over a
seeded splitmix64 the doc counts as "a fourth independent
transcription"; the `debug_no_spans` oracle with its spans-are-
provenance-not-meaning rationale; the empirically-walked precedence
regression list; sugar-collapse; whole-float; hidden-regex; #93-aware
string escaping; the comment-attachment battery mirroring parse's own
tests FROM THE OTHER END incl. the BTreeMap-reorder misattachment trap;
`fixed_rule_trivia_round_trips` covering the one node kind the derived-
Debug oracle cannot see because FixedRuleApply's hand-written Debug
omits trivia; the generator-artifact skip with a `checked > 400` floor
— no silent cap) — closed)
- **L1:** preserve-and-move whole → `kyzo-model/format.rs` (seat
  exists: "the canonical formatter: program → one source text,
  idempotent"); tests ride along as its property suite. The
  PRATT-table coupling becomes an intra-crate neighbor
  (model/parse/expr.rs) — keep the both-tables warning at both ends.
- **L2:** gold, preserve verbatim: the one-true-text doctrine; the
  both-tables warning with its failure taxonomy (non-minimal parens =
  safe, associativity disagreement = wrong meaning); flattening
  justified by associativity; the equal-precedence-by-associativity
  operand rule; honesty notes where round-trip is impossible (Set;
  unbounded/empty Interval rendering as Debug). DEFECT (doc, fix at
  migration): `write_const`'s doc says "`Set` and `Validity`/`Bot`
  have no KyzoScript constructor at all" — it names the DELETED `Bot`
  variant (third residue; see json.rs and sparse.rs) AND contradicts
  its own body, which renders Validity through a `validity(...)` call;
  verify that constructor is a real callable op and rewrite the
  sentence to match whichever way the truth lies.

## jepsen_trials.rs (682 lines; inventory: module doc (single-node
elle/Adya serializability checking over the REAL fjall storage, driven
through `write_tx`/`commit` directly because `Db::run_script`'s retry
loop never surfaces a raw abort; SCOPE STATED PLAINLY — the distributed
rig and public-surface fault injection are out, each with a named
reason and a sequencing ruling; the four cycle classes G0/G1c/
G-single/G2; G1a/G1b UNREPRESENTABLE rather than untested — commit
consumes the transaction, and `plan_txn` never reads and writes one
register — with a direct dirty/phantom-read check anyway;
"reproducibility, precisely": the seed pins the WORKLOAD, real
scheduling owns the interleaving — the honest caveat of testing real
concurrency), `#![cfg(test)]`, the transcribed splitmix Rng, the
register workload (values are unique write-ids so every read attributes
to exactly one writer), `CommittedTxn` carrying THE #95 FIX DOC (the
old post-commit `commit_seq` increment could invert relative to the
true internally-serialized commit order — forcing the window produced
false cycles in 19 of 60 seeds vs ZERO under `system_stamp` ordering on
IDENTICAL executions; the stamp is a value captured at open, so the
race class is unrepresentable, not avoided), retry-on-conflict
`run_txn`, `run_campaign` (plans drawn single-threaded up front; 4×40
across real threads), the independent checker (stamp-ordered version
chains → ww/wr/rw edges → white/gray/black DFS cycle witness →
Adya classification), the CPU-PRESSURE campaign (stressors scaled to
`available_parallelism`, reproducing #95's original surfacing condition
on every default run), env-scalable seeds (`KYZO_JEPSEN_SEEDS`/`_BASE`),
the plain campaign, the FALSIFICATION SEAL (a hand-built write-skew G2
proving the fixed checker still bites — "0 cycles must mean the engine
is correct, never the checker is now vacuous"), and the named
regression-pin slot ("None to date") — closed)
- **L1:** preserve-and-move whole → `kyzo-trials/serializability.rs`
  (seat exists: "elle/Adya-style transaction anomaly detection"). It
  already speaks only the public Storage surface, so the crate wall
  costs nothing; the two deferred legs are recorded follow-ons that
  land in trials when replication and the #31 injector exist.
- **L2:** gold, preserve verbatim: the scope ruling form (out-of-scope
  named, reasoned, and sequenced — never silent); the
  unrepresentable-plus-checked-anyway pattern for G1a/G1b; the #95
  doc's differential proof (same checker, same data, only the ordering
  witness varied); the falsification-seal discipline as a MANDATORY
  companion to any false-positive fix; pressure-reproduction of a
  bug's original surfacing condition instead of a synthetic delay;
  seed-pins-workload-not-interleaving honesty; the regression-pin slot
  convention.

## parse/schema.rs (197 lines; inventory: dual header (typed accessors
replacing grammar-shape unwraps; `VecElementType` from the value
model), module doc ("the contract that `coerce` later applies" — what
is PROVEN: unique column names, real ColTypes, non-negative constant
list lengths), `parse_schema` (typed `DuplicateNameInCols` across keys
AND dependents), `parse_col` (type/default/`=` binding; binding
defaults to the column name), `parse_nullable_type`,
`parse_type_inner` (every kind; list length as a const-evaled
non-negative int with a help-bearing refusal; vec dims parsed with
underscore stripping; tuple recursion) — closed)
- **L1:** preserve-and-move whole → `kyzo-model/parse/schema.rs` (seat
  exists: "schema clause parsing").
- **L2:** gold: error structs DEFINED AT their one use site with
  span labels and help text (the designed-diagnostics house form);
  the proof-list module doc stating exactly what the parse
  establishes. Nothing condemned.

## parse/imperative.rs (224 lines; inventory: dual header (typed
accessors; `either::Either` replaced by the NAMED `QueryOrRelation`
sum — the original used OPPOSITE Left/Right orientations at its two
use sites), module doc ("an imperative program is a composition of
proven programs"), `parse_imperative_block`, `parse_imperative_stmt`
(break/continue with optional labels; return over
relations-or-embedded-queries; if/if_not chains; labeled loops;
%swap via `expect_n`; %debug; embedded sysops and query clauses with
`as` capture; %ignore_error) — closed)
- **L1:** preserve-and-move whole → `kyzo-model/parse/script.rs` (seat
  exists: "scripts and imperative chaining").
- **L2:** gold: composition-of-proven-programs (every embedded `{…}`
  goes through the SAME `parse_query` proof as a standalone script);
  the named-sum fix (a fork change justified by a real confusion
  hazard, not taste). Nothing condemned.

## parse/fts.rs (419 lines; inventory: dual header (the fork ledger:
integer boosters parse instead of aborting — the original's dispatch
matched the SILENT `int` rule that never appears in the tree, so
`word^22` hit `unreachable!`; the Pratt table a LazyLock; the build
depth- AND operator-bounded with FLAT And/Or construction — the
original's left-nested spine aborted the process by stack overflow
from ~15k bracket-free operators), module doc (this grammar applies to
a VALUE at runtime, not script text — same law: no query string can
panic or exhaust the stack), `FTS_OPS_CEILING` = 1024 with its full
rationale (breadth counterpart of the nesting ceiling; "comfortably
above every real query and an order of magnitude below the old failure
region — and since build_infix builds flat, the bound is a refusal of
absurd inputs, not the only thing standing between us and an abort"),
typed `FtsTooManyOps` + spanned `BadFtsNumber` (replacing a span-less
passthrough), `parse_fts_query` (nesting pre-scan; ONE operator budget
threaded through every level), `parse_fts_expr` (guards run on the
FLAT child list before recursion; NOT counts depth because it boxes,
AND/OR count only breadth), `build_infix` (flat extension with the
semantic-identity argument written in place), `build_term` (NEAR
default distance 10), `build_phrase`, the Pratt table, and tests
(basics; the integer-booster regression; the reviewer's 300 KiB abort
shape refused TYPED in linear time, all three operator spellings,
budget shared across groups; NOT chains refuse as NestingTooDeep;
ceilings refuse the absurd not the legitimate — a 101-op chain arrives
FLAT, exactly-at-ceiling parses and one-over refuses; flat
construction proven semantically invisible) — closed)
- **L1:** preserve-and-move whole → `kyzo-model/parse/search.rs` (seat
  exists: "the index-search and FTS mini-language"), together with the
  lifted pure AST (see the corrected ast.rs entry).
- **L2:** gold: the two-ceiling doctrine (depth vs breadth, each with
  its own typed refusal); flat-construction-plus-bound as
  defense-in-depth stated honestly; boundary tests at exactly-the-
  ceiling and one-over. Two small defects for the migration: the local
  `is_quoted` in `build_phrase` actually means `is_prefix` (rename —
  the misnomer invites a real misread); and NEAR's distance does
  `i as u32` after an i64 parse, silently WRAPPING distances past
  u32::MAX (`NEAR/4294967306(...)` becomes distance 10) — route it
  through the same `BadFtsNumber` refusal the parse failure gets.

## parse/expr.rs (767 lines; inventory: dual header (`expr2bytecode`
relocated to data/expr.rs — "compiling an expression is the
expression's own domain"; radix overflow typed where the original
PANICKED on `0x…` past i64; typed accessors; LazyLock), module doc
(the proofs established at construction: params resolved, ops resolved
or deliberately UnboundApply, arity satisfied, literals in range; no
literal can panic, no shape can overflow the stack), the PRATT_PARSER
table (the one format.rs's table is transcribed from — the both-tables
coupling), `InvalidExpression`, `BadIntError` (ONE error for every
radix), `is_operator` + `build_expr_bounded` (belt and suspenders
around NESTING_CEILING with the division of labor NAMED: the pre-parse
scan bounds what pest recurses over, this counter bounds what only the
Pratt builder recurses over — bracketless `----1` chains; the check
runs on the flat child list BEFORE recursion), `build_expr_infix`
(&&/||/~ parse straight to `Expr::Lazy` — "laziness is structural",
a language form, not an op), `build_term` (typed unbound-param error
with help; radix/float/string literals; list/object; the apply arm:
`cond` canonicalization with auto-default, `if` → `Cond` where "2 or 3
arguments is proven by the shape of the code itself — no counting
check whose proof an unwrap then re-asserts", named lazy forms,
UnboundApply for later resolution, arity ensured with the op's own
requirement text, `post_process_args`), total `parse_radix_int`, the
three string parsers (escape whitelist; `InvalidUtf8Error` refusing
the surrogate range; `InvalidEscapeSeqError` teaching the raw-string
alternative), and tests (radix values + overflow-not-panic incl. the
decimal symmetry case; THE DECODE-ASSERTION CORPUS — story #93's
lesson written as a test class: every earlier test asserted only
"parses", "a passing suite with a dead escape decoder underneath it",
so these assert the DECODED character; raw-string backslash verbatim;
lone-surrogate designed error, reachable for the first time after the
fence fix; unrecognized-escape refusal pinned against grammar
widening) — closed)
- **L1:** preserve-and-move whole → `kyzo-model/parse/expr.rs` (seat
  exists: "the Pratt expression parser"). The format.rs precedence
  table becomes an intra-crate neighbor; keep the both-tables warning
  live at both ends.
- **L2:** gold, preserve verbatim: the two-bound division of labor
  with each bound's blind spot named; laziness-as-structure;
  shape-proves-arity; the #93 decode-assertion doctrine (assert the
  decoded VALUE, never merely "parses" — the sharpest test-philosophy
  statement in the parse tier); errors defined at their use sites with
  teaching help text. Nothing condemned.

## parse/sys.rs (928 lines; inventory: dual header, module doc (a SysOp
is "proven at parse time from pure data"; consumers are runtime-tier,
the ops parse-tier substance), the `AccessLevel` SEAM declaration (its
`Ord` derive IS its semantics — Hidden < ReadOnly < Protected < Normal,
"a landed type-driven win to preserve as-is"), the `SysOp` enum
(Compact, MerkleRoot as "the federation content-address", Verify —
story #80, parsed identically to Explain; triggers and constraints
stored as RAW SOURCE re-parsed at fire time, the inherited convention
explicitly queued for the "parsed substance with stored provenance"
redesign; `DescribeRelation` documented as UNREACHABLE-BY-GRAMMAR,
faithfully ported, wiring it in "a deliberate language decision to make
separately" — this RESOLVES the kyzoscript.pest entry's
`describe_relation_op` question: deliberate, not dead; `from_clause`/
`to_clause` remain unaccounted), the three index-config types
(`FtsIndexConfig` here being the OTHER half of the recorded dedup
obligation with `engines/text/mod.rs`), typed spanned errors replacing
span-less refusals (`ProcessIdNotInteger`; `IndexOptionError` as one
carrier for the whole option-validation family, with option-value
spans CAPTURED for post-loop range checks and defaults falling back to
the clause span), `parse_sys` (every :: op; LSH create with weight
normalization and the (0,1) threshold; FTS create; HNSW create with
must-be-set ef/m; plain index create refusing empty columns; the
shared drop shape), `parse_tokenizer_expr`/`parse_filters_expr` —
closed)
- **L1:** preserve-and-move whole → `kyzo-model/parse/sys.rs` (seat
  exists: "the :: system-operation surface"). `AccessLevel` re-homes
  to `session/access.rs` per its own seam note; the FtsIndexConfig
  duplication dies when the lifecycle tier unifies the two.
- **L2:** gold: the span-capture-for-later-checks pattern (an
  out-of-range value labelled where the USER wrote it, defaults
  falling back honestly); the Ord-is-semantics access ladder; the
  parsed-substance redesign queued visibly on every stored-source
  convention. DEFECTS for the migration: (1) `extractor`/
  `extract_filter` persist as `Expr::to_string()` — the very Display
  form format.rs declares "neither valid nor round-trippable
  KyzoScript source"; a string-literal-bearing extractor can store
  unparseable text — store `format_expr` output (the round-trip-proven
  renderer that now exists), or land the parsed-substance redesign;
  (2) the `if({extract_filter}, {extractor})` TEXTUAL splice composes
  user expressions by string formatting — same class, dies with the
  same redesign.

## parse/fuzz_tests.rs (1401 lines; inventory: module doc ("the caller
is a fuzzer with intent, so we fuzz before the callers arrive" — three
layers: a GRAMMAR-AWARE generator over the real registries because
"plausible-but-possibly-invalid text stresses far deeper paths than
random bytes"; a byte-mutation layer whose output goes through
`from_utf8_lossy` because `&str` is the real API surface; the LAWS —
Ok or a SPANNED in-bounds error, never a panic, never an abort, and a
per-case time bound honestly scoped to TERMINATING slowness with
non-termination named as the harness timeout's job), `CASE_TIME_BOUND`,
the `PROPTEST_CASES` knob, `walk_labels` (recursive over
related/diagnostic_source), `check_laws_with` (panic/time/
label-out-of-bounds/span-less/banned-message laws — spanned-ness
enforced UNCONDITIONALLY since the fix wave retired the findings
ledger; future exceptions keyed on `Diagnostic::code()`, "never a
rendered-string substring, which silently excuses off-target errors"),
`BANNED_GENERIC_MESSAGES` + prefix with the exact-equality rationale
(parameterized retirees CANNOT regress to their fixed text, so exact
match risks no false hit), the generator pools (real aggregation and
fixed-rule registries plus one deliberate stranger each; radix/i64-edge
numbers; hostile strings; validity specs), the strategy tower (expr/
atom/rule-head/fixed-arg/rule/schema/option/query/sys/imperative/
ceiling-shapes/script + the FTS strategy), the mutation layer
(invalid-UTF-8 payloads; Truncate/Splice/DupSlice/FlipBracket/Inject
with self-resolving indices), the proptest law blocks, the regression
corpus (every FINDING-1..8 minimized input + ceiling shapes + hostile
bytes + a full bracket-flip sweep, replayed on every run), the FTS
corpus, `sql_refugee_corpus` + `sql_refugee_mistakes_get_designed_help`
(one script per keyword the hint table knows, each failure implicating
ONE keyword; refusal must carry a #[help] naming the KyzoScript idiom —
"not just a refusal, a designed refusal", additive to the general
laws), and `former_findings_now_carry_spans` (the retired ledger's
INVERSE: every former finding still errors, now with an in-bounds
label) — closed)
- **L1:** preserve-and-move whole → the parse tier's adversarial suite
  inside `kyzo-model` (beside parse/); the corpus doubles as seed
  material for `kyzo-trials/fuzz.rs`'s big-run campaigns via the
  `PROPTEST_CASES` escalation already documented in the module doc.
- **L2:** gold, preserve verbatim: laws-not-coverage; the
  unconditional spanned-ness rule with its exception discipline; the
  exact-equality banned-message design; the time-bound honesty; the
  designed-refusal law for SQL refugees (a DoD bullet turned into a
  permanent test); ledger-retirement expressed as its inverse test.
  Nothing condemned.

## parse/query.rs (1719 lines; inventory: dual header (the whole rule
map assembled BEFORE `InputProgram::new` is called exactly once — the
original built a bare struct and patched the entry in afterwards, and
its first `make_empty_const_rule` site was DEAD CODE, identified and
not ported; fixed-rule named-relation args strip the `*` their grammar
rule actually carries — the original stripped `:` and panicked on every
`rule(*rel{…})`), module doc ("every program has an entry is proven at
construction, never patched up afterwards"), the option-error family
(constancy with the CAUSE chained via `#[related]`; non-neg/pos/bool),
`MultipleRuleDefinitionError` (hand-implemented Diagnostic carrying a
label per conflicting definition), total `merge_spans`, the parse_query
loop (rule/fixed/const with head-consistency and conflict checks; every
option incl. the wasm `:sleep` refusal), the synthetic-entry synthesis
point for body-less `:create` (binding order preserved from the
original's one LIVE site), `StagedRelation` (a named sum replacing
Either), the write-`@` machinery (`RawWriteValidity` staged because a
`:put` line may parse before its entry rule; two coordinates REFUSED —
"a script that could choose system time would let a writer forge when
the database learned a fact"; `@` on `:ensure`/`:ensure_not` refused
rather than silently ignored; the constant coordinate RE-PROVEN through
`ValidityTs::for_assertion` at the one place a ValidityTs becomes a
write coordinate — the #62 terminal-tick ruling; the per-row branch
resolving the head LAZILY so headless mutations don't regress),
parse_rule/disjunction/atom (all atom kinds; `_` bindings replaced by
generated `*^*n` names that cannot collide), `parse_at_expr_clause`
(shared with fixed-rule args, which deliberately never see the temporal
alternatives), the #62 clause dispatch (@spans/@delta/@delta_sys),
`parse_rule_head` + `AggrNotFound` with `suggest_aggr` (an OWN
Levenshtein — "no crate pulled in for one small function" — offered
only within edit distance 2, and the hint list's drift honesty: "the
failure mode is a weaker hint, never a wrong refusal"),
`parse_fixed_rule` (one binding namespace across the invocation;
init_options/arity resolved against the live `Arc<dyn FixedRule>`),
`insert_empty_const_entry`, and tests (THE LANDMINE — body-less
`:create` synthesizes its entry; the write-`@` battery: Now/Fixed/
'NOW'-resolves/'END'-REFUSED with the hostile-review history of the
zero-width-interval finding recorded in the test doc itself/
literal-MAX refused identically/per-row extractor/two-coordinate
refusal/ensure refusals/unbound name) — closed)
- **L1:** preserve-and-move whole → `kyzo-model/parse/query.rs` (the
  seat's own line: "rules, options, and the proofs that bind them").
  CROSS-REFERENCE the data/program.rs BLOCKER: `parse_fixed_rule`
  resolves and CALLS the engine-side `Arc<dyn FixedRule>` at parse time
  (init_options/arity) — in the target, the model's parse can only
  bind a declaration-shaped handle (name/arity vocabulary), with the
  live impl attached at the engine boundary; this file is the other
  end of that cut.
- **L2:** gold, preserve verbatim: construct-once-prove-once; the
  dead-code-identified-not-ported discipline; the forge-prevention
  argument for engine-minted system time; refuse-not-ignore;
  re-prove-through-the-smart-constructor at conversion points; the
  did-you-mean drift honesty; test docs that carry their own hostile-
  review history ('END' once resolving, now refused, and WHY).

## parse/mod.rs (2037 lines; inventory: dual header, module doc (claimed
text becomes proven syntax; the TWO LAWS: grammar-shape trust is TYPED —
drift is a spanned GrammarShapeError naming the rule, "diagnosable, not
an abort" — and no user text can panic, hang, or overflow the stack),
`ScriptParser` (the grammar as "the other half of this tier's proofs"),
the typed-accessor layer (GrammarShapeError/UnexpectedRuleError/
`unexpected`/`GrammarChildren` with expect/expect_n/`single`/
`strip_sigil` — "this boundary is where each sigil is looked at for the
last time"/ExtractSpan/IntoChildren), the `Script` genus + imperative
AST (`QueryOrRelation` replacing Either's two OPPOSITE orientations;
`needs_write_locks` walking every statement incl. sysop index names),
`ParseError::from_pest` ("the single funnel every syntax mistake in
KyzoScript passes through, so it is the highest-leverage diagnostic in
the language"): `describe_expected` (dedup on the RENDERED phrase,
capped at five + "other constructs"), `describe_rule` (~40 hand-written
phrases with a fallback that "can never bottom out in a bare Rule::foo
debug print"), `SQL_KEYWORD_HINTS` ordered by earliest appearance +
window-first `sql_refugee_hint` + whole-word `has_word`; the NESTING
CEILING = 64 placed BY MEASUREMENT (~2.5 KiB/level release, ~11–12 KiB
debug; unguarded overflow between 768–1024 release and 160–192 debug on
a 2 MiB thread; 64 ⇒ ~0.8 MiB worst-case debug, ~7× deeper than any
real query) with the language-limit-like-i64's-range help text; the
SHARED string-skip primitives (one per quote form "so a future grammar
change has exactly one place to change instead of N scanners that can
silently disagree" — the exact drift class #93's fallout fix repaired);
`scan_comments` (an independent raw-text walk; un-silencing pest's
COMMENT was "rejected after a real check, not on suspicion" — a
two-rule experiment showed it injects a stray pair into every
`.into_inner()` consumer); `reject_excessive_nesting` (the faithful
mini-lexer: joint ceiling over brackets, %-blocks, nested comments, and
OPEN `not` prefixes tracked per bracket level and closed by the
separators that end an atom; "it over-counts, never under-counts");
parse_type/parse_expressions/parse_script (species dispatch; trivia
attached in a separate pass once final spans exist); and the test
battery (a 37-case named smoke corpus; the LSH negative-option
wrap-to-allocator-abort regression refused at parse; THE SIZED-STACK
METHODOLOGY — refusal proven by SURVIVING on a 256 KiB thread,
acceptance proven on the 2 MiB basis the ceiling is documented against;
the F1 backtracking wall guard; the refusal label pinned at exactly
ceiling+1; scattered negations proven non-accumulating; the scan
proven to ignore string/comment content WITH the #93-fallout
escaped-quote regression in both directions; raw-string contiguity;
the [SEQ]/[SEQ1] language pins — ~40 accepted shapes, trailing
separators still refused on [SEQ1], the grammar-vs-semantic
empty-index distinction; grammar-drift-errors-not-panics; an eyeball
rig; the comment-trivia battery ending in the guardrail:
comments-do-not-change-meaning checked through the formatter with a
non-vacuity assert) — closed)
- **L1:** preserve-and-move whole → `kyzo-model/parse/` as the tier's
  module root (the accessor layer, ceilings, the ParseError funnel and
  scan_comments are the tier's shared substrate; species files land in
  their named seats). Same `Arc<dyn FixedRule>` blocker cross-reference
  as parse/query.rs — `parse_script`'s signature threads the live impl.
- **L2:** gold, preserve verbatim: the two laws; the measured ceiling
  (a limit PLACED by measurement and documented with its numbers); the
  single-funnel diagnostic doctrine with the can't-bottom-out fallback;
  one-skip-primitive-per-quote-form as an anti-drift design; the
  rejected-alternative recorded WITH its experiment; the sized-stack
  proof methodology; over-count-never-under-count as the scan's stated
  bias. Nothing condemned.

## query/sort.rs (132 lines; inventory: dual header (free function — the
original took `&mut SessionTx` and never touched it; law 5 — an `:order`
naming a non-head variable PANICKED upstream, now the typed
`SorterNotInHead`; upstream sort semantics preserved DELIBERATELY:
stable sort over canonical store order makes ties deterministic),
`sort_and_collect`, two tests incl. the typed-refusal pin — closed)
- **L1:** preserve-and-move whole → `exec/sort.rs` (seat exists:
  "result ordering, limits, offsets").
- **L2:** gold: the parser-validates-but-the-refusal-covers-every-
  other-road pattern; determinism-under-ties stated with its mechanism.

## query/ra/fixed.rs (149 lines; inventory: split-out header (see
ra/mod.rs for the transformation record), `InlineFixedRA` (`unit` — no
columns, one empty row — seeds every rule body; `iter_batched` chunking
literal data; `do_eliminate_temp_vars`; `join_type` naming
null/singleton/fixed specializations; `join` with the three shapes:
empty ⇒ empty, singleton ⇒ filter-extend, many ⇒ hash-grouped
flatten) — closed)
- **L1:** preserve-and-move whole → `exec/op/literal.rs` (seat exists:
  "unit and literal-block relations").
- **L2:** everything crosses; the singleton/many join specialization
  is honest micro-structure, keep it.

## query/batch.rs (159 lines; inventory: module doc ("values-based v1
... story #120's packed-u32 relations replace these internals with code
columns over the value plane's arena — this module is the seam it swaps
behind"), `ColumnBatch`/`BatchColumn` (get CLONES; the packed form
replaces it with an admitted spend), `Selection` (sortedness is the
CALLER's construction, debug-asserted, never a hidden re-sort),
`ErrorMin` (the row-ordered minimum-error keeper — "exactly the error
row-major evaluation would raise first", lazily constructed only when
it improves), two tests — closed)
- **L1:** refactor-and-move → `exec/expr/` as the columnar evaluator's
  batch vocabulary. The DataValue-cloning internals are the declared
  #120 replacement target (the execution currency's `CodeColumn`/
  `AdmittedWords` are the successors); `ErrorMin` and `Selection`'s
  contracts survive the swap unchanged.
- **L2:** gold: row-lane error identity as a NAMED design goal (the
  columnar lane may not change which error surfaces); the
  seam-it-swaps-behind self-description — a module that knows it is
  scaffolding and says so.

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
- **L2:** gold, preserve verbatim: the seven laws with enforcement
  sites (the engine's contract stated where its parts are declared);
  per-attribute justification and the removed-once-proven notes.

## query/ra/transform.rs (266 lines; inventory: split-out header,
`ReorderRA` (ONLY ever the plan root — "never a join RHS, which
RelAlgebra::join enforces at construction"; permutation validated with
a typed PlanInvariantError), `FilteredRA` (compile + eliminate; the
batched path documented as "same predicate order, same elimination,
same row order as the iterator path"), `UnificationRA` (ONE columnar
evaluation per parent batch via vm::eval_expr_batched; the spread form
flattens in row order; typed BadSpreadUnification; operators never
yield an EMPTY batch — the all-empty-lists edge filtered) — closed)
- **L1:** preserve-and-move whole → `exec/op/transform.rs` (seat
  exists: "streaming transforms: reorder, filter, project").
- **L2:** gold: invariants enforced at CONSTRUCTION and named where
  they're relied on; batched-equals-row-path stated per operator.

## query/ra/search.rs (274 lines; inventory: split-out header,
`SearchRA` ("a vector search is a join" — one engine invocation per
parent row, hits extend the parent; the TWO FRAMES named: the query
expression sees the PARENT frame, the filter sees the FULL output
frame parent ++ own_bindings), typed `SearchQueryTypeError`,
`iter_batched` (TF-IDF's N hoisted once per plan; per-invocation and
per-node cancellation), and `SearchBatches` (the resumable batch
executor: a row whose hits overflow one output batch resumes exactly
where it left off; an error arriving after a partial batch is HELD as
`pending_err` and delivered next — no rows lost, no error swallowed) —
closed)
- **L1:** preserve-and-move whole → `exec/op/search.rs` (seat exists:
  "projection searches as relations"). Note: the doc names spatial
  among the engines but `SearchConfig` has only Hnsw/Fts/Lsh — the
  spatial wiring is the staged companion patch spatial.rs's entry
  records; the doc is one patch ahead of the enum, reconcile at the
  move.
- **L2:** gold: the two-frames discipline; the
  resumable-with-held-error executor shape (partial progress is
  delivered before its error, exactly once).

## query/ra/temp.rs (311 lines; inventory: split-out header,
`TempStoreRA` — WHERE THE SEMI-NAIVE DELTA DISCIPLINE IS IMPLEMENTED:
delta-vs-total decided by `AtomOccurrence` (this atom's position, not
its store name — a store mentioned twice compiles to two RAs with two
occurrences); the anti-join doc law "negated occurrences always read
totals — negation over a delta would resurrect rows already ruled
out"; `prefix_join_batched` (kept as its OWN implementation because
the filter-less branch joins from a borrowed `TupleInIter` without
ever minting the store row as a Tuple; `compute_bounds` hoisted out
of the row loop — pure in the left row, the row path recomputed it
redundantly), and `TempStorePrefixBatchJoin` (order matches the row
path exactly; bounded probes vs the zero-clone projected prefix
probe) — closed)
- **L1:** preserve-and-move whole → `exec/op/delta.rs` (seat exists:
  "fixpoint total/delta scans").
- **L2:** gold: occurrence-keyed deltas (the twice-mentioned-store
  correctness subtlety made structural); negation-reads-totals with
  its resurrection rationale; zero-clone preserved deliberately and
  the reason written down.

## query/batch_ops.rs (316 lines; inventory: module doc ("the CURRENCY
HANDLING every batched operator shares"), `BATCH_ROWS`, `Batch`
(row-major FLATTENED with the layout argument — row-major serves the
VM and the scan "as they exist today", columnar remains possible if a
profile justifies it; "ORDER IS LOAD-BEARING: the determinism law
rides on this — batching must never reorder observable results";
`new()` DELIBERATELY unallocated with the measured 3× regression on
recursive workloads recorded; `push_with`'s torn-row discipline —
nothing lands unless the fill fully succeeds; `pop` for filtered
decodes; `into_rows` confined to RA-internal seams with the
mint-only-on-admission note), `conjunction_pred` (rejoining the
compiler's split filters so "selection refinement IS the
short-circuit and the error minimum IS row-major error identity"),
`refine_batch`, and the two accumulate-then-refine sources
(`BatchTupleFilter`/`BatchScanFilter`, each carrying the
`pending_err` row-order law: a stream/decode error must NOT outrank
an earlier accumulated row's predicate error; the scan decodes raw
bytes straight into the flat arena — no per-row Tuple survives — and
pops torn rows) — closed)
- **L1:** refactor-and-move → `exec/op/` as the operator tier's shared
  batch substrate. Same #120 seam class as query/batch.rs: the
  DataValue arena is the declared replacement target for the packed
  execution currency; the ORDER and ERROR-IDENTITY laws survive the
  swap verbatim.
- **L2:** gold: order-is-load-bearing stated as law; the pending_err
  error-identity discipline in BOTH sources; performance decisions
  carrying their measurements (the 3× note); torn-row impossibility
  by construction.

## query/ra/neg.rs (423 lines; inventory: split-out header, `NegRight`
(the CONSTRUCTOR PROOF that only the five negatable shapes reach the
dispatch — story #86 closed the last gap and
`NegationOverTimeTravelError` "no longer exists"; the original's
unreachable! arms STAY unreachable by type), `NegJoin` (semantically a
filter; introduces no columns; "negation always reads right-side
TOTALS, never deltas"), `join_type` explain names per strategy,
`iter_batched` with the five probes (prefix vs materialized for
temp/stored; the skip-scan anti-join carrying the INHERITED-PROOF doc:
"the 'never skips a tuple whose absence it is asserting' proof is
inherited, not reargued" — it reads through the SAME primitives the
positive join proved, and a first-hit probe "is strictly less work
over the same, already-proven stream"; @spans/@delta materialize whole
through the SAME production sweep the positive read uses, with the
chunk-4 posting-index pushdown gap named), and `NegBatchFilter`
(the error-identity discipline again: accepted rows emit FIRST) —
closed)
- **L1:** preserve-and-move whole → `exec/op/neg.rs` (seat exists:
  "anti-join").
- **L2:** gold: unreachability by construction instead of assertion;
  inherited-not-reargued soundness with the strictly-less-work
  argument; the named pushdown gap (chunk 4) instead of a silent
  materialize-everything.

## query/search.rs (423 lines; inventory: module doc (claim-becomes-
proof at the catalog boundary: `SearchInput` is syntax, `SearchAtom`
holds live handles, decoded manifest, engine params, and the EXACT
output frame; ONE resolution site so every mistake is "a typed,
spanned refusal at that boundary, never a downstream surprise"; the
dataflow contract — the atom binds own_bindings and requires its
query's variables, "placed exactly like a unification"), `SearchAtom`/
`SearchConfig` + the three resolved kinds (FTS/LSH analyzers built
ONCE at resolution — "a manifest that no longer builds is a refusal
here, not mid-scan"), the typed-refusal family (incl.
`SearchOverPlainIndex` redirecting to the planner and
`NegatedSearchUnsupported` with the semantics argument — "'not near'
has no single sound meaning"), the param-taking helpers (consumption-
based: LEFTOVER params refuse as unknown-for-this-kind; leftover
bindings refuse as column-not-found), `base_frame` (user variable or
generated ignored binding per column), and `resolve_search` (HNSW's
bind-column order mirroring the engine's append contract; ef defaults
to max(k, ef); LSH appending nothing beyond the base row) — closed)
- **L1:** preserve-and-move whole → `exec/plan/` as the search-atom
  resolution (plan-time vocabulary + the catalog-proof step, reached
  through the session's handle closure exactly as today); the
  operator that runs it is exec/op/search.rs (its entry above).
- **L2:** gold: one-resolution-site; consumption-based unknown
  detection (remove-then-refuse-leftovers — no allowlist to drift);
  build-at-resolution failure hoisting; the refusal-with-a-recipe
  pattern (negation and plain-index both tell the user what to do
  instead).

## query/vm.rs (519 lines; inventory: module doc (one kernel invocation
per expression node per BATCH; "control flow is SELECTION PARTITIONING,
not jumps" — short-circuit made columnar, a deciding argument's dead
rows never touch later arguments so their errors never fire; the
DuckDB/Velox lineage credited WITH the reason; TWO LAWS:
observational identity — values, presence, and ERROR IDENTITY, the
first failing row in row order and first failing subexpression within
it, kernels never raising mid-batch but recording (row, node)
candidates in ErrorMin — and totality: every op runs through the
generic gather-apply-store kernel, "typed kernels substitute as
measured optimizations, never as semantic forks"), `SelAligned`
(positional alignment is what lets Cond stitch arms by selection order
alone), `BatchEval` (monotone node counter = evaluation order;
children claim ids BEFORE the op's own apply node, matching row-order
outranking), `eval_expr_batched`/`eval_pred_batched`, `eval_node`
(the NaN CHECKPOINT mirrored from the row lane — `result_has_nan`
refused with the SAME typed diagnostic "so no op, present or future,
can hand a poison value out of this evaluator either"; poisoned rows
push an UNOBSERVABLE placeholder; Lazy via live-set shrinking over
Decision Continue/Decided/Refused with undecided rows netting the
identity; Cond partition-and-stitch with survivors netting null), and
tests (THE differential — values and error-STRING identity against
row-by-row eval; guard short-circuit proven in BOTH directions —
poison untouched behind a false guard, reached behind a true one;
first-failing-row identity; cond stitching; 500 seeded random
expression trees with poison/Null/bool leaves over an own LCG) —
closed)
- **L1:** preserve-and-move whole → `exec/expr/eval.rs` (seat exists:
  "kernel-per-expression over code columns" — this file IS that seat's
  values-v1 form; #120 swaps the ColumnBatch internals beneath it, and
  both laws survive verbatim).
- **L2:** gold, preserve verbatim: partitioning-as-control with its
  credited lineage; error identity as a LAW with its (row, node)
  mechanism; the twin-lane NaN checkpoint (belt at both evaluators, one
  diagnostic text); measured-optimization-never-semantic-fork; the
  both-directions short-circuit proof.

## query/semiring.rs (589 lines; inventory: module doc (the
Green–Karvounarakis–Tannen model; EXACTLY the two idempotent semirings
whose fixpoints are finite ship — "counting and polynomial provenance
are refused, not approximated", the PA3 boundary, "nothing here
silently degrades into them"; the two-phase soundness argument —
post-set-semantics annotation equals the annotated fixpoint for
idempotent semirings, and first-witness recording is NOT enough for
tropical, which is why the graph enumerates ALL grounded derivations;
the negation/aggregation collapse boundary "stated, not smuggled"),
the refusal family (`SemiringOverflow` — "costs are exact or absent,
never saturated: a silently clamped cost would forge a cheapest
derivation that does not exist"; a DETERMINISTIC budget refusal;
BadCertificate; NoDerivation; the invariant error), the `Semiring`
trait (axioms asserted by randomized tests in provenance.rs, plus the
SOLVER CONTRACT beyond the axioms: idempotent ⊕ with finitely
stabilizing chains — exactly what counting violates), `Boolean` and
`Tropical` (`Cost`'s derived Ord IS the tropical order; derivation
DEPTH deliberately not offered — min-max is not a semiring ⊗),
`Derivation` (weights `NonZeroU64` BY CONSTRUCTION — a zero-weight
rule would let a min-cost cycle tie with itself and unfound
extraction), `DerivationGraph` (hand-written Default with its reason;
`check_closed` turning a silently-zeroing gap into a loud refusal),
`SolverBudget` ("there is no unbounded fixpoint in KyzoDB"), `solve`
(deterministic by construction: list order + BTreeMap, "no iteration
order depends on a hash or a thread schedule"),
`extract_min_cost_proof` (well-founded by strict u64 descent — "no
cycle can be packaged into a certificate"; lowest-index determinism;
solver/graph disagreement classified as corruption, not user error),
and `verify_proof` (the STRUCTURAL half — citation + arithmetic; the
SEMANTIC half re-derived from scratch by the independent checker
"which imports no evaluator or solver symbol") — closed)
- **L1:** preserve-and-move whole → `exec/provenance/semiring.rs` (the
  seat's own line: "annotation algebra: the idempotent pair +
  certificates"). The counted tier's future home
  (`exec/provenance/counted.rs`) is the PA3 boundary's named
  destination — a NEW fixpoint, never a widening of this one.
- **L2:** gold, preserve verbatim: refused-not-approximated; the
  two-phase soundness argument with the first-witness insufficiency;
  exact-or-refused cost law; nonzero weights as the well-foundedness
  mechanism; the structural/semantic verification split with the
  no-shared-symbols independence claim.

## query/graph.rs (612 lines; inventory: dual header (Tarjan and the
reachability walk on EXPLICIT work stacks — the original recursed once
per edge and "a rule chain a few thousand deep overflowed the thread
stack"; `generalized_kahn`'s in-degree bookkeeping checked in EVERY
build with a typed invariant — the original's `debug_assert_eq!`
compiled out of release builds, so a cyclic or corrupted condensation
"would silently yield a truncated stratification — wrong answers, not
a refusal"; indices validated up front; the Poison cancellation seam
returns with the runtime tier), the Graph/StratifiedGraph vocabulary,
`strongly_connected_components` (edges to undefined names ignored by
design — the stratifier's graphs mention unresolved rules),
`reachable_components`, `generalized_kahn` (poisoned edges must cross
stratum boundaries; checked_sub underflow guard; the exit invariant
"every edge consumed, or some node was never emitted"), `TarjanScc`
(the frame-stack rewrite documented AGAINST the recursion it
replaces — open/close mapped to call/return, low-link propagation at
frame close), and tests (known graphs; SCC vs a naive transitive-
closure oracle by proptest; the OUTPUT-IDENTICAL proptest against the
kept-verbatim recursive ORIGINAL — same components, same order, same
member order, not merely the same partition; the Kahn stratification
property over random poisoned DAGs; the poisoned-split pin; the
cyclic-input refusal — the exact case the old debug_assert waved
through; out-of-range refusals; the 50k-chain small-stack thread
proof) — closed)
- **L1:** preserve-and-move whole → `exec/plan/graph.rs` (seat exists:
  "rule-dependency analysis (SCC, levels)").
- **L2:** gold, preserve verbatim: debug-assert-to-typed-invariant as
  a WRONG-ANSWER fix, not a hardening nicety; the output-identical
  oracle pattern (keep the replaced implementation as the judge of its
  replacement); the small-stack proof; up-front validation making all
  later indexing "proven in-range once, here".

## query/gauntlet.rs (688 lines; inventory: module doc (a
SQLancer-class metamorphic gauntlet adapted from TLP/NoREC/PQS with the
TERNARY→BINARY argument — KyzoScript has no NULL-as-unknown, so the
bound/unbound adornment sweep is "this oracle's one-leg-shorter
analog"; Oracle #1 = the magic-sets NoREC analog: the same script with
the rewrite on and off plus the sealed naive oracle form a TRIANGLE,
"a divergence anywhere in this triangle is an engine bug, not a
gauntlet bug"; PLUS the fully-free identity theorem checked on the
compiled plan directly — "the symbol-count anomaly that would have
caught issue #68 with no answer divergence needed"; what it
deliberately does NOT render, each exclusion reasoned; reuse-not-
recopy with the shared-file-contention rationale for the transcribed
RNG), the KyzoScript renderer (`is_idb` as "the real semantic test,
not 'is it in facts'"), `compiled_magic_symbols` (reimplemented
against pub(crate) seams — "zero edits to db.rs"), the generator (the
#68 points-to self-join shape deliberately included; optional
negation-over-recursion reader), `adornment_patterns` (bound values
pulled from REAL oracle facts so patterns are non-vacuous; bound-both
left out with the `?[]`-syntax risk named), `run_one_seed` (the
triangle + the theorem per adornment), env-scalable seeds
(`KYZO_GAUNTLET_SEEDS`/`_BASE`), the regression-pin slot ("None to
date"), generator seed-reproducibility, THE FALSIFICATION CLAUSE
(issue #29 clause 1: the checker proven to catch a deliberately
corrupted expectation, with a non-vacuity assert on the fixture), and
the REFUSAL FENCE (laws' unstratifiable corpus rendered WHOLESALE;
external EDB relations `:create`d empty so an unknown-relation error
cannot MASK the stratification refusal; every head tried — "the
robust form of 'stays refused', not a single guessed entry point";
the oracle-accepts-now guard routing corpus drift upstream to
laws.rs; the fixed-rule entry skipped for the same named boundary) —
closed)
- **L1:** preserve-and-move whole → `kyzo-trials/gauntlet.rs` (seat
  exists: "metamorphic logic-bug hunting over generated programs").
  Its `laws::Program` renderer is REUSED by #80's whole-corpus verify
  proof (per query/mod.rs's ledger) — the move keeps that consumer
  pointed at one renderer.
- **L2:** gold, preserve verbatim: the triangle; the theorem-as-
  symbol-anomaly check (catching a class of bug answers can't see);
  the ternary→binary adaptation argument; exclusions with reasons;
  the falsification clause; refusal-fence design (unmasking, all
  heads, upstream routing). Nothing condemned.

## query/ra/join.rs (697 lines; inventory: split-out header, the shared
plumbing (flatten_err/filter_iter/eliminate helpers; `push_joined_row`
— the joined row "never materialized as its own Tuple", with the
story-#77 note: right sides yield OWNED values because a byte-backed
store's decode produces a value, not a borrow), `PrefixProbeBatchJoin`
(ONE executor shared by Stored and StoredWithValidity, their
difference captured at construction in `probe`; an in-flight match
iterator held across output-batch boundaries so "an output-batch
boundary never re-scans anything"; a defensive empty-batch skip kept
"even if that invariant is ever loosened"), `join_is_prefix` (the
partial-index-match judgment written down: `[a, u => c]` with a and c
bound is NOT prefix "as it is not clear that prefix scanning in that
case really saves computation"), `Joiner` (the lockstep length
invariant attributed to its maintainer; typed `join_indices` replacing
the original's double unwrap), and `InnerJoin` (strategy chosen at
iteration time from the right side's SHAPE; the Reorder/NegJoin right
arm a typed error where the original panicked — refused at
construction; `iter_batched`'s UNIT-LEFT delegation with its
rationale — it is what lets a scan→filter→project chain run fully
batched; three native prefix dispatches;
`materialized_join_batched` — the right side materialized ONCE into a
sorted deduplicated run keyed join-columns-first, binary-searched per
left row with NO probe tuple ever built, replacing the row machine's
hash match with the note "Datalog answers are sets, so the dedup is
observationally identical"), and `MaterializedBatchJoin` (usize::MAX
run sentinel; an in-flight run resumes across output batches without
re-searching) — closed)
- **L1:** preserve-and-move whole → `exec/op/join.rs` (seat exists:
  "column joins and storage-probe joins").
- **L2:** gold: one-executor-two-configurations (the probe closure as
  the variation point); resumability as a stated property at every
  batch boundary; strategy-from-shape with panic-to-typed-refusal;
  observational-identity arguments accompanying every optimization.
  Nothing condemned.

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
- **L2:** gold: parts that know their own destinations; the
  catalog-boundary ruling for search atoms; brand-with-manifest-arity;
  the global-admission budget arming; superseded code deleted and its
  deletion recorded. Nothing condemned.

## query/ra/stored.rs (770 lines; inventory: split-out header,
`StoredRA` (keyspace-kind dispatch: Facts resolves bitemporally and
flattens into batches, AlgorithmState keeps the ZERO-MINT raw byte
path — extending it to as-of needs "a raw seek scan on the storage
contract, the columnar leg's next contract change"; `segment_at`
witnessed/built ONCE per plan-node instantiation with both decline
paths documented), `prefix_join_batched` (point lookups served from
the segment's binary search or the zero-clone projected
current-row get; the BOUNDED prefix case deliberately left on the
storage scan — "converting it now would spend risk on a path this
pass has no evidence for", the motivating workload named; the
once-per-instantiation witness so "the hot probe loop below never
touches the watermark again"), `StoredWithValidityRA` (as-of scans —
"bitemporality is the format, not a schema opt-in"; deliberately no
point-lookup sub-case, matching the row path), `SegmentScanBatches`
(observationally identical to the storage scan), and the #82
SEGMENT-GATE BATTERY exercised THROUGH THE PRODUCTION `iter_batched`
path: write-interleaved reads never build (with the post-read-witness
check rationale — a pre-read check "would pass vacuously");
the stable run builds at exactly the documented second miss; an
intervening write resets the streak end-to-end; and the seeded
mixed-workload differential comparing segments-on, segments-off, AND
an independently maintained model — "never a run of the same machine
checked against itself" — closed)
- **L1:** preserve-and-move whole → `exec/op/stored.rs` (seat exists:
  "canonical scans, current and time-travel"); the segment-gate
  battery travels to `project/current.rs`'s regression suite per the
  segments.rs entry (it proves the gate at its consumer).
- **L2:** gold: evidence-gated conversion (cold paths left alone with
  the reason written); once-per-instantiation synchronization
  discipline; the three-way differential with an independent model;
  the vacuous-check analysis inside a test comment (why the assertion
  order proves what it claims). Nothing condemned.

## query/time_travel_script_laws.rs (835 lines; inventory: header (the
LANGUAGE-surface as-of laws, story #4 — the named MISSING LAYER: trials
prove the pipeline through hand-built seams, db.rs proves the clause
parses, "neither differences a real, multi-entity, multi-transaction
as-of history against an oracle through parsed KyzoScript text"; THE
GAP THIS MODULE ONCE DOCUMENTED IS FIXED — write-side `@` landed, one
coordinate only, the system coordinate "stays engine-minted ... with
no script syntax able to touch it"; the whole history now built
through PURE KYZOSCRIPT with no internal-API backdoor, one script =
one committed transaction so same-instant collisions resolve in event
order; the HOSTILE-REVIEW FINDING FIXED doc — the old
current-belief-based supersession broke under historical `@`, three
failure modes named and pinned), the seeded history generator
(same-instant collisions by design; redundant retracts as no-op
probes), `oracle_at` ROUTED THROUGH THE UNIFIED TEMPORAL ORACLE
(story #62's `laws::resolve_relation`, write order riding the system
axis — "last write in write order governs" IS "newest system version
governs"; the sabotaged exclusive boundary is "still the one real
resolution function, just a deliberately wrong coordinate"), the
BRIDGE DIFFERENTIAL (oracle_at vs a from-scratch independent
reference over 300 seeds × every probe instant × both boundaries,
with a >500-case floor), `probe_instants`
(before-all/at-every/between-gaps/after-all), the MAIN LAW with
anti-vacuity gates (≥40 events, ≥40 script transactions, at least
half the probes nonempty, ≥10 DISTINCT answers "else this harness
could pass by every probe returning the same thing"), the
boundary-MUTATION catch (the sabotaged oracle must DISAGREE with the
engine — "else the differential is blind to the boundary"), the
two-coordinate flagship (system stamps read off `clock_floor`, "a
public, engine-owned watermark ... not a fact-write backdoor"), the
per-row `@` RUNTIME proof (the ts column never stored), and the three
hostile-review pins (a historical correction stays consistent with
its INDEX at the corrected instant; `:update` carries forward the
TARGETED instant's value, never a future one; `:insert` checks
existence at ITS OWN instant, succeeding over an unrelated current
row and refusing a genuine same-instant duplicate) — closed)
- **L1:** preserve-and-move whole → `kyzo-trials/time_travel.rs` (seat
  exists: "the temporal law and trial batteries") as its
  language-surface half; it already runs everything through
  `Db::run_script`, so the crate wall costs nothing.
- **L2:** gold, preserve verbatim: the missing-layer argument (naming
  exactly what each sibling test does NOT cover); anti-vacuity gates
  on every axis a differential could silently hollow out; the
  mutation-catch on the boundary; one-real-oracle-wrong-coordinate
  sabotage design; the no-backdoor discipline extended to reading
  system stamps; hostile-review pins that state the pre-fix failure in
  the test doc. Nothing condemned.

## query/levels.rs (863 lines; inventory: the level-tier doctrine (a
rule's TOTAL is a stack of immutable sorted levels sealed per epoch
barrier — dense walks and binary searches, "the shape the semi-naive
inner loop wants, instead of pointer-chasing a tree that is
rebalancing under an insert-heavy fixpoint"; THE DELTA IS THE NEWEST
LEVEL; newest-wins shadowing; meet folds AT the barrier so "a group's
value is always whole in one level, never split across levels"),
`NormalLevel` (story #77: rows are MEMCMP BYTES in a flat arena —
the order-embedding law makes byte compares IDENTICAL to the
DataValue compares they replace, "a probe value is encoded once per
call, not once per row visited"; (skip, refresh) flags where refresh
rows shadow a flag change and are "admitted nowhere, invisible to
delta iteration"), `MeetLevel`/`MeetSpec` (`would_admit` — ONE
admission oracle shared by the mid-epoch spend guard and the
barrier)/`MeetTotalView`, `EpochStore` with THE SEMI-NAIVE INVARIANT
stated on the type ("after every merge_in ... the newest level's
non-refresh rows are exactly the tuples admitted this epoch"),
`merge_in` (the barrier: drop the consumed empty delta — else "a
converging fixpoint would stack one empty level per epoch" — then
compact the PRE-epoch stack ONLY, because "the level just sealed IS
the delta and must survive whole until the next barrier"),
`has_delta`, the iterator family (ranged/prefix/projected zero-clone
probes; the 0xFF-tail prefix bound), `normal_merge_next` (k-way
newest-wins where among equals "the LATEST cursor — newest level —
speaks"), the compaction pair (the logarithmic half-size schedule "a
pure function of sizes — deterministic on every run"; a surviving
refresh row "stops being refresh-marked once its shadowed victim is
gone"), `meet_ranged` (suffix layout walks groups directly;
interleaved walks row mirrors skipping newer-owned groups), and the
bounded-stack test (10 productive + 50 converged epochs, ≤6 levels,
totals intact) — closed)
- **L1:** refactor-and-move → `exec/fixpoint/delta_store.rs` (the
  seat's line "working memory keyed on packed-code identity"): this
  file is the values-v1 INCUMBENT and data/value/exec.rs's `ExecDedup`
  is the packed-code SEED — #120 merges them at this seat, keeping the
  level/shadow/compaction discipline and swapping the row
  representation beneath it.
- **L2:** gold, preserve verbatim: the delta-is-the-newest-level
  identity; the invariant stated on the type; refresh-row semantics
  (a flag change that must not be a delta); compact-pre-epoch-only
  with its reason; deterministic compaction as a pure function of
  sizes; one admission oracle for guard and barrier. Nothing
  condemned.

## query/stratify.rs (1079 lines; inventory: dual header (typestate
output — `StratifiedNormalFormProgram::from_reverse_execution_order`
reverses ONCE and proves the entry sits last, where the original
returned raw reversed strata "un-reversed by convention"; the entry
reached through its FIELD, never a re-spelled `?` with a dummy span;
the POISON-SPAN fix — the refusal labels the atom that ESTABLISHES the
poison, because the dependency map keys by first occurrence and "the
first-read symbol's span would mislabel a later negation"; Kahn sized
by SCC count; one classification helper), module doc (THE REFUSAL IS
THE FEATURE: "a missed refusal here does not crash, it silently
yields wrong answers"; the poison taxonomy; the one legal
aggregation-in-recursion — an all-meet head reading ITSELF,
positively; "possession of that type IS the proof"),
`aggregation_character` carrying the DELIBERATE-INDEPENDENCE ruling
(issue #89: the engine's classification and the oracle's
`head_classes` are "two separately hand-maintained implementations" —
sharing them "would collapse that differential into a tautology";
"keep every future edit here hand-applied, never routed through the
oracle's copy"), the graph construction "decision for decision" with
poison-span capture on both insertion and upgrade, `verify_no_cycle`,
the condensation (self-edges vanish, already proven unpoisoned),
`into_stratified_program` (nine numbered steps; the two index spaces
built by DIRECT ENUMERATION — "no n_strata − 1 − i arithmetic
anywhere"; the stratum-ordering debug_assert at the one place every
dependency edge passes; the entry-lands-last argument written as a
PROOF in prose), and the test battery (the upstream test that
"asserted nothing" ported WITH assertions; the oracle refusal corpus
through the REAL stratifier — "the two must never drift"; the
meet-exemption boundary from every side incl. the
intermediary-recursion refusal preserved deliberately;
unreachable-rules pruned-not-checked as deliberate upstream behavior
— "the soundness proof is about what will be evaluated"; the
ESTABLISHING-ATOM span pin distinguishing the innocent positive read
from the poisoning `not p`; the 10k-rule chain on a 256 KiB stack) —
closed)
- **L1:** preserve-and-move whole → `exec/plan/stratify.rs` (seat
  exists: "the stratification proof: negation and aggregation are
  safe").
- **L2:** gold, preserve verbatim: refusal-is-the-feature; the
  deliberate-independence ruling (an anti-consolidation law with its
  tautology argument); establishing-atom diagnostics; direct
  enumeration over index arithmetic; proofs in prose at the mint
  sites. Nothing condemned.

## query/standing.rs (1085 lines; inventory: MPL header, module doc (the
snapshot-consistency PROOF: `current_callback_targets` read exactly once
per transaction means subscribe-FIRST-read-SECOND loses no commit — a
commit either predates registration (in the initial read; its eventual
event is redundant, absorbed by `incremental_eval`'s EDB-patch filter) or
postdates it (missed by the read; the event supplies it); the pull drive
model ("there is no thread-management infrastructure anywhere in the
callback seam today" — a Receiver, the caller decides the loop); the
`CallbackEvent`→`SignedFact` law: Put's new/old are two INDEPENDENT
non-disjoint row sets, so the SET DIFFERENCE is taken before folding —
a row on both sides nets to no change; Rm is NOT symmetric, its "new" is
bare k_bindings keys, only "old" is a fact), `entry_symbol` (the `?`
identity minted in exactly one place), `Subscription` (id/receiver/
key_arity cached at registration to avoid a second storage round-trip),
`no_duplicate_key_prefix` (debug-only invariant; Tuple's Ord puts key
columns first so duplicate key prefixes are ADJACENT — an O(n) scan, no
key index), `StandingQuery` (no bare-fields constructor: existing =
subscribed + snapshot-initialized), `register` (subscribe-then-snapshot
in the proof's order; the bootstrap is NOT a special case — empty IDB
state + all-Plus EDB seed patch through the same `incremental_eval`
walk), `current`/`current_answer` (static EMPTY), `apply_pending` (nets
each tuple's signed multiplicity IN COMMIT ORDER before evaluating — a
flat `BTreeSet<SignedFact>` cannot represent assert-then-retract across
queued events, and the pre-batch redundancy filter cannot recover it:
the 0.9.0-adversarial-review multi-commit-drain bug, where two puts of
one key left BOTH values in maintained state), `apply_pending_answer`,
`teardown` (leak-free even unclosed — the registry's lossy-by-disconnect
contract prunes idle channels), `Db::register_standing` (the public
entry: runs `compile_and_eval`'s exact prefix — parse/normalize/
stratify/magic — then STOPS before RA lowering, the erasure the
translator cannot afford; refuses sys/imperative scripts and mutations),
and the test battery: hard corner `p(x), not r(x)` through real commits;
Symbol-free accessor agreement; the three multi-commit-drain repros
(put-then-rm nets to nothing, rm-then-put stays present, two puts of one
key never leave two rows); teardown unregisters all; the recursion
refusal reaching `has_any_cycle` end-to-end through the public surface;
the real aggregating query hitting every min() hard case (current-min
retraction is a rescan, not a tally); and TWO generative real-commit
differentials whose recompute side is `db.run_script` of the SAME query
text — never a second `translate()` call, "recomputing via the real
query engine is what actually proves the maintained answer equals the
query's own answer" — one per-commit over four shapes, one BATCHED 1–4
commits per drain against a genuine key-value relation (the batch path
the per-commit campaigns structurally could not exercise) — closed)
- **L1:** preserve-and-move whole → `react/standing.rs` (seat exists:
  the react zone is the standing-query lifecycle's home; query/mod.rs's
  own ledger already names it). Its imports move with their owners:
  `incremental` (react/incremental.rs), `SignedFact` (the temporal
  vocabulary), the callback seam (session-side commit notification).
- **L2:** gold, preserve verbatim: the snapshot-consistency proof
  written at the module head (subscribe-first-read-second, with the
  redundant-case absorption argument); the netting law and its bug
  history (the module carries WHY a flat signed set is insufficient, in
  prose, at the code that fixes it); the recompute-through-the-real-
  engine differential discipline ("two runs of the SAME translation
  agreeing" named as the tautology it is); the no-bare-constructor
  existence invariant; the debug-only adjacency scan justified by
  Tuple's Ord. Nothing condemned. Design note for the destination: the
  pull model is a deliberate, argued scope cut — the react zone's law
  should inherit the argument, not silently grow a thread.

## query/dst_query.rs (1272 lines; inventory: MPL header, module doc (DST
"up the query path": the storage-seam DST proves the KV contract
seed-reproducible; this tier runs COMPILED Datalog over `SimStorage`
under a seeded fault plan and pins five query-visible laws — read faults
never lie, crash consistency is query-visible, snapshot isolation holds
at the answer level, time travel never tears, and determinism
"characterized honestly"; plus the rebuilt-helpers rationale: the compile
tier's builder plumbing is private, so rather than widen an engine
module's surface for a test this module rebuilds the thin layer over the
same pub(crate) pipeline entries, with HAND-CHECKED constants as the
oracle "wholly independent of the pipeline"), `#![cfg(test)]`, builder
plumbing (sp/sym/v/muggle/entry_symbol, `generous_budget` — "generous,
but armed": epoch ceiling + derived-tuple ceiling turn divergence into
typed LimitExceeded, col, rule/neg_rule/rel/rel_asof atom constructors,
HeadAggr, plain_rule/aggr_rule, program_of, immortal_lifetimes, rows),
the FALLIBLE TWINS `stored_relation`/`try_run` (every storage touch is
`?` so an injected fault arrives as a value, "not a panic, and not a
silently-wrong answer"; the no-fixed-rules closure returns Err, never
panics), `Fixture` + five corpora (transitive closure over a cycle,
two-hop join, grouped min, stratified negation, and the multi-head
stratum built FOR the parallelism probe; SINGLE_HEAD_FIXTURES named by
the property that eval's par_iter dispatches one element so storage ops
are not raced), harness helpers (`populate_retrying` — setup faults
absorbed by bounded retry so the QUERY's single raw attempt is the
observation, retry count a pure function of the seed; `seeds` with the
env-scalable KYZO_DST_QUERY_SEEDS knob; `Observed`; `observe_faulted`),
and the six capability sections: the headline read-fault campaign
(correct-or-typed-never-wrong, with the 3% recalibration note for the
one-machine executor's denser read pattern and BOTH-arms anti-vacuity
asserts); crash consistency (buffer-tier vs durable-tier edges read
through the closure after sim_crash/sim_powercut; plus the
under-faults variant where the ONLY legal answers are base-prefix or
full closure — anything else is a torn history read); snapshot
isolation via the a+b=C two-row detector under a real OS-thread writer
(rayon workers are not token-barrier participants, so genuine
concurrency is used and the doc says why); time travel as-of under
faults with clean-store anchors at 15/25/35; determinism — pinned for
single-head fixtures, and for multi-head MEASURED NOT ASSERTED (the
op-counter fault plan races rayon's head dispatch; the test records
the divergence rate, demands both Ok and Err arms so "0 diverged" is
never vacuous, and still asserts completed answers are correct); and
three anti-vacuity proofs (no faults ⇒ no errors; high rate ⇒ errors
actually fire; a corrupted reference is caught) — closed)
- **L1:** preserve-and-move → `kyzo-trials/src/dst.rs` (seat exists:
  "deterministic simulation: storage seam and query path"). REWIRE
  REQUIRED at the crate wall: the module today drives pub(crate)
  pipeline entries (`stratified_magic_compile`, `bind_for_eval`,
  `stratified_evaluate`) and hand-built `MagicProgram`s, but
  kyzo-trials "depends on kyzo-core's public surface" — on migration
  the corpora become real KyzoScript through the public Db (the same
  move standing.rs's differential already made), or the map must grant
  trials a deeper seam; the hand-checked constant oracles survive
  either way. `SimStorage` itself is kyzo-crashfs vocabulary in the
  target, which trials lawfully depends on.
- **L2:** gold, preserve verbatim: the fallible-twin discipline
  (faults as values end-to-end); the anti-vacuity instrument as a
  first-class capability (both arms must fire, the corrupt-reference
  self-check, the no-faults control); the measured-not-asserted
  determinism posture with its named hazard (rayon vs the op-counter
  fault plan) and the fix shape on record (an order-independent fault
  plan); the calibration notes that tie fault density to executor
  shape so the law stays tested; setup-retry-vs-observation
  separation. Condemned: nothing — but the builder plumbing is the
  third rebuild of the same rig (compile.rs tests, bench_api, here);
  the target's public-surface rewire dissolves it into KyzoScript
  text, which is the real fix for the triplication.

## query/ra/temporal.rs (1339 lines; inventory: MPL header, module doc in
three ruled sections (the SCAN-DIRECTION MISMATCH: the kernel's
governing-version resolution is a skip scan that never enumerates a
fact's full history, `SpansRA` needs every breakpoint, no storage
primitive yields "every version of one key" today, and the natural home
— runtime/relation.rs — was frozen under another builder's fix, so the
module builds the raw scan against the public contract with
`relation_keyspace_bounds` as "a small, named duplication, not a design
choice"; the BUFFERING DECISION: one fact key's version set at a time,
O(one fact's write history) never O(relation), the rejected alternative
named; the SIGNED-FACT CURRENCY of story #77 — a currency change ONLY,
not an algorithm change; and the chunk-4 POSTING-INDEX FAST PATH with
its three-step derivation and the identical-output-by-construction
claim), `SignedFact` (pub — "the one name for a signed delta fact in
this engine", Ord matched to the oracle twin so BTreeSets sort
identically) + `tuple`, `compose` (tally-and-cancel, INDEPENDENTLY
WRITTEN twin of laws::compose "so the two never share a bug through
shared code"; first real production caller is the fast path), `SpansRA`
(trailing interval binding never folded into base columns — the
SearchAtom shape), `DeltaRA` (+ the `posting: Option<RelationHandle>`
seam chunk 3 reserved and chunk 4 filled without changing bindings or
coordinates), SIGN_PLUS/SIGN_MINUS, `RawVersion` (pub(crate) for
story #80's verify oracle-feed — "reused, not re-derived, exactly as
this module's own doc argues for itself"), `decode_raw_version` (FOUR
corruption refusals: short key, missing time slots, a retract flag in a
stored slot, key-arity mismatch — refused rather than trusted),
`resolve_at` (Assert holds / Retract settles absent / Erase transparent
falls through; twin of laws::resolve_events), `derive_group` (maximal-
run sweep where "coalescing is definitional — un-coalesced output is
unrepresentable"; closed normal form on the discrete grid; an in-force
fact gets `Bound::Unbounded`, NEVER a finite sentinel end; the
start<end argument written at the site, law 5), `SpansRA::iter_batched`
+ `SpansScanBatches` (+`collect_group` with the pending-key carryover
so each row decodes once), `DeltaRA::iter_batched` (canonical sorted
output — the determinism law — over two routes), `patch_naive` (naive
BY RULING, two full snapshots), `patch_via_posting` (bounded posting
scan → candidate keys → point reads at both endpoints → ONE `compose`),
`posting_window_bounds` (the load-bearing key-encoding reasoning:
Validity encodes newest-first so hi is the ascending LOWER bound —
"backwards from what plain integer bounds would suggest, which is
exactly why this is factored out and named"),
`candidate_keys_from_posting` (lo>=hi ⇒ empty window; keys-only scan;
corrupt posting key refused), `RowChunks`, and tests: helpers incl.
`erase_at` (raw Erase rows — no production write path exposes Erase, so
test plumbing writes through the same pub(crate) encoders) and
`spans_rows` (open ends asserted as None, never the old MAX sentinel);
nine @spans tests (single assert open, retract clips exclusive, payload
change splits, idempotent double-assert, reopen after retract, dangling
retract holds nowhere, Erase transparency, no zero-width at any fixed
sys, multi-fact independence); four delta tests (Plus, Minus/Plus pair
"never a modified kind", identical snapshots empty, sys-axis
correction); and the posting battery — `make_indexed_relation` +
`write_indexed_event` driving SessionTx's real `update_indices` seam
(because put_fact never maintains indices — the comment names the trap),
`assert_paths_agree`, five agreement cases incl. the backward diff
(min/max not positional role), and the seeded generative campaign —
closed)
- **L1:** preserve-and-move whole → `exec/op/temporal.rs` (seat exists:
  "interval derivation and net-diff operators"). `SignedFact` moves
  with it and remains the engine-wide delta vocabulary consumed by
  react/ (standing, incremental) — one name, per its own doc.
  UNFREEZE ON ARRIVAL: `relation_keyspace_bounds` and the raw
  multi-version scan exist only because runtime/relation.rs was frozen;
  the target store zone (contract.rs + skip_walk.rs, "the ONE
  bitemporal skip-scan walk, generic over its driver") should grow the
  every-version-of-one-key primitive and the keyspace-bounds accessor,
  and this module's named duplication dies. The posting-path test
  writers similarly exist because put_fact bypasses index maintenance;
  the target's session/admit is the one write door, which dissolves
  that plumbing.
- **L2:** gold, preserve verbatim: independently-written-twin
  discipline (compose, resolve_at) with the differential in the trials
  crate as the bridge; the buffering decision recorded WITH its
  rejected alternative; posting_window_bounds' factored-and-named
  inversion reasoning; the corruption-refused decode posture; closed
  normal form with Unbounded-not-sentinel; canonical sorted operator
  output; naive-by-ruling with the acceleration landing at the exact
  reserved seam. Nothing condemned — the two duplications are both
  NAMED, argued, and scheduled to die with their causes.

## query/temp_store.rs (1518 lines; inventory: dual fork header naming
nine modifications (meets resolved ONCE at construction into live
`MeetAggrObj`s vs the original's per-row Option unwrap; normal-only-to-
meet-store as constructor error not downstream panic; `merge_in` as the
ratified admission seam; kind-mismatch as typed internal error; meet
range scans through DataValue's total order not partial_cmp-unwrap;
itertools::Either; no-arg meet forms; POSITIONAL grouping via a
constructed `MeetLayout` proof retiring upstream's suffix-only
`MeetNotSuffix` refusal, with two views because non-suffix group-key and
head-tuple orders differ; the corrected changed-flag contract whose
inverted original could announce "unchanged" on exactly the update that
changed a value and reach premature fixpoint; original had NO tests —
all new), module doc (the total/delta discipline IS semi-naive
evaluation, the equivalence argument written out, "empty deltas
everywhere are the termination certificate"; the three stores; the
ADMISSION SEAM: admission happens only inside merge_in at the epoch
barrier in canonical key order, so the sequence is
schedule-independent — the deterministic point where provenance
first-witness recording and budget accounting both attach; "only the
seam lives here"), `Admitted` (deterministic function of the sets
merged), `AdmissionSink` (RECORDING as compile-time const —
provenance-off is zero-cost by monomorphization, not a runtime branch;
meet admissions carry the group's POST-update value, matching the
per-group witness boundary) + the `()` off-state, `RegularTempStore`
(story #77 chunk 2: keyed by encode_tuple_bare memcmp bytes, one
Box<[u8]> per row instead of the measured ~415 B/row Vec<DataValue>
tax; the order-embedding law makes the swap representation-only, "the
adversarial check" being that every determinism test kept its
assertions unchanged) + exists/len (len IS the admission count on the
plain path — contrasted explicitly with the meet store's)/put/
put_with_skip + the edition-2024 use<'s> capture note,
`empty_tuple_ref`, `MeetLayout` (the constructed positional proof:
key/val positions partition 0..arity; `is_suffix` — a suffix store
skips the by_row mirror entirely, keeping the pre-fork footprint;
`borrow_key` always-an-encode with the zero-alloc-borrow-traded-for-
smaller-resident-key reasoning; `interleave` the exact inverse),
`MeetAggrStore` (by_group byte-keyed / VALUES stay DataValue-typed
with the full argument — byte-backing wins on comparison not
computation, set/bit/tropical folds need decode regardless, "a
marginal win traded for less code, not a wall"; by_row materialized
ONLY non-suffix as a pure mirror; the changed flag named load-bearing
for termination AND completeness with both failure directions) + len
(resident size, NOT admission count — "the refuted theorem")/
`meet_put_admission_faithful` (the mid-epoch spend guard's exact
count: monotone meet ⇒ admissibility flips false→true at most once ⇒
the sum equals merge_in's admitted count BY CONSTRUCTION)/new/
`meet_put` (slice consumer, F2b no-change puts never materialize the
clone), `TempStore`, `TupleInIter` (three representations — Bytes/
MeetSuffix/Values; the #77 consequence that accessors return OWNED
DataValues, "checked against every non-test consumer in the tree")
+ get/should_skip/into_tuple/cmp_bare/cmp_slice + `bare_nth` +
iterator machinery + Eq/Ord/slice-comparison impls, and the 16-test
battery: total/delta discipline (first-epoch swap, termination
certificate, exact-new-tuple delta), empty-epoch fixpoint, canonical
admission order on both paths, the changed-flag REGRESSION with the
old flag's failure COMPUTED in the comment (false|true=true but
old==*l says unchanged ⇒ silent missing answers) plus its benign
direction, meet_put flag contract on or/min, constructor refusal,
iteration spanning key/value with bounded range scans, skip flags
gating early-return only, kind mismatch typed, the interleaved-layout
round-trip ("the layout proof the whole positional grouping rests on
— the mutation target"), non-suffix put/scan + group-key-order-not-
row-order admissions + non-suffix delta, and the adopted adversarial
rev_* attacks (regime-aware mirror law, all-mutation-paths lockstep,
empty-group-key single group, insertion-order independence with the
laundered-out proof) — closed)
- **L1:** preserve-and-move → `exec/fixpoint/delta_store.rs` (seat
  exists: "working memory keyed on packed-code identity"), merging
  with query/levels.rs and ExecDedup per the #120 execution-currency
  seam already recorded at levels.rs's entry: this file's byte-keyed
  stores ARE the values-based v1 the packed-u32 code columns swap
  behind — the store shapes, MeetLayout proof, admission seam, and
  TupleInIter consumer surface survive; the KEY representation is what
  #120 replaces. `Admitted`/`AdmissionSink` are the "admitted
  currency" fixpoint/eval.rs recurses over — they stay at the seam.
- **L2:** gold, preserve verbatim: the semi-naive equivalence argument
  and termination certificate written at the store that embodies them;
  the admission seam's schedule-independence argument (canonical order
  at the epoch barrier) — provenance and budget BOTH depend on it; the
  monotonicity-exactness proof on meet_put_admission_faithful
  (in_flight ≤ admitted by construction, with the refuted len-counting
  theorem on record); the changed-flag contract with both failure
  directions named; the constructed-layout-proof pattern (projection
  arithmetic in exactly one place); zero-cost-by-monomorphization
  recording; the resident-size-vs-admission-count distinction stated
  on BOTH len methods; the mutation-targeted round-trip test and the
  adopted hostile battery. Nothing condemned. Watch on the #120
  merge: the values-stay-typed reasoning on meet folds is an argued
  v1 ruling — re-adjudicate it against packed-code columns when the
  currency lands, and the empty `impl TempStore {}` block dies in
  passing.

## query/incremental.rs (1562 lines; inventory: MPL header, module doc
(story #61's production IVM engine: an INDEPENDENTLY-WRITTEN twin of
laws.rs's `incremental_eval` — "never a shared import... so a bug cannot
hide behind one implementation covering for the other" — with the
transitive proof chain named: production == oracle incremental (this
module's differential) == naive recompute (the oracle's own);
`SignedFact` reused because it is ALREADY production code, `compose`
deliberately NOT — candidates-then-verify never composes two patches,
the same reason the oracle stopped after the multiset-vs-set bug; the
scope trichotomy: RECURSION refused outright (DRed is separate scope),
FIXED RULES unrepresentable — "there is nothing to refuse because
nothing constructs one", AGGREGATION fully covered; the two-phase
algorithm sketch), the IR (`Term`, `Literal`, `HeadAggr` — the REAL
landed Aggregation, "never a second hand-rolled implementation of sum
or min", `Rule` with no fixed-rule variant, `IncrementalProgram` with
no inline facts — "a standing query's whole point is that its EDB
changes out from under it", `MaintainedState` with the EpochStore
contrast written out: monotone-only, no Clone, no before-state — "exactly
the two things a standing query needs forever", `Bindings` as BTreeMap
with the hash-randomization doubt removed by construction), `unify`/
`ground`/`literal_rows`, `edb_relations(_pub)` (a patched relation is
EDB even when unreferenced; the zero-rows-is-still-EDB misclassification
guarded a SECOND way after this module's differential caught a silently
dropped relation), `topological_order` (patched-unreferenced sorted
first; asserts non-cyclic because refusal already happened),
`has_any_cycle` (edge-wise reaches(dep, head) — with the caught bug on
record: reaches(head, head) is trivially true on the first pop, which
refused EVERY program), `collect_candidates` (subset expansion over
delta-varying positions, 2^n−1 masks) + `contribute_candidates_subset`
(drivers iterate deltas regardless of sign; the rest join/gate against
old state), `head_is_derivable` + `body_bindings_from` (positives
first so negated literals probe fully bound), `IncrementalRejection`,
the TRANSLATION tier (section doc: `MagicAtom` is the right source, not
RelAlgebra — "by the time atoms reach RelAlgebra... there is nothing
left to translate"; the one real subtlety: post-rewrite constants live
in Unification atoms, folded back via a substitution map;
`TranslationRejection` — fixed rules, predicates, index searches,
non-constant unifications, each "refused, named", never silently
dropped; `magic_symbol_to_symbol` reusing the Debug rendering as the
canonical per-adornment identity), the aggregation extension
(`collect_affected_groups` reusing collect_candidates UNCHANGED,
`eval_one_group` — bounded by one group's body cost, the empty-key
global aggregate folds zero rows into the identity row instead of
vanishing, `eval_aggregating_head_incremental` — groups fully
re-derived because "a per-kind signed delta does not exist in
general", with the global-case re-check rationale), `incremental_eval`
(the well-formed-patch debug_assert — Plus/Minus disjoint per tuple,
"checked at the one seam every caller must cross"; cycle refusal; one
topological pass building new_state alongside; the EDB redundancy
filter — "the exact bug the oracle's differential caught on its first
run"), and tests: the hard corner both directions, the
second-untouched-derivation law (the multiset-vs-set bug's direct
test), recursion refusal, the production-vs-oracle GENERATIVE
differential (conv_* type converters, old_total minted by the oracle's
own naive_eval, four shapes incl. min-aggregation ×60 seeds, >100
cases asserted), and seven translation tests (positive/negated, the
rule-reference adornment identity, constant folding into head AND
body, aggregation carried through, all four refusals typed-checked,
and the composed translate→eval end-to-end) — closed)
- **L1:** preserve-and-move whole → `react/incremental.rs` (seat
  exists: "IVM: maintained views provably equal to recompute" — this
  file IS that provably-equal claim). The oracle twin stays across the
  crate wall in kyzo-oracle; the differential tests keep the bridge.
  The translation tier's source type (`StratifiedMagicProgram`) is an
  exec/plan artifact — on migration the translate() seam sits at the
  react/plan boundary, inside the engine crate, unaffected by the
  parse-to-model lift.
- **L2:** gold, preserve verbatim: the independence doctrine with its
  transitive proof chain; unrepresentable-over-refused for fixed
  rules; the refused-never-silently-wrong translation posture with
  every gap NAMED in its error; the has_any_cycle bug note (a refusal
  that refused everything is the kind of failure only an adversarial
  test corpus catches — keep the note); the well-formed-patch
  invariant checked once at the seam with its bug lineage; the
  EpochStore-vs-MaintainedState contrast as zone-boundary
  justification; groups-fully-re-derived over per-kind delta formulas.
  Nothing condemned. Arrival note: `magic_symbol_to_symbol` keys
  relation identity on a Debug rendering — lawful today (documented,
  unique per adornment), but when MagicSymbol moves into the plan
  tier, give the adornment a first-class canonical name so identity
  does not ride on Debug format stability.

## query/provenance.rs (1580 lines; inventory: MPL header, module doc
(the provenance trials: semiring provenance proven against INDEPENDENT
references — the six judges enumerated: the semiring axioms on
randomized values, the sealed oracle's naive_eval for boolean≡set
byte-identity, an independent shortest-derivation Bellman reference
"written from the model alone — no solver, no graph, no evaluator
symbol", an independent certificate checker re-deriving every step, the
1/2/4/8-thread determinism law, and the typed refusals; the ModelBody
harness named as "the shape of the trials harness"), `#![cfg(test)]`,
the splitmix64 `Rng`, the model harness (`ModelBody` implementing the
pub(crate) `RuleBody` seam — naive nested-loop unification over live
EpochStores, with the occurrence-map ruling that a negated read counts
for lifetime tracking but is never delta-selected; `premise_sources`
attributing exactly as a compiled plan will; `UnattributedBody` — the
deliberate negative control whose premise_sources stays None; `NoFixed`
unreachable), the transcribed stratum assignment (`dependency_edges`
over the SHARED laws::head_classes per issue #89 — this harness "used
to hand-copy them"; `strata_of` Bellman iteration; any valid
stratification yields the oracle's fixpoint), `compile_for` (retain_all
extending every store's lifetime to the final stratum — the documented
provenance requirement), the generous budget/ceiling/solver constants,
`at_thread_count` (asserting the pool width so "a 1-thread 8-thread
run would prove nothing"), `run_pipeline`/`PipelineOutput`/`rule_node`,
the generated positive fragment (`gen_positive`: TC with a coin-flipped
self-join vs edge-recursion, optional mutual recursion qa/qb, optional
join over two recursive stores, optional hop2; `gen_weights` 1..=8 per
(head, rule-index); `engine_weight_fn`), the independent tropical
reference (`rule_instantiations` asserting positive-fragment-only;
`reference_min_costs` — "naive and obviously correct", panics if its
own fixpoint fails to stabilize), the independent certificate checker
(`verify_model_proof`: leaves are genuine stored facts, steps are valid
instantiations with ONE binding satisfying head and every premise,
costs re-derived with checked arithmetic; opaque-store leaves and
negated-premise rules refused as boundaries), and the seven trials:
axioms (⊕ assoc/comm/identity/IDEMPOTENT-as-solver-contract, ⊗
assoc/comm/identity/annihilator, distributivity, ×2000 each semiring);
tropical overflow as typed SemiringOverflow with ∞-absorbs-lawfully and
the solver surfacing it "typed, not stringly"; boolean annotation ≡
naive_eval byte-identical over 24 seeds with the
nothing-outside-the-fixpoint converse; tropical min-cost vs the
reference at unit AND random weights (facts cost 0); certificates —
extracted for the DEEPEST path row, verified structurally against the
graph AND semantically by the independent checker, four corruptions
each rejected by BOTH checkers (cost lie, forged leaf, wrong rule
label, dropped premise), ghost target refused typed NoDerivation;
thread-count determinism over annotation+costs+proof fingerprints; the
PA4 aggregation collapse boundary (meet rows enter the graph as ground
cost-0 facts, the plain reader costed above); and the typed refusals
(unattributed body, unretained store — exercised by explicitly
dropping a store because "the map a caller passes is the contract
surface", enumeration ceiling with exact ceiling/spent, the
ceiling refusal itself deterministic across threads, solver pass
ceiling on a reversed 5-chain with the same graph solving at 6 passes,
and the open-graph closure check) — closed)
- **L1:** preserve-and-move → `kyzo-trials/src/provenance.rs`
  (NEW-SEAT, operator ratification required: the trials tree has no
  provenance lane, and this battery is exactly the map's definition of
  a campaign — an attack on a public claim, the telos's "explain",
  rerunnable by strangers). REWIRE at the crate wall, same shape as
  dst_query.rs: the harness drives pub(crate) seams
  (stratified_evaluate_with_stores, provenance_graph, RuleBody) —
  provenance is a product claim, so its graph/solve/extract/verify
  surface should become sealed-contract-public and the trial attacks
  it through the door; the independent references and the checker move
  intact (they already import nothing from the machinery they judge).
  The semiring axioms trial stays wherever `semiring.rs` seats its
  public vocabulary (exec/provenance/), as its adjacent battery.
- **L2:** gold, preserve verbatim: the six-judge architecture with
  each judge's independence argument stated; the negative-control
  pattern (UnattributedBody); verify-both-ways with corruption
  rejected by BOTH checkers (a corrupt proof passing one checker would
  localize the bug); idempotency named as the solver contract inside
  the axiom battery; the deepest-row certificate choice; the
  pool-width assertion; refusals tested for typed identity AND
  cross-thread determinism. Nothing condemned. Already-repaired
  lineage note: the issue-#89 consolidation onto laws.rs's shared
  reference-tier helpers is the sanctioned sharing direction
  (reference↔reference), distinct from the production↔oracle wall
  stratify.rs's entry records — keep both rulings visible.

## query/magic.rs (1848 lines; inventory: dual fork header naming twelve
modifications (reverse walk over the landed execution-ordered strata,
un-reversed exactly once; the adornment phase returns a LOCAL
`AdornedProgram` keyed by `AdornedHead` — Muggle or Magic BY TYPE,
turning the original's "remaining options are impossible" comment into
structure; entry exemption structural via `SymbolKind::Entry`, not a
seeded dummy-span symbol; `disable_magic_rewrite` once on the tier;
unwraps as typed internal errors; rule_idx/sup_idx u16 narrowing CHECKED
— "silent wrap-around would merge distinct supplementary relations —
extra join tuples, i.e. changed RESULTS, not just changed demand";
universal bitemporal format retires the keys.last().unwrap() panic
site; the `StoredRelationSchemaSource` seam mirroring BodyNormalizer;
index-search atom arms deferred to the index tier; Vec<bool>
adornments; the exempt walk re-homed onto the stratum;
NamedFieldNotFound declared here), module doc (magic sets as
demand-driven rewrite; THE LAW: "the rewrite may change only demand —
never result semantics... every deviation is a wrong-answers bug, not a
performance bug"; the FULLY-FREE IDENTITY THEOREM of issue #68 — SIP is
locally right but uselessly adorns self-join occurrences under a
fully-free entry, Andersen points-to minting three separately-
fixpointed pt variants plus ~twenty sup relations; the two load-bearing
passes IN ORDER, with the hostile-review finding that the redirect
without the sweep left a whole ORPHAN CLASS — "reachability from the
roots is the actual invariant"; the standing executable form
`magic_bypass_differential`; visible-internally-invisible-at-boundary;
the four exemptions with each one's REASON — the entry's store IS the
answer, an aggregate over a restricted subset is a DIFFERENT VALUE, the
flag, and cross-stratum producers), `StoredRelationSchemaSource`,
`NamedFieldNotFound` + `MagicInvariantError` (returned never panicked,
"instead of silently changed demand"), `AdornedHead` (+4 methods) +
`AdornedProgram` ("never leaves this file, which is what keeps the
Muggle-or-Magic proof airtight"), phase 0 (`magic_sets_rewrite` with
the walk-direction PROOF written as doc — consumers execute after
producers so the walk visits consumers first, "an inverted walk does
not crash: it silently drops or specializes cross-stratum producers...
wrong answers", pinned by a named regression test;
`collect_magic_exemptions`; `cross_stratum_dependencies` — fixed-rule
in-memory args unconditional), phase 1 (`adorn` with the transitive
pending-adornment loop and two typed impossible-path errors;
`adorn_fixed_rule_apply` — in-memory args always Muggle because
"demand cannot restrict an opaque algorithm's input", named-field
positional resolution with digit-leading filler names that cannot
collide with grammar identifiers; `NormalFormAtom::adorn` per variant
— validity extra_var binds like Search's own_bindings, repeated
variables adorn later positions BOUND (faithful), exempt applications
deliberately do NOT extend seen_bindings — "that only widens demand...
which the law permits", negated applications never adorned because
"negation needs the complete relation to subtract from"), phase 1.5
(`collapse_ff_redundant_variants` — the #68 driver with the MEASURED
OOM lineage: pointsto_repro.rs exhausts a 12 GiB cap through the
rewrite while the bypass completes bounded; sound "regardless of WHY
the ff variant is demanded"; `sweep_unreachable` — mark-and-sweep from
Muggle roots, closing the orphan class and subsuming collapse's
removed retain step), phase 2 (`magic_rewrite`; `push_magic_rule`
typed collision; `magic_rewrite_ruleset` — the sup-chain SIP: sup₀
seeded from the Input relation, a cut at every bound-adorned
application, the callee's Input fed by projection, and the rewritten
rule's head and aggr UNTOUCHED — "the law: the rewrite may reshape
bodies, never what a rule returns"), and the ten-test battery
(strange_case identity under the disabled flag WITH the enabled
contrast proving the flag load-bearing; the bound-TC rewrite pinned
store-by-store with the semantics-preservation induction argument in
its doc; entry/aggregation/flag/cross-stratum exemption pins — the
last the standing inverted-walk regression; the bfb mixed adornment;
the repeated-variable pin with its any-change-is-deliberate warning;
named-stored positional resolution + unknown-field refusal; the
universal-format time-travel test where keyless relations adorn
without panicking) — closed)
- **L1:** preserve-and-move whole → `exec/plan/magic.rs` (seat exists:
  "the magic-sets demand transform"). The `StoredRelationSchemaSource`
  seam binds to the session transaction where it already points; the
  deferred index-search atom arms land with project/ index tiers as
  the header says.
- **L2:** gold, preserve verbatim: the demand-vs-results law stated at
  the head and re-derived at every decision (the checked narrowing,
  the exempt-doesn't-bind ruling, the untouched head/aggr); the
  fully-free identity theorem with its two-pass proof and the
  hostile-review orphan lineage; the walk-direction proof plus its
  named regression; Muggle-or-Magic-by-type replacing a comment with
  structure; the measured OOM justification (a perf claim closed on a
  reproducer, rule #19); exemptions each carrying their reason.
  Nothing condemned.

## query/ra/mod.rs (2024 lines; inventory: dual fork header with FIVE
story-#3 transformations (storage access through the kernel's ReadTx
species — "the operator tree itself is transaction-free data"; the
Reorder/NegJoin join-RHS invariant made CONSTRUCTURAL — refused at plan
construction where the original panicked at iteration; negation over
time travel NOW COMPUTES — story #86 built NegRight::StoredWithValidity
/Spans/Delta and deleted NegationOverTimeTravelError, "nothing is left
to refuse"; every-referenced-rule-has-a-store typed via epoch_store_of;
the index-search operators as seams — since landed as the ONE Search
variant collapsing upstream's three per-engine node kinds), the
ELEVEN-SITE Law-5 panic audit with each upstream site's fate on record
(typed errors, unrepresentable states, and site 9 "RETIRED WITHOUT
SUCCESSOR" — the universal bitemporal format leaves no per-schema
validity column to check; slice-index sites argued as compiled
knowledge with the two cross-function range-slices defensively
`.unwrap_or(&[])`), and deviations D1–D5 (TupleIter homed here as
operator-tier currency; transpose over the dissolved utils; log
dropped, Debug/explain preserved; itertools::Either; Joiner::as_map
retained for the explain surface), module doc (an operator is a
TUPLE-STREAM TRANSFORMER; a compiled body is one left-deep tree and
evaluating a rule is iterating the root; positions-not-names —
"iteration never looks at a name again"; the POSITIONAL delta
discipline: only the one TempStoreRA whose AtomOccurrence matches
delta_rule reads its delta, negation always reads totals, self-joins
get independently-selectable occurrences — Δ(P⋈P) = (ΔP⋈P) ∪ (P⋈ΔP);
determinism as a function of stores and plan alone), `TupleIter`,
`BatchFilter` (an operator never yields an empty batch — "is_empty()
on a received batch is unambiguous end-of-window bookkeeping, never a
real datum"), `PlanInvariantError`, `StoredRowTooShortError` (decoded
length comes from stored bytes, so a short row is CORRUPTION surfaced
typed), `epoch_store_of`, the 12-variant `RelAlgebra` enum, its
methods (span; fill_binding_indices_and_compile — Spans/Delta carry no
Expr by construction, NegJoin's right never carries filters; unit/
is_unit; derived — occurrence-keyed; `relation` with the four validity
arms — universal time travel means "construction checks nothing about
the columns", Spans/Delta's one extra trailing binding pushed by the
constructor, Delta's posting resolved later at compile's catalog;
`filter` — per-variant pushdown, the Join arm splitting conjuncts
left/right/remaining, Spans/Delta wrapped per chunk-3 scope; `join` —
typed construction-time RHS refusals with each shape's REASON;
`neg_join` — the total constructor into NegRight; the elimination
trio; `iter_batched` — TOTAL dispatch, "no row-at-a-time fallback
exists anywhere (the iterator twin and its chunker were deleted; the
naive oracle in query/laws.rs is the semantic judge)"), the Debug
substrate for ::explain (Unit/Singlet compression, unit-left joins
render as their right), and the 13-test battery: the three
InlineFixedRA join strategies; the mutant-K4 pin
(batched_join_singleton_fixed_left_is_not_unit — "the guard must hold
independent of what constructs the plan", a survivor of the mutation
campaign pinned at the RA level); the materialized join vs a
HAND-COMPUTED analytic oracle straddling the output-batch boundary
twice with join_type routing asserted; spread unification + its typed
non-list refusal; the time-travel scan; the typed-refusals test that
ALSO proves #86's shape now constructs; the three-size batch-boundary
join (BATCH_ROWS−1/±0/+1, half-miss probes); dual-side eliminate
indices; delta threading narrowing the join to the fresh row; the two
issue-#75 segment differentials (point-lookup and prefix probes,
segments ON/OFF byte-identical AND equal to hand-computed expected);
the `#[ignore]`d segment-vs-storage cost probe carrying its measured
numbers in the doc (200k probes, storage 1023–1153 vs segment 230–243
ns/probe, ~4.5x, with the fan-out amortization explanation); and the
hostile-review error-ordering reproducer (a later stream error must
not outrank an earlier predicate poison) — closed)
- **L1:** preserve-and-move → `exec/op/` as the zone's spine (op/mod.rs:
  the RelAlgebra tree, its total constructors, the typed invariants,
  the batched dispatch, and the explain Debug substrate — structural
  glue the map's file list implies; zones are stable, files grow). The
  submodule re-exports realign to the map's names: fixed.rs →
  op/literal.rs ("unit and literal-block relations"), temp.rs →
  op/delta.rs ("fixpoint total/delta scans"), the rest map one-to-one
  (join/neg/stored/search/temporal/transform). `BatchFilter` travels
  to op/transform.rs with its kin. `StoredRowTooShortError` stays
  beside the stored scans that raise it.
- **L2:** gold, preserve verbatim: the Law-5 audit as a PERMANENT
  header artifact (every upstream abort accounted for, including the
  retired-without-successor ruling); constructural-over-runtime
  refusals (join RHS, NegRight) as the house pattern; the positional
  delta discipline doc with the self-join rewrite; the
  no-row-at-a-time-fallback ruling (one machine, oracle-judged); the
  never-empty-batch contract; judged-against-hand-computed-oracles
  discipline named inside the tests themselves; the mutation-campaign
  pin with its survivor lineage. Rule-#11 ledger (pre-existing): the
  `#[ignore]`d cost probe is a measurement rig — bench lane on
  migration; its measured numbers already satisfy rule #19's
  perf-claims-close-on-a-reproducer standard and move with it.
  Nothing condemned.

## query/time_travel_trials.rs (2526 lines; inventory: dual fork header
(story #3 item C.10 — the README's as-of claims proven through the FULL
query path, compile → RA → semi-naive eval, over a real FjallStorage;
"a disagreement is a finding"; TEST-ONLY, the harness reconstructed
from compile.rs's private test module; the PINNED BOUNDARY SEMANTICS
traceable to the key encoding: at-instant reads INCLUSIVE, assert
encodes 0x00 and beats retract at the same instant, identical triples
collapse last-write-wins), `#![cfg(test)]`, plumbing (builders incl.
pred_ge/pred_le that compute_bounds recognizes; compile_and_run),
`Version` fixtures + `write_history` (ONE transaction = one system
stamp, "the one-lineage-per-instant law") + `stored_plain` +
`write_history_multi_tx` (one tx PER version, returning the REAL
minted system stamps — with the doc explaining why spans/delta
differentials need real stamps while as-of-at-current does not), THE
UNIFIED ORACLE (`naive_asof_cfg` routed through laws::resolve_relation
per story #62's oracle unification — write order becomes the sys axis;
the two SABOTAGED configs (exclusive boundary, first-write-wins) still
route through the one real resolution function, "just fed a
deliberately wrong encoding of which write governs"), the BRIDGE
differential (`independent_asof_reference` written from scratch
"without reusing any part of naive_asof_cfg, old or new"; 300 seeds ×
all four boundary/write-order configs, >500 cases — the sabotaged
forms must agree with their own from-scratch counterparts too),
`interesting_instants`, and the batteries: TASK 1 boundary+same-
instant pins (inclusive boundary; both write orders of assert/retract
at one instant; identical-key overwrite; retraction-only key never
present; the multi-key full-history matrix at every interesting
instant); TASK 2 full-path differentials (transitive closure through
REAL recursion per instant vs naive close-after-asof; two-relation
same-instant join; count+sum over as-of with empty→[0,0]; MEET min
with the empty-population Null identity row PINNED as "a defined
value, not a silent gap"; the bounded as-of scan driving
compute_bounds → skip_scan_bounded_prefix with a hand-computed case
AND the full differential); TASK 3 retraction-is-revision (earlier
instants still addressable; plain scans read CURRENT state, never raw
versions); TASK 4 byte-identical across 1/2/4/8 threads; TASK 5 the
story-#86 negation branches (the prefix-probe branch generative over
300 seeds with a sentinel candidate, expected = candidates minus
naive_asof's present set "never by re-deriving the engine's own
answer"; the materialized non-prefix branch with a
retraction-discriminating instant; the NoStoredInputs refusing seam
pinned AS the superseded placeholder it is; validity scans
constructible at the RA layer); TASK 6 mutation-proofs (a
boundary-flipped oracle and a retraction-dropping reference must each
DISAGREE with the engine — "else the harness is blind"); the
TWO-COORDINATE flagship ("what did the record say at S about V" — the
correction invisible before its stamp, governing from it, with the
Reverse-order stamp monotonicity asserted); naive_present_edges/
naive_transitive_closure; the story-#62 chunk-3 section
(spans_atom/delta_atom builders, oracle_spans/oracle_delta shaping
laws output to engine rows, plane_interval converting the oracle's
half-open form to closed normal form; spans generative ×300 at two
sys cuts; spans COMPOSES through ordinary rule nesting — ruling item
3 proven DEFINITIONAL: rule applications have no validity field so
"the only place a clause can ever be written already IS the leaf",
plus the two grammar-refusal tests making it structural; delta
generative BOTH axes; the composition law diff(a,c) == diff(a,b) ⊕
diff(b,c) through the REAL engine via laws::compose AND separately
via the PRODUCTION temporal::compose — the story-#77 differential
that gave the tested-but-unused law its proof on real output); five
named degenerate pins ("pinned here by name so a regression fails
with a readable label rather than only a seed"); and the
hostile-review TEXTUAL PARSE coverage (every other test builds
MagicAtoms directly — the keyword-boundary bug lived in exactly that
unexercised seam: four positive clause parses and three
boundary-refusal tests pinning the CONFIRMED `@spansX` bug and its
fix's mutant) — closed)
- **L1:** preserve-and-move → `kyzo-trials/src/time_travel.rs` (seat
  exists: "the temporal law and trial batteries"). Same crate-wall
  rewire as dst_query.rs: the harness drives pub(crate) compile/eval
  seams and must speak the public surface (or real KyzoScript) on
  arrival; the oracle side already lives across the wall
  (kyzo-oracle's temporal.rs). EXCEPTION: the textual-parse coverage
  (the four clause parses, the three keyword-boundary refusals, and
  the two grammar-structural refusals) tests the GRAMMAR, not the
  engine — it travels to kyzo-model's parse-tier tests with
  kyzoscript.pest, not to trials.
- **L2:** gold, preserve verbatim: sabotaged-oracle mutation-proofing
  (the harness proves its own eyes work — both directions); the
  from-scratch bridge whose sabotaged forms are verified against
  their own counterparts; boundary semantics pinned WITH their
  encoding-level traceability; named degenerate pins beside seeded
  campaigns (readable failures); the real-stamps-vs-synthetic-index
  distinction between the two history writers, with its reasoning;
  definitional-over-implemented for ruling item 3; the
  production-twin compose differential kept separate from the
  oracle's. Nothing condemned.

## query/compile.rs (2889 lines; inventory: dual fork header with SIX
story-#3 transformations (free functions over the kernel's ReadTx
species — "the operator tree itself is transaction-free data" and the
temp-relation router named as a session-tier SEAM; strata arrive in
execution order, the original's .rev() "has no descendant here";
ruleset invariants as CONSTRUCTOR PROOFS — `CompiledInlineRules::new`
refuses an empty set and enforces ARG-LEVEL head-aggregation equality,
"the mirror of the parser's check... re-established where the
signatures collapse into one"; AggrKind NOT re-declared — "one
concept, one name", it lives in eval's HeadAggrKind; `contained_rules`
RE-HOMED here with the occurrence-numbering fix: upstream numbered by
STORE NAME, collapsing self-join occurrences into a `Many` that forced
a complete naive re-join every epoch — issue #68's dominant driver,
confirmed structurally AND by measurement (fixpoint_mem_profile:
18–43× more allocations per output row, growing super-linearly);
`CompiledRuleBody` implementing the evaluator's RuleBody seam), the
THREE-SITE Law-5 panic audit (rules[0] indexing structurally removed;
the unwrapped set-difference restructured with a typed impossible arm;
the search-arm debug_asserts deferred to the index tier as typed
checks), deviations D1–D4 (index refs resolved by name through the
catalog; dead vectors dropped; budget/interrupt wiring named with the
"never solved by re-adding Poison" ruling; premises NotRequested — the
provenance-tier seam), module doc (compilation = proven program to
executable plan; the left-deep growth walk; positions proven, unbound
head symbols refused, columns reordered so "the plan's output frame
equals the rule head, position for position"), the compiled tier
(`CompiledProgram`, `CompiledRuleSet` + total `arity`,
`CompiledInlineRules` + `RulesetHeadAggrMismatch` — "no tier between
parse and eval can smuggle a disagreement through", `CompiledRule`),
`atom_occurrences` (the numbering lives "in exactly one place"; one id
per Rule/NegatedRule atom) + `contained_rules` (negated occurrences
ENTERED — the map is StoreLifetimes' dependency source and "a store
read only inside a negation is used just as much"; never
delta-selected in practice, with the if-it-ever-fired soundness
argument), RuleNotFound/ArityMismatch/UnboundSymbolInRuleHead,
`stratified_magic_compile` (arities collected across ALL strata),
`resolve_delta_posting_index` (chunk 4's read-side seam; "returning
None here is never a correctness gap, only a missed acceleration"),
`compile_magic_rule_body` (unit seed; gen_symb for repeated variables
so "the joiner is always positional underneath"; the occurrence
counter in documented LOCKSTEP with contained_rules; the seven atom
arms — Rule join with arity proof, Relation with access-level check +
index selection where temporal clauses ALWAYS scan the base ("an
index mirrors only the current-state keyspace"), index-only vs
back-join with residual equality re-checks for join columns the index
could not bind, NegatedRule consuming-but-never-selecting an
occurrence id, NegatedRelation where a back-join index is "useless
under negation", Predicate, Search with the fresh-var-plus-equality
join discipline, Unification unify-or-filter; the tail: eliminate →
unit cartesian fix-up → the typed unbound-head refusal with the
impossible empty-difference arm → reorder-to-head),
`CompiledRuleBody` (seam impl #2, its contract clauses each named
WITH where they are discharged; rows cross the seam as borrowed
slices, "eval... mints an owned row only on admission"), the
UNINHABITED `NoFixedRules` ("running one is unrepresentable"),
`bind_for_eval` (fixed-rule evaluator injected by the caller), and
the test battery: the two CALIBRATED budgets (generous_budget's
ceiling deliberately low with the measured OOM-before-ceiling
justification — "Keep this low"; boundary_budget's sizing argued);
the ported upstream mat_join; TC end-to-end over real fjall; head
reorder; strategy-path pins via join_type introspection (prefix vs
materialized, point lookup, both negation strategies); five typed
refusals (unknown rule, arity, unbound head, the trap-(c) aggr
mismatch, hidden relation via InsufficientAccessLevel); THE
RA-vs-oracle differential (the model compiler mirroring eval's
harness but with REAL stored EDB and compiled plans; both execution
modes asserted equal to the sealed oracle, which "simultaneously
proves the batched path equal to the iterator path"); nine
differential shapes (TC, self-join, THREE-way self-join, stratified
negation, the meet self-join THROUGH RA carrying its CONFIRMED
mutation lineage — "mutating scan_epoch made this exact rule shape
diverge while the model-harness suite stayed green", meet-in-
recursion, normal aggregation, constant-argument desugaring,
recursive-right self-join with its named mutation kill); the two
occurrence pins; the non-prefix set-probe pinning the `contains`
sense; the law-5 truncated-row pair (a keys-only stored row decodes
short; point-lookup and neg-prefix joins both surface typed
StoredRowTooShortError, never a slice panic); and the batched
section (unification single/spread across boundaries + poison-row
error identity across runs; scan+filter at eleven sizes × three
thresholds spanning 0..4097; recursion sizes chosen so the STORE
straddles BATCH_ROWS; the 120-seed LCG campaign "the vectorization
ascent's mutation test sabotages"; the direct seam-drive survivor
count) — closed)
- **L1:** preserve-and-move whole → `exec/plan/compile.rs` (seat
  exists: "the plan compiler"). `CompiledRuleBody`/`bind_for_eval`/
  `NoFixedRules` are the plan→fixpoint bridge and move with it (the
  RuleBody seam's other half lives in fixpoint/eval.rs). The test
  module stays attached as the zone's module tests; the RA-vs-oracle
  differential harness is also the shape kyzo-trials' differential
  lane will drive through the public surface — migrate the module
  tests intact and let trials grow its public-surface twin, never
  thin these.
- **L2:** gold, preserve verbatim: constructor-proofs-over-
  conventions (non-empty, signature-uniform, total arity); the
  one-place occurrence numbering with its lockstep documentation at
  BOTH consumers; the re-proven-at-every-tier aggr-mismatch law; the
  measured justification for the occurrence fix (rule #19 satisfied
  with a named profile); the calibrated-budget notes that tie a test
  constant to a measured failure mode; the confirmed-mutation
  lineages naming exactly which sabotage each differential catches;
  uninhabited-type refusals; the borrowed-slice admission economy at
  the seam. Nothing condemned. Carried obligation (from D3): when
  db.rs wires fixed rules, Budget::check_interrupt/ticker go
  pub(crate) — "never solved by re-adding Poison"; the entry for
  runtime/db.rs must verify this landed lawfully.

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
  lanes: the generator + Capability 1 → `kyzo-trials/src/gauntlet.rs`
  (generated-program hunting) with its determinism assertions feeding
  `determinism.rs`'s lane; Capability 2 → the proposed
  `kyzo-trials/src/provenance.rs` (NEW-SEAT, shared with
  query/provenance.rs's entry); Capabilities 3–4 →
  `kyzo-trials/src/time_travel.rs` beside time_travel_trials.rs's
  material. Same crate-wall rewire as its siblings (pub(crate) eval
  seams → public surface or a sanctioned deeper seam; the oracle side
  is already kyzo-oracle vocabulary). OPERATOR-VISIBLE STANDING ITEM:
  the module's own stated open gap — no end-to-end demand-rewriter
  differential — is scheduled at the session tier (runtime/db.rs
  wave); the migration must carry that obligation forward, not lose
  it in the move.
- **L2:** gold, preserve verbatim: the stated-boundary discipline
  (open gaps named in the doc, never smuggled); generator dimensions
  justified by the exact mutant each discriminates (cross_join's
  masking argument); hand-mutant pairs that prove the CAMPAIGN's own
  eyes (a weakened generator shown blind, the real one shown to
  catch); the counted comparative claim over a boolean where the
  boolean would overclaim; the epistemics sections stating what each
  oracle-vs-oracle check does and does not prove; model-derived
  arities against vacuous passes; the real-landed-ops fold rule for
  references; fixed-order generator vocabularies for seed
  reproducibility. Nothing condemned.

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
- **L2:** gold, preserve verbatim: the determinism law with its four
  supports stated as an invariant system (barrier-only checks named
  as a determinism REQUIREMENT, not a style choice); the
  non-perturbation theorem WITH its refutation history and the
  landed counterexample-as-differential; the boundedness law tied to
  the incident it forecloses; N1's do-not-strip warning on the
  load-bearing dedup; refusals as first-class deterministic outputs
  (byte-identical across threads, exact spends, honest admitted-not-
  materialized accounting); mutants killed by LITERALS where a
  symbol-relative bound would move with the mutant; the honest
  generator-gap list cross-referenced to fixed pins; the traced
  limiter semantics (D2/N2) preserved as documented behavior rather
  than silently "fixed". Nothing condemned. Carried obligations:
  D3's full retirement is complete (the tests prove construct AND
  answer); the story-#80 pub(crate) widenings (epoch_ceiling,
  check_interrupt) are the sanctioned oracle seam — the kyzo-oracle
  split must give the oracle its own budget vocabulary or keep this
  seam deliberate.

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
  `kyzo-oracle/src/eval.rs`; the temporal vocabulary (AsOf, Event,
  resolve*, derive_intervals, diff/compose, Axis/Interval/OPEN_END) →
  `kyzo-oracle/src/temporal.rs`; the story-#61 incremental reference →
  NEW-SEAT `kyzo-oracle/src/incremental.rs` (operator ratification;
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
- **L2:** gold, preserve verbatim: obviously-correct-by-inspection as
  the design criterion (optimizing the oracle is a defect); the three
  upstream divergences recorded WITH their directions; the lift's
  structural never-gap argument and its deleted-check ruling; the
  one-seam constructor discipline with its five-file-fallout lesson;
  the #89 sharing-soundness argument paired with the one
  deliberately-independent copy; the terminal-tick reservation making
  the zero-width interval unrepresentable; the exact-correspondence
  doc PROVEN by the kernel cross-check; additive-budgeting so the true
  answer stays the oracle's claim; the multiset-vs-set lineage; review
  findings landed as paired positive/negative pins; loud-failure
  regressions over silent ones. Nothing condemned.

## runtime/mod.rs (52 lines; inventory: MPL header, module doc (the
session tier: entrypoint, mutation tier, catalog, constraints,
callbacks), `current_validity` — "the engine's ONE wall-clock read...
Lives in the runtime tier by law — the value plane has no ambient
clock, and determinism campaigns replay stamps rather than minting
them" — with the pre-epoch and beyond-i64 clock refusals typed, and
eight module decls whose `#[allow(dead_code)]`s each carry an honesty
note (lib-dead until consumers land; "a mod-level allow covers that
remainder honestly") — closed)
- **L1:** structural glue — dies with the directory when runtime/
  becomes the map's session/ zone (db→session/db.rs,
  json→session/json.rs, mutate→session/admit.rs,
  constraint→session/constraint.rs, relation→session/catalog.rs +
  access.rs, callback→session/observe.rs, verify→session/verify.rs;
  db_battery is deprecated-absorbed.md's claim). `current_validity`
  seats at `session/db.rs` beside the entrypoint: the one-clock law
  travels verbatim — the session tier lifts the ambient input once,
  and no other zone may mint a stamp.
- **L2:** gold: the one-clock law and its determinism rationale; the
  per-allow honesty notes (each dead_code carries its reason and its
  landing consumer). Nothing condemned. Watch on the split: the
  target uses `expect` for self-removing dead-code where possible —
  the notes here already say which allows are principled remainders.

## runtime/json.rs (163 lines; inventory: dual fork header (the wire
format itself lives in `data::json` — "it needs no live session, so it
belongs with the value kernel"; this file adds EXACTLY the one piece
that does need one, composing data::json "not reimplementing any JSON
shaping"; params as a JSON object so bindings never hand-roll the
DataValue conversion, with a non-object reported "through the same
envelope as any other query error, not a separate Result a caller
could forget to check"; `took` absent on wasm32 "rather than a compile
error or a stubbed zero that would silently misreport"), module doc,
`Db::run_script_json` (the ONE "JSON params in, JSON envelope out"
entry point every binding shares; always Ok at the Rust level; Null
params = empty map), and five tests (success envelope shape, $param
binding, parse error without panicking, non-object refusal, and the
story-#80 product-surface PROOF — "::verify rides this seam like any
other script... no new kyzo-bin code at all", proven at the seam
rather than asserted) — closed)
- **L1:** preserve-and-move whole → `session/json.rs` (seat exists:
  "the one JSON door over the envelope vocabulary"). Its wire-format
  half already lives at the model tier per data/json.rs's own entry;
  this door composes it, unchanged.
- **L2:** gold: the one-door discipline with failure-through-the-
  envelope (no forgettable Result); the wasm honesty (absent field
  over a lying zero); the proven-at-the-seam product claim pattern
  (a new SysOp reaches every host for free, and a TEST says so).
  Nothing condemned.

## runtime/callback.rs (212 lines; inventory: dual fork header (the
registry tuple made a NAMED struct so the two halves' coherence "is at
least nameable and locally audited — register/unregister/prune are the
only mutators"; std mpsc over crossbeam with the bounded capacity
REMOVED — "a bounded channel made send_callbacks... block on a slow
consumer. Unbounded + lossy-by-disconnect is the whole contract now";
the two directory unwraps gone — already-unregistered, law 5; THE
RETRY LAW — the collector is built fresh per commit attempt and
delivered only after success, "a conflicted attempt can never leak
phantom events"), module doc (delivery ordering: after
process-crash-durable commit, in relation order, in mutation order
within a relation; LOSSY BY DISCONNECT documented — "a notification
surface, not a replication log — an observer that must not miss
events should read the relation, not trust the channel"),
`CallbackOp` (+Display/as_str), `CallbackEvent`,
`CallbackDeclaration`, `CallbackCollector` ("plain data: building one
has no side effects"), `EventCallbackRegistry` (register/unregister
maintaining both maps), and the four Db methods (`register_callback`,
`unregister_callback`, `current_callback_targets` — "snapshotted once
per transaction, so a registration racing a commit either sees all of
it or none of it", the anchor standing.rs's snapshot-consistency
proof cites, and `send_callbacks` — post-commit only, pruning on send
failure) — closed)
- **L1:** preserve-and-move whole → `session/observe.rs` (seat
  exists: "post-commit callbacks and relation triggers"). The
  consumers already censused (react/standing.rs) cite this file's
  contracts by name; the citations survive the rename.
- **L2:** gold: the lossy-by-disconnect contract stated as product
  law with its read-the-relation escape hatch; the plain-data
  collector making phantom events structurally impossible under
  retry; the once-per-transaction target snapshot that standing
  queries' consistency proof is built on; coherence-by-named-struct
  over tuple-field convention. Nothing condemned.

## lib.rs (331 lines; inventory: MPL header, THE CRATE DOC (the telos —
"turn meaning into bytes and back WITHOUT LOSS OF TRUTH"; the
LLM-adversary framing — "the query authors of the next decade are
language models — brilliant, adversarial, unbounded — so the engine
hands them contracts, not hopes"; "the world model is the type graph"
with one-name-per-concept; the five tier sections each naming its
proofs — kernel/parse/query/runtime/engines/fixed-rules; THE
ENFORCEMENT LADDER "compiler > constructor > test" with named exemplars
at each rung; verification-is-architecture (oracle, differentials, DST,
fuzzing, mutation); HONEST BOUNDARIES — the dead_code accounting
promise ("each module's own comment says which, and each attribute
narrows or vanishes as its items gain a caller") and the closing law
"No claim here is aspirational; every type and law named above exists
as named in the tree"), the three crate attributes with their
justifications (`#![forbid(unsafe_code)]` — "forbid, not deny: the
strongest standard, which cannot be locally lifted", the future-unsafe
protocol named; type_complexity; the mutable_key_type false-positive
account), nine module decls with per-module dead-code honesty notes
(format awaiting story #92; fixed_rule landed with the superseded
placeholders kept live by their regression test; parse's Imperative
genus a typed refusal), `#[cfg(test)] mod jepsen_trials`, the public
re-export surface (kernel values, storage incl. backup/retry/verify,
fixed_rule vocabulary, callbacks, Db/ScriptOptions, VerifyOutcome,
SignedFact, StandingQuery), and the three façade doors (bench_api and
fuzz_api feature-gated; lsp_api ALWAYS compiled — "live diagnostics
are a first-class product surface") — closed)
- **L1:** reforge-in-place → the target `kyzo-core/src/lib.rs` ("the
  sealed public contract: the one Db façade"). On the crate split: the
  kernel-value re-exports (DataValue, Tuple, Validity, EncodedKey, …)
  become kyzo-model's public surface, re-exported or consumed
  directly; the storage re-exports narrow behind the sealed contract;
  the three façade doors DIE per deprecated-sealed.md (their
  consumers rewire); the crate doc's world-model prose survives as
  the contract's own preamble, re-tiered to the new crate boundaries.
- **L2:** gold, preserve verbatim: the telos statement and the
  LLM-adversary framing; the enforcement ladder as organizing
  doctrine; the no-aspirational-claims closing law (rule #20's
  same-truth-everywhere, self-imposed); the per-attribute and
  per-module justification discipline (no naked allow); forbid-not-
  deny with the deliberate-lowering protocol. DOC DRIFT to correct on
  arrival (rule #20): the verification section still describes the
  oracle as "`query::laws`, `cfg(test)` — judge, never production",
  but story #80's `::verify` door (VerifyOutcome re-exported HERE, in
  this same file) consumes it in production — the same stale claim
  laws.rs's entry flags; the target formulation is the map's "the
  engine summons its judge (kyzo-oracle)". Nothing else condemned.

## runtime/constraint.rs (1103 lines; inventory: MPL header, module doc
(a constraint is "a NAMED pure query that must derive nothing: the
Datalog ⊥ :- body shape" — FK, CHECK, and secondary uniqueness "all the
same species"; the mechanics stated plainly: the body MIRRORED into the
catalog row of every relation it reads "so an FK fires both when a
child appears and when its parent disappears"; enforcement after the
whole trigger cascade, before commit, against the write tx's post-write
state; budget-armed; DETERMINISTIC WITNESSES — name order, sorted and
deduped, WITNESS_CAP shown with the total always reported;
creation-over-existing-data refused with witnesses; and the NAMED
LIMITATION v1 — bodies checked at cur_vld only, a future-validity
violation "is NOT caught at commit — there is no later transaction to
re-check it... This boundary is stated, not silently assumed"),
WITNESS_CAP=8 (smallest-in-value-order, deterministic) and
MAX_COMMIT_ATTEMPTS, SEVEN typed refusals (`ConstraintViolation`
spanned+witnessed with whole-abort help; `ConstraintRejectedOnCreation`;
`ConstraintNotPure`; `ConstraintReadsNothing` — "refused rather than
admitted as dead law"; `ConstraintOnTempRelation`;
`ConstraintNameTaken` — one global namespace; `NoSuchConstraint`),
`validate_constraint_purity` (mutating bodies, :assert, :limit/:offset
— ":limit 0 would silently hide every violation", and :timeout/:sleep
— the :timeout refusal closing a HOSTILE-REVIEW PANIC VECTOR: an
unbounded value overflows Duration::from_secs_f64 in build_budget),
`stored_read_set` (positional, named-field, search atoms through
negation/conjunction/disjunction, and fixed-rule stored args),
`eval_constraint_body` (Segments::OFF — "constraint bodies read the
WRITE tx's post-write view; committed-state segments must never serve
them"; sorted+deduped), `enforce_constraints` (the DEFENSIVE purity
re-check — "the catalog row's bytes are a claim, not a proof; a
tampered body must not mutate"), `sys_create_constraint` (purity →
read-set → temp refusal → global-name scan → L4 full-state evaluation
inside the creating transaction → Protected-rung check → mirrored
name-sorted attach, under retry_on_conflict), `sys_remove_constraint`
(the SAME Protected rung — "::set_access_level r read_only would
become a backdoor to lifting a denial that the relation's writers
still rely on"), `sys_list_constraints`, and eleven tests: the CHECK
end-to-end tripwire (whole-transaction rollback incl. the co-inserted
good row, with the exact sabotages the test catches named in its
doc); FK BOTH directions through the mirroring; the creation-refusal/
repair/create/drop lifecycle; constraint × trigger ATOMIC abort (the
user's write and the trigger's roll back together); the
budget-exceeding refusal naming the constraint; witness determinism
across 1/2/4 threads AND both storage backends (cap, total,
sorted-smallest pinned); the seven-refusal creation battery with
exactly-one-attachment-survives; destroy/rename/:replace refused
while constrained (the PARENT read-set participant held too, via
RelationHasConstraints); drop-requires-the-same-rung (hostile-review
finding: "the drop path once ran with no access check" — the
read_only backdoor closed, refused-drop-detaches-nothing asserted);
the trigger cascade running past depth 1 with the cycle hitting the
TYPED depth-32 ceiling and aborting whole; and the uniqueness shape —
closed)
- **L1:** preserve-and-move whole → `session/constraint.rs` (seat
  exists: "integrity as denial rules with witnesses, gating
  admission" — this file IS the zone law's constraint clause,
  already satisfied: refusals are values naming the constraint and
  the offending rows, never error strings).
- **L2:** gold, preserve verbatim: denial-rules-as-one-species (FK/
  CHECK/unique unified); the mirroring design with its
  both-directions rationale; the stated-not-assumed v1 time
  limitation; witness determinism as product law (sorted, capped,
  totaled, thread- and backend-invariant); claims-not-proofs
  defensive re-checking of catalog bytes; the same-rung drop gate
  with its backdoor argument; purity refusals that double as panic-
  vector closures; L4 creation-over-violating refusal. Nothing
  condemned. Carried v1 obligation: the cur_vld-only check is a
  stated boundary — the target zone law should carry it forward
  explicitly until a cross-time enforcement story lands.

## runtime/verify.rs (1162 lines; inventory: MPL header, module doc
(`::verify`, story #80 — "the self-adversary primitive... 'no competing
database ships its own adversary'": one query through the production
evaluator AND the sealed oracle against ONE shared SSI snapshot; the
SCOPE OF THIS CUT stated plainly — the translator covers plain
relational Datalog + point-in-time `@` reads and REFUSES TYPED the
rest (fixed rules, predicate/unification atoms, index searches,
@spans/@delta with the extra-column shape named, :order/:limit/
mutations); A FINDING ALONG THE WAY, "named rather than routed
around" — the wildcard-in-negation safety-notion gap where the
oracle's check_safety is narrower than production's, isolated in every
test by binding negated variables positively first and left for the
refusal-fence work to characterize; THE SNAPSHOT ADAPTER — one ReadTx
for both evaluators through the SAME scan/decode primitives production
uses, "so 'byte-identical state' is structural, not a hope", with the
honest consequence stated: "verify's temporal independence lives in
the EVALUATION... a bug in decode_raw_version itself would be shared
by both sides and could escape this check"), `VerifyOutcome` ("never a
bare bool": Match, Mismatch carrying the reproduction script text and
BOTH answer sets, Unsupported, OracleRefused — "a genuine finding
about the QUERY... not evidence of an engine defect") +
`into_named_rows` (the one-row status/summary/detail product surface),
`intern` (leak-intern deduplicated by content, bounded by catalog
vocabulary — with the DESIGN DEBT NAMED PLAINLY: "this is a bridge,
not the honest end state... the honest long-term fix is laws.rs's
Rel/Term::Var owning their strings... tracked as follow-up work
rather than silently left undocumented"), the translator
(`to_oracle_asof` applying laws.rs's proven exact correspondence;
`translate_atom` per variant with each refusal REASONED — "@spans
binds an extra column beyond the relation's own arity, a distinct
translator shape"; `translate` with the
all-defs-regardless-of-reachability harmlessness argument and the
facts-XOR-histories retain), the two scanners (`scan_edb_facts`
through the production `StoredWithValidityRA` operator — "not a
second, bespoke decode path"; `scan_full_histories` through
`decode_raw_version` — "reused, not re-derived, so a bitemporal-tail
decoding bug is shared rather than independently risked twice"),
`oracle_budget` (defaults IMPORTED from db.rs's shared constants "so
the oracle path can never silently drift from build_budget's — the
exact divergence that once left this path's derived-tuple ceiling
unbounded"; no kill flag, reasoned), the three entry points
(`verify_script`, `verify_input_program` for SysOp::Verify, and the
shared `verify_program` core with the SABOTAGE HOOK — "production
always sees the real, unsabotaged snapshot — only the oracle's copy
is perturbed"), and twelve tests: Match on real recursion; THE
SABOTAGE PROOF (an edge dropped from the oracle's view only must
surface as a faithful Mismatch, never silent agreement); the
predicate refusal BY NAME; the `::verify { }` directive end-to-end
through run_script (grammar + dispatch); the directive naming
unsupported constructs; the WHOLE-CORPUS proof (40 seeds through
gauntlet's OWN generator — "reused, not re-derived" — every accepted
query Matches, with the generator's aggregation gap NAMED and closed
by the hand-written aggregation test); the REFUSAL-CORPUS proof
(unstratifiable_corpus never Matches — production refuses first or
the outcome is named, with the fixed-rule skip documented); the
point-in-time historical read at three instants incl. empty-not-
refused; the negated historical read with its positive-binding
isolation note; @spans refused by name; the starved-ceiling
propagation (ONE caller ceiling bounds BOTH evaluators by design,
with the oracle-alone case delegated to laws.rs's own tests where
"production's independent budget cannot confound it"); the generous-
budget Match; and the budget-default REGRESSION asserted at
construction ("rather than by tripping the 50M ceiling end to end,
which would cost seconds and gigabytes for no extra coverage of THIS
guarantee") — closed)
- **L1:** preserve-and-move whole → `session/verify.rs` (seat exists:
  "the ::verify door: the engine summons its judge (kyzo-oracle)" —
  and the zone law's clause "::verify summons the oracle crate; it
  never reimplements any semantics" is already this file's design).
  The crate split RESOLVES the named intern debt: kyzo-oracle's
  program model owning its strings (laws.rs's entry's arrival
  question) deletes the leak-bridge entirely — the two follow-ups
  are one work item.
- **L2:** gold, preserve verbatim: never-a-bare-bool outcomes with
  reproduction bundles; the sabotage-hook pattern (the comparison
  proves its own eyes); structural byte-identical state via shared
  scan primitives, WITH the shared-bug consequence honestly stated;
  refusals reasoned per construct; findings named rather than routed
  around (the safety-notion gap); defaults imported-never-redeclared
  with the drift lineage; corpus reuse over second corpora; the
  cheap-assertion-over-expensive-e2e regression judgment. Nothing
  condemned.

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
- **L2:** gold, preserve verbatim: knowledge-not-authority; the
  closed SystemKey with the STORAGE_VERSION merge record; the sealed
  one-door serialization boundary WITH its compile-time absence
  proof (the house pattern for two-format discipline); Ord-IS-the-
  semantics on the access ladder; uniqueness-is-isolation's-theorem;
  the deleted-amend_key_prefix provenance argument; migration records
  written where the format changed; the pinned-bytes conversation-
  starter; refused-rather-than-routed seams; the deliberately
  ungated access setter with its reason. Nothing condemned. The two
  fixes-on-port are silent-wrong-answer classes upstream shipped —
  keep their pins forever.

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
- **L2:** gold, preserve verbatim: the unconditional SSI probe with
  its lost-update argument (deleting it is a silent-wrong-answer
  class); the snapshot-monotone valid-default reasoning; resolved-at-
  this-write's-own-valid discipline on all three mutation kinds;
  retraction-is-revision; the bounded cascade as typed whole-abort;
  the temporal single-fire ruling WITH its no-byte-test-can-guard-it
  epistemics and the count-oracle guard; backfill-equals-incremental
  as the rebuildability law; the meaning-anchored byte fingerprint
  pattern; refusal-at-first-touch manifest contexts; coverage-gap
  pins named by the branch that never ran. Nothing condemned.
  Carried obligations: the Phase C parsed-substances FLAG; the
  unparsed `::temporal index create` surface (the tests' own
  documented gap) — both operator-visible.

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
- **L2:** gold, preserve verbatim: the stale-comment mea culpa as
  standing doctrine (rule #20 in its own words); the 50M ceiling's
  evidence-backed justification (rule #19 exemplary — a default
  defended by recorded benchmarks and a rejected alternative); the
  ordered commit ceremony (bumps before, evictions after, callbacks
  after durable); the one-kill-flag design; the one-machine ruling
  with its measurement; the retired-id funnel; discriminating-
  history pins over agreeable fixtures; honest reconstruction
  disclosures in tests. Nothing condemned. The `#[allow
  (clippy::collapsible_if)]` toolchain-drift note is a dated
  workaround — re-check on the next toolchain bump.

## storage/retry.rs (93 lines; inventory: MPL header, module doc
("under SSI, a conflicted transaction is not a failure — it is an
instruction to rerun. This is the single place that instruction is
obeyed"), `retry_on_conflict` (HOT retry with its audience named:
"for harnesses whose conflicts are injected or simulated (the DST
campaigns): pausing there wastes wall-clock on races that virtual
time already resolves"; the complete-transaction-cycle contract;
non-conflict errors propagate untouched), `retry_on_conflict_with_
backoff` (the fairness argument — "hot-spinning loses fairness under
real contention... the backoff de-synchronizes the herd so every
writer eventually lands"; "timing is the only thing affected; answers
never depend on it"), and `backoff` (yield ×3 then 1–64ms doubling;
the wasm no-op with its one-line reason "single-threaded wasm has no
rival to wait out") — closed)
- **L1:** preserve-and-move whole → `store/retry.rs` beside the
  storage contract (the liveness half of the SSI contract the map's
  store/contract.rs states; zones are stable, files grow — the name
  is already the concept).
- **L2:** gold: the instruction-to-rerun framing; hot-vs-backoff
  split by AUDIENCE with each variant's reason; the
  determinism-unaffected note. Nothing condemned.

## storage/verify.rs (224 lines; inventory: MPL header, module doc
(integrity verification — "a suspect store gets a REPORT, not a chain
of mystery query failures — corruption is diagnosed, never
discovered"; CATALOG-AWARE with the three value formats enumerated
and the stakes named: "a partial verifier reported as 'storage
verification' is worse than none"), MAX_RECORDED=100 (the report
"proves and locates corruption without itself growing unboundedly"),
`CorruptEntry`/`VerifyReport` (+is_clean — clean iff nothing found,
truncation counts as not-clean), `hex_prefix`, `RelKind` (the TYPED
dispatch — "the verifier dispatches by match on this kind, never by a
string test or set membership at the decode site"),
`verify_catalog_entry` (decoding the catalog row IS the verification
AND builds the taxonomy; the `index_backings` string set justified as
a catalog-NAME boundary "resolved here ONCE... It never reaches the
dispatch site" — a base always sorts before its backing), and
`verify_storage` (the total walk — "corrupt pairs are recorded and
the walk continues — one bad page must not hide the rest of the
damage"; the taxonomy complete before the first non-system entry
because SYSTEM is id 0; per-pair key decode, kind-dispatched value
decode, strict ascending-order check, and dangling-data detection) —
closed)
- **L1:** preserve-and-move whole → `store/verify_walk.rs` (seat
  exists: "the whole-store invariant walk"). The catalog-awareness
  couples it to session/catalog.rs's RelationHandle decode — on the
  split the walk consumes the catalog's public decode, keeping the
  one-codec-per-kind dispatch.
- **L2:** gold: diagnosed-never-discovered; the
  partial-verifier-worse-than-none doctrine; typed-kind dispatch with
  the string boundary confined to taxonomy construction and argued;
  total-walk-past-damage; the bounded report. Nothing condemned.

## storage/backup.rs (247 lines; inventory: MPL header, module doc
(pure-Rust length-prefixed dump, magic KYZODMP2; pairs in ascending
key order "which is exactly what batch_put requires on restore"),
`DumpClockFloorViolation` ("refuse the dump outright rather than hand
a restorer a file that lies about its own floor"),
`floor_after_snapshot` (the ORDER-BY-CONSTRUCTION trick — taking the
open snapshot BY REFERENCE means "there is no way to call this before
a snapshot exists, so the snapshot-then-floor order the proof depends
on cannot be reordered by a future edit"; the full PROOF that the
floor bounds every visible stamp, with the historical
floor-before-snapshot bug spelled out — "a silent collision"; the
harmless-overshoot note), `relation_kinds`, `relation_prefix` (the
whole-store tolerance — "misreading a short or out-of-catalog-range
key as garbage must never abort the dump"),
`verify_stamp_within_floor` (the ALWAYS-ON backstop — "cheap by
construction... so it runs on every fact row unconditionally, not
just under test — converting ANY future reintroduction of the
snapshot/floor race... from a silent lost-update into a loud, typed
refusal"), `dump_storage` (the FormatVersion carried in the dump —
"a dump of one format can never silently restore into a store of
another"; SNAPSHOT FIRST, FLOOR SECOND with the pointer to the
proof), `restore_storage` (EMPTY-target requirement — "recovery is
always 'discard and re-run' — a partial restore can never be
mistaken for a complete store"; the floor raised BEFORE importing;
fsync before return), and the incremental readers
(`read_len_prefixed` — "a corrupt length prefix yields a truncation
error — never a giant pre-allocation that aborts the process") —
closed)
- **L1:** preserve-and-move whole → `store/backup.rs` (seat exists:
  "portable whole-store dump and load").
- **L2:** gold, preserve verbatim: order-by-constructor-signature
  (the by-reference snapshot argument as an un-reorderable proof
  obligation — the same pattern stamp_after_snapshot uses);
  the always-on cheap backstop doctrine; format-version-in-the-dump;
  empty-target-only restore with its discard-and-rerun recovery
  story; incremental hostile-input reading. Nothing condemned.

## storage/crash_matrix.rs (395 lines; inventory: MPL header, module
doc (story #31 phase 2 — the crash matrix over a REAL filesystem
through kyzo-crashfs, judged against SimStorage "crashed at the
analogous logical point"; the TWO crash-point classes per the design
ruling — commit-boundary EXACT triggers "anchored to durability
barriers", and compaction/segment AMBIENT sweeps for "the design
ruling's own highest-value unknown: whether a torn SEGMENT write can
surface as silently wrong bytes", with the pinned lsm-tree finding
that data blocks are checksummed LAZILY so "'opens clean' is
necessary, never sufficient"; the FALSIFICATION CLAUSE — "any
assertion failure here is a genuine engine defect, filed as an issue
rather than patched in this module (this module drives the kernel,
it does not fix it)"; SKIPPED never failed without FUSE capability),
JOURNAL_PATH (empirically verified single-journal premise),
`round_kv`/`drive_durable_rounds` (ONE generic driver over the
Storage trait — "never two hand-written copies of the same
workload"), `total_scan_set`/`oracle_after_powercut`, the
COMMIT-BOUNDARY test (the fault-free RECORDER pass learning each
round's fsync count "by OBSERVING it once, honestly — never by
guessing fjall's internals"; the strictly-increasing premise check
failing loud "not by mis-triggering later"; one independent campaign
per boundary on fresh disk; reopen BYPASSING FUSE "exactly as a real
process reopening after a crash would see the disk"; the forced
total_scan traversal), the flood fixtures (`flood_val`'s per-index
content — "never a constant fill a coincidental truncation could
still satisfy"), `run_bounded` (the found-the-hard-way FUSE-wedge
lesson: a stuck FUSE request "parks its caller in the kernel... a
state no ordinary signal — not even SIGKILL — can unwind"; the
process::exit(90) deadline "is itself the report", naming the
never-hang property story #31 polices), and the COMPACTION/SEGMENT
test (ONE simulated crash EVENT per campaign — the earlier
ambient-rate draft REJECTED with its diagnosis: "a real crash
corrupts what was in flight at ONE instant, never a sustained
fraction of every write"; per-segment-file independent trigger
counters; the unconditional invariants — never a panic nor a hang,
opens-or-refuses-TYPED, and "every key that IS present after reopen
holds EXACTLY the bytes it was written with"; absent-is-legitimate
for best-effort commits) — closed)
- **L1:** preserve-and-move whole → `kyzo-trials/src/crash.rs` (seat
  exists: "the crash matrix over real and fault-injected
  filesystems"). Already speaks kyzo-crashfs's public surface and the
  Storage trait; the only crate-wall touch is `SimStorage` as the
  oracle, which moves with the DST instrument (see sim.rs's entry
  when it lands).
- **L2:** gold, preserve verbatim: the falsification clause
  (drives-never-fixes); observe-once-honestly over
  internals-guessing; the rejected-draft record with its
  realistic-crash-model argument; the FUSE-wedge lesson and
  exit-as-report; opens-clean-insufficient forcing reads;
  discriminating per-index fill content; skip-never-fail on missing
  environment capability. Nothing condemned.

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
- **L2:** gold, preserve verbatim: the sealed-history discipline
  (every contract change recorded WITH its ruling and industry
  context); the snapshot-then-mint proof and its unrepresentable-by-
  signature enforcement; the concurrency economics as caller-facing
  contract prose; the bump-anyway format ruling; honest durability
  semantics on fsync failure; degenerate-range laws stated at the
  trait. Nothing condemned.

## storage/merkle.rs (599 lines; inventory: MPL header, module doc (a
COLD, deterministic Merkle state root — "a content address the whole
store can be compared by"; the scan order IS the canonical order by
the memcmp law, so "two stores with identical logical content produce
byte-identical roots regardless of the write history"; the INCREMENTAL
root out of scope BY DESIGN with both reasons on record — fjall's
serialize point "is not a hook we own", and in-keyspace tree nodes
"would make every writer conflict on shared tree keys under SSI"; the
domain-separated RFC-6962 MTH with the 0x00/0x01/0x02 tags and the
classic second-preimage argument; the streaming O(log n) accumulator
"not trusted on its own word" — cross-checked against an independent
recursive MTH), `MerkleScanExceeded` (the MANDATORY ceiling — "never a
silent truncation (a truncated scan would forge a root for content
that is not there)"), `MerkleHash`/leaf/node/empty hashes (the
key-length prefix removing the (ab,c)/(a,bc) boundary collision),
`MerkleAccumulator` (equal-size merge on push; right-to-left peak
bagging), `root_over`/`state_root` ("for a validity-keyed relation
this commits to EVERY stored version... a bitemporal commitment")/
`range_root` (degenerate = empty hash, "the same contract as
range_scan")/`relation_root` (the successor arithmetic safe under the
48-bit cap), and the test battery: GOLDEN VECTORS as literal
off-tree-computed digests — "these do NOT reference the source *_TAG
constants — a mutation to a tag must move the digest away from the
literal, and a golden that reused the const would move with it and
hide the mutation"; the boundary-ambiguity pin; streaming ≡ recursive
MTH at every size 0..40 ("a second implementation that shares no
code"); THE LOAD-BEARING content-address proof (three write
histories — one big commit, reversed uneven batches, a deterministic
shuffle — all one root); single-byte value/key flips and a dropped
pair each changing the root; reopen stability; per-relation prefix
isolation with the blind-to-sibling edit; empty roots; and the
ceiling refusal with the exact-count success — closed)
- **L1:** preserve-and-move whole → `store/merkle.rs` (seat exists:
  "the deterministic state root over the ordered keyspace (placed
  here: it is a property OF the store; diff/merge capabilities
  consume it from above)").
- **L2:** gold, preserve verbatim: cold-by-design with both rejected-
  alternative reasons recorded; refuse-never-forge on the ceiling;
  goldens-as-literals doctrine (rule #18's independently-derived
  goldens, stated with its mutation argument); the
  no-shared-code second implementation; the write-history-invariance
  proof as the module's load-bearing claim. Nothing condemned.

## storage/skip_walk.rs (618 lines; inventory: MPL header, module doc
(the bitemporal skip-scan walk — "ONE implementation, generic over the
backend's seek primitive, driving a SINGLE positioned cursor"; the
previous per-step seek_first shape's fjall cost story — a fresh range
"re-derives the whole read path from scratch... ALL repeated on every
single version step"; the resolution kernel "never reimplement[ed],
only call[ed]"; the SEEK SEAM — open_skip_cursor "runs EXACTLY ONCE
per walk", seek repositions the SAME cursor with the per-backend cost
accounting incl. temp/sim's honest no-cheaper-primitive note;
degenerate bounds never reach a cursor; the FIVE-STEP walk with the
TERMINATION GUARD stated once — the kernel's bound trusted only if
strictly advancing, the byte-successor fallback making "forward
progress unconditional on ANY stored bytes, honest or hostile",
because the failure it forecloses is "a livelock, not a crash —
worse, because nothing panics to report it"; the PER-BACKEND WIRING
incl. fjall's TrackedSeekIter SSI spans and sim's FIDELITY ruling —
collapsing DST bookkeeping into open "would silently narrow the
fault-injection and scheduling-interleaving surface... down to one
decision point per walk instead of one per version step"),
`SkipCursor` (the non-decreasing-target contract)/`OpenSkipCursor`,
`SkipWalk`, `advance_past` (pulled out AS ITS OWN NAMED LAW — "that
unreachability is a property of today's algebra, not a proof
obligation this driver gets to assume"), the Iterator impl (a corrupt
key surfaces `Err` WITHOUT advancing — "every subsequent poll
re-yields the error, so a scan cannot silently step over bytes it
could not judge"), and the tests: the advance_past pinned law
(tie/regression/genuine-advance/empty-key); `MapSeek` — "the proof's
own backend: nothing but a BTreeMap, ~30 lines... NO dependency on
any of the three production backends", with the open counter; the
FROM-SCRATCH oracle ("an oracle that shares the kernel's algorithm
proves nothing about the kernel"); THE THEOREM —
`skip_walk_matches_independent_oracle_over_2000_seeded_histories`
(both axes, negative coordinates, all three polarities; issue #78's
dictation: "this driver IS #79's first theorem... every backend
inherits it verbatim"); corruption refusal over four hostile
scenarios with the re-yield pinned; the i64::MIN boundary
termination; degenerate-bounds emptiness; and
`skip_walk_opens_exactly_one_cursor_per_walk` — the story's own law,
structural AND counter-pinned across 100 facts × 10 versions —
closed)
- **L1:** preserve-and-move whole → `store/skip_walk.rs` (seat
  exists, same name: "the ONE bitemporal skip-scan walk, generic
  over its driver"). The per-backend cursor impls stay with their
  backends (fjall.rs, scratch.rs, and the sim instrument).
- **L2:** gold, preserve verbatim: one-walk-one-cursor as the
  structural law with its counter pin; the termination guard's
  belt-and-braces doctrine and the livelock-worse-than-crash
  reasoning; error-without-advancing; the standalone ~30-line proof
  backend (properties proven against the seam, not a production
  backend); the sim fidelity ruling; laws extracted into named,
  directly-tested functions. Nothing condemned.

## storage/conformance.rs (646 lines; inventory: MPL header, module doc
(the storage contract AS A CONFORMANCE KIT, story #79 — "one battery of
generic properties, quantified over `S: Storage`... so a new backend
passes exactly the same torture the fjall backend does — by CALLING
this module, not by a maintainer hand-copying fjall's test file and
swapping type names"; the maintainer-ratified THREE-ARM SCOPE — contract
laws, DST fault campaigns reusing SimStorage's seeded controls so "the
property that certifies quiescent correctness is the property the fault
campaign tortures — one definition, not two", and cross-backend
differentials generalizing temp.rs's hand-rolled three-way; the
EXPLICITLY-OUT list with reasons — per-backend time-travel re-proofs
would duplicate the skip-walk driver's ONE generic proof, backup/clock
floor are a separate surface; the whole-integration adoption recipe),
the eight contract laws (`law_send_sync_bounds_are_compiler_checked` —
"compiler > constructor > test", failing is a compile error;
`law_kv_matches_model_oracle` — mixed workload vs a BTreeMap model,
full and bounded scans; `law_mvcc_first_committer_wins` with
abort-leaves-no-trace; `law_read_your_own_writes_and_snapshot_isolation`;
`law_del_range_kills_own_writes`; `law_phantom_protection` — a scanned
range is conflict-tracked as a whole; `law_concurrent_writers_across_
threads` — 8 real threads, disjoint keys never conflict, a contended
RMW counter lands every increment exactly once;
`law_del_range_chunk_boundaries` — probed at 1023/1024/1025/2048 so "a
backend with a differently-sized chunk still gets an off-by-one probe
near its own boundary"), `run_full_battery`, the differential arm
(quantified over bare `WriteTx` SPECIES not Storage backends — TempTx
belongs "exactly as it belongs in temp.rs's own three-way check"; every
method used lacks Self: Sized so dyn WriteTx is legal, commit never
called mid-differential; `Obs` normalized so error MESSAGES differing
still compare equal on error PRESENCE; `Op`; SCAN_CAP=10_000 — "proof
that the iterator terminates rather than merely a slow assertion";
`apply_op`; `gen_key` over an alphabet straddling type-tag boundaries
incl. the empty key; gen_val/gen_op; `assert_ops_agree` naming the
first disagreeing pair at its step), the DST arm
(`read_total_scan_retrying` — reads transient BY DOCTRINE, a bare
unwrap "would fail the campaign on an injected fault that has nothing
to do with a correctness bug", the bounded retry "itself the liveness
half of the same retry doctrine retry.rs documents";
`dst_fault_campaign_kv_survives_crash` — the KV-vs-model law driven as
a fault-campaign payload, after EVERY reopen contents equal exactly
the committed (crash) or fsynced (power cut) prefix, "never more,
never less, never a panic", with the powercut-rewinds-committed
bookkeeping), and the five tests: fjall's one-call conformance (with
the leaked-TempDir scaffolding ruling written in place);
`sim_passes_the_full_battery` — "proof the battery is backend-agnostic,
not fjall-shaped in disguise"; the three-species differential
(fjall/sim/temp × 12 seeds × 120 ops, empty-key puts filtered with the
8-byte-prefix reason); the 60-seed fault campaign carrying the
kyzodb/kyzo#91 FIX DOC (sim_crash conflated the synced watermark with
commit_seq — "a power cut simulated after an intervening crash wrongly
retained buffer-tier data that was never fsynced", first observed at
seed 18, reopen now takes the two seqs separately); and the minimal
seed-free #91 pin (crash-then-powercut must still drop the
never-synced write) — closed)
- **L1:** preserve-and-move whole → `kyzo-trials/src/conformance.rs`
  (seat exists: "the storage-contract kit any backend must pass
  (public)"). REWIRE at the crate wall, the kit's own kind: today
  everything is pub(crate) over traits SEALED to kyzo-core — the map
  makes the kit PUBLIC in trials, so the sealed contract must expose
  what a generic battery needs (the Storage/ReadTx/WriteTx surface, or
  a sanctioned trials seam); `SimStorage`/`for_each_seed`/`SimRng` are
  kyzo-crashfs vocabulary on arrival (trials lawfully depends on it),
  and the temp species in the differential follows scratch.rs's seat.
  The #91 fix doc travels with the campaign; the storage/mod.rs entry
  already routed this file here.
- **L2:** gold, preserve verbatim: call-the-kit-don't-copy-the-file as
  the anti-drift design (conformance by invocation, story #79);
  torture-what-you-certify (the DST arm driving the SAME generic law,
  one definition); the explicitly-out list with each exclusion's
  reason; species-not-backends quantification for the differential
  (dyn WriteTx with the Self: Sized accounting written down);
  error-presence-normalized observations; SCAN_CAP as a termination
  proof; the chunk-boundary off-by-one probe design; reads-transient-
  by-doctrine retry with its liveness framing; the #91 lineage pinned
  both seeded and seed-free. Nothing condemned. Arrival note: `Obs`
  and the law assertions speak fjall's `Slice` as their observation
  currency — a backend-agnostic kit should observe in the CONTRACT's
  own byte vocabulary, not one backend's refcount type; resolve when
  the sealed contract names its public read currency.

## storage/temp.rs (770 lines; inventory: dual fork header (the original
was a `Storage` BACKEND — `Arc<MemStorage>` shared by every session over
a `ShardedLock<BTreeMap>`; here a plain ReadTx/WriteTx SPECIES owned
inline by a SessionTx — "no lock, no sharing, no Arc — a session is
single-threaded by construction, so the interior synchronization had
nothing left to guard"; the kernel contract implemented directly so the
catalog and scan surface route unchanged; law-5 skip scan sharing the
fjall seek loop's strict-advance guarantee; eager del_range replacing
the original's deferred bounds; the WHAT-THIS-SPECIES-IS-THIS-TIER
section "verified, not aspirational" — proven at the storage-species
level by three-way differential + mutation-confirmed oracles, and NOT
reachable from the public API: "every route in is a TYPED REFUSAL, not
a silent misplacement", the temp-mutation and temp-read refusals each
named with their exact error and verified end-to-end; "the file ships
sealed so the router can adopt it later without a format migration,
not because the feature is live"; the one-line sealed-module landing
note), module doc ("a temp relation is a fact a session is
ENTERTAINING, not one the universe has committed to"), `TempTx`
(BTreeMap + a logical-time stamp from a process-wide monotone counter —
"stamps need no wall-clock meaning, and logical time keeps runs
deterministic"; commit vacuous — "the session's life is the
transaction"), the ReadTx impl (degenerate bounds empty BY GUARD —
BTreeMap::range would panic on start>end; skip scan inherited WHOLE
from skip_walk — "this backend contributes only the OpenSkipCursor impl
below... never the walk itself"), `TempSkipCursor` + the
OpenSkipCursor impl (the walk's own loop guard makes every range call
well-formed by construction — the no-check argument written at the
cursor), the WriteTx impl, and the test battery: bk/bv/rel_bounds/
scan_at_coord helpers (values ASSERTED not counted, `.take` capping so
"a mutant that emits forever fails fast rather than merely hanging");
basic KV + degenerate laws; last-write-wins (the named
entry().or_insert_with mutant); validity semantics with the
EARLIER-DRAFT HONESTY NOTE (the first version stored a shape
extend_tuple_from_v rejects and asserted a .count() "that cannot tell
an Ok tuple from a decode Err" — the corrected test asserts returned
VALUES); the INCLUSIVE re-seek pin at the exact query instant on BOTH
axes (the Included→Excluded silent-wrong-answer mutant); the
i64::MIN-adjacent termination pin; degenerate skip bounds; and the
adopted three-way differential section (temp vs fjall vs sim over
seeded op streams, 12×120 sized-to-seconds with the 60×300 review
harness credited; final-state agreement + strict memcmp-ascending
check; the DynW shim + seven del_range boundary probes incl.
degenerate-deletes-nothing; and the skip-scan differential over seven
scenarios planting HOSTILE INHABITANTS — short key, clobbered slot
tags, garbage rmp payload, missing/unknown polarity bytes, extreme
timestamps — × 8 sys × 10 valid query instants) — closed)
- **L1:** preserve-and-move whole → `store/scratch.rs` (seat exists:
  the session-scratch species). The species contract, cursor impl,
  and the targeted mutation oracles move as the file's own tests. The
  three-way differential machinery (Obs/Op/gen_*/apply) is the
  HAND-ROLLED PREDECESSOR conformance.rs's kit explicitly generalized
  — on migration the differential arm dissolves into one call to the
  kit's `assert_ops_agree`/battery (kyzo-trials/src/conformance.rs)
  with TempTx handed in as one more species, ending the duplicated
  generator/observation code (the kit's own module doc names this
  file as what it replaces). The session router that finally reaches
  the species is session/db.rs's routing seam (`is_temp` the datum,
  the session the router — runtime/relation.rs's entry carries that
  law).
- **L2:** gold, preserve verbatim: the species-not-backend reduction
  with its nothing-left-to-guard argument; sealed-not-live honesty
  (shipped for format stability, unreachability enforced by TYPED
  refusals verified end-to-end); the honorary-transaction framing;
  the logical-clock determinism choice; walk-inherited-whole with the
  cursor-contributes-only-the-seek discipline; the earlier-draft
  correction note kept in the test doc (a test that records its own
  past weakness); hostile-inhabitant differentials; mutants named at
  the test that kills them. Nothing condemned. Arrival note: the
  differential-arm dedup above is the one deliberate deletion — the
  targeted oracles (inclusive re-seek, MIN-ts termination, degenerate
  bounds, last-write-wins) stay beside the species; only the generic
  three-way plumbing yields to the kit.

## storage/fjall.rs (857 lines; inventory: dual MPL header, module doc
(the CONTRACT MAPPING clause by clause — ordered scans are the memcmp
law realized on LSM byte order; SSI with the contract-v2 note that
fjall's own oracle validates READS only, so put/del additionally mark
the written key read; RYOW by write-set overlay; the bitemporal as-of
scan as ONE positioned cursor — a Snapshot's bare SeekIter or a write
tx's SSI-tracking TrackedSeekIter with "PostgreSQL SIREAD-lock
granularity promotion"; species-as-types — "writing through a reader
and committing twice are not errors, they are programs that do not
compile"), the meta keys (SYSTEM_CLOCK_WATERMARK with the
harmless-overshoot floor doc), `StorageOptions` ("a database engine
does not get to inherit invisible defaults") + `StorageStats`, THE
TUNING MODULE (issue #118: the ONE-KEYSPACE design decision argued in
full — bitemporal suffixes make a fact's current row and history
KEY-ADJACENT, "recency is a moving read-time predicate, not a
write-time fact", so a point/temporal keyspace split would demand
"exactly the mid-flight format surgery the storage contract forbids";
the split the model wants "already exists, just one axis down: LSM
LEVELS"; `monkey_bits_per_key` — the Monkey per-level bloom allocation
with its arithmetic derivation (log2(10)≈3.3 bits/level) cited to
Dayan/Athanassoulis/Idreos; `main_keyspace_options` — the three
shallowest filter blocks PINNED, expect_point_read_hits with the
join-into-existence rationale, and the Dostoevsky-lazy-leveling
MEASUREMENT STORY: true tiered compaction is DEAD in the vendor drop
(module commented out, choose() unimplemented!), the first pass's
l0_threshold(8)+ratio+block ramp cost as-of ~10% for no measurable
ingest win, per-knob isolation placed the blame precisely, so only
l0_threshold moves by HALF the step (6) — "landing at a measured,
disclosed -6% on as-of (published as the losing run it is)" — and the
missing cumulative-bytes-compacted counter named as "a real gap, not
a hidden one"; the 4→8 KiB block ramp kept as measured-free;
`meta_keyspace_options` with the nothing-to-act-on honesty; the
LEVELS==7 const assert), `new_fjall_storage(_with)` (format version
stamped fresh / refused on mismatch; the clock seeded max(now,
watermark)), `quarter_system_ram_bytes` (the 25%-of-RAM cache floor —
"an engine that owns the box should not hand back 15/16ths of it";
/proc/meminfo because forbid(unsafe_code) rules out libc, "a named
platform gap, not a silently wrong number"), `FjallStorage`
(Clone-is-a-handle — "SSI conflict detection spans all clones and all
threads"; the `watermark_lock` HOSTILE-REVIEW FINDING — fjall resolves
the watermark key by internal commit order, decoupled from mint order,
so an unserialized mint+persist pair could leave the on-disk floor
below a used stamp and a crash re-mints it; held only around the pair,
never across snapshots or commits), `stamp_after_snapshot` (the
SIGNATURE IS THE ENFORCEMENT — taking the open snapshot by reference
makes mint-before-snapshot unrepresentable, "the reproducer catches
wide reorderings; this catches every one at compile time"), the
Storage impl (`raise_clock_floor` persisting the CLOCK's floor not the
argument — "a stale (lower) argument must not regress the disk";
`write_tx` carrying the SNAPSHOT-FIRST-MINT-SECOND proof in full with
the shadowed-forever lost-update lineage and its named pin;
`batch_put` OUTSIDE the SSI surface with its precondition REFUSED not
documented — the emptiness probe's upper bound proven at COMPILE TIME
against RelationId::CAP "so a future id-cap bump cannot silently turn
a full store invisible to this check", unmaterialized Guards costing
no decode, and 32768-row atomic chunks — "an interrupted import
leaves a clean prefix of the input rather than a torn write"),
`stats`, the two species types, `mark_written_key_validated` (the v2
mechanism: contains_key AFTER the write resolves in the own memtable
— no disk I/O — while registering the mark_read; "mutating this call
away breaks write_write_race_aborts_second_committer"), `raw_range`
(THE DEGENERATE-BOUNDS CHOKE POINT with its poisoning argument — an
inverted range replayed through BTreeSet::range at commit time panics
"inside the commit oracle, while holding the global write-serialize
lock, poisoning the whole store for every later transaction";
skipping read tracking sound because "an empty range has no phantoms
to protect against"; borrowed bounds saving two Vecs on the skip
scan's hottest path), materialize_row/materialize_key (Guard as
UNDECIDED CURRENCY — "a caller materializes as much of each row as it
actually needs... rather than this choke point deciding for
everyone"), the read helpers, `FjallSkipCursor` (Empty/Live) + the
`FjallSeekStep` unifying trait, the `impl_read_tx!` macro
instantiated for both species, and the WriteTx impl (del_range's
chunked RESUMING cursor — "no pass re-walks the tombstones of
previous passes (a naive rescan-from-lower is quadratic in range
size)"; consuming commit/commit_durable) — closed)
- **L1:** preserve-and-move whole → `store/fjall.rs` (seat exists: the
  fjall backend). Its skip cursor stays with it per skip_walk.rs's
  per-backend ruling; its conformance proof is one `run_full_battery`
  call in the trials kit; the `current_validity` clock read it takes
  at open/mint rewires to the session tier's one-clock accessor when
  runtime/mod.rs dissolves (the one-clock law's seat is session/db.rs
  — the store consumes the ambient input through its constructor,
  never minting wall-clock itself).
- **L2:** gold, preserve verbatim: the snapshot-then-mint proof at the
  write_tx site AND its compile-time signature enforcement (the house
  exemplar of order-by-construction); the watermark-lock finding with
  its crash-re-mint scenario; the v2 write-marking mechanism with its
  own-memtable cost argument and named mutant; the degenerate-bounds
  choke point's store-poisoning rationale (one guard, every path);
  the tuning module's measurement discipline — a losing run published
  with its number, blame isolated per knob, a dead vendor primitive
  named rather than wired, an observability gap disclosed (rule #19
  at its best); the one-keyspace decision argued from the sealed key
  format; refused-not-documented preconditions with compile-time
  probe bounds; Guard-as-undecided-currency; the named Linux-only
  platform gap. Nothing condemned.

## storage/sim.rs (1216 lines; inventory: dual MPL header, module doc
(DST at the storage seam — "everything — thread interleavings, injected
faults, crashes, power cuts — is a pure function of one u64 seed";
Turso/antithesis-style practice credited, "the contract it checks is
KyzoDB's own"; WHY THIS MODULE SHIPS NOWHERE — cfg(test) in
storage/mod.rs, visible to every in-crate test module, and the
test-support cargo feature REJECTED because it "would leak a fake
backend into the shipped API surface and invite depending on it"; the
DETERMINISM DOCTRINE — no wall clock, no OS randomness, no unordered
iteration; fault injection IDENTITY-KEYED (op kind + semantic content,
attempt = per-identity occurrence count) so "the plan is positional in
nothing, and the same seed yields the byte-identical fault schedule at
ANY thread count"; the attempt component as "the retry-liveness
guarantee by construction"; the crash epoch salting post-crash streams;
what is modeled — conservative SSI tracking "legal: SSI false positives
are permitted", two durability tiers, three fault arms, and the
token-barrier adversarial scheduler), `SimRng` (inline splitmix64,
"zero new dependencies"), `FaultConfig` (three ppm rates with
empty-write-set commits exempt), the salts + op-kind tags,
`op_identity` (FNV-1a, LENGTH-DELIMITED "so distinct part lists never
collide by concatenation"; identity captures WHAT, "never when it runs,
what ran before it, or which thread carries it"), `fault_hit`
(stateless finalizer over seed/identity/attempt/salt), `SimState`
(per-key VERSION CHAINS ascending by commit seq; the logical
next_system_stamp minted under the state lock; the two watermarks; the
attempts map; total_puts/total_dels — THE WRITE-COUNT LAW'S ORACLE,
counting CALLS never post-collapse entries because "the two calls are
indistinguishable on disk", observation-only — "read nowhere in
commit/conflict/fault logic, so it perturbs no behavior"),
snapshot_at/modified_since/range_modified_since/`map_range`
(inverted-bounds-safe), the SCHEDULER (thread-local participant ids;
token-barrier — "a turn is granted only when EVERY live participant is
parked... the seeded pick is the ONLY source of interleaving order",
the pre-first-yield concurrency caveat stated; `DoneGuard` on Drop so
"a panicking body cannot hang the other participants"),
`run_interleaved`, `for_each_seed` (the FAILING-SEED stamp — "rerun
this test with the loop pinned to seed N to replay the schedule and
fault plan exactly"), `SimCtx` (epoch-salted fault_seed with epoch-0
untouched; yield_turn no-op off-scheduler; `roll_fault` — counters
advance EVEN AT RATE 0 "so attempt numbering is identical between
fault-free and faulted runs"), `SimStorage` (new/with_faults;
`reopen` carrying the kyzodb/kyzo#91 SIGNATURE FIX doc — commit_seq
and synced_seq "separate parameters, never one shared seq collapsed
into both", the prior single-seq shape's bug and its seed-18 catch
named; `sim_crash` — synced watermark carries UNCHANGED, "a process
crash neither advances nor loses which prefix was ever fsynced";
`sim_powercut` — filter to the fsynced prefix, with the HONESTY
CAVEAT: "this models the contract's guaranteed FLOOR... a property of
the simulation, not a promise about real hardware"; the
put/del_call_count accessors), the Storage impl (materialized-snapshot
read_tx — "simple and obviously correct beats clever"; write_tx
minting under the lock; `batch_put` with the LIVE-KEYS-ONLY emptiness
oracle — "tombstones must not make the test double stricter than
reality" — and chunk size DELIBERATELY 1024 vs fjall's 32768 "so
chunk-boundary behavior is testable without 100k-row fixtures; the
clean-prefix LAW is the shared contract, the chunk size is not"),
`SimReadTx`/`ReadSet`/`SimWriteTx` (reads behind a Mutex because the
trait reads through &self and requires Sync; per-tx call counters
"never collapsed to writes.len()'s one entry"), `visible_lazy` (THE
O(n²)→O(n) FIX with its lineage: the eager clone-the-range build under
the skip walk's per-seek reopen cost quadratic time ON A REAL PATH —
"runtime/db.rs routes every write-lock script's WHOLE query body
through a SimWriteTx"; the sorted two-cursor merge with
tombstone/shadow semantics written arm by arm), eager `visible` kept
only for del_range (must collect before mutating), track_key/
track_range, `commit_inner` (fault identities FIXED BEFORE ANY DICE
ROLL; commit identity = the write-set KEYS not values so "a retry that
recomputes new values over the same keys is still the same logical
commit, on its next attempt"; empty write set vacuous matching fjall
— though commit_durable's fsync still runs; spurious conflicts that
"cannot be pinned to a permanent conflict"; real SSI validation over
reads AND writes per contract v2; the call counters folded in ON THE
SUCCESS PATH ONLY — "an aborted commit's calls never reached real
storage, so they must never reach the running total either"; injected
fsync failure leaving the commit "applied, not power-cut durable"),
the two skip cursors (per-seek yield + fault roll — THE FIDELITY
RULING quoted at skip_walk.rs's entry lives here: collapsing into
open_skip_cursor "would silently narrow the fault surface the sim
exists to stress down to one decision per walk"; the write cursor
adding per-step conservative range tracking over visible_lazy), the
two ReadTx impls (lazy range_scan with the predecessor's quadratic
note; whole-range conservative tracking), and the WriteTx impl
(del_range counting each doomed key as its own del CALL "same as a
caller looping del") — closed)
- **L1:** preserve-and-move whole; PROPOSED SEAT (operator ratification
  required): `kyzo-crashfs/src/sim.rs` (NEW-SEAT) — the DST-instrument
  crate the census has already leaned on twice: dst_query.rs's entry
  states "SimStorage itself is kyzo-crashfs vocabulary in the target,
  which trials lawfully depends on", and crash_matrix.rs's oracle
  "moves with the DST instrument". The physics forcing a move at all:
  once conformance/dst/crash live in kyzo-trials, a cfg(test) module
  inside kyzo-core is unreachable across the crate wall — the double
  must live where trials and kyzo-core's own tests (the write-count
  law's put_call_count oracle at session/admit) can both reach it.
  ARRIVAL QUESTION FOR THE OPERATOR, same family as conformance.rs's:
  SimStorage implements the SEALED Storage/ReadTx/WriteTx traits — a
  cross-crate double requires the seal to admit it (a named grant in
  store/contract.rs, mirroring today's in-crate "the contract's own
  test double... not a second backend" decree) or the alternative
  seat is trials-owned; whichever way, the module-doc ruling that a
  shipped-API fake backend is refused must survive the move (crashfs
  and trials are both instrument crates, so the refusal holds under
  either answer).
- **L2:** gold, preserve verbatim: identity-keyed fault planning with
  the attempt counter as retry-liveness BY CONSTRUCTION (the sharpest
  determinism design in the tree — positional in nothing); crash-epoch
  salting with epoch-0 transparency; the token-barrier scheduler and
  the DoneGuard no-hang discipline; failing-seed-stamped campaign
  reports; the write-count oracle's three disciplines (calls not
  entries, observation-only, success-path-only); the #91 two-parameter
  reopen signature (an API shape that makes the caught bug
  unrepresentable); the honest power-cut FLOOR caveat; the
  test-double-no-stricter-than-reality batch_put oracle; law-vs-knob
  separation on chunk sizes; the visible_lazy quadratic fix with its
  real-path lineage recorded; identities-before-dice and
  keys-not-values commit identity; the per-seek fidelity ruling.
  DEFECT (doc-grade, fix at migration): a misattached doc comment —
  the "Simulated power cut" prose block INCLUDING the floor caveat is
  attached to `put_call_count` (the write-count paragraph then flows
  into it), while `sim_powercut` itself carries only a dangling
  `[sim_crash]:` link-definition line as its entire doc; rustdoc
  renders both functions wrong. Re-seat the comments on their owners
  when the file moves. Nothing else condemned.

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
  corpus doubles as fuzz-seed material for kyzo-trials/fuzz.rs). The
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
  process-abort crash test → `kyzo-trials/src/crash.rs` (real-process
  territory, same lane as the FUSE matrix). The DST semantics tests +
  seven campaigns + hardening sentinel + retry-liveness → move with
  the sim instrument to its ratified seat (they are the instrument's
  own proof battery), with the campaign SHAPES feeding
  kyzo-trials/src/dst.rs. CONDEMNED AS SUPERSEDED: the generic
  KV/MVCC/RYOW/del_range/phantom/concurrent-writers/chunk-boundary/
  send-sync scenarios here are the story-#79 kit's EXTRACTION SOURCE
  and are now verbatim-duplicated by conformance.rs's generic laws,
  which fjall and sim already pass via `run_full_battery` — on
  migration these per-backend copies die in favor of the kit call,
  keeping only what the kit deliberately excludes (the per-backend
  pins above).
- **L2:** gold, preserve verbatim: laws-not-scenarios with the
  invisible-to-examples argument; exhaustive-pairwise order checking;
  generators justified by the exact mutant/bug shape they catch (the
  shared-boundary arm, the byte-flip harness, the hardening
  sentinel's positional-plan kill, the retry-liveness mutant); scope
  honesty on what a test does NOT simulate (the abort-vs-power-cut
  paragraph); independently-re-parsed artifacts (the dump-file
  concurrency pin reads bytes, not the writer's own values);
  commit-order-observed campaign dispatch with per-branch assertions
  (write skew's serial-outcomes check, the lost-phantom order match);
  sabotage-verify with a legitimate control; the
  abandoned-mint-raises-floor ruling; real-fixture-over-raw-puts for
  catalog-aware verification; walk-continues-past-the-wound;
  re-pinned-KNOWINGLY contract history in the test doc; sane-band
  assertions over host-pinned values. The one condemned class is the
  superseded kit-source duplicates named in L1; everything else
  crosses.

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
- **L2:** gold, preserve verbatim: the arity brand as a universal
  contract ("refused at the first wrong row instead of feeding
  mis-shaped tuples into downstream joins"); the OutputSpendGuard's
  hole analysis and its determinism/boundedness laws (the mid-epoch
  guard doctrine extended to the one place it didn't reach); the
  checked-at-the-intern-site fix with its reserved-sentinel cap and
  factored-for-testability honesty; the systemic-finding test's
  design discipline (every case complete except the guard under
  test — failure isolation built into the fixture); the
  superseded-placeholder-kept-for-its-regression pattern; the
  cancellation polling doctrine; dead code identified-and-dropped in
  the header ledger; seams declared with named landing sites so "this
  draft must not reshape a landed file". Nothing condemned.

## fixed_rule/graph.rs (251 lines; inventory: MPL header, module doc
(new in KyzoDB — the CozoDB original used the external `graph` crate
v0.3.1, "effectively dormant... drags a ~20-crate subtree (`rayon`
mandatorily, `dashmap`, `memmap2`, `page_size`→`libc`...) plus heavy
`unsafe`, for what the algorithms actually consume: CSR construction
from an edge list and neighbor iteration. This file replaces exactly
that surface, dependency-free and safe"; the BEHAVIORAL PARITY NOTES
verified against the original's usage — node count from max endpoint
+1 with the +1 CHECKED where the original overflowed, sorted adjacency
with parallel edges kept, both out- AND in-adjacency because
"in_neighbors is load-bearing for PageRank", the by-value yield
adjustment named as mechanical), `GraphTooLargeError` (u32::MAX
reserved as the Dijkstra sentinel; the HONESTY NOTE on testability —
"hitting this bound for real needs ~4 billion distinct nodes... the
guard is pure arithmetic, factored... precisely so unit tests can pin
the boundary math without the allocation"), `checked_node_count`,
`Target<W>` (field names mirroring the original crate "so the
algorithm bodies read unchanged"), `DirectedCsrGraph<W>` (counting-
sort CSR build with per-node write cursors and the fills-every-slot
argument; segment sort for CsrLayout::Sorted parity; node_count/
out_degree/out_neighbors/`out_neighbor` — the O(1) cursor accessor
added FOR the iterative Tarjan "where repeatedly re-scanning the
adjacency with nth would be quadratic in degree"/
out_neighbors_with_values/in_neighbors), and tests (shape + iteration
incl. parallel edges and in-adjacency; empty graph; the F3 boundary
pin at None/0/u32::MAX−1/u32::MAX plus the same refusal surfacing
through from_edges) — closed)
- **L1:** preserve-and-move whole → `rules/graph_view.rs` (seat
  exists: "the graph the algorithms run on (backed by projections)").
  The payload's interning builders arrive beside it per mod.rs's
  split. Arrival note: the seat's "(backed by projections)" names a
  FUTURE the file doesn't yet have — today's view materializes from
  an edge relation per invocation; the project/graph CSR ("the
  resident canonical CSR for traversal") is the declared backing when
  it lands, and this file's build-from-edges remains the fallback for
  ad-hoc relations.
- **L2:** gold, preserve verbatim: the dependency-replacement record
  (what the crate dragged, what was actually consumed, and the
  parity notes verified point by point — the house form for
  replacing a dependency); checked-never-wrapping node counts with
  the reserved-sentinel rationale; factored-for-testability honesty;
  an accessor added with its complexity argument (out_neighbor's
  quadratic-nth note). Nothing condemned.

## fixed_rule/parallel.rs (108 lines; inventory: MPL header, module doc
(the parallelism seam — upstream ran fan-outs under rayon, the port
left them sequential behind SEAM markers while the workspace carried
no rayon; eval.rs's direct rayon dependency "closed" the seam; THE ONE
LAW: "parallel execution must produce byte-identical output to the
sequential path" — par_try_map is order-preserving BY CONSTRUCTION,
and cross-item float reductions "stay in a sequential fold the caller
runs over the returned, canonically ordered Vec; they are never
handed to a parallel reduction"), the native `par_try_map`
(rayon collect is index-preserving — "the output order equals the
input order regardless of how work is scheduled"), the wasm32 twin
(sequential, "callers need not know which one they got"), and tests
(the LOAD-BEARING order property; single-thread pool vs default pool
byte-agreement; error short-circuit) — closed)
- **L1:** preserve-and-move → `rules/contract.rs` as the determinism
  law's tooling (the contract line "what a fixed rule promises:
  determinism" is exactly what this function enforces under
  fan-out); a standalone `rules/parallel.rs` is the lawful
  alternative if the operator prefers the file kept whole — zones
  are stable, files grow.
- **L2:** gold: the one-law framing with the float-reduction
  boundary drawn explicitly (what may parallelize and what must
  not, stated where the tool is defined); the wasm degradation as
  an indistinguishable twin; order-preservation tested as THE
  property, not incidentally. Nothing condemned.

## fixed_rule/rng.rs (175 lines; inventory: MPL header, module doc
(upstream's LabelPropagation and RandomWalk seeded from OS entropy —
"the SAME facts and query could answer differently run to run — a
direct violation of the determinism guarantee"; the fix: an explicit
`seed` option with a pinned default, drawing from `SeededRng` — the
storage/sim.rs SimRng HOUSE PATTERN wrapped as `rand::RngCore` so the
algorithms keep Fisher–Yates shuffle/choose/WeightedIndex unchanged,
"only the entropy source moves"; portability stated — no
platform-dependent word size or endianness in the math),
`DEFAULT_SEED` (arbitrary but PINNED, with its guard chain named:
golden_stream asserts the literal stream, random_walk.rs pins a
default-seed output row), `new` (negative i64 seeds wrap
two's-complement — "every distinct i64 still maps to a distinct,
reproducible stream, which is all the determinism law needs"),
`step` (mirrors SimRng), the RngCore impl (next_u32 from the high
half "the finalizer already diffuses the whole word"; fill_bytes
with the remainder path), and tests (THE GOLDEN VECTOR — literal
words "computed offline from the splitmix64 definition, independent
of this implementation", with the epistemics stated: "a
seed_determines_stream / round-trip test cannot catch such a drift —
both sides move together"; negative-seed wrap; seed determinism;
rand-consumer reproducibility; fill_bytes remainder determinism) —
closed)
- **L1:** preserve-and-move whole → `rules/rng.rs` (seat exists,
  same name: "the seed-reproducible randomness every stochastic rule
  uses").
- **L2:** gold, preserve verbatim: the determinism-violation lineage
  (a fork fix argued from the engine's own guarantee); the
  golden-vector epistemics (why the literal is the guard and a
  round-trip is not — rule #18's independently-derived goldens
  stated in miniature); the negative-seed wrapping ruling; the
  house-pattern reuse note tying it to SimRng. Nothing condemned.

## fixed_rule/algos/mod.rs (61 lines; inventory: dual header (the
`graph-algo` feature gate gone — "the ported algorithms are
dependency-free pure Rust and always compiled"), module doc (the
built-in taxonomy: traversals, shortest paths, centralities,
components, community structure, spanning trees, random walks), 19
module decls and their 20 re-exports — closed)
- **L1:** structural glue — dies with the directory when algos/
  becomes the map's `rules/algo/` ("one algorithm per file, named for
  the algorithm"); the re-export surface re-forms as that module's
  root, feeding the registry at rules/contract.rs.
- **L2:** nothing condemned beyond the glue itself; the taxonomy
  paragraph makes a good algo/ module-root doc.

## fixed_rule/algos/degree_centrality.rs (139 lines; inventory: dual
header (arity-checked writer; otherwise unchanged), module doc
(degrees straight off the edge tuples, no graph materialization), the
`DegreeCentrality` rule (one BTreeMap counting (total, out, in) per
endpoint; per-tuple cancel poll; the OPTIONAL nodes relation carrying
the systemic-finding guard with its ruling written in place — "a
missing (unbound) nodes relation is the 'not provided' case... a
PROVIDED nullary relation is a real error, not something to silently
ignore"; arity 4), and the VALUE-ORACLE test (exact hand-counted
degrees incl. an isolated node from the optional relation) — closed)
- **L1:** preserve-and-move whole → `rules/algo/degree_centrality.rs`.
- **L2:** gold: the missing-vs-nullary distinction argued at the site;
  hand-counted value oracle. Nothing condemned.

## fixed_rule/algos/top_sort.rs (176 lines; inventory: dual header
(inline CSR; arity-checked writer; and the PRESERVED BEHAVIOR NOTE:
"on a cyclic graph Kahn's algorithm silently omits the nodes of the
cycles from the output rather than erroring"), the `TopSort` rule
(arity 2: position, node), `kahn_g` (in-degree pass, LIFO ready
stack, per-pop cancel poll), and two tests: the diamond sanity check
and the VALUE ORACLE pinning the exact order "modulo the documented
tie rule" — the hand trace written out, ending in the mutant it
kills ("reversing the adjacency segments would... give a, b, c, d
instead, so this kills the reversed-CSR-sort mutant") — closed)
- **L1:** preserve-and-move whole → `rules/algo/top_sort.rs`.
- **L2:** gold: the exact-order pin with its hand trace and named
  mutant. FLAG for the destination law (operator-visible, carried
  from the header): silent cycle omission is preserved upstream
  behavior — a fixed rule that silently returns a PARTIAL answer on
  cyclic input sits uneasily beside the engine's refuse-or-answer
  doctrine; the reforge decision (typed refusal on cycles vs the
  documented omission) belongs to the rules-zone law when the file
  arrives, not to this census. Nothing else condemned.

## fixed_rule/algos/triangles.rs (205 lines; inventory: dual header
(inline CSR; SEAM(parallelism) CLOSED with its determinism argument
in place — "each node reads only the shared CSR, n_triangles is an
order-independent integer sum, and cc is a pure function of per-node
integers, so the result is byte-identical to the sequential map"),
module doc, the `ClusteringCoefficients` rule (undirected graph;
arity 4), `clustering_coefficients` (per-node triangle count via
neighbor-pair closure with the e_src<=e_dst dedup; cc = 2T/d(d−1);
cancel polled once per degree≥2 node "unchanged from the sequential
body"), and two tests: DETERMINISM — a 200-node seeded LCG graph
"large enough that the per-node map splits across rayon workers",
single-thread pool vs default pool byte-identical over 8 runs,
pinning "both value AND order"; and the VALUE ORACLE with the full
hand computation of both triangles and every coefficient — closed)
- **L1:** preserve-and-move whole → `rules/algo/triangles.rs`.
- **L2:** gold: a closed parallelism seam carrying its own
  byte-identity argument at the site; the pool-vs-pool determinism
  test as the pattern every parallel rule copies; hand-computed
  oracles. Nothing condemned.

## fixed_rule/algos/prim.rs (206 lines; inventory: dual header (inline
CSR; arity-checked writer; "algorithm unchanged"), the
`MinimumSpanningTreePrim` rule (undirected weighted graph with
negative weights allowed; empty-graph early return; the optional
starting relation with the systemic-finding guard, typed
EmptyStarting and StartingNodeNotFound errors DEFINED AT their use
sites; arity 3), `prim` (visited bitmap + PriorityQueue with
`push_increase` on (Reverse(cost), from-node) priorities; per-pop
cancel poll; early break at n−1 edges), and two VALUE-ORACLE tests
(distinct weights making the MST unique AND directions deterministic,
hand-traced; the equal-weight CHAIN where "both edges and their
directions are forced — the pinnable half of tie behavior", with the
unpinned half stated: "exact ties beyond that follow hash-seeded
queue order and are deliberately not pinned") — closed)
- **L1:** preserve-and-move whole → `rules/algo/prim.rs`.
- **L2:** gold: errors-at-their-use-sites; pinnable-half tie tests
  with the unpinnable half stated honestly. FLAG for the destination
  law (operator-visible, determinism class): the tie comment's own
  admission — exact (cost, from-node) priority ties pop in
  HASH-SEEDED queue order (the priority_queue crate's internal map),
  so a tied-weight graph can yield a DIFFERENT spanning tree run to
  run. That is the same determinism-law breach class rng.rs was
  built to close for the stochastic rules; on arrival the tiebreak
  must become total (e.g. to-node id as the final key), a
  one-line reforge the rules-zone law should demand. Nothing else
  condemned.

## fixed_rule/algos/bfs.rs (214 lines; inventory: dual header
(`binding_indices` typed instead of panicking on an unresolved
binding; the route-reconstruction unwrap annotated structural;
arity-checked writer), the `Bfs` rule (edges/nodes/optional-starting
inputs with the min-len guards; limit + compiled condition bytecode;
`skip_query_nodes` when the condition only touches column 0 — skips
the per-node relation lookup; FIFO queue; the cancel poll placed AT
THE TOP of the per-edge unit "so no early exit... can complete a run
past a raised flag"; backtrace-based route reconstruction with the
structural argument written at the unwrap; arity 3), and the
VALUE-ORACLE test (the diamond hand-traced; the upstream-parity
ruling stated — "the start node itself is never tested against the
condition... only nodes reached over an edge are 'found'"; d's route
pinned through b) — closed)
- **L1:** preserve-and-move whole → `rules/algo/bfs.rs`.
- **L2:** gold: poll-before-early-exit placement argued in place;
  structural unwraps carrying their proofs; the exact-route oracle
  distinguishing BFS from DFS by output alone. Nothing condemned.

## fixed_rule/algos/dfs.rs (215 lines; inventory: dual header (same
three changes as bfs.rs), the `Dfs` rule (iterative — "an explicit
to-visit stack, not recursion"; same input/option/skip plumbing as
BFS; the poll at the top of the per-NODE unit; found-on-pop
semantics; arity 3), and the VALUE-ORACLE test (same diamond as
BFS's "so the two traversals are distinguished by their exact
outputs"; the hand trace naming the two behavioral differences —
DFS tests the POPPED node so the start itself appears, and d's
route goes through c where BFS went through b) — closed)
- **L1:** preserve-and-move whole → `rules/algo/dfs.rs`.
- **L2:** gold: the paired-oracle design (one fixture, two
  algorithms, outputs that cannot be confused); iterative-not-
  recursive stated as the stack-safety posture. Nothing condemned.

## fixed_rule/algos/astar.rs (259 lines; inventory: dual header (the
back-trace unwrap annotated structural with its proof; arity-checked
writer; "otherwise unchanged"), module doc (A* over RELATION-SHAPED
graphs — "edges and nodes stay as tuples (no CSR interning) so the
user-supplied heuristic expression can read node attributes"), the
`ShortestPathAStar` rule (four inputs with min-len guards; the
heuristic's binding map spanning nodes ++ goals columns; every
start × goal pair searched; arity 4), `astar` (compiled heuristic
bytecode with NaN refused typed on BOTH the heuristic and edge costs;
g-score map; PriorityQueue on (Reverse(f), sub_priority) — the
monotone sub_priority counter as the tie-break; default edge cost
1.0; unreachable = (∞, []); per-edge cancel poll), and the
VALUE-ORACLE test (a real ADMISSIBLE heuristic read off the node
relation's second column, hand-traced f=g+h, "the direct a→c edge
(cost 3) loses to the guided two-hop route") — closed)
- **L1:** preserve-and-move whole → `rules/algo/astar.rs`.
- **L2:** gold: tuples-not-CSR argued from the heuristic's needs;
  NaN refused at both cost doors; the monotone sub-priority
  tie-break (NOTE: unlike prim/kruskal this queue's ties ARE totally
  ordered — the counter makes pop order deterministic; the pattern
  the flagged files should adopt). Watch for the destination:
  unreachable pairs emit (∞, empty-list) rows rather than no row —
  a representation choice the rules-zone law should ratify or
  convert to absence. Nothing condemned.

## fixed_rule/algos/label_propagation.rs (268 lines; inventory: dual
header (inline CSR; rand 0.9 re-homes; the structural tie-break
unwrap; and the DETERMINISM FIX pinned vs upstream — OS-entropy
seeding made "the SAME facts + query answer differently run to run —
a determinism-law violation"; randomness now flows from a `seed`
option with the fixed default through SeededRng), module doc, the
`LabelPropagation` rule (undirected/max_iter/seed options; arity 2),
`label_propagation` (per-sweep seeded shuffle; per-node
weight-heaviest label via a BTreeMap fold; total_cmp sort; tied
candidates chosen through the SEEDED rng; per-node cancel poll;
early stop on stability), and three tests: DETERMINISM on a 12-ring
"where ties and shuffle order are exercised on every sweep" — 8
byte-identical runs, "the test that a mutation back to rand::rng()
fails"; the seed-is-LOAD-BEARING test (same seed twice equal, and
across seeds 2..40 at least one disagreement "proves the seed
reaches the tie-break; a constant-ignoring-seed mutant makes every
seed agree"); and the VALUE ORACLE on "the only deterministic ground
label propagation offers" — two directed stars where no decision
ever has more than one candidate, interning hand-derived, with the
adjacency-order note routing the reversed-CSR mutant to the tests
that DO kill it — closed)
- **L1:** preserve-and-move whole → `rules/algo/label_propagation.rs`.
- **L2:** gold, preserve verbatim: the determinism fix with its
  three-test guard structure (byte-identity, seed-steering
  anti-vacuity, and a genuinely tie-free oracle) — the house pattern
  for every stochastic rule; the mutant-routing note (a test that
  says which OTHER tests kill what it cannot). Nothing condemned.

## fixed_rule/algos/kruskal.rs (278 lines; inventory: dual header
(inline CSR; union-find unchanged; and the MULTIGRAPH FIX pinned vs
upstream — the pq is keyed by endpoint pair so parallel edges
collide, plain `push` overwrote the priority "leaving the LAST-seen
parallel edge's weight — a bug on a multigraph"; `push_increase`
keeps Reverse(cost)'s greater = the CHEAPEST, "prim.rs's in-file
precedent"), the `MinimumSpanningForestKruskal` rule (undirected,
negative weights permitted; arity 3), `kruskal` (all edges into the
pq, cheapest-first pops, union-find cycle skip, early break at full
span), `UnionFind` (path compression + union by size), and three
tests: the unique-MST VALUE ORACLE over `normalized` rows (min/max
endpoint + sort, with the honesty note that edge DIRECTION pop order
"is a tie among equal priorities — hash-seeded, not pinnable");
`parallel_edges_take_cheapest` (the multigraph pin, with the
input-order argument for why plain push leaves the EXPENSIVE edge);
and the equal-weight path forcing both edges ("the pinnable half of
tie behavior") — closed)
- **L1:** preserve-and-move whole → `rules/algo/kruskal.rs`.
- **L2:** gold: the multigraph fix argued and pinned with its mutant
  named; normalize-then-assert as the honest oracle shape when
  direction is unpinnable. FLAG (same determinism class as prim.rs,
  acknowledged twice in this file's own comments): equal-priority
  pops are HASH-SEEDED — on tied-weight graphs the chosen tree, and
  on ALL undirected graphs the emitted edge direction, can differ
  run to run in production output. astar.rs's monotone sub-priority
  counter is the in-tree fix pattern; the rules-zone law should
  demand it here and at prim.rs on arrival. Nothing else condemned.

## fixed_rule/algos/strongly_connected_components.rs (283 lines;
inventory: dual header (inline CSR; dead imports/cfg gates dropped;
and the LAW-5 FIX pinned vs upstream — the recursive `dfs` "one stack
frame per graph edge, so a stored component a few hundred thousand
nodes deep overflowed the thread stack and aborted the whole process
(not a typed refusal, a crash)"; the explicit frame stack is "the
exact iterative Tarjan proven in query/graph.rs... byte-identical
component labels"; the cancel poll moved INSIDE the DFS loop), module
doc (registered as ConnectedComponents too — undirected build makes
the SCCs weak components), the `StronglyConnectedComponent` rule
(strong flag at construction; the optional nodes relation with the
systemic-finding guard and its missing-vs-nullary ruling; isolated
nodes get fresh group ids; arity 2), `TarjanSccG` (open — discovery
id + component stack; `dfs` on (node, cursor) frames with the
recursive-version correspondence argued step by step — the O(1)
`out_neighbor` cursor "so the cursor walk is linear, not quadratic,
in degree", the on-stack-guarded low-link propagation at frame close
"exactly what the recursive if on_stack[to] after the nested call
did"; per-frame-step cancel poll; low-map grouping), and three tests:
the SCC/weak/isolated grouping sanity; `deep_chain_does_not_overflow`
— a 300k-node single cycle where the recursive version "overflowed
the 8 MiB thread stack and aborted the process", with the exactness
claim ("the exact answer... also proves the iterative low-link
propagation is correct at depth, not merely non-crashing"); and
`cancellation_inside_dfs` — interruptibility now depends ENTIRELY on
the in-DFS poll, "removing the in-DFS poll makes this run complete
Ok and fail here" — closed)
- **L1:** preserve-and-move whole → `rules/algo/scc.rs` (the map's
  own name for this line).
- **L2:** gold, preserve verbatim: the crash-to-refusal law-5 fix
  with its proven-elsewhere pedigree (the same frame-stack shape as
  exec/plan/graph.rs — two implementations of one pattern, each
  carrying the correspondence proof); depth tests that assert the
  ANSWER, not just survival; a cancellation test aimed at the exact
  poll that carries the property. Nothing condemned.

## fixed_rule/algos/shortest_path_bfs.rs (319 lines; inventory: dual
header (the duplicated upstream header dropped; structural unwraps
annotated; the original's DbInstance test ported onto the payload
harness; and the CANCELLATION FIX pinned vs upstream — "the inner BFS
traversal polled nothing... a single start over a huge reachable set
could not be interrupted"; now polled once per dequeued node), module
doc (one path per (start, end) pair, "Null where unreachable"), the
TEST-ONLY OBSERVABLE (a thread_local dequeue counter that "lets the
test assert a DETERMINISTIC effect of the per-node poll... instead
of a load-sensitive wall-clock ratio"; the non-test build gets "an
empty inlined no-op, so the production loop is unchanged"), the
`ShortestPathBFS` rule (starting/ending collected with min-len
guards; per-start BFS with a pending-endings set enabling early
break; count-BEFORE-poll ordering "so a pre-set flag lets at most
this one node through"; route reconstruction; Null rows for
unreachable pairs; arity 3), and three tests: the ported
`test_bfs_path`; `honors_cancel_pins_inner_poll` — the setup makes
the inner poll "the ONLY thing that can stop the run early" (one
start, 250k chain, an end node ABSENT from the graph so the frontier
never empties), baseline >200k expansions vs ≤1 under a pre-set
flag, "load-independent — it holds on a fully-loaded machine", the
reviewer's line-deletion mutant named; and the VALUE ORACLE pinning
the exact route ("the direct edge [a,c], never [a,b,c]") — closed)
- **L1:** preserve-and-move whole → `rules/algo/shortest_path_bfs.rs`
  (or folded beside bfs.rs per the map's one-algorithm-per-file
  naming — the operator names the file; the algorithm is distinct
  from Bfs and keeps its own seat by default).
- **L2:** gold, preserve verbatim: the deterministic-observable
  pattern for cancellation proofs (a counter, not a clock — the
  sharpest cancellation test in the tier); count-before-poll
  ordering reasoned in place; Null-for-unreachable as the declared
  contract (contrast astar's ∞-row — the rules-zone law should pick
  ONE unreachable representation; both are now on the record).
  Nothing condemned.

## fixed_rule/algos/louvain.rs (389 lines; inventory: dual header
(inline CSR with "same sorted-adjacency layout and node-count
derivation"; log::debug! dropped — no `log` in the workspace; the
structural hierarchy-walk unwrap; the ported sample test), module
doc, the `CommunityDetectionLouvain` rule (undirected/max_iter/
delta/keep_depth options; per-node hierarchy labels walked through
the collected levels, reversed, optionally truncated; arity 2),
`louvain` (contract until ≤2 nodes or no shrink), `calculate_delta`
(modularity gain vs a target community), `louvain_step` (weight
tallies; modularity-guarded sweep loop with the delta threshold;
per-node cancel poll placed at the top of the community scan with
its unit-of-work argument; strict `>` best-improvement; community
renumbering by first appearance; contraction into a BTreeMap-keyed
coarser graph), and three tests: the ported 16-node sample;
`adjacency_tie_break_pinned` — the VALUE ORACLE with the full hand
computation (total_weight 12, every Δ derived) ending in an EXACT
f32 tie where "the strict > keeps the first candidate met in
adjacency order — sorted CSR order visits neighbor 1 before 2...
Reversed adjacency would... flip node 4 into comm{2,3}: this pin
kills the reversed-CSR-sort mutant"; and the F2 cancellation test —
closed)
- **L1:** preserve-and-move whole → `rules/algo/louvain.rs`.
- **L2:** gold: the tie-break pinned BY hand computation down to the
  f32-exactness argument (identical expressions summed in the same
  member order — a tie that is deterministic, not hash-seeded, and
  the test that owns the reversed-CSR mutant for the whole tier);
  cancellation placed and argued at the real unit of work. Watch:
  the modularity recomputation is O(n·comm-size·degree) per sweep —
  quadratic-ish on dense communities; fine for the invocable-library
  tier today, a bench-lane candidate when rules/ gets its
  instrumentation. Nothing condemned.

## fixed_rule/algos/k_core.rs (430 lines; inventory: KyzoDB-only MPL
header, module doc (NEW in KyzoDB, no Cozo precedent — all core
numbers in one O(V+E) Batagelj–Zaversnik pass; the GRAPH
INTERPRETATION ruling — coreness is a simple-undirected-graph
invariant "matches every reference implementation, e.g. NetworkX",
so the directed relation reads undirected and degree counts DISTINCT
neighbors, "a deliberate departure from the CSR's parallel-edge
retention... documented and test-pinned"; the DETERMINISM argument —
an invariant independent of peel order, plus a fixed peel order
anyway; fully iterative "law 5 is not even in play"), the test-only
peel counter (mirroring shortest_path_bfs's observable), the
`KCoreDecomposition` rule (arity 2), `simple_adjacency` (self-filter
+ dedup over the already-sorted CSR), `core_numbers` (bucket
counting-sort with in-place offset conversion; the vert/pos/bin
peel with the swap-slide; "the load-bearing step is the neighbor
decrement + bucket slide: without it... the result collapses to raw
degree"; per-vertex cancel poll), and six tests: an INDEPENDENT
Matula–Beck reference ("O(V²), no bucket trickery — a different
implementation from the one under test. Keyed by node name so it is
independent of interning order"); `coreness_differs_from_degree`
(a's degree 3 vs coreness 2 — the raw-degree mutant); the
naive-reference differential on a K4 + rim + tails graph; the
self-loop/parallel-edge invariance pin; determinism over a seeded
graph; the 200k-cycle scale test with exact all-2s answer; and the
inner-poll cancellation pin (baseline ~60k peels vs ≤1) — closed)
- **L1:** preserve-and-move whole → `rules/algo/k_core.rs`.
- **L2:** gold, preserve verbatim: the simple-graph ruling with its
  reference-implementation citation and pinned departure from the
  shared CSR semantics; the independent structurally-different
  reference; the load-bearing-step note naming the collapse mutant;
  exact answers at scale; the counter-not-clock cancellation
  pattern adopted from its exemplar. Nothing condemned.

## fixed_rule/algos/all_pairs_shortest_path.rs (431 lines; inventory:
dual header (SEAM(parallelism) closed for the per-start Dijkstra
fan-out with the determinism accounting: order-preserving map AND
"the only cross-start float reduction — betweenness' accumulation —
is left as a sequential fold over the ordered per-start segments, so
the summation order is fixed. Closeness has no cross-start
reduction"; group_by→chunk_by), `BetweennessCentrality`
(per-start `dijkstra_keep_ties`, tied paths sharing 1/ties credit to
interior nodes; the sequential merge fold; arity 2),
`ClosenessCentrality` (per-start nc²/Σd/(n−1) over finite distances;
arity 2), `dijkstra_cost_only` (stale-entry skip; push_increase;
per-pop cancel poll), and the tests: an `#[ignore]`d
`zz_timing_evidence` rig printing 1-thread vs default-pool timings
with a byte-equality assert; the two pool-vs-pool DETERMINISM tests
(betweenness named as "the site where a parallel float reduction
would bite; the fold is kept sequential, so it does not"); and the
two hand-computed VALUE ORACLES on the a—b—c path (closeness 1.5/
2.25/1.5 "all exact in f32"; betweenness 0/2/0 with the
unnormalized-over-ordered-pairs semantics stated) — closed)
- **L1:** preserve-and-move whole → `rules/algo/` (the map's
  "centralities" line — one file for the APSP-derived pair, or split
  per rule at the operator's preference; `dijkstra_cost_only` stays
  beside its consumers). The `#[ignore]`d timing rig is a rule-#11
  ledger item — it graduates to the bench lane on migration
  (measurement rig, not a test).
- **L2:** gold: the parallel/sequential boundary drawn exactly at
  the float fold and argued at both the seam and the test;
  semantics-as-implemented stated in the oracle docs (unnormalized
  betweenness, closeness's exact formula — the destination doc
  should carry both so users don't assume NetworkX normalization).
  Nothing condemned beyond the rig's lane change.

## fixed_rule/algos/random_walk.rs (462 lines; inventory: dual header
(LAW-5 FIX — `WeightedIndex::new(..).unwrap()` panicked the engine on
an unusable user distribution, "all zeros being the reachable case";
now a typed error naming the offending expression; the structural
choose unwrap; rand 0.9; and the DETERMINISM FIX — OS entropy made
"the whole walk" vary run to run; seeded via the `seed` option
through SeededRng), the `RandomWalk` rule (edges/nodes/starting with
guards; iterations/steps/seed options; the optional `weight`
expression compiled over nodes++edges bindings with non-negative
enforcement; per-step candidate collection, weighted or uniform
seeded pick, sink break; per-step cancel poll; arity 3 rows
(counter, start, path)), and six tests: the all-zero-weights TYPED
refusal regression; the unweighted smoke; the VALUE ORACLE "on the
one graph where randomness has no choices" (forced path at three
step bounds incl. sink stop); byte-identity over 8 runs on a
branching graph WITH the anti-vacuity assert ("the paths are not
all length-1... so byte-identity is a real determinism claim"); the
seed-is-load-bearing test; and `default_seed_output_is_golden` —
the literal path the DEFAULT seed determines, "run_twice above only
proves the output is STABLE; this proves it is the specific value
DEFAULT_SEED... determine[s]", the other half of rng.rs's guard
chain — closed)
- **L1:** preserve-and-move whole → `rules/algo/random_walk.rs`.
- **L2:** gold, preserve verbatim: the reachable-panic analysis in
  the law-5 fix (which input classes are already refused upstream
  and which one actually reaches the unwrap); the three-layer
  determinism guard (stability, seed-steering, golden literal) —
  with label_propagation, the completed house pattern for
  stochastic rules; anti-vacuity asserts inside determinism tests.
  Nothing condemned.

## fixed_rule/algos/yen.rs (473 lines; inventory: dual header
(SEAM(parallelism) closed for the per-(start,goal) fan-out — pair
list built in sorted BTreeSet order + order-preserving map "so rows
land exactly as the sequential loop emitted them", the writer never
shared; structural unwraps annotated; and the MULTIGRAPH FIX pinned
vs upstream — the root-segment recomputation "took the FIRST
neighbor matching the next node on the path, ignoring weight...
Dijkstra built the path over the cheapest — so a candidate's total
cost could be overstated and mis-ranked"; now the MINIMUM matching
weight), the `KShortestPathYen` rule (undirected/k options;
start/goal sets resolved through inv_indices with absent nodes
silently skipped; arity 4), `k_shortest_path_yen` (the
ALLOCATOR-ABORT GUARD — "k is a caller-supplied option with no upper
bound: reserving from it directly would let an absurd k abort the
allocator"; spur searches over forbidden edges/nodes; the cancel
poll moved to the TOP of the spur loop "one iteration runs a full
Dijkstra, so a raised flag must refuse before the next spur search —
not after |path|−1 of them"; min-weight root recomputation with the
structural existence argument; dedup + total_cmp sort + pop
cheapest; infinite candidates filtered), and six tests: pool-vs-pool
determinism over multi-pair inputs; `parallel_root_edge_uses_min_
weight` (the mutant quantified — "a first-match recomputation
reports 12 and fails this test", with the input-order reasoning);
spur-search cancellation at the dijkstra core; the two-paths
end-to-end; the VALUE ORACLE `k_shortest_order_is_cheapest_first`
with the full three-step Yen hand computation and the rationale
("the store it reads through re-sorts rows, so order must be pinned
on the algorithm itself"); and the F2 pre-set-flag refusal — closed)
- **L1:** preserve-and-move whole → `rules/algo/yen.rs`.
- **L2:** gold: the mis-ranking analysis in the multigraph fix (a
  wrong-ANSWER class, not a perf nit); the allocator guard on an
  unbounded user option (the same discipline sparse/fts landed);
  poll placement quantified by what it saves; order pinned at the
  algorithm because the store masks it. Nothing condemned.

## fixed_rule/algos/pagerank.rs (538 lines; inventory: dual header
(the original delegated to the graph crate's page_rank —
"rayon-parallel, chunk-racing over a shared score array —
nondeterministic in the low-order float bits — and never polling the
poison"; the SEAM closed with a DELIBERATE, PINNED SEMANTIC CHANGE,
pre-authorized by the maintainer on two fronts: from upstream's
chunk-raced array "nondeterministic above ~16384 nodes" AND from the
interim sequential Gauss-Seidel port — the scheme is now two-buffer
JACOBI so nodes are independent within an iteration; the two
order-dependent float computations kept sequential in fixed order —
per-node in-neighbor sums folded in CSR ascending-source order, the
per-iteration Σ|Δ| folded over the node-index-ordered Vec; "Jacobi
and Gauss-Seidel are the same math family with the same fixpoint...
but their iterates differ at a fixed iteration count, so the numbers
change. They agree once converged (pinned by a test)"; upstream's
dead nalgebra fallback "could not have compiled; dropped"), the
`PageRank` rule (theta/epsilon/iterations options; arity 2),
`page_rank` (the frozen-prev Jacobi read; the sink
divide-by-zero-into-inf note — "the value is never read, because a
sink is nobody's in-neighbor"; per-node cancel poll; sequential
install+error fold), and NINE tests: the qualitative sanity; the
VALUE ORACLE against `naive_jacobi` — an independent reference over
plain adjacency lists whose values are "computed, not hand-typed",
byte-identical at 1/2/5/10 iterations ("were page_rank still
Gauss-Seidel, this would fail"); the WIDE-HUB fold-order pin — 300
in-neighbors with out-shares spanning magnitudes so "the f32 sum
[is] genuinely non-associative... the executable proof of the
docstring's ascending-CSR-order claim"; the TERMINATION-METRIC test
(Σ|Δ| vs max|Δ| stopping on DIFFERENT iterates at tolerance 0.1,
with an assert_ne! guarding the test's own discriminating power);
the through-the-rule plumbing oracle; pool-vs-pool ×20 and
run-twice determinism; `converges_to_same_fixpoint_as_gauss_seidel`
on a deliberately degree-IRREGULAR strongly-connected graph ("a
regular graph would leave both trivially uniform and equal, hiding
the divergence") WITH the mid-transient disagreement guard
("guards against the reference collapsing into the
implementation"); and the cancellation refusal — closed)
- **L1:** preserve-and-move whole → `rules/algo/pagerank.rs`.
- **L2:** gold, preserve verbatim: the authorized-divergence record
  (what changed, from what, why it is safe, and the test that pins
  each clause — the house form for a semantic change); the wide-hub
  non-associativity pin (a test that proves a SUMMATION ORDER, the
  memcmp-law severity applied to floats); the discriminating-power
  assert inside the metric test; derived-not-hand-typed oracle
  values; the same-fixpoint-different-path proof with its
  anti-collapse guard. Nothing condemned.

## fixed_rule/algos/shortest_path_dijkstra.rs (668 lines; inventory:
dual header (inline CSR; SEAM(parallelism) closed — sorted-BTreeSet
start list + order-preserving map, with the honesty note that
which tied path wins WITHIN a start "is priority-queue pop order,
already documented as not pinnable and unaffected by this axis";
SmallVec dropped for plain Vec; the dead upstream `HeapState` struct
"referenced nowhere in the file or the workspace" dropped; and the
CANCELLATION FIX — the plain `dijkstra` core "took no cancel flag and
never polled, so a keep_ties=false search — and every Yen spur, which
drives this core — was uninterruptible... the ratified
budget/deadline design could not stop it"; now polled once per pop,
with the unset-flag-is-byte-identical argument), the
`ShortestPathDijkstra` rule (optional termination; undirected/
keep_ties options; the single-goal Option specialization; arity 4),
the CAPABILITY TRAITS (`ForbiddenEdge`/`ForbiddenNode`/`Goal` with
()/Option/BTreeSet impls — zero-cost genericity shared by Yen and
the centralities), `dijkstra` (stale-pop skip; push_increase
relaxation; goal-exhaustion early break; unreachable = (∞, []));
`dijkstra_keep_ties` (multi-back-pointer lists; equal-cost re-push;
the recursive `CollectPath` tie enumeration), and six tests:
pool-vs-pool determinism with the tie axis EXCLUDED BY CONSTRUCTION
("weights are distinct enough that shortest-path costs are unique,
so no tie resolution... is exercised"); the cheap-route end-to-end;
the tie graph with its max-heap-mutant hand analysis ("pops d at
cost 3 first (not stale: 3 == dist(d))... and answers 3 — wrong");
keep_ties returning BOTH tied paths exactly; keep_ties with
MULTIPLE goals ("the search must not stop at the first goal it
visits"); the no-keep_ties single row asserted STRUCTURALLY
(a → (b|c) → d, the unpinnable middle stated); and
`plain_dijkstra_honors_cancel` with its never-changes-an-answer
companion assert — closed)
- **L1:** preserve-and-move whole → `rules/algo/dijkstra.rs` (the
  map's name); the capability traits and both cores travel with it
  as the shared search substrate Yen and the centralities import.
- **L2:** gold: the capability-trait design (forbidden sets and
  goals as zero-sized strategies — one core, no copies); the
  uninterruptible-core fix argued through its CONSUMERS (Yen spurs);
  tie behavior handled three ways, each honest (excluded by
  construction, enumerated exactly under keep_ties, asserted
  structurally when unpinnable); dead upstream code identified and
  dropped in the ledger. FLAG (third instance of the determinism
  class, same as prim/kruskal): the no-keep_ties tied-route pick is
  hash-seeded pop order — production answers on tied graphs vary
  run to run; astar's monotone counter remains the in-tree fix
  shape. Nothing else condemned.

## fixed_rule/algos/max_flow.rs (689 lines; inventory: KyzoDB-only MPL
header, module doc (NEW, no Cozo precedent — max flow / min cut;
output is the MINIMUM CUT's saturated edges "so the scalar the caller
usually wants is recovered by summing the third column"; the
ALGORITHM CHOICE argued — Edmonds–Karp over Dinic because the O(VE)
augmentation bound "is purely combinatorial and independent of
capacity magnitude or rationality — so it terminates on real-valued
capacities where plain Ford–Fulkerson can loop forever", and law 5:
"every phase is a BFS plus an iterative back-pointer walk — no
recursion at all"; the PARALLEL-EDGE POLICY — capacities SUM, "two
independent conduits... the physically correct policy", antiparallel
arcs independent, self-loops dropped; the DETERMINISM argument —
sorted (from,to) residual construction, fixed BFS adjacency order,
sorted cut enumeration), `EPS` = 1e-9 (the saturation threshold
"load-bearing for termination, so the contract is ENFORCED rather
than assumed": any aggregated capacity in (0, EPS] refused typed up
front), the test-only BFS-pop counter (the house observable),
`MaxFlow` (three inputs; source==sink refused typed; arity 3),
`single_node` (the Dijkstra/Prim endpoint convention with the
structural note covering the nullary shape), `ResidualArc`/
`ResidualNet` (BTreeMap-aggregated construction fixing the sorted
order the determinism argument relies on; paired reverse arcs;
`max_flow` — per-pop cancel poll, bottleneck walk, forward+reverse
push; `min_cut_edges` — residual-reachable source side, cut read
back over ORIGINAL arcs only), three typed errors (empty endpoint,
sub-epsilon capacity with its rescale help, source-is-sink), and
eleven tests: the CLRS figure-26.1 network at the textbook 23
(antiparallel arcs + flow cancellation); the unique-bottleneck
pinned cut; `parallel_edges_sum_capacity` (the max/last-wins mutants
named); the 120-seed differential against `brute_min_cut` — an
INDEPENDENT oracle enumerating all 2^(n−2) bipartitions, "shares no
logic with augmenting-path search", and "where flow cancellation
through reverse arcs is exercised broadly: breaking the reverse-arc
update makes many of these values wrong"; byte-identical determinism
×6; the 5k-chain no-recursion scale case; unknown-source /
source==sink / negative-capacity typed refusals; the inner-poll
cancellation pin (baseline ≥60k pops vs ≤1); and the F1
hostile-review pin — a 1e-12 capacity "returned a wrong max flow of
0" before the up-front refusal, covered for a plain tiny AND an f32
denormal — closed)
- **L1:** preserve-and-move whole → `rules/algo/max_flow.rs`.
- **L2:** gold, preserve verbatim: the algorithm choice argued from
  termination-on-floats and law 5 (asymptotics traded for
  guarantees, stated); the EPS contract ENFORCED at the door rather
  than assumed inside the solver (the F1 fix's shape — a threshold
  that silently rewrites answers must refuse its own gray zone); the
  brute-force bipartition oracle (max-flow/min-cut duality used as
  an independence lever); the physically-argued parallel-edge
  policy; min-cut-as-output with the sum-recovers-the-scalar
  contract. Nothing condemned.

## fixed_rule/algos/maximal_cliques.rs (855 lines; inventory:
KyzoDB-only MPL header, module doc (NEW, no Cozo precedent —
Bron–Kerbosch with Tomita pivoting in Eppstein–Löffler–Strash
degeneracy order, near-optimal O(d·n·3^{d/3}); the simple-undirected
interpretation "matching the k_core interpretation"; CANONICAL CLIQUE
NUMBERING — members sorted by value, cliques sorted lexicographically,
id = index, "a pure function of the GRAPH, independent of input edge
order or the interning"; the efficiency-vs-correctness partition
stated — pivot and ordering "neither changes the SET of maximal
cliques", the correctness-critical steps named and mutation-pinned;
LAW 5 — recursion on an EXPLICIT HEAP STACK because depth is bounded
by clique size and a dense graph would overflow a recursive form;
BOUNDED, BUDGET-HONEST — enumeration is exponential (Moon–Ganguli
3^{n/3}), the `max_cliques` cap (default 2^20) makes exceeding it "a
typed refusal, never an unbounded allocation — the OOM lesson made
law"), the THREE test-only observables (degeneracy removals,
expansion steps, and the raise-at-step-N hook — "the only
deterministic way to prove the EXPANSION poll reads the flag — a
pre-raised flag is always caught first by the degeneracy poll"),
the `MaximalCliques` rule (arity 2 membership rows), a local
`simple_adjacency` ("kept local so this file is a self-contained
deliverable"), `degeneracy_order` (Matula–Beck smallest-last with
BTreeSet buckets, deterministic ascending-id ties, per-removal
poll), the `Frame`/`Enumeration` iterative machinery
(`bron_kerbosch` on an explicit stack with per-expansion-step poll;
`make_frame` carrying the MAXIMALITY GATE doc — "dropping the X half
would report non-maximal cliques"; `choose_pivot` — Tomita coverage
max with smallest-id ties; the sorted-slice
intersect/count/contains helpers), the typed `TooManyCliquesError`
(with its raise-it-deliberately help), and NINE tests: the
two-triangles VALUE ORACLE pinned exactly; the brute-force
differential — an INDEPENDENT all-subsets reference ("a subset is a
clique iff all its pairs are adjacent; maximal iff no outside
vertex is adjacent to all of it... shares no logic with
Bron–Kerbosch") over a wheel, K4+pendant-triangle, and 40 seeded
graphs; `maximality_gate_rejects_subsets` (the exact weakened-gate
mutant named); interning-independence determinism (edge order
REVERSED must not change the numbering); the ceiling refusal at
cap 3 < 5 disjoint triangles WITH the exactly-enough companion;
`deep_clique_no_overflow` — K2000's single clique with the honest
depth note ("the structural point is the explicit stack, not raw
depth"); and the TWO cancellation pins — the degeneracy poll
(pre-raised flag, ≤1 removals AND 0 expansion steps) and the
expansion poll (flag raised BY THE HOOK at step 5, refusal at
EXACTLY step 5, with both weakenings' failure modes named) —
closed)
- **L1:** preserve-and-move whole → `rules/algo/cliques.rs` (the
  map's name).
- **L2:** gold, preserve verbatim: the bounded-enumeration law
  (exponential output capped by a typed, teachable refusal — the
  allocator-abort discipline applied to combinatorial explosion);
  canonical numbering as a pure graph function; the
  efficiency/correctness partition stated so a reviewer knows which
  mutants the oracle can and cannot see; the two-poll cancellation
  design with the raise-at-step hook (the sharpest cancellation
  instrumentation in the tier — a poll proven by the exact step it
  fires at); the self-contained-deliverable duplication of
  simple_adjacency NOTED as deliberate (on migration, k_core and
  cliques share the rules/graph_view simple-adjacency helper — one
  concept, one name). Nothing condemned.

## fixed_rule/utilities/mod.rs (24 lines; inventory: dual header ("module
docs added; contents unchanged"), module doc, four decls + re-exports —
closed)
- **L1:** structural glue — dies with the directory; the readers and
  constant land in `rules/io/`, reorder_sort in `rules/algo/`.
- **L2:** nothing beyond the glue.

## fixed_rule/utilities/constant.rs (255 lines; inventory: dual header
(the re-homing of the Constant impl from its parse/query.rs seam; the
original's `run` "unwrapped its way to the data... trusting that
init_options had normalized the option" — those unwraps are the
`proven_data` path, and DRIFT "is reported as the wrong-option error
instead of aborting the engine"), module doc (the `data` option IS the
relation; backs `<-` const rules and the body-less `:create` synthetic
entry), `proven_data` (the drift detector), the FixedRule impl (run
emitting proven rows; arity from the first row with the
EmptyConstRuleError give-it-a-head refusal and head-length fallback;
init_options — eval_to_const, per-row list check with typed
ConstRuleRowNotList, ragged rows refused with typed
ConstRuleRowArityMismatch carrying both lengths, normalized back into
the options map), and two tests (the three-phase round trip "as
parse/eval do"; drifted options refusing typed — arity before
init_options, and ragged rows) — closed)
- **L1:** preserve-and-move whole → `rules/io/constant.rs` (the map's
  io/ line names constant data explicitly).
- **L2:** gold: normalize-once-then-trust-with-a-drift-detector (the
  proven_data pattern converts convention into a checked invariant);
  errors defined at use sites with teaching help. Nothing condemned.

## fixed_rule/utilities/reorder_sort.rs (321 lines; inventory: dual
header (&'static Op deref adjustment; the structural ranking unwrap;
arity-checked writer), module doc, the `ReorderSort` rule (the `out`
option as const-list or OP_LIST apply, refused typed otherwise;
sort_by/descending/break_ties/skip/take options; bindings filled and
compiled to bytecode; buffer of evaluated out-tuples with the sort key
appended; stable sort asc/desc on the key; COMPETITION RANKING — rank
jumps to count after ties — vs break_ties running count; the
take-plus-skip window with saturating_add; per-row cancel polls;
arity = |out| + 1), and three VALUE-ORACLE tests (mixed-type keys
ordered per the DataValue total order, each rank pinned; the
tie/descending battery with the hand computation and the
stable-sort-plus-shared-rank note; the skip/take window with the
overall-rank-preserved assertion) — closed)
- **L1:** preserve-and-move whole → `rules/algo/reorder_sort.rs` (the
  algo/ list is open — "one algorithm per file"; this is the one
  non-graph algorithm in the library).
- **L2:** gold: competition-vs-dense ranking as explicit options with
  hand-computed pins; the total-order oracle doubling as a
  cross-check on DataValue's Ord. Watch for the destination: the
  rank/window semantics overlap exec/sort.rs's `:order`/`:limit` —
  the rules-zone doc should say when each is the right tool (the rule
  exposes RANKS as data; the query options only order the result).
  Nothing condemned.

## fixed_rule/utilities/jlines.rs (317 lines; inventory: dual header
(file:// arms fully ported; SEAM(network) — the original fetched
non-file URLs via minreq behind the `requests` feature; "no network
dependency is added — the URL arm refuses with the typed
UrlFetchUnavailable, pending DECISION(maintainer): should a pure
embedded engine carry an HTTP(S) client for the reader utilities?";
SEAM(json) — the conversion was local because upstream data/json.rs
was then-dropped, "re-homes to data/json.rs when that file is
reinstated... one name per concept"; log::error! gone with the
fetch), `UrlFetchUnavailable` (the help text naming the open product
decision), `json_to_datavalue` (ints when integral, floats, decimal
string fallback, arrays recurse, objects stay opaque Json), the
`JsonReader` rule (url/json_lines/null_if_absent/prepend_index
options; typed BadFields; the field projector with
absent-field-refuses-unless-nullable; STRIDE-POLLED cancellation —
"every 1024 items — a stride, not per item, since one line's parse
is cheap"; both file arms; the URL arm refusing), arity from
fields + prepend_index, and four tests (JSON-lines projection with
Null absorption; the raised-flag refusal noting the stride fires at
i==0; the typed network refusal; the conversion battery) — closed)
- **L1:** preserve-and-move whole → `rules/io/jlines.rs`. TWO
  standing items travel with it: (1) the network DECISION(maintainer)
  — operator-visible, unresolved, shared with csv.rs; (2) the
  SEAM(json) note is now STALE AND DISCHARGEABLE — data/json.rs has
  since been reinstated (censused above) carrying its own
  `From<JsonValue> for DataValue` conversions, so `json_to_datavalue`
  is today a live DUPLICATE of model vocabulary; on migration it
  dissolves into the model/envelope conversion exactly as the
  header's own promise says ("one name per concept"), after a
  differential confirms the two agree on the object/number edge
  cases.
- **L2:** gold: seams refused typed with the decision NAMED in the
  user-facing help; stride-polling argued from per-item cost.
  Nothing condemned beyond the now-dischargeable duplication.

## fixed_rule/utilities/csv.rs (327 lines; inventory: dual header (the
csv crate vetted pure Rust — "csv-core/itoa/ryu/serde, no C
toolchain"; the same SEAM(network) with the fuller product question —
"whether a pure embedded engine should carry a network stack (TLS
roots and all)"; structural unwraps after coerce; type-mismatch bails
gaining spans via WrongFixedRuleOptionError "where they were bare
strings"), the `CsvReader` rule (single-byte delimiter enforced
typed; prepend_index/has_headers; the `types` option coerced through
the REAL schema machinery — NullableColType/ColType::List/`coerce` —
then each element through the model's `parse_type`; flexible reader;
per-column coercion arms — Any/String/Uuid/Float via the real ops
with nullable-absorbs-failure, and the INT-VIA-FLOAT ruling
documented in place ("get_int alone is strict on representation (a
float 1.0 is not an int), so the integral check is explicit");
stride-polled cancellation with the same 1024 argument; the URL arm
refusing through jlines' shared error), arity from types +
prepend_index, and three tests (typed read with a nullable column
absorbing "oops"; the raised-flag refusal; the typed network
refusal) — closed)
- **L1:** preserve-and-move whole → `rules/io/csv.rs`; the network
  DECISION(maintainer) rides with jlines'.
- **L2:** gold: reusing the engine's own coercion and type-parsing
  machinery for the option surface (no second type grammar); the
  integral-check ruling written at the arm; nullable-absorbs vs
  strict-refuses split per column. DEFECT (doc-grade, fix at
  migration): despite the header's claim, `process_row` still
  carries three BARE-STRING `bail!`s on the data path (the non-null
  violation and both cannot-convert arms) — span-less, stringly
  refusals on user data; route them through a typed error with the
  option's span like their siblings. Nothing else condemned.

## kyzo-core/tests/ — the story-#88 external public-surface suite
The fourteen files below are EXTERNAL integration crates: by construction
of where they live, they reach only the public `kyzo::` façade — the exact
boundary the target architecture SEALS. Their collective verdict is
PRESERVE-IN-PLACE: `kyzo-core/tests/` remains the sealed contract's own
integration suite. The one cross-cutting rewire: on the crate split, the
model-vocabulary re-exports they import (`DataValue`, `Tuple`, `Validity`,
`UuidWrapper`, …) resolve per lib.rs's entry (kyzo-model's public surface
re-exported through the façade), which changes no test text if the façade
keeps the names — the suite is the drift detector for exactly that
decision ("if the public API reshapes and breaks a consumer, this test
breaks first, at the contract").

## tests/common/mod.rs (87 lines; inventory: MPL header, module doc (every
test file is "forced through the same public API a real embedder uses...
no pub(crate) internals reachable from here, by construction of where
this file lives"; fresh REAL fjall store per test), `no_params`,
`fresh_db` (the leaked-tempdir ruling with its reason, mirroring
language_tour), and the ints/floats/strs/bools column extractors
(panic-on-wrong-kind justified: "a test that already knows its own
schema") — closed)
- **L1:** preserve-in-place as the suite's shared fixture.
- **L2:** gold: unreachability-of-internals BY CONSTRUCTION as the
  suite's independence argument. Nothing condemned.

## tests/unified_scenario.rs (115 lines; inventory: header, module doc
(the one-substrate thesis as a TEST: "graph, vector, text, and
aggregation... one substrate with several access paths that compose in a
single query"), and the single test — six articles with topic vectors,
bodies, and a citation graph; fts + hnsw indices created; ONE query
composing recursion (citing-distance reachability), an FTS atom, a k-NN
atom with bind_distance, and grouped count+min; the expected groups and
distances hand-computed in comments (0.02 and 1.62 squared-L2) — closed)
- **L1:** preserve-in-place. This is the README's thesis made
  executable — the telos test.
- **L2:** gold: four access paths in one rule body, judged against
  hand-derived values. Nothing condemned.

## tests/relational_core.rs (312 lines; inventory: header, module doc
("the way a user actually would... every expected value hand-computed
from the literal rows written in the same test"), and eight tests:
create+filter (the language_tour chapter-1 shape "re-verified here with
its own numbers so this file stands alone"); the three-way join; the
four filter operators; constants-join-stored; put-overwrites/rm; the
:replace whole-swap pin ("ids 1,2,3 must be gone"); the
:ensure/:ensure_not battery ending in the neither-op-mutated re-read;
and :order/:limit/:offset composed with exact page contents — closed)
- **L1:** preserve-in-place.
- **L2:** gold: verify-by-reading-back after every mutation kind;
  self-contained hand oracles. Nothing condemned.

## tests/recursion_and_negation.rs (194 lines; inventory: header, module
doc, five tests: transitive closure; SAME-GENERATION (the non-linear
textbook rule, with the root-ancestors-in-the-base-case subtlety
explained — "otherwise carol/dave's shared-parent recursion has no
sg[eve, eve] to bottom out on" — and the full expected pair set
reasoned); ShortestPathDijkstra through the `<~` surface (cheapest
3900 vs 9000); ConnectedComponents (three components asserted as
sorted groups); and stratified negation (enrolled-minus-submitted) —
closed)
- **L1:** preserve-in-place.
- **L2:** gold: the same-generation base-case commentary (a test that
  teaches the semantics it pins). Nothing condemned.

## tests/aggregation.rs (176 lines; inventory: header, module doc, four
tests: grouped count/sum/min/max/mean with hand values; the global
no-bare-head aggregate; `collect` with the order-not-pinned honesty
("collection order isn't part of the contract we're pinning here");
and the RETRACTION HARD CASE — :rm the row holding the current min,
recompute to 20, then 30, then the group VANISHES ("an empty group
must vanish entirely", "there is no incremental subtract-from-running-
min") — closed)
- **L1:** preserve-in-place.
- **L2:** gold: the min-retraction ladder run to the vanishing point.
  Nothing condemned.

## tests/data_types.rs (178 lines; inventory: header, module doc (every
DataValue kind round-tripped via the public API only; where a Rust
accessor is crate-internal the check goes through KyzoScript's own
public functions — "exactly the boundary an external embedder sits
at"), four tests: the all-kinds round trip (Int/Float/String/Bool/
Bytes-from-base64/List/Uuid/Json/Validity/Interval-via-interval_start/
end); validity POLARITY as part of the value; the vector byte-exact
round trip checked through KyzoScript's own `==` with an inequality
control; and the #119 COERCION-CLASS REGRESSION — integral floats
coerce into Int columns ("the rewrite had dropped this... invisible to
the old corpus, which only inserted 3.5 into Float and 42 into Int")
with the non-integral refusal companion — closed)
- **L1:** preserve-in-place.
- **L2:** gold: the corpus-blind-spot lineage on the coercion
  regression (a gap named by why the old corpus couldn't see it).
  Nothing condemned.

## tests/errors_and_refusals.rs (127 lines; inventory: header, module doc
("refusal is a typed Err, never a panic — this file would abort the
test process on a panic, which is itself part of what's being
verified"), three tests: the textbook unstratifiable cycle refused
naming stratification; the recursive standing query refused at
registration WITH the same-query-runs-fine-as-one-shot companion ("the
refusal is specific to the standing-query seam, not to recursion in
general"); and bad coercions (String→Int, 3-element vector→<F32;2>)
refused with the relation-untouched read-backs — closed)
- **L1:** preserve-in-place.
- **L2:** gold: refusal-specificity companions (prove the refusal
  discriminates, not just fires). Nothing condemned.

## tests/adversarial_robustness.rs (271 lines; inventory: header, module
doc (the torture battery "locked in permanently"; the law — "no query
text may panic the process"), thirteen tests: i64-overflow arithmetic;
out-of-range literals; Int-vs-String comparison; unbalanced parens;
deep-but-LEGAL nesting evaluating correctly ("nesting depth alone is
not an error"); the unstratifiable cycle (kept here too "so the whole
torture battery lives in one place"); the reserved i64::MAX temporal
write refused; the 3^4=81 self-cross-product ("the honest
cardinality"); aggregation over an emptied relation = the identity
row (count 0, min Null); the EXCLUDED-CASE NOTE — unbounded recursion
"currently hangs rather than refusing — a real, tracked bug (issue
#68: the fixpoint has no default iteration budget)"; division and
modulo by zero pinned only to NEVER-PANIC ("under active change in a
concurrent story... whichever typed shape the fix lands on, this
assertion still holds"); the silent-NaN class refused through the
query surface (sqrt(-1), ln(0), cos_dist zero-vector, l2_normalize
zero) with an in-domain control; `to_float('nan')` in every spelling
refused — a USER-OBSERVABLE BEHAVIOR CHANGE "pinned here... the
correct, deliberate outcome per the engine's no-silent-poison ethos";
and the COLUMNAR twin — a runtime-produced inf−inf over a stored
value "so partial_eval cannot fold it away" — closed)
- **L1:** preserve-in-place. OPERATOR-VISIBLE STANDING ITEM carried
  from the file's own note: the unbounded-recursion hang (issue #68's
  no-default-budget gap) is deliberately untested because it would
  hang the suite — the default-budget fix is the discharge, and this
  battery gains the case then.
- **L2:** gold: excluded cases documented with their tracking issue
  (an honest hole beats a hanging test); behavior changes pinned AS
  changes with their ethos argument; the fold-proof columnar twin.
  Nothing condemned.

## tests/system_ops.rs (141 lines; inventory: header, module doc
("asserting on the actual NamedRows headers and rows, not just 'it
didn't error'"), two tests: ::relations/::columns with exact headers,
arities, and key flags; and the ::index create/list/drop ROUND TRIP
with the acceleration-not-truth companion ("the index doesn't change
ordinary query results") and data-untouched-after-drop — closed)
- **L1:** preserve-in-place.
- **L2:** gold: headers asserted verbatim (the introspection surface
  is a contract too). Nothing condemned.

## tests/vector_and_fts.rs (168 lines; inventory: header, module doc
(search as "ordinary relational-algebra operators"), five tests:
plain k-NN nearest-first; the filtered k-NN with hand squared-L2
arithmetic; k-NN JOINED to a stored relation; plain FTS; FTS composed
inside a join sharing `id` — closed)
- **L1:** preserve-in-place.
- **L2:** gold: search-composes-like-any-relation proven three ways
  (filter, join, both engines). Nothing condemned.

## tests/time_travel.rs (301 lines; inventory: header, module doc, six
tests: as-of reads across two corrections plus the
unqualified-read-is-now pin; :rm at a later instant with before/after
as-of reads; @spans maximal runs with the #119 CLOSED-GRID ruling in
the expected values ("interval_end is 199 (the last included
instant), not the exclusive 200") and the genuinely-OPEN final run
(end Null, `interval_has_end` false, `interval_is_end_unbounded`
true — "i64::MAX is a finite instant, not infinity; the value plane
exposes real unboundedness, so the scripting surface must too"); the
reserved-END write refusal naming the terminal tick; the
SENTINEL-NEVER-LEAKS sweep (no interval_end is ever i64::MAX, with
the exactly-one-Null anti-vacuity count); and @delta(lo, hi) signed
net change (correction = both signs, removal = −1 only, untouched =
no row) — closed)
- **L1:** preserve-in-place.
- **L2:** gold: the closed-grid and open-end semantics pinned at the
  USER surface with their reasoning quoted; anti-vacuity counts on
  the leak sweep. Nothing condemned.

## tests/standing_queries.rs (165 lines; inventory: header, module doc
(the seam "where a real bug slipped through" — multi-commit drains;
every case checks the maintained answer against a FRESH run_script
recompute "the real production evaluator, not a second incremental
registration"), the repeated crate-boundary clippy allow with its
lib.rs cross-reference, two tests: five commits (correct-twice,
assert-then-retract, fresh key) before ONE poll — recompute equality
plus the exactly-once corrected key and the netted-to-nothing key;
and the aggregation hard case live (retract the current min AND a
new group in one drain, recompute equality plus the exact (1,20),
(2,5) values); both tear down — closed)
- **L1:** preserve-in-place.
- **L2:** gold: recompute-through-the-real-engine as the judge (the
  same discipline standing.rs's own differential states); the bug
  lineage in the module doc. Nothing condemned.

## tests/storage_allocation_law.rs (143 lines; inventory: header, module
doc (story #118's second law — Slice as the byte currency, "never a
per-row Vec<u8> copy"; the prior design's two-allocations-per-row
lineage; the COUNTING GLOBAL ALLOCATOR methodology — real
alloc/alloc_zeroed/realloc CALLS, two row counts within the SAME
store "no cross-store confound", and the one-test-binary isolation
argument: "cargo test runs test BINARIES one at a time... this
file's own single test can install a process-wide #[global_allocator]
without racing unrelated tests"), the `Counting` allocator
(unsafe impl GlobalAlloc — instrument code OUTSIDE the crate-root
forbid boundary, the pointsto_repro precedent), write_rows (one
transaction, under flush thresholds "so both scans hit the same
memtable/SSTable shape"), allocs_for_range (snapshot-open excluded),
and the law test — 100 vs 10,000 rows, ratio < row_ratio/4 with the
measured SAME-7-allocations disclosure and the
deliberately-looser-than-measured thresholds ("the law is about the
shape of the scaling, not a brittle pin on one number"), plus the
direct marginal-per-row < 1.0 assertion against the old design's
2.0 — closed)
- **L1:** preserve-in-place (the sealed contract's own perf-law
  test; its store-level imports — ReadTx/WriteTx/Storage — ride the
  public re-export decision the crate split makes for the trials
  kit, the same question conformance.rs's entry records).
- **L2:** gold: allocation LAWS asserted by counting calls, not
  guessing at bytes; same-store confound control; measured numbers
  disclosed with thresholds looser than the measurement; the
  unsafe-bearing instrument confined to an external test binary the
  forbid cannot and need not reach (rule #2's boundary is the
  first-party CRATE roots; flag any drift of instrument unsafe INTO
  a crate). Nothing condemned.

## tests/public_api_surface.rs (186 lines; inventory: header, module doc
(ONE smoke over "the WHOLE public kyzo:: façade... if the public API
reshapes and breaks a consumer, this test breaks first, at the
contract"; dump/restore and verify_storage "covered by NO other
integration test"), fresh_storage, and the single test walking:
relational CRUD over six value kinds; the integral-float coercion
contract; transitive closure (6 pairs); aggregation; hnsw create +
k-NN; bitemporal create/put/@-read; the typed-refusal path; the
dump→restore round trip incl. the REBUILT-INDEX search on the
restored store; and a clean verify_storage report — closed)
- **L1:** preserve-in-place — this file IS the sealed contract's
  drift detector; on the crate split it is the first test the
  re-export decision must keep green.
- **L2:** gold: the breaks-first-at-the-contract framing; the only
  integration coverage of backup and verify called out as such.
  Nothing condemned.

## benches/db_scan.rs (65 lines; inventory: header, module doc ("the
current-state segment engine's kill-gate instrument. Steady-state
iterations here run entirely against the served segment; the same bench
on a segments-reverted tree is the A/B baseline"), seeded_db, and
bench_scans (one seeded store per size driving both queries with the
pay-the-seed-once reasoning; the warm read that "builds the segment";
full and filtered scans at 50k/200k, sample_size 20) — closed)
- **L1:** preserve-in-place → the target `kyzo-core/benches/`
  ("permanent performance instrumentation").
- **L2:** gold: an instrument named for the DECISION it gates (the
  segments kill-gate) with its A/B protocol stated. Nothing condemned.

## benches/ra_exec.rs (124 lines; inventory: header, module doc ("the
permanent instrumentation the vectorization ascent measures against";
four workload classes each named for "a distinct cost the design must
answer"; both backends; the HISTORICAL-TAG ruling — the id keeps
"batch" "so its row stays comparable across the iterator twin's
deletion in the committed bench-results/ logs"; seeded generators,
machine stamped in the results files not here), DirFactory,
bench_workload, and the `all` sweep (TC chain/dense/random with the
O(n²)-pairs sizing note; the 3-way join with the
measure-parity-not-speedup honesty; scan+filter at three
selectivities; aggregation at two scales) — closed)
- **L1:** preserve-in-place → `kyzo-core/benches/`. REWIRE: it
  consumes `kyzo::bench_api` — the sealed door deprecated-sealed.md
  deletes; per bench_api's own entry the Workload constructors and
  Graph shapes land IN benches/, so this file's imports become local
  bench-rig vocabulary rather than a crate façade (the
  bench-internals feature stays the gate).
- **L2:** gold: comparability-across-history as a design constraint
  (the batch tag ruling — bench identity is part of the recorded
  evidence chain, rule #19's ledger discipline); workloads named for
  the cost they isolate. Nothing condemned.

## benches/storage.rs (224 lines; inventory: header, module doc
("designed around the decisions they inform" — the commit-ceiling
question, the SSI scan-tracking question, the asof
earns-its-complexity question, and the ops baseline), the key/
bitemp_key/assert_val builders, `ops` (get hit/miss, put-1k-commit),
`scan_tracking_overhead` (read-tx vs write-tx 10k scans, with the
fresh-write-tx-per-iteration honesty — "read marks accumulate per tx,
and an honest number includes that cost"), `commit_parallel` (fixed
total work split across 1/2/4/8 threads — "if commits applied in
parallel, wall time would fall with N; the ceiling shows as a flat
line"), and `asof` (TWO SHAPES, same total entries — shallow 1000×8
"where naive streaming can win on iterator-setup costs" vs deep
50×160 "where seeking must win — the crossover is the honest
characterization of the seek design"; the in-bench naive
scan-and-filter oracle) — closed)
- **L1:** preserve-in-place → `kyzo-core/benches/`; its store-surface
  imports ride the same public-currency decision as the trials kit.
- **L2:** gold: benches framed as questions with a stated
  interpretation for each outcome (a flat line MEANS the ceiling);
  the crossover-not-victory framing on asof; per-iteration honesty
  notes. Nothing condemned.

## benches/string_eq.rs (271 lines; inventory: header, module doc
(story #119's ">=3x" claim "measured rather than cited"; "this number
goes public — a rigged or unrepresentative bench is worse than none";
the three-distribution design; the BRANCH-AGNOSTIC ruling — "nothing
GermanStr-specific appears in this file's body... the identical file
compiles against origin/main... so criterion --save-baseline main on
the former and --baseline main on the latter measures the real delta,
not two different benchmarks"), the three DISTRIBUTIONS with their
mechanism analysis (long_shared_prefix — head4 never resolves, both
sides pay the tail compare; short_strings — both inline, no
heap-avoidance argument available to either side; early_differ — the
design claim's home turf, with the honesty note that which wins "is
exactly what running this bench answers, not what this comment
asserts"), EQ_SIZES with the flat-line-is-evidence-of-nothing rule,
`bench_germanstr_eq` (the equality sweep against a seeded shuffle;
the sort arm with iter_batched re-cloning "rather than a no-op
re-sort"), and `bench_string_scan` (the applied proof: the NON-KEY
filter target ruling — "filtering on the indexed key instead would
risk the query planner turning it into a point seek that never
compares most rows at all"; the cardinality-preserving self-join
with its still-compares-per-row argument) — closed)
- **L1:** preserve-in-place → `kyzo-core/benches/`.
- **L2:** gold, preserve verbatim: the anti-rigging doctrine stated
  at the head (distributions chosen to include the design's WORST
  cases, reported "honestly even where the win is small or absent");
  branch-agnostic construction as the A/B mechanism; self-diagnosing
  design (a flat line indicts the bench itself); planner-aware
  workload construction. The house standard for a public perf claim
  (rule #19 in bench form). Nothing condemned.

## examples/language_tour.rs (371 lines; inventory: header, module doc
(story #73 — "this file is not narration ABOUT the language, it is a
KyzoScript program that the engine executes... A comment describing a
construct this file doesn't also exercise is a defect (CLAUDE.md: doc
drift), so every claim below is load-bearing"; each chapter is a
#[test] AND a fn main calls "so CI keeps every example honest without
a second copy of the scripts to drift from the first"), the fresh-db
fixture with the leaked-tempdir ruling, and eight chapters in the
newcomer's order: relations (`=>` key/dependent split, the no-SELECT
body-atom read); rules (shared-variable-IS-the-join, named rules
called like functions); recursion (fixpoint as "Datalog's answer to
WITH RECURSIVE, and it costs no extra syntax"); aggregation (implicit
grouping over bare head variables); time travel ("now is simply the
default when @ is omitted, not a distinct write mode"; as-of reads at
two instants); vector search as a join; FTS in the same shape; and a
built-in graph algorithm through `<~` — plus main and the eight test
wrappers — closed)
- **L1:** preserve-in-place → the target `kyzo-core/examples/` as the
  public surface's teaching document (story #73's deliverable); like
  the tests/ suite it is a drift detector for the crate split's
  re-export decision.
- **L2:** gold, preserve verbatim: executable-documentation doctrine
  (every prose claim load-bearing under CI); the chapter order as the
  designed learning path; teaching prose that states the design
  argument at each construct (time-as-parameter, search-as-join).
  Nothing condemned.

## kyzo-bin/ — the native host against the map's door layout
The map keeps the crate and renames doors: client.rs → repl/fetch.rs,
relations.rs → bulk.rs (the shared codec), repl/output.rs →
repl/render.rs, server/changes.rs + server/standing.rs → server/feeds.rs,
server/pages.rs → server/console.rs; main/auth/query/bulk/rules keep
their names, engine.rs is a lawful unlisted file (zones are stable,
files grow). Entries below are per-file; the zone law (zone-bin.md)
adjudicates quality.

## kyzo-bin/src/main.rs (72 lines; inventory: dual header (dead rouille
server_main dropped; `exit(-1)` → `exit(1)` with the 255-reads-as-crash
reasoning; errors printed `{e:?}` so "miette's fancy Debug rendering...
is what a user sees"), the `#![forbid(unsafe_code)]` with its
check-script note, clap AppArgs/Commands, and main (server under a
tokio runtime; repl with the exit-1 error path) — closed)
- **L1:** preserve-in-place → the map's `main.rs`. Watch: the map wants
  "typed config lifted from args/env — malformed config is a typed
  refusal"; clap already delivers that for args.
- **L2:** gold: forbid at the second crate root; exit-code semantics
  reasoned. Nothing condemned.

## kyzo-bin/src/engine.rs (103 lines; inventory: header, module doc (five
upstream backends → ONE; "what upstream's mem engine bought a caller
was 'no persistence, no path to manage'; that is reproduced here as an
ephemeral fjall store... because a second backend is not what the
feature ever meant"), `StorageArgs` (clap-flattened), the
StorageOptions conversion (memtable/table sizes deliberately not
CLI-exposed), `DbHandle` (the keep-a-clone-before-Db::new ruling — "Db's
storage field is crate-private by design"; the held ephemeral dir), and
`open` (mem/fjall dispatch; unknown engine "a refusal, not a silent
fallback" with a teaching message) — closed)
- **L1:** preserve-in-place (lawful unlisted bin-zone file; main.rs's
  config-lift companion).
- **L2:** gold: the mem-means-ephemeral-not-a-backend ruling; the
  clone-don't-widen-the-contract pattern for storage access. DOC NIT:
  the module doc cites "data/memcmp.rs's engine choice" — a path that
  no longer exists in the tree (the value plane superseded it);
  re-point at the storage contract on migration. Nothing condemned.

## kyzo-bin/src/client.rs (346 lines; inventory: header, module doc (the
upstream file was "an empty, licensed placeholder — never referenced by
anything"; this is NOT a port; the TWO DISCARDED DESIGNS recorded with
load-bearing reasons — minreq/rustls-ring rejected because ring "shells
out to cc" (verified via cargo tree), ureq+global-provider rejected
because it "depended on a GLOBAL default... an invisible dependency
between two unrelated modules"; what's here: hand-rolled HTTP/1.1 GET
over TcpStream + rustls with the rustls-rustcrypto provider "passed
explicitly into this module's own ClientConfig, not installed anywhere
globally"; "proven end-to-end against a live server before being wired
in"), MAX_REDIRECTS=5, `get`/`fetch` (redirect resolution with a
no-Location typed refusal; non-2xx refused), `Target`
(parse/resolve_redirect/connect with webpki roots), `Transport`
(Plain/Tls sharing every line of fetch), header/body readers
(content-length, chunked with trailers, read-to-end justified by
Connection: close), and six tests (URL parsing incl. defaults and
refusals; redirect resolution; content-length and chunked bodies over
byte fixtures) — closed)
- **L1:** preserve-and-move whole → `repl/fetch.rs` (the map's "%import
  egress" line).
- **L2:** gold, preserve verbatim: discarded designs recorded WITH
  their verification commands (the pure-Rust invariant enforced at
  dependency-selection time); explicit-provider over global-install;
  protocol parsing tested over byte fixtures. Nothing condemned.

## kyzo-bin/src/relations.rs (177 lines; inventory: header, module doc
(upstream reached into live-transaction storage; here composed "from
the same public entry point every other caller uses" — ::columns + a
scan + a $data-bound mutation, "every step is a real query the engine
already proves sound"; the SPLICE-VALIDATION doctrine — every
relation/column name entering composed KyzoScript is validated against
the grammar's compound_ident shape, with the honest threat model: "a
caller reaching either endpoint already has full script privilege...
this is not privilege escalation either way; it closes off a name
smuggling extra script syntax"), `validate_identifier`, `columns_of`,
`export_relations`, `import_relations` (the `-`-prefix :rm convention;
per-column validation "the same grammar-shaped validation applies"),
and three tests incl. the INJECTION corpus (ten hostile "names" each
trying to close the pattern early, start a second statement, or escape
the identifier position) — closed)
- **L1:** preserve-and-move whole → `bulk.rs` (the map's "one
  bulk-movement codec (export/import), shared by both doors" — this
  file already IS that shared codec; server/bulk.rs and %import both
  route through it).
- **L2:** gold: compose-through-the-public-door instead of asking the
  engine for a private one (zone-bin's forbidden-imports law obeyed
  under pressure); splice validation with an honest threat model and
  an injection test corpus. Nothing condemned.

## kyzo-bin/src/repl/mod.rs (164 lines; inventory: dual header
(rhn's authorship preserved; the flat repl.rs split by concern; the
behavior-change ledger — no %eval "dropped rather than faked with a
wrapper query, which would silently change its semantics";
%backup/%restore now the whole-store dump; Ctrl-C "no longer claims to
kill a running query" — ::kill isn't landed, so "rather than port a
loop that iterates nothing and calls a script guaranteed to fail, the
handler says plainly what it can't do yet"), ReplArgs, `repl_main`
(the honest Ctrl-C handler; history load/save; double-Ctrl-C exit),
and `process_line` (%-dispatch vs script, both rendered through
output::render) — closed)
- **L1:** preserve-in-place as the repl/ root (the read-eval-render
  loop). OPERATOR-VISIBLE carried item: the Ctrl-C handler and
  ::running/::kill stubs discharge together when session/jobs.rs
  lands — the map's jobs door is the fix's seat.
- **L2:** gold: dropped-not-faked and says-what-it-cannot-do as
  ledger discipline; errors rendered with spans intact (zone law).
  Nothing condemned.

## kyzo-bin/src/repl/editor.rs (76 lines; inventory: dual header (rhn
authorship; split rationale; the `Completer::update` unreachable!()
REMOVED with the invariant-lives-in-a-different-method argument — "a
future change adding real completions here would have to remember to
touch this one too, silently, or reintroduce a panic reachable from a
keypress"), the `Indented` helper (Hinter/Highlighter/Completer
trivially, the continuation Validator — starts-with-space continues
until a blank line) — closed)
- **L1:** preserve-in-place → the map's `repl/editor.rs`.
- **L2:** gold: the cross-method-invariant analysis behind removing an
  unreachable!() (law 5 applied to a keypress path). Nothing
  condemned.

## kyzo-bin/src/repl/commands.rs (164 lines; inventory: dual header (one
function per command replacing nine inline arms; %import grew https
"client.rs now provides one"; unrecognized %foo "falls through to a
named error" instead of handing the % to the script parser), module
doc, `dispatch` (pure routing), and the nine commands (set/unset/
clear/params over the session param map; backup/restore over
dump_storage; save arming the next render; run over a script file;
import over http(s)/file through client::get + relations::
import_relations) — closed)
- **L1:** preserve-in-place → the map's `repl/commands.rs` ("each an
  enumerated deliberate surface" — this file already is that).
- **L2:** gold: routing/behavior separation; named refusals for
  unknown commands. Nothing condemned.

## kyzo-bin/src/repl/output.rs (70 lines; inventory: header, module doc
("the one place in the REPL that turns a NamedRows into something
shown to the user" — contrasted with the machine-to-machine JSON that
goes through kyzo-core's own envelope), `render` (save-arm consumed or
table), `save_to_file` ({column: value} records), `print_table`
(prettytable, borderless format) — closed)
- **L1:** preserve-and-move → the map's `repl/render.rs` ("envelope →
  human: tables and readable errors, adding no meaning").
- **L2:** gold: one-rendering-site discipline; no meaning added
  (zone law's rendering clause satisfied). Nothing condemned.

## kyzo-bin/src/server/mod.rs (304 lines; inventory: dual header — the
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
- **L2:** gold, preserve verbatim: the dropped-not-stubbed ledger
  (each upstream feature's absence argued from a contract, with the
  fix's real seat named); the per-route body-limit DoS fix; the
  C-dependency exclusion verified not asserted. Zone-law NOTE:
  server_main's startup unwraps/panics (bind parse, listener, token
  file write) are process-entry failures, not request paths — lawful
  under the zone's no-panic-escapes-a-HANDLER clause, but the map's
  "malformed config is a typed refusal" line wants the bind/port
  parse lifted to a typed refusal on arrival. Nothing condemned.

## kyzo-bin/src/server/auth.rs (125 lines; inventory: dual header
(mechanism here, account in server/mod.rs; the token-table name "an
operator-supplied --token-table value, not per-request attacker
input" still validated like relations.rs's splices), module doc (the
gate every route except / passes; localhost skips), `MyAuth`
(`token_table_authorizes` — "a broken auth check refuses, it never
fails open", invalid relation name = operator misconfiguration
logged and refused), and the AsyncAuthorizeRequest impl (header,
?auth= query fallback, Bearer-token table check; 401 otherwise) —
closed)
- **L1:** preserve-in-place → the map's `server/auth.rs` ("the gate
  every route passes; the route table is the attack surface").
- **L2:** gold: never-fails-open stated and implemented; splice
  validation reused. ZONE-LAW FINDING (operator-visible): the 401
  arm builds its response with `Response::builder()...unwrap()` — an
  `unwrap` ON THE REQUEST PATH (zone-bin forbids exactly this).
  It is infallible today (static builder inputs), but the law is
  mechanical; replace with an infallible constructor or a
  const-built response on arrival. Nothing else condemned.

## kyzo-bin/src/server/query.rs (39 lines; inventory: header, module doc
(all JSON shaping is run_script_json; "this handler only runs it off
the async runtime's blocking pool and maps the envelope to an HTTP
status"), QueryPayload, text_query (spawn_blocking + wrap_json/
internal_error) — closed)
- **L1:** preserve-in-place → the map's `server/query.rs` ("execute:
  script + params in, envelope out").
- **L2:** the thinnest possible door — exactly what zone-bin demands.
  Nothing condemned.

## kyzo-bin/src/server/bulk.rs (112 lines; inventory: dual header (the
/import-from-backup absence deferred to mod.rs's doc), module doc
(HTTP plumbing around crate::relations), export_relations (comma-split
path param → relations::export), import_relations (JSON-object
refusal; per-relation NamedRows::from_json), backup (dump_storage) —
all through spawn_blocking with Ok(Err)/Err(join) split — closed)
- **L1:** preserve-in-place → the map's `server/bulk.rs`, consuming
  the shared codec at its bulk.rs seat.
- **L2:** clean plumbing; refusals rendered as 400 envelopes.
  Nothing condemned.

## kyzo-bin/src/server/changes.rs (81 lines; inventory: dual header
(single-argument register_callback matching the port's signature; the
PANIC FIX — upstream's `Event::json_data(item).unwrap()` "panicked the
whole per-connection async task... that panic is user-reachable (any
change to a watched relation drives it)"; now logs and ends the
stream), module doc, observe_changes (register_callback bridged
through a blocking forwarder into a tokio channel; the Drop `Guard`
unregistering the callback; the SSE loop with encode-failure
log-and-break; KeepAlive) — closed)
- **L1:** preserve-and-move → `server/feeds.rs` (merging with
  standing.rs per the map's one-feeds-door line).
- **L2:** gold: the user-reachable-panic analysis; prompt
  unregistration by Drop guard (the engine's lossy-by-disconnect
  contract respected — the host never buffers beyond the channel).
  Nothing condemned.

## kyzo-bin/src/server/standing.rs (195 lines; inventory: header, module
doc (the per-query analog of /changes one tier up; the pull-model
tradeoff "carried through here rather than inventing a second drive
model at the HTTP tier"), StandingQueryParams (params through "the
SAME JsonValue → DataValue conversion run_script_json uses... never a
second hand-rolled one"), tuple/signed-fact JSON shapers, the
`TeardownGuard` (Option-wrapped because teardown takes owned self —
the Drop-through-take pattern explained), and observe_standing
(refused registrations rendered "as a clean 400, never a panic or a
hung connection"; the init snapshot event; the 150 ms poll loop with
guard-take/spawn_blocking/guard-restore choreography; the repeated
crate-boundary clippy-allow with its lib.rs cross-reference;
encode-failure log-and-break) — closed)
- **L1:** preserve-and-move → `server/feeds.rs` beside changes.rs.
- **L2:** gold: no-second-conversion and no-second-drive-model
  disciplines (host adds no semantics — the zone's core law); the
  teardown-through-Drop promptness argument; deltas streamed exactly
  as SignedFacts (guarantees intact). Watch: the 150 ms poll is a
  host-chosen constant — when react/feed.rs lands a push surface,
  this door should consume it rather than poll. Nothing condemned.

## kyzo-bin/src/server/rules.rs (159 lines; inventory: dual header
(std::mpsc following the port; the same json_data panic fix as
changes.rs "both sites here now log and end the stream"), module doc
(the downstream-computed fixed-rule bridge: register + SSE of
invocation requests; POST/DELETE /rule-result/{id} answers),
post_rule_result (malformed downstream result unblocks the waiting
rule with a typed error), post_rule_err (downstream cancellation),
register_rule (rule_with_channel; the rayon forwarder assigning ids
and parking senders; the Drop Guard unregistering; register-error
rendered as an SSE event) — closed)
- **L1:** preserve-in-place → the map's `server/rules.rs` ("extend:
  downstream-computed fixed rules bridged to the engine").
- **L2:** gold: the malformed-result path UNBLOCKS the engine-side
  waiter (a stuck rule would hang a query); register-refusal as a
  first-class stream event. ZONE-LAW FINDING (same class as
  auth.rs): `st.rule_senders.lock().unwrap()` at three request-path
  sites — a poisoned mutex panics the handler; the zone law wants
  the poison branch rendered as the 500 envelope instead. Nothing
  else condemned.

## kyzo-bin/src/server/pages.rs (30 lines; inventory: dual header,
module doc ("the two routes that aren't API calls"), root
(include_str! console page), not_found (JSON 404) — closed)
- **L1:** preserve-and-move → the map's `server/console.rs` (with
  src/index.html riding along).
- **L2:** nothing condemned.

## kyzo-bin/tests/repl_smoke.rs (98 lines; inventory: header, module doc
("cargo check proves the CLI COMPILES; it does not prove the binary
RUNS" — the real built binary via CARGO_BIN_EXE_kyzo, piped stdin,
"the end-to-end path an actual user takes, which no in-process test
exercises"), two tests: the script-through-the-REPL run (banner +
three rendered rows + clean exit, hermetic mem engine, temp cwd
keeping the history file out of the tree) and
`repl_survives_a_bad_script` (reported on stderr, exit still clean —
"the CLI's error path is wired to the engine's typed refusals, not to
a panic") — closed)
- **L1:** preserve-in-place as the bin crate's integration test; on
  the map's layout it gains a sibling for the server door (the route
  table has no equivalent spawn-the-binary proof — a named gap for
  the bin zone's own backlog, recorded here, not fixed).
- **L2:** gold: compiles-vs-runs distinction driving a real-process
  test; survival-of-bad-input as an explicit exit-code claim.
  Nothing condemned.
