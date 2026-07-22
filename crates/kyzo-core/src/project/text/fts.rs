/*
 * Copyright 2023, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0), re-architected for the KyzoDB kernel:
 *
 * - The engine is PURE FUNCTIONS over the kernel's [`ReadTx`]/[`WriteTx`]
 *   species ([`fts_put`], [`fts_del`], [`fts_search`]); the original's
 *   `SessionTx` methods (`put_fts_index_item`, `fts_search`, …) die with
 *   `SessionTx`'s old shape. The RA operator tier drives the search per
 *   parent tuple; the mutation tier drives put/del.
 * - Law 5 throughout. The original decoded every posting value with
 *   `rmp_serde::from_slice(&v[8..]).unwrap()` and read its columns with
 *   `get_slice().unwrap()` / `get_int().unwrap()` — a corrupt posting
 *   panicked mid-search. Here every stored row is decoded through the
 *   kernel's fallible scan helpers ([`RelationHandle::scan_prefix`] /
 *   [`scan_bounded_prefix`], which return `Result<Tuple>`), and every column
 *   access that could fail on malformed bytes is the typed
 *   [`IndexRowCorrupt`] with the row's key context. A base row an index
 *   points at that has vanished is the same typed error, not the original's
 *   bare `miette!("corrupted index")`.
 * - The original's `l_iter.next().unwrap()` on the first child of an `And` /
 *   `Near` node is gone: an empty boolean node contributes no documents
 *   rather than panicking (the parser's [`FtsExpr::flatten`] already drops
 *   empties, so this is belt-and-braces, but the engine never trusts the
 *   shape of an AST it did not itself build).
 * - Scoring is UNCHANGED (`Tf` / `TfIdf`, the original's formulae, pinned by
 *   a reference-scoring test against a naive scan) — and it is NOT BM25: the
 *   original never stored the `k1`/`b` parameters, and neither do we. The
 *   `FtsScoreKind` lives here rather than in `data/program.rs` (whose search
 *   config lands with the RA tier); it is the engine's own vocabulary.
 * - Total-document count `N` (needed only for `TfIdf`) is HOISTED to the
 *   caller via [`fts_total_docs`], the way the LSH engine hoists its
 *   permutation decode: a multi-tuple search pays the base-relation count
 *   once, not per literal, and the pure functions stay free of a hidden
 *   per-transaction cache.
 */

//! The full-text-search index engine: inverted-index maintenance and boolean
//! query evaluation with TF/TF-IDF scoring, against the kernel's transaction
//! species.
//!
//! An FTS index IS a single stored relation ([`fts_index_metadata`]): one row
//! per `(term, document)` posting, keyed `[word, src_key…]`, whose value holds
//! the parallel `(offset_from, offset_to, position)` arrays of every
//! occurrence of `word` in that document plus the document's total token
//! count. A row's text is produced by the manifest's compiled `extractor`,
//! tokenized by the index's [`TextAnalyzer`], and one posting is written per
//! distinct term.
//!
//! ## Query
//!
//! [`fts_search`] parses the query string ([`parse_fts_query`]) into a bounded
//! boolean AST ([`FtsExpr`]: literals, prefix literals, `AND`/`OR`/`NOT`, and
//! positional `NEAR`), rewrites every literal through the same analyzer that
//! built the index, evaluates the AST to a `document -> score` map, and
//! returns the highest-scoring base rows.
//!
//! ## Scoring — read the score kind before you compare runs
//!
//! - **`Tf`**: `term_frequency * booster` (per literal booster from `^n`).
//! - **`TfIdf`**: `tf * ln(1 + (N - df + 0.5) / (df + 0.5)) * booster`, where
//!   `N` is the total document count (via [`fts_total_docs`]) and `df` the
//!   number of documents a literal matched. This is a classic TF-IDF, **not
//!   BM25** — there is no length normalization and no `k1`/`b`, exactly as in
//!   the CozoDB original.
//!
//! ## Search result contract (post-filter semantics — user-visible)
//!
//! [`fts_search`] applies its `filter` predicate AFTER scoring and (when a
//! filter is present) suppresses the score-based truncation until after
//! filtering, so `k` counts MATCHING rows. See its docs.
//!
//! ## Projection kind (story #305)
//!
//! [`Fts`] is this engine's `K` parameterization of the shared
//! [`crate::project::projection`] build→seal→query machine. Build→seal→query
//! goes through that machine; there is no bespoke per-engine seal or
//! freshness protocol. Relation-backed [`fts_put`] / [`fts_search`] remain
//! the kernel inverted-index algorithms.
//!
//! ## Seams
//!
//! - **RA operator tier** (`query/ra.rs`): drives [`fts_search`] per parent
//!   tuple and maps the appended `score` column to a binding.
//! - **Mutation tier**: calls [`fts_put`] after every base-relation put and
//!   [`fts_del`] before every delete, in the same transaction.
//! - **Lifecycle tier**: `::fts create/drop` — creates the index relation
//!   from [`fts_index_metadata`], validates + builds the analyzer, keys the
//!   [`crate::project::text::TokenizerCache`] by the FULL index handle name, compiles
//!   the extractor, backfills via [`fts_put`], and attaches the
//!   [`crate::project::text::FtsIndexManifest`] to the base handle keeping `indices`
//!   sorted by name.

use std::cmp::Reverse;

use crate::project::contract::RankScore;
use miette::{Diagnostic, Result, bail, miette};
use rustc_hash::{FxHashMap, FxHashSet};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::parse::parse_fts_query;
use crate::project::contract::{IndexCorruptReason, IndexRowCorrupt};
use crate::project::projection::{ProjectionKind, RelationIndexSearch};
use crate::project::text::ast::TokenizeFtsExpr;
use crate::project::text::tokenizer::TextAnalyzer;
use crate::session::catalog::RelationHandle;
use crate::store::{ReadTx, WriteTx};
use kyzo_model::SourceSpan;
use kyzo_model::data_value_any;
use kyzo_model::parse::search::{FtsExpr, FtsLiteral, FtsNear};
use kyzo_model::program::expr::Expr;
use kyzo_model::schema::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
use kyzo_model::value::{DataValue, LARGEST_UTF_CHAR, ScanBound, Tuple};

// ---------------------------------------------------------------------------
// Projection kind — `K` of the shared build→seal→query machine (#305).
// ---------------------------------------------------------------------------

/// FTS as a projection kind: one `K` of
/// [`ProjectionBuilder`](crate::project::projection::ProjectionBuilder) /
/// [`Sealed`](crate::project::projection::Sealed).
///
/// Relation-backed posting maintenance and search ([`fts_put`],
/// [`Fts::search_index`]) are the kernel algorithms — not a second
/// build/seal/freshness protocol. Search is owned by
/// [`RelationIndexSearch::search_relation`] (P103); [`Fts::search_index`]
/// is the UFCS alias into that door.
#[cfg(test)]
use kyzo_model::program::expr::BindingPos;
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fts;

impl ProjectionKind for Fts {}

// ---------------------------------------------------------------------------
// Scoring vocabulary.
// ---------------------------------------------------------------------------

/// How [`fts_search`] scores a matched document. The engine's own type: the
/// RA-tier search config maps onto it — `query/search.rs` resolves the
/// `score_kind` search param (`tf`/`tf_idf`) to a variant, and
/// `query/ra/search.rs` reads it back off `FtsSearchParams`. **Not BM25** —
/// see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FtsScoreKind {
    /// `term_frequency * booster`.
    Tf,
    /// `tf * ln(1 + (N - df + 0.5) / (df + 0.5)) * booster`.
    TfIdf,
}

// ---------------------------------------------------------------------------
// Typed errors.
// ---------------------------------------------------------------------------

/// The FTS extractor evaluated to something other than a string (or `Null`).
/// A document must be text to be tokenized; a non-string extraction is a
/// definition error surfaced at index time, typed rather than the original's
/// ad-hoc inline error.
#[derive(Debug, Error, Diagnostic)]
#[error("FTS index extractor must return a string or null, got {got}")]
#[diagnostic(code(index::fts::extractor_type))]
pub(crate) struct FtsExtractorType {
    pub(crate) got: String,
}

// ---------------------------------------------------------------------------
// The index relation's schema.
// ---------------------------------------------------------------------------

/// Mint the index relation's column metadata for an FTS index over `base`.
///
/// Keys: `word` (the term), then `src_*` (the base relation's key columns).
/// Non-keys: `offset_from`, `offset_to`, `position` (parallel `List<Int>`
/// arrays, one entry per occurrence of `word` in the document) and
/// `total_length` (the document's total token count).
pub(crate) fn fts_index_metadata(base: &StoredRelationMetadata) -> StoredRelationMetadata {
    let mut keys = vec![ColumnDef {
        name: SmartString::from("word"),
        typing: NullableColType::required(ColType::String),
        default_gen: None,
    }];
    for k in base.keys.iter() {
        keys.push(ColumnDef {
            name: format!("src_{}", k.name).into(),
            typing: k.typing.clone(),
            default_gen: None,
        });
    }
    let int_list = || {
        NullableColType::required(ColType::List {
            eltype: Box::new(NullableColType::required(ColType::Int)),
            len: None,
        })
    };
    let non_keys = vec![
        ColumnDef {
            name: SmartString::from("offset_from"),
            typing: int_list(),
            default_gen: None,
        },
        ColumnDef {
            name: SmartString::from("offset_to"),
            typing: int_list(),
            default_gen: None,
        },
        ColumnDef {
            name: SmartString::from("position"),
            typing: int_list(),
            default_gen: None,
        },
        ColumnDef {
            name: SmartString::from("total_length"),
            typing: NullableColType::required(ColType::Int),
            default_gen: None,
        },
    ];
    StoredRelationMetadata { keys, non_keys }
}

// ---------------------------------------------------------------------------
// Index maintenance.
// ---------------------------------------------------------------------------

/// Evaluate the extractor and return the document text, or `None` when the
/// extraction is `Null` (a row with no text is simply not indexed).
fn extract_text(extractor: &Expr, tuple: &[DataValue]) -> Result<Option<String>> {
    match crate::exec::expr::eval_expr(extractor, tuple)? {
        DataValue::Null => Ok(None),
        DataValue::Str(s) => Ok(Some(s)),
        other @ (data_value_any!()) => bail!(FtsExtractorType {
            got: format!("{other:?}"),
        }),
    }
}

/// The base-key suffix of an FTS posting: `src_key…` copied from the base
/// row. Callers prepend the term themselves — no placeholder slot.
fn posting_src_tail(base_key_len: usize, tuple: &[DataValue]) -> Tuple {
    Tuple::from_iter(tuple[..base_key_len].iter().cloned())
}

/// Index one base-relation row: evaluate the extractor, tokenize the text,
/// and write one posting per distinct term (its occurrence offsets/positions
/// and the document's token count). A `Null` extraction indexes nothing.
///
/// Contract: the mutation tier calls this after every put on the base
/// relation, in the same transaction, having first removed the row's previous
/// postings via [`fts_del`] (a put over an existing key overwrites its
/// postings, but terms that vanished from the new text must be deleted — the
/// mutation tier's del-before-put discipline owns that, exactly as the LSH
/// engine's re-put path does).
pub(crate) fn fts_put<T: WriteTx>(
    tx: &mut T,
    tuple: &[DataValue],
    extractor: &Expr,
    tokenizer: &TextAnalyzer,
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
    let Some(text) = extract_text(extractor, tuple)? else {
        return Ok(());
    };

    // Collect, per distinct term, the parallel occurrence arrays.
    #[allow(clippy::type_complexity)]
    let mut collector: FxHashMap<
        SmartString<LazyCompact>,
        (Vec<DataValue>, Vec<DataValue>, Vec<DataValue>),
    > = FxHashMap::default();
    let mut count = 0i64;
    let mut token_stream = tokenizer.token_stream(&text);
    while let Some(token) = token_stream.next() {
        let term = SmartString::<LazyCompact>::from(&token.text);
        let (fr, to, position) = collector.entry(term).or_default();
        fr.push(DataValue::from(i64::try_from(token.offset_from()).map_err(|_| miette!("token offset_from overflow"))?));
        to.push(DataValue::from(i64::try_from(token.offset_to()).map_err(|_| miette!("token offset_to overflow"))?));
        position.push(DataValue::from(i64::try_from(token.position).map_err(|_| miette!("token position overflow"))?));
        count += 1;
    }

    let tail = posting_src_tail(base_key_len, tuple);
    for (term, (from, to, position)) in collector {
        let mut key = Tuple::with_capacity(1 + base_key_len);
        key.push(DataValue::Str(term.to_string()));
        key.extend(tail.as_slice().iter().cloned());
        let val = vec![
            DataValue::List(from),
            DataValue::List(to),
            DataValue::List(position),
            DataValue::from(count),
        ];
        let key_bytes = idx.encode_key_for_store(key.as_slice(), SourceSpan::default())?;
        let val_bytes = idx.encode_val_only_for_store(&val, SourceSpan::default())?;
        tx.put(&key_bytes, &val_bytes)?;
    }
    Ok(())
}

/// Un-index one base-relation row: re-tokenize its text and delete every
/// posting it contributed.
///
/// Contract: the mutation tier calls this before deleting the row from the
/// base relation (and before re-putting a changed row), in the same
/// transaction. The extractor and analyzer must be the same ones the row was
/// indexed with, so the set of terms matches what [`fts_put`] wrote.
pub(crate) fn fts_del<T: WriteTx>(
    tx: &mut T,
    tuple: &[DataValue],
    extractor: &Expr,
    tokenizer: &TextAnalyzer,
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
    let Some(text) = extract_text(extractor, tuple)? else {
        return Ok(());
    };
    let mut terms: FxHashSet<SmartString<LazyCompact>> = FxHashSet::default();
    let mut token_stream = tokenizer.token_stream(&text);
    while let Some(token) = token_stream.next() {
        terms.insert(SmartString::<LazyCompact>::from(&token.text));
    }
    let tail = posting_src_tail(base_key_len, tuple);
    for term in terms {
        let mut key = Tuple::with_capacity(1 + base_key_len);
        key.push(DataValue::Str(term.to_string()));
        key.extend(tail.as_slice().iter().cloned());
        let key_bytes = idx.encode_key_for_store(key.as_slice(), SourceSpan::default())?;
        tx.del(&key_bytes)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Query.
// ---------------------------------------------------------------------------

/// The total number of documents in the base relation — the `N` in TF-IDF.
/// Hoisted so a multi-tuple search counts once; pass `0` when the score kind
/// is `Tf` (the count is unused there).
pub(crate) fn fts_total_docs(tx: &impl ReadTx, base: &RelationHandle) -> Result<usize> {
    let (start, end) = base.whole_relation_bounds();
    tx.range_count(&start, &end)
}

/// One document's positions for one matched literal.
struct LiteralPostings {
    /// The document's base key columns.
    doc_key: Tuple,
    /// The token positions at which the literal occurred in the document.
    positions: Vec<u32>,
}

/// All documents (with occurrence positions) matching one literal. Exact
/// literals scan their term's postings; prefix literals scan the term-string
/// range `[value, value·MAX_CHAR]`.
fn literal_postings(
    tx: &impl ReadTx,
    idx: &RelationHandle,
    base_key_len: usize,
    literal: &FtsLiteral,
) -> Result<Vec<LiteralPostings>> {
    let value = literal.value();
    let scan: Box<dyn Iterator<Item = Result<Tuple>>> = if literal.is_prefix() {
        let mut upper = SmartString::<LazyCompact>::from(value);
        upper.push(LARGEST_UTF_CHAR);
        idx.scan_bounded_prefix(
            tx,
            &[],
            &[ScanBound::Value(DataValue::Str(value.to_string()))],
            &[ScanBound::Value(DataValue::Str(upper.to_string()))],
        )
    } else {
        idx.scan_prefix(
            tx,
            &Tuple::from_vec(vec![DataValue::Str(value.to_string())]),
        )
    };
    let scan = crate::project::contract::index_rows(&idx.name, scan);

    // Value column indices in the decoded tuple: word(0), src keys
    // (1..=base_key_len), then offset_from, offset_to, position, total_length.
    let position_col = base_key_len + 3;
    let expected_len = base_key_len + 5;
    let mut out = vec![];
    for row in scan {
        let row = row?;
        if row.len() != expected_len {
            bail!(IndexRowCorrupt::new(
                &idx.name,
                row.as_slice(),
                IndexCorruptReason::WrongColumnCount {
                    found: row.len(),
                    expected: expected_len,
                },
            ));
        }
        let positions = row[position_col].get_slice().ok_or_else(|| {
            miette!(IndexRowCorrupt::new(
                &idx.name,
                row.as_slice(),
                IndexCorruptReason::FtsPositionsNotList,
            ))
        })?;
        let positions = positions
            .iter()
            .map(|p| {
                p.get_int().and_then(|i| u32::try_from(i).ok()).ok_or_else(|| {
                    miette!(IndexRowCorrupt::new(
                        &idx.name,
                        row.as_slice(),
                        IndexCorruptReason::FtsPositionNotInt,
                    ))
                })
            })
            .collect::<Result<Vec<u32>>>()?;
        out.push(LiteralPostings {
            doc_key: Tuple::from_vec(row.as_slice()[1..=base_key_len].to_vec()),
            positions,
        });
    }
    Ok(out)
}

/// `tf * idf-or-1 * booster`. See the module docs for the exact formulae.
fn compute_score(
    tf: usize,
    n_found_docs: usize,
    n_total: usize,
    booster: f64,
    score_kind: FtsScoreKind,
) -> f64 {
    let tf = match u32::try_from(tf) {
        Ok(v) => f64::from(v),
        Err(_gt_u32) => match i64::try_from(tf) {
            Ok(i) => kyzo_model::value::Num::int(i).to_f64(),
            Err(_gt_i64) => 0.0,
        },
    };
    match score_kind {
        FtsScoreKind::Tf => tf * booster,
        FtsScoreKind::TfIdf => {
            let n_found_docs = match u32::try_from(n_found_docs) {
        Ok(v) => f64::from(v),
        Err(_gt_u32) => match i64::try_from(n_found_docs) {
            Ok(i) => kyzo_model::value::Num::int(i).to_f64(),
            Err(_gt_i64) => 0.0,
        },
    };
            let n_total_f = match u32::try_from(n_total) {
        Ok(v) => f64::from(v),
        Err(_gt_u32) => match i64::try_from(n_total) {
            Ok(i) => kyzo_model::value::Num::int(i).to_f64(),
            Err(_gt_i64) => 0.0,
        },
    };
            let idf = (1.0 + (n_total_f - n_found_docs + 0.5) / (n_found_docs + 0.5)).ln();
            tf * idf * booster
        }
    }
}

/// Evaluate the boolean AST to a `document -> score` map. Recursion depth is
/// bounded by the parser ([`parse_fts_query`] rejects excessive nesting), so
/// this cannot blow the stack on a hostile query.
fn eval_ast(
    tx: &impl ReadTx,
    ast: &FtsExpr,
    idx: &RelationHandle,
    base_key_len: usize,
    score_kind: FtsScoreKind,
    n_total: usize,
) -> Result<FxHashMap<Tuple, f64>> {
    Ok(match ast {
        FtsExpr::Literal(l) => {
            let found = literal_postings(tx, idx, base_key_len, l)?;
            let df = found.len();
            let mut res = FxHashMap::default();
            for lp in found {
                let score =
                    compute_score(lp.positions.len(), df, n_total, l.booster().0, score_kind);
                res.insert(lp.doc_key, score);
            }
            res
        }
        FtsExpr::And(children) => {
            // NonEmptyFtsExprs: at least one child.
            let mut iter = children.iter();
            let first = iter.next().expect("NonEmptyFtsExprs");
            let mut res = eval_ast(tx, first, idx, base_key_len, score_kind, n_total)?;
            for child in iter {
                let next = eval_ast(tx, child, idx, base_key_len, score_kind, n_total)?;
                res = res
                    .into_iter()
                    .filter_map(|(k, v)| next.get(&k).map(|nv| (k, v + nv)))
                    .collect();
            }
            res
        }
        FtsExpr::Or(children) => {
            let mut res: FxHashMap<Tuple, f64> = FxHashMap::default();
            for child in children.iter() {
                let next = eval_ast(tx, child, idx, base_key_len, score_kind, n_total)?;
                for (k, v) in next {
                    res.entry(k)
                        .and_modify(|old| *old = old.max(v))
                        .or_insert(v);
                }
            }
            res
        }
        FtsExpr::Near(FtsNear { literals, distance }) => eval_near(
            tx,
            literals.as_slice(),
            *distance,
            idx,
            base_key_len,
            score_kind,
            n_total,
        )?,
        FtsExpr::Not(fst, snd) => {
            let mut res = eval_ast(tx, fst, idx, base_key_len, score_kind, n_total)?;
            let exclude = eval_ast(tx, snd, idx, base_key_len, score_kind, n_total)?;
            for k in exclude.keys() {
                res.remove(k);
            }
            res
        }
    })
}

/// Positions still live for the NEAR chain after intersecting `prev` with
/// `cur` under `distance`. Extracted so the per-literal step is one named
/// authority — not nested filter_map/and_then twins the copy detector pairs.
fn near_live_positions(prev: &[u32], cur: &[u32], distance: u32) -> Option<Vec<u32>> {
    let mut live = FxHashSet::default();
    for &p in prev {
        for &c in cur {
            let within = if c > p {
                c - p <= distance
            } else {
                p - c <= distance
            };
            if within {
                live.insert(if c > p { p } else { c });
            }
        }
    }
    if live.is_empty() {
        None
    } else {
        Some(live.into_iter().collect())
    }
}

/// Positional `NEAR`: documents where every literal occurs, each within
/// `distance` token positions of an occurrence carried forward from the
/// previous literals.
fn eval_near(
    tx: &impl ReadTx,
    literals: &[FtsLiteral],
    distance: u32,
    idx: &RelationHandle,
    base_key_len: usize,
    score_kind: FtsScoreKind,
    n_total: usize,
) -> Result<FxHashMap<Tuple, f64>> {
    let mut iter = literals.iter();
    let Some(first) = iter.next() else {
        return Ok(FxHashMap::default());
    };
    // doc key -> the positions still "live" for the NEAR chain.
    let mut coll: FxHashMap<Tuple, Vec<u32>> = FxHashMap::default();
    for lp in literal_postings(tx, idx, base_key_len, first)? {
        coll.insert(lp.doc_key, lp.positions);
    }
    // The original re-scans the first literal too (iterating all literals,
    // not the tail); preserved so the semantics match exactly.
    for lit in literals {
        let next = literal_postings(tx, idx, base_key_len, lit)?;
        coll = next
            .into_iter()
            .filter_map(|lp| {
                let prev = coll.remove(&lp.doc_key)?;
                near_live_positions(&prev, &lp.positions, distance).map(|live| (lp.doc_key, live))
            })
            .collect();
    }
    let booster: f64 = literals.iter().map(|l| l.booster().0).sum();
    let df = coll.len();
    Ok(coll
        .into_iter()
        .map(|(k, live)| {
            (
                k,
                compute_score(live.len(), df, n_total, booster, score_kind),
            )
        })
        .collect())
}

/// Whether FTS appends the score column — the **one** bind encoding (P038).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FtsBindScore {
    Omit,
    Append,
}

/// The parameters of one FTS query; the RA operator tier constructs this from
/// the resolved search atom.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FtsSearchParams {
    pub(crate) k: usize,
    pub(crate) score_kind: FtsScoreKind,
    /// Append the score as a trailing `Float` column (the RA tier maps it to
    /// a binding).
    pub(crate) bind_score: FtsBindScore,
}

/// One FTS relation-backed search invocation — [`RelationIndexSearch::Request`]
/// for [`Fts`] (P103).
#[derive(Clone, Copy)]
pub(crate) struct FtsSearchRequest<'a> {
    pub(crate) cancel: &'a crate::rules::contract::CancelFlag,
    pub(crate) query: &'a str,
    pub(crate) base: &'a RelationHandle,
    pub(crate) idx: &'a RelationHandle,
    pub(crate) params: &'a FtsSearchParams,
    pub(crate) filter_code: &'a Option<Expr>,
    pub(crate) tokenizer: &'a TextAnalyzer,
    pub(crate) n_total: usize,
}

impl RelationIndexSearch for Fts {
    type Request<'a> = FtsSearchRequest<'a>;

    fn search_relation<Tx: ReadTx>(
        tx: &Tx,
        request: Self::Request<'_>,
    ) -> Result<kyzo_model::value::SearchHits> {
        crate::project::contract::admit_relation_search_hits(fts_search_body(
            request.cancel,
            tx,
            request.query,
            request.base,
            request.idx,
            request.params,
            request.filter_code,
            request.tokenizer,
            request.n_total,
        )?)
    }
}

/// Full-text search. Returns matching base-relation rows, highest score
/// first, each optionally extended by its score (a trailing `Float`).
///
/// # Filter semantics: `k` counts rows that pass the filter
///
/// A filtered search returns exactly `min(k, M)` rows, where `M` is the
/// number of query-matching documents whose base rows pass the filter —
/// the same result-set guarantee as the HNSW operator
/// (`hnsw::hnsw_knn`'s filter semantics), and for the same reason: a
/// search node's output
/// joins and negates like any relation, so a silently short result would
/// be a wrong answer, not a ranking artifact. Here the guarantee needs no
/// fallback: the scored candidate set is the COMPLETE evaluation of the
/// query AST over the posting lists, so deferring the `k`-truncation
/// until after the per-row filter is exact by construction. The cost is
/// that a selective filter fetches base rows down the score order until
/// `k` pass. With no filter, results are truncated to `k` by score before
/// any base row is fetched.
///
/// `n_total` is [`fts_total_docs`] when `score_kind` is `TfIdf`, else `0`.

#[cfg(test)]
impl Fts {
    /// Test-only UFCS alias of [`RelationIndexSearch::search_relation`] (P103).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn search_index(
        cancel: &crate::rules::contract::CancelFlag,
        tx: &impl ReadTx,
        query: &str,
        base: &RelationHandle,
        idx: &RelationHandle,
        params: &FtsSearchParams,
        filter_code: &Option<Expr>,
        tokenizer: &TextAnalyzer,
        n_total: usize,
    ) -> Result<kyzo_model::value::SearchHits> {
        Self::search_relation(
            tx,
            FtsSearchRequest {
                cancel,
                query,
                base,
                idx,
                params,
                filter_code,
                tokenizer,
                n_total,
            },
        )
    }
}

fn fts_search_body(
    cancel: &crate::rules::contract::CancelFlag,
    tx: &impl ReadTx,
    query: &str,
    base: &RelationHandle,
    idx: &RelationHandle,
    params: &FtsSearchParams,
    filter_code: &Option<Expr>,
    tokenizer: &TextAnalyzer,
    n_total: usize,
) -> Result<Vec<Tuple>> {
    let ast = parse_fts_query(query)?.tokenize(tokenizer);
    if ast.is_empty() {
        return Ok(vec![]);
    }
    let base_key_len = base.metadata.keys.len();
    let scored = eval_ast(tx, &ast, idx, base_key_len, params.score_kind, n_total)?;
    let mut result: Vec<(Tuple, f64)> = scored.into_iter().collect();
    // Deterministic order: score descending, then the memcmp order of the
    // document key breaks ties (the original left ties to hash-map order).
    result.sort_by(|(ka, sa), (kb, sb)| {
        Reverse(RankScore::of(*sa))
            .cmp(&Reverse(RankScore::of(*sb)))
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
        // whenever a filter predicate was present (`filter_code.is_none()`
        // truncates `result` to `k` up front and so never hit this loop
        // body at all, which is why the no-filter path never showed it).
        if ret.len() >= params.k {
            break;
        }
        cancel.check()?;
        let mut cand = base.get(tx, doc_key.as_slice())?.ok_or_else(|| {
            miette!(IndexRowCorrupt::new(
                &idx.name,
                doc_key.as_slice(),
                IndexCorruptReason::BaseRowMissing,
            ))
        })?;
        if matches!(params.bind_score, FtsBindScore::Append) {
            cand.push(DataValue::from(score));
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
    use super::*;

    use crate::project::text::TokenizerConfig;
    use crate::rules::contract::CancelFlag;
    use crate::session::catalog::{KeyspaceKind, RelationHandle, create_relation};
    use crate::store::Storage;
    use crate::store::fjall::new_fjall_storage;
    use kyzo_model::program::InputRelationHandle;
    use kyzo_model::program::symbol::Symbol;

    macro_rules! fts_rows {
        ($($arg:expr),* $(,)?) => {
            crate::project::contract::search_rows(
                Fts::search_index($($arg),*).unwrap()
            ).unwrap()
        };
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

    fn base_meta() -> StoredRelationMetadata {
        StoredRelationMetadata {
            keys: vec![col("k", ColType::Int)],
            non_keys: vec![col("v", ColType::String)],
        }
    }

    /// Simple whitespace-ish tokenizer, lowercased: the same analyzer used to
    /// build and to query.
    fn analyzer() -> TextAnalyzer {
        TokenizerConfig::admit("Simple", vec![])
            .unwrap()
            .build(&[TokenizerConfig::admit("Lowercase", vec![]).unwrap()])
            .unwrap()
    }

    /// The compiled extractor: project the text column (position 1).
    fn extractor() -> Expr {
        Expr::Binding {
            var: Symbol::new("v", SourceSpan(0, 0)),
            tuple_pos: BindingPos::Resolved(1),
        }
    }

    struct Fixture {
        base: RelationHandle,
        idx: RelationHandle,
        analyzer: TextAnalyzer,
        extractor: Expr,
    }

    fn setup(db: &impl Storage, rows: &[(i64, &str)]) -> Fixture {
        let meta = base_meta();
        let analyzer = analyzer();
        let extractor = extractor();
        let mut tx = db.write_tx().unwrap();
        let base = create_relation(
            &mut tx,
            input_handle("docs", meta.clone()),
            KeyspaceKind::Facts,
        )
        .unwrap();
        let idx = create_relation(
            &mut tx,
            input_handle("docs:fts", fts_index_metadata(&meta)),
            KeyspaceKind::AlgorithmState,
        )
        .unwrap();
        for (k, text) in rows {
            let row = vec![DataValue::from(*k), DataValue::from(*text)];
            base.put_fact(
                &mut tx,
                &row,
                kyzo_model::value::ValidityTs::from_raw(0),
                SourceSpan(0, 0),
            )
            .unwrap();
            fts_put(&mut tx, &row, &extractor, &analyzer, &base, &idx).unwrap();
        }
        tx.commit().unwrap();
        Fixture {
            base,
            idx,
            analyzer,
            extractor,
        }
    }

    fn params(k: usize, score_kind: FtsScoreKind) -> FtsSearchParams {
        FtsSearchParams {
            k,
            score_kind,
            bind_score: FtsBindScore::Append,
        }
    }

    fn run(db: &impl Storage, f: &Fixture, q: &str, p: FtsSearchParams) -> Vec<(i64, f64)> {
        let rtx = db.read_tx().unwrap();
        let n = if p.score_kind == FtsScoreKind::TfIdf {
            fts_total_docs(&rtx, &f.base).unwrap()
        } else {
            0
        };
        let hits = fts_rows!(
            &CancelFlag::default(),
            &rtx,
            q,
            &f.base,
            &f.idx,
            &p,
            &None,
            &f.analyzer,
            n,
        );
        hits.iter()
            .map(|t| {
                (
                    t[0].get_int().unwrap(),
                    t.last().unwrap().get_float().unwrap(),
                )
            })
            .collect()
    }

    /// Naive reference: scan the base relation, tokenize each doc, and score a
    /// single-term TF query by hand. `fts_search` must agree.
    fn naive_tf(rows: &[(i64, &str)], term: &str) -> Vec<i64> {
        let mut hits: Vec<(i64, usize)> = rows
            .iter()
            .filter_map(|(k, text)| {
                let tf = text
                    .to_lowercase()
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|w| *w == term)
                    .count();
                if tf > 0 { Some((*k, tf)) } else { None }
            })
            .collect();
        hits.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        hits.into_iter().map(|(k, _)| k).collect()
    }

    #[test]
    fn tf_scoring_matches_naive_reference() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let rows = [
            (1, "the cat sat on the mat"),
            (2, "the cat chased the cat away"), // cat twice
            (3, "a dog barked loudly"),
        ];
        let f = setup(&db, &rows);

        let got: Vec<i64> = run(&db, &f, "cat", params(10, FtsScoreKind::Tf))
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(got, naive_tf(&rows, "cat"), "doc 2 (tf=2) outranks doc 1");
        // Its score is exactly tf * booster = 2.0.
        let scored = run(&db, &f, "cat", params(10, FtsScoreKind::Tf));
        assert_eq!(scored[0].0, 2);
        assert!((scored[0].1 - 2.0).abs() < 1e-9, "tf score is 2.0");
        assert!((scored[1].1 - 1.0).abs() < 1e-9, "tf score is 1.0");

        // A term nobody has: no hits.
        assert!(run(&db, &f, "elephant", params(10, FtsScoreKind::Tf)).is_empty());
    }

    #[test]
    fn boolean_and_or_not_and_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let rows = [
            (1, "quick brown fox"),
            (2, "quick red fox"),
            (3, "slow brown bear"),
            (4, "quicksand trap"),
        ];
        let f = setup(&db, &rows);
        let keys = |v: Vec<(i64, f64)>| {
            let mut k: Vec<i64> = v.into_iter().map(|(k, _)| k).collect();
            k.sort();
            k
        };

        assert_eq!(
            keys(run(&db, &f, "quick AND fox", params(10, FtsScoreKind::Tf))),
            vec![1, 2]
        );
        assert_eq!(
            keys(run(&db, &f, "brown OR red", params(10, FtsScoreKind::Tf))),
            vec![1, 2, 3]
        );
        assert_eq!(
            keys(run(&db, &f, "fox NOT red", params(10, FtsScoreKind::Tf))),
            vec![1],
            "doc 2 has red, excluded"
        );
        // Prefix: quick* matches quick and quicksand.
        assert_eq!(
            keys(run(&db, &f, "quick*", params(10, FtsScoreKind::Tf))),
            vec![1, 2, 4]
        );
    }

    #[test]
    fn near_respects_distance() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let rows = [
            (1, "alpha beta gamma delta"), // alpha..delta distance 3
            (2, "alpha gamma beta delta"),
            (3, "alpha one two three four five delta"), // distance 6
        ];
        let f = setup(&db, &rows);
        let keys = |v: Vec<(i64, f64)>| {
            let mut k: Vec<i64> = v.into_iter().map(|(k, _)| k).collect();
            k.sort();
            k
        };
        // Within 3 positions: docs 1 and 2 qualify, doc 3 does not.
        assert_eq!(
            keys(run(
                &db,
                &f,
                "NEAR/3(alpha delta)",
                params(10, FtsScoreKind::Tf)
            )),
            vec![1, 2]
        );
        // Widen to 6: doc 3 joins.
        assert_eq!(
            keys(run(
                &db,
                &f,
                "NEAR/6(alpha delta)",
                params(10, FtsScoreKind::Tf)
            )),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn tfidf_prefers_rare_terms_and_uses_n() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        // "common" is in every doc; "rare" in one.
        let rows = [
            (1, "common rare"),
            (2, "common word"),
            (3, "common thing"),
            (4, "common again"),
        ];
        let f = setup(&db, &rows);
        let rare = run(&db, &f, "rare", params(10, FtsScoreKind::TfIdf));
        let common = run(&db, &f, "common", params(10, FtsScoreKind::TfIdf));
        // rare (df=1) scores strictly higher than common (df=4) at equal tf.
        assert!(
            rare[0].1 > common[0].1,
            "rare {} should beat common {}",
            rare[0].1,
            common[0].1
        );
        // And the exact idf formula: N=4, df=1 -> ln(1 + (4-1+0.5)/(1+0.5)).
        let expect = (1.0f64 + (4.0 - 1.0 + 0.5) / (1.0 + 0.5)).ln();
        assert!((rare[0].1 - expect).abs() < 1e-9, "tfidf formula pinned");
    }

    #[test]
    fn delete_withdraws_postings() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let rows = [(1, "findme keep"), (2, "findme also")];
        let f = setup(&db, &rows);
        assert_eq!(
            run(&db, &f, "findme", params(10, FtsScoreKind::Tf)).len(),
            2
        );

        let mut tx = db.write_tx().unwrap();
        let row1 = vec![DataValue::from(1), DataValue::from("findme keep")];
        fts_del(&mut tx, &row1, &f.extractor, &f.analyzer, &f.base, &f.idx).unwrap();
        tx.commit().unwrap();

        let got = run(&db, &f, "findme", params(10, FtsScoreKind::Tf));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 2, "only the surviving doc remains");
        // The deleted doc's other term is gone too.
        assert!(run(&db, &f, "keep", params(10, FtsScoreKind::Tf)).is_empty());
    }

    #[test]
    fn non_string_extraction_is_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let meta = base_meta();
        let mut tx = db.write_tx().unwrap();
        let base = create_relation(
            &mut tx,
            input_handle("docs", meta.clone()),
            KeyspaceKind::Facts,
        )
        .unwrap();
        let idx = create_relation(
            &mut tx,
            input_handle("docs:fts", fts_index_metadata(&meta)),
            KeyspaceKind::AlgorithmState,
        )
        .unwrap();
        let a = analyzer();
        // Extractor projects the INT key column (position 0): not a string.
        let bad_extractor = Expr::Binding {
            var: Symbol::new("k", SourceSpan(0, 0)),
            tuple_pos: BindingPos::Resolved(0),
        };
        let row = vec![DataValue::from(1), DataValue::from("text")];
        let err = fts_put(&mut tx, &row, &bad_extractor, &a, &base, &idx).unwrap_err();
        assert!(
            err.downcast_ref::<FtsExtractorType>().is_some(),
            "typed extractor error, got: {err:?}"
        );
        match tx.abort() {
            crate::store::tx::Aborted => {}
        }
    }

    #[test]
    fn corrupt_posting_is_typed_error_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let f = setup(&db, &[(1, "hello world")]);

        // Byte-flip every index row's value to reserved-msgpack garbage.
        let mut tx = db.write_tx().unwrap();
        let kvs: Vec<(fjall::Slice, fjall::Slice)> = {
            let lower = kyzo_model::value::encode_key_with_suffix(f.idx.id, &[], &[]);
            let upper = (f.idx.id.raw() + 1).to_be_bytes();
            tx.range_scan(lower.as_bytes(), &upper)
                .collect::<Result<Vec<_>>>()
                .unwrap()
        };
        assert!(!kvs.is_empty());
        for (k, _) in &kvs {
            let mut garbage = vec![0u8; 8];
            garbage.push(0xc1); // reserved, never-valid msgpack byte
            tx.put(k, &garbage).unwrap();
        }
        tx.commit().unwrap();

        let rtx = db.read_tx().unwrap();
        let err = Fts::search_index(
            &CancelFlag::default(),
            &rtx,
            "hello",
            &f.base,
            &f.idx,
            &params(1, FtsScoreKind::Tf),
            &None,
            &f.analyzer,
            0,
        )
        .expect_err("corrupt postings must error, not panic");
        assert!(
            err.downcast_ref::<crate::project::contract::IndexRowCorrupt>()
                .is_some(),
            "corrupt index bytes must surface as the typed IndexRowCorrupt: {err:?}"
        );
    }

    /// A query that PARSES but tokenizes to nothing (every term is a
    /// stopword) reaches the `ast.is_empty()` early return: empty results, not
    /// an error and not a panic. (An unparseable query — e.g. all whitespace —
    /// is a different, error path.)
    #[test]
    fn stopword_only_query_returns_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let meta = base_meta();
        // Analyzer with a stopword filter: "the"/"a" tokenize away.
        let a = TokenizerConfig::admit("Simple", vec![])
            .unwrap()
            .build(&[
                TokenizerConfig::admit("Lowercase", vec![]).unwrap(),
                TokenizerConfig::admit(
                    "Stopwords",
                    vec![DataValue::List(vec![
                        DataValue::from("the"),
                        DataValue::from("a"),
                    ])],
                )
                .unwrap(),
            ])
            .unwrap();
        let ex = extractor();
        let mut tx = db.write_tx().unwrap();
        let base = create_relation(
            &mut tx,
            input_handle("docs", meta.clone()),
            KeyspaceKind::Facts,
        )
        .unwrap();
        let idx = create_relation(
            &mut tx,
            input_handle("docs:fts", fts_index_metadata(&meta)),
            KeyspaceKind::AlgorithmState,
        )
        .unwrap();
        let row = vec![DataValue::from(1), DataValue::from("the cat sat")];
        base.put_fact(
            &mut tx,
            &row,
            kyzo_model::value::ValidityTs::from_raw(0),
            SourceSpan(0, 0),
        )
        .unwrap();
        fts_put(&mut tx, &row, &ex, &a, &base, &idx).unwrap();
        tx.commit().unwrap();

        let rtx = db.read_tx().unwrap();
        // "the" and "a" are stopwords: the query tokenizes to nothing.
        let hits = fts_rows!(
            &CancelFlag::default(),
            &rtx,
            "the AND a",
            &base,
            &idx,
            &params(10, FtsScoreKind::Tf),
            &None,
            &a,
            0,
        );
        assert!(
            hits.is_empty(),
            "stopword-only query yields no hits, no error"
        );
        // "cat" survives tokenization and matches.
        let hits = fts_rows!(
            &CancelFlag::default(),
            &rtx,
            "cat",
            &base,
            &idx,
            &params(10, FtsScoreKind::Tf),
            &None,
            &a,
            0,
        );
        assert_eq!(hits.len(), 1);
    }

    /// One-law surface: CJK / multi-byte text indexed under Cangjie must be
    /// prefix-searchable. Offsets written into postings are UTF-8 byte
    /// ranges that reconstruct the term — a wrong Cangjie offset stream
    /// would index garbage and miss the prefix hit.
    #[test]
    fn cjk_multibyte_index_and_prefix_search() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let meta = base_meta();
        // ForSearch: jieba-rs keeps 南京长江大桥 as one Default dict entry;
        // search mode emits 南京/长江/大桥 (+ whole) so prefix is meaningful.
        let a = TokenizerConfig::admit("Cangjie", vec![DataValue::from("search")])
            .unwrap()
            .build(&[])
            .unwrap();
        let ex = extractor();
        let mut tx = db.write_tx().unwrap();
        let base = create_relation(
            &mut tx,
            input_handle("docs", meta.clone()),
            KeyspaceKind::Facts,
        )
        .unwrap();
        let idx = create_relation(
            &mut tx,
            input_handle("docs:fts", fts_index_metadata(&meta)),
            KeyspaceKind::AlgorithmState,
        )
        .unwrap();
        let rows = [(1i64, "南京长江大桥"), (2i64, "北京天安门")];
        for (k, text) in rows {
            let row = vec![DataValue::from(k), DataValue::from(text)];
            base.put_fact(
                &mut tx,
                &row,
                kyzo_model::value::ValidityTs::from_raw(0),
                SourceSpan(0, 0),
            )
            .unwrap();
            fts_put(&mut tx, &row, &ex, &a, &base, &idx).unwrap();
        }
        tx.commit().unwrap();

        let f = Fixture {
            base,
            idx,
            analyzer: a,
            extractor: ex,
        };
        // Exact multi-byte term.
        let hit = run(&db, &f, "南京", params(10, FtsScoreKind::Tf));
        assert_eq!(hit.len(), 1, "exact CJK term: {hit:?}");
        assert_eq!(hit[0].0, 1);
        // Prefix over a multi-byte term: 南* matches 南京, not 北京.
        let pref = run(&db, &f, "南*", params(10, FtsScoreKind::Tf));
        assert_eq!(pref.len(), 1, "CJK prefix: {pref:?}");
        assert_eq!(pref[0].0, 1);
        let other = run(&db, &f, "北京", params(10, FtsScoreKind::Tf));
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].0, 2);
    }

    /// `k == 0` must bound the filtered path exactly like the unfiltered
    /// one: zero rows. The loop used to check `ret.len() >= k` AFTER
    /// pushing a candidate, so a filter-present search with k=0 returned
    /// one row instead of zero (the unfiltered path never showed it: it
    /// truncates `result` to `k` up front and skips the loop body
    /// entirely at k=0). Fixed by checking before pushing, in both this
    /// engine and the identical shape in `project/sparse/sparse.rs::sparse_search`.
    #[test]
    fn k_zero_filter_path_returns_zero_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let f = setup(&db, &[(1, "cat sat")]);
        let rtx = db.read_tx().unwrap();
        // Always-true filter: the constant `true`.
        let filter = Expr::Const {
            val: DataValue::from(true),
            span: SourceSpan(0, 0),
        };
        let p = params(0, FtsScoreKind::Tf);
        let with_filter = fts_rows!(
            &CancelFlag::default(),
            &rtx,
            "cat",
            &f.base,
            &f.idx,
            &p,
            &Some(filter),
            &f.analyzer,
            0,
        );
        assert!(
            with_filter.is_empty(),
            "k=0 + filter must return 0 rows, got {}",
            with_filter.len()
        );
        let without = fts_rows!(
            &CancelFlag::default(),
            &rtx,
            "cat",
            &f.base,
            &f.idx,
            &p,
            &None,
            &f.analyzer,
            0,
        );
        assert!(without.is_empty(), "k=0 without filter returns 0 rows");
    }
}
