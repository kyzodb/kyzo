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
use crate::exec::fixpoint::delta_store::EpochStore;
use crate::exec::fixpoint::eval::AtomOccurrence;
use crate::exec::op::batch_ops::{Batch, BatchIter};
use crate::exec::plan::program::MagicSymbol;
use crate::exec::plan::search::SearchConfig;
use crate::project::current::Segments;
use crate::store::ReadTx;
use kyzo_model::SourceSpan;
use kyzo_model::data_value_any;
use kyzo_model::value::{DataValue, SearchHits, Tag, Tuple};
use miette::{Diagnostic, Result, bail};
use std::collections::BTreeMap;
use thiserror::Error;

/// An index search driven once per parent row: the query expression is
/// evaluated against the parent tuple, the engine's pure search function
/// runs, and each result row (the full base row plus the engine's appended
/// columns, in the engine's fixed order — [`SearchAtom::own_bindings`]
/// names them) extends the parent row. "A vector search is a join."
///
/// [`SearchAtom::own_bindings`]: crate::exec::plan::search::SearchAtom
pub struct SearchRA {
    pub parent: Box<RelAlgebra>,
    pub(crate) atom: crate::exec::plan::search::SearchAtom,
}

#[derive(Debug, Error, Diagnostic)]
#[error("search query has wrong type for this index: expected {expected:?}, got {got:?}")]
#[diagnostic(code(query::search_query_type_error))]
pub(crate) struct SearchQueryTypeError {
    pub(crate) expected: Tag,
    pub(crate) got: Tag,
    #[label]
    pub(crate) span: SourceSpan,
}

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
        want_premises: bool,
    ) -> Result<BatchIter<'a>> {
        let span = self.atom.span;
        let fts_n_total = match &self.atom.cfg {
            SearchConfig::Fts(c)
                if c.params.score_kind == crate::project::text::fts::FtsScoreKind::TfIdf =>
            {
                crate::project::text::fts::fts_total_docs(tx, &c.base)?
            }
            SearchConfig::Hnsw(_) | SearchConfig::Fts(_) | SearchConfig::Lsh(_) => 0,
        };

        let filter_expr = self.atom.filter.clone();
        let query_expr = self.atom.query.clone();
        let cancel = self.atom.cancel.clone();
        let cfg = &self.atom.cfg;
        let base_arity = self.atom.cfg.base().arity();

        let search = move |row: &[DataValue]| -> Result<SearchHits> {
            cancel.check()?;
            let q = crate::exec::expr::eval_expr(&query_expr, row)?;
            match cfg {
                SearchConfig::Hnsw(c) => {
                    let v = match &q {
                        DataValue::Vector(v) => v,
                        other @ (data_value_any!()) => {
                            bail!(SearchQueryTypeError {
                                expected: Tag::Vector,
                                got: other.tag(),
                                span,
                            })
                        }
                    };
                    c.search_relation(tx, v, &filter_expr, &cancel)
                }
                SearchConfig::Fts(c) => {
                    let text = match &q {
                        DataValue::Str(t) => t.as_str(),
                        other @ (data_value_any!()) => {
                            bail!(SearchQueryTypeError {
                                expected: Tag::Str,
                                got: other.tag(),
                                span,
                            })
                        }
                    };
                    c.search_relation(tx, text, &filter_expr, &cancel, fts_n_total)
                }
                SearchConfig::Lsh(c) => c.search_relation(tx, &q, &filter_expr, &cancel),
            }
        };

        Ok(Box::new(SearchBatches {
            parent: self
                .parent
                .iter_batched(tx, delta_rule, stores, segments, want_premises)?,
            parent_batch: None,
            parent_row: 0,
            hits: SearchHits::empty(),
            hit_idx: 0,
            search: Box::new(search),
            pending_err: None,
            want_premises,
            base_arity,
        }))
    }
}

/// The search operator's batch executor.
struct SearchBatches<'a> {
    parent: BatchIter<'a>,
    parent_batch: Option<Batch>,
    parent_row: usize,
    hits: SearchHits,
    hit_idx: usize,
    search: Box<dyn FnMut(&[DataValue]) -> Result<SearchHits> + 'a>,
    pending_err: Option<miette::Error>,
    want_premises: bool,
    /// Leading columns of each hit that form the base-relation premise row.
    base_arity: usize,
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
                    return Err(crate::exec::op::PlanInvariantError(
                        "hits in flight imply a parent batch",
                    )
                    .into());
                };
                let row = match batch.row(self.parent_row) {
                    Ok(r) => r.to_vec(),
                    Err(e) => return Err(e.into()),
                };
                let left_premises = if self.want_premises {
                    batch.row_premises(self.parent_row)
                } else {
                    Vec::new()
                };
                while self.hit_idx < self.hits.len() {
                    let hit = self.hits.materialize_hit(self.hit_idx)?;
                    let right_premise = if self.want_premises {
                        Some(hit.iter().take(self.base_arity).cloned().collect::<Tuple>())
                    } else {
                        None
                    };
                    out.push_with(|buf| {
                        buf.extend_from_slice(&row);
                        buf.extend(hit);
                        Ok(())
                    })?;
                    if self.want_premises {
                        let mut premises = left_premises.clone();
                        if let Some(r) = right_premise {
                            premises.push(r);
                        }
                        out.push_premise_list(premises);
                    }
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
            let row = batch.row(self.parent_row)?;
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
