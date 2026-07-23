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

use miette::Diagnostic;
use thiserror::Error;

use crate::project::projection::{Generation, ProjectionBuilder, ResidentIndexKey, Sealed, Stale};
use crate::project::residency::{Residency, ResidencyRefuse};
use crate::store::ReadTx;
use kyzo_model::value::{DataValue, RelationId, Tuple};

/// Typed refuses for the segment-cache door.
///
/// Reachable lock failures — never `.expect` / panic costumes on poison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error, Diagnostic)]
pub(crate) enum SegmentRefuse {
    /// Process-local segments mutex poisoned after a panic under lock.
    #[error("SegmentsLockPoisoned: segment cache mutex poisoned")]
    #[diagnostic(code(project::current::segments_lock_poisoned))]
    SegmentsLockPoisoned,
    /// Projection residency marks/misses lock poisoned (see [`ResidencyRefuse`]).
    #[error(transparent)]
    #[diagnostic(transparent)]
    Residency(#[from] ResidencyRefuse),
}

/// The execution path's segment context: `OFF` (tests, benches, callers
/// without a session) or a borrow of the session's engine. `Copy`, so it
/// threads through operator dispatch like `tx` does.
#[derive(Clone, Copy, Debug)]
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
#[derive(Debug)]
pub(crate) struct SegmentEngine {
    residency: Residency,
    segments: Mutex<BTreeMap<ResidentIndexKey, Sealed<SegmentHandle>>>,
}

impl SegmentEngine {
    /// Empty engine — no sealed segments installed.
    pub(crate) fn new() -> Self {
        Self {
            residency: Residency::new(),
            segments: Mutex::new(BTreeMap::new()),
        }
    }

    /// Fail-closed segments-mutex — lock poison is a typed refuse.
    fn segments_lock(
        &self,
    ) -> Result<
        std::sync::MutexGuard<'_, BTreeMap<ResidentIndexKey, Sealed<SegmentHandle>>>,
        SegmentRefuse,
    > {
        self.segments
            .lock()
            .map_err(|_| SegmentRefuse::SegmentsLockPoisoned)
    }

    /// Live generation for `relation` — see [`Residency::witness_after_snapshot`].
    pub(crate) fn witness_after_snapshot(
        &self,
        tx: &impl ReadTx,
        relation: RelationId,
    ) -> Result<Generation, ResidencyRefuse> {
        self.residency.witness_after_snapshot(tx, relation)
    }

    /// Record an imminent committed write — see [`Residency::bump_before_commit`].
    pub(crate) fn bump_before_commit(
        &self,
        relation: RelationId,
    ) -> Result<(), ResidencyRefuse> {
        self.residency.bump_before_commit(relation)
    }

    /// Serve a sealed segment when `live` matches its stamped generation.
    ///
    /// Outer [`SegmentRefuse`] is lock-door failure. Inner
    /// [`Result<SegmentHandle, SegmentMiss>`] is serve outcome: freshness is
    /// [`Generation::classify`] (matching keeps [`Sealed`]; mismatch yields
    /// [`Stale`] wrapped as [`SegmentMiss::Stale`]). No installed segment is
    /// [`SegmentMiss::Absent`] — never `Option` for either case.
    pub(crate) fn get(
        &self,
        relation: RelationId,
        live: Generation,
    ) -> Result<Result<SegmentHandle, SegmentMiss>, SegmentRefuse> {
        let key = ResidentIndexKey::for_relation(relation);
        let segments = self.segments_lock()?;
        let Some(sealed) = segments.get(&key) else {
            return Ok(Err(SegmentMiss::Absent));
        };
        match live.classify(sealed.clone()) {
            Ok(fresh) => Ok(Ok(fresh.into_kind())),
            Err(stale) => {
                // Stale carries installed vs expected — read those fields so
                // Absent vs Stale stays a carried fact. classify contract
                // broken → decline as Absent (typed), not an unreachable arm.
                if stale.generation() == live || stale.expected() != live {
                    return Ok(Err(SegmentMiss::Absent));
                }
                Ok(Err(SegmentMiss::Stale(stale)))
            }
        }
    }

    /// Admit a rebuild — see [`Residency::should_build`].
    pub(crate) fn should_build(
        &self,
        relation: RelationId,
        live: Generation,
    ) -> Result<bool, ResidencyRefuse> {
        self.residency.should_build(relation, live)
    }

    /// Seal `segment` at `generation` and install it, replacing any
    /// predecessor (which stays alive for readers holding its handle).
    pub(crate) fn install(
        &self,
        relation: RelationId,
        segment: Segment,
        generation: Generation,
    ) -> Result<SegmentHandle, SegmentRefuse> {
        let key = ResidentIndexKey::for_relation(relation);
        let handle = SegmentHandle(Arc::new(segment));
        let sealed = ProjectionBuilder::new(handle.clone()).seal(generation);
        self.segments_lock()?.insert(key, sealed);
        self.residency.clear_miss(relation)?;
        Ok(handle)
    }

    /// Drop a relation's segment, miss streak, and write-counter slot
    /// outright (destructive schema ops: the relation identity itself is
    /// being reused or destroyed).
    pub(crate) fn evict(&self, relation: RelationId) -> Result<(), SegmentRefuse> {
        let key = ResidentIndexKey::for_relation(relation);
        self.segments_lock()?.remove(&key);
        self.residency.forget(relation)?;
        Ok(())
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

/// Byte/value span of row `i` in an end-exclusive `u32` offset table
/// (`offsets[i]` ends row `i`; row 0 starts at 0). Shared by
/// [`Segment::row_at`] and the fixpoint level arena's identical layout.
///
/// INVARIANT(offset_row_in_bounds): `i < offsets.len()`.
pub(crate) fn offset_row_span(offsets: &[u32], i: usize) -> (usize, usize) {
    let start = if i == 0 {
        0
    } else {
        crate::rules::convert::usize_from_u32(offsets[i - 1])
    };
    let end = crate::rules::convert::usize_from_u32(offsets[i]);
    (start, end)
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
        // Bound is the caller's proof — public door is `row()` → None OOB.
        // Offset/slice indexing refuses OOB in every build (not debug-only).
        let (start, end) = offset_row_span(&self.offsets, i);
        &self.values[start..end]
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
    use miette::{Result, miette};

    use super::*;
    use crate::store::Storage;
    use crate::store::sim::SimStorage;

    fn row(vals: &[i64]) -> Tuple {
        vals.iter().map(|&i| DataValue::from(i)).collect()
    }

    #[test]
    fn prefix_ranges_match_linear_scan_across_types() -> Result<()> {
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
        let s = Segment::build(rows.clone().into_iter()).ok_or_else(|| miette!("segment"))?;
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
        Ok(())
    }

    #[test]
    fn empty_segment_probes_cleanly() -> Result<()> {
        let s = Segment::build(std::iter::empty()).ok_or_else(|| miette!("segment"))?;
        assert_eq!(s.len(), 0);
        assert!(s.prefix_range(&[DataValue::from(1)]).is_empty());
        Ok(())
    }

    /// F7: the row-offset boundary arithmetic, isolated from the ~4.3
    /// billion-value relation it would otherwise take to reach.
    #[test]
    fn checked_row_end_boundary() -> Result<()> {
        assert_eq!(checked_row_end(0), Some(0));
        assert_eq!(checked_row_end(1), Some(1));
        let u32_max_usize =
            usize::try_from(u32::MAX).map_err(|_| miette!("u32::MAX must fit usize"))?;
        assert_eq!(checked_row_end(u32_max_usize), Some(u32::MAX));
        assert_eq!(checked_row_end(u32_max_usize + 1), None);
        assert_eq!(checked_row_end(usize::MAX), None);
        Ok(())
    }

    #[test]
    fn classify_serves_matching_generation_and_rejects_stale() -> Result<()> {
        let db = SimStorage::new(3);
        let rtx = db.read_tx()?;
        let engine = SegmentEngine::new();
        let relation = RelationId::new(1).ok_or_else(|| miette!("below cap"))?;
        let live = engine.witness_after_snapshot(&rtx, relation)?;
        let handle = engine.install(
            relation,
            Segment::build(std::iter::once(row(&[1, 2]))).ok_or_else(|| miette!("segment"))?,
            live,
        )?;
        assert_eq!(
            handle.row(0),
            Some([DataValue::from(1), DataValue::from(2)].as_slice())
        );
        assert!(matches!(engine.get(relation, live), Ok(Ok(_))));

        engine.bump_before_commit(relation)?;
        let after = engine.witness_after_snapshot(&rtx, relation)?;
        assert!(matches!(
            engine.get(relation, after),
            Ok(Err(SegmentMiss::Stale(_)))
        ));
        Ok(())
    }

    #[test]
    fn orphan_evict_held_arc_still_serves() -> Result<()> {
        let db = SimStorage::new(3);
        let rtx = db.read_tx()?;
        let engine = SegmentEngine::new();
        let relation = RelationId::new(5).ok_or_else(|| miette!("below cap"))?;
        let live = engine.witness_after_snapshot(&rtx, relation)?;
        let handle = engine.install(
            relation,
            Segment::build(std::iter::once(row(&[9]))).ok_or_else(|| miette!("segment"))?,
            live,
        )?;
        engine.evict(relation)?;
        // Held Arc still serves after eviction.
        assert_eq!(handle.row(0), Some([DataValue::from(9)].as_slice()));
        assert!(matches!(
            engine.get(relation, live),
            Ok(Err(SegmentMiss::Absent))
        ));
        Ok(())
    }

    /// Adversary: a poisoned segments mutex must typed-refuse, never
    /// into_inner and silent continue, never panic costume.
    #[test]
    fn poisoned_segments_mutex_refuses_silent_continue() -> Result<()> {
        let engine = SegmentEngine::new();
        let relation = RelationId::new(1).ok_or_else(|| miette!("below cap"))?;
        let live = Generation::stamp_from_counter(0);
        let poison = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let Ok(guard) = engine.segments.lock() else {
                panic!("lock already poisoned before deliberate poison setup");
            };
            let _hold = guard;
            panic!("deliberate poison");
        }));
        assert!(
            poison.is_err(),
            "poison setup must panic while holding the guard"
        );
        assert!(
            matches!(
                engine.get(relation, live),
                Err(SegmentRefuse::SegmentsLockPoisoned)
            ),
            "poisoned mutex must typed-refuse get, not into_inner and continue"
        );
        assert_eq!(
            engine.evict(relation),
            Err(SegmentRefuse::SegmentsLockPoisoned),
            "poisoned mutex must typed-refuse evict, not into_inner and continue"
        );
        assert!(
            matches!(
                engine.install(
                    relation,
                    Segment::build(std::iter::once(row(&[1])))
                        .ok_or_else(|| miette!("segment"))?,
                    live,
                ),
                Err(SegmentRefuse::SegmentsLockPoisoned)
            ),
            "poisoned mutex must typed-refuse install, not into_inner and continue"
        );
        Ok(())
    }
}
