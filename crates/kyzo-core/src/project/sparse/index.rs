/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The sparse-vector index engine: inverted-list maintenance and exact
//! dot-product retrieval over the SPLADE/BM25-family shape (a document is a
//! sparse map from a `u32` dimension to a non-negative `f32` weight, most
//! dimensions absent), against the kernel's transaction species.
//!
//! This module is NEW to KyzoDB — CozoDB had no sparse-vector index — but it is
//! cut from the same cloth as the FTS engine ([`crate::project::text::fts`]):
//! PURE FUNCTIONS over the kernel's [`ReadTx`]/[`WriteTx`] species, one stored
//! relation as the inverted structure, the typed [`IndexRowCorrupt`] on every
//! decode path, and the total-document count hoisted to the caller.
//!
//! ## Storage — one inverted relation
//!
//! A sparse index IS a single stored relation ([`sparse_index_metadata`]): one
//! row per `(dimension, document)` posting, keyed `[dim, src_key…]` with the
//! `weight` as its only value column. A document with `d` non-zero dimensions
//! writes `d` postings; the inverted layout means a query touching dimension
//! `q` reads exactly the documents that also carry `q`, in ascending
//! `src_key` order (the on-disk memcmp order).
//!
//! ## Scoring — exact dot product, fixed summation order
//!
//! [`sparse_search`] scores each candidate document by the dot product of the
//! query vector and the document vector over their shared dimensions. Scoring
//! is EXACT (a full accumulation over the intersected posting lists); pruning
//! (WAND-style upper-bound skipping) is a named next camp, not this engine —
//! see the design notes.
//!
//! The accumulation order is FIXED so results are byte-identical across runs:
//! the outer loop walks the query's dimensions in ascending order and each
//! posting list scans in ascending `src_key` order, so a given document's
//! score is a sum whose terms are added in ascending-dimension order — the
//! same order the naive reference uses. The accumulator's hash-map iteration
//! order is irrelevant to the score VALUE, because each dimension contributes
//! exactly one term to a given document and the dimensions are visited in
//! canonical order. Scores are `f32`; only positive scores are hits.
//!
//! ## Laws (inherited from the index-operator tier)
//!
//! - **Admission** ([`admit_sparse`]): a NaN, infinite, or negative weight is
//!   unrepresentable — refused typed at admission, the way the HNSW engine
//!   refuses a non-finite vector. A repeated dimension is a malformed vector,
//!   also refused. Both [`sparse_put`] and [`sparse_search`] admit their input
//!   through the one gate, and the read path re-checks stored weights so a
//!   corrupted store can never poison a score with a NaN.
//! - **Corruption is an error, never a panic** (Law 5): every stored posting is
//!   decoded through the kernel's fallible scan helpers, and every column
//!   access that could fail on malformed bytes is the typed [`IndexRowCorrupt`]
//!   with the row's key context. A base row an index points at that has
//!   vanished is the same typed error.
//! - **Determinism**: candidate iteration and tie-breaks are in canonical
//!   memcmp order; top-k selection is score-descending with the document key
//!   (memcmp order) as a total-ordering tie-break.
//!
//! ## Seams
//!
//! - **RA operator tier** (`query/ra.rs`): drives [`sparse_search`] per parent
//!   tuple and maps the appended `score` column to a binding — a sparse hit is
//!   a relation row like any other, joinable against dense/FTS results.
//! - **Mutation tier**: calls [`sparse_put`] after every base-relation put and
//!   [`sparse_del`] before every delete, in the same transaction, having
//!   produced the sparse vector from the row via the manifest's extractor
//!   (the extraction from documents/embeddings is the caller's business, the
//!   way the FTS extractor seam produces text).
//!
//! ## Projection kind (story #305)
//!
//! [`Sparse`] is this engine's `K` parameterization of the shared
//! [`crate::project::projection`] build→seal→query machine. Build→seal→query
//! goes through that machine; there is no bespoke per-engine seal or
//! freshness protocol. Relation-backed [`sparse_put`] / [`sparse_search`]
//! remain the kernel inverted-list algorithms.
//!
//! ## Design reference — Qdrant (Apache-2.0)
//!
//! The scoring and storage design here follows the sparse-vector implementation
//! of [Qdrant](https://github.com/qdrant/qdrant) (`lib/sparse`, Apache-2.0) as
//! its reference standard. This module is an INDEPENDENT KyzoDB implementation
//! written against the KyzoDB kernel — **no Qdrant code is copied or adapted**,
//! so no Qdrant copyright header applies; the credit is for the design we
//! learned from. Qdrant does not endorse or maintain KyzoDB. The convergences
//! and the one deliberate divergence:
//!
//! - **Types converge exactly.** Qdrant's public shape is `Vec<(u32, f32)>` —
//!   `pub type DimId = u32; pub type DimWeight = f32;` — which is precisely this
//!   engine's `(dimension, weight)` input.
//! - **Storage converges.** An inverted index mapping each dimension to a
//!   posting list of `(record, weight)`; dot product is the only metric.
//! - **Divergence — negative weights.** Qdrant PERMITS negative weights and, as
//!   a consequence, DISABLES its `max_next_weight` pruning whenever a query
//!   carries a negative (the precomputed upper bounds do not account for
//!   negative contributions). This engine instead REFUSES negative weights at
//!   admission (a tier law). The trade: it forecloses signed-weight models that
//!   Qdrant supports, but it makes the future WAND pruning (see the design
//!   notes) UNCONDITIONALLY sound and keeps scoring NaN-free by construction.
//!   Lifting this to Qdrant parity is a scope decision, not a silent default.

use std::cmp::Reverse;

use crate::project::contract::RankScore;
use miette::{Diagnostic, Result, bail, miette};
use rustc_hash::FxHashMap;
use smartstring::SmartString;
use thiserror::Error;

use crate::project::contract::{IndexCorruptReason, IndexRowCorrupt};
use crate::project::projection::{ProjectionKind, RelationIndexSearch};
use crate::session::catalog::RelationHandle;
use crate::store::{ReadTx, WriteTx};
use kyzo_model::SourceSpan;
use kyzo_model::program::expr::Expr;
use kyzo_model::schema::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
use kyzo_model::value::{DataValue, Tuple};

// ---------------------------------------------------------------------------
// Projection kind — `K` of the shared build→seal→query machine (#305).
// ---------------------------------------------------------------------------

/// Sparse-vector index as a projection kind: one `K` of
/// [`ProjectionBuilder`](crate::project::projection::ProjectionBuilder) /
/// [`Sealed`](crate::project::projection::Sealed).
///
/// Relation-backed posting maintenance and search ([`sparse_put`],
/// [`Sparse::search_index`]) are the kernel algorithms — not a second
/// build/seal/freshness protocol. Search is owned by
/// [`RelationIndexSearch::search_relation`] (P103); [`Sparse::search_index`]
/// is the UFCS alias into that door.
#[cfg(test)]
use kyzo_model::program::expr::BindingPos;


pub(crate) use crate::exec::stdlib::convert::f64_to_f32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sparse;

impl ProjectionKind for Sparse {}

// ---------------------------------------------------------------------------
// Typed errors — the admission gate's refusals.
// ---------------------------------------------------------------------------

/// A sparse-vector weight was NaN, infinite, or negative. Non-negative finite
/// weights are the SPLADE/BM25-family invariant this engine enforces at
/// admission, the way the HNSW engine refuses a non-finite vector: a weight
/// that cannot participate in an honest dot product never reaches storage, and
/// a NaN can never enter a score.
#[derive(Debug, Error, Diagnostic)]
#[error("sparse weight for dimension {dim} is invalid: {reason}")]
#[diagnostic(code(index::sparse::weight_invalid))]
pub(crate) struct SparseWeightInvalid {
    pub(crate) dim: u32,
    pub(crate) reason: &'static str,
}

/// A sparse vector named the same dimension twice. A sparse vector IS a map
/// from dimension to weight; a repeated dimension is a malformed input, not a
/// summation the engine will silently perform.
#[derive(Debug, Error, Diagnostic)]
#[error("sparse vector names dimension {dim} more than once")]
#[diagnostic(code(index::sparse::duplicate_dimension))]
pub(crate) struct SparseDuplicateDimension {
    pub(crate) dim: u32,
}

/// An admitted sparse vector: finite non-negative weights, unique dimensions,
/// sorted ascending by dimension. The only construction door is
/// [`admit_sparse`] — a raw `Vec<(u32, f32)>` is never type-equal to this.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SparseVector {
    pairs: Vec<(u32, f32)>,
}

impl SparseVector {
    /// Whether this vector carries no dimensions.
    pub(crate) fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }
}

impl IntoIterator for SparseVector {
    type Item = (u32, f32);
    type IntoIter = std::vec::IntoIter<(u32, f32)>;

    fn into_iter(self) -> Self::IntoIter {
        self.pairs.into_iter()
    }
}

/// Validate and canonicalize a sparse vector: refuse NaN/infinite/negative
/// weights ([`SparseWeightInvalid`]) and repeated dimensions
/// ([`SparseDuplicateDimension`]), and return a sealed [`SparseVector`] whose
/// pairs are sorted by dimension ascending — the canonical memcmp order that
/// fixes the summation order of every score (see the module docs).
///
/// This is the engine's single admission gate: both [`sparse_put`] and
/// [`sparse_search`] pass their input through it, so a weight that cannot
/// participate in an honest dot product is unrepresentable downstream.
fn admit_sparse(vector: &[(u32, f32)]) -> Result<SparseVector> {
    let mut out = Vec::with_capacity(vector.len());
    for &(dim, weight) in vector {
        if !weight.is_finite() {
            bail!(SparseWeightInvalid {
                dim,
                reason: "not finite (NaN or infinite)",
            });
        }
        if weight < 0.0 {
            bail!(SparseWeightInvalid {
                dim,
                reason: "negative",
            });
        }
        out.push((dim, weight));
    }
    out.sort_by_key(|&(dim, _)| dim);
    for pair in out.windows(2) {
        if pair[0].0 == pair[1].0 {
            bail!(SparseDuplicateDimension { dim: pair[0].0 });
        }
    }
    Ok(SparseVector { pairs: out })
}

// ---------------------------------------------------------------------------
// The index relation's schema.
// ---------------------------------------------------------------------------

/// Mint the index relation's column metadata for a sparse index over `base`.
///
/// Keys: `dim` (the dimension id, a non-negative `Int`), then `src_*` (the base
/// relation's key columns). Non-key: `weight` (a `Float`). The dimension is the
/// leading key column so a query dimension's posting list is a single prefix
/// scan.
pub(crate) fn sparse_index_metadata(base: &StoredRelationMetadata) -> StoredRelationMetadata {
    let mut keys = vec![ColumnDef {
        name: SmartString::from("dim"),
        typing: NullableColType::required(ColType::Int),
        default_gen: None,
    }];
    for k in base.keys.iter() {
        keys.push(ColumnDef {
            name: format!("src_{}", k.name).into(),
            typing: k.typing.clone(),
            default_gen: None,
        });
    }
    let non_keys = vec![ColumnDef {
        name: SmartString::from("weight"),
        typing: NullableColType::required(ColType::Float),
        default_gen: None,
    }];
    StoredRelationMetadata { keys, non_keys }
}

// ---------------------------------------------------------------------------
// Index maintenance.
// ---------------------------------------------------------------------------

/// The base-key suffix of a sparse posting: `src_key…` copied from the base
/// row. Callers prepend the dimension themselves — no placeholder slot.
fn posting_src_tail(base_key_len: usize, tuple: &[DataValue]) -> Tuple {
    Tuple::from_iter(tuple[..base_key_len].iter().cloned())
}

/// Index one base-relation row's sparse vector: write one posting per
/// dimension, its `weight` as the value.
///
/// The `vector` is the caller's already-extracted `(dimension, weight)` pairs
/// (the extraction seam); it is admitted here, so a NaN/infinite/negative
/// weight or a repeated dimension is refused typed before any write.
///
/// Contract: the mutation tier calls this after every put on the base
/// relation, in the same transaction, having first removed the row's previous
/// postings via [`sparse_del`] — a dimension that vanished from the new vector
/// must be deleted, and the del-before-put discipline owns that (exactly as the
/// FTS engine's re-put path does).
///
/// Host mutation door: session `IndexKind::Sparse` is [OPEN] — this algorithm
/// is complete; the catalog/ops arm is the unbuilt seat.
pub(crate) fn sparse_put<T: WriteTx>(
    tx: &mut T,
    tuple: &[DataValue],
    vector: &[(u32, f32)],
    base: &RelationHandle,
    idx: &RelationHandle,
) -> Result<()> {
    let base_key_len = base.metadata.keys.len();
    if tuple.len() < base_key_len {
        bail!(IndexRowCorrupt::new(
            &base.name,
            tuple,
            IndexCorruptReason::RowShorterThanKey,
        ));
    }
    let vector = admit_sparse(vector)?;
    let tail = posting_src_tail(base_key_len, tuple);
    for (dim, weight) in vector {
        let mut key = Tuple::with_capacity(1 + base_key_len);
        key.push(DataValue::from(i64::from(dim)));
        key.extend(tail.as_slice().iter().cloned());
        let val = [DataValue::from(f64::from(weight))];
        let key_bytes = idx.encode_key_for_store(key.as_slice(), SourceSpan::default())?;
        let val_bytes = idx.encode_val_only_for_store(&val, SourceSpan::default())?;
        tx.put(&key_bytes, &val_bytes)?;
    }
    Ok(())
}

/// Un-index one base-relation row: delete every posting its sparse vector
/// contributed.
///
/// Contract: the mutation tier calls this before deleting the row from the base
/// relation (and before re-putting a changed row), in the same transaction. The
/// `vector` must be the same one the row was indexed with, so the set of
/// dimensions matches what [`sparse_put`] wrote.
///
/// Host mutation door: session `IndexKind::Sparse` is [OPEN] — this algorithm
/// is complete; the catalog/ops arm is the unbuilt seat.
pub(crate) fn sparse_del<T: WriteTx>(
    tx: &mut T,
    tuple: &[DataValue],
    vector: &[(u32, f32)],
    base: &RelationHandle,
    idx: &RelationHandle,
) -> Result<()> {
    let base_key_len = base.metadata.keys.len();
    if tuple.len() < base_key_len {
        bail!(IndexRowCorrupt::new(
            &base.name,
            tuple,
            IndexCorruptReason::RowShorterThanKey,
        ));
    }
    let vector = admit_sparse(vector)?;
    let tail = posting_src_tail(base_key_len, tuple);
    for (dim, _weight) in vector {
        let mut key = Tuple::with_capacity(1 + base_key_len);
        key.push(DataValue::from(i64::from(dim)));
        key.extend(tail.as_slice().iter().cloned());
        let key_bytes = idx.encode_key_for_store(key.as_slice(), SourceSpan::default())?;
        tx.del(&key_bytes)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Query.
// ---------------------------------------------------------------------------

/// The total number of documents in the base relation. Hoisted so a multi-tuple
/// search counts once, mirroring the FTS engine's `fts_total_docs`.
///
/// Dot-product scoring does NOT consult this count — it is provided for the
/// hybrid-fusion / future df-idf reweighting seam (a BM25-family variant would
/// need `N`), so the RA tier can obtain it once without a hidden per-search
/// cache in these pure functions.
pub(crate) fn sparse_total_docs(tx: &impl ReadTx, base: &RelationHandle) -> Result<usize> {
    let (start, end) = base.whole_relation_bounds();
    tx.range_count(&start, &end)
}

/// Decode one posting row into `(src_key, weight)`. The row layout is
/// `[dim, src_key…, weight]`; every access that could fail on malformed bytes
/// is the typed [`IndexRowCorrupt`]. The stored weight is re-checked against
/// the admission invariant (finite, non-negative) so a corrupted store can
/// never inject a NaN or a negative into a score.
fn decode_posting(idx_name: &str, base_key_len: usize, row: &[DataValue]) -> Result<(Tuple, f32)> {
    let expected_len = base_key_len + 2;
    if row.len() != expected_len {
        bail!(IndexRowCorrupt::new(
            idx_name,
            row,
            IndexCorruptReason::WrongColumnCount {
                found: row.len(),
                expected: expected_len,
            },
        ));
    }
    let weight = row[base_key_len + 1].get_float().ok_or_else(|| {
        miette!(IndexRowCorrupt::new(
            idx_name,
            row,
            IndexCorruptReason::SparseWeightNotFloat,
        ))
    })?;
    let weight = f64_to_f32(weight);
    if !weight.is_finite() || weight < 0.0 {
        bail!(IndexRowCorrupt::new(
            idx_name,
            row,
            IndexCorruptReason::SparseWeightNotFiniteNonNeg,
        ));
    }
    Ok((Tuple::from_vec(row[1..=base_key_len].to_vec()), weight))
}

/// Whether sparse search appends the score column — the **one** bind encoding
/// (P038).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SparseBindScore {
    Omit,
    Append,
}

/// The parameters of one sparse search; the RA operator tier constructs this
/// from the resolved search atom.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SparseSearchParams {
    pub(crate) k: usize,
    /// Append the score as a trailing `Float` column (the RA tier maps it to a
    /// binding).
    pub(crate) bind_score: SparseBindScore,
}

/// One sparse relation-backed search — [`RelationIndexSearch::Request`] for
/// [`Sparse`] (P103).
#[derive(Debug, Clone, Copy)]
pub(crate) struct SparseSearchRequest<'a> {
    pub(crate) query: &'a [(u32, f32)],
    pub(crate) base: &'a RelationHandle,
    pub(crate) idx: &'a RelationHandle,
    pub(crate) params: &'a SparseSearchParams,
    pub(crate) filter_code: &'a Option<Expr>,
}

impl RelationIndexSearch for Sparse {
    type Request<'a> = SparseSearchRequest<'a>;

    fn search_relation<Tx: ReadTx>(
        tx: &Tx,
        request: Self::Request<'_>,
    ) -> Result<kyzo_model::value::SearchHits> {
        crate::project::contract::admit_relation_search_hits(sparse_search_body(
            tx,
            request.query,
            request.base,
            request.idx,
            request.params,
            request.filter_code,
        )?)
    }
}

/// Sparse dot-product search. Returns matching base-relation rows, highest
/// score first, each optionally extended by its score (a trailing `Float`).
///
/// # Filter semantics — `k` counts matching rows
///
/// When `filter_code` is present the score-based truncation is deferred until
/// after the filter runs, so `k` bounds the number of rows that pass the filter
/// (mirroring the FTS engine). With no filter, results are truncated to `k` by
/// score before the base rows are fetched.
///
/// # Determinism
///
/// The query is admitted (canonical ascending-dimension order); each query
/// dimension's posting list is scanned in ascending `src_key` order; each
/// document's score therefore accumulates in a FIXED order. Ties in score are
/// broken by the document key's memcmp order — a total order, so the result is
/// byte-identical across runs.
impl Sparse {
    /// Relation-backed sparse search — UFCS door into
    /// [`RelationIndexSearch::search_relation`] (P103). Formerly the free
    /// function `sparse_search`. Session `IndexKind::Sparse` is [OPEN]; the
    /// trait door and this UFCS alias are complete.
    pub(crate) fn search_index(
        tx: &impl ReadTx,
        query: &[(u32, f32)],
        base: &RelationHandle,
        idx: &RelationHandle,
        params: &SparseSearchParams,
        filter_code: &Option<Expr>,
    ) -> Result<kyzo_model::value::SearchHits> {
        Self::search_relation(
            tx,
            SparseSearchRequest {
                query,
                base,
                idx,
                params,
                filter_code,
            },
        )
    }
}

fn sparse_search_body(
    tx: &impl ReadTx,
    query: &[(u32, f32)],
    base: &RelationHandle,
    idx: &RelationHandle,
    params: &SparseSearchParams,
    filter_code: &Option<Expr>,
) -> Result<Vec<Tuple>> {
    let query = admit_sparse(query)?;
    if query.is_empty() {
        return Ok(vec![]);
    }
    let base_key_len = base.metadata.keys.len();

    // Accumulate the dot product per candidate document. The outer loop walks
    // the query's dimensions in ascending order and each posting list scans in
    // ascending src_key order, so every document's score is a sum whose terms
    // are added in ascending-dimension order — byte-identical across runs,
    // independent of the map's iteration order (each dimension adds exactly one
    // term to a given document).
    let mut scores: FxHashMap<Tuple, f32> = FxHashMap::default();
    for (dim, q_weight) in query {
        let prefix = Tuple::from_vec(vec![DataValue::from(i64::from(dim))]);
        for row in crate::project::contract::index_rows(&idx.name, idx.scan_prefix(tx, &prefix)) {
            let row = row?;
            let (doc_key, d_weight) = decode_posting(&idx.name, base_key_len, row.as_slice())?;
            *scores.entry(doc_key).or_insert(0.0f32) += q_weight * d_weight;
        }
    }

    // Only positive scores are hits: a zero accumulated dot product is not a
    // match (mirrors the FTS engine, where a matched document always carries a
    // positive term frequency). Admission forbids negative weights, so a score
    // is never negative — this excludes exactly the zero-overlap documents
    // (including the all-zero-weight edge case).
    let mut result: Vec<(Tuple, f32)> = scores.into_iter().filter(|(_, s)| *s > 0.0).collect();
    // Deterministic order: score descending, then the memcmp order of the
    // document key breaks ties (DataValue order equals the on-disk key order).
    result.sort_by(|(ka, sa), (kb, sb)| {
        Reverse(RankScore::of(f64::from(*sa)))
            .cmp(&Reverse(RankScore::of(f64::from(*sb))))
            .then_with(|| ka.cmp(kb))
    });
    if filter_code.is_none() {
        result.truncate(params.k);
    }

    // `params.k` is caller-controlled and unbounded; admit it through the one
    // allocation seam, bounded by the real (already-materialized) candidate
    // count, so an absurd `k` can never abort the allocator.
    let mut ret = Vec::with_capacity(crate::session::capacity::admit(params.k, result.len()));
    for (doc_key, score) in result {
        // Checked BEFORE pushing: `k == 0` (or any k already met) must
        // yield zero more rows, not "one past the limit" — pushing first
        // and checking `>= k` after made `k == 0` push exactly one row
        // whenever a filter predicate was present (mirrors the identical
        // fix in `project/text/fts.rs::fts_search`; `filter_code.is_none()`
        // truncates `result` to `k` up front and so never hit this loop
        // body at all, which is why the no-filter path never showed it).
        if ret.len() >= params.k {
            break;
        }
        let mut cand = base.get(tx, doc_key.as_slice())?.ok_or_else(|| {
            miette!(IndexRowCorrupt::new(
                &idx.name,
                doc_key.as_slice(),
                IndexCorruptReason::BaseRowMissing,
            ))
        })?;
        if matches!(params.bind_score, SparseBindScore::Append) {
            cand.push(DataValue::from(f64::from(score)));
        }
        if let Some(code) = filter_code
            && !crate::exec::expr::eval_pred(code, &cand)?
        {
            continue;
        }
        ret.push(cand);
    }
    Ok(ret)
}

// ---------------------------------------------------------------------------
// Tests: the engine's executable law.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::session::catalog::{KeyspaceKind, RelationHandle, create_relation};
    use crate::store::Storage;
    use crate::store::fjall::new_fjall_storage;
    use kyzo_model::program::InputRelationHandle;
    use kyzo_model::program::symbol::Symbol;
    use miette::{IntoDiagnostic, Result, miette};

    macro_rules! sparse_rows {
        ($($arg:expr),* $(,)?) => {{
            crate::project::contract::search_rows(Sparse::search_index($($arg),*)?)?
        }};
    }

    fn col(name: &str, coltype: ColType) -> ColumnDef {
        ColumnDef {
            name: SmartString::from(name),
            typing: NullableColType::required(coltype),
            default_gen: None,
        }
    }

    fn input_handle(name: &str, metadata: StoredRelationMetadata) -> InputRelationHandle {
        let key_bindings = metadata
            .keys
            .iter()
            .map(|c| Symbol::new(c.name.clone(), SourceSpan(0, 0)))
            .collect();
        let dep_bindings = metadata
            .non_keys
            .iter()
            .map(|c| Symbol::new(c.name.clone(), SourceSpan(0, 0)))
            .collect();
        InputRelationHandle {
            name: Symbol::new(name, SourceSpan(0, 0)),
            metadata,
            key_bindings,
            dep_bindings,
            span: SourceSpan(0, 0),
        }
    }

    /// Base relation: `k` (Int key) and a `tag` (String) so base rows exist to
    /// be fetched, and a filter has a column to read.
    fn base_meta() -> StoredRelationMetadata {
        StoredRelationMetadata {
            keys: vec![col("k", ColType::Int)],
            non_keys: vec![col("tag", ColType::String)],
        }
    }

    struct Fixture {
        base: RelationHandle,
        idx: RelationHandle,
    }

    /// A document: base key, a tag payload, and its sparse vector.
    type Doc<'a> = (i64, &'a str, &'a [(u32, f32)]);

    fn setup(db: &impl Storage, docs: &[Doc]) -> Result<Fixture> {
        let meta = base_meta();
        let mut tx = db.write_tx()?;
        let base = create_relation(
            &mut tx,
            input_handle("docs", meta.clone()),
            KeyspaceKind::Facts,
        )?;
        let idx = create_relation(
            &mut tx,
            input_handle("docs:sparse", sparse_index_metadata(&meta)),
            KeyspaceKind::AlgorithmState,
        )?;
        for (k, tag, vector) in docs {
            let row = vec![DataValue::from(*k), DataValue::from(*tag)];
            base.put_fact(
                &mut tx,
                &row,
                kyzo_model::value::ValidityTs::of_micros(0),
                SourceSpan(0, 0),
            )?;
            sparse_put(&mut tx, &row, vector, &base, &idx)?;
        }
        tx.commit().map_err(|e| miette!("{e}"))?;
        Ok(Fixture { base, idx })
    }

    fn params(k: usize) -> SparseSearchParams {
        SparseSearchParams {
            k,
            bind_score: SparseBindScore::Append,
        }
    }

    /// Run a search and project `(key, score)`.
    fn run(db: &impl Storage, f: &Fixture, query: &[(u32, f32)], k: usize) -> Result<Vec<(i64, f32)>> {
        let rtx = db.read_tx()?;
        let hits = sparse_rows!(&rtx, query, &f.base, &f.idx, &params(k), &None);
        let mut out = Vec::with_capacity(hits.len());
        for t in &hits {
            let key = t[0].get_int().ok_or_else(|| miette!("expected int key"))?;
            let score_dv = t.last().ok_or_else(|| miette!("expected score"))?;
            let score = score_dv
                .get_float()
                .ok_or_else(|| miette!("expected float score"))?;
            out.push((key, f64_to_f32(score)));
        }
        Ok(out)
    }

    /// Naive reference: for every document, dot-product its stored sparse vector
    /// against the query BY HAND, accumulating in ascending shared-dimension
    /// order (the same fixed order the engine uses). Obviously correct — a full
    /// scan, no inverted index — so `sparse_search` must agree byte-for-byte.
    fn naive_dot(docs: &[Doc], query: &[(u32, f32)]) -> Vec<(i64, f32)> {
        let mut q = query.to_vec();
        q.sort_by_key(|&(d, _)| d);
        let mut out = vec![];
        for (key, _tag, vector) in docs {
            let dmap: BTreeMap<u32, f32> = vector.iter().copied().collect();
            let mut score = 0.0f32;
            for &(qd, qw) in &q {
                if let Some(&dw) = dmap.get(&qd) {
                    score += qw * dw; // ascending dimension order
                }
            }
            if score > 0.0 {
                out.push((*key, score));
            }
        }
        out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        out
    }

    #[test]
    fn dot_scoring_matches_naive_reference() -> Result<()> {
        let dir = tempfile::tempdir().into_diagnostic()?;
        let db = new_fjall_storage(dir.path())?;
        // A spread of overlaps: shared dims, disjoint dims, differing weights.
        let docs: &[Doc] = &[
            (1, "a", &[(0, 1.0), (2, 2.0), (5, 0.5)]),
            (2, "b", &[(0, 3.0), (1, 1.0)]),
            (3, "c", &[(2, 1.0), (5, 4.0), (9, 2.0)]),
            (4, "d", &[(7, 1.0), (8, 1.0)]), // disjoint from the query below
        ];
        let f = setup(&db, docs)?;

        let query = &[(0, 2.0f32), (2, 1.0f32), (5, 1.0f32)];
        let got = run(&db, &f, query, 10)?;
        let want = naive_dot(docs, query);
        assert_eq!(got, want, "engine must match the naive full-scan reference");
        // Doc 4 shares no dimension with the query: never a hit.
        assert!(!got.iter().any(|(k, _)| *k == 4));

        // A dimension nobody carries contributes nothing, not an error.
        assert!(run(&db, &f, &[(100, 5.0)], 10)?.is_empty());
        Ok(())
    }

    /// The summation order is PINNED: a document whose shared dimensions carry
    /// weights that make the f32 sum order-sensitive scores to the ascending-
    /// dimension result exactly. If the accumulation order were reversed, the
    /// score would round differently and this assertion would bite.
    #[test]
    fn summation_order_is_pinned() -> Result<()> {
        let dir = tempfile::tempdir().into_diagnostic()?;
        let db = new_fjall_storage(dir.path())?;
        // dims 1,2 -> 1.0 ; dim 3 -> 2^24. Query weights all 1.0.
        // Ascending: (1 + 1) + 2^24 = 2^24 + 2 = 16777218 (exact at ULP 2).
        // Descending: ((2^24 + 1) + 1) = 2^24 = 16777216 (the 1s fall below ULP).
        let big = 16_777_216.0f32; // 2^24
        let docs: &[Doc] = &[(1, "x", &[(1, 1.0), (2, 1.0), (3, big)])];
        let f = setup(&db, docs)?;
        let got = run(&db, &f, &[(1, 1.0), (2, 1.0), (3, 1.0)], 1)?;
        assert_eq!(got.len(), 1);
        // 16777218 vs the reversed-order 16777216 differ by 2; a tolerance < 1
        // distinguishes them while staying clippy-clean.
        assert!(
            (got[0].1 - 16_777_218.0f32).abs() < 1.0,
            "ascending-order sum is 2^24 + 2, got {}",
            got[0].1
        );
        // The reference agrees.
        assert_eq!(got, naive_dot(docs, &[(1, 1.0), (2, 1.0), (3, 1.0)]));
        Ok(())
    }

    #[test]
    fn topk_tie_determinism_and_truncation() -> Result<()> {
        let dir = tempfile::tempdir().into_diagnostic()?;
        let db = new_fjall_storage(dir.path())?;
        // Three docs with IDENTICAL scores against the query (dim 0, weight 1).
        let docs: &[Doc] = &[
            (30, "c", &[(0, 1.0)]),
            (10, "a", &[(0, 1.0)]),
            (20, "b", &[(0, 1.0)]),
        ];
        let f = setup(&db, docs)?;
        let query = &[(0, 1.0f32)];

        // All tie at score 1.0; the tie-break is ascending document key.
        let got = run(&db, &f, query, 10)?;
        assert_eq!(
            got.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            vec![10, 20, 30],
            "score ties break on ascending document key"
        );
        for (_, s) in &got {
            assert!((s - 1.0).abs() < 1e-6);
        }

        // Truncation to k keeps the k lowest keys among the tied set.
        let got = run(&db, &f, query, 2)?;
        assert_eq!(
            got.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            vec![10, 20]
        );
        Ok(())
    }

    #[test]
    fn put_del_round_trip() -> Result<()> {
        let dir = tempfile::tempdir().into_diagnostic()?;
        let db = new_fjall_storage(dir.path())?;
        let vec1: &[(u32, f32)] = &[(0, 1.0), (1, 2.0)];
        let vec2: &[(u32, f32)] = &[(0, 1.0), (2, 3.0)];
        let docs: &[Doc] = &[(1, "a", vec1), (2, "b", vec2)];
        let f = setup(&db, docs)?;

        // Both carry dim 0.
        assert_eq!(run(&db, &f, &[(0, 1.0)], 10)?.len(), 2);

        // Delete doc 1's postings.
        let mut tx = db.write_tx()?;
        let row1 = vec![DataValue::from(1), DataValue::from("a")];
        sparse_del(&mut tx, &row1, vec1, &f.base, &f.idx)?;
        tx.commit().map_err(|e| miette!("{e}"))?;

        // Doc 1 is gone from every dimension it contributed.
        let got = run(&db, &f, &[(0, 1.0)], 10)?;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 2);
        assert!(
            run(&db, &f, &[(1, 1.0)], 10)?.is_empty(),
            "doc 1's dim 1 withdrawn"
        );
        // Doc 2 untouched.
        assert_eq!(run(&db, &f, &[(2, 1.0)], 10)?[0].0, 2);
        Ok(())
    }

    /// Two fresh builds of the same documents produce byte-identical search
    /// results — the full result tuples AND the score bit patterns.
    #[test]
    fn byte_identical_across_two_fresh_builds() -> Result<()> {
        let docs: &[Doc] = &[
            (1, "a", &[(0, 0.25), (3, 1.5), (7, 0.75)]),
            (2, "b", &[(0, 0.5), (3, 0.5), (9, 2.0)]),
            (3, "c", &[(3, 1.0), (7, 1.0), (9, 1.0)]),
        ];
        let query = &[(0, 1.0f32), (3, 2.0f32), (7, 0.5f32), (9, 1.0f32)];

        let build_and_search = || -> Result<_> {
            let dir = tempfile::tempdir().into_diagnostic()?;
            let db = new_fjall_storage(dir.path())?;
            let f = setup(&db, docs)?;
            let rtx = db.read_tx()?;
            Ok(sparse_rows!(&rtx, query, &f.base, &f.idx, &params(10), &None))
        };

        let a = build_and_search()?;
        let b = build_and_search()?;
        assert_eq!(a, b, "two fresh builds must yield identical result tuples");
        // Strict: the score column's bit pattern is identical, not merely equal.
        let bits = |t: &[DataValue]| -> Result<u64> {
            let score = t
                .last()
                .ok_or_else(|| miette!("expected score"))?
                .get_float()
                .ok_or_else(|| miette!("expected float"))?;
            Ok(score.to_bits())
        };
        let mut a_bits = Vec::new();
        for t in &a {
            a_bits.push(bits(t.as_slice())?);
        }
        let mut b_bits = Vec::new();
        for t in &b {
            b_bits.push(bits(t.as_slice())?);
        }
        assert_eq!(a_bits, b_bits, "score bit patterns are byte-identical");
        assert!(!a.is_empty());
        Ok(())
    }

    #[test]
    fn corrupt_posting_is_typed_error_not_panic() -> Result<()> {
        let dir = tempfile::tempdir().into_diagnostic()?;
        let db = new_fjall_storage(dir.path())?;
        let f = setup(&db, &[(1, "a", &[(0, 1.0)])])?;

        let idx_keys = || -> Result<Vec<(fjall::Slice, fjall::Slice)>> {
            let rtx = db.read_tx()?;
            let lower = kyzo_model::value::encode_key_with_suffix(f.idx.id, &[], &[]);
            let upper = (f.idx.id.raw() + 1).to_be_bytes();
            rtx.range_scan(lower.as_bytes(), &upper)
                .collect::<Result<Vec<_>, _>>()
        };
        let search_err = || -> Result<miette::Report> {
            let rtx = db.read_tx()?;
            Sparse::search_index(&rtx, &[(0, 1.0)], &f.base, &f.idx, &params(1), &None)
                .err()
                .ok_or_else(|| miette!("corrupt posting must error, not panic"))
        };

        // (a) Garbage msgpack value: the scan's decode fails, still not a panic.
        let kvs = idx_keys()?;
        assert!(!kvs.is_empty());
        {
            let mut tx = db.write_tx()?;
            for (k, _) in &kvs {
                let mut garbage = vec![0u8; 8];
                garbage.push(0xc1); // reserved, never-valid msgpack byte
                tx.put(k, &garbage)?;
            }
            tx.commit().map_err(|e| miette!("{e}"))?;
        }
        assert!(
            search_err()?
                .downcast_ref::<crate::project::contract::IndexRowCorrupt>()
                .is_some(),
            "garbage value must surface as the typed IndexRowCorrupt"
        );

        // (b) A VALID tuple whose weight column is the wrong type: our own
        // decode_posting raises the typed IndexRowCorrupt.
        {
            let mut tx = db.write_tx()?;
            for (k, _) in &kvs {
                let bad = f.idx.encode_val_only_for_store(
                    &[DataValue::from("not a float")],
                    SourceSpan::default(),
                )?;
                tx.put(k, &bad)?;
            }
            tx.commit().map_err(|e| miette!("{e}"))?;
        }
        assert!(
            search_err()?
                .downcast_ref::<crate::project::contract::IndexRowCorrupt>()
                .is_some(),
            "wrong-typed weight must surface as the typed IndexRowCorrupt"
        );

        // (c) A finite-but-negative stored weight violates the admission
        // invariant on read — rejected typed so no NaN/negative reaches a score.
        {
            let mut tx = db.write_tx()?;
            for (k, _) in &kvs {
                let neg = f
                    .idx
                    .encode_val_only_for_store(&[DataValue::from(-1.0f64)], SourceSpan::default())?;
                tx.put(k, &neg)?;
            }
            tx.commit().map_err(|e| miette!("{e}"))?;
        }
        assert!(
            search_err()?
                .downcast_ref::<crate::project::contract::IndexRowCorrupt>()
                .is_some(),
            "negative stored weight must surface as the typed IndexRowCorrupt"
        );
        Ok(())
    }

    #[test]
    fn edges_empty_query_empty_index_all_zero() -> Result<()> {
        // Each sub-case uses its own store so the relation name does not
        // collide (a store holds one `docs`).
        let fresh = || -> Result<_> {
            let dir = tempfile::tempdir().into_diagnostic()?;
            let db = new_fjall_storage(dir.path())?;
            Ok((dir, db))
        };

        // Empty index: any query yields nothing.
        let (_d0, db0) = fresh()?;
        let empty = setup(&db0, &[])?;
        assert!(run(&db0, &empty, &[(0, 1.0)], 10)?.is_empty(), "empty index");

        // Empty query against a populated index: nothing.
        let (_d1, db1) = fresh()?;
        let f = setup(&db1, &[(1, "a", &[(0, 1.0), (1, 2.0)])])?;
        assert!(run(&db1, &f, &[], 10)?.is_empty(), "empty query");
        // All-zero query against a populated index: zero dot product, no hit.
        assert!(
            run(&db1, &f, &[(0, 0.0), (1, 0.0)], 10)?.is_empty(),
            "all-zero query"
        );

        // All-zero weights in a DOCUMENT: storing zero weights is allowed; they
        // simply never score (positive-score contract).
        let (_d2, db2) = fresh()?;
        let z = setup(&db2, &[(1, "a", &[(0, 0.0), (1, 0.0)])])?;
        assert!(
            run(&db2, &z, &[(0, 1.0), (1, 1.0)], 10)?.is_empty(),
            "all-zero doc"
        );
        Ok(())
    }

    #[test]
    fn admission_refuses_nan_infinite_negative_and_duplicate() -> Result<()> {
        let dir = tempfile::tempdir().into_diagnostic()?;
        let db = new_fjall_storage(dir.path())?;
        let meta = base_meta();
        let mut tx = db.write_tx()?;
        let base = create_relation(
            &mut tx,
            input_handle("docs", meta.clone()),
            KeyspaceKind::Facts,
        )?;
        let idx = create_relation(
            &mut tx,
            input_handle("docs:sparse", sparse_index_metadata(&meta)),
            KeyspaceKind::AlgorithmState,
        )?;
        let row = vec![DataValue::from(1), DataValue::from("a")];

        let put = |tx: &mut _, v: &[(u32, f32)]| sparse_put(tx, &row, v, &base, &idx);

        let e = put(&mut tx, &[(0, f32::NAN)]).err().ok_or_else(|| miette!("NaN must refuse"))?;
        assert!(
            e.downcast_ref::<SparseWeightInvalid>().is_some(),
            "NaN refused: {e:?}"
        );
        let e = put(&mut tx, &[(0, f32::INFINITY)]).err().ok_or_else(|| miette!("inf must refuse"))?;
        assert!(
            e.downcast_ref::<SparseWeightInvalid>().is_some(),
            "inf refused: {e:?}"
        );
        let e = put(&mut tx, &[(0, -0.5)]).err().ok_or_else(|| miette!("negative must refuse"))?;
        assert!(
            e.downcast_ref::<SparseWeightInvalid>().is_some(),
            "negative refused: {e:?}"
        );
        let e = put(&mut tx, &[(3, 1.0), (3, 2.0)]).err().ok_or_else(|| miette!("duplicate must refuse"))?;
        assert!(
            e.downcast_ref::<SparseDuplicateDimension>().is_some(),
            "duplicate dimension refused: {e:?}"
        );
        // A clean vector still admits.
        assert!(put(&mut tx, &[(0, 1.0), (2, 0.5)]).is_ok());
        match tx.abort() {
            crate::store::tx::Aborted => {}
        }
        Ok(())
    }

    /// A filter runs AFTER scoring and `k` counts matching rows (mirrors the
    /// FTS engine's post-filter semantics).
    #[test]
    fn filter_counts_matching_rows() -> Result<()> {
        let dir = tempfile::tempdir().into_diagnostic()?;
        let db = new_fjall_storage(dir.path())?;
        let docs: &[Doc] = &[
            (1, "keep", &[(0, 3.0)]),
            (2, "drop", &[(0, 2.0)]),
            (3, "keep", &[(0, 1.0)]),
        ];
        let f = setup(&db, docs)?;
        // Filter: tag == "keep". Column layout of the candidate: [k, tag, score].
        let filter = Expr::Apply {
            op: kyzo_model::program::op::OP_EQ,
            args: Box::new([
                Expr::Binding {
                    var: Symbol::new("tag", SourceSpan(0, 0)),
                    tuple_pos: BindingPos::Resolved(1),
                },
                Expr::Const {
                    val: DataValue::from("keep"),
                    span: SourceSpan(0, 0),
                },
            ]),
            span: SourceSpan(0, 0),
        };
        let rtx = db.read_tx()?;
        let p = SparseSearchParams {
            k: 10,
            bind_score: SparseBindScore::Append,
        };
        let hits = sparse_rows!(&rtx, &[(0, 1.0)], &f.base, &f.idx, &p, &Some(filter));
        let mut keys = Vec::with_capacity(hits.len());
        for t in &hits {
            keys.push(t[0].get_int().ok_or_else(|| miette!("expected int key"))?);
        }
        assert_eq!(
            keys,
            vec![1, 3],
            "only the 'keep' rows survive, score order"
        );
        Ok(())
    }
}
