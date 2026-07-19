# Storage-Model Report

Authored by the `storage-architect` agent (Fable) from decisions.md (frozen Spec), the semantic-storage-network-architecture doc, target-arch.md, and the deprecated-*.md migration record. All rulings, QAE triplets, campaigns, and ledgers live inside the instances in this directory — this report is the executive index.

Verification (parent session): all seven instances pass `validate_plan.py --decisions docs/decisions.md` with 0 errors, 0 warnings. Totals: 100 by_target seats, 13 QAE triplets (all `cut_destiny`, each with a recommended A), 25 campaigns_proposed.

## Instances

A = migration (deprecated-*.md re-derived under the law) · B = seat map (storage epic).

- **01-query-tree.json** (A) — `query/{mod,normalize,eval,laws,trials}.rs` → `exec/fixpoint` + `exec/provenance`, `exec/plan/normalize`, `session` (SessionView), `rules/contract` (SessionFixedRule), `kyzo-oracle/{eval,temporal,incremental}`, kyzo-trials lanes.
- **02-runtime-session.json** (A) — `runtime/{relation,mutate,db,db_battery}.rs` → `session/{catalog,access,admit,ops,db,jobs,verify,observe}`; `IndexPositionUse` → `exec/plan/compile.rs`.
- **03-storage-store.json** (A) — `storage/{mod,tests}.rs` → `store/{contract,tx,fjall,backup,verify_walk}`; encoding-law battery → `kyzo-model/format/tests.rs`; DST battery → `kyzo-crashfs/sim.rs`; generic scenarios condemned as superseded by the conformance kit.
- **04-engines-project.json** (A) — `engines/*` → `project/{contract,residency,current,gazetteer,sparse}`; FTS AST pure-data half → `kyzo-model/parse/search.rs` (crate wall).
- **05-data-rules-peels.json** (A) — `data/{program,aggr}`, `data/mod` severance (→ `pub(crate) mod json;` only), empty `data/sketch`, `fixed_rule/*` → `kyzo-model/program/{rule,query NEW-SEATs,aggregate}`, `exec/plan/program`, `exec/fold/aggr`, `rules/{contract,graph_view}`.
- **06-hosts-lsp.json** (A) — kyzo-lsp translate split; `kyzo::lsp_api` door dies; kyzo-core/tests preserve-in-place; kyzo-bin ledger sealed.
- **07-storage-seats.json** (B) — **the mech suit**: 18 NEW-SEATs (`store/{open,authority,epoch,grants,sweep,commit_cap,wal,nonce,seal,objects,transcript,crypto,compact,failure,replica,idempotency}`, `session/{footprint,composition}`) + extensions of 10 existing seats; 39-variant Refused ledger (closed `StoreRefuse` enum); 6-entry Unexposed ledger; 16 type-guarded compile-fail Unconstructibles; 10 verb-hole seated_laws (retry / partial failure / lateness per constructor); 18 campaigns.

## Census closure

Every deprecated-*.md entry is covered by exactly one instance or by the executed move_plan.json v10 (functions bag) — verified against the live tree (query/runtime/storage/engines/fixed_rule condemned files still present; target seats mostly 1-line stubs; oracle crate empty scaffold).

## Five most consequential findings

1. **§1 overrules the map.** target-arch's `session/db.rs` "primary Db capability handle" and kyzo-bin `engine.rs` "live Db" are condemned language — `session/db.rs` becomes the `Engine(Store, Catalog)` composition seat; carried obligation in 02, discharged in 07; no half-rename during migration.
2. **target-arch has ZERO seats for any L1–L14 storage construct.** 18 NEW-SEATs proposed with owns/bans/meters, story-cuttable directly from `by_target`.
3. **Constructor-guard vs type-guard split of the Unconstructible tag.** Spec lines like `PermanenceWitness::mint(cut ≥ expires_at)` can never be compile-fail; 07 states two honest proof forms (trybuild vs private-constructor + DST), foreclosing the epic's biggest claim-inflation vector.
4. **Oracle crate-wall questions RULED.** kyzo-oracle owns `OracleBudget`; real-landed-aggregations survives via an `AggrFold` injection seam (trials binds engine folds); no core→oracle edge ever — production `::verify` uses the `exec/provenance` checker with metered import-independence + a checker-vs-checker campaign.
5. **One commit door during the SweepDoor transition.** SweepDoor lands wrapping today's WriteTx commit as its first StableCommitCap arm; the `Committed` private constructor moves into `store/sweep.rs` in the same story — no flag-gated dual-door window exists.

## Also on record

- NamedRows census-vs-map conflict resolved for the map (`data/json.rs` seat; `data/mod.rs` survives as one glue line).
- `choice_rand` refused at declaration (`UnseededChoiceRand`) instead of migrating dirty.
- Nonce / WriteAuthority / SnapshotFork signatures stay **UNFROZEN** until the two-clone at-rest and live-fork SIV campaigns are green (carried at three seats).
- LSP catalog-door contradiction QAE'd with recommended A (amend zone-lsp to forbid engine internals, not the sealed door).
- Map's duplicated `project/text/tokenizer/` subtree ruled flat-as-law.

## Operator inbox

The 13 QAE triplets across the instances are the only open decisions; everything else arrives pre-made. Doubt about frozen law appears only as campaigns_proposed (25), per the freeze.
