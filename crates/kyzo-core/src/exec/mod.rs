//! Evaluation zone: plans, operators, fixpoint, provenance — never persistence.
//!
//! ## The engine laws, and where each is enforced
//!
//! 1. **Answer correctness** — optimized evaluation (semi-naive, magic-sets)
//!    produces exactly the naive fixpoint of the logic program, aggregation
//!    included: normal aggregations group and fold at the fixpoint of the
//!    strata beneath them, meet aggregations fold *inside* recursion, and
//!    fixed rules run once on stratum boundaries.
//!    *Enforcement:* differential tests against the naive reference
//!    evaluator in `kyzo_oracle::eval` (the oracle is deliberately
//!    unoptimized and obviously correct, and folds through the real landed
//!    [`crate::exec::fold::aggr`] ops); the oracle itself is cross-checked
//!    against a second, semi-naive evaluation strategy on generated
//!    meet-recursive programs.
//! 2. **Stratification safety** — programs with negation or aggregation
//!    through a recursive cycle are **rejected**, never mis-answered.
//!    *Enforcement:* the unstratifiable-program corpus in `kyzo_oracle::eval`
//!    must be refused by the real compiler (`exec::plan::stratify`) exactly
//!    as the reference checker refuses it.
//! 3. **Termination** — recursion over finite data reaches a fixpoint;
//!    no query runs forever.
//!    *Enforcement:* the reference evaluator's fixpoint bound plus
//!    generated-program differential tests (`kyzo_oracle::eval`).
//! 4. **Rule safety** — every head variable is bound by a positive body
//!    literal; negation applies only to fully bound literals.
//!    *Enforcement:* reference checker in `kyzo_oracle::eval`; the real
//!    compiler must agree on the corpus.
//! 5. **Total input handling** — no query text and no stored data may panic
//!    the process; parse and evaluation errors are values.
//!    *Enforcement:* parser property tests and a fuzz target that land with
//!    the parser; the kernel's fallible-decode laws already cover stored
//!    bytes.
//! 6. **Concurrency liveness** — write queries retry typed conflicts to
//!    completion ([`crate::store::retry`]); concurrent writers make
//!    progress without lost updates.
//!    *Enforcement:* multi-threaded contention tests over the retry helper.
//! 7. **Operator coherence** — an index search (HNSW, LSH, FTS) is a
//!    relation: it joins, filters, negates, and recurses like any other.
//!    *Enforcement:* query-level tests exercising each operator inside
//!    joins, negation, and recursion, landing with the operators
//!    (`exec::op::search`, `exec::plan::search`).
//!
//! Stratified fixpoint evaluation lives in [`fixpoint`]; relational
//! operators in [`op`]; planning/normalize/magic/stratify in [`plan`];
//! provenance in [`provenance`]; temporal oracle twins in
//! `kyzo_oracle::temporal`; incremental oracle twins in
//! `kyzo_oracle::incremental`.

pub(crate) mod expr;
pub(crate) mod fold {
    pub(crate) mod aggr;
    pub(crate) mod sketch;
}
pub(crate) mod fixpoint {
    pub(crate) mod delta_store;
    pub(crate) mod eval;
    pub(crate) mod parallel;
}
pub(crate) mod op;
pub(crate) mod plan {
    pub(crate) mod compile;
    pub(crate) mod graph;
    pub(crate) mod magic;
    pub(crate) mod normalize;
    pub(crate) mod program;
    pub(crate) mod search;
    pub(crate) mod stratify;
}
pub(crate) mod provenance {
    pub(crate) mod eval;
    pub(crate) mod semiring;
}
pub(crate) mod sort;
pub(crate) mod stdlib;
