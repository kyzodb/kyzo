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
