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
//! [`AncestorReadGrant`], [`AncestorEntitlementProof`],
//! [`AncestorEntitlementTable`], [`RecoveryQuorumProof`],
//! [`PredecessorConsentProof`], [`PredecessorConsentTable`],
//! [`PriorRecoveryTable`], [`MaterializeRefuse::QuorumEquivocationPoison`].
//!
//! Bans: discovery-time identity entropy outside the grant seed; grant-time
//! kill of the original token; post-grant shared-confidentiality Rotate;
//! in-place WriteAuthority reissue for the same epoch; signatureless recovery
//! mint of [`WriteAuthority`]; signatureless fork mint of [`WriteAuthority`];
//! caller-supplied consent verifying keys as the trust root (self-issued consent);
//! a second RecoveryGrant lineage for one predecessor epoch (quorum equivocation);
//! signatureless / caller-supplied entitlement for [`AncestorReadGrant`]
//! (self-issued decrypt-scope forge); home-rolled N-of-M ed25519 quorum
//! counting in place of FROST (`frost-ed25519` / RFC 9591).

use std::collections::BTreeMap;

use ed25519_dalek::{Signature as Ed25519Signature, VerifyingKey};
use sha2::{Digest as ShaDigest, Sha256};

use super::authority::{RecoveryMatrix, WriteAuthority, WriteTokenId};
use super::crypto::{Digest, Signature};
use super::epoch::{CryptoDomain, FenceEpoch};
use super::open::StoreId;
use super::transcript::{
    CanonicalTranscript, TranscriptRefuse, encode_ancestor_entitlement_key_id,
    encode_ancestor_read_grant_payload, encode_fork_consent_key_id, encode_fork_grant_payload,
    encode_fork_store_id, encode_fork_write_token, encode_recovery_grant_payload,
    encode_recovery_matrix, encode_recovery_write_token,
};

/// Domain-separated prefix for recovery quorum signatures.
const RECOVERY_QUORUM_DOMAIN: &[u8] = b"kyzo.recovery_quorum.v1";
/// Domain-separated prefix for predecessor fork-consent signatures.
const FORK_CONSENT_DOMAIN: &[u8] = b"kyzo.fork_consent.v1";
/// Domain-separated prefix for ancestor-entitlement signatures.
const ANCESTOR_ENTITLEMENT_DOMAIN: &[u8] = b"kyzo.ancestor_entitlement.v1";

/// Stable grant identity bound into the signed payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GrantId([u8; 32]);

impl GrantId {
    /// Wrap an already-proven grant id.
    pub fn admit(bytes: [u8; 32]) -> Self {
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
///
/// [`Self::insert`] is operator/genesis-scoped: untrusted or peer input must
/// never reach it unvalidated. Seal-once per StoreId — first key wins;
/// same key re-register is idempotent; a different key refuses
/// [`MaterializeRefuse::TrustRootAlreadySealed`] (rotation is a separate
/// explicit verb, not insert).
#[derive(Debug, Clone)]
pub struct PredecessorConsentTable {
    /// store_id bytes → ed25519 verifying key bytes (32).
    keys: BTreeMap<[u8; 32], [u8; 32]>,
}

impl PredecessorConsentTable {
    /// Empty sealed consent-key registry.
    pub fn new() -> Self {
        Self {
            keys: BTreeMap::new(),
        }
    }

    /// Register the consent verifying key for a StoreId (operator/genesis door).
    ///
    /// Invalid ed25519 material refuses. Seal-once per StoreId: first key
    /// wins; same key → idempotent Ok; different key →
    /// [`MaterializeRefuse::TrustRootAlreadySealed`] (never silent overwrite).
    /// Untrusted/peer input must not reach this door unvalidated.
    pub fn insert(
        &mut self,
        store_id: StoreId,
        verifying_key: [u8; 32],
    ) -> Result<(), MaterializeRefuse> {
        VerifyingKey::from_bytes(&verifying_key)
            .map_err(|_| MaterializeRefuse::ConsentUnverified)?;
        match self.keys.get(store_id.as_bytes()) {
            None => {
                self.keys.insert(*store_id.as_bytes(), verifying_key);
                Ok(())
            }
            Some(existing) if existing == &verifying_key => Ok(()),
            Some(_) => Err(MaterializeRefuse::TrustRootAlreadySealed { store_id }),
        }
    }

    /// Lookup the sealed consent verifying key for `store_id`, if registered.
    pub fn get(&self, store_id: StoreId) -> Option<&[u8; 32]> {
        self.keys.get(store_id.as_bytes())
    }
}

/// Opaque sealed evidence that a predecessor consented to a fork payload.
///
/// No public free constructor — mint only via [`PredecessorConsentProof::verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredecessorConsentProof {
    predecessor_store: StoreId,
    payload_digest: Digest,
    key_id_digest: Digest,
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
        payload_digest: &Digest,
        signature: &Signature,
    ) -> Result<Self, MaterializeRefuse> {
        let Some(consent_verifying_key) = consent_table.get(predecessor_store) else {
            return Err(MaterializeRefuse::ConsentKeyUnknown);
        };
        let Ok(verifying) = VerifyingKey::from_bytes(consent_verifying_key) else {
            return Err(MaterializeRefuse::ConsentUnverified);
        };
        let Ok(sig) = Ed25519Signature::try_from(signature.as_bytes().as_slice()) else {
            return Err(MaterializeRefuse::ConsentUnverified);
        };
        let mut message = Vec::with_capacity(FORK_CONSENT_DOMAIN.len() + 32 + 32);
        message.extend_from_slice(FORK_CONSENT_DOMAIN);
        message.extend_from_slice(predecessor_store.as_bytes());
        message.extend_from_slice(payload_digest.as_bytes());
        if verifying.verify_strict(message.as_slice(), &sig).is_err() {
            return Err(MaterializeRefuse::ConsentUnverified);
        }
        Ok(Self {
            predecessor_store,
            payload_digest: *payload_digest,
            key_id_digest: consent_key_id_digest(consent_verifying_key)?,
            _priv: (),
        })
    }

    /// Predecessor StoreId sealed into this proof.
    pub fn predecessor_store(&self) -> StoreId {
        self.predecessor_store
    }

    /// Payload digest the predecessor consented to.
    pub fn payload_digest(&self) -> &Digest {
        &self.payload_digest
    }

    /// Digest of the consent verifying key sealed into this proof.
    pub fn key_id_digest(&self) -> &Digest {
        &self.key_id_digest
    }
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
        )?;
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

/// Opaque sealed evidence that a [`RecoveryMatrix`] FROST quorum signed a payload.
///
/// Carries only matrix + payload digests — **no signer identifiers**. The
/// signer subset is not recoverable from sealed proof bytes (FROST aggregate
/// is one group signature under the sealed group verifying key).
///
/// No public free constructor — mint only via [`RecoveryQuorumProof::verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryQuorumProof {
    matrix_digest: Digest,
    payload_digest: Digest,
    _priv: (),
}

impl RecoveryQuorumProof {
    /// Verify one FROST (RFC 9591 / `frost-ed25519`) aggregate signature against
    /// `matrix` and seal the proof.
    ///
    /// Message: `kyzo.recovery_quorum.v1 || payload_digest`. Verification uses
    /// the matrix's sealed FROST group verifying key — never an enumerated
    /// N-of-M ed25519 count. Below-threshold or wrong-share aggregates fail
    /// FROST verify and refuse; the sealed proof never records which custodians
    /// participated.
    pub fn verify(
        matrix: &RecoveryMatrix,
        payload_digest: &Digest,
        aggregate_signature: &[u8],
    ) -> Result<Self, MaterializeRefuse> {
        let Ok(verifying) = frost_ed25519::VerifyingKey::deserialize(
            matrix.group_verifying_key().as_bytes().as_slice(),
        ) else {
            return Err(MaterializeRefuse::QuorumUnverified);
        };
        let Ok(signature) = frost_ed25519::Signature::deserialize(aggregate_signature) else {
            return Err(MaterializeRefuse::QuorumInsufficient {
                have: 0,
                need: matrix.threshold(),
            });
        };

        let mut message = Vec::with_capacity(RECOVERY_QUORUM_DOMAIN.len() + 32);
        message.extend_from_slice(RECOVERY_QUORUM_DOMAIN);
        message.extend_from_slice(payload_digest.as_bytes());

        if verifying.verify(message.as_slice(), &signature).is_err() {
            // Below-threshold / wrong-share / forged aggregate: FROST verify
            // refuses. Do not distinguish which custodians failed — that would
            // reintroduce signer-subset leakage into the refuse surface.
            return Err(MaterializeRefuse::QuorumInsufficient {
                have: 0,
                need: matrix.threshold(),
            });
        }

        Ok(Self {
            matrix_digest: recovery_matrix_digest(matrix)?,
            payload_digest: *payload_digest,
            _priv: (),
        })
    }

    /// Matrix digest sealed into this proof.
    pub fn matrix_digest(&self) -> &Digest {
        &self.matrix_digest
    }

    /// Payload digest this quorum signed.
    pub fn payload_digest(&self) -> &Digest {
        &self.payload_digest
    }
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
        )?;
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
    /// Recovery quorum aggregate / matrix binding failed verification.
    #[error("QuorumUnverified: recovery quorum proof does not bind the store RecoveryMatrix")]
    #[diagnostic(code(store::grants::quorum_unverified))]
    QuorumUnverified,
    /// FROST aggregate failed verify (below-threshold, wrong-share, or forged).
    ///
    /// `have` is not a recovered signer count — sealed proofs carry no signer
    /// subset. Hosts may treat `have == 0` as "aggregate refused".
    #[error(
        "QuorumInsufficient: FROST recovery aggregate refused (need threshold {need}; have={have})"
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
    #[error("ConsentKeyUnknown: no sealed predecessor-consent verifying key for the named StoreId")]
    #[diagnostic(code(store::grants::consent_key_unknown))]
    ConsentKeyUnknown,
    /// StoreId already has a sealed trust root; a different key cannot overwrite it.
    ///
    /// First registration wins. Same key re-register is idempotent Ok on
    /// [`PredecessorConsentTable::insert`]. Rotation is a separate explicit
    /// verb — never silent overwrite via insert.
    #[error(
        "TrustRootAlreadySealed: StoreId {store_id:?} already has a sealed predecessor-consent trust root"
    )]
    #[diagnostic(code(store::grants::trust_root_already_sealed))]
    TrustRootAlreadySealed { store_id: StoreId },
    /// Second distinct RecoveryGrant for one predecessor epoch — quorum equivocation poison.
    ///
    /// Never a second WriteAuthority / second lineage. Distinct from
    /// [`GrantAlreadyMaterialized`] (same-grant rediscovery mismatch) and from
    /// quorum verification / threshold refuses.
    #[error("{0}")]
    #[diagnostic(code(store::grants::quorum_equivocation_poison))]
    QuorumEquivocationPoison(Box<QuorumEquivocationPoisonBody>),
    /// Canonical transcript encode failed for a typed grant/consent field.
    #[error(transparent)]
    #[diagnostic(transparent)]
    Transcript(#[from] TranscriptRefuse),
}

/// Payload for [`MaterializeRefuse::QuorumEquivocationPoison`], boxed so the
/// refuse enum stays Result-sized (clippy::result_large_err).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuorumEquivocationPoisonBody {
    pub store_id: StoreId,
    pub predecessor_epoch: FenceEpoch,
    pub first_grant: GrantId,
    pub second_grant: GrantId,
}

impl std::fmt::Display for QuorumEquivocationPoisonBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "QuorumEquivocationPoison: second RecoveryGrant {:?} for predecessor epoch after {:?} on store {:?}",
            self.second_grant, self.first_grant, self.store_id
        )
    }
}

impl MaterializeRefuse {
    /// Construct [`Self::QuorumEquivocationPoison`] with a boxed body.
    pub fn quorum_equivocation_poison(
        store_id: StoreId,
        predecessor_epoch: FenceEpoch,
        first_grant: GrantId,
        second_grant: GrantId,
    ) -> Self {
        Self::QuorumEquivocationPoison(Box::new(QuorumEquivocationPoisonBody {
            store_id,
            predecessor_epoch,
            first_grant,
            second_grant,
        }))
    }
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

/// Host-side one-shot ledger: which RecoveryGrant already materialized for each
/// predecessor [`FenceEpoch`] (binds [`StoreId`]).
///
/// Pure [`materialize`] consults this evidence; a second distinct RecoveryGrant
/// naming the same predecessor epoch refuses
/// [`MaterializeRefuse::QuorumEquivocationPoison`] — never a second lineage.
#[derive(Debug, Clone)]
pub struct PriorRecoveryTable {
    /// predecessor FenceEpoch → GrantId that took the one-shot.
    shots: BTreeMap<FenceEpoch, GrantId>,
}

impl PriorRecoveryTable {
    /// Empty one-shot ledger (no recovery yet observed).
    pub fn new() -> Self {
        Self {
            shots: BTreeMap::new(),
        }
    }

    /// Record that recovery already materialized for `predecessor_epoch`.
    ///
    /// Requires a [`MaterializedGrant`] witness — proof recovery actually
    /// materialized. A bare fabricated [`GrantId`] cannot occupy the one-shot
    /// slot (closes pre-squat grief via [`MaterializeRefuse::QuorumEquivocationPoison`]).
    ///
    /// Idempotent for the same grant. A distinct grant for the same predecessor
    /// epoch refuses [`MaterializeRefuse::QuorumEquivocationPoison`].
    pub fn record(
        &mut self,
        materialized: &MaterializedGrant,
        predecessor_epoch: FenceEpoch,
    ) -> Result<(), MaterializeRefuse> {
        let store_id = materialized.store_id();
        let grant_id = materialized.grant_id();
        assert_eq!(
            predecessor_epoch.store_id(),
            store_id,
            "INVARIANT(PriorRecoveryTable): predecessor FenceEpoch must bind StoreId"
        );
        // Witness must be recovery materialization for this predecessor epoch
        // (successor CryptoDomain epoch), not a ForkGrant or other grant shape.
        let expected_successor = predecessor_epoch
            .successor()
            .map_err(|_| MaterializeRefuse::EpochSpaceExhausted)?;
        assert_eq!(
            materialized.crypto_domain().fence_epoch(),
            expected_successor,
            "INVARIANT(PriorRecoveryTable): MaterializedGrant must be recovery for predecessor epoch"
        );
        match self.shots.get(&predecessor_epoch) {
            None => {
                self.shots.insert(predecessor_epoch, grant_id);
                Ok(())
            }
            Some(&first) if first == grant_id => Ok(()),
            Some(&first) => Err(MaterializeRefuse::quorum_equivocation_poison(
                store_id,
                predecessor_epoch,
                first,
                grant_id,
            )),
        }
    }

    /// GrantId that already took the recovery one-shot for this predecessor epoch, if any.
    pub fn shot_for(&self, store_id: StoreId, predecessor_epoch: FenceEpoch) -> Option<GrantId> {
        if predecessor_epoch.store_id() != store_id {
            return None;
        }
        self.shots.get(&predecessor_epoch).copied()
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
/// A second distinct RecoveryGrant for one predecessor epoch named in
/// `prior_recovery` refuses [`MaterializeRefuse::QuorumEquivocationPoison`].
pub fn materialize(
    grant: &Grant,
    prior: Option<PriorMaterialization>,
    recovery_matrix: Option<&RecoveryMatrix>,
    consent_table: Option<&PredecessorConsentTable>,
    prior_recovery: Option<&PriorRecoveryTable>,
) -> Result<MaterializedGrant, MaterializeRefuse> {
    let computed = match grant {
        Grant::Fork(fork) => materialize_fork(fork, consent_table)?,
        Grant::Recovery(recovery) => {
            materialize_recovery(recovery, recovery_matrix, prior_recovery)?
        }
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
    )?;
    if fork.consent_proof().payload_digest() != &expected {
        return Err(MaterializeRefuse::ConsentUnverified);
    }
    if fork.consent_proof().key_id_digest() != &consent_key_id_digest(sealed_key)? {
        return Err(MaterializeRefuse::ConsentUnverified);
    }
    let store_id = derive_fork_store_id(fork)?;
    let fence_epoch = FenceEpoch::genesis(store_id);
    let crypto_domain = CryptoDomain::new(store_id, fence_epoch);
    let token_id = derive_fork_write_token(fork, store_id)?;
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
    prior_recovery: Option<&PriorRecoveryTable>,
) -> Result<MaterializedGrant, MaterializeRefuse> {
    let matrix = recovery_matrix.ok_or(MaterializeRefuse::RecoveryMatrixAbsent)?;
    // Sealed proof is trusted only when its matrix_digest matches THIS store matrix.
    if recovery.quorum_proof().matrix_digest() != &recovery_matrix_digest(matrix)? {
        return Err(MaterializeRefuse::QuorumUnverified);
    }
    // One-shot quorum: a second distinct RecoveryGrant for this predecessor
    // epoch is equivocation poison — never a second WriteAuthority lineage.
    if let Some(table) = prior_recovery
        && let Some(first_grant) = table.shot_for(recovery.store_id(), recovery.predecessor_epoch())
        && first_grant != recovery.grant_id()
    {
        return Err(MaterializeRefuse::quorum_equivocation_poison(
            recovery.store_id(),
            recovery.predecessor_epoch(),
            first_grant,
            recovery.grant_id(),
        ));
    }
    // Same StoreId; new CryptoDomain at successor epoch; new WriteAuthority.
    let store_id = recovery.store_id();
    let next_epoch = recovery
        .predecessor_epoch()
        .successor()
        .map_err(|_| MaterializeRefuse::EpochSpaceExhausted)?;
    let crypto_domain = CryptoDomain::new(store_id, next_epoch);
    let token_id = derive_recovery_write_token(recovery)?;
    let write_authority = WriteAuthority::mint(store_id, token_id);
    Ok(MaterializedGrant {
        grant_id: recovery.grant_id(),
        store_id,
        crypto_domain,
        write_authority,
        key_material_commitment: *recovery.key_material_commitment(),
    })
}

/// Sealed registry of ancestor-decrypt entitlement verifying keys keyed by [`StoreId`].
///
/// Register at genesis/seal time. [`AncestorEntitlementProof::verify`] and
/// [`AncestorReadGrant::new`] resolve the trust root by StoreId lookup — never a
/// caller-supplied verifying key (self-issued decrypt-scope is Unconstructible).
///
/// [`Self::insert`] is operator/genesis-scoped: untrusted or peer input must
/// never reach it unvalidated. Seal-once per StoreId — first key wins;
/// same key re-register is idempotent; a different key refuses
/// [`AncestorReadRefuse::TrustRootAlreadySealed`] (rotation is a separate
/// explicit verb, not insert).
#[derive(Debug, Clone)]
pub struct AncestorEntitlementTable {
    /// store_id bytes → ed25519 verifying key bytes (32).
    keys: BTreeMap<[u8; 32], [u8; 32]>,
}

impl AncestorEntitlementTable {
    /// Empty sealed entitlement-key registry.
    pub fn new() -> Self {
        Self {
            keys: BTreeMap::new(),
        }
    }

    /// Register the entitlement verifying key for a StoreId (operator/genesis door).
    ///
    /// Invalid ed25519 material refuses. Seal-once per StoreId: first key
    /// wins; same key → idempotent Ok; different key →
    /// [`AncestorReadRefuse::TrustRootAlreadySealed`] (never silent overwrite).
    /// Untrusted/peer input must not reach this door unvalidated.
    pub fn insert(
        &mut self,
        store_id: StoreId,
        verifying_key: [u8; 32],
    ) -> Result<(), AncestorReadRefuse> {
        VerifyingKey::from_bytes(&verifying_key)
            .map_err(|_| AncestorReadRefuse::EntitlementUnverified)?;
        match self.keys.get(store_id.as_bytes()) {
            None => {
                self.keys.insert(*store_id.as_bytes(), verifying_key);
                Ok(())
            }
            Some(existing) if existing == &verifying_key => Ok(()),
            Some(_) => Err(AncestorReadRefuse::TrustRootAlreadySealed { store_id }),
        }
    }

    /// Lookup the sealed entitlement verifying key for `store_id`, if registered.
    pub fn get(&self, store_id: StoreId) -> Option<&[u8; 32]> {
        self.keys.get(store_id.as_bytes())
    }
}

/// Opaque sealed evidence that a sealed entitlement key authorized ancestor decrypt-scope.
///
/// No public free constructor — mint only via [`AncestorEntitlementProof::verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AncestorEntitlementProof {
    store_id: StoreId,
    payload_digest: Digest,
    key_id_digest: Digest,
    _priv: (),
}

impl AncestorEntitlementProof {
    /// Verify an ed25519 entitlement signature against the sealed table and seal the proof.
    ///
    /// Resolves the verifying key from `entitlement_table` for `store_id` — never
    /// accepts a caller-supplied verifying key as the trust root.
    /// Message: `kyzo.ancestor_entitlement.v1 || store_id || payload_digest`.
    pub fn verify(
        entitlement_table: &AncestorEntitlementTable,
        store_id: StoreId,
        payload_digest: &Digest,
        signature: &Signature,
    ) -> Result<Self, AncestorReadRefuse> {
        let Some(entitlement_verifying_key) = entitlement_table.get(store_id) else {
            return Err(AncestorReadRefuse::EntitlementKeyUnknown);
        };
        let Ok(verifying) = VerifyingKey::from_bytes(entitlement_verifying_key) else {
            return Err(AncestorReadRefuse::EntitlementUnverified);
        };
        let Ok(sig) = Ed25519Signature::try_from(signature.as_bytes().as_slice()) else {
            return Err(AncestorReadRefuse::EntitlementUnverified);
        };
        let mut message = Vec::with_capacity(ANCESTOR_ENTITLEMENT_DOMAIN.len() + 32 + 32);
        message.extend_from_slice(ANCESTOR_ENTITLEMENT_DOMAIN);
        message.extend_from_slice(store_id.as_bytes());
        message.extend_from_slice(payload_digest.as_bytes());
        if verifying.verify_strict(message.as_slice(), &sig).is_err() {
            return Err(AncestorReadRefuse::EntitlementUnverified);
        }
        Ok(Self {
            store_id,
            payload_digest: *payload_digest,
            key_id_digest: entitlement_key_id_digest(entitlement_verifying_key)?,
            _priv: (),
        })
    }

    /// StoreId sealed into this proof.
    pub fn store_id(&self) -> StoreId {
        self.store_id
    }

    /// Payload digest the entitlement key authorized.
    pub fn payload_digest(&self) -> &Digest {
        &self.payload_digest
    }

    /// Digest of the entitlement verifying key sealed into this proof.
    pub fn key_id_digest(&self) -> &Digest {
        &self.key_id_digest
    }
}

/// Ancestor-epoch plaintext read grant — O(epochs) rewrap.
///
/// Cross-fork foreign-CryptoDomain plaintext is Unconstructible.
/// AuditKey ≠ AncestorReadGrant ≠ decrypt ≠ WriteAuthority.
/// Carries a verified [`AncestorEntitlementProof`] — signatureless mint is Unconstructible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AncestorReadGrant {
    store_id: StoreId,
    /// Inclusive epoch range this grant may rewrap under the holding KEK.
    from_epoch: FenceEpoch,
    to_epoch: FenceEpoch,
    entitlement_proof: AncestorEntitlementProof,
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
    /// No sealed entitlement verifying key is registered for the StoreId.
    #[error(
        "EntitlementKeyUnknown: no sealed ancestor-entitlement verifying key for the named StoreId"
    )]
    #[diagnostic(code(store::grants::entitlement_key_unknown))]
    EntitlementKeyUnknown,
    /// Ancestor entitlement signature failed verification against the sealed key.
    #[error("EntitlementUnverified: ancestor entitlement signature does not verify")]
    #[diagnostic(code(store::grants::entitlement_unverified))]
    EntitlementUnverified,
    /// Entitlement proof does not name the grant's StoreId.
    #[error("EntitlementMismatch: entitlement proof store does not match the ancestor-read grant")]
    #[diagnostic(code(store::grants::entitlement_mismatch))]
    EntitlementMismatch,
    /// StoreId already has a sealed trust root; a different key cannot overwrite it.
    ///
    /// First registration wins. Same key re-register is idempotent Ok on
    /// [`AncestorEntitlementTable::insert`]. Rotation is a separate explicit
    /// verb — never silent overwrite via insert.
    #[error(
        "TrustRootAlreadySealed: StoreId {store_id:?} already has a sealed ancestor-entitlement trust root"
    )]
    #[diagnostic(code(store::grants::entitlement_trust_root_already_sealed))]
    TrustRootAlreadySealed { store_id: StoreId },
    /// Canonical transcript encode failed for a typed ancestor-read field.
    #[error(transparent)]
    #[diagnostic(transparent)]
    Transcript(#[from] TranscriptRefuse),
}

impl AncestorReadGrant {
    /// Seal an O(epochs) rewrap grant bound to verified sealed entitlement evidence.
    ///
    /// Resolves the trust root from `entitlement_table` for `store_id` — never a
    /// caller-supplied verifying key. The proof's store and payload digest must
    /// match this grant's sealed fields; signatureless construction is Unconstructible.
    pub fn new(
        entitlement_table: &AncestorEntitlementTable,
        store_id: StoreId,
        from_epoch: FenceEpoch,
        to_epoch: FenceEpoch,
        entitlement_proof: AncestorEntitlementProof,
    ) -> Result<Self, AncestorReadRefuse> {
        assert_eq!(
            from_epoch.store_id(),
            store_id,
            "INVARIANT(AncestorReadGrant): from_epoch must bind StoreId"
        );
        assert_eq!(
            to_epoch.store_id(),
            store_id,
            "INVARIANT(AncestorReadGrant): to_epoch must bind StoreId"
        );
        if from_epoch > to_epoch {
            return Err(AncestorReadRefuse::InvalidEpochRange);
        }
        if entitlement_proof.store_id() != store_id {
            return Err(AncestorReadRefuse::EntitlementMismatch);
        }
        let expected = ancestor_read_grant_payload_digest(store_id, from_epoch, to_epoch)?;
        if entitlement_proof.payload_digest() != &expected {
            return Err(AncestorReadRefuse::EntitlementUnverified);
        }
        let Some(sealed_key) = entitlement_table.get(store_id) else {
            return Err(AncestorReadRefuse::EntitlementKeyUnknown);
        };
        // Sealed entitlement is trusted only when it still binds the sealed table
        // key for this store (not an attacker-chosen key from a private table).
        if entitlement_proof.key_id_digest() != &entitlement_key_id_digest(sealed_key)? {
            return Err(AncestorReadRefuse::EntitlementUnverified);
        }
        Ok(Self {
            store_id,
            from_epoch,
            to_epoch,
            entitlement_proof,
        })
    }

    /// Store this grant covers.
    pub fn store_id(&self) -> StoreId {
        self.store_id
    }

    /// First covered epoch.
    pub fn covered_from(&self) -> FenceEpoch {
        self.from_epoch
    }

    /// Last covered epoch.
    pub fn to_epoch(&self) -> FenceEpoch {
        self.to_epoch
    }

    /// Verified ancestor-entitlement evidence bound into this grant.
    pub fn entitlement_proof(&self) -> &AncestorEntitlementProof {
        &self.entitlement_proof
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

/// SHA-256 over sealed [`CanonicalTranscript`] bytes — the only digest step on
/// the grant surface. Field layout lives solely in the transcript encoders.
fn hash_transcript(transcript: &CanonicalTranscript) -> Digest {
    let mut h = Sha256::new();
    h.update(transcript.as_bytes());
    Digest::admit(h.finalize().into())
}

/// Fork-consent verifying-key id via [`encode_fork_consent_key_id`].
fn consent_key_id_digest(verifying_key: &[u8; 32]) -> Result<Digest, TranscriptRefuse> {
    let transcript = encode_fork_consent_key_id(verifying_key)?;
    Ok(hash_transcript(&transcript))
}

/// ForkGrant payload digest via [`encode_fork_grant_payload`].
pub(crate) fn fork_grant_payload_digest(
    grant_id: GrantId,
    predecessor_store: StoreId,
    fork_point_root: &ForkPointRoot,
    successor_principal: &SuccessorPrincipal,
    identity_seed: &IdentitySeed,
    key_material_commitment: &KeyMaterialCommitment,
) -> Result<Digest, TranscriptRefuse> {
    let transcript = encode_fork_grant_payload(
        grant_id.as_bytes(),
        predecessor_store.as_bytes(),
        fork_point_root.as_bytes(),
        successor_principal.as_bytes(),
        identity_seed.as_bytes(),
        key_material_commitment.as_bytes(),
    )?;
    Ok(hash_transcript(&transcript))
}

/// RecoveryMatrix digest via [`encode_recovery_matrix`].
fn recovery_matrix_digest(matrix: &RecoveryMatrix) -> Result<Digest, TranscriptRefuse> {
    let transcript = encode_recovery_matrix(
        matrix.threshold(),
        matrix.max_signers(),
        matrix.group_verifying_key().as_bytes(),
    )?;
    Ok(hash_transcript(&transcript))
}

/// RecoveryGrant payload digest via [`encode_recovery_grant_payload`].
pub(crate) fn recovery_grant_payload_digest(
    grant_id: GrantId,
    store_id: StoreId,
    predecessor_epoch: FenceEpoch,
    successor_identity_seed: &IdentitySeed,
    key_material_commitment: &KeyMaterialCommitment,
) -> Result<Digest, TranscriptRefuse> {
    let transcript = encode_recovery_grant_payload(
        grant_id.as_bytes(),
        store_id.as_bytes(),
        predecessor_epoch.get(),
        predecessor_epoch.store_id().as_bytes(),
        successor_identity_seed.as_bytes(),
        key_material_commitment.as_bytes(),
    )?;
    Ok(hash_transcript(&transcript))
}

/// Successor StoreId for a fork — hash of [`encode_fork_store_id`] bytes once.
fn derive_fork_store_id(fork: &ForkGrant) -> Result<StoreId, TranscriptRefuse> {
    let transcript = encode_fork_store_id(
        fork.grant_id().as_bytes(),
        fork.predecessor_store().as_bytes(),
        fork.fork_point_root().as_bytes(),
        fork.successor_principal().as_bytes(),
        fork.identity_seed().as_bytes(),
        fork.key_material_commitment().as_bytes(),
    )?;
    Ok(StoreId::from_digest(
        *hash_transcript(&transcript).as_bytes(),
    ))
}

/// Fork WriteAuthority token — hash of [`encode_fork_write_token`] bytes once.
fn derive_fork_write_token(
    fork: &ForkGrant,
    store_id: StoreId,
) -> Result<WriteTokenId, TranscriptRefuse> {
    let transcript = encode_fork_write_token(
        store_id.as_bytes(),
        fork.grant_id().as_bytes(),
        fork.identity_seed().as_bytes(),
        fork.key_material_commitment().as_bytes(),
    )?;
    Ok(WriteTokenId::from_digest(
        *hash_transcript(&transcript).as_bytes(),
    ))
}

/// Recovery WriteAuthority token — hash of [`encode_recovery_write_token`] bytes once.
fn derive_recovery_write_token(recovery: &RecoveryGrant) -> Result<WriteTokenId, TranscriptRefuse> {
    let pred = recovery.predecessor_epoch();
    let transcript = encode_recovery_write_token(
        recovery.store_id().as_bytes(),
        recovery.grant_id().as_bytes(),
        pred.get(),
        pred.store_id().as_bytes(),
        recovery.successor_identity_seed().as_bytes(),
        recovery.key_material_commitment().as_bytes(),
    )?;
    Ok(WriteTokenId::from_digest(
        *hash_transcript(&transcript).as_bytes(),
    ))
}

/// Ancestor-entitlement verifying-key id via [`encode_ancestor_entitlement_key_id`].
fn entitlement_key_id_digest(verifying_key: &[u8; 32]) -> Result<Digest, TranscriptRefuse> {
    let transcript = encode_ancestor_entitlement_key_id(verifying_key)?;
    Ok(hash_transcript(&transcript))
}

/// AncestorReadGrant payload digest via [`encode_ancestor_read_grant_payload`].
fn ancestor_read_grant_payload_digest(
    store_id: StoreId,
    from_epoch: FenceEpoch,
    to_epoch: FenceEpoch,
) -> Result<Digest, TranscriptRefuse> {
    let transcript = encode_ancestor_read_grant_payload(
        store_id.as_bytes(),
        from_epoch.get(),
        from_epoch.store_id().as_bytes(),
        to_epoch.get(),
        to_epoch.store_id().as_bytes(),
    )?;
    Ok(hash_transcript(&transcript))
}

#[cfg(test)]
use miette::{IntoDiagnostic, Result, miette};

/// Test-only: mint a FROST 2-of-3 recovery matrix + one aggregate signature
/// over the quorum domain via `frost-ed25519` (trusted dealer + threshold sign).
///
/// Real threshold crypto only — never home-rolled share counting.
#[cfg(test)]
pub(crate) fn frost_sign_recovery_quorum(
    dealer_seed: [u8; 32],
    payload_digest: &Digest,
) -> Result<(RecoveryMatrix, Vec<u8>)> {
    frost_recovery_aggregate(dealer_seed, 3, 2, 2, payload_digest)
}

/// Test-only FROST dealer → aggregate over `kyzo.recovery_quorum.v1 || payload`.
///
/// `participating` must be ≥ `min_signers` for a valid aggregate; callers that
/// want a below-threshold attempt should use [`frost_try_aggregate_below_threshold`].
#[cfg(test)]
fn frost_recovery_aggregate(
    dealer_seed: [u8; 32],
    max_signers: u16,
    min_signers: u16,
    participating: u16,
    payload_digest: &Digest,
) -> Result<(RecoveryMatrix, Vec<u8>)> {
    use std::collections::BTreeMap;

    use frost_ed25519 as frost;
    use frost_ed25519::keys::IdentifierList;
    use rand_chacha::ChaCha20Rng;
    use rand_core::SeedableRng;

    let mut rng = ChaCha20Rng::from_seed(dealer_seed);
    let (shares, pubkey_package) = frost::keys::generate_with_dealer(
        max_signers,
        min_signers,
        IdentifierList::Default,
        &mut rng,
    )
    .map_err(|e| miette!("FROST dealer keygen: {e}"))?;

    let mut key_packages = BTreeMap::new();
    for (identifier, secret_share) in shares {
        let key_package = frost::keys::KeyPackage::try_from(secret_share)
            .map_err(|e| miette!("key package: {e}"))?;
        key_packages.insert(identifier, key_package);
    }

    let vk_bytes = pubkey_package
        .verifying_key()
        .serialize()
        .map_err(|e| miette!("serialize group verifying key: {e}"))?;
    assert_eq!(vk_bytes.len(), 32, "FROST Ed25519 group VK is 32 bytes");
    let mut group_vk = [0u8; 32];
    group_vk.copy_from_slice(&vk_bytes);
    let matrix = RecoveryMatrix::new(
        u32::from(min_signers),
        u32::from(max_signers),
        super::authority::RecoveryPublicKey::admit(group_vk),
    )
    .into_diagnostic()?;

    let mut message = Vec::with_capacity(RECOVERY_QUORUM_DOMAIN.len() + 32);
    message.extend_from_slice(RECOVERY_QUORUM_DOMAIN);
    message.extend_from_slice(payload_digest.as_bytes());

    let mut nonces_map = BTreeMap::new();
    let mut commitments_map = BTreeMap::new();
    for participant_index in 1..=participating {
        let participant_identifier = participant_index
            .try_into()
            .map_err(|_| miette!("nonzero id"))?;
        let key_package = &key_packages[&participant_identifier];
        let (nonces, commitments) = frost::round1::commit(key_package.signing_share(), &mut rng);
        nonces_map.insert(participant_identifier, nonces);
        commitments_map.insert(participant_identifier, commitments);
    }

    let signing_package = frost::SigningPackage::new(commitments_map, message.as_slice());
    let mut signature_shares = BTreeMap::new();
    for participant_identifier in nonces_map.keys() {
        let key_package = &key_packages[participant_identifier];
        let nonces = &nonces_map[participant_identifier];
        let signature_share = frost::round2::sign(&signing_package, nonces, key_package)
            .map_err(|e| miette!("FROST round2 share: {e}"))?;
        signature_shares.insert(*participant_identifier, signature_share);
    }

    let group_signature = frost::aggregate(&signing_package, &signature_shares, &pubkey_package)
        .map_err(|e| miette!("FROST aggregate: {e}"))?;
    let aggregate_bytes = group_signature
        .serialize()
        .map_err(|e| miette!("serialize aggregate: {e}"))?;
    Ok((matrix, aggregate_bytes))
}

/// Test-only: attempt FROST aggregate with fewer than min_signers signature shares.
///
/// Round1 commitments cover `min_signers` so the SigningPackage is well-formed
/// and round2 can produce shares; aggregate then receives a strict subset of
/// those shares and must return `Err` (below-threshold refuse — no panic).
#[cfg(test)]
fn frost_try_aggregate_below_threshold(
    dealer_seed: [u8; 32],
    payload_digest: &Digest,
) -> Result<Vec<u8>> {
    use std::collections::BTreeMap;

    use frost_ed25519 as frost;
    use frost_ed25519::keys::IdentifierList;
    use rand_chacha::ChaCha20Rng;
    use rand_core::SeedableRng;

    let max_signers = 3u16;
    let min_signers = 2u16;
    let share_count = 1u16; // below threshold at aggregate

    let mut rng = ChaCha20Rng::from_seed(dealer_seed);
    let (shares, pubkey_package) = frost::keys::generate_with_dealer(
        max_signers,
        min_signers,
        IdentifierList::Default,
        &mut rng,
    )
    .map_err(|e| miette!("FROST dealer keygen: {e}"))?;

    let mut key_packages = BTreeMap::new();
    for (identifier, secret_share) in shares {
        let key_package = frost::keys::KeyPackage::try_from(secret_share)
            .map_err(|e| miette!("key package: {e}"))?;
        key_packages.insert(identifier, key_package);
    }

    let mut message = Vec::with_capacity(RECOVERY_QUORUM_DOMAIN.len() + 32);
    message.extend_from_slice(RECOVERY_QUORUM_DOMAIN);
    message.extend_from_slice(payload_digest.as_bytes());

    // Enough commitments for a valid SigningPackage (min_signers).
    let mut nonces_map = BTreeMap::new();
    let mut commitments_map = BTreeMap::new();
    for participant_index in 1..=min_signers {
        let participant_identifier = participant_index
            .try_into()
            .map_err(|_| miette!("nonzero id"))?;
        let key_package = &key_packages[&participant_identifier];
        let (nonces, commitments) = frost::round1::commit(key_package.signing_share(), &mut rng);
        nonces_map.insert(participant_identifier, nonces);
        commitments_map.insert(participant_identifier, commitments);
    }

    let signing_package = frost::SigningPackage::new(commitments_map, message.as_slice());
    // Produce fewer signature shares than min_signers — refuse lands at aggregate.
    let mut signature_shares = BTreeMap::new();
    for participant_index in 1..=share_count {
        let participant_identifier = participant_index
            .try_into()
            .map_err(|_| miette!("nonzero id"))?;
        let key_package = &key_packages[&participant_identifier];
        let nonces = &nonces_map[&participant_identifier];
        let signature_share = frost::round2::sign(&signing_package, nonces, key_package)
            .map_err(|e| miette!("FROST round2 share with valid package: {e}"))?;
        signature_shares.insert(participant_identifier, signature_share);
    }

    let group_signature = frost::aggregate(&signing_package, &signature_shares, &pubkey_package)
        .map_err(|e| miette!("FROST aggregate below threshold: {e}"))?;
    group_signature
        .serialize()
        .map_err(|e| miette!("serialize: {e}"))
}

/// Test-only: one ed25519 signer scaffold; domain label is the independence.
#[cfg(test)]
pub(crate) fn sign_domain_label(
    domain: &[u8],
    seed: [u8; 32],
    store_id: StoreId,
    payload_digest: &Digest,
) -> ([u8; 32], Signature) {
    use ed25519_dalek::{Signer, SigningKey};

    let signing = SigningKey::from_bytes(&seed);
    let verifying = signing.verifying_key().to_bytes();
    let mut message = Vec::with_capacity(domain.len() + 32 + 32);
    message.extend_from_slice(domain);
    message.extend_from_slice(store_id.as_bytes());
    message.extend_from_slice(payload_digest.as_bytes());
    let sig = Signature::admit(signing.sign(message.as_slice()).to_bytes());
    (verifying, sig)
}

/// Test-only: ed25519 predecessor consent sign over the fork-consent domain.
/// Path-wired dst callers use this so positive fork paths still compile.
#[cfg(test)]
pub(crate) fn sign_fork_consent(
    seed: [u8; 32],
    predecessor_store: StoreId,
    payload_digest: &Digest,
) -> ([u8; 32], Signature) {
    sign_domain_label(FORK_CONSENT_DOMAIN, seed, predecessor_store, payload_digest)
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};
    use miette::{IntoDiagnostic, Result, miette};

    use super::*;
    use crate::store::authority::RecoveryMatrix;

    // ---- Hostile-peer verify gates (#376) ----------------------------------
    // ed25519-dalek: VerifyingKey::from_bytes accepts small-order keys; only
    // verify_strict refuses the identity-R / S=0 forgery. Fork consent and
    // ancestor entitlement doors use verify_strict (a5be07a).
    // FROST recovery: frost-ed25519 Group::deserialize rejects identity and
    // non-torsion-free points, so a dalek-style degenerate group key cannot
    // seal RecoveryMatrix / reach RecoveryQuorumProof::verify — see seat 2
    // (frost verify strict by construction). Forged aggregates against a
    // sealed matrix still refuse at the production verify door.

    /// Invalid / small-order group verifying key cannot seal a RecoveryMatrix;
    /// forged aggregate bytes against a real matrix must refuse.
    #[test]
    fn frost_recovery_refuses_invalid_group_key_and_forged_aggregate() -> Result<()> {
        use crate::store::authority::RecoveryPublicKey;

        let store = StoreId::from_digest([0x71; 32]);
        let payload = recovery_grant_payload_digest(
            GrantId::admit([0x90; 32]),
            store,
            FenceEpoch::genesis(store),
            &IdentitySeed::from_digest([0xEE; 32]),
            &KeyMaterialCommitment::from_digest([0xEF; 32]),
        );
        let mut weak = [0u8; 32];
        weak[0] = 1;
        let weak_pk = RecoveryPublicKey::admit(weak);
        assert!(
            matches!(
                RecoveryMatrix::new(1, 1, weak_pk),
                Err(crate::store::authority::RecoveryMatrixRefuse::InvalidGroupVerifyingKey)
            ),
            "small-order / invalid bytes must not seal as a FROST group verifying key"
        );

        let (matrix, _sig) = frost_sign_recovery_quorum([0xA1; 32], &payload)?;
        let mut forged = vec![0u8; 64];
        forged[0] = 1;
        assert_eq!(
            RecoveryQuorumProof::verify(&matrix, &payload, &forged),
            Err(MaterializeRefuse::QuorumInsufficient {
                have: 0,
                need: matrix.threshold(),
            }),
            "FORGED AGGREGATE: garbage FROST bytes must not mint RecoveryQuorumProof"
        );

        Ok(())
    }

    /// Weak-key forged FORK CONSENT must refuse (verify_strict).
    #[test]
    fn verify_strict_refuses_weak_key_forged_fork_consent() -> Result<()> {
        let store = StoreId::from_digest([0x72; 32]);
        let mut weak = [0u8; 32];
        weak[0] = 1;
        let mut table = PredecessorConsentTable::new();
        table.insert(store, weak)?;
        let mut forged = [0u8; 64];
        forged[0] = 1;
        assert!(
            PredecessorConsentProof::verify(
                &table,
                store,
                &Digest::admit([0x33; 32]),
                &Signature::admit(forged),
            )
            .is_err(),
            "FORGED CONSENT: small-order consent key + weak-key forgery must refuse \
             at PredecessorConsentProof::verify (verify_strict)"
        );

        Ok(())
    }

    /// Weak-key forged ANCESTOR ENTITLEMENT must refuse (verify_strict).
    #[test]
    fn verify_strict_refuses_weak_key_forged_ancestor_entitlement() -> Result<()> {
        let store = StoreId::from_digest([0x73; 32]);
        let mut weak = [0u8; 32];
        weak[0] = 1;
        let mut table = AncestorEntitlementTable::new();
        table.insert(store, weak)?;
        let mut forged = [0u8; 64];
        forged[0] = 1;
        assert!(
            AncestorEntitlementProof::verify(
                &table,
                store,
                &Digest::admit([0x33; 32]),
                &Signature::admit(forged),
            )
            .is_err(),
            "FORGED ENTITLEMENT: small-order entitlement key + weak-key forgery must refuse \
             at AncestorEntitlementProof::verify (verify_strict)"
        );

        Ok(())
    }
    // -----------------------------------------------------------------------

    /// Nasty: victim StoreId+epoch grant with empty/forged/wrong-matrix FROST
    /// aggregate must not mint WriteAuthority.
    #[test]
    fn recovery_grant_without_valid_quorum_refuses_write_authority() -> Result<()> {
        let victim = StoreId::from_digest([0x71; 32]);
        let pred_epoch = FenceEpoch::genesis(victim);
        let grant_id = GrantId::admit([0x90; 32]);
        let successor_seed = IdentitySeed::from_digest([0xEE; 32]);
        let commitment = KeyMaterialCommitment::from_digest([0xEF; 32]);
        let payload = recovery_grant_payload_digest(
            grant_id,
            victim,
            pred_epoch,
            &successor_seed,
            &commitment,
        );

        let (store_matrix, _store_sig) = frost_sign_recovery_quorum([0xA1; 32], &payload)?;

        // Empty aggregate — no proof, no grant power.
        assert_eq!(
            RecoveryQuorumProof::verify(&store_matrix, &payload, &[]),
            Err(MaterializeRefuse::QuorumInsufficient { have: 0, need: 2 })
        );

        // Forged aggregate bytes — still refuse.
        assert_eq!(
            RecoveryQuorumProof::verify(&store_matrix, &payload, &[0xFFu8; 64]),
            Err(MaterializeRefuse::QuorumInsufficient { have: 0, need: 2 })
        );

        // Below-threshold share set cannot form a FROST aggregate (production
        // frost::aggregate door) — wrong-share / short quorum is refuse.
        assert!(
            frost_try_aggregate_below_threshold([0xA1; 32], &payload).is_err(),
            "below-threshold FROST aggregate must refuse at frost-ed25519 aggregate"
        );

        // Attacker mints a real FROST aggregate against a *different* matrix,
        // names the victim, then materializes against the victim store matrix.
        let (attacker_matrix, attacker_sig) = frost_sign_recovery_quorum([0xB1; 32], &payload)?;
        let forged_proof = RecoveryQuorumProof::verify(&attacker_matrix, &payload, &attacker_sig)?;
        let grant = RecoveryGrant::new(
            grant_id,
            victim,
            pred_epoch,
            successor_seed,
            commitment,
            forged_proof,
        )?;

        // Absent store matrix → refuse (no WriteAuthority).
        let absent = materialize(&Grant::Recovery(grant.clone()), None, None, None, None);
        assert_eq!(absent, Err(MaterializeRefuse::RecoveryMatrixAbsent));

        // Wrong store matrix → QuorumUnverified (no WriteAuthority).
        let wrong = materialize(
            &Grant::Recovery(grant),
            None,
            Some(&store_matrix),
            None,
            None,
        );
        assert_eq!(wrong, Err(MaterializeRefuse::QuorumUnverified));

        Ok(())
    }

    #[test]
    fn recovery_grant_valid_quorum_materializes() -> Result<()> {
        let store_id = StoreId::from_digest([0xC0; 32]);
        let pred_epoch = FenceEpoch::genesis(store_id);
        let grant_id = GrantId::admit([0x91; 32]);
        let successor_seed = IdentitySeed::from_digest([0xD1; 32]);
        let commitment = KeyMaterialCommitment::from_digest([0xD2; 32]);
        let payload = recovery_grant_payload_digest(
            grant_id,
            store_id,
            pred_epoch,
            &successor_seed,
            &commitment,
        );
        let (matrix, aggregate) = frost_sign_recovery_quorum([0xC1; 32], &payload)?;
        let proof = RecoveryQuorumProof::verify(&matrix, &payload, &aggregate)?;
        // Sealed proof carries only digests — signer subset is not recoverable
        // from proof bytes (no custodian id / share index field exists).
        assert_eq!(proof.payload_digest(), &payload);
        assert_eq!(proof.matrix_digest(), &recovery_matrix_digest(&matrix));
        let proof_dbg = format!("{proof:?}");
        assert!(
            !proof_dbg.contains("signer")
                && !proof_dbg.contains("participant")
                && !proof_dbg.contains("share"),
            "sealed RecoveryQuorumProof debug must not surface signer-subset fields: {proof_dbg}"
        );
        let grant = RecoveryGrant::new(
            grant_id,
            store_id,
            pred_epoch,
            successor_seed,
            commitment,
            proof,
        )?;
        let matured = materialize(&Grant::Recovery(grant), None, Some(&matrix), None, None)?;
        assert_eq!(matured.store_id(), store_id);
        assert_ne!(matured.crypto_domain().fence_epoch(), pred_epoch);
        assert_eq!(matured.write_authority().store_id(), store_id);
        assert_eq!(matured.key_material_commitment(), &commitment);

        Ok(())
    }

    /// Nasty: aggregate under group A must refuse verify against group B
    /// (wrong-share / wrong group key) — production verify door.
    #[test]
    fn frost_recovery_wrong_group_aggregate_refuses() -> Result<()> {
        let payload = Digest::admit([0x44u8; 32]);
        let (matrix_a, sig_a) = frost_sign_recovery_quorum([0x11; 32], &payload)?;
        let (matrix_b, _sig_b) = frost_sign_recovery_quorum([0x22; 32], &payload)?;
        assert_ne!(
            matrix_a.group_verifying_key(),
            matrix_b.group_verifying_key(),
            "control: distinct dealer seeds yield distinct group keys"
        );
        assert_eq!(
            RecoveryQuorumProof::verify(&matrix_b, &payload, &sig_a),
            Err(MaterializeRefuse::QuorumInsufficient {
                have: 0,
                need: matrix_b.threshold(),
            }),
            "wrong-group FROST aggregate must refuse at RecoveryQuorumProof::verify"
        );

        Ok(())
    }

    /// Nasty: second distinct RecoveryGrant for one predecessor epoch is poison.
    #[test]
    fn recovery_grant_second_for_same_predecessor_epoch_is_equivocation_poison() -> Result<()> {
        let store_id = StoreId::from_digest([0xE0; 32]);
        let pred_epoch = FenceEpoch::genesis(store_id);

        // One sealed matrix for the store; each grant gets its own FROST aggregate
        // over its payload under that same group key (re-deal with fixed seed so
        // the group verifying key matches).
        let probe_payload = Digest::admit([0u8; 32]);
        let (matrix, _) = frost_sign_recovery_quorum([0xE1; 32], &probe_payload)?;

        let mint = |grant_id: GrantId, seed: [u8; 32], commit: [u8; 32]| {
            let successor_seed = IdentitySeed::from_digest(seed);
            let commitment = KeyMaterialCommitment::from_digest(commit);
            let payload = recovery_grant_payload_digest(
                grant_id,
                store_id,
                pred_epoch,
                &successor_seed,
                &commitment,
            );
            // Same dealer seed → same group VK as `matrix`.
            let (signed_matrix, aggregate) = frost_sign_recovery_quorum([0xE1; 32], &payload)?;
            assert_eq!(
                signed_matrix.group_verifying_key(),
                matrix.group_verifying_key()
            );
            let proof = RecoveryQuorumProof::verify(&matrix, &payload, &aggregate)?;
            RecoveryGrant::new(
                grant_id,
                store_id,
                pred_epoch,
                successor_seed,
                commitment,
                proof,
            )?
        };

        let g1 = mint(GrantId::admit([0x01; 32]), [0xA1; 32], [0xA2; 32]);
        let g2 = mint(GrantId::admit([0x02; 32]), [0xB1; 32], [0xB2; 32]);

        let first = materialize(
            &Grant::Recovery(g1.clone()),
            None,
            Some(&matrix),
            None,
            None,
        )?;
        assert_eq!(first.store_id(), store_id);

        let mut prior = PriorRecoveryTable::new();
        prior.record(&first, pred_epoch)?;

        // Same grant rediscovery with prior shot → still ok (idempotent).
        let again = materialize(
            &Grant::Recovery(g1.clone()),
            None,
            Some(&matrix),
            None,
            Some(&prior),
        )?;
        assert_eq!(again.grant_id(), g1.grant_id());

        // Distinct second grant → QuorumEquivocationPoison, never a second lineage.
        assert_eq!(
            materialize(
                &Grant::Recovery(g2.clone()),
                None,
                Some(&matrix),
                None,
                Some(&prior),
            ),
            Err(MaterializeRefuse::quorum_equivocation_poison(
                store_id,
                pred_epoch,
                g1.grant_id(),
                g2.grant_id(),
            ))
        );

        Ok(())
    }

    /// Nasty: fabricated GrantId cannot pre-squat the predecessor-epoch one-shot
    /// and grief legitimate quorum-materialized recovery via QuorumEquivocationPoison.
    ///
    /// `PriorRecoveryTable::record` is witness-bound ([`MaterializedGrant`]); a
    /// bare fabricated id has no path into the ledger, so the legitimate recovery
    /// still materializes.
    #[test]
    fn prior_recovery_fabricated_grant_id_cannot_presquat_legitimate_recovery() -> Result<()> {
        let store_id = StoreId::from_digest([0xF0; 32]);
        let pred_epoch = FenceEpoch::genesis(store_id);
        let probe_payload = Digest::admit([0u8; 32]);
        let (matrix, _) = frost_sign_recovery_quorum([0xF1; 32], &probe_payload)?;

        let mint = |grant_id: GrantId, seed: [u8; 32], commit: [u8; 32]| {
            let successor_seed = IdentitySeed::from_digest(seed);
            let commitment = KeyMaterialCommitment::from_digest(commit);
            let payload = recovery_grant_payload_digest(
                grant_id,
                store_id,
                pred_epoch,
                &successor_seed,
                &commitment,
            );
            let (signed_matrix, aggregate) = frost_sign_recovery_quorum([0xF1; 32], &payload)?;
            assert_eq!(
                signed_matrix.group_verifying_key(),
                matrix.group_verifying_key()
            );
            let proof = RecoveryQuorumProof::verify(&matrix, &payload, &aggregate)?;
            RecoveryGrant::new(
                grant_id,
                store_id,
                pred_epoch,
                successor_seed,
                commitment,
                proof,
            )?
        };

        let legit = mint(GrantId::admit([0x11; 32]), [0xA1; 32], [0xA2; 32]);

        // Attack closed at the type boundary: record takes &MaterializedGrant, not
        // a bare GrantId (e.g. GrantId::admit([0xDE; 32])). Without a real
        // materialization witness the one-shot is empty — fabricated pre-squat
        // cannot poison the slot.
        let mut prior = PriorRecoveryTable::new();
        assert!(
            prior.shot_for(store_id, pred_epoch).is_none(),
            "fabricated grant_id must not occupy the one-shot without materialization"
        );

        let matured = materialize(
            &Grant::Recovery(legit.clone()),
            None,
            Some(&matrix),
            None,
            Some(&prior),
        )?;
        assert_eq!(matured.grant_id(), legit.grant_id());
        assert_eq!(matured.store_id(), store_id);

        prior.record(&matured, pred_epoch)?;
        assert_eq!(
            prior.shot_for(store_id, pred_epoch),
            Some(legit.grant_id()),
            "one-shot must bind the legitimate materialized grant, not a fabricated id"
        );

        Ok(())
    }

    /// Nasty: ForkGrant naming a real predecessor without valid consent must not mint.
    #[test]
    fn fork_grant_without_valid_consent_refuses_write_authority() -> Result<()> {
        let victim = StoreId::from_digest([0x72; 32]);
        let grant_id = GrantId::admit([0xA0; 32]);
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
        table.insert(victim, *consent.verifying_key())?;

        // Forged signature bytes — no proof, no grant power.
        assert_eq!(
            PredecessorConsentProof::verify(
                &table,
                victim,
                &payload,
                &Signature::admit([0xFFu8; 64]),
            ),
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
        table.insert(other, *consent.verifying_key())?;
        let other_sig = consent.sign_consent(other, &other_payload);
        let wrong_store_proof =
            PredecessorConsentProof::verify(&table, other, &other_payload, &other_sig)?;
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
            GrantId::admit([0xA1; 32]),
            victim,
            &fork_point,
            &successor,
            &identity,
            &commitment,
        );
        let forged_sig = consent.sign_consent(victim, &forged_lineage_payload);
        let forged_lineage_proof =
            PredecessorConsentProof::verify(&table, victim, &forged_lineage_payload, &forged_sig)?;
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

        Ok(())
    }

    /// Nasty: consent signed by an attacker key NOT bound in the sealed table
    /// for predecessor_store must refuse — closes self-issued consent forge.
    #[test]
    fn fork_grant_attacker_own_key_not_bound_in_table_refuses() -> Result<()> {
        let victim = StoreId::from_digest([0x75; 32]);
        let grant_id = GrantId::admit([0xA3; 32]);
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
        sealed_table.insert(victim, *legitimate.verifying_key())?;

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
        attacker_table.insert(victim, *attacker.verifying_key())?;
        let forged_proof =
            PredecessorConsentProof::verify(&attacker_table, victim, &payload, &attacker_sig)?;
        let grant = ForkGrant::new(
            grant_id,
            victim,
            fork_point,
            successor,
            identity,
            commitment,
            forged_proof,
        )?;

        assert_eq!(
            materialize(&Grant::Fork(grant.clone()), None, None, None, None),
            Err(MaterializeRefuse::ConsentKeyUnknown)
        );
        assert_eq!(
            materialize(&Grant::Fork(grant), None, None, Some(&sealed_table), None),
            Err(MaterializeRefuse::ConsentUnverified)
        );

        Ok(())
    }

    #[test]
    fn fork_grant_valid_consent_materializes() -> Result<()> {
        let predecessor = StoreId::from_digest([0x74; 32]);
        let grant_id = GrantId::admit([0xA2; 32]);
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
        table.insert(predecessor, *consent.verifying_key())?;
        let sig = consent.sign_consent(predecessor, &payload);
        let proof = PredecessorConsentProof::verify(&table, predecessor, &payload, &sig)?;
        let grant = ForkGrant::new(
            grant_id,
            predecessor,
            fork_point,
            successor,
            identity,
            commitment,
            proof,
        )?;
        let matured = materialize(&Grant::Fork(grant), None, None, Some(&table), None)?;
        assert_ne!(matured.store_id(), predecessor);
        assert_eq!(matured.write_authority().store_id(), matured.store_id());
        assert_eq!(matured.key_material_commitment(), &commitment);

        Ok(())
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

        fn sign_consent(&self, predecessor_store: StoreId, payload_digest: &Digest) -> Signature {
            let mut message = Vec::with_capacity(FORK_CONSENT_DOMAIN.len() + 32 + 32);
            message.extend_from_slice(FORK_CONSENT_DOMAIN);
            message.extend_from_slice(predecessor_store.as_bytes());
            message.extend_from_slice(payload_digest.as_bytes());
            Signature::admit(self.signing.sign(message.as_slice()).to_bytes())
        }
    }

    /// Test-only ancestor-entitlement signing key.
    struct EntitlementSigningKey {
        signing: SigningKey,
        verifying: [u8; 32],
    }

    impl EntitlementSigningKey {
        fn from_seed(seed: [u8; 32]) -> Self {
            let signing = SigningKey::from_bytes(&seed);
            let verifying = signing.verifying_key().to_bytes();
            Self { signing, verifying }
        }

        fn verifying_key(&self) -> &[u8; 32] {
            &self.verifying
        }

        fn sign_entitlement(&self, store_id: StoreId, payload_digest: &Digest) -> Signature {
            let mut message = Vec::with_capacity(ANCESTOR_ENTITLEMENT_DOMAIN.len() + 32 + 32);
            message.extend_from_slice(ANCESTOR_ENTITLEMENT_DOMAIN);
            message.extend_from_slice(store_id.as_bytes());
            message.extend_from_slice(payload_digest.as_bytes());
            Signature::admit(self.signing.sign(message.as_slice()).to_bytes())
        }
    }

    #[test]
    fn ancestor_read_grant_registered_entitlement_succeeds() -> Result<()> {
        let store_id = StoreId::from_digest([0x81; 32]);
        let from_epoch = FenceEpoch::genesis(store_id);
        let to_epoch = from_epoch.successor()?;
        let payload = ancestor_read_grant_payload_digest(store_id, from_epoch, to_epoch);

        let entitlement = EntitlementSigningKey::from_seed([0xE1; 32]);
        let mut table = AncestorEntitlementTable::new();
        table.insert(store_id, *entitlement.verifying_key())?;

        let sig = entitlement.sign_entitlement(store_id, &payload);
        let proof = AncestorEntitlementProof::verify(&table, store_id, &payload, &sig)?;
        let grant = AncestorReadGrant::new(&table, store_id, from_epoch, to_epoch, proof)?;

        assert_eq!(grant.store_id(), store_id);
        assert_eq!(grant.covered_from(), from_epoch);
        assert_eq!(grant.to_epoch(), to_epoch);
        assert_eq!(grant.entitlement_proof().store_id(), store_id);
        assert_eq!(grant.entitlement_proof().payload_digest(), &payload);

        let domain = CryptoDomain::new(store_id, from_epoch);
        grant.authorize(domain)?;

        Ok(())
    }

    /// Nasty: entitlement signed by an attacker key NOT bound in the sealed
    /// registry for store_id must refuse — closes self-issued decrypt-scope forge.
    #[test]
    fn ancestor_read_grant_attacker_own_key_not_bound_in_registry_refuses() -> Result<()> {
        let victim = StoreId::from_digest([0x82; 32]);
        let from_epoch = FenceEpoch::genesis(victim);
        let to_epoch = from_epoch.successor()?;
        let payload = ancestor_read_grant_payload_digest(victim, from_epoch, to_epoch);

        let legitimate = EntitlementSigningKey::from_seed([0xE2; 32]);
        let attacker = EntitlementSigningKey::from_seed([0xE3; 32]);

        // Sealed store authority: only the legitimate key is registered.
        let mut sealed_table = AncestorEntitlementTable::new();
        sealed_table.insert(victim, *legitimate.verifying_key())?;

        // Attacker signs with their own keypair — must not verify against sealed table.
        let attacker_sig = attacker.sign_entitlement(victim, &payload);
        assert_eq!(
            AncestorEntitlementProof::verify(&sealed_table, victim, &payload, &attacker_sig),
            Err(AncestorReadRefuse::EntitlementUnverified)
        );

        // Unknown store (no sealed key) → EntitlementKeyUnknown, even with a
        // cryptographically valid signature under some key.
        let empty = AncestorEntitlementTable::new();
        assert_eq!(
            AncestorEntitlementProof::verify(&empty, victim, &payload, &attacker_sig),
            Err(AncestorReadRefuse::EntitlementKeyUnknown)
        );

        // Cross-stream: attacker seals a proof against a *private* table that
        // binds their own key to the victim StoreId, then constructs against
        // the real sealed store table — key_id mismatch must refuse.
        let mut attacker_table = AncestorEntitlementTable::new();
        attacker_table.insert(victim, *attacker.verifying_key())?;
        let forged_proof =
            AncestorEntitlementProof::verify(&attacker_table, victim, &payload, &attacker_sig)?;
        assert_eq!(
            AncestorReadGrant::new(&sealed_table, victim, from_epoch, to_epoch, forged_proof),
            Err(AncestorReadRefuse::EntitlementUnverified)
        );

        // Forged signature bytes under the registered key — still refuse.
        assert_eq!(
            AncestorEntitlementProof::verify(
                &sealed_table,
                victim,
                &payload,
                &Signature::admit([0xFFu8; 64]),
            ),
            Err(AncestorReadRefuse::EntitlementUnverified)
        );

        Ok(())
    }

    // ---- #375 T3 — seal-once registration doors (operator/genesis) ----------

    /// NASTY (#375 T3): register key A for victim StoreId, then attacker key
    /// B(!=A) for the same StoreId on the production
    /// [`PredecessorConsentTable::insert`] door → typed refuse (never silent
    /// overwrite of the sealed consent trust root).
    #[test]
    fn predecessor_consent_registration_attacker_key_refuses_overwrite() -> Result<()> {
        let victim = StoreId::from_digest([0x91; 32]);
        let key_a = ConsentSigningKey::from_seed([0xA1; 32]);
        let key_b = ConsentSigningKey::from_seed([0xB2; 32]);
        assert_ne!(
            key_a.verifying_key(),
            key_b.verifying_key(),
            "control: attacker key must differ from sealed key A"
        );

        let mut table = PredecessorConsentTable::new();
        table.insert(victim, *key_a.verifying_key())?;
        assert_eq!(table.get(victim), Some(key_a.verifying_key()));

        // Same key re-register → idempotent Ok (seal-once, not one-shot grief).
        table.insert(victim, *key_a.verifying_key())?;
        assert_eq!(table.get(victim), Some(key_a.verifying_key()));

        // Attacker key B for the already-sealed StoreId → TrustRootAlreadySealed.
        assert_eq!(
            table.insert(victim, *key_b.verifying_key()),
            Err(MaterializeRefuse::TrustRootAlreadySealed { store_id: victim })
        );
        assert_eq!(
            table.get(victim),
            Some(key_a.verifying_key()),
            "sealed key A must survive the refused overwrite attempt"
        );

        Ok(())
    }

    /// NASTY (#375 T3): register key A for victim StoreId, then attacker key
    /// B(!=A) for the same StoreId on the production
    /// [`AncestorEntitlementTable::insert`] door → typed refuse (never silent
    /// overwrite of the sealed entitlement trust root).
    #[test]
    fn ancestor_entitlement_registration_attacker_key_refuses_overwrite() -> Result<()> {
        let victim = StoreId::from_digest([0x92; 32]);
        let key_a = EntitlementSigningKey::from_seed([0xC1; 32]);
        let key_b = EntitlementSigningKey::from_seed([0xD2; 32]);
        assert_ne!(
            key_a.verifying_key(),
            key_b.verifying_key(),
            "control: attacker key must differ from sealed key A"
        );

        let mut table = AncestorEntitlementTable::new();
        table.insert(victim, *key_a.verifying_key())?;
        assert_eq!(table.get(victim), Some(key_a.verifying_key()));

        table.insert(victim, *key_a.verifying_key())?;
        assert_eq!(table.get(victim), Some(key_a.verifying_key()));

        assert_eq!(
            table.insert(victim, *key_b.verifying_key()),
            Err(AncestorReadRefuse::TrustRootAlreadySealed { store_id: victim })
        );
        assert_eq!(
            table.get(victim),
            Some(key_a.verifying_key()),
            "sealed key A must survive the refused overwrite attempt"
        );

        Ok(())
    }
}
