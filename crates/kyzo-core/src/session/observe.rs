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

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::sync::mpsc::{Receiver, Sender, channel};

use smartstring::{LazyCompact, SmartString};

use crate::data::json::NamedRows;
use crate::session::db::Engine;
use crate::store::Storage;
use crate::store::verify_walk::{DeepVerifyDigest, DeepVerifyReport, deep_verify_storage};

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
    pub schedule_ordinal: u64,
}

impl DeepVerifyLastResult {
    fn from_report(report: &DeepVerifyReport, schedule_ordinal: u64) -> Self {
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
        pending_ordinal: u64,
    },
}

impl DeepVerifyStaleness {
    /// True when the operator should run (or re-run) deep-verify.
    pub fn is_stale(&self) -> bool {
        matches!(self, DeepVerifyStaleness::NeverRun | DeepVerifyStaleness::Stale { .. })
    }
}

/// Operator schedule + persisted last-result for deep-verify (§51).
#[derive(Debug, Default)]
struct DeepVerifyOperatorState {
    /// Next schedule ordinal (monotone).
    next_ordinal: u64,
    /// When `Some`, a run is armed at that ordinal.
    pending: Option<u64>,
    /// Last completed result, if any.
    last_result: Option<DeepVerifyLastResult>,
}

impl DeepVerifyOperatorState {
    fn schedule(&mut self) -> u64 {
        self.next_ordinal = self.next_ordinal.saturating_add(1).max(1);
        let ord = self.next_ordinal;
        self.pending = Some(ord);
        ord
    }

    fn take_pending(&mut self) -> Option<u64> {
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
pub type CallbackEvent = (CallbackOp, NamedRows, NamedRows);

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
pub(crate) type CallbackCollector = BTreeMap<SmartString<LazyCompact>, Vec<CallbackEvent>>;

/// The callback registry: every registered callback by id, and the index
/// from relation name to the ids watching it. The two maps agree by
/// construction — `register`, `unregister`, and the disconnect pruning in
/// [`Db::send_callbacks`] are the only mutators, and each maintains both.
///
/// Also carries the deep-verify operator schedule + persisted last-result
/// (§51) — same lock door so observe stays one session observation surface.
#[derive(Default)]
pub(crate) struct EventCallbackRegistry {
    pub(crate) by_id: BTreeMap<u32, CallbackDeclaration>,
    pub(crate) by_relation: BTreeMap<SmartString<LazyCompact>, BTreeSet<u32>>,
    deep_verify: DeepVerifyOperatorState,
}

impl EventCallbackRegistry {
    fn register(&mut self, id: u32, decl: CallbackDeclaration) {
        self.by_relation
            .entry(decl.dependent.clone())
            .or_default()
            .insert(id);
        self.by_id.insert(id, decl);
    }

    fn unregister(&mut self, id: u32) -> bool {
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
    pub fn register_callback(&self, relation: &str) -> (u32, Receiver<CallbackEvent>) {
        let (sender, receiver) = channel();
        let decl = CallbackDeclaration {
            dependent: SmartString::from(relation),
            sender,
        };
        let mut registry = self
            .event_callbacks
            .write()
            .expect("registry lock poisoned");
        let new_id = self.next_callback_id();
        registry.register(new_id, decl);
        (new_id, receiver)
    }

    /// Unregister a callback; true if it existed.
    pub fn unregister_callback(&self, id: u32) -> bool {
        self.event_callbacks
            .write()
            .expect("registry lock poisoned")
            .unregister(id)
    }

    /// The relations any callback currently watches: mutation collects
    /// old/new rows only for these (snapshotted once per transaction, so a
    /// registration racing a commit either sees all of it or none of it).
    pub(crate) fn current_callback_targets(&self) -> BTreeSet<SmartString<LazyCompact>> {
        self.event_callbacks
            .read()
            .expect("registry lock poisoned")
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
            let registry = self.event_callbacks.read().expect("registry lock poisoned");
            for (relation, events) in collector {
                let Some(ids) = registry.by_relation.get(&relation) else {
                    continue;
                };
                for (op, new, old) in events {
                    for id in ids {
                        if let Some(decl) = registry.by_id.get(id)
                            && decl.sender.send((op, new.clone(), old.clone())).is_err()
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
                .expect("registry lock poisoned");
            for id in to_remove {
                registry.unregister(id);
            }
        }
    }

    /// Arm an operator-scheduled deep-verify (§51). Does not run it —
    /// [`Self::run_scheduled_deep_verify`] does. Returns the schedule ordinal.
    pub fn schedule_deep_verify(&self) -> u64 {
        self.event_callbacks
            .write()
            .expect("registry lock poisoned")
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
                .expect("registry lock poisoned");
            match registry.deep_verify.take_pending() {
                Some(ord) => ord,
                None => return Ok(None),
            }
        };
        let report = deep_verify_storage(&self.store)?;
        let result = DeepVerifyLastResult::from_report(&report, ordinal);
        self.event_callbacks
            .write()
            .expect("registry lock poisoned")
            .deep_verify
            .record(result.clone());
        Ok(Some(result))
    }

    /// Query the persisted deep-verify last-result, if any run has completed.
    pub fn deep_verify_last_result(&self) -> Option<DeepVerifyLastResult> {
        self.event_callbacks
            .read()
            .expect("registry lock poisoned")
            .deep_verify
            .last_result()
    }

    /// Query deep-verify staleness relative to the operator schedule.
    pub fn deep_verify_staleness(&self) -> DeepVerifyStaleness {
        self.event_callbacks
            .read()
            .expect("registry lock poisoned")
            .deep_verify
            .staleness()
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
        while let Ok((op, new, _old)) = receiver.try_recv() {
            assert_eq!(op.as_str(), "Put");
            for row in new.rows() {
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
        while let Ok((_op, new, _old)) = receiver.try_recv() {
            for row in new.rows() {
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
        assert!(ord >= 1);
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
            other => panic!("expected Stale with prior result, got {other:?}"),
        }
        assert!(db.run_scheduled_deep_verify().unwrap().is_some());
        assert!(db.run_scheduled_deep_verify().unwrap().is_none());
    }
}
