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
//!    produces exactly the naive fixpoint of the logic program.
//!    *Enforcement:* differential tests against the naive reference
//!    evaluator in [`laws`] (the oracle is deliberately unoptimized and
//!    obviously correct).
//! 2. **Stratification safety** — programs with negation or aggregation
//!    through a recursive cycle are **rejected**, never mis-answered.
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
