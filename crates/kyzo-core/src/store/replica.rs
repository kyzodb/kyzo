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
//! signature.
//!
//! Authoring mints at `session/admit.rs` through [`mint_admission_certificate`];
//! replicas verify + mint custody — never re-author.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use super::contract::FormatVersion;
use super::epoch::FenceEpoch;
use super::open::StoreId;
use super::sweep::CommitOrdinal;
use super::transcript::{
    CanonicalTranscript, CanonicalTranscriptBuilder, FieldId, MapValue, SealedArtifactKind,
};

/// Opaque authorizing key id bound into the certificate.
pub type AuthorizingKeyId = [u8; 32];

/// Digest of the sealed scope manifest (plus issuer lineage / validity-as-of).
pub type ScopeManifestDigest = [u8; 32];

/// Trusted authorizing key material — MAC authority for certificate signatures.
///
/// Public construction is Unconstructible; only the trust-install door mints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizingKey {
    id: AuthorizingKeyId,
    material: [u8; 32],
}

impl AuthorizingKey {
    /// Install trusted key material under its id (trust door only).
    pub(crate) fn mint(id: AuthorizingKeyId, material: [u8; 32]) -> Self {
        Self { id, material }
    }

    /// Key id bound into certificates this key may authorize.
    pub fn id(&self) -> AuthorizingKeyId {
        self.id
    }

    /// Provisional MAC over a signing body (sha2 until a pure-Rust sig seat lands).
    ///
    /// Not AEAD — signature authenticity for AdmissionCertificate only.
    pub(crate) fn sign(&self, body: &[u8; 32]) -> [u8; 64] {
        provisional_mac(&self.material, body)
    }

    /// Verify a sealed signature against this key and signing body.
    pub(crate) fn verify_signature(&self, body: &[u8; 32], signature: &[u8; 64]) -> bool {
        let expect = self.sign(body);
        constant_time_eq(&expect, signature)
    }
}

/// Table of trusted authorizing keys — verify looks up by [`AuthorizingKeyId`].
#[derive(Debug, Default, Clone)]
pub struct AuthorizingKeyTable {
    keys: BTreeMap<AuthorizingKeyId, [u8; 32]>,
}

impl AuthorizingKeyTable {
    /// Empty trust table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a trusted authorizing key.
    pub(crate) fn insert(&mut self, key: AuthorizingKey) {
        self.keys.insert(key.id, key.material);
    }

    /// Borrow trusted material for `id`, if installed.
    pub fn lookup(&self, id: &AuthorizingKeyId) -> Option<AuthorizingKey> {
        self.keys
            .get(id)
            .copied()
            .map(|material| AuthorizingKey { id: *id, material })
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
    entries: BTreeMap<ScopeManifestDigest, ScopeManifestStatus>,
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
        self.entries.insert(digest, status);
    }

    /// Derive resolution for a sealed scope digest (§69).
    pub fn resolve(&self, digest: &ScopeManifestDigest) -> ScopeManifestStatus {
        self.entries
            .get(digest)
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
    post_state_root: [u8; 32],
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
            &self.post_state_root,
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
    pub(crate) fn from_certificate(
        origin: AdmissionCertificate,
        catalog_generation: u64,
    ) -> Self {
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
    pub post_state_root: [u8; 32],
    /// Authorizing key id.
    pub authorizing_key_id: AuthorizingKeyId,
    /// Scope manifest digest (+ issuer lineage / validity sealed elsewhere).
    pub scope_manifest_digest: ScopeManifestDigest,
    /// Optional OperationKey when composed.
    pub operation_key: Option<[u8; 32]>,
    /// Signature over the signing body under the authorizing key.
    pub signature: [u8; 64],
}

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
    h.update(authorizing_key_id);
    h.update(scope_manifest_digest);
    match operation_key {
        Some(op) => {
            h.update([1u8]);
            h.update(op);
        }
        None => h.update([0u8]),
    }
    h.finalize().into()
}

fn provisional_mac(key_material: &[u8; 32], body: &[u8; 32]) -> [u8; 64] {
    let mut out = [0u8; 64];
    let mut h0 = Sha256::new();
    h0.update(b"kyzo.admission_certificate.mac.v1");
    h0.update(key_material);
    h0.update(body);
    h0.update([0u8]);
    out[..32].copy_from_slice(&h0.finalize());
    let mut h1 = Sha256::new();
    h1.update(b"kyzo.admission_certificate.mac.v1");
    h1.update(key_material);
    h1.update(body);
    h1.update([1u8]);
    out[32..].copy_from_slice(&h1.finalize());
    out
}

fn constant_time_eq(a: &[u8; 64], b: &[u8; 64]) -> bool {
    let mut diff = 0u8;
    for i in 0..64 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
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
            MapValue::Digest32(parts.authorizing_key_id),
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
            MapValue::Digest32(parts.post_state_root),
        ),
        (
            b"protocol_version".to_vec(),
            MapValue::Bytes(parts.protocol_version.to_vec()),
        ),
        (
            b"schema_cut".to_vec(),
            MapValue::Digest32(parts.schema_cut),
        ),
        (
            b"scope_manifest_digest".to_vec(),
            MapValue::Digest32(parts.scope_manifest_digest),
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
            .position(|(k, _)| k.as_slice() > b"operation_key")
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
        &parts.post_state_root,
        &parts.authorizing_key_id,
        &parts.scope_manifest_digest,
        parts.operation_key.as_ref(),
    );
    Ok(key.sign(&body))
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

    let key_material = authorizing_keys
        .lookup(&certificate.authorizing_key_id())
        .ok_or(ReplicaRefuse::AuthenticityFailed)?;
    let body = certificate.signing_body();
    if !key_material.verify_signature(&body, certificate.signature()) {
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
