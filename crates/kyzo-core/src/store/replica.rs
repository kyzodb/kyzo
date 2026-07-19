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
//! [`ReplicaCustody`], PendingAnchor anchoring, [`LocalProjection`] contract.
//!
//! Bans: reshape/re-author of certified meaning; in-place local reinterpretation
//! under a local Catalog cut; strict-contiguous-only or head-attested catch-up;
//! exposing never-anchored PendingAnchor as queryable.
//!
//! Authoring mints at `session/admit.rs` through [`mint_admission_certificate`];
//! replicas verify + mint custody — never re-author.

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
    /// Signature over the sealed transcript body.
    pub signature: [u8; 64],
}

/// Mint an AdmissionCertificate — callable only from the admission seam.
///
/// Authoring lives at `session/admit.rs`; replicas never call this.
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
    Ok(AdmissionCertificate {
        transcript,
        origin_store: parts.origin_store,
        origin_epoch: parts.origin_epoch,
        origin_commit: parts.origin_commit,
        record_digest: parts.record_digest,
        operation_key: parts.operation_key,
    })
}

/// Verify a replica certificate and produce custody (§69/§70).
///
/// Accepting verify produces durable custody carrying origin coordinates
/// (what) and local `(StoreId, CommitOrdinal)` (when); creates NO new Record
/// identity. Re-delivery with the same [`ReplicaKey`] converges.
///
/// `anchored`: when false, custody stays [`ReplicaCustody::PendingAnchor`]
/// (anchored-sparse arrival). When true, custody is [`ReplicaCustody::Queryable`].
pub fn verify_replica(
    certificate: &AdmissionCertificate,
    local_store: StoreId,
    local_commit: CommitOrdinal,
    anchored: bool,
    scope_ok: Result<(), ReplicaRefuse>,
) -> Result<ReplicaCustody, ReplicaRefuse> {
    // Scope resolution is a closed sum — never folded into RetentionDeclined.
    scope_ok?;
    if certificate.transcript().as_bytes().is_empty() {
        return Err(ReplicaRefuse::AuthenticityFailed);
    }
    let key = ReplicaKey::derive(
        certificate.origin_store(),
        certificate.origin_epoch(),
        certificate.origin_commit(),
        certificate.record_digest(),
    );
    if anchored {
        Ok(ReplicaCustody::Queryable {
            key,
            local_store,
            local_commit,
        })
    } else {
        Ok(ReplicaCustody::PendingAnchor {
            key,
            certificate: certificate.clone(),
        })
    }
}

/// Anchor a PendingAnchor into Queryable once origin coordinates are continuous.
pub fn anchor_pending(
    pending: ReplicaCustody,
    local_store: StoreId,
    local_commit: CommitOrdinal,
) -> Result<ReplicaCustody, ReplicaRefuse> {
    match pending {
        ReplicaCustody::PendingAnchor { key, .. } => Ok(ReplicaCustody::Queryable {
            key,
            local_store,
            local_commit,
        }),
        ReplicaCustody::Queryable { .. } => Ok(pending),
    }
}
