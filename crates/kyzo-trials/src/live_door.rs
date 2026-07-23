/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! One live SweepDoor open seat for crash / DST harnesses (copy_detector).

use crate::store::authority::{Entropy, OpenOrdinal};
use crate::store::commit_cap::{SnapshotFork, StableCommitCap};
use crate::store::grants::IdentitySeed;
use crate::store::merkle::{StateRoot, GENESIS_ROOT};
use crate::store::open::{
    genesis, EntropyArm, GenesisParams, SizeClass, StableCommitCapArm, StagingTtl,
};
use crate::store::sweep::{SweepDoor, SweepSession};
use crate::store::IncarnationId;

/// Loud admit — kit/campaign step that must hold. Diverges on Err (never silent).
/// `#[cfg(test)]`: path-wired under sweep's test wall; ProductionOnly exemption.
#[cfg(test)]
fn admit<T, E: core::fmt::Display>(r: Result<T, E>, what: &str) -> T {
    match r {
        Ok(v) => v,
        Err(e) => loop {
            assert!(false, "{what}: {e}");
        },
    }
}

/// Open a SweepDoor under a fresh genesis WriteAuthority + live session.
pub(super) fn open_live_door(
    identity_seed: IdentitySeed,
    entropy: Entropy,
    cap: StableCommitCap,
) -> (SweepDoor, IncarnationId, SweepSession) {
    let sealed = genesis(GenesisParams {
        identity_seed: *identity_seed.as_bytes(),
        recovery_matrix: None,
        staging_ttl: StagingTtl::new(1_024),
        size_class: SizeClass::Compact,
        entropy_arm: EntropyArm::OsRandom,
        stable_commit_cap: StableCommitCapArm::NativeFsyncProof {
            snapshot_fork: SnapshotFork::No,
        },
    });
    let store_id = sealed.store_id();
    let fence_epoch = sealed.fence_epoch();
    let (_view, auth) = sealed.take_write_authority();
    let incarnation = admit(
        auth.incarnation_mint_cap(OpenOrdinal::ZERO).mint(entropy),
        "INVARIANT(incarnation_mint): genesis mint admits",
    );
    let session = SweepSession::new(store_id, fence_epoch, incarnation);
    let door = admit(
        SweepDoor::open(store_id, fence_epoch, session, auth, cap),
        "INVARIANT(live_sweep_door): SweepDoor open admits",
    );
    (door, incarnation, session)
}

/// NativeFsyncProof{No} door — the default crash/DST live seat.
pub(super) fn open_native_live_door(
    identity_seed: IdentitySeed,
    entropy: Entropy,
) -> (SweepDoor, IncarnationId, SweepSession) {
    open_live_door(
        identity_seed,
        entropy,
        StableCommitCap::NativeFsyncProof {
            snapshot_fork: SnapshotFork::No,
        },
    )
}

/// Tag-flipped content root for overlap-batch seal proofs.
pub(super) fn content_root(tag: u8) -> StateRoot {
    let mut bytes = *GENESIS_ROOT.as_bytes();
    bytes[0] = tag;
    StateRoot::from_digest(bytes)
}
