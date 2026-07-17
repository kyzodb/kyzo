/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0). The load-bearing changes:
 *
 * - **The stored-input seam.** The original payload held a live
 *   `&SessionTx` and read stored relations from it directly. The
 *   transaction-facing arm is abstracted behind [`StoredInputSource`];
 *   `query/normalize.rs`'s `SessionView` (the runtime tier's query-time
 *   view) now implements it for real, so both in-memory inputs
 *   (`EpochStore`-backed) and stored/validity reads work in production.
 *   The pre-runtime `NoStoredInputs` placeholder is gone (P090). No
 *   algorithm sees the seam — they consume [`FixedRulePayload`] /
 *   [`FixedRuleInputRelation`] exactly as before (one lifetime instead of
 *   the original's two, since the `SessionTx<'b>` lifetime is erased by
 *   the trait object).
 * - **The arity-branded output writer.** The original's `run` wrote into a
 *   bare `RegularTempStore` whose `put` accepts any width; output arity
 *   was a doc-comment convention, checked at runtime only by
 *   `SimpleFixedRule`. Here `run` writes through [`FixedRuleOutput`],
 *   constructed with the arity the rule declared: every `put` is checked,
 *   for every rule, and a mismatch is a typed error, not corrupt rows
 *   downstream ("SimpleFixedRule's check made universal").
 * - **`Poison` becomes the cancel lifecycle** ([`CancelAuthority`] +
 *   [`CancelFlag`] + consuming [`Cancelled`]), defined here (the original's
 *   lived in `runtime/db.rs`). The budget/deadline path shares the same
 *   poll handle; cancellation is a typestate, not a free `AtomicBool`.
 * - **The `graph` crate is replaced** by the inline CSR in
 *   `fixed_rule/graph.rs` (see the decision record there), and the graph
 *   builders return errors by straightforward `Result` flow instead of
 *   smuggling a captured `Option<Report>` out of a `filter_map` closure.
 * - `SimpleFixedRule::rule_with_channel` uses `std::sync::mpsc`'s
 *   rendezvous channel instead of `crossbeam` (`sync_channel(0)` ≡
 *   `bounded(0)`); `lazy_static` becomes `std::sync::LazyLock`; the
 *   `graph-algo` cargo feature is gone (the algorithms are dependency-free
 *   pure Rust, so they are always compiled).
 * - Registration stores `Arc<dyn FixedRule>` (the original had
 *   `Arc<Box<dyn FixedRule>>` — one pointer hop for nothing).
 * - `FixedRule`, `FixedRuleHandle` are re-homed here from their seam
 *   declarations in `data/program.rs`; `FixedRuleNotFoundError` and the
 *   `Constant` rule from their seam in `parse/query.rs` (`Constant` now in
 *   `utilities/constant.rs`); `NamedRows` and `TupleIter` are declared
 *   here behind seams and re-home to the runtime tier and `data/tuple.rs`
 *   respectively when those land.
 * - The original's `InvalidInverseTripleUse` error was defined here but
 *   referenced nowhere in the workspace (a leftover from the pre-0.5
 *   triple-store engine); it is dropped.
 */

//! The fixed-rule tier: algorithms and utilities invoked as rules
//! (`?[...] <~ PageRank(...)`).
//!
//! A **fixed rule** is an opaque computation the Datalog engine treats as
//! a single stratum-bounded rule: it consumes whole input relations and
//! produces one output relation of a declared arity, and it never
//! participates in recursion (stratification places it, the magic rewrite
//! passes it through untouched). The pieces:
//!
//! - [`FixedRule`], the trait: `init_options` (normalize options at parse
//!   time), `arity` (the declared output width — enforced, see
//!   [`FixedRuleOutput`]), and `run`.
//! - [`FixedRulePayload`], what `run` receives: the resolved application
//!   (options + argument manifests) plus access to the input relations,
//!   via [`FixedRuleInputRelation`] — in-memory rule results now, stored
//!   relations through the [`StoredInputSource`] seam when the runtime
//!   tier lands.
//! - [`FixedRuleOutput`], the arity-branded writer `run` fills.
//! - [`CancelAuthority`] / [`CancelFlag`] / [`Cancelled`], the cooperative
//!   cancellation lifecycle every long-running algorithm polls.
//! - [`DEFAULT_FIXED_RULES`], the registry of the built-ins declared in
//!   `algos/` (graph algorithms) and `utilities/`.
//! - [`SimpleFixedRule`], the reduced-boilerplate wrapper for user-defined
//!   rules over realized [`NamedRows`] — named [`SimpleRuleBody`] owner
//!   types only (P083: no `Fn`/`dyn Fn` body).
//!
//! **P112.** Production host door is `FixedRule::run` (via
//! `runtime/db.rs` / `SessionFixedRule`). No module-level
//! `allow(dead_code)` on `fixed_rule` in `lib.rs`; unused residual symbols
//! warn rather than hide behind a blanket.

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, LazyLock, OnceLock};

use itertools::Itertools;
use miette::{Diagnostic, Result, bail, ensure};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::expr::Expr;
use crate::data::program::{
    FixedRuleOptionNotFoundError, MagicFixedRuleApply, MagicFixedRuleRuleArg, MagicSymbol,
    WrongFixedRuleOptionError,
};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::Tuple;
use crate::data::value::{AsOf, DataValue};
use crate::fixed_rule::algos::*;
use crate::fixed_rule::graph::{DirectedCsrGraph, GraphTooLargeError};
use crate::fixed_rule::utilities::*;
use crate::query::eval::{BudgetDimension, LimitExceeded};
use crate::query::levels::EpochStore;
use crate::query::temp_store::RegularTempStore;
use crate::data::value::data_value_any;

pub(crate) mod algos;
pub(crate) mod graph;
pub(crate) mod parallel;
pub(crate) mod rng;
pub(crate) mod utilities;

// ─────────────────────────────────────────────────────────────────────────
// SEAM: `TupleIter` re-homes to `data/tuple.rs` on landing (it is the
// tuple tier's iterator species, declared here only because this draft
// must not reshape a landed file).
// ─────────────────────────────────────────────────────────────────────────

/// A stream of tuples, each fallibly produced (a storage read can fail
/// mid-stream).
pub(crate) type TupleIter<'a> = Box<dyn Iterator<Item = Result<Tuple>> + 'a>;

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
        let _ = self.cell.0.set(());
        Cancelled
    }
}

/// Cooperative poll handle for long-running fixed rules (and the budget
/// interrupt path). Clone into algorithms; cannot request cancel — that
/// is [`CancelAuthority`]'s job (species pair).
///
/// This is the CozoDB original's `Poison` (`runtime/db.rs`), re-homed here
/// and joined to the budget lifecycle: the session arms one
/// [`CancelAuthority`], clones the [`CancelFlag`] into fixed rules /
/// search / [`Budget`], and spends the authority to kill.
#[derive(Clone)]
pub struct CancelFlag {
    cell: Arc<CancelCell>,
}

impl Default for CancelFlag {
    /// Inert poll handle: never observes cancellation (no authority is
    /// retained). Prefer [`CancelAuthority::arm`] when cancel must be
    /// requestable.
    fn default() -> Self {
        Self::inert()
    }
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
// SEAM: stored-relation input (landed with the runtime tier)
// ─────────────────────────────────────────────────────────────────────────

/// What the payload needs from the transaction in order to serve a
/// `MagicFixedRuleRuleArg::Stored` input: arity lookup and (validity-
/// aware) scans. `query/normalize.rs`'s `SessionView` implements this in
/// production, so a fixed rule reading a stored (including historical)
/// relation runs for real. The condemned pre-runtime `NoStoredInputs`
/// placeholder is demolished (P090). Algorithms never see this trait — it
/// exists so their code is final now.
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
                Box::new(store.all_iter().map(|t| Ok(t.into_tuple())))
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
                Box::new(store.prefix_iter(&t).map(|t| Ok(t.into_tuple())))
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

    /// The first two columns of each tuple as an edge, interning the node
    /// values to dense `u32` ids. Shared skeleton of the two graph
    /// builders below; errors flow straight out (the original collected
    /// them into a captured `Option<Report>` inside a `filter_map` and
    /// re-raised after the build). Minting is guarded by
    /// [`checked_node_id`]: at the 2^32-node bound the build refuses,
    /// typed, where the original's `indices.len() as u32` truncated
    /// silently.
    fn intern_edges<W: Copy>(
        &self,
        mut weight: impl FnMut(Option<&DataValue>) -> Result<W>,
        undirected: bool,
    ) -> Result<(Vec<(u32, u32, W)>, Vec<DataValue>, BTreeMap<DataValue, u32>)> {
        let mut indices: Vec<DataValue> = vec![];
        let mut inv_indices: BTreeMap<DataValue, u32> = Default::default();
        let mut edges: Vec<(u32, u32, W)> = vec![];
        for tuple in self.iter()? {
            let mut tuple = tuple?.into_iter();
            let from = tuple.next().ok_or_else(|| NotAnEdgeError(self.span()))?;
            let to = tuple.next().ok_or_else(|| NotAnEdgeError(self.span()))?;
            let mut intern = |val: DataValue| -> Result<u32> {
                Ok(match inv_indices.get(&val) {
                    Some(idx) => *idx,
                    None => {
                        let idx = checked_node_id(indices.len())?;
                        inv_indices.insert(val.clone(), idx);
                        indices.push(val);
                        idx
                    }
                })
            };
            let from_idx = intern(from)?;
            let to_idx = intern(to)?;
            let w = weight(tuple.next().as_ref())?;
            edges.push((from_idx, to_idx, w));
            if undirected {
                edges.push((to_idx, from_idx, w));
            }
        }
        Ok((edges, indices, inv_indices))
    }

    /// Convert the input relation into a directed graph.
    /// If `undirected` is true, then each edge in the input relation is treated as a pair
    /// of edges, one for each direction.
    ///
    /// Returns the graph, the vertices in a vector with the index the same as used in the graph,
    /// and the inverse vertex mapping.
    pub(crate) fn as_directed_graph(
        &self,
        undirected: bool,
    ) -> Result<(DirectedCsrGraph, Vec<DataValue>, BTreeMap<DataValue, u32>)> {
        let (edges, indices, inv_indices) = self.intern_edges(|_| Ok(()), undirected)?;
        Ok((DirectedCsrGraph::from_edges(edges)?, indices, inv_indices))
    }

    /// Convert the input relation into a directed weighted graph, the
    /// weight taken from the third column (`1.0` when absent). Weights
    /// must be finite numbers, and non-negative unless
    /// `allow_negative_weights`.
    /// If `undirected` is true, then each edge in the input relation is treated as a pair
    /// of edges, one for each direction.
    ///
    /// Returns the graph, the vertices in a vector with the index the same as used in the graph,
    /// and the inverse vertex mapping.
    pub(crate) fn as_directed_weighted_graph(
        &self,
        undirected: bool,
        allow_negative_weights: bool,
    ) -> Result<(
        DirectedCsrGraph<f32>,
        Vec<DataValue>,
        BTreeMap<DataValue, u32>,
    )> {
        let weight_span = self
            .arg_manifest
            .bindings()
            .get(2)
            .map(|s| s.span)
            .unwrap_or_else(|| self.span());
        let (edges, indices, inv_indices) = self.intern_edges(
            |d| -> Result<f32> {
                let d = match d {
                    None => return Ok(1.0),
                    Some(d) => d,
                };
                let f = d
                    .get_float()
                    .ok_or_else(|| BadEdgeWeightError(d.clone(), weight_span))?;
                if !f.is_finite() || (f < 0. && !allow_negative_weights) {
                    bail!(BadEdgeWeightError(d.clone(), weight_span));
                }
                Ok(f as f32)
            },
            undirected,
        )?;
        Ok((DirectedCsrGraph::from_edges(edges)?, indices, inv_indices))
    }
}

/// Mints the next dense node id at the intern site, refusing with the
/// typed [`GraphTooLargeError`] at the 2^32-node bound — the CozoDB
/// original's `indices.len() as u32` silently truncated there, aliasing
/// the 2^32-th node onto id 0. The cap is `u32::MAX` mintable ids
/// (`0..=u32::MAX - 1`); predecessor absence uses `Option` (P078), so
/// `u32::MAX` is no longer reserved as a sentinel.
///
/// The bound is untestable at scale (it would take ~4 billion interned
/// values); it is factored into this function precisely so a unit test
/// can pin the boundary arithmetic without the allocation. See the
/// honesty note on [`GraphTooLargeError`].
fn checked_node_id(interned_so_far: usize) -> Result<u32> {
    ensure!(interned_so_far < u32::MAX as usize, GraphTooLargeError);
    Ok(u32::try_from(interned_so_far).expect(
        "INVARIANT(node_id_fit): ensure! proved interned_so_far < u32::MAX",
    ))
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
                    name: name.to_string(),
                    span: self.manifest.span,
                    rule_name: self.manifest.fixed_handle.name.to_string(),
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
                    name: name.to_string(),
                    span: ex.span(),
                    rule_name: self.manifest.fixed_handle.name.to_string(),
                    help: "a string is required".to_string(),
                }
                .into()),
            },
            None => match default {
                None => Err(FixedRuleOptionNotFoundError {
                    name: name.to_string(),
                    span: self.manifest.span,
                    rule_name: self.manifest.fixed_handle.name.to_string(),
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
                name: name.to_string(),
                span: self.manifest.span,
                rule_name: self.manifest.fixed_handle.name.to_string(),
            }
            .into()),
            Some(v) => Ok(v.span()),
        }
    }
    /// Extract an integer option
    pub fn integer_option(&self, name: &str, default: Option<i64>) -> Result<i64> {
        match self.manifest.options.get(name) {
            Some(v) => match v.clone().eval_to_const() {
                Ok(DataValue::Num(n)) => match n.as_int() {
                    Some(i) => Ok(i),
                    None => Err(FixedRuleOptionNotFoundError {
                        name: name.to_string(),
                        span: self.manifest.span,
                        rule_name: self.manifest.fixed_handle.name.to_string(),
                    }
                    .into()),
                },
                _ => Err(WrongFixedRuleOptionError {
                    name: name.to_string(),
                    span: v.span(),
                    rule_name: self.manifest.fixed_handle.name.to_string(),
                    help: "an integer is required".to_string(),
                }
                .into()),
            },
            None => match default {
                Some(v) => Ok(v),
                None => Err(FixedRuleOptionNotFoundError {
                    name: name.to_string(),
                    span: self.manifest.span,
                    rule_name: self.manifest.fixed_handle.name.to_string(),
                }
                .into()),
            },
        }
    }
    /// Extract a positive integer option
    pub fn pos_integer_option(&self, name: &str, default: Option<usize>) -> Result<usize> {
        let i = self.integer_option(name, default.map(|i| i as i64))?;
        ensure!(
            i > 0,
            WrongFixedRuleOptionError {
                name: name.to_string(),
                span: self.option_span(name)?,
                rule_name: self.manifest.fixed_handle.name.to_string(),
                help: "a positive integer is required".to_string(),
            }
        );
        let span = self.option_span(name).unwrap_or(self.manifest.span);
        usize::try_from(i).map_err(|_| {
            WrongFixedRuleOptionError {
                name: name.to_string(),
                span,
                rule_name: self.manifest.fixed_handle.name.to_string(),
                help: "a positive integer fitting usize is required".to_string(),
            }
            .into()
        })
    }
    /// Extract a non-negative integer option
    pub fn non_neg_integer_option(&self, name: &str, default: Option<usize>) -> Result<usize> {
        let i = self.integer_option(name, default.map(|i| i as i64))?;
        ensure!(
            i >= 0,
            WrongFixedRuleOptionError {
                name: name.to_string(),
                span: self.option_span(name)?,
                rule_name: self.manifest.fixed_handle.name.to_string(),
                help: "a non-negative integer is required".to_string(),
            }
        );
        let span = self.option_span(name).unwrap_or(self.manifest.span);
        usize::try_from(i).map_err(|_| {
            WrongFixedRuleOptionError {
                name: name.to_string(),
                span,
                rule_name: self.manifest.fixed_handle.name.to_string(),
                help: "a non-negative integer fitting usize is required".to_string(),
            }
            .into()
        })
    }
    /// Extract a floating point option
    pub fn float_option(&self, name: &str, default: Option<f64>) -> Result<f64> {
        match self.manifest.options.get(name) {
            Some(v) => match v.clone().eval_to_const() {
                Ok(DataValue::Num(n)) => {
                    let f = n.to_f64();
                    Ok(f)
                }
                _ => Err(WrongFixedRuleOptionError {
                    name: name.to_string(),
                    span: v.span(),
                    rule_name: self.manifest.fixed_handle.name.to_string(),
                    help: "a floating number is required".to_string(),
                }
                .into()),
            },
            None => match default {
                Some(v) => Ok(v),
                None => Err(FixedRuleOptionNotFoundError {
                    name: name.to_string(),
                    span: self.manifest.span,
                    rule_name: self.manifest.fixed_handle.name.to_string(),
                }
                .into()),
            },
        }
    }
    /// Extract a floating point option between 0. and 1.
    pub fn unit_interval_option(&self, name: &str, default: Option<f64>) -> Result<f64> {
        let f = self.float_option(name, default)?;
        ensure!(
            (0. ..=1.).contains(&f),
            WrongFixedRuleOptionError {
                name: name.to_string(),
                span: self.option_span(name)?,
                rule_name: self.manifest.fixed_handle.name.to_string(),
                help: "a number between 0. and 1. is required".to_string(),
            }
        );
        Ok(f)
    }
    /// Extract a boolean option
    pub fn bool_option(&self, name: &str, default: Option<bool>) -> Result<bool> {
        match self.manifest.options.get(name) {
            Some(v) => match v.clone().eval_to_const() {
                Ok(DataValue::Bool(b)) => Ok(b),
                _ => Err(WrongFixedRuleOptionError {
                    name: name.to_string(),
                    span: v.span(),
                    rule_name: self.manifest.fixed_handle.name.to_string(),
                    help: "a boolean value is required".to_string(),
                }
                .into()),
            },
            None => match default {
                Some(v) => Ok(v),
                None => Err(FixedRuleOptionNotFoundError {
                    name: name.to_string(),
                    span: self.manifest.span,
                    rule_name: self.manifest.fixed_handle.name.to_string(),
                }
                .into()),
            },
        }
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
/// mis-shaped tuples into downstream joins. (In the CozoDB original the
/// output was a bare `RegularTempStore` whose `put` accepts anything;
/// arity was a doc-comment convention, enforced at runtime only by
/// `SimpleFixedRule` — that check is now universal and lives here.)
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
/// mid-epoch check in `query::eval::InterruptTicker`. A fixed rule (a graph
/// algorithm) runs to completion *inside one epoch*, filling its output
/// store before the barrier that merges it ever checks the
/// `derived_tuple_ceiling`. So a rule whose output is a near-cross-product
/// (e.g. an all-pairs result on a large graph) can materialize an unbounded
/// intermediate before any ceiling fires — the same hole this story closes
/// for ordinary rules. This bounds it: at each stride the writer refuses if
/// `baseline + <distinct rows put so far> > ceiling`, where `baseline` is
/// the globally admitted total as of this stratum's epoch-0 barrier.
///
/// Determinism and boundedness follow the same laws as the ordinary check:
/// the count is the output store's own distinct size (a function of the
/// algorithm's deterministic output alone), so the refusal — dimension,
/// spend, span — is byte-identical on every run, and peak output
/// materialization is bounded by `ceiling + OUTPUT_STRIDE`.
struct OutputSpendGuard {
    /// Globally admitted total as of this stratum's epoch-0 barrier.
    baseline: u64,
    ceiling: u64,
    /// Remaining puts until the next ceiling check (P097: proven stride).
    stride_left: OutputStrideLeft,
}

/// Rows a fixed rule may `put` between mid-run ceiling checks — harmonized
/// with `query::eval`'s `INTERRUPT_STRIDE`. Non-zero by construction.
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
    /// Brand a fresh output store with the rule's declared arity (the
    /// evaluator computes it via [`FixedRule::arity`] at parse time) and
    /// the application's span for error labeling.
    pub(crate) fn new(arity: usize, span: SourceSpan) -> Self {
        Self {
            store: RegularTempStore::default(),
            arity,
            span,
            guard: None,
        }
    }

    /// As [`Self::new`], but armed with the query's derived-tuple ceiling so
    /// the writer refuses mid-run once `baseline + rows > ceiling`. The
    /// evaluator passes the epoch-0 `spent_derived` as the `baseline`
    /// argument of `query::eval::FixedRuleEval::run`; the session's fixed-
    /// rule adapter (`query::normalize::SessionFixedRule`) forwards it here
    /// unchanged, along with the budget's `derived_tuple_ceiling`. `None`
    /// leaves the writer unbounded.
    pub(crate) fn new_budgeted(
        arity: usize,
        span: SourceSpan,
        baseline: u64,
        ceiling: Option<u64>,
    ) -> Self {
        Self {
            store: RegularTempStore::default(),
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
        if let Some(guard) = self.guard.as_mut() {
            if guard.stride_left.tick() {
                // Distinct rows materialized so far (the store dedups), the
                // same quantity the barrier will admit — so this never
                // refuses an output the barrier would have accepted.
                let spent = guard.baseline.saturating_add(self.store.len() as u64);
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
        }
        self.store.put(tuple);
        Ok(())
    }

    /// Surrender the filled store to the evaluator (which merges it into
    /// the rule's `EpochStore` at the epoch barrier). Called by the
    /// fixed-rule harness after `run` and by normalize when wiring a
    /// fixed rule into an epoch.
    pub(crate) fn into_store(self) -> RegularTempStore {
        self.store
    }
}

#[cfg(test)]
mod fixed_rule_output_budget_tests {
    use super::*;
    use crate::data::value::DataValue;

    fn row(i: i64) -> Tuple {
        Tuple::from_vec(vec![DataValue::from(i), DataValue::from(i)])
    }

    /// A fixed rule that floods its output refuses mid-run, typed, once its
    /// distinct output crosses the ceiling — before the epoch barrier that
    /// merges it. Mirrors the ordinary rule's mid-epoch guard.
    #[test]
    fn budgeted_output_refuses_mid_run() {
        let mut out = FixedRuleOutput::new_budgeted(2, SourceSpan(3, 5), 0, Some(10));
        let mut err = None;
        for i in 0..1_000 {
            if let Err(e) = out.put(row(i)) {
                err = Some(e);
                break;
            }
        }
        let err = err.expect("must refuse mid-run");
        let refusal: &LimitExceeded = err.downcast_ref().expect("typed LimitExceeded");
        assert_eq!(refusal.dimension, BudgetDimension::InFlightDerivations);
        assert_eq!(refusal.ceiling, 10);
        assert!(refusal.spent > 10);
        // Bounded by ceiling + stride: never materialized the whole flood.
        assert!(refusal.spent <= 10 + OUTPUT_STRIDE as u64);
        assert_eq!(refusal.span, Some(SourceSpan(3, 5)));
    }

    /// A small output (below one stride) is never perturbed, and an
    /// unbudgeted writer never refuses.
    #[test]
    fn small_and_unbudgeted_outputs_never_refuse() {
        let mut small = FixedRuleOutput::new_budgeted(2, SourceSpan(0, 0), 0, Some(3));
        for i in 0..5 {
            small.put(row(i)).expect("under a stride, never checked");
        }
        let mut unbudgeted = FixedRuleOutput::new(2, SourceSpan(0, 0));
        for i in 0..500 {
            unbudgeted.put(row(i)).expect("no ceiling, never refuses");
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// The trait
// ─────────────────────────────────────────────────────────────────────────

/// Trait for an implementation of an algorithm or a utility
pub trait FixedRule: Send + Sync {
    /// Consuming option normalize (P086). Called once before `arity`/`run`.
    /// Returns the (possibly rewritten) options map; the default is identity.
    fn init_options(
        &self,
        options: BTreeMap<SmartString<LazyCompact>, Expr>,
        _span: SourceSpan,
    ) -> Result<BTreeMap<SmartString<LazyCompact>, Expr>> {
        Ok(options)
    }
    /// You must return the row width of the returned relation and it must be accurate.
    /// This function may be called multiple times.
    fn arity(
        &self,
        options: &BTreeMap<SmartString<LazyCompact>, Expr>,
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
// SEAM: `NamedRows` re-homes to the runtime tier on landing (it is the
// public result type of `Db::run_script`; only the slice `SimpleFixedRule`
// needs is declared here).
// ─────────────────────────────────────────────────────────────────────────

/// Private seal: any private field blocks struct-literal minting outside
/// this module, so header/row/next cannot be forged past [`NamedRows::try_new`]
/// (P082).
#[derive(Debug, Clone, Default)]
struct NamedRowsSeal;

/// The rows of a relation, together with its header names.
///
/// Sole door: [`Self::try_new`] proves every row's width equals
/// `headers.len()`. Payload fields are private; the private
/// [`NamedRowsSeal`] blocks struct-literal minting outside this module
/// (P082). Read via [`Self::headers`] / [`Self::rows`] / [`Self::next`];
/// consume via [`Self::into_parts`] / [`Self::into_rows`] /
/// [`Self::with_next`]. Illegal (misaligned) NamedRows are unconstructible.
#[derive(Debug, Clone, Default)]
pub struct NamedRows {
    headers: Vec<String>,
    rows: Vec<Tuple>,
    next: Option<Box<NamedRows>>,
    _seal: NamedRowsSeal,
}

/// Header↔row width mismatch at the [`NamedRows`] door (P082).
#[derive(Debug, Error, Diagnostic)]
#[error(
    "NamedRows arity mismatch: header width {header_arity}, row {row_index} has width {row_arity}"
)]
#[diagnostic(code(fixed_rule::named_rows_arity))]
pub struct NamedRowsArityError {
    pub header_arity: usize,
    pub row_index: usize,
    pub row_arity: usize,
}

impl NamedRows {
    /// Sole door: every row's width equals `headers.len()`.
    pub fn try_new(
        headers: Vec<String>,
        rows: Vec<Tuple>,
    ) -> std::result::Result<Self, NamedRowsArityError> {
        let header_arity = headers.len();
        for (row_index, row) in rows.iter().enumerate() {
            let row_arity = row.len();
            if row_arity != header_arity {
                return Err(NamedRowsArityError {
                    header_arity,
                    row_index,
                    row_arity,
                });
            }
        }
        Ok(Self {
            headers,
            rows,
            next: None,
            _seal: NamedRowsSeal,
        })
    }

    /// Alias of [`Self::try_new`] — typed refuse, never panic (P082).
    pub fn new(
        headers: Vec<String>,
        rows: Vec<Tuple>,
    ) -> std::result::Result<Self, NamedRowsArityError> {
        Self::try_new(headers, rows)
    }

    /// Header names.
    pub fn headers(&self) -> &[String] {
        &self.headers
    }

    /// Result rows.
    pub fn rows(&self) -> &[Tuple] {
        &self.rows
    }

    /// Follow-on page, when present.
    pub fn next(&self) -> Option<&NamedRows> {
        self.next.as_deref()
    }

    /// Consume into headers, rows, and optional follow-on page.
    pub fn into_parts(self) -> (Vec<String>, Vec<Tuple>, Option<Box<NamedRows>>) {
        (self.headers, self.rows, self.next)
    }

    /// Consume into the row vector only.
    pub fn into_rows(self) -> Vec<Tuple> {
        self.rows
    }

    /// Attach a follow-on page (pagination chain). Does not re-prove arity —
    /// the page was already admitted through [`Self::try_new`].
    pub fn with_next(mut self, next: Option<Box<NamedRows>>) -> Self {
        self.next = next;
        self
    }

    /// Encode this result set as a self-contained Arrow IPC stream (story
    /// #77's export boundary): a Schema message naming every header, one
    /// RecordBatch message, and the end-of-stream marker — readable by any
    /// conforming Arrow implementation, built without depending on the
    /// `arrow` crate itself (see `data::arrow_ipc`'s module doc for why).
    /// Refuses (never silently drops data) when a column mixes more than
    /// one non-null kind, or a kind this encoder has no Arrow mapping for.
    pub fn to_arrow_ipc(&self) -> Result<Vec<u8>> {
        let batch =
            crate::data::arrow_ipc::ColumnBatch::from_rows(self.rows.clone(), self.headers.len());
        let names: Vec<&str> = self.headers.iter().map(String::as_str).collect();
        crate::data::arrow_ipc::encode_stream(&batch, &names)
    }
}

impl IntoIterator for NamedRows {
    type Item = Tuple;
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.into_iter()
    }
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
struct ChannelRuleBody {
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
        Self {
            return_arity,
            body,
        }
    }
}

impl SimpleFixedRule<ChannelRuleBody> {
    /// Construct a SimpleFixedRule that uses channels for communication.
    /// (The original returned `crossbeam` channel halves; a std rendezvous
    /// channel — `sync_channel(0)` ≡ crossbeam's `bounded(0)` — carries the
    /// same protocol without the dependency.)
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
        _options: &BTreeMap<SmartString<LazyCompact>, Expr>,
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
            // The row-width check the original performed here per rule is
            // now `out`'s own contract, enforced for every fixed rule.
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

/// The name under which a fixed rule is registered. (Re-homed here from
/// its seam declaration in `data/program.rs`.)
#[derive(Clone, Debug, serde_derive::Serialize, serde_derive::Deserialize)]
pub(crate) struct FixedRuleHandle {
    pub(crate) name: Symbol,
}

impl FixedRuleHandle {
    pub(crate) fn new(name: &str, span: SourceSpan) -> Self {
        FixedRuleHandle {
            name: Symbol::new(name, span),
        }
    }
}

#[derive(Error, Diagnostic, Debug)]
#[error("The requested fixed rule '{0}' is not found")]
#[diagnostic(code(parser::fixed_rule_not_found))]
pub(crate) struct FixedRuleNotFoundError(pub(crate) String, #[label] pub(crate) SourceSpan);

/// The built-in fixed rules: every graph algorithm in `algos/` (registered
/// under its full name and, for some, a short alias) and every utility in
/// `utilities/`. Parsing resolves rule names against this map (plus any
/// user-registered rules) and carries the resolved `Arc<dyn FixedRule>` in
/// the program.
pub(crate) static DEFAULT_FIXED_RULES: LazyLock<BTreeMap<String, Arc<dyn FixedRule>>> =
    LazyLock::new(|| {
        BTreeMap::from([
            (
                "ClusteringCoefficients".to_string(),
                Arc::new(ClusteringCoefficients) as Arc<dyn FixedRule>,
            ),
            (
                "DegreeCentrality".to_string(),
                Arc::new(DegreeCentrality) as Arc<dyn FixedRule>,
            ),
            (
                "ClosenessCentrality".to_string(),
                Arc::new(ClosenessCentrality) as Arc<dyn FixedRule>,
            ),
            (
                "BetweennessCentrality".to_string(),
                Arc::new(BetweennessCentrality) as Arc<dyn FixedRule>,
            ),
            (
                "DepthFirstSearch".to_string(),
                Arc::new(Dfs) as Arc<dyn FixedRule>,
            ),
            ("DFS".to_string(), Arc::new(Dfs) as Arc<dyn FixedRule>),
            (
                "BreadthFirstSearch".to_string(),
                Arc::new(Bfs) as Arc<dyn FixedRule>,
            ),
            ("BFS".to_string(), Arc::new(Bfs) as Arc<dyn FixedRule>),
            (
                "ShortestPathBFS".to_string(),
                Arc::new(ShortestPathBFS) as Arc<dyn FixedRule>,
            ),
            (
                "ShortestPathDijkstra".to_string(),
                Arc::new(ShortestPathDijkstra) as Arc<dyn FixedRule>,
            ),
            (
                "ShortestPathAStar".to_string(),
                Arc::new(ShortestPathAStar) as Arc<dyn FixedRule>,
            ),
            (
                "KShortestPathYen".to_string(),
                Arc::new(KShortestPathYen) as Arc<dyn FixedRule>,
            ),
            (
                "MinimumSpanningTreePrim".to_string(),
                Arc::new(MinimumSpanningTreePrim) as Arc<dyn FixedRule>,
            ),
            (
                "MinimumSpanningForestKruskal".to_string(),
                Arc::new(MinimumSpanningForestKruskal) as Arc<dyn FixedRule>,
            ),
            (
                "TopSort".to_string(),
                Arc::new(TopSort) as Arc<dyn FixedRule>,
            ),
            (
                "ConnectedComponents".to_string(),
                Arc::new(StronglyConnectedComponent::new(false)) as Arc<dyn FixedRule>,
            ),
            (
                "StronglyConnectedComponents".to_string(),
                Arc::new(StronglyConnectedComponent::new(true)) as Arc<dyn FixedRule>,
            ),
            (
                "SCC".to_string(),
                Arc::new(StronglyConnectedComponent::new(true)) as Arc<dyn FixedRule>,
            ),
            (
                "PageRank".to_string(),
                Arc::new(PageRank) as Arc<dyn FixedRule>,
            ),
            (
                "KCoreDecomposition".to_string(),
                Arc::new(KCoreDecomposition) as Arc<dyn FixedRule>,
            ),
            (
                "MaxFlow".to_string(),
                Arc::new(MaxFlow) as Arc<dyn FixedRule>,
            ),
            (
                "MaximalCliques".to_string(),
                Arc::new(MaximalCliques) as Arc<dyn FixedRule>,
            ),
            (
                "CommunityDetectionLouvain".to_string(),
                Arc::new(CommunityDetectionLouvain) as Arc<dyn FixedRule>,
            ),
            (
                "LabelPropagation".to_string(),
                Arc::new(LabelPropagation) as Arc<dyn FixedRule>,
            ),
            (
                "RandomWalk".to_string(),
                Arc::new(RandomWalk) as Arc<dyn FixedRule>,
            ),
            (
                "ReorderSort".to_string(),
                Arc::new(ReorderSort) as Arc<dyn FixedRule>,
            ),
            (
                "JsonReader".to_string(),
                Arc::new(JsonReader) as Arc<dyn FixedRule>,
            ),
            (
                "CsvReader".to_string(),
                Arc::new(CsvReader) as Arc<dyn FixedRule>,
            ),
            (
                "Constant".to_string(),
                Arc::new(Constant) as Arc<dyn FixedRule>,
            ),
        ])
    });

#[derive(Error, Diagnostic, Debug)]
#[error("The relation cannot be interpreted as an edge")]
#[diagnostic(code(algo::not_an_edge))]
#[diagnostic(help("Edge relation requires tuples of length at least two"))]
struct NotAnEdgeError(#[label] SourceSpan);

#[derive(Error, Diagnostic, Debug)]
#[error(
    "The value {0:?} at the third position in the relation cannot be interpreted as edge weights"
)]
#[diagnostic(code(algo::invalid_edge_weight))]
#[diagnostic(help(
    "Edge weights must be finite numbers. Some algorithm also requires positivity."
))]
struct BadEdgeWeightError(DataValue, #[label] SourceSpan);

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
// Test harness (new in KyzoDB)
// ─────────────────────────────────────────────────────────────────────────

/// Runs fixed rules without a database: builds a [`MagicFixedRuleApply`]
/// over in-memory inputs, executes `run`, and returns the output rows in
/// canonical order. Stored-relation arguments are refused by the harness
/// double [`HarnessStoredClosed`] (not a production placeholder — P090).
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

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
    /// algorithm body across several [`Self::run`] calls (e.g. an
    /// uncancelled baseline vs a cancelled run), which is what the BFS
    /// inner-poll cancellation test needs.
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
        mut options: BTreeMap<SmartString<LazyCompact>, Expr>,
    ) -> Result<PreparedFixedRule> {
        let span = SourceSpan::default();
        options = rule.init_options(options, span)?;
        let mut stores = BTreeMap::new();
        let mut rule_args = vec![];
        for (i, input) in inputs.into_iter().enumerate() {
            let name = MagicSymbol::Muggle {
                inner: Symbol::new(format!("_test_input_{i}"), span),
            };
            let mut fresh = RegularTempStore::default();
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
            options: Arc::new(options),
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
        /// output, returning the rows in canonical order. Reusable, so a
        /// test can time this call in isolation from store construction.
        pub(crate) fn run(&self, rule: &dyn FixedRule, cancel: CancelFlag) -> Result<Vec<Tuple>> {
            let payload = FixedRulePayload {
                manifest: &self.manifest,
                stores: &self.stores,
                stored: &HarnessStoredClosed,
            };
            let mut out = FixedRuleOutput::new(self.arity, SourceSpan::default());
            rule.run(payload, &mut out, cancel)?;
            let store = out.into_store().wrap();
            let mut collected = EpochStore::new_normal(self.arity);
            collected.merge_in(store, &mut ())?;
            Ok(collected.all_iter().map(|t| t.into_tuple()).collect_vec())
        }
    }

    pub(crate) fn run_fixed_rule(
        rule: &dyn FixedRule,
        inputs: Vec<TestInput>,
        options: BTreeMap<SmartString<LazyCompact>, Expr>,
        cancel: CancelFlag,
    ) -> Result<Vec<Tuple>> {
        prepare_fixed_rule(rule, inputs, options)?.run(rule, cancel)
    }

    /// A placeholder occupying `MagicFixedRuleApply::fixed_impl` in the
    /// harness (the payload never invokes it; the rule under test is
    /// driven directly).
    struct NeverRun;

    impl FixedRule for NeverRun {
        fn arity(
            &self,
            _options: &BTreeMap<SmartString<LazyCompact>, Expr>,
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
            unreachable!("the test harness never runs its placeholder impl")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::{TestInput, run_fixed_rule};
    use super::*;

    fn s(v: &str) -> DataValue {
        DataValue::from(v)
    }

    /// The arity brand refuses a mis-shaped row, typed — the check that
    /// was a doc-comment convention in the original.
    #[test]
    fn output_writer_rejects_wrong_arity() {
        let mut out = FixedRuleOutput::new(2, SourceSpan::default());
        out.put(Tuple::from_vec(vec![s("a"), s("b")])).unwrap();
        let err = out.put(Tuple::from_vec(vec![s("a")])).unwrap_err();
        assert!(err.to_string().contains("arity 2"), "{err}");
        let err = out
            .put(Tuple::from_vec(vec![s("a"), s("b"), s("c")]))
            .unwrap_err();
        assert!(err.to_string().contains("width 3"), "{err}");
    }

    /// A rule that lies about its arity is refused end-to-end: the writer
    /// is branded with the declared arity before `run` begins.
    #[test]
    fn lying_rule_is_refused() {
        struct Liar;
        impl FixedRule for Liar {
            fn arity(
                &self,
                _o: &BTreeMap<SmartString<LazyCompact>, Expr>,
                _h: &[Symbol],
                _s: SourceSpan,
            ) -> Result<usize> {
                Ok(3)
            }
            fn run(
                &self,
                _payload: FixedRulePayload<'_>,
                out: &mut FixedRuleOutput,
                _cancel: CancelFlag,
            ) -> Result<()> {
                out.put(Tuple::from_vec(vec![DataValue::from(1i64)]))?; // declared 3, wrote 1
                Ok(())
            }
        }
        let res = run_fixed_rule(&Liar, vec![], BTreeMap::new(), CancelFlag::default());
        assert!(res.is_err());
    }

    /// `SimpleFixedRule` rides the universal check: its rows are
    /// width-checked by the writer. Bodies are named types (P083).
    #[test]
    fn simple_fixed_rule_arity_check_is_universal() {
        let rule = SimpleFixedRule::new(2, MismatchedArityBody);
        let res = run_fixed_rule(&rule, vec![], BTreeMap::new(), CancelFlag::default());
        assert!(res.is_err());

        let rule = SimpleFixedRule::new(1, IdentityNamedRowsBody);
        let got = run_fixed_rule(
            &rule,
            vec![TestInput::new(
                vec!["x"],
                vec![Tuple::from_vec(vec![s("p")]), Tuple::from_vec(vec![s("q")])],
            )],
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap();
        let want: Vec<Tuple> = vec![Tuple::from_vec(vec![s("p")]), Tuple::from_vec(vec![s("q")])];
        assert_eq!(got, want);
    }

    /// Harness stored-input double refuses stored args (P090: production
    /// placeholder gone; this pins the test door only).
    #[test]
    fn harness_stored_inputs_refuse() {
        use tests_support::HarnessStoredClosed;
        let span = SourceSpan::default();
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

    /// Cancellation is honored mid-run: a spent [`CancelAuthority`] makes a
    /// graph traversal return the typed refusal instead of completing.
    #[test]
    fn cancellation_is_honored_mid_run() {
        let (auth, cancel) = CancelAuthority::arm();
        let _ = auth.cancel();
        // A graph with an edge, so BFS enters its per-edge loop where the
        // flag is polled.
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
            BTreeMap::from([(
                SmartString::from("condition"),
                Expr::Const {
                    val: DataValue::from(true),
                    span: SourceSpan::default(),
                },
            )]),
            cancel,
        );
        let err = res.unwrap_err();
        assert!(err.to_string().contains("killed"), "{err}");
    }

    /// The registry: every built-in resolves, and `Arc<dyn FixedRule>` is
    /// directly usable.
    #[test]
    fn default_rules_registry() {
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
        let pr = DEFAULT_FIXED_RULES.get("PageRank").unwrap().clone();
        assert_eq!(
            pr.arity(&BTreeMap::new(), &[], SourceSpan::default())
                .unwrap(),
            2
        );
    }

    /// The channel-backed custom rule round-trips over the std rendezvous
    /// channel.
    #[test]
    fn rule_with_channel_round_trip() {
        let (rule, receiver) = SimpleFixedRule::rule_with_channel(1);
        let handle = std::thread::spawn(move || {
            let (inputs, _opts, reply) = receiver.recv().unwrap();
            let (_headers, rows, _next) = inputs.into_iter().next().unwrap().into_parts();
            reply
                .send(Ok(NamedRows::try_new(vec!["x".to_string()], rows).unwrap()))
                .unwrap();
        });
        let got = run_fixed_rule(
            &rule,
            vec![TestInput::new(
                vec!["x"],
                vec![Tuple::from_vec(vec![s("z")])],
            )],
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap();
        let want: Vec<Tuple> = vec![Tuple::from_vec(vec![s("z")])];
        assert_eq!(got, want);
        handle.join().unwrap();
    }

    /// F3: the intern site refuses, typed, at the 2^32-node bound instead
    /// of truncating `indices.len() as u32`. The bound cannot be reached
    /// end-to-end (it needs ~4B distinct node values), so this pins the
    /// boundary arithmetic of the factored check:
    ///   - 0 interned                → id 0
    ///   - u32::MAX - 1 interned     → id u32::MAX - 1 (last mintable id;
    ///     CSR `max_id + 1` must still fit in `u32`)
    ///   - u32::MAX interned         → GraphTooLargeError (would mint id
    ///     `u32::MAX`, making `node_count` overflow)
    #[test]
    fn intern_site_refuses_at_u32_bound() {
        assert_eq!(checked_node_id(0).unwrap(), 0);
        assert_eq!(
            checked_node_id((u32::MAX - 1) as usize).unwrap(),
            u32::MAX - 1
        );
        let err = checked_node_id(u32::MAX as usize).unwrap_err();
        assert!(
            err.downcast_ref::<GraphTooLargeError>().is_some(),
            "expected the typed GraphTooLargeError, got: {err}"
        );
        assert!(err.to_string().contains("2^32"), "{err}");
    }

    /// Graph builders: interning, undirected doubling, and the typed edge
    /// errors that used to be smuggled through a captured `Option<Report>`.
    #[test]
    fn graph_builders() {
        struct Probe;
        impl FixedRule for Probe {
            fn arity(
                &self,
                _o: &BTreeMap<SmartString<LazyCompact>, Expr>,
                _h: &[Symbol],
                _s: SourceSpan,
            ) -> Result<usize> {
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
                    4 // two edges, doubled
                );
                let (gw, _, _) = rel.as_directed_weighted_graph(false, false)?;
                let w: Vec<_> = gw.out_neighbors_with_values(0).map(|t| t.value).collect();
                assert_eq!(w, vec![1.0]); // absent third column defaults to 1.0
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
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap();

        // A one-column tuple is not an edge: typed error, straight out.
        struct BadEdge;
        impl FixedRule for BadEdge {
            fn arity(
                &self,
                _o: &BTreeMap<SmartString<LazyCompact>, Expr>,
                _h: &[Symbol],
                _s: SourceSpan,
            ) -> Result<usize> {
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
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("edge"), "{err}");

        // A NaN weight is refused with the typed weight error.
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
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("edge weight"), "{err}");
        struct BadWeight;
        impl FixedRule for BadWeight {
            fn arity(
                &self,
                _o: &BTreeMap<SmartString<LazyCompact>, Expr>,
                _h: &[Symbol],
                _s: SourceSpan,
            ) -> Result<usize> {
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
    }

    /// Systemic finding: ten graph algorithms read a "node" relation's first
    /// column (`tuple[0]`, or `.next().unwrap()` on the same shape) without
    /// first proving the relation has at least one bound column. A NULLARY
    /// relation (zero columns) supplied as that argument made every one of
    /// them panic instead of refusing cleanly — `ensure_min_len` checks the
    /// relation's declared arity (from its bindings), not its row count, so
    /// this is a schema-level guard, not a data-shape one (mirrors the
    /// pre-existing guard in `shortest_path_bfs.rs`).
    ///
    /// Each of the ten now guards with `.ensure_min_len(1)?` (or `?` after
    /// an `and_then`/match for the algorithms where the relation is
    /// optional and a MISSING one is a legitimate "skip", but a PROVIDED
    /// nullary one must still be a real error). This test drives one
    /// nullary relation through each and asserts a typed arity refusal, not
    /// a panic — so a run that used to abort the whole process instead
    /// fails just this one assertion if the guard is ever removed.
    #[test]
    fn nullary_node_relation_refuses_not_panics_across_algos() {
        use crate::fixed_rule::algos::{
            Bfs, DegreeCentrality, Dfs, KShortestPathYen, MaxFlow, MinimumSpanningTreePrim,
            RandomWalk, ShortestPathAStar, ShortestPathDijkstra, StronglyConnectedComponent,
        };

        fn e(a: &str, b: &str, w: f64) -> Tuple {
            Tuple::from_vec(vec![s(a), s(b), DataValue::from(w)])
        }
        fn const_expr(v: DataValue) -> Expr {
            Expr::Const {
                val: v,
                span: SourceSpan::default(),
            }
        }
        // Zero bindings, one zero-length row: exactly the shape that used
        // to reach `tuple[0]` / `.next().unwrap()` and panic. Every case
        // below is otherwise a complete, valid, non-empty setup — options
        // included — so removing just the one guard under test lets
        // execution reach the real indexing site instead of stopping on
        // some unrelated missing-input/-option error.
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
                    BTreeMap::new(),
                    CancelFlag::default(),
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
                    BTreeMap::from([(SmartString::from("k"), const_expr(DataValue::from(1i64)))]),
                    CancelFlag::default(),
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
                    BTreeMap::from([(
                        SmartString::from("heuristic"),
                        const_expr(DataValue::from(0.0)),
                    )]),
                    CancelFlag::default(),
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
                    BTreeMap::from([(
                        SmartString::from("condition"),
                        const_expr(DataValue::from(true)),
                    )]),
                    CancelFlag::default(),
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
                    BTreeMap::from([(
                        SmartString::from("condition"),
                        const_expr(DataValue::from(true)),
                    )]),
                    CancelFlag::default(),
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
                    BTreeMap::new(),
                    CancelFlag::default(),
                ),
            ),
            (
                "MinimumSpanningTreePrim: starting",
                run_fixed_rule(
                    &MinimumSpanningTreePrim,
                    // A real edge, so `graph.node_count() != 0` and the run
                    // reaches the starting-node check instead of the
                    // legitimate empty-graph early return.
                    vec![
                        TestInput::new(vec!["fr", "to", "w"], vec![e("a", "b", 1.0)]),
                        nullary(),
                    ],
                    BTreeMap::new(),
                    CancelFlag::default(),
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
                    BTreeMap::new(),
                    CancelFlag::default(),
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
                    BTreeMap::from([(
                        SmartString::from("steps"),
                        const_expr(DataValue::from(1i64)),
                    )]),
                    CancelFlag::default(),
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
                    BTreeMap::new(),
                    CancelFlag::default(),
                ),
            ),
        ];

        for (name, res) in cases {
            let err = match res {
                Ok(rows) => panic!(
                    "{name}: a nullary node relation must refuse, not succeed — got {} rows",
                    rows.len()
                ),
                Err(e) => e,
            };
            assert!(
                err.to_string().contains("arity"),
                "{name}: expected the typed arity refusal, got: {err}"
            );
        }
    }

    /// `to_arrow_ipc` is the actual production call site of story #77's
    /// encoder (`data::arrow_ipc::encode_stream`) — not just a test-only
    /// path. A real Arrow reader proves the byte layout in
    /// `kyzo-arrow-interop`; this just proves `NamedRows` wires its own
    /// headers/rows into that encoder correctly, including the refusal
    /// path for a genuinely heterogeneous column.
    #[test]
    fn to_arrow_ipc_encodes_a_real_result_set() {
        let named = NamedRows::try_new(
            vec!["n".into(), "name".into()],
            vec![
                Tuple::from_vec(vec![DataValue::from(1), s("a")]),
                Tuple::from_vec(vec![DataValue::from(2), s("b")]),
            ],
        )
        .unwrap();
        let bytes = named
            .to_arrow_ipc()
            .expect("uniformly-typed columns encode");
        assert!(bytes.len() > 16, "a real stream is more than a bare marker");
    }

    #[test]
    fn to_arrow_ipc_refuses_a_heterogeneous_column() {
        let named = NamedRows::try_new(
            vec!["mixed".into()],
            vec![
                Tuple::from_vec(vec![DataValue::from(1)]),
                Tuple::from_vec(vec![s("x")]),
            ],
        )
        .unwrap();
        let err = named.to_arrow_ipc().unwrap_err();
        assert!(
            err.to_string().contains("more than one non-null kind"),
            "{err}"
        );
    }

    /// `try_new` proves header↔row arity (P082).
    #[test]
    fn named_rows_try_new_proves_arity() {
        assert!(
            NamedRows::try_new(
                vec!["a".into(), "b".into()],
                vec![Tuple::from_vec(vec![DataValue::from(1)])],
            )
            .is_err()
        );
        let ok = NamedRows::try_new(
            vec!["a".into()],
            vec![Tuple::from_vec(vec![DataValue::from(1)])],
        )
        .unwrap();
        assert_eq!(ok.headers().len(), 1);
        assert_eq!(ok.rows().len(), 1);
    }
}
