/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The ruled commit door (decisions.md §25).
//!
//! Owns: [`SweepDoor`], [`IntentionQueue`], [`AdmittedIntent`],
//! [`IntentOrdinal`], [`CommitOrdinal`], [`Committed`].
//!
//! Bans: timers (wake ≠ timer — park only on queue non-empty or shutdown);
//! early ack before the batch barrier; refuse-as-durable-event to fill
//! ordinal holes; cut advancement on ghosts; seal without session recheck.
//!
//! One-door transition: [`SweepDoor`] wraps existing [`WriteTx`]
//! `commit` / `commit_durable` as the first [`StableCommitCap`] arm's
//! physical apply. [`Committed`] is minted only here — never from adapter
//! `WriteTx` impls.

use std::collections::VecDeque;

use super::authority::{IncarnationId, WriteAuthority};
use super::commit_cap::StableCommitCap;
use super::epoch::FenceEpoch;
use super::open::StoreId;
use super::tx::{CommitFailure, WriteTx};

/// Store-monotonic contention ordinal. Minted at admission; may gap freely
/// (conflicts, capacity refuses, cancels). Never sealed history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IntentOrdinal(u64);

impl IntentOrdinal {
    /// First intent ordinal.
    pub const ZERO: IntentOrdinal = IntentOrdinal(0);

    /// Wrap an already-proven ordinal (decode / test sites that hold the proof).
    pub(crate) fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Raw ordinal value.
    pub fn get(self) -> u64 {
        self.0
    }

    /// Strict successor. Refuses at `u64::MAX`.
    pub fn successor(self) -> Result<IntentOrdinal, SweepRefuse> {
        self.0
            .checked_add(1)
            .map(IntentOrdinal)
            .ok_or(SweepRefuse::IntentOrdinalExhausted)
    }
}

/// Dense history ordinal. Assigned only inside the SweepDoor at the durable
/// event, in IntentOrdinal order among successes. Predecessor-hash seals it —
/// sole logical history authority. Never minted at admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommitOrdinal(u64);

impl CommitOrdinal {
    /// Genesis / pre-first-commit floor (no sealed history yet).
    pub const ZERO: CommitOrdinal = CommitOrdinal(0);

    /// Wrap an already-proven ordinal (WAL / seal decode).
    pub(crate) fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Raw ordinal value.
    pub fn get(self) -> u64 {
        self.0
    }

    /// Dense successor. Refuses at `u64::MAX`.
    pub fn successor(self) -> Result<CommitOrdinal, SweepRefuse> {
        self.0
            .checked_add(1)
            .map(CommitOrdinal)
            .ok_or(SweepRefuse::CommitOrdinalExhausted)
    }
}

/// Proof that an Open write transaction committed through the SweepDoor.
///
/// Carries Store identity, fence epoch, and dense [`CommitOrdinal`]. Private
/// fields — construction sites only inside this module (the ruled door).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use]
pub struct Committed {
    store_id: StoreId,
    fence_epoch: FenceEpoch,
    commit_ordinal: CommitOrdinal,
}

impl Committed {
    /// Store identity this commit sealed under.
    pub fn store_id(self) -> StoreId {
        self.store_id
    }

    /// Fence epoch at the durable event.
    pub fn fence_epoch(self) -> FenceEpoch {
        self.fence_epoch
    }

    /// Dense history ordinal assigned at the durable event.
    pub fn commit_ordinal(self) -> CommitOrdinal {
        self.commit_ordinal
    }

    /// Sole mint site for [`Committed`] — the SweepDoor seal path.
    fn mint(store_id: StoreId, fence_epoch: FenceEpoch, commit_ordinal: CommitOrdinal) -> Self {
        Self {
            store_id,
            fence_epoch,
            commit_ordinal,
        }
    }
}

/// An intent admitted to the IntentionQueue — carries only [`IntentOrdinal`].
///
/// `CommitOrdinal` construction at admission is Unconstructible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedIntent {
    intent_ordinal: IntentOrdinal,
    store_id: StoreId,
    fence_epoch: FenceEpoch,
    incarnation_id: IncarnationId,
}

impl AdmittedIntent {
    /// Intent ordinal (contention carriage — may gap).
    pub fn intent_ordinal(&self) -> IntentOrdinal {
        self.intent_ordinal
    }

    /// Store this intent targets.
    pub fn store_id(&self) -> StoreId {
        self.store_id
    }

    /// Fence epoch presented at admission.
    pub fn fence_epoch(&self) -> FenceEpoch {
        self.fence_epoch
    }

    /// Session incarnation presented at admission.
    pub fn incarnation_id(&self) -> IncarnationId {
        self.incarnation_id
    }
}

/// LMAX-shaped intention queue. Wake ≠ timer: park only on non-empty or shutdown.
#[derive(Debug, Default)]
pub struct IntentionQueue {
    intents: VecDeque<AdmittedIntent>,
}

impl IntentionQueue {
    /// Empty queue.
    pub fn new() -> Self {
        Self {
            intents: VecDeque::new(),
        }
    }

    /// Whether the queue has work (the only legal wake condition besides shutdown).
    pub fn is_empty(&self) -> bool {
        self.intents.is_empty()
    }

    /// Number of waiting intents.
    pub fn len(&self) -> usize {
        self.intents.len()
    }

    /// Push an admitted intent (carriage — never order authority for history).
    pub fn push(&mut self, intent: AdmittedIntent) {
        self.intents.push_back(intent);
    }

    /// Pop the next intent in contention order, if any.
    pub fn pop(&mut self) -> Option<AdmittedIntent> {
        self.intents.pop_front()
    }
}

/// Session coordinates the SweepDoor rechecks before sealing any batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepSession {
    store_id: StoreId,
    fence_epoch: FenceEpoch,
    incarnation_id: IncarnationId,
}

impl SweepSession {
    /// Live write-session coordinates.
    pub fn new(store_id: StoreId, fence_epoch: FenceEpoch, incarnation_id: IncarnationId) -> Self {
        Self {
            store_id,
            fence_epoch,
            incarnation_id,
        }
    }

    /// Store identity.
    pub fn store_id(self) -> StoreId {
        self.store_id
    }

    /// Current fence epoch.
    pub fn fence_epoch(self) -> FenceEpoch {
        self.fence_epoch
    }

    /// Current live incarnation.
    pub fn incarnation_id(self) -> IncarnationId {
        self.incarnation_id
    }
}

/// The ruled commit door: single commit thread, continuous sweep → one
/// [`StableCommitCap`] barrier → sweep again.
///
/// First arm: physical apply = today's [`WriteTx::commit_durable`] (native
/// fsync proof). [`Committed`] mint lives only here.
pub struct SweepDoor {
    store_id: StoreId,
    fence_epoch: FenceEpoch,
    session: SweepSession,
    /// Affine WriteAuthority — authorizes this door only.
    _write_authority: WriteAuthority,
    stable_commit_cap: StableCommitCap,
    queue: IntentionQueue,
    next_intent: IntentOrdinal,
    /// Highest sealed CommitOrdinal (dense floor); next seal assigns successor.
    highest_commit: CommitOrdinal,
    /// Predecessor-hash chain head over CommitOrdinal (sole history seal).
    predecessor_hash: [u8; 32],
}

impl SweepDoor {
    /// Open the door for one Store under WriteAuthority + sealed arm + session.
    pub fn open(
        store_id: StoreId,
        fence_epoch: FenceEpoch,
        session: SweepSession,
        write_authority: WriteAuthority,
        stable_commit_cap: StableCommitCap,
    ) -> Result<Self, SweepRefuse> {
        if session.store_id() != store_id {
            return Err(SweepRefuse::SessionStoreMismatch);
        }
        if write_authority.store_id() != store_id {
            return Err(SweepRefuse::AuthorityStoreMismatch);
        }
        if session.fence_epoch() != fence_epoch {
            return Err(SweepRefuse::WriteSessionDead);
        }
        Ok(Self {
            store_id,
            fence_epoch,
            session,
            _write_authority: write_authority,
            stable_commit_cap,
            queue: IntentionQueue::new(),
            next_intent: IntentOrdinal::ZERO,
            highest_commit: CommitOrdinal::ZERO,
            predecessor_hash: [0u8; 32],
        })
    }

    /// Sealed StableCommitCap arm this door barriers through.
    pub fn stable_commit_cap(&self) -> StableCommitCap {
        self.stable_commit_cap
    }

    /// Intention queue (carriage).
    pub fn queue(&self) -> &IntentionQueue {
        &self.queue
    }

    /// Highest sealed CommitOrdinal (dense history floor).
    pub fn highest_commit_ordinal(&self) -> CommitOrdinal {
        self.highest_commit
    }

    /// Admit an intent: mint Store-monotonic IntentOrdinal (may gap later).
    pub fn admit(&mut self, incarnation_id: IncarnationId) -> Result<AdmittedIntent, SweepRefuse> {
        self.recheck_session(incarnation_id)?;
        let intent_ordinal = self.next_intent;
        self.next_intent = intent_ordinal.successor()?;
        let intent = AdmittedIntent {
            intent_ordinal,
            store_id: self.store_id,
            fence_epoch: self.fence_epoch,
            incarnation_id,
        };
        self.queue.push(intent.clone());
        Ok(intent)
    }

    /// Sweep one intent through the StableCommitCap barrier.
    ///
    /// Physical apply for the first arm: `WriteTx::commit_durable`.
    /// Assigns dense [`CommitOrdinal`] only on success; refuses advance no cut
    /// on failure. Session recheck before any seal — mismatch seals zero bytes.
    pub fn seal_durable<W: WriteTx>(
        &mut self,
        intent: AdmittedIntent,
        tx: W,
    ) -> Result<Committed, SweepSealFailure> {
        self.recheck_session(intent.incarnation_id())
            .map_err(SweepSealFailure::Sweep)?;
        if intent.store_id() != self.store_id {
            return Err(SweepSealFailure::Sweep(SweepRefuse::SessionStoreMismatch));
        }
        if intent.fence_epoch() != self.fence_epoch {
            return Err(SweepSealFailure::Sweep(SweepRefuse::WriteSessionDead));
        }

        // Barrier IS the arm's commit proof — first arm: native fsync apply.
        match self.stable_commit_cap {
            StableCommitCap::NativeFsyncProof { .. }
            | StableCommitCap::PlatformTransactionProof { .. } => {
                tx.commit_durable().map_err(SweepSealFailure::Apply)?;
            }
        }

        let commit_ordinal = self
            .highest_commit
            .successor()
            .map_err(SweepSealFailure::Sweep)?;
        self.predecessor_hash =
            seal_predecessor_hash(self.predecessor_hash, self.store_id, commit_ordinal);
        self.highest_commit = commit_ordinal;

        Ok(Committed::mint(
            self.store_id,
            self.fence_epoch,
            commit_ordinal,
        ))
    }

    /// Sweep one intent through a process-crash-durable (non-fsync) barrier.
    ///
    /// Same ordinal / recheck law as [`seal_durable`]; physical apply is
    /// `WriteTx::commit` (survives process crash, not power cut).
    pub fn seal<W: WriteTx>(
        &mut self,
        intent: AdmittedIntent,
        tx: W,
    ) -> Result<Committed, SweepSealFailure> {
        self.recheck_session(intent.incarnation_id())
            .map_err(SweepSealFailure::Sweep)?;
        if intent.store_id() != self.store_id {
            return Err(SweepSealFailure::Sweep(SweepRefuse::SessionStoreMismatch));
        }
        if intent.fence_epoch() != self.fence_epoch {
            return Err(SweepSealFailure::Sweep(SweepRefuse::WriteSessionDead));
        }

        tx.commit().map_err(SweepSealFailure::Apply)?;

        let commit_ordinal = self
            .highest_commit
            .successor()
            .map_err(SweepSealFailure::Sweep)?;
        self.predecessor_hash =
            seal_predecessor_hash(self.predecessor_hash, self.store_id, commit_ordinal);
        self.highest_commit = commit_ordinal;

        Ok(Committed::mint(
            self.store_id,
            self.fence_epoch,
            commit_ordinal,
        ))
    }

    fn recheck_session(&self, incarnation_id: IncarnationId) -> Result<(), SweepRefuse> {
        if incarnation_id != self.session.incarnation_id()
            || self.session.fence_epoch() != self.fence_epoch
            || self.session.store_id() != self.store_id
        {
            return Err(SweepRefuse::WriteSessionDead);
        }
        Ok(())
    }
}

fn seal_predecessor_hash(
    predecessor: [u8; 32],
    store_id: StoreId,
    commit_ordinal: CommitOrdinal,
) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"kyzo.commit_ordinal.seal.v1");
    h.update(predecessor);
    h.update(store_id.as_bytes());
    h.update(u64::to_be_bytes(commit_ordinal.get()));
    h.finalize().into()
}

/// Typed refuse from the SweepDoor (non-SSI family).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum SweepRefuse {
    #[error("WriteSessionDead: incarnation or fence epoch mismatch — zero bytes sealed")]
    #[diagnostic(code(store::sweep::write_session_dead))]
    WriteSessionDead,
    #[error("SweepDoor session StoreId does not match the door's Store")]
    #[diagnostic(code(store::sweep::session_store_mismatch))]
    SessionStoreMismatch,
    #[error("WriteAuthority StoreId does not match the door's Store")]
    #[diagnostic(code(store::sweep::authority_store_mismatch))]
    AuthorityStoreMismatch,
    #[error("IntentOrdinal space exhausted at u64::MAX")]
    #[diagnostic(code(store::sweep::intent_ordinal_exhausted))]
    IntentOrdinalExhausted,
    #[error("CommitOrdinal space exhausted at u64::MAX")]
    #[diagnostic(code(store::sweep::commit_ordinal_exhausted))]
    CommitOrdinalExhausted,
}

/// Seal path refusal: SweepDoor law or physical apply failure.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum SweepSealFailure {
    /// Session / ordinal / authority refuse — zero bytes sealed when session-dead.
    #[error(transparent)]
    #[diagnostic(transparent)]
    Sweep(#[from] SweepRefuse),
    /// Physical StableCommitCap apply failed (SSI conflict / IO / corruption).
    #[error(transparent)]
    #[diagnostic(transparent)]
    Apply(CommitFailure),
}
