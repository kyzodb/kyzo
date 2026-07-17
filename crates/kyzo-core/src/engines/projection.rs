/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! One generic build→seal→query machine for every projection kind.
//!
//! [`ProjectionBuilder<K>`] is the building form: it exposes no query
//! surface. A consuming [`ProjectionBuilder::seal`] yields [`Sealed<K>`],
//! which carries its [`Generation`] in the type-visible contract and is the
//! only form that can be queried. A generation mismatch is not an
//! `Option`/`Err` from a get-shaped call — it is the distinct type
//! [`Stale<K>`], which has no query method.
//!
//! Kind-specific engines re-land as `K` parameterizations of this machine
//! (story #305 T3): [`crate::engines::hnsw::Hnsw`], [`crate::engines::fts::Fts`],
//! [`crate::engines::lsh::Lsh`], [`crate::engines::sparse::Sparse`], and
//! [`crate::engines::spatial::Spatial`]. Relation-backed search is owned by
//! inherent methods on those kinds (`Hnsw::knn`, `Fts::search_index`, …) —
//! free-fn duals are gone (P103). This module owns the shared protocol types.
//! Segment freshness (T5) consumes [`Generation::classify`] at
//! [`crate::engines::segments`] — staleness is [`Stale`], never an `Option`
//! from a get-shaped call. The segment cache is rebuildable acceleration
//! only: meaning clocks come from [`crate::runtime::generation`]; the cache
//! cannot own truth (P106).

use std::fmt;

use crate::data::value::RelationId;

/// A projection kind's build payload and search law.
///
/// One implementation per engine; the machine is parameterized over `Self`
/// so build→seal→query is proved once for every kind.
pub trait ProjectionKind {
    /// Kind-specific query input.
    type Query;
    /// Kind-specific candidate answer.
    type Candidates;

    /// Search the sealed kind payload. Invoked only through [`Sealed::query`].
    fn search(&self, query: &Self::Query) -> Self::Candidates;
}

/// Generation stamp carried by a sealed projection.
///
/// Private field: the stamp is minted only through
/// [`CatalogGeneration::projection_stamp`](crate::runtime::generation::CatalogGeneration)
/// (story #337 / P099). There is no public `Generation::new(raw)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Generation(u64);

impl Generation {
    /// Crate-internal admit from a catalog-proven counter.
    ///
    /// Call sites outside [`crate::runtime::generation`] must not mint
    /// stamps — [`CatalogGeneration::projection_stamp`](crate::runtime::generation::CatalogGeneration)
    /// is the authority door. Name avoids the raw-door constructor heuristic.
    pub(crate) fn stamp_from_counter(raw: u64) -> Self {
        Generation(raw)
    }

    /// The underlying monotone counter value.
    pub fn raw(self) -> u64 {
        self.0
    }

    /// Type-visible freshness: matching stamp keeps [`Sealed`]; mismatch
    /// yields the distinguishable [`Stale`] type (no query surface).
    pub fn classify<K>(self, sealed: Sealed<K>) -> Result<Sealed<K>, Stale<K>> {
        if self == sealed.generation {
            Ok(sealed)
        } else {
            Err(Stale {
                generation: sealed.generation,
                kind: sealed.kind,
                expected: self,
            })
        }
    }
}

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "generation({})", self.0)
    }
}

/// Residency identity for a rebuildable projection (sealed segment/index body).
///
/// @authority ResidentIndexKey
/// @layer engines
/// @owns rebuildable projection residency identity — a sealed kind's cache key under one Generation
/// @constructs ResidentIndexKey::for_relation
/// @forbids bare RelationId standing for resident index identity across engines
/// @gate segment/index resident maps keyed only by ResidentIndexKey (#337)
/// @status established #337
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ResidentIndexKey(RelationId);

impl ResidentIndexKey {
    /// THE DOOR: residency key for one relation's rebuildable projection.
    pub(crate) fn for_relation(relation: RelationId) -> Self {
        ResidentIndexKey(relation)
    }
}

/// Building form of a projection — no query method exists on this type.
///
/// Constructed while rows are ingested; spent by [`ProjectionBuilder::seal`].
#[derive(Debug)]
pub struct ProjectionBuilder<K> {
    kind: K,
}

impl<K> ProjectionBuilder<K> {
    /// Start building a projection of kind `K`.
    pub fn new(kind: K) -> Self {
        ProjectionBuilder { kind }
    }

    /// Access the kind payload while still building (put/insert paths).
    ///
    /// `pub(crate)` so external callers cannot reach [`ProjectionKind::search`]
    /// through the builder — only [`Sealed::query`] is the query door.
    pub(crate) fn kind_mut(&mut self) -> &mut K {
        &mut self.kind
    }

    /// Borrow the kind payload while still building.
    pub(crate) fn kind(&self) -> &K {
        &self.kind
    }

    /// Consuming seal: spends the builder and yields the queryable form
    /// stamped with `generation`.
    pub fn seal(self, generation: Generation) -> Sealed<K> {
        Sealed {
            generation,
            kind: self.kind,
        }
    }
}

/// Queryable form of a projection — carries its [`Generation`] in contract.
///
/// Produced only by [`ProjectionBuilder::seal`]. The sole type that exposes
/// [`Sealed::query`].
#[derive(Debug, Clone)]
pub struct Sealed<K> {
    generation: Generation,
    kind: K,
}

impl<K> Sealed<K> {
    /// The generation stamp sealed into this projection.
    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// Borrow the kind payload.
    pub fn kind(&self) -> &K {
        &self.kind
    }

    /// Spend the sealed wrapper and return the kind payload.
    pub fn into_kind(self) -> K {
        self.kind
    }
}

impl<K: ProjectionKind> Sealed<K> {
    /// Query this sealed projection. Absent from [`ProjectionBuilder`] and
    /// from [`Stale`] — querying an unsealed or stale projection is not a
    /// method that returns an error; those types have no query method.
    pub fn query(&self, query: &K::Query) -> K::Candidates {
        self.kind.search(query)
    }
}

/// A sealed projection whose generation does not match the live stamp.
///
/// Distinguishable type for staleness: no [`Sealed::query`]-shaped method.
/// Produced by [`Generation::classify`] on mismatch — never by an
/// `Option`-returning get.
#[derive(Debug)]
pub struct Stale<K> {
    generation: Generation,
    kind: K,
    expected: Generation,
}

impl<K> Stale<K> {
    /// Generation the sealed projection carried when classified stale.
    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// Live generation that failed to match.
    pub fn expected(&self) -> Generation {
        self.expected
    }

    /// Borrow the kind payload (inspection only — no search).
    pub fn kind(&self) -> &K {
        &self.kind
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::generation::{CatalogGeneration, RelationGeneration};

    fn stamp(raw: u64) -> Generation {
        CatalogGeneration::from_relation(RelationGeneration::witness(raw)).projection_stamp()
    }

    #[derive(Debug, PartialEq, Eq)]
    struct DemoKind {
        hits: usize,
    }

    impl ProjectionKind for DemoKind {
        type Query = usize;
        type Candidates = usize;

        fn search(&self, query: &Self::Query) -> Self::Candidates {
            self.hits + *query
        }
    }

    #[test]
    fn builder_seal_query_machine() {
        let builder = ProjectionBuilder::new(DemoKind { hits: 3 });
        let sealed = builder.seal(stamp(7));
        assert_eq!(sealed.generation(), stamp(7));
        assert_eq!(sealed.query(&2), 5);
    }

    #[test]
    fn classify_keeps_matching_generation() {
        let sealed = ProjectionBuilder::new(DemoKind { hits: 1 }).seal(stamp(4));
        let current = stamp(4)
            .classify(sealed)
            .expect("matching generation stays Sealed");
        assert_eq!(current.query(&0), 1);
    }

    #[test]
    fn classify_mismatch_yields_stale() {
        let sealed = ProjectionBuilder::new(DemoKind { hits: 1 }).seal(stamp(4));
        let stale = stamp(9)
            .classify(sealed)
            .expect_err("mismatched generation is Stale");
        assert_eq!(stale.generation(), stamp(4));
        assert_eq!(stale.expected(), stamp(9));
        assert_eq!(stale.kind(), &DemoKind { hits: 1 });
    }

    /// Closure test (story #305): one machine typechecks build→seal→query
    /// for all five engine kinds — no per-engine protocol twin.
    #[test]
    fn five_engine_kinds_share_one_machine() {
        use crate::engines::fts::{Fts, FtsScoreKind, FtsSearchParams};
        use crate::engines::hnsw::{Hnsw, HnswKnnParams};
        use crate::engines::lsh::{Lsh, LshSearchParams};
        use crate::engines::sparse::{Sparse, SparseSearchParams};
        use crate::engines::spatial::{Spatial, SpatialQuery};

        let generation = stamp(1);

        let hnsw = ProjectionBuilder::new(Hnsw).seal(generation);
        assert_eq!(
            hnsw.query(&HnswKnnParams {
                k: 10,
                ef: 5,
                radius: None,
                bind: crate::engines::hnsw::HnswBindPack::default(),
            }),
            10,
            "HNSW search law: ef is at least k"
        );

        let fts = ProjectionBuilder::new(Fts).seal(generation);
        assert_eq!(
            fts.query(&FtsSearchParams {
                k: 3,
                score_kind: FtsScoreKind::Tf,
                bind_score: crate::engines::fts::FtsBindScore::Append,
            }),
            3
        );

        let lsh = ProjectionBuilder::new(Lsh).seal(generation);
        assert_eq!(lsh.query(&LshSearchParams { k: Some(7) }), Some(7));
        assert_eq!(lsh.query(&LshSearchParams { k: None }), None);

        let sparse = ProjectionBuilder::new(Sparse).seal(generation);
        assert_eq!(
            sparse.query(&SparseSearchParams {
                k: 4,
                bind_score: crate::engines::sparse::SparseBindScore::Omit,
            }),
            4
        );

        let spatial = ProjectionBuilder::new(Spatial).seal(generation);
        assert_eq!(spatial.query(&SpatialQuery::Range), 0);
        assert_eq!(spatial.query(&SpatialQuery::Knn { k: 5 }), 5);

        // Freshness classify works uniformly for every kind.
        let sealed = ProjectionBuilder::new(Hnsw).seal(stamp(2));
        assert!(stamp(2).classify(sealed).is_ok());
        let sealed = ProjectionBuilder::new(Fts).seal(stamp(2));
        let stale = stamp(3).classify(sealed).expect_err("stale");
        assert_eq!(stale.expected(), stamp(3));
    }
}
