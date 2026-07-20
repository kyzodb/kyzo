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
//! **One boundary, two chains (§24 + §56).** At the durable seal the door
//! advances the WAL byte hash-chain (`WalRecord` / [`WalHash`] tip) and the
//! meaning-layer [`RootChain`] together, then seals a
//! [`DurableCommitCut`](super::merkle::DurableCommitCut) that composes both
//! tips. A third independent predecessor digest is deleted — the WAL tip *is*
//! the CommitOrdinal predecessor-hash seal.
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
use super::merkle::{
    ChainLinkKind, ChainedStateRoot, DurableCommitCut, MerkleChainRefuse, RootChain, StateRoot,
};
use super::open::StoreId;
use super::tx::{CommitFailure, WriteTx};
use super::wal::{
    GENESIS_PREDECESSOR, WalHash, WalPayload, WalRecord, WalRefuse, WalSegment,
};

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
    /// WAL byte-chain tip ([`WalHash`] / replay `final_hash`) — predecessor-hash
    /// seal over CommitOrdinal (seat 24 + 25). Advanced only at durable seal.
    wal_tip: WalHash,
    /// Live WAL segment receiving durable Commit records at this door.
    wal_segment: WalSegment,
    /// Spec §56 chained state roots — extended only at [`Self::seal_durable`]
    /// (`Committed` mint). Prior tip is stored here so accountability is not
    /// cold on-demand only. Never extended by [`Self::seal`] / [`Applied`].
    root_chain: RootChain,
    /// Last composed durable cut (meaning tip × WAL tip) at this door.
    last_durable_cut: Option<DurableCommitCut>,
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
            wal_tip: GENESIS_PREDECESSOR,
            wal_segment: WalSegment::open(store_id, fence_epoch, 0),
            root_chain: RootChain::empty(),
            last_durable_cut: None,
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

    /// WAL byte-chain tip ([`WalHash`] / replay `final_hash`) at this door.
    pub fn wal_final_hash(&self) -> WalHash {
        self.wal_tip
    }

    /// Live WAL segment sealed at this door's durable commits.
    pub fn wal_segment(&self) -> &WalSegment {
        &self.wal_segment
    }

    /// Last composed durable cut (meaning × WAL), if any durable seal occurred.
    pub fn last_durable_cut(&self) -> Option<DurableCommitCut> {
        self.last_durable_cut
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
    /// (plaintext-canonical digest at this cut), extends [`RootChain`] via
    /// [`RootChain::append`], advances the WAL byte hash-chain with a Commit
    /// record whose body binds the meaning tip, and seals a
    /// [`DurableCommitCut`] composing both tips. History-authoritative roots
    /// and WAL tips extend only here — never on [`Self::seal`] / [`Applied`]
    /// (§24 / §25 / §56).
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

    /// Post-barrier sole [`Committed`] mint: RootChain + WAL byte chain +
    /// composed [`DurableCommitCut`] at one durable boundary (§24 / §25 / §56).
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
        self.highest_commit = commit_ordinal;

        // Meaning-layer chain (seat 56).
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

        // WAL byte chain (seat 24): Commit body binds the meaning tip so the
        // byte chain covers the RootChain digest at this ordinal.
        let record = WalRecord::seal(
            self.wal_tip,
            WalPayload::Commit {
                commit_ordinal,
                body: chained.root().as_bytes().to_vec(),
            },
        );
        self.wal_segment
            .append(record.clone())
            .map_err(SweepSealFailure::Wal)?;
        self.wal_tip = record.record_hash();

        // One boundary, two chains — compose both tips.
        self.last_durable_cut = Some(DurableCommitCut::compose(&chained, self.wal_tip));

        Ok(Committed::mint(
            self.store_id,
            self.fence_epoch,
            commit_ordinal,
        ))
    }
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
    /// WAL byte-chain append refused (predecessor / segment law).
    #[error(transparent)]
    #[diagnostic(transparent)]
    Wal(#[from] WalRefuse),
}

// ── Recovery SLA claim / bench-lane emit (decisions.md §28 / §86) ─────────
//
// Coefficients are **derived then sealed** from wall-clock calibration in
// `crates/kyzo-core/benches/recovery_sla.rs` over real
// `kyzo::bench_recovery::replay` / `wal::replay` on MB / tens-of-MB WalSegment
// dirty-tails (§87). Campaign identity:
// - opponent pin `kyzo.recovery_sla.corpus.v2` (1–32 MiB dirty-tails)
// - tagged commit `kyzo.recovery_sla.seal.v2`
// Campaign-derived ceiling (margin 2/1) — sealed from the first honest derive
// printout. Later bench runs re-derive for transparency but assert only
// measured_p999 ≤ f(sealed) and that derived does not exceed sealed (re-seal
// upward on regression; never require bit-stable equality — wall-clock noise).
// Unit is nanoseconds — honest for sub-ms / per-byte replay cost. The
// path-wired DST corpus (`kyzo-trials/src/dst.rs` → `dst` below) proves
// recovery correctness + structural bound *shape* against these sealed numbers
// — it does not invent them and does not equate structural work-units with
// wall-clock. This surface publishes sealed `f` and refuses the durability/SLA
// *claim* when exceeded — never Store open of a recoverable Store.

/// Sealed intercept (ns) of `f(bytes_since_last_flush)`.
///
/// Campaign ceiling from `kyzo.recovery_sla.corpus.v2` /
/// `kyzo.recovery_sla.seal.v2` real `wal::replay` (margin 2/1) — story #221 T3.
/// Bound, not bit-stable equality to every re-derive.
pub const RECOVERY_SLA_INTERCEPT_NS: u64 = 811_352;

/// Sealed slope numerator (ns per byte) of `f` — corpus.v2 / seal.v2 real-replay.
pub const RECOVERY_SLA_SLOPE_NUM: u64 = 2;

/// Sealed slope denominator of `f` — corpus.v2 / seal.v2 real-replay.
pub const RECOVERY_SLA_SLOPE_DEN: u64 = 1;

/// Sealed bound `f(bytes_since_last_flush)` in nanoseconds.
#[inline]
pub fn recovery_time_bound_ns(bytes_since_last_flush: u64) -> u64 {
    RECOVERY_SLA_INTERCEPT_NS
        + bytes_since_last_flush.saturating_mul(RECOVERY_SLA_SLOPE_NUM) / RECOVERY_SLA_SLOPE_DEN
}

/// Successful bench-lane emit of the §86 recovery SLA claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoverySlaEmit {
    /// Observed / published recovery-time p999 (ns).
    pub recovery_time_p999_ns: u64,
    /// Dirty-tail bytes the bound is evaluated against.
    pub bytes_since_last_flush: u64,
    /// Sealed `f(bytes_since_last_flush)` at emit time.
    pub bound_ns: u64,
}

/// Refuse the published durability / SLA *claim* — not Store open (§28).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum RecoverySlaClaimRefuse {
    /// Observed p999 exceeds sealed `f(bytes_since_last_flush)`.
    #[error(
        "recovery SLA claim refused: recovery_time_p999={recovery_time_p999_ns}ns \
         exceeds f(bytes_since_last_flush={bytes_since_last_flush})={bound_ns}ns"
    )]
    #[diagnostic(code(store::sweep::recovery_sla_claim_above_bound))]
    AboveBound {
        /// Observed recovery-time p999 (ns).
        recovery_time_p999_ns: u64,
        /// Bytes since last flush at the claim site.
        bytes_since_last_flush: u64,
        /// Sealed bound at those bytes.
        bound_ns: u64,
    },
}

/// Bench-lane emit for the §86 recovery SLA claim.
///
/// When `recovery_time_p999_ns` exceeds sealed `f(bytes_since_last_flush)`,
/// refuses the **claim** (badge / Spec “meets recovery SLA”). Does not gate
/// Store open — recoverability is independent of the marketing bound (§28).
pub fn emit_recovery_sla_claim(
    recovery_time_p999_ns: u64,
    bytes_since_last_flush: u64,
) -> Result<RecoverySlaEmit, RecoverySlaClaimRefuse> {
    let bound_ns = recovery_time_bound_ns(bytes_since_last_flush);
    if recovery_time_p999_ns > bound_ns {
        return Err(RecoverySlaClaimRefuse::AboveBound {
            recovery_time_p999_ns,
            bytes_since_last_flush,
            bound_ns,
        });
    }
    Ok(RecoverySlaEmit {
        recovery_time_p999_ns,
        bytes_since_last_flush,
        bound_ns,
    })
}

#[cfg(test)]
mod composition_tests {
    //! Prove RootChain × WAL byte-chain meet at [`SweepDoor::seal_durable`].

    use super::*;
    use crate::store::authority::{Entropy, OpenOrdinal};
    use crate::store::commit_cap::{SnapshotFork, StableCommitCap};
    use crate::store::merkle::{DurableCommitCut, GENESIS_ROOT, cuts_equal};
    use crate::store::open::{
        EntropyArm, GenesisParams, SizeClass, StableCommitCapArm, StagingTtl, genesis,
    };
    use crate::store::scratch::TempTx;
    use crate::store::wal::{GENESIS_PREDECESSOR, WalPayload, WalRecord, replay};

    fn open_live_door() -> (SweepDoor, IncarnationId, SweepSession) {
        let sealed = genesis(GenesisParams {
            identity_seed: [0x24; 32],
            recovery_matrix: None,
            staging_ttl: StagingTtl::new(1_024),
            size_class: SizeClass::Compact,
            entropy_arm: EntropyArm::OsRandom,
            stable_commit_cap: StableCommitCapArm::NativeFsyncProof {
                snapshot_fork: false,
            },
        });
        let store_id = sealed.store_id();
        let fence_epoch = sealed.fence_epoch();
        let (_view, auth) = sealed.take_write_authority();
        let incarnation = auth
            .incarnation_mint_cap(OpenOrdinal::ZERO)
            .mint(Entropy::from_bytes([0x56; 32]))
            .expect("incarnation mint");
        let session = SweepSession::new(store_id, fence_epoch, incarnation);
        let cap = StableCommitCap::NativeFsyncProof {
            snapshot_fork: SnapshotFork::No,
        };
        let door = SweepDoor::open(store_id, fence_epoch, session, auth, cap)
            .expect("live SweepDoor");
        (door, incarnation, session)
    }

    fn content_root(tag: u8) -> StateRoot {
        let mut bytes = *GENESIS_ROOT.as_bytes();
        bytes[0] = tag;
        StateRoot::from_digest(bytes)
    }

    /// Load-bearing at the door: seal_durable advances both chains and seals
    /// a composed cut. Breaking the WAL tip bind (or the meaning tip) at the
    /// boundary breaks cut equality against the door's last durable cut.
    #[test]
    fn seal_durable_composes_root_chain_with_wal_byte_chain() {
        let (mut door, incarnation, session) = open_live_door();
        let store_id = session.store_id();

        let intent = door.admit(incarnation, &session).expect("admit");
        let proof = door
            .seal_durable(intent, TempTx::default(), content_root(0xA1), &session)
            .expect("durable seal");

        let cut = door
            .last_durable_cut()
            .expect("composed cut after durable seal");
        assert_eq!(cut.commit_ordinal(), proof.commit_ordinal());
        assert_eq!(cut.wal_final_hash(), door.wal_final_hash());
        assert_eq!(
            door.root_chain().links().last().map(|l| l.root()),
            Some(cut.meaning_root()),
            "meaning tip on RootChain must equal composed cut"
        );

        // Replay of the door's WAL segment reproduces final_hash.
        let recovered = replay(store_id, std::slice::from_ref(door.wal_segment()))
            .expect("WAL replay");
        assert_eq!(
            recovered.final_hash, door.wal_final_hash(),
            "replay final_hash must equal door WAL tip"
        );
        assert_eq!(
            recovered.commit_bodies.len(),
            1,
            "one Commit record at the durable boundary"
        );
        assert_eq!(
            recovered.commit_bodies[0].1.as_slice(),
            cut.meaning_root().as_bytes(),
            "WAL Commit body binds the meaning tip"
        );

        // Recompose from observed tips — equals the door's cut.
        let meaning = door.root_chain().links().last().copied().expect("link");
        let recomposed = DurableCommitCut::compose(&meaning, door.wal_final_hash());
        assert!(
            cuts_equal(cut, recomposed),
            "recomposed cut must equal door cut"
        );

        // Break WAL tip bind: reseal a Commit with a different body under the
        // same predecessor — forged tip ≠ door tip → composed cut breaks.
        let forged_wal = WalRecord::seal(
            GENESIS_PREDECESSOR,
            WalPayload::Commit {
                commit_ordinal: proof.commit_ordinal(),
                body: vec![0xFF; 32],
            },
        )
        .record_hash();
        let wal_broken = DurableCommitCut::compose(&meaning, forged_wal);
        assert!(
            !cuts_equal(cut, wal_broken),
            "breaking WAL bind at the boundary must break composed cut equality"
        );

        // Break meaning tip: remint with different content under same ordinal
        // metadata shape — forged meaning ≠ door meaning → cut breaks.
        let forged_meaning = ChainedStateRoot::mint(
            store_id,
            session.fence_epoch(),
            proof.commit_ordinal(),
            content_root(0xB2),
            GENESIS_ROOT,
            ChainLinkKind::Ordinary,
        );
        let meaning_broken =
            DurableCommitCut::compose(&forged_meaning, door.wal_final_hash());
        assert!(
            !cuts_equal(cut, meaning_broken),
            "breaking meaning bind at the boundary must break composed cut equality"
        );

        // Second durable seal advances both chains again (cross-commit chain).
        let intent2 = door.admit(incarnation, &session).expect("admit 2");
        let _ = door
            .seal_durable(intent2, TempTx::default(), content_root(0xC3), &session)
            .expect("second durable seal");
        let cut2 = door.last_durable_cut().expect("second cut");
        assert!(!cuts_equal(cut, cut2), "second cut must differ from first");
        assert_eq!(door.root_chain().links().len(), 2);
        let recovered2 = replay(store_id, std::slice::from_ref(door.wal_segment()))
            .expect("replay after second seal");
        assert_eq!(recovered2.final_hash, door.wal_final_hash());
        assert_eq!(recovered2.commit_bodies.len(), 2);
    }
}

/// Overlap-only group-commit proof (story #221 T2) — lives in kyzo-trials
/// `crash.rs` and is path-wired here so the test observes SweepDoor batch
/// membership under the same crate wall as the door (no second commit door).
#[cfg(test)]
#[path = "../../../kyzo-trials/src/crash.rs"]
mod crash;

/// Power-cut / recovery-bound + query-path DST corpus (story #221 T3) —
/// lives in kyzo-trials `dst.rs` and is path-wired here so campaigns
/// compile under the crate wall (`pub(crate)` store/exec doors) and the
/// power-cut campaign asserts `recovery_time_p999 ≤ f(bytes_since_last_flush)`
/// against the same SweepDoor that mints `Committed` (no second commit door).
#[cfg(test)]
#[path = "../../../kyzo-trials/src/dst.rs"]
mod dst;
