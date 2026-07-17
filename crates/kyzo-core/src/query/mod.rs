/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The Datalog query engine. Its purpose, as the standard every part is
//! judged against: a declarative query over any shape of data returns
//! exactly what the logic says it must — whatever the plan, parallelism, or
//! optimization. Clever execution must be invisible.
//!
//! ## The engine laws, and where each is enforced
//!
//! 1. **Answer correctness** — optimized evaluation (semi-naive, magic-sets)
//!    produces exactly the naive fixpoint of the logic program, aggregation
//!    included: normal aggregations group and fold at the fixpoint of the
//!    strata beneath them, meet aggregations fold *inside* recursion, and
//!    fixed rules run once on stratum boundaries.
//!    *Enforcement:* differential tests against the naive reference
//!    evaluator in [`laws`] (the oracle is deliberately unoptimized and
//!    obviously correct, and folds through the real landed
//!    [`crate::data::aggr`] ops); the oracle itself is cross-checked
//!    against a second, semi-naive evaluation strategy on generated
//!    meet-recursive programs, which is also the standing regression for
//!    upstream's inverted `and`/`or` changed-flag (a lying flag stops
//!    delta propagation one hop short of the fixpoint).
//! 2. **Stratification safety** — programs with negation or aggregation
//!    through a recursive cycle are **rejected**, never mis-answered.
//!    Self-recursion through aggregation is legal only when every rule of
//!    the head aggregates with meet forms; normal aggregation over any
//!    dependency, a fixed rule in a cycle, and negation in a cycle all
//!    force refusal.
//!    *Enforcement:* the unstratifiable-program corpus in [`laws`] must be
//!    refused by the real compiler exactly as the reference checker refuses
//!    it.
//! 3. **Termination** — recursion over finite data reaches a fixpoint;
//!    no query runs forever.
//!    *Enforcement:* the reference evaluator's fixpoint bound plus
//!    generated-program differential tests.
//! 4. **Rule safety** — every head variable is bound by a positive body
//!    literal; negation applies only to fully bound literals.
//!    *Enforcement:* reference checker in [`laws`]; the real compiler must
//!    agree on the corpus.
//! 5. **Total input handling** — no query text and no stored data may panic
//!    the process; parse and evaluation errors are values.
//!    *Enforcement:* parser property tests and a fuzz target that land with
//!    the parser; the kernel's fallible-decode laws already cover stored
//!    bytes.
//! 6. **Concurrency liveness** — write queries retry typed conflicts to
//!    completion ([`crate::storage::retry`]); concurrent writers make
//!    progress without lost updates.
//!    *Enforcement:* multi-threaded contention tests over the retry helper.
//! 7. **Operator coherence** — an index search (HNSW, LSH, FTS) is a
//!    relation: it joins, filters, negates, and recurses like any other.
//!    *Enforcement:* query-level tests exercising each operator inside
//!    joins, negation, and recursion, landing with the operators.

// The naive reference oracle. Production host door:
// `runtime/verify.rs` calls `laws::naive_eval_at_budgeted`; differential
// tests and `runtime/db.rs` tests call `naive_eval`/`naive_eval_at`
// directly. Residual oracle surface not yet on those paths warns honestly
// (P112) — no module-level `allow(dead_code)`.
pub(crate) mod batch;
pub(crate) mod laws;

// Trial (issue #29): the SQLancer-class metamorphic logic-bug gauntlet —
// magic-sets NoREC-analog oracle, generated programs, swept demand
// adornment. Test-only; adds no lib code. Kept out of `trials.rs` (its own
// module, per the design ruling) and out of `laws.rs` (reuses it instead).
// `pub(crate)` (not private): story #80's `::verify` whole-corpus proof
// (`runtime/verify.rs`'s test module) reuses this module's
// `laws::Program` -> KyzoScript-text renderer and generator directly
// rather than re-deriving a second one — the same "reused, not re-derived"
// principle this module's own refusal-fence test states for itself.
#[cfg(test)]
pub(crate) mod gauntlet;

// Deterministic simulation testing up the query path: compiled programs run
// over the storage double under seeded fault/crash/contention plans. Test-only.
#[cfg(test)]
mod dst_query;

// Trials: the determinism campaign at scale and the provenance MVP, driving
// the evaluator's `pub(crate)` seams against the sealed oracle. Test-only.
#[cfg(test)]
mod trials;

// Time-travel trials (story #3, item C.10): the README's as-of claims proven
// through the full compile→RA→eval path against the naive as-of oracle.
// Test-only; adds no lib code.
#[cfg(test)]
mod time_travel_trials;

// Time-travel LANGUAGE-surface laws (story #4): the same as-of claim, proven
// through the actual public surface — `Db::run_script` parsing real
// KyzoScript `@` clauses — rather than hand-built magic-program ASTs.
// Test-only; adds no lib code.
#[cfg(test)]
mod time_travel_script_laws;

// Production host door: `runtime/db.rs::compile_and_eval` runs
// `stratified_magic_compile` and `bind_for_eval` on every query (P112).
pub(crate) mod compile;
// Production host door: `compile`'s plans and `eval`'s loop drive every
// `ra` operator (`runtime/db.rs::compile_and_eval`).
pub(crate) mod ra;

// Production host door: `runtime/db.rs::compile_and_eval` runs
// `stratified_evaluate` on every query. Provenance-graph plumbing with no
// production caller yet warns honestly until wired (P112).
pub(crate) mod eval;
pub(crate) mod graph;
// Story #61's production incremental-maintenance engine: an independently-
// written twin of `laws::incremental_eval`, proven equal to it by
// differential (this module's own test suite). Its lifecycle caller is
// `standing` below.
pub(crate) mod incremental;
// Story #61's standing-query lifecycle (registration, snapshot-consistent
// init, patch application, teardown) on `runtime::callback`'s existing
// per-relation commit-notification seam. `Db::register_standing` (defined
// here, re-exported at the crate root as `StandingQuery`) is the real,
// live production entry point.
pub(crate) mod standing;
// The provenance trials: semiring axiom, oracle-differential, certificate,
// and thread-determinism tests over the eval seams. Test-only, like `laws`.
#[cfg(test)]
mod provenance;
// Semiring provenance (boolean/tropical annotations + certificates).
// Production caller (`runtime/db.rs` provenance recording) lands later;
// unused lib surface warns honestly until wired (P112) — no module-level
// dead_code silence.
pub(crate) mod semiring;
// Production host door: every query runs `magic_sets_rewrite` via
// `compile::stratified_magic_compile` (P112).
pub(crate) mod magic;
// Production host door: every query is stratified before magic-sets via
// `runtime/db.rs::compile_and_eval` (P112).
pub(crate) mod stratify;
// Production host door: columnar execution currency (`batch_ops::Batch`)
// driven by every `ra` operator's `iter` (P112).
pub(crate) mod batch_ops;
pub(crate) mod levels;
// Production host door: `runtime/db.rs` and `runtime/verify.rs` call
// `SessionNormalizer::new` on every query path that normalizes (P112).
pub(crate) mod normalize;
// Production host door: `resolve_search` is called from normalize's body
// normalizer, itself production-called (P112).
pub(crate) mod search;
// Production host door: `runtime/db.rs::sort_and_collect` runs on every
// query result path (P112).
pub(crate) mod sort;
pub(crate) mod temp_store;
pub(crate) mod vm;
