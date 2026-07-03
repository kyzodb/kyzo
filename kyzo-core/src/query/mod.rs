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

#[cfg(test)]
pub(crate) mod laws;

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

// The plan compiler's production caller (runtime/db.rs::run_query) lands
// later; until then its lib code (and, transitively, `ra` below) is dead in
// lib builds, while the in-file tests (the real-storage compile-then-eval
// queries and the RA-vs-oracle differentials) keep it live in test builds.
#[allow(dead_code)]
pub(crate) mod compile;
// The relational-algebra operators: consumed by `compile`, which is dead in
// lib builds until db.rs lands — same pattern.
#[allow(dead_code)]
pub(crate) mod ra;

// The evaluator's production callers (query/compile.rs, runtime/db.rs) land
// later; until then its lib code is dead like `magic` below, while the
// in-file tests (oracle differentials, the determinism law, budget
// refusals) keep it live in test builds.
#[allow(dead_code)]
pub(crate) mod eval;
pub(crate) mod graph;
// The provenance trials: semiring axiom, oracle-differential, certificate,
// and thread-determinism tests over the eval seams. Test-only, like `laws`.
#[cfg(test)]
mod provenance;
// Semiring provenance (boolean/tropical annotations + certificates). Its
// production caller (runtime/db.rs) lands later; until then its lib code
// is dead, kept live in test builds by the `provenance` trials — the same
// pattern as `eval` above.
#[cfg_attr(not(test), expect(dead_code))]
#[cfg_attr(test, allow(dead_code))]
pub(crate) mod semiring;
// The magic tier's consumers (query/compile.rs, runtime/db.rs) land later;
// until then its lib code is dead, like `stratify` below.
#[allow(dead_code)]
pub(crate) mod magic;
// The stratifier's caller (runtime/db.rs::run_query) lands later. In the
// lib build the module is dead (expect); in test builds the in-file tests
// keep it live but not every item, so a plain `allow` covers the remainder
// — the same pattern as the `parse` module in lib.rs.
#[allow(dead_code)]
pub(crate) mod normalize;
#[allow(dead_code)]
pub(crate) mod search;
#[allow(dead_code)]
pub(crate) mod sort;
#[allow(dead_code)]
pub(crate) mod stored;
#[allow(dead_code)]
pub(crate) mod stratify;
