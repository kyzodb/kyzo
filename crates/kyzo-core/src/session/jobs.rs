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
//! ranges and failure topology stay behind [`HealthQueryAudience::Operator`]
//! — a tenant ask refuses ([`TenantBlindRefuse`]).
//!
//! `::kill` remains a typed refusal until job cancellation lands. Dispatch
//! stays here so `session/db.rs` remains the composition root.

use miette::{Result, bail};
use kyzo_model::value::{DataValue, Tuple};

use crate::data::json::NamedRows;
use crate::session::db::EngineRefuse;
use crate::store::failure::{
    EphemeralEngineState, FailureLattice, HealthQueryAudience, OperatorHealthSurface,
    TenantBlindRefuse,
};

/// Projector: ephemeral counters → relation rows on the sealed operator door.
#[derive(Debug, Clone)]
pub struct OperatorEphemeralRelations {
    surface: OperatorHealthSurface,
    audience: HealthQueryAudience,
}

impl OperatorEphemeralRelations {
    /// Seal an operator (or tenant) ask over a health surface snapshot.
    pub fn new(surface: OperatorHealthSurface, audience: HealthQueryAudience) -> Self {
        Self { surface, audience }
    }

    /// Audience this projector was sealed with.
    pub fn audience(&self) -> HealthQueryAudience {
        self.audience
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

    /// Quarantine-range relation — **operator only**; tenant refuses (§82).
    pub fn quarantine_relation(&self) -> Result<NamedRows, TenantBlindRefuse> {
        let ranges = self.surface.quarantine_ranges(self.audience)?;
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

    /// Failure-topology probe — **operator only**; tenant refuses (§82).
    pub fn failure_topology<'a>(
        &self,
        lattice: &'a FailureLattice,
    ) -> Result<&'a FailureLattice, TenantBlindRefuse> {
        lattice.topology_for(self.audience)
    }
}

/// `::running` — in-flight-tx relation through the sealed operator surface.
///
/// Live Engine counters wire through [`crate::session::observe`]; until that
/// snapshot is pushed, this projects the default ephemeral state (zero
/// in-flight) via the real relation door — not a hardcoded empty stub shape.
pub(crate) fn list_running() -> Result<NamedRows> {
    let surface = OperatorHealthSurface::default();
    OperatorEphemeralRelations::new(surface, HealthQueryAudience::Operator)
        .in_flight_tx_relation()
}

/// `::kill` — cancel a running job. Typed refusal until jobs land.
pub(crate) fn kill_running() -> Result<NamedRows> {
    bail!(EngineRefuse::IndexOpNotLanded("::kill"))
}

/// Build relations from an explicit ephemeral snapshot (observe / tests).
pub fn relations_from_ephemeral(
    ephemeral: EphemeralEngineState,
    audience: HealthQueryAudience,
) -> OperatorEphemeralRelations {
    let mut surface = OperatorHealthSurface::default();
    *surface.ephemeral_mut() = ephemeral;
    OperatorEphemeralRelations::new(surface, audience)
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
        let rels = relations_from_ephemeral(ephemeral, HealthQueryAudience::Operator);
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

    /// Adversarial tenant-blindness: selecting quarantine / failure topology
    /// from a tenant ask must refuse.
    #[test]
    fn tenant_blindness_cannot_select_quarantine_or_failure_topology() {
        let mut surface = OperatorHealthSurface::default();
        surface.record_quarantine(mint_quarantine(
            KeyspaceId::from_raw(9),
            b"q0".to_vec(),
            b"q1".to_vec(),
        ));
        let tenant = OperatorEphemeralRelations::new(surface.clone(), HealthQueryAudience::Tenant);
        assert!(matches!(
            tenant.quarantine_relation(),
            Err(TenantBlindRefuse::QuarantineTopologyForbidden)
        ));
        let lattice = FailureLattice::Quarantined {
            ranges: surface
                .quarantine_ranges(HealthQueryAudience::Operator)
                .unwrap()
                .to_vec(),
        };
        assert!(matches!(
            tenant.failure_topology(&lattice),
            Err(TenantBlindRefuse::FailureTopologyForbidden)
        ));

        // Operator path still works.
        let op = OperatorEphemeralRelations::new(surface, HealthQueryAudience::Operator);
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
