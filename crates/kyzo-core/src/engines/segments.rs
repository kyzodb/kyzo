/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Current-state segments: a derived acceleration structure over a base
//! relation — like its engine siblings (HNSW, FTS) a *rebuildable index*,
//! never a second source of truth.
//!
//! A [`Segment`] is one relation's current state — the rows a plain scan at
//! the session's read coordinate returns, in key order — decoded ONCE into
//! a dense row-major buffer (the execution currency's own shape), so
//! repeated scans and prefix probes are memcpy runs and binary searches
//! over decoded values instead of LSM iteration plus per-row memcmp decode.
//!
//! ## Freshness (story #305 T5)
//!
//! Serving uses the projection machine's generation contract:
//! [`Generation::classify`] keeps a matching [`Sealed`] handle or yields
//! [`Stale`] — never a `Watermark(u64)` equality re-check, and never
//! `Option` standing for staleness. Absence of an installed segment is the
//! distinct [`SegmentMiss::Absent`]; a generation mismatch is
//! [`SegmentMiss::Stale`].
//!
//! - **Readers witness AFTER opening a snapshot**
//!   ([`SegmentEngine::generation_after_snapshot`]): the open
//!   [`ReadTx`](crate::storage::ReadTx) is required by signature so a
//!   generation cannot be sampled before the snapshot that must see it.
//! - **Writers bump BEFORE commit** ([`SegmentEngine::bump_before_commit`]):
//!   if a commit's rows are visible to any snapshot, its bump already
//!   happened. (A rolled-back transaction that bumped merely advances the
//!   counter early — safe.)
//! - **Rebuild is gated** ([`SegmentEngine::should_build`]): N consecutive
//!   misses at the SAME live generation (issue #82) — declining to build is
//!   always sound; a segment is optional speed.
//!
//! Handles are [`SegmentHandle`] (`Arc` over the dense buffer): an orphaned
//! segment stays alive for readers mid-scan and is freed when the last one
//! drops.

use std::collections::BTreeMap;
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use crate::data::value::{DataValue, RelationId, Tuple};
use crate::engines::projection::{Generation, ProjectionBuilder, Sealed, Stale};
use crate::storage::ReadTx;

/// Consecutive misses at one live generation before a rebuild is admitted.
/// One miss declines (not yet proven stable); the second at the same
/// generation builds. Alternating write+read never crosses this gate.
const REBUILD_AFTER_STABLE_MISSES: u32 = 2;

/// The execution path's segment context: `OFF` (tests, benches, callers
/// without a session) or a borrow of the session's engine. `Copy`, so it
/// threads through operator dispatch like `tx` does.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Segments<'a>(pub(crate) Option<&'a SegmentEngine>);

impl Segments<'_> {
    pub(crate) const OFF: Segments<'static> = Segments(None);
}

/// Shared handle to a sealed segment body — `Clone` is an `Arc` bump, so
/// [`Sealed<SegmentHandle>`] can be classified without copying the dense
/// buffer.
#[derive(Clone, Debug)]
pub(crate) struct SegmentHandle(Arc<Segment>);

impl SegmentHandle {
    pub(crate) fn arc(&self) -> Arc<Segment> {
        self.0.clone()
    }
}

impl Deref for SegmentHandle {
    type Target = Segment;

    fn deref(&self) -> &Segment {
        &self.0
    }
}

/// Why a live generation could not serve an installed segment.
///
/// Distinguishable from a successful serve: absence is not staleness, and
/// staleness is [`Stale`] — never collapsed into `Option::None`.
#[derive(Debug)]
pub(crate) enum SegmentMiss {
    /// No sealed segment is installed for the relation.
    Absent,
    /// An installed segment's generation does not match the live stamp.
    Stale(Stale<SegmentHandle>),
}

/// The session's segment engine: per-relation write counters plus the
/// sealed-segment cache. One per [`Db`](crate::runtime::db::Db), shared by
/// all its transactions.
#[derive(Debug, Default)]
pub(crate) struct SegmentEngine {
    marks: Mutex<BTreeMap<RelationId, Arc<AtomicU64>>>,
    segments: Mutex<BTreeMap<RelationId, Sealed<SegmentHandle>>>,
    misses: Mutex<BTreeMap<RelationId, (Generation, u32)>>,
}

impl SegmentEngine {
    fn slot(&self, relation: RelationId) -> Arc<AtomicU64> {
        let mut marks = self.marks.lock().expect("generation lock poisoned");
        marks.entry(relation).or_default().clone()
    }

    /// Live generation for `relation`, sampled only after `tx` proves a
    /// snapshot is open — the racy "read mark then open snapshot" order is
    /// unrepresentable.
    pub(crate) fn generation_after_snapshot(
        &self,
        _tx: &impl ReadTx,
        relation: RelationId,
    ) -> Generation {
        Generation::new(self.slot(relation).load(AtomicOrdering::Acquire))
    }

    /// Record an imminent committed write to `relation` — called BEFORE the
    /// storage commit, so a bump precedes any snapshot that can see the
    /// write. A subsequent rollback leaves a harmless early counter advance.
    ///
    /// # Non-transition (proven invariant)
    ///
    /// Story #302 T4: not a Domain-style consuming field transition. The engine is an
    /// `Arc`-shared capability handle ([`crate::runtime::db::Db::segments`]);
    /// the per-relation counter is an [`AtomicU64`] under a stated
    /// concurrent-access requirement (many writers/readers across
    /// transactions). The bump is a monotone counter advance on that shared
    /// atomic — reassignment of a Domain-like proof is unrepresentable
    /// without breaking Arc sharing. `rust-state` Capability Handle permits
    /// Atomic/Mutex only with that concurrency need; `rust-verbs` Transition
    /// (field reassignment) applies to single-owner handles, which this is
    /// not.
    pub(crate) fn bump_before_commit(&self, relation: RelationId) {
        self.slot(relation).fetch_add(1, AtomicOrdering::AcqRel);
    }

    /// Serve a sealed segment when `live` matches its stamped generation.
    ///
    /// Freshness is [`Generation::classify`]: matching keeps [`Sealed`];
    /// mismatch yields [`Stale`] (wrapped as [`SegmentMiss::Stale`]). No
    /// installed segment is [`SegmentMiss::Absent`] — never `Option` for
    /// either case.
    pub(crate) fn get(
        &self,
        relation: RelationId,
        live: Generation,
    ) -> Result<SegmentHandle, SegmentMiss> {
        let segments = self.segments.lock().expect("segment lock poisoned");
        let Some(sealed) = segments.get(&relation) else {
            return Err(SegmentMiss::Absent);
        };
        match live.classify(sealed.clone()) {
            Ok(fresh) => Ok(fresh.into_kind()),
            Err(stale) => Err(SegmentMiss::Stale(stale)),
        }
    }

    /// Admit a rebuild after [`REBUILD_AFTER_STABLE_MISSES`] consecutive
    /// misses at the same live generation. A write (generation advance)
    /// resets the streak. Declining is always sound — the caller falls
    /// back to storage.
    pub(crate) fn should_build(&self, relation: RelationId, live: Generation) -> bool {
        let mut misses = self.misses.lock().expect("miss lock poisoned");
        match misses.get_mut(&relation) {
            Some((recorded, count)) if *recorded == live => {
                *count = count.saturating_add(1);
                *count >= REBUILD_AFTER_STABLE_MISSES
            }
            _ => {
                misses.insert(relation, (live, 1));
                false
            }
        }
    }

    /// Seal `segment` at `generation` and install it, replacing any
    /// predecessor (which stays alive for readers holding its handle).
    pub(crate) fn install(
        &self,
        relation: RelationId,
        segment: Segment,
        generation: Generation,
    ) -> SegmentHandle {
        let handle = SegmentHandle(Arc::new(segment));
        let sealed = ProjectionBuilder::new(handle.clone()).seal(generation);
        self.segments
            .lock()
            .expect("segment lock poisoned")
            .insert(relation, sealed);
        self.misses
            .lock()
            .expect("miss lock poisoned")
            .remove(&relation);
        handle
    }

    /// Drop a relation's segment, miss streak, and write-counter slot
    /// outright (destructive schema ops: the relation identity itself is
    /// being reused or destroyed).
    pub(crate) fn evict(&self, relation: RelationId) {
        self.segments
            .lock()
            .expect("segment lock poisoned")
            .remove(&relation);
        self.misses
            .lock()
            .expect("miss lock poisoned")
            .remove(&relation);
        self.marks
            .lock()
            .expect("generation lock poisoned")
            .remove(&relation);
    }
}

/// One relation's current state as dense row-major flattened rows in key
/// order — the execution currency's own layout, so serving a scan is a
/// contiguous copy and a prefix probe is a binary search over decoded
/// values.
#[derive(Debug)]
pub(crate) struct Segment {
    values: Vec<DataValue>,
    /// `offsets[i]` is the END of row `i` in `values` (row 0 starts at 0).
    offsets: Vec<u32>,
}

/// The checked cast at the heart of [`Segment::build`]'s row-offset
/// encoding, factored out so the u32 boundary is unit-testable without
/// materializing the ~4.3 billion `DataValue`s it would take to actually
/// reach it through `build`. `None` past `u32::MAX`, exactly where a bare
/// `as u32` would silently wrap and corrupt every later row's boundaries.
fn checked_row_end(values_len: usize) -> Option<u32> {
    u32::try_from(values_len).ok()
}

impl Segment {
    /// Build from the rows a plain current-state scan produced, in the
    /// scan's own (key) order.
    ///
    /// `None` iff the relation's flattened value count would overflow the
    /// `u32` offset encoding (~4.3 billion `DataValue`s in one relation): a
    /// segment is an optional, rebuildable acceleration structure, so
    /// declining to build one is semantically free — the caller falls back
    /// to a normal scan, which has no such ceiling.
    pub(crate) fn build(rows: impl Iterator<Item = Tuple>) -> Option<Self> {
        let mut values = Vec::new();
        let mut offsets = Vec::new();
        for row in rows {
            values.extend(row);
            offsets.push(checked_row_end(values.len())?);
        }
        Some(Segment { values, offsets })
    }

    pub(crate) fn len(&self) -> usize {
        self.offsets.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    pub(crate) fn row(&self, i: usize) -> &[DataValue] {
        let start = if i == 0 {
            0
        } else {
            self.offsets[i - 1] as usize
        };
        &self.values[start..self.offsets[i] as usize]
    }

    /// Compare stored row `i` against a probe prefix, coordinate-wise.
    fn cmp_prefix(&self, i: usize, prefix: &[DataValue]) -> std::cmp::Ordering {
        let row = self.row(i);
        for (v, p) in row.iter().zip(prefix) {
            match v.cmp(p) {
                std::cmp::Ordering::Equal => continue,
                ord => return ord,
            }
        }
        std::cmp::Ordering::Equal
    }

    /// The half-open row range whose leading columns equal `prefix`.
    pub(crate) fn prefix_range(&self, prefix: &[DataValue]) -> std::ops::Range<usize> {
        let lo = self.partition(|s, i| s.cmp_prefix(i, prefix) == std::cmp::Ordering::Less);
        let hi = self.partition(|s, i| s.cmp_prefix(i, prefix) != std::cmp::Ordering::Greater);
        lo..hi.max(lo)
    }

    /// First index where `pred` turns false (rows are pred-partitioned by
    /// key order).
    fn partition(&self, pred: impl Fn(&Self, usize) -> bool) -> usize {
        let mut lo = 0usize;
        let mut hi = self.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if pred(self, mid) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sim::SimStorage;
    use crate::storage::Storage;

    fn row(vals: &[i64]) -> Tuple {
        vals.iter().map(|&i| DataValue::from(i)).collect()
    }

    #[test]
    fn prefix_ranges_match_linear_scan_across_types() {
        let mut rows: Vec<Tuple> = (0..7)
            .flat_map(|a| (0..5).map(move |b| row(&[a, b * 2])))
            .collect();
        // mixed-type tail rows: cross-type order is DataValue's declaration
        // order, same as the scan produces.
        rows.push(Tuple::from_vec(vec![
            DataValue::from(7),
            DataValue::from("x"),
        ]));
        rows.push(Tuple::from_vec(vec![
            DataValue::from(7),
            DataValue::from("y"),
        ]));
        let s = Segment::build(rows.clone().into_iter()).unwrap();
        for a in -1..9 {
            let probe = [DataValue::from(a)];
            let got = s.prefix_range(&probe);
            let want_lo = rows
                .iter()
                .position(|r| r[0] >= probe[0])
                .unwrap_or(rows.len());
            let want_hi = rows
                .iter()
                .position(|r| r[0] > probe[0])
                .unwrap_or(rows.len());
            assert_eq!(got, want_lo..want_hi.max(want_lo), "prefix a={a}");
            for i in got {
                assert_eq!(s.row(i), rows[i].as_slice());
            }
        }
    }

    #[test]
    fn empty_segment_probes_cleanly() {
        let s = Segment::build(std::iter::empty()).unwrap();
        assert!(s.is_empty());
        assert!(s.prefix_range(&[DataValue::from(1)]).is_empty());
    }

    /// F7: the row-offset boundary arithmetic, isolated from the ~4.3
    /// billion-value relation it would otherwise take to reach. Before the
    /// fix, `Segment::build` computed this as a bare `values.len() as u32`,
    /// which wraps silently past `u32::MAX` and corrupts every later row's
    /// boundary; `checked_row_end` is that exact cast, made total.
    #[test]
    fn checked_row_end_boundary() {
        assert_eq!(checked_row_end(0), Some(0));
        assert_eq!(checked_row_end(1), Some(1));
        assert_eq!(checked_row_end(u32::MAX as usize), Some(u32::MAX));
        assert_eq!(checked_row_end(u32::MAX as usize + 1), None);
        assert_eq!(checked_row_end(usize::MAX), None);
    }

    #[test]
    fn classify_serves_matching_generation_and_rejects_stale() {
        let db = SimStorage::new(3);
        let rtx = db.read_tx().unwrap();
        let engine = SegmentEngine::default();
        let relation = RelationId::new(1).expect("below cap");
        let live = engine.generation_after_snapshot(&rtx, relation);
        let handle = engine.install(
            relation,
            Segment::build(std::iter::once(row(&[1, 2]))).unwrap(),
            live,
        );
        assert_eq!(handle.row(0), &[DataValue::from(1), DataValue::from(2)]);
        assert!(engine.get(relation, live).is_ok());

        engine.bump_before_commit(relation);
        let after = engine.generation_after_snapshot(&rtx, relation);
        assert!(matches!(
            engine.get(relation, after),
            Err(SegmentMiss::Stale(_))
        ));
    }

    #[test]
    fn rebuild_gated_by_stable_miss_streak() {
        let db = SimStorage::new(5);
        let rtx = db.read_tx().unwrap();
        let engine = SegmentEngine::default();
        let relation = RelationId::new(2).expect("below cap");
        let live = engine.generation_after_snapshot(&rtx, relation);

        assert!(
            !engine.should_build(relation, live),
            "first miss declines"
        );
        assert!(
            engine.should_build(relation, live),
            "second stable miss admits build"
        );

        engine.bump_before_commit(relation);
        let next = engine.generation_after_snapshot(&rtx, relation);
        assert!(
            !engine.should_build(relation, next),
            "write resets the streak"
        );
    }

    #[test]
    fn alternating_writes_never_cross_rebuild_gate() {
        let db = SimStorage::new(7);
        let engine = SegmentEngine::default();
        let relation = RelationId::new(3).expect("below cap");
        for _ in 0..20 {
            engine.bump_before_commit(relation);
            let rtx = db.read_tx().unwrap();
            let live = engine.generation_after_snapshot(&rtx, relation);
            assert!(
                !engine.should_build(relation, live),
                "write-interleaved single miss must never admit a build"
            );
        }
    }
}
