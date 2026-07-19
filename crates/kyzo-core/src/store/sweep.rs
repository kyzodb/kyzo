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
//! [`IntentOrdinal`], [`CommitOrdinal`], [`Applied`], [`Committed`].
//!
//! Also owns the live [`RootChain`](super::merkle::RootChain) on the door:
//! each [`Committed`] mint extends it via [`RootChain::append`](super::merkle::RootChain::append);
//! [`Applied`] never extends history-authoritative roots (§25 / §56).
//!
//! Bans: timers (wake ≠ timer — park only on queue non-empty or shutdown);
//! early ack before the batch barrier; refuse-as-durable-event to fill
//! ordinal holes; cut advancement on ghosts; seal without session recheck
//! against the Store's **current** live session; soft dual-mint of one
//! proof type across fsync and non-fsync paths; out-of-order IntentOrdinal
//! seal.
//!
//! One-door transition: [`SweepDoor`] wraps existing [`WriteTx`]
//! `commit` / `commit_durable` as the first [`StableCommitCap`] arm's
//! physical apply. Two proof types for two durability strengths:
//! [`Applied`] (process-crash) from [`SweepDoor::seal`], [`Committed`]
//! (backing fsync) from [`SweepDoor::seal_durable`] — mint sites only here.

use std::collections::VecDeque;

use super::authority::{IncarnationId, WriteAuthority};
use super::commit_cap::StableCommitCap;
use super::epoch::FenceEpoch;
use super::merkle::{ChainLinkKind, ChainedStateRoot, MerkleChainRefuse, RootChain, StateRoot};
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

/// Proof that an Open write transaction applied through the SweepDoor without
/// a backing fsync (process-crash durable, not power-cut durable).
///
/// Distinct from [`Committed`]: soft dual-mint of one proof type across
/// durability strengths is banned (decisions.md §25). Carries no
/// [`CommitOrdinal`] — history ordinals mint only at the durable event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use]
pub struct Applied {
    store_id: StoreId,
    fence_epoch: FenceEpoch,
    intent_ordinal: IntentOrdinal,
}

impl Applied {
    /// Store identity this apply sealed under.
    pub fn store_id(self) -> StoreId {
        self.store_id
    }

    /// Fence epoch at the apply event.
    pub fn fence_epoch(self) -> FenceEpoch {
        self.fence_epoch
    }

    /// Intent ordinal that applied (contention carriage — not history).
    pub fn intent_ordinal(self) -> IntentOrdinal {
        self.intent_ordinal
    }

    /// Sole mint site for [`Applied`] — the SweepDoor non-fsync seal path.
    fn mint(store_id: StoreId, fence_epoch: FenceEpoch, intent_ordinal: IntentOrdinal) -> Self {
        Self {
            store_id,
            fence_epoch,
            intent_ordinal,
        }
    }
}

/// Proof that an Open write transaction committed through the SweepDoor after
/// the backing fsync (power-cut durable).
///
/// Carries Store identity, fence epoch, and dense [`CommitOrdinal`]. Private
/// fields — construction sites only inside this module (the ruled door).
/// Mintable only after the StableCommitCap barrier returns (§25).
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

    /// Sole mint site for [`Committed`] — the SweepDoor durable seal path.
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
///
/// Open-time snapshot on the door is **not** live authority — admit/seal
/// paths take a `current: &SweepSession` so a superseded session fails
/// [`SweepRefuse::WriteSessionDead`].
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
/// fsync proof) for [`Committed`]; [`WriteTx::commit`] for [`Applied`].
pub struct SweepDoor {
    store_id: StoreId,
    fence_epoch: FenceEpoch,
    /// Session this door opened under — compared to live `current` on every
    /// admit/seal so a superseded live session cannot resurrect this door.
    session: SweepSession,
    /// Affine WriteAuthority — authorizes this door only.
    _write_authority: WriteAuthority,
    stable_commit_cap: StableCommitCap,
    queue: IntentionQueue,
    next_intent: IntentOrdinal,
    /// Highest sealed IntentOrdinal among successful seals (strictly increasing).
    last_sealed_intent: Option<IntentOrdinal>,
    /// Highest sealed CommitOrdinal (dense floor); next durable seal assigns successor.
    highest_commit: CommitOrdinal,
    /// Predecessor-hash chain head over CommitOrdinal (sole history seal).
    predecessor_hash: [u8; 32],
    /// Spec §56 chained state roots — extended only at [`Self::seal_durable`]
    /// (`Committed` mint). Prior tip is stored here so accountability is not
    /// cold on-demand only. Never extended by [`Self::seal`] / [`Applied`].
    root_chain: RootChain,
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
            last_sealed_intent: None,
            highest_commit: CommitOrdinal::ZERO,
            predecessor_hash: [0u8; 32],
            root_chain: RootChain::empty(),
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

    /// Last successfully sealed IntentOrdinal, if any.
    pub fn last_sealed_intent_ordinal(&self) -> Option<IntentOrdinal> {
        self.last_sealed_intent
    }

    /// Stored per-commit [`RootChain`] (tip = prior root for the next mint).
    pub fn root_chain(&self) -> &RootChain {
        &self.root_chain
    }

    /// Prior [`StateRoot`] the next `Committed` mint must cover (or genesis).
    pub fn prior_root(&self) -> StateRoot {
        self.root_chain.prior_root()
    }

    /// Admit an intent: mint Store-monotonic IntentOrdinal (may gap later).
    ///
    /// `current` is the Store's live session authority — not the open-time
    /// snapshot alone. A superseded live session → [`SweepRefuse::WriteSessionDead`].
    pub fn admit(
        &mut self,
        incarnation_id: IncarnationId,
        current: &SweepSession,
    ) -> Result<AdmittedIntent, SweepRefuse> {
        self.recheck_session(incarnation_id, current)?;
        let intent_ordinal = self.next_intent;
        self.next_intent = intent_ordinal.successor()?;
        let intent = AdmittedIntent {
            intent_ordinal,
            store_id: self.store_id,
            fence_epoch: current.fence_epoch(),
            incarnation_id,
        };
        self.queue.push(intent.clone());
        Ok(intent)
    }

    /// Sweep one intent through the StableCommitCap barrier (backing fsync).
    ///
    /// Physical apply for the first arm: `WriteTx::commit_durable`.
    /// Assigns dense [`CommitOrdinal`] only on success; refuses advance no cut
    /// on failure. Session recheck against live `current` before any seal —
    /// mismatch seals zero bytes. Intents must seal in strictly increasing
    /// [`IntentOrdinal`] order.
    ///
    /// On success, mints a Spec [`ChainedStateRoot`] over `content_root`
    /// (plaintext-canonical digest at this cut) and extends [`RootChain`] via
    /// [`RootChain::append`]. History-authoritative roots extend only here —
    /// never on [`Self::seal`] / [`Applied`] (§25 / §56).
    pub fn seal_durable<W: WriteTx>(
        &mut self,
        intent: AdmittedIntent,
        tx: W,
        content_root: StateRoot,
        current: &SweepSession,
    ) -> Result<Committed, SweepSealFailure> {
        self.prepare_seal(&intent, current)?;

        // Barrier IS the arm's commit proof — first arm: native fsync apply.
        match self.stable_commit_cap {
            StableCommitCap::NativeFsyncProof { .. }
            | StableCommitCap::PlatformTransactionProof { .. } => {
                tx.commit_durable().map_err(SweepSealFailure::Apply)?;
            }
        }

        self.note_sealed_intent(intent.intent_ordinal());
        let commit_ordinal = self
            .highest_commit
            .successor()
            .map_err(SweepSealFailure::Sweep)?;
        self.predecessor_hash =
            seal_predecessor_hash(self.predecessor_hash, self.store_id, commit_ordinal);
        self.highest_commit = commit_ordinal;

        // Sole Committed mint site for the Spec chain (§56): bind content to
        // the stored prior tip, then append so the next seal sees it.
        let chained = ChainedStateRoot::mint(
            self.store_id,
            self.fence_epoch,
            commit_ordinal,
            content_root,
            self.root_chain.prior_root(),
            ChainLinkKind::Ordinary,
        );
        self.root_chain
            .append(chained)
            .map_err(SweepSealFailure::MerkleChain)?;

        Ok(Committed::mint(
            self.store_id,
            self.fence_epoch,
            commit_ordinal,
        ))
    }

    /// Sweep one intent through a process-crash-durable (non-fsync) barrier.
    ///
    /// Returns [`Applied`] — never [`Committed`]. Soft dual-mint of one proof
    /// type across durability strengths is banned. Does not assign
    /// [`CommitOrdinal`] and does not extend [`RootChain`] (history-authoritative
    /// roots mint only at the durable / [`Committed`] event — §25 / §56).
    pub fn seal<W: WriteTx>(
        &mut self,
        intent: AdmittedIntent,
        tx: W,
        current: &SweepSession,
    ) -> Result<Applied, SweepSealFailure> {
        self.prepare_seal(&intent, current)?;

        tx.commit().map_err(SweepSealFailure::Apply)?;

        self.note_sealed_intent(intent.intent_ordinal());
        Ok(Applied::mint(
            self.store_id,
            self.fence_epoch,
            intent.intent_ordinal(),
        ))
    }

    /// Shared pre-apply law: live session recheck + intent identity + intent order.
    fn prepare_seal(
        &self,
        intent: &AdmittedIntent,
        current: &SweepSession,
    ) -> Result<(), SweepSealFailure> {
        self.recheck_session(intent.incarnation_id(), current)
            .map_err(SweepSealFailure::Sweep)?;
        if intent.store_id() != self.store_id {
            return Err(SweepSealFailure::Sweep(SweepRefuse::SessionStoreMismatch));
        }
        if intent.fence_epoch() != current.fence_epoch() {
            return Err(SweepSealFailure::Sweep(SweepRefuse::WriteSessionDead));
        }
        self.check_intent_order(intent.intent_ordinal())
            .map_err(SweepSealFailure::Sweep)?;
        Ok(())
    }

    /// Recheck ask + door against the Store's **current** live session.
    ///
    /// Comparing only to the open-time snapshot lets a stale door pass its
    /// own recheck — resurrection cannot fire. Live `current` must still
    /// equal the session this door opened under; ask incarnation must match
    /// that live authority.
    fn recheck_session(
        &self,
        incarnation_id: IncarnationId,
        current: &SweepSession,
    ) -> Result<(), SweepRefuse> {
        if current != &self.session {
            return Err(SweepRefuse::WriteSessionDead);
        }
        if incarnation_id != current.incarnation_id()
            || current.fence_epoch() != self.fence_epoch
            || current.store_id() != self.store_id
        {
            return Err(SweepRefuse::WriteSessionDead);
        }
        Ok(())
    }

    fn check_intent_order(&self, intent_ordinal: IntentOrdinal) -> Result<(), SweepRefuse> {
        if let Some(last) = self.last_sealed_intent
            && intent_ordinal <= last
        {
            return Err(SweepRefuse::IntentOrderRegression);
        }
        Ok(())
    }

    fn note_sealed_intent(&mut self, intent_ordinal: IntentOrdinal) {
        self.last_sealed_intent = Some(intent_ordinal);
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
    #[error(
        "IntentOrderRegression: seal IntentOrdinal must strictly increase among successful seals"
    )]
    #[diagnostic(code(store::sweep::intent_order_regression))]
    IntentOrderRegression,
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
    /// Spec root-chain append refused (predecessor mismatch).
    #[error(transparent)]
    #[diagnostic(transparent)]
    MerkleChain(MerkleChainRefuse),
}
