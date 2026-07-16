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

//! Stored-relation scans: current state and bitemporal time travel.
// ─────────────────────────────────────────────────────────────────────────
// StoredRA: stored-relation scans
// ─────────────────────────────────────────────────────────────────────────

use super::{StoredRowTooShortError, TupleIter};
use crate::Tuple;
use crate::data::expr::{Expr, compute_bounds};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::{AsOf, DataValue, ScanBound};
use crate::engines::segments::Segments;
use crate::query::batch_ops::{
    BatchIter, BatchScanFilter, BatchTupleFilter, conjunction_pred,
};
use crate::query::ra::join::PrefixProbeBatchJoin;
use crate::runtime::relation::KeyspaceKind;
use crate::runtime::relation::RelationHandle;
use crate::storage::ReadTx;
use itertools::Itertools;
use miette::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::iter;

/// A scan of a stored relation at the current time, through the landed
/// scan surface of `runtime/relation.rs` (every method takes the
/// transaction to read; see that module's routing note for temp
/// relations).
#[derive(Debug)]
pub(crate) struct StoredRA {
    pub(crate) bindings: Vec<Symbol>,
    pub(crate) storage: RelationHandle,
    pub(crate) filters: Vec<Expr>,
    pub(crate) filters_bytecodes: Vec<Expr>,
    pub(crate) span: SourceSpan,
}

impl StoredRA {
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
            self.filters_bytecodes.push(e.clone());
        }
        Ok(())
    }

    /// Join by point lookup: the left tuples bind the relation's full key,
    /// so each left tuple costs one `get` instead of a scan.
    /// Batched form of [`iter`](Self::iter): the same rows in the same
    /// order, grouped into batches. A facts keyspace resolves
    /// bitemporally (its as-of scan seeks, so it cannot be fed a flat raw
    /// byte stream) and its resolved rows are flattened into batches; the
    /// zero-mint raw path remains for algorithm-state keyspaces, and
    /// extending it to as-of resolution needs a raw seek scan on the
    /// storage contract — the columnar leg's next contract change.
    pub(crate) fn iter_batched<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        segments: Segments<'a>,
    ) -> Result<BatchIter<'a>> {
        // Story #305 demolition: Watermark / Option-staleness `get` /
        // `should_build` serving path cut. `segments` is retained in the
        // signature so callers keep threading the session handle; no
        // segment is served until the replacement freshness machine lands.
        let _ = segments;
        match self.storage.keyspace_kind {
            KeyspaceKind::Facts => Ok(Box::new(BatchTupleFilter {
                inner: self.storage.scan_all(tx),
                pred: conjunction_pred(&self.filters),
                pending_err: None,
            })),
            KeyspaceKind::AlgorithmState => Ok(Box::new(BatchScanFilter {
                inner: self.storage.scan_all_raw(tx),
                pred: conjunction_pred(&self.filters),
                pending_err: None,
            })),
        }
    }

    /// Batched form of [`prefix_join`](Self::prefix_join) (which dispatches
    /// to [`point_lookup_join`](Self::point_lookup_join) internally): the
    /// left side is consumed as batches, and `probe` — built once here,
    /// exactly mirroring the row-at-a-time dispatch — is handed a
    /// `&[DataValue]` slice per left row through [`PrefixProbeBatchJoin`].
    ///
    /// Story #305 demolition: the current-state segment probe path that
    /// depended on `SegmentEngine::get`'s Option-staleness answer is cut.
    /// Probes go through storage until typed generation freshness lands.
    /// A `TempStore` or as-of (`StoredWithValidity`) right side never
    /// reached the demolished path — a segment is current-state-only, and
    /// [`StoredWithValidityRA::prefix_join_batched`] has its own probe with
    /// no segment argument at all.
    pub(crate) fn prefix_join_batched<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        left: BatchIter<'a>,
        (left_join_indices, right_join_indices): (Vec<usize>, Vec<usize>),
        eliminate_indices: BTreeSet<usize>,
        segments: Segments<'a>,
    ) -> Result<BatchIter<'a>> {
        let _ = segments;
        let mut right_invert_indices = right_join_indices.iter().enumerate().collect_vec();
        right_invert_indices.sort_by_key(|(_, b)| **b);
        let left_to_prefix_indices = right_invert_indices
            .into_iter()
            .map(|(a, _)| left_join_indices[a])
            .collect_vec();

        let key_len = self.storage.metadata.keys.len();

        let probe: Box<dyn FnMut(&[DataValue]) -> Result<TupleIter<'a>> + 'a> =
            if left_to_prefix_indices.len() >= key_len {
                Box::new(move |left_row: &[DataValue]| -> Result<TupleIter<'a>> {
                    // Zero-clone: the key bytes are encoded straight from
                    // the projected left row; no prefix tuple exists.
                    Ok(
                        match self.storage.current_row_projected(
                            tx,
                            left_row,
                            &left_to_prefix_indices,
                        )? {
                            None => Box::new(iter::empty()),
                            Some(found) => {
                                for (lk, rk) in
                                    left_join_indices.iter().zip(right_join_indices.iter())
                                {
                                    let found_val = found.get(*rk).ok_or_else(|| {
                                        StoredRowTooShortError(
                                            self.storage.name.to_string(),
                                            *rk,
                                            found.len(),
                                            self.span,
                                        )
                                    })?;
                                    if left_row[*lk] != *found_val {
                                        return Ok(Box::new(iter::empty()));
                                    }
                                }
                                Box::new(iter::once(Ok(found)))
                            }
                        },
                    )
                })
            } else {
                let other_bindings = self
                    .bindings
                    .get(right_join_indices.len()..self.storage.metadata.keys.len())
                    .unwrap_or(&[]);
                let bounds = if self.filters.is_empty() {
                    None
                } else {
                    let (l_bound, u_bound) =
                        compute_bounds(&self.filters, other_bindings).unwrap_or_default();
                    if l_bound.iter().any(|b| *b != ScanBound::Least)
                        || u_bound.iter().any(|b| *b != ScanBound::Greatest)
                    {
                        Some((l_bound, u_bound))
                    } else {
                        None
                    }
                };
                Box::new(move |left_row: &[DataValue]| -> Result<TupleIter<'a>> {
                    // Zero-clone: bounds encoded through the projection.
                    Ok(match &bounds {
                        Some((l_bound, u_bound)) => self.storage.scan_bounded_prefix_projected(
                            tx,
                            left_row,
                            &left_to_prefix_indices,
                            l_bound,
                            u_bound,
                        ),
                        None => self.storage.scan_prefix_projected(
                            tx,
                            left_row,
                            &left_to_prefix_indices,
                        ),
                    })
                })
            };

        Ok(Box::new(PrefixProbeBatchJoin {
            left,
            probe,
            filters_bytecodes: &self.filters_bytecodes,
            eliminate_indices,
            cur: None,
            active: None,
        }))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// StoredWithValidityRA: time-travel scans
// ─────────────────────────────────────────────────────────────────────────

/// An as-of scan at an explicit [`AsOf`] coordinate: each fact resolved
/// to what the record said at `as_of.sys` about the world at
/// `as_of.valid`, asserted facts only, as logical rows (the storage
/// contract's `range_skip_scan_tuple`). Any facts relation constructs
/// this — bitemporality is the format, not a schema opt-in.
#[derive(Debug)]
pub(crate) struct StoredWithValidityRA {
    pub(crate) bindings: Vec<Symbol>,
    pub(crate) storage: RelationHandle,
    pub(crate) filters: Vec<Expr>,
    pub(crate) filters_bytecodes: Vec<Expr>,
    pub(crate) as_of: AsOf,
    pub(crate) span: SourceSpan,
}

impl StoredWithValidityRA {
    /// The as-of scan, batch-native: the bitemporal skip scan feeds the
    /// standard accumulate-then-refine batch filter — the same machine the
    /// current-state scan uses, over the time-travel row stream.
    pub(crate) fn iter_batched<'a>(&'a self, tx: &'a impl ReadTx) -> Result<BatchIter<'a>> {
        Ok(Box::new(BatchTupleFilter {
            inner: self.storage.skip_scan_all(tx, self.as_of),
            pred: conjunction_pred(&self.filters),
            pending_err: None,
        }))
    }

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
            self.filters_bytecodes.push(e.clone());
        }
        Ok(())
    }
    /// Batched form of [`prefix_join`](Self::prefix_join): same as
    /// [`StoredRA::prefix_join_batched`] but through the as-of scan surface
    /// (no point-lookup sub-case — the row-at-a-time path has none either).
    pub(crate) fn prefix_join_batched<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        left: BatchIter<'a>,
        (left_join_indices, right_join_indices): (Vec<usize>, Vec<usize>),
        eliminate_indices: BTreeSet<usize>,
    ) -> Result<BatchIter<'a>> {
        let mut right_invert_indices = right_join_indices.iter().enumerate().collect_vec();
        right_invert_indices.sort_by_key(|(_, b)| **b);
        let left_to_prefix_indices = right_invert_indices
            .into_iter()
            .map(|(a, _)| left_join_indices[a])
            .collect_vec();

        let other_bindings = self
            .bindings
            .get(right_join_indices.len()..self.storage.metadata.keys.len())
            .unwrap_or(&[]);
        let bounds = if self.filters.is_empty() {
            None
        } else {
            let (l_bound, u_bound) =
                compute_bounds(&self.filters, other_bindings).unwrap_or_default();
            if l_bound.iter().any(|b| *b != ScanBound::Least)
                || u_bound.iter().any(|b| *b != ScanBound::Greatest)
            {
                Some((l_bound, u_bound))
            } else {
                None
            }
        };

        let probe: Box<dyn FnMut(&[DataValue]) -> Result<TupleIter<'a>> + 'a> =
            Box::new(move |left_row: &[DataValue]| -> Result<TupleIter<'a>> {
                // Zero-clone: bounds encoded through the projection.
                Ok(match &bounds {
                    Some((l_bound, u_bound)) => self.storage.skip_scan_bounded_prefix_projected(
                        tx,
                        left_row,
                        &left_to_prefix_indices,
                        l_bound,
                        u_bound,
                        self.as_of,
                    ),
                    None => self.storage.skip_scan_prefix_projected(
                        tx,
                        left_row,
                        &left_to_prefix_indices,
                        self.as_of,
                    ),
                })
            });

        Ok(Box::new(PrefixProbeBatchJoin {
            left,
            probe,
            filters_bytecodes: &self.filters_bytecodes,
            eliminate_indices,
            cur: None,
            active: None,
        }))
    }
}
