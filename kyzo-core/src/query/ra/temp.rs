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

//! Rule-store scans: totals and deltas of the semi-naive fixpoint,
//! including the prefix-probe batch join against level stores.
// ─────────────────────────────────────────────────────────────────────────
// TempStoreRA: in-memory rule store scans (the semi-naive seam)
// ─────────────────────────────────────────────────────────────────────────

use super::epoch_store_of;
use crate::data::expr::{Bytecode, Expr, compute_bounds, eval_bytecode_pred};
use crate::data::program::MagicSymbol;
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::tuple::Tuple;
use crate::data::value::DataValue;
use crate::query::batch_ops::{BATCH_ROWS, Batch, BatchIter, BatchTupleFilter, conjunction_pred};
use crate::query::eval::AtomOccurrence;
use crate::query::levels::EpochStore;
use crate::query::ra::join::push_joined_row;
use crate::query::temp_store::TupleInIter;
use itertools::Either::{Left, Right};
use itertools::Itertools;
use miette::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;

/// A scan of one rule's [`EpochStore`]. This variant is where the
/// semi-naive delta discipline is *implemented*: when `delta_rule` names
/// THIS occurrence (this atom's specific position in its body, not merely
/// its store name — a store mentioned twice in one body compiles to two
/// `TempStoreRA`s with two distinct [`AtomOccurrence`]s), the scan reads
/// the delta instead of the total, per the `RuleBody` seam contract.
#[derive(Debug)]
pub(crate) struct TempStoreRA {
    pub(crate) bindings: Vec<Symbol>,
    pub(crate) storage_key: MagicSymbol,
    /// This atom's position among its body's `Rule`/`NegatedRule` atoms —
    /// the key `delta_from` compares against (`compile.rs`'s shared
    /// numbering, `atom_occurrences`).
    pub(crate) occurrence: AtomOccurrence,
    pub(crate) filters: Vec<Expr>,
    pub(crate) filters_bytecodes: Vec<(Vec<Bytecode>, SourceSpan)>,
    pub(crate) span: SourceSpan,
}

impl TempStoreRA {
    pub(crate) fn fill_binding_indices_and_compile(&mut self) -> Result<()> {
        let bindings: BTreeMap<_, _> = self
            .bindings
            .iter()
            .cloned()
            .enumerate()
            .map(|(a, b)| (b, a))
            .collect();
        for e in self.filters.iter_mut() {
            e.fill_binding_indices(&bindings)?;
            self.filters_bytecodes.push((e.compile()?, e.span()))
        }
        Ok(())
    }
    /// Batched form of [`iter`](Self::iter): the same store scan (delta or
    /// total by the same `scan_epoch` test, same pushed-down filters, same
    /// order), accumulated into [`Batch`]es with a reused eval stack.
    pub(crate) fn iter_batched<'a>(
        &'a self,
        delta_rule: Option<AtomOccurrence>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    ) -> Result<BatchIter<'a>> {
        let storage = epoch_store_of(stores, &self.storage_key)?;
        let scan_epoch = delta_rule == Some(self.occurrence);
        let it = if scan_epoch {
            Left(storage.delta_all_iter().map(|t| Ok(t.into_tuple())))
        } else {
            Right(storage.all_iter().map(|t| Ok(t.into_tuple())))
        };
        Ok(Box::new(BatchTupleFilter {
            inner: it,
            pred: conjunction_pred(&self.filters),
            pending_err: None,
        }))
    }

    /// Anti-join against this store. Always reads the TOTAL, never the
    /// delta — negation over a delta would resurrect rows already ruled
    /// out (the seam contract: "negated occurrences always read totals").
    /// Prefix join: for each left tuple, prefix-scan this store on the
    /// join values. Reads the delta when `delta_rule` names this store.
    /// Batched form of [`prefix_join`](Self::prefix_join): the left side is
    /// consumed as batches — no `Tuple` is minted for a left row — and each
    /// matched row is written once, straight into the output batch
    /// ([`push_joined_row`]). Kept as its own implementation (rather than
    /// routed through [`PrefixProbeBatchJoin`]) because a filter-less match
    /// here is a borrowed [`TupleInIter`], not an owned `Tuple`: the
    /// iterator path's no-filter branch clones its columns directly into
    /// the joined row without ever materializing the store's own row as a
    /// `Tuple`, and this preserves that.
    ///
    /// `compute_bounds` is a pure function of `self.filters` and the
    /// trailing bindings — never of the left row — so it is computed once
    /// here; the row-at-a-time path recomputed it on every row that took
    /// the bounded branch (same result, redundant work).
    pub(crate) fn prefix_join_batched<'a>(
        &'a self,
        left: BatchIter<'a>,
        (left_join_indices, right_join_indices): (Vec<usize>, Vec<usize>),
        eliminate_indices: BTreeSet<usize>,
        delta_rule: Option<AtomOccurrence>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    ) -> Result<BatchIter<'a>> {
        let storage = epoch_store_of(stores, &self.storage_key)?;

        let mut right_invert_indices = right_join_indices.iter().enumerate().collect_vec();
        right_invert_indices.sort_by_key(|(_, b)| **b);
        let left_to_prefix_indices = right_invert_indices
            .into_iter()
            .map(|(a, _)| left_join_indices[a])
            .collect_vec();
        let scan_epoch = delta_rule == Some(self.occurrence);
        let other_bindings = self.bindings.get(right_join_indices.len()..).unwrap_or(&[]);
        let (l_bound, u_bound) = if self.filters.is_empty() {
            Default::default()
        } else {
            compute_bounds(&self.filters, other_bindings).unwrap_or_default()
        };
        let bounds = if !l_bound.iter().all(|v| *v == DataValue::Null)
            || !u_bound.iter().all(|v| *v == DataValue::Bot)
        {
            Some((l_bound, u_bound))
        } else {
            None
        };

        Ok(Box::new(TempStorePrefixBatchJoin {
            left,
            inner: self,
            storage,
            scan_epoch,
            left_to_prefix_indices,
            bounds,
            eliminate_indices,
            cur: None,
            active: None,
            stack: vec![],
        }))
    }
}

/// The batched native prefix join over a rule store — see
/// [`TempStoreRA::prefix_join_batched`]. Order matches the row-at-a-time
/// path exactly: one probe per left row, in left-stream order, each
/// yielding its store matches in the store's own (memcmp-ordered for
/// stored relations, canonical for temp stores) order.
struct TempStorePrefixBatchJoin<'a> {
    left: BatchIter<'a>,
    inner: &'a TempStoreRA,
    storage: &'a EpochStore,
    scan_epoch: bool,
    left_to_prefix_indices: Vec<usize>,
    bounds: Option<(Tuple, Tuple)>,
    eliminate_indices: BTreeSet<usize>,
    /// The left batch currently being probed, and the cursor into it.
    cur: Option<(Batch, usize)>,
    /// The in-flight match iterator for the row at `cur`'s cursor.
    active: Option<Box<dyn Iterator<Item = TupleInIter<'a>> + 'a>>,
    stack: Vec<DataValue>,
}

impl<'a> TempStorePrefixBatchJoin<'a> {
    fn advance_left_batch(&mut self) -> Result<bool> {
        loop {
            match self.left.next() {
                None => {
                    self.cur = None;
                    return Ok(false);
                }
                Some(Err(e)) => return Err(e),
                Some(Ok(b)) => {
                    if !b.is_empty() {
                        self.cur = Some((b, 0));
                        return Ok(true);
                    }
                }
            }
        }
    }

    fn probe(&self, left_row: &[DataValue]) -> Box<dyn Iterator<Item = TupleInIter<'a>> + 'a> {
        match &self.bounds {
            Some((l_bound, u_bound)) => {
                // Range-bounded probes carry residual filter bounds beyond
                // the prefix: the merged bound tuples must own (the level
                // merge filters per row against them).
                let prefix: Tuple = self
                    .left_to_prefix_indices
                    .iter()
                    .map(|i| left_row[*i].clone())
                    .collect();
                let mut lower = prefix.clone();
                lower.extend(l_bound.iter().cloned());
                let mut upper = prefix;
                upper.extend(u_bound.iter().cloned());
                if self.scan_epoch {
                    Box::new(self.storage.delta_range_iter(&lower, &upper, true))
                } else {
                    Box::new(self.storage.range_iter(&lower, &upper, true))
                }
            }
            // The plain prefix probe is zero-clone: cursors are built up
            // front through the projection and nothing is retained.
            None => Box::new(self.storage.prefix_iter_projected(
                left_row,
                &self.left_to_prefix_indices,
                self.scan_epoch,
            )),
        }
    }
}

impl<'a> Iterator for TempStorePrefixBatchJoin<'a> {
    type Item = Result<Batch>;
    fn next(&mut self) -> Option<Self::Item> {
        let mut out = Batch::new();
        loop {
            if self.active.is_none() {
                let need_new_batch = match &self.cur {
                    Some((b, idx)) => *idx >= b.len(),
                    None => true,
                };
                if need_new_batch {
                    match self.advance_left_batch() {
                        Ok(false) => return if out.is_empty() { None } else { Some(Ok(out)) },
                        Ok(true) => {}
                        Err(e) => return Some(Err(e)),
                    }
                }
                let left_row = {
                    let (b, idx) = self.cur.as_ref().unwrap();
                    b.row(*idx)
                };
                self.active = Some(self.probe(left_row));
            }

            let (b, idx) = self.cur.as_ref().unwrap();
            let left_row = b.row(*idx);
            let active = self.active.as_mut().unwrap();
            let mut exhausted = false;
            while out.len() < BATCH_ROWS {
                match active.next() {
                    None => {
                        exhausted = true;
                        break;
                    }
                    Some(found) => {
                        if self.inner.filters.is_empty() {
                            if let Err(e) = push_joined_row(
                                &mut out,
                                left_row,
                                found.into_iter(),
                                &self.eliminate_indices,
                            ) {
                                return Some(Err(e));
                            }
                        } else {
                            let found_tuple = found.into_tuple();
                            let mut keep = true;
                            for (p, span) in self.inner.filters_bytecodes.iter() {
                                match eval_bytecode_pred(p, &found_tuple, &mut self.stack, *span) {
                                    Ok(true) => {}
                                    Ok(false) => {
                                        keep = false;
                                        break;
                                    }
                                    Err(e) => return Some(Err(e)),
                                }
                            }
                            if keep
                                && let Err(e) = push_joined_row(
                                    &mut out,
                                    left_row,
                                    found_tuple.iter(),
                                    &self.eliminate_indices,
                                )
                            {
                                return Some(Err(e));
                            }
                        }
                    }
                }
            }
            if !exhausted {
                return Some(Ok(out));
            }
            self.active = None;
            if let Some((_, idx)) = self.cur.as_mut() {
                *idx += 1;
            }
        }
    }
}
