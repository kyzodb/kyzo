/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The one idempotency organ (decisions.md §38, §39).
//!
//! Owns: [`OperationKey`], [`OperationOutcome`], request_digest memo.
//!
//! Bans: memoizing transient refuses as terminal; parallel token maps;
//! key-as-mutable-slot.
//!
//! `OperationKey = H(domain_label, CompositionId, StoreId, StepId)`.
//! Single-store safe-retry uses the same construction with a degenerate
//! CompositionId — one organ. ReadAt mints no entries.

use sha2::{Digest, Sha256};

use super::failure::StoreRefuse;
use super::open::StoreId;
use super::transcript::Digest32;

/// Store-scoped idempotency identity (§38/§39).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OperationKey([u8; 32]);

impl OperationKey {
    /// Derive `H(domain_label, CompositionId, StoreId, StepId)`.
    ///
    /// `composition_id` is the caller-derived CompositionId digest (session
    /// owns the type; Store sees only the sealed digest so the layering stays
    /// one-way).
    pub fn derive(
        domain_label: &[u8],
        composition_id: Digest32,
        store_id: StoreId,
        step_id: &[u8],
    ) -> Self {
        let mut h = Sha256::new();
        h.update(b"kyzo.operation_key.v1");
        h.update(domain_label);
        h.update(composition_id.as_bytes());
        h.update(store_id.as_bytes());
        h.update(step_id);
        Self(h.finalize().into())
    }

    /// Degenerate single-store safe-retry key over a caller-durable client op id.
    pub fn single_store(
        domain_label: &[u8],
        client_operation_id: &[u8],
        store_id: StoreId,
        step_id: &[u8],
    ) -> Self {
        let mut h = Sha256::new();
        h.update(b"kyzo.composition_id.degenerate.v1");
        h.update(client_operation_id);
        let degenerate = Digest32::admit(h.finalize().into());
        Self::derive(domain_label, degenerate, store_id, step_id)
    }

    /// Borrow the key digest.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Sealed request digest covering canonical envelope + schema + authority coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestDigest([u8; 32]);

impl RequestDigest {
    /// Wrap an already-proven request digest.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Borrow the digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}


/// Memoized terminal outcome for an [`OperationKey`] (§38).
///
/// Transient refuses (capacity, availability, transport) are **never**
/// memoized as terminal — only these three arms exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationOutcome {
    /// Prior admission committed under this key + digest.
    Committed {
        /// Sealed request digest that produced the commit.
        request_digest: RequestDigest,
    },
    /// Deterministic terminal refuse under this key + digest.
    DeterministicTerminalRefuse {
        /// Sealed request digest that produced the refuse.
        request_digest: RequestDigest,
        /// Store ledger refuse that terminated.
        refuse: StoreRefuse,
    },
    /// No memo entry (ReadAt / never admitted).
    Absent,
}

/// One memo entry: `(key, request_digest, terminal_outcome)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdempotencyEntry {
    key: OperationKey,
    request_digest: RequestDigest,
    outcome: OperationOutcome,
}

impl IdempotencyEntry {
    /// Key.
    pub fn key(&self) -> OperationKey {
        self.key
    }

    /// Request digest covering canonical envelope + schema + authority coordinates.
    pub fn request_digest(&self) -> RequestDigest {
        self.request_digest
    }

    /// Terminal outcome.
    pub fn outcome(&self) -> &OperationOutcome {
        &self.outcome
    }
}

/// In-memory OperationKey memo (Store-scoped).
#[derive(Debug)]
pub struct IdempotencyMemo {
    entries: std::collections::BTreeMap<[u8; 32], IdempotencyEntry>,
}

impl IdempotencyMemo {
    /// Empty memo.
    pub fn new() -> Self {
        Self {
            entries: std::collections::BTreeMap::new(),
        }
    }

    /// Digest of a request envelope (canonical bytes the caller already sealed).
    pub fn digest_request(envelope: &[u8]) -> RequestDigest {
        let mut h = Sha256::new();
        h.update(b"kyzo.request_digest.v1");
        h.update(envelope);
        RequestDigest::from_digest(h.finalize().into())
    }

    /// Look up a key. Missing → [`OperationOutcome::Absent`].
    pub fn lookup(&self, key: &OperationKey) -> OperationOutcome {
        match self.entries.get(key.as_bytes()) {
            Some(e) => e.outcome.clone(),
            None => OperationOutcome::Absent,
        }
    }

    /// Consult the memo for a safe-retry door (admit / commit).
    ///
    /// - No entry → [`Ok`]`(None)` (fresh).
    /// - Same key + digest → [`Ok`]`(Some(entry))` (replay).
    /// - Same key + different digest → [`StoreRefuse::OperationKeyReuse`].
    pub fn consult(
        &self,
        key: &OperationKey,
        request_digest: RequestDigest,
    ) -> Result<Option<&IdempotencyEntry>, StoreRefuse> {
        match self.entries.get(key.as_bytes()) {
            None => Ok(None),
            Some(existing) if existing.request_digest == request_digest => Ok(Some(existing)),
            Some(_) => Err(StoreRefuse::OperationKeyReuse),
        }
    }

    /// Replay or record a terminal outcome.
    ///
    /// Same key + digest → replay prior outcome. Same key + different digest →
    /// [`StoreRefuse::OperationKeyReuse`]. Transient outcomes must not call this.
    pub fn remember(
        &mut self,
        key: OperationKey,
        request_digest: RequestDigest,
        outcome: OperationOutcome,
    ) -> Result<OperationOutcome, StoreRefuse> {
        match outcome {
            OperationOutcome::Absent => {
                // Absent is not a terminal memo — never store it.
                return Ok(OperationOutcome::Absent);
            }
            OperationOutcome::Committed { .. }
            | OperationOutcome::DeterministicTerminalRefuse { .. } => {}
        }
        match self.consult(&key, request_digest)? {
            Some(existing) => Ok(existing.outcome.clone()),
            None => {
                let entry = IdempotencyEntry {
                    key,
                    request_digest,
                    outcome: outcome.clone(),
                };
                self.entries.insert(*key.as_bytes(), entry);
                Ok(outcome)
            }
        }
    }

    /// Safe-retry door: require a key, then lookup/replay.
    pub fn require_key(key: Option<OperationKey>) -> Result<OperationKey, StoreRefuse> {
        key.ok_or(StoreRefuse::MissingIdempotencyToken)
    }
}

#[cfg(test)]
mod composition_crash_replay_tests {
    use super::*;
    use crate::store::commit_cap::SnapshotFork;
    use crate::store::open::{
        EntropyArm, GenesisParams, SizeClass, StableCommitCapArm, StagingTtl, genesis,
    };
    use miette::{IntoDiagnostic, Result, miette};

    #[test]
    fn same_intent_converges_and_replay_is_not_duplicate_effect() -> Result<()> {
        let store_id = genesis(GenesisParams {
            identity_seed: [0x38; 32],
            recovery_matrix: None,
            staging_ttl: StagingTtl::new(1_024),
            size_class: SizeClass::Compact,
            entropy_arm: EntropyArm::OsRandom,
            stable_commit_cap: StableCommitCapArm::NativeFsyncProof {
                snapshot_fork: SnapshotFork::No,
            },
        })
        .store_id();

        let mut composition_bytes = [0u8; 32];
        composition_bytes[..16].copy_from_slice(b"client-op-crash1");
        composition_bytes[16..].copy_from_slice(b"comp-digest-fixe");
        let composition_id = Digest32::admit(composition_bytes);
        let domain = b"kyzo.composition";
        let step = b"step-0";

        let key_pre = OperationKey::derive(domain, composition_id, store_id, step);
        let key_post = OperationKey::derive(domain, composition_id, store_id, step);
        assert_eq!(key_pre, key_post);

        let mut memo = IdempotencyMemo::new();
        let request_digest = IdempotencyMemo::digest_request(b"envelope+schema+authority");
        let first = memo.remember(
            key_pre,
            request_digest,
            OperationOutcome::Committed { request_digest },
        )?;
        let replay = memo.remember(
            key_post,
            request_digest,
            OperationOutcome::Committed { request_digest },
        )?;
        assert_eq!(first, replay);
        assert_eq!(
            memo.lookup(&key_pre),
            OperationOutcome::Committed { request_digest }
        );

        Ok(())
    }
}
