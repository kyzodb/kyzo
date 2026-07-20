/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! WAL segments — sole commit-order authority in the retained suffix
//! (decisions.md §21, §24).
//!
//! Owns: epoch-headed segment format, cross-segment hash chain, [`replay`],
//! mutable floors ([`NonceLease`] floors + [`IncarnationId`] records).
//!
//! The byte hash-chain ([`WalHash`] / replay [`WalReplayState::final_hash`])
//! meets the meaning-layer [`super::merkle::RootChain`] at the SweepDoor
//! durable commit — one boundary, two chains ([`super::merkle::DurableCommitCut`]).
//!
//! Bans: SST-as-source-of-truth after flush; per-segment checksums without
//! the cross-boundary chain.

use sha2::{Digest, Sha256};

use super::authority::IncarnationId;
use super::epoch::FenceEpoch;
use super::nonce::{DomainCounter, MintDomain, NonceLease};
use super::open::StoreId;
use super::sweep::CommitOrdinal;

/// Fixed-width predecessor / record hash (SHA-256).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WalHash([u8; 32]);

impl WalHash {
    /// Wrap an already-proven WAL hash digest.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Borrow the digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for WalHash {
    fn from(digest: [u8; 32]) -> Self {
        Self(digest)
    }
}

impl AsRef<[u8]> for WalHash {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Genesis predecessor hash — first record of the first segment covers this.
pub const GENESIS_PREDECESSOR: WalHash = WalHash([0u8; 32]);

/// One hash-chained WAL record. Covers its predecessor hash; the first
/// record of segment N covers the last hash of segment N−1 — splice of two
/// valid prefixes is unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalRecord {
    predecessor_hash: WalHash,
    payload: WalPayload,
    record_hash: WalHash,
}

impl WalRecord {
    /// Seal a record over `predecessor_hash` and `payload`.
    pub fn seal(predecessor_hash: WalHash, payload: WalPayload) -> Self {
        let record_hash = hash_record(predecessor_hash, &payload);
        Self {
            predecessor_hash,
            payload,
            record_hash,
        }
    }

    /// Predecessor hash this record covers.
    pub fn predecessor_hash(&self) -> WalHash {
        self.predecessor_hash
    }

    /// Record payload.
    pub fn payload(&self) -> &WalPayload {
        &self.payload
    }

    /// This record's hash (covered by the next record / next segment head).
    pub fn record_hash(&self) -> WalHash {
        self.record_hash
    }
}

/// Closed payload kinds carried in the WAL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalPayload {
    /// Durable commit event at a dense [`CommitOrdinal`].
    Commit {
        /// History ordinal sealed at the SweepDoor durable event.
        commit_ordinal: CommitOrdinal,
        /// Opaque commit body bytes (adapter currency until transcript seat).
        body: Vec<u8>,
    },
    /// Mutable floor: NonceLease reservation ceiling for a MintDomain.
    NonceFloor {
        /// Mint domain whose counter floor advanced.
        domain: MintDomain,
        /// Highest durably reserved exclusive ceiling.
        ceiling: DomainCounter,
    },
    /// Mutable floor: sealed IncarnationId history boundary.
    IncarnationSealed {
        /// Incarnation sealed into the WAL lineage.
        incarnation_id: IncarnationId,
    },
}

/// Epoch-headed WAL segment. Records inside are hash-chained; the first
/// record covers the prior segment's terminal hash (or [`GENESIS_PREDECESSOR`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalSegment {
    store_id: StoreId,
    fence_epoch: FenceEpoch,
    segment_index: u64,
    records: Vec<WalRecord>,
}

impl WalSegment {
    /// Open a new empty segment headed by `fence_epoch`.
    pub fn open(store_id: StoreId, fence_epoch: FenceEpoch, segment_index: u64) -> Self {
        Self {
            store_id,
            fence_epoch,
            segment_index,
            records: Vec::new(),
        }
    }

    /// Store identity this segment belongs to.
    pub fn store_id(&self) -> StoreId {
        self.store_id
    }

    /// Epoch heading this segment.
    pub fn fence_epoch(&self) -> FenceEpoch {
        self.fence_epoch
    }

    /// Dense segment index in the retained suffix.
    pub fn segment_index(&self) -> u64 {
        self.segment_index
    }

    /// Records in chain order.
    pub fn records(&self) -> &[WalRecord] {
        &self.records
    }

    /// Terminal hash of this segment (or genesis predecessor if empty).
    pub fn terminal_hash(&self) -> WalHash {
        self.records
            .last()
            .map(|r| r.record_hash())
            .unwrap_or(GENESIS_PREDECESSOR)
    }

    /// Append a sealed record. Predecessor must equal current terminal.
    pub fn append(&mut self, record: WalRecord) -> Result<(), WalRefuse> {
        let expected = self.terminal_hash();
        if record.predecessor_hash() != expected {
            return Err(WalRefuse::ChainBreak {
                expected,
                got: record.predecessor_hash(),
            });
        }
        self.records.push(record);
        Ok(())
    }
}

/// Mutable floors reconstructed from the retained WAL suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalFloors {
    /// Highest sealed open-ordinal incarnation observed (if any).
    pub highest_incarnation: Option<IncarnationId>,
    /// Commit-domain exclusive ceiling (resume strictly above).
    pub commit_nonce_ceiling: DomainCounter,
    /// Compact-domain exclusive ceiling.
    pub compact_nonce_ceiling: DomainCounter,
    /// Rotate-domain exclusive ceiling.
    pub rotate_nonce_ceiling: DomainCounter,
    /// Highest dense CommitOrdinal sealed in the suffix (if any).
    pub highest_commit_ordinal: Option<CommitOrdinal>,
}

impl WalFloors {
    /// Empty floors at genesis / wiped suffix.
    pub fn genesis() -> Self {
        Self {
            highest_incarnation: None,
            commit_nonce_ceiling: DomainCounter::ZERO,
            compact_nonce_ceiling: DomainCounter::ZERO,
            rotate_nonce_ceiling: DomainCounter::ZERO,
            highest_commit_ordinal: None,
        }
    }

    /// Ceiling for a MintDomain.
    pub fn nonce_ceiling(&self, domain: MintDomain) -> DomainCounter {
        match domain {
            MintDomain::Commit => self.commit_nonce_ceiling,
            MintDomain::Compact => self.compact_nonce_ceiling,
            MintDomain::Rotate => self.rotate_nonce_ceiling,
        }
    }
}

/// Engine-visible state reconstructed by [`replay`] from durable segments alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalReplayState {
    /// Mutable floors (NonceLease + IncarnationId).
    pub floors: WalFloors,
    /// Commit bodies in CommitOrdinal order (memtables/SSTs wipe and rebuild).
    pub commit_bodies: Vec<(CommitOrdinal, Vec<u8>)>,
    /// Final WAL hash of the replayed suffix.
    pub final_hash: WalHash,
}

/// Typed WAL refuse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum WalRefuse {
    #[error("WAL chain break: expected predecessor {expected:?}, got {got:?}")]
    #[diagnostic(code(store::wal::chain_break))]
    ChainBreak {
        /// Hash the chain required.
        expected: WalHash,
        /// Hash the record claimed.
        got: WalHash,
    },
    #[error("WAL segment index gap: expected {expected}, got {got}")]
    #[diagnostic(code(store::wal::segment_gap))]
    SegmentGap {
        /// Dense index expected next.
        expected: u64,
        /// Index present on the segment.
        got: u64,
    },
    #[error("WAL segment StoreId mismatch under replay")]
    #[diagnostic(code(store::wal::store_id_mismatch))]
    StoreIdMismatch,
    #[error("WAL record hash does not match sealed payload")]
    #[diagnostic(code(store::wal::record_hash_mismatch))]
    RecordHashMismatch,
}

/// Replay the retained WAL suffix from durable segments alone.
///
/// Reconstructs Engine-visible state; memtables/SSTs wipe and rebuild as cache.
/// Cross-segment: first record of segment N covers last hash of segment N−1.
pub fn replay(store_id: StoreId, segments: &[WalSegment]) -> Result<WalReplayState, WalRefuse> {
    let mut floors = WalFloors::genesis();
    let mut commit_bodies = Vec::new();
    let mut pred = GENESIS_PREDECESSOR;
    let mut expected_index = 0u64;

    for segment in segments {
        if segment.store_id() != store_id {
            return Err(WalRefuse::StoreIdMismatch);
        }
        if segment.segment_index() != expected_index {
            return Err(WalRefuse::SegmentGap {
                expected: expected_index,
                got: segment.segment_index(),
            });
        }
        for record in segment.records() {
            if record.predecessor_hash() != pred {
                return Err(WalRefuse::ChainBreak {
                    expected: pred,
                    got: record.predecessor_hash(),
                });
            }
            let recomputed = hash_record(record.predecessor_hash(), record.payload());
            if recomputed != record.record_hash() {
                return Err(WalRefuse::RecordHashMismatch);
            }
            apply_payload(&mut floors, &mut commit_bodies, record.payload());
            pred = record.record_hash();
        }
        expected_index = expected_index.saturating_add(1);
    }

    Ok(WalReplayState {
        floors,
        commit_bodies,
        final_hash: pred,
    })
}

fn apply_payload(
    floors: &mut WalFloors,
    commit_bodies: &mut Vec<(CommitOrdinal, Vec<u8>)>,
    payload: &WalPayload,
) {
    match payload {
        WalPayload::Commit {
            commit_ordinal,
            body,
        } => {
            floors.highest_commit_ordinal = Some(*commit_ordinal);
            commit_bodies.push((*commit_ordinal, body.clone()));
        }
        WalPayload::NonceFloor { domain, ceiling } => match domain {
            MintDomain::Commit => floors.commit_nonce_ceiling = *ceiling,
            MintDomain::Compact => floors.compact_nonce_ceiling = *ceiling,
            MintDomain::Rotate => floors.rotate_nonce_ceiling = *ceiling,
        },
        WalPayload::IncarnationSealed { incarnation_id } => {
            floors.highest_incarnation = Some(*incarnation_id);
        }
    }
}

fn hash_record(predecessor_hash: WalHash, payload: &WalPayload) -> WalHash {
    let mut h = Sha256::new();
    h.update(b"kyzo.wal.record.v1");
    h.update(predecessor_hash.as_bytes());
    match payload {
        WalPayload::Commit {
            commit_ordinal,
            body,
        } => {
            h.update(b"commit");
            h.update(u64::to_be_bytes(commit_ordinal.get()));
            h.update(body);
        }
        WalPayload::NonceFloor { domain, ceiling } => {
            h.update(b"nonce_floor");
            h.update([match domain {
                MintDomain::Commit => 1,
                MintDomain::Compact => 2,
                MintDomain::Rotate => 3,
            }]);
            h.update(u64::to_be_bytes(ceiling.get()));
        }
        WalPayload::IncarnationSealed { incarnation_id } => {
            h.update(b"incarnation");
            h.update(u64::to_be_bytes(incarnation_id.open_ordinal().get()));
            h.update(incarnation_id.entropy().as_bytes());
        }
    }
    WalHash::from_digest(h.finalize().into())
}

/// Bind a [`NonceLease`]'s exclusive ceiling into a floor payload for append.
pub fn nonce_floor_payload(lease: &NonceLease) -> WalPayload {
    WalPayload::NonceFloor {
        domain: lease.domain(),
        ceiling: lease.ceiling(),
    }
}
