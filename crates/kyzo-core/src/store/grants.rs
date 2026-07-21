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
//! [`AncestorReadGrant`], [`RecoveryQuorumProof`].
//!
//! Bans: discovery-time identity entropy outside the grant seed; grant-time
//! kill of the original token; post-grant shared-confidentiality Rotate;
//! in-place WriteAuthority reissue for the same epoch; signatureless recovery
//! mint of [`WriteAuthority`].

use std::collections::BTreeSet;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

use super::authority::{RecoveryMatrix, RecoveryPublicKey, WriteAuthority};
use super::epoch::{CryptoDomain, FenceEpoch};
use super::open::StoreId;

/// Domain-separated prefix for recovery quorum signatures.
const RECOVERY_QUORUM_DOMAIN: &[u8] = b"kyzo.recovery_quorum.v1";
/// Domain-separated prefix for RecoveryMatrix digests bound into proofs.
const RECOVERY_MATRIX_DIGEST_DOMAIN: &[u8] = b"kyzo.recovery_matrix.v1";
/// Domain-separated prefix for RecoveryGrant payload digests.
const RECOVERY_GRANT_PAYLOAD_DOMAIN: &[u8] = b"kyzo.recovery_grant.payload.v1";

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

/// Fork-point state root sealed in a ForkGrant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ForkPointRoot([u8; 32]);

impl ForkPointRoot {
    /// Wrap an already-proven fork-point root.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Borrow the digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for ForkPointRoot {
    fn from(digest: [u8; 32]) -> Self {
        Self(digest)
    }
}

/// Successor principal binding in a ForkGrant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SuccessorPrincipal([u8; 32]);

impl SuccessorPrincipal {
    /// Wrap an already-proven successor principal digest.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Borrow the digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for SuccessorPrincipal {
    fn from(digest: [u8; 32]) -> Self {
        Self(digest)
    }
}

/// Successor identity seed (sole entropy for materialize).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IdentitySeed([u8; 32]);

impl IdentitySeed {
    /// Wrap an already-proven identity seed.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Borrow the seed bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for IdentitySeed {
    fn from(digest: [u8; 32]) -> Self {
        Self(digest)
    }
}

/// Key-material commitment bound into a grant seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyMaterialCommitment([u8; 32]);

impl KeyMaterialCommitment {
    /// Wrap an already-proven key-material commitment.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Borrow the commitment bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for KeyMaterialCommitment {
    fn from(digest: [u8; 32]) -> Self {
        Self(digest)
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
    fork_point_root: ForkPointRoot,
    successor_principal: SuccessorPrincipal,
    identity_seed: IdentitySeed,
    key_material_commitment: KeyMaterialCommitment,
}

impl ForkGrant {
    /// Construct a fork grant seed from its sealed payload fields.
    pub fn new(
        grant_id: GrantId,
        predecessor_store: StoreId,
        fork_point_root: impl Into<ForkPointRoot>,
        successor_principal: impl Into<SuccessorPrincipal>,
        identity_seed: impl Into<IdentitySeed>,
        key_material_commitment: impl Into<KeyMaterialCommitment>,
    ) -> Self {
        Self {
            grant_id,
            predecessor_store,
            fork_point_root: fork_point_root.into(),
            successor_principal: successor_principal.into(),
            identity_seed: identity_seed.into(),
            key_material_commitment: key_material_commitment.into(),
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
    pub fn fork_point_root(&self) -> &ForkPointRoot {
        &self.fork_point_root
    }

    /// Successor principal binding.
    pub fn successor_principal(&self) -> &SuccessorPrincipal {
        &self.successor_principal
    }

    /// Successor identity seed (sole entropy for materialize — no discovery-time draw).
    pub fn identity_seed(&self) -> &IdentitySeed {
        &self.identity_seed
    }

    /// Key-material commitment.
    pub fn key_material_commitment(&self) -> &KeyMaterialCommitment {
        &self.key_material_commitment
    }
}

/// Opaque sealed evidence that a [`RecoveryMatrix`] quorum signed a payload.
///
/// No public free constructor — mint only via [`RecoveryQuorumProof::verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryQuorumProof {
    matrix_digest: [u8; 32],
    payload_digest: [u8; 32],
    _priv: (),
}

impl RecoveryQuorumProof {
    /// Verify ed25519 quorum signatures against `matrix` and seal the proof.
    ///
    /// Each signature is over `kyzo.recovery_quorum.v1 || payload_digest`.
    /// Only verifying keys that appear in `matrix.keys()` count; distinct
    /// valid signers must meet `matrix.threshold()`.
    pub fn verify(
        matrix: &RecoveryMatrix,
        payload_digest: &[u8; 32],
        signatures: &[(RecoveryPublicKey, [u8; 64])],
    ) -> Result<Self, MaterializeRefuse> {
        let mut message = Vec::with_capacity(RECOVERY_QUORUM_DOMAIN.len() + 32);
        message.extend_from_slice(RECOVERY_QUORUM_DOMAIN);
        message.extend_from_slice(payload_digest);

        let matrix_key_bytes: BTreeSet<[u8; 32]> =
            matrix.keys().iter().map(|k| *k.as_bytes()).collect();
        let mut distinct_valid: BTreeSet<[u8; 32]> = BTreeSet::new();

        for (pk, sig_bytes) in signatures {
            let pk_bytes = *pk.as_bytes();
            if !matrix_key_bytes.contains(&pk_bytes) || distinct_valid.contains(&pk_bytes) {
                continue;
            }
            let Ok(verifying) = VerifyingKey::from_bytes(&pk_bytes) else {
                continue;
            };
            let Ok(signature) = Signature::try_from(sig_bytes.as_slice()) else {
                continue;
            };
            if verifying.verify(message.as_slice(), &signature).is_ok() {
                distinct_valid.insert(pk_bytes);
            }
        }

        let have = distinct_valid.len() as u32;
        let need = matrix.threshold();
        if have < need {
            return Err(MaterializeRefuse::QuorumInsufficient { have, need });
        }

        Ok(Self {
            matrix_digest: recovery_matrix_digest(matrix),
            payload_digest: *payload_digest,
            _priv: (),
        })
    }

    /// Matrix digest sealed into this proof.
    pub fn matrix_digest(&self) -> &[u8; 32] {
        &self.matrix_digest
    }

    /// Payload digest this quorum signed.
    pub fn payload_digest(&self) -> &[u8; 32] {
        &self.payload_digest
    }
}

/// Canonical digest of a [`RecoveryMatrix`] (threshold + ordered key bytes).
fn recovery_matrix_digest(matrix: &RecoveryMatrix) -> [u8; 32] {
    let mut ordered: Vec<[u8; 32]> = matrix.keys().iter().map(|k| *k.as_bytes()).collect();
    ordered.sort_unstable();
    let mut h = Sha256::new();
    h.update(RECOVERY_MATRIX_DIGEST_DOMAIN);
    h.update(u32::to_be_bytes(matrix.threshold()));
    for key in &ordered {
        h.update(key);
    }
    h.finalize().into()
}

/// Payload digest a recovery quorum must sign for a grant's sealed fields.
pub fn recovery_grant_payload_digest(
    grant_id: GrantId,
    store_id: StoreId,
    predecessor_epoch: FenceEpoch,
    successor_identity_seed: &IdentitySeed,
    key_material_commitment: &KeyMaterialCommitment,
) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(RECOVERY_GRANT_PAYLOAD_DOMAIN);
    h.update(grant_id.as_bytes());
    h.update(store_id.as_bytes());
    h.update(u64::to_be_bytes(predecessor_epoch.get()));
    h.update(predecessor_epoch.store_id().as_bytes());
    h.update(successor_identity_seed.as_bytes());
    h.update(key_material_commitment.as_bytes());
    h.finalize().into()
}

/// Same-principal recovery seed — one-shot quorum under RecoveryMatrix.
///
/// Predecessor FenceEpoch in the signed payload. A second valid grant for
/// one predecessor epoch is quorum equivocation → poison for the signing
/// set's authority, never a second lineage. Recovery keys are a distinct
/// custodian class. Obeys the same seed / [`materialize`] law as ForkGrant.
/// Carries a verified [`RecoveryQuorumProof`] — signatureless mint is Unconstructible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryGrant {
    grant_id: GrantId,
    store_id: StoreId,
    predecessor_epoch: FenceEpoch,
    successor_identity_seed: IdentitySeed,
    key_material_commitment: KeyMaterialCommitment,
    quorum_proof: RecoveryQuorumProof,
}

impl RecoveryGrant {
    /// Construct a recovery grant seed bound to a verified quorum proof.
    ///
    /// The proof's payload digest must match this grant's sealed fields;
    /// signatureless construction that mints power is condemned.
    pub fn new(
        grant_id: GrantId,
        store_id: StoreId,
        predecessor_epoch: FenceEpoch,
        successor_identity_seed: impl Into<IdentitySeed>,
        key_material_commitment: impl Into<KeyMaterialCommitment>,
        quorum_proof: RecoveryQuorumProof,
    ) -> Result<Self, MaterializeRefuse> {
        assert_eq!(
            predecessor_epoch.store_id(),
            store_id,
            "INVARIANT(RecoveryGrant): predecessor FenceEpoch must bind StoreId"
        );
        let successor_identity_seed = successor_identity_seed.into();
        let key_material_commitment = key_material_commitment.into();
        let expected = recovery_grant_payload_digest(
            grant_id,
            store_id,
            predecessor_epoch,
            &successor_identity_seed,
            &key_material_commitment,
        );
        if quorum_proof.payload_digest() != &expected {
            return Err(MaterializeRefuse::QuorumUnverified);
        }
        Ok(Self {
            grant_id,
            store_id,
            predecessor_epoch,
            successor_identity_seed,
            key_material_commitment,
            quorum_proof,
        })
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
    pub fn successor_identity_seed(&self) -> &IdentitySeed {
        &self.successor_identity_seed
    }

    /// Key-material commitment.
    pub fn key_material_commitment(&self) -> &KeyMaterialCommitment {
        &self.key_material_commitment
    }

    /// Verified quorum-signature evidence bound into this grant.
    pub fn quorum_proof(&self) -> &RecoveryQuorumProof {
        &self.quorum_proof
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
    key_material_commitment: KeyMaterialCommitment,
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
    pub fn key_material_commitment(&self) -> &KeyMaterialCommitment {
        &self.key_material_commitment
    }
}

/// Typed refuse from [`materialize`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum MaterializeRefuse {
    /// Second discovery incompatible with an existing materialization.
    #[error(
        "GrantAlreadyMaterialized: grant {grant_id:?} already yielded successor {existing_successor:?}"
    )]
    #[diagnostic(code(store::grants::already_materialized))]
    GrantAlreadyMaterialized {
        grant_id: GrantId,
        existing_successor: StoreId,
    },
    #[error("INVARIANT(FenceEpoch): epoch space exhausted at u64::MAX during recovery materialize")]
    #[diagnostic(code(store::grants::epoch_exhausted))]
    EpochSpaceExhausted,
    /// Recovery quorum signatures / matrix binding failed verification.
    #[error("QuorumUnverified: recovery quorum proof does not bind the store RecoveryMatrix")]
    #[diagnostic(code(store::grants::quorum_unverified))]
    QuorumUnverified,
    /// Distinct valid matrix signers below the RecoveryMatrix threshold.
    #[error(
        "QuorumInsufficient: recovery quorum has {have} distinct valid signatures, need {need}"
    )]
    #[diagnostic(code(store::grants::quorum_insufficient))]
    QuorumInsufficient { have: u32, need: u32 },
    /// Recovery materialize requires the store's sealed RecoveryMatrix.
    #[error("RecoveryMatrixAbsent: recovery materialize requires the store RecoveryMatrix")]
    #[diagnostic(code(store::grants::recovery_matrix_absent))]
    RecoveryMatrixAbsent,
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
/// in-place reissue for the same epoch — and only when `recovery_matrix`
/// matches the grant's sealed [`RecoveryQuorumProof`].
pub fn materialize(
    grant: &Grant,
    prior: Option<PriorMaterialization>,
    recovery_matrix: Option<&RecoveryMatrix>,
) -> Result<MaterializedGrant, MaterializeRefuse> {
    let computed = match grant {
        Grant::Fork(fork) => materialize_fork(fork),
        Grant::Recovery(recovery) => materialize_recovery(recovery, recovery_matrix)?,
    };
    if let Some(prior) = prior
        && prior.grant_id() == computed.grant_id()
        && prior.successor() != computed.store_id()
    {
        return Err(MaterializeRefuse::GrantAlreadyMaterialized {
            grant_id: prior.grant_id(),
            existing_successor: prior.successor(),
        });
    }
    // Idempotent converge: recompute yields the same successor.
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
    recovery_matrix: Option<&RecoveryMatrix>,
) -> Result<MaterializedGrant, MaterializeRefuse> {
    let matrix = recovery_matrix.ok_or(MaterializeRefuse::RecoveryMatrixAbsent)?;
    // Sealed proof is trusted only when its matrix_digest matches THIS store matrix.
    if recovery.quorum_proof().matrix_digest() != &recovery_matrix_digest(matrix) {
        return Err(MaterializeRefuse::QuorumUnverified);
    }
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
    h.update(fork.fork_point_root().as_bytes());
    h.update(fork.successor_principal().as_bytes());
    h.update(fork.identity_seed().as_bytes());
    h.update(fork.key_material_commitment().as_bytes());
    StoreId::from_digest(h.finalize().into())
}

fn derive_fork_write_token(fork: &ForkGrant, store_id: StoreId) -> super::authority::WriteTokenId {
    let mut h = Sha256::new();
    h.update(b"kyzo.write_authority.fork.v1");
    h.update(store_id.as_bytes());
    h.update(fork.grant_id().as_bytes());
    h.update(fork.identity_seed().as_bytes());
    h.update(fork.key_material_commitment().as_bytes());
    super::authority::WriteTokenId::from_digest(h.finalize().into())
}

fn derive_recovery_write_token(recovery: &RecoveryGrant) -> super::authority::WriteTokenId {
    let mut h = Sha256::new();
    h.update(b"kyzo.write_authority.recovery.v1");
    h.update(recovery.store_id().as_bytes());
    h.update(recovery.grant_id().as_bytes());
    h.update(u64::to_be_bytes(recovery.predecessor_epoch().get()));
    h.update(recovery.predecessor_epoch().store_id().as_bytes());
    h.update(recovery.successor_identity_seed().as_bytes());
    h.update(recovery.key_material_commitment().as_bytes());
    super::authority::WriteTokenId::from_digest(h.finalize().into())
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
    #[allow(clippy::wrong_self_convention)] // from_epoch is a field accessor on the grant, not a converting constructor
    #[allow(clippy::wrong_self_convention)] // from_epoch is a field accessor on the grant, not a converting constructor
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

/// Test-only: ed25519 recovery-custodian sign over the quorum domain.
/// Path-wired dst callers use this so positive recovery paths still compile.
#[cfg(test)]
pub(crate) fn sign_recovery_quorum(
    seed: [u8; 32],
    payload_digest: &[u8; 32],
) -> (RecoveryPublicKey, [u8; 64]) {
    use ed25519_dalek::{Signer, SigningKey};

    let signing = SigningKey::from_bytes(&seed);
    let public = RecoveryPublicKey::from_bytes(signing.verifying_key().to_bytes());
    let mut message = Vec::with_capacity(RECOVERY_QUORUM_DOMAIN.len() + 32);
    message.extend_from_slice(RECOVERY_QUORUM_DOMAIN);
    message.extend_from_slice(payload_digest);
    let sig = signing.sign(message.as_slice()).to_bytes();
    (public, sig)
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};

    use super::*;
    use crate::store::authority::RecoveryMatrix;

    /// Test-only recovery custodian signing key (ed25519 seed → verifying bytes).
    struct RecoverySigningKey {
        signing: SigningKey,
        public: RecoveryPublicKey,
    }

    impl RecoverySigningKey {
        fn from_seed(seed: [u8; 32]) -> Self {
            let signing = SigningKey::from_bytes(&seed);
            let public = RecoveryPublicKey::from_bytes(signing.verifying_key().to_bytes());
            Self { signing, public }
        }

        fn public_key(&self) -> RecoveryPublicKey {
            self.public
        }

        fn sign_quorum(&self, payload_digest: &[u8; 32]) -> [u8; 64] {
            let mut message = Vec::with_capacity(RECOVERY_QUORUM_DOMAIN.len() + 32);
            message.extend_from_slice(RECOVERY_QUORUM_DOMAIN);
            message.extend_from_slice(payload_digest);
            self.signing.sign(message.as_slice()).to_bytes()
        }
    }

    fn matrix_2of3(seeds: [[u8; 32]; 3]) -> (Vec<RecoverySigningKey>, RecoveryMatrix) {
        let keys: Vec<RecoverySigningKey> = seeds
            .into_iter()
            .map(RecoverySigningKey::from_seed)
            .collect();
        let matrix = RecoveryMatrix::new(
            2,
            keys.iter().map(RecoverySigningKey::public_key).collect::<Vec<_>>(),
        )
        .expect("2-of-3 matrix");
        (keys, matrix)
    }

    /// Nasty: victim StoreId+epoch grant with empty/forged quorum must not mint WriteAuthority.
    #[test]
    fn recovery_grant_without_valid_quorum_refuses_write_authority() {
        let victim = StoreId::from_digest([0x71; 32]);
        let pred_epoch = FenceEpoch::genesis(victim);
        let grant_id = GrantId::from_bytes([0x90; 32]);
        let successor_seed = IdentitySeed::from_digest([0xEE; 32]);
        let commitment = KeyMaterialCommitment::from_digest([0xEF; 32]);
        let payload = recovery_grant_payload_digest(
            grant_id,
            victim,
            pred_epoch,
            &successor_seed,
            &commitment,
        );

        let (custodians, store_matrix) =
            matrix_2of3([[0xA1; 32], [0xA2; 32], [0xA3; 32]]);

        // Empty signatures — no proof, no grant power.
        assert_eq!(
            RecoveryQuorumProof::verify(&store_matrix, &payload, &[]),
            Err(MaterializeRefuse::QuorumInsufficient { have: 0, need: 2 })
        );

        // Forged signature bytes under a matrix key — still insufficient.
        let forged = [(
            custodians[0].public_key(),
            [0xFFu8; 64],
        )];
        assert_eq!(
            RecoveryQuorumProof::verify(&store_matrix, &payload, &forged),
            Err(MaterializeRefuse::QuorumInsufficient { have: 0, need: 2 })
        );

        // Attacker mints a real quorum against a *different* matrix, names the victim,
        // then tries materialize against the victim store's matrix — must refuse.
        let (attacker_keys, attacker_matrix) =
            matrix_2of3([[0xB1; 32], [0xB2; 32], [0xB3; 32]]);
        let attacker_sigs = [
            (
                attacker_keys[0].public_key(),
                attacker_keys[0].sign_quorum(&payload),
            ),
            (
                attacker_keys[1].public_key(),
                attacker_keys[1].sign_quorum(&payload),
            ),
        ];
        let forged_proof = RecoveryQuorumProof::verify(&attacker_matrix, &payload, &attacker_sigs)
            .expect("attacker quorum against their own matrix");
        let grant = RecoveryGrant::new(
            grant_id,
            victim,
            pred_epoch,
            successor_seed,
            commitment,
            forged_proof,
        )
        .expect("payload binds; proof is sealed against wrong matrix");

        // Absent store matrix → refuse (no WriteAuthority).
        let absent = materialize(&Grant::Recovery(grant.clone()), None, None);
        assert_eq!(absent, Err(MaterializeRefuse::RecoveryMatrixAbsent));

        // Wrong store matrix → QuorumUnverified (no WriteAuthority).
        let wrong = materialize(
            &Grant::Recovery(grant),
            None,
            Some(&store_matrix),
        );
        assert_eq!(wrong, Err(MaterializeRefuse::QuorumUnverified));
    }

    #[test]
    fn recovery_grant_valid_quorum_materializes() {
        let store_id = StoreId::from_digest([0xC0; 32]);
        let pred_epoch = FenceEpoch::genesis(store_id);
        let grant_id = GrantId::from_bytes([0x91; 32]);
        let successor_seed = IdentitySeed::from_digest([0xD1; 32]);
        let commitment = KeyMaterialCommitment::from_digest([0xD2; 32]);
        let payload = recovery_grant_payload_digest(
            grant_id,
            store_id,
            pred_epoch,
            &successor_seed,
            &commitment,
        );
        let (keys, matrix) = matrix_2of3([[0xC1; 32], [0xC2; 32], [0xC3; 32]]);
        let sigs = [
            (keys[0].public_key(), keys[0].sign_quorum(&payload)),
            (keys[1].public_key(), keys[1].sign_quorum(&payload)),
        ];
        let proof = RecoveryQuorumProof::verify(&matrix, &payload, &sigs).expect("quorum");
        let grant = RecoveryGrant::new(
            grant_id,
            store_id,
            pred_epoch,
            successor_seed,
            commitment,
            proof,
        )
        .expect("grant");
        let matured = materialize(&Grant::Recovery(grant), None, Some(&matrix))
            .expect("valid quorum must mint");
        assert_eq!(matured.store_id(), store_id);
        assert_ne!(matured.crypto_domain().fence_epoch(), pred_epoch);
    }
}
