/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Bench-only recovery façade — real WAL replay for the `recovery_sla` lane.
//!
//! Feature-gated so this module does **not** compile into default/production
//! builds. Re-exports only WAL types + [`replay`] + minimal genesis helpers
//! to mint [`StoreId`] / [`FenceEpoch`] and [`CommitOrdinal`] for dirty-tail
//! corpora. No SweepDoor, write/commit, or general internals door.

#![cfg(feature = "bench-internals")]

use crate::store::open::{
    genesis, EntropyArm, GenesisParams, SizeClass, StableCommitCapArm, StagingTtl,
};
use crate::store::commit_cap::SnapshotFork;

pub use crate::store::epoch::FenceEpoch;
pub use crate::store::open::StoreId;
pub use crate::store::sweep::{
    recovery_time_bound_ns, CommitOrdinal, RECOVERY_SLA_INTERCEPT_NS, RECOVERY_SLA_SLOPE_DEN,
    RECOVERY_SLA_SLOPE_NUM,
};
pub use crate::store::wal::{
    replay, WalPayload, WalRecord, WalRefuse, WalReplayState, WalSegment,
};

/// Mint [`StoreId`] + genesis [`FenceEpoch`] via [`genesis`] — no SweepDoor.
pub fn mint_store_identity(identity_seed: [u8; 32]) -> (StoreId, FenceEpoch) {
    let sealed = genesis(GenesisParams {
        identity_seed,
        recovery_matrix: None,
        staging_ttl: StagingTtl::new(1_024),
        size_class: SizeClass::Compact,
        entropy_arm: EntropyArm::OsRandom,
        stable_commit_cap: StableCommitCapArm::NativeFsyncProof {
            snapshot_fork: SnapshotFork::No,
        },
    });
    (sealed.store_id(), sealed.fence_epoch())
}

/// Wrap a dense commit ordinal for WAL [`WalPayload::Commit`] bodies (bench corpus).
pub fn commit_ordinal(raw: u64) -> CommitOrdinal {
    CommitOrdinal::from_raw(raw)
}
