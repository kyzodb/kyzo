/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Object seam: staging, permanence, durability classes, GC (decisions.md §16, §22, §23, §32, §76).
//!
//! Owns: [`ObjectRef`], [`StagingToken`], [`ObjectSlot`], [`VolatilePending`],
//! [`PermanenceCandidate`], [`PermanenceWitness`], [`StagingTtl`] (genesis-sealed
//! ordinal count from [`super::open`]), [`ObjectDurabilityClass`], [`Repair`],
//! [`Downgrade`], reclaim / retention certificates, object GC, closed
//! [`ObjectStore`] driver trait (put/get/delete/list).
//!
//! Bans: wall-clock expiry authority; class-less Durable; total-order class
//! ladders; confirm-then-strip; immortal candidates; orphan puts; direct
//! backend API calls bypassing the closed trait.
//!
//! Physical seam: `VolatilePending → PermanenceCandidate → Durable`.
//! Logical slot: `Pending(StagingToken) | Durable(ObjectRef)`.

use super::open::{StagingTtl, StoreId};
use super::sweep::CommitOrdinal;

/// Opaque object identity bytes within a Store scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId([u8; 32]);

impl ObjectId {
    /// Wrap an already-proven object identity digest.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Borrow the identity bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for ObjectId {
    fn from(digest: [u8; 32]) -> Self {
        Self(digest)
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Content hash of object bytes (plaintext-canonical).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Wrap an already-proven content hash.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Borrow the hash bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for ContentHash {
    fn from(digest: [u8; 32]) -> Self {
        Self(digest)
    }
}

/// Store-identity-prefixed durable object reference.
///
/// Cross-Store resolve → refuse (ObjectRefForeignStore). Bare keys without
/// Store prefix are Unconstructible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectRef {
    store_id: StoreId,
    object_id: ObjectId,
}

impl ObjectRef {
    /// Mint a ref scoped to `store_id` (admission / permanence confirm only).
    pub fn mint(store_id: StoreId, object_id: impl Into<ObjectId>) -> Self {
        Self {
            store_id,
            object_id: object_id.into(),
        }
    }

    /// Store scope.
    pub fn store_id(self) -> StoreId {
        self.store_id
    }

    /// Object id within the Store.
    pub fn object_id(self) -> ObjectId {
        self.object_id
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Store-identity-prefixed staging token for Pending slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StagingToken {
    store_id: StoreId,
    object_id: ObjectId,
}

impl StagingToken {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Mint a staging token scoped to `store_id`.
    pub(crate) fn mint(store_id: StoreId, object_id: impl Into<ObjectId>) -> Self {
        Self {
            store_id,
            object_id: object_id.into(),
        }
    }

    /// Store scope.
    pub fn store_id(self) -> StoreId {
        self.store_id
    }

    /// Object id within the Store.
    pub fn object_id(self) -> ObjectId {
        self.object_id
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Logical object naming: Pending or Durable.
///
/// Exactly these two arms — delete_meter requires `Pending | Durable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectSlot {
    /// Staged under ordinal StagingTTL cut law.
    Pending(StagingToken),
    /// Permanence confirmed under a sealed [`ObjectDurabilityClass`].
    Durable(ObjectRef),
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Physical first stage: volatile bytes not yet a PermanenceCandidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolatilePending {
    token: StagingToken,
    content_hash: ContentHash,
    /// `stage_commit + StagingTTL` — cut comparison only; never wall clock.
    expires_at: CommitOrdinal,
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
impl VolatilePending {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Stage bytes under a Store-scoped token with an ordinal expiry cut.
    pub(crate) fn stage(
        token: StagingToken,
        content_hash: ContentHash,
        stage_commit: CommitOrdinal,
        ttl: StagingTtl,
    ) -> Result<VolatilePending, ObjectRefuse> {
        let expires_at = add_ttl(stage_commit, ttl)?;
        Ok(VolatilePending {
            token,
            content_hash,
            expires_at,
        })
    }

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Staging token.
    pub fn token(&self) -> StagingToken {
        self.token
    }

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Content hash of staged bytes.
    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    /// Ordinal cut at which Pending decays (idle Stores never advance this).
    pub fn expires_at(&self) -> CommitOrdinal {
        self.expires_at
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Physical mid stage: permanence in flight; inherits `expires_at`.
///
/// Before cut: confirm or reclaim. At/past cut: reclaim only.
/// Ordinary read of a candidate is not licensed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermanenceCandidate {
    token: StagingToken,
    content_hash: ContentHash,
    expires_at: CommitOrdinal,
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
impl PermanenceCandidate {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Lift VolatilePending into a PermanenceCandidate (strip-before-confirm ban:
    /// confirm is a separate [`PermanenceWitness`] mint).
    pub(crate) fn from_volatile(pending: VolatilePending) -> Self {
        PermanenceCandidate {
            token: pending.token,
            content_hash: pending.content_hash,
            expires_at: pending.expires_at,
        }
    }

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Staging token (still Pending logically until Durable supersession).
    pub fn token(&self) -> StagingToken {
        self.token
    }

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Content hash.
    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    /// Inherited ordinal expiry cut.
    pub fn expires_at(&self) -> CommitOrdinal {
        self.expires_at
    }

    /// Whether the current cut still permits confirm (strictly before expires_at).
    pub fn may_confirm(&self, cut: CommitOrdinal) -> bool {
        cut.get() < self.expires_at.get()
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Product of sealed durability dimensions — never a total-order ladder.
///
/// Dominance = every dimension ≥. Soft `SingleCopy ≤ ReplicatedN ≤ CrossRegion`
/// ladders are deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectDurabilityClass {
    confirmed_copies: ConfirmedCopies,
    failure_domains: FailureDomains,
    regions: Regions,
    consistency: ConsistencyClass,
    integrity_verification: IntegrityVerification,
    backend_contract: BackendContract,
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
impl ObjectDurabilityClass {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Seal a product class from declared arm dimensions.
    pub fn new(
        confirmed_copies: ConfirmedCopies,
        failure_domains: FailureDomains,
        regions: Regions,
        consistency: ConsistencyClass,
        integrity_verification: IntegrityVerification,
        backend_contract: BackendContract,
    ) -> Self {
        Self {
            confirmed_copies,
            failure_domains,
            regions,
            consistency,
            integrity_verification,
            backend_contract,
        }
    }

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// True iff `self` dominates `other` on every dimension.
    pub fn dominates(self, other: ObjectDurabilityClass) -> bool {
        self.confirmed_copies >= other.confirmed_copies
            && self.failure_domains >= other.failure_domains
            && self.regions >= other.regions
            && self.consistency >= other.consistency
            && self.integrity_verification >= other.integrity_verification
            && self.backend_contract >= other.backend_contract
    }

    /// True when neither class dominates the other.
    pub fn incomparable(self, other: ObjectDurabilityClass) -> bool {
        !self.dominates(other) && !other.dominates(self)
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Sealed dimension: confirmed copy count class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConfirmedCopies {
    /// Single confirmed copy.
    One,
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Quorum / multi-copy within one failure domain.
    Quorum,
    /// Multi-site confirmed copies.
    MultiSite,
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Sealed dimension: failure-domain separation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FailureDomains {
    /// Single failure domain.
    Single,
    /// Distinct failure domains.
    Distinct,
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Sealed dimension: region placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Regions {
    /// Single region.
    Single,
    /// Multi-region.
    Multi,
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Sealed dimension: consistency contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConsistencyClass {
    /// Eventual visibility of confirmed copies.
    Eventual,
    /// Strong read-after-confirm.
    Strong,
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Sealed dimension: integrity verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IntegrityVerification {
    /// Content-hash verify on get.
    ContentHash,
    /// Content-hash plus periodic scrub.
    HashAndScrub,
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Sealed dimension: backend contract arm identity (opaque digest).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BackendContract([u8; 32]);

impl BackendContract {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Bind an arm identity digest.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Arm digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Witness that staged bytes became Durable under a sealed class.
///
/// `mint` is constructor-guarded Unconstructible when `cut ≥ expires_at`.
/// Repair requires dominating class (or explicit [`Downgrade`]) and
/// content-hash-identical bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermanenceWitness {
    object_ref: ObjectRef,
    content_hash: ContentHash,
    class: ObjectDurabilityClass,
    confirmed_at: CommitOrdinal,
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
impl PermanenceWitness {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Confirm a candidate before its cut under `class`.
    pub(crate) fn mint(
        candidate: &PermanenceCandidate,
        cut: CommitOrdinal,
        class: ObjectDurabilityClass,
    ) -> Result<PermanenceWitness, ObjectRefuse> {
        if cut.get() >= candidate.expires_at.get() {
            return Err(ObjectRefuse::Decayed);
        }
        let object_ref = ObjectRef::mint(candidate.token.store_id(), candidate.token.object_id());
        Ok(PermanenceWitness {
            object_ref,
            content_hash: candidate.content_hash,
            class,
            confirmed_at: cut,
        })
    }

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Seal a permanence witness from already-proven parts (campaign / trust door).
    ///
    /// Does not re-check StagingTTL — that law lives on [`PermanenceWitness::mint`].
    pub fn from_sealed(
        object_ref: ObjectRef,
        content_hash: ContentHash,
        class: ObjectDurabilityClass,
        confirmed_at: CommitOrdinal,
    ) -> Self {
        Self {
            object_ref,
            content_hash,
            class,
            confirmed_at,
        }
    }

    /// Repair path: re-stage content-hash-verified identical bytes under a
    /// dominating class (or after explicit Downgrade).
    ///
    /// Incomparable proposal → [`ObjectRefuse::IncomparableClasses`] carrying
    /// both classes (seat 22) — never a panic.
    pub fn repair(
        original: &PermanenceWitness,
        bytes_hash: ContentHash,
        proposed: ObjectDurabilityClass,
        downgrade: Option<Downgrade>,
    ) -> Result<Repair, ObjectRefuse> {
        if bytes_hash != original.content_hash {
            return Err(ObjectRefuse::RepairBytesMismatch);
        }
        let class = match downgrade {
            Some(d) => {
                if d.from != original.class || d.to != proposed {
                    return Err(ObjectRefuse::DowngradeMismatch);
                }
                d.to
            }
            None => {
                if proposed.incomparable(original.class) {
                    return Err(ObjectRefuse::IncomparableClasses {
                        original: original.class,
                        proposed,
                    });
                }
                if !proposed.dominates(original.class) {
                    return Err(ObjectRefuse::NonDominatingRepair);
                }
                proposed
            }
        };
        Ok(Repair {
            object_ref: original.object_ref,
            content_hash: original.content_hash,
            class,
            prior_class: original.class,
            downgrade,
        })
    }

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Durable ref sealed by this witness.
    pub fn object_ref(&self) -> ObjectRef {
        self.object_ref
    }

    /// Content hash.
    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Sealed durability class product.
    pub fn class(&self) -> ObjectDurabilityClass {
        self.class
    }

    /// Commit ordinal at confirm.
    pub fn confirmed_at(&self) -> CommitOrdinal {
        self.confirmed_at
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Auditable append-only class supersession (never silent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Downgrade {
    /// Class being superseded.
    pub from: ObjectDurabilityClass,
    /// Explicit lower/incomparable successor class.
    pub to: ObjectDurabilityClass,
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Repair outcome under PermanenceWitness law.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repair {
    object_ref: ObjectRef,
    content_hash: ContentHash,
    class: ObjectDurabilityClass,
    prior_class: ObjectDurabilityClass,
    downgrade: Option<Downgrade>,
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
impl Repair {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Object under repair.
    pub fn object_ref(&self) -> ObjectRef {
        self.object_ref
    }

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Verified content hash (must match original).
    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    /// Class after repair / Downgrade.
    pub fn class(&self) -> ObjectDurabilityClass {
        self.class
    }

    /// Prior sealed class.
    pub fn prior_class(&self) -> ObjectDurabilityClass {
        self.prior_class
    }

    /// Explicit Downgrade when present.
    pub fn downgrade(&self) -> Option<Downgrade> {
        self.downgrade
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Capability-decided reclaim of unconfirmed staged objects (VolatilePending
/// or PermanenceCandidate). Always lawful idle or busy — never wall clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReclaimCertificate {
    store_id: StoreId,
    object_id: ObjectId,
    digest: [u8; 32],
}

impl ReclaimCertificate {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Mint a reclaim certificate for a staged object.
    pub(crate) fn mint(store_id: StoreId, object_id: ObjectId, digest: [u8; 32]) -> Self {
        Self {
            store_id,
            object_id,
            digest,
        }
    }

    /// Store scope.
    pub fn store_id(self) -> StoreId {
        self.store_id
    }

    /// Object id.
    pub fn object_id(self) -> ObjectId {
        self.object_id
    }

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Certificate digest.
    pub fn digest(self) -> [u8; 32] {
        self.digest
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Retention certificate gating Durable GC only (never Pending).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RetentionCertificate {
    store_id: StoreId,
    /// Max retaining snapshot / as-of cut covered.
    covers_through: CommitOrdinal,
    digest: [u8; 32],
}

impl RetentionCertificate {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Mint a retention certificate covering as-of obligations through `cut`.
    pub(crate) fn mint(store_id: StoreId, covers_through: CommitOrdinal, digest: [u8; 32]) -> Self {
        Self {
            store_id,
            covers_through,
            digest,
        }
    }

    /// Store scope.
    pub fn store_id(self) -> StoreId {
        self.store_id
    }

    /// Covered through this cut.
    pub fn covers_through(self) -> CommitOrdinal {
        self.covers_through
    }

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Certificate digest.
    pub fn digest(self) -> [u8; 32] {
        self.digest
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Resolve a logical slot against the current cut / Store scope.
pub fn resolve(
    slot: ObjectSlot,
    calling_store: StoreId,
    cut: CommitOrdinal,
) -> Result<ResolvedObject, ObjectRefuse> {
    match slot {
        ObjectSlot::Pending(token) => {
            if token.store_id() != calling_store {
                return Err(ObjectRefuse::ObjectRefForeignStore);
            }
            // Expiry authority is cut comparison at the holder; Pending
            // without a live staged record past cut → Decayed at the door
            // that holds expires_at. Here we only scope-check the token.
            let _ = cut;
            Ok(ResolvedObject::Pending(token))
        }
        ObjectSlot::Durable(object_ref) => {
            if object_ref.store_id() != calling_store {
                return Err(ObjectRefuse::ObjectRefForeignStore);
            }
            Ok(ResolvedObject::Durable(object_ref))
        }
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Resolve outcome (Pending licenses ref+expiry; Durable licenses class).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResolvedObject {
    /// Pending — client handles Decayed / ObjectMissing on materialize.
    Pending(StagingToken),
    /// Durable — client still handles corruption/unavailability.
    Durable(ObjectRef),
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Store-owned GC for Durable bytes under a covering retention certificate.
pub fn gc_durable(
    object_ref: ObjectRef,
    retention: &RetentionCertificate,
    max_retaining_snapshot: CommitOrdinal,
) -> Result<(), ObjectRefuse> {
    if retention.store_id() != object_ref.store_id() {
        return Err(ObjectRefuse::ObjectRefForeignStore);
    }
    if retention.covers_through().get() < max_retaining_snapshot.get() {
        return Err(ObjectRefuse::ObjectRetainRequired);
    }
    Ok(())
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Reclaim a PermanenceCandidate (lawful any time) or VolatilePending.
pub fn reclaim_candidate(
    candidate: PermanenceCandidate,
    certificate: &ReclaimCertificate,
) -> Result<(), ObjectRefuse> {
    if certificate.store_id() != candidate.token.store_id()
        || certificate.object_id() != candidate.token.object_id()
    {
        return Err(ObjectRefuse::ReclaimMismatch);
    }
    Ok(())
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Closed object-store driver trait — put/get/delete/list only.
///
/// Direct backend API calls bypassing this trait are banned (§15).
/// Orphan put without a prior committed ObjectSlot/Record naming is
/// Unconstructible at the Store door (federation cannot invent refs).
pub trait ObjectStore {
    /// Driver I/O / capacity refuse.
    type Error;

    /// Put bytes already named by a committed ObjectSlot / Record.
    fn put(&mut self, object_id: &ObjectId, bytes: &[u8]) -> Result<(), Self::Error>;

    /// Get bytes by object id.
    fn get(&self, object_id: &ObjectId) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Delete bytes by object id (GC / reclaim only through Store doors).
    fn delete(&mut self, object_id: &ObjectId) -> Result<(), Self::Error>;

    /// List object ids under this backend namespace.
    fn list(&self) -> Result<Vec<ObjectId>, Self::Error>;
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Typed refusals on the object seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum ObjectRefuse {
    /// Resolve of Pending past its cut.
    #[error("object: Decayed — Pending past expires_at cut")]
    #[diagnostic(code(store::objects::decayed))]
    Decayed,
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Object bytes gone while cut still live.
    #[error("object: ObjectMissing — bytes gone while cut live")]
    #[diagnostic(code(store::objects::object_missing))]
    ObjectMissing,
    /// Durable delete without covering retention certificate.
    #[error("object: ObjectRetainRequired — retention certificate does not cover snapshot")]
    #[diagnostic(code(store::objects::object_retain_required))]
    ObjectRetainRequired,
    /// Cross-Store object resolution.
    #[error("object: ObjectRefForeignStore — ref/token Store scope mismatch")]
    #[diagnostic(code(store::objects::object_ref_foreign_store))]
    ObjectRefForeignStore,
    /// As-of naming a Durable whose retention was violated.
    #[error("object: ObjectMissingForAsOf — retention violated at as-of")]
    #[diagnostic(code(store::objects::object_missing_for_as_of))]
    ObjectMissingForAsOf,
    /// Incomparable ObjectDurabilityClass on Repair.
    #[error("object: incomparable durability classes on Repair")]
    #[diagnostic(code(store::objects::incomparable_classes))]
    IncomparableClasses {
        /// Original sealed class.
        original: ObjectDurabilityClass,
        /// Proposed class.
        proposed: ObjectDurabilityClass,
    },
    /// Non-dominating Repair without Downgrade.
    #[error("object: non-dominating Repair without Downgrade")]
    #[diagnostic(code(store::objects::non_dominating_repair))]
    NonDominatingRepair,
    /// Repair bytes not content-hash identical.
    #[error("object: Repair bytes do not match sealed content hash")]
    #[diagnostic(code(store::objects::repair_bytes_mismatch))]
    RepairBytesMismatch,
    /// Downgrade record does not match from/to classes.
    #[error("object: Downgrade record mismatch")]
    #[diagnostic(code(store::objects::downgrade_mismatch))]
    DowngradeMismatch,
    /// Reclaim certificate does not name the candidate.
    #[error("object: reclaim certificate mismatch")]
    #[diagnostic(code(store::objects::reclaim_mismatch))]
    ReclaimMismatch,
    /// StagingTTL ordinal overflow.
    #[error("object: StagingTTL ordinal overflow")]
    #[diagnostic(code(store::objects::staging_ttl_overflow))]
    StagingTtlOverflow,
    /// Object backend full.
    #[error("object: ObjectBackendFull")]
    #[diagnostic(code(store::objects::object_backend_full))]
    ObjectBackendFull,
    /// ObjectRef resolution from a deleted Store.
    #[error("object: StoreDeleted")]
    #[diagnostic(code(store::objects::store_deleted))]
    StoreDeleted,
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
fn add_ttl(stage: CommitOrdinal, ttl: StagingTtl) -> Result<CommitOrdinal, ObjectRefuse> {
    stage
        .get()
        .checked_add(ttl.ordinals())
        .map(CommitOrdinal::from_raw)
        .ok_or(ObjectRefuse::StagingTtlOverflow)
}

#[cfg(test)]
mod durability_dominance_tests {
    use super::*;
    use crate::store::commit_cap::SnapshotFork;
    use crate::store::open::{EntropyArm, GenesisParams, SizeClass, StableCommitCapArm, genesis};

    fn sample_store() -> StoreId {
        genesis(GenesisParams {
            identity_seed: [0x22; 32],
            recovery_matrix: None,
            staging_ttl: StagingTtl::new(1_024),
            size_class: SizeClass::Compact,
            entropy_arm: EntropyArm::OsRandom,
            stable_commit_cap: StableCommitCapArm::NativeFsyncProof {
                snapshot_fork: SnapshotFork::No,
            },
        })
        .store_id()
    }

    #[test]
    fn incomparable_repair_is_typed_refuse_carrying_both_classes() {
        let backend = BackendContract::from_digest([0xBC; 32]);
        // True seat-22 incomparable pair: more copies vs more failure domains.
        let more_copies = ObjectDurabilityClass::new(
            ConfirmedCopies::MultiSite,
            FailureDomains::Single,
            Regions::Single,
            ConsistencyClass::Eventual,
            IntegrityVerification::ContentHash,
            backend,
        );
        let more_domains = ObjectDurabilityClass::new(
            ConfirmedCopies::One,
            FailureDomains::Distinct,
            Regions::Single,
            ConsistencyClass::Eventual,
            IntegrityVerification::ContentHash,
            backend,
        );
        assert!(more_copies.incomparable(more_domains));

        let hash = ContentHash::from_digest([0xCC; 32]);
        let witness = PermanenceWitness::from_sealed(
            ObjectRef::mint(sample_store(), ObjectId::from_digest([0x0B; 32])),
            hash,
            more_copies,
            CommitOrdinal::ZERO,
        );
        match PermanenceWitness::repair(&witness, hash, more_domains, None) {
            Err(ObjectRefuse::IncomparableClasses { original, proposed }) => {
                assert_eq!(original, more_copies);
                assert_eq!(proposed, more_domains);
            }
            other => panic!("expected IncomparableClasses, got {other:?}"),
        }
    }

    #[test]
    fn copies_only_lift_is_dominance_not_incomparable() {
        let backend = BackendContract::from_digest([0xBC; 32]);
        let base = ObjectDurabilityClass::new(
            ConfirmedCopies::One,
            FailureDomains::Single,
            Regions::Single,
            ConsistencyClass::Eventual,
            IntegrityVerification::ContentHash,
            backend,
        );
        let copies_lift = ObjectDurabilityClass::new(
            ConfirmedCopies::MultiSite,
            FailureDomains::Single,
            Regions::Single,
            ConsistencyClass::Eventual,
            IntegrityVerification::ContentHash,
            backend,
        );
        assert!(copies_lift.dominates(base));
        assert!(!base.incomparable(copies_lift));
    }
}
