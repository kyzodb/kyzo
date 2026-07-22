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

//! Anti-join: left rows with no matching right row.
// ─────────────────────────────────────────────────────────────────────────
// NegJoin: anti-join
// ─────────────────────────────────────────────────────────────────────────

use super::{
    DeltaRA, Joiner, RelAlgebra, SpansRA, StoredRA, StoredRowTooShortError, StoredWithValidityRA,
    TempStoreRA, epoch_store_of,
};
use crate::exec::fixpoint::delta_store::EpochStore;
use crate::exec::fixpoint::eval::AtomOccurrence;
use crate::exec::op::batch_ops::{Batch, BatchIter};
use crate::exec::op::join::{get_eliminate_indices, join_is_prefix};
use crate::exec::plan::program::MagicSymbol;
use crate::project::current::Segments;
use crate::session::catalog::RelationHandle;
use crate::store::ReadTx;
use itertools::Itertools;
use kyzo_model::SourceSpan;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::{AsOf, DataValue, MAX_VALIDITY_TS};
use miette::{Result, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;

/// The permitted right sides of a negation: a rule-store scan, a
/// stored-relation scan at the current state, or one of the three
/// time-travel shapes (as-of, `@spans`, `@delta`/`@delta_sys`) — story #86
/// closed the last of these against `NegationOverTimeTravelError`, which no
/// longer exists: every shape a negated relation atom can compile to now
/// has a serving [`NegJoin`] strategy. This type remains the constructor
/// proof that nothing outside the enum can reach the negation dispatch
/// below (the original's `unreachable!()` arms stay unreachable).
#[derive(Debug)]
pub enum NegRight {
    TempStore(Box<TempStoreRA>),
    Stored(Box<StoredRA>),
    StoredWithValidity(Box<StoredWithValidityRA>),
    Spans(Box<SpansRA>),
    Delta(Box<DeltaRA>),
}

impl NegRight {
    pub(crate) fn bindings(&self) -> &[Symbol] {
        match self {
            NegRight::TempStore(r) => &r.bindings,
            NegRight::Stored(r) => &r.bindings,
            NegRight::StoredWithValidity(r) => &r.bindings,
            NegRight::Spans(r) => &r.bindings,
            NegRight::Delta(r) => &r.bindings,
        }
    }
}

/// Anti-join: a left tuple passes iff no right row matches it on the join
/// columns. Introduces no columns of its own — semantically a filter over
/// the left stream. Negation always reads right-side TOTALS, never deltas.
#[derive(Debug)]
pub struct NegJoin {
    pub left: RelAlgebra,
    pub right: NegRight,
    pub(crate) joiner: Joiner,
    pub(crate) to_eliminate: BTreeSet<Symbol>,
    pub span: SourceSpan,
}

impl NegJoin {
    pub(crate) fn do_eliminate_temp_vars(&mut self, used: &BTreeSet<Symbol>) -> Result<()> {
        for binding in self.left.bindings_after_eliminate() {
            if !used.contains(&binding) {
                self.to_eliminate.insert(binding.clone());
            }
        }
        let mut left = used.clone();
        left.extend(self.joiner.left_keys.clone());
        self.left.eliminate_temp_vars(&left)?;
        // right acts as a filter, introduces nothing, no need to eliminate
        Ok(())
    }

    /// The join strategy this node will use (explain output).
    pub fn join_type(&self) -> Result<&'static str> {
        let join_indices = self
            .joiner
            .join_indices(&self.left.bindings_after_eliminate(), self.right.bindings())?;
        Ok(match &self.right {
            NegRight::TempStore(_) => {
                if join_is_prefix(&join_indices.1) {
                    "mem_neg_prefix_join"
                } else {
                    "mem_neg_mat_join"
                }
            }
            NegRight::Stored(_) => {
                if join_is_prefix(&join_indices.1) {
                    "stored_neg_prefix_join"
                } else {
                    "stored_neg_mat_join"
                }
            }
            NegRight::StoredWithValidity(_) => {
                if join_is_prefix(&join_indices.1) {
                    "asof_neg_prefix_join"
                } else {
                    "asof_neg_mat_join"
                }
            }
            // No prefix probe exists for a derived scan yet (pushdown into
            // `@spans`/`@delta` is chunk 4's posting-index work — the
            // module doc on `RelAlgebra::filter`'s matching arm says the
            // same): the anti-join always materializes the whole right
            // side.
            NegRight::Spans(_) => "spans_neg_mat_join",
            NegRight::Delta(_) => "delta_neg_mat_join",
        })
    }
}

impl NegJoin {
    /// One door for [`NegRight::Stored`] and [`NegRight::StoredWithValidity`]
    /// anti-join probes. Both read through the as-of skip-scan primitives
    /// (`skip_scan_prefix_projected` / `skip_scan_all`); current-state is
    /// `AsOf::current(MAX_VALIDITY_TS)` — the coordinate `scan_*` already
    /// uses for Facts. Arms differ only by validity coordinate.
    fn stored_asof_has_match<'a>(
        tx: &'a impl ReadTx,
        storage: &'a RelationHandle,
        span: SourceSpan,
        as_of: AsOf,
        right_join_indices: &[usize],
        left_join_indices: Vec<usize>,
        left_to_prefix_indices: Vec<usize>,
    ) -> Result<Box<dyn FnMut(&[DataValue]) -> Result<bool> + 'a>> {
        let name = storage.name.clone();
        if join_is_prefix(right_join_indices) {
            let lji = left_join_indices;
            let rji = right_join_indices.to_vec();
            Ok(Box::new(move |row: &[DataValue]| {
                'outer: for found in
                    storage.skip_scan_prefix_projected(tx, row, &left_to_prefix_indices, as_of)
                {
                    let found = found?;
                    for (l, r) in lji.iter().zip(rji.iter()) {
                        let found_val = found.get(*r).ok_or_else(|| {
                            StoredRowTooShortError(
                                Symbol::new(name.clone(), span),
                                *r,
                                found.len(),
                                span,
                            )
                        })?;
                        if row[*l] != *found_val {
                            continue 'outer;
                        }
                    }
                    return Ok(true);
                }
                Ok(false)
            }))
        } else {
            let mut right_join_vals = BTreeSet::new();
            for tuple in storage.skip_scan_all(tx, as_of) {
                let tuple = tuple?;
                let to_join: Box<[DataValue]> = right_join_indices
                    .iter()
                    .map(|i| tuple[*i].clone())
                    .collect();
                right_join_vals.insert(to_join);
            }
            let lji = left_join_indices;
            Ok(Box::new(move |row: &[DataValue]| {
                let left_join_vals: Box<[DataValue]> =
                    lji.iter().map(|i| row[*i].clone()).collect();
                Ok(right_join_vals.contains(&left_join_vals))
            }))
        }
    }

    /// The anti-join, batch-native: a filter over the left batch stream.
    /// Per left row (a slice into the batch — no `Tuple` minted) one probe
    /// per [`NegRight`] variant answers "does any right row match?": a
    /// prefix scan or a materialized join-column set, against a rule
    /// store, a current-state stored relation, an as-of skip-scan, or a
    /// materialized `@spans`/`@delta` derivation. Survivors are written
    /// once into the output batch, with this node's eliminations applied.
    /// Negation always reads right-side TOTALS, never deltas.
    pub(crate) fn iter_batched<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        delta_rule: Option<AtomOccurrence>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
        segments: Segments<'a>,
        want_premises: bool,
    ) -> Result<BatchIter<'a>> {
        let bindings = self.left.bindings_after_eliminate();
        let eliminate_indices = get_eliminate_indices(&bindings, &self.to_eliminate);
        let (left_join_indices, right_join_indices) =
            self.joiner.join_indices(&bindings, self.right.bindings())?;
        ensure!(!right_join_indices.is_empty(), "negation join requires at least one join key");

        let mut right_invert_indices = right_join_indices.iter().enumerate().collect_vec();
        right_invert_indices.sort_by_key(|(_, b)| **b);
        let mut left_to_prefix_indices = vec![];
        for (ord, (idx, ord_sorted)) in right_invert_indices.iter().enumerate() {
            if ord != **ord_sorted {
                break;
            }
            left_to_prefix_indices.push(left_join_indices[*idx]);
        }

        let has_match: Box<dyn FnMut(&[DataValue]) -> Result<bool> + 'a> = match &self.right {
            NegRight::TempStore(r) => {
                let storage = epoch_store_of(stores, &r.storage_key)?;
                if join_is_prefix(&right_join_indices) {
                    let lji = left_join_indices;
                    let rji = right_join_indices;
                    Box::new(move |row: &[DataValue]| {
                        'outer: for found in
                            storage.prefix_iter_projected(row, &left_to_prefix_indices, false)?
                        {
                            for (l, r) in lji.iter().zip(rji.iter()) {
                                if row[*l] != found.try_get(*r)? {
                                    continue 'outer;
                                }
                            }
                            return Ok(true);
                        }
                        Ok(false)
                    })
                } else {
                    let mut right_join_vals = BTreeSet::new();
                    for tuple in storage.all_iter()? {
                        let to_join: Box<[DataValue]> = right_join_indices
                            .iter()
                            .map(|i| tuple.try_get(*i))
                            .collect::<Result<Vec<_>, _>>()?
                            .into_boxed_slice();
                        right_join_vals.insert(to_join);
                    }
                    let lji = left_join_indices;
                    Box::new(move |row: &[DataValue]| {
                        let left_join_vals: Box<[DataValue]> =
                            lji.iter().map(|i| row[*i].clone()).collect();
                        Ok(right_join_vals.contains(&left_join_vals))
                    })
                }
            }
            NegRight::Stored(v) => Self::stored_asof_has_match(
                tx,
                &v.storage,
                v.span,
                AsOf::current(MAX_VALIDITY_TS),
                &right_join_indices,
                left_join_indices,
                left_to_prefix_indices,
            )?,
            // Story #86: same skip-scan primitives as the positive as-of
            // join (`StoredWithValidityRA::prefix_join_batched`); the
            // "never skips a tuple whose absence it is asserting" proof
            // is inherited — arms differ only by validity coordinate.
            NegRight::StoredWithValidity(v) => Self::stored_asof_has_match(
                tx,
                &v.storage,
                v.span,
                v.as_of,
                &right_join_indices,
                left_join_indices,
                left_to_prefix_indices,
            )?,
            // `@spans`/`@delta` right sides: no prefix-probe primitive
            // exists for either yet (same chunk-4 gap `RelAlgebra::filter`
            // already lives with), so the anti-join always materializes
            // the derived relation whole — one pass through the same
            // production sweep/set-difference the POSITIVE read uses
            // (`SpansRA`/`DeltaRA::iter_batched`), projected onto the join
            // columns into a set. Soundness is the positive scan's own:
            // this probe can only be wrong if the materialization missed a
            // row the positive read would have produced, and nothing here
            // reads differently from that shared `iter_batched`.
            NegRight::Spans(v) => {
                let mut right_join_vals = BTreeSet::new();
                for batch in v.iter_batched(tx)? {
                    let batch = batch?;
                    for row in batch.iter_rows() {
                        let to_join: Box<[DataValue]> =
                            right_join_indices.iter().map(|i| row[*i].clone()).collect();
                        right_join_vals.insert(to_join);
                    }
                }
                let lji = left_join_indices;
                Box::new(move |row: &[DataValue]| {
                    let left_join_vals: Box<[DataValue]> =
                        lji.iter().map(|i| row[*i].clone()).collect();
                    Ok(right_join_vals.contains(&left_join_vals))
                })
            }
            NegRight::Delta(v) => {
                let mut right_join_vals = BTreeSet::new();
                for batch in v.iter_batched(tx)? {
                    let batch = batch?;
                    for row in batch.iter_rows() {
                        let to_join: Box<[DataValue]> =
                            right_join_indices.iter().map(|i| row[*i].clone()).collect();
                        right_join_vals.insert(to_join);
                    }
                }
                let lji = left_join_indices;
                Box::new(move |row: &[DataValue]| {
                    let left_join_vals: Box<[DataValue]> =
                        lji.iter().map(|i| row[*i].clone()).collect();
                    Ok(right_join_vals.contains(&left_join_vals))
                })
            }
        };

        Ok(Box::new(NegBatchFilter {
            left: self
                .left
                .iter_batched(tx, delta_rule, stores, segments, want_premises)?,
            has_match,
            eliminate_indices,
            pending_err: None,
        }))
    }
}

/// The anti-join's batch executor: survivors accumulate densely across
/// input batches; a probe error is stashed so already-accepted rows emit
/// FIRST, in row order, before the error surfaces (the error-identity
/// discipline every batch node keeps). Negation contributes no premise —
/// left premises pass through unchanged.
struct NegBatchFilter<'a> {
    left: BatchIter<'a>,
    has_match: Box<dyn FnMut(&[DataValue]) -> Result<bool> + 'a>,
    eliminate_indices: BTreeSet<usize>,
    pending_err: Option<miette::Error>,
}

impl NegBatchFilter<'_> {
    fn next_batch(&mut self) -> Result<Option<Batch>> {
        if let Some(err) = self.pending_err.take() {
            return Err(err);
        }
        let mut out = Batch::new();
        while !out.is_full() {
            let Some(batch) = self.left.next() else { break };
            let batch = match batch {
                Ok(b) => b,
                Err(e) => {
                    if out.is_empty() {
                        return Err(e);
                    }
                    self.pending_err = Some(e);
                    return Ok(Some(out));
                }
            };
            let tracking = batch.premises().is_some();
            for (i, row) in batch.iter_rows().enumerate() {
                match (self.has_match)(row) {
                    Ok(true) => {}
                    Ok(false) => {
                        out.push_with(|buf| {
                            for (j, v) in row.iter().enumerate() {
                                if !self.eliminate_indices.contains(&j) {
                                    buf.push(v.clone());
                                }
                            }
                            Ok(())
                        })?;
                        if tracking {
                            out.push_premise_list(batch.row_premises(i));
                        }
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
        }
        Ok(if out.is_empty() { None } else { Some(out) })
    }
}

impl Iterator for NegBatchFilter<'_> {
    type Item = Result<Batch>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_batch().transpose()
    }
}
