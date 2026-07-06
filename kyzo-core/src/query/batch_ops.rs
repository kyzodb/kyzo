/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The batch-operator plumbing: the flattened [`Batch`] container, the
//! chunker that adapts any tuple stream onto the batched path, and the
//! two accumulate-then-refine filter sources (tuple-fed and raw-byte-fed)
//! that evaluate their residual predicates through the columnar
//! evaluator. The relational operators themselves live in `ra.rs`; what
//! lives here is the CURRENCY HANDLING every batched operator shares.

use fjall::Slice;
use miette::Result;

use crate::data::expr::Expr;
use crate::data::tuple::Tuple;
use crate::data::value::DataValue;

pub(crate) const BATCH_ROWS: usize = 1024;

/// A run of rows flowing through the vectorized (batched) execution path.
///
/// **Row-major, flattened.** Every row's values sit end to end in one
/// `values` buffer, with `offsets` marking row boundaries ([`Batch::row`],
/// [`Batch::push_with`]) — not whole [`Tuple`]s, and not per-column
/// `Vec<DataValue>` arrays. Row-major matches the substrate: the engine's
/// currency is the positional tuple, the predicate/unification VM
/// (`eval_bytecode`) reads one `&[DataValue]` row at a time, and the batched
/// scan (`BatchScanFilter`) decodes raw key/value bytes straight into the
/// flattened buffer, so a scanned row never exists as its own `Tuple`. A
/// columnar layout remains possible if a future profile justifies it; row-
/// major is what serves the VM and the scan as they exist today.
///
/// **Order is load-bearing.** A batch is an order-preserving window over the
/// operator's tuple stream: [`Batch::iter_rows`] yields rows in exactly the
/// order the iterator path would emit them. The determinism law (canonical
/// output order) rides on this — batching must never reorder observable
/// results.
pub(crate) struct Batch {
    /// Every row's values, flattened end to end: two allocations per BATCH
    /// instead of one `Vec` per row. The batched scan decodes raw key/value
    /// bytes straight into this buffer, so a scanned row never exists as
    /// its own `Tuple`.
    values: Vec<DataValue>,
    /// Row end offsets into `values`: row `i` is `offsets[i-1]..offsets[i]`
    /// (row 0 starts at 0).
    offsets: Vec<usize>,
}

impl Default for Batch {
    fn default() -> Self {
        Batch::new()
    }
}

impl Batch {
    pub(crate) fn new() -> Self {
        // Deliberately unallocated: the semi-naive loop mints thousands of
        // small delta batches per fixpoint, so an eager BATCH_ROWS-sized
        // reservation here is a measured 3x regression on recursive
        // workloads. Growth is amortized; full scan batches pay a few
        // doublings once.
        Batch {
            values: Vec::new(),
            offsets: Vec::new(),
        }
    }

    /// The slice of row `i` — the allocation-free view filters and
    /// projections evaluate against.
    pub(crate) fn row(&self, i: usize) -> &[DataValue] {
        let start = if i == 0 { 0 } else { self.offsets[i - 1] };
        &self.values[start..self.offsets[i]]
    }

    /// Iterate rows as slices, in stream order.
    pub(crate) fn iter_rows(&self) -> impl Iterator<Item = &[DataValue]> {
        (0..self.offsets.len()).map(|i| self.row(i))
    }

    /// Append a row by extending `values` in place through `fill`; the row
    /// is whatever `fill` pushed. The batched scan decodes key and value
    /// bytes straight through this — no per-row buffer exists at all.
    pub(crate) fn push_with(
        &mut self,
        fill: impl FnOnce(&mut Vec<DataValue>) -> Result<()>,
    ) -> Result<()> {
        fill(&mut self.values)?;
        self.offsets.push(self.values.len());
        Ok(())
    }

    /// Drop the most recently pushed row (a filtered-out decode).
    pub(crate) fn pop(&mut self) {
        if let Some(end) = self.offsets.pop() {
            let start = self.offsets.last().copied().unwrap_or(0);
            debug_assert!(end == self.values.len());
            self.values.truncate(start);
            let _ = end;
        }
    }
    pub(crate) fn with_rows(rows: Vec<Tuple>) -> Self {
        let mut b = Batch::new();
        for r in rows {
            b.push(r);
        }
        b
    }
    pub(crate) fn len(&self) -> usize {
        self.offsets.len()
    }
    pub(crate) fn is_full(&self) -> bool {
        self.len() >= BATCH_ROWS
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }
    pub(crate) fn push(&mut self, row: Tuple) {
        self.values.extend(row);
        self.offsets.push(self.values.len());
    }
    /// Consume the batch into owned rows, in stream order. Used only at
    /// the RA-internal seams where a batched operator feeds a row-oriented
    /// one (general join, unification); each call mints one `Tuple` per
    /// row. The eval boundary instead consumes rows as borrowed slices
    /// ([`Self::iter_rows`]) and mints only on admission.
    pub(crate) fn into_rows(self) -> Vec<Tuple> {
        let mut out = Vec::with_capacity(self.offsets.len());
        let mut values = self.values.into_iter();
        let mut start = 0usize;
        for end in self.offsets {
            out.push(values.by_ref().take(end - start).collect());
            start = end;
        }
        out
    }
}

/// A lazy stream of fallible [`Batch`]es: what the vectorized path yields.
pub(crate) type BatchIter<'a> = Box<dyn Iterator<Item = Result<Batch>> + 'a>;

/// The residual-filter conjunction of an RA node, as ONE expression: the
/// compiler split top-level `&&` into separate filters (`to_conjunction`);
/// rejoining them as a lazy And hands the whole list to the columnar
/// evaluator in one call, whose selection refinement IS the filters'
/// short-circuit and whose error minimum IS row-major error identity.
pub(crate) fn conjunction_pred(filters: &[Expr]) -> Option<Expr> {
    match filters {
        [] => None,
        [one] => Some(one.clone()),
        many => Some(Expr::build_and(many.to_vec(), many[0].span())),
    }
}

/// Refine an accumulated batch through the columnar evaluator: rows
/// surviving `pred` survive, in order; a poisoned live row raises exactly
/// the error row-at-a-time evaluation would raise first.
pub(crate) fn refine_batch(pred: &Option<Expr>, batch: Batch) -> Result<Batch> {
    let Some(pred) = pred else { return Ok(batch) };
    if batch.is_empty() {
        return Ok(batch);
    }
    let rows: Vec<&[DataValue]> = batch.iter_rows().collect();
    let width = rows[0].len();
    let columns = crate::data::batch::ColumnBatch::from_rows(&rows, width);
    let sel = crate::query::vm::eval_pred_batched(
        pred,
        &columns,
        &crate::data::batch::Selection::all(rows.len()),
    )?;
    let mut out = Batch::new();
    for r in sel.iter() {
        out.push_with(|buf| {
            buf.extend_from_slice(batch.row(r));
            Ok(())
        })?;
    }
    Ok(out)
}

/// The in-memory sibling of [`BatchScanFilter`]: temp-store rows arrive
/// already as owned [`Tuple`]s (they live in the epoch stores, not on
/// disk), so they are flattened into the batch and filtered in place
/// against one reused eval stack, with no per-row dynamic dispatch.
pub(crate) struct BatchTupleFilter<I> {
    pub(crate) inner: I,
    pub(crate) pred: Option<Expr>,
    /// A stream error met during accumulation. It must NOT surface before
    /// the rows accumulated ahead of it are refined: an earlier row's
    /// predicate error outranks it in row order (the row path interleaves
    /// decode and predicate per row, so it reports the FIRST failure in
    /// stream order — this slot is what keeps the batched path identical).
    pub(crate) pending_err: Option<miette::Report>,
}

impl<I: Iterator<Item = Result<Tuple>>> Iterator for BatchTupleFilter<I> {
    type Item = Result<Batch>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(e) = self.pending_err.take() {
                return Some(Err(e));
            }
            let mut batch = Batch::new();
            while batch.len() < BATCH_ROWS {
                match self.inner.next() {
                    None => break,
                    Some(Err(e)) => {
                        self.pending_err = Some(e);
                        break;
                    }
                    Some(Ok(t)) => batch.push(t),
                }
            }
            if batch.is_empty() {
                match self.pending_err.take() {
                    Some(e) => return Some(Err(e)),
                    None => return None,
                }
            }
            match refine_batch(&self.pred, batch) {
                // A predicate error in the accumulated prefix precedes the
                // pending stream error in row order.
                Err(e) => return Some(Err(e)),
                // Wholly rejected: surface the pending error or pull the
                // next chunk (an operator never yields an empty batch).
                Ok(b) if b.is_empty() => continue,
                Ok(b) => return Some(Ok(b)),
            }
        }
    }
}

/// The batched **scan** (+ leaf filter) source. Accumulates up to
/// `BATCH_ROWS` *surviving* rows from a raw store iterator, applying the
/// leaf's pushed-down predicates inline against **one reused eval stack**.
/// The iterator path pays a boxed `filter_map_ok` closure and a
/// `flatten_err` per row and re-borrows the stack through a captured
/// closure; here the predicate loop and the stack are a plain owned struct,
/// monomorphized per source type, with no per-row dynamic dispatch. Order
/// is the store iterator's order, unchanged — batching only regroups.
pub(crate) struct BatchScanFilter<I> {
    /// The RAW key/value byte stream: rows decode straight into the
    /// flattened batch, so no per-row `Tuple` is ever minted on this path.
    pub(crate) inner: I,
    pub(crate) pred: Option<Expr>,
    /// See [`BatchTupleFilter::pending_err`]: a stream or decode error
    /// must not outrank an earlier accumulated row's predicate error.
    pub(crate) pending_err: Option<miette::Report>,
}

impl<I: Iterator<Item = Result<(Slice, Slice)>>> Iterator for BatchScanFilter<I> {
    type Item = Result<Batch>;
    fn next(&mut self) -> Option<Self::Item> {
        use crate::data::tuple::{decode_key_into, extend_tuple_from_v};
        loop {
            if let Some(e) = self.pending_err.take() {
                return Some(Err(e));
            }
            let mut batch = Batch::new();
            while batch.len() < BATCH_ROWS {
                match self.inner.next() {
                    None => break,
                    Some(Err(e)) => {
                        self.pending_err = Some(e);
                        break;
                    }
                    Some(Ok((k, v))) => {
                        if let Err(e) = batch.push_with(|buf| {
                            decode_key_into(&k, buf)?;
                            extend_tuple_from_v(buf, &v)
                        }) {
                            // The decode failed mid-push: drop the torn row
                            // and hold the error behind the refined prefix.
                            batch.pop();
                            self.pending_err = Some(e);
                            break;
                        }
                    }
                }
            }
            if batch.is_empty() {
                match self.pending_err.take() {
                    Some(e) => return Some(Err(e)),
                    None => return None,
                }
            }
            match refine_batch(&self.pred, batch) {
                Err(e) => return Some(Err(e)),
                Ok(b) if b.is_empty() => continue,
                Ok(b) => return Some(Ok(b)),
            }
        }
    }
}
