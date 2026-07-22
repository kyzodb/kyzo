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

/// Content hash of object bytes (plaintext-canonical).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    /// Wrap an already-proven content hash.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

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

    /// Staging dual of this durable identity (same Store scope + object id).
    pub fn as_staging_token(self) -> StagingToken {
        StagingToken::mint(self.store_id, self.object_id)
    }

    /// Durable arm of [`ObjectSlot`].
    pub fn as_durable_slot(self) -> ObjectSlot {
        ObjectSlot::Durable(self)
    }

    /// Pending arm naming the same identity under staging law.
    pub fn as_pending_slot(self) -> ObjectSlot {
        ObjectSlot::Pending(self.as_staging_token())
    }

    /// Content-hash bind for bytes under this durable identity.
    pub fn content_hash(self, digest: [u8; 32]) -> ContentHash {
        ContentHash::from_digest(digest)
    }

    /// Stage volatile bytes under this identity's staging token.
    pub fn stage_volatile(
        self,
        content_hash: ContentHash,
        stage_commit: CommitOrdinal,
        ttl: StagingTtl,
    ) -> Result<VolatilePending, ObjectRefuse> {
        VolatilePending::stage(self.as_staging_token(), content_hash, stage_commit, ttl)
    }

    /// Lift staged bytes into a permanence candidate, then confirm under `class`.
    pub fn confirm_permanence(
        self,
        pending: VolatilePending,
        cut: CommitOrdinal,
        class: ObjectDurabilityClass,
    ) -> Result<PermanenceWitness, ObjectRefuse> {
        if pending.token().store_id() != self.store_id
            || pending.token().object_id() != self.object_id
        {
            return Err(ObjectRefuse::ObjectRefForeignStore);
        }
        let candidate = PermanenceCandidate::from_volatile(pending);
        if !candidate.may_confirm(cut) {
            return Err(ObjectRefuse::Decayed);
        }
        let witness = PermanenceWitness::mint(&candidate, cut, class)?;
        Ok(witness)
    }

    /// Resolve this durable ref at `cut` under `calling_store` scope law.
    pub fn resolve_at(
        self,
        calling_store: StoreId,
        cut: CommitOrdinal,
    ) -> Result<ResolvedObject, ObjectRefuse> {
        resolve(self.as_durable_slot(), calling_store, cut)
    }

    /// Retention certificate covering as-of obligations through `cut`.
    pub fn retention_certificate(
        self,
        covers_through: CommitOrdinal,
        digest: [u8; 32],
    ) -> RetentionCertificate {
        RetentionCertificate::mint(self.store_id, covers_through, digest)
    }

    /// Reclaim certificate for this identity's staged form.
    pub fn reclaim_certificate(self, digest: [u8; 32]) -> ReclaimCertificate {
        ReclaimCertificate::mint(self.store_id, self.object_id, digest)
    }

    /// GC this durable object under a covering retention certificate.
    pub fn gc_under(
        self,
        retention: &RetentionCertificate,
        max_retaining_snapshot: CommitOrdinal,
    ) -> Result<(), ObjectRefuse> {
        gc_durable(self, retention, max_retaining_snapshot)
    }

    /// Reclaim a permanence candidate naming this identity.
    pub fn reclaim(
        self,
        candidate: PermanenceCandidate,
        certificate: &ReclaimCertificate,
    ) -> Result<(), ObjectRefuse> {
        if candidate.token().store_id() != self.store_id
            || candidate.token().object_id() != self.object_id
        {
            return Err(ObjectRefuse::ReclaimMismatch);
        }
        reclaim_candidate(candidate, certificate)
    }

    /// Put/get/delete/list through the closed [`ObjectStore`] driver only.
    pub fn with_object_store<S: ObjectStore>(self, store: &mut S) -> &mut S {
        drop(self);
        store
    }

    /// Seal a permanence witness from already-proven parts (campaign / trust door).
    pub fn permanence_from_sealed(
        self,
        content_hash: ContentHash,
        class: ObjectDurabilityClass,
        confirmed_at: CommitOrdinal,
    ) -> PermanenceWitness {
        let witness =
            PermanenceWitness::from_sealed(self, content_hash, class, confirmed_at);
        witness
    }

    /// Repair path under PermanenceWitness law (dominating class or Downgrade).
    pub fn repair_witness(
        self,
        original: &PermanenceWitness,
        bytes_hash: ContentHash,
        proposed: ObjectDurabilityClass,
        downgrade: Option<Downgrade>,
    ) -> Result<Repair, ObjectRefuse> {
        if original.object_ref() != self {
            return Err(ObjectRefuse::ObjectRefForeignStore);
        }
        let repair = PermanenceWitness::repair(original, bytes_hash, proposed, downgrade)?;
        Ok(repair)
    }

    /// Product durability class for this identity's permanence confirm.
    pub fn durability_class(
        self,
        confirmed_copies: ConfirmedCopies,
        failure_domains: FailureDomains,
        regions: Regions,
        consistency: ConsistencyClass,
        integrity_verification: IntegrityVerification,
        backend_contract: BackendContract,
    ) -> ObjectDurabilityClass {
        drop(self);
        ObjectDurabilityClass::new(
            confirmed_copies,
            failure_domains,
            regions,
            consistency,
            integrity_verification,
            backend_contract,
        )
    }

    /// Backend contract arm identity for durability product sealing.
    pub fn backend_contract(self, digest: [u8; 32]) -> BackendContract {
        drop(self);
        BackendContract::from_digest(digest)
    }

    /// Closed unit refuse arms (DST / delete_meter enumeration).
    pub fn refuse_unit_arms(self) -> [ObjectRefuse; 10] {
        drop(self);
        [
            ObjectRefuse::Decayed,
            ObjectRefuse::ObjectMissing,
            ObjectRefuse::ObjectRetainRequired,
            ObjectRefuse::ObjectRefForeignStore,
            ObjectRefuse::ObjectMissingForAsOf,
            ObjectRefuse::NonDominatingRepair,
            ObjectRefuse::RepairBytesMismatch,
            ObjectRefuse::DowngradeMismatch,
            ObjectRefuse::ReclaimMismatch,
            ObjectRefuse::StagingTtlOverflow,
        ]
    }

    /// Named refuse arms that carry payload or stand alone at the Store door.
    pub fn refuse_payload_arms(
        self,
        original: ObjectDurabilityClass,
        proposed: ObjectDurabilityClass,
    ) -> [ObjectRefuse; 3] {
        drop(self);
        [
            ObjectRefuse::IncomparableClasses { original, proposed },
            ObjectRefuse::ObjectBackendFull,
            ObjectRefuse::StoreDeleted,
        ]
    }

    /// Closed ConfirmedCopies sum arms.
    pub fn confirmed_copies_arms(self) -> [ConfirmedCopies; 3] {
        drop(self);
        [
            ConfirmedCopies::One,
            ConfirmedCopies::Quorum,
            ConfirmedCopies::MultiSite,
        ]
    }

    /// Closed FailureDomains sum arms.
    pub fn failure_domains_arms(self) -> [FailureDomains; 2] {
        drop(self);
        [FailureDomains::Single, FailureDomains::Distinct]
    }

    /// Closed Regions sum arms.
    pub fn regions_arms(self) -> [Regions; 2] {
        drop(self);
        [Regions::Single, Regions::Multi]
    }

    /// Closed ConsistencyClass sum arms.
    pub fn consistency_arms(self) -> [ConsistencyClass; 2] {
        drop(self);
        [ConsistencyClass::Eventual, ConsistencyClass::Strong]
    }

    /// Closed IntegrityVerification sum arms.
    pub fn integrity_arms(self) -> [IntegrityVerification; 2] {
        drop(self);
        [
            IntegrityVerification::ContentHash,
            IntegrityVerification::HashAndScrub,
        ]
    }

    /// Explicit Downgrade record (never silent class supersession).
    pub fn downgrade(self, from: ObjectDurabilityClass, to: ObjectDurabilityClass) -> Downgrade {
        drop(self);
        Downgrade { from, to }
    }
}

/// Store-identity-prefixed staging token for Pending slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StagingToken {
    store_id: StoreId,
    object_id: ObjectId,
}

impl StagingToken {
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

/// Physical first stage: volatile bytes not yet a PermanenceCandidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolatilePending {
    token: StagingToken,
    content_hash: ContentHash,
    /// `stage_commit + StagingTTL` — cut comparison only; never wall clock.
    expires_at: CommitOrdinal,
}

impl VolatilePending {
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

    /// Staging token.
    pub fn token(&self) -> StagingToken {
        self.token
    }

    /// Content hash of staged bytes.
    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    /// Ordinal cut at which Pending decays (idle Stores never advance this).
    pub fn expires_at(&self) -> CommitOrdinal {
        self.expires_at
    }
}

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

impl PermanenceCandidate {
    /// Lift VolatilePending into a PermanenceCandidate (strip-before-confirm ban:
    /// confirm is a separate [`PermanenceWitness`] mint).
    pub(crate) fn from_volatile(pending: VolatilePending) -> Self {
        PermanenceCandidate {
            token: pending.token,
            content_hash: pending.content_hash,
            expires_at: pending.expires_at,
        }
    }

    /// Staging token (still Pending logically until Durable supersession).
    pub fn token(&self) -> StagingToken {
        self.token
    }

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

impl ObjectDurabilityClass {
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

/// Sealed dimension: confirmed copy count class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConfirmedCopies {
    /// Single confirmed copy.
    One,
    /// Quorum / multi-copy within one failure domain.
    Quorum,
    /// Multi-site confirmed copies.
    MultiSite,
}

/// Sealed dimension: failure-domain separation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FailureDomains {
    /// Single failure domain.
    Single,
    /// Distinct failure domains.
    Distinct,
}

/// Sealed dimension: region placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Regions {
    /// Single region.
    Single,
    /// Multi-region.
    Multi,
}

/// Sealed dimension: consistency contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConsistencyClass {
    /// Eventual visibility of confirmed copies.
    Eventual,
    /// Strong read-after-confirm.
    Strong,
}

/// Sealed dimension: integrity verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IntegrityVerification {
    /// Content-hash verify on get.
    ContentHash,
    /// Content-hash plus periodic scrub.
    HashAndScrub,
}

/// Sealed dimension: backend contract arm identity (opaque digest).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BackendContract([u8; 32]);

impl BackendContract {
    /// Bind an arm identity digest.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Arm digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

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

impl PermanenceWitness {
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

    /// Durable ref sealed by this witness.
    pub fn object_ref(&self) -> ObjectRef {
        self.object_ref
    }

    /// Content hash.
    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    /// Sealed durability class product.
    pub fn class(&self) -> ObjectDurabilityClass {
        self.class
    }

    /// Commit ordinal at confirm.
    pub fn confirmed_at(&self) -> CommitOrdinal {
        self.confirmed_at
    }
}

/// Auditable append-only class supersession (never silent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Downgrade {
    /// Class being superseded.
    pub from: ObjectDurabilityClass,
    /// Explicit lower/incomparable successor class.
    pub to: ObjectDurabilityClass,
}

/// Repair outcome under PermanenceWitness law.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repair {
    object_ref: ObjectRef,
    content_hash: ContentHash,
    class: ObjectDurabilityClass,
    prior_class: ObjectDurabilityClass,
    downgrade: Option<Downgrade>,
}

impl Repair {
    /// Object under repair.
    pub fn object_ref(&self) -> ObjectRef {
        self.object_ref
    }

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

/// Capability-decided reclaim of unconfirmed staged objects (VolatilePending
/// or PermanenceCandidate). Always lawful idle or busy — never wall clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReclaimCertificate {
    store_id: StoreId,
    object_id: ObjectId,
    digest: [u8; 32],
}

impl ReclaimCertificate {
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

    /// Certificate digest.
    pub fn digest(self) -> [u8; 32] {
        self.digest
    }
}

/// Retention certificate gating Durable GC only (never Pending).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RetentionCertificate {
    store_id: StoreId,
    /// Max retaining snapshot / as-of cut covered.
    covers_through: CommitOrdinal,
    digest: [u8; 32],
}

impl RetentionCertificate {
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

    /// Certificate digest.
    pub fn digest(self) -> [u8; 32] {
        self.digest
    }
}

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
            drop(cut);
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

/// Resolve outcome (Pending licenses ref+expiry; Durable licenses class).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResolvedObject {
    /// Pending — client handles Decayed / ObjectMissing on materialize.
    Pending(StagingToken),
    /// Durable — client still handles corruption/unavailability.
    Durable(ObjectRef),
}

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

/// Typed refusals on the object seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum ObjectRefuse {
    /// Resolve of Pending past its cut.
    #[error("object: Decayed — Pending past expires_at cut")]
    #[diagnostic(code(store::objects::decayed))]
    Decayed,
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

fn add_ttl(stage: CommitOrdinal, ttl: StagingTtl) -> Result<CommitOrdinal, ObjectRefuse> {
    stage
        .get()
        .checked_add(ttl.ordinals())
        .map(CommitOrdinal::of_u64)
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
    fn incomparable_repair_is_typed_refuse_carrying_both_classes() -> miette::Result<()> {
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
            other => {
                return Err(miette::miette!("expected IncomparableClasses, got {other:?}"));
            }
        }
        Ok(())
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
