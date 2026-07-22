/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0). The transformations, each per the ratified designs (story #3):
 *
 * - **Budget is a required parameter.** The original's only resource
 *   controls were a `Poison` flag set by a sleeper thread per timeout and
 *   an unbounded `for epoch in 0u32..` fixpoint loop. Here
 *   [`stratified_evaluate`] takes a [`Budget`]: the deterministic
 *   dimensions (`derived_tuple_ceiling`, counted from the [`Admitted`]
 *   totals of the admission seam; `epoch_ceiling`, which bounds every
 *   fixpoint loop) are checked at epoch barriers ONLY, so a refusal is a
 *   pure function of program+facts+budget; the deadline is *read* at every
 *   check site, including inside rule iteration ([`InterruptTicker`]),
 *   which closes the original's unkillable-scan gap (poison was checked
 *   once per rule, after the whole scan) and retires the sleeper thread.
 *   Poison survives as the cancel lifecycle
 *   ([`crate::rules::contract::CancelFlag`] shared with fixed rules; request via
 *   consuming [`crate::rules::contract::CancelAuthority::cancel`]). Refusals are
 *   typed: [`LimitExceeded`] and [`crate::rules::contract::Cancelled`].
 * - **No unbounded fixpoint exists.** The epoch loop is
 *   `0..budget.epoch_ceiling`; exhausting it without a fixpoint is a
 *   deterministic [`LimitExceeded`] refusal.
 * - **Provenance hooks at the admission seam.** When a query opts in
 *   (passes a [`WitnessTable`]), each rule evaluation records the *first*
 *   derivation of each candidate tuple (rule index + premise rows), and a
 *   [`WitnessBinder`] — an [`AdmissionSink`] — binds those pending
 *   witnesses to admitted tuples at the merge barrier, in canonical order.
 *   Off is the `()` sink: zero-cost by monomorphization in the stores.
 *   Hooks only; proof-tree reconstruction is out of scope by design.
 * - **The evaluator consumes a seam, not relational algebra.** The
 *   original evaluated `CompiledProgram`/`RelAlgebra` (compile.rs/ra.rs,
 *   not yet ported). Eval here is generic over [`RuleBody`] (iterate one
 *   rule's satisfying head tuples given stores + delta context) and
 *   [`FixedRuleEval`]; the compile tier will implement both over its RA
 *   plans (see the SEAM notes below), and the differential tests implement
 *   them over the oracle's rule model.
 *
 * Upstream abort-site audit (Law 5), all 13 sites in the original file —
 * each former force-unwrap / abort is a typed refuse or is structurally gone:
 *   1. eval.rs:109  `stores.remove(entry).ok_or(NoEntryError)` — already an
 *      error; here [`EvalProgram::from_execution_order`] proves the entry
 *      exists in the final stratum, and the residual lookup is a typed
 *      [`EvalInvariantError`].
 *   2. eval.rs:91   abort on a fixed rule set reaching the meet path —
 *      structurally removed: [`EvalRuleSet`] carries its
 *      [`HeadAggrKind`] and its store is minted from the same value.
 *   3. eval.rs:293  forced `stores.get_mut(k)` at the merge barrier —
 *      typed [`EvalInvariantError`] via [`store_of_mut`].
 *   4. eval.rs:516  forced `stores.get(rule_symb)` (previous-total lookup)
 *      — typed via [`store_of`].
 *   5. eval.rs:524  forced `stores.get(symb)` (delta discipline, plain)
 *      — typed via [`store_of`].
 *   6. eval.rs:628  the same lookup in the meet path — typed via
 *      [`store_of`].
 *   7. eval.rs:372  forced `a.as_ref()` building the meet identity row —
 *      the checked all-aggregated condition drives a `flatten()`.
 *   8. eval.rs:373  forced `aggr.meet_op.as_ref()` — the landed
 *      `Aggregation::meet_op` returns `Option`; a `None` under the Meet
 *      classification is a typed [`EvalInvariantError`].
 *   9. eval.rs:430  forced `normal_op.as_mut().set(..)` — gone: the
 *      landed aggregation API mints live ops per group
 *      (`Aggregation::normal_op(args) -> Result<NormalAggr>`),
 *      there is no `Option` field to force.
 *  10. eval.rs:439  the vacant-entry twin of 9 — gone the same way.
 *  11. eval.rs:467-470  forced `a.as_ref()` + `normal_op` for
 *      the empty fold — gone the same way.
 *  12. eval.rs:482  forced `aggrs[idx].normal_op.as_ref().get()` — gone
 *      the same way.
 *  13. `ruleset[0]` indexing throughout — [`EvalRuleSet::new`] refuses
 *      empty rule sets, so the signature accessor is structurally total.
 *
 * Documented deviations from the original, beyond the ratified ones:
 *   D1. Limiter skip dispositions (`LimiterSkip`) are recorded only for
 *       the entry rule (`should_check_limit`); the original's incremental
 *       path called `should_skip_next` for every rule, flagging tuples in
 *       non-entry stores. Those flags were dead (only the entry store's
 *       flags are read) but misleading; the initial path already gated them.
 *       The pre-runtime `NoStoredInputs` placeholder is gone (P090) — fixed
 *       rules read stored inputs through `SessionView`, never that seam.
 *   D2. The incremental limiter path deduplicates against the epoch's own
 *       out-store (`!out.exists(&item)`) exactly as the original's initial
 *       path did. The original's incremental path did not, so a tuple
 *       re-derived twice within one epoch double-counted toward `:limit`
 *       and could early-stop the entry rule short of the requested rows.
 *   D3. RETIRED. An all-meet head whose aggregated positions are not a
 *       suffix used to be refused at [`EvalRuleSet::new`]. The landed
 *       [`HeadAggrKind::Meet`] carries grouping [`HeadPos`]s wherever they
 *       sit, and [`MeetAggrStore`]/[`MeetLayout`] group positionally —
 *       the shape the original silently demoted (wrong answers) now
 *       constructs and evaluates. Comments are not a second law (P096).
 *   D4. Delta iteration walks the rule's own `contained_rules()` keys
 *       instead of the whole store map filtered by `contained_rules` — the
 *       same set in the same canonical (BTreeMap) order, with a typed error
 *       for a missing store instead of skipping it silently.
 *   D5. One `execution` closure serves all epochs (the original duplicated
 *       it for epoch 0 and epoch > 0); the epoch-0/incremental dispatch is
 *       inside. Semantics are identical, including that normal-aggregation
 *       and fixed rule sets contribute *empty* stores in epochs > 0 so
 *       their deltas clear.
 *
 * Notes (preserved behavior, not deviations):
 *   N1. The incremental path's `prev_store.exists` filter is an
 *       optimization, not the enforcement: the load-bearing re-derivation
 *       dedup is `merge_in`'s (a re-derived tuple is not admitted and not
 *       in the delta — see `runtime/temp_store.rs`). Do NOT strip
 *       merge_in's dedup on the strength of eval's filter; removing the
 *       filter is survivable, removing merge_in's dedup is not.
 *   N2. The limiter's cross-epoch overshoot is preserved from the
 *       original: a row is put before the counter is checked, so once the
 *       take-count is reached, every later epoch in which the entry rule
 *       still fires admits one more row before stopping. The entry store
 *       may therefore hold more than `num_to_take` rows; callers needing
 *       an exact count re-truncate downstream (as the original's db tier
 *       did). Pinned by
 *       `limiter_incremental_entry_recursion_dedups_and_overshoots`.
 */

//! Semi-naive stratified evaluation: the fixpoint engine.
//!
//! [`stratified_evaluate`] runs an execution-ordered stratified program
//! (the shape minted by the magic tier; see
//! [`crate::exec::plan::program::StratifiedMagicProgram`]) over the delta stores
//! of [`crate::exec::fixpoint::delta_store`]. Per stratum, rules are evaluated
//! epoch by epoch: epoch 0 computes every rule from the current totals;
//! later epochs re-derive only rules whose dependencies changed, joining
//! against the *delta* of one changed store at a time (semi-naive). Each
//! epoch ends at the **merge barrier**: every rule's freshly derived
//! tuples are merged into its [`EpochStore`] sequentially, in canonical
//! `MagicSymbol` order — that single-threaded merge is what makes the
//! computation deterministic under any parallel schedule. The fixpoint is
//! reached when no store has a delta.
//!
//! ## The determinism law
//!
//! Same program + facts + budget ⇒ identical result sets *and identical
//! refusals*, at any thread count. It holds because:
//! - rule evaluations within an epoch only read the stores (immutably) and
//!   write their own out-store, so their results are schedule-independent;
//! - admissions happen only at the sequential merge barrier, in canonical
//!   order, so witness recording and the [`Admitted`] counts are
//!   schedule-independent;
//! - the deterministic budget dimensions (derived tuples, epochs) are
//!   checked at the barrier ONLY — a mid-epoch check would observe a
//!   schedule-dependent partial count and is therefore a determinism bug;
//! - entry rules run *sequentially* when a `:limit` is in force (the
//!   original's carve-out, preserved): the limiter counter is shared
//!   mutable state, and ordering entry evaluation makes the early-stop
//!   point a function of the data alone. Non-entry rules never advance the
//!   counter, so the parallel batch reads it stably.
//!
//! The deadline and the cancel poll are *interrupts*, not deterministic
//! dimensions: they are checked at every barrier and (unlike the original)
//! inside rule iteration, so no scan is unkillable, but *when* they fire
//! depends on the wall clock and the user.
//!
//! ## Seams
//!
//! - [`RuleBody`] + [`FixedRuleEval`]: the compile tier's entry point.
//!   When compile.rs/ra.rs land, a compiled rule (bindings + RA plan +
//!   `SessionTx`) implements `RuleBody` — `for_each_derivation` walks the
//!   plan's `TupleIter` with `delta_from` threaded to the stored-rule
//!   scans, and `contained_rules` is the compiled dependency map. Nothing
//!   in this module changes.
//! - [`AdmissionSink`]: the landed seam in `fixpoint/delta_store.rs`; the
//!   provenance [`WitnessBinder`] and the `()` off-state both flow through
//!   it.
//! - WASM: on `wasm32` the per-epoch batch runs sequentially (no rayon),
//!   as in the original. A [`Budget`] without a timeout never touches the
//!   clock, so timeout-less budgets are wasm-safe; the wasm binding must
//!   not set `with_timeout` until a clock shim lands there.
//!
//! ## Ownership map
//!
//! | Section | Owns |
//! | --- | --- |
//! | Refusals and invariants | [`BudgetDimension`], [`LimitExceeded`], [`EvalInvariantError`] |
//! | Budget | [`Budget`] — epoch / derived-tuple ceilings, deadline, cancel poll |
//! | InterruptTicker | [`INTERRUPT_STRIDE`], mid-epoch interrupt + in-flight spend guard |
//! | SEAM: RuleBody / FixedRuleEval | [`RuleBody`], [`FixedRuleEval`], premises / occurrence keys |
//! | Evaluable program tier | [`EvalRuleSet`], [`HeadAggrKind`], [`EvalProgram`], [`RuleSetShapeError`] |
//! | Query limiter | [`RowLimit`], early-return counter for `:limit` / `:offset` |
//! | Evaluator | [`stratified_evaluate`], merge barrier, [`store_of`] / [`store_of_mut`] |
//! | Per-kind epoch bodies | plain / meet / normal initial + incremental rule evaluation |
//! | Tests (`cfg(test)`) | oracle differentials, determinism law, budget refusal identity |

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use miette::{Diagnostic, Result};
use thiserror::Error;

use crate::exec::fixpoint::delta_store::{
    EpochStore, HeadPos, MeetAggrStore, RegularTempStore, TempStore,
};
use crate::exec::fold::aggr::NormalAggr;
use crate::exec::plan::program::MagicSymbol;
use crate::exec::provenance::eval::{
    PendingWitnesses, WitnessBinder, WitnessKeyMode, WitnessTable,
};
use kyzo_model::SourceSpan;
use kyzo_model::program::aggregate::Aggregation;
use kyzo_model::program::rule::HeadAggrSlot;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::DataValue;
use kyzo_model::value::Tuple;

/// One head position's aggregation slot — the shape carried through
/// every program tier (see `MagicInlineRule::aggr` in `data/program.rs`).
type HeadAggr = HeadAggrSlot;

// ─────────────────────────────────────────────────────────────────────────
// Refusals and invariants
// ─────────────────────────────────────────────────────────────────────────

/// A budget dimension. The first three are **deterministic**: their spend
/// is a function of program+facts+budget alone, so their refusals are
/// byte-identical on every run at any thread count. `Deadline` is an
/// interrupt: its spend is wall-clock elapsed milliseconds.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum BudgetDimension {
    /// Derivations admitted to any store's total, summed over the whole
    /// query — the [`Admitted`](crate::exec::fixpoint::delta_store::Admitted)
    /// counts of the admission seam, checked at the epoch **barrier**.
    DerivedTuples,
    /// A single rule's in-flight materialized derivations *within* one
    /// epoch, plus the globally admitted total as of that epoch's barrier,
    /// checked at [`INTERRUPT_STRIDE`] mid-iteration. This is the guard that
    /// keeps a single hostile-but-legal epoch — a near-cross-product join —
    /// from materializing an unbounded intermediate before the barrier's
    /// [`DerivedTuples`](BudgetDimension::DerivedTuples) check ever fires.
    /// See [`InterruptTicker`] for the determinism and boundedness laws.
    InFlightDerivations,
    /// Fixpoint epochs, per stratum.
    Epochs,
    /// Wall-clock milliseconds since the budget's timeout was armed.
    Deadline,
}

impl std::fmt::Display for BudgetDimension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            BudgetDimension::DerivedTuples => "derived tuples",
            BudgetDimension::InFlightDerivations => "in-flight derivations",
            BudgetDimension::Epochs => "epochs",
            BudgetDimension::Deadline => "deadline (ms)",
        })
    }
}

/// The query exceeded a budget ceiling. For the deterministic dimensions
/// this refusal is itself deterministic — same fields, same rendering, on
/// every run (the determinism law covers refusals, and the test suite
/// asserts byte-identity across thread counts).
///
/// A mid-epoch [`InFlightDerivations`](BudgetDimension::InFlightDerivations)
/// refusal additionally names the offending rule and labels its source
/// span; the whole-query barrier dimensions (`DerivedTuples`, `Epochs`,
/// `Deadline`) leave both `None`, since no single rule owns the spend.
#[derive(Debug, Error, Diagnostic, PartialEq, Eq)]
#[error("query budget exceeded: {dimension} spent {spent} of ceiling {ceiling}")]
#[diagnostic(
    code(eval::limit_exceeded),
    help("raise the corresponding budget ceiling, or narrow the query")
)]
pub struct LimitExceeded {
    pub dimension: BudgetDimension,
    pub spent: u64,
    pub ceiling: u64,
    /// The rule whose in-flight materialization crossed the ceiling
    /// (mid-epoch dimension only); `None` for the barrier dimensions.
    pub rule: Option<MagicSymbol>,
    /// The offending rule's source span, so the diagnostic points back at
    /// the query text. `None` for the barrier dimensions.
    #[label("this rule's in-flight derivations crossed the budget ceiling")]
    pub span: Option<SourceSpan>,
}

/// A cross-stage invariant that construction should have made impossible
/// (e.g. "every referenced rule has a store"). Surfaced as an error, never
/// an abort, so corruption of the proof is loud but recoverable.
#[derive(Debug, Error, Diagnostic)]
#[error("evaluation invariant violated: {0}")]
#[diagnostic(code(eval::invariant), help("This is a bug. Please report it."))]
pub(crate) struct EvalInvariantError(pub(crate) &'static str);

// ─────────────────────────────────────────────────────────────────────────
// Budget
// ─────────────────────────────────────────────────────────────────────────

/// An armed deadline: a start instant and the allotted duration. Kept
/// together so the refusal can report both spend and ceiling.
#[derive(Debug, Copy, Clone)]
struct Deadline {
    started: Instant,
    allotted: Duration,
}

/// What one query evaluation is allowed to spend. Required by parameter:
/// there is no way to call [`stratified_evaluate`] without one, and the
/// epoch ceiling is not optional — no unbounded fixpoint exists in KyzoDB.
///
/// Deterministic dimensions (`epoch_ceiling`, `derived_tuple_ceiling`) are
/// checked at epoch barriers only. The deadline and the cancel poll are
/// checked at every barrier *and* inside rule iteration.
#[derive(Debug, Clone)]
pub struct Budget {
    epoch_ceiling: NonZeroU32,
    derived_tuple_ceiling: Option<u64>,
    deadline: Option<Deadline>,
    cancel: Option<crate::rules::contract::CancelFlag>,
}

impl Budget {
    /// A budget with the one mandatory dimension. Note that any stratum
    /// deriving anything needs at least two epochs (one to derive, one to
    /// observe the empty delta), so a ceiling of 1 refuses every non-empty
    /// program — deterministically.
    pub fn new(epoch_ceiling: NonZeroU32) -> Self {
        Self {
            epoch_ceiling,
            derived_tuple_ceiling: None,
            deadline: None,
            cancel: None,
        }
    }

    /// Cap the total derivations admitted across the whole query.
    /// The deterministic derived-tuple ceiling, if armed — read by the
    /// session fixed-rule wrapper to arm its output writer.
    pub(crate) fn derived_tuple_ceiling(&self) -> Option<u64> {
        self.derived_tuple_ceiling
    }

    /// The fixpoint-epoch ceiling.
    ///
    /// Sanctioned oracle seam until `OracleBudget` (oracle-seam-shrink):
    /// `kyzo_oracle`'s naive evaluator threads the same `Budget` through its
    /// own, simpler stratum/round loop (barrier-only, no per-rule in-flight
    /// ticker — that granularity is a production-only concern for bounding
    /// rayon's mid-epoch parallel materialization) and needs its ceiling to
    /// check against directly.
    pub(crate) fn epoch_ceiling(&self) -> NonZeroU32 {
        self.epoch_ceiling
    }

    pub fn with_derived_tuple_ceiling(mut self, ceiling: u64) -> Self {
        self.derived_tuple_ceiling = Some(ceiling);
        self
    }

    /// Arm a wall-clock timeout, starting now. (This is the only budget
    /// dimension that touches the clock; see the module doc's WASM note.)
    pub fn with_timeout(mut self, allotted: Duration) -> Self {
        self.deadline = Some(Deadline {
            started: Instant::now(),
            allotted,
        });
        self
    }

    /// Attach the cancel poll (session arms
    /// [`crate::rules::contract::CancelAuthority`]; eval only reads via
    /// [`crate::rules::contract::CancelFlag::check`]).
    pub fn with_cancel(mut self, cancel: crate::rules::contract::CancelFlag) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// The interrupt check: user cancel, then deadline. Called at every
    /// epoch barrier and, via [`InterruptTicker`], inside rule iteration.
    ///
    /// Sanctioned oracle seam until `OracleBudget` (oracle-seam-shrink):
    /// `kyzo_oracle`'s naive evaluator calls this directly at its own
    /// barrier points (visibility only; the check itself is unchanged).
    pub(crate) fn check_interrupt(&self) -> Result<()> {
        if let Some(cancel) = &self.cancel {
            cancel.check()?;
        }
        if let Some(deadline) = &self.deadline {
            let elapsed = deadline.started.elapsed();
            if elapsed > deadline.allotted {
                let millis_u64 = |ms: u128| match u64::try_from(ms) {
                    Ok(v) => v,
                    Err(_gt_u64) => u64::MAX,
                };
                return Err(LimitExceeded {
                    dimension: BudgetDimension::Deadline,
                    spent: millis_u64(elapsed.as_millis()),
                    ceiling: millis_u64(deadline.allotted.as_millis()),
                    rule: None,
                    span: None,
                }
                .into());
            }
        }
        Ok(())
    }

    /// Arm a per-rule mid-epoch meter for one eval-function invocation.
    /// `baseline` is the globally admitted total as of this epoch's barrier
    /// (a snapshot of `spent_derived`, deterministic and fixed for the whole
    /// epoch); `rule` names the rule for a refusal's attribution.
    pub fn ticker<'a>(&'a self, baseline: u64, rule: &'a MagicSymbol) -> InterruptTicker<'a> {
        InterruptTicker {
            budget: self,
            countdown: InterruptCountdown::fresh(),
            baseline,
            ceiling: self.derived_tuple_ceiling,
            rule,
        }
    }
}

/// How many derivations may pass between interrupt checks inside a rule's
/// iteration. Small enough that no scan is unkillable for long; large
/// enough that the check does not dominate the loop. NonZero by construction
/// — wrap-through-zero is unrepresentable.
pub const INTERRUPT_STRIDE: std::num::NonZeroU32 = match std::num::NonZeroU32::new(64) {
    Some(n) => n,
    // 64 is nonzero; this arm is compile-time dead. [`NonZeroU32::MIN`]
    // keeps the match total without a panic-shape (Law 5).
    None => std::num::NonZeroU32::MIN,
};

/// Proven mid-epoch interrupt stride counter: starts at [`INTERRUPT_STRIDE`]
/// and resets there after each poll. [`NonZeroU32`] makes wrap-through-zero
/// unrepresentable — never an unnamed wrapping `u32` countdown (P097).
#[derive(Debug, Clone, Copy)]
struct InterruptCountdown(std::num::NonZeroU32);

impl InterruptCountdown {
    fn fresh() -> Self {
        Self(INTERRUPT_STRIDE)
    }

    /// Count one derivation. Returns `true` when the stride elapsed and the
    /// interrupt/spend guard must run.
    pub fn tick(&mut self) -> bool {
        match std::num::NonZeroU32::new(self.0.get() - 1) {
            None => {
                // Was 1: stride elapsed.
                *self = Self::fresh();
                true
            }
            Some(next) => {
                self.0 = next;
                false
            }
        }
    }
}

/// The in-iteration interrupt-and-spend site: `tick` once per derivation;
/// every [`INTERRUPT_STRIDE`]th tick reads the cancel poll and the deadline
/// (closing the original's unkillable-scan gap — it checked poison once per
/// rule, *after* the scan finished) **and** enforces the mid-epoch
/// derived-tuple ceiling.
///
/// ## The mid-epoch spend guard
///
/// The barrier's [`DerivedTuples`](BudgetDimension::DerivedTuples) ceiling
/// is checked only *between* epochs, so a single epoch — a legitimate
/// near-cross-product join over two large relations — can materialize an
/// unbounded intermediate before any barrier fires. This guard closes that
/// hole. Each eval-function invocation evaluates one rule's output stream
/// sequentially, so `in_flight` (the count of distinct tuples this rule has
/// materialized this epoch, passed by the caller as its accumulator's
/// `len`) is deterministic at every point. At each stride we refuse when
///
/// ```text
///     baseline + in_flight  >  derived_tuple_ceiling
/// ```
///
/// where `baseline` is the globally admitted total as of this epoch's
/// barrier (a fixed snapshot, deterministic).
///
/// ### Determinism law
///
/// Both terms are deterministic. `baseline` is a barrier value. `in_flight`
/// is a function of this rule's stores and body alone: rules in a stratum
/// run in parallel, but each rule's *own* output stream is strictly
/// sequential and we never read another in-flight rule's count. So the same
/// program+facts+budget yields a byte-identical refusal — same rule, span,
/// spend — at any thread count. Because we cannot see sibling rules' live
/// counts, `baseline + in_flight` is a deterministic *under-approximation*
/// of true global in-flight spend; it is guaranteed to trip within bounded
/// slack of the ceiling (below), never past it non-deterministically.
///
/// ### Non-perturbation (the theorem)
///
/// `in_flight` is the count of derivations this rule has made that the
/// epoch barrier **would admit** — never the raw materialized volume. The
/// two eval paths establish this differently, and the distinction is the
/// whole repair:
///
/// - **Plain / normal-aggregation rules:** the out-store holds only
///   genuinely-new tuples (`incremental_plain_eval` filters each derivation
///   against `prev_store.exists` and dedups within the epoch), so its `len`
///   *is* the admission count: `in_flight = out.len() ≤ admitted_r`.
/// - **Meet rules:** a meet epoch folds every re-derived group into a FRESH
///   out-store, so `out.len()` counts unchanged re-derivations the barrier
///   drops — an epoch re-deriving `N` unchanged groups has `out.len() == N`
///   but `admitted_r == 0`. Counting `out.len()` there refused programs the
///   barrier completes (the refuted theorem). Instead
///   `incremental_meet_eval` counts with
///   [`MeetAggrStore::meet_put_admission_faithful`], which increments only
///   on the single monotone `false → true` admissibility transition of a
///   group against the running total — the meet twin of the plain filter.
///   So again `in_flight ≤ admitted_r`, by construction.
///
/// Hence on every path, if the barrier never trips
/// (`baseline + Σ_r admitted_r ≤ ceiling` at every epoch), then
/// `baseline + in_flight ≤ ceiling` at every point and this guard never
/// fires. It refuses *only* queries the barrier would also refuse — earlier,
/// before the OOM — and never changes an answer or an admission order. A
/// mid-epoch refusal's `spent` (`baseline + in_flight`) is therefore true
/// admitted spend, not raw materialized volume.
///
/// ### Boundedness law
///
/// A rule halts once `baseline + in_flight > ceiling`, checked every
/// `INTERRUPT_STRIDE` derivations. Because `in_flight` counts admissions,
/// the *admitted* rows this epoch never exceed
/// `(ceiling − baseline) + INTERRUPT_STRIDE`. Resident materialization is
/// this plus what a re-derivation may hold without admitting:
///
/// - Plain/normal rules materialize only admitted rows, so resident ≤
///   `(ceiling − baseline) + INTERRUPT_STRIDE`.
/// - A meet out-store additionally holds re-derived-but-unchanged groups;
///   every such group already sits in the running total, whose size the
///   barrier holds `≤ ceiling`. So a meet out-store is resident-bounded by
///   `ceiling + (ceiling − baseline) + INTERRUPT_STRIDE ≤ 2·ceiling + STRIDE`.
///
/// With at most `P` rules materializing concurrently in one stratum, peak
/// resident tuples are `O(P · (ceiling + STRIDE))` — linear in the ceiling
/// and the stride, **independent of the input relations' product size**.
/// That is the guarantee the incident violated: no single epoch can OOM the
/// host.
pub struct InterruptTicker<'a> {
    budget: &'a Budget,
    countdown: InterruptCountdown,
    /// Globally admitted total as of this epoch's barrier (fixed snapshot).
    baseline: u64,
    /// The derived-tuple ceiling, mirrored from the budget; `None` disables
    /// the spend guard (the interrupt poll still runs).
    ceiling: Option<u64>,
    /// The rule owning this stream, for a refusal's name and span.
    rule: &'a MagicSymbol,
}

impl InterruptTicker<'_> {
    /// One derivation. `in_flight` is this rule's count of derivations the
    /// epoch barrier would admit so far — `out.len()` on the plain/normal
    /// paths (whose out-store holds only admissions) and the
    /// admission-faithful transition count on the meet path (see the
    /// Non-perturbation section). Every [`INTERRUPT_STRIDE`]th call polls the
    /// interrupt and enforces the mid-epoch ceiling.
    pub fn tick(&mut self, in_flight: usize) -> Result<()> {
        if self.countdown.tick() {
            self.budget.check_interrupt()?;
            if let Some(ceiling) = self.ceiling {
                let in_flight_u64 = match u64::try_from(in_flight) {
                    Ok(v) => v,
                    Err(_gt_u64) => u64::MAX,
                };
                let spent = self.baseline.saturating_add(in_flight_u64);
                if spent > ceiling {
                    let symb = self.rule.as_plain_symbol();
                    return Err(LimitExceeded {
                        dimension: BudgetDimension::InFlightDerivations,
                        spent,
                        ceiling,
                        rule: Some(self.rule.clone()),
                        span: Some(symb.span),
                    }
                    .into());
                }
            }
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// SEAM: the compile tier's rule surface
// ─────────────────────────────────────────────────────────────────────────

/// The stable, positional key selecting one body-atom occurrence for
/// semi-naive delta substitution — assigned by walking a rule body
/// left-to-right, one id per `Rule`/`NegatedRule` atom in body order
/// (`compile.rs`'s `MagicInlineRule::contained_rules` and the matching
/// `TempStoreRA` construction sites in `query/ra/mod.rs` number in
/// lockstep, both walking the same `body: &[MagicAtom]`).
///
/// Positional, not name-keyed: a store mentioned twice in one body — the
/// self-join shape (`tc(x,z), tc(z,y)`; Andersen's `load`/`store` rules,
/// each mentioning `pt` twice) — gets two distinct occurrences, and each
/// can be delta-selected independently. That is the standard semi-naive
/// self-join rewrite, `Δ(P⋈P) = (ΔP⋈P) ∪ (P⋈ΔP)` — one derivation pass per
/// OCCURRENCE. The predecessor name-keyed scheme could only ask "does the
/// body mention store `k`", collapsing both occurrences together; it could
/// not select one occurrence's delta while the other still reads the
/// total, so it fell back to a complete naive re-join of the WHOLE
/// accumulated relation every epoch — no delta narrowing at all for
/// exactly this rule shape (issue #68's dominant memory-blowup driver).
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AtomOccurrence(pub usize);

/// The premise rows of one derivation, for provenance. `NotRequested` when
/// eval did not ask (`want_premises` false) — the body implementation must
/// not pay to collect them in that case.
pub enum Premises<'a> {
    NotRequested,
    Rows(&'a [Tuple]),
}

impl Premises<'_> {
    pub(crate) fn to_rows(&self) -> Vec<Tuple> {
        match self {
            Premises::NotRequested => Vec::new(),
            Premises::Rows(rows) => rows.to_vec(),
        }
    }
}

/// Where one positive body literal's rows come from, for provenance
/// attribution. The premise rows [`Premises::Rows`] passes are bare
/// tuples; without the body naming each one's source, two relations
/// holding the same tuple would be indistinguishable and a derivation
/// graph over them ill-defined. A body that can attribute returns one
/// entry per positive literal, in body order (the order of the premise
/// rows); a body that cannot returns `None` and provenance refuses,
/// typed — it never guesses.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum PremiseSource {
    /// The literal reads an in-memory rule store.
    Rule(MagicSymbol),
    /// The literal reads a base (ground-fact) relation, by name. The rows
    /// are attested by the body that read them; the independent
    /// certificate checker re-verifies membership from the model.
    Fact(Symbol),
}

/// Private supertrait seal for [`RuleBody`]: only types this crate admits
/// (via an explicit `impl seal::Sealed`) can implement the eval seam.
/// Crate visibility alone is not the seal — when the crate splits, an open
/// `pub(crate)` trait would reopen.
pub mod seal {
    pub trait Sealed {}
}

/// One rule body, as eval consumes it: a generator of satisfying head
/// tuples against the current stores. This is the seam where the compile
/// tier's relational-algebra plans plug in (bindings in, tuples out,
/// `delta_from` threaded to the stored-rule scans); the differential tests
/// implement it over the oracle's rule model.
///
/// Sealed: [`seal::Sealed`] is a private supertrait — no type outside the
/// admitted set can implement this seam.
///
/// Contract:
/// - `delta_from: Some(k)` means every occurrence of store `k` in the body
///   reads that store's *delta* instead of its total (matching the
///   original's `delta_rule` threading). `None` reads totals only.
///   Negated occurrences always read totals.
/// - The callback **consumes a slice**: each derived row crosses the seam
///   as `Cow<[DataValue]>`, so a producer whose rows live in a batch
///   buffer passes `Cow::Borrowed` and mints nothing — the consumer
///   dedups/filters on the slice and materializes ownership
///   (`into_owned`) only for rows it actually admits. A producer that
///   already owns the row passes `Cow::Owned`, and admission moves it
///   without a copy. Re-derived and rejected rows — the bulk of every
///   recursive fixpoint — therefore allocate nothing on either path.
/// - The callback returns `ControlFlow::Break` to stop iteration early
///   (limiter early-return) — the implementation must stop and return
///   `Ok(())`. Errors from the callback (budget interrupts) propagate.
/// - When `want_premises` is true the implementation should pass the
///   positive body rows grounding each derivation via [`Premises::Rows`];
///   `NotRequested` is always legal (witnesses then record no premises).
/// - **Determinism**: iteration order must be a function of the stores and
///   the body alone (the landed stores iterate in canonical order; stored
///   relations scan in key order). The limiter's early-stop point depends
///   on it.
pub trait RuleBody: Send + Sync + seal::Sealed {
    /// `delta_from: Some(occ)` means the ONE body-atom occurrence `occ`
    /// reads that store's delta instead of its total; every other
    /// occurrence — including another occurrence of the SAME store name —
    /// reads its total. `None` reads totals only. Negated occurrences
    /// always read totals, whatever `delta_from` names.
    fn for_each_derivation(
        &self,
        stores: &BTreeMap<MagicSymbol, EpochStore>,
        delta_from: Option<AtomOccurrence>,
        want_premises: bool,
        f: &mut dyn FnMut(Cow<'_, [DataValue]>, Premises<'_>) -> Result<ControlFlow<()>>,
    ) -> Result<()>;

    /// Every in-memory rule store this body's POSITIVE atoms read, keyed by
    /// occurrence (body order) — not collapsed by name, so a store
    /// mentioned twice gets two independent entries. The delta discipline
    /// runs one derivation pass per occurrence whose named store has a
    /// delta this epoch; canonical (BTreeMap, i.e. occurrence-index) order
    /// is the per-rule delta iteration order.
    fn contained_rules(&self) -> &BTreeMap<AtomOccurrence, MagicSymbol>;

    /// Provenance attribution: the source of each **positive** body
    /// literal, in body order — matching the premise rows this body
    /// passes when `want_premises` is true. The default `None` states
    /// the body cannot attribute; [`crate::exec::provenance::eval::provenance_graph`]
    /// then refuses with
    /// the typed [`crate::exec::provenance::eval::ProvenanceUnsupported`]
    /// instead of building a graph with unattributed nodes.
    fn premise_sources(&self) -> Option<Vec<PremiseSource>> {
        None
    }
}

/// A fixed rule (graph algorithm / utility), as eval runs it: once, at
/// epoch 0 of its stratum — stratification proves its inputs are complete
/// strictly below and its readers sit strictly above. The fixed-rule tier
/// (`query::normalize::SessionFixedRule`, over `fixed_rule::FixedRule`)
/// implements this over `MagicFixedRuleApply`; the `budget` is passed so
/// long-running algorithms can check interrupts cooperatively (the original
/// passed `Poison` for the same purpose).
///
/// `baseline` is the globally admitted total as of this stratum's epoch-0
/// barrier — the same quantity [`Budget::ticker`]'s `baseline` is for
/// inline rules, so a fixed rule's own mid-run spend guard (e.g.
/// [`crate::rules::contract::FixedRuleOutput::new_budgeted`]) can count prior
/// admissions instead of starting from zero.
pub trait FixedRuleEval: Send + Sync {
    fn run(
        &self,
        stores: &BTreeMap<MagicSymbol, EpochStore>,
        out: &mut RegularTempStore,
        budget: &Budget,
        baseline: u64,
    ) -> Result<()>;
}

// ─────────────────────────────────────────────────────────────────────────
// The evaluable program tier
// ─────────────────────────────────────────────────────────────────────────

/// How a rule set's head aggregates — the classification that picks its
/// store and its evaluation schedule. (Distinct from
/// `data::aggr::AggrKind`, which classifies one *aggregation*; this
/// classifies a whole rule set.) Meet owns its grouping-key positions; a
/// freestanding `kind` beside an empty-for-other-modes `meet_key_positions`
/// field is unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadAggrKind {
    /// No aggregation: a plain rule set, re-derived every epoch it has
    /// changed dependencies.
    None,
    /// At least one non-meet aggregation: grouped and folded exactly once,
    /// at epoch 0 — stratification proves every dependency is complete
    /// strictly below, so epoch 0 already sees the fixpoint beneath.
    Normal,
    /// All aggregated positions are meet (semilattice) forms: folded into
    /// a [`MeetAggrStore`] *inside* recursion, epoch by epoch. The head
    /// positions that are grouping keys (the non-aggregated positions, in
    /// head order) travel with the variant — eval's copy of the store's
    /// [`MeetLayout`] key positions, used to key per-group provenance
    /// witnesses.
    Meet { key_positions: Vec<HeadPos> },
}

/// The rules of one head, ready to evaluate. Construction proves what the
/// original re-derived (or unwrapped) downstream: the rule set is
/// non-empty and every rule shares one aggregation signature. A meet head's
/// grouping positions may sit anywhere in the head — [`MeetAggrStore`]
/// groups positionally via [`MeetLayout`].
#[derive(Debug)]
pub struct EvalRuleSet<R> {
    pub(crate) aggr: Vec<HeadAggr>,
    pub kind: HeadAggrKind,
    pub(crate) bodies: Vec<R>,
}

/// A rule-set shape the evaluator refuses at construction.
#[derive(Debug, Error, Diagnostic)]
pub enum RuleSetShapeError {
    #[error("a rule set must contain at least one rule")]
    #[diagnostic(code(eval::empty_rule_set))]
    Empty,
}

impl<R> EvalRuleSet<R> {
    /// Classify and validate one head's rules. `aggr` is the head's
    /// per-position aggregation signature (uniform across the head's rules
    /// — the parser refuses disagreement as `parser::head_aggr_mismatch`,
    /// so the signature travels once, on the set).
    pub fn new(aggr: Vec<HeadAggr>, bodies: Vec<R>) -> Result<Self, RuleSetShapeError> {
        if bodies.is_empty() {
            return Err(RuleSetShapeError::Empty);
        }
        let has_aggr = aggr.iter().any(|a| a.is_aggregated());
        let all_meet = aggr
            .iter()
            .filter_map(|a| a.as_aggregated())
            .all(|(aggregation, _)| aggregation.is_meet());
        // MeetAggrStore groups positionally: key positions are every
        // non-aggregated head slot, wherever they sit.
        let kind = match (has_aggr, all_meet) {
            (false, _) => HeadAggrKind::None,
            (true, false) => HeadAggrKind::Normal,
            (true, true) => HeadAggrKind::Meet {
                key_positions: aggr
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| !a.is_aggregated())
                    .map(|(i, _)| HeadPos::from_index(i))
                    .collect(),
            },
        };
        Ok(Self { aggr, kind, bodies })
    }

    fn arity(&self) -> usize {
        self.aggr.len()
    }
}

/// One definition in a stratum: an inline rule set, or a fixed rule with
/// its declared output arity.
#[derive(Debug)]
pub enum EvalDefinition<R, F> {
    Rules(EvalRuleSet<R>),
    Fixed { arity: usize, rule: F },
}

/// One stratum, keyed by store name in canonical order — the order of the
/// merge barrier.
#[derive(Debug)]
pub struct EvalStratum<R, F> {
    pub defs: BTreeMap<MagicSymbol, EvalDefinition<R, F>>,
}

impl<R, F> EvalStratum<R, F> {
    /// Empty stratum — no definitions yet.
    pub(crate) fn empty() -> Self {
        Self {
            defs: BTreeMap::new(),
        }
    }
}

/// The evaluable program: execution-ordered strata with the entry proven
/// present in the final stratum. The compile tier mints this from a
/// [`crate::exec::plan::program::StratifiedMagicProgram`] stratum by stratum,
/// carrying that tier's entry proof forward.
#[derive(Debug)]
pub struct EvalProgram<R, F> {
    pub(crate) strata: Vec<EvalStratum<R, F>>,
    entry: MagicSymbol,
}

impl<R, F> EvalProgram<R, F> {
    /// Mint from execution-ordered strata; proves the entry sits in the
    /// final stratum (the mirror of `StratifiedMagicProgram`'s proof).
    pub fn from_execution_order(strata: Vec<EvalStratum<R, F>>) -> Result<Self> {
        let entry = strata
            .last()
            .and_then(|last| last.defs.keys().find(|k| k.is_prog_entry()))
            .cloned()
            // This later-tier site is structurally unreachable once
            // `InputProgram::new` has proven an entry exists, so it carries
            // no span (see `NoEntry::spanless` in `kyzo_model::program::rule`).
            .ok_or(kyzo_model::program::rule::NoEntry::spanless())?;
        Ok(Self { strata, entry })
    }

    /// The program entry store — provenance certificates target this symbol.
    pub(crate) fn entry(&self) -> &MagicSymbol {
        &self.entry
    }
}

// ─────────────────────────────────────────────────────────────────────────
// The query limiter (`:limit` / `:offset` early return)
// ─────────────────────────────────────────────────────────────────────────

/// The row limit of a query: how many entry rows to produce before
/// stopping early, and how many of the first to flag as offset-skipped.
#[derive(Debug, Copy, Clone)]
pub struct RowLimit {
    /// `limit + offset` when a limit is given (see
    /// `QueryOutOptions::num_to_take`); `None` disables the limiter.
    pub num_to_take: Option<usize>,
    pub num_to_skip: Option<usize>,
}

impl RowLimit {
    /// No limit / offset — every entry row is produced.
    pub fn unlimited() -> Self {
        Self {
            num_to_take: None,
            num_to_skip: None,
        }
    }
}

/// The shared early-return counter. Only entry-rule evaluations advance it
/// (and, under a limit, they run sequentially — see the module doc), so
/// its readings are deterministic.
struct QueryLimiter {
    total: Option<usize>,
    skip: Option<usize>,
    counter: AtomicUsize,
}

impl QueryLimiter {
    fn new(limit: RowLimit) -> Self {
        Self {
            total: limit.num_to_take,
            skip: limit.num_to_skip,
            counter: AtomicUsize::new(0),
        }
    }
    fn enabled(&self) -> bool {
        self.total.is_some()
    }
    /// Count one produced entry row; true when the take-count is reached.
    fn incr_and_should_stop(&self) -> bool {
        if let Some(limit) = self.total {
            let old_count = self.counter.fetch_add(1, Ordering::Relaxed);
            old_count + 1 >= limit
        } else {
            false
        }
    }
    fn is_stopped(&self) -> bool {
        if let Some(limit) = self.total {
            self.counter.load(Ordering::Acquire) >= limit
        } else {
            false
        }
    }
    /// Whether the next produced row still falls inside the `:offset`.
    fn should_skip_next(&self) -> bool {
        match self.skip {
            None => false,
            Some(i) => i > self.counter.load(Ordering::Relaxed),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// The evaluator
// ─────────────────────────────────────────────────────────────────────────

/// What evaluation produced: the entry rule's store, and whether the
/// limiter filtered it (in which case its authoritative rows are
/// [`EpochStore::early_returned_iter`] — offset-skipped rows excluded —
/// rather than the full contents).
#[derive(Debug)]
pub struct EvalOutcome {
    pub store: EpochStore,
    pub limited: bool,
}

/// Typed lookup for the cross-stage invariant "every referenced rule has a
/// store" (upstream panic sites 4–6).
pub fn store_of<'m>(
    stores: &'m BTreeMap<MagicSymbol, EpochStore>,
    name: &MagicSymbol,
) -> Result<&'m EpochStore> {
    stores
        .get(name)
        .ok_or_else(|| EvalInvariantError("a referenced rule has no store").into())
}

/// The `get_mut` twin of [`store_of`] (upstream panic site 3).
fn store_of_mut<'m>(
    stores: &'m mut BTreeMap<MagicSymbol, EpochStore>,
    name: &MagicSymbol,
) -> Result<&'m mut EpochStore> {
    stores
        .get_mut(name)
        .ok_or_else(|| EvalInvariantError("a merged rule has no store").into())
}

/// Evaluate a stratified program to its fixpoint, stratum by stratum.
/// Returns the entry store (the mirror of the original's
/// `stratified_magic_evaluate`).
///
/// - `lifetimes`: stores are dropped before a stratum runs unless still
///   live there ([`crate::exec::plan::program::StoreLifetimes::is_live_at`]).
/// - `limit`: `:limit`/`:offset` early return, applied to the entry rule.
/// - `budget`: required — see [`Budget`].
/// - `witnesses`: `Some` opts in to first-witness provenance recording.
pub fn stratified_evaluate<R: RuleBody, F: FixedRuleEval>(
    program: &EvalProgram<R, F>,
    lifetimes: &crate::exec::plan::program::StoreLifetimes,
    limit: RowLimit,
    budget: &Budget,
    witnesses: Option<&mut WitnessTable>,
) -> Result<EvalOutcome> {
    Ok(stratified_evaluate_with_stores(program, lifetimes, limit, budget, witnesses)?.0)
}

/// [`stratified_evaluate`], additionally returning the final store map
/// (minus the entry store, which rides in the outcome). This is the
/// provenance entry point: [`crate::exec::provenance::eval::provenance_graph`]
/// enumerates grounded
/// derivations over these completed stores. Stores dropped by `lifetimes`
/// en route are absent from the map — a caller wanting provenance must
/// keep every rule store live through the final stratum, or the
/// enumeration refuses (typed) on the missing store.
pub(crate) fn stratified_evaluate_with_stores<R: RuleBody, F: FixedRuleEval>(
    program: &EvalProgram<R, F>,
    lifetimes: &crate::exec::plan::program::StoreLifetimes,
    limit: RowLimit,
    budget: &Budget,
    mut witnesses: Option<&mut WitnessTable>,
) -> Result<(EvalOutcome, BTreeMap<MagicSymbol, EpochStore>)> {
    let mut stores: BTreeMap<MagicSymbol, EpochStore> = BTreeMap::new();
    let mut spent_derived: u64 = 0;
    let mut limited = false;
    for (stratum_idx, stratum) in program.strata.iter().enumerate() {
        if stratum_idx > 0 {
            // Drop stores that have outlived their last reader.
            stores.retain(|name, _| lifetimes.is_live_at(name, stratum_idx));
        }
        for (name, def) in &stratum.defs {
            let store = match def {
                EvalDefinition::Rules(rule_set)
                    if matches!(rule_set.kind, HeadAggrKind::Meet { .. }) =>
                {
                    EpochStore::new_meet(&rule_set.aggr)?
                }
                EvalDefinition::Rules(rule_set) => EpochStore::new_normal(rule_set.arity()),
                EvalDefinition::Fixed { arity, .. } => EpochStore::new_normal(*arity),
            };
            let clobbered = stores.insert(name.clone(), store);
            // Stratification (`stratify.rs`) places every rule name into
            // EXACTLY ONE stratum — a second definition of the same store
            // showing up here would mean two strata both claim to define
            // `name`, silently overwriting whichever one ran first with an
            // empty store.
            if clobbered.is_some() {
                return Err(EvalInvariantError(
                    "a rule name must belong to exactly one stratum; store redefined across strata",
                )
                .into());
            }
        }
        limited = evaluate_stratum(
            &stratum.defs,
            &mut stores,
            limit,
            budget,
            &mut spent_derived,
            witnesses.as_deref_mut(),
        )?;
    }
    let store = stores.remove(&program.entry).ok_or(EvalInvariantError(
        "the entry store vanished during evaluation",
    ))?;
    Ok((EvalOutcome { store, limited }, stores))
}

/// One stratum's semi-naive fixpoint (the mirror of the original's
/// `semi_naive_magic_evaluate`). Returns whether the limiter filtered the
/// entry store.
pub(crate) fn evaluate_stratum<R: RuleBody, F: FixedRuleEval>(
    defs: &BTreeMap<MagicSymbol, EvalDefinition<R, F>>,
    stores: &mut BTreeMap<MagicSymbol, EpochStore>,
    limit: RowLimit,
    budget: &Budget,
    spent_derived: &mut u64,
    mut witnesses: Option<&mut WitnessTable>,
) -> Result<bool> {
    let limiter = QueryLimiter::new(limit);
    let used_limiter = AtomicBool::new(false);
    let recording = witnesses.is_some();

    for epoch in 0..budget.epoch_ceiling.get() {
        budget.check_interrupt()?;
        let borrowed_stores = &*stores;
        // The globally admitted total as of this epoch's barrier — fixed
        // for the whole epoch, so every rule's mid-epoch spend check reads
        // the same deterministic baseline (see [`InterruptTicker`]).
        let epoch_baseline = *spent_derived;

        // One rule set's epoch: dispatch on kind and on epoch 0 vs later.
        // Runs (possibly in parallel) against the immutable store map; all
        // writes go to the fresh out-store it returns.
        let execution = |(name, def): (&MagicSymbol, &EvalDefinition<R, F>)| -> Result<_> {
            let (engaged_limiter, out, pending) = match def {
                EvalDefinition::Rules(rule_set) => match &rule_set.kind {
                    HeadAggrKind::None => {
                        if epoch == 0 {
                            initial_plain_eval(
                                name,
                                rule_set,
                                borrowed_stores,
                                &limiter,
                                budget,
                                epoch_baseline,
                                recording,
                            )?
                        } else {
                            incremental_plain_eval(
                                name,
                                rule_set,
                                borrowed_stores,
                                &limiter,
                                budget,
                                epoch_baseline,
                                recording,
                            )?
                        }
                    }
                    HeadAggrKind::Normal => {
                        if epoch == 0 {
                            initial_normal_aggr_eval(
                                name,
                                rule_set,
                                borrowed_stores,
                                &limiter,
                                budget,
                                epoch_baseline,
                            )?
                        } else {
                            // Complete at epoch 0 (all dependencies sit in
                            // lower strata); an empty store clears the delta.
                            (
                                false,
                                RegularTempStore::new().wrap(),
                                PendingWitnesses::new(),
                            )
                        }
                    }
                    HeadAggrKind::Meet { key_positions } => {
                        if epoch == 0 {
                            initial_meet_eval(
                                name,
                                rule_set,
                                borrowed_stores,
                                budget,
                                epoch_baseline,
                                recording,
                                key_positions,
                            )?
                        } else {
                            incremental_meet_eval(
                                name,
                                rule_set,
                                borrowed_stores,
                                budget,
                                epoch_baseline,
                                recording,
                                key_positions,
                            )?
                        }
                    }
                },
                EvalDefinition::Fixed { rule, .. } => {
                    if epoch == 0 {
                        let mut out = RegularTempStore::new();
                        rule.run(borrowed_stores, &mut out, budget, epoch_baseline)?;
                        (false, out.wrap(), PendingWitnesses::new())
                    } else {
                        // Fixed rules run exactly once.
                        (
                            false,
                            RegularTempStore::new().wrap(),
                            PendingWitnesses::new(),
                        )
                    }
                }
            };
            used_limiter.fetch_or(engaged_limiter, Ordering::Relaxed);
            Ok((name.clone(), out, pending))
        };

        let mut to_merge: BTreeMap<MagicSymbol, (TempStore, PendingWitnesses)> = BTreeMap::new();
        let limiter_enabled = limiter.enabled();
        // Entry rules under a limit run sequentially, in order, stopping
        // as soon as the take-count is reached: the deterministic-ordering
        // carve-out preserved from the original (its eval.rs:261).
        for res in defs
            .iter()
            .filter(|(name, _)| limiter_enabled && name.is_prog_entry())
            .map(&execution)
        {
            let (name, out, pending) = res?;
            to_merge.insert(name, (out, pending));
            if limiter.is_stopped() {
                break;
            }
        }
        // Non-entry batch: rayon on native, sequential on wasm32.
        for res in crate::exec::fixpoint::parallel::collect_non_entry_batch(
            defs,
            limiter_enabled,
            execution,
        ) {
            let (name, out, pending) = res?;
            to_merge.insert(name, (out, pending));
        }

        // ── The merge barrier ────────────────────────────────────────────
        // Sequential, in canonical store order: this is where admissions
        // happen (witnesses bind, Admitted counts accrue) and the ONLY
        // place the deterministic budget dimensions may be checked.
        let mut changed = false;
        let mut epoch_admitted: u64 = 0;
        for (name, (out, pending)) in to_merge {
            let epoch_store = store_of_mut(stores, &name)?;
            let admitted = match witnesses.as_deref_mut() {
                Some(table) => {
                    let key_mode = match defs.get(&name) {
                        Some(EvalDefinition::Rules(rule_set)) => match &rule_set.kind {
                            HeadAggrKind::Meet { key_positions } => {
                                WitnessKeyMode::MeetGroup(key_positions.as_slice())
                            }
                            HeadAggrKind::None | HeadAggrKind::Normal => WitnessKeyMode::FullTuple,
                        },
                        Some(EvalDefinition::Fixed { .. }) | None => WitnessKeyMode::FullTuple,
                    };
                    let mut binder = WitnessBinder {
                        store: &name,
                        pending: &pending,
                        key_mode,
                        table,
                    };
                    epoch_store.merge_in(out, &mut binder)?
                }
                None => epoch_store.merge_in(out, &mut ())?,
            };
            epoch_admitted += match u64::try_from(admitted.0) {
                Ok(v) => v,
                Err(_gt_u64) => u64::MAX,
            };
            changed |= epoch_store.has_delta();
        }
        *spent_derived += epoch_admitted;
        if let Some(ceiling) = budget.derived_tuple_ceiling
            && *spent_derived > ceiling
        {
            return Err(LimitExceeded {
                dimension: BudgetDimension::DerivedTuples,
                spent: *spent_derived,
                ceiling,
                rule: None,
                span: None,
            }
            .into());
        }
        if !changed {
            // Fixpoint: every delta is empty.
            return Ok(used_limiter.load(Ordering::Acquire));
        }
    }
    // The epoch ceiling exhausted without a fixpoint: the deterministic
    // refusal that replaces the original's unbounded loop.
    let ceiling = u64::from(budget.epoch_ceiling.get());
    Err(LimitExceeded {
        dimension: BudgetDimension::Epochs,
        spent: ceiling,
        ceiling,
        rule: None,
        span: None,
    }
    .into())
}

/// Record the first derivation of a candidate tuple for provenance.
fn note_pending(
    pending: &mut PendingWitnesses,
    key: Tuple,
    rule_n: usize,
    premises: &Premises<'_>,
) {
    pending.entry(key).or_insert_with(|| {
        (
            crate::exec::provenance::semiring::DerivationId::from_rule_index(rule_n),
            premises.to_rows(),
        )
    });
}

/// A tuple's projection onto the given head positions, in the order the
/// positions are listed. For a meet head this is the grouping key — eval's
/// mirror of the store's [`MeetAggrStore`] layout projection, so the two
/// agree on a group's identity whatever positions the meet columns occupy.
pub(crate) fn project_positions(row: &[DataValue], positions: &[HeadPos]) -> Tuple {
    positions.iter().map(|p| row[p.get()].clone()).collect()
}

/// Shared ingest for one plain-rule derivation: dedup → optional provenance
/// note → limiter put. Used by both epoch-0 and semi-naive epochs; the
/// epochs differ in *which* derivations are offered (full body vs delta
/// occurrence), not in how a new row is admitted to `out`.
#[allow(clippy::too_many_arguments)] // eval staging arity — pending/limiter/out are distinct seats
fn ingest_plain_derivation(
    out: &mut RegularTempStore,
    pending: &mut PendingWitnesses,
    limiter: &QueryLimiter,
    should_check_limit: bool,
    recording: bool,
    rule_n: usize,
    item: Cow<'_, [DataValue]>,
    premises: &Premises<'_>,
    hit_limit: &mut bool,
) -> Result<ControlFlow<()>> {
    if should_check_limit {
        if !out.exists(&item) {
            let item = Tuple::from_vec(item.into_owned());
            if recording {
                note_pending(pending, item.clone(), rule_n, premises);
            }
            if limiter.should_skip_next() {
                out.put_with_skip(item);
            } else {
                out.put(item);
            }
            if limiter.incr_and_should_stop() {
                *hit_limit = true;
                return Ok(ControlFlow::Break(()));
            }
        }
    } else if !out.exists(&item) {
        let item = Tuple::from_vec(item.into_owned());
        if recording {
            note_pending(pending, item.clone(), rule_n, premises);
        }
        out.put(item);
    }
    Ok(ControlFlow::Continue(()))
}

/// Epoch 0 for a plain (non-aggregating) rule set. Returns
/// `(engaged_limiter, out, pending)`; `engaged_limiter` is true whenever
/// this evaluation applied the limiter (limit set and rule is the entry),
/// telling the caller the entry store's rows are limit-filtered.
fn initial_plain_eval<R: RuleBody>(
    rule_symb: &MagicSymbol,
    rule_set: &EvalRuleSet<R>,
    stores: &BTreeMap<MagicSymbol, EpochStore>,
    limiter: &QueryLimiter,
    budget: &Budget,
    baseline: u64,
    recording: bool,
) -> Result<(bool, TempStore, PendingWitnesses)> {
    let mut out = RegularTempStore::new();
    let mut pending = PendingWitnesses::new();
    let should_check_limit = limiter.enabled() && rule_symb.is_prog_entry();
    let mut ticker = budget.ticker(baseline, rule_symb);
    for (rule_n, body) in rule_set.bodies.iter().enumerate() {
        let mut hit_limit = false;
        body.for_each_derivation(stores, None, recording, &mut |item, premises| {
            ticker.tick(out.len())?;
            ingest_plain_derivation(
                &mut out,
                &mut pending,
                limiter,
                should_check_limit,
                recording,
                rule_n,
                item,
                &premises,
                &mut hit_limit,
            )
        })?;
        if hit_limit {
            return Ok((true, out.wrap(), pending));
        }
        budget.check_interrupt()?;
    }
    Ok((should_check_limit, out.wrap(), pending))
}

/// Epochs > 0 for a plain rule set: the semi-naive delta discipline. One
/// derivation pass per body-atom OCCURRENCE whose named store has a delta
/// this epoch, in canonical (occurrence-index) order — a store mentioned
/// twice in one body (the self-join shape) gets two independent passes,
/// each narrowing a DIFFERENT occurrence to delta while every other
/// occurrence (including the other occurrence of the same store) reads
/// its total: `Δ(P⋈P) = (ΔP⋈P) ∪ (P⋈ΔP)`.
fn incremental_plain_eval<R: RuleBody>(
    rule_symb: &MagicSymbol,
    rule_set: &EvalRuleSet<R>,
    stores: &BTreeMap<MagicSymbol, EpochStore>,
    limiter: &QueryLimiter,
    budget: &Budget,
    baseline: u64,
    recording: bool,
) -> Result<(bool, TempStore, PendingWitnesses)> {
    let prev_store = store_of(stores, rule_symb)?;
    let mut out = RegularTempStore::new();
    let mut pending = PendingWitnesses::new();
    let should_check_limit = limiter.enabled() && rule_symb.is_prog_entry();
    let mut ticker = budget.ticker(baseline, rule_symb);

    for (rule_n, body) in rule_set.bodies.iter().enumerate() {
        // A `Cell` because `handle` lives across the per-occurrence passes
        // while the flag is read between them.
        let hit_limit = std::cell::Cell::new(false);
        let mut handle =
            |item: Cow<'_, [DataValue]>, premises: Premises<'_>| -> Result<ControlFlow<()>> {
                ticker.tick(out.len())?;
                if prev_store.exists(&item) {
                    // Re-derived: already in the total, invisible to the next
                    // epoch — this is what terminates the fixpoint. The probe
                    // runs on the slice, so this dominant case mints nothing.
                    return Ok(ControlFlow::Continue(()));
                }
                let mut hit = false;
                let flow = ingest_plain_derivation(
                    &mut out,
                    &mut pending,
                    limiter,
                    should_check_limit,
                    recording,
                    rule_n,
                    item,
                    &premises,
                    &mut hit,
                )?;
                if hit {
                    hit_limit.set(true);
                }
                Ok(flow)
            };

        for (occurrence, store_name) in body.contained_rules() {
            if !store_of(stores, store_name)?.has_delta() {
                // This occurrence's own store didn't change: a pass narrowed
                // to its (empty) delta would derive nothing — skip it rather
                // than pay for a no-op join.
                continue;
            }
            body.for_each_derivation(stores, Some(*occurrence), recording, &mut handle)?;
            if hit_limit.get() {
                return Ok((true, out.wrap(), pending));
            }
            budget.check_interrupt()?;
        }
    }
    Ok((should_check_limit, out.wrap(), pending))
}

/// Epoch 0 for a meet-aggregation head: fold every derived row into a
/// fresh [`MeetAggrStore`]. If nothing was derived and *every* position is
/// aggregated, the identity row (each meet's `init_val`) is inserted as a
/// real fact the recursion builds on — and only then, so the identity can
/// never leak alongside real derivations (see the oracle's law).
fn initial_meet_eval<R: RuleBody>(
    rule_symb: &MagicSymbol,
    rule_set: &EvalRuleSet<R>,
    stores: &BTreeMap<MagicSymbol, EpochStore>,
    budget: &Budget,
    baseline: u64,
    recording: bool,
    key_positions: &[HeadPos],
) -> Result<(bool, TempStore, PendingWitnesses)> {
    let mut out = MeetAggrStore::new(rule_set.aggr.clone())?;
    let mut pending = PendingWitnesses::new();
    let mut ticker = budget.ticker(baseline, rule_symb);
    for (rule_n, body) in rule_set.bodies.iter().enumerate() {
        body.for_each_derivation(stores, None, recording, &mut |item, premises| {
            ticker.tick(out.len())?;
            if recording {
                // Witnesses for a meet head are per group (the seam's
                // documented boundary), keyed by the grouping projection —
                // the non-aggregated positions wherever they sit, not a
                // prefix.
                note_pending(
                    &mut pending,
                    project_positions(&item, key_positions),
                    rule_n,
                    &premises,
                );
            }
            out.meet_put(&item)?;
            Ok(ControlFlow::Continue(()))
        })?;
        budget.check_interrupt()?;
    }
    if out.is_empty() && rule_set.aggr.iter().all(|a| a.is_aggregated()) {
        // No pending entry: the identity row's witness is `None` by design.
        out.seed_identity()?;
    }
    Ok((false, out.wrap(), pending))
}

/// Epochs > 0 for a meet head: the same delta discipline as the plain
/// path, folding into a fresh meet store (whose merge admits a group only
/// when its folded value actually changed — the landed changed-flag
/// contract).
fn incremental_meet_eval<R: RuleBody>(
    rule_symb: &MagicSymbol,
    rule_set: &EvalRuleSet<R>,
    stores: &BTreeMap<MagicSymbol, EpochStore>,
    budget: &Budget,
    baseline: u64,
    recording: bool,
    key_positions: &[HeadPos],
) -> Result<(bool, TempStore, PendingWitnesses)> {
    let mut out = MeetAggrStore::new(rule_set.aggr.clone())?;
    let mut pending = PendingWitnesses::new();
    let mut ticker = budget.ticker(baseline, rule_symb);
    // The rule's running total, against which a re-derivation counts as
    // in-flight ONLY if the barrier would actually admit it. This is the
    // meet twin of the plain path's `!prev_store.exists(item)` filter: a
    // meet epoch folds every re-derived group into a FRESH out-store, so
    // `out.len()` counts unchanged re-derivations the barrier drops — the
    // defect that refused completing programs. Counting admissions here
    // instead makes `in_flight ≤ admitted_r` hold BY CONSTRUCTION (see
    // `MeetAggrStore::meet_put_admission_faithful`: monotone meet ⇒ each
    // admitted group contributes exactly one tick, so the guard fires only
    // when the barrier would too, and its `spent` is true admitted spend).
    let total_meet = store_of(stores, rule_symb)?.meet_total()?;
    // Admission-faithful in-flight count for the whole rule (spans bodies),
    // the meet analogue of the plain path's cross-body `out.len()`.
    let mut effective: u64 = 0;
    for (rule_n, body) in rule_set.bodies.iter().enumerate() {
        let mut handle =
            |item: Cow<'_, [DataValue]>, premises: Premises<'_>| -> Result<ControlFlow<()>> {
                if recording {
                    note_pending(
                        &mut pending,
                        project_positions(&item, key_positions),
                        rule_n,
                        &premises,
                    );
                }
                // The meet stores are slice consumers end to end: no derived
                // row is ever minted on this path, owned or not.
                if out.meet_put_admission_faithful(&item, &total_meet)? {
                    effective += 1;
                }
                let effective_usize = match usize::try_from(effective) {
                    Ok(v) => v,
                    Err(_gt_usize) => usize::MAX,
                };
                ticker.tick(effective_usize)?;
                Ok(ControlFlow::Continue(()))
            };
        for (occurrence, store_name) in body.contained_rules() {
            if !store_of(stores, store_name)?.has_delta() {
                continue;
            }
            body.for_each_derivation(stores, Some(*occurrence), recording, &mut handle)?;
            budget.check_interrupt()?;
        }
    }
    Ok((false, out.wrap(), pending))
}

/// The one-shot evaluation of a normal-aggregation head (epoch 0 of its
/// stratum): group every rule's derived rows by the non-aggregated head
/// positions, fold each group through fresh normal ops, one output row per
/// group — groups shared across the head's rules. No rows with every
/// position aggregated yields the single empty-fold row. Aggregated rows
/// carry no per-row witness (`derivation: None` at admission).
fn initial_normal_aggr_eval<R: RuleBody>(
    rule_symb: &MagicSymbol,
    rule_set: &EvalRuleSet<R>,
    stores: &BTreeMap<MagicSymbol, EpochStore>,
    limiter: &QueryLimiter,
    budget: &Budget,
    baseline: u64,
) -> Result<(bool, TempStore, PendingWitnesses)> {
    let mut out = RegularTempStore::new();
    let should_check_limit = limiter.enabled() && rule_symb.is_prog_entry();
    let signature = &rule_set.aggr;

    let key_indices: Vec<usize> = signature
        .iter()
        .enumerate()
        .filter(|(_, a)| !a.is_aggregated())
        .map(|(i, _)| i)
        .collect();
    let val_specs: Vec<(usize, &Aggregation, &[DataValue])> = signature
        .iter()
        .enumerate()
        .filter_map(|(i, a)| {
            a.as_aggregated()
                .map(|(aggregation, args)| (i, aggregation, args))
        })
        .collect();
    let fresh_ops = || -> Result<Vec<NormalAggr>> {
        val_specs
            .iter()
            .map(|(_, aggregation, args)| crate::exec::fold::aggr::normal_op(aggregation, args))
            .collect()
    };

    let mut aggr_work: BTreeMap<Tuple, Vec<NormalAggr>> = BTreeMap::new();
    let mut ticker = budget.ticker(baseline, rule_symb);
    for body in &rule_set.bodies {
        body.for_each_derivation(stores, None, false, &mut |item, _premises| {
            ticker.tick(aggr_work.len())?;
            let key: Tuple = key_indices.iter().map(|i| item[*i].clone()).collect();
            let ops = match aggr_work.entry(key) {
                std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::btree_map::Entry::Vacant(e) => e.insert(fresh_ops()?),
            };
            for (op, (i, _, _)) in ops.iter_mut().zip(&val_specs) {
                op.set(&item[*i])?;
            }
            Ok(ControlFlow::Continue(()))
        })?;
        budget.check_interrupt()?;
    }

    if aggr_work.is_empty() && key_indices.is_empty() && !val_specs.is_empty() {
        let empty_fold: Tuple = fresh_ops()?
            .iter()
            .map(|op| op.get())
            .collect::<Result<_>>()?;
        out.put(empty_fold);
    }

    for (key, ops) in aggr_work {
        let mut row = Tuple::from_vec(vec![DataValue::Null; signature.len()]);
        for (slot, i) in key_indices.iter().enumerate() {
            row[*i] = key[slot].clone();
        }
        for (op, (i, _, _)) in ops.iter().zip(&val_specs) {
            row[*i] = op.get()?;
        }
        ticker.tick(out.len())?;
        if should_check_limit {
            if !out.exists(row.as_slice()) {
                if limiter.should_skip_next() {
                    out.put_with_skip(row);
                } else {
                    out.put(row);
                }
                if limiter.incr_and_should_stop() {
                    return Ok((true, out.wrap(), PendingWitnesses::new()));
                }
            }
        } else {
            out.put(row);
        }
    }
    Ok((should_check_limit, out.wrap(), PendingWitnesses::new()))
}

// ═════════════════════════════════════════════════════════════════════════
// Tests: the oracle differentials, the determinism law, budget refusals
// ═════════════════════════════════════════════════════════════════════════

// Oracle-differential fixpoint unit corpus: see kyzo-trials gauntlet /
// verify_differential (crate wall forbids kyzo_oracle inside kyzo-core).
