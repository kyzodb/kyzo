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
use crate::exec::fixpoint::delta_store::EpochStore;
use crate::exec::fixpoint::delta_store::TupleInIter;
use crate::exec::fixpoint::eval::AtomOccurrence;
use crate::exec::op::batch_ops::{
    BATCH_ROWS, Batch, BatchIter, BatchTupleFilter, conjunction_pred,
};
use crate::exec::op::join::{push_join_premises, push_joined_row};
use crate::exec::plan::program::MagicSymbol;
use itertools::Either::{Left, Right};
use itertools::Itertools;
use kyzo_model::SourceSpan;
use kyzo_model::program::expr::{Expr, compute_bounds};
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::DataValue;
use kyzo_model::value::ScanBound;
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
pub struct TempStoreRA {
    pub bindings: Vec<Symbol>,
    pub(crate) storage_key: MagicSymbol,
    /// This atom's position among its body's `Rule`/`NegatedRule` atoms —
    /// the key `delta_from` compares against (`compile.rs`'s shared
    /// numbering, `atom_occurrences`).
    pub(crate) occurrence: AtomOccurrence,
    /// Residual predicates; binding indices filled in place at compile.
    pub(crate) filters: Vec<Expr>,
    pub span: SourceSpan,
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
            Left(
                storage
                    .delta_all_iter()?
                    .map(|t| t.try_into_tuple().map_err(Into::into)),
            )
        } else {
            Right(
                storage
                    .all_iter()?
                    .map(|t| t.try_into_tuple().map_err(Into::into)),
            )
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
    #[allow(clippy::too_many_arguments)] // sealed admit/join/digest doors carry explicit domain params
    #[allow(clippy::too_many_arguments)] // sealed admit/join/digest doors carry explicit domain params
    pub(crate) fn prefix_join_batched<'a>(
        &'a self,
        left: BatchIter<'a>,
        (left_join_indices, right_join_indices): (Vec<usize>, Vec<usize>),
        eliminate_indices: BTreeSet<usize>,
        delta_rule: Option<AtomOccurrence>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
        want_premises: bool,
        capture_right_as_premise: bool,
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
        let informative = l_bound.iter().any(|b| *b != ScanBound::Least)
            || u_bound.iter().any(|b| *b != ScanBound::Greatest);
        let bounds = if informative {
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
            want_premises,
            capture_right_as_premise,
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
    bounds: Option<(Vec<ScanBound>, Vec<ScanBound>)>,
    eliminate_indices: BTreeSet<usize>,
    /// The left batch currently being probed, and the cursor into it.
    cur: Option<(Batch, usize)>,
    /// The in-flight match iterator for the row at `cur`'s cursor.
    active: Option<Box<dyn Iterator<Item = TupleInIter<'a>> + 'a>>,
    want_premises: bool,
    capture_right_as_premise: bool,
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

    fn probe(
        &self,
        left_row: &[DataValue],
    ) -> Result<Box<dyn Iterator<Item = TupleInIter<'a>> + 'a>> {
        Ok(match &self.bounds {
            Some((l_bound, u_bound)) => {
                // Range-bounded probes carry residual filter bounds beyond
                // the prefix: the merged bound tuples must own (the level
                // merge filters per row against them).
                let prefix_bounds = || {
                    self.left_to_prefix_indices
                        .iter()
                        .map(|i| ScanBound::Value(left_row[*i].clone()))
                };
                let lower: Vec<ScanBound> =
                    prefix_bounds().chain(l_bound.iter().cloned()).collect();
                let upper: Vec<ScanBound> =
                    prefix_bounds().chain(u_bound.iter().cloned()).collect();
                if self.scan_epoch {
                    Box::new(self.storage.delta_range_iter(&lower, &upper, true)?)
                } else {
                    Box::new(self.storage.range_iter(&lower, &upper, true)?)
                }
            }
            // The plain prefix probe is zero-clone: cursors are built up
            // front through the projection and nothing is retained.
            None => Box::new(self.storage.prefix_iter_projected(
                left_row,
                &self.left_to_prefix_indices,
                self.scan_epoch,
            )?),
        })
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
                    let Some((b, idx)) = self.cur.as_ref() else {
                        return Some(Err(crate::exec::op::PlanInvariantError(
                            "temp-store join left cursor missing after batch advance",
                        )
                        .into()));
                    };
                    match b.row(*idx) {
                        Ok(r) => r,
                        Err(e) => return Some(Err(e.into())),
                    }
                };
                match self.probe(left_row) {
                    Ok(it) => self.active = Some(it),
                    Err(e) => return Some(Err(e)),
                }
            }

            let Some((b, idx)) = self.cur.as_ref() else {
                return Some(Err(crate::exec::op::PlanInvariantError(
                    "temp-store join left cursor missing while probing",
                )
                .into()));
            };
            let left_idx = *idx;
            let left_owned = match b.row(left_idx) {
                Ok(r) => r.to_vec(),
                Err(e) => return Some(Err(e.into())),
            };
            let left_premises = if self.want_premises {
                b.row_premises(left_idx)
            } else {
                Vec::new()
            };
            let Some(active) = self.active.as_mut() else {
                return Some(Err(crate::exec::op::PlanInvariantError(
                    "temp-store join active probe missing after setup",
                )
                .into()));
            };
            let mut exhausted = false;
            while out.len() < BATCH_ROWS {
                match active.next() {
                    None => {
                        exhausted = true;
                        break;
                    }
                    Some(found) => {
                        let found_tuple = match found.try_into_tuple() {
                            Ok(t) => t,
                            Err(e) => return Some(Err(e.into())),
                        };
                        let mut keep = true;
                        if !self.inner.filters.is_empty() {
                            for p in self.inner.filters.iter() {
                                match crate::exec::expr::eval_pred(p, &found_tuple) {
                                    Ok(true) => {}
                                    Ok(false) => {
                                        keep = false;
                                        break;
                                    }
                                    Err(e) => return Some(Err(e)),
                                }
                            }
                        }
                        if keep {
                            let right_premise =
                                if self.want_premises && self.capture_right_as_premise {
                                    Some(found_tuple.clone())
                                } else {
                                    None
                                };
                            if let Err(e) = push_joined_row(
                                &mut out,
                                &left_owned,
                                found_tuple.into_iter(),
                                &self.eliminate_indices,
                            ) {
                                return Some(Err(e));
                            }
                            push_join_premises(
                                &mut out,
                                left_premises.clone(),
                                right_premise.as_ref(),
                                self.want_premises,
                            );
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
