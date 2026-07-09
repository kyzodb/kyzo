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
- **query/laws.rs** → `kyzo-oracle/eval.rs`. On arrival it must depend on
  the model ONLY — today it lives beside the evaluator it judges; the move
  is what makes its independence physics. Gains its own expression
  evaluator (today's `unsupported` on expressions is a coverage hole).

## To kyzo-trials (campaigns: public claims, published seeds)
- **query/gauntlet.rs** → metamorphic campaign; reforge to run through the
  public surface only.
- **query/dst_query.rs** + **storage/sim.rs** → the DST drivers, unified
  under one seed discipline.
- **query/provenance.rs** → the provenance trials; its independent-reference
  checkers must arrive sharing nothing with exec's semiring code.
- **query/trials.rs**, **time_travel_script_laws.rs**,
  **time_travel_trials.rs** → the claim campaigns and temporal law
  batteries.
- **jepsen_trials.rs** → `serializability.rs`.
- **storage/conformance.rs** → the public kit; reforge so a stranger's
  backend runs it unmodified.
- **storage/crash_matrix.rs** → the crash campaign, driving kyzo-crashfs.
- **parse/fuzz_tests.rs** → the fuzz drivers and corpus; generative
  machinery becomes trials property.

## To kyzo-model (pure vocabulary: no IO, no evaluation)
- **data/value/{tag,canonical,cell,number,string,prefix,proofs}.rs** and
  **wide/** → `model/value/` (+ `value/kind/`). Already the house standard;
  arrival check is only that no execution import rides along.
- **data/relation.rs** → `model/schema/relation.rs`.
- **data/span.rs**, **data/symb.rs** → `model/program/span.rs`,
  `model/program/symbol.rs` (symb gains its full name).
- **data/json.rs** → `model/envelope/json.rs`; **data/arrow_ipc.rs** →
  `model/envelope/arrow.rs` — codecs as total round-trip views.
- **parse/** (minus fuzz_tests) → `model/parse/`; **kyzoscript.pest** →
  `model/parse/grammar.pest`; on arrival, every parsed-but-unowned grammar
  rule gets an owner or an owned typed refusal.
- **format.rs** (+ its tests) → `model/format.rs`.

## To exec (one currency, one evaluator, deterministic everything)
- **data/value/{arena,code,column,exec}.rs** → `exec/currency/` — the
  currency is engine-internal, not vocabulary; the model/currency cut is
  the arrival check.
- **query/{compile,stratify,magic,graph}.rs** → `exec/plan/`.
- **query/eval.rs** → `exec/fixpoint/eval.rs`; the provenance seams it
  carries are load-bearing and must survive the move proven (the semiring
  trials stay green).
- **query/ra/** → `exec/op/` (`temp.rs` arrives as `delta.rs`, `fixed.rs`
  as `literal.rs`).
- **query/sort.rs**, **query/search.rs** → `exec/`.
- **query/semiring.rs** → `exec/provenance/semiring.rs`.
- **data/aggr.rs** → `exec/fold/aggr.rs`; **data/sketch/** →
  `exec/fold/sketch/` — already deterministic folds; arrival check is
  merge-order invariance stated as law.

## To react (continuity: provably equal to recompute)
- **query/incremental.rs** → `react/incremental.rs`.
- **query/standing.rs** → `react/standing.rs`.

## To project (rebuildable speed, never truth)
- **engines/{hnsw,fts,lsh,sparse,spatial,gazetteer}.rs** →
  `project/{vector,text,dedup,sparse,spatial,text}/` — each arrives into
  the uniform per-engine shape (maintenance, search, law) and the
  projection contract; raw decode errors become typed engine corruption
  on the way in.
- **engines/segments.rs** → `project/current.rs`, reconciled with the one
  residency/generation discipline.
- **engines/text/** tokenizer + cangjie tree — fate pending the
  owned-or-replaced ruling; migrates only if owned to house law (module
  docs, our types) — undocumented foreign code does not cross.

## To store (persistence and nothing else)
- **storage/fjall.rs** → `store/fjall.rs`; **storage/mod.rs** contract →
  `store/contract.rs`.
- **storage/skip_walk.rs**, **storage/retry.rs**, **storage/backup.rs**,
  **storage/temp.rs** (→ `scratch.rs`), **storage/verify.rs**
  (→ `verify_walk.rs`), **storage/merkle.rs**, **storage/tests.rs**.
- **data/bitemporal.rs** → `store/time.rs` — the key law lives with the
  keys.

## To session (the one door)
- **runtime/db.rs** → `session/db.rs`; **runtime/json.rs** →
  `session/json.rs`.
- **runtime/mutate.rs** → `session/admit.rs` — the one admission path,
  named for what it is.
- **runtime/relation.rs** → `session/catalog.rs`; its typed refusals stand
  until the coherent-move story replaces them.
- **runtime/constraint.rs** → `session/constraint.rs`.
- **runtime/verify.rs** → `session/verify.rs` — reforged to summon
  kyzo-oracle instead of an in-crate twin.
- **runtime/callback.rs** → `session/observe.rs`; its feed-shaped parts
  belong to `react/feed.rs`.
