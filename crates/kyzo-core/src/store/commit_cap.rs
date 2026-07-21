/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Host commit-proof arms (decisions.md §27, §74, §85).
//!
//! Owns: [`StableCommitCap`], [`SnapshotFork`], [`ForkGenerationWitness`].
//!
//! Bans: open bool capability; arm without SnapshotFork declared (omission is
//! not exclusion); production [`super::sweep::Committed`] from kit pass alone
//! (or from a non-fsync [`super::sweep::Applied`] path).
//!
//! Host injection of the arm *choice* lives in
//! [`super::open::StableCommitCapArm`] (T8). This module owns the closed sum
//! with per-arm SnapshotFork — never a second host-injection definition.

use super::open::StableCommitCapArm;

/// Whether live process/snapshot fork is inside this arm's failure model.
///
/// Omission is not exclusion — every [`StableCommitCap`] arm declares exactly
/// one of these. `Yes` arms require misuse-resistant AEAD (e.g. AES-SIV) as a
/// condition of the arm; nonce repeat degrades to message-equality leak only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SnapshotFork {
    /// Snapshot fork is inside the licensed failure model — SIV required.
    Yes,
    /// Snapshot fork is excluded from this arm's failure DST.
    No,
}

impl SnapshotFork {
    /// Whether misuse-resistant AEAD is a condition of the arm.
    pub fn requires_misuse_resistant_aead(self) -> bool {
        matches!(self, SnapshotFork::Yes)
    }
}

/// Closed sum of production commit-proof arms.
///
/// The SweepDoor barrier *is* the arm's commit proof. Kit pass alone never
/// mints production [`super::sweep::Committed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StableCommitCap {
    /// Native fsync proof (today's `WriteTx::commit_durable` physical apply).
    NativeFsyncProof {
        /// Declared snapshot-fork membership in the failure model.
        snapshot_fork: SnapshotFork,
    },
    /// Platform transaction proof (OPFS / DO / etc.).
    PlatformTransactionProof {
        /// Declared snapshot-fork membership in the failure model.
        snapshot_fork: SnapshotFork,
    },
}

impl StableCommitCap {
    /// Lift the genesis-sealed host arm choice into the closed sum.
    ///
    /// Consumes [`StableCommitCapArm`] — does not redefine host injection.
    /// SnapshotFork is already the closed sum on the arm (no bool lift).
    pub fn from_arm(arm: StableCommitCapArm) -> Self {
        match arm {
            StableCommitCapArm::NativeFsyncProof { snapshot_fork } => {
                StableCommitCap::NativeFsyncProof { snapshot_fork }
            }
            StableCommitCapArm::PlatformTransactionProof { snapshot_fork } => {
                StableCommitCap::PlatformTransactionProof { snapshot_fork }
            }
        }
    }

    /// This arm's SnapshotFork declaration.
    pub fn snapshot_fork(self) -> SnapshotFork {
        match self {
            StableCommitCap::NativeFsyncProof { snapshot_fork }
            | StableCommitCap::PlatformTransactionProof { snapshot_fork } => snapshot_fork,
        }
    }

    /// Whether this arm requires misuse-resistant AEAD (SIV).
    pub fn requires_misuse_resistant_aead(self) -> bool {
        self.snapshot_fork().requires_misuse_resistant_aead()
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Optional host witness that a fork-generation changed mid-session.
///
/// Density-local: reminting [`super::authority::IncarnationId`] on generation
/// change makes mid-session uniqueness Unconstructible *for that arm only* —
/// not a global remote-freshness authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ForkGenerationWitness {
    generation: u64,
}

impl ForkGenerationWitness {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Host-observed generation counter.
    pub fn new(generation: u64) -> Self {
        Self { generation }
    }

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// The witnessed generation.
    pub fn generation(self) -> u64 {
        self.generation
    }
}
