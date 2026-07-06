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

//! Joins: the column joiner, prefix/point storage-probe joins
//! (batch-native), and the sorted-merge materialized fallback.
// ─────────────────────────────────────────────────────────────────────────
// Shared plumbing
// ─────────────────────────────────────────────────────────────────────────

use super::{BindingFormatter, PlanInvariantError, RelAlgebra, TupleIter};
use crate::data::expr::{Bytecode, eval_bytecode_pred};
use crate::data::program::MagicSymbol;
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::tuple::Tuple;
use crate::data::value::DataValue;
use crate::engines::segments::Segments;
use crate::query::batch_ops::{BATCH_ROWS, Batch, BatchIter};
use crate::query::eval::AtomOccurrence;
use crate::query::levels::EpochStore;
use crate::storage::ReadTx;
use itertools::Itertools;
use miette::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Debug, Formatter};

pub(crate) fn flatten_err<T, E1: Into<miette::Error>, E2: Into<miette::Error>>(
    v: std::result::Result<std::result::Result<T, E2>, E1>,
) -> Result<T> {
    match v {
        Err(e) => Err(e.into()),
        Ok(Err(e)) => Err(e.into()),
        Ok(Ok(v)) => Ok(v),
    }
}

pub(crate) fn filter_iter(
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

pub(crate) fn get_eliminate_indices(
    bindings: &[Symbol],
    eliminate: &BTreeSet<Symbol>,
) -> BTreeSet<usize> {
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

pub(crate) fn eliminate_from_tuple(mut ret: Tuple, eliminate_indices: &BTreeSet<usize>) -> Tuple {
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
            .collect();
    }
    ret
}

/// Write one joined row — a left row followed by a right row — straight
/// into `batch`'s flattened buffer, dropping any position in
/// `eliminate_indices`. The batched general join's row-construction
/// primitive: the equivalent of `tuple.extend(found); eliminate_from_tuple`
/// on the iterator path, but the joined row is never materialized as its
/// own `Tuple` — only the columns that survive elimination are ever copied,
/// and they go directly into the output batch.
///
/// `right` yields OWNED values (story #77 chunk 2: a `TupleInIter` over a
/// byte-backed regular store has nothing to reference — decoding produces
/// a value, not a borrow — so every caller now hands over values it
/// already owns or has decoded, never a borrow to reclone here).
pub(crate) fn push_joined_row(
    batch: &mut Batch,
    left: &[DataValue],
    right: impl Iterator<Item = DataValue>,
    eliminate_indices: &BTreeSet<usize>,
) -> Result<()> {
    batch.push_with(|buf| {
        if eliminate_indices.is_empty() {
            buf.extend_from_slice(left);
            buf.extend(right);
        } else {
            for (i, v) in left.iter().enumerate() {
                if !eliminate_indices.contains(&i) {
                    buf.push(v.clone());
                }
            }
            let base = left.len();
            for (j, v) in right.enumerate() {
                if !eliminate_indices.contains(&(base + j)) {
                    buf.push(v);
                }
            }
        }
        Ok(())
    })
}

/// A native batched prefix/point-lookup join against a stored relation
/// (current or as-of): shared by [`StoredRA`] and [`StoredWithValidityRA`],
/// whose row-at-a-time `prefix_join`s differ only in which storage method
/// builds the per-row match iterator (plain vs. as-of, plus the point-
/// lookup sub-case only `StoredRA` has) — captured once, at construction,
/// in `probe`.
///
/// The left side arrives pre-batched: no `Tuple` is ever minted for a left
/// row, since `probe` is handed a `&[DataValue]` slice straight out of the
/// batch buffer. Every accepted match is written once, directly into the
/// output batch ([`push_joined_row`]). A left row whose matches overflow
/// one output batch resumes exactly where it left off on the next
/// `next()` call: `active` holds the in-flight match iterator across
/// calls, so an output-batch boundary never re-scans anything.
pub(crate) struct PrefixProbeBatchJoin<'a> {
    pub(crate) left: BatchIter<'a>,
    /// Given one left row, the matching right-side rows — exactly what the
    /// row-at-a-time path's per-tuple closure would yield before the left
    /// prefix is appended.
    pub(crate) probe: Box<dyn FnMut(&[DataValue]) -> Result<TupleIter<'a>> + 'a>,
    pub(crate) filters_bytecodes: &'a [(Vec<Bytecode>, SourceSpan)],
    pub(crate) eliminate_indices: BTreeSet<usize>,
    /// The left batch currently being probed, and the cursor into it.
    pub(crate) cur: Option<(Batch, usize)>,
    /// The in-flight match iterator for the row at `cur`'s cursor.
    pub(crate) active: Option<TupleIter<'a>>,
    pub(crate) stack: Vec<DataValue>,
}

impl<'a> PrefixProbeBatchJoin<'a> {
    /// Pull left batches until a non-empty one arrives (or the stream
    /// ends), positioning the cursor at its first row.
    fn advance_left_batch(&mut self) -> Result<bool> {
        loop {
            match self.left.next() {
                None => {
                    self.cur = None;
                    return Ok(false);
                }
                Some(Err(e)) => return Err(e),
                Some(Ok(b)) => {
                    // An operator never yields an empty batch, but a
                    // defensive skip costs nothing and keeps this correct
                    // even if that invariant is ever loosened.
                    if !b.is_empty() {
                        self.cur = Some((b, 0));
                        return Ok(true);
                    }
                }
            }
        }
    }
}

impl<'a> Iterator for PrefixProbeBatchJoin<'a> {
    type Item = Result<Batch>;
    fn next(&mut self) -> Option<Self::Item> {
        let mut out = Batch::new();
        loop {
            if self.active.is_none() {
                let need_new_batch = match &self.cur {
                    Some((b, idx)) => *idx >= b.len(),
                    None => true,
                };
                if need_new_batch {
                    match self.advance_left_batch() {
                        Ok(false) => return if out.is_empty() { None } else { Some(Ok(out)) },
                        Ok(true) => {}
                        Err(e) => return Some(Err(e)),
                    }
                }
                let left_row = {
                    let (b, idx) = self.cur.as_ref().unwrap();
                    b.row(*idx)
                };
                match (self.probe)(left_row) {
                    Ok(it) => self.active = Some(it),
                    Err(e) => return Some(Err(e)),
                }
            }

            let (b, idx) = self.cur.as_ref().unwrap();
            let left_row = b.row(*idx);
            let mut exhausted = false;
            let active = self.active.as_mut().unwrap();
            while out.len() < BATCH_ROWS {
                match active.next() {
                    None => {
                        exhausted = true;
                        break;
                    }
                    Some(Err(e)) => return Some(Err(e)),
                    Some(Ok(found)) => {
                        let mut keep = true;
                        for (p, span) in self.filters_bytecodes.iter() {
                            match eval_bytecode_pred(p, &found, &mut self.stack, *span) {
                                Ok(true) => {}
                                Ok(false) => {
                                    keep = false;
                                    break;
                                }
                                Err(e) => return Some(Err(e)),
                            }
                        }
                        if keep
                            && let Err(e) = push_joined_row(
                                &mut out,
                                left_row,
                                found.iter().cloned(),
                                &self.eliminate_indices,
                            )
                        {
                            return Some(Err(e));
                        }
                    }
                }
            }
            if !exhausted {
                return Some(Ok(out));
            }
            self.active = None;
            if let Some((_, idx)) = self.cur.as_mut() {
                *idx += 1;
            }
        }
    }
}

/// Whether the right join columns are exactly a leading run of the right
/// relation's columns — the condition for a prefix scan instead of a
/// materialization. We do not consider a partial index match to be
/// "prefix", e.g. `[a, u => c]` with `a`, `c` bound and `u` unbound is not
/// "prefix", as it is not clear that prefix scanning in that case really
/// saves computation.
pub(crate) fn join_is_prefix(right_join_indices: &[usize]) -> bool {
    let mut indices = right_join_indices.to_vec();
    indices.sort();
    let l = indices.len();
    indices.into_iter().eq(0..l)
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
            | RelAlgebra::Search(_)
            | RelAlgebra::Spans(_)
            | RelAlgebra::Delta(_) => "generic_mat_join",
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
    /// Batched form of [`iter`](Self::iter). Covers two cases natively:
    ///
    /// - **Unit-left join**: every rule body is seeded with the `unit`
    ///   relation (one empty row, no columns) and atoms are folded on by
    ///   joining, so a single-relation scan compiles to `Join(unit, scan)`.
    ///   With an empty left the join has no keys and its output is exactly
    ///   the right relation's rows (each extended by the empty tuple), minus
    ///   any eliminated columns — identical rows in identical order to the
    ///   iterator path's `prefix_join` over the single unit row. Delegating
    ///   to `right.iter_batched` is what lets a scan→filter→project chain
    ///   run fully batched (otherwise the scan sits under this join and the
    ///   default chunker would re-run the iterator scan).
    /// - **General prefix join**: a non-unit left whose right side is a
    ///   `TempStore`, `Stored`, or `StoredWithValidity` scan joined on a
    ///   leading run of the right's columns (`join_is_prefix`) — exactly the
    ///   shape the iterator path routes to `prefix_join`/`point_lookup_join`
    ///   rather than [`materialized_join`](Self::materialized_join). The left
    ///   side is consumed as batches ([`RelAlgebra::iter_batched`]) and each
    ///   left row drives the same storage probe the row-at-a-time path
    ///   drives, via [`PrefixProbeBatchJoin`] /
    ///   [`TempStorePrefixBatchJoin`] — no `Tuple` minted for a left row,
    ///   and every joined row built once, straight into the output batch.
    ///
    /// Every other shape — a `Fixed` right side, and any non-prefix
    /// `TempStore`/`Stored`/`StoredWithValidity`, `Join`, `Filter`,
    /// `Unification`, or `Search` right side — goes through
    /// [`materialized_join_batched`](Self::materialized_join_batched):
    /// the right side materializes ONCE into a sorted deduplicated run
    /// and left batches probe it. (For `Fixed` right sides this replaced
    /// the deleted row-machine's in-memory hash match with the same
    /// sorted-run probe; Datalog answers are sets, so the dedup is
    /// observationally identical.)
    pub(crate) fn iter_batched<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        delta_rule: Option<AtomOccurrence>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
        segments: Segments<'a>,
    ) -> Result<BatchIter<'a>> {
        if self.left.is_unit() {
            let bindings = self.bindings();
            let eliminate_indices = get_eliminate_indices(&bindings, &self.to_eliminate);
            let right = self.right.iter_batched(tx, delta_rule, stores, segments)?;
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
        match &self.right {
            RelAlgebra::TempStore(r) => {
                let join_indices = self.joiner.join_indices(
                    &self.left.bindings_after_eliminate(),
                    &self.right.bindings_after_eliminate(),
                )?;
                if join_is_prefix(&join_indices.1) {
                    let bindings = self.bindings();
                    let eliminate_indices = get_eliminate_indices(&bindings, &self.to_eliminate);
                    let left = self.left.iter_batched(tx, delta_rule, stores, segments)?;
                    return r.prefix_join_batched(
                        left,
                        join_indices,
                        eliminate_indices,
                        delta_rule,
                        stores,
                    );
                }
            }
            RelAlgebra::Stored(r) => {
                let join_indices = self.joiner.join_indices(
                    &self.left.bindings_after_eliminate(),
                    &self.right.bindings_after_eliminate(),
                )?;
                if join_is_prefix(&join_indices.1) {
                    let bindings = self.bindings();
                    let eliminate_indices = get_eliminate_indices(&bindings, &self.to_eliminate);
                    let left = self.left.iter_batched(tx, delta_rule, stores, segments)?;
                    return r.prefix_join_batched(
                        tx,
                        left,
                        join_indices,
                        eliminate_indices,
                        segments,
                    );
                }
            }
            RelAlgebra::StoredWithValidity(r) => {
                let join_indices = self.joiner.join_indices(
                    &self.left.bindings_after_eliminate(),
                    &self.right.bindings_after_eliminate(),
                )?;
                if join_is_prefix(&join_indices.1) {
                    let bindings = self.bindings();
                    let eliminate_indices = get_eliminate_indices(&bindings, &self.to_eliminate);
                    let left = self.left.iter_batched(tx, delta_rule, stores, segments)?;
                    return r.prefix_join_batched(tx, left, join_indices, eliminate_indices);
                }
            }
            _ => {}
        }
        let bindings = self.bindings();
        let eliminate_indices = get_eliminate_indices(&bindings, &self.to_eliminate);
        self.materialized_join_batched(tx, eliminate_indices, delta_rule, stores, segments)
    }

    /// The general join, batch-native: materialize the right side ONCE into
    /// a sorted, deduplicated run keyed join-columns-first (consumed as
    /// batches — no right `Tuple` round-trips through an iterator), then
    /// drive it with left batches. No `Tuple` is minted for a left row: the
    /// probe prefix is built per row from the batch slice, the run is
    /// binary-searched, and every joined row is written once, straight into
    /// the output batch. Replaces the row-at-a-time
    /// `CachedMaterializedIterator` outright.
    fn materialized_join_batched<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        eliminate_indices: BTreeSet<usize>,
        delta_rule: Option<AtomOccurrence>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
        segments: Segments<'a>,
    ) -> Result<BatchIter<'a>> {
        let right_bindings = self.right.bindings_after_eliminate();
        let (left_join_indices, right_join_indices) = self
            .joiner
            .join_indices(&self.left.bindings_after_eliminate(), &right_bindings)?;

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

        let materialized = {
            let mut cache = BTreeSet::new();
            for batch in self.right.iter_batched(tx, delta_rule, stores, segments)? {
                let batch = batch?;
                for row in batch.iter_rows() {
                    cache.insert(
                        right_store_indices
                            .iter()
                            .map(|i| row[*i].clone())
                            .collect::<Tuple>(),
                    );
                }
            }
            cache.into_iter().collect_vec()
        };

        Ok(Box::new(MaterializedBatchJoin {
            left: self.left.iter_batched(tx, delta_rule, stores, segments)?,
            left_batch: None,
            left_row: 0,
            run_idx: usize::MAX,
            materialized,
            left_join_indices,
            right_invert_indices,
            eliminate_indices,
        }))
    }
}

/// The general join's batch executor: a sorted, deduplicated right run
/// probed by left batch rows. `run_idx == usize::MAX` marks "no run in
/// flight for the current left row"; an in-flight run resumes across
/// output-batch boundaries without re-searching.
struct MaterializedBatchJoin<'a> {
    left: BatchIter<'a>,
    left_batch: Option<Batch>,
    left_row: usize,
    run_idx: usize,
    materialized: Vec<Tuple>,
    left_join_indices: Vec<usize>,
    right_invert_indices: Vec<usize>,
    eliminate_indices: BTreeSet<usize>,
}

impl MaterializedBatchJoin<'_> {
    fn next_batch(&mut self) -> Result<Option<Batch>> {
        let mut out = Batch::new();
        loop {
            let Some(batch) = &self.left_batch else {
                match self.left.next() {
                    None => break,
                    Some(b) => {
                        self.left_batch = Some(b?);
                        self.left_row = 0;
                        self.run_idx = usize::MAX;
                        continue;
                    }
                }
            };
            if self.left_row >= batch.len() {
                self.left_batch = None;
                continue;
            }
            let left = batch.row(self.left_row);
            if self.run_idx == usize::MAX {
                // New left row: binary-search the run start by comparing
                // stored join columns against the projected left row — no
                // probe tuple is ever built.
                let lji = &self.left_join_indices;
                self.run_idx = self.materialized.partition_point(|stored| {
                    stored
                        .iter()
                        .take(lji.len())
                        .cmp(lji.iter().map(|i| &left[*i]))
                        == std::cmp::Ordering::Less
                });
            }
            let mut advanced = false;
            while let Some(stored) = self.materialized.get(self.run_idx) {
                let matches = self
                    .left_join_indices
                    .iter()
                    .map(|i| &left[*i])
                    .eq(stored.iter().take(self.left_join_indices.len()));
                if !matches {
                    break;
                }
                push_joined_row(
                    &mut out,
                    left,
                    self.right_invert_indices.iter().map(|i| stored[*i].clone()),
                    &self.eliminate_indices,
                )?;
                self.run_idx += 1;
                if out.is_full() {
                    advanced = true;
                    break;
                }
            }
            if advanced {
                return Ok(Some(out));
            }
            self.left_row += 1;
            self.run_idx = usize::MAX;
        }
        Ok(if out.is_empty() { None } else { Some(out) })
    }
}

impl Iterator for MaterializedBatchJoin<'_> {
    type Item = Result<Batch>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_batch().transpose()
    }
}
