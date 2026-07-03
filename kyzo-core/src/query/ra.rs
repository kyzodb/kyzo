/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0). The transformations, each per the ratified designs (story #3):
 *
 * - **Storage access through the kernel species.** The original's operators
 *   took `&SessionTx` (a RocksDB-backed session transaction). Here every
 *   stored-relation scan goes through the landed scan surface of
 *   `runtime/relation.rs` over `&impl ReadTx` — the storage contract's
 *   read species (`storage/mod.rs`). The operator tree itself is
 *   transaction-free data; the transaction is threaded through `iter` as a
 *   generic parameter and bound once, by the `RuleBody` implementation in
 *   `query/compile.rs`. A `WriteTx` is a `ReadTx`, so `runtime/db.rs` can
 *   thread either species when it lands (SEAM, db tier).
 * - **The Reorder/NegJoin join-RHS invariant is constructural.** The
 *   original `panic!`d ("joining on reordered" / "joining on NegJoin") at
 *   iteration time and `unreachable!()`d on an unexpected negation RHS.
 *   Here [`RelAlgebra::join`] refuses those right sides at plan
 *   construction with a typed error, and [`NegJoin`]'s right side is the
 *   narrower [`NegRight`] type — a negation against anything but a rule
 *   store or a stored-relation scan is *unrepresentable*, and
 *   [`RelAlgebra::neg_join`] is the total constructor that refuses the
 *   rest.
 * - **Negation over a time-travel scan is a typed refusal.** The original
 *   compiled `not *rel{..} @ t` into a `NegJoin` whose right side was
 *   `StoredWithValidity` — a shape its own iterator dispatched to
 *   `unreachable!()`, i.e. a user-reachable abort. Until the operator tier
 *   implements a skip-scan negation, the shape is refused at plan
 *   construction ([`NegationOverTimeTravelError`]) — loud, typed, and at
 *   compile time rather than mid-query.
 * - **"Every referenced rule has a store" is a typed invariant.** The
 *   original `unwrap`ped `stores.get(..)` at three scan sites; here the
 *   lookup is [`epoch_store_of`], returning [`PlanInvariantError`] — the
 *   mirror of `query/eval.rs`'s `store_of` (upstream panic sites 4–6 of
 *   that file's audit).
 * - **The index-search operators are seams.** `HnswSearch`, `FtsSearch`
 *   and `LshSearch` variants (and their `RelAlgebra` constructors) land
 *   with the index-operator tier, which owns the corresponding `MagicAtom`
 *   variants (see `data/program.rs`) and their manifests. Nothing here
 *   needs to change shape for them: each is one more enum variant with a
 *   parent, own bindings, and an `iter` that maps parent tuples to search
 *   results (SEAM, operator tier).
 *
 * Upstream panic-site audit (Law 5) for this file — every site outside
 * `#[cfg(test)]`, and what became of it:
 *   1. ra.rs:718   Reorder `.expect("program logic error: reorder indices
 *      mismatch")` — typed [`PlanInvariantError`].
 *   2. ra.rs:922   `lsh_search.filter.as_mut().unwrap()` — gone with the
 *      search seams (three sites, one per search operator).
 *   3. ra.rs:1036  FTS `coll.write_str(..).unwrap()` — gone (seam).
 *   4. ra.rs:1549,1573,1672  `stores.get(&self.storage_key).unwrap()` —
 *      typed via [`epoch_store_of`].
 *   5. ra.rs:1812f Joiner `left_binding_map.get(l).unwrap()` (and the right
 *      twin) — typed [`PlanInvariantError`]; the function already returned
 *      `Result`.
 *   6. ra.rs:1954,1968,2076,2090,2107,2141,... `join_indices(..).unwrap()`
 *      at every join dispatch — now `?` (the indices are minted from the
 *      same binding maps, but a bug is an error, never an abort).
 *   7. ra.rs:1976,2021  `unreachable!()` on a NegJoin right side — the
 *      [`NegRight`] type makes the state unrepresentable.
 *   8. ra.rs:2118,2121,2214,2217  `panic!("joining on reordered"/"joining
 *      on NegJoin")` — refused at construction by [`RelAlgebra::join`];
 *      the residual iteration arms are typed errors, not panics.
 *   9. ra.rs:446   `storage.metadata.keys.last().unwrap()` in the validity
 *      check of `RelAlgebra::relation` — a zero-key relation is an honest
 *      [`InvalidTimeTravelScanning`] refusal.
 *  10. ra.rs:266   Debug-impl `r.data.get(0).unwrap()` — `if let` fallback.
 *  11. Slice-index sites (`tuple[*i]`, `bindings[n..m]`): positions are
 *      minted at compile time from the same plan's binding maps
 *      (`fill_binding_indices_and_compile`), so they are compiled
 *      knowledge, not data — the same structural argument as
 *      `TupleInIter::get` in `runtime/temp_store.rs`. The two range-slices
 *      whose in-bounds proof crosses functions (`prefix_join`'s
 *      `other_bindings`) use `.get(..).unwrap_or(&[])` instead.
 *
 * Other deviations from the original, documented:
 *   D1. `TupleIter` is declared here, not in `data/tuple.rs`: a boxed
 *       stream of fallible tuples is the operator tier's own currency; the
 *       kernel's tuple module carries only value/encoding substance.
 *   D2. `utils::swap_option_result` / local `invert_option_err` are
 *       `Result::transpose` (the utils module was dissolved; see the
 *       reconciliation notes).
 *   D3. `log::debug!`/`log::error!` tracing is dropped; the workspace
 *       carries no `log` dependency. `Debug` formatting of plans (used by
 *       the `::explain` surface when db.rs lands) is preserved.
 *   D4. `either::{Left, Right}` is `itertools::Either` (no `either`
 *       dependency of our own).
 *   D5. `Joiner::as_map` (explain-output helper) is retained; it lands a
 *       consumer with db.rs's explain surface.
 */

//! Relational-algebra operators: the executable form of one rule body.
//!
//! **Essence**: an operator is a *tuple-stream transformer*. Each
//! [`RelAlgebra`] node consumes the (possibly empty) stream of binding
//! tuples produced by its parent and emits a transformed stream: a scan
//! emits stored or in-memory rows, a join extends each left tuple with
//! every matching right row, a negation join lets a tuple pass only when
//! no right row matches, a filter drops tuples, a unification appends a
//! computed column, a reorder permutes columns. A compiled rule body is one
//! left-deep tree of these nodes (built by
//! `query/compile.rs::compile_magic_rule_body`), and *evaluating a rule is
//! iterating the root* — [`RelAlgebra::iter`] returns a lazy [`TupleIter`]
//! that pulls through the whole tree.
//!
//! Positions, not names, at runtime: while the tree is being built each
//! node carries its output *bindings* (`Vec<Symbol>`, one per column); at
//! the end of compilation `fill_binding_indices_and_compile` resolves every
//! symbol reference inside filters and unification expressions to a tuple
//! position and compiles the expressions to [`Bytecode`]. Iteration never
//! looks at a name again.
//!
//! The delta discipline (the seam contract of `query/eval.rs::RuleBody`):
//! `iter` takes `delta_rule: Option<&MagicSymbol>`; when it names store
//! `k`, **every** [`TempStoreRA`] occurrence of `k` in the tree reads that
//! store's *delta* instead of its total, while negation
//! ([`NegJoin`]) always reads totals. Determinism: iteration order is a
//! function of the stores and the plan alone — the in-memory stores
//! iterate in canonical order, stored relations scan in memcmp key order,
//! and every operator here is order-preserving (the materialized join
//! sorts its cache).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Debug, Formatter};
use std::iter;

use itertools::Either::{Left, Right};
use itertools::Itertools;
use miette::{Diagnostic, Result, bail};
use thiserror::Error;

use crate::data::expr::{Bytecode, Expr, compute_bounds, eval_bytecode, eval_bytecode_pred};
use crate::data::program::MagicSymbol;
use crate::data::relation::{ColType, NullableColType};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::tuple::Tuple;
use crate::data::value::{DataValue, ValidityTs};
use crate::query::magic::InvalidTimeTravelScanning;
use crate::runtime::relation::RelationHandle;
use crate::runtime::temp_store::EpochStore;
use crate::storage::ReadTx;

/// A lazy stream of fallible tuples: what iterating an operator yields.
/// (The original homed this alias in `data/tuple.rs`; it is operator-tier
/// currency and lives here — deviation D1.)
pub(crate) type TupleIter<'a> = Box<dyn Iterator<Item = Result<Tuple>> + 'a>;

/// The target number of rows per [`Batch`] on the vectorized path. Chosen
/// large enough to amortize per-batch dynamic dispatch and buffer setup,
/// small enough that a batch stays L2-resident. A power of two so the
/// eventual columnar SIMD path can rely on it. See `vectorized-ra-design.md`.
pub(crate) const BATCH_ROWS: usize = 1024;

/// A run of rows flowing through the vectorized (batched) execution path.
///
/// **Row-major for the first camp.** The batch holds whole [`Tuple`]s rather
/// than per-column `Vec<DataValue>` arrays. This is a deliberate,
/// measured choice (see the design doc): the substrate is positional tuples,
/// the predicate/unification VM (`eval_bytecode`) reads a `&[DataValue]`
/// row, and the store scan already yields split key/value row views — a
/// row-batch amortizes the per-row costs a profile actually shows (boxed
/// iterator dispatch, allocation churn, bounds re-derivation) without
/// rewriting the value VM or the memcmp substrate. A columnar `Batch` is a
/// later camp, gated on the profile justifying it.
///
/// **Order is load-bearing.** A batch is an order-preserving window over the
/// operator's tuple stream: `rows` are in exactly the order the iterator
/// path would emit them. The determinism law (canonical output order) rides
/// on this — batching must never reorder observable results.
pub(crate) struct Batch {
    /// Every row's values, flattened end to end: two allocations per BATCH
    /// instead of one `Vec` per row. This is the scan-decode vectorization
    /// the first camp's profile named — the per-row `Tuple` mint at the
    /// leaves was the dominant fixpoint cost, and the batched scan now
    /// decodes straight into this buffer.
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
    fn row(&self, i: usize) -> &[DataValue] {
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
    pub(crate) fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }
    pub(crate) fn push(&mut self, row: Tuple) {
        self.values.extend(row);
        self.offsets.push(self.values.len());
    }
    /// Consume the batch into owned rows, in stream order. This is the seam
    /// where the batched path rejoins the row-oriented eval callback — the
    /// ONE place a per-row `Tuple` is minted, and only for admitted rows.
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

/// Adapts a [`TupleIter`] into a [`BatchIter`] by accumulating up to
/// `BATCH_ROWS` rows per batch. This is the *default* batched implementation
/// every operator inherits: it is trivially equivalent to the iterator path
/// (same rows, same order, only regrouped), so an operator that has not yet
/// grown a native `iter_batched` still participates correctly on the batched
/// path. Operators override `iter_batched` to avoid the tuple-at-a-time
/// round-trip where it pays.
struct BatchChunker<'a> {
    inner: TupleIter<'a>,
}

impl Iterator for BatchChunker<'_> {
    type Item = Result<Batch>;
    fn next(&mut self) -> Option<Self::Item> {
        let mut batch = Batch::new();
        for _ in 0..BATCH_ROWS {
            match self.inner.next() {
                Some(Ok(t)) => batch.push(t),
                Some(Err(e)) => return Some(Err(e)),
                None => break,
            }
        }
        if batch.is_empty() {
            None
        } else {
            Some(Ok(batch))
        }
    }
}

/// The batched **scan** (+ leaf filter) source. Accumulates up to
/// `BATCH_ROWS` *surviving* rows from a raw store iterator, applying the
/// leaf's pushed-down predicates inline against **one reused eval stack**.
///
/// This is where the first camp's amortization lives: the iterator path
/// pays a boxed `filter_map_ok` closure and a `flatten_err` per row and
/// re-borrows the stack through a captured closure; here the predicate loop
/// and the stack are a plain owned struct, monomorphized per source type,
/// with no per-row dynamic dispatch. Order is the store iterator's order,
/// unchanged — batching only regroups.
/// The in-memory sibling of [`BatchScanFilter`]: temp-store rows arrive as
/// owned tuples (they live in the epoch stores, not on disk), so they are
/// flattened into the batch and filtered in place.
struct BatchTupleFilter<I> {
    inner: I,
    filters: Vec<(Vec<Bytecode>, SourceSpan)>,
    stack: Vec<DataValue>,
}

impl<I: Iterator<Item = Result<Tuple>>> Iterator for BatchTupleFilter<I> {
    type Item = Result<Batch>;
    fn next(&mut self) -> Option<Self::Item> {
        let mut batch = Batch::new();
        while batch.len() < BATCH_ROWS {
            match self.inner.next() {
                None => break,
                Some(Err(e)) => return Some(Err(e)),
                Some(Ok(t)) => {
                    let mut keep = true;
                    for (p, span) in self.filters.iter() {
                        match eval_bytecode_pred(p, &t, &mut self.stack, *span) {
                            Ok(true) => {}
                            Ok(false) => {
                                keep = false;
                                break;
                            }
                            Err(e) => return Some(Err(e)),
                        }
                    }
                    if keep {
                        batch.push(t);
                    }
                }
            }
        }
        if batch.is_empty() {
            None
        } else {
            Some(Ok(batch))
        }
    }
}

struct BatchScanFilter<I> {
    /// The RAW key/value byte stream: rows decode straight into the
    /// flattened batch, so no per-row `Tuple` is ever minted on this path.
    inner: I,
    filters: Vec<(Vec<Bytecode>, SourceSpan)>,
    stack: Vec<DataValue>,
}

impl<I: Iterator<Item = Result<(Vec<u8>, Vec<u8>)>>> Iterator for BatchScanFilter<I> {
    type Item = Result<Batch>;
    fn next(&mut self) -> Option<Self::Item> {
        use crate::data::tuple::{decode_key_into, extend_tuple_from_v};
        let mut batch = Batch::new();
        while batch.len() < BATCH_ROWS {
            match self.inner.next() {
                None => break,
                Some(Err(e)) => return Some(Err(e)),
                Some(Ok((k, v))) => {
                    if let Err(e) = batch.push_with(|buf| {
                        decode_key_into(&k, buf)?;
                        extend_tuple_from_v(buf, &v)
                    }) {
                        return Some(Err(e));
                    }
                    let row = batch.row(batch.len() - 1);
                    let mut keep = true;
                    for (p, span) in self.filters.iter() {
                        match eval_bytecode_pred(p, row, &mut self.stack, *span) {
                            Ok(true) => {}
                            Ok(false) => {
                                keep = false;
                                break;
                            }
                            Err(e) => return Some(Err(e)),
                        }
                    }
                    if !keep {
                        batch.pop();
                    }
                }
            }
        }
        if batch.is_empty() {
            None
        } else {
            Some(Ok(batch))
        }
    }
}

/// The batched **filter** (+ column elimination) operator. Consumes its
/// parent's batch stream and, for each batch, applies the residual
/// predicates row by row over the contiguous buffer with one reused stack,
/// then drops eliminated columns. A batch that is wholly rejected is not
/// emitted (the loop pulls the next parent batch) — an operator never yields
/// an empty batch, so `is_empty()` on a received batch is unambiguous
/// end-of-window bookkeeping, never a real datum.
struct BatchFilter<'a> {
    parent: BatchIter<'a>,
    filters: &'a [(Vec<Bytecode>, SourceSpan)],
    eliminate_indices: BTreeSet<usize>,
    stack: Vec<DataValue>,
}

impl Iterator for BatchFilter<'_> {
    type Item = Result<Batch>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let batch = match self.parent.next()? {
                Ok(b) => b,
                Err(e) => return Some(Err(e)),
            };
            let mut out = Batch::new();
            for t in batch.into_rows() {
                let mut keep = true;
                for (p, span) in self.filters.iter() {
                    match eval_bytecode_pred(p, &t, &mut self.stack, *span) {
                        Ok(true) => {}
                        Ok(false) => {
                            keep = false;
                            break;
                        }
                        Err(e) => return Some(Err(e)),
                    }
                }
                if keep {
                    out.push(eliminate_from_tuple(t, &self.eliminate_indices));
                }
            }
            if !out.is_empty() {
                return Some(Ok(out));
            }
            // Whole batch rejected: pull the next parent batch.
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Typed invariants and refusals
// ─────────────────────────────────────────────────────────────────────────

/// A cross-stage invariant that plan construction should have made
/// impossible (e.g. "every referenced rule has a store", "reorder columns
/// are a permutation of the parent's"). Surfaced as an error, never an
/// abort — the mirror of `query/eval.rs`'s `EvalInvariantError`.
#[derive(Debug, Error, Diagnostic)]
#[error("query plan invariant violated: {0}")]
#[diagnostic(
    code(compile::plan_invariant),
    help("This is a bug. Please report it.")
)]
pub(crate) struct PlanInvariantError(pub(crate) &'static str);

/// Negating a time-travel scan (`not *rel{..} @ t`) is refused, typed, at
/// plan construction. The original compiled the shape and then aborted in
/// the negation join's dispatch (`unreachable!()`) — a user-reachable
/// panic. A skip-scan negation is implementable; it lands with the
/// operator tier if wanted (SEAM), and until then the refusal is loud.
#[derive(Debug, Error, Diagnostic)]
#[error("negation over a time-travel scan of stored relation '{0}' is not supported")]
#[diagnostic(
    code(compile::neg_time_travel),
    help("bind the historical rows in a positive rule first, then negate that rule")
)]
pub(crate) struct NegationOverTimeTravelError(pub(crate) String, #[label] pub(crate) SourceSpan);

/// A row decoded from a stored relation was shorter than a column the plan
/// needs to read from it. `decode_tuple_from_kv`'s arity is only a capacity
/// hint (`data/tuple.rs`) — the decoded length comes from the stored bytes,
/// so a truncated or corrupt stored value yields a short row. Reading past
/// its end is corruption, surfaced as a typed error rather than a slice
/// panic (the mirror of the bytecode filters' short-tuple error in
/// `data/expr.rs`).
#[derive(Debug, Error, Diagnostic)]
#[error("a stored row of relation '{0}' is too short: index is {1}, length is {2}")]
#[diagnostic(code(query::stored_row_too_short))]
#[diagnostic(help("The stored value is truncated or corrupt. Please report it."))]
pub(crate) struct StoredRowTooShortError(String, usize, usize, #[label] SourceSpan);

/// Typed lookup for "every referenced rule has a store" (upstream panic
/// sites: three `stores.get(..).unwrap()`s in this file).
fn epoch_store_of<'m>(
    stores: &'m BTreeMap<MagicSymbol, EpochStore>,
    key: &MagicSymbol,
) -> Result<&'m EpochStore> {
    stores
        .get(key)
        .ok_or_else(|| PlanInvariantError("a compiled scan references a rule with no store").into())
}

// ─────────────────────────────────────────────────────────────────────────
// The operator tree
// ─────────────────────────────────────────────────────────────────────────

/// One node of a compiled rule body. See the module docs for the essence;
/// see each payload type for its semantics.
///
/// SEAM (index-operator tier): the `HnswSearch`, `FtsSearch` and
/// `LshSearch` variants of the original land here together with their
/// `MagicAtom` variants and manifests — each is a parent plus its own
/// bindings, mapping every parent tuple to the search results seeded by
/// one bound column.
pub(crate) enum RelAlgebra {
    /// Inline rows (the unit relation, or literal data).
    Fixed(InlineFixedRA),
    /// Scan of an in-memory rule store (total or delta — the semi-naive
    /// discipline lives in this variant).
    TempStore(TempStoreRA),
    /// Scan of a stored relation at the current time.
    Stored(StoredRA),
    /// Time-travel scan of a stored relation: newest version at or before
    /// `valid_at`, asserted rows only.
    StoredWithValidity(StoredWithValidityRA),
    /// Inner join of two subtrees on named columns.
    Join(Box<InnerJoin>),
    /// Anti-join: left tuples with no matching right row. The right side
    /// is the narrower [`NegRight`] by construction.
    NegJoin(Box<NegJoin>),
    /// Column permutation (only ever the plan root, aligning to the rule
    /// head; [`RelAlgebra::join`] refuses it as a join RHS).
    Reorder(ReorderRA),
    /// Bytecode predicate filter.
    Filter(FilteredRA),
    /// Append one computed column (`binding = expr`), or one row per list
    /// element (`binding in expr`).
    Unification(UnificationRA),
    /// An index search (HNSW/FTS/LSH): for each parent row, evaluate the
    /// query expression, run the engine's pure search once, and append each
    /// result row (base row + the engine's extra columns).
    Search(Box<SearchRA>),
}

impl RelAlgebra {
    pub(crate) fn span(&self) -> SourceSpan {
        match self {
            RelAlgebra::Fixed(i) => i.span,
            RelAlgebra::TempStore(i) => i.span,
            RelAlgebra::Stored(i) => i.span,
            RelAlgebra::Join(i) => i.span,
            RelAlgebra::NegJoin(i) => i.span,
            RelAlgebra::Reorder(i) => i.relation.span(),
            RelAlgebra::Filter(i) => i.span,
            RelAlgebra::Unification(i) => i.span,
            RelAlgebra::StoredWithValidity(i) => i.span,
            RelAlgebra::Search(i) => i.atom.span,
        }
    }

    /// Resolve every symbol reference inside this tree's expressions to a
    /// tuple position and compile the expressions to bytecode. Called once,
    /// at the end of `compile_magic_rule_body`; iteration never resolves a
    /// name again.
    pub(crate) fn fill_binding_indices_and_compile(&mut self) -> Result<()> {
        match self {
            RelAlgebra::Fixed(_) => {}
            RelAlgebra::Search(s) => {
                s.fill_binding_indices_and_compile()?;
            }
            RelAlgebra::TempStore(d) => {
                d.fill_binding_indices_and_compile()?;
            }
            RelAlgebra::Stored(v) => {
                v.fill_binding_indices_and_compile()?;
            }
            RelAlgebra::StoredWithValidity(v) => {
                v.fill_binding_indices_and_compile()?;
            }
            RelAlgebra::Reorder(r) => {
                r.relation.fill_binding_indices_and_compile()?;
            }
            RelAlgebra::Filter(f) => {
                f.parent.fill_binding_indices_and_compile()?;
                f.fill_binding_indices_and_compile()?
            }
            RelAlgebra::NegJoin(r) => {
                // The negation's right side is a raw scan (its key columns
                // are matched positionally by the joiner) and never carries
                // filters; only the left subtree holds expressions.
                r.left.fill_binding_indices_and_compile()?;
            }
            RelAlgebra::Unification(u) => {
                u.parent.fill_binding_indices_and_compile()?;
                u.fill_binding_indices_and_compile()?
            }
            RelAlgebra::Join(r) => {
                r.left.fill_binding_indices_and_compile()?;
                r.right.fill_binding_indices_and_compile()?;
            }
        }
        Ok(())
    }

    /// The unit relation: one empty tuple. The seed of every rule body.
    pub(crate) fn unit(span: SourceSpan) -> Self {
        Self::Fixed(InlineFixedRA::unit(span))
    }

    pub(crate) fn is_unit(&self) -> bool {
        if let RelAlgebra::Fixed(r) = self {
            r.bindings.is_empty() && r.data.len() == 1
        } else {
            false
        }
    }

    pub(crate) fn cartesian_join(self, right: RelAlgebra, span: SourceSpan) -> Result<Self> {
        self.join(right, vec![], vec![], span)
    }

    /// A scan of an in-memory rule store (a "derived" relation).
    pub(crate) fn derived(
        bindings: Vec<Symbol>,
        storage_key: MagicSymbol,
        span: SourceSpan,
    ) -> Self {
        Self::TempStore(TempStoreRA {
            bindings,
            storage_key,
            filters: vec![],
            filters_bytecodes: vec![],
            span,
        })
    }

    /// A scan of a stored relation, optionally as-of `validity`. Time
    /// travel demands the relation's last key column be of type `Validity`
    /// — a zero-key relation or a wrong-typed column is a typed refusal
    /// (the original `unwrap`ped `keys.last()`).
    pub(crate) fn relation(
        bindings: Vec<Symbol>,
        storage: RelationHandle,
        span: SourceSpan,
        validity: Option<ValidityTs>,
    ) -> Result<Self> {
        match validity {
            None => Ok(Self::Stored(StoredRA {
                bindings,
                storage,
                filters: vec![],
                filters_bytecodes: vec![],
                span,
            })),
            Some(vld) => {
                let last_key_typing = storage.metadata.keys.last().map(|col| &col.typing);
                if last_key_typing
                    != Some(&NullableColType {
                        coltype: ColType::Validity,
                        nullable: false,
                    })
                {
                    bail!(InvalidTimeTravelScanning(storage.name.to_string(), span));
                };
                Ok(Self::StoredWithValidity(StoredWithValidityRA {
                    bindings,
                    storage,
                    filters: vec![],
                    filters_bytecodes: vec![],
                    valid_at: vld,
                    span,
                }))
            }
        }
    }

    pub(crate) fn reorder(self, new_order: Vec<Symbol>) -> Self {
        Self::Reorder(ReorderRA {
            relation: Box::new(self),
            new_order,
        })
    }

    pub(crate) fn filter(self, filter: Expr) -> Result<Self> {
        Ok(match self {
            s @ (RelAlgebra::Fixed(_)
            | RelAlgebra::Reorder(_)
            | RelAlgebra::NegJoin(_)
            | RelAlgebra::Unification(_)
            | RelAlgebra::Search(_)) => {
                let span = filter.span();
                RelAlgebra::Filter(FilteredRA {
                    parent: Box::new(s),
                    filters: vec![filter],
                    filters_bytecodes: vec![],
                    to_eliminate: Default::default(),
                    span,
                })
            }
            RelAlgebra::Filter(FilteredRA {
                parent,
                filters: mut pred,
                filters_bytecodes,
                to_eliminate,
                span,
            }) => {
                pred.push(filter);
                RelAlgebra::Filter(FilteredRA {
                    parent,
                    filters: pred,
                    filters_bytecodes,
                    to_eliminate,
                    span,
                })
            }
            RelAlgebra::TempStore(TempStoreRA {
                bindings,
                storage_key,
                mut filters,
                filters_bytecodes,
                span,
            }) => {
                filters.push(filter);
                RelAlgebra::TempStore(TempStoreRA {
                    bindings,
                    storage_key,
                    filters,
                    filters_bytecodes,
                    span,
                })
            }
            RelAlgebra::Stored(StoredRA {
                bindings,
                storage,
                mut filters,
                filters_bytecodes,
                span,
            }) => {
                filters.push(filter);
                RelAlgebra::Stored(StoredRA {
                    bindings,
                    storage,
                    filters,
                    filters_bytecodes,
                    span,
                })
            }
            RelAlgebra::StoredWithValidity(StoredWithValidityRA {
                bindings,
                storage,
                mut filters,
                filters_bytecodes,
                span,
                valid_at,
            }) => {
                filters.push(filter);
                RelAlgebra::StoredWithValidity(StoredWithValidityRA {
                    bindings,
                    storage,
                    filters,
                    span,
                    valid_at,
                    filters_bytecodes,
                })
            }
            RelAlgebra::Join(inner) => {
                // Push each conjunct of the filter as deep as its bindings
                // allow: onto the left subtree, the right subtree, or (for
                // conjuncts spanning both) a Filter above the join.
                let filters = filter.to_conjunction();
                let left_bindings: BTreeSet<Symbol> =
                    inner.left.bindings_before_eliminate().into_iter().collect();
                let right_bindings: BTreeSet<Symbol> = inner
                    .right
                    .bindings_before_eliminate()
                    .into_iter()
                    .collect();
                let mut remaining = vec![];
                let InnerJoin {
                    mut left,
                    mut right,
                    joiner,
                    to_eliminate,
                    span,
                } = *inner;
                for filter in filters {
                    let f_bindings = filter.bindings()?;
                    if f_bindings.is_subset(&left_bindings) {
                        left = left.filter(filter)?;
                    } else if f_bindings.is_subset(&right_bindings) {
                        right = right.filter(filter)?;
                    } else {
                        remaining.push(filter);
                    }
                }
                let mut joined = RelAlgebra::Join(Box::new(InnerJoin {
                    left,
                    right,
                    joiner,
                    to_eliminate,
                    span,
                }));
                if !remaining.is_empty() {
                    joined = RelAlgebra::Filter(FilteredRA {
                        parent: Box::new(joined),
                        filters: remaining,
                        filters_bytecodes: vec![],
                        to_eliminate: Default::default(),
                        span,
                    });
                }
                joined
            }
        })
    }

    pub(crate) fn unify(
        self,
        binding: Symbol,
        expr: Expr,
        is_multi: bool,
        span: SourceSpan,
    ) -> Self {
        RelAlgebra::Unification(UnificationRA {
            parent: Box::new(self),
            binding,
            expr,
            expr_bytecode: vec![],
            is_multi,
            to_eliminate: Default::default(),
            span,
        })
    }

    /// Inner-join constructor. Refuses a `Reorder` or `NegJoin` right side
    /// with a typed error at construction — the original accepted the
    /// shape and `panic!`d at iteration time. (A reorder is only ever the
    /// plan root; a negation join is a filter over its left side, not a
    /// row source — neither has join semantics as an RHS.)
    pub(crate) fn join(
        self,
        right: RelAlgebra,
        left_keys: Vec<Symbol>,
        right_keys: Vec<Symbol>,
        span: SourceSpan,
    ) -> Result<Self> {
        match &right {
            RelAlgebra::Reorder(_) => {
                bail!(PlanInvariantError(
                    "a Reorder cannot be the right side of a join"
                ))
            }
            RelAlgebra::NegJoin(_) => {
                bail!(PlanInvariantError(
                    "a NegJoin cannot be the right side of a join"
                ))
            }
            _ => {}
        }
        Ok(RelAlgebra::Join(Box::new(InnerJoin {
            left: self,
            right,
            joiner: Joiner {
                left_keys,
                right_keys,
            },
            to_eliminate: Default::default(),
            span,
        })))
    }

    /// Anti-join constructor: the total function from a general right side
    /// to the narrower [`NegRight`]. A rule-store or stored-relation scan
    /// is accepted; a time-travel scan is a typed *user-facing* refusal
    /// (see [`NegationOverTimeTravelError`]); anything else is a typed
    /// invariant error (the compiler never builds it).
    pub(crate) fn neg_join(
        self,
        right: RelAlgebra,
        left_keys: Vec<Symbol>,
        right_keys: Vec<Symbol>,
        span: SourceSpan,
    ) -> Result<Self> {
        let right = match right {
            RelAlgebra::TempStore(r) => NegRight::TempStore(r),
            RelAlgebra::Stored(r) => NegRight::Stored(r),
            RelAlgebra::StoredWithValidity(v) => {
                bail!(NegationOverTimeTravelError(
                    v.storage.name.to_string(),
                    span
                ))
            }
            _ => bail!(PlanInvariantError(
                "the right side of a negation must be a rule or stored-relation scan"
            )),
        };
        Ok(RelAlgebra::NegJoin(Box::new(NegJoin {
            left: self,
            right,
            joiner: Joiner {
                left_keys,
                right_keys,
            },
            to_eliminate: Default::default(),
            span,
        })))
    }

    /// Mark for elimination every column not in `used`, recursively; the
    /// tuples shed the columns during iteration (`eliminate_from_tuple`).
    pub(crate) fn eliminate_temp_vars(&mut self, used: &BTreeSet<Symbol>) -> Result<()> {
        match self {
            RelAlgebra::Fixed(r) => r.do_eliminate_temp_vars(used),
            RelAlgebra::TempStore(_r) => Ok(()),
            RelAlgebra::Stored(_v) => Ok(()),
            RelAlgebra::StoredWithValidity(_v) => Ok(()),
            RelAlgebra::Join(r) => r.do_eliminate_temp_vars(used),
            RelAlgebra::Reorder(r) => r.relation.eliminate_temp_vars(used),
            RelAlgebra::Filter(r) => r.do_eliminate_temp_vars(used),
            RelAlgebra::NegJoin(r) => r.do_eliminate_temp_vars(used),
            RelAlgebra::Unification(r) => r.do_eliminate_temp_vars(used),
            // Search bindings are terminal: elimination recurses to the
            // parent only.
            RelAlgebra::Search(r) => r.parent.eliminate_temp_vars(used),
        }
    }

    fn eliminate_set(&self) -> Option<&BTreeSet<Symbol>> {
        match self {
            RelAlgebra::Fixed(r) => Some(&r.to_eliminate),
            RelAlgebra::TempStore(_) => None,
            RelAlgebra::Stored(_) => None,
            RelAlgebra::StoredWithValidity(_) => None,
            RelAlgebra::Join(r) => Some(&r.to_eliminate),
            RelAlgebra::Reorder(_) => None,
            RelAlgebra::Filter(r) => Some(&r.to_eliminate),
            RelAlgebra::NegJoin(r) => Some(&r.to_eliminate),
            RelAlgebra::Unification(u) => Some(&u.to_eliminate),
            RelAlgebra::Search(_) => None,
        }
    }

    /// This node's output columns, elimination applied: the frame every
    /// consumer (parents, the head aligner in `compile_magic_rule_body`)
    /// indexes against.
    pub(crate) fn bindings_after_eliminate(&self) -> Vec<Symbol> {
        let ret = self.bindings_before_eliminate();
        if let Some(to_eliminate) = self.eliminate_set() {
            ret.into_iter()
                .filter(|kw| !to_eliminate.contains(kw))
                .collect()
        } else {
            ret
        }
    }

    fn bindings_before_eliminate(&self) -> Vec<Symbol> {
        match self {
            RelAlgebra::Fixed(f) => f.bindings.clone(),
            RelAlgebra::TempStore(d) => d.bindings.clone(),
            RelAlgebra::Stored(v) => v.bindings.clone(),
            RelAlgebra::StoredWithValidity(v) => v.bindings.clone(),
            RelAlgebra::Join(j) => j.bindings(),
            RelAlgebra::Reorder(r) => r.bindings(),
            RelAlgebra::Filter(r) => r.parent.bindings_after_eliminate(),
            RelAlgebra::NegJoin(j) => j.left.bindings_after_eliminate(),
            RelAlgebra::Unification(u) => {
                let mut bindings = u.parent.bindings_after_eliminate();
                bindings.push(u.binding.clone());
                bindings
            }
            RelAlgebra::Search(s) => {
                let mut bindings = s.parent.bindings_after_eliminate();
                bindings.extend(s.atom.own_bindings.iter().cloned());
                bindings
            }
        }
    }

    /// Iterate the whole tree lazily. `delta_rule` is the semi-naive delta
    /// context (see the module docs); `stores` is the epoch-store map of
    /// the running stratum.
    pub(crate) fn iter<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        delta_rule: Option<&MagicSymbol>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    ) -> Result<TupleIter<'a>> {
        match self {
            RelAlgebra::Fixed(f) => Ok(Box::new(f.data.iter().map(|t| Ok(t.clone())))),
            RelAlgebra::TempStore(r) => r.iter(delta_rule, stores),
            RelAlgebra::Stored(v) => v.iter(tx),
            RelAlgebra::StoredWithValidity(v) => v.iter(tx),
            RelAlgebra::Join(j) => j.iter(tx, delta_rule, stores),
            RelAlgebra::Reorder(r) => r.iter(tx, delta_rule, stores),
            RelAlgebra::Filter(r) => r.iter(tx, delta_rule, stores),
            RelAlgebra::NegJoin(r) => r.iter(tx, delta_rule, stores),
            RelAlgebra::Unification(r) => r.iter(tx, delta_rule, stores),
            RelAlgebra::Search(r) => r.iter(tx, delta_rule, stores),
        }
    }

    /// Iterate the whole tree lazily as a stream of [`Batch`]es — the
    /// vectorized execution path. Same three-argument seam as [`iter`], same
    /// observable stream (identical rows in identical order); the batch is
    /// an execution-internal grouping that never escapes to the semi-naive
    /// loop (`CompiledRuleBody::for_each_derivation` flattens it back).
    ///
    /// The default for every operator is [`BatchChunker`] over its `iter`
    /// stream — correct-but-unamortized. Operators that own a native batched
    /// implementation (the scan→filter→project pipeline of the first camp)
    /// override this to skip the tuple-at-a-time round trip. Batched and
    /// unbatched nodes compose freely because both currencies are just
    /// windows over the same ordered tuple stream.
    pub(crate) fn iter_batched<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        delta_rule: Option<&MagicSymbol>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    ) -> Result<BatchIter<'a>> {
        match self {
            RelAlgebra::Filter(r) => r.iter_batched(tx, delta_rule, stores),
            RelAlgebra::Reorder(r) => r.iter_batched(tx, delta_rule, stores),
            RelAlgebra::TempStore(r) => r.iter_batched(delta_rule, stores),
            RelAlgebra::Stored(v) => v.iter_batched(tx),
            // Join is batched only for the unit-left case (the scan seed);
            // a general join chunks the iterator join (later camp).
            RelAlgebra::Join(j) => j.iter_batched(tx, delta_rule, stores),
            // Every other operator inherits the correct default: chunk its
            // existing tuple stream. Negation, unification and the
            // fixed/time-travel scans stay tuple-at-a-time for now (later
            // camps); they still compose on the batched path via the chunker.
            other => Ok(Box::new(BatchChunker {
                inner: other.iter(tx, delta_rule, stores)?,
            })),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Shared plumbing
// ─────────────────────────────────────────────────────────────────────────

fn flatten_err<T, E1: Into<miette::Error>, E2: Into<miette::Error>>(
    v: std::result::Result<std::result::Result<T, E2>, E1>,
) -> Result<T> {
    match v {
        Err(e) => Err(e.into()),
        Ok(Err(e)) => Err(e.into()),
        Ok(Ok(v)) => Ok(v),
    }
}

fn filter_iter(
    filters_bytecodes: Vec<(Vec<Bytecode>, SourceSpan)>,
    it: impl Iterator<Item = Result<Tuple>>,
) -> impl Iterator<Item = Result<Tuple>> {
    let mut stack = vec![];
    it.filter_map_ok(move |t| -> Option<Result<Tuple>> {
        for (p, span) in filters_bytecodes.iter() {
            match eval_bytecode_pred(p, &t, &mut stack, *span) {
                Ok(false) => return None,
                Err(e) => return Some(Err(e)),
                Ok(true) => {}
            }
        }
        Some(Ok(t))
    })
    .map(flatten_err)
}

fn get_eliminate_indices(bindings: &[Symbol], eliminate: &BTreeSet<Symbol>) -> BTreeSet<usize> {
    bindings
        .iter()
        .enumerate()
        .filter_map(|(idx, kw)| {
            if eliminate.contains(kw) {
                Some(idx)
            } else {
                None
            }
        })
        .collect::<BTreeSet<_>>()
}

fn eliminate_from_tuple(mut ret: Tuple, eliminate_indices: &BTreeSet<usize>) -> Tuple {
    if !eliminate_indices.is_empty() {
        ret = ret
            .into_iter()
            .enumerate()
            .filter_map(|(i, v)| {
                if eliminate_indices.contains(&i) {
                    None
                } else {
                    Some(v)
                }
            })
            .collect_vec();
    }
    ret
}

/// Whether the right join columns are exactly a leading run of the right
/// relation's columns — the condition for a prefix scan instead of a
/// materialization. We do not consider a partial index match to be
/// "prefix", e.g. `[a, u => c]` with `a`, `c` bound and `u` unbound is not
/// "prefix", as it is not clear that prefix scanning in that case really
/// saves computation.
fn join_is_prefix(right_join_indices: &[usize]) -> bool {
    let mut indices = right_join_indices.to_vec();
    indices.sort();
    let l = indices.len();
    indices.into_iter().eq(0..l)
}

// ─────────────────────────────────────────────────────────────────────────
// InlineFixedRA: inline rows
// ─────────────────────────────────────────────────────────────────────────

/// Literal rows known at compile time. `unit` (no columns, one empty row)
/// seeds every rule body; data-bearing forms arrive with the constant-rule
/// wiring in db.rs.
#[derive(Debug)]
pub(crate) struct InlineFixedRA {
    pub(crate) bindings: Vec<Symbol>,
    pub(crate) data: Vec<Vec<DataValue>>,
    pub(crate) to_eliminate: BTreeSet<Symbol>,
    pub(crate) span: SourceSpan,
}

impl InlineFixedRA {
    pub(crate) fn unit(span: SourceSpan) -> Self {
        Self {
            bindings: vec![],
            data: vec![vec![]],
            to_eliminate: Default::default(),
            span,
        }
    }

    pub(crate) fn do_eliminate_temp_vars(&mut self, used: &BTreeSet<Symbol>) -> Result<()> {
        for binding in &self.bindings {
            if !used.contains(binding) {
                self.to_eliminate.insert(binding.clone());
            }
        }
        Ok(())
    }

    pub(crate) fn join_type(&self) -> &'static str {
        if self.data.is_empty() {
            "null_join"
        } else if self.data.len() == 1 {
            "singleton_join"
        } else {
            "fixed_join"
        }
    }

    pub(crate) fn join<'a>(
        &'a self,
        left_iter: TupleIter<'a>,
        (left_join_indices, right_join_indices): (Vec<usize>, Vec<usize>),
        eliminate_indices: BTreeSet<usize>,
    ) -> Result<TupleIter<'a>> {
        Ok(if self.data.is_empty() {
            Box::new(iter::empty())
        } else if self.data.len() == 1 {
            let data = self.data[0].clone();
            let right_join_values = right_join_indices
                .into_iter()
                .map(|v| data[v].clone())
                .collect_vec();
            Box::new(left_iter.filter_map_ok(move |tuple| {
                let left_join_values = left_join_indices.iter().map(|v| &tuple[*v]).collect_vec();
                if left_join_values.into_iter().eq(right_join_values.iter()) {
                    let mut ret = tuple;
                    ret.extend_from_slice(&data);
                    let ret = eliminate_from_tuple(ret, &eliminate_indices);
                    Some(ret)
                } else {
                    None
                }
            }))
        } else {
            let mut right_mapping = BTreeMap::new();
            for data in &self.data {
                let right_join_values = right_join_indices.iter().map(|v| &data[*v]).collect_vec();
                match right_mapping.get_mut(&right_join_values) {
                    None => {
                        right_mapping.insert(right_join_values, vec![data]);
                    }
                    Some(coll) => {
                        coll.push(data);
                    }
                }
            }
            Box::new(
                left_iter
                    .filter_map_ok(move |tuple| {
                        let left_join_values =
                            left_join_indices.iter().map(|v| &tuple[*v]).collect_vec();
                        right_mapping.get(&left_join_values).map(|v| {
                            v.iter()
                                .map(|right_values| {
                                    let mut left_data = tuple.clone();
                                    left_data.extend_from_slice(right_values);
                                    left_data
                                })
                                .collect_vec()
                        })
                    })
                    .flatten_ok(),
            )
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────
// TempStoreRA: in-memory rule store scans (the semi-naive seam)
// ─────────────────────────────────────────────────────────────────────────

/// A scan of one rule's [`EpochStore`]. This variant is where the
/// semi-naive delta discipline is *implemented*: when `delta_rule` names
/// this store, the scan reads the delta instead of the total — every
/// occurrence, per the `RuleBody` seam contract.
#[derive(Debug)]
pub(crate) struct TempStoreRA {
    pub(crate) bindings: Vec<Symbol>,
    pub(crate) storage_key: MagicSymbol,
    pub(crate) filters: Vec<Expr>,
    pub(crate) filters_bytecodes: Vec<(Vec<Bytecode>, SourceSpan)>,
    pub(crate) span: SourceSpan,
}

impl TempStoreRA {
    fn fill_binding_indices_and_compile(&mut self) -> Result<()> {
        let bindings: BTreeMap<_, _> = self
            .bindings
            .iter()
            .cloned()
            .enumerate()
            .map(|(a, b)| (b, a))
            .collect();
        for e in self.filters.iter_mut() {
            e.fill_binding_indices(&bindings)?;
            self.filters_bytecodes.push((e.compile()?, e.span()))
        }
        Ok(())
    }

    fn iter<'a>(
        &'a self,
        delta_rule: Option<&MagicSymbol>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    ) -> Result<TupleIter<'a>> {
        let storage = epoch_store_of(stores, &self.storage_key)?;

        let scan_epoch = match delta_rule {
            None => false,
            Some(name) => *name == self.storage_key,
        };
        let it = if scan_epoch {
            Left(storage.delta_all_iter().map(|t| Ok(t.into_tuple())))
        } else {
            Right(storage.all_iter().map(|t| Ok(t.into_tuple())))
        };
        Ok(if self.filters.is_empty() {
            Box::new(it)
        } else {
            Box::new(filter_iter(self.filters_bytecodes.clone(), it))
        })
    }

    /// Batched form of [`iter`](Self::iter): the same store scan (delta or
    /// total by the same `scan_epoch` test, same pushed-down filters, same
    /// order), accumulated into [`Batch`]es with a reused eval stack.
    fn iter_batched<'a>(
        &'a self,
        delta_rule: Option<&MagicSymbol>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    ) -> Result<BatchIter<'a>> {
        let storage = epoch_store_of(stores, &self.storage_key)?;
        let scan_epoch = match delta_rule {
            None => false,
            Some(name) => *name == self.storage_key,
        };
        let it = if scan_epoch {
            Left(storage.delta_all_iter().map(|t| Ok(t.into_tuple())))
        } else {
            Right(storage.all_iter().map(|t| Ok(t.into_tuple())))
        };
        Ok(Box::new(BatchTupleFilter {
            inner: it,
            filters: self.filters_bytecodes.clone(),
            stack: vec![],
        }))
    }

    /// Anti-join against this store. Always reads the TOTAL, never the
    /// delta — negation over a delta would resurrect rows already ruled
    /// out (the seam contract: "negated occurrences always read totals").
    fn neg_join<'a>(
        &'a self,
        left_iter: TupleIter<'a>,
        (left_join_indices, right_join_indices): (Vec<usize>, Vec<usize>),
        eliminate_indices: BTreeSet<usize>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    ) -> Result<TupleIter<'a>> {
        let storage = epoch_store_of(stores, &self.storage_key)?;
        debug_assert!(!right_join_indices.is_empty());
        let mut right_invert_indices = right_join_indices.iter().enumerate().collect_vec();
        right_invert_indices.sort_by_key(|(_, b)| **b);
        let mut left_to_prefix_indices = vec![];
        for (ord, (idx, ord_sorted)) in right_invert_indices.iter().enumerate() {
            if ord != **ord_sorted {
                break;
            }
            left_to_prefix_indices.push(left_join_indices[*idx]);
        }
        if join_is_prefix(&right_join_indices) {
            Ok(Box::new(
                left_iter
                    .map_ok(move |tuple| -> Result<Option<Tuple>> {
                        let prefix = left_to_prefix_indices
                            .iter()
                            .map(|i| tuple[*i].clone())
                            .collect_vec();

                        'outer: for found in storage.prefix_iter(&prefix) {
                            for (left_idx, right_idx) in
                                left_join_indices.iter().zip(right_join_indices.iter())
                            {
                                if tuple[*left_idx] != *found.get(*right_idx) {
                                    continue 'outer;
                                }
                            }
                            return Ok(None);
                        }

                        Ok(Some(eliminate_from_tuple(tuple, &eliminate_indices)))
                    })
                    .map(flatten_err)
                    .filter_map(Result::transpose),
            ))
        } else {
            let mut right_join_vals = BTreeSet::new();
            for tuple in storage.all_iter() {
                let to_join: Box<[DataValue]> = right_join_indices
                    .iter()
                    .map(|i| tuple.get(*i).clone())
                    .collect();
                right_join_vals.insert(to_join);
            }

            Ok(Box::new(
                left_iter
                    .map_ok(move |tuple| -> Result<Option<Tuple>> {
                        let left_join_vals: Box<[DataValue]> = left_join_indices
                            .iter()
                            .map(|i| tuple[*i].clone())
                            .collect();
                        if right_join_vals.contains(&left_join_vals) {
                            return Ok(None);
                        }
                        Ok(Some(eliminate_from_tuple(tuple, &eliminate_indices)))
                    })
                    .map(flatten_err)
                    .filter_map(Result::transpose),
            ))
        }
    }

    /// Prefix join: for each left tuple, prefix-scan this store on the
    /// join values. Reads the delta when `delta_rule` names this store.
    fn prefix_join<'a>(
        &'a self,
        left_iter: TupleIter<'a>,
        (left_join_indices, right_join_indices): (Vec<usize>, Vec<usize>),
        eliminate_indices: BTreeSet<usize>,
        delta_rule: Option<&MagicSymbol>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    ) -> Result<TupleIter<'a>> {
        let storage = epoch_store_of(stores, &self.storage_key)?;

        let mut right_invert_indices = right_join_indices.iter().enumerate().collect_vec();
        right_invert_indices.sort_by_key(|(_, b)| **b);
        let left_to_prefix_indices = right_invert_indices
            .into_iter()
            .map(|(a, _)| left_join_indices[a])
            .collect_vec();
        let scan_epoch = match delta_rule {
            None => false,
            Some(name) => *name == self.storage_key,
        };
        let mut skip_range_check = false;
        let it = left_iter
            .map_ok(move |tuple| {
                let prefix = left_to_prefix_indices
                    .iter()
                    .map(|i| tuple[*i].clone())
                    .collect_vec();
                let mut stack = vec![];

                if !skip_range_check && !self.filters.is_empty() {
                    let other_bindings =
                        self.bindings.get(right_join_indices.len()..).unwrap_or(&[]);
                    let (l_bound, u_bound) =
                        compute_bounds(&self.filters, other_bindings).unwrap_or_default();
                    if !l_bound.iter().all(|v| *v == DataValue::Null)
                        || !u_bound.iter().all(|v| *v == DataValue::Bot)
                    {
                        let mut lower_bound = prefix.clone();
                        lower_bound.extend(l_bound);
                        let mut upper_bound = prefix;
                        upper_bound.extend(u_bound);
                        let it = if scan_epoch {
                            Left(storage.delta_range_iter(&lower_bound, &upper_bound, true))
                        } else {
                            Right(storage.range_iter(&lower_bound, &upper_bound, true))
                        };
                        return Left(
                            it.map(move |res_found| -> Result<Option<Tuple>> {
                                let found = res_found.into_tuple();
                                for (p, span) in self.filters_bytecodes.iter() {
                                    if !eval_bytecode_pred(p, &found, &mut stack, *span)? {
                                        return Ok(None);
                                    }
                                }
                                let mut ret = tuple.clone();
                                ret.extend(found);
                                Ok(Some(ret))
                            })
                            .filter_map(Result::transpose),
                        );
                    }
                }
                skip_range_check = true;

                let it = if scan_epoch {
                    Left(storage.delta_prefix_iter(&prefix))
                } else {
                    Right(storage.prefix_iter(&prefix))
                };

                Right(
                    it.map(move |res_found| -> Result<Option<Tuple>> {
                        if self.filters.is_empty() {
                            let mut ret = tuple.clone();
                            ret.extend(res_found.into_iter().cloned());
                            Ok(Some(ret))
                        } else {
                            let found = res_found.into_tuple();
                            for (p, span) in self.filters_bytecodes.iter() {
                                if !eval_bytecode_pred(p, &found, &mut stack, *span)? {
                                    return Ok(None);
                                }
                            }
                            let mut ret = tuple.clone();
                            ret.extend(found);
                            Ok(Some(ret))
                        }
                    })
                    .filter_map(Result::transpose),
                )
            })
            .flatten_ok()
            .map(flatten_err);
        Ok(if eliminate_indices.is_empty() {
            Box::new(it)
        } else {
            Box::new(it.map_ok(move |t| eliminate_from_tuple(t, &eliminate_indices)))
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────
// StoredRA: stored-relation scans
// ─────────────────────────────────────────────────────────────────────────

/// A scan of a stored relation at the current time, through the landed
/// scan surface of `runtime/relation.rs` (every method takes the
/// transaction to read; see that module's routing note for temp
/// relations).
#[derive(Debug)]
pub(crate) struct StoredRA {
    pub(crate) bindings: Vec<Symbol>,
    pub(crate) storage: RelationHandle,
    pub(crate) filters: Vec<Expr>,
    pub(crate) filters_bytecodes: Vec<(Vec<Bytecode>, SourceSpan)>,
    pub(crate) span: SourceSpan,
}

impl StoredRA {
    fn fill_binding_indices_and_compile(&mut self) -> Result<()> {
        let bindings: BTreeMap<_, _> = self
            .bindings
            .iter()
            .cloned()
            .enumerate()
            .map(|(a, b)| (b, a))
            .collect();
        for e in self.filters.iter_mut() {
            e.fill_binding_indices(&bindings)?;
            self.filters_bytecodes.push((e.compile()?, e.span()));
        }
        Ok(())
    }

    /// Join by point lookup: the left tuples bind the relation's full key,
    /// so each left tuple costs one `get` instead of a scan.
    fn point_lookup_join<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        left_iter: TupleIter<'a>,
        key_len: usize,
        left_to_prefix_indices: Vec<usize>,
        (left_join_indices, right_join_indices): (Vec<usize>, Vec<usize>),
        eliminate_indices: BTreeSet<usize>,
    ) -> Result<TupleIter<'a>> {
        let mut stack = vec![];

        let it = left_iter
            .map_ok(move |tuple| -> Result<Option<Tuple>> {
                let prefix = left_to_prefix_indices
                    .iter()
                    .map(|i| tuple[*i].clone())
                    .collect_vec();
                let key = &prefix[0..key_len];
                match self.storage.get(tx, key)? {
                    None => Ok(None),
                    Some(found) => {
                        for (lk, rk) in left_join_indices.iter().zip(right_join_indices.iter()) {
                            let found_val = found.get(*rk).ok_or_else(|| {
                                StoredRowTooShortError(
                                    self.storage.name.to_string(),
                                    *rk,
                                    found.len(),
                                    self.span,
                                )
                            })?;
                            if tuple[*lk] != *found_val {
                                return Ok(None);
                            }
                        }
                        for (p, span) in self.filters_bytecodes.iter() {
                            if !eval_bytecode_pred(p, &found, &mut stack, *span)? {
                                return Ok(None);
                            }
                        }
                        let mut ret = tuple;
                        ret.extend(found);
                        Ok(Some(ret))
                    }
                }
            })
            // `map(flatten_err)`, not `flatten_ok`: the closure returns
            // `Result<Option<Tuple>>`, and `flatten_ok` would treat that
            // inner `Result` as an iterable and DROP its `Err` (silently
            // swallowing a `get`/predicate/short-row error). `flatten_err`
            // collapses the nested `Result`, preserving the error — the same
            // shape `neg_join` uses.
            .map(flatten_err)
            .filter_map(Result::transpose);
        Ok(if eliminate_indices.is_empty() {
            Box::new(it)
        } else {
            Box::new(it.map_ok(move |t| eliminate_from_tuple(t, &eliminate_indices)))
        })
    }

    fn prefix_join<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        left_iter: TupleIter<'a>,
        (left_join_indices, right_join_indices): (Vec<usize>, Vec<usize>),
        eliminate_indices: BTreeSet<usize>,
    ) -> Result<TupleIter<'a>> {
        let mut right_invert_indices = right_join_indices.iter().enumerate().collect_vec();
        right_invert_indices.sort_by_key(|(_, b)| **b);
        let left_to_prefix_indices = right_invert_indices
            .into_iter()
            .map(|(a, _)| left_join_indices[a])
            .collect_vec();

        let key_len = self.storage.metadata.keys.len();
        if left_to_prefix_indices.len() >= key_len {
            return self.point_lookup_join(
                tx,
                left_iter,
                key_len,
                left_to_prefix_indices,
                (left_join_indices, right_join_indices),
                eliminate_indices,
            );
        }

        let mut skip_range_check = false;
        let it = left_iter
            .map_ok(move |tuple| {
                let prefix = left_to_prefix_indices
                    .iter()
                    .map(|i| tuple[*i].clone())
                    .collect_vec();
                let mut stack = vec![];

                if !skip_range_check && !self.filters.is_empty() {
                    let other_bindings = self
                        .bindings
                        .get(right_join_indices.len()..self.storage.metadata.keys.len())
                        .unwrap_or(&[]);
                    let (l_bound, u_bound) =
                        compute_bounds(&self.filters, other_bindings).unwrap_or_default();
                    if !l_bound.iter().all(|v| *v == DataValue::Null)
                        || !u_bound.iter().all(|v| *v == DataValue::Bot)
                    {
                        return Left(
                            self.storage
                                .scan_bounded_prefix(tx, &prefix, &l_bound, &u_bound)
                                .map(move |res_found| -> Result<Option<Tuple>> {
                                    let found = res_found?;
                                    for (p, span) in self.filters_bytecodes.iter() {
                                        if !eval_bytecode_pred(p, &found, &mut stack, *span)? {
                                            return Ok(None);
                                        }
                                    }
                                    let mut ret = tuple.clone();
                                    ret.extend(found);
                                    Ok(Some(ret))
                                })
                                .filter_map(Result::transpose),
                        );
                    }
                }
                skip_range_check = true;
                Right(
                    self.storage
                        .scan_prefix(tx, &prefix)
                        .map(move |res_found| -> Result<Option<Tuple>> {
                            let found = res_found?;
                            for (p, span) in self.filters_bytecodes.iter() {
                                if !eval_bytecode_pred(p, &found, &mut stack, *span)? {
                                    return Ok(None);
                                }
                            }
                            let mut ret = tuple.clone();
                            ret.extend(found);
                            Ok(Some(ret))
                        })
                        .filter_map(Result::transpose),
                )
            })
            .flatten_ok()
            .map(flatten_err);
        Ok(if eliminate_indices.is_empty() {
            Box::new(it)
        } else {
            Box::new(it.map_ok(move |t| eliminate_from_tuple(t, &eliminate_indices)))
        })
    }

    fn neg_join<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        left_iter: TupleIter<'a>,
        (left_join_indices, right_join_indices): (Vec<usize>, Vec<usize>),
        eliminate_indices: BTreeSet<usize>,
    ) -> Result<TupleIter<'a>> {
        debug_assert!(!right_join_indices.is_empty());
        let mut right_invert_indices = right_join_indices.iter().enumerate().collect_vec();
        right_invert_indices.sort_by_key(|(_, b)| **b);
        let mut left_to_prefix_indices = vec![];
        for (ord, (idx, ord_sorted)) in right_invert_indices.iter().enumerate() {
            if ord != **ord_sorted {
                break;
            }
            left_to_prefix_indices.push(left_join_indices[*idx]);
        }

        if join_is_prefix(&right_join_indices) {
            Ok(Box::new(
                left_iter
                    .map_ok(move |tuple| -> Result<Option<Tuple>> {
                        let prefix = left_to_prefix_indices
                            .iter()
                            .map(|i| tuple[*i].clone())
                            .collect_vec();

                        'outer: for found in self.storage.scan_prefix(tx, &prefix) {
                            let found = found?;
                            for (left_idx, right_idx) in
                                left_join_indices.iter().zip(right_join_indices.iter())
                            {
                                let found_val = found.get(*right_idx).ok_or_else(|| {
                                    StoredRowTooShortError(
                                        self.storage.name.to_string(),
                                        *right_idx,
                                        found.len(),
                                        self.span,
                                    )
                                })?;
                                if tuple[*left_idx] != *found_val {
                                    continue 'outer;
                                }
                            }
                            return Ok(None);
                        }

                        Ok(Some(eliminate_from_tuple(tuple, &eliminate_indices)))
                    })
                    .map(flatten_err)
                    .filter_map(Result::transpose),
            ))
        } else {
            let mut right_join_vals = BTreeSet::new();

            for tuple in self.storage.scan_all(tx) {
                let tuple = tuple?;
                let to_join: Box<[DataValue]> = right_join_indices
                    .iter()
                    .map(|i| tuple[*i].clone())
                    .collect();
                right_join_vals.insert(to_join);
            }
            Ok(Box::new(
                left_iter
                    .map_ok(move |tuple| -> Result<Option<Tuple>> {
                        let left_join_vals: Box<[DataValue]> = left_join_indices
                            .iter()
                            .map(|i| tuple[*i].clone())
                            .collect();
                        if right_join_vals.contains(&left_join_vals) {
                            return Ok(None);
                        }

                        Ok(Some(eliminate_from_tuple(tuple, &eliminate_indices)))
                    })
                    .map(flatten_err)
                    .filter_map(Result::transpose),
            ))
        }
    }

    fn iter<'a>(&'a self, tx: &'a impl ReadTx) -> Result<TupleIter<'a>> {
        let it = self.storage.scan_all(tx);
        Ok(if self.filters.is_empty() {
            Box::new(it)
        } else {
            Box::new(filter_iter(self.filters_bytecodes.clone(), it))
        })
    }

    /// Batched form of [`iter`](Self::iter): the same on-disk range, same
    /// pushed-down filters, same memcmp order — but fed RAW bytes and
    /// decoded straight into the flattened batch, so the scan mints no
    /// per-row `Tuple` (the first camp's named target).
    fn iter_batched<'a>(&'a self, tx: &'a impl ReadTx) -> Result<BatchIter<'a>> {
        Ok(Box::new(BatchScanFilter {
            inner: self.storage.scan_all_raw(tx),
            filters: self.filters_bytecodes.clone(),
            stack: vec![],
        }))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// StoredWithValidityRA: time-travel scans
// ─────────────────────────────────────────────────────────────────────────

/// An as-of scan: among key-identical rows differing only in the trailing
/// validity column, only the newest version at or before `valid_at`, and
/// only if asserted (the storage contract's `range_skip_scan_tuple`).
/// Construction ([`RelAlgebra::relation`]) proved the last key column is
/// of type `Validity`.
#[derive(Debug)]
pub(crate) struct StoredWithValidityRA {
    pub(crate) bindings: Vec<Symbol>,
    pub(crate) storage: RelationHandle,
    pub(crate) filters: Vec<Expr>,
    pub(crate) filters_bytecodes: Vec<(Vec<Bytecode>, SourceSpan)>,
    pub(crate) valid_at: ValidityTs,
    pub(crate) span: SourceSpan,
}

impl StoredWithValidityRA {
    fn fill_binding_indices_and_compile(&mut self) -> Result<()> {
        let bindings: BTreeMap<_, _> = self
            .bindings
            .iter()
            .cloned()
            .enumerate()
            .map(|(a, b)| (b, a))
            .collect();
        for e in self.filters.iter_mut() {
            e.fill_binding_indices(&bindings)?;
            self.filters_bytecodes.push((e.compile()?, e.span()));
        }
        Ok(())
    }

    fn iter<'a>(&'a self, tx: &'a impl ReadTx) -> Result<TupleIter<'a>> {
        let it = self.storage.skip_scan_all(tx, self.valid_at);
        Ok(if self.filters.is_empty() {
            Box::new(it)
        } else {
            Box::new(filter_iter(self.filters_bytecodes.clone(), it))
        })
    }

    fn prefix_join<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        left_iter: TupleIter<'a>,
        (left_join_indices, right_join_indices): (Vec<usize>, Vec<usize>),
        eliminate_indices: BTreeSet<usize>,
    ) -> Result<TupleIter<'a>> {
        let mut right_invert_indices = right_join_indices.iter().enumerate().collect_vec();
        right_invert_indices.sort_by_key(|(_, b)| **b);
        let left_to_prefix_indices = right_invert_indices
            .into_iter()
            .map(|(a, _)| left_join_indices[a])
            .collect_vec();

        let mut skip_range_check = false;

        let it = left_iter
            .map_ok(move |tuple| {
                let prefix = left_to_prefix_indices
                    .iter()
                    .map(|i| tuple[*i].clone())
                    .collect_vec();

                if !skip_range_check && !self.filters.is_empty() {
                    let other_bindings = self
                        .bindings
                        .get(right_join_indices.len()..self.storage.metadata.keys.len())
                        .unwrap_or(&[]);
                    let (l_bound, u_bound) =
                        compute_bounds(&self.filters, other_bindings).unwrap_or_default();
                    if !l_bound.iter().all(|v| *v == DataValue::Null)
                        || !u_bound.iter().all(|v| *v == DataValue::Bot)
                    {
                        let mut stack = vec![];
                        return Left(
                            self.storage
                                .skip_scan_bounded_prefix(
                                    tx,
                                    &prefix,
                                    &l_bound,
                                    &u_bound,
                                    self.valid_at,
                                )
                                .map(move |res_found| -> Result<Option<Tuple>> {
                                    let found = res_found?;
                                    for (p, span) in self.filters_bytecodes.iter() {
                                        if !eval_bytecode_pred(p, &found, &mut stack, *span)? {
                                            return Ok(None);
                                        }
                                    }
                                    let mut ret = tuple.clone();
                                    ret.extend(found);
                                    Ok(Some(ret))
                                })
                                .filter_map(Result::transpose),
                        );
                    }
                }
                skip_range_check = true;
                let mut stack = vec![];
                Right(
                    self.storage
                        .skip_scan_prefix(tx, &prefix, self.valid_at)
                        .map(move |res_found| -> Result<Option<Tuple>> {
                            let found = res_found?;
                            for (p, span) in self.filters_bytecodes.iter() {
                                if !eval_bytecode_pred(p, &found, &mut stack, *span)? {
                                    return Ok(None);
                                }
                            }
                            let mut ret = tuple.clone();
                            ret.extend(found);
                            Ok(Some(ret))
                        })
                        .filter_map(Result::transpose),
                )
            })
            .flatten_ok()
            .map(flatten_err);
        Ok(if eliminate_indices.is_empty() {
            Box::new(it)
        } else {
            Box::new(it.map_ok(move |t| eliminate_from_tuple(t, &eliminate_indices)))
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────
// ReorderRA: column permutation
// ─────────────────────────────────────────────────────────────────────────

/// Permute the parent's columns into `new_order`. Only ever the plan root
/// (aligning body bindings to the rule head); never a join RHS, which
/// [`RelAlgebra::join`] enforces at construction.
#[derive(Debug)]
pub(crate) struct ReorderRA {
    pub(crate) relation: Box<RelAlgebra>,
    pub(crate) new_order: Vec<Symbol>,
}

impl ReorderRA {
    fn bindings(&self) -> Vec<Symbol> {
        self.new_order.clone()
    }

    fn iter<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        delta_rule: Option<&MagicSymbol>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    ) -> Result<TupleIter<'a>> {
        let old_order = self.relation.bindings_after_eliminate();
        let old_order_indices: BTreeMap<_, _> = old_order
            .into_iter()
            .enumerate()
            .map(|(k, v)| (v, k))
            .collect();
        let reorder_indices = self
            .new_order
            .iter()
            .map(|k| {
                old_order_indices
                    .get(k)
                    .copied()
                    // The original `expect`ed here ("program logic error:
                    // reorder indices mismatch"); a bug is an error.
                    .ok_or(PlanInvariantError(
                        "reorder columns are not a permutation of the parent's",
                    ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Box::new(
            self.relation
                .iter(tx, delta_rule, stores)?
                .map_ok(move |tuple| {
                    reorder_indices
                        .iter()
                        .map(|i| tuple[*i].clone())
                        .collect_vec()
                }),
        ))
    }

    /// Batched form of [`iter`](Self::iter): the same positional gather,
    /// applied batch by batch. Reorder is only ever the plan root, so this
    /// is the last transform before the eval callback.
    fn iter_batched<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        delta_rule: Option<&MagicSymbol>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    ) -> Result<BatchIter<'a>> {
        let old_order = self.relation.bindings_after_eliminate();
        let old_order_indices: BTreeMap<_, _> = old_order
            .into_iter()
            .enumerate()
            .map(|(k, v)| (v, k))
            .collect();
        let reorder_indices = self
            .new_order
            .iter()
            .map(|k| {
                old_order_indices.get(k).copied().ok_or(PlanInvariantError(
                    "reorder columns are not a permutation of the parent's",
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Box::new(
            self.relation.iter_batched(tx, delta_rule, stores)?.map(
                move |batch| -> Result<Batch> {
                    let rows = batch?
                        .into_rows()
                        .into_iter()
                        .map(|tuple| {
                            reorder_indices
                                .iter()
                                .map(|i| tuple[*i].clone())
                                .collect_vec()
                        })
                        .collect();
                    Ok(Batch::with_rows(rows))
                },
            ),
        ))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// FilteredRA: bytecode predicate filters
// ─────────────────────────────────────────────────────────────────────────

/// Keep only tuples satisfying every compiled predicate.
pub(crate) struct FilteredRA {
    pub(crate) parent: Box<RelAlgebra>,
    pub(crate) filters: Vec<Expr>,
    pub(crate) filters_bytecodes: Vec<(Vec<Bytecode>, SourceSpan)>,
    pub(crate) to_eliminate: BTreeSet<Symbol>,
    pub(crate) span: SourceSpan,
}

impl FilteredRA {
    pub(crate) fn do_eliminate_temp_vars(&mut self, used: &BTreeSet<Symbol>) -> Result<()> {
        for binding in self.parent.bindings_before_eliminate() {
            if !used.contains(&binding) {
                self.to_eliminate.insert(binding.clone());
            }
        }
        let mut nxt = used.clone();
        for e in self.filters.iter() {
            nxt.extend(e.bindings()?);
        }
        self.parent.eliminate_temp_vars(&nxt)?;
        Ok(())
    }

    fn fill_binding_indices_and_compile(&mut self) -> Result<()> {
        let parent_bindings: BTreeMap<_, _> = self
            .parent
            .bindings_after_eliminate()
            .into_iter()
            .enumerate()
            .map(|(a, b)| (b, a))
            .collect();
        for e in self.filters.iter_mut() {
            e.fill_binding_indices(&parent_bindings)?;
            self.filters_bytecodes.push((e.compile()?, e.span()));
        }
        Ok(())
    }

    fn iter<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        delta_rule: Option<&MagicSymbol>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    ) -> Result<TupleIter<'a>> {
        let bindings = self.parent.bindings_after_eliminate();
        let eliminate_indices = get_eliminate_indices(&bindings, &self.to_eliminate);
        let mut stack = vec![];
        Ok(Box::new(
            self.parent
                .iter(tx, delta_rule, stores)?
                .filter_map(move |tuple| match tuple {
                    Ok(t) => {
                        for (p, span) in self.filters_bytecodes.iter() {
                            match eval_bytecode_pred(p, &t, &mut stack, *span) {
                                Ok(false) => return None,
                                Err(e) => return Some(Err(e)),
                                Ok(true) => {}
                            }
                        }
                        let t = eliminate_from_tuple(t, &eliminate_indices);
                        Some(Ok(t))
                    }
                    Err(e) => Some(Err(e)),
                }),
        ))
    }

    /// Batched form of [`iter`](Self::iter): pull the parent's batch stream
    /// and filter each batch in place over a contiguous buffer with one
    /// reused stack, then drop eliminated columns. Same predicate order,
    /// same elimination, same row order as the iterator path.
    fn iter_batched<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        delta_rule: Option<&MagicSymbol>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    ) -> Result<BatchIter<'a>> {
        let bindings = self.parent.bindings_after_eliminate();
        let eliminate_indices = get_eliminate_indices(&bindings, &self.to_eliminate);
        Ok(Box::new(BatchFilter {
            parent: self.parent.iter_batched(tx, delta_rule, stores)?,
            filters: &self.filters_bytecodes,
            eliminate_indices,
            stack: vec![],
        }))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// UnificationRA: computed columns
// ─────────────────────────────────────────────────────────────────────────

/// Append one computed column per tuple (`binding = expr`), or — when
/// `is_multi` (`binding in expr`) — one output tuple per element of the
/// list `expr` evaluates to.
pub(crate) struct UnificationRA {
    pub(crate) parent: Box<RelAlgebra>,
    pub(crate) binding: Symbol,
    pub(crate) expr: Expr,
    pub(crate) expr_bytecode: Vec<Bytecode>,
    pub(crate) is_multi: bool,
    pub(crate) to_eliminate: BTreeSet<Symbol>,
    pub(crate) span: SourceSpan,
}

impl UnificationRA {
    fn fill_binding_indices_and_compile(&mut self) -> Result<()> {
        let parent_bindings: BTreeMap<_, _> = self
            .parent
            .bindings_after_eliminate()
            .into_iter()
            .enumerate()
            .map(|(a, b)| (b, a))
            .collect();
        self.expr.fill_binding_indices(&parent_bindings)?;
        self.expr_bytecode = self.expr.compile()?;
        Ok(())
    }

    pub(crate) fn do_eliminate_temp_vars(&mut self, used: &BTreeSet<Symbol>) -> Result<()> {
        for binding in self.parent.bindings_before_eliminate() {
            if !used.contains(&binding) {
                self.to_eliminate.insert(binding.clone());
            }
        }
        let mut nxt = used.clone();
        nxt.extend(self.expr.bindings()?);
        self.parent.eliminate_temp_vars(&nxt)?;
        Ok(())
    }

    fn iter<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        delta_rule: Option<&MagicSymbol>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    ) -> Result<TupleIter<'a>> {
        let mut bindings = self.parent.bindings_after_eliminate();
        bindings.push(self.binding.clone());
        let eliminate_indices = get_eliminate_indices(&bindings, &self.to_eliminate);
        let mut stack = vec![];
        Ok(if self.is_multi {
            let it = self
                .parent
                .iter(tx, delta_rule, stores)?
                .map_ok(move |tuple| -> Result<Vec<Tuple>> {
                    let result_list = eval_bytecode(&self.expr_bytecode, &tuple, &mut stack)?;
                    let result_list = result_list.get_slice().ok_or_else(|| {
                        #[derive(Debug, Error, Diagnostic)]
                        #[error("Invalid spread unification")]
                        #[diagnostic(code(eval::invalid_spread_unif))]
                        #[diagnostic(help("Spread unification requires a list at the right"))]
                        struct BadSpreadUnification(#[label] SourceSpan);

                        BadSpreadUnification(self.span)
                    })?;
                    let mut coll = vec![];
                    for result in result_list {
                        let mut ret = tuple.clone();
                        ret.push(result.clone());
                        let ret = eliminate_from_tuple(ret, &eliminate_indices);
                        coll.push(ret);
                    }
                    Ok(coll)
                })
                .map(flatten_err)
                .flatten_ok();
            Box::new(it)
        } else {
            Box::new(
                self.parent
                    .iter(tx, delta_rule, stores)?
                    .map_ok(move |tuple| -> Result<Tuple> {
                        let result = eval_bytecode(&self.expr_bytecode, &tuple, &mut stack)?;
                        let mut ret = tuple;
                        ret.push(result);
                        let ret = eliminate_from_tuple(ret, &eliminate_indices);
                        Ok(ret)
                    })
                    .map(flatten_err),
            )
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Joiner: named join columns → positional join indices
// ─────────────────────────────────────────────────────────────────────────

/// The named join columns of a join node.
///
/// Invariant (maintained by `compile_magic_rule_body`, which pushes to both
/// sides in lockstep): `left_keys` and `right_keys` have the same length.
pub(crate) struct Joiner {
    pub(crate) left_keys: Vec<Symbol>,
    pub(crate) right_keys: Vec<Symbol>,
}

impl Debug for Joiner {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let left_bindings = BindingFormatter(self.left_keys.clone());
        let right_bindings = BindingFormatter(self.right_keys.clone());
        write!(f, "{left_bindings:?}<->{right_bindings:?}")
    }
}

impl Joiner {
    /// The join columns as a left-name → right-name map (explain output;
    /// its consumer lands with db.rs — deviation D5).
    pub(crate) fn as_map(&self) -> BTreeMap<&str, &str> {
        self.left_keys
            .iter()
            .zip(self.right_keys.iter())
            .map(|(l, r)| (&l.name as &str, &r.name as &str))
            .collect()
    }

    /// Resolve the named join columns to positions in the given frames.
    /// A name missing from its frame is a typed invariant error (the
    /// original `unwrap`ped both lookups).
    pub(crate) fn join_indices(
        &self,
        left_bindings: &[Symbol],
        right_bindings: &[Symbol],
    ) -> Result<(Vec<usize>, Vec<usize>)> {
        let left_binding_map = left_bindings
            .iter()
            .enumerate()
            .map(|(k, v)| (v, k))
            .collect::<BTreeMap<_, _>>();
        let right_binding_map = right_bindings
            .iter()
            .enumerate()
            .map(|(k, v)| (v, k))
            .collect::<BTreeMap<_, _>>();
        let mut ret_l = Vec::with_capacity(self.left_keys.len());
        let mut ret_r = Vec::with_capacity(self.left_keys.len());
        for (l, r) in self.left_keys.iter().zip(self.right_keys.iter()) {
            let l_pos = left_binding_map.get(l).ok_or(PlanInvariantError(
                "a left join key is not in the left frame",
            ))?;
            let r_pos = right_binding_map.get(r).ok_or(PlanInvariantError(
                "a right join key is not in the right frame",
            ))?;
            ret_l.push(*l_pos);
            ret_r.push(*r_pos)
        }
        Ok((ret_l, ret_r))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// NegJoin: anti-join
// ─────────────────────────────────────────────────────────────────────────

/// The permitted right sides of a negation: a rule-store scan or a
/// stored-relation scan. Everything else is refused at construction by
/// [`RelAlgebra::neg_join`] — this type is the constructor proof that the
/// original's `unreachable!()` dispatch arms cannot be reached.
#[derive(Debug)]
pub(crate) enum NegRight {
    TempStore(TempStoreRA),
    Stored(StoredRA),
}

impl NegRight {
    fn bindings(&self) -> &[Symbol] {
        match self {
            NegRight::TempStore(r) => &r.bindings,
            NegRight::Stored(r) => &r.bindings,
        }
    }
}

/// Anti-join: a left tuple passes iff no right row matches it on the join
/// columns. Introduces no columns of its own — semantically a filter over
/// the left stream. Negation always reads right-side TOTALS, never deltas.
#[derive(Debug)]
pub(crate) struct NegJoin {
    pub(crate) left: RelAlgebra,
    pub(crate) right: NegRight,
    pub(crate) joiner: Joiner,
    pub(crate) to_eliminate: BTreeSet<Symbol>,
    pub(crate) span: SourceSpan,
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
    pub(crate) fn join_type(&self) -> Result<&'static str> {
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
        })
    }

    pub(crate) fn iter<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        delta_rule: Option<&MagicSymbol>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    ) -> Result<TupleIter<'a>> {
        let bindings = self.left.bindings_after_eliminate();
        let eliminate_indices = get_eliminate_indices(&bindings, &self.to_eliminate);
        let join_indices = self.joiner.join_indices(&bindings, self.right.bindings())?;
        match &self.right {
            NegRight::TempStore(r) => r.neg_join(
                self.left.iter(tx, delta_rule, stores)?,
                join_indices,
                eliminate_indices,
                stores,
            ),
            NegRight::Stored(v) => v.neg_join(
                tx,
                self.left.iter(tx, delta_rule, stores)?,
                join_indices,
                eliminate_indices,
            ),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// InnerJoin
// ─────────────────────────────────────────────────────────────────────────

/// Inner join: each left tuple extended with every matching right row.
/// Strategy is chosen at iteration time from the right side's shape: a
/// prefix scan when the join columns are a leading run of the right
/// relation's columns, a point lookup when they cover a stored relation's
/// whole key, and a sorted materialization otherwise.
pub(crate) struct InnerJoin {
    pub(crate) left: RelAlgebra,
    pub(crate) right: RelAlgebra,
    pub(crate) joiner: Joiner,
    pub(crate) to_eliminate: BTreeSet<Symbol>,
    pub(crate) span: SourceSpan,
}

impl InnerJoin {
    pub(crate) fn do_eliminate_temp_vars(&mut self, used: &BTreeSet<Symbol>) -> Result<()> {
        for binding in self.bindings() {
            if !used.contains(&binding) {
                self.to_eliminate.insert(binding.clone());
            }
        }
        let mut left = used.clone();
        left.extend(self.joiner.left_keys.clone());
        if let Some(filters) = match &self.right {
            RelAlgebra::TempStore(r) => Some(&r.filters),
            _ => None,
        } {
            for filter in filters {
                left.extend(filter.bindings()?);
            }
        }
        self.left.eliminate_temp_vars(&left)?;
        let mut right = used.clone();
        right.extend(self.joiner.right_keys.clone());
        self.right.eliminate_temp_vars(&right)?;
        Ok(())
    }

    pub(crate) fn bindings(&self) -> Vec<Symbol> {
        let mut ret = self.left.bindings_after_eliminate();
        ret.extend(self.right.bindings_after_eliminate());
        debug_assert_eq!(ret.len(), ret.iter().collect::<BTreeSet<_>>().len());
        ret
    }

    /// The join strategy this node will use (explain output).
    pub(crate) fn join_type(&self) -> Result<&'static str> {
        Ok(match &self.right {
            RelAlgebra::Fixed(f) => f.join_type(),
            RelAlgebra::TempStore(_) => {
                let join_indices = self.joiner.join_indices(
                    &self.left.bindings_after_eliminate(),
                    &self.right.bindings_after_eliminate(),
                )?;
                if join_is_prefix(&join_indices.1) {
                    "mem_prefix_join"
                } else {
                    "mem_mat_join"
                }
            }
            RelAlgebra::Stored(_) | RelAlgebra::StoredWithValidity(_) => {
                let join_indices = self.joiner.join_indices(
                    &self.left.bindings_after_eliminate(),
                    &self.right.bindings_after_eliminate(),
                )?;
                if join_is_prefix(&join_indices.1) {
                    "stored_prefix_join"
                } else {
                    "stored_mat_join"
                }
            }
            RelAlgebra::Join(_)
            | RelAlgebra::Filter(_)
            | RelAlgebra::Unification(_)
            | RelAlgebra::Search(_) => "generic_mat_join",
            // Refused at construction by `RelAlgebra::join` (the original
            // `panic!`d here).
            RelAlgebra::Reorder(_) | RelAlgebra::NegJoin(_) => {
                return Err(PlanInvariantError(
                    "join right side is a Reorder or NegJoin — refused at construction",
                )
                .into());
            }
        })
    }

    pub(crate) fn iter<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        delta_rule: Option<&MagicSymbol>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    ) -> Result<TupleIter<'a>> {
        let bindings = self.bindings();
        let eliminate_indices = get_eliminate_indices(&bindings, &self.to_eliminate);
        match &self.right {
            RelAlgebra::Fixed(f) => {
                let join_indices = self.joiner.join_indices(
                    &self.left.bindings_after_eliminate(),
                    &self.right.bindings_after_eliminate(),
                )?;
                f.join(
                    self.left.iter(tx, delta_rule, stores)?,
                    join_indices,
                    eliminate_indices,
                )
            }
            RelAlgebra::TempStore(r) => {
                let join_indices = self.joiner.join_indices(
                    &self.left.bindings_after_eliminate(),
                    &self.right.bindings_after_eliminate(),
                )?;
                if join_is_prefix(&join_indices.1) {
                    r.prefix_join(
                        self.left.iter(tx, delta_rule, stores)?,
                        join_indices,
                        eliminate_indices,
                        delta_rule,
                        stores,
                    )
                } else {
                    self.materialized_join(tx, eliminate_indices, delta_rule, stores)
                }
            }
            RelAlgebra::Stored(r) => {
                let join_indices = self.joiner.join_indices(
                    &self.left.bindings_after_eliminate(),
                    &self.right.bindings_after_eliminate(),
                )?;
                if join_is_prefix(&join_indices.1) {
                    r.prefix_join(
                        tx,
                        self.left.iter(tx, delta_rule, stores)?,
                        join_indices,
                        eliminate_indices,
                    )
                } else {
                    self.materialized_join(tx, eliminate_indices, delta_rule, stores)
                }
            }
            RelAlgebra::StoredWithValidity(r) => {
                let join_indices = self.joiner.join_indices(
                    &self.left.bindings_after_eliminate(),
                    &self.right.bindings_after_eliminate(),
                )?;
                if join_is_prefix(&join_indices.1) {
                    r.prefix_join(
                        tx,
                        self.left.iter(tx, delta_rule, stores)?,
                        join_indices,
                        eliminate_indices,
                    )
                } else {
                    self.materialized_join(tx, eliminate_indices, delta_rule, stores)
                }
            }
            RelAlgebra::Join(_)
            | RelAlgebra::Filter(_)
            | RelAlgebra::Unification(_)
            | RelAlgebra::Search(_) => {
                self.materialized_join(tx, eliminate_indices, delta_rule, stores)
            }
            // Refused at construction by `RelAlgebra::join` (the original
            // `panic!`d here).
            RelAlgebra::Reorder(_) | RelAlgebra::NegJoin(_) => Err(PlanInvariantError(
                "join right side is a Reorder or NegJoin — refused at construction",
            )
            .into()),
        }
    }

    /// Batched form of [`iter`](Self::iter), covering the first camp's one
    /// case: a **unit-left join**. Every rule body is seeded with the `unit`
    /// relation (one empty row, no columns) and atoms are folded on by
    /// joining, so a single-relation scan compiles to `Join(unit, scan)`.
    /// With an empty left the join has no keys and its output is exactly the
    /// right relation's rows (each extended by the empty tuple), minus any
    /// eliminated columns — identical rows in identical order to the
    /// iterator path's `prefix_join` over the single unit row. Delegating to
    /// `right.iter_batched` is what lets the scan→filter→project pipeline run
    /// fully batched (otherwise the scan sits under this join and the
    /// default chunker would re-run the iterator scan).
    ///
    /// A general (non-unit) join is a later camp: fall back to chunking the
    /// iterator join, which is correct but unamortized.
    pub(crate) fn iter_batched<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        delta_rule: Option<&MagicSymbol>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    ) -> Result<BatchIter<'a>> {
        if self.left.is_unit() {
            let bindings = self.bindings();
            let eliminate_indices = get_eliminate_indices(&bindings, &self.to_eliminate);
            let right = self.right.iter_batched(tx, delta_rule, stores)?;
            if eliminate_indices.is_empty() {
                return Ok(right);
            }
            return Ok(Box::new(right.map(move |b| -> Result<Batch> {
                let rows = b?
                    .into_rows()
                    .into_iter()
                    .map(|t| eliminate_from_tuple(t, &eliminate_indices))
                    .collect();
                Ok(Batch::with_rows(rows))
            })));
        }
        Ok(Box::new(BatchChunker {
            inner: self.iter(tx, delta_rule, stores)?,
        }))
    }

    /// Materialize the right side into a sorted cache keyed by the join
    /// columns, then merge against the left stream. `delta_rule` is passed
    /// through to BOTH sides — a delta store on the right is still a delta
    /// scan (the "every occurrence" clause of the seam contract).
    fn materialized_join<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        eliminate_indices: BTreeSet<usize>,
        delta_rule: Option<&MagicSymbol>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    ) -> Result<TupleIter<'a>> {
        let right_bindings = self.right.bindings_after_eliminate();
        let (left_join_indices, right_join_indices) = self
            .joiner
            .join_indices(&self.left.bindings_after_eliminate(), &right_bindings)?;

        let mut left_iter = self.left.iter(tx, delta_rule, stores)?;
        let left_cache = match left_iter.next() {
            None => return Ok(Box::new(iter::empty())),
            Some(Err(err)) => return Err(err),
            Some(Ok(data)) => data,
        };

        let right_join_indices_set = BTreeSet::from_iter(right_join_indices.iter().cloned());
        let mut right_store_indices = right_join_indices;
        for i in 0..right_bindings.len() {
            if !right_join_indices_set.contains(&i) {
                right_store_indices.push(i)
            }
        }

        let right_invert_indices = right_store_indices
            .iter()
            .enumerate()
            .sorted_by_key(|(_, b)| **b)
            .map(|(a, _)| a)
            .collect_vec();
        let cached_data = {
            let mut cache = BTreeSet::new();
            for item in self.right.iter(tx, delta_rule, stores)? {
                match item {
                    Ok(tuple) => {
                        let stored_tuple = right_store_indices
                            .iter()
                            .map(|i| tuple[*i].clone())
                            .collect_vec();
                        cache.insert(stored_tuple);
                    }
                    Err(e) => return Err(e),
                }
            }
            cache.into_iter().collect_vec()
        };

        let (prefix, right_idx) =
            build_mat_range_iter(&cached_data, &left_join_indices, &left_cache);

        let it = CachedMaterializedIterator {
            eliminate_indices,
            left: left_iter,
            left_cache,
            left_join_indices,
            materialized: cached_data,
            right_invert_indices,
            right_idx,
            prefix,
        };
        Ok(Box::new(it))
    }
}

struct CachedMaterializedIterator<'a> {
    materialized: Vec<Tuple>,
    eliminate_indices: BTreeSet<usize>,
    left_join_indices: Vec<usize>,
    right_invert_indices: Vec<usize>,
    right_idx: usize,
    prefix: Tuple,
    left: TupleIter<'a>,
    left_cache: Tuple,
}

impl CachedMaterializedIterator<'_> {
    fn advance_right(&mut self) -> Option<&Tuple> {
        if self.right_idx == self.materialized.len() {
            None
        } else {
            let ret = &self.materialized[self.right_idx];
            if ret.starts_with(&self.prefix) {
                self.right_idx += 1;
                Some(ret)
            } else {
                None
            }
        }
    }

    fn next_inner(&mut self) -> Result<Option<Tuple>> {
        loop {
            let right_nxt = self.advance_right();
            match right_nxt {
                Some(data) => {
                    let data = data.clone();
                    let mut ret = self.left_cache.clone();
                    for i in &self.right_invert_indices {
                        ret.push(data[*i].clone());
                    }
                    let tuple = eliminate_from_tuple(ret, &self.eliminate_indices);
                    return Ok(Some(tuple));
                }
                None => {
                    let next_left = self.left.next();
                    match next_left {
                        None => return Ok(None),
                        Some(l) => {
                            let left_tuple = l?;
                            let (prefix, idx) = build_mat_range_iter(
                                &self.materialized,
                                &self.left_join_indices,
                                &left_tuple,
                            );
                            self.left_cache = left_tuple;

                            self.right_idx = idx;
                            self.prefix = prefix;
                        }
                    }
                }
            }
        }
    }
}

fn build_mat_range_iter(
    mat: &[Tuple],
    left_join_indices: &[usize],
    left_tuple: &Tuple,
) -> (Tuple, usize) {
    let prefix = left_join_indices
        .iter()
        .map(|i| left_tuple[*i].clone())
        .collect_vec();
    let idx = match mat.binary_search(&prefix) {
        Ok(i) => i,
        Err(i) => i,
    };
    (prefix, idx)
}

impl Iterator for CachedMaterializedIterator<'_> {
    type Item = Result<Tuple>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_inner().transpose()
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Debug formatting (the `::explain` substrate)
// ─────────────────────────────────────────────────────────────────────────

struct BindingFormatter(Vec<Symbol>);

impl Debug for BindingFormatter {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let s = self.0.iter().map(|f| f.to_string()).join(", ");
        write!(f, "[{s}]")
    }
}

// ─────────────────────────────────────────────────────────────────────────
// SearchRA: index searches as joins
// ─────────────────────────────────────────────────────────────────────────

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
    pub(crate) query_bytecode: Vec<Bytecode>,
    pub(crate) filter_bytecode: Option<(Vec<Bytecode>, SourceSpan)>,
}

/// A search query expression evaluated to a value the engine cannot accept.
#[derive(Debug, Error, Diagnostic)]
#[error("the search query evaluated to {1}, which this index cannot search for")]
#[diagnostic(code(query::search_query_type))]
pub(crate) struct SearchQueryTypeError(#[label] pub(crate) SourceSpan, pub(crate) String);

impl SearchRA {
    fn fill_binding_indices_and_compile(&mut self) -> Result<()> {
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
        self.query_bytecode = self.atom.query.compile()?;
        // The filter sees the FULL output frame: parent ++ own_bindings.
        if let Some(filter) = self.atom.filter.as_mut() {
            let mut names = self.parent.bindings_after_eliminate();
            names.extend(self.atom.own_bindings.iter().cloned());
            let full_frame: BTreeMap<_, _> =
                names.into_iter().enumerate().map(|(i, b)| (b, i)).collect();
            filter.fill_binding_indices(&full_frame)?;
            self.filter_bytecode = Some((filter.compile()?, filter.span()));
        }
        Ok(())
    }

    fn iter<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        delta_rule: Option<&MagicSymbol>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    ) -> Result<TupleIter<'a>> {
        use crate::query::search::SearchConfig;
        let span = self.atom.span;
        let filter_code = self.filter_bytecode.clone();
        let query_code = self.query_bytecode.clone();
        let mut q_stack = vec![];
        let mut e_stack = vec![];

        // Per-search constants, hoisted out of the per-tuple closure.
        let fts_n_total = match &self.atom.cfg {
            SearchConfig::Fts(c)
                if c.params.score_kind == crate::runtime::fts_index::FtsScoreKind::TfIdf =>
            {
                crate::runtime::fts_index::fts_total_docs(tx, &c.base)?
            }
            _ => 0,
        };
        let cfg = &self.atom.cfg;

        let cancel = self.atom.cancel.clone();
        let it = self
            .parent
            .iter(tx, delta_rule, stores)?
            .map_ok(move |tuple| -> Result<_> {
                // Killable at both granularities (Q5): here per search
                // invocation, and inside each engine per scanned node.
                cancel.check()?;
                let q = eval_bytecode(&query_code, &tuple, &mut q_stack)?;
                let results = match cfg {
                    SearchConfig::Hnsw(c) => {
                        let v = match &q {
                            DataValue::Vec(v) => v,
                            other => bail!(SearchQueryTypeError(span, format!("{other:?}"))),
                        };
                        crate::runtime::hnsw::hnsw_knn(
                            tx,
                            v,
                            &c.manifest,
                            &c.base,
                            &c.idx,
                            &c.params,
                            &filter_code,
                            &mut e_stack,
                            &cancel,
                        )?
                    }
                    SearchConfig::Fts(c) => {
                        let text = match &q {
                            DataValue::Str(t) => t,
                            other => bail!(SearchQueryTypeError(span, format!("{other:?}"))),
                        };
                        crate::runtime::fts_index::fts_search(
                            &cancel,
                            tx,
                            text,
                            &c.base,
                            &c.idx,
                            &c.params,
                            &filter_code,
                            &mut e_stack,
                            &c.analyzer,
                            fts_n_total,
                        )?
                    }
                    SearchConfig::Lsh(c) => crate::runtime::minhash_lsh::lsh_search(
                        &cancel,
                        tx,
                        &q,
                        &c.manifest,
                        &c.base,
                        &c.idx,
                        &c.params,
                        &mut e_stack,
                        &filter_code,
                        &c.perms,
                        &c.analyzer,
                    )?,
                };
                Ok(results.into_iter().map(move |t| {
                    let mut r = tuple.clone();
                    r.extend(t);
                    r
                }))
            })
            .map(flatten_err)
            .flatten_ok();
        Ok(Box::new(it))
    }
}

impl Debug for RelAlgebra {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let bindings = BindingFormatter(self.bindings_after_eliminate());
        match self {
            RelAlgebra::Fixed(r) => {
                if r.bindings.is_empty() && r.data.len() == 1 {
                    f.write_str("Unit")
                } else if let [row] = &r.data[..] {
                    f.debug_tuple("Singlet")
                        .field(&bindings)
                        .field(row)
                        .finish()
                } else {
                    f.debug_tuple("Fixed")
                        .field(&bindings)
                        .field(&["..."])
                        .finish()
                }
            }
            RelAlgebra::Search(r) => f
                .debug_tuple("Search")
                .field(&bindings)
                .field(&r.atom.cfg)
                .field(&r.parent)
                .finish(),
            RelAlgebra::TempStore(r) => f
                .debug_tuple("TempStore")
                .field(&bindings)
                .field(&r.storage_key)
                .field(&r.filters)
                .finish(),
            RelAlgebra::Stored(r) => f
                .debug_tuple("Stored")
                .field(&bindings)
                .field(&r.storage.name)
                .field(&r.filters)
                .finish(),
            RelAlgebra::StoredWithValidity(r) => f
                .debug_tuple("StoredWithValidity")
                .field(&bindings)
                .field(&r.storage.name)
                .field(&r.filters)
                .field(&r.valid_at)
                .finish(),
            RelAlgebra::Join(r) => {
                if r.left.is_unit() {
                    r.right.fmt(f)
                } else {
                    f.debug_tuple("Join")
                        .field(&bindings)
                        .field(&r.joiner)
                        .field(&r.left)
                        .field(&r.right)
                        .finish()
                }
            }
            RelAlgebra::NegJoin(r) => f
                .debug_tuple("NegJoin")
                .field(&bindings)
                .field(&r.joiner)
                .field(&r.left)
                .field(&r.right)
                .finish(),
            RelAlgebra::Reorder(r) => f
                .debug_tuple("Reorder")
                .field(&r.new_order)
                .field(&r.relation)
                .finish(),
            RelAlgebra::Filter(r) => f
                .debug_tuple("Filter")
                .field(&bindings)
                .field(&r.filters)
                .field(&r.parent)
                .finish(),
            RelAlgebra::Unification(r) => f
                .debug_tuple("Unify")
                .field(&bindings)
                .field(&r.parent)
                .field(&r.binding)
                .field(&r.expr)
                .finish(),
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════
// Tests (new in KyzoDB; the original ra.rs had one script-level test,
// ported to `query/compile.rs` where the pipeline to drive it lives)
// ═════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use std::cmp::Reverse;

    use smartstring::SmartString;

    use super::*;
    use crate::data::program::InputRelationHandle;
    use crate::data::relation::{ColumnDef, StoredRelationMetadata};
    use crate::data::value::Validity;
    use crate::runtime::relation::create_relation;
    use crate::storage::fjall::new_fjall_storage;
    use crate::storage::{Storage, WriteTx};

    fn sp() -> SourceSpan {
        SourceSpan(0, 0)
    }
    fn sym(name: &str) -> Symbol {
        Symbol::new(name, sp())
    }
    fn v(i: i64) -> DataValue {
        DataValue::from(i)
    }
    fn no_stores() -> BTreeMap<MagicSymbol, EpochStore> {
        BTreeMap::new()
    }

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

    fn input_handle(
        name: &str,
        keys: Vec<ColumnDef>,
        non_keys: Vec<ColumnDef>,
    ) -> InputRelationHandle {
        let key_bindings = keys.iter().map(|c| sym(&c.name)).collect();
        let dep_bindings = non_keys.iter().map(|c| sym(&c.name)).collect();
        InputRelationHandle {
            name: sym(name),
            metadata: StoredRelationMetadata { keys, non_keys },
            key_bindings,
            dep_bindings,
            span: sp(),
        }
    }

    /// The three inline-join strategies of `InlineFixedRA` (null,
    /// singleton, fixed) against a two-row left stream.
    #[test]
    fn inline_fixed_join_branches() {
        let left_rows = [vec![v(1)], vec![v(2)]];
        let mk_left = || -> TupleIter<'_> { Box::new(left_rows.iter().map(|t| Ok(t.clone()))) };

        let null = InlineFixedRA {
            bindings: vec![sym("y")],
            data: vec![],
            to_eliminate: Default::default(),
            span: sp(),
        };
        assert_eq!(null.join_type(), "null_join");
        let got: Vec<Tuple> = null
            .join(mk_left(), (vec![0], vec![0]), Default::default())
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(got.is_empty());

        let singleton = InlineFixedRA {
            bindings: vec![sym("y")],
            data: vec![vec![v(2)]],
            to_eliminate: Default::default(),
            span: sp(),
        };
        assert_eq!(singleton.join_type(), "singleton_join");
        let got: Vec<Tuple> = singleton
            .join(mk_left(), (vec![0], vec![0]), Default::default())
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(got, vec![vec![v(2), v(2)]]);

        let fixed = InlineFixedRA {
            bindings: vec![sym("y"), sym("z")],
            data: vec![vec![v(1), v(10)], vec![v(1), v(11)], vec![v(3), v(12)]],
            to_eliminate: Default::default(),
            span: sp(),
        };
        assert_eq!(fixed.join_type(), "fixed_join");
        let got: Vec<Tuple> = fixed
            .join(mk_left(), (vec![0], vec![0]), Default::default())
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(got, vec![vec![v(1), v(1), v(10)], vec![v(1), v(1), v(11)]]);
    }

    /// `InnerJoin::iter_batched`'s unit-left fast path must NOT fire for a
    /// data-bearing singleton Fixed left: `is_unit` requires empty bindings,
    /// not just a single row. Today the compiler only ever seeds `unit`, so
    /// no compiled plan can reach this shape — but the constant-rule wiring
    /// (db.rs, later story) will mint data-bearing Fixed nodes, and the
    /// mutation campaign showed the bindings check is otherwise untested
    /// (mutant K4 survived every compiled-plan differential). This pins the
    /// guard at the RA level: Join(singleton Fixed, spread-unify) must be
    /// identical on both paths, i.e. a real join, not a right passthrough.
    #[test]
    fn batched_join_singleton_fixed_left_is_not_unit() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let rtx = db.read_tx().unwrap();

        let right = RelAlgebra::unit(sp()).unify(
            sym("x"),
            Expr::Const {
                val: DataValue::List(vec![v(1), v(2), v(3)]),
                span: sp(),
            },
            true,
            sp(),
        );
        let left = RelAlgebra::Fixed(InlineFixedRA {
            bindings: vec![sym("y")],
            data: vec![vec![v(2)]],
            to_eliminate: Default::default(),
            span: sp(),
        });
        assert!(!left.is_unit(), "a data-bearing singleton must not be unit");
        let mut ra = left
            .join(right, vec![sym("y")], vec![sym("x")], sp())
            .unwrap();
        ra.fill_binding_indices_and_compile().unwrap();
        let stores = no_stores();
        let it: Vec<Tuple> = ra
            .iter(&rtx, None, &stores)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let ba: Vec<Tuple> = ra
            .iter_batched(&rtx, None, &stores)
            .unwrap()
            .map(Result::unwrap)
            .flat_map(Batch::into_rows)
            .collect();
        assert_eq!(
            it, ba,
            "FINDING: batched unit fast path fired for a non-unit Fixed left"
        );
        assert_eq!(it, vec![vec![v(2), v(2)]]);
    }

    /// Spread unification (`binding in list`) emits one row per element;
    /// a non-list right side is a typed error, not a panic.
    #[test]
    fn spread_unification() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let rtx = db.read_tx().unwrap();

        let mut ra = RelAlgebra::unit(sp()).unify(
            sym("x"),
            Expr::Const {
                val: DataValue::List(vec![v(1), v(2), v(3)]),
                span: sp(),
            },
            true,
            sp(),
        );
        ra.fill_binding_indices_and_compile().unwrap();
        let stores = no_stores();
        let got: Vec<Tuple> = ra
            .iter(&rtx, None, &stores)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(got, vec![vec![v(1)], vec![v(2)], vec![v(3)]]);

        let mut bad = RelAlgebra::unit(sp()).unify(
            sym("x"),
            Expr::Const {
                val: v(7),
                span: sp(),
            },
            true,
            sp(),
        );
        bad.fill_binding_indices_and_compile().unwrap();
        let err = bad
            .iter(&rtx, None, &stores)
            .unwrap()
            .next()
            .unwrap()
            .unwrap_err();
        assert!(err.to_string().contains("Invalid spread unification"));
    }

    /// A time-travel scan through the operator: only the newest asserted
    /// version at or before `valid_at` per key.
    #[test]
    fn stored_with_validity_scan() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let mut tx = db.write_tx().unwrap();
        let handle = create_relation(
            &mut tx,
            input_handle(
                "hist",
                vec![col("k", ColType::Int), col("at", ColType::Validity)],
                vec![col("v", ColType::String)],
            ),
        )
        .unwrap();
        for (ts, val) in [(10i64, "ten"), (20, "twenty")] {
            let row = vec![
                v(1),
                DataValue::Validity(Validity::from((ts, true))),
                DataValue::from(val),
            ];
            let key = handle.encode_key_for_store(&row, sp()).unwrap();
            let val = handle.encode_val_for_store(&row, sp()).unwrap();
            tx.put(&key, &val).unwrap();
        }
        tx.commit().unwrap();

        let rtx = db.read_tx().unwrap();
        let stores = no_stores();
        let scan_at = |ts: i64| -> Vec<Tuple> {
            let ra = RelAlgebra::relation(
                vec![sym("k"), sym("at"), sym("v")],
                handle.clone(),
                sp(),
                Some(ValidityTs(Reverse(ts))),
            )
            .unwrap();
            ra.iter(&rtx, None, &stores)
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        let at_15 = scan_at(15);
        assert_eq!(at_15.len(), 1);
        assert_eq!(at_15[0][2], DataValue::from("ten"));
        let at_25 = scan_at(25);
        assert_eq!(at_25.len(), 1);
        assert_eq!(at_25[0][2], DataValue::from("twenty"));
        let at_5 = scan_at(5);
        assert!(at_5.is_empty());
    }

    /// Time travel on a relation whose last key column is not `Validity`
    /// (or that has no key columns) is a typed refusal.
    #[test]
    fn time_travel_needs_validity_key() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let mut tx = db.write_tx().unwrap();
        let plain = create_relation(
            &mut tx,
            input_handle("plain", vec![col("k", ColType::Int)], vec![]),
        )
        .unwrap();
        tx.commit().unwrap();

        let err = RelAlgebra::relation(vec![sym("k")], plain, sp(), Some(ValidityTs(Reverse(0))))
            .unwrap_err();
        assert!(
            err.downcast_ref::<InvalidTimeTravelScanning>().is_some(),
            "expected InvalidTimeTravelScanning, got {err:?}"
        );
    }

    /// The join-RHS refusals are typed and fire at CONSTRUCTION — the
    /// original `panic!`d/`unreachable!()`d at iteration time.
    #[test]
    fn join_rhs_refusals_are_typed() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let mut tx = db.write_tx().unwrap();
        let handle = create_relation(
            &mut tx,
            input_handle(
                "hist2",
                vec![col("k", ColType::Int), col("at", ColType::Validity)],
                vec![],
            ),
        )
        .unwrap();
        tx.commit().unwrap();

        // Reorder as inner-join RHS.
        let reordered = RelAlgebra::derived(vec![sym("x")], entry(), sp()).reorder(vec![sym("x")]);
        let err = RelAlgebra::unit(sp())
            .join(reordered, vec![], vec![], sp())
            .unwrap_err();
        assert!(err.downcast_ref::<PlanInvariantError>().is_some());

        // NegJoin as inner-join RHS.
        let neg = RelAlgebra::derived(vec![sym("x")], entry(), sp())
            .neg_join(
                RelAlgebra::derived(vec![sym("x")], entry(), sp()),
                vec![sym("x")],
                vec![sym("x")],
                sp(),
            )
            .unwrap();
        let err = RelAlgebra::unit(sp())
            .join(neg, vec![], vec![], sp())
            .unwrap_err();
        assert!(err.downcast_ref::<PlanInvariantError>().is_some());

        // A general operator as negation RHS is an invariant error…
        let err = RelAlgebra::unit(sp())
            .neg_join(RelAlgebra::unit(sp()), vec![], vec![], sp())
            .unwrap_err();
        assert!(err.downcast_ref::<PlanInvariantError>().is_some());

        // …and a time-travel scan as negation RHS is the USER-facing typed
        // refusal (upstream compiled it, then aborted mid-query).
        let vld_scan = RelAlgebra::relation(
            vec![sym("k"), sym("at")],
            handle,
            sp(),
            Some(ValidityTs(Reverse(0))),
        )
        .unwrap();
        let err = RelAlgebra::unit(sp())
            .neg_join(vld_scan, vec![], vec![], sp())
            .unwrap_err();
        assert!(
            err.downcast_ref::<NegationOverTimeTravelError>().is_some(),
            "expected NegationOverTimeTravelError, got {err:?}"
        );
    }

    fn entry() -> MagicSymbol {
        MagicSymbol::Muggle {
            inner: Symbol::prog_entry(sp()),
        }
    }
}
