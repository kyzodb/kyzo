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
use crate::data::expr::{Bytecode, Expr};
use crate::data::program::MagicSymbol;
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::tuple::Tuple;
use crate::data::value::DataValue;
use crate::engines::segments::Segments;
use crate::query::batch_ops::{Batch, BatchIter};
use crate::query::eval::AtomOccurrence;
use crate::query::levels::EpochStore;
use crate::query::ra::join::{eliminate_from_tuple, get_eliminate_indices};
use crate::storage::ReadTx;
use miette::{Diagnostic, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use thiserror::Error;

/// Permute the parent's columns into `new_order`. Only ever the plan root
/// (aligning body bindings to the rule head); never a join RHS, which
/// [`RelAlgebra::join`] enforces at construction.
#[derive(Debug)]
pub(crate) struct ReorderRA {
    pub(crate) relation: Box<RelAlgebra>,
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
                .iter_batched(tx, delta_rule, stores, segments)?
                .map(move |batch| -> Result<Batch> {
                    let rows = batch?
                        .into_rows()
                        .into_iter()
                        .map(|tuple| {
                            reorder_indices
                                .iter()
                                .map(|i| tuple[*i].clone())
                                .collect::<Tuple>()
                        })
                        .collect();
                    Ok(Batch::with_rows(rows))
                }),
        ))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// FilteredRA: bytecode predicate filters
// ─────────────────────────────────────────────────────────────────────────

/// Keep only tuples satisfying every compiled predicate.
pub(crate) struct FilteredRA {
    pub(crate) parent: Box<RelAlgebra>,
    pub(crate) filters: Vec<Expr>,
    pub(crate) filters_bytecodes: Vec<(Vec<Bytecode>, SourceSpan)>,
    pub(crate) to_eliminate: BTreeSet<Symbol>,
    pub(crate) span: SourceSpan,
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
            self.filters_bytecodes.push((e.compile()?, e.span()));
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
    ) -> Result<BatchIter<'a>> {
        let bindings = self.parent.bindings_after_eliminate();
        let eliminate_indices = get_eliminate_indices(&bindings, &self.to_eliminate);
        Ok(Box::new(BatchFilter {
            parent: self.parent.iter_batched(tx, delta_rule, stores, segments)?,
            filters: &self.filters_bytecodes,
            eliminate_indices,
            stack: vec![],
        }))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// UnificationRA: computed columns
// ─────────────────────────────────────────────────────────────────────────

/// Append one computed column per tuple (`binding = expr`), or — when
/// `is_multi` (`binding in expr`) — one output tuple per element of the
/// list `expr` evaluates to.
pub(crate) struct UnificationRA {
    pub(crate) parent: Box<RelAlgebra>,
    pub(crate) binding: Symbol,
    pub(crate) expr: Expr,
    pub(crate) expr_bytecode: Vec<Bytecode>,
    pub(crate) is_multi: bool,
    pub(crate) to_eliminate: BTreeSet<Symbol>,
    pub(crate) span: SourceSpan,
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
        self.expr_bytecode = self.expr.compile()?;
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
    /// spread form (`is_multi`) flattens each row's list result in row
    /// order, exactly like the row path.
    pub(crate) fn iter_batched<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        delta_rule: Option<AtomOccurrence>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
        segments: Segments<'a>,
    ) -> Result<BatchIter<'a>> {
        let mut bindings = self.parent.bindings_after_eliminate();
        bindings.push(self.binding.clone());
        let eliminate_indices = get_eliminate_indices(&bindings, &self.to_eliminate);
        let parent = self.parent.iter_batched(tx, delta_rule, stores, segments)?;
        let ra = self;
        let it = parent
            .map(move |batch| -> Result<Batch> {
                let batch = batch?;
                let rows: Vec<&[DataValue]> = batch.iter_rows().collect();
                let width = rows.first().map_or(0, |r| r.len());
                let owned_rows: Vec<Tuple> = rows.iter().map(|r| r.to_vec().into()).collect();
                let columns = crate::data::batch::ColumnBatch::from_rows(owned_rows, width);
                let values = crate::query::vm::eval_expr_batched(
                    &ra.expr,
                    &columns,
                    &crate::data::batch::Selection::all(rows.len()),
                )?;
                let mut out = Batch::new();
                let mut emit = |row: &[DataValue], v: DataValue| -> Result<()> {
                    let mut ret: Tuple = row.to_vec().into();
                    ret.push(v);
                    out.push(eliminate_from_tuple(ret, &eliminate_indices));
                    Ok(())
                };
                if ra.is_multi {
                    for (row, list) in rows.iter().zip(values) {
                        let items = list.get_slice().ok_or_else(|| {
                            #[derive(Debug, Error, Diagnostic)]
                            #[error("Invalid spread unification")]
                            #[diagnostic(code(eval::invalid_spread_unif))]
                            #[diagnostic(help("Spread unification requires a list at the right"))]
                            struct BadSpreadUnification(#[label] SourceSpan);

                            BadSpreadUnification(ra.span)
                        })?;
                        for item in items {
                            emit(row, item.clone())?;
                        }
                    }
                } else {
                    for (row, v) in rows.iter().zip(values) {
                        emit(row, v)?;
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
