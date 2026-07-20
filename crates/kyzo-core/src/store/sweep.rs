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
//! [`IntentOrdinal`], [`CommitOrdinal`], [`Applied`], [`Committed`],
//! [`OverlapBatch`].
//!
//! Also owns the live [`RootChain`](super::merkle::RootChain) on the door:
//! each [`Committed`] mint extends it via [`RootChain::append`](super::merkle::RootChain::append);
//! [`Applied`] never extends history-authoritative roots (§25 / §56).
//!
//! **Overlap-only group commit.** A durable barrier batches only writers
//! whose arrival overlaps an in-flight fsync ([`SweepDoor::begin_fsync_window`]
//! … [`SweepDoor::seal_durable_overlap_batch`]). A non-overlapping arrival
//! after that window closes waits for a later barrier and must not appear in
//! the prior [`OverlapBatch`]. Wake ≠ timer: park only on queue non-empty or
//! shutdown — never a coalescing sleep.
//!
//! Bans: timers (wake ≠ timer — park only on queue non-empty or shutdown);
//! early ack before the batch barrier; refuse-as-durable-event to fill
//! ordinal holes; cut advancement on ghosts; seal without session recheck
//! against the Store's **current** live session; soft dual-mint of one
//! proof type across fsync and non-fsync paths; out-of-order IntentOrdinal
//! seal; timer-coalesced batch membership.
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

/// Observable membership of one StableCommitCap barrier under overlap-only
/// group commit (decisions.md §25).
///
/// IntentOrdinals listed here shared exactly one in-flight fsync window.
/// A non-overlapping arrival after that window closed is Unconstructible as
/// a member of this batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlapBatch {
    /// IntentOrdinals that shared the barrier, in intent order.
    members: Vec<IntentOrdinal>,
    /// Dense CommitOrdinals minted at the durable event (parallel to members).
    commit_ordinals: Vec<CommitOrdinal>,
}

impl OverlapBatch {
    /// IntentOrdinals that shared this barrier.
    pub fn members(&self) -> &[IntentOrdinal] {
        &self.members
    }

    /// CommitOrdinals assigned at the durable event (same order as members).
    pub fn commit_ordinals(&self) -> &[CommitOrdinal] {
        &self.commit_ordinals
    }

    /// Whether this barrier's overlap cohort includes `intent`.
    pub fn contains_overlap_member(&self, intent: IntentOrdinal) -> bool {
        self.members.contains(&intent)
    }

    fn from_sealed(members: Vec<IntentOrdinal>, commit_ordinals: Vec<CommitOrdinal>) -> Self {
        debug_assert_eq!(members.len(), commit_ordinals.len());
        Self {
            members,
            commit_ordinals,
        }
    }
}

/// In-flight fsync window: arrivals during `Open` overlap the barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum FsyncWindow {
    /// No barrier in flight — admits land on the IntentionQueue.
    #[default]
    Closed,
    /// Barrier in flight — admits join the overlap cohort for this window.
    Open,
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
    /// Overlap-only fsync window: `Open` means a barrier is in flight.
    fsync_window: FsyncWindow,
    /// Intents whose arrival overlaps the in-flight fsync (carriage for one barrier).
    overlap_cohort: Vec<AdmittedIntent>,
    /// Last completed overlap-only barrier membership (proof observation).
    last_overlap_batch: Option<OverlapBatch>,
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
            fsync_window: FsyncWindow::Closed,
            overlap_cohort: Vec::new(),
            last_overlap_batch: None,
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

    /// Whether an overlap-only fsync window is currently in flight.
    pub fn fsync_window_open(&self) -> bool {
        matches!(self.fsync_window, FsyncWindow::Open)
    }

    /// IntentOrdinals in the current in-flight overlap cohort (empty when closed).
    pub fn overlap_cohort_ordinals(&self) -> impl Iterator<Item = IntentOrdinal> + '_ {
        self.overlap_cohort.iter().map(AdmittedIntent::intent_ordinal)
    }

    /// Last completed overlap-only barrier membership, if any.
    pub fn last_overlap_batch(&self) -> Option<&OverlapBatch> {
        self.last_overlap_batch.as_ref()
    }

    /// Admit an intent: mint Store-monotonic IntentOrdinal (may gap later).
    ///
    /// `current` is the Store's live session authority — not the open-time
    /// snapshot alone. A superseded live session → [`SweepRefuse::WriteSessionDead`].
    ///
    /// While a fsync window is open, the arrival **overlaps** the in-flight
    /// barrier and joins that cohort. After the window closes, admits land on
    /// the IntentionQueue for a later barrier — never the prior overlap batch.
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
        match self.fsync_window {
            FsyncWindow::Open => self.overlap_cohort.push(intent.clone()),
            FsyncWindow::Closed => self.queue.push(intent.clone()),
        }
        Ok(intent)
    }

    /// Open the overlap-only fsync window: queue waiters become the initial
    /// overlap cohort, and further admits join until
    /// [`Self::seal_durable_overlap_batch`] closes the window.
    ///
    /// Wake ≠ timer — this is an explicit barrier compose, never a sleep.
    pub fn begin_fsync_window(
        &mut self,
        incarnation_id: IncarnationId,
        current: &SweepSession,
    ) -> Result<(), SweepRefuse> {
        self.recheck_session(incarnation_id, current)?;
        if matches!(self.fsync_window, FsyncWindow::Open) {
            return Err(SweepRefuse::FsyncWindowAlreadyOpen);
        }
        while let Some(intent) = self.queue.pop() {
            self.overlap_cohort.push(intent);
        }
        self.fsync_window = FsyncWindow::Open;
        Ok(())
    }

    /// Seal the in-flight overlap cohort through **one** StableCommitCap barrier.
    ///
    /// `work` must supply one `(WriteTx, StateRoot)` per overlap cohort member,
    /// in IntentOrdinal order. Assigns dense [`CommitOrdinal`]s in that order,
    /// records [`OverlapBatch`] membership, then **closes** the fsync window so
    /// a non-overlapping arrival cannot join this batch.
    ///
    /// [`Committed`] mints only after the barrier returns (§25).
    pub fn seal_durable_overlap_batch<W: WriteTx>(
        &mut self,
        work: Vec<(W, StateRoot)>,
        current: &SweepSession,
    ) -> Result<(OverlapBatch, Vec<Committed>), SweepSealFailure> {
        if !matches!(self.fsync_window, FsyncWindow::Open) {
            return Err(SweepSealFailure::Sweep(SweepRefuse::FsyncWindowNotOpen));
        }
        if work.len() != self.overlap_cohort.len() {
            return Err(SweepSealFailure::Sweep(SweepRefuse::OverlapCohortMismatch));
        }
        if self.overlap_cohort.is_empty() {
            return Err(SweepSealFailure::Sweep(SweepRefuse::EmptyOverlapCohort));
        }

        for intent in &self.overlap_cohort {
            self.prepare_seal(intent, current)?;
        }
        let cohort = std::mem::take(&mut self.overlap_cohort);

        let mut txs = Vec::with_capacity(work.len());
        let mut content_roots = Vec::with_capacity(work.len());
        for (tx, root) in work {
            txs.push(tx);
            content_roots.push(root);
        }

        // One logical barrier for the whole overlap cohort — first arm applies
        // each physical tx under the same in-flight window, then the window closes.
        // Close the window before returning apply failure so a non-overlapping
        // retry cannot join a half-applied cohort.
        match self.stable_commit_cap {
            StableCommitCap::NativeFsyncProof { .. }
            | StableCommitCap::PlatformTransactionProof { .. } => {
                for tx in txs {
                    if let Err(e) = tx.commit_durable() {
                        self.fsync_window = FsyncWindow::Closed;
                        return Err(SweepSealFailure::Apply(e));
                    }
                }
            }
        }

        let mut members = Vec::with_capacity(cohort.len());
        let mut commit_ordinals = Vec::with_capacity(cohort.len());
        let mut committed = Vec::with_capacity(cohort.len());
        for (intent, content_root) in cohort.into_iter().zip(content_roots) {
            let proof = self.mint_committed_after_barrier(intent.intent_ordinal(), content_root)?;
            members.push(intent.intent_ordinal());
            commit_ordinals.push(proof.commit_ordinal());
            committed.push(proof);
        }

        let batch = OverlapBatch::from_sealed(members, commit_ordinals);
        self.last_overlap_batch = Some(batch.clone());
        self.fsync_window = FsyncWindow::Closed;
        Ok((batch, committed))
    }

    /// Sweep one intent through the StableCommitCap barrier (backing fsync).
    ///
    /// Physical apply for the first arm: `WriteTx::commit_durable`.
    /// Assigns dense [`CommitOrdinal`] only on success; refuses advance no cut
    /// on failure. Session recheck against live `current` before any seal —
    /// mismatch seals zero bytes. Intents must seal in strictly increasing
    /// [`IntentOrdinal`] order.
    ///
    /// Singleton path: refuses while an overlap fsync window is already open
    /// (use [`Self::seal_durable_overlap_batch`]). Records a one-member
    /// [`OverlapBatch`] on success.
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
        if matches!(self.fsync_window, FsyncWindow::Open) {
            return Err(SweepSealFailure::Sweep(SweepRefuse::FsyncWindowAlreadyOpen));
        }
        self.prepare_seal(&intent, current)?;

        // Barrier IS the arm's commit proof — first arm: native fsync apply.
        match self.stable_commit_cap {
            StableCommitCap::NativeFsyncProof { .. }
            | StableCommitCap::PlatformTransactionProof { .. } => {
                tx.commit_durable().map_err(SweepSealFailure::Apply)?;
            }
        }

        let committed = self.mint_committed_after_barrier(intent.intent_ordinal(), content_root)?;
        self.last_overlap_batch = Some(OverlapBatch::from_sealed(
            vec![intent.intent_ordinal()],
            vec![committed.commit_ordinal()],
        ));
        Ok(committed)
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

    /// Post-barrier sole [`Committed`] mint + root-chain append (§25 / §56).
    fn mint_committed_after_barrier(
        &mut self,
        intent_ordinal: IntentOrdinal,
        content_root: StateRoot,
    ) -> Result<Committed, SweepSealFailure> {
        self.note_sealed_intent(intent_ordinal);
        let commit_ordinal = self
            .highest_commit
            .successor()
            .map_err(SweepSealFailure::Sweep)?;
        self.predecessor_hash =
            seal_predecessor_hash(self.predecessor_hash, self.store_id, commit_ordinal);
        self.highest_commit = commit_ordinal;

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
    #[error("FsyncWindowAlreadyOpen: overlap fsync window is already in flight")]
    #[diagnostic(code(store::sweep::fsync_window_already_open))]
    FsyncWindowAlreadyOpen,
    #[error("FsyncWindowNotOpen: overlap seal requires an in-flight fsync window")]
    #[diagnostic(code(store::sweep::fsync_window_not_open))]
    FsyncWindowNotOpen,
    #[error("OverlapCohortMismatch: work items must match the in-flight overlap cohort")]
    #[diagnostic(code(store::sweep::overlap_cohort_mismatch))]
    OverlapCohortMismatch,
    #[error("EmptyOverlapCohort: cannot seal an empty overlap batch")]
    #[diagnostic(code(store::sweep::empty_overlap_cohort))]
    EmptyOverlapCohort,
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

// ── Recovery SLA claim / bench-lane emit (decisions.md §28 / §86) ─────────
//
// Coefficients are **measured** by the power-cut DST corpus
// (`kyzo-trials/src/dst.rs` → path-wired `dst` below). This surface publishes
// the sealed `f` for the bench lane and refuses the durability/SLA *claim*
// when exceeded — never Store open of a recoverable Store.

/// Sealed intercept (ms) of `f(bytes_since_last_flush)`.
///
/// Measured as the corpus p999 residual
/// `recovery_time_ms - bytes_since_last_flush` under slope 1 (story #221 T3).
pub const RECOVERY_SLA_INTERCEPT_MS: u64 = 8;

/// Sealed slope numerator (ms per byte) of `f`.
pub const RECOVERY_SLA_SLOPE_NUM: u64 = 1;

/// Sealed slope denominator of `f`.
pub const RECOVERY_SLA_SLOPE_DEN: u64 = 1;

/// Sealed bound `f(bytes_since_last_flush)` in milliseconds.
#[inline]
pub fn recovery_time_bound_ms(bytes_since_last_flush: u64) -> u64 {
    RECOVERY_SLA_INTERCEPT_MS
        + bytes_since_last_flush.saturating_mul(RECOVERY_SLA_SLOPE_NUM) / RECOVERY_SLA_SLOPE_DEN
}

/// Successful bench-lane emit of the §86 recovery SLA claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoverySlaEmit {
    /// Observed / published recovery-time p999 (ms).
    pub recovery_time_p999_ms: u64,
    /// Dirty-tail bytes the bound is evaluated against.
    pub bytes_since_last_flush: u64,
    /// Sealed `f(bytes_since_last_flush)` at emit time.
    pub bound_ms: u64,
}

/// Refuse the published durability / SLA *claim* — not Store open (§28).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum RecoverySlaClaimRefuse {
    /// Observed p999 exceeds sealed `f(bytes_since_last_flush)`.
    #[error(
        "recovery SLA claim refused: recovery_time_p999={recovery_time_p999_ms}ms \
         exceeds f(bytes_since_last_flush={bytes_since_last_flush})={bound_ms}ms"
    )]
    #[diagnostic(code(store::sweep::recovery_sla_claim_above_bound))]
    AboveBound {
        /// Observed recovery-time p999 (ms).
        recovery_time_p999_ms: u64,
        /// Bytes since last flush at the claim site.
        bytes_since_last_flush: u64,
        /// Sealed bound at those bytes.
        bound_ms: u64,
    },
}

/// Bench-lane emit for the §86 recovery SLA claim.
///
/// When `recovery_time_p999_ms` exceeds sealed `f(bytes_since_last_flush)`,
/// refuses the **claim** (badge / Spec “meets recovery SLA”). Does not gate
/// Store open — recoverability is independent of the marketing bound (§28).
pub fn emit_recovery_sla_claim(
    recovery_time_p999_ms: u64,
    bytes_since_last_flush: u64,
) -> Result<RecoverySlaEmit, RecoverySlaClaimRefuse> {
    let bound_ms = recovery_time_bound_ms(bytes_since_last_flush);
    if recovery_time_p999_ms > bound_ms {
        return Err(RecoverySlaClaimRefuse::AboveBound {
            recovery_time_p999_ms,
            bytes_since_last_flush,
            bound_ms,
        });
    }
    Ok(RecoverySlaEmit {
        recovery_time_p999_ms,
        bytes_since_last_flush,
        bound_ms,
    })
}

/// Overlap-only group-commit proof (story #221 T2) — lives in kyzo-trials
/// `crash.rs` and is path-wired here so the test observes SweepDoor batch
/// membership under the same crate wall as the door (no second commit door).
#[cfg(test)]
#[path = "../../../kyzo-trials/src/crash.rs"]
mod crash;

/// Power-cut / recovery-bound DST corpus (story #221 T3) — lives in
/// kyzo-trials `dst.rs` and is path-wired here so the campaign seals
/// `recovery_time_p999 ≤ f(bytes_since_last_flush)` against the same
/// SweepDoor that mints `Committed` (no second commit door).
#[cfg(test)]
#[path = "../../../kyzo-trials/src/dst.rs"]
mod dst;
