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
//! ## Seams
//!
//! - **RA operator tier** (`query/ra.rs`): drives [`fts_search`] per parent
//!   tuple and maps the appended `score` column to a binding.
//! - **Mutation tier**: calls [`fts_put`] after every base-relation put and
//!   [`fts_del`] before every delete, in the same transaction.
//! - **Lifecycle tier**: `::fts create/drop` — creates the index relation
//!   from [`fts_index_metadata`], validates + builds the analyzer, keys the
//!   [`crate::fts::TokenizerCache`] by the FULL index handle name, compiles
//!   the extractor, backfills via [`fts_put`], and attaches the
//!   [`crate::fts::FtsIndexManifest`] to the base handle keeping `indices`
//!   sorted by name.

use std::cmp::Reverse;

use miette::{Diagnostic, Result, bail, miette};
use ordered_float::OrderedFloat;
use rustc_hash::{FxHashMap, FxHashSet};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::expr::{Bytecode, eval_bytecode, eval_bytecode_pred};
use crate::data::relation::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
use crate::data::span::SourceSpan;
use crate::data::tuple::Tuple;
use crate::data::value::{DataValue, LARGEST_UTF_CHAR};
use crate::fts::ast::{FtsExpr, FtsLiteral, FtsNear};
use crate::fts::tokenizer::TextAnalyzer;
use crate::parse::fts::parse_fts_query;
use crate::runtime::index::IndexRowCorrupt;
use crate::runtime::relation::RelationHandle;
use crate::storage::{ReadTx, WriteTx};

// ---------------------------------------------------------------------------
// Scoring vocabulary.
// ---------------------------------------------------------------------------

/// How [`fts_search`] scores a matched document. The engine's own type: the
/// RA-tier search config (`data/program.rs`) lands later and will map onto
/// this. **Not BM25** — see the module docs.
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
        typing: NullableColType {
            coltype: ColType::String,
            nullable: false,
        },
        default_gen: None,
    }];
    for k in base.keys.iter() {
        keys.push(ColumnDef {
            name: format!("src_{}", k.name).into(),
            typing: k.typing.clone(),
            default_gen: None,
        });
    }
    let int_list = || NullableColType {
        coltype: ColType::List {
            eltype: Box::new(NullableColType {
                coltype: ColType::Int,
                nullable: false,
            }),
            len: None,
        },
        nullable: false,
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
            typing: NullableColType {
                coltype: ColType::Int,
                nullable: false,
            },
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
fn extract_text(
    extractor: &[Bytecode],
    tuple: &[DataValue],
    stack: &mut Vec<DataValue>,
) -> Result<Option<SmartString<LazyCompact>>> {
    match eval_bytecode(extractor, tuple, stack)? {
        DataValue::Null => Ok(None),
        DataValue::Str(s) => Ok(Some(s)),
        other => bail!(FtsExtractorType {
            got: format!("{other:?}"),
        }),
    }
}

/// The FTS posting key under construction: `[word, src_key…]` with the word
/// slot left as `Bot` for the caller to fill per term.
fn posting_key_scaffold(base_key_len: usize, tuple: &[DataValue]) -> Tuple {
    let mut key = Vec::with_capacity(1 + base_key_len);
    key.push(DataValue::Bot);
    key.extend_from_slice(&tuple[..base_key_len]);
    key
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
    extractor: &[Bytecode],
    stack: &mut Vec<DataValue>,
    tokenizer: &TextAnalyzer,
    base: &RelationHandle,
    idx: &RelationHandle,
) -> Result<()> {
    let base_key_len = base.metadata.keys.len();
    if tuple.len() < base_key_len {
        bail!(IndexRowCorrupt::new(
            &base.name,
            tuple,
            "row shorter than the base relation's key",
        ));
    }
    let Some(text) = extract_text(extractor, tuple, stack)? else {
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
        fr.push(DataValue::from(token.offset_from as i64));
        to.push(DataValue::from(token.offset_to as i64));
        position.push(DataValue::from(token.position as i64));
        count += 1;
    }

    let mut key = posting_key_scaffold(base_key_len, tuple);
    for (term, (from, to, position)) in collector {
        key[0] = DataValue::Str(term);
        let val = vec![
            DataValue::List(from),
            DataValue::List(to),
            DataValue::List(position),
            DataValue::from(count),
        ];
        let key_bytes = idx.encode_key_for_store(&key, SourceSpan::default())?;
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
    extractor: &[Bytecode],
    stack: &mut Vec<DataValue>,
    tokenizer: &TextAnalyzer,
    base: &RelationHandle,
    idx: &RelationHandle,
) -> Result<()> {
    let base_key_len = base.metadata.keys.len();
    if tuple.len() < base_key_len {
        bail!(IndexRowCorrupt::new(
            &base.name,
            tuple,
            "row shorter than the base relation's key",
        ));
    }
    let Some(text) = extract_text(extractor, tuple, stack)? else {
        return Ok(());
    };
    let mut terms: FxHashSet<SmartString<LazyCompact>> = FxHashSet::default();
    let mut token_stream = tokenizer.token_stream(&text);
    while let Some(token) = token_stream.next() {
        terms.insert(SmartString::<LazyCompact>::from(&token.text));
    }
    let mut key = posting_key_scaffold(base_key_len, tuple);
    for term in terms {
        key[0] = DataValue::Str(term);
        let key_bytes = idx.encode_key_for_store(&key, SourceSpan::default())?;
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
    let start = base.encode_partial_key_for_store(&[]);
    let end = base.encode_partial_key_for_store(&[DataValue::Bot]);
    tx.range_count(start.as_bytes(), end.as_bytes())
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
    let value = literal.value.as_str();
    let scan: Box<dyn Iterator<Item = Result<Tuple>>> = if literal.is_prefix {
        let mut upper = literal.value.clone();
        upper.push(LARGEST_UTF_CHAR);
        idx.scan_bounded_prefix(
            tx,
            &[],
            &[DataValue::Str(SmartString::from(value))],
            &[DataValue::Str(upper)],
        )
    } else {
        idx.scan_prefix(tx, &vec![DataValue::Str(SmartString::from(value))])
    };

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
                &row,
                format!(
                    "FTS posting has {} columns, expected {expected_len}",
                    row.len()
                ),
            ));
        }
        let positions = row[position_col].get_slice().ok_or_else(|| {
            miette!(IndexRowCorrupt::new(
                &idx.name,
                &row,
                "FTS posting position column is not a list",
            ))
        })?;
        let positions = positions
            .iter()
            .map(|p| {
                p.get_int().map(|i| i as u32).ok_or_else(|| {
                    miette!(IndexRowCorrupt::new(
                        &idx.name,
                        &row,
                        "FTS posting position is not an integer",
                    ))
                })
            })
            .collect::<Result<Vec<u32>>>()?;
        out.push(LiteralPostings {
            doc_key: row[1..=base_key_len].to_vec(),
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
    let tf = tf as f64;
    match score_kind {
        FtsScoreKind::Tf => tf * booster,
        FtsScoreKind::TfIdf => {
            let n_found_docs = n_found_docs as f64;
            let idf = (1.0 + (n_total as f64 - n_found_docs + 0.5) / (n_found_docs + 0.5)).ln();
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
                let score = compute_score(lp.positions.len(), df, n_total, l.booster.0, score_kind);
                res.insert(lp.doc_key, score);
            }
            res
        }
        FtsExpr::And(children) => {
            let mut iter = children.iter();
            // An empty conjunction matches nothing (the parser's flatten drops
            // empties; the engine still never unwraps an AST it did not build).
            let Some(first) = iter.next() else {
                return Ok(FxHashMap::default());
            };
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
            for child in children {
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
            literals,
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
                coll.remove(&lp.doc_key).and_then(|prev| {
                    let mut live = FxHashSet::default();
                    for &p in &prev {
                        for &cur in &lp.positions {
                            let within = if cur > p {
                                cur - p <= distance
                            } else {
                                p - cur <= distance
                            };
                            if within {
                                live.insert(if cur > p { p } else { cur });
                            }
                        }
                    }
                    if live.is_empty() {
                        None
                    } else {
                        Some((lp.doc_key, live.into_iter().collect::<Vec<_>>()))
                    }
                })
            })
            .collect();
    }
    let booster: f64 = literals.iter().map(|l| l.booster.0).sum();
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

/// The parameters of one FTS query; the RA operator tier constructs this from
/// the resolved search atom.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FtsSearchParams {
    pub(crate) k: usize,
    pub(crate) score_kind: FtsScoreKind,
    /// Append the score as a trailing `Float` column (the RA tier maps it to
    /// a binding).
    pub(crate) bind_score: bool,
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn fts_search(
    cancel: &crate::fixed_rule::CancelFlag,
    tx: &impl ReadTx,
    query: &str,
    base: &RelationHandle,
    idx: &RelationHandle,
    params: &FtsSearchParams,
    filter_code: &Option<(Vec<Bytecode>, SourceSpan)>,
    stack: &mut Vec<DataValue>,
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
        Reverse(OrderedFloat(*sa))
            .cmp(&Reverse(OrderedFloat(*sb)))
            .then_with(|| ka.cmp(kb))
    });
    if filter_code.is_none() {
        result.truncate(params.k);
    }

    let mut ret = Vec::with_capacity(params.k);
    for (doc_key, score) in result {
        cancel.check()?;
        let mut cand = base.get(tx, &doc_key)?.ok_or_else(|| {
            miette!(IndexRowCorrupt::new(
                &idx.name,
                &doc_key,
                "FTS index references a base row that does not exist",
            ))
        })?;
        if params.bind_score {
            cand.push(DataValue::from(score));
        }
        if let Some((code, span)) = filter_code
            && !eval_bytecode_pred(code, &cand, stack, *span)?
        {
            continue;
        }
        ret.push(cand);
        if ret.len() >= params.k {
            break;
        }
    }
    Ok(ret)
}

// ---------------------------------------------------------------------------
// Tests: the engine's executable law.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use crate::data::program::InputRelationHandle;
    use crate::data::symb::Symbol;
    use crate::fixed_rule::CancelFlag;
    use crate::fts::TokenizerConfig;
    use crate::runtime::relation::{RelationHandle, create_relation};
    use crate::storage::Storage;
    use crate::storage::fjall::new_fjall_storage;

    fn col(name: &str, coltype: ColType) -> ColumnDef {
        ColumnDef {
            name: SmartString::from(name),
            typing: NullableColType {
                coltype,
                nullable: false,
            },
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
        TokenizerConfig {
            name: SmartString::from("Simple"),
            args: vec![],
        }
        .build(&[TokenizerConfig {
            name: SmartString::from("Lowercase"),
            args: vec![],
        }])
        .unwrap()
    }

    /// The compiled extractor: project the text column (position 1).
    fn extractor() -> Vec<Bytecode> {
        vec![Bytecode::Binding {
            var: Symbol::new("v", SourceSpan(0, 0)),
            tuple_pos: Some(1),
        }]
    }

    struct Fixture {
        base: RelationHandle,
        idx: RelationHandle,
        analyzer: TextAnalyzer,
        extractor: Vec<Bytecode>,
    }

    fn setup(db: &impl Storage, rows: &[(i64, &str)]) -> Fixture {
        let meta = base_meta();
        let analyzer = analyzer();
        let extractor = extractor();
        let mut tx = db.write_tx().unwrap();
        let base = create_relation(&mut tx, input_handle("docs", meta.clone())).unwrap();
        let idx =
            create_relation(&mut tx, input_handle("docs:fts", fts_index_metadata(&meta))).unwrap();
        let mut stack = vec![];
        for (k, text) in rows {
            let row = vec![DataValue::from(*k), DataValue::from(*text)];
            let key = base.encode_key_for_store(&row, SourceSpan(0, 0)).unwrap();
            let val = base.encode_val_for_store(&row, SourceSpan(0, 0)).unwrap();
            tx.put(&key, &val).unwrap();
            fts_put(
                &mut tx, &row, &extractor, &mut stack, &analyzer, &base, &idx,
            )
            .unwrap();
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
            bind_score: true,
        }
    }

    fn run(db: &impl Storage, f: &Fixture, q: &str, p: FtsSearchParams) -> Vec<(i64, f64)> {
        let rtx = db.read_tx().unwrap();
        let n = if p.score_kind == FtsScoreKind::TfIdf {
            fts_total_docs(&rtx, &f.base).unwrap()
        } else {
            0
        };
        let mut stack = vec![];
        let hits = fts_search(
            &CancelFlag::default(),
            &rtx,
            q,
            &f.base,
            &f.idx,
            &p,
            &None,
            &mut stack,
            &f.analyzer,
            n,
        )
        .unwrap();
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
        let mut stack = vec![];
        let row1 = vec![DataValue::from(1), DataValue::from("findme keep")];
        fts_del(
            &mut tx,
            &row1,
            &f.extractor,
            &mut stack,
            &f.analyzer,
            &f.base,
            &f.idx,
        )
        .unwrap();
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
        let base = create_relation(&mut tx, input_handle("docs", meta.clone())).unwrap();
        let idx =
            create_relation(&mut tx, input_handle("docs:fts", fts_index_metadata(&meta))).unwrap();
        let a = analyzer();
        // Extractor projects the INT key column (position 0): not a string.
        let bad_extractor = vec![Bytecode::Binding {
            var: Symbol::new("k", SourceSpan(0, 0)),
            tuple_pos: Some(0),
        }];
        let row = vec![DataValue::from(1), DataValue::from("text")];
        let mut stack = vec![];
        let err = fts_put(&mut tx, &row, &bad_extractor, &mut stack, &a, &base, &idx).unwrap_err();
        assert!(
            err.downcast_ref::<FtsExtractorType>().is_some(),
            "typed extractor error, got: {err:?}"
        );
    }

    #[test]
    fn corrupt_posting_is_typed_error_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let f = setup(&db, &[(1, "hello world")]);

        // Byte-flip every index row's value to reserved-msgpack garbage.
        let mut tx = db.write_tx().unwrap();
        let kvs: Vec<(Vec<u8>, Vec<u8>)> = {
            let lower = crate::data::tuple::encode_tuple_key(f.idx.id.0, &[]);
            let upper = (f.idx.id.0 + 1).to_be_bytes();
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
        let mut stack = vec![];
        let err = fts_search(
            &CancelFlag::default(),
            &rtx,
            "hello",
            &f.base,
            &f.idx,
            &params(1, FtsScoreKind::Tf),
            &None,
            &mut stack,
            &f.analyzer,
            0,
        )
        .expect_err("corrupt postings must error, not panic");
        assert!(
            format!("{err:?}").contains("corrupt"),
            "names corruption: {err:?}"
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
        let a = TokenizerConfig {
            name: SmartString::from("Simple"),
            args: vec![],
        }
        .build(&[
            TokenizerConfig {
                name: SmartString::from("Lowercase"),
                args: vec![],
            },
            TokenizerConfig {
                name: SmartString::from("Stopwords"),
                args: vec![DataValue::List(vec![
                    DataValue::from("the"),
                    DataValue::from("a"),
                ])],
            },
        ])
        .unwrap();
        let ex = extractor();
        let mut tx = db.write_tx().unwrap();
        let base = create_relation(&mut tx, input_handle("docs", meta.clone())).unwrap();
        let idx =
            create_relation(&mut tx, input_handle("docs:fts", fts_index_metadata(&meta))).unwrap();
        let mut stack = vec![];
        let row = vec![DataValue::from(1), DataValue::from("the cat sat")];
        let key = base.encode_key_for_store(&row, SourceSpan(0, 0)).unwrap();
        let val = base.encode_val_for_store(&row, SourceSpan(0, 0)).unwrap();
        tx.put(&key, &val).unwrap();
        fts_put(&mut tx, &row, &ex, &mut stack, &a, &base, &idx).unwrap();
        tx.commit().unwrap();

        let rtx = db.read_tx().unwrap();
        // "the" and "a" are stopwords: the query tokenizes to nothing.
        let hits = fts_search(
            &CancelFlag::default(),
            &rtx,
            "the AND a",
            &base,
            &idx,
            &params(10, FtsScoreKind::Tf),
            &None,
            &mut stack,
            &a,
            0,
        )
        .unwrap();
        assert!(
            hits.is_empty(),
            "stopword-only query yields no hits, no error"
        );
        // "cat" survives tokenization and matches.
        let hits = fts_search(
            &CancelFlag::default(),
            &rtx,
            "cat",
            &base,
            &idx,
            &params(10, FtsScoreKind::Tf),
            &None,
            &mut stack,
            &a,
            0,
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
    }
}
