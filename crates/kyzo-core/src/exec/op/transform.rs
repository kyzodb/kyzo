/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0); split out of query/ra.rs — see query/ra/mod.rs for the
 * transformation record.
 */

//! Unary streaming transforms: column reorder, residual filters,
//! and unification (computed-column extension).
// ─────────────────────────────────────────────────────────────────────────
// ReorderRA: column permutation
// ─────────────────────────────────────────────────────────────────────────

use super::{BatchFilter, PlanInvariantError, RelAlgebra};
use crate::exec::fixpoint::delta_store::EpochStore;
use crate::exec::fixpoint::eval::AtomOccurrence;
use crate::exec::op::batch_ops::{Batch, BatchIter};
use crate::exec::op::join::{eliminate_from_tuple, get_eliminate_indices};
use crate::exec::plan::program::MagicSymbol;
use crate::project::current::Segments;
use crate::store::ReadTx;
use kyzo_model::SourceSpan;
use kyzo_model::program::expr::Expr;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::DataValue;
use kyzo_model::value::Tuple;
use miette::{Diagnostic, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use thiserror::Error;

/// Permute the parent's columns into `new_order`. Only ever the plan root
/// (aligning body bindings to the rule head); never a join RHS, which
/// [`RelAlgebra::join`] enforces at construction.
#[derive(Debug)]
pub struct ReorderRA {
    pub relation: Box<RelAlgebra>,
    pub(crate) new_order: Vec<Symbol>,
}

impl ReorderRA {
    pub(crate) fn bindings(&self) -> Vec<Symbol> {
        self.new_order.clone()
    }
    /// Batched form of [`iter`](Self::iter): the same positional gather,
    /// applied batch by batch. Reorder is only ever the plan root, so this
    /// is the last transform before the eval callback.
    pub(crate) fn iter_batched<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        delta_rule: Option<AtomOccurrence>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
        segments: Segments<'a>,
        want_premises: bool,
    ) -> Result<BatchIter<'a>> {
        let old_order = self.relation.bindings_after_eliminate();
        let old_order_indices: BTreeMap<_, _> = old_order
            .into_iter()
            .enumerate()
            .map(|(k, v)| (v, k))
            .collect();
        let reorder_indices = self
            .new_order
            .iter()
            .map(|k| {
                old_order_indices.get(k).copied().ok_or(PlanInvariantError(
                    "reorder columns are not a permutation of the parent's",
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Box::new(
            self.relation
                .iter_batched(tx, delta_rule, stores, segments, want_premises)?
                .map(move |batch| -> Result<Batch> {
                    let src = batch?;
                    let tracking = src.premises().is_some();
                    let mut out = Batch::new();
                    for (i, tuple) in src.iter_rows().enumerate() {
                        let reordered: Tuple =
                            reorder_indices.iter().map(|j| tuple[*j].clone()).collect();
                        out.push(reordered);
                        if tracking {
                            out.push_premise_list(src.row_premises(i));
                        }
                    }
                    Ok(out)
                }),
        ))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// FilteredRA: Expr predicate filters
// ─────────────────────────────────────────────────────────────────────────

/// Keep only tuples satisfying every compiled predicate.
///
/// One owner for residual filters: binding indices are filled in place
/// (`fill_binding_indices_and_compile`); evaluation reads the same field.
pub struct FilteredRA {
    pub parent: Box<RelAlgebra>,
    pub(crate) filters: Vec<Expr>,
    pub(crate) to_eliminate: BTreeSet<Symbol>,
    pub span: SourceSpan,
}

impl FilteredRA {
    pub(crate) fn do_eliminate_temp_vars(&mut self, used: &BTreeSet<Symbol>) -> Result<()> {
        for binding in self.parent.bindings_before_eliminate() {
            if !used.contains(&binding) {
                self.to_eliminate.insert(binding.clone());
            }
        }
        let mut nxt = used.clone();
        for e in self.filters.iter() {
            nxt.extend(e.bindings()?);
        }
        self.parent.eliminate_temp_vars(&nxt)?;
        Ok(())
    }

    pub(crate) fn fill_binding_indices_and_compile(&mut self) -> Result<()> {
        let parent_bindings: BTreeMap<_, _> = self
            .parent
            .bindings_after_eliminate()
            .into_iter()
            .enumerate()
            .map(|(a, b)| (b, a))
            .collect();
        for e in self.filters.iter_mut() {
            e.fill_binding_indices(&parent_bindings)?;
        }
        Ok(())
    }
    /// Batched form of [`iter`](Self::iter): pull the parent's batch stream
    /// and filter each batch in place over a contiguous buffer with one
    /// reused stack, then drop eliminated columns. Same predicate order,
    /// same elimination, same row order as the iterator path.
    pub(crate) fn iter_batched<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        delta_rule: Option<AtomOccurrence>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
        segments: Segments<'a>,
        want_premises: bool,
    ) -> Result<BatchIter<'a>> {
        let bindings = self.parent.bindings_after_eliminate();
        let eliminate_indices = get_eliminate_indices(&bindings, &self.to_eliminate);
        Ok(Box::new(BatchFilter {
            parent: self
                .parent
                .iter_batched(tx, delta_rule, stores, segments, want_premises)?,
            filters: &self.filters,
            eliminate_indices,
        }))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// UnificationRA: computed columns
// ─────────────────────────────────────────────────────────────────────────

/// How a computed column attaches to each parent tuple.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnificationKind {
    /// One output row: `binding = expr`.
    Single,
    /// One output row per list element: `binding in expr`.
    Spread,
}

/// Append one computed column per tuple (`binding = expr`), or — when
/// [`UnificationKind::Spread`] (`binding in expr`) — one output tuple per
/// element of the list `expr` evaluates to.
pub struct UnificationRA {
    pub parent: Box<RelAlgebra>,
    pub(crate) binding: Symbol,
    pub(crate) expr: Expr,
    pub(crate) kind: UnificationKind,
    pub(crate) to_eliminate: BTreeSet<Symbol>,
    pub span: SourceSpan,
}

impl UnificationRA {
    pub(crate) fn fill_binding_indices_and_compile(&mut self) -> Result<()> {
        let parent_bindings: BTreeMap<_, _> = self
            .parent
            .bindings_after_eliminate()
            .into_iter()
            .enumerate()
            .map(|(a, b)| (b, a))
            .collect();
        self.expr.fill_binding_indices(&parent_bindings)?;
        Ok(())
    }

    pub(crate) fn do_eliminate_temp_vars(&mut self, used: &BTreeSet<Symbol>) -> Result<()> {
        for binding in self.parent.bindings_before_eliminate() {
            if !used.contains(&binding) {
                self.to_eliminate.insert(binding.clone());
            }
        }
        let mut nxt = used.clone();
        nxt.extend(self.expr.bindings()?);
        self.parent.eliminate_temp_vars(&nxt)?;
        Ok(())
    }

    /// Batched unification: ONE columnar evaluation of the bound
    /// expression per parent batch, then rows extend positionally. The
    /// [`UnificationKind::Spread`] form flattens each row's list result in
    /// row order, exactly like the row path.
    pub(crate) fn iter_batched<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        delta_rule: Option<AtomOccurrence>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
        segments: Segments<'a>,
        want_premises: bool,
    ) -> Result<BatchIter<'a>> {
        let mut bindings = self.parent.bindings_after_eliminate();
        bindings.push(self.binding.clone());
        let eliminate_indices = get_eliminate_indices(&bindings, &self.to_eliminate);
        let parent = self
            .parent
            .iter_batched(tx, delta_rule, stores, segments, want_premises)?;
        let ra = self;
        let it = parent
            .map(move |batch| -> Result<Batch> {
                let batch = batch?;
                let tracking = batch.premises().is_some();
                let rows: Vec<&[DataValue]> = batch.iter_rows().collect();
                let width = rows.first().map_or(0, |r| r.len());
                let owned_rows: Vec<Tuple> =
                    rows.iter().map(|r| Tuple::from_vec(r.to_vec())).collect();
                let columns = crate::exec::expr::batch::ColumnBatch::from_rows(owned_rows, width)?;
                let values = crate::exec::expr::eval::eval_expr_batched(
                    &ra.expr,
                    &columns,
                    &crate::exec::expr::batch::Selection::all(rows.len())?,
                )?;
                let mut out = Batch::new();
                let mut emit = |row_idx: usize, row: &[DataValue], v: DataValue| -> Result<()> {
                    let mut ret: Tuple = Tuple::from_vec(row.to_vec());
                    ret.push(v);
                    out.push(eliminate_from_tuple(ret, &eliminate_indices));
                    if tracking {
                        out.push_premise_list(batch.row_premises(row_idx));
                    }
                    Ok(())
                };
                match ra.kind {
                    UnificationKind::Spread => {
                        for (i, (row, list)) in rows.iter().zip(values).enumerate() {
                            let items = list.get_slice().ok_or_else(|| {
                                #[derive(Debug, Error, Diagnostic)]
                                #[error("Invalid spread unification")]
                                #[diagnostic(code(eval::invalid_spread_unif))]
                                #[diagnostic(help(
                                    "Spread unification requires a list at the right"
                                ))]
                                struct BadSpreadUnification(#[label] SourceSpan);

                                BadSpreadUnification(ra.span)
                            })?;
                            for item in items {
                                emit(i, row, item.clone())?;
                            }
                        }
                    }
                    UnificationKind::Single => {
                        for (i, (row, v)) in rows.iter().zip(values).enumerate() {
                            emit(i, row, v)?;
                        }
                    }
                }
                Ok(out)
            })
            // An operator never yields an empty batch (a spread over
            // all-empty lists could otherwise produce one).
            .filter(|b| !matches!(b, Ok(b) if b.is_empty()));
        Ok(Box::new(it))
    }
}
