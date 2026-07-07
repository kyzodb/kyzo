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
//! ## Validity is typed, not sequenced
//!
//! Segments are built on demand and *never maintained*. Soundness is the
//! pairing of two rules, each carried by a signature rather than a calling
//! convention (the enforcement-ladder ruling after a hostile review proved
//! the documented-ordering version racy):
//!
//! - **Writers bump BEFORE commit** ([`SegmentEngine::bump_before_commit`]):
//!   if a commit's rows are visible to any snapshot, its bump already
//!   happened. (A rolled-back transaction that bumped merely orphans a
//!   segment early — safe, never wrong.)
//! - **Readers witness AFTER their snapshot opens**
//!   ([`SegmentEngine::witness_after_snapshot`] takes the open snapshot as
//!   an argument, so the reverse order is unrepresentable): if a write is
//!   visible to the snapshot, the witness already reflects its bump, so a
//!   segment built before that write can never serve.
//!
//! Together: served segment ⇒ witness equality ⇒ no write committed between
//! the segment's snapshot and the reader's ⇒ identical current state.
//!
//! Segments hold `Arc`s: an orphaned segment stays alive for readers
//! mid-scan and is freed when the last one drops.
//!
//! ## The rebuild is gated, not unconditional
//!
//! A miss (no segment, or one built at a stale witness) does not
//! automatically rebuild: [`SegmentEngine::should_build`] requires a few
//! consecutive misses at the SAME witness first (see its doc). A build
//! pays a full relation scan no matter how small the read that triggered
//! it — cheap-to-amortize for a segment that then serves many probes,
//! ruinous for a caller whose every read is preceded by a write to the
//! same relation (every write bumps the watermark, so every such read
//! misses): unconditional rebuild-on-miss turned a point lookup into a
//! full-relation scan on every single mixed read/write op (issue #82).
//! Declining is always sound — identical to the existing "relation too
//! large for `u32` offsets" decline in [`Segment::build`] — the caller
//! falls back to its own unsegmented path, which pays no more than the
//! scan a build would have paid anyway.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use crate::data::value::{DataValue, RelationId, Tuple};

/// A relation-version witness: proof of "which write-history instant my
/// snapshot belongs to", obtainable only through
/// [`SegmentEngine::witness_after_snapshot`]. Monotone and process-local;
/// a fresh process starts every relation at zero with an empty cache, so
/// cross-process staleness cannot arise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Watermark(u64);

/// The execution path's segment context: `OFF` (tests, benches, callers
/// without a session) or a borrow of the session's engine. `Copy`, so it
/// threads through operator dispatch like `tx` does.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Segments<'a>(pub(crate) Option<&'a SegmentEngine>);

impl Segments<'_> {
    pub(crate) const OFF: Segments<'static> = Segments(None);
}

/// A build only pays off if the segment survives to serve at least one
/// probe beyond the one that triggered it; below this many consecutive
/// misses at the SAME witness, [`SegmentEngine::should_build`] declines.
/// See that method's doc for why the number is small and why a
/// write-interleaved caller never crosses it.
const REBUILD_AFTER_STABLE_MISSES: u32 = 2;

/// The session's segment engine: per-relation write watermarks plus the
/// segment cache they guard. One per [`Db`](crate::runtime::db::Db),
/// shared by all its transactions.
#[derive(Debug, Default)]
pub(crate) struct SegmentEngine {
    marks: Mutex<BTreeMap<RelationId, Arc<AtomicU64>>>,
    segments: Mutex<BTreeMap<RelationId, Arc<Segment>>>,
    /// Per-relation (witness, consecutive-miss-count) for the rebuild gate:
    /// a cache miss at a witness this map hasn't seen resets the count to
    /// 1; a repeat miss at the SAME witness (no write happened between the
    /// two reads) increments it. Never a source of truth about relation
    /// content — losing this map (e.g. on process restart) only costs a
    /// few extra ungated misses, never a wrong answer.
    misses: Mutex<BTreeMap<RelationId, (Watermark, u32)>>,
}

impl SegmentEngine {
    fn slot(&self, relation: RelationId) -> Arc<AtomicU64> {
        let mut marks = self.marks.lock().expect("watermark lock poisoned");
        marks.entry(relation).or_default().clone()
    }

    /// The relation's version witness, valid for `_snapshot`. Taking the
    /// open snapshot by reference makes witness-before-snapshot
    /// unrepresentable — the ordering the soundness proof (module docs)
    /// stands on, enforced by signature exactly like the storage layer's
    /// `stamp_after_snapshot`.
    pub(crate) fn witness_after_snapshot<T>(
        &self,
        _snapshot: &T,
        relation: RelationId,
    ) -> Watermark {
        Watermark(self.slot(relation).load(AtomicOrdering::Acquire))
    }

    /// Record an imminent committed write to `relation` — called BEFORE the
    /// storage commit, so a bump precedes any snapshot that can see the
    /// write. A subsequent rollback leaves a harmless early orphan.
    pub(crate) fn bump_before_commit(&self, relation: RelationId) {
        self.slot(relation).fetch_add(1, AtomicOrdering::AcqRel);
    }

    /// The relation's segment, iff still exactly valid at `witness`.
    pub(crate) fn get(&self, relation: RelationId, witness: Watermark) -> Option<Arc<Segment>> {
        let segments = self.segments.lock().expect("segment lock poisoned");
        segments
            .get(&relation)
            .filter(|s| s.built_at == witness)
            .cloned()
    }

    /// Whether a cache miss at `witness` should trigger a full-relation
    /// rebuild. `Segment::build` pays the same relation scan an unsegmented
    /// read would have paid, plus a flatten and an `Arc` install — a good
    /// trade when the segment then serves many later probes, a pure loss
    /// when the very next write orphans it before it serves even one. A
    /// witness only changes on a committed write to this relation (see the
    /// module doc's soundness pairing), so "N misses at the same witness"
    /// is exactly "N reads with no intervening write": below
    /// [`REBUILD_AFTER_STABLE_MISSES`], the caller should fall back to its
    /// unsegmented path (a point probe or plain scan — no more expensive
    /// than the scan a build would pay anyway) instead of building. A
    /// write-interleaved (OLTP mixed-op) caller bumps the watermark before
    /// every read, so every miss resets this count to 1 and a build is
    /// never attempted — the pathological O(n)-rebuild-per-read case this
    /// gate exists to close. A read-heavy caller (the segment's intended
    /// case) crosses the threshold after a couple of stable reads and
    /// builds once, same as before this gate existed.
    pub(crate) fn should_build(&self, relation: RelationId, witness: Watermark) -> bool {
        let mut misses = self.misses.lock().expect("miss-streak lock poisoned");
        let count = match misses.get_mut(&relation) {
            Some(entry) if entry.0 == witness => {
                entry.1 += 1;
                entry.1
            }
            _ => {
                misses.insert(relation, (witness, 1));
                1
            }
        };
        count >= REBUILD_AFTER_STABLE_MISSES
    }

    /// Install a freshly built segment, replacing any predecessor (which
    /// stays alive for readers holding its `Arc`).
    pub(crate) fn install(&self, relation: RelationId, segment: Segment) -> Arc<Segment> {
        let seg = Arc::new(segment);
        self.segments
            .lock()
            .expect("segment lock poisoned")
            .insert(relation, seg.clone());
        seg
    }

    /// Drop a relation's segment, watermark, and miss-streak outright
    /// (destructive schema ops: the relation identity itself is being
    /// reused or destroyed).
    pub(crate) fn evict(&self, relation: RelationId) {
        self.segments
            .lock()
            .expect("segment lock poisoned")
            .remove(&relation);
        self.misses
            .lock()
            .expect("miss-streak lock poisoned")
            .remove(&relation);
        self.marks
            .lock()
            .expect("watermark lock poisoned")
            .remove(&relation);
    }
}

/// One relation's current state as dense row-major flattened rows in key
/// order — the execution currency's own layout, so serving a scan is a
/// contiguous copy and a prefix probe is a binary search over decoded
/// values.
#[derive(Debug)]
pub(crate) struct Segment {
    built_at: Watermark,
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
    /// scan's own (key) order, at the witness taken for that scan's
    /// snapshot.
    ///
    /// `None` iff the relation's flattened value count would overflow the
    /// `u32` offset encoding (~4.3 billion `DataValue`s in one relation): a
    /// segment is an optional, rebuildable acceleration structure, so
    /// declining to build one is semantically free — the caller falls back
    /// to a normal scan, which has no such ceiling.
    pub(crate) fn build(rows: impl Iterator<Item = Tuple>, built_at: Watermark) -> Option<Self> {
        let mut values = Vec::new();
        let mut offsets = Vec::new();
        for row in rows {
            values.extend(row);
            offsets.push(checked_row_end(values.len())?);
        }
        Some(Segment {
            built_at,
            values,
            offsets,
        })
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
        rows.push(vec![DataValue::from(7), DataValue::from("x")]);
        rows.push(vec![DataValue::from(7), DataValue::from("y")]);
        let s = Segment::build(rows.clone().into_iter(), Watermark(0)).unwrap();
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
    fn witness_equality_governs_service() {
        let engine = SegmentEngine::default();
        let rel = RelationId::new(7).expect("below cap");
        let snapshot = (); // any open snapshot stands in

        let w0 = engine.witness_after_snapshot(&snapshot, rel);
        engine.install(rel, Segment::build([row(&[1, 2])].into_iter(), w0).unwrap());
        assert!(engine.get(rel, w0).is_some(), "fresh segment serves");

        engine.bump_before_commit(rel);
        let w1 = engine.witness_after_snapshot(&snapshot, rel);
        assert_ne!(w0, w1);
        assert!(
            engine.get(rel, w1).is_none(),
            "a write orphans the segment: it must NOT serve at the new witness"
        );

        let held = engine.install(rel, Segment::build([row(&[3, 4])].into_iter(), w1).unwrap());
        assert!(engine.get(rel, w1).is_some(), "rebuilt segment serves");
        engine.evict(rel);
        assert!(engine.get(rel, w1).is_none(), "evicted");
        assert_eq!(held.len(), 1, "held Arc outlives eviction");
    }

    /// The rebuild gate (`should_build`): a lone miss at a witness declines
    /// (not yet proven stable); a second miss at the SAME witness (no write
    /// happened between the two reads) crosses the threshold and triggers.
    #[test]
    fn rebuild_gated_by_stable_miss_streak() {
        let engine = SegmentEngine::default();
        let rel = RelationId::new(3).expect("below cap");
        let snapshot = ();

        let w = engine.witness_after_snapshot(&snapshot, rel);
        assert!(
            !engine.should_build(rel, w),
            "first miss at a witness must not trigger a build"
        );
        assert!(
            engine.should_build(rel, w),
            "second miss at the same witness must trigger a build"
        );

        // A write bumps the witness: the next miss resets the streak to 1,
        // exactly like a fresh relation.
        engine.bump_before_commit(rel);
        let w2 = engine.witness_after_snapshot(&snapshot, rel);
        assert_ne!(w, w2);
        assert!(
            !engine.should_build(rel, w2),
            "a witness change must reset the streak"
        );
        assert!(
            engine.should_build(rel, w2),
            "second stable miss at the new witness must trigger"
        );
    }

    /// The OLTP mixed-op shape (issue #82): a caller whose every read is
    /// preceded by a committed write to the same relation never sees two
    /// misses at the same witness, so the gate must NEVER cross threshold —
    /// this is what keeps such a caller off the O(n)-rebuild-per-read path.
    #[test]
    fn alternating_writes_never_cross_the_rebuild_gate() {
        let engine = SegmentEngine::default();
        let rel = RelationId::new(5).expect("below cap");
        let snapshot = ();
        for _ in 0..50 {
            engine.bump_before_commit(rel);
            let w = engine.witness_after_snapshot(&snapshot, rel);
            assert!(
                !engine.should_build(rel, w),
                "a witness that changes before every miss must never reach the rebuild threshold"
            );
        }
    }

    /// (e) the miss map is documented as "never a source of truth" — losing
    /// it costs a few extra ungated misses, never a wrong decision. Proven
    /// directly: clearing it mid-streak (standing in for whatever external
    /// event could lose it, e.g. a process restart) only restarts the
    /// stable-miss count at the same witness; it can never make
    /// `should_build` return `true` early, and it can never affect what
    /// [`SegmentEngine::get`] would serve (that question is answered by
    /// witness equality alone, a wholly separate map).
    #[test]
    fn miss_map_loss_only_delays_rebuild_never_corrupts_serving() {
        let engine = SegmentEngine::default();
        let rel = RelationId::new(11).expect("below cap");
        let snapshot = ();

        let w = engine.witness_after_snapshot(&snapshot, rel);
        assert!(
            !engine.should_build(rel, w),
            "first miss at a witness must not trigger a build"
        );

        // Simulate losing the miss-streak map outright.
        engine
            .misses
            .lock()
            .expect("miss-streak lock poisoned")
            .clear();

        assert!(
            !engine.should_build(rel, w),
            "a miss after losing the streak must restart at 1, not resume at 2"
        );
        assert!(
            engine.should_build(rel, w),
            "the streak still reaches threshold normally after the loss"
        );

        // The loss never touches what a served segment answers: install one
        // and confirm `get` still serves purely off witness equality.
        let seg = Segment::build([row(&[9, 99])].into_iter(), w).unwrap();
        engine.install(rel, seg);
        engine
            .misses
            .lock()
            .expect("miss-streak lock poisoned")
            .clear();
        assert!(
            engine.get(rel, w).is_some(),
            "losing the miss map must never un-serve a validly-witnessed segment"
        );
    }

    #[test]
    fn empty_segment_probes_cleanly() {
        let s = Segment::build(std::iter::empty(), Watermark(0)).unwrap();
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
}
