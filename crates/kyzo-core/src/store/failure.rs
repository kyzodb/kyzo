/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Failure lattice + closed [`StoreRefuse`] ledger + debt economy (decisions.md
//! §18, §42, §50, §52, §54, §55, §82).
//!
//! Owns: [`StoreRefuse`] (the Refused claim-tag ledger for Store doors),
//! [`FailureLattice`], [`QuarantineRange`], [`mint_quarantine`],
//! [`ScopedMismatchCarriage`] / [`CarriageReport`] (vendor fault carriage into
//! the lattice), debt ledger, [`OperatorCap`], operator health surface.
//!
//! Bans: bool soup instead of the closed lattice; last-known-good serve;
//! quarantine visibility to ordinary tenant queries (§82); silent stall
//! anywhere in the economy; escalating a scoped checksum mismatch into
//! whole-store [`FailureLattice::Poisoned`] (availability inversion).
//!
//! Engine starts-or-not refuses stay in `session/db.rs` (`EngineRefuse`) —
//! never merged into this ledger.

use super::objects::ObjectRef;
use super::open::StoreId;

/// Inclusive byte-range identity for a quarantined keyspace slice (§50).
///
/// Scoped by table/keyspace identity so misplaced-but-intact blocks are
/// caught (§49). Ordinary tenant queries never see quarantine metadata (§82).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QuarantineRange {
    /// Table / keyspace identity the checksum binds (§49).
    keyspace: KeyspaceId,
    /// Inclusive start of the quarantined key bytes.
    start: Vec<u8>,
    /// Inclusive end of the quarantined key bytes.
    end: Vec<u8>,
}

impl QuarantineRange {
    /// Mint a quarantine range (verify_walk / failure doors only).
    pub(crate) fn mint(keyspace: KeyspaceId, start: Vec<u8>, end: Vec<u8>) -> Self {
        Self {
            keyspace,
            start,
            end,
        }
    }

    /// Keyspace identity.
    pub fn keyspace(&self) -> KeyspaceId {
        self.keyspace
    }

    /// Inclusive start key.
    pub fn start(&self) -> &[u8] {
        &self.start
    }

    /// Inclusive end key.
    pub fn end(&self) -> &[u8] {
        &self.end
    }

    /// Whether `key` falls in this inclusive keyspace range.
    pub fn contains(&self, keyspace: KeyspaceId, key: &[u8]) -> bool {
        self.keyspace == keyspace && key >= self.start.as_slice() && key <= self.end.as_slice()
    }
}

/// Mint a quarantine range for carriage into [`FailureLattice`] (§50).
///
/// Public door for scoped checksum-mismatch reports. Never escalates to
/// whole-store [`FailureLattice::Poisoned`].
pub fn mint_quarantine(keyspace: KeyspaceId, start: Vec<u8>, end: Vec<u8>) -> QuarantineRange {
    QuarantineRange::mint(keyspace, start, end)
}

/// Vendor-shaped scoped checksum mismatch carried into the lattice (§50/§52).
///
/// fjall must not import kyzo-core: the boundary maps a scoped fault into this
/// carriage; [`FailureLattice`] is the authority. A scoped mismatch never
/// becomes whole-store Poisoned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedMismatchCarriage {
    range: QuarantineRange,
}

impl ScopedMismatchCarriage {
    /// Build carriage from an already-minted quarantine range.
    pub fn from_range(range: QuarantineRange) -> Self {
        Self { range }
    }

    /// Build carriage by minting the quarantine range.
    pub fn new(keyspace: KeyspaceId, start: Vec<u8>, end: Vec<u8>) -> Self {
        Self {
            range: mint_quarantine(keyspace, start, end),
        }
    }

    /// Quarantine range this carriage reports.
    pub fn range(&self) -> &QuarantineRange {
        &self.range
    }

    /// Map into the lattice as Quarantined — never Poisoned.
    pub fn into_lattice(self) -> FailureLattice {
        FailureLattice::Quarantined {
            ranges: vec![self.range],
        }
    }
}

/// Unknown-invariant carriage — the only path to whole-store Poisoned (§50).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownInvariantCarriage;

impl UnknownInvariantCarriage {
    /// Map into the lattice as Poisoned.
    pub fn into_lattice(self) -> FailureLattice {
        FailureLattice::Poisoned {
            quarantine_retained: None,
        }
    }
}

/// Closed carriage report sum — scoped mismatch vs unknown-invariant (§52).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CarriageReport {
    /// Ordinary scoped block checksum mismatch → quarantine one range.
    ScopedMismatch(ScopedMismatchCarriage),
    /// Unknown invariant → whole-store Poisoned fail-stop.
    UnknownInvariant(UnknownInvariantCarriage),
}

impl CarriageReport {
    /// Lift a carriage report into a lattice fragment.
    pub fn into_lattice(self) -> FailureLattice {
        match self {
            CarriageReport::ScopedMismatch(c) => c.into_lattice(),
            CarriageReport::UnknownInvariant(c) => c.into_lattice(),
        }
    }
}

/// Stable table/keyspace identity for scoped block checksums (§49).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyspaceId(u64);

impl KeyspaceId {
    /// Wrap an already-proven keyspace id.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Raw keyspace discriminant.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Closed failure lattice (§52).
///
/// - Quarantined-only → reads/writes outside ranges OK; quarantined ranges refuse.
/// - Poisoned → operator recover/verify only.
/// - Both → Poisoned dominates while quarantine metadata is retained for diagnosis.
/// - Unknown-invariant → whole-Store Poisoned fail-stop (§50), never silent degrade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureLattice {
    /// No active quarantine or poison.
    Healthy,
    /// Range-scoped quarantine; rest of keyspace stays up.
    Quarantined {
        /// Active quarantine ranges.
        ranges: Vec<QuarantineRange>,
    },
    /// Operator-only recover/verify; optional retained quarantine for diagnosis.
    Poisoned {
        /// Quarantine metadata retained when poison dominates a dual fault.
        quarantine_retained: Option<Vec<QuarantineRange>>,
    },
}

impl FailureLattice {
    /// Apply §52 dominance: Poisoned wins; quarantine metadata retained.
    pub fn combine(self, other: FailureLattice) -> FailureLattice {
        match (self, other) {
            (FailureLattice::Healthy, x) | (x, FailureLattice::Healthy) => x,
            (
                FailureLattice::Quarantined { ranges: a },
                FailureLattice::Quarantined { ranges: mut b },
            ) => {
                let mut ranges = a;
                ranges.append(&mut b);
                FailureLattice::Quarantined { ranges }
            }
            (
                FailureLattice::Poisoned {
                    quarantine_retained,
                },
                FailureLattice::Quarantined { ranges },
            )
            | (
                FailureLattice::Quarantined { ranges },
                FailureLattice::Poisoned {
                    quarantine_retained,
                },
            ) => {
                let retained = match quarantine_retained {
                    Some(mut existing) => {
                        existing.extend(ranges);
                        Some(existing)
                    }
                    None => Some(ranges),
                };
                FailureLattice::Poisoned {
                    quarantine_retained: retained,
                }
            }
            (
                FailureLattice::Poisoned {
                    quarantine_retained: a,
                },
                FailureLattice::Poisoned {
                    quarantine_retained: b,
                },
            ) => FailureLattice::Poisoned {
                quarantine_retained: merge_opt_ranges(a, b),
            },
        }
    }

    /// Absorb a vendor carriage report into this lattice (§50/§52).
    ///
    /// Scoped mismatch → quarantine; unknown-invariant → Poisoned. Ordinary
    /// scoped checksum mismatch never alone yields Poisoned.
    pub fn report(self, carriage: CarriageReport) -> FailureLattice {
        self.combine(carriage.into_lattice())
    }

    /// Admit a key under the lattice: quarantined ranges refuse; Poisoned
    /// refuses all user keys; healthy ranges outside quarantine serve (§50).
    pub fn admit_key(&self, keyspace: KeyspaceId, key: &[u8]) -> Result<(), StoreRefuse> {
        match self {
            FailureLattice::Healthy => Ok(()),
            FailureLattice::Poisoned { .. } => Err(StoreRefuse::OrderedCorrupt),
            FailureLattice::Quarantined { ranges } => {
                for range in ranges {
                    if range.contains(keyspace, key) {
                        return Err(StoreRefuse::Quarantined {
                            range: range.clone(),
                        });
                    }
                }
                Ok(())
            }
        }
    }
}

fn merge_opt_ranges(
    a: Option<Vec<QuarantineRange>>,
    b: Option<Vec<QuarantineRange>>,
) -> Option<Vec<QuarantineRange>> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(x),
        (Some(mut a), Some(b)) => {
            a.extend(b);
            Some(a)
        }
    }
}

/// Store debt economy counters (§42) — silent stall is unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DebtLedger {
    /// Admitted ask debt units currently outstanding.
    outstanding: u64,
    /// Ceiling beyond which admission refuses [`StoreRefuse::StoreDebtExceeded`].
    ceiling: u64,
}

impl DebtLedger {
    /// Empty debt ledger with a capacity ceiling.
    pub fn with_ceiling(ceiling: u64) -> Self {
        Self {
            outstanding: 0,
            ceiling,
        }
    }

    /// Outstanding debt.
    pub fn outstanding(self) -> u64 {
        self.outstanding
    }

    /// Debt ceiling.
    pub fn ceiling(self) -> u64 {
        self.ceiling
    }

    /// Admit `units` of debt, or refuse when the ceiling would be exceeded.
    pub fn admit(&mut self, units: u64) -> Result<(), StoreRefuse> {
        let next = self
            .outstanding
            .checked_add(units)
            .ok_or(StoreRefuse::StoreDebtExceeded)?;
        if next > self.ceiling {
            return Err(StoreRefuse::StoreDebtExceeded);
        }
        self.outstanding = next;
        Ok(())
    }

    /// Release previously admitted debt (saturating at zero).
    pub fn release(&mut self, units: u64) {
        self.outstanding = match self.outstanding.checked_sub(units) {
            Some(n) => n,
            None => 0,
        };
    }
}

/// Unforgeable capability for operator health / quarantine / failure topology (§82).
///
/// Same pattern as [`crate::store::open::StoreOpen`]: private field, mint only
/// via [`OperatorCap::mint`] (`pub(crate)`). Composition-root / host only —
/// ordinary tenant doors have **no path** to mint or pass this token. A free
/// enum audience is not the door: Cap-absent → quarantine unreachable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorCap {
    _private: (),
}

impl OperatorCap {
    /// Mint an operator capability (composition root / host only — like
    /// [`crate::store::open::StoreOpen::mint`]). Not a public constructor.
    pub(crate) fn mint() -> Self {
        Self { _private: () }
    }
}

/// Tenant-blind refuse (§82): quarantine / failure topology are operator-only.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum TenantBlindRefuse {
    /// Tenant tried to select quarantine ranges.
    #[error("tenant-blind: quarantine ranges are operator-only (§82)")]
    #[diagnostic(code(store::refuse::tenant_blind_quarantine))]
    QuarantineTopologyForbidden,
    /// Tenant tried to select failure-lattice topology.
    #[error("tenant-blind: failure topology is operator-only (§82)")]
    #[diagnostic(code(store::refuse::tenant_blind_failure_topology))]
    FailureTopologyForbidden,
}

/// Operator-surface metric refuse — authority absent or Cap door required (§82).
///
/// Wired-complete or typed refuse: never a pub-raw zero standing in for an
/// unbuilt seat-44 / seat-22 / seat-36 feed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum OperatorHealthRefuse {
    /// Seat-44 reclaimable feed not attached (compaction debt authority).
    #[error("operator health: reclaimable authority (seat 44) is not attached")]
    #[diagnostic(code(store::refuse::reclaimable_authority_unbuilt))]
    ReclaimableAuthorityUnbuilt,
    /// Seat-22 staged-object pressure feed not attached.
    #[error("operator health: staged-object pressure authority (seat 22) is not attached")]
    #[diagnostic(code(store::refuse::staged_object_authority_unbuilt))]
    StagedObjectPressureAuthorityUnbuilt,
    /// Seat-36 fence-pressure feed not attached (live Fenced footprints).
    #[error("operator health: fence-pressure authority (seat 36) is not attached")]
    #[diagnostic(code(store::refuse::fence_pressure_authority_unbuilt))]
    FencePressureAuthorityUnbuilt,
    /// No deep-verify has completed — last_verify has nothing to render.
    #[error("operator health: last_verify has never been set (deep-verify never completed)")]
    #[diagnostic(code(store::refuse::last_verify_absent))]
    LastVerifyAbsent,
}

/// Point-in-time storage counters carried on the operator ephemeral surface.
///
/// Distinct from the fjall `StorageStats` type so the failure seat never
/// imports the backend — operators project these as relation rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageStatsSnapshot {
    /// Bytes resident in the block cache.
    pub cache_size_bytes: u64,
    /// Block-cache capacity.
    pub cache_capacity_bytes: u64,
    /// Write-buffer / memtable bytes.
    pub write_buffer_size_bytes: u64,
    /// Compactions currently running.
    pub active_compactions: u64,
    /// Journal / WAL segment count.
    pub journal_count: u64,
}

impl StorageStatsSnapshot {
    /// All counters zero — no storage activity observed yet.
    pub fn empty() -> Self {
        Self {
            cache_size_bytes: 0,
            cache_capacity_bytes: 0,
            write_buffer_size_bytes: 0,
            active_compactions: 0,
            journal_count: 0,
        }
    }
}

/// Ephemeral engine counters queryable as relations on the sealed operator
/// surface (§82): in-flight tx + storage-stats only.
///
/// Compaction-debt renders from the one [`DebtLedger`]; index-status renders
/// from [`crate::session::generation::IndexStatus`] — neither is duplicated
/// here as a second counter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EphemeralEngineState {
    /// Open / in-flight transactions (projection of the live registry).
    in_flight_tx: u64,
    /// Storage stats snapshot.
    storage_stats: StorageStatsSnapshot,
}

impl EphemeralEngineState {
    /// Zero in-flight / empty storage stats.
    pub fn empty() -> Self {
        Self {
            in_flight_tx: 0,
            storage_stats: StorageStatsSnapshot::empty(),
        }
    }

    /// In-flight transaction count.
    pub fn in_flight_tx(&self) -> u64 {
        self.in_flight_tx
    }

    /// Storage-stats snapshot.
    pub fn storage_stats(&self) -> StorageStatsSnapshot {
        self.storage_stats
    }

    /// Operator/wiring door: replace the ephemeral snapshot.
    pub fn replace(&mut self, in_flight_tx: u64, storage_stats: StorageStatsSnapshot) {
        self.in_flight_tx = in_flight_tx;
        self.storage_stats = storage_stats;
    }
}

/// Operator-sealed health surface (§82) — tenant-blind.
///
/// Ephemeral engine state is queryable as relations on this sealed operator
/// door. Debt ledger, last-verify digest, and quarantine are **private**:
/// Cap-gated doors set them; render doors project them. Reclaimable /
/// staged-object / fence-pressure have no pub-raw zero fields — each renders
/// from its real authority or typed-refuses when that authority is unbuilt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorHealthSurface {
    /// Debt ledger — **private**; set only through Cap-gated [`Self::set_debt`].
    debt: DebtLedger,
    /// Seat-44 reclaimable feed (compaction debt) — absent until attached.
    compaction_debt_feed: Option<crate::store::compact::CompactionDebt>,
    /// Active quarantine ranges — **private**; Cap-gated accessor only.
    quarantine: Vec<QuarantineRange>,
    /// Last deep-verify digest — **private**; never a zero-filled `[u8; 32]`.
    last_verify: Option<crate::store::verify_walk::DeepVerifyDigest>,
    /// Ephemeral counters projected as relations (in-flight + storage-stats).
    ephemeral: EphemeralEngineState,
}

impl OperatorHealthSurface {
    /// Empty operator health — zero debt ceiling, no quarantine/verify/feeds.
    pub fn empty() -> Self {
        Self {
            debt: DebtLedger::with_ceiling(0),
            compaction_debt_feed: None,
            quarantine: Vec::new(),
            last_verify: None,
            ephemeral: EphemeralEngineState::empty(),
        }
    }

    /// Borrow the ephemeral engine-state counters.
    pub fn ephemeral(&self) -> &EphemeralEngineState {
        &self.ephemeral
    }

    /// Mutable ephemeral counters (operator / session wiring).
    pub fn ephemeral_mut(&mut self) -> &mut EphemeralEngineState {
        &mut self.ephemeral
    }

    /// Render outstanding debt from the private [`DebtLedger`] (tenant-visible metric).
    pub fn render_debt_outstanding(&self) -> u64 {
        self.debt.outstanding()
    }

    /// Cap-gated write door for the debt ledger (§82 / §44).
    pub fn set_debt(&mut self, _cap: &OperatorCap, debt: DebtLedger) {
        self.debt = debt;
    }

    /// Cap-gated read of the debt ledger.
    pub fn debt(&self, _cap: &OperatorCap) -> DebtLedger {
        self.debt
    }

    /// Attach seat-44 compaction-debt feed (reclaimable authority).
    pub fn set_compaction_debt_feed(
        &mut self,
        _cap: &OperatorCap,
        feed: crate::store::compact::CompactionDebt,
    ) {
        self.compaction_debt_feed = Some(feed);
    }

    /// Render reclaimable bytes from seat-44 compaction debt — or refuse.
    pub fn render_reclaimable(&self, _cap: &OperatorCap) -> Result<u64, OperatorHealthRefuse> {
        self.compaction_debt_feed
            .map(|d| d.reclaimable_bytes)
            .ok_or(OperatorHealthRefuse::ReclaimableAuthorityUnbuilt)
    }

    /// Staged-object pressure (seat 22) — refuse until ObjectStore feed is attached.
    pub fn render_staged_object_pressure(
        &self,
        _cap: &OperatorCap,
    ) -> Result<u64, OperatorHealthRefuse> {
        Err(OperatorHealthRefuse::StagedObjectPressureAuthorityUnbuilt)
    }

    /// Fence pressure (seat 36) — refuse until FootprintIndex feed is attached.
    pub fn render_fence_pressure(&self, _cap: &OperatorCap) -> Result<u64, OperatorHealthRefuse> {
        Err(OperatorHealthRefuse::FencePressureAuthorityUnbuilt)
    }

    /// Cap-gated write of last deep-verify digest.
    pub fn set_last_verify(
        &mut self,
        _cap: &OperatorCap,
        digest: crate::store::verify_walk::DeepVerifyDigest,
    ) {
        self.last_verify = Some(digest);
    }

    /// Render last-verify digest — refuse when never run (never zero-fill).
    pub fn render_last_verify(
        &self,
    ) -> Result<crate::store::verify_walk::DeepVerifyDigest, OperatorHealthRefuse> {
        self.last_verify
            .ok_or(OperatorHealthRefuse::LastVerifyAbsent)
    }

    /// Borrow last-verify when present (for integrity relation rendering).
    pub fn last_verify(&self) -> Option<crate::store::verify_walk::DeepVerifyDigest> {
        self.last_verify
    }

    /// Record a quarantine range on the operator surface (never a tenant door).
    pub fn record_quarantine(&mut self, range: QuarantineRange) {
        self.quarantine.push(range);
    }

    /// Select quarantine ranges — requires [`OperatorCap`] (§82).
    ///
    /// Without Cap this method is unreachable: Cap-absent doors never obtain
    /// an [`OperatorCap`] to pass. The private `quarantine` field is not a
    /// public leak.
    pub fn quarantine_ranges(&self, _cap: &OperatorCap) -> &[QuarantineRange] {
        self.quarantine.as_slice()
    }
}

impl FailureLattice {
    /// Inspect failure topology — requires [`OperatorCap`] (§82).
    ///
    /// Cap-absent callers cannot invoke this; tenant projectors refuse before
    /// reaching here ([`TenantBlindRefuse::FailureTopologyForbidden`]).
    pub fn topology_for(&self, _cap: &OperatorCap) -> &FailureLattice {
        self
    }
}

/// Closed StoreRefuse ledger — the Refused claim-tag ledger in types (§42+).
///
/// Every Store-door refuse in the 07 refused ledger lands here. Session-door
/// admission refuses (footprint / admit / composition) and Engine starts-or-not
/// refuses stay exclusive to their seats — never merged into this enum.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum StoreRefuse {
    /// Open without StoreOpen capability (§5).
    #[error("MissingStoreOpenCapability: path-only open is Unconstructible")]
    #[diagnostic(code(store::refuse::missing_store_open_capability))]
    MissingStoreOpenCapability,

    /// Second writer against a locally fenced address (§2).
    #[error("StoreFenced: second writer against a locally fenced address")]
    #[diagnostic(code(store::refuse::store_fenced))]
    StoreFenced,

    /// Seal attempt from a dead incarnation or epoch (§25/§36).
    #[error("WriteSessionDead: incarnation or fence epoch mismatch — zero bytes sealed")]
    #[diagnostic(code(store::refuse::write_session_dead))]
    WriteSessionDead,

    /// Orphan write after observed RecoveryGrant (§2/§36).
    #[error("AuthorityRecovered: write after observed RecoveryGrant")]
    #[diagnostic(code(store::refuse::authority_recovered))]
    AuthorityRecovered,

    /// Epoch advance while current-epoch Fenced footprints live (§36/§72).
    #[error("EpochAdvanceBlocked: current-epoch Fenced footprints still live")]
    #[diagnostic(code(store::refuse::epoch_advance_blocked))]
    EpochAdvanceBlocked,

    /// Resolve of a Pending object past its cut (§22).
    #[error("Decayed: Pending past expires_at cut")]
    #[diagnostic(code(store::refuse::decayed))]
    Decayed,

    /// Object bytes gone while cut still live (§22).
    #[error("ObjectMissing: bytes gone while cut live")]
    #[diagnostic(code(store::refuse::object_missing))]
    ObjectMissing,

    /// Durable delete without covering retention certificate (§23).
    #[error("ObjectRetainRequired: retention certificate required")]
    #[diagnostic(code(store::refuse::object_retain_required))]
    ObjectRetainRequired,

    /// Cross-Store object resolution (§16).
    #[error("ObjectRefForeignStore: ref/token Store scope mismatch")]
    #[diagnostic(code(store::refuse::object_ref_foreign_store))]
    ObjectRefForeignStore,

    /// StagingToken cousin of ObjectRefForeignStore (§16).
    #[error("StagingTokenForeignStore: staging token Store scope mismatch")]
    #[diagnostic(code(store::refuse::staging_token_foreign_store))]
    StagingTokenForeignStore,

    /// As-of naming a Durable whose retention was violated (§32).
    #[error("ObjectMissingForAsOf: retention violated at as-of")]
    #[diagnostic(code(store::refuse::object_missing_for_as_of))]
    ObjectMissingForAsOf,

    /// Restore/open against a seal failing any bound digest (§26).
    #[error("SealMismatch: bound digest disagreed; never prefer-dump")]
    #[diagnostic(code(store::refuse::seal_mismatch))]
    SealMismatch,

    /// Post-shred restore of a shredded segment (§64/§79).
    #[error("Shredded: typed tombstone for shredded segment")]
    #[diagnostic(code(store::refuse::shredded))]
    Shredded,

    /// Client-supplied transaction time (§30).
    #[error("ClientTxnTimeForbidden: txn time is Store-assigned at the durable event")]
    #[diagnostic(code(store::refuse::client_txn_time_forbidden))]
    ClientTxnTimeForbidden,

    /// Peer/client timestamp reordering another Store's commits (§31).
    #[error("ForeignTxnTime: foreign timestamps do not order local commits")]
    #[diagnostic(code(store::refuse::foreign_txn_time))]
    ForeignTxnTime,

    /// Read continuing past snapshot/pin death (§33).
    #[error("SnapshotExpired: pin budget exhausted")]
    #[diagnostic(code(store::refuse::snapshot_expired))]
    SnapshotExpired,

    /// Admission beyond the debt economy (§42).
    #[error("StoreDebtExceeded: admission beyond the debt ceiling")]
    #[diagnostic(code(store::refuse::store_debt_exceeded))]
    StoreDebtExceeded,

    /// Write at the Store capacity ceiling (§54/§88).
    #[error("StoreFull: capacity ceiling reached")]
    #[diagnostic(code(store::refuse::store_full))]
    StoreFull,

    /// Object backend full (§54).
    #[error("ObjectBackendFull: object backend at capacity")]
    #[diagnostic(code(store::refuse::object_backend_full))]
    ObjectBackendFull,

    /// Read of a quarantined range (§50).
    #[error("Quarantined: range-scoped quarantine refuse")]
    #[diagnostic(code(store::refuse::quarantined))]
    Quarantined {
        /// Quarantined range that refused the read.
        range: QuarantineRange,
    },

    /// Object half of a dual fault (§55) — typed partial beside intact facts.
    #[error("ObjectCorrupt: named broken object refs beside intact ordered facts")]
    #[diagnostic(code(store::refuse::object_corrupt))]
    ObjectCorrupt {
        /// Broken object refs (facts at the cut remain serveable).
        broken: Vec<ObjectRef>,
    },

    /// Ordered half of a dual fault (§55) — quarantine/poison path.
    #[error("OrderedCorrupt: ordered store wrong; objects may be intact")]
    #[diagnostic(code(store::refuse::ordered_corrupt))]
    OrderedCorrupt,

    /// OperationKey reuse with a different request digest (§38).
    #[error("OperationKeyReuse: same key with a changed request digest")]
    #[diagnostic(code(store::refuse::operation_key_reuse))]
    OperationKeyReuse,

    /// Safe-retry door without an idempotency identity (§39).
    #[error("MissingIdempotencyToken: OperationKey required on safe-retry doors")]
    #[diagnostic(code(store::refuse::missing_idempotency_token))]
    MissingIdempotencyToken,

    /// Open without the root KEK capability (§63).
    #[error("MissingRootKek: zero-access open without root KEK")]
    #[diagnostic(code(store::refuse::missing_root_kek))]
    MissingRootKek,

    /// ObjectRef resolution from a deleted Store (§76).
    #[error("StoreDeleted: Store shredded; dangling refs do not resolve")]
    #[diagnostic(code(store::refuse::store_deleted))]
    StoreDeleted {
        /// Deleted Store identity.
        store_id: StoreId,
    },

    /// Unverified foreign dump import (§80).
    #[error("ForeignHistoryUnverified: blind import refused")]
    #[diagnostic(code(store::refuse::foreign_history_unverified))]
    ForeignHistoryUnverified,

    /// Mixed-encoding decode during a format migration (§81).
    #[error("FormatMigrationInProgress: silent mixed-decode is unrepresentable")]
    #[diagnostic(code(store::refuse::format_migration_in_progress))]
    FormatMigrationInProgress,

    /// Second discovery incompatible with an existing materialization (§68).
    #[error("GrantAlreadyMaterialized: grant already yielded a successor")]
    #[diagnostic(code(store::refuse::grant_already_materialized))]
    GrantAlreadyMaterialized {
        /// Existing successor Store identity.
        existing_successor: StoreId,
    },

    /// Fabric insufficient for required movement (§18).
    #[error("FabricUnavailable: fabric insufficient; Kyzo never peer-dials")]
    #[diagnostic(code(store::refuse::fabric_unavailable))]
    FabricUnavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §50/§52: a scoped checksum-mismatch carriage quarantines one range
    /// while another range still serves; whole-store Poisoned only for
    /// unknown-invariant — never a scoped mismatch alone.
    #[test]
    fn scoped_mismatch_quarantines_one_range_while_another_serves() {
        let ks = KeyspaceId::from_raw(1);
        let other_ks = KeyspaceId::from_raw(2);

        let lattice = FailureLattice::Healthy.report(CarriageReport::ScopedMismatch(
            ScopedMismatchCarriage::new(ks, b"a".to_vec(), b"c".to_vec()),
        ));

        // Carriage of a scoped mismatch is Quarantined, never Poisoned.
        assert!(
            matches!(lattice, FailureLattice::Quarantined { .. }),
            "scoped mismatch must quarantine, not poison the store: {lattice:?}"
        );

        // Quarantined range refuses.
        assert!(matches!(
            lattice.admit_key(ks, b"b"),
            Err(StoreRefuse::Quarantined { .. })
        ));
        assert!(matches!(
            lattice.admit_key(ks, b"a"),
            Err(StoreRefuse::Quarantined { .. })
        ));
        assert!(matches!(
            lattice.admit_key(ks, b"c"),
            Err(StoreRefuse::Quarantined { .. })
        ));

        // Healthy sibling range still serves.
        assert!(
            lattice.admit_key(ks, b"z").is_ok(),
            "key outside quarantine must serve"
        );
        assert!(
            lattice.admit_key(ks, b"0").is_ok(),
            "key before quarantine must serve"
        );

        // Different keyspace is unaffected (same key bytes still serve).
        assert!(lattice.admit_key(other_ks, b"b").is_ok());

        // Unknown-invariant alone → whole-store Poisoned; no user key serves.
        let poisoned = FailureLattice::Healthy
            .report(CarriageReport::UnknownInvariant(UnknownInvariantCarriage));
        assert!(matches!(
            poisoned,
            FailureLattice::Poisoned {
                quarantine_retained: None
            }
        ));
        assert!(matches!(
            poisoned.admit_key(ks, b"z"),
            Err(StoreRefuse::OrderedCorrupt)
        ));
    }

    #[test]
    fn mint_quarantine_feeds_carriage_not_poison() {
        let range = mint_quarantine(KeyspaceId::from_raw(7), b"x".to_vec(), b"y".to_vec());
        let lattice = FailureLattice::Healthy.report(CarriageReport::ScopedMismatch(
            ScopedMismatchCarriage::from_range(range),
        ));
        assert!(!matches!(lattice, FailureLattice::Poisoned { .. }));
        assert!(matches!(lattice, FailureLattice::Quarantined { ranges } if ranges.len() == 1));
    }

    /// §82: quarantine / failure topology require [`OperatorCap`] — Cap mint is
    /// composition-root / host only (`OperatorCap::mint`, like `StoreOpen::mint`).
    /// Without Cap there is no door: accessors take `&OperatorCap`, not a free
    /// enum a caller invents. Cap-absent projectors refuse in session/jobs.
    #[test]
    fn quarantine_unreachable_without_operator_cap() {
        let mut surface = OperatorHealthSurface::empty();
        surface.record_quarantine(mint_quarantine(
            KeyspaceId::from_raw(1),
            b"a".to_vec(),
            b"b".to_vec(),
        ));

        // With Cap (crate-local mint — host/composition-root only): operator sees data.
        let cap = OperatorCap::mint();
        let op_ranges = surface.quarantine_ranges(&cap);
        assert_eq!(op_ranges.len(), 1);

        let lattice = FailureLattice::Quarantined {
            ranges: op_ranges.to_vec(),
        };
        assert!(
            lattice
                .topology_for(&cap)
                .admit_key(KeyspaceId::from_raw(1), b"a")
                .is_err()
        );
    }
}
