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
//! [`AncestorReadGrant`], [`RecoveryQuorumProof`], [`PredecessorConsentProof`],
//! [`PredecessorConsentTable`].
//!
//! Bans: discovery-time identity entropy outside the grant seed; grant-time
//! kill of the original token; post-grant shared-confidentiality Rotate;
//! in-place WriteAuthority reissue for the same epoch; signatureless recovery
//! mint of [`WriteAuthority`]; signatureless fork mint of [`WriteAuthority`];
//! caller-supplied consent verifying keys as the trust root (self-issued consent).

use std::collections::{BTreeMap, BTreeSet};

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
/// Domain-separated prefix for predecessor fork-consent signatures.
const FORK_CONSENT_DOMAIN: &[u8] = b"kyzo.fork_consent.v1";
/// Domain-separated prefix for ForkGrant payload digests.
const FORK_GRANT_PAYLOAD_DOMAIN: &[u8] = b"kyzo.fork_grant.payload.v1";

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

/// Sealed registry of predecessor-consent verifying keys keyed by [`StoreId`].
///
/// Register at genesis/seal time. [`PredecessorConsentProof::verify`] and
/// [`materialize`] resolve the trust root by StoreId lookup — never a
/// caller-supplied verifying key (self-issued consent is Unconstructible).
#[derive(Debug, Default, Clone)]
pub struct PredecessorConsentTable {
    /// store_id bytes → ed25519 verifying key bytes (32).
    keys: BTreeMap<[u8; 32], [u8; 32]>,
}

impl PredecessorConsentTable {
    /// Empty sealed consent-key registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the consent verifying key for a StoreId (genesis/seal door).
    ///
    /// Invalid ed25519 material refuses. Overwrites a prior key for the same
    /// StoreId (seal construction is single-writer).
    pub fn insert(
        &mut self,
        store_id: StoreId,
        verifying_key: [u8; 32],
    ) -> Result<(), MaterializeRefuse> {
        VerifyingKey::from_bytes(&verifying_key)
            .map_err(|_| MaterializeRefuse::ConsentUnverified)?;
        self.keys.insert(*store_id.as_bytes(), verifying_key);
        Ok(())
    }

    /// Lookup the sealed consent verifying key for `store_id`, if registered.
    pub fn get(&self, store_id: StoreId) -> Option<&[u8; 32]> {
        self.keys.get(store_id.as_bytes())
    }
}

/// Digest of a consent verifying key (bound into [`PredecessorConsentProof`]).
fn consent_key_id_digest(verifying_key: &[u8; 32]) -> [u8; 32] {
    let mut key_h = Sha256::new();
    key_h.update(b"kyzo.fork_consent.key_id.v1");
    key_h.update(verifying_key);
    key_h.finalize().into()
}

/// Opaque sealed evidence that a predecessor consented to a fork payload.
///
/// No public free constructor — mint only via [`PredecessorConsentProof::verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredecessorConsentProof {
    predecessor_store: StoreId,
    payload_digest: [u8; 32],
    key_id_digest: [u8; 32],
    _priv: (),
}

impl PredecessorConsentProof {
    /// Verify an ed25519 consent signature against the sealed table and seal the proof.
    ///
    /// Resolves the verifying key from `consent_table` for `predecessor_store`
    /// — never accepts a caller-supplied verifying key as the trust root.
    /// Message: `kyzo.fork_consent.v1 || predecessor_store || payload_digest`.
    pub fn verify(
        consent_table: &PredecessorConsentTable,
        predecessor_store: StoreId,
        payload_digest: &[u8; 32],
        signature: &[u8; 64],
    ) -> Result<Self, MaterializeRefuse> {
        let Some(consent_verifying_key) = consent_table.get(predecessor_store) else {
            return Err(MaterializeRefuse::ConsentKeyUnknown);
        };
        let Ok(verifying) = VerifyingKey::from_bytes(consent_verifying_key) else {
            return Err(MaterializeRefuse::ConsentUnverified);
        };
        let Ok(sig) = Signature::try_from(signature.as_slice()) else {
            return Err(MaterializeRefuse::ConsentUnverified);
        };
        let mut message =
            Vec::with_capacity(FORK_CONSENT_DOMAIN.len() + 32 + 32);
        message.extend_from_slice(FORK_CONSENT_DOMAIN);
        message.extend_from_slice(predecessor_store.as_bytes());
        message.extend_from_slice(payload_digest);
        if verifying.verify(message.as_slice(), &sig).is_err() {
            return Err(MaterializeRefuse::ConsentUnverified);
        }
        Ok(Self {
            predecessor_store,
            payload_digest: *payload_digest,
            key_id_digest: consent_key_id_digest(consent_verifying_key),
            _priv: (),
        })
    }

    /// Predecessor StoreId sealed into this proof.
    pub fn predecessor_store(&self) -> StoreId {
        self.predecessor_store
    }

    /// Payload digest the predecessor consented to.
    pub fn payload_digest(&self) -> &[u8; 32] {
        &self.payload_digest
    }

    /// Digest of the consent verifying key sealed into this proof.
    pub fn key_id_digest(&self) -> &[u8; 32] {
        &self.key_id_digest
    }
}

/// Payload digest a predecessor must consent to for a fork grant's sealed fields.
pub fn fork_grant_payload_digest(
    grant_id: GrantId,
    predecessor_store: StoreId,
    fork_point_root: &ForkPointRoot,
    successor_principal: &SuccessorPrincipal,
    identity_seed: &IdentitySeed,
    key_material_commitment: &KeyMaterialCommitment,
) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(FORK_GRANT_PAYLOAD_DOMAIN);
    h.update(grant_id.as_bytes());
    h.update(predecessor_store.as_bytes());
    h.update(fork_point_root.as_bytes());
    h.update(successor_principal.as_bytes());
    h.update(identity_seed.as_bytes());
    h.update(key_material_commitment.as_bytes());
    h.finalize().into()
}

/// Different-principal fork seed — not an event.
///
/// Binds GrantId, predecessor StoreId, fork-point root, successor principal,
/// identity seed, and key-material commitment. Changes nothing about the
/// original lineage. Materialization is at discovery via [`materialize`].
/// Carries a verified [`PredecessorConsentProof`] — signatureless mint is Unconstructible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkGrant {
    grant_id: GrantId,
    predecessor_store: StoreId,
    fork_point_root: ForkPointRoot,
    successor_principal: SuccessorPrincipal,
    identity_seed: IdentitySeed,
    key_material_commitment: KeyMaterialCommitment,
    consent_proof: PredecessorConsentProof,
}

impl ForkGrant {
    /// Construct a fork grant seed bound to a verified predecessor consent proof.
    ///
    /// The proof's predecessor and payload digest must match this grant's sealed
    /// fields; signatureless construction that mints power is condemned.
    pub fn new(
        grant_id: GrantId,
        predecessor_store: StoreId,
        fork_point_root: impl Into<ForkPointRoot>,
        successor_principal: impl Into<SuccessorPrincipal>,
        identity_seed: impl Into<IdentitySeed>,
        key_material_commitment: impl Into<KeyMaterialCommitment>,
        consent_proof: PredecessorConsentProof,
    ) -> Result<Self, MaterializeRefuse> {
        let fork_point_root = fork_point_root.into();
        let successor_principal = successor_principal.into();
        let identity_seed = identity_seed.into();
        let key_material_commitment = key_material_commitment.into();
        if consent_proof.predecessor_store() != predecessor_store {
            return Err(MaterializeRefuse::ConsentMismatch);
        }
        let expected = fork_grant_payload_digest(
            grant_id,
            predecessor_store,
            &fork_point_root,
            &successor_principal,
            &identity_seed,
            &key_material_commitment,
        );
        if consent_proof.payload_digest() != &expected {
            return Err(MaterializeRefuse::ConsentUnverified);
        }
        Ok(Self {
            grant_id,
            predecessor_store,
            fork_point_root,
            successor_principal,
            identity_seed,
            key_material_commitment,
            consent_proof,
        })
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

    /// Verified predecessor-consent evidence bound into this grant.
    pub fn consent_proof(&self) -> &PredecessorConsentProof {
        &self.consent_proof
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
    /// Predecessor consent signature failed verification.
    #[error("ConsentUnverified: predecessor consent signature does not verify")]
    #[diagnostic(code(store::grants::consent_unverified))]
    ConsentUnverified,
    /// Consent proof does not name the grant's predecessor StoreId.
    #[error("ConsentMismatch: consent proof predecessor does not match the fork grant")]
    #[diagnostic(code(store::grants::consent_mismatch))]
    ConsentMismatch,
    /// No sealed consent verifying key is registered for the predecessor StoreId.
    #[error(
        "ConsentKeyUnknown: no sealed predecessor-consent verifying key for the named StoreId"
    )]
    #[diagnostic(code(store::grants::consent_key_unknown))]
    ConsentKeyUnknown,
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
/// Original lineage continues untouched (ForkGrant) and only when the grant
/// carries a verified [`PredecessorConsentProof`] for the named predecessor
/// whose verifying key is resolved from `consent_table` (never caller-supplied).
/// Recovery mints a new WriteAuthority for the same StoreId under a new
/// CryptoDomain — never in-place reissue for the same epoch — and only when
/// `recovery_matrix` matches the grant's sealed [`RecoveryQuorumProof`].
pub fn materialize(
    grant: &Grant,
    prior: Option<PriorMaterialization>,
    recovery_matrix: Option<&RecoveryMatrix>,
    consent_table: Option<&PredecessorConsentTable>,
) -> Result<MaterializedGrant, MaterializeRefuse> {
    let computed = match grant {
        Grant::Fork(fork) => materialize_fork(fork, consent_table)?,
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

fn materialize_fork(
    fork: &ForkGrant,
    consent_table: Option<&PredecessorConsentTable>,
) -> Result<MaterializedGrant, MaterializeRefuse> {
    let table = consent_table.ok_or(MaterializeRefuse::ConsentKeyUnknown)?;
    let Some(sealed_key) = table.get(fork.predecessor_store()) else {
        return Err(MaterializeRefuse::ConsentKeyUnknown);
    };
    // Sealed consent is trusted only when it still binds THIS grant's fields
    // and the sealed table key for the predecessor (not an attacker-chosen key).
    if fork.consent_proof().predecessor_store() != fork.predecessor_store() {
        return Err(MaterializeRefuse::ConsentMismatch);
    }
    let expected = fork_grant_payload_digest(
        fork.grant_id(),
        fork.predecessor_store(),
        fork.fork_point_root(),
        fork.successor_principal(),
        fork.identity_seed(),
        fork.key_material_commitment(),
    );
    if fork.consent_proof().payload_digest() != &expected {
        return Err(MaterializeRefuse::ConsentUnverified);
    }
    if fork.consent_proof().key_id_digest() != &consent_key_id_digest(sealed_key) {
        return Err(MaterializeRefuse::ConsentUnverified);
    }
    let store_id = derive_fork_store_id(fork);
    let fence_epoch = FenceEpoch::genesis(store_id);
    let crypto_domain = CryptoDomain::new(store_id, fence_epoch);
    let token_id = derive_fork_write_token(fork, store_id);
    let write_authority = WriteAuthority::mint(store_id, token_id);
    Ok(MaterializedGrant {
        grant_id: fork.grant_id(),
        store_id,
        crypto_domain,
        write_authority,
        key_material_commitment: *fork.key_material_commitment(),
    })
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

/// Test-only: ed25519 predecessor consent sign over the fork-consent domain.
/// Path-wired dst callers use this so positive fork paths still compile.
#[cfg(test)]
pub(crate) fn sign_fork_consent(
    seed: [u8; 32],
    predecessor_store: StoreId,
    payload_digest: &[u8; 32],
) -> ([u8; 32], [u8; 64]) {
    use ed25519_dalek::{Signer, SigningKey};

    let signing = SigningKey::from_bytes(&seed);
    let verifying = signing.verifying_key().to_bytes();
    let mut message =
        Vec::with_capacity(FORK_CONSENT_DOMAIN.len() + 32 + 32);
    message.extend_from_slice(FORK_CONSENT_DOMAIN);
    message.extend_from_slice(predecessor_store.as_bytes());
    message.extend_from_slice(payload_digest);
    let sig = signing.sign(message.as_slice()).to_bytes();
    (verifying, sig)
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
        let absent = materialize(&Grant::Recovery(grant.clone()), None, None, None);
        assert_eq!(absent, Err(MaterializeRefuse::RecoveryMatrixAbsent));

        // Wrong store matrix → QuorumUnverified (no WriteAuthority).
        let wrong = materialize(
            &Grant::Recovery(grant),
            None,
            Some(&store_matrix),
            None,
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
        let matured = materialize(&Grant::Recovery(grant), None, Some(&matrix), None)
            .expect("valid quorum must mint");
        assert_eq!(matured.store_id(), store_id);
        assert_ne!(matured.crypto_domain().fence_epoch(), pred_epoch);
    }

    /// Nasty: ForkGrant naming a real predecessor without valid consent must not mint.
    #[test]
    fn fork_grant_without_valid_consent_refuses_write_authority() {
        let victim = StoreId::from_digest([0x72; 32]);
        let grant_id = GrantId::from_bytes([0xA0; 32]);
        let fork_point = ForkPointRoot::from_digest([0xAA; 32]);
        let successor = SuccessorPrincipal::from_digest([0xBB; 32]);
        let identity = IdentitySeed::from_digest([0xCC; 32]);
        let commitment = KeyMaterialCommitment::from_digest([0xDD; 32]);
        let payload = fork_grant_payload_digest(
            grant_id,
            victim,
            &fork_point,
            &successor,
            &identity,
            &commitment,
        );

        let consent = ConsentSigningKey::from_seed([0xF1; 32]);
        let mut table = PredecessorConsentTable::new();
        table
            .insert(victim, *consent.verifying_key())
            .expect("register victim consent key");

        // Forged signature bytes — no proof, no grant power.
        assert_eq!(
            PredecessorConsentProof::verify(&table, victim, &payload, &[0xFFu8; 64]),
            Err(MaterializeRefuse::ConsentUnverified)
        );

        // Valid consent for a *different* store, then name the victim — ConsentMismatch.
        let other = StoreId::from_digest([0x73; 32]);
        let other_payload = fork_grant_payload_digest(
            grant_id,
            other,
            &fork_point,
            &successor,
            &identity,
            &commitment,
        );
        table
            .insert(other, *consent.verifying_key())
            .expect("register other consent key");
        let other_sig = consent.sign_consent(other, &other_payload);
        let wrong_store_proof =
            PredecessorConsentProof::verify(&table, other, &other_payload, &other_sig)
                .expect("consent for other store");
        assert_eq!(
            ForkGrant::new(
                grant_id,
                victim,
                fork_point,
                successor,
                identity,
                commitment,
                wrong_store_proof,
            ),
            Err(MaterializeRefuse::ConsentMismatch)
        );

        // Cross-stream: attacker signs a real consent for the victim under a
        // *different* payload (forged lineage fields), then tries to attach it
        // to the victim-named grant — construction and materialize refuse.
        let forged_lineage_payload = fork_grant_payload_digest(
            GrantId::from_bytes([0xA1; 32]),
            victim,
            &fork_point,
            &successor,
            &identity,
            &commitment,
        );
        let forged_sig = consent.sign_consent(victim, &forged_lineage_payload);
        let forged_lineage_proof = PredecessorConsentProof::verify(
            &table,
            victim,
            &forged_lineage_payload,
            &forged_sig,
        )
        .expect("consent verifies for forged lineage payload");
        assert_eq!(
            ForkGrant::new(
                grant_id,
                victim,
                fork_point,
                successor,
                identity,
                commitment,
                forged_lineage_proof,
            ),
            Err(MaterializeRefuse::ConsentUnverified)
        );
    }

    /// Nasty: consent signed by an attacker key NOT bound in the sealed table
    /// for predecessor_store must refuse — closes self-issued consent forge.
    #[test]
    fn fork_grant_attacker_own_key_not_bound_in_table_refuses() {
        let victim = StoreId::from_digest([0x75; 32]);
        let grant_id = GrantId::from_bytes([0xA3; 32]);
        let fork_point = ForkPointRoot::from_digest([0x55; 32]);
        let successor = SuccessorPrincipal::from_digest([0x66; 32]);
        let identity = IdentitySeed::from_digest([0x77; 32]);
        let commitment = KeyMaterialCommitment::from_digest([0x88; 32]);
        let payload = fork_grant_payload_digest(
            grant_id,
            victim,
            &fork_point,
            &successor,
            &identity,
            &commitment,
        );

        let legitimate = ConsentSigningKey::from_seed([0xD1; 32]);
        let attacker = ConsentSigningKey::from_seed([0xD2; 32]);

        // Sealed store authority: only the legitimate key is registered.
        let mut sealed_table = PredecessorConsentTable::new();
        sealed_table
            .insert(victim, *legitimate.verifying_key())
            .expect("register legitimate predecessor consent key");

        // Attacker signs with their own keypair — must not verify against sealed table.
        let attacker_sig = attacker.sign_consent(victim, &payload);
        assert_eq!(
            PredecessorConsentProof::verify(&sealed_table, victim, &payload, &attacker_sig),
            Err(MaterializeRefuse::ConsentUnverified)
        );

        // Unknown predecessor (no sealed key) → ConsentKeyUnknown, even with a
        // cryptographically valid signature under some key.
        let empty = PredecessorConsentTable::new();
        assert_eq!(
            PredecessorConsentProof::verify(&empty, victim, &payload, &attacker_sig),
            Err(MaterializeRefuse::ConsentKeyUnknown)
        );

        // Cross-stream: attacker seals a proof against a *private* table that
        // binds their own key to the victim StoreId, then materializes against
        // the real sealed store table — key_id mismatch must refuse.
        let mut attacker_table = PredecessorConsentTable::new();
        attacker_table
            .insert(victim, *attacker.verifying_key())
            .expect("attacker private table");
        let forged_proof =
            PredecessorConsentProof::verify(&attacker_table, victim, &payload, &attacker_sig)
                .expect("verifies only against attacker's private table");
        let grant = ForkGrant::new(
            grant_id,
            victim,
            fork_point,
            successor,
            identity,
            commitment,
            forged_proof,
        )
        .expect("payload binds; proof sealed against wrong table");

        assert_eq!(
            materialize(&Grant::Fork(grant.clone()), None, None, None),
            Err(MaterializeRefuse::ConsentKeyUnknown)
        );
        assert_eq!(
            materialize(&Grant::Fork(grant), None, None, Some(&sealed_table)),
            Err(MaterializeRefuse::ConsentUnverified)
        );
    }

    #[test]
    fn fork_grant_valid_consent_materializes() {
        let predecessor = StoreId::from_digest([0x74; 32]);
        let grant_id = GrantId::from_bytes([0xA2; 32]);
        let fork_point = ForkPointRoot::from_digest([0x11; 32]);
        let successor = SuccessorPrincipal::from_digest([0x22; 32]);
        let identity = IdentitySeed::from_digest([0x33; 32]);
        let commitment = KeyMaterialCommitment::from_digest([0x44; 32]);
        let payload = fork_grant_payload_digest(
            grant_id,
            predecessor,
            &fork_point,
            &successor,
            &identity,
            &commitment,
        );
        let consent = ConsentSigningKey::from_seed([0xF2; 32]);
        let mut table = PredecessorConsentTable::new();
        table
            .insert(predecessor, *consent.verifying_key())
            .expect("register predecessor consent key");
        let sig = consent.sign_consent(predecessor, &payload);
        let proof = PredecessorConsentProof::verify(&table, predecessor, &payload, &sig)
            .expect("consent");
        let grant = ForkGrant::new(
            grant_id,
            predecessor,
            fork_point,
            successor,
            identity,
            commitment,
            proof,
        )
        .expect("grant");
        let matured = materialize(&Grant::Fork(grant), None, None, Some(&table))
            .expect("valid consent must mint");
        assert_ne!(matured.store_id(), predecessor);
    }

    /// Test-only predecessor consent signing key.
    struct ConsentSigningKey {
        signing: SigningKey,
        verifying: [u8; 32],
    }

    impl ConsentSigningKey {
        fn from_seed(seed: [u8; 32]) -> Self {
            let signing = SigningKey::from_bytes(&seed);
            let verifying = signing.verifying_key().to_bytes();
            Self { signing, verifying }
        }

        fn verifying_key(&self) -> &[u8; 32] {
            &self.verifying
        }

        fn sign_consent(&self, predecessor_store: StoreId, payload_digest: &[u8; 32]) -> [u8; 64] {
            let mut message =
                Vec::with_capacity(FORK_CONSENT_DOMAIN.len() + 32 + 32);
            message.extend_from_slice(FORK_CONSENT_DOMAIN);
            message.extend_from_slice(predecessor_store.as_bytes());
            message.extend_from_slice(payload_digest);
            self.signing.sign(message.as_slice()).to_bytes()
        }
    }
}
