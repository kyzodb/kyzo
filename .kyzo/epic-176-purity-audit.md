# Epic #176 — Max-purity work list (decisive)

Verified at cited lines. Meter: types sole authority. No DEFER — every purity hole found under this epic's outcome stays on this list. No planning residual.

Format: `ID | defect | decisive fix | done-when`

---

## KEEP — do this (ordered)

### A. Validity / time authority (blockers)

P001 | `ValidityTs(pub Reverse<i64>)` + public `from_raw` admits reserved terminal (`validity.rs`) | Private field; mint only via `for_assertion` and a storage-decode door | Terminal assert-tick unconstructible outside named doors; compile-fail on field/struct literal
P002 | `Validity::new` accepts any `ValidityTs` including reserved assert tick | Consume sealed assert-coordinate type (or refuse reserved inside `new`) | Assert+`i64::MAX` unrepresentable as `Validity`
P003 | `AsOf { pub valid, pub sys }` forgeable pairs | Private fields; only `current` / checked constructors | Field poke does not compile
P004 | `StoredValiditySlot(pub ValidityTs)` bypasses `::new` | Private field; only `StoredValiditySlot::new` | Pub tuple construction gone
P005 | `op_validity` / `data_value_to_vld_spec` use `from_raw` (user path) | Route through `for_assertion` | User int → reserved Validity is typed refusal
P006 | RA temporal `SpansRA.sys` / `RawVersion::{valid,sys}` bare `i64` | Carry `ValidityTs` / stored slots | Bare i64 time fields gone from RA temporal

### B. Identity / admission half-theorems (blockers)

P007 | `RelationId::next` skips CAP (`row.rs`) | Refuse `>= CAP` same as `new`/`raw_decode` | `CAP-1.next()` is `None`; over-cap id unconstructible
P008 | Empty projection → `Arity::ONE` fabrication (`exec.rs`) | Typed refuse empty projection; never invent width | Empty `out` cannot yield ExecRows
P009 | Domain visibility cut is `assert!` (`column.rs`) | Cut overflow → `Denial` like arena/epoch | Reachable cut mismatch is typed refusal; panic tests rewritten
P010 | Snapshot cut overflow is `assert!` (`arena.rs`) | Cut overflow → `Denial` | Same
P011 | Frame bounds after typed Denial still `assert!` (`arena.rs`) | Bounds failure → `Denial` | Same
P012 | `Rows::push_row` arity mismatch panics | Typed `Denial`/`PushError` | Wrong-width push does not abort process
P013 | `Domain::absorb_stamp` `raw + 1` wraps at `u32::MAX` | `checked_add` / named overflow refusal | Extent wrap unrepresentable
P014 | `ExecDedup` accepts bare `&[u32]` | Admit only domain-proven codes/keys | Illegal codes not insertable under Domain stamp
P098 | Authority-baseline floors: `missing-authority`, `searchra-decoded-tuples`, freshness-twin (was D002 / F-FIX-034) | Mint Catalog/Index/RelationGeneration + QueryDomainAdmission + ResidentIndexKey; SearchRA admits codes not decoded `Vec<Tuple>`; close twin | `authority-baseline.json` floors for those classes at 0
P099 | `Generation::new(raw)` public mint (was D009 / F-ESR-014) | Mint only via CatalogGeneration authority | Public raw `Generation` mint gone

### C. Geo / HNSW / constraint deferred proof (critical — purity NOW)

P015 | `GeoPoint` docs claim admit-only; `pub(crate)` fields forge NaN | Private fields; only `admit` constructs | Struct literal / field write does not compile
P016 | `BoundingBox` same pub-field bypass | Private fields; only `admit` | Same
P017 | HNSW `index_filter: Option<String>` + mutate clones String after compile | Persist compiled/typed filter substance; delete String residual | Manifest cannot hold raw filter source
P018 | `pending_constraints: BTreeMap<…, String>` + reparse cache | Typed/compiled constraint substance | Raw constraint source unstorable
P019 | Catalog/session still holds re-parseable String bodies for constraints (trigger path fixed; constraint not) | Same typed-substance door as triggers | Grep: zero raw constraint source fields in non-test runtime
P020 | `HnswIndexManifest` all-pub + serde admits `vec_dim=0` / empty fields outside parse | Private fields; parse/admit-only mint | Illegal manifests unconstructible
P021 | `m=1` legal → `level_multiplier = Inf` | `MNeighbours` newtype `m >= 2` (or refuse Inf) | Inf multiplier unrepresentable
P022 | Sparse `admit_sparse` returns `Vec<(u32,f32)>` | Return sealed `SparseVector` | Illegal sparse not type-equal to admitted
P100 | `Vector` components bare `f64` after door (was D008 / F-VAL-046) | Proven component/dimension newtypes after content-addressed identity door | Bare `f64` after door gone

### D. Query algebra / batch / meet (high)

P023 | Independent `Semiring`×`Annotation`; mismatch `unreachable!` | Sealed product types (`BooleanAnn` / `TropicalAnn`) | Kind mismatch does not compile; delete unreachable arms
P024 | `Batch` misaligned values/offsets constructible; unchecked `row` | Opaque Batch + transactional push; OOB typed refuse | Illegal Batch unconstructible
P025 | Meet still `meet_key_positions().expect` Option residual | Pass `key_positions` by value from `HeadAggrKind::Meet`; delete Option getter | No Option/expect on Meet path
P101 | Meet `key_positions: Vec<usize>` lacks HeadPos newtype (was D014 / F-QRY-016) | `HeadPos` proven newtype at Meet/head boundary | Bare `usize` head positions gone
P026 | Dual `filters` + `filters_bytecodes` | Single compiled-filter field; fill consumes into it | One owner; twin field gone
P027 | Levels `Vec<(bool,bool)>` skip/refresh | Named flag/sum type; illegal pairs unrepresentable | Bool pairs gone
P028 | `LevelBoundKey`/`LevelArenaBytes` `From<Vec<u8>>` | Remove `From`; mint only via encode doors | Arbitrary bytes not a bound key
P029 | Level arena `len as u32` truncates | Checked `u32` / refuse past `u32::MAX` | Silent trunc gone
P030 | `levels.last().expect` — empty stack representable | Non-empty level-stack type | `last` needs no expect
P031 | `NormalLevel::row` / `Segment::row` unchecked OOB | `Option`/`Result` with bounds proof | OOB not process abort
P032 | `UnificationRA.is_multi: bool` | `Single \| Spread` sum | Flag×expr illegal combos gone
P033 | `Literal.negated: bool` | Positive/negative sum type | Bool polarity gone
P034 | RA join/temp `unwrap` after control-flow Some; search `unreachable!` parent batch; incremental `unreachable!` after `is_const` | Typestate / exhaustive enums so paths are unrepresentable or typed refuse | Those unwrap/unreachable sites gone
P035 | `compose` `_ => Minus` collapses non-unit nets | Exhaust `{0,±1}`; else named refuse/invariant | Silent Minus on bad tally gone
P036 | Meet `by_row` twin of `by_group` | Single owner; derive scans from `by_group` | Twin map gone
P037 | Premise/rule identities as `String` in eval errors | `Symbol` / `MagicSymbol` | String rule ids gone on those types
P102 | Domain/error display `String` where Symbol exists at edge (was D013 / F-QRY-027) | `Symbol` / `MagicSymbol` at that edge | String identities gone there
P038 | HNSW bind flags dual-encoded bools + bindings | One bind encoding in the type | Dual bool pack gone
P039 | VM `row as u32` truncation | Proven row index / checked cast | Truncating cast gone
P040 | Semiring/error String payloads (`BadCertificate`, etc.) | Structured refusal enums | Bare String reason fields gone there
P103 | ProjectionKind/Sealed `search` is k-bound façade; real search free fns (was D001 / F-ESR-008/009/046/047) | Sealed/ProjectionKind owns real engine search; free-fn dual deleted or behind one trait seam | Search not a façade; free-fn dual gone
P104 | DerivationGraph free construct + bare ProofNode indices (was D011 / F-QRY-003/004) | DAG-by-construction; indices proven | Cycle/illegal proof graph unconstructible
P105 | `DeltaRA.posting: Option` Naive vs accelerated (was D012 / F-QRY-022) | Sum type `Naive \| Accelerated` | `posting: Option` gone
P106 | Segment cache as independent truth vs relation projection (was D015 / F-QRY-031) | One staleness vocabulary (CatalogGeneration/meaning clock); cache cannot own truth | Independent cache-as-truth gone

### E. Value/schema/aggr representation

P041 | `NumericOrd(pub Num)` | Private field; wrap only through door | Pub field gone
P042 | `TupleKey(pub(crate) Vec<u8>)` | Private; mint only `from_values`/`from_stored` | Unvalidated crate mint gone
P043 | `ByteLen`/`ByteOff`/`ChunkId::from_usize` `expect` | `TryFrom`/`Option` typed refuse | Panic construction gone
P044 | `Arity::new_unchecked` panic escape | Delete; only `NonZeroUsize`/`try_new` | Escape hatch gone
P045 | Vector encode `assert!` on dim | Typed encode refuse / proven dim | Panic path gone
P046 | Remap `Code((r+lo) as u32)` unchecked | `checked_add` into `Code` | Truncating remap gone
P047 | `NullableColType` pub fields + bare nullable bool | Private + sum/newtype nullability | Open field poke gone
P048 | `ColType` bare `usize`/`Option<usize>` lengths | Proven length newtypes | Bare lengths gone on schema
P049 | `Binding.tuple_pos: Option<usize>` deferred resolve | Typestate unresolved vs resolved Expr | Option pos gone
P050 | `Op` open `deterministic`/`vararg` bools | Seal via `define_op!` / private fields | Open bool Op mint gone
P051 | `ExprDe` omits `Lazy` | Add Lazy to wire or refuse serialize Lazy | Round-trip total
P052 | Bitemporal polarity/key `bail!(String)` | Named decode/polarity refusal types | String bail gone on that path
P053 | `JsonData(pub JsonValue)` | Private field; only `new`/`value` | Pub field gone
P107 | `json_to_datavalue` twin in jlines (was D004 / F-FIX-029) | One decode door; twin deleted or private re-export of kernel | Single json→DataValue authority
P054 | `Adornment = Vec<bool>` | Bound/free adornment type | Bool vector adornment gone
P055 | Head `aggr: Vec<Option<…>>` misalignable | Structured aligned head/aggr slots | Option-hole vector gone
P056 | Aggr “no cost yet” via `DataValue::Null` | `Option`/`Empty` lattice | Null not absence sentinel
P057 | `AggrMinCost` empty via `f64::INFINITY` | `Option`/`Empty` | Infinity sentinel gone
P058 | TDigest empty via `f64::NAN` min/max | `Option`/`Empty` | NaN absence gone
P108 | HLL estimate bare `f64` (was D007 / F-VAL-043) | Proven estimate newtype | Bare `f64` estimate gone
P059 | Arrow offsets `cur += len as i32` unchecked | Checked i32 math / typed refuse | Unchecked offset math gone
P060 | `PlannedColumn.nullable: bool` | Nullability sum/newtype | Bare bool gone
P061 | `ColumnBatch::from_rows` `.take(arity)` truncates | Refuse wrong-width rows | Silent truncate gone
P062 | Num key `(e+EXP_OFFSET) as u16` debug_assert only | Prove range + `try_into` | Release trunc gone
P063 | `format_error_as_json` `expect` | `Result` on diagnostic path | Panic diagnostic gone
P064 | `InvalidRegex(pub String)` | Structured parse-refusal type | Pub String error gone

### F. Engines / text / LSH / storage clock

P065 | Empty `FtsExpr::And/Or(vec![])` constructible | Non-empty constructors / flatten refuse | Empty And/Or unrepresentable
P066 | `FtsLiteral` / Token offset pubs admit illegal ranges | Private fields; checked constructors | `offset_from > offset_to` unrepresentable
P109 | TokenizerConfig deferred name proof (was D010 / F-ESR-013) | Unknown names unstorable until admitted (typed config/pack door) | Deferred name proof closed
P067 | LSH `HashPermutations`/`HashValues`/`LshPermutationBytes` pub Vec + From | Private; length law at mint; typed from_bytes refuse | Arbitrary Vec not a permutation set
P068 | LSH/from_bytes and index lifecycle errors as `String` | Named refusal enums | Stringly lifecycle/decode gone
P069 | SearchParams `bind_*: bool` packs | Bind sum/struct without free bool soup | Illegal bind flag combos gone
P070 | `IndexKind` `unreachable!` / non-exhaustive else | Exhaustive Plain vs manifest split | Unreachable arm gone
P071 | SystemClock `last + 1` unchecked | `checked_add` / refuse at `i64::MAX` + `INVARIANT` | Overflow panic/wrap gone
P072 | `op_rand_vec` `get_int()? as usize` (negative → huge) | Non-negative length type before allocate | Negative length cannot allocate
P073 | HNSW `VectorId.sub` Option vs wire `-1` sentinel dual | One absence encoding (Option only through codec) | Sentinel dual gone
P074 | Stale module docs claiming raw triggers / wrong fire story (`relation.rs`) | Docs match typed substances | Doc claims match types
P075 | Runtime JSON `expect` on into_json object shape | Typed refuse / infallible by construction | Expect gone
P076 | `storage/retry` `last_err.expect` if `max_attempts==0` | NonZero attempts type | Zero-attempt config unrepresentable
P077 | `IndexRowCorrupt.reason: String` | Named corrupt-reason enum | String reason gone
P110 | URL fetch `SEAM(network)` stubs (was D005 / F-FIX-032) | Typed refuse only; no untyped fetch side door (engine does not grow HTTP) | Network fetch not an open seam
P111 | Conflict detect via `err.code()` string (was D016 / F-ESR-039) | Typed `Conflict` refusal on store layer | String code branch gone

### G. Fixed-rule / parse casts / public APIs

P078 | Dijkstra/APSP `u32::MAX` predecessor sentinel | `Vec<Option<NodeId>>` (or sum); never reserve MAX as absence | Sentinel absence gone; MAX free for node ids if needed
P079 | ~20 production Structural `.unwrap()`s across bfs/dfs/astar/yen/max_flow/dijkstra/cliques/random_walk/csv/reorder_sort | Typed refusal / Option preds; replace “Structural:” with real `INVARIANT(name):` only where proof is sound | Production unwraps on those paths gone
P080 | Parse/fixed_rule `i64 → usize/u32/u64` after sign-check only (limit/offset/n_gram/dim/ef/m/NEAR/list len/float→i64/radix→u32) | `try_from` / fit checks after proof | Truncating/wrapping casts gone
P081 | `KillRunning(i_val as u64)` negative wraps | Non-negative `ProcessId`; refuse negatives | Negative PID unconstructible
P082 | `NamedRows` pub fields; `new` does not prove arity | Private fields; prove header↔row arity at door | Illegal NamedRows unconstructible
P083 | `SimpleFixedRule` `Box<dyn Fn…>` erased owner | Typed `FixedRule` impls only | Dyn Fn rule owner gone
P084 | `Goal::visit(&mut self)` drains in place | Consuming Goal transition | Non-consuming drain gone
P085 | Constant proof re-owned by map + re-validate | Sealed `ConstantData` after init | Second validation owner gone
P086 | `FixedRule::init_options(&mut BTreeMap)` | Consuming normalize returning new map | In-place option rewrite gone
P087 | `into_store` false `dead_code` / “when query lands” | Remove lie; normalize already calls it | Allow/doc match reality
P112 | `allow(dead_code)` on format/parse residual consumers (was D006 / F-FIX-030) | Wire behind typed host doors or delete | `dead_code` allows gone or named residual with door
P088 | Format `unreachable!` on Expr arms | Spanned `GrammarShapeError` | Unreachable format arms gone
P089 | Schema default `ColType::Any` as unset | Explicit unset typestate before parse complete | Bare Any-as-unset gone
P090 | `NoStoredInputs` residual placeholder after SessionView | Demolish condemned seam | Placeholder gone
P091 | Fuzz/interval: `interval_bounds` projects Interval as `Option<(i64,i64)>` | Keep Interval opaque in fuzz API or typed accessors only | Bypass projection gone (if kept in fuzz surface)
P113 | Fuzz Interval `Option<(i64,i64)>` bypass if still present after P091 (was D017 / F-FIX-033) | Same opaque-Interval law as P091 | Bypass projection gone
P092 | Parse path `HnswConfigBuilder.dim(0)` weaker than parse refuse | Builder setters enforce same law as parse (`dim>=1`, `m>=2`) | Illegal builder states unconstructible
P114 | `CancelFlag` AtomicBool vs Cancelled typestate (was D003 / F-FIX-026) | Consuming `Cancelled` typestate (or equivalent budget lifecycle) | AtomicBool cancel flag gone

### H. Smaller query/runtime keeps

P093 | `RegularTempStore` limiter-skip bare `bool` | Named skip token/enum | Bare bool gone
P094 | Temp-store decode `expect("own bytes")` | Typed corrupt refuse (or prove by type after seal) | Expect-on-decode gone or `INVARIANT` at unsafe rung
P095 | Levels/store expect cluster already partly in P030–P031 — include remaining compiled-position expects | Same | Same
P096 | Eval header stale MeetNotSuffix claims | Delete/fix comments to match Meet enum | Comments not a second law
P097 | Countdown/`u32` decrement in eval — only if unnamed; prefer checked or proven stride type | Checked or proven | Unnamed wrap risk gone
P115 | Verify intern `OnceLock<HashSet<&str>>` bridge (was D018 / F-ESR-048) | Typed proof bridge | String-set intern bridge gone

---

## DROP — not a purity hole (verified)

X001 | `Code(pub(super) u32)` / `Epoch(pub(super))` | Plane-internal; Arena/`StampedCode` mint
X002 | `RegexSource::from_stored` without re-parse | Intentional total decode; proof at write/compile
X003 | `Value::tag` expect / saturating_sub inline trailer | Mint law; no open forge door
X004 | `DataValue::Ord` ↔ codec dual | Intentional; property-tested equal
X005 | Meet `update(&mut MeetAccum)` + change bool | Intended delta algebra
X006 | AsOf test construction (F-VAL-033) | Duplicate of P003
X007 | Fjall commit `Option::take`+expect / Drop-bomb Open | Intentional consuming WriteTx protocol
X008 | Doc-only `is_temp` lag in temp.rs/db one-liners where code already uses residency | No type hole
X009 | Parent F001–F008 duplicates of P0xx | Merged into KEEP above
F-QRY-030 | Duplicate of P023 | Merged
F-FIX-003..013 | Merged into P079 | Cluster
F-FIX-016..023 | Merged into P080/P081 | Cluster
F-ESR-029 | Merged into P020/P021 | Cluster
F-VAL-001..004 / F002–F003 parent | Merged into P001–P004 | Cluster

### Former DEFER → KEEP

| Was | Now | Section |
|-----|-----|---------|
| D001 | P103 | D query / search ownership |
| D002 | P098 | B admission / authority baseline |
| D003 | P114 | G cancel typestate |
| D004 | P107 | E json door |
| D005 | P110 | F network seam |
| D006 | P112 | G dead_code residuals |
| D007 | P108 | E HLL estimate |
| D008 | P100 | C vector components |
| D009 | P099 | B Generation mint |
| D010 | P109 | F tokenizer config |
| D011 | P104 | D DerivationGraph |
| D012 | P105 | D DeltaRA posting |
| D013 | P102 | D error identity |
| D014 | P101 | D Meet HeadPos |
| D015 | P106 | D segment cache |
| D016 | P111 | F conflict string |
| D017 | P113 | G fuzz Interval |
| D018 | P115 | H verify intern |

---

## Counts

| | |
|---|---|
| KEEP work items | **115** (P001–P097 + P098–P115; former D001–D018) |
| DEFER | **0** |
| DROP / merge | **rest of ~165 raw findings** |
| Planning residual | **0** |

## Done-when for the epic outcome

Completing **KEEP** makes: wrong states in those rows unrepresentable or typed-refused; no half-theorem `assert!` on admission/cut; no raw filter/constraint String; no Semiring×Annotation panic pairing; no sentinel predecessors; no pub-field admit bypass on Validity/Geo/Manifest/NamedRows; authority-baseline floors for missing-authority/searchra/freshness-twin at 0; no deferred parking of type-authority holes found under this outcome.
