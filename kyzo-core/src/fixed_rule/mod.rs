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
 *   `&SessionTx` and read stored relations from it directly. The runtime
 *   tier has not landed, so the transaction-facing arm is abstracted
 *   behind [`StoredInputSource`]; in-memory inputs (`EpochStore`-backed)
 *   work fully now, and `SessionTx` implements the trait when it lands.
 *   No algorithm sees the seam — they consume [`FixedRulePayload`] /
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
 * - **`Poison` becomes [`CancelFlag`]**, defined here (the original's
 *   lived in `runtime/db.rs`). Same substance (an `Arc<AtomicBool>`
 *   checked cooperatively); it is the integration point where the ratified
 *   budget/deadline design (story #3) attaches when the runtime lands.
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
//! - [`CancelFlag`], the cooperative cancellation check every long-running
//!   algorithm polls.
//! - [`DEFAULT_FIXED_RULES`], the registry of the built-ins declared in
//!   `algos/` (graph algorithms) and `utilities/`.
//! - [`SimpleFixedRule`], the reduced-boilerplate wrapper for user-defined
//!   rules over realized [`NamedRows`].

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, LazyLock};

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
use crate::data::tuple::Tuple;
use crate::data::value::{DataValue, ValidityTs};
use crate::fixed_rule::algos::*;
use crate::fixed_rule::graph::{DirectedCsrGraph, GraphTooLargeError};
use crate::fixed_rule::utilities::*;
use crate::query::eval::{BudgetDimension, LimitExceeded};
use crate::runtime::temp_store::{EpochStore, RegularTempStore};

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
// Cancellation
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Error, Diagnostic)]
#[error("Running query is killed before completion")]
#[diagnostic(code(eval::killed))]
#[diagnostic(help("A query may be killed by timeout, or explicit command"))]
pub(crate) struct QueryCancelledError;

/// Cooperative cancellation: a shared flag that long-running fixed rules
/// (and, once the runtime lands, the whole evaluator) poll via
/// [`Self::check`], which refuses with a typed error once the flag is set.
///
/// This is the CozoDB original's `Poison` (`runtime/db.rs`), re-homed to
/// the payload tier because it is self-contained and every algorithm needs
/// it now. It is also the designated integration point for story #3's
/// ratified budget design: the runtime's kill switch, query timeouts, and
/// the deadline half of `Budget` all act by setting this flag, so a rule
/// that honors `check` honors all of them for free.
#[derive(Clone, Default)]
pub struct CancelFlag(pub(crate) Arc<AtomicBool>);

impl std::fmt::Debug for CancelFlag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CancelFlag({})", self.0.load(Ordering::Relaxed))
    }
}

impl CancelFlag {
    /// Refuses with a typed error if cancellation has been requested.
    /// Poll this at least once per unit of unbounded work (per node
    /// visited, per edge relaxed) — a loop that never checks is a loop
    /// that cannot be killed.
    #[inline(always)]
    pub fn check(&self) -> Result<()> {
        if self.0.load(Ordering::Relaxed) {
            bail!(QueryCancelledError)
        }
        Ok(())
    }

    /// Request cancellation: every subsequent `check` on any clone of this
    /// flag refuses.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

// ─────────────────────────────────────────────────────────────────────────
// SEAM: stored-relation input (lands with the runtime tier)
// ─────────────────────────────────────────────────────────────────────────

/// What the payload needs from the transaction in order to serve a
/// `MagicFixedRuleRuleArg::Stored` input: arity lookup and (validity-
/// aware) scans. `SessionTx` implements this when the runtime tier lands;
/// until then [`NoStoredInputs`] refuses with a typed error. Algorithms
/// never see this trait — it exists so their code is final now.
pub(crate) trait StoredInputSource {
    fn stored_arity(&self, name: &Symbol) -> Result<usize>;
    /// Scan the whole relation, as-of `valid_at` if given.
    fn stored_scan_all<'a>(
        &'a self,
        name: &Symbol,
        valid_at: Option<ValidityTs>,
    ) -> Result<TupleIter<'a>>;
    /// Scan the tuples whose first key column equals `prefix`.
    fn stored_scan_prefix<'a>(
        &'a self,
        name: &Symbol,
        prefix: &DataValue,
        valid_at: Option<ValidityTs>,
    ) -> Result<TupleIter<'a>>;
}

#[derive(Debug, Error, Diagnostic)]
#[error("Stored relation '{name}' is not available to fixed rules yet")]
#[diagnostic(code(algo::stored_input_unavailable))]
#[diagnostic(help(
    "Reading stored relations from a fixed rule requires the runtime tier, \
     which has not landed in this build"
))]
pub(crate) struct StoredInputUnavailable {
    name: String,
    #[label]
    span: SourceSpan,
}

/// The pre-runtime state of the [`StoredInputSource`] seam: every stored
/// read refuses, typed. Deleted (or kept for tx-less contexts) when
/// `SessionTx` lands.
pub(crate) struct NoStoredInputs;

impl NoStoredInputs {
    fn refuse<T>(&self, name: &Symbol) -> Result<T> {
        Err(StoredInputUnavailable {
            name: name.to_string(),
            span: name.span,
        }
        .into())
    }
}

impl StoredInputSource for NoStoredInputs {
    fn stored_arity(&self, name: &Symbol) -> Result<usize> {
        self.refuse(name)
    }
    fn stored_scan_all<'a>(
        &'a self,
        name: &Symbol,
        _valid_at: Option<ValidityTs>,
    ) -> Result<TupleIter<'a>> {
        self.refuse(name)
    }
    fn stored_scan_prefix<'a>(
        &'a self,
        name: &Symbol,
        _prefix: &DataValue,
        _valid_at: Option<ValidityTs>,
    ) -> Result<TupleIter<'a>> {
        self.refuse(name)
    }
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
            MagicFixedRuleRuleArg::Stored { name, valid_at, .. } => {
                self.stored.stored_scan_all(name, *valid_at)?
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
                let t = vec![prefix.clone()];
                Box::new(store.prefix_iter(&t).map(|t| Ok(t.into_tuple())))
            }
            MagicFixedRuleRuleArg::Stored { name, valid_at, .. } => {
                self.stored.stored_scan_prefix(name, prefix, *valid_at)?
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
/// the 2^32-th node onto id 0. The cap is `u32::MAX - 1`: `u32::MAX`
/// itself stays reserved as the Dijkstra core's "no back-pointer"
/// sentinel.
///
/// The bound is untestable at scale (it would take ~4 billion interned
/// values); it is factored into this function precisely so a unit test
/// can pin the boundary arithmetic without the allocation. See the
/// honesty note on [`GraphTooLargeError`].
fn checked_node_id(interned_so_far: usize) -> Result<u32> {
    ensure!(interned_so_far < u32::MAX as usize, GraphTooLargeError);
    Ok(interned_so_far as u32)
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
    pub fn string_option(
        &self,
        name: &str,
        default: Option<&str>,
    ) -> Result<SmartString<LazyCompact>> {
        match self.manifest.options.get(name) {
            Some(ex) => match ex.clone().eval_to_const()? {
                DataValue::Str(s) => Ok(s),
                _ => Err(WrongFixedRuleOptionError {
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
                Some(s) => Ok(SmartString::from(s)),
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
                Ok(DataValue::Num(n)) => match n.get_int() {
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
        Ok(i as usize)
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
        Ok(i as usize)
    }
    /// Extract a floating point option
    pub fn float_option(&self, name: &str, default: Option<f64>) -> Result<f64> {
        match self.manifest.options.get(name) {
            Some(v) => match v.clone().eval_to_const() {
                Ok(DataValue::Num(n)) => {
                    let f = n.get_float();
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
    countdown: u32,
}

/// Rows a fixed rule may `put` between mid-run ceiling checks — harmonized
/// with `query::eval`'s `INTERRUPT_STRIDE`.
const OUTPUT_STRIDE: u32 = 64;

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
    /// evaluator's fixed-rule dispatch (in `query::eval`, same module as
    /// `Budget`) passes the epoch-0 `spent_derived` as `baseline` and the
    /// budget's `derived_tuple_ceiling`; `None` leaves the writer unbounded.
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
                countdown: OUTPUT_STRIDE,
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
            guard.countdown -= 1;
            if guard.countdown == 0 {
                guard.countdown = OUTPUT_STRIDE;
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
    /// the rule's `EpochStore` at the epoch barrier).
    #[allow(dead_code)] // consumed by eval when the query tier lands
    pub(crate) fn into_store(self) -> RegularTempStore {
        self.store
    }
}

#[cfg(test)]
mod fixed_rule_output_budget_tests {
    use super::*;
    use crate::data::value::DataValue;

    fn row(i: i64) -> Vec<DataValue> {
        vec![DataValue::from(i), DataValue::from(i)]
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
    /// Called to initialize the options given.
    /// Will always be called once, before anything else.
    /// You can mutate the options if you need to.
    /// The default implementation does nothing.
    fn init_options(
        &self,
        _options: &mut BTreeMap<SmartString<LazyCompact>, Expr>,
        _span: SourceSpan,
    ) -> Result<()> {
        Ok(())
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

/// The rows of a relation, together with its header names.
#[derive(Debug, Clone, Default)]
pub struct NamedRows {
    /// The headers
    pub headers: Vec<String>,
    /// The rows
    pub rows: Vec<Tuple>,
    /// Contains the next named rows, if exists
    pub next: Option<Box<NamedRows>>,
}

impl NamedRows {
    /// create a named rows with the given headers and rows
    pub fn new(headers: Vec<String>, rows: Vec<Tuple>) -> Self {
        Self {
            headers,
            rows,
            next: None,
        }
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
// SimpleFixedRule
// ─────────────────────────────────────────────────────────────────────────

/// Simple wrapper for custom fixed rule. You have less control than implementing [FixedRule] directly,
/// but implementation is simpler.
pub struct SimpleFixedRule {
    return_arity: usize,
    rule: Box<
        dyn Fn(Vec<NamedRows>, BTreeMap<String, DataValue>) -> Result<NamedRows>
            + Send
            + Sync
            + 'static,
    >,
}

impl SimpleFixedRule {
    /// Construct a SimpleFixedRule.
    ///
    /// * `return_arity`: The return arity of this rule.
    /// * `rule`:  The rule implementation as a closure.
    //    The first argument is a vector of input relations, realized into NamedRows,
    //    and the second argument is a JSON object of passed in options.
    //    The returned NamedRows is the return relation of the application of this rule.
    //    Every row of the returned relation must have length equal to `return_arity`.
    pub fn new<R>(return_arity: usize, rule: R) -> Self
    where
        R: Fn(Vec<NamedRows>, BTreeMap<String, DataValue>) -> Result<NamedRows>
            + Send
            + Sync
            + 'static,
    {
        Self {
            return_arity,
            rule: Box::new(rule),
        }
    }
    /// Construct a SimpleFixedRule that uses channels for communication.
    /// (The original returned `crossbeam` channel halves; a std rendezvous
    /// channel — `sync_channel(0)` ≡ crossbeam's `bounded(0)` — carries the
    /// same protocol without the dependency.)
    pub fn rule_with_channel(
        return_arity: usize,
    ) -> (
        Self,
        Receiver<(
            Vec<NamedRows>,
            BTreeMap<String, DataValue>,
            SyncSender<Result<NamedRows>>,
        )>,
    ) {
        let (db2app_sender, db2app_receiver) = sync_channel(0);
        (
            Self {
                return_arity,
                rule: Box::new(move |inputs, options| -> Result<NamedRows> {
                    let (app2db_sender, app2db_receiver) = sync_channel(0);
                    db2app_sender
                        .send((inputs, options, app2db_sender))
                        .map_err(|_| DisconnectedChannelRule)?;
                    app2db_receiver
                        .recv()
                        .map_err(|_| DisconnectedChannelRule)?
                }),
            },
            db2app_receiver,
        )
    }
}

#[derive(Debug, Error, Diagnostic)]
#[error("The channel backing this custom fixed rule has disconnected")]
#[diagnostic(code(algo::channel_rule_disconnected))]
struct DisconnectedChannelRule;

impl FixedRule for SimpleFixedRule {
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
                // Structural: `i < rule_args.len()`, so the index resolves.
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
                Ok(NamedRows::new(headers, rows))
            })
            .try_collect()?;
        let results: NamedRows = (self.rule)(inputs, options)?;
        for row in results.rows {
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
#[derive(Clone, Debug)]
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
/// canonical order. The stored-input seam stays closed
/// ([`NoStoredInputs`]), which is exactly the point: everything except
/// stored-relation arguments is testable before the runtime lands.
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
        rule.init_options(&mut options, span)?;
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
                stored: &NoStoredInputs,
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
        out.put(vec![s("a"), s("b")]).unwrap();
        let err = out.put(vec![s("a")]).unwrap_err();
        assert!(err.to_string().contains("arity 2"), "{err}");
        let err = out.put(vec![s("a"), s("b"), s("c")]).unwrap_err();
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
                out.put(vec![DataValue::from(1i64)])?; // declared 3, wrote 1
                Ok(())
            }
        }
        let res = run_fixed_rule(&Liar, vec![], BTreeMap::new(), CancelFlag::default());
        assert!(res.is_err());
    }

    /// `SimpleFixedRule` rides the universal check: its rows are
    /// width-checked by the writer.
    #[test]
    fn simple_fixed_rule_arity_check_is_universal() {
        let rule = SimpleFixedRule::new(2, |_inputs, _opts| {
            Ok(NamedRows::new(
                vec!["a".to_string()],
                vec![vec![DataValue::from(1i64)]], // width 1, declared 2
            ))
        });
        let res = run_fixed_rule(&rule, vec![], BTreeMap::new(), CancelFlag::default());
        assert!(res.is_err());

        let rule = SimpleFixedRule::new(1, |inputs, _opts| {
            // Identity over the single input.
            Ok(NamedRows::new(
                vec!["a".to_string()],
                inputs.into_iter().next().unwrap().rows,
            ))
        });
        let got = run_fixed_rule(
            &rule,
            vec![TestInput::new(vec!["x"], vec![vec![s("p")], vec![s("q")]])],
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap();
        assert_eq!(got, vec![vec![s("p")], vec![s("q")]]);
    }

    /// The stored-input seam refuses, typed, until the runtime lands.
    #[test]
    fn stored_inputs_refuse_before_runtime() {
        let span = SourceSpan::default();
        let arg = MagicFixedRuleRuleArg::Stored {
            name: Symbol::new("some_relation", span),
            bindings: vec![],
            valid_at: None,
            span,
        };
        let stores = BTreeMap::new();
        let err = arg.arity(&stores, &NoStoredInputs).unwrap_err();
        assert!(err.to_string().contains("not available"), "{err}");
    }

    /// Cancellation is honored mid-run: a pre-set flag makes a graph
    /// traversal return the typed refusal instead of completing.
    #[test]
    fn cancellation_is_honored_mid_run() {
        let cancel = CancelFlag::default();
        cancel.cancel();
        // A graph with an edge, so BFS enters its per-edge loop where the
        // flag is polled.
        let res = run_fixed_rule(
            &Bfs,
            vec![
                TestInput::new(vec!["fr", "to"], vec![vec![s("a"), s("b")]]),
                TestInput::new(vec!["id"], vec![vec![s("a")], vec![s("b")]]),
                TestInput::new(vec!["start"], vec![vec![s("a")]]),
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
            let rows = inputs.into_iter().next().unwrap().rows;
            reply
                .send(Ok(NamedRows::new(vec!["x".to_string()], rows)))
                .unwrap();
        });
        let got = run_fixed_rule(
            &rule,
            vec![TestInput::new(vec!["x"], vec![vec![s("z")]])],
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap();
        assert_eq!(got, vec![vec![s("z")]]);
        handle.join().unwrap();
    }

    /// F3: the intern site refuses, typed, at the 2^32-node bound instead
    /// of truncating `indices.len() as u32`. The bound cannot be reached
    /// end-to-end (it needs ~4B distinct node values), so this pins the
    /// boundary arithmetic of the factored check:
    ///   - 0 interned                → id 0
    ///   - u32::MAX - 1 interned     → id u32::MAX - 1 (the last mintable;
    ///     u32::MAX stays free as the Dijkstra back-pointer sentinel)
    ///   - u32::MAX interned         → GraphTooLargeError (`as u32` would
    ///     collided with the u32::MAX Dijkstra sentinel; the wrap
    ///     to id 0 happens one node later)
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
                out.put(vec![DataValue::from(true)])?;
                Ok(())
            }
        }
        run_fixed_rule(
            &Probe,
            vec![TestInput::new(
                vec!["fr", "to"],
                vec![
                    vec![DataValue::from("a"), DataValue::from("b")],
                    vec![DataValue::from("b"), DataValue::from("c")],
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
            vec![TestInput::new(vec!["x"], vec![vec![DataValue::from("a")]])],
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
                vec![vec![s("a"), s("b"), DataValue::from(f64::NAN)]],
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
}
