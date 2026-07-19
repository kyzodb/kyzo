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

    /// Lift into the catalog clock, then mint a projection stamp.
    #[allow(dead_code)] // door lands with index-resident rebuild consumers
    pub(crate) fn projection_stamp(self) -> Generation {
        CatalogGeneration::from_index(self).projection_stamp()
    }
}
