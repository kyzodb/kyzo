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
//! [`NamespacedRecordIdentity`], [`ReplicaCustody`], [`ReplicaCustodyTable`],
//! PendingAnchor anchoring, [`LocalProjection`] contract,
//! [`AuthorizingKey`] / [`AuthorizingKeyTable`], [`ScopeManifestTable`],
//! [`OriginContinuity`], full crossing-contract validation before lowering
//! ([`validate_crossing_before_lower`], [`CrossingEnvelope`], [`CrossingValidated`]),
//! schema-preserving [`PromotionMeaning`] replay equality,
//! [`GraphBoundKey`] key-never-crosses-graph-boundary enforcement (#270 T3),
//! and the enforced signed-state-root-head (STH) gossip obligation
//! ([`SthGossipObligation`], [`SignedStateRootHead`], [`enforce_sth_gossip`])
//! so split-view equivocation is **Detected-on-gossip** (seats 2/56/58/69)
//! over NATS JetStream fabric carriage — never a peer-dial (seat 92).
//!
//! Bans: reshape/re-author of certified meaning; in-place local reinterpretation
//! under a local Catalog cut; strict-contiguous-only or head-attested catch-up;
//! exposing never-anchored PendingAnchor as queryable; caller-asserted
//! `scope_ok` / `anchored` verdicts; authenticity that ignores the sealed
//! signature; symmetric MAC standing in for certificate signatures (ed25519
//! public-key verify only — receivers must not hold the origin seed);
//! lowering a crossing record without validating kind/schema/authority/
//! context/evidence/status/capabilities; folding ScopeUnknown/Revoked/Denied
//! into RetentionDeclined; silent drop of missing declared evidence;
//! identity that lets distinct origins collapse into one (content-only id);
//! promotion that diverges identity/time/provenance/schema; a storage key
//! authorizing outside its [`GraphBoundary`]; peer-dial STH transport;
//! leaving federation non-equivocation Unexposed-until-chains-meet.
//!
//! Authoring mints at `session/admit.rs` through [`mint_admission_certificate`];
//! replicas verify + mint custody — never re-author.

use std::collections::BTreeMap;
use std::fmt;

use ed25519_dalek::{Signature as Ed25519Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest as ShaDigest, Sha256};

use super::contract::FormatVersion;
use super::crypto::{Digest, Signature};
use super::epoch::FenceEpoch;
use super::merkle::{
    ConsistencyProof, GossipConsistency, MerkleChainRefuse, StateRootHead, check_sth_gossip,
};
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
    pub(crate) fn sign(&self, body: &Digest) -> Result<Signature, ReplicaRefuse> {
        let signing = self
            .signing
            .as_ref()
            .ok_or(ReplicaRefuse::AuthenticityFailed)?;
        Ok(Signature::from_bytes(
            signing.sign(body.as_bytes().as_slice()).to_bytes(),
        ))
    }

    /// Verify a sealed ed25519 signature against this key's public material.
    pub(crate) fn verify_signature(&self, body: &Digest, signature: &Signature) -> bool {
        let Ok(sig) = Ed25519Signature::try_from(signature.as_bytes().as_slice()) else {
            return false;
        };
        self.verifying
            .verify_strict(body.as_bytes().as_slice(), &sig)
            .is_ok()
    }
}

/// Table of trusted authorizing **public** keys — verify looks up by [`AuthorizingKeyId`].
///
/// Stores verifying material only. A replica with this table can authenticate
/// certificates but cannot forge the origin's signatures.
#[derive(Debug, Clone)]
pub struct AuthorizingKeyTable {
    /// key_id → ed25519 verifying key bytes (32).
    keys: BTreeMap<[u8; 32], [u8; 32]>,
}

impl AuthorizingKeyTable {
    /// Empty trust table.
    pub fn new() -> Self {
        Self {
            keys: BTreeMap::new(),
        }
    }

    /// Install a trusted authorizing **public** key (signing seed discarded).
    pub(crate) fn insert(&mut self, key: AuthorizingKey) {
        self.keys.insert(*key.id.as_bytes(), key.verifying_bytes());
    }

    /// Lookup trusted public verifying material for `id`, if installed.
    ///
    /// Returned key can verify; [`AuthorizingKey::can_sign`] is always false.
    pub fn lookup(&self, id: &AuthorizingKeyId) -> Result<Option<AuthorizingKey>, ReplicaRefuse> {
        match self.keys.get(id.as_bytes()).copied() {
            None => Ok(None),
            Some(pk) => Ok(Some(AuthorizingKey::mint_verifying(*id, pk)?)),
        }
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
#[derive(Debug, Clone)]
pub struct ScopeManifestTable {
    entries: BTreeMap<[u8; 32], ScopeManifestStatus>,
}

impl ScopeManifestTable {
    /// Empty registry.
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Record a verified / revoked / incompatible manifest (trust door).
    pub(crate) fn set(&mut self, digest: ScopeManifestDigest, status: ScopeManifestStatus) {
        // INVARIANT(scope_status_stored): callers never pass Unknown — it is
        // the absent-entry default from resolve(), not a table value.
        self.entries.insert(*digest.as_bytes(), status);
    }

    /// Derive resolution for a sealed scope digest (§69).
    pub fn resolve(&self, digest: &ScopeManifestDigest) -> ScopeManifestStatus {
        match self.entries.get(digest.as_bytes()).copied() {
            Some(status) => status,
            None => ScopeManifestStatus::Unknown,
        }
    }

    /// Trust door: mark a previously verified manifest revoked.
    pub fn mark_revoked(&mut self, digest: ScopeManifestDigest) {
        self.set(digest, ScopeManifestStatus::Revoked);
    }

    /// Trust door: mark a known manifest incompatible with this replica.
    pub fn mark_incompatible(&mut self, digest: ScopeManifestDigest) {
        self.set(digest, ScopeManifestStatus::Incompatible);
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
    signature: Signature,
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

    /// Protocol / format version tag sealed into the certificate.
    pub fn protocol_version(&self) -> &[u8; 8] {
        &self.protocol_version
    }

    /// Origin schema cut sealed into the certificate (§69).
    ///
    /// Crossing receivers interpret as-of this cut forever. A local Catalog
    /// cut never rewrites this field in place — only [`LocalProjection`] or a
    /// newly admitted derived Record may carry a different reading.
    pub fn schema_cut(&self) -> &[u8; 32] {
        &self.schema_cut
    }

    /// Sealed signature bytes.
    pub fn signature(&self) -> &Signature {
        &self.signature
    }

    /// Signing body digest recomputed from sealed fields (excludes signature).
    pub fn signing_body(&self) -> Digest {
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
/// The one custody authority — [`NamespacedRecordIdentity::custody_key`]
/// delegates here; never a second competing key space.
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

/// Tenant / graph-scope digest bound into federation identity (#270 T2).
///
/// Distinct from origin authority and StoreId: a receiver may hold many
/// tenants; collapsing tenant out of identity lets distinct graphs collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TenantId([u8; 32]);

impl TenantId {
    /// Wrap an already-proven tenant / graph-scope digest.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Borrow the tenant digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for TenantId {
    fn from(digest: [u8; 32]) -> Self {
        Self(digest)
    }
}

impl AsRef<[u8]> for TenantId {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Namespaced federation record identity (#270 T2 / seat 70).
///
/// Four-tuple: **local id + origin authority + tenant + content**. Content
/// alone is never federation identity — two origins admitting identical
/// bytes must remain distinct. Custody still keys through [`ReplicaKey`]
/// (one authority); this type is the anti-collapse equality domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NamespacedRecordIdentity {
    local_id: [u8; 32],
    origin_authority: AuthorizingKeyId,
    tenant: TenantId,
    content: [u8; 32],
}

impl NamespacedRecordIdentity {
    /// Bind the four namespace seats — private fields, one constructor.
    pub fn bind(
        local_id: [u8; 32],
        origin_authority: AuthorizingKeyId,
        tenant: TenantId,
        content: [u8; 32],
    ) -> Self {
        Self {
            local_id,
            origin_authority,
            tenant,
            content,
        }
    }

    /// Bind from a verified certificate's authority + content, plus local id
    /// and tenant supplied by the receiving / admitting door.
    pub fn from_certificate(
        local_id: [u8; 32],
        certificate: &AdmissionCertificate,
        tenant: TenantId,
    ) -> Self {
        Self::bind(
            local_id,
            certificate.authorizing_key_id(),
            tenant,
            *certificate.record_digest(),
        )
    }

    /// Local admitted id bytes (session [`crate::session::record_id::RecordId`]).
    pub fn local_id(&self) -> &[u8; 32] {
        &self.local_id
    }

    /// Origin authorizing key id — distinct origins never share this seat.
    pub fn origin_authority(&self) -> AuthorizingKeyId {
        self.origin_authority
    }

    /// Tenant / graph scope.
    pub fn tenant(&self) -> TenantId {
        self.tenant
    }

    /// Record content digest.
    pub fn content(&self) -> &[u8; 32] {
        &self.content
    }

    /// Federation-stable digest of the four-tuple (not a custody key).
    pub fn digest(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(b"kyzo.namespaced_record_identity.v1");
        h.update(self.local_id);
        h.update(self.origin_authority.as_bytes());
        h.update(self.tenant.as_bytes());
        h.update(self.content);
        h.finalize().into()
    }

    /// Custody key under origin coordinates — delegates to [`ReplicaKey`]
    /// (seat 70); does not mint a second custody authority.
    pub fn custody_key(
        &self,
        origin_store: StoreId,
        origin_epoch: FenceEpoch,
        origin_commit: CommitOrdinal,
    ) -> ReplicaKey {
        ReplicaKey::derive(origin_store, origin_epoch, origin_commit, &self.content)
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

impl ReplicaCustody {
    /// Convergent [`ReplicaKey`] for this custody — Queryable and PendingAnchor share it.
    pub fn key(&self) -> ReplicaKey {
        match self {
            Self::Queryable { key, .. } | Self::PendingAnchor { key, .. } => *key,
        }
    }
}

/// Exactly-once custody ledger keyed by [`ReplicaKey`] (§70 / #270 T2).
///
/// At-least-once fabric deliveries admit through [`ReplicaCustodyTable::admit`]:
/// first delivery inserts; duplicates converge on the held custody — no
/// reshape, no double-mint.
#[derive(Debug, Clone)]
pub struct ReplicaCustodyTable {
    by_key: BTreeMap<[u8; 32], ReplicaCustody>,
}

impl ReplicaCustodyTable {
    /// Empty custody ledger.
    pub fn new() -> Self {
        Self {
            by_key: BTreeMap::new(),
        }
    }

    /// Admit custody under its [`ReplicaKey`]. Duplicate key → existing entry
    /// (idempotent). Never reshapes a held custody into a second mint.
    pub fn admit(&mut self, custody: ReplicaCustody) -> &ReplicaCustody {
        let key = *custody.key().as_bytes();
        self.by_key.entry(key).or_insert(custody)
    }

    /// Lookup held custody by key, if any.
    pub fn get(&self, key: &ReplicaKey) -> Option<&ReplicaCustody> {
        self.by_key.get(key.as_bytes())
    }

    /// Number of distinct custody keys held.
    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    /// True when no custody is held.
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }
}

/// Local rebuildable projection of origin-schema interpretation (§69).
///
/// A local Catalog produces exactly this (rebuildable cache) or a derived
/// Record through ordinary admission carrying the certificate as evidence —
/// no third constructor. In-place reinterpretation of a certified Record
/// under a local Catalog cut is Unconstructible
/// ([`refuse_in_place_local_reinterpretation`] /
/// [`view_under_schema_cut`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalProjection {
    /// Origin certificate this projection was built from.
    origin: AdmissionCertificate,
    /// Local Catalog schema cut at projection build — never reshapes origin
    /// certificate meaning; cache under projection law only.
    local_schema_cut: [u8; 32],
}

impl LocalProjection {
    /// Bind a verified certificate to a local Catalog schema cut.
    pub(crate) fn from_certificate(
        origin: AdmissionCertificate,
        local_schema_cut: [u8; 32],
    ) -> Self {
        Self {
            origin,
            local_schema_cut,
        }
    }

    /// Origin certificate.
    pub fn origin(&self) -> &AdmissionCertificate {
        &self.origin
    }

    /// Local Catalog schema cut at build (projection cache only).
    pub fn local_schema_cut(&self) -> &[u8; 32] {
        &self.local_schema_cut
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
    /// Split-view / equivocation detected on STH gossip (Detected-on-gossip).
    #[error("SplitViewDetected: divergent STH gossip before chains meet")]
    #[diagnostic(code(store::replica::split_view_detected))]
    SplitViewDetected,
    /// STH gossip consistency proof required but absent.
    #[error("SthConsistencyProofRequired: unequal STH ordinals need a proof")]
    #[diagnostic(code(store::replica::sth_consistency_proof_required))]
    SthConsistencyProofRequired,
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
    pub signature: Signature,
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
) -> Digest {
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
    Digest::from_bytes(h.finalize().into())
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
            MapValue::Bytes(parts.signature.as_bytes().to_vec()),
        ),
    ];
    if let Some(op) = parts.operation_key {
        // Insert "operation_key" in sorted position (after origin_epoch, before post_state_root).
        let idx = match bindings
            .iter()
            .position(|(k, _)| k.as_slice() > b"operation_key".as_slice())
        {
            Some(i) => i,
            None => bindings.len(),
        };
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
) -> Result<Signature, ReplicaRefuse> {
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
/// identity. Re-delivery with the same [`ReplicaKey`] converges — admit into
/// [`ReplicaCustodyTable`] for exactly-once ledger retention.
///
/// Federation-facing equality is [`NamespacedRecordIdentity`] (local id +
/// origin authority + tenant + content); content-only ids must not collapse
/// distinct origins. Custody still keys through [`ReplicaKey`] — one authority.
///
/// Authenticity, scope, and anchor are **derived**:
/// - signature checked against [`AuthorizingKeyTable`] lookup of
///   `certificate.authorizing_key_id`
/// - scope refuse derived from [`ScopeManifestTable::resolve`]
/// - Queryable vs PendingAnchor from [`OriginContinuity`] evidence (projection
///   / chain door), never a caller-asserted bool
///
/// Full crossing-contract validation (kind/schema/authority/context/evidence/
/// status/capabilities) before lowering is [`validate_crossing_before_lower`] —
/// this door alone does not authorize lowering.
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

    let authorizing_key = match authorizing_keys.lookup(&certificate.authorizing_key_id())? {
        Some(k) => k,
        None => return Err(ReplicaRefuse::AuthenticityFailed),
    };
    // INVARIANT(verifying_only_lookup): AuthorizingKeyTable::lookup mints
    // verifying material only — can_sign is always false.
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
        Some(_evidence) => Ok(verify_replica_finish(
            certificate,
            ReplicaCustody::Queryable {
                key,
                local_store,
                local_commit,
            },
        )),
        None => Ok(verify_replica_finish(
            certificate,
            ReplicaCustody::PendingAnchor {
                key,
                certificate: certificate.clone(),
            },
        )),
    }
}

/// Finish verify after authenticity + custody arm selection.
fn verify_replica_finish(
    _certificate: &AdmissionCertificate,
    custody: ReplicaCustody,
) -> ReplicaCustody {
    custody
}

// ── Crossing contract before lowering (§69 / story #270 T1) ─────────────────

/// Closed ONTOK kind wire tags on a crossing envelope — unknown tag refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CrossingKind {
    /// Entity.
    Entity = 0,
    /// Event.
    Event = 1,
    /// State.
    State = 2,
    /// Role.
    Role = 3,
    /// Relation.
    Relation = 4,
    /// Claim.
    Claim = 5,
    /// Evidence.
    Evidence = 6,
    /// Context.
    Context = 7,
    /// Concept.
    Concept = 8,
    /// Rule.
    Rule = 9,
    /// Derivation.
    Derivation = 10,
    /// Invalidation.
    Invalidation = 11,
}

impl CrossingKind {
    /// Decode a wire tag; unknown → [`CrossingRefuse::KindInvalid`].
    pub fn from_wire(tag: u8) -> Result<Self, CrossingRefuse> {
        match tag {
            0 => Ok(Self::Entity),
            1 => Ok(Self::Event),
            2 => Ok(Self::State),
            3 => Ok(Self::Role),
            4 => Ok(Self::Relation),
            5 => Ok(Self::Claim),
            6 => Ok(Self::Evidence),
            7 => Ok(Self::Context),
            8 => Ok(Self::Concept),
            9 => Ok(Self::Rule),
            10 => Ok(Self::Derivation),
            11 => Ok(Self::Invalidation),
            12..=u8::MAX => Err(CrossingRefuse::KindInvalid),
        }
    }

    /// Wire tag for this kind.
    pub fn as_wire(self) -> u8 {
        match self {
            Self::Entity => 0,
            Self::Event => 1,
            Self::State => 2,
            Self::Role => 3,
            Self::Relation => 4,
            Self::Claim => 5,
            Self::Evidence => 6,
            Self::Context => 7,
            Self::Concept => 8,
            Self::Rule => 9,
            Self::Derivation => 10,
            Self::Invalidation => 11,
        }
    }
}

/// Context scope carried on a crossing envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CrossingContext {
    /// No durable context scope.
    Unscoped,
    /// Scoped to a context digest.
    Scoped([u8; 32]),
}

impl CrossingContext {
    /// Decode wire form: `0` → Unscoped, `1` + digest → Scoped; else ContextInvalid.
    pub fn from_wire(tag: u8, digest: Option<[u8; 32]>) -> Result<Self, CrossingRefuse> {
        match (tag, digest) {
            (0, None) => Ok(Self::Unscoped),
            (1, Some(d)) => Ok(Self::Scoped(d)),
            (0, Some(_)) | (1, None) | (2..=u8::MAX, _) => Err(CrossingRefuse::ContextInvalid),
        }
    }
}

/// Whether the envelope declares evidence as required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CrossingEvidenceDemand {
    /// Interpreted knowledge — evidence must be present.
    DeclaredRequired,
    /// Evidence not required for this surface/kind.
    NotRequired,
}

/// Evidence payload presence on the envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CrossingEvidence {
    /// Evidence digest present.
    Present([u8; 32]),
    /// No evidence payload.
    Absent,
}

/// Crossing record status — only [`CrossingStatus::Active`] may lower.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CrossingStatus {
    /// Live certified meaning — may lower after full validation.
    Active,
    /// Semantic invalidation — not lowerable as live meaning.
    Invalidated,
    /// Tombstone supersession — not lowerable as live meaning.
    Tombstoned,
    /// Retention redaction — not lowerable as live meaning.
    RetentionRedacted,
}

/// Opaque shared-capability digests claimed by a crossing envelope.
///
/// Receiver must hold every claimed digest or refuse
/// [`CrossingRefuse::CapabilityMissing`] — never reshape into
/// [`ReplicaRefuse::RetentionDeclined`] or [`ReplicaRefuse::ScopeDenied`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CrossingCapabilitySet {
    digests: BTreeMap<[u8; 32], ()>,
}

impl CrossingCapabilitySet {
    /// Empty capability set.
    pub fn new() -> Self {
        Self {
            digests: BTreeMap::new(),
        }
    }

    /// Insert a claimed / held capability digest.
    pub fn insert(&mut self, digest: [u8; 32]) {
        self.digests.insert(digest, ());
    }

    /// True when every digest in `claimed` is present in this set.
    pub fn covers(&self, claimed: &CrossingCapabilitySet) -> bool {
        claimed.digests.keys().all(|d| self.digests.contains_key(d))
    }

    /// Borrow claimed digests in ascending order.
    pub fn digests(&self) -> impl Iterator<Item = &[u8; 32]> {
        self.digests.keys()
    }
}

/// Declared crossing envelope validated against the certificate before lowering.
///
/// Kind, schema version, schema cut, issuing authority, context, evidence
/// demand/presence, status, and shared capabilities — the full contract the
/// receiver checks before any projection lower (§69 / #270 T1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossingEnvelope {
    /// ONTOK kind wire tag.
    kind: CrossingKind,
    /// Protocol / schema version — must equal certificate protocol_version.
    schema_version: [u8; 8],
    /// Schema cut — must equal certificate schema_cut (origin cut forever).
    schema_cut: [u8; 32],
    /// Issuing authority — must equal certificate authorizing_key_id.
    issuing_authority: AuthorizingKeyId,
    /// Durable context scope.
    context: CrossingContext,
    /// Whether evidence is declared required.
    evidence_demand: CrossingEvidenceDemand,
    /// Evidence payload presence.
    evidence: CrossingEvidence,
    /// Record status.
    status: CrossingStatus,
    /// Shared capabilities the envelope claims.
    shared_capabilities: CrossingCapabilitySet,
}

impl CrossingEnvelope {
    /// Assemble a crossing envelope from declared contract fields.
    #[allow(clippy::too_many_arguments)] // closed contract fields are explicit seats
    pub fn new(
        kind: CrossingKind,
        schema_version: [u8; 8],
        schema_cut: [u8; 32],
        issuing_authority: AuthorizingKeyId,
        context: CrossingContext,
        evidence_demand: CrossingEvidenceDemand,
        evidence: CrossingEvidence,
        status: CrossingStatus,
        shared_capabilities: CrossingCapabilitySet,
    ) -> Self {
        Self {
            kind,
            schema_version,
            schema_cut,
            issuing_authority,
            context,
            evidence_demand,
            evidence,
            status,
            shared_capabilities,
        }
    }

    /// ONTOK kind.
    pub fn kind(&self) -> CrossingKind {
        self.kind
    }

    /// Schema / protocol version bytes.
    pub fn schema_version(&self) -> &[u8; 8] {
        &self.schema_version
    }

    /// Schema cut digest.
    pub fn schema_cut(&self) -> &[u8; 32] {
        &self.schema_cut
    }

    /// Issuing authority key id.
    pub fn issuing_authority(&self) -> AuthorizingKeyId {
        self.issuing_authority
    }

    /// Context scope.
    pub fn context(&self) -> CrossingContext {
        self.context
    }

    /// Evidence demand.
    pub fn evidence_demand(&self) -> CrossingEvidenceDemand {
        self.evidence_demand
    }

    /// Evidence presence.
    pub fn evidence(&self) -> CrossingEvidence {
        self.evidence
    }

    /// Status.
    pub fn status(&self) -> CrossingStatus {
        self.status
    }

    /// Shared capabilities claimed.
    pub fn shared_capabilities(&self) -> &CrossingCapabilitySet {
        &self.shared_capabilities
    }
}

/// Proof that the full crossing contract passed — required before lowering.
///
/// Opaque: only [`validate_crossing_before_lower`] mints this. Holding this
/// token means kind/schema/authority/context/evidence/status/capabilities
/// were checked, [`verify_replica`] accepted custody, and the certificate's
/// record content digest is bound — the token authorizes that record only
/// (seat 69: no confused-deputy reuse across same-kind records).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossingValidated {
    custody: ReplicaCustody,
    origin_schema_cut: [u8; 32],
    kind: CrossingKind,
    /// Certificate [`AdmissionCertificate::record_digest`] this validation sealed.
    record_content_digest: [u8; 32],
    _priv: (),
}

impl CrossingValidated {
    /// Custody produced by [`verify_replica`] under this validation.
    pub fn custody(&self) -> &ReplicaCustody {
        &self.custody
    }

    /// Origin schema cut sealed on the certificate — never a local Catalog cut.
    pub fn origin_schema_cut(&self) -> &[u8; 32] {
        &self.origin_schema_cut
    }

    /// Validated ONTOK kind.
    pub fn kind(&self) -> CrossingKind {
        self.kind
    }

    /// Record content digest this token validated — identity gate for lowering.
    pub fn record_digest(&self) -> &[u8; 32] {
        &self.record_content_digest
    }

    /// Alias for [`Self::record_digest`].
    pub fn record_content_digest(&self) -> &[u8; 32] {
        self.record_digest()
    }
}

/// Closed crossing-contract refuse sum (§69 / #270 T1).
///
/// Manifest outcomes stay on [`ReplicaRefuse`] (`ScopeUnknown` / `ScopeRevoked`
/// / `ScopeDenied`) and are **never** folded into `RetentionDeclined`.
/// Envelope-specific refuses are distinct arms here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum CrossingRefuse {
    /// Replica authenticity / scope / chain refuse (includes typed Scope*).
    #[error(transparent)]
    #[diagnostic(transparent)]
    Replica(#[from] ReplicaRefuse),
    /// Envelope kind wire tag unknown or refused.
    #[error("KindInvalid: crossing kind is not a known ONTOK tag")]
    #[diagnostic(code(store::replica::crossing_kind_invalid))]
    KindInvalid,
    /// Record ONTOK kind ≠ validated envelope kind (release-enforced).
    #[error("KindMismatch: record kind disagreed with CrossingValidated kind")]
    #[diagnostic(code(store::replica::crossing_kind_mismatch))]
    KindMismatch,
    /// Record content digest ≠ digest sealed on [`CrossingValidated`] (seat 69).
    #[error("RecordIdentityMismatch: record digest disagreed with CrossingValidated record digest")]
    #[diagnostic(code(store::replica::crossing_record_identity))]
    RecordIdentityMismatch,
    /// Envelope schema version ≠ certificate protocol_version.
    #[error("SchemaVersionMismatch: envelope schema version disagreed with certificate")]
    #[diagnostic(code(store::replica::crossing_schema_version))]
    SchemaVersionMismatch,
    /// Envelope schema cut ≠ certificate origin schema_cut.
    #[error("SchemaCutMismatch: envelope schema cut disagreed with certificate")]
    #[diagnostic(code(store::replica::crossing_schema_cut))]
    SchemaCutMismatch,
    /// Envelope issuing authority ≠ certificate authorizing_key_id.
    #[error("AuthorityMismatch: envelope authority disagreed with certificate")]
    #[diagnostic(code(store::replica::crossing_authority))]
    AuthorityMismatch,
    /// Envelope context is not a closed CrossingContext form.
    #[error("ContextInvalid: crossing context refused")]
    #[diagnostic(code(store::replica::crossing_context))]
    ContextInvalid,
    /// Envelope declared evidence required but evidence is absent.
    #[error("DeclaredEvidenceMissing: declared evidence required but absent")]
    #[diagnostic(code(store::replica::declared_evidence_missing))]
    DeclaredEvidenceMissing,
    /// Status is not Active — cannot lower as live meaning.
    #[error("StatusNotLowerable: crossing status refuses live lowering")]
    #[diagnostic(code(store::replica::crossing_status))]
    StatusNotLowerable,
    /// Receiver lacks a claimed shared capability.
    #[error("CapabilityMissing: shared capability claimed but not held")]
    #[diagnostic(code(store::replica::crossing_capability))]
    CapabilityMissing,
    /// In-place local reinterpretation under a local schema cut (§69).
    #[error(
        "LocalReinterpretationUnconstructible: certified meaning under a local Catalog cut is Unconstructible — use LocalProjection or a derived Record"
    )]
    #[diagnostic(code(store::replica::local_reinterpretation))]
    LocalReinterpretationUnconstructible,
}

/// Validate the full crossing contract **before** any lowering (§69 / #270 T1).
///
/// Order: [`verify_replica`] (authenticity + typed Scope* ≠ RetentionDeclined)
/// then envelope kind / schema version / schema cut / authority / context /
/// evidence / status / shared capabilities. Missing declared evidence →
/// [`CrossingRefuse::DeclaredEvidenceMissing`] (typed, not silent drop).
///
/// Success mints [`CrossingValidated`] — the only token that authorizes
/// crossing lowering. In-place reinterpretation under a local schema cut is
/// Unconstructible ([`CrossingRefuse::LocalReinterpretationUnconstructible`]);
/// use [`LocalProjection`] or admit a derived Record.
#[allow(clippy::too_many_arguments)] // trust tables + envelope seats are explicit
pub fn validate_crossing_before_lower(
    certificate: &AdmissionCertificate,
    envelope: &CrossingEnvelope,
    local_store: StoreId,
    local_commit: CommitOrdinal,
    authorizing_keys: &AuthorizingKeyTable,
    scopes: &ScopeManifestTable,
    continuity: Option<&OriginContinuity>,
    held_capabilities: &CrossingCapabilitySet,
) -> Result<CrossingValidated, CrossingRefuse> {
    // 1) Replica path first — ScopeUnknown/Revoked/Denied stay distinct.
    let custody = verify_replica(
        certificate,
        local_store,
        local_commit,
        authorizing_keys,
        scopes,
        continuity,
    )?;

    // 2) Kind — closed wire set (construction already typed; belt for raw tags).
    let _kind = CrossingKind::from_wire(envelope.kind().as_wire())?;

    // 3) Schema version ↔ certificate protocol_version.
    if envelope.schema_version() != certificate.protocol_version() {
        return Err(CrossingRefuse::SchemaVersionMismatch);
    }

    // 4) Schema cut ↔ certificate origin cut (never a local Catalog rewrite).
    if envelope.schema_cut() != certificate.schema_cut() {
        return Err(CrossingRefuse::SchemaCutMismatch);
    }

    // 5) Issuing authority ↔ certificate authorizing key.
    if envelope.issuing_authority() != certificate.authorizing_key_id() {
        return Err(CrossingRefuse::AuthorityMismatch);
    }

    // 6) Context — closed sum only (Unscoped | Scoped); third form Unconstructible
    // at construction / [`CrossingContext::from_wire`].
    match envelope.context() {
        CrossingContext::Unscoped | CrossingContext::Scoped(_) => {}
    }

    // 7) Evidence — declared required + absent → typed refuse (not silent drop).
    match (envelope.evidence_demand(), envelope.evidence()) {
        (CrossingEvidenceDemand::DeclaredRequired, CrossingEvidence::Absent) => {
            return Err(CrossingRefuse::DeclaredEvidenceMissing);
        }
        (CrossingEvidenceDemand::DeclaredRequired, CrossingEvidence::Present(_))
        | (CrossingEvidenceDemand::NotRequired, CrossingEvidence::Absent)
        | (CrossingEvidenceDemand::NotRequired, CrossingEvidence::Present(_)) => {}
    }

    // 8) Status — only Active may lower as live meaning.
    if !matches!(envelope.status(), CrossingStatus::Active) {
        return Err(CrossingRefuse::StatusNotLowerable);
    }

    // 9) Shared capabilities — every claimed digest must be held.
    if !held_capabilities.covers(envelope.shared_capabilities()) {
        return Err(CrossingRefuse::CapabilityMissing);
    }

    Ok(CrossingValidated {
        custody,
        origin_schema_cut: *certificate.schema_cut(),
        kind: envelope.kind(),
        record_content_digest: *certificate.record_digest(),
        _priv: (),
    })
}

/// Explicit refuse for in-place local reinterpretation under a local Catalog cut.
///
/// §69: certified origin meaning is sealed as-of the certificate schema cut.
/// A local Catalog wanting a different reading produces [`LocalProjection`] or
/// a newly admitted derived Record — never mutates the certified record in place.
pub fn refuse_in_place_local_reinterpretation() -> CrossingRefuse {
    CrossingRefuse::LocalReinterpretationUnconstructible
}

/// View certified origin meaning under a candidate schema cut (§69 / #270 T3).
///
/// Equal cuts → same sealed interpretation. A different local Catalog cut is
/// Unconstructible in place — produce [`LocalProjection`] or a derived Record.
pub fn view_under_schema_cut(
    origin_schema_cut: &[u8; 32],
    candidate_cut: &[u8; 32],
) -> Result<(), CrossingRefuse> {
    if origin_schema_cut == candidate_cut {
        Ok(())
    } else {
        Err(CrossingRefuse::LocalReinterpretationUnconstructible)
    }
}

// ── Promotion replay equality + graph-bound keys (#270 T3) ───────────────────

/// Graph / tenant boundary a storage key is confined to (#270 T3).
///
/// A key never crosses this boundary as authority — per-graph blast radius.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphBoundary(TenantId);

impl GraphBoundary {
    /// Bind a tenant / graph-scope as the confinement boundary.
    pub fn from_tenant(tenant: TenantId) -> Self {
        Self(tenant)
    }

    /// Borrow the tenant / graph-scope.
    pub fn tenant(self) -> TenantId {
        self.0
    }
}

/// Typed refuse when a storage key is offered as authority outside its graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum KeyBoundaryRefuse {
    /// Key's sealed graph ≠ acting graph — authority does not travel.
    #[error("KeyCrossesGraphBoundary: storage key cannot authorize outside its graph")]
    #[diagnostic(code(store::replica::key_crosses_graph_boundary))]
    KeyCrossesGraphBoundary,
}

/// Storage / custody key confined to one [`GraphBoundary`] (#270 T3).
///
/// Holding key bytes is not authority under a foreign graph. Authorize only
/// when the acting graph equals the sealed boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GraphBoundKey {
    graph: GraphBoundary,
    key: [u8; 32],
}

impl GraphBoundKey {
    /// Confine a [`ReplicaKey`] to a graph boundary.
    pub fn bind(graph: GraphBoundary, key: &ReplicaKey) -> Self {
        Self {
            graph,
            key: *key.as_bytes(),
        }
    }

    /// Confine an already-proven key digest to a graph boundary.
    pub fn bind_digest(graph: GraphBoundary, key: [u8; 32]) -> Self {
        Self { graph, key }
    }

    /// Sealed graph boundary.
    pub fn graph(self) -> GraphBoundary {
        self.graph
    }

    /// Key digest bytes (not authority under a foreign graph).
    pub fn key_digest(&self) -> &[u8; 32] {
        &self.key
    }

    /// Authorize under `acting` only when it equals the sealed boundary.
    pub fn authorize(&self, acting: GraphBoundary) -> Result<(), KeyBoundaryRefuse> {
        if self.graph == acting {
            Ok(())
        } else {
            Err(KeyBoundaryRefuse::KeyCrossesGraphBoundary)
        }
    }
}

/// Meaning seats preserved across promotion (#270 T3).
///
/// Identity / valid-time / provenance / schema — export/import and
/// local-to-hosted promotion must replay-equal on these four. Digests only
/// at the store seat (session binds live record fields into digests).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PromotionMeaning {
    identity: NamespacedRecordIdentity,
    valid_time: [u8; 32],
    provenance: [u8; 32],
    schema_cut: [u8; 32],
}

impl PromotionMeaning {
    /// Bind the four preserved seats — private fields, one constructor.
    pub fn bind(
        identity: NamespacedRecordIdentity,
        valid_time: [u8; 32],
        provenance: [u8; 32],
        schema_cut: [u8; 32],
    ) -> Self {
        Self {
            identity,
            valid_time,
            provenance,
            schema_cut,
        }
    }

    /// Federation identity seat.
    pub fn identity(&self) -> &NamespacedRecordIdentity {
        &self.identity
    }

    /// Valid-time digest seat.
    pub fn valid_time(&self) -> &[u8; 32] {
        &self.valid_time
    }

    /// Provenance / source digest seat.
    pub fn provenance(&self) -> &[u8; 32] {
        &self.provenance
    }

    /// Origin schema cut seat — never a local Catalog rewrite.
    pub fn schema_cut(&self) -> &[u8; 32] {
        &self.schema_cut
    }

    /// Stable digest over all four seats (replay meter).
    pub fn digest(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(b"kyzo.promotion_meaning.v1");
        h.update(self.identity.digest());
        h.update(self.valid_time);
        h.update(self.provenance);
        h.update(self.schema_cut);
        h.finalize().into()
    }

    /// Replay equality: every seat identical (where it runs may change).
    pub fn replay_equal(&self, other: &Self) -> bool {
        self == other
    }
}

/// Typed refuse when promotion diverges sealed meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum PromotionRefuse {
    /// Identity / time / provenance / schema diverged across promotion.
    #[error("MeaningDiverged: promotion did not preserve identity/time/provenance/schema")]
    #[diagnostic(code(store::replica::promotion_meaning_diverged))]
    MeaningDiverged,
}

/// Prove export/import or local-to-hosted promotion preserved meaning (#270 T3).
///
/// Destination StoreId / host may change; the four meaning seats must
/// [`PromotionMeaning::replay_equal`].
pub fn prove_promotion_replay(
    before: &PromotionMeaning,
    after: &PromotionMeaning,
) -> Result<(), PromotionRefuse> {
    if before.replay_equal(after) {
        Ok(())
    } else {
        Err(PromotionRefuse::MeaningDiverged)
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

// ── STH gossip obligation (CT non-equivocation; seats 2/56/58/69/92) ───────

/// JetStream subject that carries compact STH digests for one Store.
///
/// Gossip rides the NATS fabric (seat 92) — cadence publish + designated
/// peer/auditor pull. A peer-dial endpoint type is Unconstructible here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SthGossipSubject {
    store_id: StoreId,
}

impl SthGossipSubject {
    /// Subject for one Store identity.
    pub fn for_store(store_id: StoreId) -> Self {
        Self { store_id }
    }

    /// Store this subject names.
    pub fn store_id(self) -> StoreId {
        self.store_id
    }

    /// Canonical JetStream subject string for compact root-digest gossip.
    pub fn jetstream_subject(self) -> String {
        let mut hex = String::with_capacity(64);
        for b in self.store_id.as_bytes() {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            hex.push(char::from(HEX[usize::from(b >> 4)]));
            hex.push(char::from(HEX[usize::from(b & 0x0f)]));
        }
        format!("kyzo.sth.{hex}")
    }
}

/// Closed STH gossip carriage — JetStream fabric only (seat 92).
///
/// No peer-dial variant exists; fabric-down is host/`Refuse(FabricUnavailable)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SthGossipCarriage {
    /// Compact STH digest on a JetStream subject (cadence publish + auditor pull).
    JetStream(SthGossipSubject),
}

/// Enforced signed-state-root-head gossip obligation for one Store.
///
/// Origin publishes signed compact heads on [`SthGossipCarriage::JetStream`];
/// designated peers/auditors pull and cross-check via [`enforce_sth_gossip`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SthGossipObligation {
    carriage: SthGossipCarriage,
}

impl SthGossipObligation {
    /// Seat the obligation on the NATS JetStream subject for `store_id`.
    pub fn on_jetstream(store_id: StoreId) -> Self {
        Self {
            carriage: SthGossipCarriage::JetStream(SthGossipSubject::for_store(store_id)),
        }
    }

    /// Fabric carriage (JetStream only).
    pub fn carriage(self) -> SthGossipCarriage {
        self.carriage
    }

    /// JetStream subject when carriage is JetStream.
    pub fn subject(self) -> SthGossipSubject {
        match self.carriage {
            SthGossipCarriage::JetStream(s) => s,
        }
    }
}

/// Signed state-root head — CT STH analogue under the origin authorizing key.
///
/// The signed body is [`StateRootHead::compact_digest`]: SHA-256 over the ONE
/// `encode_state_root_head` CanonicalTranscript bytes — never a hand-rolled
/// field hash of the head (seat 59).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SignedStateRootHead {
    head: StateRootHead,
    authorizing_key_id: AuthorizingKeyId,
    signature: Signature,
}

impl SignedStateRootHead {
    /// Sign a compact head under an origin authorizing key.
    ///
    /// Signs [`StateRootHead::compact_digest`] (transcript-derived), not raw
    /// head fields.
    pub(crate) fn sign(head: StateRootHead, key: &AuthorizingKey) -> Result<Self, ReplicaRefuse> {
        let body = Digest::from_bytes(
            head.compact_digest()
                .map_err(|_| ReplicaRefuse::AuthenticityFailed)?,
        );
        let signature = key.sign(&body)?;
        Ok(Self {
            head,
            authorizing_key_id: key.id(),
            signature,
        })
    }

    /// Unsigned head meaning.
    pub fn head(&self) -> StateRootHead {
        self.head
    }

    /// Authorizing key id bound into the signature.
    pub fn authorizing_key_id(&self) -> AuthorizingKeyId {
        self.authorizing_key_id
    }

    /// ed25519 signature.
    pub fn signature(&self) -> &Signature {
        &self.signature
    }

    /// Verify signature against a trusted key table.
    ///
    /// Recomputes the transcript-derived compact digest — same body as
    /// [`Self::sign`].
    pub fn verify_authenticity(&self, keys: &AuthorizingKeyTable) -> Result<(), ReplicaRefuse> {
        let key = match keys.lookup(&self.authorizing_key_id)? {
            Some(k) => k,
            None => return Err(ReplicaRefuse::AuthenticityFailed),
        };
        let body = Digest::from_bytes(
            self.head
                .compact_digest()
                .map_err(|_| ReplicaRefuse::AuthenticityFailed)?,
        );
        if key.verify_signature(&body, &self.signature) {
            Ok(())
        } else {
            Err(ReplicaRefuse::AuthenticityFailed)
        }
    }
}

/// Enforce STH gossip non-equivocation between two fabric-observed signed heads.
///
/// Verifies both signatures, then runs [`check_sth_gossip`]. Split-view is
/// [`ReplicaRefuse::SplitViewDetected`] — Detected-on-gossip, before chains meet.
pub fn enforce_sth_gossip(
    obligation: &SthGossipObligation,
    observed_a: &SignedStateRootHead,
    observed_b: &SignedStateRootHead,
    proof: Option<&ConsistencyProof>,
    keys: &AuthorizingKeyTable,
) -> Result<GossipConsistency, ReplicaRefuse> {
    let subject_store = obligation.subject().store_id();
    if observed_a.head().store_id() != subject_store
        || observed_b.head().store_id() != subject_store
    {
        return Err(ReplicaRefuse::AuthenticityFailed);
    }
    observed_a.verify_authenticity(keys)?;
    observed_b.verify_authenticity(keys)?;
    match check_sth_gossip(&observed_a.head(), &observed_b.head(), proof) {
        Ok(c) => Ok(c),
        Err(MerkleChainRefuse::SplitViewDetected) => Err(ReplicaRefuse::SplitViewDetected),
        Err(MerkleChainRefuse::ConsistencyProofRequired) => {
            Err(ReplicaRefuse::SthConsistencyProofRequired)
        }
        Err(MerkleChainRefuse::SthStoreMismatch) => Err(ReplicaRefuse::AuthenticityFailed),
        Err(_) => Err(ReplicaRefuse::SplitViewDetected),
    }
}

#[cfg(test)]
mod authorizing_key_ed25519_tests {
    use super::*;
    use ed25519_dalek::Verifier;
    use miette::{IntoDiagnostic, Result, miette};

    /// GUARDIAN RED GATE (#376 T1) -- empirically confirmed against the pinned
    /// ed25519-dalek 3.0.0-rc.1. `verify_signature` uses the PERMISSIVE `.verify()`,
    /// which accepts a TOTAL forgery under a small-order (identity) authorizing key
    /// over ANY body: the verify equation [S]B = R + [H]*A degenerates to [S]B = R
    /// when A is the identity, so (R = identity, S = 0) verifies for every message.
    /// A hostile peer who lands a weak authorizing key (e.g. via an ungated
    /// registration door, #375 T3) thereby forges any certificate signature.
    /// RED until every hostile-peer site swaps `.verify` -> `verify_strict`
    /// (Taming the Many EdDSAs; ed25519-dalek's `verify_strict` rejects weak keys).
    #[test]
    fn permissive_verify_accepts_weak_key_forgery() -> Result<()> {
        // identity point (0, 1): y = 1 little-endian, sign bit 0.
        let mut identity_key = [0u8; 32];
        identity_key[0] = 1;
        // forged signature: R = identity, S = 0 (probe-confirmed permissive-accepted).
        let mut forged_sig = [0u8; 64];
        forged_sig[0] = 1;
        let weak = AuthorizingKey::mint_verifying(
            AuthorizingKeyId::from_digest([0xAA; 32]),
            identity_key,
        )?;
        assert!(
            !weak.verify_signature(
                &Digest::from_bytes([0x99u8; 32]),
                &Signature::from_bytes(forged_sig),
            ),
            "FORGERY ACCEPTED: verify_signature admits a signature under a small-order \
             (identity) authorizing key over an arbitrary body -- the permissive \
             .verify() must become verify_strict"
        );

        Ok(())
    }

    /// RFC 8032 ed25519 test vector 1 (empty message) — public verify golden.
    ///
    /// Public key + signature bytes are the RFC §7.1 TEST 1 constants. Origin
    /// mint uses the matching 32-byte seed via dalek; receiver verifies with
    /// public material only (cannot forge).
    #[test]
    fn rfc8032_vector1_public_key_and_empty_message() -> Result<()> {
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
        let vk = VerifyingKey::from_bytes(&expect_pk)?;
        let rfc_sig = Ed25519Signature::try_from(expect_sig.as_slice())?;
        assert!(
            vk.verify(b"", &rfc_sig).is_ok(),
            "RFC 8032 TEST 1 signature must verify under RFC public key"
        );
        let receiver = AuthorizingKey::mint_verifying([0xA1; 32], expect_pk)?;
        assert!(!receiver.can_sign());
        assert!(
            matches!(
                receiver.sign(&Digest::from_bytes([0u8; 32])),
                Err(ReplicaRefuse::AuthenticityFailed)
            ),
            "public-only key cannot forge"
        );

        // Origin mint: dalek seed→SigningKey; AuthorizingKey must expose the
        // same verifying bytes and round-trip sign/verify on a body digest.
        let dalek_pk = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
        let origin = AuthorizingKey::mint([0xA1; 32], seed);
        assert_eq!(origin.verifying_bytes(), dalek_pk);
        assert!(origin.can_sign());
        let body = Digest::from_bytes([0x5eu8; 32]);
        let sig = origin.sign(&body)?;
        assert!(origin.verify_signature(&body, &sig));
        assert!(
            AuthorizingKey::mint_verifying([0xA1; 32], dalek_pk)?.verify_signature(&body, &sig),
            "receiver with public-only table material verifies origin sig"
        );
        // RFC public key must not verify a signature under the dalek-derived
        // key from this seed if they diverge — document the installed dalek
        // wire; RFC vector above remains the public-verify golden.

        Ok(())
    }

    #[test]
    fn mint_with_verifying_id_is_public_id_not_seed() -> Result<()> {
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
        let body = Digest::from_bytes([0xABu8; 32]);
        let sig = origin.sign(&body)?;
        let mut table = AuthorizingKeyTable::new();
        table.insert(origin.clone());
        let looked = table.lookup(&origin.id())?;
        assert!(!looked.can_sign());
        assert!(looked.verify_signature(&body, &sig));

        Ok(())
    }

    #[test]
    fn sign_verify_round_trip_and_table_stores_public_only() -> Result<()> {
        let seed = [0x42u8; 32];
        let id = AuthorizingKeyId::from_digest([0x11; 32]);
        let origin = AuthorizingKey::mint(id, seed);
        let body = Digest::from_bytes([0xCAu8; 32]);
        let sig = origin.sign(&body)?;
        assert_eq!(sig.as_bytes().len(), 64);
        assert!(origin.verify_signature(&body, &sig));

        let mut table = AuthorizingKeyTable::new();
        table.insert(origin.clone());
        let looked = table.lookup(&id)?;
        assert!(
            !looked.can_sign(),
            "table must not reconstitute signing seed"
        );
        assert_eq!(looked.verifying_bytes(), origin.verifying_bytes());
        assert!(
            looked.verify_signature(&body, &sig),
            "receiver verifies with public key only"
        );
        assert!(
            matches!(looked.sign(&body), Err(ReplicaRefuse::AuthenticityFailed)),
            "receiver without seed cannot forge"
        );

        Ok(())
    }

    #[test]
    fn wrong_key_and_flipped_byte_fail_verify() -> Result<()> {
        let body = Digest::from_bytes([0xEEu8; 32]);
        let origin = AuthorizingKey::mint([0x01; 32], [0x7Au8; 32]);
        let sig = origin.sign(&body)?;
        assert!(origin.verify_signature(&body, &sig));

        // Flipped signature byte → refuse.
        let mut flipped = *sig.as_bytes();
        flipped[0] ^= 0x01;
        let flipped_sig = Signature::from_bytes(flipped);
        assert!(!origin.verify_signature(&body, &flipped_sig));
        assert!(origin.verify_signature(&body, &sig));

        // Wrong verifying key → refuse.
        let other = AuthorizingKey::mint([0x02; 32], [0x7Bu8; 32]);
        assert!(!other.verify_signature(&body, &sig));

        // Table with wrong public key → verify fails.
        let mut table = AuthorizingKeyTable::new();
        table.insert(other.authorizing_public()?);
        let looked = table.lookup(&AuthorizingKeyId::from_digest([0x02; 32]))??;
        assert!(!looked.verify_signature(&body, &sig));

        Ok(())
    }

    impl AuthorizingKey {
        /// Test helper: public-only clone (same as table insert/lookup shape).
        fn authorizing_public(&self) -> Result<AuthorizingKey> {
            AuthorizingKey::mint_verifying(self.id, self.verifying_bytes()).into_diagnostic()
        }
    }
}

#[cfg(test)]
mod crossing_contract_tests {
    use super::*;
    use miette::{IntoDiagnostic, Result, miette};

    fn mint_signed(
        key: &AuthorizingKey,
        scope: ScopeManifestDigest,
        schema_cut: [u8; 32],
    ) -> Result<AdmissionCertificate> {
        let store = StoreId::from_digest([0xC7; 32]);
        let mut parts = AdmissionCertificateParts {
            protocol_version: *b"kyzo.v01",
            origin_store: store,
            origin_epoch: FenceEpoch::genesis(store),
            origin_commit: CommitOrdinal::ZERO,
            schema_cut,
            record_digest: [0xE1; 32],
            predecessor_history_digest: [0x52; 32],
            post_state_root: PostStateRoot::from_digest([0x53; 32]),
            authorizing_key_id: key.id(),
            scope_manifest_digest: scope,
            operation_key: None,
            signature: Signature::from_bytes([0u8; 64]),
        };
        parts.signature = sign_admission_parts(&parts, key).into_diagnostic()?;
        mint_admission_certificate(parts).into_diagnostic()
    }

    fn matching_envelope(cert: &AdmissionCertificate) -> CrossingEnvelope {
        CrossingEnvelope::new(
            CrossingKind::Claim,
            *cert.protocol_version(),
            *cert.schema_cut(),
            cert.authorizing_key_id(),
            CrossingContext::Unscoped,
            CrossingEvidenceDemand::DeclaredRequired,
            CrossingEvidence::Present([0xEE; 32]),
            CrossingStatus::Active,
            CrossingCapabilitySet::new(),
        )
    }

    #[test]
    fn missing_declared_evidence_typed_refuse() -> Result<()> {
        let key = AuthorizingKey::mint_with_verifying_id([0xC1; 32]);
        let scope = ScopeManifestDigest::from_digest([0x5C; 32]);
        let mut keys = AuthorizingKeyTable::new();
        keys.insert(key.clone());
        let mut scopes = ScopeManifestTable::new();
        scopes.set(scope, ScopeManifestStatus::Verified);
        let cert = mint_signed(&key, scope, [0x51; 32])?;
        let local = StoreId::from_digest([0xD1; 32]);

        let envelope = CrossingEnvelope::new(
            CrossingKind::Claim,
            *cert.protocol_version(),
            *cert.schema_cut(),
            cert.authorizing_key_id(),
            CrossingContext::Unscoped,
            CrossingEvidenceDemand::DeclaredRequired,
            CrossingEvidence::Absent,
            CrossingStatus::Active,
            CrossingCapabilitySet::new(),
        );

        assert_eq!(
            validate_crossing_before_lower(
                &cert,
                &envelope,
                local,
                CommitOrdinal::ZERO,
                &keys,
                &scopes,
                Some(&OriginContinuity::mint()),
                &CrossingCapabilitySet::new(),
            ),
            Err(CrossingRefuse::DeclaredEvidenceMissing),
            "missing declared evidence must typed-refuse, not silent drop"
        );

        Ok(())
    }

    #[test]
    fn scope_unknown_revoked_denied_distinct_from_retention_declined() -> Result<()> {
        let key = AuthorizingKey::mint_with_verifying_id([0xC2; 32]);
        let scope = ScopeManifestDigest::from_digest([0x5D; 32]);
        let mut keys = AuthorizingKeyTable::new();
        keys.insert(key.clone());
        let cert = mint_signed(&key, scope, [0x51; 32])?;
        let local = StoreId::from_digest([0xD2; 32]);
        let envelope = matching_envelope(&cert);
        let held = CrossingCapabilitySet::new();

        let empty_scopes = ScopeManifestTable::new();
        assert_eq!(
            validate_crossing_before_lower(
                &cert,
                &envelope,
                local,
                CommitOrdinal::ZERO,
                &keys,
                &empty_scopes,
                Some(&OriginContinuity::mint()),
                &held,
            ),
            Err(CrossingRefuse::Replica(ReplicaRefuse::ScopeUnknown))
        );

        let mut revoked = ScopeManifestTable::new();
        revoked.set(scope, ScopeManifestStatus::Revoked);
        assert_eq!(
            validate_crossing_before_lower(
                &cert,
                &envelope,
                local,
                CommitOrdinal::ZERO,
                &keys,
                &revoked,
                Some(&OriginContinuity::mint()),
                &held,
            ),
            Err(CrossingRefuse::Replica(ReplicaRefuse::ScopeRevoked))
        );

        let mut denied = ScopeManifestTable::new();
        denied.set(scope, ScopeManifestStatus::Incompatible);
        assert_eq!(
            validate_crossing_before_lower(
                &cert,
                &envelope,
                local,
                CommitOrdinal::ZERO,
                &keys,
                &denied,
                Some(&OriginContinuity::mint()),
                &held,
            ),
            Err(CrossingRefuse::Replica(ReplicaRefuse::ScopeDenied))
        );

        assert_ne!(
            ReplicaRefuse::ScopeUnknown,
            ReplicaRefuse::RetentionDeclined
        );
        assert_ne!(
            ReplicaRefuse::ScopeRevoked,
            ReplicaRefuse::RetentionDeclined
        );
        assert_ne!(ReplicaRefuse::ScopeDenied, ReplicaRefuse::RetentionDeclined);
        assert_ne!(
            CrossingRefuse::DeclaredEvidenceMissing,
            CrossingRefuse::Replica(ReplicaRefuse::RetentionDeclined)
        );

        Ok(())
    }

    #[test]
    fn full_contract_validates_before_lower_token() -> Result<()> {
        let key = AuthorizingKey::mint_with_verifying_id([0xC3; 32]);
        let scope = ScopeManifestDigest::from_digest([0x5E; 32]);
        let mut keys = AuthorizingKeyTable::new();
        keys.insert(key.clone());
        let mut scopes = ScopeManifestTable::new();
        scopes.set(scope, ScopeManifestStatus::Verified);
        let cert = mint_signed(&key, scope, [0x51; 32])?;
        let local = StoreId::from_digest([0xD3; 32]);
        let mut claimed = CrossingCapabilitySet::new();
        claimed.insert([0xCA; 32]);
        let envelope = CrossingEnvelope::new(
            CrossingKind::Claim,
            *cert.protocol_version(),
            *cert.schema_cut(),
            cert.authorizing_key_id(),
            CrossingContext::Scoped([0xC0; 32]),
            CrossingEvidenceDemand::DeclaredRequired,
            CrossingEvidence::Present([0xEE; 32]),
            CrossingStatus::Active,
            claimed.clone(),
        );
        let mut held = CrossingCapabilitySet::new();
        held.insert([0xCA; 32]);

        let validated = validate_crossing_before_lower(
            &cert,
            &envelope,
            local,
            CommitOrdinal::ZERO,
            &keys,
            &scopes,
            Some(&OriginContinuity::mint()),
            &held,
        )?;
        assert_eq!(validated.kind(), CrossingKind::Claim);
        assert_eq!(validated.origin_schema_cut(), cert.schema_cut());
        assert!(matches!(
            validated.custody(),
            ReplicaCustody::Queryable { .. }
        ));

        assert_eq!(
            validate_crossing_before_lower(
                &cert,
                &envelope,
                local,
                CommitOrdinal::ZERO,
                &keys,
                &scopes,
                Some(&OriginContinuity::mint()),
                &CrossingCapabilitySet::new(),
            ),
            Err(CrossingRefuse::CapabilityMissing)
        );

        let tombstoned = CrossingEnvelope::new(
            CrossingKind::Claim,
            *cert.protocol_version(),
            *cert.schema_cut(),
            cert.authorizing_key_id(),
            CrossingContext::Unscoped,
            CrossingEvidenceDemand::NotRequired,
            CrossingEvidence::Absent,
            CrossingStatus::Tombstoned,
            CrossingCapabilitySet::new(),
        );
        assert_eq!(
            validate_crossing_before_lower(
                &cert,
                &tombstoned,
                local,
                CommitOrdinal::ZERO,
                &keys,
                &scopes,
                Some(&OriginContinuity::mint()),
                &CrossingCapabilitySet::new(),
            ),
            Err(CrossingRefuse::StatusNotLowerable)
        );

        assert_eq!(
            refuse_in_place_local_reinterpretation(),
            CrossingRefuse::LocalReinterpretationUnconstructible
        );

        Ok(())
    }

    #[test]
    fn schema_authority_mismatch_refuse() -> Result<()> {
        let key = AuthorizingKey::mint_with_verifying_id([0xC4; 32]);
        let scope = ScopeManifestDigest::from_digest([0x5F; 32]);
        let mut keys = AuthorizingKeyTable::new();
        keys.insert(key.clone());
        let mut scopes = ScopeManifestTable::new();
        scopes.set(scope, ScopeManifestStatus::Verified);
        let cert = mint_signed(&key, scope, [0x51; 32])?;
        let local = StoreId::from_digest([0xD4; 32]);

        let bad_schema = CrossingEnvelope::new(
            CrossingKind::Entity,
            *b"bad.vers",
            *cert.schema_cut(),
            cert.authorizing_key_id(),
            CrossingContext::Unscoped,
            CrossingEvidenceDemand::NotRequired,
            CrossingEvidence::Absent,
            CrossingStatus::Active,
            CrossingCapabilitySet::new(),
        );
        assert_eq!(
            validate_crossing_before_lower(
                &cert,
                &bad_schema,
                local,
                CommitOrdinal::ZERO,
                &keys,
                &scopes,
                Some(&OriginContinuity::mint()),
                &CrossingCapabilitySet::new(),
            ),
            Err(CrossingRefuse::SchemaVersionMismatch)
        );

        let bad_cut = CrossingEnvelope::new(
            CrossingKind::Entity,
            *cert.protocol_version(),
            [0xFF; 32],
            cert.authorizing_key_id(),
            CrossingContext::Unscoped,
            CrossingEvidenceDemand::NotRequired,
            CrossingEvidence::Absent,
            CrossingStatus::Active,
            CrossingCapabilitySet::new(),
        );
        assert_eq!(
            validate_crossing_before_lower(
                &cert,
                &bad_cut,
                local,
                CommitOrdinal::ZERO,
                &keys,
                &scopes,
                Some(&OriginContinuity::mint()),
                &CrossingCapabilitySet::new(),
            ),
            Err(CrossingRefuse::SchemaCutMismatch)
        );

        let bad_auth = CrossingEnvelope::new(
            CrossingKind::Entity,
            *cert.protocol_version(),
            *cert.schema_cut(),
            AuthorizingKeyId::from_digest([0x00; 32]),
            CrossingContext::Unscoped,
            CrossingEvidenceDemand::NotRequired,
            CrossingEvidence::Absent,
            CrossingStatus::Active,
            CrossingCapabilitySet::new(),
        );
        assert_eq!(
            validate_crossing_before_lower(
                &cert,
                &bad_auth,
                local,
                CommitOrdinal::ZERO,
                &keys,
                &scopes,
                Some(&OriginContinuity::mint()),
                &CrossingCapabilitySet::new(),
            ),
            Err(CrossingRefuse::AuthorityMismatch)
        );

        assert_eq!(
            CrossingKind::from_wire(99),
            Err(CrossingRefuse::KindInvalid)
        );
        assert_eq!(
            CrossingContext::from_wire(2, None),
            Err(CrossingRefuse::ContextInvalid)
        );

        Ok(())
    }
}

/// Non-collision + custody-idempotency proofs (#270 T2 / seat 70).
///
/// Board Check filters `replica::identity` — keep this module name stable.
#[cfg(test)]
mod identity {
    use super::*;
    use miette::{IntoDiagnostic, Result};

    fn mint_signed(
        origin_store: StoreId,
        origin_epoch: FenceEpoch,
        origin_commit: CommitOrdinal,
        record_digest: [u8; 32],
        key: &AuthorizingKey,
        scope: ScopeManifestDigest,
    ) -> Result<AdmissionCertificate> {
        let mut parts = AdmissionCertificateParts {
            protocol_version: *b"kyzo.v01",
            origin_store,
            origin_epoch,
            origin_commit,
            schema_cut: [0x51; 32],
            record_digest,
            predecessor_history_digest: [0x52; 32],
            post_state_root: PostStateRoot::from_digest([0x53; 32]),
            authorizing_key_id: key.id(),
            scope_manifest_digest: scope,
            operation_key: None,
            signature: Signature::from_bytes([0u8; 64]),
        };
        parts.signature = sign_admission_parts(&parts, key).into_diagnostic()?;
        mint_admission_certificate(parts).into_diagnostic()
    }

    #[test]
    fn distinct_origins_never_collapse_on_same_content() {
        // Same local id + content; only origin authority differs → identities diverge.
        let content = [0xE1; 32];
        let local_id = content; // admission law: RecordId is a view of content
        let tenant = TenantId::from_digest([0x7E; 32]);
        let auth_a = AuthorizingKeyId::from_digest([0xA1; 32]);
        let auth_b = AuthorizingKeyId::from_digest([0xB2; 32]);

        let a = NamespacedRecordIdentity::bind(local_id, auth_a, tenant, content);
        let b = NamespacedRecordIdentity::bind(local_id, auth_b, tenant, content);

        assert_ne!(
            a, b,
            "distinct origin authorities must not collapse into one identity"
        );
        assert_ne!(a.digest(), b.digest());
        assert_eq!(a.content(), b.content());
        assert_eq!(a.local_id(), b.local_id());
        assert_eq!(a.tenant(), b.tenant());
    }

    #[test]
    fn distinct_tenants_never_collapse_on_same_content() {
        let content = [0xE1; 32];
        let local_id = content;
        let auth = AuthorizingKeyId::from_digest([0xA1; 32]);
        let tenant_a = TenantId::from_digest([0x71; 32]);
        let tenant_b = TenantId::from_digest([0x72; 32]);

        let a = NamespacedRecordIdentity::bind(local_id, auth, tenant_a, content);
        let b = NamespacedRecordIdentity::bind(local_id, auth, tenant_b, content);

        assert_ne!(a, b, "distinct tenants must not collapse into one identity");
        assert_ne!(a.digest(), b.digest());
    }

    #[test]
    fn distinct_local_ids_never_collapse_on_same_content() {
        let content = [0xE1; 32];
        let auth = AuthorizingKeyId::from_digest([0xA1; 32]);
        let tenant = TenantId::from_digest([0x7E; 32]);

        let a = NamespacedRecordIdentity::bind([0x01; 32], auth, tenant, content);
        let b = NamespacedRecordIdentity::bind([0x02; 32], auth, tenant, content);

        assert_ne!(
            a, b,
            "distinct local ids must not collapse into one identity"
        );
        assert_ne!(a.digest(), b.digest());
    }

    #[test]
    fn same_namespace_tuple_is_equal_and_stable() {
        let content = [0xE1; 32];
        let auth = AuthorizingKeyId::from_digest([0xA1; 32]);
        let tenant = TenantId::from_digest([0x7E; 32]);
        let a = NamespacedRecordIdentity::bind(content, auth, tenant, content);
        let b = NamespacedRecordIdentity::bind(content, auth, tenant, content);
        assert_eq!(a, b);
        assert_eq!(a.digest(), b.digest());
    }

    #[test]
    fn certificate_bind_carries_authority_and_content() -> Result<()> {
        let store = StoreId::from_digest([0xC7; 32]);
        let key = AuthorizingKey::mint_with_verifying_id([0xC1; 32]);
        let scope = ScopeManifestDigest::from_digest([0x5C; 32]);
        let content = [0xE1; 32];
        let cert = mint_signed(
            store,
            FenceEpoch::genesis(store),
            CommitOrdinal::ZERO,
            content,
            &key,
            scope,
        )?;
        let tenant = TenantId::from_digest([0x7E; 32]);
        let id = NamespacedRecordIdentity::from_certificate(content, &cert, tenant);
        assert_eq!(id.origin_authority(), key.id());
        assert_eq!(id.content(), &content);
        assert_eq!(id.tenant(), tenant);
        assert_eq!(id.local_id(), &content);
        Ok(())
    }

    #[test]
    fn distinct_origin_stores_yield_distinct_replica_keys() {
        let content = [0xE1; 32];
        let store_a = StoreId::from_digest([0x01; 32]);
        let store_b = StoreId::from_digest([0x02; 32]);
        let epoch_a = FenceEpoch::genesis(store_a);
        let epoch_b = FenceEpoch::genesis(store_b);
        let key_a = ReplicaKey::derive(store_a, epoch_a, CommitOrdinal::ZERO, &content);
        let key_b = ReplicaKey::derive(store_b, epoch_b, CommitOrdinal::ZERO, &content);
        assert_ne!(
            key_a, key_b,
            "same content under distinct origins must not share ReplicaKey"
        );
    }

    #[test]
    fn custody_key_delegates_to_replica_key_authority() {
        let content = [0xE1; 32];
        let store = StoreId::from_digest([0xC7; 32]);
        let epoch = FenceEpoch::genesis(store);
        let id = NamespacedRecordIdentity::bind(
            content,
            AuthorizingKeyId::from_digest([0xA1; 32]),
            TenantId::from_digest([0x7E; 32]),
            content,
        );
        assert_eq!(
            id.custody_key(store, epoch, CommitOrdinal::ZERO),
            ReplicaKey::derive(store, epoch, CommitOrdinal::ZERO, &content),
            "namespaced custody must extend ReplicaKey — not fork a second key"
        );
    }

    #[test]
    fn replica_key_custody_idempotent_under_duplicate_delivery() -> Result<()> {
        let origin = StoreId::from_digest([0x69; 32]);
        let local = StoreId::from_digest([0x70; 32]);
        let origin_epoch = FenceEpoch::genesis(origin);
        let origin_commit = CommitOrdinal::ZERO;
        let record_digest = [0xE1; 32];
        let key = AuthorizingKey::mint_with_verifying_id([0x69; 32]);
        let scope = ScopeManifestDigest::from_digest([0x5C; 32]);
        let mut keys = AuthorizingKeyTable::new();
        keys.insert(key.clone());
        let mut scopes = ScopeManifestTable::new();
        scopes.set(scope, ScopeManifestStatus::Verified);
        let continuity = OriginContinuity::mint();
        let cert = mint_signed(
            origin,
            origin_epoch,
            origin_commit,
            record_digest,
            &key,
            scope,
        )?;
        let expected = ReplicaKey::derive(origin, origin_epoch, origin_commit, &record_digest);
        let tenant = TenantId::from_digest([0x7E; 32]);
        let namespaced = NamespacedRecordIdentity::from_certificate(record_digest, &cert, tenant);
        assert_eq!(
            namespaced.custody_key(origin, origin_epoch, origin_commit),
            expected
        );

        let mut table = ReplicaCustodyTable::new();
        let mut first: Option<ReplicaCustody> = None;
        for delivery in 0..5 {
            let custody = match verify_replica(
                &cert,
                local,
                CommitOrdinal::ZERO,
                &keys,
                &scopes,
                Some(&continuity),
            ) {
                Ok(c) => c,
                Err(e) => {
                    assert!(false, "delivery {delivery}: verify_replica {e:?}");
                    return Ok(());
                }
            };
            assert_eq!(custody.key(), expected, "delivery {delivery}: ReplicaKey");
            let held = table.admit(custody.clone());
            assert_eq!(held.key(), expected);
            match &first {
                None => first = Some(custody),
                Some(prior) => assert_eq!(
                    prior, &custody,
                    "delivery {delivery}: duplicate delivery must converge"
                ),
            }
        }
        assert_eq!(
            table.len(),
            1,
            "five at-least-once deliveries → exactly one custody"
        );
        assert_eq!(table.get(&expected), first.as_ref());
        Ok(())
    }

    #[test]
    fn content_only_id_is_not_federation_identity() {
        // Two origins, identical content: content digests equal, namespaced ids do not.
        let content = [0xAA; 32];
        let a = NamespacedRecordIdentity::bind(
            content,
            AuthorizingKeyId::from_digest([0x01; 32]),
            TenantId::from_digest([0x10; 32]),
            content,
        );
        let b = NamespacedRecordIdentity::bind(
            content,
            AuthorizingKeyId::from_digest([0x02; 32]),
            TenantId::from_digest([0x10; 32]),
            content,
        );
        assert_eq!(a.content(), b.content());
        assert_ne!(a.digest(), b.digest());
        assert_ne!(a, b);
    }
}

/// Promotion replay equality + graph-bound key proofs (#270 T3).
#[cfg(test)]
mod promotion {
    use super::*;
    use miette::{IntoDiagnostic, Result, miette};

    fn sample_meaning(schema_cut: [u8; 32], provenance: [u8; 32]) -> PromotionMeaning {
        let identity = NamespacedRecordIdentity::bind(
            [0x11; 32],
            AuthorizingKeyId::from_digest([0xA1; 32]),
            TenantId::from_digest([0x7E; 32]),
            [0xC0; 32],
        );
        PromotionMeaning::bind(identity, [0x71; 32], provenance, schema_cut)
    }

    #[test]
    fn promotion_replay_equality_preserves_four_seats() -> Result<()> {
        let before = sample_meaning([0x51; 32], [0xB1; 32]);
        // Local-to-hosted: same meaning seats, different host — equality holds.
        let after = sample_meaning([0x51; 32], [0xB1; 32]);
        assert!(before.replay_equal(&after));
        assert_eq!(before.digest(), after.digest());
        assert_eq!(prove_promotion_replay(&before, &after), Ok(()));

        Ok(())
    }

    #[test]
    fn promotion_schema_divergence_refuses() -> Result<()> {
        let before = sample_meaning([0x51; 32], [0xB1; 32]);
        let after = sample_meaning([0x99; 32], [0xB1; 32]);
        assert!(!before.replay_equal(&after));
        assert_eq!(
            prove_promotion_replay(&before, &after),
            Err(PromotionRefuse::MeaningDiverged)
        );

        Ok(())
    }

    #[test]
    fn promotion_provenance_divergence_refuses() -> Result<()> {
        let before = sample_meaning([0x51; 32], [0xB1; 32]);
        let after = sample_meaning([0x51; 32], [0xB2; 32]);
        assert_eq!(
            prove_promotion_replay(&before, &after),
            Err(PromotionRefuse::MeaningDiverged)
        );

        Ok(())
    }

    #[test]
    fn view_under_schema_cut_consumes_origin_cut() -> Result<()> {
        let origin = [0x51u8; 32];
        assert_eq!(view_under_schema_cut(&origin, &origin), Ok(()));
        assert_eq!(
            view_under_schema_cut(&origin, &[0x99; 32]),
            Err(CrossingRefuse::LocalReinterpretationUnconstructible)
        );
        // LocalProjection is the lawful different-cut path — not in-place reshape.
        let key = AuthorizingKey::mint_with_verifying_id([0xC3; 32]);
        let scope = ScopeManifestDigest::from_digest([0x5E; 32]);
        let mut parts = AdmissionCertificateParts {
            protocol_version: *b"kyzo.v01",
            origin_store: StoreId::from_digest([0x01; 32]),
            origin_epoch: FenceEpoch::genesis(StoreId::from_digest([0x01; 32])),
            origin_commit: CommitOrdinal::ZERO,
            schema_cut: origin,
            record_digest: [0xAA; 32],
            predecessor_history_digest: [0x52; 32],
            post_state_root: PostStateRoot::from_digest([0x53; 32]),
            authorizing_key_id: key.id(),
            scope_manifest_digest: scope,
            operation_key: None,
            signature: Signature::from_bytes([0u8; 64]),
        };
        parts.signature = sign_admission_parts(&parts, &key)?;
        let cert = mint_admission_certificate(parts)?;
        let projection = LocalProjection::from_certificate(cert, [0x99; 32]);
        assert_ne!(
            projection.local_schema_cut(),
            projection.origin().schema_cut()
        );

        Ok(())
    }

    #[test]
    fn graph_bound_key_never_crosses_boundary() -> Result<()> {
        let home = GraphBoundary::from_tenant(TenantId::from_digest([0x7E; 32]));
        let foreign = GraphBoundary::from_tenant(TenantId::from_digest([0x7F; 32]));
        let key = ReplicaKey::derive(
            StoreId::from_digest([0x01; 32]),
            FenceEpoch::genesis(StoreId::from_digest([0x01; 32])),
            CommitOrdinal::ZERO,
            &[0xC0; 32],
        );
        let bound = GraphBoundKey::bind(home, &key);
        assert_eq!(bound.authorize(home), Ok(()));
        assert_eq!(
            bound.authorize(foreign),
            Err(KeyBoundaryRefuse::KeyCrossesGraphBoundary)
        );
        // Same digest under foreign graph is a different confinement — no bleed.
        let foreign_bound = GraphBoundKey::bind(foreign, &key);
        assert_ne!(bound, foreign_bound);
        assert_eq!(
            foreign_bound.authorize(home),
            Err(KeyBoundaryRefuse::KeyCrossesGraphBoundary)
        );

        Ok(())
    }
}

#[cfg(test)]
mod sth_gossip_obligation_tests {
    use super::*;
    use crate::store::merkle::{
        ChainLinkKind, ChainedStateRoot, GENESIS_ROOT, GossipConsistency, RootChain, StateRoot,
        StateRootHead, build_consistency_proof,
    };
    use miette::{IntoDiagnostic, Result, miette};

    /// Fabric carriage is JetStream-only — subject names the Store; no peer-dial.
    #[test]
    fn sth_gossip_obligation_seats_on_jetstream_not_peer_dial() -> Result<()> {
        let store = StoreId::from_digest([0x92; 32]);
        let obligation = SthGossipObligation::on_jetstream(store);
        assert!(matches!(
            obligation.carriage(),
            SthGossipCarriage::JetStream(_)
        ));
        let subject = obligation.subject().jetstream_subject();
        assert!(subject.starts_with("kyzo.sth."));
        assert_eq!(subject.len(), "kyzo.sth.".len() + 64);

        Ok(())
    }

    /// Signed STH gossip detects split-view before chains meet.
    #[test]
    fn enforce_sth_gossip_detects_equivocating_signed_heads() -> Result<()> {
        let store = StoreId::from_digest([0x69; 32]);
        let fence = FenceEpoch::genesis(store);
        let o1 = CommitOrdinal::ZERO.successor()?;
        let key = AuthorizingKey::mint_with_verifying_id([0x51; 32]);
        let mut keys = AuthorizingKeyTable::new();
        keys.insert(key.clone());

        let content_a = StateRoot::from_digest([0xA1; 32]);
        let content_b = StateRoot::from_digest([0xB2; 32]);
        let mut chain_a = RootChain::empty();
        chain_a.append(ChainedStateRoot::mint(
            store,
            fence,
            o1,
            content_a,
            GENESIS_ROOT,
            ChainLinkKind::Ordinary,
        ))?;
        let mut chain_b = RootChain::empty();
        chain_b.append(ChainedStateRoot::mint(
            store,
            fence,
            o1,
            content_b,
            GENESIS_ROOT,
            ChainLinkKind::Ordinary,
        ))?;

        let signed_a = SignedStateRootHead::sign(StateRootHead::from_chain_tip(&chain_a)?, &key)?;
        let signed_b = SignedStateRootHead::sign(StateRootHead::from_chain_tip(&chain_b)?, &key)?;
        assert_ne!(signed_a.head().root(), signed_b.head().root());

        let obligation = SthGossipObligation::on_jetstream(store);
        assert_eq!(
            enforce_sth_gossip(&obligation, &signed_a, &signed_b, None, &keys),
            Err(ReplicaRefuse::SplitViewDetected)
        );

        // Honest identical observations pass.
        assert_eq!(
            enforce_sth_gossip(&obligation, &signed_a, &signed_a, None, &keys),
            Ok(GossipConsistency::Identical)
        );

        Ok(())
    }

    /// Honest extension under consistency proof is Detected-consistent on gossip.
    #[test]
    fn enforce_sth_gossip_honest_extension_with_proof() -> Result<()> {
        let store = StoreId::from_digest([0x58; 32]);
        let fence = FenceEpoch::genesis(store);
        let o1 = CommitOrdinal::ZERO.successor()?;
        let o2 = o1.successor()?;
        let key = AuthorizingKey::mint_with_verifying_id([0x58; 32]);
        let mut keys = AuthorizingKeyTable::new();
        keys.insert(key.clone());

        let mut chain = RootChain::empty();
        chain.append(ChainedStateRoot::mint(
            store,
            fence,
            o1,
            StateRoot::from_digest([0x01; 32]),
            GENESIS_ROOT,
            ChainLinkKind::Ordinary,
        ))?;
        chain.append(ChainedStateRoot::mint(
            store,
            fence,
            o2,
            StateRoot::from_digest([0x02; 32]),
            chain.prior_root(),
            ChainLinkKind::Ordinary,
        ))?;

        let older = SignedStateRootHead::sign(StateRootHead::from_cut(&chain, o1)?, &key)?;
        let newer = SignedStateRootHead::sign(StateRootHead::from_cut(&chain, o2)?, &key)?;
        let proof = build_consistency_proof(&chain, o1, o2)?;
        let obligation = SthGossipObligation::on_jetstream(store);
        assert_eq!(
            enforce_sth_gossip(&obligation, &older, &newer, Some(&proof), &keys),
            Ok(GossipConsistency::ConsistentExtension)
        );

        Ok(())
    }
}
