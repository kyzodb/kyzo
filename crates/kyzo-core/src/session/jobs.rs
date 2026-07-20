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
//! from [`OperatorHealthSurface`] / [`EphemeralEngineState`]. Quarantine
//! ranges and failure topology require [`OperatorCap`] — Cap-absent (tenant)
//! doors refuse ([`TenantBlindRefuse`]). Cap mint is composition-root / host
//! only (`OperatorCap::mint`, like `StoreOpen::mint`).
//!
//! `::kill` remains a typed refusal until job cancellation lands. Dispatch
//! stays here so `session/db.rs` remains the composition root.

use miette::{Result, bail};
use kyzo_model::value::{DataValue, Tuple};

use crate::data::json::NamedRows;
use crate::session::db::EngineRefuse;
use crate::store::failure::{
    EphemeralEngineState, FailureLattice, OperatorCap, OperatorHealthSurface, TenantBlindRefuse,
};

/// Projector: ephemeral counters → relation rows on the sealed operator door.
///
/// Cap-present (`for_operator`) may select quarantine / failure topology.
/// Cap-absent (`for_tenant`) keeps ephemeral metrics but refuses topology.
#[derive(Debug, Clone)]
pub struct OperatorEphemeralRelations {
    surface: OperatorHealthSurface,
    /// Present only when sealed with [`OperatorCap`]. Tenant constructors leave
    /// this absent — quarantine is unreachable without Cap.
    operator: Option<OperatorCap>,
}

impl OperatorEphemeralRelations {
    /// Cap-absent tenant / ordinary door — quarantine and failure topology refuse.
    pub fn for_tenant(surface: OperatorHealthSurface) -> Self {
        Self {
            surface,
            operator: None,
        }
    }

    /// Cap-present operator door — may select quarantine and failure topology.
    pub fn for_operator(surface: OperatorHealthSurface, cap: OperatorCap) -> Self {
        Self {
            surface,
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

    /// `in_flight_tx` relation — one row: count.
    pub fn in_flight_tx_relation(&self) -> Result<NamedRows> {
        let n = self.surface.ephemeral().in_flight_tx();
        Ok(NamedRows::try_new(
            vec!["in_flight_tx".into()],
            vec![Tuple::from_vec(vec![DataValue::from(n as i64)])],
        )?)
    }

    /// `compaction_debt` relation — outstanding debt units.
    pub fn compaction_debt_relation(&self) -> Result<NamedRows> {
        let debt = self.surface.ephemeral().compaction_debt();
        Ok(NamedRows::try_new(
            vec!["compaction_debt".into()],
            vec![Tuple::from_vec(vec![DataValue::from(debt as i64)])],
        )?)
    }

    /// `index_status` relation — catalog generation / staleness witness.
    pub fn index_status_relation(&self) -> Result<NamedRows> {
        let index_gen = self.surface.ephemeral().index_status_generation();
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
    pub fn quarantine_relation(&self) -> Result<NamedRows, TenantBlindRefuse> {
        let Some(cap) = self.operator.as_ref() else {
            return Err(TenantBlindRefuse::QuarantineTopologyForbidden);
        };
        let ranges = self.surface.quarantine_ranges(cap);
        let mut rows = Vec::with_capacity(ranges.len());
        for range in ranges {
            rows.push(Tuple::from_vec(vec![
                DataValue::from(range.keyspace().get() as i64),
                DataValue::from(format!("{:?}", range.start())),
                DataValue::from(format!("{:?}", range.end())),
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

/// `::running` — in-flight-tx relation through the sealed operator surface.
///
/// Live Engine counters wire through [`crate::session::observe`]; until that
/// snapshot is pushed, this projects the default ephemeral state (zero
/// in-flight) via the Cap-absent relation door — not a hardcoded empty stub.
pub(crate) fn list_running() -> Result<NamedRows> {
    let surface = OperatorHealthSurface::default();
    OperatorEphemeralRelations::for_tenant(surface).in_flight_tx_relation()
}

/// `::kill` — cancel a running job. Typed refusal until jobs land.
pub(crate) fn kill_running() -> Result<NamedRows> {
    bail!(EngineRefuse::IndexOpNotLanded("::kill"))
}

/// Build relations from an explicit ephemeral snapshot (observe / tests).
/// Cap-absent — ephemeral metrics only; quarantine unreachable.
pub fn relations_from_ephemeral(ephemeral: EphemeralEngineState) -> OperatorEphemeralRelations {
    let mut surface = OperatorHealthSurface::default();
    *surface.ephemeral_mut() = ephemeral;
    OperatorEphemeralRelations::for_tenant(surface)
}

/// Build Cap-present relations from an ephemeral snapshot (operator / tests).
pub fn relations_from_ephemeral_for_operator(
    ephemeral: EphemeralEngineState,
    cap: OperatorCap,
) -> OperatorEphemeralRelations {
    let mut surface = OperatorHealthSurface::default();
    *surface.ephemeral_mut() = ephemeral;
    OperatorEphemeralRelations::for_operator(surface, cap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::failure::{KeyspaceId, mint_quarantine};

    #[test]
    fn ephemeral_relations_project_four_metrics() {
        let mut ephemeral = EphemeralEngineState::default();
        ephemeral.replace(
            3,
            11,
            7,
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
        assert_eq!(
            rels.compaction_debt_relation().unwrap().rows()[0][0]
                .get_int()
                .unwrap(),
            11
        );
        assert_eq!(
            rels.index_status_relation().unwrap().rows()[0][0]
                .get_int()
                .unwrap(),
            7
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
        let tenant = OperatorEphemeralRelations::for_tenant(surface.clone());
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

        let op = OperatorEphemeralRelations::for_operator(surface, cap);
        assert!(op.has_operator_cap());
        assert_eq!(op.quarantine_relation().unwrap().rows().len(), 1);
        assert!(op.failure_topology(&lattice).is_ok());
    }

    #[test]
    fn list_running_uses_operator_ephemeral_relation_not_empty_stub() {
        let rows = list_running().expect("list_running");
        assert_eq!(rows.headers(), &["in_flight_tx".to_string()]);
        assert_eq!(rows.rows().len(), 1);
        assert_eq!(rows.rows()[0][0].get_int().unwrap(), 0);
    }
}
