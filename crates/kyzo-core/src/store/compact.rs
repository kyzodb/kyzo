/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Deterministic compaction under encryption (decisions.md §45, §46, §47, §66).
//!
//! Owns: [`pace`] = f(debt), range-class commit-time classifier,
//! [`MergeProof`], KV-separation threshold.
//!
//! Bans: wall-clock/CPU/IO inputs to pace (replay determinism dies);
//! background reclassification of frozen↔current; ciphertext bit-identity
//! as sealed compact identity.
//!
//! Carried: the large-value KV-separation byte threshold is measured (RUM
//! benches), never dogma in either direction — inventing a constant without
//! the campaign is forbidden (carried obligation `kv-separation-threshold`).

use super::nonce::DomainCounter;
use super::sweep::CommitOrdinal;

/// Committed-byte / reclaimable-debt quantities — the only legal pace inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CompactionDebt {
    /// Bytes committed since last leveled compact.
    pub committed_bytes: u64,
    /// Reclaimable debt ledger quantity.
    pub reclaimable_bytes: u64,
}

/// Pure pace outcome — reconstructible from Store debt facts alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CompactionPace {
    /// Work units to schedule (deterministic from debt).
    pub work_units: u64,
}

/// `pace = f(debt)` — pure function of committed-byte / reclaimable-debt
/// quantities only. Wall-clock, CPU, and IO-util inputs are Unconstructible
/// (absent from this signature).
pub fn pace(debt: CompactionDebt) -> CompactionPace {
    // Deterministic fold: reclaimable pressure weighted with committed growth.
    // Coefficients are Spec placeholders until WA/space-amp benches seal them;
    // purity (debt-only) is the law this seat enforces.
    let work_units = debt
        .committed_bytes
        .saturating_add(debt.reclaimable_bytes.saturating_mul(2));
    CompactionPace { work_units }
}

/// Range class fixed at write from the bitemporal coordinate (§46).
///
/// Background heuristic reclassification of frozen↔current is Unconstructible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RangeClass {
    /// Current / hot range — densify under pace.
    Current,
    /// Frozen historical range — densify-once-then-pin.
    Frozen,
}

/// Commit-time classifier: range class from the key’s bitemporal fact.
///
/// `written_at` is the commit ordinal that sealed the key; `frozen_before`
/// is the Store’s frozen watermark. Classification is fixed at write —
/// never reopened by a background worker.
pub fn classify_range_at_commit(
    written_at: CommitOrdinal,
    frozen_before: CommitOrdinal,
) -> RangeClass {
    if written_at.get() < frozen_before.get() {
        RangeClass::Frozen
    } else {
        RangeClass::Current
    }
}

/// Plaintext content hash of a sealed packet.
pub type PacketContentHash = [u8; 32];

/// Lineage hash covering predecessor packet identities.
pub type LineageHash = [u8; 32];

/// Plaintext-canonical state root bound into a merged packet header.
pub type CompactStateRoot = [u8; 32];

/// Inputs required to privately mint a [`MergeProof`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeProofParts {
    /// Input packets’ plaintext content hashes (order is lineage order).
    pub input_content_hashes: Vec<PacketContentHash>,
    /// Lineage hash sealed into the output header.
    pub lineage_hash: LineageHash,
    /// State root bound into sealed identity.
    pub state_root: CompactStateRoot,
    /// Compact-domain counter advanced under the write fence.
    pub compact_counter: DomainCounter,
    /// Output packet plaintext content hash.
    pub output_content_hash: PacketContentHash,
}

/// Private proof that a merged packet was minted under §66 law.
///
/// Sealed / DST-replayable identity is over plaintext content + lineage
/// hashes + state roots — never ciphertext. A compacted packet without
/// lawful MergeProof lineage is Unconstructible (merged-packet constructor
/// is private to this mint).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeProof {
    input_content_hashes: Vec<PacketContentHash>,
    lineage_hash: LineageHash,
    state_root: CompactStateRoot,
    compact_counter: DomainCounter,
    output_content_hash: PacketContentHash,
    /// Sealed identity digest (plaintext-canonical).
    sealed_identity: [u8; 32],
}

impl MergeProof {
    /// Privately mint a MergeProof and the merged packet it authorizes.
    ///
    /// Requires at least one input content hash. Empty merge is refused.
    pub(crate) fn mint(parts: MergeProofParts) -> Result<(MergeProof, MergedPacket), CompactRefuse> {
        if parts.input_content_hashes.is_empty() {
            return Err(CompactRefuse::EmptyMerge);
        }
        let sealed_identity = sealed_identity_digest(&parts);
        let proof = MergeProof {
            input_content_hashes: parts.input_content_hashes,
            lineage_hash: parts.lineage_hash,
            state_root: parts.state_root,
            compact_counter: parts.compact_counter,
            output_content_hash: parts.output_content_hash,
            sealed_identity,
        };
        let packet = MergedPacket {
            content_hash: proof.output_content_hash,
            lineage_hash: proof.lineage_hash,
            state_root: proof.state_root,
            compact_counter: proof.compact_counter,
            sealed_identity: proof.sealed_identity,
        };
        Ok((proof, packet))
    }

    /// Input plaintext content hashes.
    pub fn input_content_hashes(&self) -> &[PacketContentHash] {
        &self.input_content_hashes
    }

    /// Lineage hash.
    pub fn lineage_hash(&self) -> LineageHash {
        self.lineage_hash
    }

    /// Bound state root.
    pub fn state_root(&self) -> CompactStateRoot {
        self.state_root
    }

    /// Compact-domain counter at mint.
    pub fn compact_counter(&self) -> DomainCounter {
        self.compact_counter
    }

    /// Output plaintext content hash.
    pub fn output_content_hash(&self) -> PacketContentHash {
        self.output_content_hash
    }

    /// Sealed identity (plaintext content + lineage + roots — never ciphertext).
    pub fn sealed_identity(&self) -> [u8; 32] {
        self.sealed_identity
    }
}

/// Merged packet — constructible only via [`MergeProof::mint`].
///
/// Salt regeneration under MergeProof does not change sealed identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedPacket {
    content_hash: PacketContentHash,
    lineage_hash: LineageHash,
    state_root: CompactStateRoot,
    compact_counter: DomainCounter,
    sealed_identity: [u8; 32],
}

impl MergedPacket {
    /// Plaintext content hash.
    pub fn content_hash(&self) -> PacketContentHash {
        self.content_hash
    }

    /// Lineage hash.
    pub fn lineage_hash(&self) -> LineageHash {
        self.lineage_hash
    }

    /// Bound state root.
    pub fn state_root(&self) -> CompactStateRoot {
        self.state_root
    }

    /// Compact-domain counter.
    pub fn compact_counter(&self) -> DomainCounter {
        self.compact_counter
    }

    /// Sealed identity (cipher-invariant).
    pub fn sealed_identity(&self) -> [u8; 32] {
        self.sealed_identity
    }
}

/// KV-separation threshold seat — measured, never dogma.
///
/// The byte threshold is sealed from RUM benches (carried obligation). Until
/// that campaign greens, this type carries the unmeasured marker only —
/// inventing a production constant here is forbidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KvSeparationThreshold {
    /// Measured threshold once benched; `None` until the campaign seals it.
    measured_bytes: Option<u64>,
}

impl KvSeparationThreshold {
    /// Unmeasured seat — threshold not yet sealed from benches.
    pub const UNMEASURED: KvSeparationThreshold = KvSeparationThreshold {
        measured_bytes: None,
    };

    /// Seal a measured threshold from the bench campaign (host/trials only).
    pub(crate) fn from_measured(bytes: u64) -> Self {
        Self {
            measured_bytes: Some(bytes),
        }
    }

    /// Measured byte threshold, if the campaign has sealed one.
    pub fn measured_bytes(self) -> Option<u64> {
        self.measured_bytes
    }
}

/// Typed refusals on the compact path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum CompactRefuse {
    /// Merge with zero input packets.
    #[error("compact: empty MergeProof input set")]
    #[diagnostic(code(store::compact::empty_merge))]
    EmptyMerge,
}

fn sealed_identity_digest(parts: &MergeProofParts) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"kyzo.merge_proof.v1");
    for content in &parts.input_content_hashes {
        h.update(content);
    }
    h.update(parts.lineage_hash);
    h.update(parts.state_root);
    h.update(u64::to_be_bytes(parts.compact_counter.get()));
    h.update(parts.output_content_hash);
    h.finalize().into()
}
