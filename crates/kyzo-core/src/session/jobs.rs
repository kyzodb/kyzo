/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Ephemeral engine-state relations on the sealed operator surface (§82).
//!
//! Replaces the former empty-rows `::running` stub: in-flight tx,
//! compaction-debt, index-status, and storage-stats project as [`NamedRows`]
//! from live authorities — [`DebtLedger`], [`IndexStatus`], and the in-flight
//! registry — never a second counter or hardcoded zero. Quarantine ranges and
//! failure topology require [`OperatorCap`] — Cap-absent (tenant) doors refuse
//! ([`TenantBlindRefuse`]). Cap mint is composition-root / host only
//! (`OperatorCap::mint`, like `StoreOpen::mint`).
//!
//! `::kill` refuses with [`JobsRefuse::KillNotLanded`] until job cancellation
//! lands. Dispatch stays here so `session/db.rs` remains the composition root.

use miette::{Diagnostic, Result, bail};
use thiserror::Error;
use kyzo_model::value::{DataValue, Tuple};

use crate::data::json::NamedRows;
use crate::session::generation::IndexStatus;
use crate::store::failure::{
    EphemeralEngineState, FailureLattice, OperatorCap, OperatorHealthSurface, TenantBlindRefuse,
};

/// Closed job-system op sum for `::running` / `::kill` (§82).
///
/// Illegal combinations are unconstructable: every variant is exhaustive at
/// this seat. Dispatch is [`run_job_op`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobSysOp {
    /// List in-flight transactions (`::running`).
    ListRunning,
    /// Cancel a running job (`::kill`) — refused until cancellation lands.
    KillRunning {
        /// Process / job id the operator asked to cancel.
        pid: u64,
    },
}

/// Typed refuses for the job-system door — never `IndexOpNotLanded("::kill")`.
#[derive(Debug, Error, Diagnostic)]
pub enum JobsRefuse {
    /// `::kill` needs job cancellation infrastructure that has not landed.
    #[error("::kill needs job cancellation, which has not landed")]
    #[diagnostic(code(jobs::kill_not_landed))]
    KillNotLanded,
    /// `::running` was invoked without a live in-flight-tx registry source.
    #[error(
        "::running needs a live in-flight-tx registry; none was supplied \
         (never project a hardcoded zero)"
    )]
    #[diagnostic(code(jobs::in_flight_registry_absent))]
    InFlightRegistryAbsent,
}

/// Projector: ephemeral counters → relation rows on the sealed operator door.
///
/// Cap-present (`for_operator`) may select quarantine / failure topology.
/// Cap-absent (`for_tenant`) keeps ephemeral metrics but refuses topology.
/// Compaction-debt / index-status render from authorities, not ephemeral fields.
#[derive(Debug, Clone)]
pub struct OperatorEphemeralRelations {
    surface: OperatorHealthSurface,
    /// Index-status authority (§20) — never an ephemeral u64 twin.
    index_status: IndexStatus,
    /// Present only when sealed with [`OperatorCap`]. Tenant constructors leave
    /// this absent — quarantine is unreachable without Cap.
    operator: Option<OperatorCap>,
}

impl OperatorEphemeralRelations {
    /// Cap-absent tenant / ordinary door — quarantine and failure topology refuse.
    pub fn for_tenant(surface: OperatorHealthSurface, index_status: IndexStatus) -> Self {
        Self {
            surface,
            index_status,
            operator: None,
        }
    }

    /// Cap-present operator door — may select quarantine and failure topology.
    pub fn for_operator(
        surface: OperatorHealthSurface,
        index_status: IndexStatus,
        cap: OperatorCap,
    ) -> Self {
        Self {
            surface,
            index_status,
            operator: Some(cap),
        }
    }

    /// Whether this projector holds [`OperatorCap`].
    pub fn has_operator_cap(&self) -> bool {
        self.operator.is_some()
    }

    /// Borrow the underlying operator health surface.
    pub fn surface(&self) -> &OperatorHealthSurface {
        &self.surface
    }

    /// `in_flight_tx` relation — one row: count from the live registry projection.
    pub fn in_flight_tx_relation(&self) -> Result<NamedRows> {
        let n = self.surface.ephemeral().in_flight_tx();
        Ok(NamedRows::try_new(
            vec!["in_flight_tx".into()],
            vec![Tuple::from_vec(vec![DataValue::from(n as i64)])],
        )?)
    }

    /// `compaction_debt` relation — renders the one [`DebtLedger`] (§42/§44).
    pub fn compaction_debt_relation(&self) -> Result<NamedRows> {
        let debt = self.surface.render_debt_outstanding();
        Ok(NamedRows::try_new(
            vec!["compaction_debt".into()],
            vec![Tuple::from_vec(vec![DataValue::from(debt as i64)])],
        )?)
    }

    /// `index_status` relation — renders [`IndexStatus`] Catalog generation (§20).
    pub fn index_status_relation(&self) -> Result<NamedRows> {
        let index_gen = self.index_status.counter();
        Ok(NamedRows::try_new(
            vec!["index_status_generation".into()],
            vec![Tuple::from_vec(vec![DataValue::from(index_gen as i64)])],
        )?)
    }

    /// `storage_stats` relation — one row of backend counters.
    pub fn storage_stats_relation(&self) -> Result<NamedRows> {
        let s = self.surface.ephemeral().storage_stats();
        Ok(NamedRows::try_new(
            vec![
                "cache_size_bytes".into(),
                "cache_capacity_bytes".into(),
                "write_buffer_size_bytes".into(),
                "active_compactions".into(),
                "journal_count".into(),
            ],
            vec![Tuple::from_vec(vec![
                DataValue::from(s.cache_size_bytes as i64),
                DataValue::from(s.cache_capacity_bytes as i64),
                DataValue::from(s.write_buffer_size_bytes as i64),
                DataValue::from(s.active_compactions as i64),
                DataValue::from(s.journal_count as i64),
            ])],
        )?)
    }

    /// Quarantine-range relation — **Cap required**; Cap-absent refuses (§82).
    ///
    /// `start` / `end` are memcomparable key bytes as [`DataValue::Bytes`] —
    /// never `Debug` string formatting that loses byte order.
    pub fn quarantine_relation(&self) -> Result<NamedRows, TenantBlindRefuse> {
        let Some(cap) = self.operator.as_ref() else {
            return Err(TenantBlindRefuse::QuarantineTopologyForbidden);
        };
        let ranges = self.surface.quarantine_ranges(cap);
        let mut rows = Vec::with_capacity(ranges.len());
        for range in ranges {
            rows.push(Tuple::from_vec(vec![
                DataValue::from(range.keyspace().get() as i64),
                DataValue::Bytes(range.start().to_vec()),
                DataValue::Bytes(range.end().to_vec()),
            ]));
        }
        Ok(NamedRows::try_new(
            vec!["keyspace".into(), "start".into(), "end".into()],
            rows,
        )
        .expect("quarantine relation arity"))
    }

    /// Failure-topology probe — **Cap required**; Cap-absent refuses (§82).
    pub fn failure_topology<'a>(
        &self,
        lattice: &'a FailureLattice,
    ) -> Result<&'a FailureLattice, TenantBlindRefuse> {
        let Some(cap) = self.operator.as_ref() else {
            return Err(TenantBlindRefuse::FailureTopologyForbidden);
        };
        Ok(lattice.topology_for(cap))
    }
}

/// Dispatch a closed [`JobSysOp`].
pub(crate) fn run_job_op(op: JobSysOp) -> Result<NamedRows> {
    match op {
        JobSysOp::ListRunning => list_running(),
        JobSysOp::KillRunning { pid: _ } => kill_running(),
    }
}

/// `::running` without a live registry — typed refuse, never hardcoded zero.
pub(crate) fn list_running() -> Result<NamedRows> {
    bail!(JobsRefuse::InFlightRegistryAbsent)
}

/// `::running` from the live in-flight-tx registry count.
pub(crate) fn list_running_from(in_flight: u64) -> Result<NamedRows> {
    Ok(NamedRows::try_new(
        vec!["in_flight_tx".into()],
        vec![Tuple::from_vec(vec![DataValue::from(in_flight as i64)])],
    )?)
}

/// `::kill` — cancel a running job. Own typed refuse until jobs land.
pub(crate) fn kill_running() -> Result<NamedRows> {
    bail!(JobsRefuse::KillNotLanded)
}

/// Build relations from an explicit ephemeral snapshot (observe / tests).
/// Cap-absent — ephemeral metrics only; quarantine unreachable.
pub fn relations_from_ephemeral(ephemeral: EphemeralEngineState) -> OperatorEphemeralRelations {
    let mut surface = OperatorHealthSurface::default();
    *surface.ephemeral_mut() = ephemeral;
    OperatorEphemeralRelations::for_tenant(surface, IndexStatus::default())
}

/// Build Cap-present relations from an ephemeral snapshot (operator / tests).
pub fn relations_from_ephemeral_for_operator(
    ephemeral: EphemeralEngineState,
    cap: OperatorCap,
) -> OperatorEphemeralRelations {
    let mut surface = OperatorHealthSurface::default();
    *surface.ephemeral_mut() = ephemeral;
    OperatorEphemeralRelations::for_operator(surface, IndexStatus::default(), cap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::catalog::Catalog;
    use crate::session::db::Engine;
    use crate::store::Storage;
    use crate::store::failure::{KeyspaceId, mint_quarantine};
    use crate::store::fjall::new_fjall_storage;

    #[test]
    fn ephemeral_relations_project_in_flight_and_storage() {
        let mut ephemeral = EphemeralEngineState::default();
        ephemeral.replace(
            3,
            crate::store::failure::StorageStatsSnapshot {
                cache_size_bytes: 100,
                cache_capacity_bytes: 200,
                write_buffer_size_bytes: 50,
                active_compactions: 1,
                journal_count: 2,
            },
        );
        let rels = relations_from_ephemeral(ephemeral);
        assert_eq!(
            rels.in_flight_tx_relation().unwrap().rows()[0][0]
                .get_int()
                .unwrap(),
            3
        );
        let stats = rels.storage_stats_relation().unwrap();
        assert_eq!(stats.rows()[0][0].get_int().unwrap(), 100);
        assert_eq!(stats.rows()[0][4].get_int().unwrap(), 2);
    }

    /// Adversarial: quarantine / failure topology unreachable without Cap —
    /// not merely "when you pass Tenant you refuse". Cap-absent refuses;
    /// with Cap (test mint via pub(crate)), operator sees data.
    #[test]
    fn quarantine_unreachable_without_operator_cap() {
        let mut surface = OperatorHealthSurface::default();
        surface.record_quarantine(mint_quarantine(
            KeyspaceId::from_raw(9),
            b"q0".to_vec(),
            b"q1".to_vec(),
        ));

        // Cap-absent: no path to topology.
        let tenant = OperatorEphemeralRelations::for_tenant(surface.clone(), IndexStatus::default());
        assert!(!tenant.has_operator_cap());
        assert!(matches!(
            tenant.quarantine_relation(),
            Err(TenantBlindRefuse::QuarantineTopologyForbidden)
        ));

        // With Cap (composition-root mint only): operator sees data.
        let cap = OperatorCap::mint();
        let ranges = surface.quarantine_ranges(&cap).to_vec();
        let lattice = FailureLattice::Quarantined { ranges };
        assert!(matches!(
            tenant.failure_topology(&lattice),
            Err(TenantBlindRefuse::FailureTopologyForbidden)
        ));

        let op = OperatorEphemeralRelations::for_operator(surface, IndexStatus::default(), cap);
        assert!(op.has_operator_cap());
        let rows = op.quarantine_relation().unwrap();
        assert_eq!(rows.rows().len(), 1);
        // Memcomparable bytes preserved — not Debug strings.
        assert!(matches!(&rows.rows()[0][1], DataValue::Bytes(b) if b.as_slice() == b"q0"));
        assert!(matches!(&rows.rows()[0][2], DataValue::Bytes(b) if b.as_slice() == b"q1"));
        assert!(op.failure_topology(&lattice).is_ok());
    }

    #[test]
    fn list_running_without_registry_refuses_never_hardcoded_zero() {
        let err = list_running().expect_err("must refuse without live registry");
        assert!(
            err.downcast_ref::<JobsRefuse>()
                .is_some_and(|r| matches!(r, JobsRefuse::InFlightRegistryAbsent)),
            "expected InFlightRegistryAbsent, got {err}"
        );
    }

    #[test]
    fn kill_running_refuses_with_own_typed_variant() {
        let err = kill_running().expect_err("kill not landed");
        assert!(
            err.downcast_ref::<JobsRefuse>()
                .is_some_and(|r| matches!(r, JobsRefuse::KillNotLanded)),
            "expected KillNotLanded, got {err}"
        );
    }

    #[test]
    fn list_running_live_registry_nonzero_when_real_tx_open() {
        let dir = tempfile::tempdir().unwrap();
        let db = Engine::compose(new_fjall_storage(dir.path()).unwrap(), Catalog::new())
            .expect("compose");

        // Open a real write transaction (live Store tx) and register it on the
        // in-flight registry — the registry is the ::running authority.
        let _tx = db.store.write_tx().expect("open write tx");
        db.in_flight_tx_begin();

        let rows = db.list_running_jobs().expect("list_running from live registry");
        assert_eq!(rows.headers(), &["in_flight_tx".to_string()]);
        assert_eq!(rows.rows().len(), 1);
        assert!(
            rows.rows()[0][0].get_int().unwrap() > 0,
            "live registry must be nonzero while a real tx is open"
        );

        db.in_flight_tx_end();
        let after = db.list_running_jobs().expect("list after end");
        assert_eq!(after.rows()[0][0].get_int().unwrap(), 0);
    }

    #[test]
    fn compaction_debt_renders_from_debt_ledger_not_ephemeral() {
        let mut surface = OperatorHealthSurface::default();
        let cap = OperatorCap::mint();
        let mut debt = crate::store::failure::DebtLedger::with_ceiling(50);
        debt.admit(11).expect("admit");
        surface.set_debt(&cap, debt);

        let rels = OperatorEphemeralRelations::for_tenant(surface, IndexStatus::default());
        assert_eq!(
            rels.compaction_debt_relation().unwrap().rows()[0][0]
                .get_int()
                .unwrap(),
            11
        );
    }
}
