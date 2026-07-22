/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (`session/observe.rs`, MPL-2.0):
 *
 * - The registry tuple `(BTreeMap<u32, decl>, BTreeMap<name, ids>)` is a
 *   named struct ([`EventCallbackRegistry`]) — the two halves' coherence
 *   (every id in `by_relation` exists in `by_id`) was a convention across
 *   field-`0`/field-`1` accesses; now it is at least nameable and locally
 *   audited (register/unregister/prune are the only mutators).
 * - Channels are std `mpsc` instead of crossbeam (the kernel dropped that
 *   dependency; `fixed_rule/mod.rs` made the same substitution). The
 *   original's optional bounded capacity is gone: a bounded channel made
 *   `send_callbacks` — which runs on the session thread after commit —
 *   block on a slow consumer. Unbounded + lossy-by-disconnect is the whole
 *   contract now, stated below.
 * - `unregister_callback`'s two `unwrap`s on the directory are gone: a
 *   missing directory entry is treated as already-unregistered (law 5).
 * - The retry law (story #3): the collector is built fresh per commit
 *   attempt and delivered only after a successful commit, so a conflicted
 *   attempt can never leak phantom events. The reset lives at the retry
 *   sites in `session/db.rs`; this file's contribution is that
 *   [`CallbackCollector`] is plain data with no channel side effects until
 *   [`Db::send_callbacks`].
 */

//! Post-commit event callbacks: the universe telling its observers what
//! changed.
//!
//! A callback is a channel registered against a relation name. During a
//! mutating query the session *collects* (relation, op, new rows, old
//! rows) into a [`CallbackCollector`] — pure data, no delivery — and only
//! after the transaction commits does [`Db::send_callbacks`] fan the
//! collected events out to the registered channels. Ordering guarantees:
//! events for one commit are delivered after that commit is durable in the
//! process-crash sense, in relation order, in mutation order within a
//! relation.
//!
//! **Lossy by disconnect, documented**: delivery is `send` on an unbounded
//! channel; if the receiver has been dropped the callback is pruned and
//! the event is gone. Callbacks are a notification surface, not a
//! replication log — an observer that must not miss events should read the
//! relation, not trust the channel.
//!
//! ## Deep-verify operator surface (§51)
//!
//! Deep-verify is **operator-scheduled** (never a silent background-only
//! pass with no query door). [`Engine::schedule_deep_verify`] arms a run;
//! [`Engine::run_scheduled_deep_verify`] executes
//! [`crate::store::verify_walk::deep_verify_storage`] and persists the
//! [`DeepVerifyLastResult`] on this registry; [`Engine::deep_verify_last_result`]
//! and [`Engine::deep_verify_staleness`] are the queryable doors.
//!
//! ## Sealed operator health / ephemeral surface (§82)
//!
//! Ephemeral engine state (in-flight tx, compaction-debt, index-status,
//! storage-stats) is queryable as relations via
//! [`Engine::operator_ephemeral_relations`]. Quarantine ranges and failure
//! topology require [`OperatorCap`] on the sealed [`OperatorHealthSurface`]
//! door — Cap-absent (tenant) asks refuse ([`TenantBlindRefuse`]); the
//! Cap-unreachable test is the gate.
//!
//! ## One authoritative counter per metric (§20 / §42 / §44)
//!
//! Each metric has exactly one counter. Exporters hold a [`MetricExporter`]
//! that **renders** [`MetricCounter`] — they never recompute a divergent
//! number. Compaction-debt is witnessed only from [`DebtLedger`]; index-status
//! is witnessed only from [`IndexStatus`] (Catalog generation / staleness).
//! Ephemeral relation rows are projections of those authorities, not a second
//! source of truth.
//!
//! ## Three independently-queryable health tiers
//!
//! [`Liveness`], [`Readiness`], and [`Integrity`] are **three distinct types**
//! with three distinct Engine doors — not one bool wearing three names. Each
//! tier can pass or fail independently of the others.
//!
//! ## Tracing verbosity is behavior-invariant
//!
//! [`TracingVerbosity`] may change diagnostic emission only.
//! [`observe_probe`] proves rows and budget spend are identical at every
//! verbosity — turning tracing up or down never changes result rows or budget.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::sync::mpsc::{Receiver, Sender, channel};

use kyzo_model::value::{DataValue, Tuple};
use smartstring::{LazyCompact, SmartString};

use crate::data::json::NamedRows;
use crate::session::db::Engine;
use crate::session::generation::IndexStatus;
use crate::session::jobs::OperatorEphemeralRelations;
use crate::store::Storage;
use crate::store::failure::{DebtLedger, OperatorCap, OperatorHealthSurface, QuarantineRange};
use crate::store::verify_walk::{DeepVerifyDigest, DeepVerifyReport, deep_verify_storage};

/// One authoritative metric counter. Private field — constructed only by
/// authority doors ([`compaction_debt_counter`], [`index_status_counter`]).
/// Exporters call [`MetricCounter::render`]; there is no recompute path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MetricCounter(u64);

impl MetricCounter {
    /// Render for exporters — the only legal way to obtain the exportable number.
    pub fn render(self) -> RenderedMetric {
        RenderedMetric(self.0)
    }
}

/// A metric value obtained solely by rendering a [`MetricCounter`].
/// An exporter cannot invent or recompute this; it can only hold and emit it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RenderedMetric(u64);

impl RenderedMetric {
    /// The rendered counter value.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// Exporter surface: renders one [`MetricCounter`], never recomputes.
///
/// Construction requires a [`MetricCounter`] from an authority door. There is
/// no API that accepts raw inputs to invent a divergent number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MetricExporter {
    rendered: RenderedMetric,
}

impl MetricExporter {
    /// Bind an exporter to one authoritative counter (render-only).
    pub fn from_counter(counter: MetricCounter) -> Self {
        Self {
            rendered: counter.render(),
        }
    }

    /// Emit the rendered counter value.
    pub fn value(self) -> u64 {
        self.rendered.as_u64()
    }

    /// Borrow the rendered metric (same number as [`Self::value`]).
    pub fn rendered(self) -> RenderedMetric {
        self.rendered
    }
}

/// Compaction-debt counter — sole authority is [`DebtLedger`] (§42/§44).
pub fn compaction_debt_counter(ledger: &DebtLedger) -> MetricCounter {
    MetricCounter(ledger.outstanding())
}

/// Index-status counter — sole authority is [`IndexStatus`] Catalog generation (§20).
pub(crate) fn index_status_counter(status: IndexStatus) -> MetricCounter {
    MetricCounter(status.counter())
}

/// Closed verdict carried inside each health tier — never exposed as "the" health bool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum TierVerdict {
    Passing,
    Failing,
}

/// Liveness tier — process/engine answering. Independently queryable from
/// [`Readiness`] and [`Integrity`] (not one bool with three faces).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Liveness(TierVerdict);

impl Liveness {
    /// Witness a passing liveness tier.
    pub fn passing() -> Self {
        Self(TierVerdict::Passing)
    }

    /// Witness a failing liveness tier.
    pub fn failing() -> Self {
        Self(TierVerdict::Failing)
    }

    /// Whether this liveness tier is passing.
    pub fn is_passing(self) -> bool {
        matches!(self.0, TierVerdict::Passing)
    }

    /// Independently-queryable `liveness` relation — one row, one column.
    pub fn relation(self) -> NamedRows {
        NamedRows::single_bool_column("liveness", self.is_passing())
    }
}

/// Readiness tier — ready to serve work. Independently queryable from
/// [`Liveness`] and [`Integrity`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Readiness(TierVerdict);

impl Readiness {
    /// Witness a passing readiness tier.
    pub fn passing() -> Self {
        Self(TierVerdict::Passing)
    }

    /// Witness a failing readiness tier.
    pub fn failing() -> Self {
        Self(TierVerdict::Failing)
    }

    /// Whether this readiness tier is passing.
    pub fn is_passing(self) -> bool {
        matches!(self.0, TierVerdict::Passing)
    }

    /// Independently-queryable `readiness` relation — one row, one column.
    pub fn relation(self) -> NamedRows {
        NamedRows::single_bool_column("readiness", self.is_passing())
    }
}

/// Integrity tier — storage/data integrity. Independently queryable from
/// [`Liveness`] and [`Readiness`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Integrity(TierVerdict);

impl Integrity {
    /// Witness a passing integrity tier.
    pub fn passing() -> Self {
        Self(TierVerdict::Passing)
    }

    /// Witness a failing integrity tier.
    pub fn failing() -> Self {
        Self(TierVerdict::Failing)
    }

    /// Whether this integrity tier is passing.
    pub fn is_passing(self) -> bool {
        matches!(self.0, TierVerdict::Passing)
    }

    /// Independently-queryable `integrity` relation — one row.
    ///
    /// When a last-verify digest is present it is **rendered** as a second
    /// column (`last_verify` bytes); when absent the column is omitted
    /// (never a zero-filled digest placeholder).
    pub fn relation(self) -> NamedRows {
        NamedRows::single_bool_column("integrity", self.is_passing())
    }

    /// Integrity relation with rendered [`DeepVerifyDigest`] when present.
    pub fn relation_with_last_verify(self, last_verify: Option<DeepVerifyDigest>) -> NamedRows {
        match last_verify {
            Some(digest) => NamedRows::bool_and_bytes_columns(
                "integrity",
                self.is_passing(),
                "last_verify",
                digest.as_bytes().to_vec(),
            ),
            None => self.relation(),
        }
    }
}

/// Three independent health-tier witnesses held on the observe registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct HealthTiers {
    liveness: Liveness,
    readiness: Readiness,
    integrity: Integrity,
}

impl Default for HealthTiers {
    fn default() -> Self {
        // Engine that answers is live and ready; integrity waits on verify.
        Self {
            liveness: Liveness::passing(),
            readiness: Readiness::passing(),
            integrity: Integrity::failing(),
        }
    }
}

/// Tracing detail level. May change diagnostic emission only — never result
/// rows or budget spend ([`observe_probe`] is behavior-invariant in verbosity).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum TracingVerbosity {
    /// Emit no diagnostic detail.
    #[default]
    Silent,
    /// Emit summary diagnostics.
    Summary,
    /// Emit full diagnostic detail.
    Detail,
}

/// Units charged by an observation probe — spent identically at every verbosity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ObservationBudget {
    ceiling: u64,
    spent: u64,
}

impl ObservationBudget {
    /// Fresh budget with the given ceiling and zero spend.
    pub fn with_ceiling(ceiling: u64) -> Self {
        Self { ceiling, spent: 0 }
    }

    /// Units spent so far.
    pub fn spent(self) -> u64 {
        self.spent
    }

    /// Budget ceiling.
    pub fn ceiling(self) -> u64 {
        self.ceiling
    }

    fn charge(&mut self, units: u64) {
        self.spent = self.spent.saturating_add(units).min(self.ceiling);
    }
}

/// Outcome of an observation probe: result rows + budget spend.
///
/// Constructed only by [`observe_probe`] — verbosity cannot reshape these fields.
#[derive(Clone, Debug)]
pub struct ObservationOutcome {
    rows: NamedRows,
    budget_spent: u64,
}

impl ObservationOutcome {
    /// Result rows — identical across all [`TracingVerbosity`] levels.
    pub fn rows(&self) -> &NamedRows {
        &self.rows
    }

    /// Budget units spent — identical across all [`TracingVerbosity`] levels.
    pub fn budget_spent(&self) -> u64 {
        self.budget_spent
    }
}

/// Diagnostic events emitted under a verbosity (side channel — not result rows).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DiagnosticEmission {
    /// How many diagnostic events the verbosity chose to emit.
    pub event_count: usize,
}

/// Run a fixed observation probe under [`TracingVerbosity`].
///
/// **Behavior-invariant:** `rows` and budget charge are taken from the probe
/// arguments only — verbosity changes [`DiagnosticEmission`] alone. Turning
/// tracing up or down never changes result rows or budget spend.
pub fn observe_probe(
    rows: NamedRows,
    charge: u64,
    budget: &mut ObservationBudget,
    verbosity: TracingVerbosity,
) -> (ObservationOutcome, DiagnosticEmission) {
    budget.charge(charge);
    let event_count = match verbosity {
        TracingVerbosity::Silent => 0,
        TracingVerbosity::Summary => 1,
        TracingVerbosity::Detail => 2,
    };
    (
        ObservationOutcome {
            rows,
            budget_spent: budget.spent(),
        },
        DiagnosticEmission { event_count },
    )
}

/// Sync live in-flight count into ephemeral for relation export.
///
/// Compaction-debt / index-status are **not** projected into ephemeral —
/// relations render [`DebtLedger`] / [`IndexStatus`] directly.
fn sync_in_flight_into_ephemeral(surface: &mut OperatorHealthSurface, in_flight: u64) {
    let stats = surface.ephemeral().storage_stats();
    surface.ephemeral_mut().replace(in_flight, stats);
}

/// Persisted outcome of one operator-scheduled deep-verify run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepVerifyLastResult {
    /// Whether walk + every index re-derivation agreed.
    pub clean: bool,
    /// Indexes re-derived in that run.
    pub indices_checked: u64,
    /// Count of index mismatches (re-derived ≠ stored).
    pub mismatch_count: u64,
    /// Stable digest of the full [`DeepVerifyReport`].
    pub digest: DeepVerifyDigest,
    /// Schedule ordinal at which this result was produced.
    pub schedule_ordinal: ScheduleOrdinal,
}

impl DeepVerifyLastResult {
    fn from_report(report: &DeepVerifyReport, schedule_ordinal: ScheduleOrdinal) -> Self {
        Self {
            clean: report.is_clean(),
            indices_checked: report.indices_checked,
            mismatch_count: report.index_mismatches.len() as u64,
            digest: report.digest(),
            schedule_ordinal,
        }
    }
}

/// Queryable staleness of the persisted deep-verify last-result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeepVerifyStaleness {
    /// No deep-verify has ever completed on this Engine.
    NeverRun,
    /// Last result matches the latest completed schedule (nothing pending).
    Fresh { last_result: DeepVerifyLastResult },
    /// Operator has scheduled (or re-scheduled) since the last completed run,
    /// or a run is armed and has never completed.
    Stale {
        last_result: Option<DeepVerifyLastResult>,
        /// Schedule ordinal the operator most recently armed.
        pending_ordinal: ScheduleOrdinal,
    },
}

impl DeepVerifyStaleness {
    /// True when the operator should run (or re-run) deep-verify.
    pub fn is_stale(&self) -> bool {
        matches!(
            self,
            DeepVerifyStaleness::NeverRun | DeepVerifyStaleness::Stale { .. }
        )
    }
}

/// Operator schedule + persisted last-result for deep-verify (§51).
#[derive(Debug, Default)]
struct DeepVerifyOperatorState {
    /// Next schedule ordinal (monotone).
    next_ordinal: u64,
    /// When `Some`, a run is armed at that ordinal.
    pending: Option<ScheduleOrdinal>,
    /// Last completed result, if any.
    last_result: Option<DeepVerifyLastResult>,
}

impl DeepVerifyOperatorState {
    fn schedule(&mut self) -> ScheduleOrdinal {
        self.next_ordinal = self.next_ordinal.saturating_add(1).max(1);
        let ord = ScheduleOrdinal::from_raw(self.next_ordinal);
        self.pending = Some(ord);
        ord
    }

    fn take_pending(&mut self) -> Option<ScheduleOrdinal> {
        self.pending.take()
    }

    fn record(&mut self, result: DeepVerifyLastResult) {
        self.last_result = Some(result);
    }

    fn last_result(&self) -> Option<DeepVerifyLastResult> {
        self.last_result.clone()
    }

    fn staleness(&self) -> DeepVerifyStaleness {
        match (&self.pending, &self.last_result) {
            (None, None) => DeepVerifyStaleness::NeverRun,
            (None, Some(r)) => DeepVerifyStaleness::Fresh {
                last_result: r.clone(),
            },
            (Some(pending), last) => DeepVerifyStaleness::Stale {
                last_result: last.clone(),
                pending_ordinal: *pending,
            },
        }
    }
}

/// Represents the kind of operation that triggered the callback.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CallbackOp {
    /// Triggered by Put operations.
    Put,
    /// Triggered by Rm operations.
    Rm,
}

impl Display for CallbackOp {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl CallbackOp {
    /// Get the string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            CallbackOp::Put => "Put",
            CallbackOp::Rm => "Rm",
        }
    }
}

/// One delivered event: what happened, the new rows, the old rows.
#[derive(Debug, Clone)]
pub struct CallbackEvent {
    /// The mutation kind that produced this event.
    pub op: CallbackOp,
    /// Rows present after the mutation (full key+value for Put; key-only for Rm).
    pub new_rows: NamedRows,
    /// Rows present before the mutation (full key+value).
    pub old_rows: NamedRows,
}

impl CallbackEvent {
    /// Build a delivered event from collector halves.
    pub fn new(op: CallbackOp, new_rows: NamedRows, old_rows: NamedRows) -> Self {
        Self {
            op,
            new_rows,
            old_rows,
        }
    }
}

/// Opaque callback registration identity — never a bare `u32` at the door.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CallbackId(u32);

impl CallbackId {
    /// Wrap a proven id (registry / Engine mint only).
    pub(crate) fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Borrow the raw discriminant (unregister / diagnostics).
    pub fn get(self) -> u32 {
        self.0
    }
}

/// Operator deep-verify schedule ordinal — monotone, never a bare `u64`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScheduleOrdinal(u64);

impl ScheduleOrdinal {
    /// Wrap a proven ordinal (scheduler mint only).
    pub(crate) fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Borrow the raw ordinal.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// One registered callback: the relation it watches and the channel it is
/// delivered on.
pub struct CallbackDeclaration {
    pub(crate) dependent: SmartString<LazyCompact>,
    pub(crate) sender: Sender<CallbackEvent>,
}

/// The events one commit attempt collected, keyed by relation, in mutation
/// order. Plain data: building one has no side effects, so the retry loop
/// discards a conflicted attempt's collector wholesale and no observer
/// ever sees an aborted transaction's events.
///
/// Stored as `(op, new, old)` tuples so admit's push sites stay compatible;
/// [`Db::send_callbacks`] lifts each into a [`CallbackEvent`] at delivery.
pub(crate) type CallbackCollector =
    BTreeMap<SmartString<LazyCompact>, Vec<(CallbackOp, NamedRows, NamedRows)>>;

/// The callback registry: every registered callback by id, and the index
/// from relation name to the ids watching it. The two maps agree by
/// construction — `register`, `unregister`, and the disconnect pruning in
/// [`Db::send_callbacks`] are the only mutators, and each maintains both.
///
/// Also carries the deep-verify operator schedule + persisted last-result
/// (§51) and the sealed [`OperatorHealthSurface`] (§82) — same lock door so
/// observe stays one session observation surface. Index-status authority
/// ([`IndexStatus`]) lives here so ephemeral index rows render one Catalog
/// generation counter, never a second independent u64. Liveness / readiness /
/// integrity are three distinct independently-queryable tiers; tracing
/// verbosity is behavior-invariant for rows and budget.
#[derive(Default)]
pub(crate) struct EventCallbackRegistry {
    pub(crate) by_id: BTreeMap<CallbackId, CallbackDeclaration>,
    pub(crate) by_relation: BTreeMap<SmartString<LazyCompact>, BTreeSet<CallbackId>>,
    deep_verify: DeepVerifyOperatorState,
    /// Sealed operator health + ephemeral engine-state surface (§82).
    operator_health: OperatorHealthSurface,
    /// Index-status authority (§20) — Catalog generation / staleness.
    index_status: IndexStatus,
    /// Live in-flight transaction registry — THE `::running` authority.
    in_flight_tx: u64,
    /// Three independently-queryable health tiers.
    health_tiers: HealthTiers,
    /// Tracing verbosity — diagnostics only; never rows or budget.
    tracing_verbosity: TracingVerbosity,
}

impl EventCallbackRegistry {
    fn register(&mut self, id: CallbackId, decl: CallbackDeclaration) {
        self.by_relation
            .entry(decl.dependent.clone())
            .or_default()
            .insert(id);
        self.by_id.insert(id, decl);
    }

    fn unregister(&mut self, id: CallbackId) -> bool {
        match self.by_id.remove(&id) {
            None => false,
            Some(decl) => {
                if let Some(ids) = self.by_relation.get_mut(&decl.dependent) {
                    ids.remove(&id);
                    if ids.is_empty() {
                        self.by_relation.remove(&decl.dependent);
                    }
                }
                true
            }
        }
    }
}

impl<S: Storage> Engine<S> {
    /// Register a callback channel to receive changes when the requested
    /// relation is successfully committed. The returned id unregisters it.
    ///
    /// (The CozoDB original took an optional bounded capacity; see the
    /// header — delivery is unbounded and lossy-by-disconnect.)
    pub fn register_callback(&self, relation: &str) -> (CallbackId, Receiver<CallbackEvent>) {
        let (sender, receiver) = channel();
        let decl = CallbackDeclaration {
            dependent: SmartString::from(relation),
            sender,
        };
        let mut registry = self
            .event_callbacks
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let new_id = CallbackId::from_raw(self.next_callback_id());
        registry.register(new_id, decl);
        (new_id, receiver)
    }

    /// Unregister a callback; true if it existed.
    pub fn unregister_callback(&self, id: CallbackId) -> bool {
        self.event_callbacks
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .unregister(id)
    }

    /// The relations any callback currently watches: mutation collects
    /// old/new rows only for these (snapshotted once per transaction, so a
    /// registration racing a commit either sees all of it or none of it).
    pub(crate) fn current_callback_targets(&self) -> BTreeSet<SmartString<LazyCompact>> {
        self.event_callbacks
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .by_relation
            .keys()
            .cloned()
            .collect()
    }

    /// Deliver a committed transaction's collected events. Post-commit
    /// only. A send failing means the receiver is gone; the callback is
    /// pruned and the event dropped (lossy by disconnect — the documented
    /// contract).
    pub(crate) fn send_callbacks(&self, collector: CallbackCollector) {
        let mut to_remove = vec![];
        {
            let registry = self.event_callbacks.read().unwrap_or_else(|p| p.into_inner());
            for (relation, events) in collector {
                let Some(ids) = registry.by_relation.get(&relation) else {
                    continue;
                };
                for (op, new, old) in events {
                    let event = CallbackEvent::new(op, new, old);
                    for id in ids {
                        if let Some(decl) = registry.by_id.get(id)
                            && decl
                                .sender
                                .send(CallbackEvent {
                                    op: event.op,
                                    new_rows: event.new_rows.clone(),
                                    old_rows: event.old_rows.clone(),
                                })
                                .is_err()
                        {
                            to_remove.push(*id);
                        }
                    }
                }
            }
        }
        if !to_remove.is_empty() {
            let mut registry = self
                .event_callbacks
                .write()
                .unwrap_or_else(|p| p.into_inner());
            for id in to_remove {
                registry.unregister(id);
            }
        }
    }

    /// Arm an operator-scheduled deep-verify (§51). Does not run it —
    /// [`Self::run_scheduled_deep_verify`] does. Returns the schedule ordinal.
    pub fn schedule_deep_verify(&self) -> ScheduleOrdinal {
        self.event_callbacks
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .deep_verify
            .schedule()
    }

    /// Run deep-verify if one is scheduled. Persists
    /// [`DeepVerifyLastResult`] for later query. Returns `Ok(None)` when
    /// nothing is pending.
    pub fn run_scheduled_deep_verify(&self) -> miette::Result<Option<DeepVerifyLastResult>> {
        let ordinal = {
            let mut registry = self
                .event_callbacks
                .write()
                .unwrap_or_else(|p| p.into_inner());
            match registry.deep_verify.take_pending() {
                Some(ord) => ord,
                None => return Ok(None),
            }
        };
        let report = deep_verify_storage(&self.store)?;
        let result = DeepVerifyLastResult::from_report(&report, ordinal);
        {
            let mut registry = self
                .event_callbacks
                .write()
                .unwrap_or_else(|p| p.into_inner());
            registry.health_tiers.integrity = if result.clean {
                Integrity::passing()
            } else {
                Integrity::failing()
            };
            // Cap-gated door for last_verify on the operator surface.
            let cap = OperatorCap::mint();
            registry
                .operator_health
                .set_last_verify(&cap, result.digest);
            registry.deep_verify.record(result.clone());
        }
        Ok(Some(result))
    }

    /// Query the persisted deep-verify last-result, if any run has completed.
    pub fn deep_verify_last_result(&self) -> Option<DeepVerifyLastResult> {
        self.event_callbacks
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .deep_verify
            .last_result()
    }

    /// Query deep-verify staleness relative to the operator schedule.
    pub fn deep_verify_staleness(&self) -> DeepVerifyStaleness {
        self.event_callbacks
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .deep_verify
            .staleness()
    }

    /// Snapshot the sealed [`OperatorHealthSurface`] (§82).
    ///
    /// Ephemeral in-flight is synced from the live registry. Compaction-debt
    /// and index-status are **not** mirrored into ephemeral — relation doors
    /// render [`DebtLedger`] / [`IndexStatus`] directly.
    pub fn operator_health_surface(&self) -> OperatorHealthSurface {
        let registry = self.event_callbacks.read().unwrap_or_else(|p| p.into_inner());
        let mut surface = registry.operator_health.clone();
        sync_in_flight_into_ephemeral(&mut surface, registry.in_flight_tx);
        surface
    }

    /// Replace the sealed operator health surface (operator wiring door).
    ///
    /// Re-syncs ephemeral in-flight from the live registry so a stale
    /// ephemeral cannot disagree with the registry counter.
    pub fn set_operator_health_surface(&self, mut surface: OperatorHealthSurface) {
        let mut registry = self
            .event_callbacks
            .write()
            .unwrap_or_else(|p| p.into_inner());
        sync_in_flight_into_ephemeral(&mut surface, registry.in_flight_tx);
        registry.operator_health = surface;
    }

    /// Set the index-status authority (§20).
    pub(crate) fn set_index_status(&self, status: IndexStatus) {
        let mut registry = self
            .event_callbacks
            .write()
            .unwrap_or_else(|p| p.into_inner());
        registry.index_status = status;
    }

    /// Query the index-status authority (Catalog generation / staleness).
    pub(crate) fn index_status(&self) -> IndexStatus {
        self.event_callbacks
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .index_status
    }

    /// Compaction-debt metric exporter — renders [`DebtLedger`] only (§42/§44).
    pub fn compaction_debt_exporter(&self) -> MetricExporter {
        let surface = self.operator_health_surface();
        let cap = OperatorCap::mint();
        let ledger = surface.debt(&cap);
        MetricExporter::from_counter(compaction_debt_counter(&ledger))
    }

    /// Index-status metric exporter — renders [`IndexStatus`] only (§20).
    pub fn index_status_exporter(&self) -> MetricExporter {
        MetricExporter::from_counter(index_status_counter(self.index_status()))
    }

    /// Record a quarantine range on the sealed operator surface (never a
    /// tenant door). Cap-absent asks still cannot select it — see
    /// [`OperatorHealthSurface::quarantine_ranges`].
    pub fn operator_record_quarantine(&self, range: QuarantineRange) {
        self.event_callbacks
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .operator_health
            .record_quarantine(range);
    }

    /// Cap-absent ephemeral engine-state relations (§82).
    ///
    /// Projects in-flight tx / debt / index-status / storage-stats.
    /// Quarantine and failure topology are **unreachable** without
    /// [`OperatorCap`] — this door does not mint or accept Cap.
    pub fn operator_ephemeral_relations(&self) -> OperatorEphemeralRelations {
        OperatorEphemeralRelations::for_tenant(self.operator_health_surface(), self.index_status())
    }

    /// Cap-present operator ephemeral relations (§82).
    ///
    /// Requires unforgeable [`OperatorCap`] (composition-root / host mint
    /// only — like `StoreOpen::mint`). Tenant doors have no path here.
    pub fn operator_ephemeral_relations_for(&self, cap: OperatorCap) -> OperatorEphemeralRelations {
        OperatorEphemeralRelations::for_operator(
            self.operator_health_surface(),
            self.index_status(),
            cap,
        )
    }

    /// Live in-flight-tx registry count — THE `::running` authority.
    pub fn in_flight_tx_count(&self) -> u64 {
        self.event_callbacks
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .in_flight_tx
    }

    /// Register an open transaction on the live in-flight registry.
    ///
    /// SessionTx open/close in `session/db.rs` should call these; until that
    /// wire lands, tests and Cap doors call them explicitly.
    pub fn in_flight_tx_begin(&self) {
        let mut registry = self
            .event_callbacks
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let in_flight = registry.in_flight_tx.saturating_add(1);
        registry.in_flight_tx = in_flight;
        sync_in_flight_into_ephemeral(&mut registry.operator_health, in_flight);
    }

    /// Unregister a closed transaction from the live in-flight registry.
    pub fn in_flight_tx_end(&self) {
        let mut registry = self
            .event_callbacks
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let in_flight = registry.in_flight_tx.saturating_sub(1);
        registry.in_flight_tx = in_flight;
        sync_in_flight_into_ephemeral(&mut registry.operator_health, in_flight);
    }

    /// `::running` via the live in-flight-tx registry (never a default-zero surface).
    pub fn list_running_jobs(&self) -> miette::Result<NamedRows> {
        crate::session::jobs::list_running_from(self.in_flight_tx_count())
    }

    /// Query the liveness tier — independently of readiness and integrity.
    pub fn liveness(&self) -> Liveness {
        self.event_callbacks
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .health_tiers
            .liveness
    }

    /// Query the readiness tier — independently of liveness and integrity.
    pub fn readiness(&self) -> Readiness {
        self.event_callbacks
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .health_tiers
            .readiness
    }

    /// Query the integrity tier — independently of liveness and readiness.
    ///
    /// The relation **renders** [`DeepVerifyDigest`] from the private
    /// last_verify field when a deep-verify has completed; otherwise the
    /// digest column is absent (never zero-filled).
    pub fn integrity(&self) -> Integrity {
        self.event_callbacks
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .health_tiers
            .integrity
    }

    /// Operator integrity relation with rendered last-verify digest.
    pub fn integrity_relation(&self) -> NamedRows {
        let registry = self.event_callbacks.read().unwrap_or_else(|p| p.into_inner());
        registry
            .health_tiers
            .integrity
            .relation_with_last_verify(registry.operator_health.last_verify())
    }

    /// Operator wiring: set the liveness tier without touching readiness/integrity.
    pub fn set_liveness(&self, tier: Liveness) {
        self.event_callbacks
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .health_tiers
            .liveness = tier;
    }

    /// Operator wiring: set the readiness tier without touching liveness/integrity.
    pub fn set_readiness(&self, tier: Readiness) {
        self.event_callbacks
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .health_tiers
            .readiness = tier;
    }

    /// Operator wiring: set the integrity tier without touching liveness/readiness.
    pub fn set_integrity(&self, tier: Integrity) {
        self.event_callbacks
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .health_tiers
            .integrity = tier;
    }

    /// Current tracing verbosity (diagnostics only).
    pub fn tracing_verbosity(&self) -> TracingVerbosity {
        self.event_callbacks
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .tracing_verbosity
    }

    /// Set tracing verbosity — must not change query result rows or budget.
    pub fn set_tracing_verbosity(&self, verbosity: TracingVerbosity) {
        self.event_callbacks
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .tracing_verbosity = verbosity;
    }

    /// Observe a fixed probe under the engine's current tracing verbosity.
    ///
    /// Rows and budget spend are behavior-invariant in verbosity; only
    /// [`DiagnosticEmission`] may differ.
    pub fn observe_under_tracing(
        &self,
        rows: NamedRows,
        charge: u64,
        budget: &mut ObservationBudget,
    ) -> (ObservationOutcome, DiagnosticEmission) {
        let verbosity = self.tracing_verbosity();
        observe_probe(rows, charge, budget, verbosity)
    }
}

#[cfg(test)]
mod exactly_once_battery {
    //! Absorbed from runtime/db_battery.rs (story #350 T2): callback
    //! exactly-once under contention and seeded spurious conflicts.

    use std::collections::BTreeMap;

    use crate::session::catalog::Catalog;
    use crate::session::db::Engine;
    use crate::store::fjall::new_fjall_storage;
    use crate::store::sim::{FaultConfig, SimStorage};
    use kyzo_model::value::DataValue;

    fn no_params() -> BTreeMap<String, DataValue> {
        BTreeMap::new()
    }

    fn open_engine<S: crate::store::Storage>(store: S) -> Engine<S> {
        Engine::compose(store, Catalog::new()).expect("compose engine")
    }

    /// The phantom-event law, actually exercised: a callback registered on a
    /// contended counter must deliver exactly one Put event per COMMITTED
    /// increment — the new values are exactly {1..=N}, no duplicates from
    /// conflicted-and-retried attempts, none missing.
    #[test]
    fn rs3_callbacks_exactly_once_under_contention() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_engine(new_fjall_storage(dir.path()).unwrap());
        db.run_script("?[k, v] <- [[0, 0]] :create ctr {k => v}", no_params())
            .expect("create counter");

        let (_id, receiver) = db.register_callback("ctr");

        const PER_THREAD: i64 = 15;
        const THREADS: i64 = 2;
        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                let db = db.clone();
                scope.spawn(move || {
                    for _ in 0..PER_THREAD {
                        db.run_script(
                            "?[k, v] := *ctr[k, old], v = old + 1 :put ctr {k, v}",
                            no_params(),
                        )
                        .expect("increment");
                    }
                });
            }
        });

        let mut new_values: Vec<i64> = vec![];
        while let Ok(event) = receiver.try_recv() {
            assert_eq!(event.op.as_str(), "Put");
            for row in event.new_rows.rows() {
                new_values.push(row[1].get_int().expect("int"));
            }
        }
        new_values.sort();
        let want: Vec<i64> = (1..=THREADS * PER_THREAD).collect();
        assert_eq!(
            new_values, want,
            "exactly one event per committed increment: a conflicted attempt must \
             leak nothing and a committed one must lose nothing"
        );
    }

    /// DETERMINISTIC phantom-event detector: seeded spurious conflicts force the
    /// retry loop to replay commits with no thread races. A collector that is
    /// not rebuilt per attempt (or delivered pre-commit) duplicates events; the
    /// callback stream must be exactly one Put event per committed increment.
    #[test]
    fn rs3_callbacks_exactly_once_under_seeded_spurious_conflicts() {
        let faults = FaultConfig {
            spurious_conflict_ppm: 400_000, // ~40% of commits conflict spuriously
            ..Default::default()
        };
        let db = open_engine(SimStorage::with_faults(77, faults));
        db.run_script("?[k, v] <- [[0, 0]] :create ctr {k => v}", no_params())
            .expect("create counter (retries through spurious conflicts)");

        let (_id, receiver) = db.register_callback("ctr");
        const N: i64 = 20;
        for _ in 0..N {
            db.run_script(
                "?[k, v] := *ctr[k, old], v = old + 1 :put ctr {k, v}",
                no_params(),
            )
            .expect("increment (retries through spurious conflicts)");
        }

        let mut new_values: Vec<i64> = vec![];
        while let Ok(event) = receiver.try_recv() {
            for row in event.new_rows.rows() {
                new_values.push(row[1].get_int().expect("int"));
            }
        }
        new_values.sort();
        let want: Vec<i64> = (1..=N).collect();
        assert_eq!(
            new_values, want,
            "spurious-conflict retries must leak no phantom events and lose none"
        );
    }
}

#[cfg(test)]
mod deep_verify_schedule {
    //! Operator-scheduled deep-verify: last_result + staleness are queryable.

    use std::collections::BTreeMap;

    use crate::session::catalog::Catalog;
    use crate::session::db::Engine;
    use crate::session::observe::DeepVerifyStaleness;
    use crate::store::fjall::new_fjall_storage;
    use kyzo_model::value::DataValue;

    fn no_params() -> BTreeMap<String, DataValue> {
        BTreeMap::new()
    }

    #[test]
    fn scheduled_deep_verify_persists_queryable_last_result_and_staleness() {
        let dir = tempfile::tempdir().unwrap();
        let db = Engine::compose(new_fjall_storage(dir.path()).unwrap(), Catalog::new())
            .expect("compose");
        db.run_script("?[k, v] <- [[1, 7]] :create t {k => v}", no_params())
            .unwrap();
        db.run_script("::index create t:by_v {v}", no_params())
            .unwrap();

        assert!(matches!(
            db.deep_verify_staleness(),
            DeepVerifyStaleness::NeverRun
        ));
        assert!(db.deep_verify_last_result().is_none());

        let ord = db.schedule_deep_verify();
        assert!(ord.get() >= 1);
        assert!(
            db.deep_verify_staleness().is_stale(),
            "armed schedule must report staleness before the run"
        );

        let result = db
            .run_scheduled_deep_verify()
            .expect("deep-verify runs")
            .expect("pending schedule must produce a result");
        assert!(result.clean, "healthy store deep-verify: {result:?}");
        assert!(result.indices_checked >= 1);
        assert_eq!(result.schedule_ordinal, ord);

        let persisted = db.deep_verify_last_result().expect("last_result queryable");
        assert_eq!(persisted, result);
        assert!(matches!(
            db.deep_verify_staleness(),
            DeepVerifyStaleness::Fresh { .. }
        ));

        // Re-schedule → stale again even though a prior result exists.
        db.schedule_deep_verify();
        match db.deep_verify_staleness() {
            DeepVerifyStaleness::Stale {
                last_result: Some(prev),
                pending_ordinal,
            } => {
                assert_eq!(prev.digest, result.digest);
                assert!(pending_ordinal > ord);
            }
            other @ (DeepVerifyStaleness::NeverRun
            | DeepVerifyStaleness::Fresh { .. }
            | DeepVerifyStaleness::Stale {
                last_result: None, ..
            }) => panic!("expected Stale with prior result, got {other:?}"),
        }
        assert!(db.run_scheduled_deep_verify().unwrap().is_some());
        assert!(db.run_scheduled_deep_verify().unwrap().is_none());
    }
}

#[cfg(test)]
mod tenant_blind_operator_surface {
    //! §82: quarantine / failure topology unreachable without OperatorCap.
    //! Cap-absent refuses; Cap (pub(crate) mint) sees data. Not a costume gate.

    use std::collections::BTreeMap;

    use crate::session::catalog::Catalog;
    use crate::session::db::Engine;
    use crate::store::failure::{
        FailureLattice, KeyspaceId, OperatorCap, TenantBlindRefuse, mint_quarantine,
    };
    use crate::store::fjall::new_fjall_storage;
    use kyzo_model::value::DataValue;

    fn no_params() -> BTreeMap<String, DataValue> {
        BTreeMap::new()
    }

    /// Adversarial: quarantine unreachable without Cap — not "pass Tenant → refuse".
    #[test]
    fn quarantine_unreachable_without_operator_cap() {
        let dir = tempfile::tempdir().unwrap();
        let db = Engine::compose(new_fjall_storage(dir.path()).unwrap(), Catalog::new())
            .expect("compose");
        db.run_script("?[k, v] <- [[1, 7]] :create t {k => v}", no_params())
            .unwrap();

        db.operator_record_quarantine(mint_quarantine(
            KeyspaceId::from_raw(3),
            b"lo".to_vec(),
            b"hi".to_vec(),
        ));

        // Cap-absent ordinary door: ephemeral metrics OK, topology unreachable.
        let tenant = db.operator_ephemeral_relations();
        assert!(!tenant.has_operator_cap());
        assert_eq!(
            tenant.in_flight_tx_relation().unwrap().rows()[0][0]
                .get_int()
                .unwrap(),
            0
        );
        assert!(matches!(
            tenant.quarantine_relation(),
            Err(TenantBlindRefuse::QuarantineTopologyForbidden)
        ));

        // With Cap (composition-root mint only — like StoreOpen::mint).
        let cap = OperatorCap::mint();
        let lattice = FailureLattice::Quarantined {
            ranges: db
                .operator_health_surface()
                .quarantine_ranges(&cap)
                .to_vec(),
        };
        assert!(matches!(
            tenant.failure_topology(&lattice),
            Err(TenantBlindRefuse::FailureTopologyForbidden)
        ));

        let op = db.operator_ephemeral_relations_for(cap);
        assert!(op.has_operator_cap());
        assert_eq!(op.quarantine_relation().unwrap().rows().len(), 1);
        assert!(op.failure_topology(&lattice).is_ok());
    }
}

#[cfg(test)]
mod one_counter_per_metric {
    //! Adversarial: one authoritative counter per metric; exporters render,
    //! never recompute a divergent value (§20 / §42 / §44).

    use crate::session::catalog::Catalog;
    use crate::session::db::Engine;
    use crate::session::generation::{
        CatalogGeneration, IndexGeneration, IndexStaleness, IndexStatus, RelationGeneration,
    };
    use crate::session::observe::{MetricExporter, compaction_debt_counter, index_status_counter};
    use crate::store::failure::{DebtLedger, OperatorCap, OperatorHealthSurface};
    use crate::store::fjall::new_fjall_storage;

    /// Prove: DebtLedger is THE compaction-debt counter; Catalog IndexStatus
    /// is THE index-status counter; MetricExporter can only render those —
    /// a forged recompute disagrees and is not on the export path.
    #[test]
    fn exporters_render_one_counter_never_recompute_divergent() {
        let mut ledger = DebtLedger::with_ceiling(100);
        ledger.admit(17).expect("admit debt");

        let debt_counter = compaction_debt_counter(&ledger);
        let debt_exporter = MetricExporter::from_counter(debt_counter);
        assert_eq!(debt_exporter.value(), 17);
        assert_eq!(debt_exporter.value(), ledger.outstanding());
        // Second exporter from the same authority agrees — one source.
        assert_eq!(
            MetricExporter::from_counter(compaction_debt_counter(&ledger)).value(),
            debt_exporter.value()
        );
        // A recompute that invents another formula diverges — exporters must not.
        let forged_recompute = ledger.outstanding().saturating_add(ledger.ceiling());
        assert_ne!(
            debt_exporter.value(),
            forged_recompute,
            "exporter must render the DebtLedger counter, not a recomputed formula"
        );

        let live = CatalogGeneration::from_relation(RelationGeneration::witness(9));
        let status = IndexStatus::witness(live, Some(IndexGeneration::witness(4)));
        assert!(matches!(status.staleness(), IndexStaleness::Stale { .. }));
        assert!(status.staleness().is_stale());

        let index_exporter = MetricExporter::from_counter(index_status_counter(status));
        assert_eq!(index_exporter.value(), 9);
        assert_eq!(index_exporter.value(), status.counter());
        assert_eq!(
            MetricExporter::from_counter(index_status_counter(status)).value(),
            index_exporter.value()
        );
        // Forged recompute from sealed generation alone diverges from live Catalog.
        let forged_from_sealed = status.sealed().unwrap().counter();
        assert_ne!(
            index_exporter.value(),
            forged_from_sealed,
            "index-status exporter renders Catalog generation, not a recomputed sealed stamp"
        );
    }

    /// Engine projects DebtLedger + IndexStatus into ephemeral relations —
    /// relation rows agree with exporters; a mismatched ephemeral seed is
    /// overwritten by authority on seal (no second counter survives).
    #[test]
    fn engine_projects_authorities_so_ephemeral_cannot_diverge() {
        let dir = tempfile::tempdir().unwrap();
        let db = Engine::compose(new_fjall_storage(dir.path()).unwrap(), Catalog::new())
            .expect("compose");

        let mut debt = DebtLedger::with_ceiling(50);
        debt.admit(11).expect("admit");
        let mut surface = OperatorHealthSurface::default();
        let cap = OperatorCap::mint();
        surface.set_debt(&cap, debt);
        // Hostile seed: ephemeral in-flight only (debt/index no longer live here).
        surface.ephemeral_mut().replace(0, Default::default());
        db.set_operator_health_surface(surface);

        // After seal, debt relation must equal DebtLedger.
        assert_eq!(db.compaction_debt_exporter().value(), 11);
        let rels = db.operator_ephemeral_relations();
        assert_eq!(
            rels.compaction_debt_relation().unwrap().rows()[0][0]
                .get_int()
                .unwrap(),
            11,
            "relation must render DebtLedger, not a forged ephemeral seed"
        );

        let status = IndexStatus::witness(
            CatalogGeneration::from_relation(RelationGeneration::witness(7)),
            Some(IndexGeneration::witness(7)),
        );
        assert!(!status.staleness().is_stale());
        db.set_index_status(status);

        assert_eq!(db.index_status_exporter().value(), 7);
        assert_eq!(
            db.operator_ephemeral_relations()
                .index_status_relation()
                .unwrap()
                .rows()[0][0]
                .get_int()
                .unwrap(),
            7
        );
        // Exporter and relation are the same counter — not two sources.
        assert_eq!(
            db.index_status_exporter().value(),
            db.operator_ephemeral_relations()
                .index_status_relation()
                .unwrap()
                .rows()[0][0]
                .get_int()
                .unwrap() as u64
        );
    }
}

#[cfg(test)]
mod health_tiers_and_tracing {
    //! Adversarial: three independently-queryable health tiers (not one bool
    //! with three faces); tracing verbosity never changes result rows or budget.

    use crate::data::json::NamedRows;
    use crate::session::catalog::Catalog;
    use crate::session::db::Engine;
    use crate::session::observe::{
        Integrity, Liveness, ObservationBudget, Readiness, TracingVerbosity, observe_probe,
    };
    use crate::store::fjall::new_fjall_storage;
    use kyzo_model::value::{DataValue, Tuple};

    fn probe_rows() -> NamedRows {
        NamedRows::try_new(
            vec!["k".into(), "v".into()],
            vec![
                Tuple::from_vec(vec![DataValue::from(1i64), DataValue::from(7i64)]),
                Tuple::from_vec(vec![DataValue::from(2i64), DataValue::from(9i64)]),
            ],
        )
        .expect("probe rows")
    }

    /// Prove: liveness / readiness / integrity are three distinct doors that
    /// can disagree — flipping one never mutates the other two.
    #[test]
    fn three_tiers_independently_queryable_not_one_bool() {
        let dir = tempfile::tempdir().unwrap();
        let db = Engine::compose(new_fjall_storage(dir.path()).unwrap(), Catalog::new())
            .expect("compose");

        // Defaults: live + ready; integrity fails until verify witnesses clean.
        assert!(db.liveness().is_passing());
        assert!(db.readiness().is_passing());
        assert!(!db.integrity().is_passing());

        // Distinct relation columns — three query surfaces, not one shared name.
        assert_eq!(
            db.liveness().relation().headers(),
            &["liveness".to_string()]
        );
        assert_eq!(
            db.readiness().relation().headers(),
            &["readiness".to_string()]
        );
        assert_eq!(
            db.integrity().relation().headers(),
            &["integrity".to_string()]
        );

        // Flip only readiness — liveness and integrity must be unchanged.
        db.set_readiness(Readiness::failing());
        assert!(
            db.liveness().is_passing(),
            "liveness must not follow readiness"
        );
        assert!(!db.readiness().is_passing());
        assert!(
            !db.integrity().is_passing(),
            "integrity must not follow readiness"
        );

        // Flip only integrity — liveness and readiness must be unchanged.
        db.set_integrity(Integrity::passing());
        assert!(db.liveness().is_passing());
        assert!(!db.readiness().is_passing());
        assert!(db.integrity().is_passing());

        // Flip only liveness — readiness and integrity must be unchanged.
        db.set_liveness(Liveness::failing());
        assert!(!db.liveness().is_passing());
        assert!(!db.readiness().is_passing());
        assert!(db.integrity().is_passing());

        // Type-level independence: the three types are not interchangeable.
        let live = Liveness::passing();
        let ready = Readiness::failing();
        let integ = Integrity::passing();
        assert_ne!(live.is_passing(), ready.is_passing());
        assert_eq!(live.is_passing(), integ.is_passing());
        assert_ne!(
            live.relation().headers(),
            ready.relation().headers(),
            "tiers must not share one relation face"
        );
    }

    /// Prove: turning tracing verbosity up/down changes neither result rows
    /// nor budget spend — only diagnostic emission may differ.
    #[test]
    fn tracing_verbosity_behavior_invariant_rows_and_budget() {
        let dir = tempfile::tempdir().unwrap();
        let db = Engine::compose(new_fjall_storage(dir.path()).unwrap(), Catalog::new())
            .expect("compose");

        let rows = probe_rows();
        let charge = 5u64;
        let ceiling = 100u64;
        let verbosities = [
            TracingVerbosity::Silent,
            TracingVerbosity::Summary,
            TracingVerbosity::Detail,
        ];

        let mut outcomes = Vec::new();
        let mut emissions = Vec::new();
        for &verbosity in &verbosities {
            let mut budget = ObservationBudget::with_ceiling(ceiling);
            let (outcome, emission) = observe_probe(rows.clone(), charge, &mut budget, verbosity);
            assert_eq!(
                outcome.budget_spent(),
                charge,
                "budget spend must equal the probe charge at {verbosity:?}"
            );
            assert_eq!(
                budget.spent(),
                charge,
                "budget ledger must record the same charge at {verbosity:?}"
            );
            outcomes.push(outcome);
            emissions.push(emission);
        }

        // Rows identical across Silent / Summary / Detail.
        assert_eq!(
            outcomes[0].rows().headers(),
            outcomes[1].rows().headers(),
            "Silent vs Summary must not change result headers"
        );
        assert_eq!(
            outcomes[0].rows().rows(),
            outcomes[1].rows().rows(),
            "Silent vs Summary must not change result rows"
        );
        assert_eq!(
            outcomes[1].rows().headers(),
            outcomes[2].rows().headers(),
            "Summary vs Detail must not change result headers"
        );
        assert_eq!(
            outcomes[1].rows().rows(),
            outcomes[2].rows().rows(),
            "Summary vs Detail must not change result rows"
        );
        // Budget spend identical across all three.
        assert_eq!(outcomes[0].budget_spent(), outcomes[1].budget_spent());
        assert_eq!(outcomes[1].budget_spent(), outcomes[2].budget_spent());

        // Diagnostics MAY differ — proving verbosity is not a no-op knob.
        assert_ne!(
            emissions[0].event_count, emissions[2].event_count,
            "Silent vs Detail must differ in diagnostic emission"
        );
        assert!(emissions[0].event_count < emissions[1].event_count);
        assert!(emissions[1].event_count < emissions[2].event_count);

        // Engine door: set_tracing_verbosity up/down — same rows + budget.
        db.set_tracing_verbosity(TracingVerbosity::Silent);
        let mut budget_lo = ObservationBudget::with_ceiling(ceiling);
        let (lo, emit_lo) = db.observe_under_tracing(probe_rows(), charge, &mut budget_lo);

        db.set_tracing_verbosity(TracingVerbosity::Detail);
        let mut budget_hi = ObservationBudget::with_ceiling(ceiling);
        let (hi, emit_hi) = db.observe_under_tracing(probe_rows(), charge, &mut budget_hi);

        assert_eq!(
            lo.rows().headers(),
            hi.rows().headers(),
            "engine verbosity must not change headers"
        );
        assert_eq!(
            lo.rows().rows(),
            hi.rows().rows(),
            "engine verbosity must not change rows"
        );
        assert_eq!(
            lo.budget_spent(),
            hi.budget_spent(),
            "engine verbosity must not change budget spend"
        );
        assert_eq!(budget_lo.spent(), budget_hi.spent());
        assert_ne!(
            emit_lo.event_count, emit_hi.event_count,
            "engine verbosity must still change diagnostics"
        );
    }
}
