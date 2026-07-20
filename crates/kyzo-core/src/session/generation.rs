/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Catalog freshness authorities: the one meaning-clock for sealed
//! projections and rebuildable indices.
//!
//! [`CatalogGeneration`] is the sole door that mints a projection
//! [`Generation`](crate::project::projection::Generation). Per-relation and
//! per-index counters witness through [`RelationGeneration`] and
//! [`IndexGeneration`], then lift into the catalog clock — never a bare
//! `u64` and never a public `Generation::new(raw)`.
//!
//! [`IndexStatus`] is the index-status metric authority (§20): Catalog
//! generation plus staleness of a sealed index. Exporters render
//! [`IndexStatus::counter`] only — they never recompute a divergent value.

use crate::project::projection::Generation;

/// The one catalog-meaning freshness clock.
///
/// @authority CatalogGeneration
/// @layer runtime-catalog
/// @owns the one catalog-meaning freshness clock; sole door that mints projection Generation stamps from a proven counter
/// @constructs CatalogGeneration::from_relation | CatalogGeneration::from_index
/// @forbids bare u64 freshness | public Generation::new(raw)
/// @converts CatalogGeneration -> Generation (projection seal/classify stamp)
/// @gate Generation stamps only via CatalogGeneration::projection_stamp (#337)
/// @status established #337
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct CatalogGeneration(u64);

impl CatalogGeneration {
    /// Lift a per-relation witness into the catalog clock.
    pub(crate) fn from_relation(relation: RelationGeneration) -> Self {
        CatalogGeneration(relation.0)
    }

    /// Lift a per-index witness into the catalog clock.
    pub(crate) fn from_index(index: IndexGeneration) -> Self {
        CatalogGeneration(index.0)
    }

    /// THE DOOR: mint a projection [`Generation`] from this catalog authority.
    pub(crate) fn projection_stamp(self) -> Generation {
        Generation::stamp_from_counter(self.0)
    }

    /// The one catalog-meaning counter exporters may render (§20).
    pub(crate) fn counter(self) -> u64 {
        self.0
    }
}

/// Per-relation monotone write counter that witnesses catalog freshness.
///
/// @authority RelationGeneration
/// @layer runtime-catalog
/// @owns per-relation monotone write counter that witnesses catalog freshness for one relation
/// @constructs RelationGeneration::witness
/// @forbids bare AtomicU64 load standing for relation freshness without this witness
/// @converts RelationGeneration -> CatalogGeneration (lift into the catalog clock)
/// @gate segment freshness samples only through RelationGeneration::witness (#337)
/// @status established #337
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct RelationGeneration(u64);

impl RelationGeneration {
    /// Witness a loaded per-relation counter as relation freshness.
    pub(crate) fn witness(raw: u64) -> Self {
        RelationGeneration(raw)
    }

    /// The relation freshness counter (render-only for exporters).
    pub(crate) fn counter(self) -> u64 {
        self.0
    }

    /// Lift into the catalog clock, then mint a projection stamp.
    pub(crate) fn projection_stamp(self) -> Generation {
        CatalogGeneration::from_relation(self).projection_stamp()
    }
}

/// Per-index monotone rebuild counter that witnesses catalog freshness.
///
/// @authority IndexGeneration
/// @layer runtime-catalog
/// @owns per-index monotone rebuild counter that witnesses catalog freshness for one index
/// @constructs IndexGeneration::witness
/// @forbids bare integer index freshness outside this authority
/// @converts IndexGeneration -> CatalogGeneration (lift into the catalog clock)
/// @gate index rebuild stamps only through IndexGeneration::witness (#337)
/// @status established #337
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct IndexGeneration(u64);

impl IndexGeneration {
    /// Witness a loaded per-index counter as index freshness.
    pub(crate) fn witness(raw: u64) -> Self {
        IndexGeneration(raw)
    }

    /// The index freshness counter (render-only for exporters).
    pub(crate) fn counter(self) -> u64 {
        self.0
    }

    /// Lift into the catalog clock, then mint a projection stamp.
    pub(crate) fn projection_stamp(self) -> Generation {
        CatalogGeneration::from_index(self).projection_stamp()
    }
}

/// Index-status authority (§20): Catalog generation + staleness of a sealed index.
///
/// The sole counter exporters render for index-status is the live Catalog
/// generation ([`IndexStatus::counter`]). Staleness is derived from that
/// clock vs the index's last rebuild stamp — never a second independent
/// metric counter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct IndexStatus {
    /// Live Catalog generation (Store commit-order position).
    live: CatalogGeneration,
    /// Last index rebuild stamp, if any rebuild has completed.
    sealed: Option<IndexGeneration>,
}

impl Default for IndexStatus {
    fn default() -> Self {
        IndexStatus::witness(
            CatalogGeneration::from_relation(RelationGeneration::witness(0)),
            None,
        )
    }
}

impl IndexStatus {
    /// Witness live Catalog generation and optional sealed index generation.
    pub(crate) fn witness(live: CatalogGeneration, sealed: Option<IndexGeneration>) -> Self {
        Self { live, sealed }
    }

    /// THE authoritative index-status counter — live Catalog generation (§20).
    pub(crate) fn counter(self) -> u64 {
        self.live.counter()
    }

    /// Live Catalog generation clock.
    pub(crate) fn live(self) -> CatalogGeneration {
        self.live
    }

    /// Sealed index rebuild stamp, if any.
    pub(crate) fn sealed(self) -> Option<IndexGeneration> {
        self.sealed
    }

    /// Staleness of the sealed index relative to the live Catalog clock.
    pub(crate) fn staleness(self) -> IndexStaleness {
        match self.sealed {
            None => IndexStaleness::NeverBuilt { live: self.live },
            Some(sealed) => {
                let sealed_as_catalog = CatalogGeneration::from_index(sealed);
                if sealed_as_catalog == self.live {
                    IndexStaleness::Fresh {
                        generation: self.live,
                    }
                } else {
                    IndexStaleness::Stale {
                        live: self.live,
                        sealed,
                    }
                }
            }
        }
    }
}

/// Staleness of an index relative to Catalog generation (§20).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum IndexStaleness {
    /// No index rebuild has completed against this Catalog clock.
    NeverBuilt { live: CatalogGeneration },
    /// Sealed index generation matches the live Catalog clock.
    Fresh { generation: CatalogGeneration },
    /// Sealed index lags (or otherwise disagrees with) the live Catalog clock.
    Stale {
        live: CatalogGeneration,
        sealed: IndexGeneration,
    },
}

impl IndexStaleness {
    /// True when the operator should treat the index as not matching live Catalog.
    pub(crate) fn is_stale(self) -> bool {
        !matches!(self, IndexStaleness::Fresh { .. })
    }
}

#[cfg(test)]
mod index_status_authority {
    use super::*;

    #[test]
    fn index_status_counter_is_live_catalog_generation() {
        let live = CatalogGeneration::from_relation(RelationGeneration::witness(12));
        let status = IndexStatus::witness(live, Some(IndexGeneration::witness(9)));
        assert_eq!(status.counter(), 12);
        assert_eq!(status.counter(), live.counter());
        match status.staleness() {
            IndexStaleness::Stale { live: l, sealed } => {
                assert_eq!(l.counter(), 12);
                assert_eq!(sealed.counter(), 9);
            }
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    #[test]
    fn index_status_fresh_when_sealed_matches_live() {
        let sealed = IndexGeneration::witness(4);
        let live = CatalogGeneration::from_index(sealed);
        let status = IndexStatus::witness(live, Some(sealed));
        assert!(matches!(
            status.staleness(),
            IndexStaleness::Fresh { generation } if generation == live
        ));
        assert!(!status.staleness().is_stale());
    }

    #[test]
    fn index_status_never_built_is_stale() {
        let live = CatalogGeneration::from_relation(RelationGeneration::witness(1));
        let status = IndexStatus::witness(live, None);
        assert!(status.staleness().is_stale());
        assert!(matches!(
            status.staleness(),
            IndexStaleness::NeverBuilt { .. }
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::data::json::NamedRows;
    use crate::session::catalog::Catalog;
    use crate::session::db::Engine;
    use crate::store::Storage;
    use crate::store::sim::SimStorage;
    use kyzo_model::value::DataValue;

    fn no_params() -> BTreeMap<String, DataValue> {
        BTreeMap::new()
    }

    fn open_engine<S: Storage>(store: S) -> Engine<S> {
        Engine::compose(store, Catalog::new()).expect("compose engine")
    }

    /// Result rows as sorted `i64` vectors, for order-independent assertions.
    fn int_rows(nr: &NamedRows) -> Vec<Vec<i64>> {
        let mut out: Vec<Vec<i64>> = nr
            .rows()
            .iter()
            .map(|r| r.iter().map(|v| v.get_int().expect("int")).collect())
            .collect();
        out.sort();
        out
    }

    /// The segment law, end to end: a run of pure reads with no
    /// intervening write eventually builds and serves the relation's
    /// current-state segment (the rebuild gate declines the first miss and
    /// builds on the second — `engines/segments.rs`'s
    /// `rebuild_gated_by_stable_miss_streak`); ANY committed write to the
    /// relation orphans it (a re-read sees the write, never the cached
    /// past, whether or not a segment had actually been built yet); a
    /// DENIED write leaves state and answers untouched; and the same query
    /// inside a write session reads the transaction's own uncommitted view,
    /// never a committed-state segment.
    #[test]
    fn segments_serve_fresh_and_never_dirty() {
        let db = open_engine(SimStorage::new(7));
        db.run_script(
            "?[k, v] <- [[1, 10], [2, 20]] :create w {k => v}",
            no_params(),
        )
        .unwrap();

        // The first read's miss is ungated (declines to build, per the
        // rebuild gate); the second read's miss is at the same generation
        // (stable) and crosses the threshold, building the segment. Either
        // way both reads return the correct answer.
        let q = "?[k, v] := *w[k, v]";
        assert_eq!(
            int_rows(&db.run_script(q, no_params()).unwrap()),
            vec![vec![1, 10], vec![2, 20]]
        );
        assert_eq!(
            int_rows(&db.run_script(q, no_params()).unwrap()),
            vec![vec![1, 10], vec![2, 20]]
        );

        // A committed write orphans the segment: the re-read sees it.
        db.run_script("?[k, v] <- [[3, 30]] :put w {k, v}", no_params())
            .unwrap();
        assert_eq!(
            int_rows(&db.run_script(q, no_params()).unwrap()),
            vec![vec![1, 10], vec![2, 20], vec![3, 30]]
        );

        // A retraction is a write like any other: served state updates.
        db.run_script("?[k, v] <- [[2, 20]] :rm w {k, v}", no_params())
            .unwrap();
        assert_eq!(
            int_rows(&db.run_script(q, no_params()).unwrap()),
            vec![vec![1, 10], vec![3, 30]]
        );
        assert_eq!(
            int_rows(&db.run_script(q, no_params()).unwrap()),
            vec![vec![1, 10], vec![3, 30]]
        );

        // A write whose transaction rolls back (parse-stage failure after
        // the relation was touched is hard to stage; a constraint denial
        // is the canonical rollback) leaves both state and served answers
        // untouched — the early bump merely orphans, never lies.
        db.run_script(
            "::constraint create nonneg { ?[k, v] := *w[k, v], v < 0 }",
            no_params(),
        )
        .unwrap();
        assert!(
            db.run_script("?[k, v] <- [[4, -1]] :put w {k, v}", no_params())
                .is_err(),
            "violating write denied"
        );
        assert_eq!(
            int_rows(&db.run_script(q, no_params()).unwrap()),
            vec![vec![1, 10], vec![3, 30]]
        );

        // A PLAIN INDEX is a mutated relation in its own right: its
        // segment must orphan when a base-relation write updates the
        // mirrored rows (hostile-review reproducer: the served index
        // segment returned the stale two-row past after a base `:put`).
        db.run_script("::index create w:by_v {v}", no_params())
            .unwrap();
        let qi = "?[v, k] := *w:by_v{v, k}";
        assert_eq!(
            int_rows(&db.run_script(qi, no_params()).unwrap()),
            vec![vec![10, 1], vec![30, 3]]
        );
        db.run_script("?[k, v] <- [[5, 50]] :put w {k, v}", no_params())
            .unwrap();
        assert_eq!(
            int_rows(&db.run_script(qi, no_params()).unwrap()),
            vec![vec![10, 1], vec![30, 3], vec![50, 5]],
            "an index segment must never outlive a base write"
        );
    }

    /// [`segments_serve_fresh_and_never_dirty`]'s reproducer, extended to
    /// the JOIN PROBE path (issue #75's fix): `*jl[k], *jr[k, v]` compiles
    /// to a prefix join whose right side (`jr`) is now served, current-
    /// state, straight from its segment (`StoredRA::prefix_join_batched`)
    /// instead of the bitemporal seek-based probe. The probe side must
    /// obey the identical freshness law as a plain scan — a write to `jr`
    /// bumps its generation BEFORE commit, so the very next read's live
    /// stamp can never classify a segment sealed before that write as
    /// fresh, and the join sees the new row immediately, never a cached
    /// probe answer.
    #[test]
    fn segments_serve_fresh_and_never_dirty_for_join_probes() {
        let db = open_engine(SimStorage::new(9));
        db.run_script("?[k] <- [[1], [2]] :create jl {k}", no_params())
            .unwrap();
        db.run_script("?[k2, v] <- [[1, 10]] :create jr {k2 => v}", no_params())
            .unwrap();

        // The first read's miss declines (rebuild gate); the second read's
        // miss is at the same stable generation and builds jr's segment, so
        // its point-lookup probe is served from the cache from here on.
        let q = "?[k, v] := *jl[k], *jr[k, v]";
        assert_eq!(
            int_rows(&db.run_script(q, no_params()).unwrap()),
            vec![vec![1, 10]]
        );
        assert_eq!(
            int_rows(&db.run_script(q, no_params()).unwrap()),
            vec![vec![1, 10]]
        );

        // A committed write to jr — the PROBE side of the join — orphans
        // its segment: the re-read must see the new row, not a stale
        // probe answer served from before the write.
        db.run_script("?[k2, v] <- [[2, 20]] :put jr {k2 => v}", no_params())
            .unwrap();
        assert_eq!(
            int_rows(&db.run_script(q, no_params()).unwrap()),
            vec![vec![1, 10], vec![2, 20]]
        );
        assert_eq!(
            int_rows(&db.run_script(q, no_params()).unwrap()),
            vec![vec![1, 10], vec![2, 20]]
        );

        // A retraction on the probe side is a write like any other.
        db.run_script("?[k2, v] <- [[1, 10]] :rm jr {k2, v}", no_params())
            .unwrap();
        assert_eq!(
            int_rows(&db.run_script(q, no_params()).unwrap()),
            vec![vec![2, 20]]
        );
    }
}
