/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The fixed-rule contract surface: cancellation, payload/input/output,
//! the [`FixedRule`] trait, the built-in registry, order-preserving
//! parallelism, and the session-backed [`SessionFixedRule`] evaluation
//! adapter.
//!
//! A **fixed rule** is an opaque computation the Datalog engine treats as
//! a single stratum-bounded rule: it consumes whole input relations and
//! produces one output relation of a declared arity, and it never
//! participates in recursion. Algorithms live in [`crate::rules::algo`];
//! I/O utilities in [`crate::rules::io`]; CSR graph builders in
//! [`crate::rules::graph_view`].
//!
//! Several fixed rules fan out an independent, side-effect-free computation
//! per node / per start / per node-pair, then fold the results.
//! [`par_try_map`] is order-preserving by construction — so the axis it
//! parallelizes never reaches the output as scheduling order.
//!
//! [`SessionFixedRule`] bridges one `MagicFixedRuleApply` to `FixedRule::run`
//! at evaluation time. Output is branded with the manifest arity (never a
//! caller-supplied one); the budget's cancel poll is shared so a cancelled
//! query stops the rule; budgeted output is armed with the true global
//! admitted total.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, LazyLock, OnceLock};

use itertools::Itertools;
use miette::{Diagnostic, Result, bail, ensure};
#[cfg(not(target_arch = "wasm32"))]
use rayon::prelude::*;
use thiserror::Error;

use kyzo_model::SourceSpan;
use kyzo_model::data_value_any;
use kyzo_model::program::expr::Expr;
use kyzo_model::program::rule::FixedRuleOptions;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::row::TupleIter;
use kyzo_model::value::{AsOf, DataValue, Tuple};

use crate::data::json::NamedRows;
use crate::exec::fixpoint::delta_store::{EpochStore, RegularTempStore};
use crate::exec::fixpoint::eval::{Budget, BudgetDimension, FixedRuleEval, LimitExceeded};
use crate::exec::plan::program::{
    FixedRuleOptionNotFoundError, MagicFixedRuleApply, MagicFixedRuleRuleArg, MagicSymbol,
    WrongFixedRuleOptionError, WrongFixedRuleOptionHelp,
};
use crate::rules::algo::*;
use crate::rules::graph_view::{DirectedCsrGraph, as_directed_graph, as_directed_weighted_graph};
use crate::rules::io::*;

// Model owns the name wrapper; re-export so engine call sites share one type.
pub use kyzo_model::program::rule::FixedRuleHandle;

#[cfg(test)]
use crate::exec::fixpoint::delta_store::TupleInIter;
/// Order-preserving fallible parallel map: apply `f` to every item, collect
/// the results into a `Vec` **in the same order as `items`**, and
/// short-circuit on the first `Err`.
///
/// On native targets the map runs on `rayon`'s thread pool; on `wasm32`
/// (no threads) it degrades to a sequential map, matching how
/// `query/eval.rs` gates its per-epoch batch. `rayon`'s `collect` into a
/// `Vec` is index-preserving, so the output order equals the input order
/// regardless of how work is scheduled across threads — that is the
/// property callers rely on for determinism.
///
/// This parallelizes only the per-item compute. Any reduction *across*
/// items whose result depends on evaluation order (a float sum, say) must be
/// performed by the caller as a sequential fold over the returned `Vec`,
/// never smuggled into a parallel reduction — see the algorithm call sites.
#[cfg(not(target_arch = "wasm32"))]
#[cfg(test)]
use smartstring::LazyCompact;
#[cfg(test)]
use smartstring::SmartString;
pub(crate) fn par_try_map<T, R, F>(items: Vec<T>, f: F) -> Result<Vec<R>>
where
    T: Send,
    R: Send,
    F: Fn(T) -> Result<R> + Send + Sync,
{
    items.into_par_iter().map(f).collect()
}

/// `wasm32` has no threads; run the same fallible map sequentially. The
/// output is identical to the native path (both preserve input order), so
/// callers need not know which one they got.
#[cfg(target_arch = "wasm32")]
pub(crate) fn par_try_map<T, R, F>(items: Vec<T>, f: F) -> Result<Vec<R>>
where
    F: Fn(T) -> Result<R>,
{
    items.into_iter().map(f).collect()
}

// ─────────────────────────────────────────────────────────────────────────
// Cancellation lifecycle
// ─────────────────────────────────────────────────────────────────────────

/// Private shared cancel publish cell: a one-shot latch (`OnceLock`), not
/// an `AtomicBool` stop-bit. Cancellation is minted only through
/// [`CancelAuthority::arm`] / [`CancelAuthority::cancel`].
struct CancelCell(OnceLock<()>);

/// Typestate proof that cancellation was requested (via
/// [`CancelAuthority::cancel`]) or observed (via [`CancelFlag::check`]).
///
/// Consuming [`CancelAuthority::cancel`] is the only door that requests
/// stop; shared mutable `AtomicBool` lifecycle bits are not part of the
/// surface (S337-08 / P114).
#[derive(Debug, Error, Diagnostic)]
#[error("Running query is killed before completion")]
#[diagnostic(code(eval::killed))]
#[diagnostic(help("A query may be killed by timeout, or explicit command"))]
pub struct Cancelled;

/// Authority to request cancellation. Paired with [`CancelFlag`] by
/// [`Self::arm`]. [`Self::cancel`] consumes the authority and yields
/// [`Cancelled`] — one authority, one request.
pub struct CancelAuthority {
    cell: Arc<CancelCell>,
}

impl CancelAuthority {
    /// Arm a paired authority + poll handle that share one cancel cell.
    pub fn arm() -> (Self, CancelFlag) {
        let cell = Arc::new(CancelCell(OnceLock::new()));
        (
            Self {
                cell: Arc::clone(&cell),
            },
            CancelFlag { cell },
        )
    }

    /// Spend this authority: publish cancellation and return the
    /// [`Cancelled`] proof. Every subsequent [`CancelFlag::check`] on the
    /// paired poll handle refuses. `OnceLock` publish is the synchronization
    /// edge (no `Relaxed` `AtomicBool`).
    pub fn cancel(self) -> Cancelled {
        match self.cell.0.set(()) {
            Ok(()) | Err(()) => Cancelled,
        }
    }
}

/// Cooperative poll handle for long-running fixed rules (and the budget
/// interrupt path). Clone into algorithms; cannot request cancel — that
/// is [`CancelAuthority`]'s job (species pair).
#[derive(Clone)]
pub struct CancelFlag {
    cell: Arc<CancelCell>,
}

impl std::fmt::Debug for CancelFlag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CancelFlag({})", self.cell.0.get().is_some())
    }
}

impl CancelFlag {
    /// Inert poll handle that never observes cancellation.
    pub fn inert() -> Self {
        Self {
            cell: Arc::new(CancelCell(OnceLock::new())),
        }
    }

    /// Refuses with [`Cancelled`] if cancellation has been requested.
    /// Poll this at least once per unit of unbounded work (per node
    /// visited, per edge relaxed) — a loop that never checks is a loop
    /// that cannot be killed.
    #[inline(always)]
    pub fn check(&self) -> Result<()> {
        if self.cell.0.get().is_some() {
            bail!(Cancelled)
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// SEAM: stored-relation input
// ─────────────────────────────────────────────────────────────────────────

/// What the payload needs from the transaction in order to serve a
/// `MagicFixedRuleRuleArg::Stored` input: arity lookup and (validity-
/// aware) scans. `SessionView` implements this in production.
pub(crate) trait StoredInputSource {
    fn stored_arity(&self, name: &Symbol) -> Result<usize>;
    /// Scan the whole relation, as-of `as_of` if given.
    fn stored_scan_all<'a>(&'a self, name: &Symbol, as_of: Option<AsOf>) -> Result<TupleIter<'a>>;
    /// Scan the tuples whose first key column equals `prefix`.
    fn stored_scan_prefix<'a>(
        &'a self,
        name: &Symbol,
        prefix: &DataValue,
        as_of: Option<AsOf>,
    ) -> Result<TupleIter<'a>>;
}

// ─────────────────────────────────────────────────────────────────────────
// Payload and input relations
// ─────────────────────────────────────────────────────────────────────────

/// Passed into implementation of fixed rule, can be used to obtain relation inputs and options
pub struct FixedRulePayload<'a> {
    pub(crate) manifest: &'a MagicFixedRuleApply,
    pub(crate) stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    pub(crate) stored: &'a dyn StoredInputSource,
}

/// Represents an input relation during the execution of a fixed rule
#[derive(Copy, Clone)]
pub struct FixedRuleInputRelation<'a> {
    arg_manifest: &'a MagicFixedRuleRuleArg,
    stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    stored: &'a dyn StoredInputSource,
}

impl<'a> FixedRuleInputRelation<'a> {
    /// The arity of the input relation
    pub fn arity(&self) -> Result<usize> {
        self.arg_manifest.arity(self.stores, self.stored)
    }
    /// Ensure the input relation contains tuples of the given minimal length.
    pub fn ensure_min_len(self, len: usize) -> Result<Self> {
        #[derive(Error, Diagnostic, Debug)]
        #[error("Input relation to algorithm has insufficient arity")]
        #[diagnostic(help("Arity should be at least {0} but is {1}"))]
        #[diagnostic(code(algo::input_relation_bad_arity))]
        struct InputRelationArityError(usize, usize, #[label] SourceSpan);

        let arity = self.arity()?;
        ensure!(
            arity >= len,
            InputRelationArityError(len, arity, self.arg_manifest.span())
        );
        Ok(self)
    }
    /// Get the binding map of the input relation
    pub fn get_binding_map(&self, offset: usize) -> BTreeMap<Symbol, usize> {
        self.arg_manifest.get_binding_map(offset)
    }
    /// Iterate the input relation
    pub fn iter(&self) -> Result<TupleIter<'a>> {
        Ok(match &self.arg_manifest {
            MagicFixedRuleRuleArg::InMem { name, .. } => {
                let store = self.stores.get(name).ok_or_else(|| {
                    RuleNotFoundError(
                        name.as_plain_symbol().to_string(),
                        name.as_plain_symbol().span,
                    )
                })?;
                Box::new(
                    store
                        .all_iter()?
                        .map(|t| t.try_into_tuple().map_err(Into::into)),
                )
            }
            MagicFixedRuleRuleArg::Stored { name, as_of, .. } => {
                self.stored.stored_scan_all(name, *as_of)?
            }
        })
    }
    /// Iterate the relation with the given single-value prefix
    pub fn prefix_iter(&self, prefix: &DataValue) -> Result<TupleIter<'a>> {
        Ok(match self.arg_manifest {
            MagicFixedRuleRuleArg::InMem { name, .. } => {
                let store = self.stores.get(name).ok_or_else(|| {
                    RuleNotFoundError(
                        name.as_plain_symbol().to_string(),
                        name.as_plain_symbol().span,
                    )
                })?;
                let t: Tuple = Tuple::from_vec(vec![prefix.clone()]);
                Box::new(
                    store
                        .prefix_iter(&t)?
                        .map(|t| t.try_into_tuple().map_err(Into::into)),
                )
            }
            MagicFixedRuleRuleArg::Stored { name, as_of, .. } => {
                self.stored.stored_scan_prefix(name, prefix, *as_of)?
            }
        })
    }
    /// Get the source span of the input relation. Useful for generating informative error messages.
    pub fn span(&self) -> SourceSpan {
        self.arg_manifest.span()
    }

    /// Convert the input relation into a directed graph.
    /// If `undirected` is true, then each edge in the input relation is treated as a pair
    /// of edges, one for each direction.
    pub(crate) fn as_directed_graph(
        &self,
        undirected: bool,
    ) -> Result<(DirectedCsrGraph, Vec<DataValue>, BTreeMap<DataValue, u32>)> {
        as_directed_graph(self, undirected)
    }

    /// Convert the input relation into a directed weighted graph, the
    /// weight taken from the third column (`1.0` when absent).
    pub(crate) fn as_directed_weighted_graph(
        &self,
        undirected: bool,
        allow_negative_weights: bool,
    ) -> Result<(
        DirectedCsrGraph<f64>,
        Vec<DataValue>,
        BTreeMap<DataValue, u32>,
    )> {
        let weight_span = match self.arg_manifest.bindings().get(2) {
            Some(s) => s.span,
            None => self.span(),
        };
        as_directed_weighted_graph(self, undirected, allow_negative_weights, weight_span)
    }
}

impl<'a> FixedRulePayload<'a> {
    /// Get the total number of input relations.
    pub fn inputs_count(&self) -> usize {
        self.manifest.relations_count()
    }
    /// Get the input relation at `idx`.
    pub fn get_input(&self, idx: usize) -> Result<FixedRuleInputRelation<'a>> {
        let arg_manifest = self.manifest.relation(idx)?;
        Ok(FixedRuleInputRelation {
            arg_manifest,
            stores: self.stores,
            stored: self.stored,
        })
    }
    /// Get the name of the current fixed rule
    pub fn name(&self) -> &str {
        &self.manifest.fixed_handle.name
    }
    /// Get the source span of the payloads. Useful for generating informative errors.
    pub fn span(&self) -> SourceSpan {
        self.manifest.span
    }
    /// Extract an expression option
    pub fn expr_option(&self, name: &str, default: Option<Expr>) -> Result<Expr> {
        match self.manifest.options.get(name) {
            Some(ex) => Ok(ex.clone()),
            None => match default {
                Some(ex) => Ok(ex),
                None => Err(FixedRuleOptionNotFoundError {
                    name: Symbol::new(name, self.manifest.span),
                    span: self.manifest.span,
                    rule_name: self.manifest.fixed_handle.name.clone(),
                }
                .into()),
            },
        }
    }

    /// Extract a string option
    pub fn string_option(&self, name: &str, default: Option<&str>) -> Result<String> {
        match self.manifest.options.get(name) {
            Some(ex) => match ex.clone().eval_to_const()? {
                DataValue::Str(s) => Ok(s),
                data_value_any!() => Err(WrongFixedRuleOptionError {
                    name: Symbol::new(name, ex.span()),
                    span: ex.span(),
                    rule_name: self.manifest.fixed_handle.name.clone(),
                    help: WrongFixedRuleOptionHelp::StringRequired,
                }
                .into()),
            },
            None => match default {
                None => Err(FixedRuleOptionNotFoundError {
                    name: Symbol::new(name, self.manifest.span),
                    span: self.manifest.span,
                    rule_name: self.manifest.fixed_handle.name.clone(),
                }
                .into()),
                Some(s) => Ok(s.to_string()),
            },
        }
    }

    /// Get the source span of the named option. Useful for generating informative error messages.
    pub fn option_span(&self, name: &str) -> Result<SourceSpan> {
        match self.manifest.options.get(name) {
            None => Err(FixedRuleOptionNotFoundError {
                name: Symbol::new(name, self.manifest.span),
                span: self.manifest.span,
                rule_name: self.manifest.fixed_handle.name.clone(),
            }
            .into()),
            Some(v) => Ok(v.span()),
        }
    }
    /// Shared option lookup: missing→default/err; present→`extract` or typed refuse.
    fn typed_option<T>(
        &self,
        name: &str,
        default: Option<T>,
        extract: impl FnOnce(DataValue) -> Option<T>,
        help: WrongFixedRuleOptionHelp,
    ) -> Result<T> {
        match self.manifest.options.get(name) {
            Some(v) => match v.clone().eval_to_const() {
                Ok(val) => match extract(val) {
                    Some(t) => Ok(t),
                    None => Err(WrongFixedRuleOptionError {
                        name: Symbol::new(name, v.span()),
                        span: v.span(),
                        rule_name: self.manifest.fixed_handle.name.clone(),
                        help,
                    }
                    .into()),
                },
                Err(_) => Err(WrongFixedRuleOptionError {
                    name: Symbol::new(name, v.span()),
                    span: v.span(),
                    rule_name: self.manifest.fixed_handle.name.clone(),
                    help,
                }
                .into()),
            },
            None => match default {
                Some(v) => Ok(v),
                None => Err(FixedRuleOptionNotFoundError {
                    name: Symbol::new(name, self.manifest.span),
                    span: self.manifest.span,
                    rule_name: self.manifest.fixed_handle.name.clone(),
                }
                .into()),
            },
        }
    }

    /// Extract an integer option
    pub fn integer_option(&self, name: &str, default: Option<i64>) -> Result<i64> {
        // Non-integral Num → NotFound (historical seat). Missing / wrong-type
        // share [`Self::typed_option`]; this arm is only the integral Num door.
        match self.manifest.options.get(name) {
            Some(v) => match v.clone().eval_to_const() {
                Ok(DataValue::Num(n)) => match n.as_int() {
                    Some(i) => Ok(i),
                    None => Err(FixedRuleOptionNotFoundError {
                        name: Symbol::new(name, self.manifest.span),
                        span: self.manifest.span,
                        rule_name: self.manifest.fixed_handle.name.clone(),
                    }
                    .into()),
                },
                Ok(_) | Err(_) => self.typed_option(
                    name,
                    None,
                    |_| None,
                    WrongFixedRuleOptionHelp::IntegerRequired,
                ),
            },
            None => self.typed_option(
                name,
                default,
                |_| None,
                WrongFixedRuleOptionHelp::IntegerRequired,
            ),
        }
    }
    fn bounded_usize_option(
        &self,
        name: &str,
        default: Option<usize>,
        bound_ok: impl FnOnce(i64) -> bool,
        bound_help: WrongFixedRuleOptionHelp,
        fits_help: WrongFixedRuleOptionHelp,
    ) -> Result<usize> {
        let default_i64 = match default {
            None => None,
            Some(d) => Some(crate::rules::convert::i64_from_usize(d)?),
        };
        let i = self.integer_option(name, default_i64)?;
        ensure!(
            bound_ok(i),
            WrongFixedRuleOptionError {
                name: Symbol::new(name, self.option_span(name)?),
                span: self.option_span(name)?,
                rule_name: self.manifest.fixed_handle.name.clone(),
                help: bound_help,
            }
        );
        let span = match self.option_span(name) {
            Ok(s) => s,
            // Option took its default — label the refusal with the rule span.
            Err(_) => self.manifest.span,
        };
        usize::try_from(i).map_err(|_| {
            WrongFixedRuleOptionError {
                name: Symbol::new(name, span),
                span,
                rule_name: self.manifest.fixed_handle.name.clone(),
                help: fits_help,
            }
            .into()
        })
    }

    /// Extract a positive integer option
    pub fn pos_integer_option(&self, name: &str, default: Option<usize>) -> Result<usize> {
        self.bounded_usize_option(
            name,
            default,
            |i| i > 0,
            WrongFixedRuleOptionHelp::PositiveIntegerRequired,
            WrongFixedRuleOptionHelp::PositiveIntegerFitsUsizeRequired,
        )
    }
    /// Extract a non-negative integer option
    pub fn non_neg_integer_option(&self, name: &str, default: Option<usize>) -> Result<usize> {
        self.bounded_usize_option(
            name,
            default,
            |i| i >= 0,
            WrongFixedRuleOptionHelp::NonNegIntegerRequired,
            WrongFixedRuleOptionHelp::NonNegIntegerFitsUsizeRequired,
        )
    }
    /// Extract a floating point option
    pub fn float_option(&self, name: &str, default: Option<f64>) -> Result<f64> {
        self.typed_option(
            name,
            default,
            |val| match val {
                DataValue::Num(n) => Some(n.to_f64()),
                data_value_any!() => None,
            },
            WrongFixedRuleOptionHelp::FloatRequired,
        )
    }
    /// Extract a floating point option between 0. and 1.
    pub fn unit_interval_option(&self, name: &str, default: Option<f64>) -> Result<f64> {
        let f = self.float_option(name, default)?;
        ensure!(
            (0. ..=1.).contains(&f),
            WrongFixedRuleOptionError {
                name: Symbol::new(name, self.option_span(name)?),
                span: self.option_span(name)?,
                rule_name: self.manifest.fixed_handle.name.clone(),
                help: WrongFixedRuleOptionHelp::UnitIntervalRequired,
            }
        );
        Ok(f)
    }
    /// Extract a boolean option
    pub fn bool_option(&self, name: &str, default: Option<bool>) -> Result<bool> {
        self.typed_option(
            name,
            default,
            |val| match val {
                DataValue::Bool(b) => Some(b),
                data_value_any!() => None,
            },
            WrongFixedRuleOptionHelp::BoolRequired,
        )
    }
}

// ─────────────────────────────────────────────────────────────────────────
// The arity-branded output writer
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Error, Diagnostic)]
#[error("Fixed rule declared arity {declared} but produced a row of width {got}")]
#[diagnostic(code(algo::output_arity_mismatch))]
#[diagnostic(help(
    "The arity a fixed rule declares (its `arity` implementation) is a \
     contract: every row it emits must have exactly that width"
))]
pub(crate) struct FixedRuleOutputArityMismatch {
    declared: usize,
    got: usize,
    #[label]
    span: SourceSpan,
}

/// The output relation a fixed rule fills, branded with the arity the rule
/// declared: every [`Self::put`] is width-checked, so a rule that lies
/// about its arity is refused at the first wrong row instead of feeding
/// mis-shaped tuples into downstream joins.
pub struct FixedRuleOutput {
    store: RegularTempStore,
    arity: usize,
    span: SourceSpan,
    /// The mid-run derived-tuple guard, when the query armed a
    /// `derived_tuple_ceiling`. `None` leaves the writer unbounded (only the
    /// epoch barrier that merges this output then checks the ceiling).
    guard: Option<OutputSpendGuard>,
}

/// A fixed rule's mid-run spend guard — the fixed-rule twin of the per-rule
/// mid-epoch check in `exec::fixpoint::eval::InterruptTicker`.
struct OutputSpendGuard {
    /// Globally admitted total as of this stratum's epoch-0 barrier.
    baseline: u64,
    ceiling: u64,
    /// Remaining puts until the next ceiling check (P097: proven stride).
    stride_left: OutputStrideLeft,
}

/// Rows a fixed rule may `put` between mid-run ceiling checks — harmonized
/// with `exec::fixpoint::eval`'s `INTERRUPT_STRIDE`. Non-zero by construction.
const OUTPUT_STRIDE: u32 = 64;

/// Countdown to the next mid-run ceiling check (P097).
struct OutputStrideLeft(u32);

impl OutputStrideLeft {
    fn fresh() -> Self {
        Self(OUTPUT_STRIDE)
    }

    /// Tick one put; returns true when a ceiling check is due.
    fn tick(&mut self) -> bool {
        // INVARIANT(output_stride): `OUTPUT_STRIDE >= 1`, so reset never
        // installs a zero countdown that would skip checks forever.
        self.0 -= 1;
        if self.0 == 0 {
            self.0 = OUTPUT_STRIDE;
            true
        } else {
            false
        }
    }
}

impl FixedRuleOutput {
    /// Brand a fresh output store with the rule's declared arity and
    /// the application's span for error labeling.
    pub(crate) fn new(arity: usize, span: SourceSpan) -> Self {
        Self {
            store: RegularTempStore::new(),
            arity,
            span,
            guard: None,
        }
    }

    /// As [`Self::new`], but armed with the query's derived-tuple ceiling so
    /// the writer refuses mid-run once `baseline + rows > ceiling`.
    pub(crate) fn new_budgeted(
        arity: usize,
        span: SourceSpan,
        baseline: u64,
        ceiling: Option<u64>,
    ) -> Self {
        Self {
            store: RegularTempStore::new(),
            arity,
            span,
            guard: ceiling.map(|ceiling| OutputSpendGuard {
                baseline,
                ceiling,
                stride_left: OutputStrideLeft::fresh(),
            }),
        }
    }

    /// Add a row to the output relation. Refuses, typed, if the row's
    /// width is not the declared arity, or — when budgeted — if this rule's
    /// output has crossed the derived-tuple ceiling mid-run.
    pub fn put(&mut self, tuple: Tuple) -> Result<()> {
        ensure!(
            tuple.len() == self.arity,
            FixedRuleOutputArityMismatch {
                declared: self.arity,
                got: tuple.len(),
                span: self.span,
            }
        );
        if let Some(guard) = self.guard.as_mut()
            && guard.stride_left.tick()
        {
            let spent = match guard
                .baseline
                .checked_add(crate::rules::convert::u64_from_usize(self.store.len())?)
            {
                Some(v) => v,
                None => {
                    // Published ceiling for this overflow.
                    u64::MAX
                },
            };
            if spent > guard.ceiling {
                return Err(LimitExceeded {
                    dimension: BudgetDimension::InFlightDerivations,
                    spent,
                    ceiling: guard.ceiling,
                    rule: None,
                    span: Some(self.span),
                }
                .into());
            }
        }
        self.store.put(tuple);
        Ok(())
    }

    /// Surrender the filled store to the evaluator.
    pub(crate) fn into_store(self) -> RegularTempStore {
        self.store
    }
}

#[cfg(test)]
mod fixed_rule_output_budget_tests {
    use super::*;
    use kyzo_model::value::DataValue;

    use miette::{IntoDiagnostic, Result, miette};
    fn row(i: i64) -> Tuple {
        Tuple::from_vec(vec![DataValue::from(i), DataValue::from(i)])
    }

    #[test]
    fn budgeted_output_refuses_mid_run() -> Result<()> {
        let mut out = FixedRuleOutput::new_budgeted(2, SourceSpan(3, 5), 0, Some(10));
        let mut err = None;
        for i in 0..1_000 {
            if let Err(e) = out.put(row(i)) {
                err = Some(e);
                break;
            }
        }
        let err = err.ok_or_else(|| miette!("must refuse mid-run"))?;
        let refusal: &LimitExceeded = err
            .downcast_ref()
            .ok_or_else(|| miette!("typed LimitExceeded"))?;
        assert_eq!(refusal.dimension, BudgetDimension::InFlightDerivations);
        assert_eq!(refusal.ceiling, 10);
        assert!(refusal.spent > 10);
        assert!(refusal.spent <= 10 + u64::from(OUTPUT_STRIDE));
        assert_eq!(refusal.span, Some(SourceSpan(3, 5)));
        Ok(())
    }

    #[test]
    fn small_and_unbudgeted_outputs_never_refuse() -> Result<()> {
        let mut small = FixedRuleOutput::new_budgeted(2, SourceSpan(0, 0), 0, Some(3));
        for i in 0..5 {
            small.put(row(i))?;
        }
        let mut unbudgeted = FixedRuleOutput::new(2, SourceSpan(0, 0));
        for i in 0..500 {
            unbudgeted.put(row(i))?;
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// The trait
// ─────────────────────────────────────────────────────────────────────────

/// Trait for an implementation of an algorithm or a utility
pub trait FixedRule: Send + Sync {
    /// Consuming option normalize (P086). Called once before `arity`/`run`.
    /// Returns the (possibly rewritten) options bag; the default is identity.
    fn init_options(
        &self,
        options: FixedRuleOptions,
        _span: SourceSpan,
    ) -> Result<FixedRuleOptions> {
        Ok(options)
    }
    /// You must return the row width of the returned relation and it must be accurate.
    /// This function may be called multiple times.
    fn arity(
        &self,
        options: &FixedRuleOptions,
        rule_head: &[Symbol],
        span: SourceSpan,
    ) -> Result<usize>;
    /// You should implement the logic of your algorithm/utility in this function.
    /// The outputs are written to `out` (width-checked against the arity
    /// you declared). You should call `cancel.check()?` periodically —
    /// at least once per unit of unbounded work — so user-initiated
    /// termination (and, later, budget deadlines) can take effect.
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()>;
}

// ─────────────────────────────────────────────────────────────────────────
// SimpleFixedRule — named body owner only (P083: no Fn / dyn Fn)
// ─────────────────────────────────────────────────────────────────────────

/// Named body for a simple fixed rule. Prefer a named [`FixedRule`] impl for
/// production algorithms; this trait is the reduced door for host rules that
/// already work over realized [`NamedRows`]. Bodies are named types only —
/// there is no `Fn` blanket impl (P083).
pub trait SimpleRuleBody: Send + Sync + 'static {
    fn apply(
        &self,
        inputs: Vec<NamedRows>,
        options: BTreeMap<String, DataValue>,
    ) -> Result<NamedRows>;
}

/// Channel-backed simple-rule body: one named owner (P083).
///
/// Public because [`SimpleFixedRule::rule_with_channel`] is a sealed host
/// door that returns `impl FixedRule` backed by this type — the concrete
/// name must be visible at the crate root for that opaque return.
pub struct ChannelRuleBody {
    db2app: SyncSender<(
        Vec<NamedRows>,
        BTreeMap<String, DataValue>,
        SyncSender<Result<NamedRows>>,
    )>,
}

impl SimpleRuleBody for ChannelRuleBody {
    fn apply(
        &self,
        inputs: Vec<NamedRows>,
        options: BTreeMap<String, DataValue>,
    ) -> Result<NamedRows> {
        let (app2db_sender, app2db_receiver) = sync_channel(0);
        self.db2app
            .send((inputs, options, app2db_sender))
            .map_err(|_| DisconnectedChannelRule)?;
        app2db_receiver
            .recv()
            .map_err(|_| DisconnectedChannelRule)?
    }
}

/// Named body: emit no rows under the declared headers (empty relation).
pub struct EmptyNamedRowsBody;

impl SimpleRuleBody for EmptyNamedRowsBody {
    fn apply(
        &self,
        _inputs: Vec<NamedRows>,
        _options: BTreeMap<String, DataValue>,
    ) -> Result<NamedRows> {
        Ok(NamedRows::try_new(vec![], vec![])?)
    }
}

/// Named body: forward the first input relation unchanged.
pub struct IdentityNamedRowsBody;

impl SimpleRuleBody for IdentityNamedRowsBody {
    fn apply(
        &self,
        inputs: Vec<NamedRows>,
        _options: BTreeMap<String, DataValue>,
    ) -> Result<NamedRows> {
        let input = inputs
            .into_iter()
            .next()
            .ok_or_else(|| miette::miette!("IdentityNamedRowsBody requires one input relation"))?;
        let (headers, rows, next) = input.into_parts();
        Ok(NamedRows::try_new(headers, rows)?.with_next(next))
    }
}

/// Named body: deliberately emit a one-column row under a mismatched
/// arity declaration — used to pin the universal writer check.
pub struct MismatchedArityBody;

impl SimpleRuleBody for MismatchedArityBody {
    fn apply(
        &self,
        _inputs: Vec<NamedRows>,
        _options: BTreeMap<String, DataValue>,
    ) -> Result<NamedRows> {
        Ok(NamedRows::try_new(
            vec!["a".to_string()],
            vec![Tuple::from_vec(vec![DataValue::from(1i64)])],
        )?)
    }
}

/// Simple wrapper for custom fixed rule. You have less control than implementing [FixedRule] directly,
/// but implementation is simpler. The body is a named [`SimpleRuleBody`] —
/// never a closure / `Fn` owner (P083).
pub struct SimpleFixedRule<B> {
    return_arity: usize,
    body: B,
}

impl<B: SimpleRuleBody> SimpleFixedRule<B> {
    /// Construct a SimpleFixedRule.
    ///
    /// * `return_arity`: The return arity of this rule.
    /// * `body`: A named [`SimpleRuleBody`] (not a closure).
    pub fn new(return_arity: usize, body: B) -> Self {
        Self { return_arity, body }
    }
}

impl SimpleFixedRule<ChannelRuleBody> {
    /// Construct a SimpleFixedRule that uses channels for communication.
    pub fn rule_with_channel(
        return_arity: usize,
    ) -> (
        impl FixedRule,
        Receiver<(
            Vec<NamedRows>,
            BTreeMap<String, DataValue>,
            SyncSender<Result<NamedRows>>,
        )>,
    ) {
        let (db2app_sender, db2app_receiver) = sync_channel(0);
        (
            SimpleFixedRule {
                return_arity,
                body: ChannelRuleBody {
                    db2app: db2app_sender,
                },
            },
            db2app_receiver,
        )
    }
}

#[derive(Debug, Error, Diagnostic)]
#[error("The channel backing this custom fixed rule has disconnected")]
#[diagnostic(code(algo::channel_rule_disconnected))]
struct DisconnectedChannelRule;

impl<B: SimpleRuleBody> FixedRule for SimpleFixedRule<B> {
    fn arity(
        &self,
        _options: &FixedRuleOptions,
        _rule_head: &[Symbol],
        _span: SourceSpan,
    ) -> Result<usize> {
        Ok(self.return_arity)
    }

    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        _cancel: CancelFlag,
    ) -> Result<()> {
        let options: BTreeMap<_, _> = payload
            .manifest
            .options
            .iter()
            .map(|(k, v)| -> Result<_> {
                let val = v.clone().eval_to_const()?;
                Ok((k.to_string(), val))
            })
            .try_collect()?;
        let input_arity = payload.manifest.rule_args.len();
        let inputs: Vec<_> = (0..input_arity)
            .map(|i| -> Result<_> {
                // INVARIANT(simple_input_index): `i < rule_args.len()`.
                let input = payload.get_input(i)?;
                let rows: Vec<_> = input.iter()?.try_collect()?;
                let mut headers = input
                    .arg_manifest
                    .bindings()
                    .iter()
                    .map(|s| s.name.to_string())
                    .collect_vec();
                let l = headers.len();
                let m = input.arity()?;
                for i in l..m {
                    headers.push(format!("_{i}"));
                }
                Ok(NamedRows::try_new(headers, rows)?)
            })
            .try_collect()?;
        let results: NamedRows = self.body.apply(inputs, options)?;
        for row in results {
            out.put(row)?;
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Handle, registry, errors
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Error, Diagnostic)]
#[error("Cannot determine arity for algo {0} since {1}")]
#[diagnostic(code(parser::no_algo_arity))]
pub(crate) struct CannotDetermineArity(
    pub(crate) String,
    pub(crate) String,
    #[label] pub(crate) SourceSpan,
);

#[derive(Error, Diagnostic, Debug)]
#[error("The requested fixed rule '{0}' is not found")]
#[diagnostic(code(parser::fixed_rule_not_found))]
pub(crate) struct FixedRuleNotFoundError(pub(crate) String, #[label] pub(crate) SourceSpan);

/// Seal a concrete fixed rule into the registry trait object — return-type
/// coercion, never an `as` cast.
#[inline]
pub(crate) fn seal_fixed_rule(rule: impl FixedRule + 'static) -> Arc<dyn FixedRule> {
    Arc::new(rule)
}

/// The built-in fixed rules: every graph algorithm in `rules/algo` and
/// every utility in `rules/io`.
pub(crate) static DEFAULT_FIXED_RULES: LazyLock<BTreeMap<String, Arc<dyn FixedRule>>> =
    LazyLock::new(|| {
        BTreeMap::from([
            (
                "ClusteringCoefficients".to_string(),
                seal_fixed_rule(ClusteringCoefficients),
            ),
            (
                "DegreeCentrality".to_string(),
                seal_fixed_rule(DegreeCentrality),
            ),
            (
                "ClosenessCentrality".to_string(),
                seal_fixed_rule(ClosenessCentrality),
            ),
            (
                "BetweennessCentrality".to_string(),
                seal_fixed_rule(BetweennessCentrality),
            ),
            (
                "DepthFirstSearch".to_string(),
                seal_fixed_rule(Dfs),
            ),
            ("DFS".to_string(), seal_fixed_rule(Dfs)),
            (
                "BreadthFirstSearch".to_string(),
                seal_fixed_rule(Bfs),
            ),
            ("BFS".to_string(), seal_fixed_rule(Bfs)),
            (
                "ShortestPathBFS".to_string(),
                seal_fixed_rule(ShortestPathBFS),
            ),
            (
                "ShortestPathDijkstra".to_string(),
                seal_fixed_rule(ShortestPathDijkstra),
            ),
            (
                "ShortestPathAStar".to_string(),
                seal_fixed_rule(ShortestPathAStar),
            ),
            (
                "KShortestPathYen".to_string(),
                seal_fixed_rule(KShortestPathYen),
            ),
            (
                "MinimumSpanningTreePrim".to_string(),
                seal_fixed_rule(MinimumSpanningTreePrim),
            ),
            (
                "MinimumSpanningForestKruskal".to_string(),
                seal_fixed_rule(MinimumSpanningForestKruskal),
            ),
            (
                "TopSort".to_string(),
                seal_fixed_rule(TopSort),
            ),
            (
                "ConnectedComponents".to_string(),
                seal_fixed_rule(StronglyConnectedComponent::new(false)),
            ),
            (
                "StronglyConnectedComponents".to_string(),
                seal_fixed_rule(StronglyConnectedComponent::new(true)),
            ),
            (
                "SCC".to_string(),
                seal_fixed_rule(StronglyConnectedComponent::new(true)),
            ),
            (
                "PageRank".to_string(),
                seal_fixed_rule(PageRank),
            ),
            (
                "KCoreDecomposition".to_string(),
                seal_fixed_rule(KCoreDecomposition),
            ),
            (
                "MaxFlow".to_string(),
                seal_fixed_rule(MaxFlow),
            ),
            (
                "MaximalCliques".to_string(),
                seal_fixed_rule(MaximalCliques),
            ),
            (
                "CommunityDetectionLouvain".to_string(),
                seal_fixed_rule(CommunityDetectionLouvain),
            ),
            (
                "LabelPropagation".to_string(),
                seal_fixed_rule(LabelPropagation),
            ),
            (
                "RandomWalk".to_string(),
                seal_fixed_rule(RandomWalk),
            ),
            (
                "ReorderSort".to_string(),
                seal_fixed_rule(ReorderSort),
            ),
            (
                "JsonReader".to_string(),
                seal_fixed_rule(JsonReader),
            ),
            (
                "CsvReader".to_string(),
                seal_fixed_rule(CsvReader),
            ),
            (
                "Constant".to_string(),
                seal_fixed_rule(Constant),
            ),
        ])
    });

#[derive(Error, Diagnostic, Debug)]
#[error("The requested rule '{0}' cannot be found")]
#[diagnostic(code(algo::rule_not_found))]
struct RuleNotFoundError(String, #[label] SourceSpan);

#[derive(Error, Diagnostic, Debug)]
#[error("Required node with key {missing:?} not found")]
#[diagnostic(code(algo::node_with_key_not_found))]
#[diagnostic(help(
    "The relation is interpreted as a relation of nodes, but the required key is missing"
))]
pub(crate) struct NodeNotFoundError {
    pub(crate) missing: DataValue,
    #[label]
    pub(crate) span: SourceSpan,
}

/// Typed refusal when a fixed-rule internal proof fails — sealed options
/// or buffered state the rule already constructed, never a user-input shape
/// error (those are refused earlier at option/input boundaries).
#[derive(Debug, Error, Diagnostic)]
#[error("Fixed-rule invariant violated: {invariant}")]
#[diagnostic(code(algo::fixed_rule_invariant_violation))]
#[diagnostic(help(
    "The fixed rule reached an internal state its proofs rule out — \
     likely a bug in option sealing or the algorithm"
))]
pub(crate) struct FixedRuleInvariantError {
    invariant: &'static str,
}

impl FixedRuleInvariantError {
    pub(crate) fn refuse(invariant: &'static str) -> miette::Report {
        Self { invariant }.into()
    }
}

/// Typed refusal when a graph algorithm's internal proof fails — corrupt
/// graph state or a broken algorithm invariant, never a user input shape
/// error (those are refused earlier at the input boundary).
#[derive(Debug, Error, Diagnostic)]
#[error("Graph algorithm invariant violated: {invariant}")]
#[diagnostic(code(algo::graph_invariant_violation))]
#[diagnostic(help(
    "The fixed-rule graph algorithm reached an internal state its proofs \
     rule out — likely corrupt graph data or a bug in the algorithm"
))]
pub(crate) struct GraphAlgorithmInvariantError {
    invariant: &'static str,
}

impl GraphAlgorithmInvariantError {
    pub(crate) fn refuse(invariant: &'static str) -> miette::Report {
        Self { invariant }.into()
    }
}

/// First column of an owned tuple (consumes the head value).
pub(crate) fn tuple_into_first_column(tuple: Tuple) -> Result<DataValue> {
    tuple
        .into_iter()
        .next()
        .ok_or_else(|| GraphAlgorithmInvariantError::refuse("tuple_first_column"))
}

/// Dense node id → interned value; `indices` has one entry per graph node.
pub(crate) fn graph_node_value(indices: &[DataValue], node: u32) -> Result<&DataValue> {
    indices
        .get(crate::rules::convert::usize_from_u32(node))
        .ok_or_else(|| GraphAlgorithmInvariantError::refuse("graph_node_index"))
}

/// Backtrace predecessor for route reconstruction.
pub(crate) fn backtrace_predecessor(
    backtrace: &BTreeMap<DataValue, DataValue>,
    current: &DataValue,
    invariant: &'static str,
) -> Result<DataValue> {
    backtrace
        .get(current)
        .cloned()
        .ok_or_else(|| GraphAlgorithmInvariantError::refuse(invariant))
}

/// Shared BFS/DFS payload admit — ONE seat for conditioned traversal fixed rules.
pub(crate) struct ConditionedTraversal<'a> {
    pub edges: FixedRuleInputRelation<'a>,
    pub nodes: FixedRuleInputRelation<'a>,
    pub starting_nodes: FixedRuleInputRelation<'a>,
    pub limit: usize,
    pub condition: Expr,
    pub skip_query_nodes: bool,
}

/// Admit edges/nodes/starts + condition/limit options for BFS and DFS.
pub(crate) fn admit_conditioned_traversal(
    payload: FixedRulePayload<'_>,
) -> Result<ConditionedTraversal<'_>> {
    let edges = payload.get_input(0)?.ensure_min_len(2)?;
    let nodes = payload.get_input(1)?;
    let starting_nodes = if payload.inputs_count() > 2 {
        payload.get_input(2)?
    } else {
        nodes
    }
    .ensure_min_len(1)?;
    let limit = payload.pos_integer_option("limit", Some(1))?;
    let mut condition = payload.expr_option("condition", None)?;
    let binding_map = nodes.get_binding_map(0);
    condition.fill_binding_indices(&binding_map)?;
    let binding_indices = condition.binding_indices()?;
    let skip_query_nodes = binding_indices.is_subset(&BTreeSet::from([0]));
    Ok(ConditionedTraversal {
        edges,
        nodes,
        starting_nodes,
        limit,
        condition,
        skip_query_nodes,
    })
}

/// Resolve a traversal candidate to its node tuple — ONE seat for BFS/DFS.
pub(crate) fn traversal_node_tuple(
    nodes: FixedRuleInputRelation<'_>,
    candidate: &DataValue,
    skip_query_nodes: bool,
    missing: &DataValue,
) -> Result<Tuple> {
    if skip_query_nodes {
        return Ok(Tuple::from_vec(vec![candidate.clone()]));
    }
    nodes
        .prefix_iter(candidate)?
        .next()
        .ok_or_else(|| NodeNotFoundError {
            missing: missing.clone(),
            span: nodes.span(),
        })?
}

/// Emit `(start, end, route)` rows from a backtrace — ONE seat for BFS/DFS.
/// When `cancel` is set, poll after each row (DFS emit path).
pub(crate) fn emit_backtrace_routes(
    found: Vec<(DataValue, DataValue)>,
    backtrace: &BTreeMap<DataValue, DataValue>,
    out: &mut FixedRuleOutput,
    pred_invariant: &'static str,
    cancel: Option<&CancelFlag>,
) -> Result<()> {
    for (starting, ending) in found {
        let mut route = vec![];
        let mut current = ending.clone();
        while current != starting {
            route.push(current.clone());
            current = backtrace_predecessor(backtrace, &current, pred_invariant)?;
        }
        route.push(starting.clone());
        route.reverse();
        out.put(Tuple::from_vec(vec![
            starting,
            ending,
            DataValue::List(route),
        ]))?;
        if let Some(flag) = cancel {
            flag.check()?;
        }
    }
    Ok(())
}

/// Predecessor in a dense `Option<u32>` table (Dijkstra path walk).
pub(crate) fn path_predecessor(
    back_pointers: &[Option<u32>],
    current: u32,
    invariant: &'static str,
) -> Result<u32> {
    back_pointers[crate::rules::convert::usize_from_u32(current)]
        .ok_or_else(|| GraphAlgorithmInvariantError::refuse(invariant))
}

/// Edmonds–Karp BFS parent on the augmenting path.
pub(crate) fn ek_bfs_parent(
    prev: &[Option<(u32, usize)>],
    node: u32,
    invariant: &'static str,
) -> Result<(u32, usize)> {
    prev[crate::rules::convert::usize_from_u32(node)]
        .ok_or_else(|| GraphAlgorithmInvariantError::refuse(invariant))
}

/// The sole element of a set whose length is already known to be 1.
pub(crate) fn btree_set_only_element(set: &BTreeSet<u32>, invariant: &'static str) -> Result<u32> {
    set.iter()
        .copied()
        .next()
        .ok_or_else(|| GraphAlgorithmInvariantError::refuse(invariant))
}

#[derive(Error, Diagnostic, Debug)]
#[error("Unacceptable value {0:?} encountered")]
#[diagnostic(code(algo::unacceptable_value))]
pub(crate) struct BadExprValueError(
    pub(crate) DataValue,
    #[label] pub(crate) SourceSpan,
    #[help] pub(crate) String,
);

impl MagicFixedRuleRuleArg {
    pub(crate) fn arity(
        &self,
        stores: &BTreeMap<MagicSymbol, EpochStore>,
        stored: &dyn StoredInputSource,
    ) -> Result<usize> {
        Ok(match self {
            MagicFixedRuleRuleArg::InMem { name, .. } => {
                let store = stores.get(name).ok_or_else(|| {
                    RuleNotFoundError(
                        name.as_plain_symbol().to_string(),
                        name.as_plain_symbol().span,
                    )
                })?;
                store.arity
            }
            MagicFixedRuleRuleArg::Stored { name, .. } => stored.stored_arity(name)?,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────
// The fixed-rule evaluation adapter
// ─────────────────────────────────────────────────────────────────────────

/// Bridges one `MagicFixedRuleApply` to `FixedRule::run` at evaluation time.
/// It assembles the payload (in-memory rule inputs from the epoch stores,
/// stored-relation inputs through a [`StoredInputSource`]), brands the output
/// store with the manifest arity (never a caller-supplied one), and shares the
/// budget's cancel poll as the rule's [`CancelFlag`] so a cancelled query stops
/// the rule too. This is the concrete `F` that `bind_for_eval`'s `make_fixed`
/// factory produces — the seam that lets a stored/derived query APPLY a fixed
/// rule (including the `Constant` rule behind every `<- [[…]]` inline datum).
///
/// `S` is the session read surface (production: `SessionView`); rules never
/// import the concrete session type — only the [`StoredInputSource`] seam.
pub(crate) struct SessionFixedRule<'a, S> {
    apply: &'a MagicFixedRuleApply,
    view: S,
    cancel: CancelFlag,
}

impl<'a, S> SessionFixedRule<'a, S> {
    pub(crate) fn new(apply: &'a MagicFixedRuleApply, view: S, cancel: CancelFlag) -> Self {
        Self {
            apply,
            view,
            cancel,
        }
    }
}

impl<S: StoredInputSource + Send + Sync> FixedRuleEval for SessionFixedRule<'_, S> {
    fn run(
        &self,
        stores: &BTreeMap<MagicSymbol, EpochStore>,
        out: &mut RegularTempStore,
        budget: &Budget,
        baseline: u64,
    ) -> Result<()> {
        let payload = FixedRulePayload {
            manifest: self.apply,
            stores,
            stored: &self.view,
        };
        // Armed with the query's derived-tuple ceiling and the true global
        // admitted total as of this stratum's epoch-0 barrier, so a
        // row-amplifying algorithm refuses mid-run — counting every prior
        // admission, not just this writer's own rows — instead of
        // materializing unbounded output.
        let mut output = FixedRuleOutput::new_budgeted(
            self.apply.arity,
            self.apply.span,
            baseline,
            budget.derived_tuple_ceiling(),
        );
        self.apply
            .fixed_impl
            .clone()
            .run(payload, &mut output, self.cancel.clone())?;
        // Replace eval's fresh epoch-0 store with the branded output wholesale.
        *out = output.into_store();
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Test harness (new in KyzoDB)
// ─────────────────────────────────────────────────────────────────────────

/// Runs fixed rules without a database: builds a [`MagicFixedRuleApply`]
/// over in-memory inputs, executes `run`, and returns the output rows in
/// canonical order. Stored-relation arguments are refused by the harness
/// double [`HarnessStoredClosed`] (not a production placeholder — P090).
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    use miette::{IntoDiagnostic, Result, miette};
    pub(crate) struct TestInput {
        pub(crate) bindings: Vec<&'static str>,
        pub(crate) rows: Vec<Tuple>,
        pub(crate) arity: usize,
    }

    impl TestInput {
        pub(crate) fn new(bindings: Vec<&'static str>, rows: Vec<Tuple>) -> Self {
            let arity = bindings.len();
            Self {
                bindings,
                rows,
                arity,
            }
        }
    }

    /// Lift a legacy SmartString-keyed options map into [`FixedRuleOptions`].
    pub(crate) fn opts_map(
        map: BTreeMap<SmartString<LazyCompact>, Expr>,
    ) -> Result<FixedRuleOptions> {
        FixedRuleOptions::from_entries(
            map.into_iter()
                .map(|(k, v)| (Symbol::new(k, SourceSpan::empty()), v)),
        )
        .into_diagnostic()
    }

    /// Empty options bag for harness call sites.
    pub(crate) fn empty_opts() -> FixedRuleOptions {
        FixedRuleOptions::empty()
    }

    /// Build options from string keys (known fixed-rule option names only).
    pub(crate) fn opts(pairs: &[(&str, Expr)]) -> Result<FixedRuleOptions> {
        FixedRuleOptions::from_entries(
            pairs
                .iter()
                .map(|(k, v)| (Symbol::new(*k, SourceSpan::empty()), v.clone())),
        )
        .into_diagnostic()
    }

    /// Test-only stored-input double: every stored read refuses. Not the
    /// demolished production `NoStoredInputs` seam (P090) — harness door only.
    pub(crate) struct HarnessStoredClosed;

    #[derive(Debug, Error, Diagnostic)]
    #[error("test harness has no stored relation '{name}'")]
    #[diagnostic(code(algo::harness_stored_closed))]
    struct HarnessStoredClosedError {
        name: String,
        #[label]
        span: SourceSpan,
    }

    impl HarnessStoredClosed {
        fn refuse<T>(&self, name: &Symbol) -> Result<T> {
            Err(HarnessStoredClosedError {
                name: name.to_string(),
                span: name.span,
            }
            .into())
        }
    }

    impl StoredInputSource for HarnessStoredClosed {
        fn stored_arity(&self, name: &Symbol) -> Result<usize> {
            self.refuse(name)
        }
        fn stored_scan_all<'a>(
            &'a self,
            name: &Symbol,
            _as_of: Option<AsOf>,
        ) -> Result<TupleIter<'a>> {
            self.refuse(name)
        }
        fn stored_scan_prefix<'a>(
            &'a self,
            name: &Symbol,
            _prefix: &DataValue,
            _as_of: Option<AsOf>,
        ) -> Result<TupleIter<'a>> {
            self.refuse(name)
        }
    }

    /// A fixed-rule invocation environment with its input relations already
    /// materialized. Building the in-memory stores is O(total input rows);
    /// splitting it out lets a test pay that cost once and then time only the
    /// algorithm body across several [`Self::run`] calls.
    pub(crate) struct PreparedFixedRule {
        stores: BTreeMap<MagicSymbol, EpochStore>,
        manifest: MagicFixedRuleApply,
        arity: usize,
    }

    /// Build the input stores and manifest for `rule` over `inputs`; the
    /// returned value runs the rule body without rebuilding them.
    pub(crate) fn prepare_fixed_rule(
        rule: &dyn FixedRule,
        inputs: Vec<TestInput>,
        mut options: FixedRuleOptions,
    ) -> Result<PreparedFixedRule> {
        let span = SourceSpan::empty();
        options = rule.init_options(options, span)?;
        let mut stores = BTreeMap::new();
        let mut rule_args = vec![];
        for (i, input) in inputs.into_iter().enumerate() {
            let name = MagicSymbol::Muggle {
                inner: Symbol::new(format!("_test_input_{i}"), span),
            };
            let mut fresh = RegularTempStore::new();
            for row in input.rows {
                fresh.put(row);
            }
            let mut store = EpochStore::new_normal(input.arity);
            store.merge_in(fresh.wrap(), &mut ())?;
            stores.insert(name.clone(), store);
            rule_args.push(MagicFixedRuleRuleArg::InMem {
                name,
                bindings: input
                    .bindings
                    .iter()
                    .map(|b| Symbol::new(*b, span))
                    .collect(),
                span,
            });
        }
        let arity = rule.arity(&options, &[], span)?;
        let manifest = MagicFixedRuleApply {
            fixed_handle: FixedRuleHandle::new("TestRule", span),
            rule_args,
            options,
            span,
            arity,
            fixed_impl: Arc::new(NeverRun),
        };
        Ok(PreparedFixedRule {
            stores,
            manifest,
            arity,
        })
    }

    impl PreparedFixedRule {
        /// Execute `run` against the prepared environment with a fresh
        /// output, returning the rows in canonical order.
        pub(crate) fn run(&self, rule: &dyn FixedRule, cancel: CancelFlag) -> Result<Vec<Tuple>> {
            let payload = FixedRulePayload {
                manifest: &self.manifest,
                stores: &self.stores,
                stored: &HarnessStoredClosed,
            };
            let mut out = FixedRuleOutput::new(self.arity, SourceSpan::empty());
            rule.run(payload, &mut out, cancel)?;
            let store = out.into_store().wrap();
            let mut collected = EpochStore::new_normal(self.arity);
            collected.merge_in(store, &mut ())?;
            Ok(collected
                .all_iter()?
                .map(TupleInIter::try_into_tuple)
                .collect::<Result<Vec<_>, _>>()?)
        }
    }

    pub(crate) fn run_fixed_rule(
        rule: &dyn FixedRule,
        inputs: Vec<TestInput>,
        options: FixedRuleOptions,
        cancel: CancelFlag,
    ) -> Result<Vec<Tuple>> {
        prepare_fixed_rule(rule, inputs, options)?.run(rule, cancel)
    }

    /// DETERMINISM seat: byte-identical results on a 1-thread rayon pool vs
    /// the default pool across repeated runs (copy_detector — one harness).
    pub(crate) fn assert_parallel_matches_single_thread(
        run: impl Fn() -> Result<Vec<Tuple>> + Send,
    ) -> Result<()> {
        let single = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .into_diagnostic()?;
        let seq = single.install(|| run())?;
        for _ in 0..8 {
            let par = run()?;
            assert_eq!(seq, par);
        }
        Ok(())
    }

    /// IO reader seam: URL/file fetch must refuse typed (copy_detector seat).
    pub(crate) fn assert_fetch_refuses_typed(
        rule: &dyn FixedRule,
        url: &str,
        mut options: BTreeMap<SmartString<LazyCompact>, Expr>,
        needle: &str,
    ) -> Result<()> {
        options.insert(
            SmartString::from("url"),
            Expr::Const {
                val: DataValue::from(url),
                span: SourceSpan::empty(),
            },
        );
        let err = run_fixed_rule(rule, vec![], opts_map(options)?, CancelFlag::inert()).unwrap_err();
        assert!(err.to_string().contains(needle), "{err}");
        Ok(())
    }

    /// Cancellation seat: armed flag must refuse before rows emit.
    pub(crate) fn assert_honors_cancel(
        rule: &dyn FixedRule,
        options: FixedRuleOptions,
    ) -> Result<()> {
        let (auth, flag) = CancelAuthority::arm();
        let Cancelled = auth.cancel();
        assert!(run_fixed_rule(rule, vec![], options, flag).is_err());
        Ok(())
    }

    /// Path graph v0—v{n-1} edge inputs — ONE seat for cancel-pin harnesses.
    pub(crate) fn path_edge_inputs(n: u32) -> Vec<TestInput> {
        let edges: Vec<Tuple> = match n.checked_sub(1) {
            Some(last) => (0..last)
                .map(|i| {
                    Tuple::from_vec(vec![
                        DataValue::from(format!("v{i}").as_str()),
                        DataValue::from(format!("v{}", i + 1).as_str()),
                    ])
                })
                .collect(),
            None => {
                let empty_edge_list = Vec::new();
                empty_edge_list
            }
        };
        vec![TestInput::new(vec!["fr", "to"], edges)]
    }

    /// Diamond a→{b,c}, b→d, c→d with nodes {a,b,c,d} and start a —
    /// ONE seat for BFS/DFS exact-route oracles (copy_detector).
    pub(crate) fn diamond_traversal_inputs() -> Vec<TestInput> {
        fn s(v: &str) -> DataValue {
            DataValue::from(v)
        }
        vec![
            TestInput::new(
                vec!["fr", "to"],
                vec![
                    Tuple::from_vec(vec![s("a"), s("b")]),
                    Tuple::from_vec(vec![s("a"), s("c")]),
                    Tuple::from_vec(vec![s("b"), s("d")]),
                    Tuple::from_vec(vec![s("c"), s("d")]),
                ],
            ),
            TestInput::new(
                vec!["id"],
                vec![
                    Tuple::from_vec(vec![s("a")]),
                    Tuple::from_vec(vec![s("b")]),
                    Tuple::from_vec(vec![s("c")]),
                    Tuple::from_vec(vec![s("d")]),
                ],
            ),
            TestInput::new(vec!["start"], vec![Tuple::from_vec(vec![s("a")])]),
        ]
    }

    /// `condition: true` + `limit` options bag for traversal fixed rules.
    pub(crate) fn condition_true_limit(limit: i64) -> Result<FixedRuleOptions> {
        opts_map(BTreeMap::from([
            (
                SmartString::from("condition"),
                Expr::Const {
                    val: DataValue::from(true),
                    span: SourceSpan::empty(),
                },
            ),
            (
                SmartString::from("limit"),
                Expr::Const {
                    val: DataValue::from(limit),
                    span: SourceSpan::empty(),
                },
            ),
        ]))
    }

    /// Diamond oracle runner — ONE seat for BFS/DFS exact-route harnesses.
    pub(crate) fn assert_diamond_traversal_routes(
        rule: &dyn FixedRule,
        want: &[(&str, &str, &[&str])],
    ) -> Result<()> {
        let got = run_fixed_rule(
            rule,
            diamond_traversal_inputs(),
            condition_true_limit(10)?,
            CancelFlag::inert(),
        )?;
        let want: Vec<Tuple> = want
            .iter()
            .map(|(start, end, route)| {
                Tuple::from_vec(vec![
                    DataValue::from(*start),
                    DataValue::from(*end),
                    DataValue::List(route.iter().map(|s| DataValue::from(*s)).collect()),
                ])
            })
            .collect();
        assert_eq!(got, want);
        Ok(())
    }

    /// A placeholder occupying `MagicFixedRuleApply::fixed_impl` in the
    /// harness (the payload never invokes it; the rule under test is
    /// driven directly).
    struct NeverRun;

    impl FixedRule for NeverRun {
        fn arity(
            &self,
            _options: &FixedRuleOptions,
            _rule_head: &[Symbol],
            _span: SourceSpan,
        ) -> Result<usize> {
            Ok(0)
        }
        fn run(
            &self,
            _payload: FixedRulePayload<'_>,
            _out: &mut FixedRuleOutput,
            _cancel: CancelFlag,
        ) -> Result<()> {
            Err(miette!("the test harness never runs its placeholder impl"))
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod par_try_map_tests {
    use super::*;

    use miette::{IntoDiagnostic, Result, miette};
    #[test]
    fn preserves_input_order() -> Result<()> {
        let got = par_try_map((0u32..1000).collect(), |i| Ok::<_, miette::Report>(i * 2));
        assert_eq!(got?, (0u32..1000).map(|i| i * 2).collect::<Vec<_>>());
        Ok(())
    }

    #[test]
    fn single_thread_matches_default_pool() -> Result<()> {
        // INVARIANT(test_hash_mix): golden-hash mul in a unit test; wrap is intentional.
        let f = |i: u32| {
            Ok::<_, miette::Report>((std::num::Wrapping(i) * std::num::Wrapping(2_654_435_761)).0)
        };
        let default = par_try_map((0u32..2000).collect(), f)?;
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .into_diagnostic()?;
        let single = pool.install(|| par_try_map((0u32..2000).collect(), f))?;
        assert_eq!(default, single);
        Ok(())
    }

    #[test]
    fn propagates_error() {
        let got: Result<Vec<u32>> = par_try_map((0u32..100).collect(), |i| {
            if i == 42 {
                Err(miette::miette!("boom"))
            } else {
                Ok(i)
            }
        });
        assert!(got.is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::{TestInput, empty_opts, opts_map, run_fixed_rule};
    use super::*;

    use miette::{IntoDiagnostic, Result, miette};
    fn s(v: &str) -> DataValue {
        DataValue::from(v)
    }

    #[test]
    fn output_writer_rejects_wrong_arity() -> Result<()> {
        let mut out = FixedRuleOutput::new(2, SourceSpan::empty());
        out.put(Tuple::from_vec(vec![s("a"), s("b")]))?;
        let err = out.put(Tuple::from_vec(vec![s("a")])).unwrap_err();
        assert!(err.to_string().contains("arity 2"), "{err}");
        let err = out
            .put(Tuple::from_vec(vec![s("a"), s("b"), s("c")]))
            .unwrap_err();
        assert!(err.to_string().contains("width 3"), "{err}");
        Ok(())
    }

    #[test]
    fn lying_rule_is_refused() {
        struct Liar;
        impl FixedRule for Liar {
            fn arity(&self, _o: &FixedRuleOptions, _h: &[Symbol], _s: SourceSpan) -> Result<usize> {
                Ok(3)
            }
            fn run(
                &self,
                _payload: FixedRulePayload<'_>,
                out: &mut FixedRuleOutput,
                _cancel: CancelFlag,
            ) -> Result<()> {
                out.put(Tuple::from_vec(vec![DataValue::from(1i64)]))?;
                Ok(())
            }
        }
        let res = run_fixed_rule(&Liar, vec![], empty_opts(), CancelFlag::inert());
        assert!(res.is_err());
    }

    #[test]
    fn simple_fixed_rule_arity_check_is_universal() -> Result<()> {
        let rule = SimpleFixedRule::new(2, MismatchedArityBody);
        let res = run_fixed_rule(&rule, vec![], empty_opts(), CancelFlag::inert());
        assert!(res.is_err());

        let rule = SimpleFixedRule::new(1, IdentityNamedRowsBody);
        let got = run_fixed_rule(
            &rule,
            vec![TestInput::new(
                vec!["x"],
                vec![Tuple::from_vec(vec![s("p")]), Tuple::from_vec(vec![s("q")])],
            )],
            empty_opts(),
            CancelFlag::inert(),
        )?;
        let want: Vec<Tuple> = vec![Tuple::from_vec(vec![s("p")]), Tuple::from_vec(vec![s("q")])];
        assert_eq!(got, want);
        Ok(())
    }

    #[test]
    fn harness_stored_inputs_refuse() {
        use tests_support::HarnessStoredClosed;
        let span = SourceSpan::empty();
        let arg = MagicFixedRuleRuleArg::Stored {
            name: Symbol::new("some_relation", span),
            bindings: vec![],
            as_of: None,
            span,
        };
        let stores = BTreeMap::new();
        let err = arg.arity(&stores, &HarnessStoredClosed).unwrap_err();
        assert!(err.to_string().contains("test harness"), "{err}");
    }

    #[test]
    fn cancellation_is_honored_mid_run() -> Result<()> {
        let (auth, cancel) = CancelAuthority::arm();
        let Cancelled = auth.cancel();
        let res = run_fixed_rule(
            &Bfs,
            vec![
                TestInput::new(
                    vec!["fr", "to"],
                    vec![Tuple::from_vec(vec![s("a"), s("b")])],
                ),
                TestInput::new(
                    vec!["id"],
                    vec![Tuple::from_vec(vec![s("a")]), Tuple::from_vec(vec![s("b")])],
                ),
                TestInput::new(vec!["start"], vec![Tuple::from_vec(vec![s("a")])]),
            ],
            opts_map(BTreeMap::from([(
                SmartString::from("condition"),
                Expr::Const {
                    val: DataValue::from(true),
                    span: SourceSpan::empty(),
                },
            )]))?,
            cancel,
        );
        let err = res.unwrap_err();
        assert!(err.to_string().contains("killed"), "{err}");
        Ok(())
    }

    #[test]
    fn default_rules_registry() -> Result<()> {
        for name in [
            "PageRank",
            "Constant",
            "ReorderSort",
            "CsvReader",
            "JsonReader",
            "ShortestPathDijkstra",
            "SCC",
        ] {
            assert!(DEFAULT_FIXED_RULES.contains_key(name), "{name} missing");
        }
        let pr = DEFAULT_FIXED_RULES
            .get("PageRank")
            .ok_or_else(|| miette!("PageRank missing"))?
            .clone();
        assert_eq!(pr.arity(&empty_opts(), &[], SourceSpan::empty())?, 2);
        Ok(())
    }

    #[test]
    fn rule_with_channel_round_trip() -> Result<()> {
        let (rule, receiver) = SimpleFixedRule::rule_with_channel(1);
        let handle = std::thread::spawn(move || -> Result<()> {
            let (inputs, _opts, reply) = receiver.recv().into_diagnostic()?;
            let (_headers, rows, _next) = inputs
                .into_iter()
                .next()
                .ok_or_else(|| miette!("test expected Some"))?
                .into_parts();
            reply
                .send(Ok(NamedRows::try_new(vec!["x".to_string()], rows)?))
                .into_diagnostic()?;
            Ok(())
        });
        let got = run_fixed_rule(
            &rule,
            vec![TestInput::new(
                vec!["x"],
                vec![Tuple::from_vec(vec![s("z")])],
            )],
            empty_opts(),
            CancelFlag::inert(),
        )?;
        let want: Vec<Tuple> = vec![Tuple::from_vec(vec![s("z")])];
        assert_eq!(got, want);
        handle
            .join()
            .map_err(|_| miette!("channel worker panicked"))??;
        Ok(())
    }

    #[test]
    fn graph_builders() -> Result<()> {
        struct Probe;
        impl FixedRule for Probe {
            fn arity(&self, _o: &FixedRuleOptions, _h: &[Symbol], _s: SourceSpan) -> Result<usize> {
                Ok(1)
            }
            fn run(
                &self,
                payload: FixedRulePayload<'_>,
                out: &mut FixedRuleOutput,
                _cancel: CancelFlag,
            ) -> Result<()> {
                let rel = payload.get_input(0)?;
                let (g, indices, inv) = rel.as_directed_graph(false)?;
                assert_eq!(g.node_count(), 3);
                assert_eq!(indices.len(), 3);
                assert_eq!(inv.len(), 3);
                let (g2, _, _) = rel.as_directed_graph(true)?;
                assert_eq!(
                    g2.out_neighbors(0).count()
                        + g2.out_neighbors(1).count()
                        + g2.out_neighbors(2).count(),
                    4
                );
                let (gw, _, _) = rel.as_directed_weighted_graph(false, false)?;
                let w: Vec<_> = gw.out_neighbors_with_values(0).map(|t| t.value).collect();
                assert_eq!(w, vec![1.0]);
                out.put(Tuple::from_vec(vec![DataValue::from(true)]))?;
                Ok(())
            }
        }
        run_fixed_rule(
            &Probe,
            vec![TestInput::new(
                vec!["fr", "to"],
                vec![
                    Tuple::from_vec(vec![DataValue::from("a"), DataValue::from("b")]),
                    Tuple::from_vec(vec![DataValue::from("b"), DataValue::from("c")]),
                ],
            )],
            empty_opts(),
            CancelFlag::inert(),
        )?;

        struct BadEdge;
        impl FixedRule for BadEdge {
            fn arity(&self, _o: &FixedRuleOptions, _h: &[Symbol], _s: SourceSpan) -> Result<usize> {
                Ok(1)
            }
            fn run(
                &self,
                payload: FixedRulePayload<'_>,
                _out: &mut FixedRuleOutput,
                _cancel: CancelFlag,
            ) -> Result<()> {
                payload.get_input(0)?.as_directed_graph(false)?;
                Ok(())
            }
        }
        let err = run_fixed_rule(
            &BadEdge,
            vec![TestInput::new(
                vec!["x"],
                vec![Tuple::from_vec(vec![DataValue::from("a")])],
            )],
            empty_opts(),
            CancelFlag::inert(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("edge"), "{err}");

        struct BadWeight;
        impl FixedRule for BadWeight {
            fn arity(&self, _o: &FixedRuleOptions, _h: &[Symbol], _s: SourceSpan) -> Result<usize> {
                Ok(1)
            }
            fn run(
                &self,
                payload: FixedRulePayload<'_>,
                _out: &mut FixedRuleOutput,
                _cancel: CancelFlag,
            ) -> Result<()> {
                payload
                    .get_input(0)?
                    .as_directed_weighted_graph(false, false)?;
                Ok(())
            }
        }
        let err = run_fixed_rule(
            &BadWeight,
            vec![TestInput::new(
                vec!["fr", "to", "w"],
                vec![Tuple::from_vec(vec![
                    s("a"),
                    s("b"),
                    DataValue::from(f64::NAN),
                ])],
            )],
            empty_opts(),
            CancelFlag::inert(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("edge weight"), "{err}");
        Ok(())
    }

    #[test]
    fn nullary_node_relation_refuses_not_panics_across_algos() -> Result<()> {
        use crate::rules::algo::astar::ShortestPathAStar;
        use crate::rules::algo::bfs::Bfs;
        use crate::rules::algo::degree_centrality::DegreeCentrality;
        use crate::rules::algo::dfs::Dfs;
        use crate::rules::algo::dijkstra::ShortestPathDijkstra;
        use crate::rules::algo::max_flow::MaxFlow;
        use crate::rules::algo::prim::MinimumSpanningTreePrim;
        use crate::rules::algo::random_walk::RandomWalk;
        use crate::rules::algo::scc::StronglyConnectedComponent;
        use crate::rules::algo::yen::KShortestPathYen;

        fn e(a: &str, b: &str, w: f64) -> Tuple {
            Tuple::from_vec(vec![s(a), s(b), DataValue::from(w)])
        }
        fn const_expr(v: DataValue) -> Expr {
            Expr::Const {
                val: v,
                span: SourceSpan::empty(),
            }
        }
        let nullary = || TestInput::new(vec![], vec![Tuple::from_vec(vec![])]);

        let cases: Vec<(&str, Result<Vec<Tuple>>)> = vec![
            (
                "ShortestPathDijkstra: starting",
                run_fixed_rule(
                    &ShortestPathDijkstra,
                    vec![
                        TestInput::new(vec!["fr", "to", "w"], vec![e("a", "b", 1.0)]),
                        nullary(),
                        TestInput::new(vec!["end"], vec![Tuple::from_vec(vec![s("b")])]),
                    ],
                    empty_opts(),
                    CancelFlag::inert(),
                ),
            ),
            (
                "KShortestPathYen: starting",
                run_fixed_rule(
                    &KShortestPathYen,
                    vec![
                        TestInput::new(vec!["fr", "to", "w"], vec![e("a", "b", 1.0)]),
                        nullary(),
                        TestInput::new(vec!["end"], vec![Tuple::from_vec(vec![s("b")])]),
                    ],
                    opts_map(BTreeMap::from([(
                        SmartString::from("k"),
                        const_expr(DataValue::from(1i64)),
                    )]))?,
                    CancelFlag::inert(),
                ),
            ),
            (
                "ShortestPathAStar: starting",
                run_fixed_rule(
                    &ShortestPathAStar,
                    vec![
                        TestInput::new(
                            vec!["fr", "to"],
                            vec![Tuple::from_vec(vec![s("a"), s("b")])],
                        ),
                        TestInput::new(
                            vec!["id"],
                            vec![Tuple::from_vec(vec![s("a")]), Tuple::from_vec(vec![s("b")])],
                        ),
                        nullary(),
                        TestInput::new(vec!["goal"], vec![Tuple::from_vec(vec![s("b")])]),
                    ],
                    opts_map(BTreeMap::from([(
                        SmartString::from("heuristic"),
                        const_expr(DataValue::from(0.0)),
                    )]))?,
                    CancelFlag::inert(),
                ),
            ),
            (
                "Bfs: starting_nodes",
                run_fixed_rule(
                    &Bfs,
                    vec![
                        TestInput::new(
                            vec!["fr", "to"],
                            vec![Tuple::from_vec(vec![s("a"), s("b")])],
                        ),
                        TestInput::new(
                            vec!["id"],
                            vec![Tuple::from_vec(vec![s("a")]), Tuple::from_vec(vec![s("b")])],
                        ),
                        nullary(),
                    ],
                    opts_map(BTreeMap::from([(
                        SmartString::from("condition"),
                        const_expr(DataValue::from(true)),
                    )]))?,
                    CancelFlag::inert(),
                ),
            ),
            (
                "Dfs: starting_nodes",
                run_fixed_rule(
                    &Dfs,
                    vec![
                        TestInput::new(
                            vec!["fr", "to"],
                            vec![Tuple::from_vec(vec![s("a"), s("b")])],
                        ),
                        TestInput::new(
                            vec!["id"],
                            vec![Tuple::from_vec(vec![s("a")]), Tuple::from_vec(vec![s("b")])],
                        ),
                        nullary(),
                    ],
                    opts_map(BTreeMap::from([(
                        SmartString::from("condition"),
                        const_expr(DataValue::from(true)),
                    )]))?,
                    CancelFlag::inert(),
                ),
            ),
            (
                "MaxFlow: source_rel",
                run_fixed_rule(
                    &MaxFlow,
                    vec![
                        TestInput::new(vec!["fr", "to", "w"], vec![e("a", "b", 1.0)]),
                        nullary(),
                        TestInput::new(vec!["sink"], vec![Tuple::from_vec(vec![s("b")])]),
                    ],
                    empty_opts(),
                    CancelFlag::inert(),
                ),
            ),
            (
                "MinimumSpanningTreePrim: starting",
                run_fixed_rule(
                    &MinimumSpanningTreePrim,
                    vec![
                        TestInput::new(vec!["fr", "to", "w"], vec![e("a", "b", 1.0)]),
                        nullary(),
                    ],
                    empty_opts(),
                    CancelFlag::inert(),
                ),
            ),
            (
                "DegreeCentrality: nodes",
                run_fixed_rule(
                    &DegreeCentrality,
                    vec![
                        TestInput::new(
                            vec!["fr", "to"],
                            vec![Tuple::from_vec(vec![s("a"), s("b")])],
                        ),
                        nullary(),
                    ],
                    empty_opts(),
                    CancelFlag::inert(),
                ),
            ),
            (
                "RandomWalk: starting",
                run_fixed_rule(
                    &RandomWalk,
                    vec![
                        TestInput::new(
                            vec!["fr", "to"],
                            vec![Tuple::from_vec(vec![s("a"), s("b")])],
                        ),
                        TestInput::new(
                            vec!["id"],
                            vec![Tuple::from_vec(vec![s("a")]), Tuple::from_vec(vec![s("b")])],
                        ),
                        nullary(),
                    ],
                    opts_map(BTreeMap::from([(
                        SmartString::from("steps"),
                        const_expr(DataValue::from(1i64)),
                    )]))?,
                    CancelFlag::inert(),
                ),
            ),
            (
                "StronglyConnectedComponent: nodes",
                run_fixed_rule(
                    &StronglyConnectedComponent::new(true),
                    vec![
                        TestInput::new(
                            vec!["fr", "to"],
                            vec![Tuple::from_vec(vec![s("a"), s("b")])],
                        ),
                        nullary(),
                    ],
                    empty_opts(),
                    CancelFlag::inert(),
                ),
            ),
        ];

        for (name, res) in cases {
            let err = match res {
                Ok(rows) => {
                    return Err(miette!(
                        "{name}: a nullary node relation must refuse, not succeed — got {} rows",
                        rows.len()
                    ));
                }
                Err(e) => e,
            };
            assert!(
                err.to_string().contains("arity"),
                "{name}: expected the typed arity refusal, got: {err}"
            );
        }
        Ok(())
    }
}
