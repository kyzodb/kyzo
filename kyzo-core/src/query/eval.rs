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
 *   Poison survives only as the user-kill flag folded into the same
 *   sites. Refusals are typed: [`LimitExceeded`] and [`Killed`].
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
 * Upstream panic-site audit (Law 5), all 13 sites in the original file:
 *   1. eval.rs:109  `stores.remove(entry).ok_or(NoEntryError)` — already an
 *      error; here [`EvalProgram::from_execution_order`] proves the entry
 *      exists in the final stratum, and the residual lookup is a typed
 *      [`EvalInvariantError`].
 *   2. eval.rs:91   `unreachable!()` on a fixed rule set reaching the meet
 *      path — structurally removed: [`EvalRuleSet`] carries its
 *      [`HeadAggrKind`] and its store is minted from the same value.
 *   3. eval.rs:293  `stores.get_mut(k).unwrap()` at the merge barrier —
 *      typed [`EvalInvariantError`] via [`store_of_mut`].
 *   4. eval.rs:516  `stores.get(rule_symb).unwrap()` (previous-total lookup)
 *      — typed via [`store_of`].
 *   5. eval.rs:524  `stores.get(symb).unwrap()` (delta discipline, plain)
 *      — typed via [`store_of`].
 *   6. eval.rs:628  the same lookup in the meet path — typed via
 *      [`store_of`].
 *   7. eval.rs:372  `a.as_ref().unwrap()` building the meet identity row —
 *      the checked all-aggregated condition drives a `flatten()`.
 *   8. eval.rs:373  `aggr.meet_op.as_ref().unwrap()` — the landed
 *      `Aggregation::meet_op` returns `Option`; a `None` under the Meet
 *      classification is a typed [`EvalInvariantError`].
 *   9. eval.rs:430  `normal_op.as_mut().unwrap().set(..)` — gone: the
 *      landed aggregation API mints live ops per group
 *      (`Aggregation::normal_op(args) -> Result<Box<dyn NormalAggrObj>>`),
 *      there is no `Option` field to unwrap.
 *  10. eval.rs:439  the vacant-entry twin of 9 — gone the same way.
 *  11. eval.rs:467-470  `a.as_ref().unwrap()` + `normal_op.unwrap()` for
 *      the empty fold — gone the same way.
 *  12. eval.rs:482  `aggrs[idx].normal_op.as_ref().unwrap().get()` — gone
 *      the same way.
 *  13. `ruleset[0]` indexing throughout — [`EvalRuleSet::new`] refuses
 *      empty rule sets, so the signature accessor is structurally total.
 *
 * Documented deviations from the original, beyond the ratified ones:
 *   D1. Limiter skip-flags are recorded only for the entry rule
 *       (`should_check_limit`); the original's incremental path called
 *       `should_skip_next` for every rule, flagging tuples in non-entry
 *       stores. Those flags were dead (only the entry store's flags are
 *       read) but misleading; the initial path already gated them.
 *   D2. The incremental limiter path deduplicates against the epoch's own
 *       out-store (`!out.exists(&item)`) exactly as the original's initial
 *       path did. The original's incremental path did not, so a tuple
 *       re-derived twice within one epoch double-counted toward `:limit`
 *       and could early-stop the entry rule short of the requested rows.
 *   D3. An all-meet head whose aggregated positions are not a suffix is
 *       refused at [`EvalRuleSet::new`] with a typed error. The original
 *       silently demoted that shape to a *normal* aggregation and froze it
 *       after epoch 0, dropping recursive derivations (see the oracle's
 *       divergence note in `query/laws.rs`). Full oracle parity (evaluating
 *       such heads inside recursion) requires `MeetAggrStore` to grow
 *       positional grouping; until then the shape is a loud refusal, never
 *       a wrong answer.
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
//! [`crate::data::program::StratifiedMagicProgram`]) over the delta stores
//! of [`crate::query::temp_store`]. Per stratum, rules are evaluated
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
//! The deadline and the user-kill flag are *interrupts*, not deterministic
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
//! - [`AdmissionSink`]: the landed seam in `runtime/temp_store.rs`; the
//!   provenance [`WitnessBinder`] and the `()` off-state both flow through
//!   it.
//! - WASM: on `wasm32` the per-epoch batch runs sequentially (no rayon),
//!   as in the original. A [`Budget`] without a timeout never touches the
//!   clock, so timeout-less budgets are wasm-safe; the wasm binding must
//!   not set `with_timeout` until a clock shim lands there.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use miette::{Diagnostic, Result};
#[cfg(not(target_arch = "wasm32"))]
use rayon::prelude::*;
use thiserror::Error;

use crate::data::aggr::{Aggregation, NormalAggrObj};
use crate::data::program::MagicSymbol;
use crate::data::span::SourceSpan;
use crate::data::tuple::Tuple;
use crate::data::value::DataValue;
use crate::query::levels::EpochStore;
use crate::query::semiring::{Derivation, DerivationGraph};
use crate::query::temp_store::{
    AdmissionSink, MeetAggrStore, RegularTempStore, TempStore, TupleInIter,
};

/// One head position's aggregation, if any — the shape carried through
/// every program tier (see `MagicInlineRule::aggr` in `data/program.rs`).
type HeadAggr = Option<(Aggregation, Vec<DataValue>)>;

// ─────────────────────────────────────────────────────────────────────────
// Refusals and invariants
// ─────────────────────────────────────────────────────────────────────────

/// A budget dimension. The first three are **deterministic**: their spend
/// is a function of program+facts+budget alone, so their refusals are
/// byte-identical on every run at any thread count. `Deadline` is an
/// interrupt: its spend is wall-clock elapsed milliseconds.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum BudgetDimension {
    /// Derivations admitted to any store's total, summed over the whole
    /// query — the [`Admitted`](crate::query::temp_store::Admitted)
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
pub(crate) struct LimitExceeded {
    pub(crate) dimension: BudgetDimension,
    pub(crate) spent: u64,
    pub(crate) ceiling: u64,
    /// The rule whose in-flight materialization crossed the ceiling
    /// (mid-epoch dimension only); `None` for the barrier dimensions.
    pub(crate) rule: Option<String>,
    /// The offending rule's source span, so the diagnostic points back at
    /// the query text. `None` for the barrier dimensions.
    #[label("this rule's in-flight derivations crossed the budget ceiling")]
    pub(crate) span: Option<SourceSpan>,
}

/// The user killed the query. The flag is *read* at the same check sites
/// as the deadline — the original's `Poison`, minus its sleeper thread.
#[derive(Debug, Error, Diagnostic)]
#[error("query killed")]
#[diagnostic(code(eval::killed))]
pub(crate) struct Killed;

/// A cross-stage invariant that construction should have made impossible
/// (e.g. "every referenced rule has a store"). Surfaced as an error, never
/// an abort, so corruption of the proof is loud but recoverable.
#[derive(Debug, Error, Diagnostic)]
#[error("evaluation invariant violated: {0}")]
#[diagnostic(code(eval::invariant), help("This is a bug. Please report it."))]
struct EvalInvariantError(&'static str);

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
/// checked at epoch barriers only. The deadline and the kill flag are
/// checked at every barrier *and* inside rule iteration.
#[derive(Debug, Clone)]
pub(crate) struct Budget {
    epoch_ceiling: NonZeroU32,
    derived_tuple_ceiling: Option<u64>,
    deadline: Option<Deadline>,
    kill: Option<Arc<AtomicBool>>,
}

impl Budget {
    /// A budget with the one mandatory dimension. Note that any stratum
    /// deriving anything needs at least two epochs (one to derive, one to
    /// observe the empty delta), so a ceiling of 1 refuses every non-empty
    /// program — deterministically.
    pub(crate) fn new(epoch_ceiling: NonZeroU32) -> Self {
        Self {
            epoch_ceiling,
            derived_tuple_ceiling: None,
            deadline: None,
            kill: None,
        }
    }

    /// Cap the total derivations admitted across the whole query.
    /// The deterministic derived-tuple ceiling, if armed — read by the
    /// session fixed-rule wrapper to arm its output writer.
    pub(crate) fn derived_tuple_ceiling(&self) -> Option<u64> {
        self.derived_tuple_ceiling
    }

    /// The fixpoint-epoch ceiling. `pub(crate)` (story #80): `laws.rs`'s
    /// naive oracle threads the same `Budget` through its own, simpler
    /// stratum/round loop (barrier-only, no per-rule in-flight ticker —
    /// that granularity is a production-only concern for bounding rayon's
    /// mid-epoch parallel materialization) and needs its ceiling to check
    /// against directly.
    pub(crate) fn epoch_ceiling(&self) -> NonZeroU32 {
        self.epoch_ceiling
    }

    pub(crate) fn with_derived_tuple_ceiling(mut self, ceiling: u64) -> Self {
        self.derived_tuple_ceiling = Some(ceiling);
        self
    }

    /// Arm a wall-clock timeout, starting now. (This is the only budget
    /// dimension that touches the clock; see the module doc's WASM note.)
    pub(crate) fn with_timeout(mut self, allotted: Duration) -> Self {
        self.deadline = Some(Deadline {
            started: Instant::now(),
            allotted,
        });
        self
    }

    /// Attach the user-kill flag (the session sets it; eval only reads).
    pub(crate) fn with_kill_flag(mut self, kill: Arc<AtomicBool>) -> Self {
        self.kill = Some(kill);
        self
    }

    /// The interrupt check: user kill, then deadline. Called at every
    /// epoch barrier and, via [`InterruptTicker`], inside rule iteration.
    /// `pub(crate)` (story #80): `laws.rs`'s naive oracle calls this
    /// directly at its own barrier points (visibility only; the check
    /// itself is unchanged).
    pub(crate) fn check_interrupt(&self) -> Result<()> {
        if let Some(kill) = &self.kill
            && kill.load(Ordering::Relaxed)
        {
            return Err(Killed.into());
        }
        if let Some(deadline) = &self.deadline {
            let elapsed = deadline.started.elapsed();
            if elapsed > deadline.allotted {
                return Err(LimitExceeded {
                    dimension: BudgetDimension::Deadline,
                    spent: elapsed.as_millis() as u64,
                    ceiling: deadline.allotted.as_millis() as u64,
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
    fn ticker<'a>(&'a self, baseline: u64, rule: &'a MagicSymbol) -> InterruptTicker<'a> {
        InterruptTicker {
            budget: self,
            countdown: INTERRUPT_STRIDE,
            baseline,
            ceiling: self.derived_tuple_ceiling,
            rule,
        }
    }
}

/// How many derivations may pass between interrupt checks inside a rule's
/// iteration. Small enough that no scan is unkillable for long; large
/// enough that the check does not dominate the loop.
const INTERRUPT_STRIDE: u32 = 64;

/// The in-iteration interrupt-and-spend site: `tick` once per derivation;
/// every [`INTERRUPT_STRIDE`]th tick reads the kill flag and the deadline
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
struct InterruptTicker<'a> {
    budget: &'a Budget,
    countdown: u32,
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
    fn tick(&mut self, in_flight: usize) -> Result<()> {
        self.countdown -= 1;
        if self.countdown == 0 {
            self.countdown = INTERRUPT_STRIDE;
            self.budget.check_interrupt()?;
            if let Some(ceiling) = self.ceiling {
                let spent = self.baseline.saturating_add(in_flight as u64);
                if spent > ceiling {
                    let symb = self.rule.as_plain_symbol();
                    return Err(LimitExceeded {
                        dimension: BudgetDimension::InFlightDerivations,
                        spent,
                        ceiling,
                        rule: Some(symb.name.to_string()),
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
pub(crate) struct AtomOccurrence(pub(crate) usize);

/// The premise rows of one derivation, for provenance. `NotRequested` when
/// eval did not ask (`want_premises` false) — the body implementation must
/// not pay to collect them in that case.
pub(crate) enum Premises<'a> {
    NotRequested,
    Rows(&'a [Tuple]),
}

impl Premises<'_> {
    fn to_rows(&self) -> Vec<Tuple> {
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
pub(crate) enum PremiseSource {
    /// The literal reads an in-memory rule store.
    Rule(MagicSymbol),
    /// The literal reads a base (ground-fact) relation, by name. The rows
    /// are attested by the body that read them; the independent
    /// certificate checker re-verifies membership from the model.
    Fact(String),
}

/// One rule body, as eval consumes it: a generator of satisfying head
/// tuples against the current stores. This is the seam where the compile
/// tier's relational-algebra plans plug in (bindings in, tuples out,
/// `delta_from` threaded to the stored-rule scans); the differential tests
/// implement it over the oracle's rule model.
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
pub(crate) trait RuleBody: Send + Sync {
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
    /// the body cannot attribute; [`provenance_graph`] then refuses with
    /// the typed [`ProvenanceUnsupported`] instead of building a graph
    /// with unattributed nodes.
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
/// [`crate::fixed_rule::FixedRuleOutput::new_budgeted`]) can count prior
/// admissions instead of starting from zero.
pub(crate) trait FixedRuleEval: Send + Sync {
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
/// classifies a whole rule set.)
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum HeadAggrKind {
    /// No aggregation: a plain rule set, re-derived every epoch it has
    /// changed dependencies.
    None,
    /// At least one non-meet aggregation: grouped and folded exactly once,
    /// at epoch 0 — stratification proves every dependency is complete
    /// strictly below, so epoch 0 already sees the fixpoint beneath.
    Normal,
    /// All aggregated positions are meet (semilattice) forms: folded into
    /// a [`MeetAggrStore`] *inside* recursion, epoch by epoch.
    Meet,
}

/// The rules of one head, ready to evaluate. Construction proves what the
/// original re-derived (or unwrapped) downstream: the rule set is
/// non-empty and every rule shares one aggregation signature. A meet head's
/// grouping positions may sit anywhere in the head — the landed
/// [`MeetAggrStore`] groups positionally — so there is no suffix
/// restriction to check (the retired `MeetNotSuffix` refusal).
#[derive(Debug)]
pub(crate) struct EvalRuleSet<R> {
    aggr: Vec<HeadAggr>,
    kind: HeadAggrKind,
    /// For a Meet head: the head positions that are grouping keys (the
    /// non-aggregated positions, in head order). Empty for non-meet heads.
    /// This is eval's copy of the store's [`MeetLayout`] key positions,
    /// used to key per-group provenance witnesses.
    meet_key_positions: Vec<usize>,
    bodies: Vec<R>,
}

/// A rule-set shape the evaluator refuses at construction.
#[derive(Debug, Error, Diagnostic)]
pub(crate) enum RuleSetShapeError {
    #[error("a rule set must contain at least one rule")]
    #[diagnostic(code(eval::empty_rule_set))]
    Empty,
}

impl<R> EvalRuleSet<R> {
    /// Classify and validate one head's rules. `aggr` is the head's
    /// per-position aggregation signature (uniform across the head's rules
    /// — the parser refuses disagreement as `parser::head_aggr_mismatch`,
    /// so the signature travels once, on the set).
    pub(crate) fn new(aggr: Vec<HeadAggr>, bodies: Vec<R>) -> Result<Self, RuleSetShapeError> {
        if bodies.is_empty() {
            return Err(RuleSetShapeError::Empty);
        }
        let has_aggr = aggr.iter().any(Option::is_some);
        let all_meet = aggr
            .iter()
            .flatten()
            .all(|(aggregation, _)| aggregation.is_meet());
        let kind = match (has_aggr, all_meet) {
            (false, _) => HeadAggrKind::None,
            (true, false) => HeadAggrKind::Normal,
            (true, true) => HeadAggrKind::Meet,
        };
        // The landed MeetAggrStore groups positionally, so the grouping
        // positions are simply the non-aggregated ones, wherever they sit —
        // no suffix restriction (the retired `MeetNotSuffix` refusal, which
        // the original needed because its store keyed by a byte prefix and
        // silently demoted non-suffix heads to a frozen normal aggregation,
        // dropping recursive derivations).
        let meet_key_positions = if kind == HeadAggrKind::Meet {
            aggr.iter()
                .enumerate()
                .filter(|(_, a)| a.is_none())
                .map(|(i, _)| i)
                .collect()
        } else {
            Vec::new()
        };
        Ok(Self {
            aggr,
            kind,
            meet_key_positions,
            bodies,
        })
    }

    fn arity(&self) -> usize {
        self.aggr.len()
    }
}

/// One definition in a stratum: an inline rule set, or a fixed rule with
/// its declared output arity.
#[derive(Debug)]
pub(crate) enum EvalDefinition<R, F> {
    Rules(EvalRuleSet<R>),
    Fixed { arity: usize, rule: F },
}

/// One stratum, keyed by store name in canonical order — the order of the
/// merge barrier.
#[derive(Debug)]
pub(crate) struct EvalStratum<R, F> {
    pub(crate) defs: BTreeMap<MagicSymbol, EvalDefinition<R, F>>,
}

// Manual: the derive would needlessly bound `R: Default, F: Default`.
impl<R, F> Default for EvalStratum<R, F> {
    fn default() -> Self {
        Self {
            defs: BTreeMap::new(),
        }
    }
}

/// The evaluable program: execution-ordered strata with the entry proven
/// present in the final stratum. The compile tier mints this from a
/// [`crate::data::program::StratifiedMagicProgram`] stratum by stratum,
/// carrying that tier's entry proof forward.
#[derive(Debug)]
pub(crate) struct EvalProgram<R, F> {
    strata: Vec<EvalStratum<R, F>>,
    entry: MagicSymbol,
}

impl<R, F> EvalProgram<R, F> {
    /// Mint from execution-ordered strata; proves the entry sits in the
    /// final stratum (the mirror of `StratifiedMagicProgram`'s proof).
    pub(crate) fn from_execution_order(strata: Vec<EvalStratum<R, F>>) -> Result<Self> {
        let entry = strata
            .last()
            .and_then(|last| last.defs.keys().find(|k| k.is_prog_entry()))
            .cloned()
            // This later-tier site is structurally unreachable once
            // `InputProgram::new` has proven an entry exists, so it carries
            // no span (see `NoEntryError::spanless` in `data/program.rs`).
            .ok_or(crate::data::program::NoEntryError::spanless())?;
        Ok(Self { strata, entry })
    }
}

// ─────────────────────────────────────────────────────────────────────────
// The query limiter (`:limit` / `:offset` early return)
// ─────────────────────────────────────────────────────────────────────────

/// The row limit of a query: how many entry rows to produce before
/// stopping early, and how many of the first to flag as offset-skipped.
#[derive(Debug, Copy, Clone, Default)]
pub(crate) struct RowLimit {
    /// `limit + offset` when a limit is given (see
    /// `QueryOutOptions::num_to_take`); `None` disables the limiter.
    pub(crate) num_to_take: Option<usize>,
    pub(crate) num_to_skip: Option<usize>,
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
// Provenance: first-witness recording at the admission seam
// ─────────────────────────────────────────────────────────────────────────

/// The first witness of one admitted tuple: which rule of its set derived
/// it first this epoch, from which premise rows. `None` for rows without a
/// per-row derivation: normal-aggregation folds (their support is a whole
/// group), fixed-rule output, and the meet identity row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Witness {
    pub(crate) store: MagicSymbol,
    pub(crate) tuple: Tuple,
    pub(crate) derivation: Option<(usize, Vec<Tuple>)>,
}

/// The witness table of one query: append-only, one entry per admission,
/// in admission order — which is canonical per store, per epoch, per
/// stratum, and therefore deterministic (asserted by the determinism
/// tests). Passing one to [`stratified_evaluate`] opts the query in;
/// `None` evaluates through the `()` sink at zero cost.
#[derive(Debug, Default)]
pub(crate) struct WitnessTable {
    entries: Vec<Witness>,
}

impl WitnessTable {
    pub(crate) fn entries(&self) -> &[Witness] {
        &self.entries
    }
}

/// The pending witnesses of one rule set's epoch: candidate tuple (full
/// tuple for a regular store, group key for a meet store) → first
/// derivation. Built during (possibly parallel) rule evaluation, each map
/// owned by its own rule set; consumed at the sequential merge barrier.
type PendingWitnesses = BTreeMap<Tuple, (usize, Vec<Tuple>)>;

/// The [`AdmissionSink`] that binds pending witnesses to admitted tuples
/// at the merge barrier. `key_positions` is `Some(meet_key_positions)` for
/// a meet store — its pending map is keyed by the group (the projection of
/// the head tuple onto its non-aggregated positions, wherever they sit),
/// matching the per-group witness boundary documented on the seam — and
/// `None` for a regular one (whose witnesses key on the full tuple).
struct WitnessBinder<'a> {
    store: &'a MagicSymbol,
    pending: &'a PendingWitnesses,
    key_positions: Option<&'a [usize]>,
    table: &'a mut WitnessTable,
}

impl AdmissionSink for WitnessBinder<'_> {
    const RECORDING: bool = true;
    fn admit(&mut self, tuple: TupleInIter<'_>) {
        let full = tuple.into_tuple();
        let derivation = match self.key_positions {
            None => self.pending.get(&full).cloned(),
            // Project the admitted head tuple onto the grouping positions to
            // recover the group key the pending map was recorded under — the
            // same projection eval used at derivation and the store used to
            // fold, so a non-suffix layout binds exactly as a suffix one.
            Some(positions) => self
                .pending
                .get(&project_positions(&full, positions))
                .cloned(),
        };
        self.table.entries.push(Witness {
            store: self.store.clone(),
            tuple: full,
            derivation,
        });
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
pub(crate) struct EvalOutcome {
    pub(crate) store: EpochStore,
    pub(crate) limited: bool,
}

/// Typed lookup for the cross-stage invariant "every referenced rule has a
/// store" (upstream panic sites 4–6).
fn store_of<'m>(
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
///   live there ([`crate::data::program::StoreLifetimes::is_live_at`]).
/// - `limit`: `:limit`/`:offset` early return, applied to the entry rule.
/// - `budget`: required — see [`Budget`].
/// - `witnesses`: `Some` opts in to first-witness provenance recording.
pub(crate) fn stratified_evaluate<R: RuleBody, F: FixedRuleEval>(
    program: &EvalProgram<R, F>,
    lifetimes: &crate::data::program::StoreLifetimes,
    limit: RowLimit,
    budget: &Budget,
    witnesses: Option<&mut WitnessTable>,
) -> Result<EvalOutcome> {
    Ok(stratified_evaluate_with_stores(program, lifetimes, limit, budget, witnesses)?.0)
}

/// [`stratified_evaluate`], additionally returning the final store map
/// (minus the entry store, which rides in the outcome). This is the
/// provenance entry point: [`provenance_graph`] enumerates grounded
/// derivations over these completed stores. Stores dropped by `lifetimes`
/// en route are absent from the map — a caller wanting provenance must
/// keep every rule store live through the final stratum, or the
/// enumeration refuses (typed) on the missing store.
pub(crate) fn stratified_evaluate_with_stores<R: RuleBody, F: FixedRuleEval>(
    program: &EvalProgram<R, F>,
    lifetimes: &crate::data::program::StoreLifetimes,
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
                EvalDefinition::Rules(rule_set) if rule_set.kind == HeadAggrKind::Meet => {
                    EpochStore::new_meet(&rule_set.aggr)?
                }
                EvalDefinition::Rules(rule_set) => EpochStore::new_normal(rule_set.arity()),
                EvalDefinition::Fixed { arity, .. } => EpochStore::new_normal(*arity),
            };
            stores.insert(name.clone(), store);
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
fn evaluate_stratum<R: RuleBody, F: FixedRuleEval>(
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
                EvalDefinition::Rules(rule_set) => match rule_set.kind {
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
                                RegularTempStore::default().wrap(),
                                PendingWitnesses::new(),
                            )
                        }
                    }
                    HeadAggrKind::Meet => {
                        if epoch == 0 {
                            initial_meet_eval(
                                name,
                                rule_set,
                                borrowed_stores,
                                budget,
                                epoch_baseline,
                                recording,
                            )?
                        } else {
                            incremental_meet_eval(
                                name,
                                rule_set,
                                borrowed_stores,
                                budget,
                                epoch_baseline,
                                recording,
                            )?
                        }
                    }
                },
                EvalDefinition::Fixed { rule, .. } => {
                    if epoch == 0 {
                        let mut out = RegularTempStore::default();
                        rule.run(borrowed_stores, &mut out, budget, epoch_baseline)?;
                        (false, out.wrap(), PendingWitnesses::new())
                    } else {
                        // Fixed rules run exactly once.
                        (
                            false,
                            RegularTempStore::default().wrap(),
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
        #[cfg(not(target_arch = "wasm32"))]
        {
            let results: Vec<_> = defs
                .par_iter()
                .filter(|(name, _)| !(limiter_enabled && name.is_prog_entry()))
                .map(&execution)
                .collect();
            for res in results {
                let (name, out, pending) = res?;
                to_merge.insert(name, (out, pending));
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            for res in defs
                .iter()
                .filter(|(name, _)| !(limiter_enabled && name.is_prog_entry()))
                .map(&execution)
            {
                let (name, out, pending) = res?;
                to_merge.insert(name, (out, pending));
            }
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
                    let key_positions = match defs.get(&name) {
                        Some(EvalDefinition::Rules(rule_set))
                            if rule_set.kind == HeadAggrKind::Meet =>
                        {
                            Some(rule_set.meet_key_positions.as_slice())
                        }
                        _ => None,
                    };
                    let mut binder = WitnessBinder {
                        store: &name,
                        pending: &pending,
                        key_positions,
                        table,
                    };
                    epoch_store.merge_in(out, &mut binder)?
                }
                None => epoch_store.merge_in(out, &mut ())?,
            };
            epoch_admitted += admitted.0 as u64;
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
    pending
        .entry(key)
        .or_insert_with(|| (rule_n, premises.to_rows()));
}

// ─────────────────────────────────────────────────────────────────────────
// Provenance: the derivation graph over a completed fixpoint
// ─────────────────────────────────────────────────────────────────────────

/// Provenance was requested where it cannot be honestly computed. Typed:
/// the engine refuses rather than returning a graph with silent gaps.
#[derive(Debug, Error, Diagnostic)]
#[error("provenance unavailable for '{store}': {reason}")]
#[diagnostic(
    code(provenance::unsupported),
    help(
        "provenance needs every rule body to attribute its premises and \
         every premised store to stay live through the final stratum"
    )
)]
pub(crate) struct ProvenanceUnsupported {
    pub(crate) store: String,
    pub(crate) reason: &'static str,
}

/// A provenance graph node: which source a tuple belongs to. Two
/// relations may hold byte-identical tuples; the source keeps them
/// distinct.
pub(crate) type ProvNode = (PremiseSource, Tuple);

/// Enumerate every grounded derivation the completed stores admit and
/// build the semiring derivation graph, for
/// [`crate::query::semiring::solve`].
///
/// - Only plain ([`HeadAggrKind::None`]) rule sets contribute
///   derivations. Meet- and normal-aggregated heads and fixed rules are
///   the **collapse boundary**: aggregation folds and opaque algorithms
///   are not semiring operations, so their stores' tuples enter the graph
///   as ground facts (annotation `1`) and full provenance is claimed only
///   for the positive plain-rule fragment above them. Negated literals
///   contribute no premise (they are absent from [`Premises::Rows`]).
/// - `stores` is the map [`stratified_evaluate_with_stores`] returned; a
///   store a body premises that is absent (dropped by lifetimes, or never
///   retained) is a typed [`ProvenanceUnsupported`] refusal.
/// - Every `Rule`-sourced premise row is verified to be in its store's
///   total — a mismatch is an invariant error, never a silently wrong
///   graph. `Fact`-sourced rows are attested by the body that read them;
///   the independent certificate checker re-verifies them from the model.
/// - `derivation_ceiling` arms the enumeration (grounded derivations can
///   be quadratic in store rows); crossing it is the typed
///   [`ProvenanceLimitExceeded`] refusal. `budget` threads the same
///   kill/deadline interrupts as evaluation.
/// - `weights` prices one rule application, keyed by store and per-head
///   rule index; the tropical semiring charges it, the boolean one
///   ignores it. Unit weights make cost = number of rule firings.
/// - The enumeration is limiter-blind: it re-derives from the completed
///   stores and does not replay a `:limit` early stop.
///
/// [`ProvenanceLimitExceeded`]: crate::query::semiring::ProvenanceLimitExceeded
pub(crate) fn provenance_graph<R: RuleBody, F: FixedRuleEval>(
    program: &EvalProgram<R, F>,
    stores: &BTreeMap<MagicSymbol, EpochStore>,
    budget: &Budget,
    derivation_ceiling: std::num::NonZeroU64,
    weights: &dyn Fn(&MagicSymbol, usize) -> std::num::NonZeroU64,
) -> Result<DerivationGraph<ProvNode>> {
    let mut graph: DerivationGraph<ProvNode> = DerivationGraph::default();
    let ceiling = derivation_ceiling.get();
    let mut spent: u64 = 0;

    for stratum in &program.strata {
        for (name, def) in &stratum.defs {
            let rule_set = match def {
                EvalDefinition::Rules(rule_set) if rule_set.kind == HeadAggrKind::None => rule_set,
                // The collapse boundary: aggregated and fixed-rule stores
                // ground out. (An absent store here is fine — nothing can
                // premise it without tripping the liveness refusal below.)
                _ => {
                    if let Some(store) = stores.get(name) {
                        for t in store.all_iter() {
                            graph
                                .facts
                                .insert((PremiseSource::Rule(name.clone()), t.into_tuple()));
                        }
                    }
                    continue;
                }
            };
            // Interrupt poll only (in_flight 0): enumeration spend is governed
            // by the provenance derivation ceiling below, not the derived-tuple
            // meter, so this ticker must not contribute mid-epoch spend.
            let mut ticker = budget.ticker(0, name);
            for (rule_n, body) in rule_set.bodies.iter().enumerate() {
                let sources = body
                    .premise_sources()
                    .ok_or_else(|| ProvenanceUnsupported {
                        store: name.as_plain_symbol().name.to_string(),
                        reason: "a rule body does not attribute its premises",
                    })?;
                for dep in body.contained_rules().values() {
                    if !stores.contains_key(dep) {
                        return Err(ProvenanceUnsupported {
                            store: dep.as_plain_symbol().name.to_string(),
                            reason: "a premised store was not retained to the final stratum",
                        }
                        .into());
                    }
                }
                let weight = weights(name, rule_n);
                body.for_each_derivation(stores, None, true, &mut |head, premises| {
                    ticker.tick(0)?;
                    spent += 1;
                    if spent > ceiling {
                        return Err(crate::query::semiring::ProvenanceLimitExceeded {
                            dimension: "enumerated derivations",
                            spent,
                            ceiling,
                        }
                        .into());
                    }
                    let rows = premises.to_rows();
                    if rows.len() != sources.len() {
                        return Err(EvalInvariantError(
                            "premise rows disagree with the body's attribution",
                        )
                        .into());
                    }
                    let mut premise_nodes = Vec::with_capacity(rows.len());
                    for (src, row) in sources.iter().zip(rows) {
                        if let PremiseSource::Rule(sym) = src {
                            let dep = store_of(stores, sym)?;
                            let present = dep
                                .prefix_iter(&row)
                                .next()
                                .is_some_and(|t| t.into_tuple() == row);
                            if !present {
                                return Err(EvalInvariantError(
                                    "a premise row is missing from its attributed store",
                                )
                                .into());
                            }
                        } else {
                            graph.facts.insert((src.clone(), row.clone()));
                        }
                        premise_nodes.push((src.clone(), row));
                    }
                    graph.derivations.push(Derivation {
                        head: (PremiseSource::Rule(name.clone()), head.into_owned()),
                        label: rule_n,
                        weight,
                        premises: premise_nodes,
                    });
                    Ok(ControlFlow::Continue(()))
                })?;
            }
        }
    }
    graph.check_closed()?;
    Ok(graph)
}

/// A tuple's projection onto the given head positions, in the order the
/// positions are listed. For a meet head this is the grouping key — eval's
/// mirror of the store's [`MeetAggrStore`] layout projection, so the two
/// agree on a group's identity whatever positions the meet columns occupy.
fn project_positions(row: &[DataValue], positions: &[usize]) -> Tuple {
    positions.iter().map(|i| row[*i].clone()).collect()
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
    let mut out = RegularTempStore::default();
    let mut pending = PendingWitnesses::new();
    let should_check_limit = limiter.enabled() && rule_symb.is_prog_entry();
    let mut ticker = budget.ticker(baseline, rule_symb);
    for (rule_n, body) in rule_set.bodies.iter().enumerate() {
        let mut hit_limit = false;
        body.for_each_derivation(stores, None, recording, &mut |item, premises| {
            ticker.tick(out.len())?;
            if should_check_limit {
                // Dedup on the slice; ownership is materialized only for
                // rows that are genuinely new (the slice-consuming seam).
                if !out.exists(&item) {
                    let item = item.into_owned();
                    if recording {
                        note_pending(&mut pending, item.clone(), rule_n, &premises);
                    }
                    if limiter.should_skip_next() {
                        out.put_with_skip(item);
                    } else {
                        out.put(item);
                    }
                    if limiter.incr_and_should_stop() {
                        hit_limit = true;
                        return Ok(ControlFlow::Break(()));
                    }
                }
            } else if !out.exists(&item) {
                // Same dedup-before-mint; `note_pending` is first-writer-wins
                // (`or_insert_with`), so skipping re-derivations changes no
                // witness. A re-inserted key would only rewrite `false` over
                // `false` — nothing observable.
                let item = item.into_owned();
                if recording {
                    note_pending(&mut pending, item.clone(), rule_n, &premises);
                }
                out.put(item);
            }
            Ok(ControlFlow::Continue(()))
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
    let mut out = RegularTempStore::default();
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
                if should_check_limit {
                    // Deviations D1/D2: dedup within the epoch before counting,
                    // and record skip flags only here, on the entry rule.
                    if !out.exists(&item) {
                        let item = item.into_owned();
                        if recording {
                            note_pending(&mut pending, item.clone(), rule_n, &premises);
                        }
                        if limiter.should_skip_next() {
                            out.put_with_skip(item);
                        } else {
                            out.put(item);
                        }
                        if limiter.incr_and_should_stop() {
                            hit_limit.set(true);
                            return Ok(ControlFlow::Break(()));
                        }
                    }
                } else if !out.exists(&item) {
                    // Same dedup-before-mint as the initial epoch: first-writer
                    // `note_pending` and a `false`-over-`false` re-insert make
                    // skipping intra-epoch re-derivations unobservable.
                    let item = item.into_owned();
                    if recording {
                        note_pending(&mut pending, item.clone(), rule_n, &premises);
                    }
                    out.put(item);
                }
                Ok(ControlFlow::Continue(()))
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
) -> Result<(bool, TempStore, PendingWitnesses)> {
    let mut out = MeetAggrStore::new(rule_set.aggr.clone())?;
    let mut pending = PendingWitnesses::new();
    let key_positions = rule_set.meet_key_positions.as_slice();
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
    if out.is_empty() && rule_set.aggr.iter().all(Option::is_some) {
        let identity: Tuple = rule_set
            .aggr
            .iter()
            .flatten()
            .map(|(aggregation, _)| -> Result<DataValue> {
                let op = aggregation.meet_op().ok_or(EvalInvariantError(
                    "a Meet-classified head holds a non-meet aggregation",
                ))?;
                Ok(op.init_val())
            })
            .collect::<Result<_>>()?;
        // No pending entry: the identity row's witness is `None` by design.
        out.meet_put(&identity)?;
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
) -> Result<(bool, TempStore, PendingWitnesses)> {
    let mut out = MeetAggrStore::new(rule_set.aggr.clone())?;
    let mut pending = PendingWitnesses::new();
    let key_positions = rule_set.meet_key_positions.as_slice();
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
                ticker.tick(effective as usize)?;
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
    let mut out = RegularTempStore::default();
    let should_check_limit = limiter.enabled() && rule_symb.is_prog_entry();
    let signature = &rule_set.aggr;

    let key_indices: Vec<usize> = signature
        .iter()
        .enumerate()
        .filter(|(_, a)| a.is_none())
        .map(|(i, _)| i)
        .collect();
    let val_specs: Vec<(usize, &Aggregation, &[DataValue])> = signature
        .iter()
        .enumerate()
        .filter_map(|(i, a)| {
            a.as_ref()
                .map(|(aggregation, args)| (i, aggregation, args.as_slice()))
        })
        .collect();
    let fresh_ops = || -> Result<Vec<Box<dyn NormalAggrObj>>> {
        val_specs
            .iter()
            .map(|(_, aggregation, args)| aggregation.normal_op(args))
            .collect()
    };

    let mut aggr_work: BTreeMap<Tuple, Vec<Box<dyn NormalAggrObj>>> = BTreeMap::new();
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
        let mut row = vec![DataValue::Null; signature.len()];
        for (slot, i) in key_indices.iter().enumerate() {
            row[*i] = key[slot].clone();
        }
        for (op, (i, _, _)) in ops.iter().zip(&val_specs) {
            row[*i] = op.get()?;
        }
        ticker.tick(out.len())?;
        if should_check_limit {
            if !out.exists(&row) {
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;

    use itertools::Itertools;
    use proptest::prelude::*;

    use super::*;
    use crate::data::aggr::parse_aggr;
    use crate::data::program::StoreLifetimes;
    use crate::data::span::SourceSpan;
    use crate::data::symb::Symbol;
    use crate::query::laws::{FixedRule, Literal, Program, Rel, Rule, Term, naive_eval};

    // ── plumbing ─────────────────────────────────────────────────────────

    fn v(i: i64) -> DataValue {
        DataValue::from(i)
    }
    fn muggle(rel: &str) -> MagicSymbol {
        MagicSymbol::Muggle {
            inner: Symbol::new(rel, SourceSpan(0, 0)),
        }
    }
    fn entry_symbol() -> MagicSymbol {
        MagicSymbol::Muggle {
            inner: Symbol::prog_entry(SourceSpan(0, 0)),
        }
    }
    fn generous_budget() -> Budget {
        Budget::new(NonZeroU32::new(10_000).unwrap())
    }
    fn no_limit() -> RowLimit {
        RowLimit::default()
    }
    fn lit(rel: Rel, args: Vec<Term>, negated: bool) -> Literal {
        if negated {
            Literal::neg(rel, args)
        } else {
            Literal::pos(rel, args)
        }
    }
    fn x() -> Term {
        Term::Var("X")
    }
    fn y() -> Term {
        Term::Var("Y")
    }
    fn z() -> Term {
        Term::Var("Z")
    }
    fn named(name: &str) -> HeadAggr {
        Some((
            parse_aggr(name).unwrap_or_else(|| panic!("real aggregation exists: {name}")),
            vec![],
        ))
    }

    // ── the differential harness: the oracle's rule model as a RuleBody ──
    //
    // This is the first of the two RuleBody implementations the seam was
    // designed for: it evaluates one `laws::Rule` body against the live
    // EpochStore map by naive nested-loop unification, positives (in
    // order) before negatives, mirroring `laws::body_bindings` — except
    // that IDB literals read the stores (total, or the delta of
    // `delta_from`), which is exactly the context the compile tier's RA
    // plans will receive.

    type Bindings = HashMap<&'static str, DataValue>;

    fn unify(args: &[Term], tuple: &[DataValue], bound: &Bindings) -> Option<Bindings> {
        if args.len() != tuple.len() {
            return None;
        }
        let mut out = bound.clone();
        for (t, val) in args.iter().zip(tuple) {
            match t {
                Term::Const(c) => {
                    if c != val {
                        return None;
                    }
                }
                Term::Var(name) => match out.get(name) {
                    Some(existing) if existing != val => return None,
                    Some(_) => {}
                    None => {
                        out.insert(name, val.clone());
                    }
                },
            }
        }
        Some(out)
    }

    fn ground(args: &[Term], bound: &Bindings) -> Tuple {
        args.iter()
            .map(|t| match t {
                Term::Const(c) => c.clone(),
                Term::Var(var) => bound[var].clone(),
            })
            .collect()
    }

    #[derive(Debug)]
    struct ModelBody {
        head: Vec<Term>,
        body: Vec<Literal>,
        /// The EDB: base facts, read-only, shared across the program.
        facts: Arc<BTreeMap<Rel, BTreeSet<Tuple>>>,
        /// Which relations are rule/fixed heads (stores), not EDB.
        idb: Arc<BTreeSet<Rel>>,
        /// Occurrence key = this literal's position in `body` (stable
        /// across the positive/negated reordering `stream_join` uses for
        /// evaluation order) — one entry per idb literal, positive OR
        /// negated: this map is also the lifetime-tracking dependency
        /// source (`note_use`, below), and a store read only inside a
        /// negation is used just as much as one read positively. A
        /// relation mentioned twice gets two independent occurrences, each
        /// independently delta-selectable (matches the real engine's
        /// `compile.rs::contained_rules`, which numbers the same way over
        /// `MagicInlineRule::body`) — though `stream_join` never selects a
        /// negated occurrence's delta (negation always reads totals).
        contained: BTreeMap<AtomOccurrence, MagicSymbol>,
    }

    impl ModelBody {
        fn new(
            head: Vec<Term>,
            body: Vec<Literal>,
            facts: Arc<BTreeMap<Rel, BTreeSet<Tuple>>>,
            idb: Arc<BTreeSet<Rel>>,
        ) -> Self {
            let mut contained: BTreeMap<AtomOccurrence, MagicSymbol> = BTreeMap::new();
            for (i, l) in body.iter().enumerate() {
                if idb.contains(l.rel) {
                    contained.insert(AtomOccurrence(i), muggle(l.rel));
                }
            }
            Self {
                head,
                body,
                facts,
                idb,
                contained,
            }
        }

        fn rows_of(
            &self,
            stores: &BTreeMap<MagicSymbol, EpochStore>,
            rel: Rel,
            use_delta: bool,
        ) -> Result<Vec<Tuple>> {
            if self.idb.contains(rel) {
                let store = store_of(stores, &muggle(rel))?;
                Ok(if use_delta {
                    store
                        .delta_all_iter()
                        .map(TupleInIter::into_tuple)
                        .collect()
                } else {
                    store.all_iter().map(TupleInIter::into_tuple).collect()
                })
            } else {
                Ok(self
                    .facts
                    .get(rel)
                    .map(|set| set.iter().cloned().collect())
                    .unwrap_or_default())
            }
        }

        fn negated_probe_hits(
            &self,
            stores: &BTreeMap<MagicSymbol, EpochStore>,
            rel: Rel,
            probe: &Tuple,
        ) -> Result<bool> {
            if self.idb.contains(rel) {
                // Full-tuple probe via prefix_iter: exact at full-tuple
                // granularity for meet stores too (EpochStore::exists
                // truncates meet probes to the group key, which is the
                // wrong question for negation).
                let store = store_of(stores, &muggle(rel))?;
                Ok(store
                    .prefix_iter(probe)
                    .next()
                    .is_some_and(|t| t == probe[..]))
            } else {
                Ok(self.facts.get(rel).is_some_and(|set| set.contains(probe)))
            }
        }
    }

    impl ModelBody {
        /// Depth-first STREAMING join: recurse literal by literal, calling
        /// `f` at each fully-bound leaf. This never materializes the join
        /// frontier — the old frontier `Vec` grew to the whole cross product
        /// *below* the budget's tick seam (`f`), so a near-cross-product OOMed
        /// the harness before the guard could fire (reviewer finding F3).
        /// Streaming pushes every derivation through `f` as it is found, so
        /// the mid-epoch guard bounds the harness exactly as it bounds the
        /// compiled production path. The nested-loop recursion visits leaves
        /// in the SAME order as the old frontier expansion (outer literal
        /// slowest), so derivation order, premises, and admission order are
        /// byte-identical to before — only the memory profile changes.
        #[allow(clippy::too_many_arguments)]
        fn stream_join(
            &self,
            stores: &BTreeMap<MagicSymbol, EpochStore>,
            delta_from: Option<AtomOccurrence>,
            want_premises: bool,
            ordered: &[(usize, &Literal)],
            idx: usize,
            bound: &Bindings,
            premises: &mut Vec<Tuple>,
            f: &mut dyn FnMut(Cow<'_, [DataValue]>, Premises<'_>) -> Result<ControlFlow<()>>,
        ) -> Result<ControlFlow<()>> {
            if idx == ordered.len() {
                let head = ground(&self.head, bound);
                let arg = if want_premises {
                    Premises::Rows(premises)
                } else {
                    Premises::NotRequested
                };
                return f(Cow::Owned(head), arg);
            }
            let (body_pos, l) = ordered[idx];
            if l.negated {
                let probe = ground(&l.args, bound);
                if self.negated_probe_hits(stores, l.rel, &probe)? {
                    return Ok(ControlFlow::Continue(()));
                }
                return self.stream_join(
                    stores,
                    delta_from,
                    want_premises,
                    ordered,
                    idx + 1,
                    bound,
                    premises,
                    f,
                );
            }
            // This literal's OWN occurrence — its position in the original
            // body, stable across the positive/negated reordering above —
            // must match `delta_from` exactly; a different occurrence of
            // the SAME relation reads the total, per the seam contract.
            let is_delta = delta_from == Some(AtomOccurrence(body_pos));
            let rows = self.rows_of(stores, l.rel, is_delta)?;
            for row in &rows {
                if let Some(b) = unify(&l.args, row, bound) {
                    if want_premises {
                        premises.push(row.clone());
                    }
                    let cf = self.stream_join(
                        stores,
                        delta_from,
                        want_premises,
                        ordered,
                        idx + 1,
                        &b,
                        premises,
                        f,
                    )?;
                    if want_premises {
                        premises.pop();
                    }
                    if cf.is_break() {
                        return Ok(ControlFlow::Break(()));
                    }
                }
            }
            Ok(ControlFlow::Continue(()))
        }
    }

    impl RuleBody for ModelBody {
        fn for_each_derivation(
            &self,
            stores: &BTreeMap<MagicSymbol, EpochStore>,
            delta_from: Option<AtomOccurrence>,
            want_premises: bool,
            f: &mut dyn FnMut(Cow<'_, [DataValue]>, Premises<'_>) -> Result<ControlFlow<()>>,
        ) -> Result<()> {
            let mut ordered: Vec<(usize, &Literal)> = self
                .body
                .iter()
                .enumerate()
                .filter(|(_, l)| !l.negated)
                .collect();
            ordered.extend(self.body.iter().enumerate().filter(|(_, l)| l.negated));
            let mut premises: Vec<Tuple> = Vec::new();
            // The driver ignores the break/continue verdict: a break here
            // just means the visitor stopped early, which is fine at the top.
            let _ = self.stream_join(
                stores,
                delta_from,
                want_premises,
                &ordered,
                0,
                &Bindings::new(),
                &mut premises,
                f,
            )?;
            Ok(())
        }

        fn contained_rules(&self) -> &BTreeMap<AtomOccurrence, MagicSymbol> {
            &self.contained
        }
    }

    struct ModelFixed {
        inputs: Vec<Rel>,
        eval: fn(&[BTreeSet<Tuple>]) -> BTreeSet<Tuple>,
        facts: Arc<BTreeMap<Rel, BTreeSet<Tuple>>>,
        idb: Arc<BTreeSet<Rel>>,
    }

    impl FixedRuleEval for ModelFixed {
        fn run(
            &self,
            stores: &BTreeMap<MagicSymbol, EpochStore>,
            out: &mut RegularTempStore,
            _budget: &Budget,
            _baseline: u64,
        ) -> Result<()> {
            let inputs: Vec<BTreeSet<Tuple>> = self
                .inputs
                .iter()
                .map(|rel| -> Result<BTreeSet<Tuple>> {
                    if self.idb.contains(rel) {
                        Ok(store_of(stores, &muggle(rel))?
                            .all_iter()
                            .map(TupleInIter::into_tuple)
                            .collect())
                    } else {
                        Ok(self.facts.get(rel).cloned().unwrap_or_default())
                    }
                })
                .collect::<Result<_>>()?;
            for row in (self.eval)(&inputs) {
                out.put(row);
            }
            Ok(())
        }
    }

    // ── the harness compiler: oracle model → EvalProgram ────────────────
    //
    // Stratum assignment duplicates the oracle's Bellman-Ford (the oracle
    // is sealed; its `strata` is private, and this scaffolding must not
    // depend on the judge's internals anyway). An extra final stratum
    // holds the entry `?[vars…] := target[vars…]`.

    struct HeadClass {
        has_aggr: bool,
        is_meet: bool,
    }

    fn head_classes(program: &Program) -> HashMap<Rel, HeadClass> {
        let mut per_head: HashMap<Rel, Vec<&Rule>> = HashMap::new();
        for rule in &program.rules {
            per_head.entry(rule.head_rel).or_default().push(rule);
        }
        per_head
            .into_iter()
            .map(|(rel, rules)| {
                let has_aggr = rules.iter().any(|r| r.aggr.iter().any(|a| a.is_some()));
                let is_meet = has_aggr
                    && rules.iter().all(|r| {
                        r.aggr.iter().all(|a| match a {
                            None => true,
                            Some((aggregation, _)) => aggregation.is_meet(),
                        })
                    });
                (rel, HeadClass { has_aggr, is_meet })
            })
            .collect()
    }

    /// head → dependency, `forcing` when the dependency must be complete
    /// strictly below (transcribed from the oracle's edge rules).
    fn dependency_edges(program: &Program) -> Vec<(Rel, Rel, bool)> {
        let classes = head_classes(program);
        let fixed_heads: BTreeSet<Rel> = program.fixed.iter().map(|f| f.head_rel).collect();
        let is_meet = |rel: Rel| classes.get(rel).is_some_and(|c| c.is_meet);
        let mut edges = Vec::new();
        for rule in &program.rules {
            let head = rule.head_rel;
            let class = &classes[&head];
            for l in &rule.body {
                let forcing = if class.has_aggr {
                    if class.is_meet && l.rel == head {
                        l.negated
                    } else {
                        true
                    }
                } else {
                    l.negated || fixed_heads.contains(l.rel) || is_meet(l.rel)
                };
                edges.push((head, l.rel, forcing));
            }
        }
        for f in &program.fixed {
            for dep in &f.inputs {
                edges.push((f.head_rel, *dep, true));
            }
        }
        edges
    }

    fn strata_of(program: &Program) -> HashMap<Rel, usize> {
        let edges = dependency_edges(program);
        let mut s: HashMap<Rel, usize> = HashMap::new();
        for rule in &program.rules {
            s.insert(rule.head_rel, 0);
            for l in &rule.body {
                s.insert(l.rel, 0);
            }
        }
        for f in &program.fixed {
            s.insert(f.head_rel, 0);
            for i in &f.inputs {
                s.insert(i, 0);
            }
        }
        for rel in program.facts.keys() {
            s.insert(rel, 0);
        }
        let bound = s.len() + 1;
        for _ in 0..bound {
            let mut changed = false;
            for (head, dep, forcing) in &edges {
                let need = s[dep] + usize::from(*forcing);
                if s[head] < need {
                    s.insert(head, need);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        s
    }

    const ENTRY_VARS: [&str; 8] = ["v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7"];

    struct Compiled {
        program: EvalProgram<ModelBody, ModelFixed>,
        lifetimes: StoreLifetimes,
    }

    /// Compile the oracle model for evaluation with `target` as the entry
    /// (`?[vars…] := target[vars…]`). `fixed_arities` declares the output
    /// arity of each fixed head (opaque to the model).
    fn compile_for(
        model: &Program,
        target: Rel,
        target_arity: usize,
        fixed_arities: &BTreeMap<Rel, usize>,
    ) -> Compiled {
        let idb: Arc<BTreeSet<Rel>> = Arc::new(
            model
                .rules
                .iter()
                .map(|r| r.head_rel)
                .chain(model.fixed.iter().map(|f| f.head_rel))
                .collect(),
        );
        for rel in idb.iter() {
            assert!(
                !model.facts.contains_key(rel),
                "harness limitation: facts under rule head {rel} (the real \
                 engine separates stored relations from rules)"
            );
        }
        let facts = Arc::new(model.facts.clone());
        let strata_map = strata_of(model);
        // The entry sits strictly above the whole program, in its own final
        // stratum (EvalProgram's construction proof demands it).
        let entry_stratum = strata_map.values().copied().max().unwrap_or(0) + 1;

        let mut strata: Vec<EvalStratum<ModelBody, ModelFixed>> = (0..=entry_stratum)
            .map(|_| EvalStratum::default())
            .collect();
        let mut lifetimes = StoreLifetimes::default();

        // Group inline rules per head, preserving rule order within a head.
        let mut heads_in_order: Vec<Rel> = Vec::new();
        let mut per_head: BTreeMap<Rel, Vec<&Rule>> = BTreeMap::new();
        for rule in &model.rules {
            if !per_head.contains_key(rule.head_rel) {
                heads_in_order.push(rule.head_rel);
            }
            per_head.entry(rule.head_rel).or_default().push(rule);
        }
        for head in heads_in_order {
            let rules = &per_head[head];
            let stratum = strata_map[head];
            let bodies: Vec<ModelBody> = rules
                .iter()
                .map(|r| {
                    ModelBody::new(
                        r.head_args.clone(),
                        r.body.clone(),
                        facts.clone(),
                        idb.clone(),
                    )
                })
                .collect();
            for body in &bodies {
                for dep in body.contained_rules().values() {
                    lifetimes.note_use(dep.clone(), stratum);
                }
            }
            let rule_set =
                EvalRuleSet::new(rules[0].aggr.clone(), bodies).expect("well-shaped rule set");
            strata[stratum]
                .defs
                .insert(muggle(head), EvalDefinition::Rules(rule_set));
        }
        for f in &model.fixed {
            let stratum = strata_map[f.head_rel];
            for input in &f.inputs {
                if idb.contains(input) {
                    lifetimes.note_use(muggle(input), stratum);
                }
            }
            strata[stratum].defs.insert(
                muggle(f.head_rel),
                EvalDefinition::Fixed {
                    // No silent arity default: a wrong arity makes the entry
                    // copy unify nothing, masking disagreements (see
                    // `model_arities`).
                    arity: fixed_arities.get(f.head_rel).copied().unwrap_or_else(|| {
                        panic!("fixed head {} missing from fixed_arities", f.head_rel)
                    }),
                    rule: ModelFixed {
                        inputs: f.inputs.clone(),
                        eval: f.eval,
                        facts: facts.clone(),
                        idb: idb.clone(),
                    },
                },
            );
        }
        // The entry: ?[v0..vn] := target[v0..vn].
        let vars: Vec<Term> = ENTRY_VARS[..target_arity]
            .iter()
            .copied()
            .map(Term::Var)
            .collect();
        let entry_body = ModelBody::new(
            vars.clone(),
            vec![lit(target, vars, false)],
            facts.clone(),
            idb.clone(),
        );
        lifetimes.note_use(muggle(target), entry_stratum);
        let entry_set = EvalRuleSet::new(
            std::iter::repeat_n(None, target_arity).collect(),
            vec![entry_body],
        )
        .expect("entry rule set");
        strata[entry_stratum]
            .defs
            .insert(entry_symbol(), EvalDefinition::Rules(entry_set));

        let program = EvalProgram::from_execution_order(strata).expect("entry in final stratum");
        Compiled { program, lifetimes }
    }

    fn real_eval(
        model: &Program,
        target: Rel,
        target_arity: usize,
        fixed_arities: &BTreeMap<Rel, usize>,
        budget: &Budget,
    ) -> Result<BTreeSet<Tuple>> {
        let compiled = compile_for(model, target, target_arity, fixed_arities);
        let outcome = stratified_evaluate(
            &compiled.program,
            &compiled.lifetimes,
            no_limit(),
            budget,
            None,
        )?;
        Ok(outcome
            .store
            .all_iter()
            .map(TupleInIter::into_tuple)
            .collect())
    }

    /// Relation arities derived from the MODEL alone (rule heads and
    /// literal usage), never from oracle output. This is what keeps the
    /// judge sound: an oracle-empty relation used to default to arity 1,
    /// making the entry copy silently unify nothing — any over-derivation
    /// into such a relation was invisible (a vacuous pass).
    fn model_arities(model: &Program) -> BTreeMap<Rel, usize> {
        fn note(arities: &mut BTreeMap<Rel, usize>, rel: Rel, n: usize) {
            match arities.entry(rel) {
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert(n);
                }
                std::collections::btree_map::Entry::Occupied(o) => {
                    assert_eq!(*o.get(), n, "model uses '{rel}' at two arities");
                }
            }
        }
        let mut arities = BTreeMap::new();
        for r in &model.rules {
            note(&mut arities, r.head_rel, r.head_args.len());
            for l in &r.body {
                note(&mut arities, l.rel, l.args.len());
            }
        }
        for (rel, rows) in &model.facts {
            if let Some(t) = rows.first() {
                note(&mut arities, rel, t.len());
            }
        }
        arities
    }

    /// THE differential: every IDB relation of the model, evaluated by the
    /// real semi-naive engine, must equal the sealed oracle's answer.
    fn assert_matches_oracle(model: &Program) {
        let oracle_db = naive_eval(model).expect("oracle accepts the program");
        let idb: BTreeSet<Rel> = model
            .rules
            .iter()
            .map(|r| r.head_rel)
            .chain(model.fixed.iter().map(|f| f.head_rel))
            .collect();
        let arities = model_arities(model);
        let arity_of = |rel: Rel| -> usize {
            arities.get(rel).copied().unwrap_or_else(|| {
                panic!(
                    "harness limitation: '{rel}' is never used at a known arity \
                     in the model (an unreferenced fixed head?) — reference it \
                     in a rule so the judge knows its shape"
                )
            })
        };
        let fixed_arities: BTreeMap<Rel, usize> = model
            .fixed
            .iter()
            .map(|f| (f.head_rel, arity_of(f.head_rel)))
            .collect();
        for rel in idb {
            let oracle_rows = oracle_db.get(rel).cloned().unwrap_or_default();
            let arity = arity_of(rel);
            if let Some(first) = oracle_rows.iter().next() {
                assert_eq!(first.len(), arity, "oracle/model arity drift on '{rel}'");
            }
            let real_rows = real_eval(model, rel, arity, &fixed_arities, &generous_budget())
                .unwrap_or_else(|e| panic!("real eval failed for {rel}: {e:?}"));
            assert_eq!(
                real_rows, oracle_rows,
                "FINDING: real eval disagrees with the oracle on '{rel}'"
            );
        }
    }

    // ── shared program shapes ────────────────────────────────────────────

    fn edge_facts(edges: &[(i64, i64)]) -> BTreeMap<Rel, BTreeSet<Tuple>> {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = Default::default();
        facts.insert(
            "edge",
            edges.iter().map(|(a, b)| vec![v(*a), v(*b)]).collect(),
        );
        facts
    }

    fn transitive_closure() -> Vec<Rule> {
        vec![
            Rule::plain(
                "path",
                vec![x(), y()],
                vec![lit("edge", vec![x(), y()], false)],
            ),
            Rule::plain(
                "path",
                vec![x(), y()],
                vec![
                    lit("edge", vec![x(), z()], false),
                    lit("path", vec![z(), y()], false),
                ],
            ),
        ]
    }

    /// TC by self-join: `path` appears twice in the recursive body, so its
    /// multiplicity is Many and every changed epoch forces a complete run.
    fn transitive_closure_self_join() -> Vec<Rule> {
        vec![
            Rule::plain(
                "path",
                vec![x(), y()],
                vec![lit("edge", vec![x(), y()], false)],
            ),
            Rule::plain(
                "path",
                vec![x(), z()],
                vec![
                    lit("path", vec![x(), y()], false),
                    lit("path", vec![y(), z()], false),
                ],
            ),
        ]
    }

    fn meet_reach_rules(aggr_name: &str) -> Vec<Rule> {
        vec![
            Rule::aggregated(
                "m",
                vec![x(), y()],
                vec![None, named(aggr_name)],
                vec![lit("seed", vec![x(), y()], false)],
            ),
            Rule::aggregated(
                "m",
                vec![y(), z()],
                vec![None, named(aggr_name)],
                vec![
                    lit("edge", vec![x(), y()], false),
                    lit("m", vec![x(), z()], false),
                ],
            ),
        ]
    }

    /// The mirror image of [`meet_reach_rules`]: the exact same meet
    /// recursion, but with the meet column at position **0** and the
    /// grouping node at position 1 — a non-suffix layout the retired
    /// `MeetNotSuffix` refusal used to reject and the suffix-prefix store
    /// could not represent. `m[val, node]` reads back as "node → folded
    /// value", so the recursive body reads `m[z, x]` (value first, node
    /// second). The oracle groups by position, so `assert_matches_oracle`
    /// judges this against the same fixpoint as the suffix form.
    fn meet_reach_rules_pos0(aggr_name: &str) -> Vec<Rule> {
        vec![
            Rule::aggregated(
                "m",
                vec![y(), x()],
                vec![named(aggr_name), None],
                vec![lit("seed", vec![x(), y()], false)],
            ),
            Rule::aggregated(
                "m",
                vec![z(), y()],
                vec![named(aggr_name), None],
                vec![
                    lit("edge", vec![x(), y()], false),
                    lit("m", vec![z(), x()], false),
                ],
            ),
        ]
    }

    // ── fixed-case differentials ─────────────────────────────────────────

    #[test]
    fn differential_transitive_closure() {
        assert_matches_oracle(&Program {
            rules: transitive_closure(),
            facts: edge_facts(&[(1, 2), (2, 3), (3, 4), (4, 2)]),
            ..Program::default()
        });
    }

    #[test]
    fn differential_self_join_many_multiplicity() {
        assert_matches_oracle(&Program {
            rules: transitive_closure_self_join(),
            facts: edge_facts(&[(1, 2), (2, 3), (3, 4), (5, 6)]),
            ..Program::default()
        });
    }

    #[test]
    fn differential_stratified_negation() {
        let mut facts = edge_facts(&[(1, 2), (2, 3)]);
        facts.insert("node", (1..=3).map(|i| vec![v(i)]).collect());
        let mut rules = transitive_closure();
        rules.push(Rule::plain(
            "unreachable",
            vec![x(), y()],
            vec![
                lit("node", vec![x()], false),
                lit("node", vec![y()], false),
                lit("path", vec![x(), y()], true),
            ],
        ));
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    #[test]
    fn differential_normal_aggregation_over_recursion() {
        let mut rules = transitive_closure();
        rules.push(Rule::aggregated(
            "reach_count",
            vec![x(), y()],
            vec![None, named("count")],
            vec![lit("path", vec![x(), y()], false)],
        ));
        assert_matches_oracle(&Program {
            rules,
            facts: edge_facts(&[(1, 2), (2, 3), (3, 4)]),
            ..Program::default()
        });
    }

    #[test]
    fn differential_normal_aggregation_empty_fold() {
        // Every position aggregated over no rows: the single empty-fold row.
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert("nothing", BTreeSet::new());
        assert_matches_oracle(&Program {
            rules: vec![Rule::aggregated(
                "c",
                vec![x(), x()],
                vec![named("count"), named("sum")],
                vec![lit("nothing", vec![x()], false)],
            )],
            facts,
            ..Program::default()
        });
    }

    #[test]
    fn differential_meet_recursion_min_on_cycle() {
        let mut facts = edge_facts(&[(1, 2), (2, 3), (3, 1), (3, 4)]);
        facts.insert(
            "seed",
            [(1, 5), (4, 1)]
                .iter()
                .map(|(k, l)| vec![v(*k), v(*l)])
                .collect(),
        );
        assert_matches_oracle(&Program {
            rules: meet_reach_rules("min"),
            facts,
            ..Program::default()
        });
    }

    /// The and/or END-TO-END differential: the exact propagation shape on
    /// which the original's inverted changed-flag reached a premature
    /// fixpoint one hop short (laws.rs pins the store-level half; this
    /// runs the real evaluator through the landed stores and must reach
    /// the oracle's full fixpoint).
    #[test]
    fn differential_and_or_propagation_end_to_end() {
        for (name, seed_of) in [("or", [true, false, false]), ("and", [false, true, true])] {
            let mut facts = edge_facts(&[(1, 2), (2, 3)]);
            facts.insert(
                "seed",
                (1..=3)
                    .map(|k| vec![v(k), DataValue::from(seed_of[(k - 1) as usize])])
                    .collect(),
            );
            let model = Program {
                rules: meet_reach_rules(name),
                facts,
                ..Program::default()
            };
            assert_matches_oracle(&model);
            // And explicitly: node 3 must have flipped (the premature
            // fixpoint stranded it at its seed).
            let real = real_eval(&model, "m", 2, &BTreeMap::new(), &generous_budget()).unwrap();
            let fixpoint = name == "or";
            assert!(
                real.contains(&vec![v(3), DataValue::from(fixpoint)]),
                "{name}: node 3 must reach the fixpoint value"
            );
        }
    }

    // ── non-suffix meet layouts: the capability the refusal used to deny ──

    /// The `min` recursion on a cycle, but with the meet column at position
    /// 0 (grouping node at position 1). Same fixpoint as
    /// `differential_meet_recursion_min_on_cycle`, judged positionally.
    #[test]
    fn differential_meet_pos0_recursion_min_on_cycle() {
        let mut facts = edge_facts(&[(1, 2), (2, 3), (3, 1), (3, 4)]);
        facts.insert(
            "seed",
            [(1, 5), (4, 1)]
                .iter()
                .map(|(k, l)| vec![v(*k), v(*l)])
                .collect(),
        );
        assert_matches_oracle(&Program {
            rules: meet_reach_rules_pos0("min"),
            facts,
            ..Program::default()
        });
    }

    /// The and/or premature-fixpoint case (the inverted changed-flag class)
    /// at a **non-suffix** position: the meet column is position 0, so a
    /// changed-flag bug or a mis-projected group key would strand node 3 at
    /// its seed exactly as the suffix form did. Mirrors
    /// `differential_and_or_propagation_end_to_end`.
    #[test]
    fn differential_and_or_pos0_propagation_end_to_end() {
        for (name, seed_of) in [("or", [true, false, false]), ("and", [false, true, true])] {
            let mut facts = edge_facts(&[(1, 2), (2, 3)]);
            facts.insert(
                "seed",
                (1..=3)
                    .map(|k| vec![v(k), DataValue::from(seed_of[(k - 1) as usize])])
                    .collect(),
            );
            let model = Program {
                rules: meet_reach_rules_pos0(name),
                facts,
                ..Program::default()
            };
            assert_matches_oracle(&model);
            // Node 3 (now at head position 1) must have flipped to the
            // fixpoint value carried in head position 0.
            let real = real_eval(&model, "m", 2, &BTreeMap::new(), &generous_budget()).unwrap();
            let fixpoint = name == "or";
            assert!(
                real.contains(&vec![DataValue::from(fixpoint), v(3)]),
                "{name}: node 3 must reach the fixpoint value at a non-suffix position"
            );
        }
    }

    /// Two meet columns split apart by a grouping column (val positions
    /// [0, 2], key position [1]): for each group `K`, position 0 folds the
    /// minimum and position 2 the maximum of the observed values. Exercises
    /// the store's interleave rebuilding a 3-tuple from split projections.
    #[test]
    fn differential_meet_interleaved_split_columns() {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "obs",
            [(1, 5), (1, 2), (1, 8), (2, 4), (2, 7), (3, 3)]
                .iter()
                .map(|(k, val)| vec![v(*k), v(*val)])
                .collect(),
        );
        // g[min(V), K, max(V)] :- obs[K, V].
        let rules = vec![Rule::aggregated(
            "g",
            vec![Term::Var("V"), Term::Var("K"), Term::Var("V")],
            vec![named("min"), None, named("max")],
            vec![lit("obs", vec![Term::Var("K"), Term::Var("V")], false)],
        )];
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    /// A recursive meet with the grouping column between two meet columns
    /// (key position [1], val positions [0, 2]) — meet-in-recursion at a
    /// genuinely interleaved layout, not merely a swapped pair.
    #[test]
    fn differential_meet_interleaved_recursion() {
        let mut facts = edge_facts(&[(1, 2), (2, 3), (3, 1)]);
        facts.insert(
            "seed",
            [(1, 5, 5), (2, 1, 1)]
                .iter()
                .map(|(k, lo, hi)| vec![v(*k), v(*lo), v(*hi)])
                .collect(),
        );
        // m[min(Lo), K, max(Hi)] seeded, then relaxed along edges: each hop
        // carries the source group's folded (min, max) to the target node.
        let rules = vec![
            Rule::aggregated(
                "m",
                vec![Term::Var("Lo"), Term::Var("K"), Term::Var("Hi")],
                vec![named("min"), None, named("max")],
                vec![lit(
                    "seed",
                    vec![Term::Var("K"), Term::Var("Lo"), Term::Var("Hi")],
                    false,
                )],
            ),
            Rule::aggregated(
                "m",
                vec![Term::Var("Lo"), Term::Var("T"), Term::Var("Hi")],
                vec![named("min"), None, named("max")],
                vec![
                    lit("edge", vec![Term::Var("S"), Term::Var("T")], false),
                    lit(
                        "m",
                        vec![Term::Var("Lo"), Term::Var("S"), Term::Var("Hi")],
                        false,
                    ),
                ],
            ),
        ];
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    #[test]
    fn differential_meet_identity_row_feeds_recursion() {
        // No seeds: the identity `false` matches edge(false, true) and the
        // recursion derives true (laws::meet_identity_row_feeds_recursion).
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "edge",
            [vec![DataValue::from(false), DataValue::from(true)]]
                .into_iter()
                .collect(),
        );
        facts.insert("seed", BTreeSet::new());
        let rules = vec![
            Rule::aggregated(
                "m",
                vec![x()],
                vec![named("or")],
                vec![lit("seed", vec![x()], false)],
            ),
            Rule::aggregated(
                "m",
                vec![y()],
                vec![named("or")],
                vec![
                    lit("edge", vec![x(), y()], false),
                    lit("m", vec![x()], false),
                ],
            ),
        ];
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    #[test]
    fn differential_negation_reads_completed_meet_relation() {
        let mut facts = edge_facts(&[(1, 2)]);
        facts.insert(
            "seed",
            [vec![v(1), DataValue::from(true)]].into_iter().collect(),
        );
        facts.insert("node", (1..=3).map(|i| vec![v(i)]).collect());
        let mut rules = meet_reach_rules("or");
        rules.push(Rule::plain(
            "unseeded",
            vec![x()],
            vec![
                lit("node", vec![x()], false),
                lit("m", vec![x(), Term::Const(DataValue::from(true))], true),
            ],
        ));
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    #[test]
    fn differential_fixed_rules_on_stratum_boundaries() {
        let constant_edges = FixedRule {
            head_rel: "gen_edge",
            inputs: vec![],
            eval: |_| {
                [(1, 2), (2, 3)]
                    .iter()
                    .map(|(a, b)| vec![v(*a), v(*b)])
                    .collect()
            },
        };
        let path_sources = FixedRule {
            head_rel: "sources",
            inputs: vec!["path"],
            eval: |inputs| inputs[0].iter().map(|t| vec![t[0].clone()]).collect(),
        };
        let rules = vec![
            Rule::plain(
                "path",
                vec![x(), y()],
                vec![lit("gen_edge", vec![x(), y()], false)],
            ),
            Rule::plain(
                "path",
                vec![x(), y()],
                vec![
                    lit("gen_edge", vec![x(), z()], false),
                    lit("path", vec![z(), y()], false),
                ],
            ),
            Rule::plain("out", vec![x()], vec![lit("sources", vec![x()], false)]),
        ];
        assert_matches_oracle(&Program {
            rules,
            fixed: vec![constant_edges, path_sources],
            ..Program::default()
        });
    }

    /// Mutual recursion: p and q derive each other inside one stratum —
    /// a shape neither the fixed suite nor the generator produced before
    /// this review.
    #[test]
    fn differential_mutual_recursion() {
        let mut facts = edge_facts(&[(1, 2), (2, 3), (3, 4)]);
        facts.insert(
            "edge2",
            [(2, 5), (5, 3), (4, 1)]
                .iter()
                .map(|(a, b)| vec![v(*a), v(*b)])
                .collect(),
        );
        let rules = vec![
            Rule::plain(
                "p",
                vec![x(), y()],
                vec![lit("edge", vec![x(), y()], false)],
            ),
            Rule::plain(
                "p",
                vec![x(), z()],
                vec![
                    lit("q", vec![x(), y()], false),
                    lit("edge", vec![y(), z()], false),
                ],
            ),
            Rule::plain(
                "q",
                vec![x(), z()],
                vec![
                    lit("p", vec![x(), y()], false),
                    lit("edge2", vec![y(), z()], false),
                ],
            ),
        ];
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    /// One body joining TWO recursive stores that both carry deltas in the
    /// same epochs: r(x,z) :- path(x,y), path2(y,z), with r recursive too.
    /// Kills any truncation of the per-delta iteration (each contained key
    /// must contribute its delta×total combinations).
    #[test]
    fn differential_two_delta_carrying_deps_in_one_body() {
        let mut facts = edge_facts(&[(1, 2), (2, 3), (3, 4)]);
        facts.insert(
            "edge2",
            [(4, 5), (5, 6), (6, 7)]
                .iter()
                .map(|(a, b)| vec![v(*a), v(*b)])
                .collect(),
        );
        let mut rules = transitive_closure(); // path = TC(edge)
        rules.push(Rule::plain(
            "path2",
            vec![x(), y()],
            vec![lit("edge2", vec![x(), y()], false)],
        ));
        rules.push(Rule::plain(
            "path2",
            vec![x(), y()],
            vec![
                lit("edge2", vec![x(), z()], false),
                lit("path2", vec![z(), y()], false),
            ],
        ));
        rules.push(Rule::plain(
            "r",
            vec![x(), z()],
            vec![
                lit("path", vec![x(), y()], false),
                lit("path2", vec![y(), z()], false),
            ],
        ));
        rules.push(Rule::plain(
            "r",
            vec![x(), z()],
            vec![
                lit("r", vec![x(), y()], false),
                lit("path2", vec![y(), z()], false),
            ],
        ));
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    /// A meet head whose body mentions its own store TWICE positively:
    /// multiplicity Many, so every changed epoch takes
    /// `incremental_meet_eval`'s complete-run branch — dead code under the
    /// previous suite (the review's surviving mutant M6). Values propagate
    /// around the cycle only through complete re-runs; gutting the branch
    /// freezes every node at its seed.
    #[test]
    fn differential_meet_self_join_many_multiplicity() {
        let mut facts = edge_facts(&[(1, 2), (2, 3), (3, 1)]);
        facts.insert(
            "seed",
            [(1, 5), (2, 7), (3, 9)]
                .iter()
                .map(|(k, l)| vec![v(*k), v(*l)])
                .collect(),
        );
        let rules = vec![
            Rule::aggregated(
                "m",
                vec![x(), y()],
                vec![None, named("min")],
                vec![lit("seed", vec![x(), y()], false)],
            ),
            // m(x, min w) :- m(x, _), m(w', w), edge(w', x): node x adopts
            // any predecessor's value; m appears twice → Many.
            Rule::aggregated(
                "m",
                vec![x(), z()],
                vec![None, named("min")],
                vec![
                    lit("m", vec![x(), y()], false),
                    lit("m", vec![Term::Var("W"), z()], false),
                    lit("edge", vec![Term::Var("W"), x()], false),
                ],
            ),
        ];
        let model = Program {
            rules,
            facts,
            ..Program::default()
        };
        assert_matches_oracle(&model);
        // And explicitly: the cycle must drain every node to the global
        // minimum (a frozen incremental path strands nodes at their seed).
        let real = real_eval(&model, "m", 2, &BTreeMap::new(), &generous_budget()).unwrap();
        for node in 1..=3 {
            assert!(
                real.contains(&vec![v(node), v(5)]),
                "node {node} must reach the cycle minimum 5, got {real:?}"
            );
        }
    }

    /// Two recursions that converge at different epochs inside ONE
    /// stratum: `a_long` (8-hop chain, ~8 epochs) and `z_short` (2-hop
    /// chain, done by epoch 2), named so the early converger merges LAST
    /// at the barrier. Pins fixpoint detection as the accumulation over
    /// every store's delta — `changed = has_delta()` of the last store
    /// (instead of `|=`) exits the stratum epochs early and truncates the
    /// long closure. Previously only the randomized differential could
    /// catch that mutation.
    #[test]
    fn differential_two_recursions_converge_independently() {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "long_edge",
            (0..8i64).map(|i| vec![v(i), v(i + 1)]).collect(),
        );
        facts.insert(
            "short_edge",
            [(100, 101), (101, 102)]
                .iter()
                .map(|(a, b)| vec![v(*a), v(*b)])
                .collect(),
        );
        let rules = vec![
            Rule::plain(
                "a_long",
                vec![x(), y()],
                vec![lit("long_edge", vec![x(), y()], false)],
            ),
            Rule::plain(
                "a_long",
                vec![x(), z()],
                vec![
                    lit("long_edge", vec![x(), y()], false),
                    lit("a_long", vec![y(), z()], false),
                ],
            ),
            Rule::plain(
                "z_short",
                vec![x(), y()],
                vec![lit("short_edge", vec![x(), y()], false)],
            ),
            Rule::plain(
                "z_short",
                vec![x(), z()],
                vec![
                    lit("short_edge", vec![x(), y()], false),
                    lit("z_short", vec![y(), z()], false),
                ],
            ),
        ];
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    // ── the randomized differential ──────────────────────────────────────

    // Shapes the generator still cannot produce, each pinned by a fixed
    // differential where one exists:
    // - meet self-join / Many-multiplicity meet heads
    //   (differential_meet_self_join_many_multiplicity);
    // - a recursive entry under `:limit`
    //   (limiter_incremental_entry_recursion_dedups_and_overshoots);
    // - meet heads with ≥2 grouping or ≥2 aggregated positions inside
    //   recursion (identity-row shape tested non-recursively only);
    // - aggregations with arguments (`named` always passes empty args);
    // - fixed rules (differential_fixed_rules_on_stratum_boundaries only);
    // - negation over meet stores
    //   (differential_negation_reads_completed_meet_relation only);
    // - witness recording during differentials (witness paths are
    //   exercised by the dedicated provenance and determinism tests only).
    #[derive(Debug, Clone)]
    struct GenCase {
        n: i64,
        edges: BTreeSet<(i64, i64)>,
        seeds: BTreeMap<i64, DataValue>,
        aggr_name: &'static str,
        self_join: bool,
        negation: bool,
        normal_aggr: bool,
        /// Add a mutually recursive pair qa/qb (same stratum as path).
        mutual: bool,
        /// Add pj, whose body joins TWO delta-carrying stores (path, qa);
        /// implies the qa/qb pair.
        two_dep: bool,
    }

    fn arb_case() -> BoxedStrategy<GenCase> {
        let aggr = prop_oneof![
            Just("or"),
            Just("and"),
            Just("min"),
            Just("max"),
            Just("union"),
        ];
        (
            2i64..=5,
            aggr,
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
        )
            .prop_flat_map(
                |(n, aggr_name, self_join, negation, normal_aggr, mutual, two_dep)| {
                    let value: BoxedStrategy<DataValue> = match aggr_name {
                        "or" | "and" => any::<bool>().prop_map(DataValue::from).boxed(),
                        "union" => {
                            prop::collection::btree_set((0i64..4).prop_map(DataValue::from), 0..3)
                                .prop_map(DataValue::Set)
                                .boxed()
                        }
                        _ => (-10i64..10).prop_map(DataValue::from).boxed(),
                    };
                    (
                        prop::collection::btree_set((0..n, 0..n), 0..10),
                        prop::collection::btree_map(0..n, value, 0..=(n as usize)),
                    )
                        .prop_map(move |(edges, seeds)| GenCase {
                            n,
                            edges,
                            seeds,
                            aggr_name,
                            self_join,
                            negation,
                            normal_aggr,
                            mutual,
                            two_dep,
                        })
                },
            )
            .boxed()
    }

    fn build_case(case: &GenCase) -> Program {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "edge",
            case.edges.iter().map(|(a, b)| vec![v(*a), v(*b)]).collect(),
        );
        facts.insert(
            "seed",
            case.seeds
                .iter()
                .map(|(k, val)| vec![v(*k), val.clone()])
                .collect(),
        );
        facts.insert("node", (0..case.n).map(|i| vec![v(i)]).collect());
        let mut rules = if case.self_join {
            transitive_closure_self_join()
        } else {
            transitive_closure()
        };
        rules.extend(meet_reach_rules(case.aggr_name));
        rules.push(Rule::plain(
            "out",
            vec![x(), y()],
            vec![lit("m", vec![x(), y()], false)],
        ));
        if case.negation {
            rules.push(Rule::plain(
                "unreachable",
                vec![x(), y()],
                vec![
                    lit("node", vec![x()], false),
                    lit("node", vec![y()], false),
                    lit("path", vec![x(), y()], true),
                ],
            ));
        }
        if case.normal_aggr {
            rules.push(Rule::aggregated(
                "reach_count",
                vec![x(), y()],
                vec![None, named("count")],
                vec![lit("path", vec![x(), y()], false)],
            ));
        }
        if case.mutual || case.two_dep {
            // Mutual recursion: qa and qb derive each other, sharing
            // stratum 0 with path.
            rules.push(Rule::plain(
                "qa",
                vec![x(), y()],
                vec![lit("edge", vec![x(), y()], false)],
            ));
            rules.push(Rule::plain(
                "qa",
                vec![x(), z()],
                vec![
                    lit("qb", vec![x(), y()], false),
                    lit("edge", vec![y(), z()], false),
                ],
            ));
            rules.push(Rule::plain(
                "qb",
                vec![x(), z()],
                vec![
                    lit("qa", vec![x(), y()], false),
                    lit("edge", vec![y(), z()], false),
                ],
            ));
        }
        if case.two_dep {
            // One body joining two delta-carrying stores (path and qa both
            // change while pj is being derived), plus pj-recursion.
            rules.push(Rule::plain(
                "pj",
                vec![x(), z()],
                vec![
                    lit("path", vec![x(), y()], false),
                    lit("qa", vec![y(), z()], false),
                ],
            ));
            rules.push(Rule::plain(
                "pj",
                vec![x(), z()],
                vec![
                    lit("pj", vec![x(), y()], false),
                    lit("qa", vec![y(), z()], false),
                ],
            ));
        }
        Program {
            rules,
            facts,
            ..Program::default()
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        /// The moment of truth: randomized stratified programs — plain and
        /// self-join recursion, meet recursion over five lattices,
        /// negation, normal aggregation — through the real semi-naive
        /// evaluator and the sealed oracle, relation by relation.
        #[test]
        fn differential_randomized_stratified_programs(case in arb_case()) {
            assert_matches_oracle(&build_case(&case));
        }
    }

    // ── the determinism law ──────────────────────────────────────────────

    fn determinism_case() -> Program {
        let edges: Vec<(i64, i64)> = (0..12).map(|i| (i, (i * 7 + 3) % 12)).collect();
        let mut facts = edge_facts(&edges);
        facts.insert(
            "seed",
            [(0, 9), (5, 2), (11, 4)]
                .iter()
                .map(|(k, l)| vec![v(*k), v(*l)])
                .collect(),
        );
        let mut rules = transitive_closure_self_join();
        rules.extend(meet_reach_rules("min"));
        rules.push(Rule::plain(
            "out",
            vec![x(), y()],
            vec![lit("m", vec![x(), y()], false)],
        ));
        Program {
            rules,
            facts,
            ..Program::default()
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn at_thread_count<T: Send>(threads: usize, f: impl FnOnce() -> T + Send) -> T {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("thread pool")
            .install(f)
    }

    /// Same program + facts + budget ⇒ identical result sets AND identical
    /// witness tables at 1/2/4/8 rayon threads.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn determinism_results_and_witnesses_across_thread_counts() {
        let model = determinism_case();
        let run = |threads: usize| -> (BTreeSet<Tuple>, Vec<String>) {
            at_thread_count(threads, || {
                let compiled = compile_for(&model, "path", 2, &BTreeMap::new());
                let mut table = WitnessTable::default();
                let outcome = stratified_evaluate(
                    &compiled.program,
                    &compiled.lifetimes,
                    no_limit(),
                    &generous_budget(),
                    Some(&mut table),
                )
                .expect("evaluates");
                let rows = outcome
                    .store
                    .all_iter()
                    .map(TupleInIter::into_tuple)
                    .collect();
                let witnesses = table
                    .entries()
                    .iter()
                    .map(|w| format!("{w:?}"))
                    .collect_vec();
                (rows, witnesses)
            })
        };
        let baseline = run(1);
        for threads in [2, 4, 8] {
            let got = run(threads);
            assert_eq!(got.0, baseline.0, "result set differs at {threads} threads");
            assert_eq!(
                got.1, baseline.1,
                "witness table differs at {threads} threads"
            );
        }
    }

    /// A meet recursion whose meet column sits at head position 0 (a
    /// non-suffix layout), where group-key order and head-tuple order
    /// diverge. Admissions are reported in group-key order and the store's
    /// two views (`by_group`, `by_row`) must stay in lockstep regardless of
    /// how the parallel epoch schedules its rules — so results AND the
    /// per-group witness table stay byte-identical at 1/2/4/8 threads.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn determinism_nonsuffix_meet_across_thread_counts() {
        let edges: Vec<(i64, i64)> = (0..12).map(|i| (i, (i * 7 + 3) % 12)).collect();
        let mut facts = edge_facts(&edges);
        facts.insert(
            "seed",
            [(0, 9), (5, 2), (11, 4)]
                .iter()
                .map(|(k, l)| vec![v(*k), v(*l)])
                .collect(),
        );
        let model = Program {
            rules: meet_reach_rules_pos0("min"),
            facts,
            ..Program::default()
        };
        let run = |threads: usize| -> (BTreeSet<Tuple>, Vec<String>) {
            at_thread_count(threads, || {
                let compiled = compile_for(&model, "m", 2, &BTreeMap::new());
                let mut table = WitnessTable::default();
                let outcome = stratified_evaluate(
                    &compiled.program,
                    &compiled.lifetimes,
                    no_limit(),
                    &generous_budget(),
                    Some(&mut table),
                )
                .expect("evaluates");
                let rows = outcome
                    .store
                    .all_iter()
                    .map(TupleInIter::into_tuple)
                    .collect();
                let witnesses = table
                    .entries()
                    .iter()
                    .map(|w| format!("{w:?}"))
                    .collect_vec();
                (rows, witnesses)
            })
        };
        let baseline = run(1);
        for threads in [2, 4, 8] {
            let got = run(threads);
            assert_eq!(
                got.0, baseline.0,
                "non-suffix meet result set differs at {threads} threads"
            );
            assert_eq!(
                got.1, baseline.1,
                "non-suffix meet witness table differs at {threads} threads"
            );
        }
    }

    /// The refusal half of the law: a budget-exceeding case refuses
    /// byte-identically at every thread count (deterministic dimensions
    /// are checked at the barrier only, so the spend is exact).
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn determinism_budget_refusal_is_byte_identical_across_thread_counts() {
        let model = determinism_case();
        let run = |threads: usize| -> (String, BudgetDimension, u64, u64) {
            at_thread_count(threads, || {
                let compiled = compile_for(&model, "path", 2, &BTreeMap::new());
                let budget = generous_budget().with_derived_tuple_ceiling(20);
                let err = stratified_evaluate(
                    &compiled.program,
                    &compiled.lifetimes,
                    no_limit(),
                    &budget,
                    None,
                )
                .expect_err("must refuse");
                let refusal: &LimitExceeded =
                    err.downcast_ref().expect("typed LimitExceeded refusal");
                (
                    err.to_string(),
                    refusal.dimension,
                    refusal.spent,
                    refusal.ceiling,
                )
            })
        };
        let baseline = run(1);
        assert_eq!(baseline.1, BudgetDimension::DerivedTuples);
        for threads in [2, 4, 8] {
            assert_eq!(
                run(threads),
                baseline,
                "refusal differs at {threads} threads"
            );
        }
    }

    // ── budget refusals ──────────────────────────────────────────────────

    #[test]
    fn epoch_ceiling_refuses_deterministically() {
        // A 30-hop chain needs many epochs; a ceiling of 4 must refuse
        // with the exact typed spend.
        let edges: Vec<(i64, i64)> = (0..30).map(|i| (i, i + 1)).collect();
        let model = Program {
            rules: transitive_closure(),
            facts: edge_facts(&edges),
            ..Program::default()
        };
        let budget = Budget::new(NonZeroU32::new(4).unwrap());
        let err = real_eval(&model, "path", 2, &BTreeMap::new(), &budget).expect_err("refuses");
        let refusal: &LimitExceeded = err.downcast_ref().expect("typed refusal");
        assert_eq!(
            *refusal,
            LimitExceeded {
                dimension: BudgetDimension::Epochs,
                spent: 4,
                ceiling: 4,
                rule: None,
                span: None,
            }
        );
    }

    #[test]
    fn derived_tuple_ceiling_refuses_with_exact_spend() {
        let model = Program {
            rules: transitive_closure(),
            facts: edge_facts(&[(1, 2), (2, 3), (3, 4)]),
            ..Program::default()
        };
        // The full closure has 6 tuples; with the entry rule copying it
        // and the base facts admitted too, a ceiling of 3 refuses at the
        // first barrier that crosses it — always the same barrier.
        let budget = generous_budget().with_derived_tuple_ceiling(3);
        let err = real_eval(&model, "path", 2, &BTreeMap::new(), &budget).expect_err("refuses");
        let refusal: &LimitExceeded = err.downcast_ref().expect("typed refusal");
        assert_eq!(refusal.dimension, BudgetDimension::DerivedTuples);
        assert_eq!(refusal.ceiling, 3);
        assert!(refusal.spent > 3);
        // Deterministic: the same refusal again.
        let err2 = real_eval(&model, "path", 2, &BTreeMap::new(), &budget).expect_err("refuses");
        assert_eq!(err.to_string(), err2.to_string());
    }

    // ── the mid-epoch in-flight ceiling ──────────────────────────────────
    //
    // A rule body whose output stream is a near-cross-product: `a × b`
    // distinct rows in ONE epoch. This is the incident's shape — a single
    // legitimate join that materializes an unbounded intermediate before any
    // epoch barrier can check the derived-tuple ceiling. The `emitted`
    // counter is the materialization high-water mark (it upper-bounds the
    // out-store's size, since every emission is distinct here).
    struct CrossProduct {
        a: i64,
        b: i64,
        emitted: Arc<AtomicUsize>,
        contained: BTreeMap<AtomOccurrence, MagicSymbol>,
    }
    impl CrossProduct {
        fn new(a: i64, b: i64, emitted: Arc<AtomicUsize>) -> Self {
            Self {
                a,
                b,
                emitted,
                contained: BTreeMap::new(),
            }
        }
    }
    impl RuleBody for CrossProduct {
        fn for_each_derivation(
            &self,
            _stores: &BTreeMap<MagicSymbol, EpochStore>,
            _delta_from: Option<AtomOccurrence>,
            _want_premises: bool,
            f: &mut dyn FnMut(Cow<'_, [DataValue]>, Premises<'_>) -> Result<ControlFlow<()>>,
        ) -> Result<()> {
            for i in 0..self.a {
                for j in 0..self.b {
                    self.emitted.fetch_add(1, Ordering::Relaxed);
                    if f(Cow::Owned(vec![v(i), v(j)]), Premises::NotRequested)?.is_break() {
                        return Ok(());
                    }
                }
            }
            Ok(())
        }
        fn contained_rules(&self) -> &BTreeMap<AtomOccurrence, MagicSymbol> {
            &self.contained
        }
    }

    fn cross_product_program(
        symb: MagicSymbol,
        a: i64,
        b: i64,
        emitted: Arc<AtomicUsize>,
    ) -> EvalProgram<CrossProduct, NoFixed> {
        let body = CrossProduct::new(a, b, emitted);
        let rule_set = EvalRuleSet::new(vec![None, None], vec![body]).unwrap();
        let mut stratum: EvalStratum<CrossProduct, NoFixed> = EvalStratum::default();
        stratum.defs.insert(symb, EvalDefinition::Rules(rule_set));
        EvalProgram::from_execution_order(vec![stratum]).unwrap()
    }

    /// The core guarantee: a near-cross-product with a small derived-tuple
    /// ceiling refuses **mid-epoch**, before the barrier — and its
    /// materialization never exceeds `ceiling + INTERRUPT_STRIDE`. This is
    /// the hole the incident fell through: without the mid-epoch check the
    /// whole `a × b` intermediate materializes before any barrier fires.
    #[test]
    fn mid_epoch_in_flight_ceiling_refuses_before_barrier() {
        const CEILING: u64 = 100;
        let emitted = Arc::new(AtomicUsize::new(0));
        // 400 × 400 = 160_000 candidate rows if left unchecked.
        let program = cross_product_program(entry_symbol(), 400, 400, emitted.clone());
        let budget = generous_budget().with_derived_tuple_ceiling(CEILING);
        let err = stratified_evaluate(
            &program,
            &StoreLifetimes::default(),
            no_limit(),
            &budget,
            None,
        )
        .expect_err("must refuse mid-epoch");
        let refusal: &LimitExceeded = err.downcast_ref().expect("typed LimitExceeded");

        // It is the MID-EPOCH dimension, not the barrier's DerivedTuples.
        assert_eq!(refusal.dimension, BudgetDimension::InFlightDerivations);
        assert_eq!(refusal.ceiling, CEILING);
        // The refusal names the offending rule and labels its span.
        assert_eq!(refusal.rule.as_deref(), Some("?"));
        assert_eq!(refusal.span, Some(SourceSpan(0, 0)));
        // Spend crossed the ceiling but only within one stride of slack.
        assert!(refusal.spent > CEILING, "spent {} > ceiling", refusal.spent);
        assert!(
            refusal.spent <= CEILING + INTERRUPT_STRIDE as u64,
            "spend {} must be within a stride of the ceiling",
            refusal.spent
        );

        // THE BOUNDEDNESS PROOF: materialization never exceeded
        // ceiling + stride, though the full product is 160_000. This is the
        // assertion the mutation campaign shows *biting* — remove the
        // mid-epoch check and `emitted` becomes 160_000.
        let emitted = emitted.load(Ordering::Relaxed);
        assert!(
            (emitted as u64) <= CEILING + INTERRUPT_STRIDE as u64 + 1,
            "materialization {emitted} must be bounded by ceiling + stride, \
             not the {} of the full product",
            400 * 400
        );
    }

    /// Requirement 2: the mid-epoch refusal is byte-identical at 1/2/4/8
    /// rayon threads — same message, dimension, spend, ceiling, rule name,
    /// and span. Both terms of the check (barrier baseline; this rule's own
    /// sequential in-flight count) are deterministic, so the refusal is too.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn mid_epoch_refusal_is_byte_identical_across_thread_counts() {
        type Refusal = (
            String,
            BudgetDimension,
            u64,
            u64,
            Option<String>,
            Option<SourceSpan>,
        );
        let run = |threads: usize| -> Refusal {
            at_thread_count(threads, || {
                let emitted = Arc::new(AtomicUsize::new(0));
                let program = cross_product_program(entry_symbol(), 400, 400, emitted);
                let budget = generous_budget().with_derived_tuple_ceiling(100);
                let err = stratified_evaluate(
                    &program,
                    &StoreLifetimes::default(),
                    no_limit(),
                    &budget,
                    None,
                )
                .expect_err("must refuse");
                let r: &LimitExceeded = err.downcast_ref().expect("typed refusal");
                (
                    err.to_string(),
                    r.dimension,
                    r.spent,
                    r.ceiling,
                    r.rule.clone(),
                    r.span,
                )
            })
        };
        let baseline = run(1);
        assert_eq!(baseline.1, BudgetDimension::InFlightDerivations);
        assert_eq!(baseline.4.as_deref(), Some("?"));
        for threads in [2, 4, 8] {
            assert_eq!(
                run(threads),
                baseline,
                "refusal differs at {threads} threads"
            );
        }
    }

    /// When several rules of one stratum each cross the ceiling in parallel,
    /// the reported rule is the canonically-first among them — deterministic
    /// across thread counts, because we never read another in-flight rule's
    /// count. Two non-entry flooders `aaa` and `bbb` both blow the ceiling;
    /// `aaa` (canonically first) is always the one named.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn mid_epoch_refusal_names_canonically_first_tripping_rule() {
        let build = || -> EvalProgram<CrossProduct, NoFixed> {
            let mut s0: EvalStratum<CrossProduct, NoFixed> = EvalStratum::default();
            s0.defs.insert(
                muggle("aaa"),
                EvalDefinition::Rules(
                    EvalRuleSet::new(
                        vec![None, None],
                        vec![CrossProduct::new(400, 400, Arc::new(AtomicUsize::new(0)))],
                    )
                    .unwrap(),
                ),
            );
            s0.defs.insert(
                muggle("bbb"),
                EvalDefinition::Rules(
                    EvalRuleSet::new(
                        vec![None, None],
                        vec![CrossProduct::new(400, 400, Arc::new(AtomicUsize::new(0)))],
                    )
                    .unwrap(),
                ),
            );
            // The entry sits in a later stratum, never reached (stratum 0
            // refuses first), but from_execution_order requires it to exist.
            let mut s1: EvalStratum<CrossProduct, NoFixed> = EvalStratum::default();
            s1.defs.insert(
                entry_symbol(),
                EvalDefinition::Rules(
                    EvalRuleSet::new(
                        vec![None, None],
                        vec![CrossProduct::new(0, 0, Arc::new(AtomicUsize::new(0)))],
                    )
                    .unwrap(),
                ),
            );
            EvalProgram::from_execution_order(vec![s0, s1]).unwrap()
        };
        let run = |threads: usize| -> Option<String> {
            at_thread_count(threads, || {
                let program = build();
                let budget = generous_budget().with_derived_tuple_ceiling(100);
                let err = stratified_evaluate(
                    &program,
                    &StoreLifetimes::default(),
                    no_limit(),
                    &budget,
                    None,
                )
                .expect_err("must refuse");
                let r: &LimitExceeded = err.downcast_ref().expect("typed refusal");
                r.rule.clone()
            })
        };
        for threads in [1, 2, 4, 8] {
            assert_eq!(
                run(threads).as_deref(),
                Some("aaa"),
                "canonically-first tripping rule at {threads} threads"
            );
        }
    }

    // ── the refuted-theorem counterexample, landed as a differential ─────
    //
    // The hostile reviewer refuted the non-perturbation theorem on the MEET
    // path: a min-fold meet recursion over an N-node cycle with all seeds
    // EQUAL re-derives every group unchanged in epoch 1. The old guard ticked
    // `out.len()` (the fresh out-store's resident group count = N), while the
    // barrier admits ZERO of them — so the guard spuriously refused a program
    // the barrier completes, at every ceiling in `[true_spend, baseline+N]`.
    // Fix 1 counts admissions (`meet_put_admission_faithful`), so the guard
    // now fires only where the barrier would. This test sweeps that whole old
    // divergence window and demands byte-identical completion, plus an honest
    // (admitted, not in-flight) `spent` on the one barrier refusal below the
    // true spend. It FAILS on the pre-fix `out.len()` count.

    /// An N-node directed cycle `0→1→…→(N-1)→0`, every node seeded with the
    /// SAME value. `min` propagation never lowers anything, so epoch 1
    /// re-derives all N groups unchanged: resident `out.len() == N`, admitted
    /// `== 0`. This is the reviewer's `hostile_probe_meet_tightslack_low`.
    fn equal_seed_cycle_facts(n: i64, seed_val: i64) -> BTreeMap<Rel, BTreeSet<Tuple>> {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert("edge", (0..n).map(|i| vec![v(i), v((i + 1) % n)]).collect());
        facts.insert("seed", (0..n).map(|i| vec![v(i), v(seed_val)]).collect());
        facts
    }

    /// The meet recursion plus a single-row `count` on top — the reviewer's
    /// exact shape (tiny post-stratum footprint). Target relation is `cnt`.
    fn meet_tightslack_model(n: i64) -> Program {
        let mut rules = meet_reach_rules("min");
        // cnt[count(X)] :- m[X, Y] — all-aggregated, one output row.
        rules.push(Rule::aggregated(
            "cnt",
            vec![x()],
            vec![named("count")],
            vec![lit("m", vec![x(), y()], false)],
        ));
        Program {
            rules,
            facts: equal_seed_cycle_facts(n, 7),
            ..Program::default()
        }
    }

    #[test]
    fn meet_rerederivation_does_not_perturb_completing_program() {
        const N: i64 = 500;
        let model = meet_tightslack_model(N);
        let cnt = |c: u64| {
            real_eval(
                &model,
                "cnt",
                1,
                &BTreeMap::new(),
                &generous_budget().with_derived_tuple_ceiling(c),
            )
        };
        // Reference: the unbudgeted answer (no ceiling armed at all).
        let reference = real_eval(&model, "cnt", 1, &BTreeMap::new(), &generous_budget())
            .expect("unbudgeted meet recursion completes");

        // True admitted spend = the minimal ceiling at which it completes
        // (binary search; monotone in the ceiling). This is the barrier's
        // honest cost, independent of the guard.
        let (mut lo, mut hi) = (1u64, 4_000u64);
        assert!(cnt(hi).is_ok(), "completes at a generous ceiling");
        while lo < hi {
            let mid = (lo + hi) / 2;
            if cnt(mid).is_ok() {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        let true_spend = lo;
        // 500 seed groups admitted in epoch 0, plus the count stratum — the
        // reviewer's measured 502 for this exact shape.
        assert_eq!(true_spend, 502, "true admitted spend of the 500-cycle");

        // Just BELOW the true spend: the only refusal is the BARRIER
        // (DerivedTuples), and its `spent` is the true admitted spend — NOT
        // an in-flight overcount. (Pre-fix this was InFlightDerivations with
        // an inflated spend.)
        let err = cnt(true_spend - 1).expect_err("one under true spend must refuse");
        let refusal: &LimitExceeded = err.downcast_ref().expect("typed refusal");
        assert_eq!(
            refusal.dimension,
            BudgetDimension::DerivedTuples,
            "below true spend the honest refusal is the barrier, not the mid-epoch guard"
        );
        assert_eq!(
            refusal.spent, true_spend,
            "refusal spend is true admitted spend, not in-flight volume"
        );

        // THE SWEEP: every ceiling from the true spend up through the whole
        // old divergence window (`baseline + N` and beyond) must COMPLETE and
        // return the byte-identical reference answer. The pre-fix guard
        // refused the entire `[502, ~1000]` band here.
        for c in true_spend..=(true_spend + N as u64 + 40) {
            let got = cnt(c).unwrap_or_else(|e| {
                panic!("ceiling {c} ≥ true spend {true_spend} must complete, refused: {e:?}")
            });
            assert_eq!(
                got, reference,
                "ceiling {c}: guarded answer must be byte-identical to the unbudgeted answer"
            );
        }
    }

    // ── mutation-hardening at the boundaries (kills the 3 survivors) ──────

    /// Emits `distinct` distinct rows, then `dups` copies of row 0. The plain
    /// out-store dedups, so `out.len()` plateaus at `distinct` while the
    /// ticker keeps firing on the duplicate tail — this puts a stride check
    /// squarely on `out.len() == distinct`, the exact-at-ceiling boundary.
    struct DistinctThenDup {
        distinct: i64,
        dups: i64,
        contained: BTreeMap<AtomOccurrence, MagicSymbol>,
    }
    impl DistinctThenDup {
        fn new(distinct: i64, dups: i64) -> Self {
            Self {
                distinct,
                dups,
                contained: BTreeMap::new(),
            }
        }
    }
    impl RuleBody for DistinctThenDup {
        fn for_each_derivation(
            &self,
            _stores: &BTreeMap<MagicSymbol, EpochStore>,
            _delta_from: Option<AtomOccurrence>,
            _want_premises: bool,
            f: &mut dyn FnMut(Cow<'_, [DataValue]>, Premises<'_>) -> Result<ControlFlow<()>>,
        ) -> Result<()> {
            for i in 0..self.distinct {
                if f(Cow::Owned(vec![v(i), v(0)]), Premises::NotRequested)?.is_break() {
                    return Ok(());
                }
            }
            for _ in 0..self.dups {
                if f(Cow::Owned(vec![v(0), v(0)]), Premises::NotRequested)?.is_break() {
                    return Ok(());
                }
            }
            Ok(())
        }
        fn contained_rules(&self) -> &BTreeMap<AtomOccurrence, MagicSymbol> {
            &self.contained
        }
    }

    fn single_stratum_program<B: RuleBody>(symb: MagicSymbol, body: B) -> EvalProgram<B, NoFixed> {
        let rule_set = EvalRuleSet::new(vec![None, None], vec![body]).unwrap();
        let mut stratum: EvalStratum<B, NoFixed> = EvalStratum::default();
        stratum.defs.insert(symb, EvalDefinition::Rules(rule_set));
        EvalProgram::from_execution_order(vec![stratum]).unwrap()
    }

    /// Kills M3 (`spent > ceiling` → `>=`, the off-by-one). A rule that
    /// admits EXACTLY `ceiling` distinct rows then re-derives dominates:
    /// `out.len()` plateaus at the ceiling and a stride check lands on
    /// `spent == ceiling`. Exact-at-ceiling must COMPLETE (`>`), never refuse
    /// (`>=`). The barrier admits exactly `ceiling ≤ ceiling`, so the answer
    /// is the full `ceiling` distinct rows.
    #[test]
    fn exact_at_ceiling_completes_not_refused() {
        const CEILING: u64 = 128; // a stride multiple, so a check lands on it
        let emitted_distinct = CEILING as i64;
        // dups long enough that a stride check fires while out.len() == CEILING.
        let program =
            single_stratum_program(entry_symbol(), DistinctThenDup::new(emitted_distinct, 128));
        let budget = generous_budget().with_derived_tuple_ceiling(CEILING);
        let outcome = stratified_evaluate(
            &program,
            &StoreLifetimes::default(),
            no_limit(),
            &budget,
            None,
        )
        .expect("exact-at-ceiling spend must COMPLETE, not refuse (kills `>=`)");
        let rows = outcome.store.all_iter().count();
        assert_eq!(rows, CEILING as usize, "all exactly-ceiling rows survive");
    }

    /// Kills M2a (INTERRUPT_STRIDE ×64 weakening). The boundedness law is
    /// stride-linear, so the stride is load-bearing and pinned by a LITERAL
    /// — not by the `INTERRUPT_STRIDE` symbol (a bound written in terms of
    /// the symbol moves with the mutant and cannot detect it). A hostile
    /// near-cross-product must refuse having materialized no more than
    /// `ceiling + 64` rows; a 64× wider stride would let it reach ~4096.
    #[test]
    fn stride_pinned_at_64_bounds_materialization() {
        assert_eq!(
            INTERRUPT_STRIDE, 64,
            "the boundedness bound is O(ceiling + STRIDE); changing STRIDE is a \
             data-safety change — re-derive the bound and this pin deliberately"
        );
        const CEILING: u64 = 100;
        let emitted = Arc::new(AtomicUsize::new(0));
        let program = cross_product_program(entry_symbol(), 400, 400, emitted.clone());
        let budget = generous_budget().with_derived_tuple_ceiling(CEILING);
        stratified_evaluate(
            &program,
            &StoreLifetimes::default(),
            no_limit(),
            &budget,
            None,
        )
        .expect_err("must refuse mid-epoch");
        let emitted = emitted.load(Ordering::Relaxed) as u64;
        // Literal bound, NOT `CEILING + INTERRUPT_STRIDE`: with stride 64 the
        // guard trips by ~164 materialized; with the mutant's 4096 it would
        // reach ~4096, blowing this hard ceiling.
        assert!(
            emitted <= 100 + 64 + 1,
            "materialization {emitted} must stay within one 64-stride of the ceiling"
        );
    }

    /// Kills M4 (`epoch_baseline` zeroed). A completing stratum admits a
    /// NONZERO baseline; a later stratum's flooder must count it. With the
    /// real baseline the refusal spend is `baseline + in_flight`; zero it and
    /// the reported spend (and the trip point) shift. Pin the exact spend so
    /// the baseline term is load-bearing.
    #[test]
    fn nonzero_baseline_mid_epoch_refusal_counts_baseline() {
        // Stratum 0 admits exactly 100 distinct rows and COMPLETES (100 ≤
        // ceiling 101); it never trips (its only stride check sees
        // out.len()=63 < 101). Baseline for stratum 1 is therefore 100.
        let mut s0: EvalStratum<CrossProduct, NoFixed> = EvalStratum::default();
        s0.defs.insert(
            muggle("s0"),
            EvalDefinition::Rules(
                EvalRuleSet::new(
                    vec![None, None],
                    vec![CrossProduct::new(100, 1, Arc::new(AtomicUsize::new(0)))],
                )
                .unwrap(),
            ),
        );
        // Stratum 1 (the entry) floods; its FIRST stride check sees
        // out.len()=63, so spent = baseline(100) + 63 = 163 > ceiling 101.
        let mut s1: EvalStratum<CrossProduct, NoFixed> = EvalStratum::default();
        s1.defs.insert(
            entry_symbol(),
            EvalDefinition::Rules(
                EvalRuleSet::new(
                    vec![None, None],
                    vec![CrossProduct::new(400, 400, Arc::new(AtomicUsize::new(0)))],
                )
                .unwrap(),
            ),
        );
        let program = EvalProgram::from_execution_order(vec![s0, s1]).unwrap();
        let budget = generous_budget().with_derived_tuple_ceiling(101);
        let err = stratified_evaluate(
            &program,
            &StoreLifetimes::default(),
            no_limit(),
            &budget,
            None,
        )
        .expect_err("stratum 1 floods past baseline+ceiling");
        let refusal: &LimitExceeded = err.downcast_ref().expect("typed refusal");
        assert_eq!(refusal.dimension, BudgetDimension::InFlightDerivations);
        assert_eq!(refusal.ceiling, 101);
        assert_eq!(
            refusal.spent, 163,
            "spend must be baseline(100) + in_flight(63); zeroing the baseline changes it"
        );
    }

    /// A fixed rule that `put`s `rows` distinct tuples, ticking the ordinary
    /// per-rule mid-run guard ([`Budget::ticker`]) as it goes — exercising
    /// the exact `baseline` [`FixedRuleEval::run`] receives, the same way
    /// [`crate::fixed_rule::FixedRuleOutput`]'s own guard does in
    /// production.
    struct BaselineCheckingFixed {
        rows: i64,
        symb: MagicSymbol,
    }
    impl FixedRuleEval for BaselineCheckingFixed {
        fn run(
            &self,
            _stores: &BTreeMap<MagicSymbol, EpochStore>,
            out: &mut RegularTempStore,
            budget: &Budget,
            baseline: u64,
        ) -> Result<()> {
            let mut ticker = budget.ticker(baseline, &self.symb);
            for i in 0..self.rows {
                ticker.tick(out.len())?;
                out.put(vec![v(i)]);
            }
            Ok(())
        }
    }

    /// Fixed-rule twin of [`nonzero_baseline_mid_epoch_refusal_counts_baseline`]:
    /// proves the baseline `FixedRuleEval::run` now receives is the true
    /// global admitted spend, not the fixed baseline-0 compromise. Stratum 0
    /// admits exactly 100 rows and completes; stratum 1 (the entry) is a
    /// FIXED rule that puts up to 400 rows, ticking the same mid-run guard
    /// ordinary rules use.
    ///
    /// With ceiling 101: the fixed rule's first stride check lands at
    /// `out.len() == 63`, so `spent` must be `baseline(100) + 63 == 163 >
    /// 101` — refusing. Sabotage check: if the baseline were zeroed (the old
    /// compromise), `0 + 63 == 63 ≤ 101` would NOT trip at that check; the
    /// rule would keep materializing and only refuse later, at a different
    /// (lower) `spent` value — so pinning `spent == 163` exactly fails under
    /// that reversion.
    ///
    /// With ceiling 1000 (accommodates the true total of 100 + 400 = 500):
    /// the same program must COMPLETE, proving the plumbing doesn't
    /// over-refuse when the global total fits.
    #[test]
    fn fixed_rule_budget_counts_global_baseline() {
        fn program(ceiling: u64) -> (EvalProgram<CrossProduct, BaselineCheckingFixed>, Budget) {
            let mut s0: EvalStratum<CrossProduct, BaselineCheckingFixed> = EvalStratum::default();
            s0.defs.insert(
                muggle("s0"),
                EvalDefinition::Rules(
                    EvalRuleSet::new(
                        vec![None, None],
                        vec![CrossProduct::new(100, 1, Arc::new(AtomicUsize::new(0)))],
                    )
                    .unwrap(),
                ),
            );
            let mut s1: EvalStratum<CrossProduct, BaselineCheckingFixed> = EvalStratum::default();
            s1.defs.insert(
                entry_symbol(),
                EvalDefinition::Fixed {
                    arity: 1,
                    rule: BaselineCheckingFixed {
                        rows: 400,
                        symb: entry_symbol(),
                    },
                },
            );
            let program = EvalProgram::from_execution_order(vec![s0, s1]).unwrap();
            let budget = generous_budget().with_derived_tuple_ceiling(ceiling);
            (program, budget)
        }

        // Refuses: the fixed rule's own spend, uncounted, would never cross
        // 101; only the true global baseline (100 from stratum 0) does.
        let (prog, budget) = program(101);
        let err = stratified_evaluate(&prog, &StoreLifetimes::default(), no_limit(), &budget, None)
            .expect_err("the fixed rule must refuse because the global baseline is counted");
        let refusal: &LimitExceeded = err.downcast_ref().expect("typed refusal");
        assert_eq!(refusal.dimension, BudgetDimension::InFlightDerivations);
        assert_eq!(refusal.ceiling, 101);
        assert_eq!(
            refusal.spent, 163,
            "spend must be baseline(100) + in_flight(63); a zeroed baseline changes both \
             the trip point and this value"
        );

        // Completes: a ceiling that accommodates the true total (100 + 400)
        // must not refuse.
        let (prog, budget) = program(1000);
        let outcome =
            stratified_evaluate(&prog, &StoreLifetimes::default(), no_limit(), &budget, None)
                .expect("a ceiling covering the true total must not refuse");
        assert_eq!(outcome.store.all_iter().count(), 400);
    }

    /// F3 pin: the STREAMING harness bounds the killer shape. A 10_000×10_000
    /// cross product (100M candidate rows) through the `ModelBody` oracle
    /// harness with a small ceiling must refuse — typed, fast, bounded — NOT
    /// OOM below the tick seam as the pre-fix frontier-materializing harness
    /// did (reviewer finding F3). This is the harness twin of the
    /// compiled-path killer pin (`compile.rs` cross-product test). Its mere
    /// completion under the 12G cap is the boundedness proof; the assertions
    /// pin the typed, stride-bounded refusal.
    #[test]
    fn harness_killer_cross_product_streams_through_the_guard() {
        let n = 10_000i64;
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert("a", (0..n).map(|i| vec![v(i)]).collect());
        facts.insert("b", (0..n).map(|i| vec![v(i)]).collect());
        let model = Program {
            rules: vec![Rule::plain(
                "out",
                vec![x(), y()],
                vec![lit("a", vec![x()], false), lit("b", vec![y()], false)],
            )],
            facts,
            ..Program::default()
        };
        let budget = generous_budget().with_derived_tuple_ceiling(1_000);
        let err = real_eval(&model, "out", 2, &BTreeMap::new(), &budget)
            .expect_err("a 100M-row cross product must refuse, not OOM");
        let refusal: &LimitExceeded = err.downcast_ref().expect("typed refusal, not an abort");
        assert_eq!(refusal.dimension, BudgetDimension::InFlightDerivations);
        assert_eq!(refusal.ceiling, 1_000);
        assert!(refusal.spent > 1_000);
        assert!(
            refusal.spent <= 1_000 + INTERRUPT_STRIDE as u64,
            "materialization bounded within a stride of the ceiling: {}",
            refusal.spent
        );
    }

    #[test]
    fn deadline_zero_refuses() {
        let model = Program {
            rules: transitive_closure(),
            facts: edge_facts(&[(1, 2), (2, 3)]),
            ..Program::default()
        };
        let budget = generous_budget().with_timeout(Duration::ZERO);
        let err = real_eval(&model, "path", 2, &BTreeMap::new(), &budget).expect_err("refuses");
        let refusal: &LimitExceeded = err.downcast_ref().expect("typed refusal");
        assert_eq!(refusal.dimension, BudgetDimension::Deadline);
    }

    /// The unkillable-scan gap is closed: a rule mid-iteration observes
    /// the kill flag and stops, long before its scan would finish. The
    /// original checked poison once per rule, *after* the full scan.
    #[test]
    fn kill_flag_interrupts_inside_rule_iteration() {
        struct FloodBody {
            contained: BTreeMap<AtomOccurrence, MagicSymbol>,
            kill: Arc<AtomicBool>,
            emitted: Arc<AtomicUsize>,
        }
        impl RuleBody for FloodBody {
            fn for_each_derivation(
                &self,
                _stores: &BTreeMap<MagicSymbol, EpochStore>,
                _delta_from: Option<AtomOccurrence>,
                _want_premises: bool,
                f: &mut dyn FnMut(Cow<'_, [DataValue]>, Premises<'_>) -> Result<ControlFlow<()>>,
            ) -> Result<()> {
                for i in 0..1_000_000i64 {
                    if i == 10 {
                        self.kill.store(true, Ordering::Relaxed);
                    }
                    self.emitted.fetch_add(1, Ordering::Relaxed);
                    if f(Cow::Owned(vec![v(i)]), Premises::NotRequested)?.is_break() {
                        return Ok(());
                    }
                }
                Ok(())
            }
            fn contained_rules(&self) -> &BTreeMap<AtomOccurrence, MagicSymbol> {
                &self.contained
            }
        }
        let kill = Arc::new(AtomicBool::new(false));
        let emitted = Arc::new(AtomicUsize::new(0));
        let body = FloodBody {
            contained: BTreeMap::new(),
            kill: kill.clone(),
            emitted: emitted.clone(),
        };
        let rule_set = EvalRuleSet::new(vec![None], vec![body]).unwrap();
        let mut stratum: EvalStratum<FloodBody, NoFixed> = EvalStratum::default();
        stratum
            .defs
            .insert(entry_symbol(), EvalDefinition::Rules(rule_set));
        let program = EvalProgram::from_execution_order(vec![stratum]).unwrap();
        let budget = generous_budget().with_kill_flag(kill);
        let err = stratified_evaluate(
            &program,
            &StoreLifetimes::default(),
            no_limit(),
            &budget,
            None,
        )
        .expect_err("killed");
        assert!(
            err.downcast_ref::<Killed>().is_some(),
            "typed Killed refusal"
        );
        let count = emitted.load(Ordering::Relaxed);
        assert!(
            count < 10_000,
            "the scan must stop promptly after the kill (emitted {count})"
        );
    }

    /// A fixed-rule stand-in for programs that have none.
    #[derive(Debug)]
    struct NoFixed;
    impl FixedRuleEval for NoFixed {
        fn run(
            &self,
            _stores: &BTreeMap<MagicSymbol, EpochStore>,
            _out: &mut RegularTempStore,
            _budget: &Budget,
            _baseline: u64,
        ) -> Result<()> {
            Ok(())
        }
    }

    // ── limiter (early return) ───────────────────────────────────────────

    #[test]
    fn limiter_early_returns_take_minus_skip_rows() {
        let edges: Vec<(i64, i64)> = (0..10).map(|i| (i, i + 1)).collect();
        let model = Program {
            rules: transitive_closure(),
            facts: edge_facts(&edges),
            ..Program::default()
        };
        let oracle_db = naive_eval(&model).unwrap();
        let compiled = compile_for(&model, "path", 2, &BTreeMap::new());
        // :limit 2 :offset 1 → take 3, skip 1.
        let limit = RowLimit {
            num_to_take: Some(3),
            num_to_skip: Some(1),
        };
        let outcome = stratified_evaluate(
            &compiled.program,
            &compiled.lifetimes,
            limit,
            &generous_budget(),
            None,
        )
        .expect("evaluates");
        assert!(outcome.limited, "the limiter engaged");
        let returned: Vec<Tuple> = outcome
            .store
            .early_returned_iter()
            .map(TupleInIter::into_tuple)
            .collect();
        assert_eq!(returned.len(), 2, "limit rows, offset excluded");
        let taken: Vec<Tuple> = outcome
            .store
            .all_iter()
            .map(TupleInIter::into_tuple)
            .collect();
        assert_eq!(taken.len(), 3, "take = limit + offset rows produced");
        for row in taken {
            assert!(
                oracle_db["path"].contains(&row),
                "every row is a real answer"
            );
        }
    }

    /// The incremental limiter path (D1/D2 and the N2 overshoot), executed:
    /// the ENTRY rule itself is recursive (TC computed in the entry store),
    /// so `incremental_plain_eval` runs with the limiter engaged — dead
    /// code under the previous suite (the review's surviving mutant M5).
    ///
    /// Diamond + tail: edge (0,1),(0,2),(1,3),(2,3),(3,4); take = 7.
    /// Traced epochs (ModelBody iterates stores/facts in sorted order):
    ///   epoch 0: base rows (0,1),(0,2),(1,3),(2,3),(3,4)   — counter 5
    ///   epoch 1: (0,3) [count 6], (0,3) again — the D2 dedup point: the
    ///            re-derivation within the epoch must NOT count (upstream
    ///            double-counted here and stopped one row short), then
    ///            (1,4) [count 7 → stop; (2,4) never derived]
    ///   epoch 2: (0,4) put-then-counted [count 8] — the N2 overshoot row
    ///   epoch 3: nothing new → fixpoint
    /// Final store: exactly take + 1 rows, every one a real answer.
    #[test]
    fn limiter_incremental_entry_recursion_dedups_and_overshoots() {
        let edges = [(0, 1), (0, 2), (1, 3), (2, 3), (3, 4)];
        let rules = vec![
            Rule::plain(
                "?",
                vec![x(), y()],
                vec![lit("edge", vec![x(), y()], false)],
            ),
            Rule::plain(
                "?",
                vec![x(), z()],
                vec![
                    lit("?", vec![x(), y()], false),
                    lit("edge", vec![y(), z()], false),
                ],
            ),
        ];
        let oracle_model = Program {
            rules: rules.clone(),
            facts: edge_facts(&edges),
            ..Program::default()
        };
        let oracle_closure = naive_eval(&oracle_model).unwrap().remove("?").unwrap();

        let facts = Arc::new(edge_facts(&edges));
        let idb: Arc<BTreeSet<Rel>> = Arc::new(["?"].into_iter().collect());
        let bodies: Vec<ModelBody> = rules
            .iter()
            .map(|r| {
                ModelBody::new(
                    r.head_args.clone(),
                    r.body.clone(),
                    facts.clone(),
                    idb.clone(),
                )
            })
            .collect();
        let rule_set = EvalRuleSet::new(vec![None, None], bodies).unwrap();
        let mut stratum: EvalStratum<ModelBody, NoFixed> = EvalStratum::default();
        stratum
            .defs
            .insert(entry_symbol(), EvalDefinition::Rules(rule_set));
        let program = EvalProgram::from_execution_order(vec![stratum]).unwrap();

        let limit = RowLimit {
            num_to_take: Some(7),
            num_to_skip: None,
        };
        let outcome = stratified_evaluate(
            &program,
            &StoreLifetimes::default(),
            limit,
            &generous_budget(),
            None,
        )
        .expect("evaluates");
        assert!(outcome.limited, "the limiter engaged");
        let rows: BTreeSet<Tuple> = outcome
            .store
            .all_iter()
            .map(TupleInIter::into_tuple)
            .collect();
        for row in &rows {
            assert!(oracle_closure.contains(row), "every row is a real answer");
        }
        let expected: BTreeSet<Tuple> = [
            (0, 1),
            (0, 2),
            (1, 3),
            (2, 3),
            (3, 4),
            (0, 3),
            (1, 4),
            (0, 4),
        ]
        .iter()
        .map(|(a, b)| vec![v(*a), v(*b)])
        .collect();
        assert_eq!(
            rows, expected,
            "the traced limited set: D2 dedup keeps (1,4); N2 overshoot admits (0,4)"
        );
        assert_eq!(rows.len(), 8, "take + 1 rows: the documented N2 overshoot");
    }

    #[test]
    fn without_limit_the_outcome_is_not_limited() {
        let model = Program {
            rules: transitive_closure(),
            facts: edge_facts(&[(1, 2)]),
            ..Program::default()
        };
        let compiled = compile_for(&model, "path", 2, &BTreeMap::new());
        let outcome = stratified_evaluate(
            &compiled.program,
            &compiled.lifetimes,
            no_limit(),
            &generous_budget(),
            None,
        )
        .expect("evaluates");
        assert!(!outcome.limited);
    }

    // ── provenance hooks ─────────────────────────────────────────────────

    #[test]
    fn witnesses_record_first_derivations_in_canonical_order() {
        let model = Program {
            rules: transitive_closure(),
            facts: edge_facts(&[(1, 2), (2, 3)]),
            ..Program::default()
        };
        let compiled = compile_for(&model, "path", 2, &BTreeMap::new());
        let mut table = WitnessTable::default();
        stratified_evaluate(
            &compiled.program,
            &compiled.lifetimes,
            no_limit(),
            &generous_budget(),
            Some(&mut table),
        )
        .expect("evaluates");

        let path_store = muggle("path");
        let path_witnesses: Vec<&Witness> = table
            .entries()
            .iter()
            .filter(|w| w.store == path_store)
            .collect();
        // The closure of 1→2→3 is {(1,2),(2,3),(1,3)}: each admitted once.
        assert_eq!(path_witnesses.len(), 3);
        // Epoch 0 admits the base tuples in canonical order.
        assert_eq!(path_witnesses[0].tuple, vec![v(1), v(2)]);
        assert_eq!(path_witnesses[1].tuple, vec![v(2), v(3)]);
        // Base tuples: rule 0, premise = the edge row.
        assert_eq!(
            path_witnesses[0].derivation,
            Some((0, vec![vec![v(1), v(2)]]))
        );
        // The recursive tuple: rule 1, premises = edge(1,2) then path(2,3).
        assert_eq!(path_witnesses[2].tuple, vec![v(1), v(3)]);
        assert_eq!(
            path_witnesses[2].derivation,
            Some((1, vec![vec![v(1), v(2)], vec![v(2), v(3)]]))
        );
    }

    #[test]
    fn meet_identity_row_witness_has_no_derivation() {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert("nothing", BTreeSet::new());
        let model = Program {
            rules: vec![Rule::aggregated(
                "g",
                vec![x(), y()],
                vec![named("min"), named("or")],
                vec![lit("nothing", vec![x(), y()], false)],
            )],
            facts,
            ..Program::default()
        };
        let compiled = compile_for(&model, "g", 2, &BTreeMap::new());
        let mut table = WitnessTable::default();
        stratified_evaluate(
            &compiled.program,
            &compiled.lifetimes,
            no_limit(),
            &generous_budget(),
            Some(&mut table),
        )
        .expect("evaluates");
        let g_store = muggle("g");
        let identity: Vec<&Witness> = table
            .entries()
            .iter()
            .filter(|w| w.store == g_store)
            .collect();
        assert_eq!(identity.len(), 1);
        assert_eq!(
            identity[0].tuple,
            vec![DataValue::Null, DataValue::from(false)]
        );
        assert_eq!(
            identity[0].derivation, None,
            "identity row has no derivation"
        );
    }

    // ── constructor refusals and typed invariants ────────────────────────

    #[test]
    fn empty_rule_set_is_refused_at_construction() {
        let refused = EvalRuleSet::<ModelBody>::new(vec![None], vec![]);
        assert!(matches!(refused, Err(RuleSetShapeError::Empty)));
    }

    /// The retired deviation D3: a non-suffix all-meet head (here the meet
    /// column sits at position 0, ahead of its grouping position) is no
    /// longer a constructor refusal. The landed [`MeetAggrStore`] groups by
    /// position, so the shape the original silently demoted to a frozen
    /// normal aggregation (wrong answers) now constructs cleanly and its
    /// grouping positions are recorded exactly where they sit.
    #[test]
    fn non_suffix_meet_head_constructs_with_positional_grouping() {
        let facts = Arc::new(BTreeMap::new());
        let idb = Arc::new(BTreeSet::new());
        let body = ModelBody::new(
            vec![y(), x()],
            vec![lit("d", vec![x(), y()], false)],
            facts,
            idb,
        );
        // Meet at position 0, grouping at position 1 — the exact shape D3
        // used to reject.
        let rule_set =
            EvalRuleSet::new(vec![named("min"), None], vec![body]).expect("no longer refused");
        assert_eq!(rule_set.kind, HeadAggrKind::Meet);
        assert_eq!(
            rule_set.meet_key_positions,
            vec![1],
            "the grouping position is position 1, wherever the meet column sits"
        );
    }

    /// The end-to-end companion to the retired refusal: the same non-suffix
    /// shape (meet at position 0) does not merely construct — it *answers*,
    /// folding each group's meet exactly as the sealed positional oracle
    /// does, instead of the original's frozen demotion.
    #[test]
    fn non_suffix_meet_head_answers_matching_oracle() {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "obs",
            [(1, 5), (1, 3), (2, 9)]
                .iter()
                .map(|(k, val)| vec![v(*k), v(*val)])
                .collect(),
        );
        // m[min(V), K] :- obs[K, V].  Grouping position is 1 (K), the meet
        // column is position 0 — a non-suffix layout the old store refused.
        let rules = vec![Rule::aggregated(
            "m",
            vec![y(), x()],
            vec![named("min"), None],
            vec![lit("obs", vec![x(), y()], false)],
        )];
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    // ── adversarial reviewer attacks (adopted from the hostile pass) ──────
    // Adopted verbatim from the reviewer's deliverables; only imports/naming
    // match house style. These pin the witness-by-grouping-projection
    // correctness the frozen diff left unpinned (the surviving M6 mutant),
    // plus Null / shared-var / all-aggregated / negation-below coverage.

    /// ATTACK 1a: Null values in the grouping column AND in the meet column
    /// at a non-suffix layout. Null's position in DataValue's total order is
    /// load-bearing for both by_group and by_row ordering.
    #[test]
    fn rev_differential_meet_pos0_nulls_in_group_and_value() {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "obs",
            vec![
                vec![DataValue::Null, v(5)],
                vec![DataValue::Null, v(2)],
                vec![v(1), DataValue::Null],
                vec![v(1), v(7)],
                vec![v(2), v(3)],
            ]
            .into_iter()
            .collect(),
        );
        // m[min(V), K] :- obs[K, V]  — Null group key, Null meet value.
        let rules = vec![Rule::aggregated(
            "m",
            vec![y(), x()],
            vec![named("min"), None],
            vec![lit("obs", vec![x(), y()], false)],
        )];
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    /// ATTACK 1b: the same variable at a grouping position AND a meet
    /// position (m[min(V), V]): every group folds itself.
    #[test]
    fn rev_differential_meet_var_shared_by_key_and_val() {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "obs",
            [(1, 5), (1, 3), (2, 3), (2, 9)]
                .iter()
                .map(|(k, val)| vec![v(*k), v(*val)])
                .collect(),
        );
        let rules = vec![Rule::aggregated(
            "m",
            vec![y(), y()],
            vec![named("min"), None],
            vec![lit("obs", vec![x(), y()], false)],
        )];
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    /// ATTACK 1c: all-aggregated multi-column meet head (empty group key —
    /// one group, keyed by the empty tuple) inside recursion, WITH real
    /// derivations so the identity row must never appear.
    #[test]
    fn rev_differential_meet_all_aggregated_recursive() {
        let mut facts = edge_facts(&[(1, 2), (2, 3), (3, 5), (5, 1)]);
        facts.insert("start", [vec![v(3), v(3)]].into_iter().collect());
        // m[min(A), max(B)] :- start[A, B]
        // m[min(Y), max(Y)] :- m[X, _ignored], edge[X, Y]
        let rules = vec![
            Rule::aggregated(
                "m",
                vec![x(), y()],
                vec![named("min"), named("max")],
                vec![lit("start", vec![x(), y()], false)],
            ),
            Rule::aggregated(
                "m",
                vec![y(), y()],
                vec![named("min"), named("max")],
                vec![
                    lit("m", vec![x(), z()], false),
                    lit("edge", vec![x(), y()], false),
                ],
            ),
        ];
        assert_matches_oracle(&Program {
            rules,
            facts,
            ..Program::default()
        });
    }

    /// ATTACK 3/6: a nastier determinism program — a non-suffix meet
    /// recursion whose seed relation is derived THROUGH NEGATION in a lower
    /// stratum, on a bigger denser graph, plus an interleaved 3-column meet
    /// head in the same program. Results and witness tables must be
    /// byte-identical at 1/2/4/8 threads.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn rev_determinism_nonsuffix_meet_negation_below() {
        let edges: Vec<(i64, i64)> = (0..24)
            .flat_map(|i| vec![(i, (i * 5 + 7) % 24), (i, (i * 11 + 3) % 24)])
            .collect();
        let mut facts = edge_facts(&edges);
        facts.insert("node", (0..24).map(|i| vec![v(i)]).collect());
        facts.insert(
            "special",
            [0i64, 7, 13, 21].iter().map(|i| vec![v(*i)]).collect(),
        );
        let mut rules = vec![
            // Stratum below: nonspecial via negation.
            Rule::plain(
                "nonspecial",
                vec![x()],
                vec![
                    lit("node", vec![x()], false),
                    lit("special", vec![x()], true),
                ],
            ),
            // Seed: every nonspecial node seeds with its own id.
            Rule::plain(
                "seed",
                vec![x(), x()],
                vec![lit("nonspecial", vec![x()], false)],
            ),
        ];
        rules.extend(meet_reach_rules_pos0("min"));
        // A second, interleaved meet head in the same program:
        // w[min(V), K, max(V)] :- m[V, K].
        rules.push(Rule::aggregated(
            "w",
            vec![y(), x(), y()],
            vec![named("min"), None, named("max")],
            vec![lit("m", vec![y(), x()], false)],
        ));
        rules.push(Rule::plain(
            "out",
            vec![x(), y(), z()],
            vec![lit("w", vec![x(), y(), z()], false)],
        ));
        let model = Program {
            rules,
            facts,
            ..Program::default()
        };
        assert_matches_oracle(&model);
        let run = |threads: usize| -> (BTreeSet<Tuple>, Vec<String>) {
            at_thread_count(threads, || {
                let compiled = compile_for(&model, "out", 3, &BTreeMap::new());
                let mut table = WitnessTable::default();
                let outcome = stratified_evaluate(
                    &compiled.program,
                    &compiled.lifetimes,
                    no_limit(),
                    &generous_budget(),
                    Some(&mut table),
                )
                .expect("evaluates");
                let rows = outcome
                    .store
                    .all_iter()
                    .map(TupleInIter::into_tuple)
                    .collect();
                let witnesses = table
                    .entries()
                    .iter()
                    .map(|w| format!("{w:?}"))
                    .collect_vec();
                (rows, witnesses)
            })
        };
        let baseline = run(1);
        for threads in [2, 4, 8] {
            let got = run(threads);
            assert_eq!(got.0, baseline.0, "results differ at {threads} threads");
            assert_eq!(got.1, baseline.1, "witnesses differ at {threads} threads");
        }
    }

    /// ATTACK: positive witness binding for a NON-SUFFIX meet head — the
    /// admitted group's witness must carry Some(derivation) recovered
    /// through the grouping projection (this is the assertion the frozen
    /// diff's own tests never make; a consistently mis-keyed projection
    /// passes every thread-count comparison).
    #[test]
    fn rev_nonsuffix_meet_witness_binds_derivation() {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "obs",
            [(1, 5), (1, 3), (2, 9)]
                .iter()
                .map(|(k, val)| vec![v(*k), v(*val)])
                .collect(),
        );
        // m[min(V), K] :- obs[K, V]
        let model = Program {
            rules: vec![Rule::aggregated(
                "m",
                vec![y(), x()],
                vec![named("min"), None],
                vec![lit("obs", vec![x(), y()], false)],
            )],
            facts,
            ..Program::default()
        };
        let compiled = compile_for(&model, "m", 2, &BTreeMap::new());
        let mut table = WitnessTable::default();
        stratified_evaluate(
            &compiled.program,
            &compiled.lifetimes,
            no_limit(),
            &generous_budget(),
            Some(&mut table),
        )
        .expect("evaluates");
        let m_store = muggle("m");
        let ws: Vec<&Witness> = table
            .entries()
            .iter()
            .filter(|w| w.store == m_store)
            .collect();
        assert_eq!(ws.len(), 2, "one witness per admitted group");
        for w in &ws {
            assert!(
                w.derivation.is_some(),
                "a non-suffix meet admission must bind its pending derivation: {w:?}"
            );
        }
        // Group 1 folded to min 3; its witness is the FIRST derivation seen
        // for the group, whose premise row comes from obs.
        assert_eq!(ws[0].tuple, vec![v(3), v(1)]);
        let (_, premises) = ws[0].derivation.as_ref().unwrap();
        assert_eq!(premises.len(), 1);
        assert!(
            premises[0] == vec![v(1), v(3)] || premises[0] == vec![v(1), v(5)],
            "premise must be a real obs row for group 1: {:?}",
            premises[0]
        );
    }

    /// ATTACK (killer for prefix-keyed witness regressions): two groups
    /// fold to the SAME meet value at a non-suffix layout. Witness keying
    /// that collapses to the value prefix cannot tell the groups apart and
    /// binds group 2's witness to group 1's derivation. Each group's
    /// premises must come from its OWN obs rows.
    #[test]
    fn rev_nonsuffix_meet_witness_premises_are_per_group() {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "obs",
            [(1, 3), (1, 5), (2, 3)]
                .iter()
                .map(|(k, val)| vec![v(*k), v(*val)])
                .collect(),
        );
        // m[min(V), K] :- obs[K, V]; groups 1 and 2 both fold to min 3.
        let model = Program {
            rules: vec![Rule::aggregated(
                "m",
                vec![y(), x()],
                vec![named("min"), None],
                vec![lit("obs", vec![x(), y()], false)],
            )],
            facts,
            ..Program::default()
        };
        let compiled = compile_for(&model, "m", 2, &BTreeMap::new());
        let mut table = WitnessTable::default();
        stratified_evaluate(
            &compiled.program,
            &compiled.lifetimes,
            no_limit(),
            &generous_budget(),
            Some(&mut table),
        )
        .expect("evaluates");
        let m_store = muggle("m");
        let ws: Vec<&Witness> = table
            .entries()
            .iter()
            .filter(|w| w.store == m_store)
            .collect();
        assert_eq!(ws.len(), 2);
        for w in &ws {
            let group = w.tuple[1].clone();
            let (_, premises) = w
                .derivation
                .as_ref()
                .unwrap_or_else(|| panic!("unbound witness for group {group:?}"));
            assert_eq!(
                premises[0][0], group,
                "witness for group {group:?} bound a premise from another group: {premises:?}"
            );
        }
    }

    // ATTACK 1d (randomized): the full randomized stratified differential,
    // but with the meet column at position 0 — the frozen diff's proptest
    // only ever generates suffix layouts.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn rev_differential_randomized_nonsuffix_meet(case in arb_case()) {
            let mut model = build_case(&case);
            // Swap the suffix meet rules for the pos0 form and re-point the
            // reader at the swapped columns.
            model.rules.retain(|r| r.head_rel != "m" && r.head_rel != "out");
            model.rules.extend(meet_reach_rules_pos0(case.aggr_name));
            model.rules.push(Rule::plain(
                "out",
                vec![x(), y()],
                vec![lit("m", vec![y(), x()], false)],
            ));
            assert_matches_oracle(&model);
        }
    }

    #[test]
    fn missing_store_is_a_typed_error_not_a_panic() {
        // A rule whose contained-rules map names a store no stratum
        // defines: epoch 1's delta discipline must surface the invariant
        // as an error.
        struct GhostBody {
            contained: BTreeMap<AtomOccurrence, MagicSymbol>,
        }
        impl RuleBody for GhostBody {
            fn for_each_derivation(
                &self,
                _stores: &BTreeMap<MagicSymbol, EpochStore>,
                delta_from: Option<AtomOccurrence>,
                _want_premises: bool,
                f: &mut dyn FnMut(Cow<'_, [DataValue]>, Premises<'_>) -> Result<ControlFlow<()>>,
            ) -> Result<()> {
                if delta_from.is_none() {
                    let _ = f(Cow::Owned(vec![v(1)]), Premises::NotRequested)?;
                }
                Ok(())
            }
            fn contained_rules(&self) -> &BTreeMap<AtomOccurrence, MagicSymbol> {
                &self.contained
            }
        }
        let mut contained = BTreeMap::new();
        contained.insert(AtomOccurrence(0), muggle("ghost"));
        let rule_set = EvalRuleSet::new(vec![None], vec![GhostBody { contained }]).unwrap();
        let mut stratum: EvalStratum<GhostBody, NoFixed> = EvalStratum::default();
        stratum
            .defs
            .insert(entry_symbol(), EvalDefinition::Rules(rule_set));
        let program = EvalProgram::from_execution_order(vec![stratum]).unwrap();
        let err = stratified_evaluate(
            &program,
            &StoreLifetimes::default(),
            no_limit(),
            &generous_budget(),
            None,
        )
        .expect_err("typed invariant error");
        assert!(err.to_string().contains("invariant"), "got: {err}");
    }

    #[test]
    fn entry_less_program_is_refused_at_construction() {
        let mut stratum: EvalStratum<ModelBody, NoFixed> = EvalStratum::default();
        let facts = Arc::new(BTreeMap::new());
        let idb = Arc::new(BTreeSet::new());
        let body = ModelBody::new(vec![x()], vec![lit("d", vec![x()], false)], facts, idb);
        stratum.defs.insert(
            muggle("r"),
            EvalDefinition::Rules(EvalRuleSet::new(vec![None], vec![body]).unwrap()),
        );
        let err = EvalProgram::from_execution_order(vec![stratum]).expect_err("no entry");
        assert!(err.to_string().contains("no entry"), "got: {err}");
    }

    #[test]
    fn epoch_ceiling_of_one_refuses_any_deriving_program() {
        // Even a settled derivation needs a second epoch to certify the
        // fixpoint: the minimum viable ceiling is 2, deterministically.
        let model = Program {
            rules: vec![Rule::plain(
                "p",
                vec![x()],
                vec![lit("d", vec![x()], false)],
            )],
            facts: {
                let mut f: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
                f.insert("d", [vec![v(1)]].into_iter().collect());
                f
            },
            ..Program::default()
        };
        let budget = Budget::new(NonZeroU32::new(1).unwrap());
        let err = real_eval(&model, "p", 1, &BTreeMap::new(), &budget).expect_err("refuses");
        let refusal: &LimitExceeded = err.downcast_ref().expect("typed refusal");
        assert_eq!(refusal.dimension, BudgetDimension::Epochs);
        let ok = real_eval(
            &model,
            "p",
            1,
            &BTreeMap::new(),
            &Budget::new(NonZeroU32::new(2).unwrap()),
        );
        assert!(ok.is_ok(), "two epochs suffice for a settled derivation");
    }
}
