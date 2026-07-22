/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Projection residency: rebuild/validity discipline (generations, invalidation).
//!
//! Owns the process-local generation counters, the stable-miss rebuild gate,
//! and the witness-after-snapshot / bump-before-commit pairing. Serving and
//! dense segment buffers live in [`super::current`].
//!
//! ## Durable generation vocabulary (carried obligation)
//!
//! The process-local generation counter is sound ONLY while segments are
//! memory-only. If projections ever persist, the generation vocabulary must
//! become durable under decisions.md §20: Catalog generation is a Store
//! commit-order position, never a second watermark organ; §35 requires
//! two-axis coordinates on any durable projection keyspace.
//!
//! Monotone, process-local; a fresh process is zero + empty cache so
//! cross-process staleness cannot arise. `bump_before_commit` /
//! `witness_after_snapshot` pairing is soundness by SIGNATURE (open snapshot
//! required), not calling convention.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use crate::project::projection::{Generation, ResidentIndexKey};
use crate::session::generation::RelationGeneration;
use crate::store::ReadTx;
use kyzo_model::value::RelationId;

/// Consecutive misses at one live generation before a rebuild is admitted.
/// One miss declines (not yet proven stable); the second at the same
/// generation builds. Alternating write+read never crosses this gate.
pub(crate) const REBUILD_AFTER_STABLE_MISSES: u32 = 2;

/// Per-relation write counters and the stable-miss rebuild gate.
#[derive(Debug, Default)]
pub(crate) struct Residency {
    marks: Mutex<BTreeMap<ResidentIndexKey, Arc<AtomicU64>>>,
    misses: Mutex<BTreeMap<ResidentIndexKey, (Generation, u32)>>,
}

impl Residency {
    fn slot(&self, relation: RelationId) -> Arc<AtomicU64> {
        let key = ResidentIndexKey::for_relation(relation);
        let mut marks = self.marks.lock().expect("generation lock poisoned");
        marks.entry(key).or_default().clone()
    }

    /// Live generation for `relation`, sampled only after `tx` proves a
    /// snapshot is open — the racy "read mark then open snapshot" order is
    /// unrepresentable. Freshness is witnessed as [`RelationGeneration`] and
    /// stamped only through the catalog authority (no bare `Generation::new`).
    pub(crate) fn witness_after_snapshot(
        &self,
        _tx: &impl ReadTx,
        relation: RelationId,
    ) -> Generation {
        RelationGeneration::witness(self.slot(relation).load(AtomicOrdering::Acquire))
            .projection_stamp()
    }

    /// Record an imminent committed write to `relation` — called BEFORE the
    /// storage commit, so a bump precedes any snapshot that can see the
    /// write. A subsequent rollback leaves a harmless early counter advance.
    ///
    /// # Non-transition (proven invariant)
    ///
    /// Story #302 T4: not a Domain-style consuming field transition. The engine is an
    /// `Arc`-shared capability handle ([`crate::session::db::Db::segments`]);
    /// the per-relation counter is an [`AtomicU64`] under a stated
    /// concurrent-access requirement (many writers/readers across
    /// transactions). The bump is a monotone counter advance on that shared
    /// atomic — reassignment of a Domain-like proof is unrepresentable
    /// without breaking Arc sharing.
    pub(crate) fn bump_before_commit(&self, relation: RelationId) {
        self.slot(relation).fetch_add(1, AtomicOrdering::AcqRel);
    }

    /// Admit a rebuild after [`REBUILD_AFTER_STABLE_MISSES`] consecutive
    /// misses at the same live generation. A write (generation advance)
    /// resets the streak. Declining is always sound — the caller falls
    /// back to storage. Miss-map loss only delays rebuild, never corrupts
    /// serving — the cache is never a source of truth.
    pub(crate) fn should_build(&self, relation: RelationId, live: Generation) -> bool {
        let key = ResidentIndexKey::for_relation(relation);
        let mut misses = self.misses.lock().expect("miss lock poisoned");
        match misses.get_mut(&key) {
            Some((recorded, count)) if *recorded == live => {
                *count = count.saturating_add(1);
                *count >= REBUILD_AFTER_STABLE_MISSES
            }
            _other => {
                misses.insert(key, (live, 1));
                false
            }
        }
    }

    /// Clear the miss streak after a successful install.
    pub(crate) fn clear_miss(&self, relation: RelationId) {
        let key = ResidentIndexKey::for_relation(relation);
        self.misses.lock().expect("miss lock poisoned").remove(&key);
    }

    /// Drop miss streak and write-counter slot (destructive schema ops).
    pub(crate) fn forget(&self, relation: RelationId) {
        let key = ResidentIndexKey::for_relation(relation);
        self.misses.lock().expect("miss lock poisoned").remove(&key);
        self.marks
            .lock()
            .expect("generation lock poisoned")
            .remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Storage;
    use crate::store::sim::SimStorage;

    #[test]
    fn rebuild_gated_by_stable_miss_streak() {
        let db = SimStorage::new(5);
        let rtx = db.read_tx().unwrap();
        let residency = Residency::default();
        let relation = RelationId::new(2).expect("below cap");
        let live = residency.witness_after_snapshot(&rtx, relation);

        assert!(
            !residency.should_build(relation, live),
            "first miss declines"
        );
        assert!(
            residency.should_build(relation, live),
            "second stable miss admits build"
        );

        residency.bump_before_commit(relation);
        let next = residency.witness_after_snapshot(&rtx, relation);
        assert!(
            !residency.should_build(relation, next),
            "write resets the streak"
        );
    }

    /// Issue #82: alternating write+read never crosses the rebuild gate.
    #[test]
    fn alternating_writes_never_cross_rebuild_gate() {
        let db = SimStorage::new(7);
        let residency = Residency::default();
        let relation = RelationId::new(3).expect("below cap");
        for _ in 0..20 {
            residency.bump_before_commit(relation);
            let rtx = db.read_tx().unwrap();
            let live = residency.witness_after_snapshot(&rtx, relation);
            assert!(
                !residency.should_build(relation, live),
                "write-interleaved single miss must never admit a build"
            );
        }
    }

    /// Miss-map loss only delays rebuild — clearing the streak never makes
    /// a stale serve possible (serving is witness equality in current.rs).
    #[test]
    fn miss_map_loss_only_delays_rebuild() {
        let db = SimStorage::new(3);
        let rtx = db.read_tx().unwrap();
        let residency = Residency::default();
        let relation = RelationId::new(4).expect("below cap");
        let live = residency.witness_after_snapshot(&rtx, relation);
        assert!(!residency.should_build(relation, live));
        residency.clear_miss(relation);
        // After loss, the next miss starts a fresh streak — declines again.
        assert!(!residency.should_build(relation, live));
        assert!(residency.should_build(relation, live));
    }
}
