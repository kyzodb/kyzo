/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Which projection build is live for readers right now.
//!
//! A [`Segment`] is one relation's current state — the rows a plain scan at
//! the session's read coordinate returns, in key order — decoded ONCE into
//! a dense row-major buffer. Witness equality is the entire serving
//! criterion; Arc-held orphans serve mid-scan readers to completion.
//!
//! Validity / rebuild gate lives in [`super::residency`]. Declining-is-always-
//! sound: the u32 decline and the gate decline are one doctrine — a
//! projection is optional speed.
//!
//! ## Segments::OFF (carried note)
//!
//! [`Segments::OFF`] threading is door plumbing the #120 operator wiring
//! replaces — carry the note; do not entrench the threading as design.

use std::collections::BTreeMap;
use std::ops::Deref;
use std::sync::{Arc, Mutex};

use crate::project::projection::{Generation, ProjectionBuilder, ResidentIndexKey, Sealed, Stale};
use crate::project::residency::Residency;
use crate::store::ReadTx;
use kyzo_model::value::{DataValue, RelationId, Tuple};

/// The execution path's segment context: `OFF` (tests, benches, callers
/// without a session) or a borrow of the session's engine. `Copy`, so it
/// threads through operator dispatch like `tx` does.
#[derive(Clone, Copy, Debug, Default)]
pub struct Segments<'a>(pub(crate) Option<&'a SegmentEngine>);

impl Segments<'_> {
    pub const OFF: Segments<'static> = Segments(None);
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

/// The session's segment engine: residency discipline plus the sealed-segment
/// cache. One per [`Db`](crate::session::db::Db), shared by all its
/// transactions.
#[derive(Debug, Default)]
pub(crate) struct SegmentEngine {
    residency: Residency,
    segments: Mutex<BTreeMap<ResidentIndexKey, Sealed<SegmentHandle>>>,
}

impl SegmentEngine {
    /// Live generation for `relation` — see [`Residency::witness_after_snapshot`].
    pub(crate) fn witness_after_snapshot(
        &self,
        tx: &impl ReadTx,
        relation: RelationId,
    ) -> Generation {
        self.residency.witness_after_snapshot(tx, relation)
    }

    /// Record an imminent committed write — see [`Residency::bump_before_commit`].
    pub(crate) fn bump_before_commit(&self, relation: RelationId) {
        self.residency.bump_before_commit(relation);
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
        let key = ResidentIndexKey::for_relation(relation);
        let segments = self.segments.lock().expect("segment lock poisoned");
        let Some(sealed) = segments.get(&key) else {
            return Err(SegmentMiss::Absent);
        };
        match live.classify(sealed.clone()) {
            Ok(fresh) => Ok(fresh.into_kind()),
            Err(stale) => {
                // Stale carries installed vs expected — read those fields so
                // Absent vs Stale stays a carried fact. classify contract
                // broken → decline as Absent (typed), not an unreachable arm.
                if stale.generation() == live || stale.expected() != live {
                    return Err(SegmentMiss::Absent);
                }
                Err(SegmentMiss::Stale(stale))
            }
        }
    }

    /// Admit a rebuild — see [`Residency::should_build`].
    pub(crate) fn should_build(&self, relation: RelationId, live: Generation) -> bool {
        self.residency.should_build(relation, live)
    }

    /// Seal `segment` at `generation` and install it, replacing any
    /// predecessor (which stays alive for readers holding its handle).
    pub(crate) fn install(
        &self,
        relation: RelationId,
        segment: Segment,
        generation: Generation,
    ) -> SegmentHandle {
        let key = ResidentIndexKey::for_relation(relation);
        let handle = SegmentHandle(Arc::new(segment));
        let sealed = ProjectionBuilder::new(handle.clone()).seal(generation);
        self.segments
            .lock()
            .expect("segment lock poisoned")
            .insert(key, sealed);
        self.residency.clear_miss(relation);
        handle
    }

    /// Drop a relation's segment, miss streak, and write-counter slot
    /// outright (destructive schema ops: the relation identity itself is
    /// being reused or destroyed).
    pub(crate) fn evict(&self, relation: RelationId) {
        let key = ResidentIndexKey::for_relation(relation);
        self.segments
            .lock()
            .expect("segment lock poisoned")
            .remove(&key);
        self.residency.forget(relation);
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
    match u32::try_from(values_len) {
        Ok(v) => Some(v),
        Err(_) => None,
    }
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

    /// Row at index `i`, or `None` when out of bounds.
    pub(crate) fn row(&self, i: usize) -> Option<&[DataValue]> {
        (i < self.len()).then(|| self.row_at(i))
    }

    /// INVARIANT(segment_row_in_bounds): `i < self.len()`.
    fn row_at(&self, i: usize) -> &[DataValue] {
        debug_assert!(i < self.len());
        let start = if i == 0 {
            0
        } else {
            self.offsets[i - 1] as usize
        };
        &self.values[start..self.offsets[i] as usize]
    }

    /// Compare stored row `i` against a probe prefix, coordinate-wise.
    fn cmp_prefix(&self, i: usize, prefix: &[DataValue]) -> std::cmp::Ordering {
        let row = self.row_at(i);
        for (v, p) in row.iter().zip(prefix) {
            match v.cmp(p) {
                std::cmp::Ordering::Equal => continue,
                ord @ std::cmp::Ordering::Less | ord @ std::cmp::Ordering::Greater => return ord,
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
    use crate::store::Storage;
    use crate::store::sim::SimStorage;

    fn row(vals: &[i64]) -> Tuple {
        vals.iter().map(|&i| DataValue::from(i)).collect()
    }

    #[test]
    fn prefix_ranges_match_linear_scan_across_types() {
        let mut rows: Vec<Tuple> = (0..7)
            .flat_map(|a| (0..5).map(move |b| row(&[a, b * 2])))
            .collect();
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
            let want_lo = match rows.iter().position(|r| r[0] >= probe[0]) {
                Some(v) => v,
                None => rows.len(),
            };
            let want_hi = match rows.iter().position(|r| r[0] > probe[0]) {
                Some(v) => v,
                None => rows.len(),
            };
            assert_eq!(got, want_lo..want_hi.max(want_lo), "prefix a={a}");
            for i in got {
                assert_eq!(s.row(i), Some(rows[i].as_slice()));
            }
        }
    }

    #[test]
    fn empty_segment_probes_cleanly() {
        let s = Segment::build(std::iter::empty()).unwrap();
        assert_eq!(s.len(), 0);
        assert!(s.prefix_range(&[DataValue::from(1)]).is_empty());
    }

    /// F7: the row-offset boundary arithmetic, isolated from the ~4.3
    /// billion-value relation it would otherwise take to reach.
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
        let live = engine.witness_after_snapshot(&rtx, relation);
        let handle = engine.install(
            relation,
            Segment::build(std::iter::once(row(&[1, 2]))).unwrap(),
            live,
        );
        assert_eq!(
            handle.row(0),
            Some([DataValue::from(1), DataValue::from(2)].as_slice())
        );
        assert!(engine.get(relation, live).is_ok());

        engine.bump_before_commit(relation);
        let after = engine.witness_after_snapshot(&rtx, relation);
        assert!(matches!(
            engine.get(relation, after),
            Err(SegmentMiss::Stale(_))
        ));
    }

    #[test]
    fn orphan_evict_held_arc_still_serves() {
        let db = SimStorage::new(3);
        let rtx = db.read_tx().unwrap();
        let engine = SegmentEngine::default();
        let relation = RelationId::new(5).expect("below cap");
        let live = engine.witness_after_snapshot(&rtx, relation);
        let handle = engine.install(
            relation,
            Segment::build(std::iter::once(row(&[9]))).unwrap(),
            live,
        );
        engine.evict(relation);
        // Held Arc still serves after eviction.
        assert_eq!(handle.row(0), Some([DataValue::from(9)].as_slice()));
        assert!(matches!(
            engine.get(relation, live),
            Err(SegmentMiss::Absent)
        ));
    }
}
