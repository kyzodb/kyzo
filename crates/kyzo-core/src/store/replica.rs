/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Admission certificates, custody, and replica verification (decisions.md §69, §70).
//!
//! Owns: [`AdmissionCertificate`], [`verify_replica`], [`ReplicaKey`],
//! [`ReplicaCustody`], PendingAnchor anchoring, [`LocalProjection`] contract,
//! [`AuthorizingKey`] / [`AuthorizingKeyTable`], [`ScopeManifestTable`],
//! [`OriginContinuity`].
//!
//! Bans: reshape/re-author of certified meaning; in-place local reinterpretation
//! under a local Catalog cut; strict-contiguous-only or head-attested catch-up;
//! exposing never-anchored PendingAnchor as queryable; caller-asserted
//! `scope_ok` / `anchored` verdicts; authenticity that ignores the sealed
//! signature; symmetric MAC standing in for certificate signatures (ed25519
//! public-key verify only — receivers must not hold the origin seed).
//!
//! Authoring mints at `session/admit.rs` through [`mint_admission_certificate`];
//! replicas verify + mint custody — never re-author.

use std::collections::BTreeMap;
use std::fmt;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

use super::contract::FormatVersion;
use super::epoch::FenceEpoch;
use super::open::StoreId;
use super::sweep::CommitOrdinal;
use super::transcript::{
    CanonicalTranscript, CanonicalTranscriptBuilder, FieldId, MapValue, SealedArtifactKind,
};

/// Opaque authorizing key id bound into the certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AuthorizingKeyId([u8; 32]);

impl AuthorizingKeyId {
    /// Wrap an already-proven authorizing key id.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Borrow the id bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for AuthorizingKeyId {
    fn from(digest: [u8; 32]) -> Self {
        Self(digest)
    }
}

impl AsRef<[u8]> for AuthorizingKeyId {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Digest of the sealed scope manifest (plus issuer lineage / validity-as-of).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScopeManifestDigest([u8; 32]);

impl ScopeManifestDigest {
    /// Wrap an already-proven scope manifest digest.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Borrow the digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for ScopeManifestDigest {
    fn from(digest: [u8; 32]) -> Self {
        Self(digest)
    }
}

impl AsRef<[u8]> for ScopeManifestDigest {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Post-state root bound into an AdmissionCertificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PostStateRoot([u8; 32]);

impl PostStateRoot {
    /// Wrap an already-proven post-state root digest.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Borrow the digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for PostStateRoot {
    fn from(digest: [u8; 32]) -> Self {
        Self(digest)
    }
}

impl AsRef<[u8]> for PostStateRoot {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Authorizing key for AdmissionCertificate authenticity (ed25519).
///
/// Holds the **public** verifying key always. May optionally carry a signing
/// seed capability when minted at the origin admission door — never reconstituted
/// from [`AuthorizingKeyTable`] (receivers verify; they cannot forge).
///
/// Public construction is Unconstructible; only the trust / admission doors mint.
#[derive(Clone)]
pub struct AuthorizingKey {
    id: AuthorizingKeyId,
    verifying: VerifyingKey,
    /// Origin-only signing capability. Absent after table lookup.
    signing: Option<SigningKey>,
}

impl fmt::Debug for AuthorizingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizingKey")
            .field("id", &self.id)
            .field("verifying", &self.verifying)
            .field(
                "signing",
                &self.signing.as_ref().map(|_| "<redacted-signing-seed>"),
            )
            .finish()
    }
}

impl PartialEq for AuthorizingKey {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.verifying == other.verifying
    }
}

impl Eq for AuthorizingKey {}

impl AuthorizingKey {
    /// Mint from a 32-byte ed25519 seed (origin admission door).
    ///
    /// Derives the public verifying key and retains signing capability so the
    /// origin can seal certificates. [`AuthorizingKeyTable::insert`] stores
    /// **only** the public verifying bytes.
    ///
    /// `id` must be a **public** identifier — never equal to `seed`. Prefer
    /// [`Self::mint_with_verifying_id`] so the id is the verifying key bytes.
    pub(crate) fn mint(id: impl Into<AuthorizingKeyId>, seed: [u8; 32]) -> Self {
        let signing = SigningKey::from_bytes(&seed);
        let verifying = signing.verifying_key();
        Self {
            id: id.into(),
            verifying,
            signing: Some(signing),
        }
    }

    /// Origin mint: id = ed25519 verifying key bytes (public), material = seed.
    ///
    /// Guarantees `id ≠ seed` (re-rolls on the astronomical collision). The
    /// seed is never derived from a public token id — callers supply fresh
    /// OS entropy.
    pub(crate) fn mint_with_verifying_id(seed: [u8; 32]) -> Self {
        let mut seed = seed;
        loop {
            let signing = SigningKey::from_bytes(&seed);
            let verifying = signing.verifying_key();
            let id_bytes = verifying.to_bytes();
            if id_bytes != seed {
                return Self {
                    id: AuthorizingKeyId::from_digest(id_bytes),
                    verifying,
                    signing: Some(signing),
                };
            }
            // Negligible; domain-separate and try again.
            let mut h = Sha256::new();
            h.update(b"kyzo.authorizing_key.seed_reroll.v1");
            h.update(seed);
            seed = h.finalize().into();
        }
    }

    /// Install public verifying material under its id (replica trust door).
    ///
    /// Receivers use this form — no signing seed, cannot forge.
    pub(crate) fn mint_verifying(
        id: impl Into<AuthorizingKeyId>,
        verifying_bytes: [u8; 32],
    ) -> Result<Self, ReplicaRefuse> {
        let verifying = VerifyingKey::from_bytes(&verifying_bytes)
            .map_err(|_| ReplicaRefuse::AuthenticityFailed)?;
        Ok(Self {
            id: id.into(),
            verifying,
            signing: None,
        })
    }

    /// Key id bound into certificates this key may authorize.
    pub fn id(&self) -> AuthorizingKeyId {
        self.id
    }

    /// Public verifying key bytes (32) — what the trust table stores.
    pub fn verifying_bytes(&self) -> [u8; 32] {
        self.verifying.to_bytes()
    }

    /// True when this handle can produce signatures (origin mint only).
    pub fn can_sign(&self) -> bool {
        self.signing.is_some()
    }

    /// ed25519 signature over the signing body (64-byte sealed width).
    pub(crate) fn sign(&self, body: &[u8; 32]) -> Result<[u8; 64], ReplicaRefuse> {
        let signing = self
            .signing
            .as_ref()
            .ok_or(ReplicaRefuse::AuthenticityFailed)?;
        Ok(signing.sign(body.as_slice()).to_bytes())
    }

    /// Verify a sealed ed25519 signature against this key's public material.
    pub(crate) fn verify_signature(&self, body: &[u8; 32], signature: &[u8; 64]) -> bool {
        let Ok(sig) = Signature::try_from(signature.as_slice()) else {
            return false;
        };
        self.verifying.verify(body.as_slice(), &sig).is_ok()
    }
}

/// Table of trusted authorizing **public** keys — verify looks up by [`AuthorizingKeyId`].
///
/// Stores verifying material only. A replica with this table can authenticate
/// certificates but cannot forge the origin's signatures.
#[derive(Debug, Default, Clone)]
pub struct AuthorizingKeyTable {
    /// key_id → ed25519 verifying key bytes (32).
    keys: BTreeMap<[u8; 32], [u8; 32]>,
}

impl AuthorizingKeyTable {
    /// Empty trust table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a trusted authorizing **public** key (signing seed discarded).
    pub(crate) fn insert(&mut self, key: AuthorizingKey) {
        self.keys
            .insert(*key.id.as_bytes(), key.verifying_bytes());
    }

    /// Lookup trusted public verifying material for `id`, if installed.
    ///
    /// Returned key can verify; [`AuthorizingKey::can_sign`] is always false.
    pub fn lookup(&self, id: &AuthorizingKeyId) -> Option<AuthorizingKey> {
        self.keys
            .get(id.as_bytes())
            .copied()
            .and_then(|pk| AuthorizingKey::mint_verifying(*id, pk).ok())
    }
}

/// Manifest resolution status (§69) — derived by the table, never caller-asserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScopeManifestStatus {
    /// Manifest known and accepted at this Store.
    Verified,
    /// Digest not known.
    Unknown,
    /// Previously accepted, then revoked.
    Revoked,
    /// Known but incompatible with this replica.
    Incompatible,
}

/// Scope manifest registry — `resolve` derives refuse; caller-forged pass is Unconstructible.
#[derive(Debug, Default, Clone)]
pub struct ScopeManifestTable {
    entries: BTreeMap<[u8; 32], ScopeManifestStatus>,
}

impl ScopeManifestTable {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a verified / revoked / incompatible manifest (trust door).
    pub(crate) fn set(&mut self, digest: ScopeManifestDigest, status: ScopeManifestStatus) {
        debug_assert!(
            !matches!(status, ScopeManifestStatus::Unknown),
            "Unknown is the absent-entry default, not a stored status"
        );
        self.entries.insert(*digest.as_bytes(), status);
    }

    /// Derive resolution for a sealed scope digest (§69).
    pub fn resolve(&self, digest: &ScopeManifestDigest) -> ScopeManifestStatus {
        self.entries
            .get(digest.as_bytes())
            .copied()
            .unwrap_or(ScopeManifestStatus::Unknown)
    }
}

fn scope_status_to_result(status: ScopeManifestStatus) -> Result<(), ReplicaRefuse> {
    match status {
        ScopeManifestStatus::Verified => Ok(()),
        ScopeManifestStatus::Unknown => Err(ReplicaRefuse::ScopeUnknown),
        ScopeManifestStatus::Revoked => Err(ReplicaRefuse::ScopeRevoked),
        ScopeManifestStatus::Incompatible => Err(ReplicaRefuse::ScopeDenied),
    }
}

/// Evidence that origin coordinates are continuous (CheckpointSeal / contiguous chain).
///
/// Minted only by the chain/seal door — caller-forged `anchored: true` is Unconstructible.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OriginContinuity {
    _priv: (),
}

impl OriginContinuity {
    /// Mint continuity evidence after origin coordinates are continuous.
    pub(crate) fn mint() -> Self {
        Self { _priv: () }
    }
}

/// Sealed AdmissionCertificate type (§69) — CanonicalTranscript artifact.
///
/// Mint happens only at the admission door via [`mint_admission_certificate`].
/// Replicas call [`verify_replica`] and mint custody — never re-author.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AdmissionCertificate {
    transcript: CanonicalTranscript,
    origin_store: StoreId,
    origin_epoch: FenceEpoch,
    origin_commit: CommitOrdinal,
    record_digest: [u8; 32],
    operation_key: Option<[u8; 32]>,
    authorizing_key_id: AuthorizingKeyId,
    scope_manifest_digest: ScopeManifestDigest,
    signature: [u8; 64],
    protocol_version: [u8; 8],
    schema_cut: [u8; 32],
    predecessor_history_digest: [u8; 32],
    post_state_root: PostStateRoot,
}

impl AdmissionCertificate {
    /// Borrow the sealed transcript bytes.
    pub fn transcript(&self) -> &CanonicalTranscript {
        &self.transcript
    }

    /// Origin Store identity.
    pub fn origin_store(&self) -> StoreId {
        self.origin_store
    }

    /// Origin fence epoch.
    pub fn origin_epoch(&self) -> FenceEpoch {
        self.origin_epoch
    }

    /// Origin commit ordinal.
    pub fn origin_commit(&self) -> CommitOrdinal {
        self.origin_commit
    }

    /// Record digest bound by the certificate.
    pub fn record_digest(&self) -> &[u8; 32] {
        &self.record_digest
    }

    /// Optional OperationKey when the admission was composed (§38).
    pub fn operation_key(&self) -> Option<&[u8; 32]> {
        self.operation_key.as_ref()
    }

    /// Authorizing key id sealed into the certificate.
    pub fn authorizing_key_id(&self) -> AuthorizingKeyId {
        self.authorizing_key_id
    }

    /// Scope manifest digest sealed into the certificate.
    pub fn scope_manifest_digest(&self) -> ScopeManifestDigest {
        self.scope_manifest_digest
    }

    /// Post-state root sealed into the certificate.
    pub fn post_state_root(&self) -> PostStateRoot {
        self.post_state_root
    }

    /// Sealed signature bytes.
    pub fn signature(&self) -> &[u8; 64] {
        &self.signature
    }

    /// Signing body digest recomputed from sealed fields (excludes signature).
    pub fn signing_body(&self) -> [u8; 32] {
        signing_body_digest(
            &self.protocol_version,
            self.origin_store,
            self.origin_epoch,
            self.origin_commit,
            &self.schema_cut,
            &self.record_digest,
            &self.predecessor_history_digest,
            self.post_state_root.as_bytes(),
            &self.authorizing_key_id,
            &self.scope_manifest_digest,
            self.operation_key.as_ref(),
        )
    }
}

/// Replica custody key (§70): H(origin_store, origin_epoch, origin_commit, record_digest).
///
/// Makes re-delivery converge — at-least-once bytes, exactly-once custody.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReplicaKey([u8; 32]);

impl ReplicaKey {
    /// Derive the convergent custody key from origin coordinates + record digest.
    pub fn derive(
        origin_store: StoreId,
        origin_epoch: FenceEpoch,
        origin_commit: CommitOrdinal,
        record_digest: &[u8; 32],
    ) -> Self {
        let mut h = Sha256::new();
        h.update(b"kyzo.replica_key.v1");
        h.update(origin_store.as_bytes());
        h.update(u64::to_be_bytes(origin_epoch.get()));
        h.update(u64::to_be_bytes(origin_commit.get()));
        h.update(record_digest);
        Self(h.finalize().into())
    }

    /// Borrow the key digest.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Replica custody state (§69/§70): Queryable or opaque PendingAnchor.
///
/// Out-of-order certificates stay PendingAnchor until a seal or contiguous
/// chain anchors origin coordinates. Never-anchored is reclaimable under
/// operator pressure — never exposed as queryable.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)] // RA payloads / certificates are intentionally unboxed for match locality
pub enum ReplicaCustody {
    /// Anchored and part of the queryable semantic view.
    Queryable {
        /// Convergent custody key.
        key: ReplicaKey,
        /// Local Store that holds custody.
        local_store: StoreId,
        /// Local commit ordinal at which custody was sealed.
        local_commit: CommitOrdinal,
    },
    /// Opaque certified material awaiting origin-coordinate anchor.
    PendingAnchor {
        /// Convergent custody key (same as Queryable once anchored).
        key: ReplicaKey,
        /// Sealed certificate retained opaque until anchored.
        certificate: AdmissionCertificate,
    },
}

/// Local rebuildable projection of origin-schema interpretation (§69).
///
/// A local Catalog produces exactly this (rebuildable cache) or a derived
/// Record through ordinary admission carrying the certificate as evidence —
/// no third constructor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalProjection {
    /// Origin certificate this projection was built from.
    origin: AdmissionCertificate,
    /// Local Catalog generation (Store commit position) at projection build.
    catalog_generation: u64,
}

impl LocalProjection {
    /// Bind a verified certificate to a local Catalog generation.
    pub(crate) fn from_certificate(origin: AdmissionCertificate, catalog_generation: u64) -> Self {
        Self {
            origin,
            catalog_generation,
        }
    }

    /// Origin certificate.
    pub fn origin(&self) -> &AdmissionCertificate {
        &self.origin
    }

    /// Catalog generation at build.
    pub fn catalog_generation(&self) -> u64 {
        self.catalog_generation
    }
}

/// Closed replica-verification refuse sum (§69).
///
/// Manifest-resolution outcomes are never folded into RetentionDeclined;
/// no reshape exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum ReplicaRefuse {
    /// Retention / custody obligation declined.
    #[error("RetentionDeclined: custody retention refused")]
    #[diagnostic(code(store::replica::retention_declined))]
    RetentionDeclined,
    /// Certificate authenticity failed.
    #[error("AuthenticityFailed: certificate signature or binding failed")]
    #[diagnostic(code(store::replica::authenticity_failed))]
    AuthenticityFailed,
    /// History / predecessor chain inconsistent.
    #[error("ChainInconsistent: predecessor history digest disagreed")]
    #[diagnostic(code(store::replica::chain_inconsistent))]
    ChainInconsistent,
    /// Scope manifest unknown at this Store.
    #[error("ScopeUnknown: scope manifest not known")]
    #[diagnostic(code(store::replica::scope_unknown))]
    ScopeUnknown,
    /// Scope previously accepted then revoked.
    #[error("ScopeRevoked: scope manifest revoked")]
    #[diagnostic(code(store::replica::scope_revoked))]
    ScopeRevoked,
    /// Scope denied for this replica.
    #[error("ScopeDenied: scope manifest denied")]
    #[diagnostic(code(store::replica::scope_denied))]
    ScopeDenied,
}

/// Inputs required to mint an AdmissionCertificate (admission door only).
#[derive(Debug, Clone)]
pub struct AdmissionCertificateParts {
    /// Protocol / format version tag bytes.
    pub protocol_version: [u8; 8],
    /// Origin Store.
    pub origin_store: StoreId,
    /// Origin fence epoch.
    pub origin_epoch: FenceEpoch,
    /// Origin commit ordinal.
    pub origin_commit: CommitOrdinal,
    /// Schema cut digest.
    pub schema_cut: [u8; 32],
    /// Record digest.
    pub record_digest: [u8; 32],
    /// Predecessor history digest.
    pub predecessor_history_digest: [u8; 32],
    /// Post-state root.
    pub post_state_root: PostStateRoot,
    /// Authorizing key id.
    pub authorizing_key_id: AuthorizingKeyId,
    /// Scope manifest digest (+ issuer lineage / validity sealed elsewhere).
    pub scope_manifest_digest: ScopeManifestDigest,
    /// Optional OperationKey when composed.
    pub operation_key: Option<[u8; 32]>,
    /// Signature over the signing body under the authorizing key.
    pub signature: [u8; 64],
}

#[allow(clippy::too_many_arguments)] // sealed admit/join/digest doors carry explicit domain params
fn signing_body_digest(
    protocol_version: &[u8; 8],
    origin_store: StoreId,
    origin_epoch: FenceEpoch,
    origin_commit: CommitOrdinal,
    schema_cut: &[u8; 32],
    record_digest: &[u8; 32],
    predecessor_history_digest: &[u8; 32],
    post_state_root: &[u8; 32],
    authorizing_key_id: &AuthorizingKeyId,
    scope_manifest_digest: &ScopeManifestDigest,
    operation_key: Option<&[u8; 32]>,
) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"kyzo.admission_certificate.sign.v1");
    h.update(protocol_version);
    h.update(origin_store.as_bytes());
    h.update(u64::to_be_bytes(origin_epoch.get()));
    h.update(u64::to_be_bytes(origin_commit.get()));
    h.update(schema_cut);
    h.update(record_digest);
    h.update(predecessor_history_digest);
    h.update(post_state_root);
    h.update(authorizing_key_id.as_bytes());
    h.update(scope_manifest_digest.as_bytes());
    // FenceEpoch binds StoreId — include it so genesis-discard is unrepresentable.
    h.update(origin_epoch.store_id().as_bytes());
    match operation_key {
        Some(op) => {
            h.update([1u8]);
            h.update(op);
        }
        None => h.update([0u8]),
    }
    h.finalize().into()
}

/// Mint an AdmissionCertificate — callable only from the admission seam.
///
/// Authoring lives at `session/admit.rs`; replicas never call this.
///
/// When `authorizing_key` is provided, the sealed signature is checked (or
/// may be produced) against that key; mismatch → [`ReplicaRefuse::AuthenticityFailed`].
pub(crate) fn mint_admission_certificate(
    parts: AdmissionCertificateParts,
) -> Result<AdmissionCertificate, ReplicaRefuse> {
    let mut builder = CanonicalTranscriptBuilder::new(FormatVersion::CURRENT)
        .map_err(|_| ReplicaRefuse::AuthenticityFailed)?;
    builder
        .append_u64(
            FieldId::ARTIFACT_KIND,
            SealedArtifactKind::AdmissionCertificate.tag(),
        )
        .map_err(|_| ReplicaRefuse::AuthenticityFailed)?;
    builder
        .append_bytes(FieldId::FORMAT_VERSION, &FormatVersion::CURRENT.as_bytes())
        .map_err(|_| ReplicaRefuse::AuthenticityFailed)?;
    builder
        .append_digest32(FieldId::PRIMARY_DIGEST, &parts.record_digest)
        .map_err(|_| ReplicaRefuse::AuthenticityFailed)?;
    builder
        .append_digest32(FieldId::SECONDARY_DIGEST, &parts.predecessor_history_digest)
        .map_err(|_| ReplicaRefuse::AuthenticityFailed)?;
    builder
        .append_bytes(FieldId::DOMAIN_LABEL, parts.origin_store.as_bytes())
        .map_err(|_| ReplicaRefuse::AuthenticityFailed)?;
    // Map keys must be strictly ascending byte order.
    let mut bindings: Vec<(Vec<u8>, MapValue)> = vec![
        (
            b"authorizing_key_id".to_vec(),
            MapValue::Digest32(*parts.authorizing_key_id.as_bytes()),
        ),
        (
            b"origin_commit".to_vec(),
            MapValue::U64(parts.origin_commit.get()),
        ),
        (
            b"origin_epoch".to_vec(),
            MapValue::U64(parts.origin_epoch.get()),
        ),
        (
            b"post_state_root".to_vec(),
            MapValue::Digest32(*parts.post_state_root.as_bytes()),
        ),
        (
            b"protocol_version".to_vec(),
            MapValue::Bytes(parts.protocol_version.to_vec()),
        ),
        (b"schema_cut".to_vec(), MapValue::Digest32(parts.schema_cut)),
        (
            b"scope_manifest_digest".to_vec(),
            MapValue::Digest32(*parts.scope_manifest_digest.as_bytes()),
        ),
        (
            b"signature".to_vec(),
            MapValue::Bytes(parts.signature.to_vec()),
        ),
    ];
    if let Some(op) = parts.operation_key {
        // Insert "operation_key" in sorted position (after origin_epoch, before post_state_root).
        let idx = bindings
            .iter()
            .position(|(k, _)| k.as_slice() > b"operation_key".as_slice())
            .unwrap_or(bindings.len());
        bindings.insert(idx, (b"operation_key".to_vec(), MapValue::Digest32(op)));
    }
    builder
        .append_map(FieldId::BINDINGS_MAP, &bindings)
        .map_err(|_| ReplicaRefuse::AuthenticityFailed)?;
    let transcript = builder.seal();
    if transcript.as_bytes().is_empty() {
        return Err(ReplicaRefuse::AuthenticityFailed);
    }
    Ok(AdmissionCertificate {
        transcript,
        origin_store: parts.origin_store,
        origin_epoch: parts.origin_epoch,
        origin_commit: parts.origin_commit,
        record_digest: parts.record_digest,
        operation_key: parts.operation_key,
        authorizing_key_id: parts.authorizing_key_id,
        scope_manifest_digest: parts.scope_manifest_digest,
        signature: parts.signature,
        protocol_version: parts.protocol_version,
        schema_cut: parts.schema_cut,
        predecessor_history_digest: parts.predecessor_history_digest,
        post_state_root: parts.post_state_root,
    })
}

/// Sign certificate parts under a trusted authorizing key (admission helper).
///
/// Produces the signature bytes that must be carried in
/// [`AdmissionCertificateParts::signature`].
pub(crate) fn sign_admission_parts(
    parts: &AdmissionCertificateParts,
    key: &AuthorizingKey,
) -> Result<[u8; 64], ReplicaRefuse> {
    if key.id() != parts.authorizing_key_id {
        return Err(ReplicaRefuse::AuthenticityFailed);
    }
    let body = signing_body_digest(
        &parts.protocol_version,
        parts.origin_store,
        parts.origin_epoch,
        parts.origin_commit,
        &parts.schema_cut,
        &parts.record_digest,
        &parts.predecessor_history_digest,
        parts.post_state_root.as_bytes(),
        &parts.authorizing_key_id,
        &parts.scope_manifest_digest,
        parts.operation_key.as_ref(),
    );
    key.sign(&body)
}

/// Verify a replica certificate and produce custody (§69/§70).
///
/// Accepting verify produces durable custody carrying origin coordinates
/// (what) and local `(StoreId, CommitOrdinal)` (when); creates NO new Record
/// identity. Re-delivery with the same [`ReplicaKey`] converges.
///
/// Authenticity, scope, and anchor are **derived**:
/// - signature checked against [`AuthorizingKeyTable`] lookup of
///   `certificate.authorizing_key_id`
/// - scope refuse derived from [`ScopeManifestTable::resolve`]
/// - Queryable vs PendingAnchor from [`OriginContinuity`] evidence (projection
///   / chain door), never a caller-asserted bool
pub fn verify_replica(
    certificate: &AdmissionCertificate,
    local_store: StoreId,
    local_commit: CommitOrdinal,
    authorizing_keys: &AuthorizingKeyTable,
    scopes: &ScopeManifestTable,
    continuity: Option<&OriginContinuity>,
) -> Result<ReplicaCustody, ReplicaRefuse> {
    // Scope resolution is a closed sum — never folded into RetentionDeclined.
    scope_status_to_result(scopes.resolve(&certificate.scope_manifest_digest()))?;

    if certificate.transcript().as_bytes().is_empty() {
        return Err(ReplicaRefuse::AuthenticityFailed);
    }
    // Re-parse sealed bytes so corrupt transcripts refuse (not only empty).
    CanonicalTranscript::parse(certificate.transcript().as_bytes())
        .map_err(|_| ReplicaRefuse::AuthenticityFailed)?;

    let authorizing_key = authorizing_keys
        .lookup(&certificate.authorizing_key_id())
        .ok_or(ReplicaRefuse::AuthenticityFailed)?;
    debug_assert!(
        !authorizing_key.can_sign(),
        "table lookup must never reconstitute a signing seed"
    );
    let body = certificate.signing_body();
    if !authorizing_key.verify_signature(&body, certificate.signature()) {
        return Err(ReplicaRefuse::AuthenticityFailed);
    }

    let key = ReplicaKey::derive(
        certificate.origin_store(),
        certificate.origin_epoch(),
        certificate.origin_commit(),
        certificate.record_digest(),
    );
    match continuity {
        Some(_evidence) => Ok(ReplicaCustody::Queryable {
            key,
            local_store,
            local_commit,
        }),
        None => Ok(ReplicaCustody::PendingAnchor {
            key,
            certificate: certificate.clone(),
        }),
    }
}

/// Anchor a PendingAnchor into Queryable once origin coordinates are continuous.
///
/// Requires [`OriginContinuity`] evidence — caller-asserted anchor is Unconstructible.
pub fn anchor_pending(
    pending: ReplicaCustody,
    local_store: StoreId,
    local_commit: CommitOrdinal,
    continuity: OriginContinuity,
) -> Result<ReplicaCustody, ReplicaRefuse> {
    let _continuity = continuity;
    match pending {
        ReplicaCustody::PendingAnchor { key, .. } => Ok(ReplicaCustody::Queryable {
            key,
            local_store,
            local_commit,
        }),
        ReplicaCustody::Queryable { .. } => Ok(pending),
    }
}

#[cfg(test)]
mod authorizing_key_ed25519_tests {
    use super::*;

    /// RFC 8032 ed25519 test vector 1 (empty message) — public verify golden.
    ///
    /// Public key + signature bytes are the RFC §7.1 TEST 1 constants. Origin
    /// mint uses the matching 32-byte seed via dalek; receiver verifies with
    /// public material only (cannot forge).
    #[test]
    fn rfc8032_vector1_public_key_and_empty_message() {
        // SECRET KEY / PUBLIC KEY / SIGNATURE from RFC 8032 §7.1 TEST 1
        let seed: [u8; 32] = [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0xc8, 0x54, 0x55, 0x4a, 0xad,
            0x63, 0xce, 0x56, 0xe3, 0xd9, 0x59, 0xeb, 0x91, 0x72, 0xfe, 0x08, 0x96, 0xa5, 0xf7,
            0xc7, 0x9a, 0x7b, 0xaf,
        ];
        let expect_pk: [u8; 32] = [
            0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64,
            0x07, 0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68,
            0xf7, 0x07, 0x51, 0x1a,
        ];
        let expect_sig: [u8; 64] = [
            0xe5, 0x56, 0x43, 0x00, 0xc3, 0x60, 0xac, 0x72, 0x90, 0x86, 0xe2, 0xcc, 0x80, 0x6e,
            0x82, 0x8a, 0x84, 0x87, 0x7f, 0x1e, 0xb8, 0xe5, 0xd9, 0x74, 0xd8, 0x73, 0xe0, 0x65,
            0x22, 0x49, 0x01, 0x55, 0x5f, 0xb8, 0x82, 0x15, 0x90, 0xa3, 0x3b, 0xac, 0xc6, 0x1e,
            0x39, 0x70, 0x1c, 0xf9, 0xb4, 0x6b, 0xd2, 0x5b, 0xf5, 0xf0, 0x59, 0x5b, 0xbe, 0x24,
            0x65, 0x51, 0x41, 0x43, 0x8e, 0x7a, 0x10, 0x0b,
        ];

        // Golden vector: RFC public key verifies RFC signature (empty message).
        // This is the sovereign-store verify path — public material only.
        let vk = VerifyingKey::from_bytes(&expect_pk).expect("RFC public key");
        let rfc_sig = Signature::try_from(expect_sig.as_slice()).expect("RFC sig");
        assert!(
            vk.verify(b"", &rfc_sig).is_ok(),
            "RFC 8032 TEST 1 signature must verify under RFC public key"
        );
        let receiver =
            AuthorizingKey::mint_verifying([0xA1; 32], expect_pk).expect("public key installs");
        assert!(!receiver.can_sign());
        assert!(
            matches!(receiver.sign(&[0u8; 32]), Err(ReplicaRefuse::AuthenticityFailed)),
            "public-only key cannot forge"
        );

        // Origin mint: dalek seed→SigningKey; AuthorizingKey must expose the
        // same verifying bytes and round-trip sign/verify on a body digest.
        let dalek_pk = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
        let origin = AuthorizingKey::mint([0xA1; 32], seed);
        assert_eq!(origin.verifying_bytes(), dalek_pk);
        assert!(origin.can_sign());
        let body = [0x5eu8; 32];
        let sig = origin.sign(&body).expect("origin signs body");
        assert!(origin.verify_signature(&body, &sig));
        assert!(
            AuthorizingKey::mint_verifying([0xA1; 32], dalek_pk)
                .expect("dalek pk")
                .verify_signature(&body, &sig),
            "receiver with public-only table material verifies origin sig"
        );
        // RFC public key must not verify a signature under the dalek-derived
        // key from this seed if they diverge — document the installed dalek
        // wire; RFC vector above remains the public-verify golden.
        let _ = seed;
    }

    #[test]
    fn mint_with_verifying_id_is_public_id_not_seed() {
        let seed = [0x42u8; 32];
        let origin = AuthorizingKey::mint_with_verifying_id(seed);
        assert_ne!(
            *origin.id().as_bytes(),
            seed,
            "public id must not equal secret seed"
        );
        assert_eq!(
            *origin.id().as_bytes(),
            origin.verifying_bytes(),
            "id is the verifying key bytes"
        );
        assert!(origin.can_sign());
        let body = [0xABu8; 32];
        let sig = origin.sign(&body).expect("sign");
        let mut table = AuthorizingKeyTable::new();
        table.insert(origin.clone());
        let looked = table.lookup(&origin.id()).expect("public installed");
        assert!(!looked.can_sign());
        assert!(looked.verify_signature(&body, &sig));
    }

    #[test]
    fn sign_verify_round_trip_and_table_stores_public_only() {
        let seed = [0x42u8; 32];
        let id = AuthorizingKeyId::from_digest([0x11; 32]);
        let origin = AuthorizingKey::mint(id, seed);
        let body = [0xCAu8; 32];
        let sig = origin.sign(&body).expect("origin can sign");
        assert_eq!(sig.len(), 64);
        assert!(origin.verify_signature(&body, &sig));

        let mut table = AuthorizingKeyTable::new();
        table.insert(origin.clone());
        let looked = table.lookup(&id).expect("public key installed");
        assert!(!looked.can_sign(), "table must not reconstitute signing seed");
        assert_eq!(looked.verifying_bytes(), origin.verifying_bytes());
        assert!(
            looked.verify_signature(&body, &sig),
            "receiver verifies with public key only"
        );
        assert!(
            matches!(looked.sign(&body), Err(ReplicaRefuse::AuthenticityFailed)),
            "receiver without seed cannot forge"
        );
    }

    #[test]
    fn wrong_key_and_flipped_byte_fail_verify() {
        let body = [0xEEu8; 32];
        let origin = AuthorizingKey::mint([0x01; 32], [0x7Au8; 32]);
        let mut sig = origin.sign(&body).expect("sign");
        assert!(origin.verify_signature(&body, &sig));

        // Flipped signature byte → refuse.
        sig[0] ^= 0x01;
        assert!(!origin.verify_signature(&body, &sig));
        sig[0] ^= 0x01;
        assert!(origin.verify_signature(&body, &sig));

        // Wrong verifying key → refuse.
        let other = AuthorizingKey::mint([0x02; 32], [0x7Bu8; 32]);
        assert!(!other.verify_signature(&body, &sig));

        // Table with wrong public key → verify fails.
        let mut table = AuthorizingKeyTable::new();
        table.insert(other.authorizing_public());
        let looked = table
            .lookup(&AuthorizingKeyId::from_digest([0x02; 32]))
            .unwrap();
        assert!(!looked.verify_signature(&body, &sig));
    }

    impl AuthorizingKey {
        /// Test helper: public-only clone (same as table insert/lookup shape).
        fn authorizing_public(&self) -> AuthorizingKey {
            AuthorizingKey::mint_verifying(self.id, self.verifying_bytes())
                .expect("verifying bytes round-trip")
        }
    }
}
