/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Index-operator engines (HNSW, MinHash-LSH, FTS, and kin) and their shared
//! corruption boundary.
//!
//! [`IndexRowCorrupt`] is the typed error every engine raises when stored
//! index bytes (or a base row an index points at) do not decode as the
//! format says they must. It extends the kernel's corruption doctrine —
//! corruption is an error, never a panic — into every index read path.
//!
//! Projection freshness and staged construction are the shared
//! [`projection`] machine (story #305): [`projection::ProjectionBuilder`]
//! seals into generation-carrying [`projection::Sealed`]; staleness is
//! [`projection::Stale`], not an `Option` from a get-shaped call. The five
//! kind engines (`hnsw`/`fts`/`lsh`/`sparse`/`spatial`) are `K`
//! parameterizations of that machine — each implements
//! [`projection::ProjectionKind`] for build→seal→freshness and
//! [`projection::RelationIndexSearch::search_relation`] for relation-backed
//! search (P103); put/search math remains the kernel algorithm, not a
//! second protocol.

// Production host doors: `runtime/mutate.rs` dispatches `::fts|hnsw|lsh
// create/drop`; `query/search.rs` dispatches live search through
// `RelationIndexSearch::search_relation` (P112).
pub(crate) mod fts;
/// Generic build→seal→query machine for every projection kind (story #305).
pub(crate) mod projection;
// No `db.rs` surface yet — exercised only under `#[cfg(test)]` hostile
// suites and projection compile proofs until the operator lands (P112).
#[cfg(test)]
pub(crate) mod gazetteer;
#[cfg(test)]
mod gazetteer_hostile;
pub(crate) mod hnsw;
pub(crate) mod lsh;
/// Columnar current-state segments: rebuildable typed-column mirrors of a
/// relation's plain scan. Freshness is [`projection::Generation::classify`]
/// at the segment seam (story #305 T5) — [`projection::Sealed`] vs
/// [`projection::Stale`], never watermark equality or Option-as-staleness.
pub(crate) mod segments;
// No `db.rs` surface yet — exercised only under `#[cfg(test)]` until the
// operator lands (P112).
#[cfg(test)]
pub(crate) mod sparse;
#[cfg(test)]
mod sparse_hostile;
#[cfg(test)]
pub(crate) mod spatial;
/// Text analysis (tokenizers, filters, the tantivy-derived pipeline) —
/// production host door: `runtime/mutate.rs` FTS create, `query/search.rs`
/// analyzer resolution, and `runtime/relation` manifest decode (P112).
pub(crate) mod text;

use std::fmt;

use miette::Diagnostic;
use thiserror::Error;

use crate::data::value::DataValue;
use crate::data::value::{SearchHits, Tuple};

/// A stored index row (or a base row an index points at) failed to decode as
/// what the index format says it must be. Corruption is an error, never a
/// panic. Carries the row's key context so the failure is locatable.
#[derive(Debug, Error, Diagnostic)]
#[error("index '{index}' corrupt: {reason}; row {row}")]
#[diagnostic(code(index::corrupt))]
#[diagnostic(help(
    "the stored bytes do not decode as a valid index row; the index can be \
     dropped and re-created from its base relation"
))]
pub(crate) struct IndexRowCorrupt {
    pub(crate) index: String,
    pub(crate) row: String,
    pub(crate) reason: IndexCorruptReason,
}

/// Named reason a stored index row (or its base-row reference) is corrupt.
/// String reasons are unrepresentable — every engine path picks a variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IndexCorruptReason {
    RowShorterThanKey,
    WrongColumnCount { found: usize, expected: usize },
    BaseRowMissing,
    DecodeFailed { detail: String },

    SpatialCurveNot8Bytes,
    SpatialLatNotNumber,
    SpatialLonNotNumber,

    SparseWeightNotFloat,
    SparseWeightNotFiniteNonNeg,

    LshPermutations { detail: String },
    LshInvChunkNotBytes,
    LshInvNotChunkList,
    LshEmptyPosting,

    FtsPositionsNotList,
    FtsPositionNotInt,

    GazetteerSurfacesNotList,
    GazetteerSurfaceNotString,

    HnswNotInteger { what: String },
    HnswCanaryNonNullKeys,
    HnswCanaryEntryNotBytes,
    HnswCanaryEntryKeyTooShort { found: usize },
    HnswLayerOutOfRange { layer: i64 },
    HnswNegativeField { side: &'static str },
    HnswSubOutOfRange { side: &'static str, sub: i64 },
    HnswIgnoreLinkNotBool,
    HnswNodeDegreeNegative,
    HnswNodeHashNotBytes,
    HnswNodeHashWrongLength { found: usize },
    HnswEdgeDistanceNotNumber,
    HnswEdgeHashNotNull,
    HnswFieldBeyondArity { field: usize },
    HnswListElementBeyondList { sub: usize },
    HnswExpectsListOfVectors,
    HnswExpectsVector,
    HnswCanaryBelowCanaryLayer,
    HnswCanaryInsideNeighbourPrefix,
    HnswNonNodeRow,
    HnswEdgeTargetMissingNode,
    HnswNeighbourMissingNode,
    HnswManifestFieldBeyondArity { field: usize },
    HnswCanaryInsideLayer0Prefix,
    HnswIndexedFieldBeyondRelationArity,
    HnswIndexedFieldBeyondRowArity,
    HnswIndexedListElementBeyondList,
    HnswIndexedFieldNotListOfVectors,
}

impl fmt::Display for IndexCorruptReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RowShorterThanKey => {
                write!(f, "row shorter than the base relation's key")
            }
            Self::WrongColumnCount { found, expected } => {
                write!(f, "posting has {found} columns, expected {expected}")
            }
            Self::BaseRowMissing => {
                write!(f, "index references a base row that does not exist")
            }
            Self::DecodeFailed { detail } => {
                write!(f, "stored row bytes did not decode: {detail}")
            }
            Self::SpatialCurveNot8Bytes => {
                write!(f, "spatial posting curve column is not 8 bytes")
            }
            Self::SpatialLatNotNumber => write!(f, "spatial posting lat is not a number"),
            Self::SpatialLonNotNumber => write!(f, "spatial posting lon is not a number"),
            Self::SparseWeightNotFloat => write!(f, "sparse posting weight is not a float"),
            Self::SparseWeightNotFiniteNonNeg => {
                write!(f, "sparse posting weight is not a finite non-negative float")
            }
            Self::LshPermutations { detail } => {
                write!(f, "stored LSH permutations: {detail}")
            }
            Self::LshInvChunkNotBytes => {
                write!(f, "inverse LSH row holds a non-bytes chunk")
            }
            Self::LshInvNotChunkList => {
                write!(f, "inverse LSH row is not a list of chunks")
            }
            Self::LshEmptyPosting => write!(f, "empty LSH posting"),
            Self::FtsPositionsNotList => {
                write!(f, "FTS posting position column is not a list")
            }
            Self::FtsPositionNotInt => write!(f, "FTS posting position is not an integer"),
            Self::GazetteerSurfacesNotList => {
                write!(f, "gazetteer dictionary surfaces column is not a list")
            }
            Self::GazetteerSurfaceNotString => {
                write!(f, "gazetteer dictionary surface form is not a string")
            }
            Self::HnswNotInteger { what } => write!(f, "{what} is not an integer"),
            Self::HnswCanaryNonNullKeys => write!(f, "canary row with non-Null key slots"),
            Self::HnswCanaryEntryNotBytes => write!(f, "canary entry key is not bytes"),
            Self::HnswCanaryEntryKeyTooShort { found } => write!(
                f,
                "canary entry key is {found} bytes, expected at least {}",
                crate::data::value::StorageKey::RELATION_PREFIX_LEN
            ),
            Self::HnswLayerOutOfRange { layer } => {
                write!(
                    f,
                    "layer {layer} is out of range (layers are <= 0; 1 is the canary)"
                )
            }
            Self::HnswNegativeField { side } => write!(f, "{side} field is negative"),
            Self::HnswSubOutOfRange { side, sub } => {
                write!(f, "{side} sub-index {sub} is out of range")
            }
            Self::HnswIgnoreLinkNotBool => write!(f, "ignore_link is not a boolean"),
            Self::HnswNodeDegreeNegative => write!(f, "node degree is negative"),
            Self::HnswNodeHashNotBytes => write!(f, "node vector hash is not bytes"),
            Self::HnswNodeHashWrongLength { found } => write!(
                f,
                "node vector hash is {found} bytes, expected 32 (SHA-256)"
            ),
            Self::HnswEdgeDistanceNotNumber => write!(f, "edge distance is not a number"),
            Self::HnswEdgeHashNotNull => write!(f, "edge hash slot is not Null"),
            Self::HnswFieldBeyondArity { field } => {
                write!(f, "HNSW index references field {field} beyond the row's arity")
            }
            Self::HnswListElementBeyondList { sub } => {
                write!(f, "HNSW index references list element {sub} beyond the list")
            }
            Self::HnswExpectsListOfVectors => {
                write!(f, "HNSW index expects a list of vectors at this field")
            }
            Self::HnswExpectsVector => write!(f, "HNSW index expects a vector at this field"),
            Self::HnswCanaryBelowCanaryLayer => {
                write!(f, "canary row found below the canary layer")
            }
            Self::HnswCanaryInsideNeighbourPrefix => {
                write!(f, "canary row inside a neighbour prefix")
            }
            Self::HnswNonNodeRow => write!(f, "node key decoded to a non-node row"),
            Self::HnswEdgeTargetMissingNode => {
                write!(f, "edge target has no node row at its layer")
            }
            Self::HnswNeighbourMissingNode => {
                write!(f, "neighbour of a removed vector has no node row")
            }
            Self::HnswManifestFieldBeyondArity { field } => {
                write!(f, "manifest vector field {field} beyond the row's arity")
            }
            Self::HnswCanaryInsideLayer0Prefix => {
                write!(f, "canary row inside a vector's layer-0 prefix")
            }
            Self::HnswIndexedFieldBeyondRelationArity => {
                write!(f, "indexed field beyond the base relation's arity")
            }
            Self::HnswIndexedFieldBeyondRowArity => {
                write!(f, "indexed field beyond the base row's arity")
            }
            Self::HnswIndexedListElementBeyondList => {
                write!(f, "indexed list element beyond the stored list")
            }
            Self::HnswIndexedFieldNotListOfVectors => {
                write!(f, "indexed field is not a list of vectors")
            }
        }
    }
}

/// Wrap a scanned index-row stream so a codec refusal surfaces as this
/// index's OWN typed [`IndexRowCorrupt`], never a bare
/// [`DecodeError`](crate::data::value::DecodeError). Storage/IO errors are
/// NOT corruption and pass through unchanged (distinguished by diagnostic
/// code `value::decode`). Every engine consumes an index scan through this
/// boundary, so a raw codec error cannot leak out of an engine as its
/// contract.
pub(crate) fn index_rows<'a>(
    index_name: &'a str,
    scan: impl Iterator<Item = miette::Result<crate::data::value::Tuple>> + 'a,
) -> impl Iterator<Item = miette::Result<crate::data::value::Tuple>> + 'a {
    scan.map(move |r| {
        r.map_err(|e| {
            if e.code().is_some_and(|c| c.to_string() == "value::decode") {
                IndexRowCorrupt::from_decode(index_name, e).into()
            } else {
                e
            }
        })
    })
}

/// Admit decoded relation-search rows at the engine→query seam.
pub(crate) fn admit_relation_search_hits(tuples: Vec<Tuple>) -> miette::Result<SearchHits> {
    SearchHits::admit_decoded(tuples).map_err(|d| {
        miette::miette!("search hit admission refused: {d:?}")
    })
}

/// Materialize admitted search hits for test assertions (output boundary).
pub(crate) fn search_rows(hits: SearchHits) -> miette::Result<Vec<Tuple>> {
    hits.materialize_all_tuples()
        .map_err(|e| miette::miette!("search hit materialization refused: {e:?}"))
}

impl IndexRowCorrupt {
    pub(crate) fn new(index: &str, row: &[DataValue], reason: IndexCorruptReason) -> Self {
        IndexRowCorrupt {
            index: index.to_string(),
            row: format!("{row:?}"),
            reason,
        }
    }

    /// The codec refused a scanned index row's stored bytes before they
    /// could become a tuple: wrap that raw decode failure as this index's
    /// own typed corruption error, so a `DecodeError` never leaks out of
    /// an engine as the engine's contract.
    pub(crate) fn from_decode(index: &str, err: impl std::fmt::Display) -> Self {
        IndexRowCorrupt {
            index: index.to_string(),
            row: "<undecodable bytes>".to_string(),
            reason: IndexCorruptReason::DecodeFailed {
                detail: err.to_string(),
            },
        }
    }
}
