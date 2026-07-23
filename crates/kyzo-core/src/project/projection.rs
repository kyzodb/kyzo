/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! One generic build→seal→freshness machine for every projection kind.
//!
//! [`ProjectionBuilder<K>`] is the building form: it exposes no query
//! surface. A consuming [`ProjectionBuilder::seal`] yields [`Sealed<K>`],
//! which carries its [`Generation`] in the type-visible contract.
//! A generation mismatch is not an `Option`/`Err` from a get-shaped call —
//! it is the distinct type [`Stale<K>`], which has no query method.
//!
//! Kind-specific engines re-land as `K` parameterizations of this machine
//! (story #305 T3): [`crate::project::vector::hnsw::Hnsw`], [`crate::project::text::fts::Fts`],
//! [`crate::project::dedup::lsh::Lsh`], and (until their session host arms land)
//! `Sparse` / `Spatial` under `project::{sparse,spatial}` (`#[cfg(test)]`).
//! Relation-backed search is owned by
//! [`RelationIndexSearch::search_relation`] on those kinds — inherent
//! `knn` / `search_index` / `range_query` doors are thin UFCS aliases into
//! that trait, not a second authority (P103). This module owns the shared
//! protocol types. Segment freshness (T5) consumes [`Generation::classify`]
//! at [`crate::project::current`] — staleness is [`Stale`], never an
//! `Option` from a get-shaped call. The segment cache is rebuildable
//! acceleration only: meaning clocks come from [`crate::session::generation`];
//! the cache cannot own truth (P106).

use std::fmt;

use miette::Result;

use crate::store::ReadTx;
use kyzo_model::schema::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
use kyzo_model::value::{RelationId, SearchHits};

/// One required scalar column in an index relation (leading key or value).
pub(crate) fn index_col(name: &str, coltype: ColType) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        typing: NullableColType::required(coltype),
        default_gen: None,
    }
}

/// One required `List<eltype>` value column in an index relation.
pub(crate) fn index_list_col(name: &str, eltype: ColType) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        typing: NullableColType::required(ColType::List {
            eltype: Box::new(NullableColType::required(eltype)),
            len: None,
        }),
        default_gen: None,
    }
}

/// One door for every posting-style index relation schema: leading key
/// column(s), then `src_*` copies of `base.keys`, then non-key columns.
/// LSH / FTS / sparse / spatial each own their leading and value seats;
/// the `src_*` scaffold is shared so it cannot drift per engine.
pub(crate) fn index_relation_metadata(
    leading_keys: impl IntoIterator<Item = ColumnDef>,
    base: &StoredRelationMetadata,
    non_keys: Vec<ColumnDef>,
) -> StoredRelationMetadata {
    let mut keys: Vec<ColumnDef> = leading_keys.into_iter().collect();
    keys.extend(base.keys.iter().map(|k| ColumnDef {
        name: format!("src_{}", k.name).into(),
        typing: k.typing.clone(),
        default_gen: None,
    }));
    StoredRelationMetadata { keys, non_keys }
}

/// A projection kind's identity in the build→seal→freshness machine.
///
/// Relation-backed search is **not** declared here (P103): a former
/// `search` returning `k`/`ef` was a façade dual of the real engine doors.
/// Engine kinds implement [`RelationIndexSearch`]; in-memory sealed search
/// (tests) implements [`SealedQuery`].
pub trait ProjectionKind {}

/// In-memory sealed search — only for kinds whose payload answers queries
/// without a storage transaction. Invoked only through [`Sealed::query`].
pub trait SealedQuery: ProjectionKind {
    /// Kind-specific query input.
    type Query;
    /// Kind-specific candidate answer.
    type Candidates;

    /// Search the sealed kind payload.
    fn search(&self, query: &Self::Query) -> Self::Candidates;
}

/// Relation-backed index search — the sole search seam for engine kinds (P103).
///
/// Each kind supplies a [`Self::Request`] bundling one invocation's inputs
/// and implements [`Self::search_relation`] as the algorithm door. An empty
/// marker impl is condemned: ownership means the trait method runs the
/// live HNSW / FTS / LSH / sparse / spatial search. Inherent `knn` /
/// `search_index` / `range_query` names on engine types are UFCS-friendly
/// aliases that construct `Request` and call this trait — not a dual path.
pub(crate) trait RelationIndexSearch: ProjectionKind {
    /// Bundled inputs for one relation-backed search invocation.
    type Request<'a>;

    /// Run the kind's relation-backed search algorithm.
    fn search_relation<Tx: ReadTx>(tx: &Tx, request: Self::Request<'_>) -> Result<SearchHits>;
}

/// Generation stamp carried by a sealed projection.
///
/// Private field: the stamp is minted only through
/// [`CatalogGeneration::projection_stamp`](crate::session::generation::CatalogGeneration)
/// (story #337 / P099). There is no public `Generation::new(raw)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Generation(u64);

impl Generation {
    /// Crate-internal admit from a catalog-proven counter.
    ///
    /// Call sites outside [`crate::session::generation`] must not mint
    /// stamps — [`CatalogGeneration::projection_stamp`](crate::session::generation::CatalogGeneration)
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

    /// Consuming seal: spends the builder and yields the sealed form
    /// stamped with `generation`.
    pub fn seal(self, generation: Generation) -> Sealed<K> {
        Sealed {
            generation,
            kind: self.kind,
        }
    }
}

/// Sealed projection — carries its [`Generation`] in contract.
///
/// Produced only by [`ProjectionBuilder::seal`]. In-memory search is
/// [`Sealed::query`] for [`SealedQuery`] kinds; relation engines search
/// through [`RelationIndexSearch::search_relation`] on `K`.
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

impl<K: SealedQuery> Sealed<K> {
    /// Query this sealed in-memory projection. Absent from
    /// [`ProjectionBuilder`] and from [`Stale`] — and absent from
    /// relation-backed engine kinds (those use
    /// [`RelationIndexSearch::search_relation`]).
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
    use miette::{Result, miette};

    use super::*;
    use crate::session::generation::{CatalogGeneration, RelationGeneration};

    fn stamp(raw: u64) -> Generation {
        CatalogGeneration::from_relation(RelationGeneration::witness(raw)).projection_stamp()
    }

    #[derive(Debug, PartialEq, Eq)]
    struct DemoKind {
        hits: usize,
    }

    impl ProjectionKind for DemoKind {}

    impl SealedQuery for DemoKind {
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
    fn classify_keeps_matching_generation() -> Result<()> {
        let sealed = ProjectionBuilder::new(DemoKind { hits: 1 }).seal(stamp(4));
        let current = stamp(4)
            .classify(sealed)
            .map_err(|_| miette!("matching generation stays Sealed"))?;
        assert_eq!(current.query(&0), 1);
        Ok(())
    }

    #[test]
    fn classify_mismatch_yields_stale() -> Result<()> {
        let sealed = ProjectionBuilder::new(DemoKind { hits: 1 }).seal(stamp(4));
        let stale = stamp(9)
            .classify(sealed)
            .err()
            .ok_or_else(|| miette!("mismatched generation is Stale"))?;
        assert_eq!(stale.generation(), stamp(4));
        assert_eq!(stale.expected(), stamp(9));
        assert_eq!(stale.kind(), &DemoKind { hits: 1 });
        Ok(())
    }

    /// Closure test (story #305): one machine typechecks build→seal→classify
    /// for all five engine kinds — search is [`RelationIndexSearch`] with a
    /// real `search_relation` door, not a ProjectionKind façade (P103).
    #[test]
    fn five_engine_kinds_share_one_machine() -> Result<()> {
        use crate::project::dedup::lsh::Lsh;
        use crate::project::sparse::index::Sparse;
        use crate::project::spatial::index::Spatial;
        use crate::project::text::fts::Fts;
        use crate::project::vector::hnsw::Hnsw;

        fn assert_owns_relation_search<K: RelationIndexSearch>() {}
        assert_owns_relation_search::<Hnsw>();
        assert_owns_relation_search::<Fts>();
        assert_owns_relation_search::<Lsh>();
        assert_owns_relation_search::<Sparse>();
        assert_owns_relation_search::<Spatial>();

        let generation = stamp(1);

        let hnsw = ProjectionBuilder::new(Hnsw).seal(generation);
        let fts = ProjectionBuilder::new(Fts).seal(generation);
        let lsh = ProjectionBuilder::new(Lsh).seal(generation);
        let sparse = ProjectionBuilder::new(Sparse).seal(generation);
        let spatial = ProjectionBuilder::new(Spatial).seal(generation);
        assert!(stamp(1).classify(hnsw).is_ok());
        assert!(stamp(1).classify(fts).is_ok());
        assert!(stamp(1).classify(lsh).is_ok());
        assert!(stamp(1).classify(sparse).is_ok());
        assert!(stamp(1).classify(spatial).is_ok());

        let sealed = ProjectionBuilder::new(Hnsw).seal(stamp(2));
        assert!(stamp(2).classify(sealed).is_ok());
        let sealed = ProjectionBuilder::new(Fts).seal(stamp(2));
        let stale = stamp(3)
            .classify(sealed)
            .err()
            .ok_or_else(|| miette!("stale"))?;
        assert_eq!(stale.expected(), stamp(3));
        Ok(())
    }
}
