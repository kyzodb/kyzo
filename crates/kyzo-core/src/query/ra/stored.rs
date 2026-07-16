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
use crate::engines::segments::{Segment, SegmentEngine, SegmentMiss, Segments};
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
    /// snapshot's generation — served if [`Generation::classify`] keeps
    /// the sealed handle, built and installed on a STABLE miss (the build
    /// pays the same storage scan the read would have paid, plus one dense
    /// flatten; every subsequent scan at the same generation skips LSM
    /// iteration and memcmp decode entirely).
    ///
    /// `None` on any decline: either [`Segment::build`]'s (the relation is
    /// too large for the `u32` offset encoding) or
    /// [`SegmentEngine::should_build`]'s (this miss hasn't yet proven the
    /// generation is holding still). Either way the caller falls back to
    /// an unsegmented scan or point probe. That `Option` is acceleration
    /// availability — not staleness (`get` answers staleness as
    /// [`SegmentMiss::Stale`]).
    fn segment_at(&self, tx: &impl ReadTx, engine: &SegmentEngine) -> Result<Option<Arc<Segment>>> {
        let live = engine.generation_after_snapshot(tx, self.storage.id);
        match engine.get(self.storage.id, live) {
            Ok(handle) => return Ok(Some(handle.arc())),
            Err(SegmentMiss::Absent) | Err(SegmentMiss::Stale(_)) => {}
        }
        if !engine.should_build(self.storage.id, live) {
            return Ok(None);
        }
        let mut rows = Vec::new();
        for t in self.storage.scan_all(tx) {
            rows.push(t?);
        }
        Ok(Segment::build(rows.into_iter())
            .map(|seg| engine.install(self.storage.id, seg, live).arc()))
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
    /// generation/build (so the probe loop itself pays zero synchronization)
    /// and `engines/segments.rs`'s module docs for why a served segment is
    /// current-state-sound. The BOUNDED prefix case (residual filter bounds
    /// on trailing key columns) stays on the storage scan. A `TempStore` or
    /// as-of (`StoredWithValidity`) right side never reaches here — a
    /// segment is a current-state-only acceleration structure, and
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
        let mut right_invert_indices = right_join_indices.iter().enumerate().collect_vec();
        right_invert_indices.sort_by_key(|(_, b)| **b);
        let left_to_prefix_indices = right_invert_indices
            .into_iter()
            .map(|(a, _)| left_join_indices[a])
            .collect_vec();

        let key_len = self.storage.metadata.keys.len();

        // Classified/built once here, not per probe: the same discipline
        // `iter_batched`'s full-scan dispatch uses, so the soundness
        // argument (served segment ⇒ generation classify Ok ⇒ no intervening
        // write) is established once per plan-node instantiation and the
        // hot probe loop below never touches the generation counter again.
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
                                return Ok(Box::new(iter::once(Ok(Tuple::from_vec(
                                    found.to_vec(),
                                )))));
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
                        None => match &seg {
                            Some(s) => {
                                let prefix: Vec<DataValue> = left_to_prefix_indices
                                    .iter()
                                    .map(|&i| left_row[i].clone())
                                    .collect();
                                let s = s.clone();
                                let range = s.prefix_range(&prefix);
                                Box::new(range.map(move |i| Ok(Tuple::from_vec(s.row(i).to_vec()))))
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

// ─────────────────────────────────────────────────────────────────────────
// Issue #82: the rebuild gate (`engines/segments.rs`'s `should_build`),
// exercised through the real `StoredRA::iter_batched` production path
// rather than the engine in isolation — proving the gate's contract holds
// for the code the OLTP mixed-op caller actually runs, not just its
// building block.
// ─────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod segment_gate_tests {

    use smartstring::SmartString;

    use super::*;
    use crate::data::program::InputRelationHandle;
    use crate::data::relation::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
    use crate::data::value::ValidityTs;
    use crate::engines::segments::SegmentEngine;
    use crate::runtime::relation::create_relation;
    use crate::storage::fjall::new_fjall_storage;
    use crate::storage::{Storage, WriteTx};

    fn sp() -> SourceSpan {
        SourceSpan(0, 0)
    }
    fn sym(name: &str) -> Symbol {
        Symbol::new(name, sp())
    }
    fn v(i: i64) -> DataValue {
        DataValue::from(i)
    }

    fn col(name: &str, coltype: ColType) -> ColumnDef {
        ColumnDef {
            name: SmartString::from(name),
            typing: NullableColType {
                coltype,
                nullable: false,
            },
            default_gen: None,
        }
    }

    fn input_handle(
        name: &str,
        keys: Vec<ColumnDef>,
        non_keys: Vec<ColumnDef>,
    ) -> InputRelationHandle {
        let key_bindings = keys.iter().map(|c| sym(&c.name)).collect();
        let dep_bindings = non_keys.iter().map(|c| sym(&c.name)).collect();
        InputRelationHandle {
            name: sym(name),
            metadata: StoredRelationMetadata { keys, non_keys },
            key_bindings,
            dep_bindings,
            span: sp(),
        }
    }

    /// A two-column (`k`, `v`) `Facts` relation and the `StoredRA` scanning
    /// it whole — the production shape a point-read compiles down to when
    /// its bound key is exactly the relation's key (`prefix_join_batched`'s
    /// point-lookup case shares `segment_at` with this one; both dispatch
    /// through the same gate).
    fn kv_relation(db: &impl Storage, name: &str) -> crate::runtime::relation::RelationHandle {
        let mut tx = db.write_tx().unwrap();
        let handle = create_relation(
            &mut tx,
            input_handle(
                name,
                vec![col("k", ColType::Int)],
                vec![col("v", ColType::Int)],
            ),
            KeyspaceKind::Facts,
        )
        .unwrap();
        tx.commit().unwrap();
        handle
    }

    fn ra_over(handle: &crate::runtime::relation::RelationHandle) -> StoredRA {
        StoredRA {
            bindings: vec![sym("k"), sym("v")],
            storage: handle.clone(),
            filters: vec![],
            filters_bytecodes: vec![],
            span: sp(),
        }
    }

    /// Collect a `StoredRA`'s whole-relation scan under a given segment
    /// context — the same `iter_batched` door the compiled plan uses.
    fn rows(ra: &StoredRA, tx: &impl ReadTx, segments: Segments<'_>) -> Vec<Vec<DataValue>> {
        let mut out = Vec::new();
        for b in ra.iter_batched(tx, segments).unwrap() {
            let b = b.unwrap();
            out.extend((0..b.len()).map(|i| b.row(i).to_vec()));
        }
        out
    }

    fn put(
        db: &impl Storage,
        handle: &crate::runtime::relation::RelationHandle,
        engine: &SegmentEngine,
        k: i64,
        val: i64,
    ) {
        let mut wtx = db.write_tx().unwrap();
        handle
            .put_fact(&mut wtx, &[v(k), v(val)], ValidityTs::from_raw(0), sp())
            .unwrap();
        // Writers bump BEFORE commit (`engines/segments.rs` module doc's
        // soundness pairing) — mirrors exactly what `runtime/db.rs`'s
        // mutate path does around the real storage commit.
        engine.bump_before_commit(handle.id);
        wtx.commit().unwrap();
    }

    /// (a)/(c) end to end: a caller whose every read is immediately
    /// preceded by a committed write to the same relation (issue #82's
    /// exact shape) must never see the gate build — each write resets the
    /// stable-miss streak to zero before the next read's single miss can
    /// cross the threshold — and every gated-out read must still answer
    /// correctly (never stale, never an error).
    #[test]
    fn segment_gate_never_builds_under_write_interleaved_reads() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let handle = kv_relation(&db, "gate_rw");
        let engine = SegmentEngine::default();
        let ra = ra_over(&handle);

        for i in 0..50i64 {
            put(&db, &handle, &engine, 0, i);

            let rtx = db.read_tx().unwrap();
            let live = engine.generation_after_snapshot(&rtx, handle.id);
            assert_eq!(
                rows(&ra, &rtx, Segments(Some(&engine))),
                vec![vec![v(0), v(i)]],
                "iteration {i}: a gated-out read must still answer correctly"
            );
            // Checked AFTER the read, at THIS read's own generation (no
            // intervening write yet) — this is what actually proves the
            // read just performed did not build. A pre-read check here
            // would pass vacuously: `get`'s classify filter already
            // guarantees a PRIOR iteration's segment (sealed at a now-stale
            // generation) can never serve, regardless of whether the gate
            // itself is behaving.
            assert!(
                matches!(engine.get(handle.id, live), Err(SegmentMiss::Absent)),
                "iteration {i}: a write-interleaved read must never have just built a segment"
            );
        }
    }

    /// (b) a read-only run's gate crosses threshold at exactly the
    /// documented point: the first miss declines (not yet proven stable),
    /// the second miss at the SAME generation builds and installs, and every
    /// read after that — including the one that triggered the build —
    /// answers correctly.
    #[test]
    fn segment_gate_builds_after_stable_read_run_and_serves() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let handle = kv_relation(&db, "gate_stable");
        let engine = SegmentEngine::default();
        for k in 0..5i64 {
            put(&db, &handle, &engine, k, k * 10);
        }
        let ra = ra_over(&handle);
        let expected: Vec<Vec<DataValue>> = (0..5i64).map(|k| vec![v(k), v(k * 10)]).collect();

        let rtx = db.read_tx().unwrap();
        let live = engine.generation_after_snapshot(&rtx, handle.id);

        assert_eq!(rows(&ra, &rtx, Segments(Some(&engine))), expected);
        assert!(
            matches!(engine.get(handle.id, live), Err(SegmentMiss::Absent)),
            "first miss must decline to build"
        );

        assert_eq!(rows(&ra, &rtx, Segments(Some(&engine))), expected);
        assert!(
            engine.get(handle.id, live).is_ok(),
            "second stable miss (no intervening write) must build and install"
        );

        assert_eq!(
            rows(&ra, &rtx, Segments(Some(&engine))),
            expected,
            "a served segment must answer identically to the scan it was built from"
        );
    }

    /// (c), isolated from (a): one miss, then a write, then the very next
    /// miss must restart the streak at 1 (decline) rather than inherit the
    /// prior count — proven through `segment_at`'s production call site,
    /// not just the raw counter.
    #[test]
    fn segment_gate_reset_by_intervening_write_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let handle = kv_relation(&db, "gate_reset");
        let engine = SegmentEngine::default();
        put(&db, &handle, &engine, 1, 10);
        let ra = ra_over(&handle);

        let rtx1 = db.read_tx().unwrap();
        assert_eq!(
            rows(&ra, &rtx1, Segments(Some(&engine))),
            vec![vec![v(1), v(10)]]
        );

        put(&db, &handle, &engine, 2, 20);

        let rtx2 = db.read_tx().unwrap();
        let live2 = engine.generation_after_snapshot(&rtx2, handle.id);
        let expected2 = vec![vec![v(1), v(10)], vec![v(2), v(20)]];
        assert_eq!(rows(&ra, &rtx2, Segments(Some(&engine))), expected2);
        assert!(
            matches!(engine.get(handle.id, live2), Err(SegmentMiss::Absent)),
            "the first miss at the post-write generation must decline, not inherit the pre-write count"
        );
        assert_eq!(rows(&ra, &rtx2, Segments(Some(&engine))), expected2);
        assert!(
            engine.get(handle.id, live2).is_ok(),
            "the second miss at the post-write generation builds normally"
        );
    }

    /// (d) the point of the gate: it may only ever decide WHEN to build,
    /// never WHAT to serve. Over a seeded, deterministic mix of writes and
    /// read-runs of varying length (so both the gated-out fallback and the
    /// built-and-served path are exercised many times, in an order neither
    /// this test nor the gate controls), a segment-context read must be
    /// byte-identical to a segments-off read, and both must match an
    /// independently maintained model — never a run of the same machine
    /// checked against itself.
    #[test]
    fn segment_served_and_gated_out_answers_match_across_seeded_mixed_workload() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let handle = kv_relation(&db, "gate_diff");
        let engine = SegmentEngine::default();
        let ra = ra_over(&handle);

        // xorshift64*: deterministic, dependency-free, seeded so the
        // workload reproduces exactly on any run.
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next_u64 = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        let mut model: BTreeMap<i64, i64> = BTreeMap::new();
        for round in 0..40u32 {
            let r = next_u64();
            let key = (r % 12) as i64;
            let val = ((r >> 16) % 1000) as i64;
            put(&db, &handle, &engine, key, val);
            model.insert(key, val);

            // 1..=4 reads this round: a lone read after the write always
            // stays gated-out; a run of 3-4 crosses the rebuild threshold
            // partway through and serves from the segment for the rest.
            let n_reads = 1 + (next_u64() % 4);
            for read_i in 0..n_reads {
                let rtx = db.read_tx().unwrap();
                let expected: Vec<Vec<DataValue>> =
                    model.iter().map(|(&k, &val)| vec![v(k), v(val)]).collect();
                let off = rows(&ra, &rtx, Segments::OFF);
                let on = rows(&ra, &rtx, Segments(Some(&engine)));
                assert_eq!(
                    off, expected,
                    "round {round} read {read_i}: segments-off diverged from the model"
                );
                assert_eq!(
                    on, off,
                    "round {round} read {read_i}: segment-context answer diverged from segments-off"
                );
            }
        }
    }
}
