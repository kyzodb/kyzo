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
use crate::data::expr::{Bytecode, Expr, compute_bounds};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::{AsOf, DataValue};
use crate::engines::segments::{Segment, SegmentEngine, Segments};
use crate::query::batch_ops::refine_batch;
use crate::query::batch_ops::{
    Batch, BatchIter, BatchScanFilter, BatchTupleFilter, conjunction_pred,
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
use std::sync::Arc;

/// A scan of a stored relation at the current time, through the landed
/// scan surface of `runtime/relation.rs` (every method takes the
/// transaction to read; see that module's routing note for temp
/// relations).
#[derive(Debug)]
pub(crate) struct StoredRA {
    pub(crate) bindings: Vec<Symbol>,
    pub(crate) storage: RelationHandle,
    pub(crate) filters: Vec<Expr>,
    pub(crate) filters_bytecodes: Vec<(Vec<Bytecode>, SourceSpan)>,
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
            self.filters_bytecodes.push((e.compile()?, e.span()));
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
        match self.storage.keyspace_kind {
            KeyspaceKind::Facts => {
                if let Segments(Some(engine)) = segments {
                    // `None`: the relation is too large to fit the
                    // segment's `u32` offset encoding. A segment is an
                    // optional acceleration structure, so this falls
                    // through to the unsegmented scan below (which has no
                    // such ceiling) instead of refusing the query.
                    if let Some(seg) = self.segment_at(tx, engine)? {
                        return Ok(Box::new(SegmentScanBatches {
                            seg,
                            next_row: 0,
                            pred: conjunction_pred(&self.filters),
                            done: false,
                        }));
                    }
                }
                Ok(Box::new(BatchTupleFilter {
                    inner: self.storage.scan_all(tx),
                    pred: conjunction_pred(&self.filters),
                    pending_err: None,
                }))
            }
            KeyspaceKind::AlgorithmState => Ok(Box::new(BatchScanFilter {
                inner: self.storage.scan_all_raw(tx),
                pred: conjunction_pred(&self.filters),
                pending_err: None,
            })),
        }
    }

    /// The session's current-state segment for this relation at THIS
    /// snapshot's witness — served if valid, built and installed on miss
    /// (the build pays the same storage scan the read would have paid,
    /// plus one dense flatten; every subsequent scan at the same witness
    /// skips LSM iteration and memcmp decode entirely).
    ///
    /// `None` iff [`Segment::build`] declined (the relation is too large
    /// for the `u32` offset encoding): the caller falls back to an
    /// unsegmented scan. That path re-scans storage, but only in a case
    /// that needs ~4.3 billion values in one relation to reach at all.
    fn segment_at(&self, tx: &impl ReadTx, engine: &SegmentEngine) -> Result<Option<Arc<Segment>>> {
        let witness = engine.witness_after_snapshot(tx, self.storage.id);
        if let Some(seg) = engine.get(self.storage.id, witness) {
            return Ok(Some(seg));
        }
        let mut rows = Vec::new();
        for t in self.storage.scan_all(tx) {
            rows.push(t?);
        }
        Ok(Segment::build(rows.into_iter(), witness)
            .map(|seg| engine.install(self.storage.id, seg)))
    }

    /// Batched form of [`prefix_join`](Self::prefix_join) (which dispatches
    /// to [`point_lookup_join`](Self::point_lookup_join) internally): the
    /// left side is consumed as batches, and `probe` — built once here,
    /// exactly mirroring the row-at-a-time dispatch — is handed a
    /// `&[DataValue]` slice per left row through [`PrefixProbeBatchJoin`].
    ///
    /// Current-state (`Facts`) probes are served from this relation's
    /// segment when `segments` carries an engine: the point-lookup case and
    /// the plain (unbounded) prefix case both become a binary search over
    /// the dense decoded buffer instead of a bitemporal seek — see
    /// [`segment_at`](Self::segment_at) for the once-per-instantiation
    /// witness/build (so the probe loop itself pays zero synchronization)
    /// and `engines/segments.rs`'s module docs for why a served segment is
    /// current-state-sound. The BOUNDED prefix case (residual filter bounds
    /// on trailing key columns) stays on the storage scan: on the workload
    /// that motivated this conversion (`tc.kz`'s recursive join, which
    /// carries no residual filters) it is cold, so converting it now would
    /// spend risk on a path this pass has no evidence for. A `TempStore` or
    /// as-of (`StoredWithValidity`) right side never reaches here — a
    /// segment is a current-state-only acceleration structure, and
    /// [`StoredWithValidityRA::prefix_join_batched`] has its own probe with
    /// no segment argument at all.
    ///
    /// Every match this relation's storage decodes is already an owned
    /// `Tuple` (the storage layer's own decode boundary, unavoidable and
    /// identical on both paths); what this saves is the left row's
    /// `Tuple` and the joined row's intermediate `Tuple` — both now
    /// written once, straight into the output batch.
    pub(crate) fn prefix_join_batched<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        left: BatchIter<'a>,
        (left_join_indices, right_join_indices): (Vec<usize>, Vec<usize>),
        eliminate_indices: BTreeSet<usize>,
        segments: Segments<'a>,
    ) -> Result<BatchIter<'a>> {
        let mut right_invert_indices = right_join_indices.iter().enumerate().collect_vec();
        right_invert_indices.sort_by_key(|(_, b)| **b);
        let left_to_prefix_indices = right_invert_indices
            .into_iter()
            .map(|(a, _)| left_join_indices[a])
            .collect_vec();

        let key_len = self.storage.metadata.keys.len();

        // Witnessed/built once here, not per probe: the same discipline
        // `iter_batched`'s full-scan dispatch uses, so the soundness
        // argument (served segment ⇒ witness equality ⇒ no intervening
        // write) is established once per plan-node instantiation and the
        // hot probe loop below never touches the watermark again.
        let seg = match (self.storage.keyspace_kind, segments) {
            (KeyspaceKind::Facts, Segments(Some(engine))) => self.segment_at(tx, engine)?,
            _ => None,
        };

        let probe: Box<dyn FnMut(&[DataValue]) -> Result<TupleIter<'a>> + 'a> =
            if left_to_prefix_indices.len() >= key_len {
                if let Some(seg) = seg {
                    Box::new(move |left_row: &[DataValue]| -> Result<TupleIter<'a>> {
                        let prefix: Vec<DataValue> = left_to_prefix_indices[..key_len]
                            .iter()
                            .map(|&i| left_row[i].clone())
                            .collect();
                        for i in seg.prefix_range(&prefix) {
                            let found = seg.row(i);
                            let mut matches = true;
                            for (lk, rk) in left_join_indices.iter().zip(right_join_indices.iter())
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
                                    matches = false;
                                    break;
                                }
                            }
                            if matches {
                                return Ok(Box::new(iter::once(Ok(found.to_vec()))));
                            }
                        }
                        Ok(Box::new(iter::empty()))
                    })
                } else {
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
                }
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
                    if !l_bound.iter().all(|v| *v == DataValue::Null)
                        || !u_bound.iter().all(|v| *v == DataValue::Bot)
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
                        None => match &seg {
                            Some(s) => {
                                let prefix: Vec<DataValue> = left_to_prefix_indices
                                    .iter()
                                    .map(|&i| left_row[i].clone())
                                    .collect();
                                let s = s.clone();
                                let range = s.prefix_range(&prefix);
                                Box::new(range.map(move |i| Ok(s.row(i).to_vec())))
                            }
                            None => self.storage.scan_prefix_projected(
                                tx,
                                left_row,
                                &left_to_prefix_indices,
                            ),
                        },
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
            stack: vec![],
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
    pub(crate) filters_bytecodes: Vec<(Vec<Bytecode>, SourceSpan)>,
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
            self.filters_bytecodes.push((e.compile()?, e.span()));
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
            if !l_bound.iter().all(|v| *v == DataValue::Null)
                || !u_bound.iter().all(|v| *v == DataValue::Bot)
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
            stack: vec![],
        }))
    }
}

/// Serves a relation's full current-state scan from its segment: dense
/// row runs copied straight into batches, residual filters applied through
/// the standard refinement — observationally identical to the storage scan.
struct SegmentScanBatches {
    seg: Arc<Segment>,
    next_row: usize,
    pred: Option<Expr>,
    done: bool,
}

impl Iterator for SegmentScanBatches {
    type Item = Result<Batch>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.done {
                return None;
            }
            if self.next_row >= self.seg.len() {
                self.done = true;
                return None;
            }
            let mut out = Batch::new();
            while self.next_row < self.seg.len() && !out.is_full() {
                let row = self.seg.row(self.next_row);
                if let Err(e) = out.push_with(|buf| {
                    buf.extend_from_slice(row);
                    Ok(())
                }) {
                    self.done = true;
                    return Some(Err(e));
                }
                self.next_row += 1;
            }
            match refine_batch(&self.pred, out) {
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
                Ok(b) if b.is_empty() => continue,
                Ok(b) => return Some(Ok(b)),
            }
        }
    }
}
