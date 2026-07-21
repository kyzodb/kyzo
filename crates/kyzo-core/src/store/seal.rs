/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! CheckpointSeal: prefix authority transfer (decisions.md §26).
//!
//! Owns: [`CheckpointSeal`], [`truncate`] (consumes seal), seal verification.
//!
//! Bans: truncation without a covering seal; seals spanning epoch transitions;
//! plaintext ShredSalt/DEK/WriteAuthority/AuditKey material inside seals;
//! silent prefer-dump on mismatch.
//!
//! Mint requires: replay of the covered prefix reproduces the checkpoint
//! exactly; a retention certificate covers every snapshot/as-of obligation
//! in the prefix. Unsealed dumps stay disposable.

use super::authority::IncarnationId;
use super::contract::FormatVersion;
use super::epoch::{CryptoDomain, FenceEpoch};
use super::nonce::{DomainCounter, MintDomain};
use super::open::StoreId;
use super::sweep::CommitOrdinal;
use super::transcript::{
    CanonicalTranscript, CanonicalTranscriptBuilder, FieldId, SealedArtifactKind, TranscriptRefuse,
    refuse_residual_secret_bytes,
};
use super::wal::WalHash;

/// Fixed-width seal / manifest digest (SHA-256).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SealDigest([u8; 32]);

impl SealDigest {
    /// Wrap an already-proven seal digest.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Borrow the digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for SealDigest {
    fn from(digest: [u8; 32]) -> Self {
        Self(digest)
    }
}

impl AsRef<[u8]> for SealDigest {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Genesis prior-seal digest — first seal in a lineage covers this.
pub const GENESIS_PRIOR_SEAL: SealDigest = SealDigest([0u8; 32]);

/// Bound NonceLease floors for the sealed prefix (per MintDomain).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NonceLeaseFloors {
    /// Highest durably reserved exclusive ceiling for Commit.
    pub commit: DomainCounter,
    /// Highest durably reserved exclusive ceiling for Compact.
    pub compact: DomainCounter,
    /// Highest durably reserved exclusive ceiling for Rotate.
    pub rotate: DomainCounter,
}

impl NonceLeaseFloors {
    /// Genesis floors (all domains at zero).
    pub fn genesis() -> Self {
        Self {
            commit: DomainCounter::ZERO,
            compact: DomainCounter::ZERO,
            rotate: DomainCounter::ZERO,
        }
    }

    /// Ceiling for a mint domain.
    pub fn ceiling(self, domain: MintDomain) -> DomainCounter {
        match domain {
            MintDomain::Commit => self.commit,
            MintDomain::Compact => self.compact,
            MintDomain::Rotate => self.rotate,
        }
    }
}

/// Inputs required to privately mint a [`CheckpointSeal`].
///
/// Binding list (minimum, §26): StoreId; CryptoDomain; cut FenceEpoch;
/// state root at cut; final WAL hash of the covered prefix; checkpoint
/// manifest digest; FormatVersion; Catalog generation at cut; retained-object
/// manifest (incl. every Pending ObjectSlot at cut); live PermanenceCandidate
/// manifest; retained-certificate / ReplicaCustody manifest; NonceLease
/// floors; Incarnation history boundary; prior seal digest or genesis digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointSealParts {
    /// Store identity.
    pub store_id: StoreId,
    /// Crypto domain at the cut (seals never span epoch transitions).
    pub crypto_domain: CryptoDomain,
    /// Fence epoch at the cut.
    pub fence_epoch: FenceEpoch,
    /// Dense commit ordinal at the cut (Catalog generation / StagingTTL denom).
    pub cut: CommitOrdinal,
    /// Plaintext-canonical state root at the cut.
    pub state_root: SealDigest,
    /// Final WAL hash of the covered prefix.
    pub final_wal_hash: WalHash,
    /// Checkpoint manifest digest.
    pub checkpoint_manifest: SealDigest,
    /// FormatVersion of sealed artifacts at the cut.
    pub format_version: FormatVersion,
    /// Catalog generation at cut (= Store commit position).
    pub catalog_generation: CommitOrdinal,
    /// Retained-object manifest digest — includes every Pending ObjectSlot at cut.
    pub retained_object_manifest: SealDigest,
    /// Live PermanenceCandidate manifest digest (distinct from Pending-at-cut).
    pub permanence_candidate_manifest: SealDigest,
    /// Retained-certificate / ReplicaCustody manifest digest.
    pub replica_custody_manifest: SealDigest,
    /// NonceLease floors for the prefix.
    pub nonce_floors: NonceLeaseFloors,
    /// Highest IncarnationId sealed before truncation.
    pub incarnation_boundary: IncarnationId,
    /// Prior seal digest, or [`GENESIS_PRIOR_SEAL`].
    pub prior_seal_digest: SealDigest,
    /// Retention certificate covering every snapshot/as-of obligation in the prefix.
    pub retention_certificate_digest: SealDigest,
}

/// Prefix authority transfer artifact. Privately minted; truncate consumes it.
///
/// Dump without a seal stays disposable. Restore/open against a seal that
/// fails any bound digest → [`SealRefuse::SealMismatch`] — never silent prefer-dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointSeal {
    store_id: StoreId,
    crypto_domain: CryptoDomain,
    fence_epoch: FenceEpoch,
    cut: CommitOrdinal,
    state_root: SealDigest,
    final_wal_hash: WalHash,
    checkpoint_manifest: SealDigest,
    format_version: FormatVersion,
    catalog_generation: CommitOrdinal,
    retained_object_manifest: SealDigest,
    permanence_candidate_manifest: SealDigest,
    replica_custody_manifest: SealDigest,
    nonce_floors: NonceLeaseFloors,
    incarnation_boundary: IncarnationId,
    prior_seal_digest: SealDigest,
    retention_certificate_digest: SealDigest,
    /// Self-digest over the bound fields (CanonicalTranscript seat later).
    seal_digest: SealDigest,
}

impl CheckpointSeal {
    /// Privately mint a seal after replay verification + retention coverage.
    ///
    /// Callers must have verified that replay of the covered prefix reproduces
    /// the checkpoint exactly. Epoch-spanning seals are Unconstructible here:
    /// `parts.crypto_domain.fence_epoch` must equal `parts.fence_epoch`.
    pub(crate) fn mint(parts: CheckpointSealParts) -> Result<CheckpointSeal, SealRefuse> {
        if parts.crypto_domain.store_id() != parts.store_id {
            return Err(SealRefuse::StoreIdMismatch);
        }
        if parts.crypto_domain.fence_epoch() != parts.fence_epoch {
            return Err(SealRefuse::EpochSpanForbidden);
        }
        let seal_digest = digest_parts(&parts);
        Ok(CheckpointSeal {
            store_id: parts.store_id,
            crypto_domain: parts.crypto_domain,
            fence_epoch: parts.fence_epoch,
            cut: parts.cut,
            state_root: parts.state_root,
            final_wal_hash: parts.final_wal_hash,
            checkpoint_manifest: parts.checkpoint_manifest,
            format_version: parts.format_version,
            catalog_generation: parts.catalog_generation,
            retained_object_manifest: parts.retained_object_manifest,
            permanence_candidate_manifest: parts.permanence_candidate_manifest,
            replica_custody_manifest: parts.replica_custody_manifest,
            nonce_floors: parts.nonce_floors,
            incarnation_boundary: parts.incarnation_boundary,
            prior_seal_digest: parts.prior_seal_digest,
            retention_certificate_digest: parts.retention_certificate_digest,
            seal_digest,
        })
    }

    /// Store identity.
    pub fn store_id(&self) -> StoreId {
        self.store_id
    }

    /// Crypto domain at the cut.
    pub fn crypto_domain(&self) -> CryptoDomain {
        self.crypto_domain
    }

    /// Fence epoch at the cut.
    pub fn fence_epoch(&self) -> FenceEpoch {
        self.fence_epoch
    }

    /// Dense commit ordinal at the cut.
    pub fn cut(&self) -> CommitOrdinal {
        self.cut
    }

    /// State root at the cut.
    pub fn state_root(&self) -> SealDigest {
        self.state_root
    }

    /// Final WAL hash of the covered prefix.
    pub fn final_wal_hash(&self) -> WalHash {
        self.final_wal_hash
    }

    /// Checkpoint manifest digest.
    pub fn checkpoint_manifest(&self) -> SealDigest {
        self.checkpoint_manifest
    }

    /// FormatVersion at the cut.
    pub fn format_version(&self) -> FormatVersion {
        self.format_version
    }

    /// Catalog generation at the cut.
    pub fn catalog_generation(&self) -> CommitOrdinal {
        self.catalog_generation
    }

    /// Retained-object manifest (Pending-at-cut inclusive).
    pub fn retained_object_manifest(&self) -> SealDigest {
        self.retained_object_manifest
    }

    /// Live PermanenceCandidate manifest.
    pub fn permanence_candidate_manifest(&self) -> SealDigest {
        self.permanence_candidate_manifest
    }

    /// ReplicaCustody / retained-certificate manifest.
    pub fn replica_custody_manifest(&self) -> SealDigest {
        self.replica_custody_manifest
    }

    /// NonceLease floors for the prefix.
    pub fn nonce_floors(&self) -> NonceLeaseFloors {
        self.nonce_floors
    }

    /// Incarnation history boundary.
    pub fn incarnation_boundary(&self) -> IncarnationId {
        self.incarnation_boundary
    }

    /// Prior seal digest (or genesis).
    pub fn prior_seal_digest(&self) -> SealDigest {
        self.prior_seal_digest
    }

    /// Retention certificate digest covering as-of/snapshot obligations.
    pub fn retention_certificate_digest(&self) -> SealDigest {
        self.retention_certificate_digest
    }

    /// Self-digest of this seal.
    pub fn seal_digest(&self) -> SealDigest {
        self.seal_digest
    }

    /// Verify every bound digest against an observed reconstruction.
    ///
    /// Any mismatch → [`SealRefuse::SealMismatch`]. Never silent prefer-dump.
    pub fn verify(&self, observed: &CheckpointSealParts) -> Result<(), SealRefuse> {
        let expected = digest_parts(observed);
        if self.seal_digest != expected
            || self.store_id != observed.store_id
            || self.crypto_domain != observed.crypto_domain
            || self.fence_epoch != observed.fence_epoch
            || self.cut != observed.cut
            || self.state_root != observed.state_root
            || self.final_wal_hash != observed.final_wal_hash
            || self.checkpoint_manifest != observed.checkpoint_manifest
            || self.format_version != observed.format_version
            || self.catalog_generation != observed.catalog_generation
            || self.retained_object_manifest != observed.retained_object_manifest
            || self.permanence_candidate_manifest != observed.permanence_candidate_manifest
            || self.replica_custody_manifest != observed.replica_custody_manifest
            || self.nonce_floors != observed.nonce_floors
            || self.incarnation_boundary != observed.incarnation_boundary
            || self.prior_seal_digest != observed.prior_seal_digest
            || self.retention_certificate_digest != observed.retention_certificate_digest
        {
            return Err(SealRefuse::SealMismatch);
        }
        Ok(())
    }

    /// Encode this seal under the one [`CanonicalTranscript`] law (seat 59/26).
    ///
    /// Bound digests that survive into sealed bytes are the deep-reachability
    /// surface the crypto-shred campaign searches — never a second encoder.
    pub fn encode_transcript(&self) -> Result<CanonicalTranscript, SealRefuse> {
        let mut b = CanonicalTranscriptBuilder::new(self.format_version)
            .map_err(SealRefuse::from_transcript)?;
        b.append_u64(
            FieldId::ARTIFACT_KIND,
            SealedArtifactKind::CheckpointSeal.tag(),
        )
        .map_err(SealRefuse::from_transcript)?;
        b.append_bytes(FieldId::FORMAT_VERSION, &self.format_version.as_bytes())
            .map_err(SealRefuse::from_transcript)?;
        b.append_digest32(FieldId::PRIMARY_DIGEST, self.seal_digest.as_bytes())
            .map_err(SealRefuse::from_transcript)?;
        b.append_digest32(FieldId::SECONDARY_DIGEST, self.state_root.as_bytes())
            .map_err(SealRefuse::from_transcript)?;
        b.append_bytes(FieldId::DOMAIN_LABEL, b"checkpoint-seal")
            .map_err(SealRefuse::from_transcript)?;
        Ok(b.seal())
    }

    /// Deep sealed-artifact scrub (§64/§65): encode under CanonicalTranscript,
    /// then refuse if any shredded DEK / KEK / plaintext ShredSalt needle is
    /// still reachable in the sealed bytes.
    pub fn refuse_residual_secrets(
        &self,
        shredded_secret_needles: &[&[u8]],
    ) -> Result<(), SealRefuse> {
        let transcript = self.encode_transcript()?;
        match refuse_residual_secret_bytes(transcript.as_bytes(), shredded_secret_needles) {
            Ok(()) => Ok(()),
            Err(TranscriptRefuse::Corrupt) => Err(SealRefuse::ResidualSecretMaterial),
            Err(other) => Err(SealRefuse::from_transcript(other)),
        }
    }
}

/// Consume a covering [`CheckpointSeal`] against a [`TruncateLedger`].
///
/// Truncation without consuming a seal is Unconstructible — there is no
/// seal-free overload. Crash-mid-truncate converges: the ledger records the
/// spent seal digest; retry with the same seal → [`SealRefuse::SealAlreadyConsumed`]
/// (idempotent crash path). First consume transfers authority and yields a
/// [`TruncationReceipt`].
pub fn truncate(
    seal: CheckpointSeal,
    ledger: &mut TruncateLedger,
) -> Result<TruncationReceipt, SealRefuse> {
    let digest = seal.seal_digest;
    if !ledger.record_consume(digest) {
        return Err(SealRefuse::SealAlreadyConsumed);
    }
    Ok(TruncationReceipt {
        store_id: seal.store_id,
        cut: seal.cut,
        seal_digest: digest,
        final_wal_hash: seal.final_wal_hash,
    })
}

/// Durable truncate-authority ledger — records spent seal digests so crash
/// retry converges without double-truncation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TruncateLedger {
    spent: std::collections::BTreeSet<SealDigest>,
}

impl TruncateLedger {
    /// Empty ledger (no seals consumed yet).
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `digest` was already consumed.
    pub fn is_consumed(&self, digest: SealDigest) -> bool {
        self.spent.contains(&digest)
    }

    /// Record a consume. Returns `false` if already spent (idempotent refuse).
    fn record_consume(&mut self, digest: SealDigest) -> bool {
        self.spent.insert(digest)
    }
}

/// Proof that a covering seal was consumed for truncation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncationReceipt {
    store_id: StoreId,
    cut: CommitOrdinal,
    seal_digest: SealDigest,
    final_wal_hash: WalHash,
}

impl TruncationReceipt {
    /// Store whose prefix was truncated.
    pub fn store_id(&self) -> StoreId {
        self.store_id
    }

    /// Cut through which the seal authorized truncation.
    pub fn cut(&self) -> CommitOrdinal {
        self.cut
    }

    /// Digest of the consumed seal.
    pub fn seal_digest(&self) -> SealDigest {
        self.seal_digest
    }

    /// Final WAL hash that remains the retained-suffix head predecessor.
    pub fn final_wal_hash(&self) -> WalHash {
        self.final_wal_hash
    }
}

/// Typed refusals on the seal / truncate / verify path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum SealRefuse {
    /// Restore/open against a seal that fails any bound digest.
    #[error("CheckpointSeal: SealMismatch — bound digest disagreed; never prefer-dump")]
    #[diagnostic(code(store::seal::seal_mismatch))]
    SealMismatch,
    /// Seal CryptoDomain fence epoch ≠ cut fence epoch (epoch-spanning ban).
    #[error("CheckpointSeal: seals must not span an epoch transition")]
    #[diagnostic(code(store::seal::epoch_span_forbidden))]
    EpochSpanForbidden,
    /// Parts StoreId ≠ CryptoDomain StoreId.
    #[error("CheckpointSeal: StoreId does not match CryptoDomain")]
    #[diagnostic(code(store::seal::store_id_mismatch))]
    StoreIdMismatch,
    /// Truncate ledger already recorded this seal as spent (idempotent crash path).
    #[error("CheckpointSeal: seal already consumed for truncation")]
    #[diagnostic(code(store::seal::already_consumed))]
    SealAlreadyConsumed,
    /// Deep sealed-artifact scrub: residual DEK / KEK / plaintext ShredSalt in
    /// the seal's CanonicalTranscript bytes after shred (§64/§65).
    #[error("CheckpointSeal: residual secret material in sealed artifact")]
    #[diagnostic(code(store::seal::residual_secret))]
    ResidualSecretMaterial,
    /// Transcript encode/parse failed while sealing under CanonicalTranscript.
    #[error("CheckpointSeal: CanonicalTranscript encode refused")]
    #[diagnostic(code(store::seal::transcript_refuse))]
    TranscriptEncode,
}

impl SealRefuse {
    fn from_transcript(_err: TranscriptRefuse) -> Self {
        SealRefuse::TranscriptEncode
    }
}

fn digest_parts(parts: &CheckpointSealParts) -> SealDigest {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"kyzo.checkpoint_seal.v1");
    h.update(parts.store_id.as_bytes());
    h.update(parts.crypto_domain.store_id().as_bytes());
    h.update(u64::to_be_bytes(parts.crypto_domain.fence_epoch().get()));
    h.update(parts.crypto_domain.fence_epoch().store_id().as_bytes());
    h.update(u64::to_be_bytes(parts.fence_epoch.get()));
    h.update(parts.fence_epoch.store_id().as_bytes());
    h.update(u64::to_be_bytes(parts.cut.get()));
    h.update(parts.state_root.as_bytes());
    h.update(parts.final_wal_hash.as_bytes());
    h.update(parts.checkpoint_manifest.as_bytes());
    h.update(parts.format_version.as_bytes());
    h.update(u64::to_be_bytes(parts.catalog_generation.get()));
    h.update(parts.retained_object_manifest.as_bytes());
    h.update(parts.permanence_candidate_manifest.as_bytes());
    h.update(parts.replica_custody_manifest.as_bytes());
    h.update(u64::to_be_bytes(parts.nonce_floors.commit.get()));
    h.update(u64::to_be_bytes(parts.nonce_floors.compact.get()));
    h.update(u64::to_be_bytes(parts.nonce_floors.rotate.get()));
    h.update(u64::to_be_bytes(
        parts.incarnation_boundary.open_ordinal().get(),
    ));
    h.update(parts.incarnation_boundary.entropy().as_bytes());
    h.update(parts.prior_seal_digest.as_bytes());
    h.update(parts.retention_certificate_digest.as_bytes());
    SealDigest::from_digest(h.finalize().into())
}
