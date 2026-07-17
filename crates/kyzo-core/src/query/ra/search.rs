/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0); split out of query/ra.rs — see query/ra/mod.rs for the
 * transformation record.
 */

//! Index searches as relations: the engines' operators
//! (HNSW, FTS, LSH, spatial) joined into plans.
// ─────────────────────────────────────────────────────────────────────────
// SearchRA: index searches as joins
// ─────────────────────────────────────────────────────────────────────────

use super::RelAlgebra;
use crate::data::program::MagicSymbol;
use crate::data::span::SourceSpan;
use crate::data::value::DataValue;
use crate::engines::segments::Segments;
use crate::query::batch_ops::{Batch, BatchIter};
use crate::query::eval::AtomOccurrence;
use crate::query::levels::EpochStore;
use crate::storage::ReadTx;
use miette::{Diagnostic, Result, bail};
use std::collections::BTreeMap;
use std::fmt::Debug;
use thiserror::Error;
use crate::data::value::data_value_any;

/// Admitted search-hit rows as codes into this admission's private value
/// table. Engine decoded tuples never cross the SearchRA seam — they enter
/// only through [`QueryDomainAdmission::admit_hits`].
///
/// @authority QueryDomainAdmission
/// @layer query
/// @owns admission of engine search hits into the query execution currency as codes; decoded engine tuples never cross the SearchRA seam
/// @constructs QueryDomainAdmission::admit_hits
/// @forbids holding decoded engine Tuple vectors as the SearchRA hit currency
/// @gate SearchRA hit currency is QueryDomainAdmission codes only (#337)
/// @status established #337
pub(crate) struct QueryDomainAdmission {
    /// Row-major codes; each code indexes `table`.
    codes: Vec<u32>,
    /// End offset of each hit row in `codes`.
    row_ends: Vec<usize>,
    /// Intern table: code → value. Codes are local to this admission.
    table: Vec<DataValue>,
}

impl QueryDomainAdmission {
    /// No hits yet — the batch executor's idle state before the first search.
    pub(crate) fn empty() -> Self {
        QueryDomainAdmission {
            codes: Vec::new(),
            row_ends: Vec::new(),
            table: Vec::new(),
        }
    }

    /// THE DOOR: admit engine search results as codes under this admission.
    ///
    /// Consumes decoded engine hit rows at the boundary; SearchRA never
    /// stores them as a decoded-tuple vector.
    pub(crate) fn admit_hits(
        hits: impl IntoIterator<Item = impl IntoIterator<Item = DataValue>>,
    ) -> Self {
        let mut table = Vec::new();
        let mut codes = Vec::new();
        let mut row_ends = Vec::new();
        for hit in hits {
            for cell in hit {
                let code = match table.iter().position(|v| v == &cell) {
                    Some(i) => i as u32,
                    None => {
                        let i = table.len() as u32;
                        table.push(cell);
                        i
                    }
                };
                codes.push(code);
            }
            row_ends.push(codes.len());
        }
        QueryDomainAdmission {
            codes,
            row_ends,
            table,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.row_ends.len()
    }

    /// Resolve hit `i`'s codes through the admission table.
    pub(crate) fn hit_cells(&self, i: usize) -> impl Iterator<Item = &DataValue> + '_ {
        let start = if i == 0 { 0 } else { self.row_ends[i - 1] };
        let end = self.row_ends[i];
        self.codes[start..end]
            .iter()
            .map(|&c| &self.table[c as usize])
    }
}

/// An index search driven once per parent row: the query expression is
/// evaluated against the parent tuple, the engine's pure search function
/// runs, and each result row (the full base row plus the engine's appended
/// columns, in the engine's fixed order — [`SearchAtom::own_bindings`]
/// names them) extends the parent row. "A vector search is a join."
///
/// [`SearchAtom::own_bindings`]: crate::query::search::SearchAtom
pub(crate) struct SearchRA {
    pub(crate) parent: Box<RelAlgebra>,
    pub(crate) atom: crate::query::search::SearchAtom,
}

/// A search query expression evaluated to a value the engine cannot accept.
#[derive(Debug, Error, Diagnostic)]
#[error("the search query evaluated to {1}, which this index cannot search for")]
#[diagnostic(code(query::search_query_type))]
pub(crate) struct SearchQueryTypeError(#[label] pub(crate) SourceSpan, pub(crate) String);

impl SearchRA {
    pub(crate) fn fill_binding_indices_and_compile(&mut self) -> Result<()> {
        self.parent.fill_binding_indices_and_compile()?;
        // The query expression sees the PARENT frame.
        let parent_frame: BTreeMap<_, _> = self
            .parent
            .bindings_after_eliminate()
            .into_iter()
            .enumerate()
            .map(|(i, b)| (b, i))
            .collect();
        self.atom.query.fill_binding_indices(&parent_frame)?;
        // The filter sees the FULL output frame: parent ++ own_bindings.
        if let Some(filter) = self.atom.filter.as_mut() {
            let mut names = self.parent.bindings_after_eliminate();
            names.extend(self.atom.own_bindings.iter().cloned());
            let full_frame: BTreeMap<_, _> =
                names.into_iter().enumerate().map(|(i, b)| (b, i)).collect();
            filter.fill_binding_indices(&full_frame)?;
        }
        Ok(())
    }
}

impl SearchRA {
    /// The index search, batch-native: parent rows arrive as batch slices,
    /// each drives one engine invocation (HNSW / FTS / LSH / spatial —
    /// killable per invocation and per scanned node inside the engine),
    /// and every hit is written once into the output batch as
    /// `parent_row ++ match_columns`. A row whose hits overflow one output
    /// batch resumes exactly where it left off.
    pub(crate) fn iter_batched<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        delta_rule: Option<AtomOccurrence>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
        segments: Segments<'a>,
    ) -> Result<BatchIter<'a>> {
        use crate::query::search::SearchConfig;
        let span = self.atom.span;
        let fts_n_total = match &self.atom.cfg {
            SearchConfig::Fts(c)
                if c.params.score_kind == crate::engines::fts::FtsScoreKind::TfIdf =>
            {
                crate::engines::fts::fts_total_docs(tx, &c.base)?
            }
            SearchConfig::Hnsw(_) | SearchConfig::Fts(_) | SearchConfig::Lsh(_) => 0,
        };

        let filter_expr = self.atom.filter.clone();
        let query_expr = self.atom.query.clone();
        let cancel = self.atom.cancel.clone();
        let cfg = &self.atom.cfg;

        let search = move |row: &[DataValue]| -> Result<QueryDomainAdmission> {
            cancel.check()?;
            let q = query_expr.eval(row)?;
            let hits = match cfg {
                SearchConfig::Hnsw(c) => {
                    let v = match &q {
                        DataValue::Vector(v) => v,
                        other @ (data_value_any!()) => bail!(SearchQueryTypeError(span, format!("{other:?}"))),
                    };
                    crate::engines::hnsw::hnsw_knn(
                        tx,
                        v,
                        &c.manifest,
                        &c.base,
                        &c.idx,
                        &c.params,
                        &filter_expr,
                        &self.atom.cancel,
                    )?
                }
                SearchConfig::Fts(c) => {
                    let text = match &q {
                        DataValue::Str(t) => t,
                        other @ (data_value_any!()) => bail!(SearchQueryTypeError(span, format!("{other:?}"))),
                    };
                    crate::engines::fts::fts_search(
                        &self.atom.cancel,
                        tx,
                        text,
                        &c.base,
                        &c.idx,
                        &c.params,
                        &filter_expr,
                        &c.analyzer,
                        fts_n_total,
                    )?
                }
                SearchConfig::Lsh(c) => crate::engines::lsh::lsh_search(
                    &self.atom.cancel,
                    tx,
                    &q,
                    &c.manifest,
                    &c.base,
                    &c.idx,
                    &c.params,
                    &filter_expr,
                    &c.perms,
                    &c.analyzer,
                )?,
            };
            Ok(QueryDomainAdmission::admit_hits(
                hits.into_iter().map(|t| t.into_vec()),
            ))
        };

        Ok(Box::new(SearchBatches {
            parent: self.parent.iter_batched(tx, delta_rule, stores, segments)?,
            parent_batch: None,
            parent_row: 0,
            hits: QueryDomainAdmission::empty(),
            hit_idx: 0,
            search: Box::new(search),
            pending_err: None,
        }))
    }
}

/// The search operator's batch executor.
struct SearchBatches<'a> {
    parent: BatchIter<'a>,
    parent_batch: Option<Batch>,
    parent_row: usize,
    hits: QueryDomainAdmission,
    hit_idx: usize,
    search: Box<dyn FnMut(&[DataValue]) -> Result<QueryDomainAdmission> + 'a>,
    pending_err: Option<miette::Error>,
}

impl SearchBatches<'_> {
    fn next_batch(&mut self) -> Result<Option<Batch>> {
        if let Some(err) = self.pending_err.take() {
            return Err(err);
        }
        let mut out = Batch::new();
        loop {
            // Drain in-flight hits for the current parent row first.
            if self.hit_idx < self.hits.len() {
                let Some(batch) = &self.parent_batch else {
                    unreachable!("hits in flight imply a parent batch")
                };
                let row = match batch.row(self.parent_row) {
                    Ok(r) => r.to_vec(),
                    Err(e) => return Err(e.into()),
                };
                while self.hit_idx < self.hits.len() {
                    let hit: Vec<DataValue> =
                        self.hits.hit_cells(self.hit_idx).cloned().collect();
                    out.push_with(|buf| {
                        buf.extend_from_slice(&row);
                        buf.extend(hit.iter().cloned());
                        Ok(())
                    })?;
                    self.hit_idx += 1;
                    if out.is_full() {
                        return Ok(Some(out));
                    }
                }
            }
            // Advance to the next parent row.
            if let Some(batch) = &self.parent_batch {
                if self.parent_row + 1 < batch.len() {
                    self.parent_row += 1;
                } else {
                    self.parent_batch = None;
                }
            }
            if self.parent_batch.is_none() {
                match self.parent.next() {
                    None => break,
                    Some(Err(e)) => {
                        if out.is_empty() {
                            return Err(e);
                        }
                        self.pending_err = Some(e);
                        return Ok(Some(out));
                    }
                    Some(Ok(b)) => {
                        if b.is_empty() {
                            continue;
                        }
                        self.parent_batch = Some(b);
                        self.parent_row = 0;
                    }
                }
            }
            let Some(batch) = &self.parent_batch else {
                break;
            };
            let row = match batch.row(self.parent_row) {
                Ok(r) => r,
                Err(e) => return Err(e.into()),
            };
            match (self.search)(row) {
                Ok(hits) => {
                    self.hits = hits;
                    self.hit_idx = 0;
                }
                Err(e) => {
                    if out.is_empty() {
                        return Err(e);
                    }
                    self.pending_err = Some(e);
                    return Ok(Some(out));
                }
            }
        }
        Ok(if out.is_empty() { None } else { Some(out) })
    }
}

impl Iterator for SearchBatches<'_> {
    type Item = Result<Batch>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_batch().transpose()
    }
}
