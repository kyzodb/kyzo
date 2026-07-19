/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Grants are seeds; [`materialize`] is pure (decisions.md §2, §68).
//!
//! Owns: [`ForkGrant`], [`RecoveryGrant`], [`GrantId`], [`materialize`],
//! [`AncestorReadGrant`].
//!
//! Bans: discovery-time identity entropy outside the grant seed; grant-time
//! kill of the original token; post-grant shared-confidentiality Rotate;
//! in-place WriteAuthority reissue for the same epoch.

use sha2::{Digest, Sha256};

use super::authority::WriteAuthority;
use super::epoch::{CryptoDomain, FenceEpoch};
use super::open::StoreId;

/// Stable grant identity bound into the signed payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GrantId([u8; 32]);

impl GrantId {
    /// Wrap an already-proven grant id.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow the grant id bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Different-principal fork seed — not an event.
///
/// Binds GrantId, predecessor StoreId, fork-point root, successor principal,
/// identity seed, and key-material commitment. Changes nothing about the
/// original lineage. Materialization is at discovery via [`materialize`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkGrant {
    grant_id: GrantId,
    predecessor_store: StoreId,
    fork_point_root: [u8; 32],
    successor_principal: [u8; 32],
    identity_seed: [u8; 32],
    key_material_commitment: [u8; 32],
}

impl ForkGrant {
    /// Construct a fork grant seed from its sealed payload fields.
    pub fn new(
        grant_id: GrantId,
        predecessor_store: StoreId,
        fork_point_root: [u8; 32],
        successor_principal: [u8; 32],
        identity_seed: [u8; 32],
        key_material_commitment: [u8; 32],
    ) -> Self {
        Self {
            grant_id,
            predecessor_store,
            fork_point_root,
            successor_principal,
            identity_seed,
            key_material_commitment,
        }
    }

    /// Grant identity.
    pub fn grant_id(&self) -> GrantId {
        self.grant_id
    }

    /// Predecessor Store identity (original lineage continues untouched).
    pub fn predecessor_store(&self) -> StoreId {
        self.predecessor_store
    }

    /// Fork-point state root.
    pub fn fork_point_root(&self) -> &[u8; 32] {
        &self.fork_point_root
    }

    /// Successor principal binding.
    pub fn successor_principal(&self) -> &[u8; 32] {
        &self.successor_principal
    }

    /// Successor identity seed (sole entropy for materialize — no discovery-time draw).
    pub fn identity_seed(&self) -> &[u8; 32] {
        &self.identity_seed
    }

    /// Key-material commitment.
    pub fn key_material_commitment(&self) -> &[u8; 32] {
        &self.key_material_commitment
    }
}

/// Same-principal recovery seed — one-shot quorum under RecoveryMatrix.
///
/// Predecessor FenceEpoch in the signed payload. A second valid grant for
/// one predecessor epoch is quorum equivocation → poison for the signing
/// set's authority, never a second lineage. Recovery keys are a distinct
/// custodian class. Obeys the same seed / [`materialize`] law as ForkGrant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryGrant {
    grant_id: GrantId,
    store_id: StoreId,
    predecessor_epoch: FenceEpoch,
    successor_identity_seed: [u8; 32],
    key_material_commitment: [u8; 32],
}

impl RecoveryGrant {
    /// Construct a recovery grant seed from its sealed payload fields.
    pub fn new(
        grant_id: GrantId,
        store_id: StoreId,
        predecessor_epoch: FenceEpoch,
        successor_identity_seed: [u8; 32],
        key_material_commitment: [u8; 32],
    ) -> Self {
        Self {
            grant_id,
            store_id,
            predecessor_epoch,
            successor_identity_seed,
            key_material_commitment,
        }
    }

    /// Grant identity.
    pub fn grant_id(&self) -> GrantId {
        self.grant_id
    }

    /// Store identity (same principal — StoreId unchanged).
    pub fn store_id(&self) -> StoreId {
        self.store_id
    }

    /// Predecessor FenceEpoch named in the payload (one-shot quorum key).
    pub fn predecessor_epoch(&self) -> FenceEpoch {
        self.predecessor_epoch
    }

    /// Successor identity seed for WriteAuthority mint (same StoreId).
    pub fn successor_identity_seed(&self) -> &[u8; 32] {
        &self.successor_identity_seed
    }

    /// Key-material commitment.
    pub fn key_material_commitment(&self) -> &[u8; 32] {
        &self.key_material_commitment
    }
}

/// Closed sum of grant seeds that [`materialize`] accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grant {
    /// Different-principal fork.
    Fork(ForkGrant),
    /// Same-principal recovery.
    Recovery(RecoveryGrant),
}

/// Pure materialization of a grant seed: successor identity + derivation inputs.
///
/// Same grant → same result every time (deterministic, idempotent). Discovery
/// draws no identity entropy outside the grant seed. Not `Clone`: carries an
/// affine [`WriteAuthority`].
#[derive(Debug, PartialEq, Eq)]
pub struct MaterializedGrant {
    grant_id: GrantId,
    /// Successor StoreId (new for Fork; same as predecessor for Recovery).
    store_id: StoreId,
    /// Successor CryptoDomain at the post-materialize epoch.
    crypto_domain: CryptoDomain,
    /// Fresh WriteAuthority for the successor (never in-place reissue of the old token).
    write_authority: WriteAuthority,
    /// Derivation inputs sealed by the grant (key-material commitment echo).
    key_material_commitment: [u8; 32],
}

impl MaterializedGrant {
    /// Grant that produced this materialization.
    pub fn grant_id(&self) -> GrantId {
        self.grant_id
    }

    /// Successor Store identity.
    pub fn store_id(&self) -> StoreId {
        self.store_id
    }

    /// Successor CryptoDomain.
    pub fn crypto_domain(&self) -> CryptoDomain {
        self.crypto_domain
    }

    /// Borrow the successor WriteAuthority (affine — move via ownership of self).
    pub fn write_authority(&self) -> &WriteAuthority {
        &self.write_authority
    }

    /// Key-material commitment from the grant.
    pub fn key_material_commitment(&self) -> &[u8; 32] {
        &self.key_material_commitment
    }
}

/// Typed refuse from [`materialize`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum MaterializeRefuse {
    /// Second discovery incompatible with an existing materialization.
    #[error("GrantAlreadyMaterialized: grant {grant_id:?} already yielded successor {existing_successor:?}")]
    #[diagnostic(code(store::grants::already_materialized))]
    GrantAlreadyMaterialized {
        grant_id: GrantId,
        existing_successor: StoreId,
    },
    #[error("INVARIANT(FenceEpoch): epoch space exhausted at u64::MAX during recovery materialize")]
    #[diagnostic(code(store::grants::epoch_exhausted))]
    EpochSpaceExhausted,
}

/// Optional prior materialization witness for idempotent rediscovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriorMaterialization {
    grant_id: GrantId,
    successor: StoreId,
}

impl PriorMaterialization {
    /// Record that `grant_id` already materialized as `successor`.
    pub fn new(grant_id: GrantId, successor: StoreId) -> Self {
        Self {
            grant_id,
            successor,
        }
    }

    /// Grant id.
    pub fn grant_id(self) -> GrantId {
        self.grant_id
    }

    /// Existing successor identity.
    pub fn successor(self) -> StoreId {
        self.successor
    }
}

/// Pure / deterministic / idempotent materialization of a grant seed.
///
/// Same grant → same successor StoreId and derivation inputs every time.
/// Discovery draws **no** identity entropy outside the grant seed.
///
/// When `prior` names this grant:
/// - matching successor → converge (return the same materialization again);
/// - mismatched successor → [`MaterializeRefuse::GrantAlreadyMaterialized`].
///
/// Original lineage continues untouched (ForkGrant). Recovery mints a new
/// WriteAuthority for the same StoreId under a new CryptoDomain — never
/// in-place reissue for the same epoch.
pub fn materialize(
    grant: &Grant,
    prior: Option<PriorMaterialization>,
) -> Result<MaterializedGrant, MaterializeRefuse> {
    let computed = match grant {
        Grant::Fork(fork) => materialize_fork(fork),
        Grant::Recovery(recovery) => materialize_recovery(recovery)?,
    };
    if let Some(prior) = prior {
        if prior.grant_id() == computed.grant_id() {
            if prior.successor() != computed.store_id() {
                return Err(MaterializeRefuse::GrantAlreadyMaterialized {
                    grant_id: prior.grant_id(),
                    existing_successor: prior.successor(),
                });
            }
            // Idempotent converge: recompute yields the same successor.
        }
    }
    Ok(computed)
}

fn materialize_fork(fork: &ForkGrant) -> MaterializedGrant {
    let store_id = derive_fork_store_id(fork);
    let fence_epoch = FenceEpoch::genesis(store_id);
    let crypto_domain = CryptoDomain::new(store_id, fence_epoch);
    let token_id = derive_fork_write_token(fork, store_id);
    let write_authority = WriteAuthority::mint(store_id, token_id);
    MaterializedGrant {
        grant_id: fork.grant_id(),
        store_id,
        crypto_domain,
        write_authority,
        key_material_commitment: *fork.key_material_commitment(),
    }
}

fn materialize_recovery(
    recovery: &RecoveryGrant,
) -> Result<MaterializedGrant, MaterializeRefuse> {
    // Same StoreId; new CryptoDomain at successor epoch; new WriteAuthority.
    let store_id = recovery.store_id();
    let next_epoch = recovery
        .predecessor_epoch()
        .successor()
        .map_err(|_| MaterializeRefuse::EpochSpaceExhausted)?;
    let crypto_domain = CryptoDomain::new(store_id, next_epoch);
    let token_id = derive_recovery_write_token(recovery);
    let write_authority = WriteAuthority::mint(store_id, token_id);
    Ok(MaterializedGrant {
        grant_id: recovery.grant_id(),
        store_id,
        crypto_domain,
        write_authority,
        key_material_commitment: *recovery.key_material_commitment(),
    })
}

fn derive_fork_store_id(fork: &ForkGrant) -> StoreId {
    let mut h = Sha256::new();
    h.update(b"kyzo.store_id.fork.v1");
    h.update(fork.grant_id().as_bytes());
    h.update(fork.predecessor_store().as_bytes());
    h.update(fork.fork_point_root());
    h.update(fork.successor_principal());
    h.update(fork.identity_seed());
    h.update(fork.key_material_commitment());
    StoreId::from_digest(h.finalize().into())
}

fn derive_fork_write_token(fork: &ForkGrant, store_id: StoreId) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"kyzo.write_authority.fork.v1");
    h.update(store_id.as_bytes());
    h.update(fork.grant_id().as_bytes());
    h.update(fork.identity_seed());
    h.update(fork.key_material_commitment());
    h.finalize().into()
}

fn derive_recovery_write_token(recovery: &RecoveryGrant) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"kyzo.write_authority.recovery.v1");
    h.update(recovery.store_id().as_bytes());
    h.update(recovery.grant_id().as_bytes());
    h.update(u64::to_be_bytes(recovery.predecessor_epoch().get()));
    h.update(recovery.successor_identity_seed());
    h.update(recovery.key_material_commitment());
    h.finalize().into()
}

/// Ancestor-epoch plaintext read grant — O(epochs) rewrap.
///
/// Cross-fork foreign-CryptoDomain plaintext is Unconstructible.
/// AuditKey ≠ AncestorReadGrant ≠ decrypt ≠ WriteAuthority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AncestorReadGrant {
    store_id: StoreId,
    /// Inclusive epoch range this grant may rewrap under the holding KEK.
    from_epoch: FenceEpoch,
    to_epoch: FenceEpoch,
}

/// Typed refuse constructing / using [`AncestorReadGrant`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum AncestorReadRefuse {
    #[error("AncestorReadGrant: foreign CryptoDomain plaintext is Unconstructible")]
    #[diagnostic(code(store::grants::foreign_ancestor))]
    ForeignCryptoDomain,
    #[error("AncestorReadGrant: epoch range inverted or empty")]
    #[diagnostic(code(store::grants::ancestor_range))]
    InvalidEpochRange,
}

impl AncestorReadGrant {
    /// Seal an O(epochs) rewrap grant for one Store's ancestor domains.
    pub fn new(
        store_id: StoreId,
        from_epoch: FenceEpoch,
        to_epoch: FenceEpoch,
    ) -> Result<Self, AncestorReadRefuse> {
        if from_epoch > to_epoch {
            return Err(AncestorReadRefuse::InvalidEpochRange);
        }
        Ok(Self {
            store_id,
            from_epoch,
            to_epoch,
        })
    }

    /// Store this grant covers.
    pub fn store_id(&self) -> StoreId {
        self.store_id
    }

    /// First covered epoch.
    pub fn from_epoch(&self) -> FenceEpoch {
        self.from_epoch
    }

    /// Last covered epoch.
    pub fn to_epoch(&self) -> FenceEpoch {
        self.to_epoch
    }

    /// Authorize rewrap for a domain — foreign StoreId refuses.
    pub fn authorize(&self, domain: CryptoDomain) -> Result<(), AncestorReadRefuse> {
        if domain.store_id() != self.store_id {
            return Err(AncestorReadRefuse::ForeignCryptoDomain);
        }
        let epoch = domain.fence_epoch();
        if epoch < self.from_epoch || epoch > self.to_epoch {
            return Err(AncestorReadRefuse::InvalidEpochRange);
        }
        Ok(())
    }
}
