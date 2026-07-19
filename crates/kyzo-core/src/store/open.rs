/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Store identity, open capability, and genesis construction (decisions.md §4–§6, §88).
//!
//! Owns: [`StoreId`], [`StoreOpen`], genesis construction (seals
//! [`FenceEpoch::genesis`](super::epoch::FenceEpoch::genesis), [`CryptoDomain`],
//! optional [`RecoveryMatrix`], ordinal [`StagingTtl`], size class, entropy arm,
//! [`StableCommitCap`] arm selection).
//!
//! Bans: path-as-identity; public path-taking open; post-genesis
//! RecoveryMatrix / StagingTtl setters.

use sha2::{Digest, Sha256};

use super::authority::{RecoveryMatrix, WriteAuthority};
use super::epoch::{CryptoDomain, FenceEpoch};

/// Sealed Store identity digest — genesis / fork-minted, never path/URL.
///
/// Path/URL is rebindable location only. Rename/move preserves identity.
/// Restore preserves readable identity; write continuity requires matching
/// [`WriteAuthority`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StoreId([u8; 32]);

impl StoreId {
    /// Wrap an already-proven identity digest (genesis / materialize / decode).
    pub(crate) fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Borrow the identity digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Closed verb family for [`StoreOpen`] (TAG-ratcheted; sealed variants).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StoreOpenVerb {
    /// Open the Store (identity-scoped).
    Open,
    /// Read under the open capability.
    Read,
    /// Write-resume path additionally requires [`WriteAuthority`].
    Write,
}

/// TAG-ratcheted open capability: one Store identity scope, closed verb family.
///
/// Privately constructed — never from a filesystem path alone. A capability
/// that names two Store ids is Unconstructible. Transport may authenticate;
/// it never authorizes open. [`WriteAuthority`] remains a distinct affine token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreOpen {
    store_id: StoreId,
    verb: StoreOpenVerb,
}

impl StoreOpen {
    /// Private mint over an explicit Store identity (host/root grant).
    pub(crate) fn mint(store_id: StoreId, verb: StoreOpenVerb) -> Self {
        Self { store_id, verb }
    }

    /// Store identity this capability scopes to (exactly one).
    pub fn store_id(&self) -> StoreId {
        self.store_id
    }

    /// Sealed verb this capability authorizes.
    pub fn verb(&self) -> StoreOpenVerb {
        self.verb
    }
}

/// Genesis-sealed StagingTTL in dense CommitOrdinals.
///
/// Post-genesis mutation is Unconstructible (no setter). Full object-slot
/// semantics live in `store/objects.rs` (T10); genesis only seals the ordinal count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StagingTtl(u64);

impl StagingTtl {
    /// Seal a TTL as a count of dense CommitOrdinals.
    pub fn new(ordinals: u64) -> Self {
        Self(ordinals)
    }

    /// Ordinal count.
    pub fn ordinals(self) -> u64 {
        self.0
    }
}

/// Human-operable durable size class sealed at genesis (decisions.md §88).
///
/// At ceiling writes refuse StoreFull; Engine may compose splits as workflow.
/// Store never silent-auto-splits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SizeClass {
    /// Compact host — tens of GB class.
    Compact,
    /// Standard filesystem-fit class.
    Standard,
    /// Upper human-operable class (low TB depending on host) — not petabyte warehouse.
    Large,
}

/// Approved entropy arm for [`super::authority::Entropy`] at incarnation mint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntropyArm {
    /// OS CSPRNG (`getrandom` / `rand` OsRng path).
    OsRandom,
}

/// Host selection of which `StableCommitCap` arm genesis seals.
///
/// The closed sum with per-arm `SnapshotFork` declaration is seated in
/// `store/commit_cap.rs` (T9). Genesis seals the arm *choice* config-once;
/// this type is injection only — not a second commit-door definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StableCommitCapArm {
    /// Native fsync proof arm (`StableCommitCap::NativeFsyncProof` in T9).
    NativeFsyncProof {
        /// Whether this arm's failure model includes snapshot fork.
        snapshot_fork: bool,
    },
    /// Platform transaction proof arm (`StableCommitCap::PlatformTransactionProof` in T9).
    PlatformTransactionProof {
        /// Whether this arm's failure model includes snapshot fork.
        snapshot_fork: bool,
    },
}

/// Config-once genesis parameters injected from the host composition root.
///
/// No post-genesis setters for RecoveryMatrix / StagingTtl / size class /
/// entropy arm / StableCommitCap arm — mutation is Unconstructible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenesisParams {
    /// Opaque principal / deployment seed contributing to StoreId (not a path).
    pub identity_seed: [u8; 32],
    /// Optional M-of-N recovery matrix (genesis-only).
    pub recovery_matrix: Option<RecoveryMatrix>,
    /// Ordinal StagingTTL sealed into genesis.
    pub staging_ttl: StagingTtl,
    /// Human-operable size class.
    pub size_class: SizeClass,
    /// Approved entropy arm for incarnation mint.
    pub entropy_arm: EntropyArm,
    /// `StableCommitCap` arm selection (injected, not defined in the host).
    pub stable_commit_cap: StableCommitCapArm,
}

/// Genesis-sealed Store identity + open capability + write authority + domain.
#[derive(Debug)]
pub struct GenesisSealed {
    store_id: StoreId,
    store_open: StoreOpen,
    write_authority: WriteAuthority,
    crypto_domain: CryptoDomain,
    fence_epoch: FenceEpoch,
    recovery_matrix: Option<RecoveryMatrix>,
    staging_ttl: StagingTtl,
    size_class: SizeClass,
    entropy_arm: EntropyArm,
    stable_commit_cap: StableCommitCapArm,
}

impl GenesisSealed {
    /// Sealed Store identity.
    pub fn store_id(&self) -> StoreId {
        self.store_id
    }

    /// TAG open capability for this Store (verb: Open).
    pub fn store_open(&self) -> &StoreOpen {
        &self.store_open
    }

    /// Affine WriteAuthority for the keystore (moved out via [`take_write_authority`]).
    pub fn write_authority(&self) -> &WriteAuthority {
        &self.write_authority
    }

    /// Move the WriteAuthority into the host keystore (affine — one holder).
    pub fn take_write_authority(self) -> (GenesisSealedView, WriteAuthority) {
        let authority = self.write_authority;
        let view = GenesisSealedView {
            store_id: self.store_id,
            store_open: self.store_open,
            crypto_domain: self.crypto_domain,
            fence_epoch: self.fence_epoch,
            recovery_matrix: self.recovery_matrix,
            staging_ttl: self.staging_ttl,
            size_class: self.size_class,
            entropy_arm: self.entropy_arm,
            stable_commit_cap: self.stable_commit_cap,
        };
        (view, authority)
    }

    /// Genesis CryptoDomain.
    pub fn crypto_domain(&self) -> CryptoDomain {
        self.crypto_domain
    }

    /// Genesis FenceEpoch (epoch zero).
    pub fn fence_epoch(&self) -> FenceEpoch {
        self.fence_epoch
    }

    /// Optional sealed RecoveryMatrix (no setter).
    pub fn recovery_matrix(&self) -> Option<&RecoveryMatrix> {
        self.recovery_matrix.as_ref()
    }

    /// Sealed StagingTTL (no setter).
    pub fn staging_ttl(&self) -> StagingTtl {
        self.staging_ttl
    }

    /// Sealed size class (no setter).
    pub fn size_class(&self) -> SizeClass {
        self.size_class
    }

    /// Sealed entropy arm (no setter).
    pub fn entropy_arm(&self) -> EntropyArm {
        self.entropy_arm
    }

    /// Sealed StableCommitCap arm selection (no setter).
    pub fn stable_commit_cap(&self) -> StableCommitCapArm {
        self.stable_commit_cap
    }
}

/// Genesis facts after the WriteAuthority has been moved to the keystore.
#[derive(Debug, Clone)]
pub struct GenesisSealedView {
    store_id: StoreId,
    store_open: StoreOpen,
    crypto_domain: CryptoDomain,
    fence_epoch: FenceEpoch,
    recovery_matrix: Option<RecoveryMatrix>,
    staging_ttl: StagingTtl,
    size_class: SizeClass,
    entropy_arm: EntropyArm,
    stable_commit_cap: StableCommitCapArm,
}

impl GenesisSealedView {
    /// Sealed Store identity.
    pub fn store_id(&self) -> StoreId {
        self.store_id
    }

    /// TAG open capability.
    pub fn store_open(&self) -> &StoreOpen {
        &self.store_open
    }

    /// Genesis CryptoDomain.
    pub fn crypto_domain(&self) -> CryptoDomain {
        self.crypto_domain
    }

    /// Genesis FenceEpoch.
    pub fn fence_epoch(&self) -> FenceEpoch {
        self.fence_epoch
    }

    /// Optional sealed RecoveryMatrix.
    pub fn recovery_matrix(&self) -> Option<&RecoveryMatrix> {
        self.recovery_matrix.as_ref()
    }

    /// Sealed StagingTTL.
    pub fn staging_ttl(&self) -> StagingTtl {
        self.staging_ttl
    }

    /// Sealed size class.
    pub fn size_class(&self) -> SizeClass {
        self.size_class
    }

    /// Sealed entropy arm.
    pub fn entropy_arm(&self) -> EntropyArm {
        self.entropy_arm
    }

    /// Sealed StableCommitCap arm selection.
    pub fn stable_commit_cap(&self) -> StableCommitCapArm {
        self.stable_commit_cap
    }
}

/// Typed refuse from genesis construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum GenesisRefuse {
    #[error(
        "MissingStoreOpenCapability: Store open requires a StoreOpen capability (path-only open is Unconstructible)"
    )]
    #[diagnostic(code(store::open::missing_store_open))]
    MissingStoreOpenCapability,
}

/// Genesis construction: seals FenceEpoch::genesis, CryptoDomain, optional
/// RecoveryMatrix, ordinal StagingTtl, size class, entropy arm, and the
/// host-selected StableCommitCap arm. Mints StoreId, StoreOpen, WriteAuthority.
///
/// Identity is the sealed digest of genesis params — never a filesystem path.
/// There is no public path-taking open.
pub fn genesis(params: GenesisParams) -> GenesisSealed {
    let store_id = mint_store_id(&params);
    let fence_epoch = FenceEpoch::genesis(store_id);
    let crypto_domain = CryptoDomain::new(store_id, fence_epoch);
    let token_id = mint_write_authority_token(&params, store_id);
    let write_authority = WriteAuthority::mint(store_id, token_id);
    let store_open = StoreOpen::mint(store_id, StoreOpenVerb::Open);
    GenesisSealed {
        store_id,
        store_open,
        write_authority,
        crypto_domain,
        fence_epoch,
        recovery_matrix: params.recovery_matrix,
        staging_ttl: params.staging_ttl,
        size_class: params.size_class,
        entropy_arm: params.entropy_arm,
        stable_commit_cap: params.stable_commit_cap,
    }
}

/// Open an existing Store by presenting a [`StoreOpen`] capability.
///
/// Path-only open has no constructor — attempting open without StoreOpen is
/// represented as [`GenesisRefuse::MissingStoreOpenCapability`] at this door.
/// Location (path) is supplied by the host for the adapter only; it never
/// enters identity.
pub fn open_with_capability(capability: &StoreOpen) -> Result<StoreId, GenesisRefuse> {
    // Capability presence is the door; path is not an argument.
    Ok(capability.store_id())
}

fn mint_store_id(params: &GenesisParams) -> StoreId {
    let mut h = Sha256::new();
    h.update(b"kyzo.store_id.genesis.v1");
    h.update(params.identity_seed);
    h.update(u64::to_be_bytes(params.staging_ttl.ordinals()));
    h.update([size_class_tag(params.size_class)]);
    h.update([entropy_arm_tag(params.entropy_arm)]);
    h.update([stable_commit_cap_tag(params.stable_commit_cap)]);
    if let Some(matrix) = &params.recovery_matrix {
        h.update(b"recovery_matrix");
        h.update(u32::to_be_bytes(matrix.threshold()));
        for key in matrix.keys() {
            h.update(key.as_bytes());
        }
    } else {
        h.update(b"no_recovery_matrix");
    }
    StoreId::from_digest(h.finalize().into())
}

fn mint_write_authority_token(params: &GenesisParams, store_id: StoreId) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"kyzo.write_authority.genesis.v1");
    h.update(store_id.as_bytes());
    h.update(params.identity_seed);
    h.finalize().into()
}

fn size_class_tag(class: SizeClass) -> u8 {
    match class {
        SizeClass::Compact => 1,
        SizeClass::Standard => 2,
        SizeClass::Large => 3,
    }
}

fn entropy_arm_tag(arm: EntropyArm) -> u8 {
    match arm {
        EntropyArm::OsRandom => 1,
    }
}

fn stable_commit_cap_tag(arm: StableCommitCapArm) -> u8 {
    match arm {
        StableCommitCapArm::NativeFsyncProof { snapshot_fork } => {
            if snapshot_fork {
                1
            } else {
                2
            }
        }
        StableCommitCapArm::PlatformTransactionProof { snapshot_fork } => {
            if snapshot_fork {
                3
            } else {
                4
            }
        }
    }
}
