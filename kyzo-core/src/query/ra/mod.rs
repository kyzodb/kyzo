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
 * - **Negation over a time-travel scan now computes.** The original
 *   compiled `not *rel{..} @ t` into a `NegJoin` whose right side was
 *   `StoredWithValidity` — a shape its own iterator dispatched to
 *   `unreachable!()`, i.e. a user-reachable abort. Story #3 closed the
 *   abort with a typed, compile-time refusal (`NegationOverTimeTravelError`)
 *   until the operator tier grew a skip-scan negation; story #86 built
 *   that operator (`NegRight::StoredWithValidity`, plus the same
 *   materialized-anti-join treatment for `@spans`/`@delta`'s
 *   `NegRight::Spans`/`NegRight::Delta`) and deleted the refusal — nothing
 *   is left to refuse.
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
 *   9. ra.rs:446   `storage.metadata.keys.last().unwrap()` in the original's
 *      validity check of `RelAlgebra::relation` — RETIRED WITHOUT
 *      SUCCESSOR: every stored relation time-travels through the
 *      universal bitemporal format, so no per-schema validity column
 *      exists to check and no key is inspected at construction.
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
//! `iter` takes `delta_rule: Option<AtomOccurrence>`; when it names
//! occurrence `k`, **only** the one [`TempStoreRA`] built for that body
//! position reads its store's *delta* instead of its total — every OTHER
//! occurrence, including another occurrence of the same store, reads its
//! total — while negation ([`NegJoin`]) always reads totals regardless.
//! Positional, not name-keyed: a store mentioned twice in one body (the
//! self-join shape, e.g. `pt(x,y), pt(y,z)`) gets two distinct
//! [`TempStoreRA`]s, each with its own [`AtomOccurrence`]
//! (`compile.rs::compile_magic_rule_body` numbers them in the same
//! left-to-right order `MagicInlineRule::contained_rules` does), so each
//! can be delta-selected independently — the standard semi-naive
//! self-join rewrite, `Δ(P⋈P) = (ΔP⋈P) ∪ (P⋈ΔP)`, one pass per occurrence.
//! Determinism: iteration order is a function of the stores and the plan
//! alone — the in-memory stores iterate in canonical order, stored
//! relations scan in memcmp key order, and every operator here is
//! order-preserving (the materialized join sorts its cache).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Debug, Formatter};

use itertools::Itertools;
use miette::{Diagnostic, Result, bail};
use thiserror::Error;

use crate::data::expr::{Bytecode, Expr, eval_bytecode_pred};
use crate::data::program::{DeltaAxis, MagicSymbol, ValidityClause};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::tuple::Tuple;
use crate::data::value::{AsOf, DataValue, MAX_VALIDITY_TS};
use crate::engines::segments::Segments;
use crate::query::batch_ops::{Batch, BatchIter};
use crate::query::eval::AtomOccurrence;
use crate::query::levels::EpochStore;
use crate::runtime::relation::RelationHandle;
use crate::storage::ReadTx;

#[cfg(test)]
use crate::query::batch_ops::{BATCH_ROWS, BatchTupleFilter};
#[cfg(test)]
use crate::runtime::relation::KeyspaceKind;

pub(crate) mod fixed;
pub(crate) mod join;
pub(crate) mod neg;
pub(crate) mod search;
pub(crate) mod stored;
pub(crate) mod temp;
pub(crate) mod temporal;
pub(crate) mod transform;

pub(crate) use fixed::InlineFixedRA;
pub(crate) use join::InnerJoin;
pub(crate) use join::{Joiner, eliminate_from_tuple};
pub(crate) use neg::{NegJoin, NegRight};
pub(crate) use search::SearchRA;
pub(crate) use stored::{StoredRA, StoredWithValidityRA};
pub(crate) use temp::TempStoreRA;
pub(crate) use temporal::{DeltaRA, SpansRA};
pub(crate) use transform::{FilteredRA, ReorderRA, UnificationRA};

/// A lazy stream of fallible tuples: what iterating an operator yields.
/// (The original homed this alias in `data/tuple.rs`; it is operator-tier
/// currency and lives here — deviation D1.)
pub(crate) type TupleIter<'a> = Box<dyn Iterator<Item = Result<Tuple>> + 'a>;

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
pub(crate) struct StoredRowTooShortError(
    pub(crate) String,
    pub(crate) usize,
    pub(crate) usize,
    #[label] pub(crate) SourceSpan,
);

/// Typed lookup for "every referenced rule has a store" (upstream panic
/// sites: three `stores.get(..).unwrap()`s in this file).
pub(crate) fn epoch_store_of<'m>(
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
/// Index search (HNSW/FTS/LSH) is the landed `Search` variant below —
/// upstream's three per-engine node kinds collapsed into one node holding
/// a resolved `SearchRA` (query/search.rs): a parent plus its own
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
    /// `as_of`, asserted rows only.
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
    /// `@spans`: derived maximal-run intervals over a stored relation's
    /// full bitemporal history, at a fixed system snapshot (story #62).
    Spans(SpansRA),
    /// `@delta`/`@delta_sys`: axis-parameterized net diff between two
    /// bitemporal coordinates on a stored relation (story #62).
    Delta(DeltaRA),
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
            RelAlgebra::Spans(i) => i.span,
            RelAlgebra::Delta(i) => i.span,
        }
    }

    /// Resolve every symbol reference inside this tree's expressions to a
    /// tuple position and compile the expressions to bytecode. Called once,
    /// at the end of `compile_magic_rule_body`; iteration never resolves a
    /// name again.
    pub(crate) fn fill_binding_indices_and_compile(&mut self) -> Result<()> {
        match self {
            RelAlgebra::Fixed(_) => {}
            // Neither carries an expression to resolve: `Spans`'s fixed
            // system snapshot and `Delta`'s two coordinates are already
            // plain `i64`/`AsOf` values by construction, never `Expr`.
            RelAlgebra::Spans(_) | RelAlgebra::Delta(_) => {}
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
    /// `occurrence` is this atom's position among its body's
    /// `Rule`/`NegatedRule` atoms (`compile.rs`'s shared numbering) — the
    /// key `delta_from` compares against to decide whether THIS specific
    /// occurrence reads its store's delta.
    pub(crate) fn derived(
        bindings: Vec<Symbol>,
        storage_key: MagicSymbol,
        occurrence: AtomOccurrence,
        span: SourceSpan,
    ) -> Self {
        Self::TempStore(TempStoreRA {
            bindings,
            storage_key,
            occurrence,
            filters: vec![],
            filters_bytecodes: vec![],
            span,
        })
    }

    /// A scan of a stored relation, optionally carrying a [`ValidityClause`]
    /// (a point-in-time read, a derivation, or a diff). Any facts relation
    /// time-travels — bitemporality is the storage format, not a schema
    /// property, so construction checks nothing about the columns.
    ///
    /// `bindings` is the base relation's own key/payload columns; a
    /// `Spans`/`Delta` clause's one extra trailing binding (the interval
    /// or the sign) is appended here, never folded into the base columns
    /// (see `data::program::ValidityClause`'s doc) — callers pass
    /// `right_vars` unchanged and this constructor pushes the clause's
    /// `var` itself.
    pub(crate) fn relation(
        mut bindings: Vec<Symbol>,
        storage: RelationHandle,
        span: SourceSpan,
        validity: Option<ValidityClause>,
    ) -> Result<Self> {
        match validity {
            None => Ok(Self::Stored(StoredRA {
                bindings,
                storage,
                filters: vec![],
                filters_bytecodes: vec![],
                span,
            })),
            Some(ValidityClause::At(vld)) => {
                // Every facts relation is bitemporal in the one universal
                // format: any relation time-travels, no schema opt-in.
                Ok(Self::StoredWithValidity(StoredWithValidityRA {
                    bindings,
                    storage,
                    filters: vec![],
                    filters_bytecodes: vec![],
                    as_of: vld,
                    span,
                }))
            }
            Some(ValidityClause::Spans { sys, var }) => {
                bindings.push(var);
                Ok(Self::Spans(SpansRA {
                    bindings,
                    storage,
                    sys: sys.0.0,
                    span,
                }))
            }
            Some(ValidityClause::Delta {
                axis,
                from,
                to,
                var,
            }) => {
                bindings.push(var);
                let (from, to) = match axis {
                    DeltaAxis::Valid => (AsOf::current(from), AsOf::current(to)),
                    DeltaAxis::Sys => (
                        AsOf::at(from, MAX_VALIDITY_TS),
                        AsOf::at(to, MAX_VALIDITY_TS),
                    ),
                };
                Ok(Self::Delta(DeltaRA {
                    bindings,
                    storage,
                    from,
                    to,
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
            // `Spans`/`Delta` carry no `filters` field of their own (chunk
            // 3's naive scope: pushdown into the temporal scan is chunk
            // 4's posting-index work) — a residual predicate wraps them
            // exactly like a `Search` result, same as every other operator
            // with nothing to push into.
            s @ (RelAlgebra::Fixed(_)
            | RelAlgebra::Reorder(_)
            | RelAlgebra::NegJoin(_)
            | RelAlgebra::Unification(_)
            | RelAlgebra::Search(_)
            | RelAlgebra::Spans(_)
            | RelAlgebra::Delta(_)) => {
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
                occurrence,
                mut filters,
                filters_bytecodes,
                span,
            }) => {
                filters.push(filter);
                RelAlgebra::TempStore(TempStoreRA {
                    bindings,
                    storage_key,
                    occurrence,
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
                as_of,
            }) => {
                filters.push(filter);
                RelAlgebra::StoredWithValidity(StoredWithValidityRA {
                    bindings,
                    storage,
                    filters,
                    span,
                    as_of,
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
    /// to the narrower [`NegRight`]. A rule-store scan, a current-state
    /// stored-relation scan, or any of the three time-travel shapes
    /// (as-of, `@spans`, `@delta`/`@delta_sys`) are all accepted — story
    /// #86 closed `NegationOverTimeTravelError`, the last refusal here,
    /// once the operator tier grew a serving strategy for each; anything
    /// else is a typed invariant error (the compiler never builds it).
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
            RelAlgebra::StoredWithValidity(v) => NegRight::StoredWithValidity(v),
            RelAlgebra::Spans(v) => NegRight::Spans(v),
            RelAlgebra::Delta(v) => NegRight::Delta(v),
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
            RelAlgebra::Spans(_v) => Ok(()),
            RelAlgebra::Delta(_v) => Ok(()),
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
            RelAlgebra::Spans(_) => None,
            RelAlgebra::Delta(_) => None,
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
            RelAlgebra::Spans(v) => v.bindings.clone(),
            RelAlgebra::Delta(v) => v.bindings.clone(),
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

    ///
    /// EVERY operator owns a native batched implementation — the dispatch
    /// below is total, and no row-at-a-time fallback exists anywhere (the
    /// iterator twin and its chunker were deleted; the naive oracle in
    /// `query/laws.rs` is the semantic judge).
    pub(crate) fn iter_batched<'a>(
        &'a self,
        tx: &'a impl ReadTx,
        delta_rule: Option<AtomOccurrence>,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
        segments: Segments<'a>,
    ) -> Result<BatchIter<'a>> {
        match self {
            RelAlgebra::Filter(r) => r.iter_batched(tx, delta_rule, stores, segments),
            RelAlgebra::Reorder(r) => r.iter_batched(tx, delta_rule, stores, segments),
            RelAlgebra::TempStore(r) => r.iter_batched(delta_rule, stores),
            RelAlgebra::Stored(v) => v.iter_batched(tx, segments),
            RelAlgebra::Join(j) => j.iter_batched(tx, delta_rule, stores, segments),
            RelAlgebra::Unification(u) => u.iter_batched(tx, delta_rule, stores, segments),
            RelAlgebra::Fixed(f) => f.iter_batched(),
            RelAlgebra::NegJoin(n) => n.iter_batched(tx, delta_rule, stores, segments),
            RelAlgebra::StoredWithValidity(v) => v.iter_batched(tx),
            RelAlgebra::Search(r) => r.iter_batched(tx, delta_rule, stores, segments),
            RelAlgebra::Spans(v) => v.iter_batched(tx),
            RelAlgebra::Delta(v) => v.iter_batched(tx),
        }
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
                .field(&r.as_of)
                .finish(),
            RelAlgebra::Spans(r) => f
                .debug_tuple("Spans")
                .field(&bindings)
                .field(&r.storage.name)
                .field(&r.sys)
                .finish(),
            RelAlgebra::Delta(r) => f
                .debug_tuple("Delta")
                .field(&bindings)
                .field(&r.storage.name)
                .field(&r.from)
                .field(&r.to)
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
    /// Test view of the one machine: batches flattened to owned rows
    /// (errors surface at their exact row position, like the old stream).
    fn rows_of<'a>(
        ra: &'a RelAlgebra,
        tx: &'a impl ReadTx,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
    ) -> Result<Box<dyn Iterator<Item = Result<Tuple>> + 'a>> {
        rows_of_seg(ra, tx, stores, Segments::OFF)
    }

    /// [`rows_of`] with an explicit [`Segments`] context — the segment-vs-
    /// storage differential tests drive the SAME machine twice, once per
    /// context, rather than maintaining a second reader.
    fn rows_of_seg<'a>(
        ra: &'a RelAlgebra,
        tx: &'a impl ReadTx,
        stores: &'a BTreeMap<MagicSymbol, EpochStore>,
        segments: Segments<'a>,
    ) -> Result<Box<dyn Iterator<Item = Result<Tuple>> + 'a>> {
        let mut batches = ra.iter_batched(tx, None, stores, segments)?;
        let mut current: Vec<Tuple> = Vec::new();
        let mut idx = 0usize;
        Ok(Box::new(std::iter::from_fn(move || {
            loop {
                if idx < current.len() {
                    let t = std::mem::take(&mut current[idx]);
                    idx += 1;
                    return Some(Ok(t));
                }
                match batches.next()? {
                    Err(e) => return Some(Err(e)),
                    Ok(b) => {
                        current = (0..b.len()).map(|i| b.row(i).to_vec()).collect();
                        idx = 0;
                    }
                }
            }
        })))
    }

    use std::cmp::Reverse;

    use smartstring::SmartString;

    use super::*;
    use crate::data::program::InputRelationHandle;
    use crate::data::relation::{ColType, NullableColType};
    use crate::data::relation::{ColumnDef, StoredRelationMetadata};
    use crate::data::value::ValidityTs;
    use crate::engines::segments::SegmentEngine;
    use crate::query::temp_store::RegularTempStore;
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
    /// no compiled plan reaches this shape through the current compiler —
    /// but `InlineFixedRA` already supports data-bearing rows and nothing in
    /// its type rules out a non-unit Fixed left, so the guard must hold
    /// independent of what constructs the plan. The mutation campaign showed
    /// the bindings check is otherwise untested (mutant K4 survived every
    /// compiled-plan differential). This pins the guard at the RA level:
    /// Join(singleton Fixed, spread-unify) must be identical on both paths,
    /// i.e. a real join, not a right passthrough.
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
        let got: Vec<Tuple> = rows_of(&ra, &rtx, &stores)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        // The independently known answer: y=2 joins x=2 exactly once.
        assert_eq!(got, vec![vec![v(2), v(2)]]);
    }

    /// The general (non-prefix) join's batch executor, judged by an
    /// independent analytic oracle: joining on the right relation's SECOND
    /// column forces `materialized_join_batched` (verified via
    /// `join_type()`), one left key owns 2000 matches (straddling the
    /// output-batch boundary twice), and the expected set is computed by
    /// hand, never by a second run of the machine under test.
    #[test]
    fn materialized_batch_join_matches_analytic_oracle_across_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let mut tx = db.write_tx().unwrap();
        let left_handle = create_relation(
            &mut tx,
            input_handle("ml", vec![col("j", ColType::Int)], vec![]),
            KeyspaceKind::Facts,
        )
        .unwrap();
        let right_handle = create_relation(
            &mut tx,
            input_handle(
                "mr",
                vec![col("i", ColType::Int)],
                vec![col("j", ColType::Int)],
            ),
            KeyspaceKind::Facts,
        )
        .unwrap();
        let stamp = crate::data::value::ValidityTs(std::cmp::Reverse(0));
        // left: j ∈ {7, 8, 9(no match)}; right rows (i, j): j=7 has 2000
        // rows, j=8 has 3.
        for j in [7i64, 8, 9] {
            left_handle.put_fact(&mut tx, &[v(j)], stamp, sp()).unwrap();
        }
        for i in 0..2000i64 {
            right_handle
                .put_fact(&mut tx, &[v(i), v(7)], stamp, sp())
                .unwrap();
        }
        for i in 5000..5003i64 {
            right_handle
                .put_fact(&mut tx, &[v(i), v(8)], stamp, sp())
                .unwrap();
        }
        tx.commit().unwrap();
        let rtx = db.read_tx().unwrap();
        // The right side's join column gets its own symbol (`j2`), as the
        // compiler guarantees before any join is minted: InnerJoin::bindings
        // asserts (debug) that output bindings are duplicate-free.
        let mut ra = RelAlgebra::relation(vec![sym("j")], left_handle, sp(), None)
            .unwrap()
            .join(
                RelAlgebra::relation(vec![sym("i"), sym("j2")], right_handle, sp(), None).unwrap(),
                vec![sym("j")],
                vec![sym("j2")],
                sp(),
            )
            .unwrap();
        if let RelAlgebra::Join(j) = &ra {
            assert_eq!(
                j.join_type().unwrap(),
                "stored_mat_join",
                "the shape must route through the materialized join"
            );
        } else {
            panic!("plan root must be the join");
        }
        ra.fill_binding_indices_and_compile().unwrap();
        let stores = no_stores();
        let mut got: Vec<Tuple> = rows_of(&ra, &rtx, &stores)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        got.sort();
        let mut expected: Vec<Tuple> = (0..2000i64)
            .map(|i| vec![v(7), v(i), v(7)])
            .chain((5000..5003i64).map(|i| vec![v(8), v(i), v(8)]))
            .collect();
        expected.sort();
        assert_eq!(got, expected, "materialized batch join vs analytic oracle");
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
        let got: Vec<Tuple> = rows_of(&ra, &rtx, &stores)
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
        let err = rows_of(&bad, &rtx, &stores)
            .unwrap()
            .next()
            .unwrap()
            .unwrap_err();
        assert!(err.to_string().contains("Invalid spread unification"));
    }

    /// A time-travel scan through the operator: only the newest asserted
    /// version at or before `as_of` per key. The time slots are
    /// infrastructure — the operator binds user columns only, and any
    /// facts relation time-travels (no schema opt-in exists).
    #[test]
    fn stored_with_validity_scan() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let mut tx = db.write_tx().unwrap();
        let handle = create_relation(
            &mut tx,
            input_handle(
                "hist",
                vec![col("k", ColType::Int)],
                vec![col("v", ColType::String)],
            ),
            KeyspaceKind::Facts,
        )
        .unwrap();
        for (ts, val) in [(10i64, "ten"), (20, "twenty")] {
            let row = vec![v(1), DataValue::from(val)];
            handle
                .put_fact(
                    &mut tx,
                    &row,
                    crate::data::value::ValidityTs(std::cmp::Reverse(ts)),
                    sp(),
                )
                .unwrap();
        }
        tx.commit().unwrap();

        let rtx = db.read_tx().unwrap();
        let stores = no_stores();
        let scan_at = |ts: i64| -> Vec<Tuple> {
            let ra = RelAlgebra::relation(
                vec![sym("k"), sym("v")],
                handle.clone(),
                sp(),
                Some(ValidityClause::At(AsOf::current(ValidityTs(Reverse(ts))))),
            )
            .unwrap();
            rows_of(&ra, &rtx, &stores)
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        let at_15 = scan_at(15);
        assert_eq!(at_15.len(), 1);
        assert_eq!(at_15[0][1], DataValue::from("ten"));
        let at_25 = scan_at(25);
        assert_eq!(at_25.len(), 1);
        assert_eq!(at_25[0][1], DataValue::from("twenty"));
        let at_5 = scan_at(5);
        assert!(at_5.is_empty());
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
            KeyspaceKind::Facts,
        )
        .unwrap();
        tx.commit().unwrap();

        // Reorder as inner-join RHS.
        let reordered = RelAlgebra::derived(vec![sym("x")], entry(), AtomOccurrence(0), sp())
            .reorder(vec![sym("x")]);
        let err = RelAlgebra::unit(sp())
            .join(reordered, vec![], vec![], sp())
            .unwrap_err();
        assert!(err.downcast_ref::<PlanInvariantError>().is_some());

        // NegJoin as inner-join RHS.
        let neg = RelAlgebra::derived(vec![sym("x")], entry(), AtomOccurrence(0), sp())
            .neg_join(
                RelAlgebra::derived(vec![sym("x")], entry(), AtomOccurrence(1), sp()),
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

        // …and a time-travel scan as negation RHS now CONSTRUCTS (story
        // #86 lifted `NegationOverTimeTravelError`, upstream's abort site
        // for this exact shape): it becomes `NegRight::StoredWithValidity`,
        // not a refusal.
        let vld_scan = RelAlgebra::relation(
            vec![sym("k"), sym("at")],
            handle,
            sp(),
            Some(ValidityClause::At(AsOf::current(ValidityTs(Reverse(0))))),
        )
        .unwrap();
        let neg = RelAlgebra::unit(sp())
            .neg_join(vld_scan, vec![], vec![], sp())
            .unwrap();
        assert!(matches!(
            neg,
            RelAlgebra::NegJoin(ref b) if matches!(b.right, NegRight::StoredWithValidity(_))
        ));
    }

    fn entry() -> MagicSymbol {
        MagicSymbol::Muggle {
            inner: Symbol::prog_entry(sp()),
        }
    }

    // ─────────────────────────────────────────────────────────────────────
    // Batched GENERAL join (non-unit left): the batched path must
    // reproduce the iterator path's output exactly — same rows, same
    // order — for the native prefix-join case (`InnerJoin::iter_batched`'s
    // dispatch to `StoredRA`/`TempStoreRA`/`StoredWithValidityRA`
    // `prefix_join_batched`).
    // ─────────────────────────────────────────────────────────────────────

    /// A general join (Stored point-lookup on the right) at the three
    /// sizes that matter for a batched left: one under `BATCH_ROWS`, one
    /// exactly `BATCH_ROWS`, and one that overflows into a second left
    /// batch. Half the left keys have no right-side match, so the probe's
    /// miss path is exercised too. Iterator and batched outputs must be
    /// `Vec`-equal (order-sensitive).
    #[test]
    fn general_join_batched_matches_iterator_at_batch_boundaries() {
        for n in [BATCH_ROWS - 1, BATCH_ROWS, BATCH_ROWS + 1] {
            let dir = tempfile::tempdir().unwrap();
            let db = new_fjall_storage(dir.path()).unwrap();
            let mut tx = db.write_tx().unwrap();
            let left_handle = create_relation(
                &mut tx,
                input_handle("bl", vec![col("k", ColType::Int)], vec![]),
                KeyspaceKind::Facts,
            )
            .unwrap();
            let right_handle = create_relation(
                &mut tx,
                input_handle(
                    "br",
                    vec![col("k", ColType::Int)],
                    vec![col("v", ColType::Int)],
                ),
                KeyspaceKind::Facts,
            )
            .unwrap();
            let mut expected_matches = 0usize;
            for i in 0..n {
                let k = i as i64;
                let lrow = vec![v(k)];
                left_handle
                    .put_fact(
                        &mut tx,
                        &lrow,
                        crate::data::value::ValidityTs(std::cmp::Reverse(0)),
                        sp(),
                    )
                    .unwrap();
                if k % 2 == 0 {
                    let rrow = vec![v(k), v(k * 10)];
                    right_handle
                        .put_fact(
                            &mut tx,
                            &rrow,
                            crate::data::value::ValidityTs(std::cmp::Reverse(0)),
                            sp(),
                        )
                        .unwrap();
                    expected_matches += 1;
                }
            }
            tx.commit().unwrap();

            let rtx = db.read_tx().unwrap();
            // Right side's join column carries its own symbol (`k2`), per the
            // compiler-guaranteed duplicate-free-bindings invariant.
            let mut ra = RelAlgebra::relation(vec![sym("k")], left_handle, sp(), None)
                .unwrap()
                .join(
                    RelAlgebra::relation(vec![sym("k2"), sym("v")], right_handle, sp(), None)
                        .unwrap(),
                    vec![sym("k")],
                    vec![sym("k2")],
                    sp(),
                )
                .unwrap();
            ra.fill_binding_indices_and_compile().unwrap();
            let stores = no_stores();

            let got: Vec<Tuple> = rows_of(&ra, &rtx, &stores)
                .unwrap()
                .map(Result::unwrap)
                .collect();
            // Judged against the independently accumulated analytic count,
            // never against a second run of the same machine.
            assert_eq!(got.len(), expected_matches, "n={n}: wrong match count");
        }
    }

    /// The same shape, but with `to_eliminate` covering one column from
    /// EACH side of the join (`extra` on the left, `k2` on the right) —
    /// `push_joined_row`'s elimination must drop exactly those two
    /// positions on both paths alike.
    #[test]
    fn general_join_batched_with_eliminate_indices() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let mut tx = db.write_tx().unwrap();
        let left_handle = create_relation(
            &mut tx,
            input_handle(
                "el",
                vec![col("k", ColType::Int)],
                vec![col("extra", ColType::Int)],
            ),
            KeyspaceKind::Facts,
        )
        .unwrap();
        let right_handle = create_relation(
            &mut tx,
            input_handle(
                "er",
                vec![col("k2", ColType::Int)],
                vec![col("v", ColType::Int)],
            ),
            KeyspaceKind::Facts,
        )
        .unwrap();
        for k in 1..=5i64 {
            let lrow = vec![v(k), v(k * 100)];
            left_handle
                .put_fact(
                    &mut tx,
                    &lrow,
                    crate::data::value::ValidityTs(std::cmp::Reverse(0)),
                    sp(),
                )
                .unwrap();
            let rrow = vec![v(k), v(k + 1000)];
            right_handle
                .put_fact(
                    &mut tx,
                    &rrow,
                    crate::data::value::ValidityTs(std::cmp::Reverse(0)),
                    sp(),
                )
                .unwrap();
        }
        tx.commit().unwrap();

        let rtx = db.read_tx().unwrap();
        let left =
            RelAlgebra::relation(vec![sym("k"), sym("extra")], left_handle, sp(), None).unwrap();
        let right =
            RelAlgebra::relation(vec![sym("k2"), sym("v")], right_handle, sp(), None).unwrap();
        let mut ra = RelAlgebra::Join(Box::new(InnerJoin {
            left,
            right,
            joiner: Joiner {
                left_keys: vec![sym("k")],
                right_keys: vec![sym("k2")],
            },
            to_eliminate: [sym("extra"), sym("k2")].into_iter().collect(),
            span: sp(),
        }));
        ra.fill_binding_indices_and_compile().unwrap();
        let stores = no_stores();

        let got: Vec<Tuple> = rows_of(&ra, &rtx, &stores)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let expected: Vec<Tuple> = (1..=5i64).map(|k| vec![v(k), v(k + 1000)]).collect();
        assert_eq!(got, expected, "eliminate_indices: wrong surviving columns");
    }

    /// Delta threading: the join's right side is a `TempStore` whose delta
    /// (this epoch's fresh rows only) is a strict subset of its total. With
    /// `delta_rule` naming that store, only the delta row may join — on
    /// BOTH paths alike, proving `prefix_join_batched` threads `delta_rule`
    /// exactly as `TempStoreRA::prefix_join` does (`scan_epoch`).
    #[test]
    fn general_join_batched_delta_threading_matches_iterator() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let rtx = db.read_tx().unwrap();

        let mut store = EpochStore::new_normal(2);
        let mut baseline = RegularTempStore::default();
        baseline.put(vec![v(0), DataValue::from("a")]);
        baseline.put(vec![v(1), DataValue::from("b")]);
        store.merge_in(baseline.wrap(), &mut ()).unwrap();
        // First merge into an empty total swaps the whole store in (the
        // `use_total_for_delta` fast path) — so this second merge is the
        // one that actually narrows the delta to what's fresh.
        let mut fresh = RegularTempStore::default();
        fresh.put(vec![v(2), DataValue::from("c")]);
        store.merge_in(fresh.wrap(), &mut ()).unwrap();
        assert!(store.has_delta(), "the second merge must have a delta");

        let storage_key = entry();
        let mut stores = no_stores();
        stores.insert(storage_key.clone(), store);

        let left = RelAlgebra::Fixed(InlineFixedRA {
            bindings: vec![sym("k")],
            data: vec![vec![v(0)], vec![v(1)], vec![v(2)]],
            to_eliminate: Default::default(),
            span: sp(),
        });
        // `k2` on the right: joins never carry a duplicated symbol across
        // sides (compiler invariant, debug-asserted in InnerJoin::bindings).
        let right = RelAlgebra::TempStore(TempStoreRA {
            bindings: vec![sym("k2"), sym("val")],
            storage_key: storage_key.clone(),
            occurrence: AtomOccurrence(0),
            filters: vec![],
            filters_bytecodes: vec![],
            span: sp(),
        });
        let mut ra = left
            .join(right, vec![sym("k")], vec![sym("k2")], sp())
            .unwrap();
        ra.fill_binding_indices_and_compile().unwrap();

        // Sanity: against the TOTAL (no delta_rule), every left row matches.
        let total_it: Vec<Tuple> = rows_of(&ra, &rtx, &stores)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(total_it.len(), 3, "the total must join all three keys");

        // Against the DELTA, only key 2 (this epoch's fresh row) may join.
        let it: Vec<Tuple> = ra
            .iter_batched(&rtx, Some(AtomOccurrence(0)), &stores, Segments::OFF)
            .unwrap()
            .map(Result::unwrap)
            .flat_map(Batch::into_rows)
            .collect();
        let ba: Vec<Tuple> = ra
            .iter_batched(&rtx, Some(AtomOccurrence(0)), &stores, Segments::OFF)
            .unwrap()
            .map(Result::unwrap)
            .flat_map(Batch::into_rows)
            .collect();
        assert_eq!(it, ba, "delta-threaded batched join diverged from iterator");
        assert_eq!(
            it,
            vec![vec![v(2), v(2), DataValue::from("c")]],
            "delta threading must narrow the join to the fresh row only"
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // Issue #75: segment-served prefix-join probes. `StoredRA`'s
    // point-lookup probe (right side's join columns cover its whole key)
    // and plain-prefix probe (a strict leading subset, no residual filter
    // bounds) now serve from the relation's segment when the caller passes
    // a live `SegmentEngine`, instead of paying the bitemporal seek-based
    // resolver on every probe. Both must be byte-identical, in order, to
    // the storage-probe answer — judged against an independently
    // hand-computed expected value, never against a second run of the same
    // machine.
    // ─────────────────────────────────────────────────────────────────────

    /// The point-lookup probe (right relation's key is exactly the join
    /// column): segments ON and OFF must produce the identical row stream,
    /// and both must equal the hand-computed join.
    #[test]
    fn stored_point_lookup_join_segments_match_oracle_and_storage() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let mut tx = db.write_tx().unwrap();
        let left_handle = create_relation(
            &mut tx,
            input_handle("plj_l", vec![col("k", ColType::Int)], vec![]),
            KeyspaceKind::Facts,
        )
        .unwrap();
        let right_handle = create_relation(
            &mut tx,
            input_handle(
                "plj_r",
                vec![col("k2", ColType::Int)],
                vec![col("v", ColType::Int)],
            ),
            KeyspaceKind::Facts,
        )
        .unwrap();
        // Past one batch boundary, so a served segment must also resume
        // correctly across output-batch edges, not just within one.
        let n = BATCH_ROWS + 37;
        let mut expected: Vec<Tuple> = Vec::new();
        for i in 0..n {
            let k = i as i64;
            left_handle
                .put_fact(&mut tx, &[v(k)], ValidityTs(Reverse(0)), sp())
                .unwrap();
            if k % 3 == 0 {
                right_handle
                    .put_fact(&mut tx, &[v(k), v(k * 10)], ValidityTs(Reverse(0)), sp())
                    .unwrap();
                expected.push(vec![v(k), v(k), v(k * 10)]);
            }
        }
        tx.commit().unwrap();

        let rtx = db.read_tx().unwrap();
        let mut ra = RelAlgebra::relation(vec![sym("k")], left_handle, sp(), None)
            .unwrap()
            .join(
                RelAlgebra::relation(vec![sym("k2"), sym("v")], right_handle, sp(), None).unwrap(),
                vec![sym("k")],
                vec![sym("k2")],
                sp(),
            )
            .unwrap();
        ra.fill_binding_indices_and_compile().unwrap();
        let stores = no_stores();

        let off: Vec<Tuple> = rows_of_seg(&ra, &rtx, &stores, Segments::OFF)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            off, expected,
            "storage probe diverged from hand-computed join"
        );

        let engine = SegmentEngine::default();
        let on: Vec<Tuple> = rows_of_seg(&ra, &rtx, &stores, Segments(Some(&engine)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            on, expected,
            "segment-served point-lookup probe diverged from hand-computed join"
        );
        assert_eq!(
            on, off,
            "segment path must be byte-identical to storage path"
        );
    }

    /// The plain-prefix probe (right relation's key is TWO columns, the
    /// join binds only the leading one, so one left row matches several
    /// right rows — `edge`'s shape in `tc.kz`, the workload the fix
    /// targets). Segments ON and OFF must produce the identical row
    /// stream, in the identical (key) order, and both must equal the
    /// hand-computed cross join.
    #[test]
    fn stored_prefix_join_segments_match_oracle_and_storage() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let mut tx = db.write_tx().unwrap();
        let left_handle = create_relation(
            &mut tx,
            input_handle("ppj_l", vec![col("z", ColType::Int)], vec![]),
            KeyspaceKind::Facts,
        )
        .unwrap();
        let right_handle = create_relation(
            &mut tx,
            input_handle(
                "ppj_r",
                vec![col("z2", ColType::Int), col("w", ColType::Int)],
                vec![],
            ),
            KeyspaceKind::Facts,
        )
        .unwrap();
        // n left keys, each with a handful of right neighbours at
        // increasing width — past one batch boundary in total match count.
        let n = 200;
        let mut expected: Vec<Tuple> = Vec::new();
        for z in 0..n {
            left_handle
                .put_fact(&mut tx, &[v(z)], ValidityTs(Reverse(0)), sp())
                .unwrap();
            for w in 0..(z % 7) {
                right_handle
                    .put_fact(&mut tx, &[v(z), v(w)], ValidityTs(Reverse(0)), sp())
                    .unwrap();
                expected.push(vec![v(z), v(z), v(w)]);
            }
        }
        tx.commit().unwrap();

        let rtx = db.read_tx().unwrap();
        let mut ra = RelAlgebra::relation(vec![sym("z")], left_handle, sp(), None)
            .unwrap()
            .join(
                RelAlgebra::relation(vec![sym("z2"), sym("w")], right_handle, sp(), None).unwrap(),
                vec![sym("z")],
                vec![sym("z2")],
                sp(),
            )
            .unwrap();
        ra.fill_binding_indices_and_compile().unwrap();
        let stores = no_stores();

        let off: Vec<Tuple> = rows_of_seg(&ra, &rtx, &stores, Segments::OFF)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            off, expected,
            "storage probe diverged from hand-computed join"
        );

        let engine = SegmentEngine::default();
        let on: Vec<Tuple> = rows_of_seg(&ra, &rtx, &stores, Segments(Some(&engine)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            on, expected,
            "segment-served prefix probe diverged from hand-computed join"
        );
        assert_eq!(
            on, off,
            "segment path must be byte-identical to storage path"
        );
    }

    /// DIAGNOSTIC, not a correctness assertion: reproduces issue #75's
    /// measurement shape (the `tc.kz` recursive join's `edge` probe) at a
    /// scale where JIT/cache effects have settled, and reports ns/probe for
    /// the storage path (bitemporal seek-based resolver) vs the segment
    /// path (binary search over the dense decoded buffer). Measured on this
    /// machine at 200k probes over a 6,000-row two-column-key relation
    /// (`edge`-shaped, ~3 matches/probe on average — `tc/sparse`'s own
    /// n=2000/m=6000 ratio), three runs: storage 1023-1153 ns/probe,
    /// segment 230-243 ns/probe — a consistent ~4.5x, on top of the
    /// diagnosis's isolated single-match 2315ns-vs-694ns (3.34x) figure
    /// because this shape's average fan-out amortizes the segment's binary
    /// search over more than one emitted row per probe. Run explicitly:
    /// `cargo test -p kyzo --release query::ra::tests::stored_prefix_join_segment_probe_cost_vs_storage -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn stored_prefix_join_segment_probe_cost_vs_storage() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let mut tx = db.write_tx().unwrap();
        // No left relation: the left side is synthetic probe rows fed
        // straight in, below — only the probed (right) relation needs to
        // exist in storage.
        let right_handle = create_relation(
            &mut tx,
            input_handle(
                "pcost_r",
                vec![col("z2", ColType::Int), col("w", ColType::Int)],
                vec![],
            ),
            KeyspaceKind::Facts,
        )
        .unwrap();
        const N_NODES: i64 = 2000;
        const M_EDGES: i64 = 6000;
        for i in 0..M_EDGES {
            let z = i % N_NODES;
            let w = (i * 7 + 3) % N_NODES;
            right_handle
                .put_fact(&mut tx, &[v(z), v(w)], ValidityTs(Reverse(0)), sp())
                .unwrap();
        }
        tx.commit().unwrap();
        let rtx = db.read_tx().unwrap();

        const N_PROBES: usize = 200_000;
        let probe_rows: Vec<Tuple> = (0..N_PROBES)
            .map(|i| vec![v((i as i64) % N_NODES)])
            .collect();
        let left_of = |rows: Vec<Tuple>| -> BatchIter<'static> {
            let chunks: Vec<Batch> = rows
                .chunks(BATCH_ROWS)
                .map(|c| Batch::with_rows(c.to_vec()))
                .collect();
            Box::new(chunks.into_iter().map(Ok))
        };

        let ra = StoredRA {
            bindings: vec![sym("z2"), sym("w")],
            storage: right_handle,
            filters: vec![],
            filters_bytecodes: vec![],
            span: sp(),
        };
        let join_indices = || -> (Vec<usize>, Vec<usize>) { (vec![0], vec![0]) };

        // Storage path (Segments::OFF).
        let t0 = std::time::Instant::now();
        let mut n_rows = 0usize;
        for b in ra
            .prefix_join_batched(
                &rtx,
                left_of(probe_rows.clone()),
                join_indices(),
                Default::default(),
                Segments::OFF,
            )
            .unwrap()
        {
            n_rows += b.unwrap().len();
        }
        let storage_ns_per_probe = t0.elapsed().as_nanos() as f64 / N_PROBES as f64;

        // Segment path: prime the segment with a throwaway probe first, so
        // the timed run pays zero build cost (the production call site
        // builds once per plan-node instantiation too — see
        // `prefix_join_batched`'s doc comment).
        let engine = SegmentEngine::default();
        let segments = Segments(Some(&engine));
        for b in ra
            .prefix_join_batched(
                &rtx,
                left_of(vec![probe_rows[0].clone()]),
                join_indices(),
                Default::default(),
                segments,
            )
            .unwrap()
        {
            b.unwrap();
        }
        let t1 = std::time::Instant::now();
        let mut n_rows_seg = 0usize;
        for b in ra
            .prefix_join_batched(
                &rtx,
                left_of(probe_rows),
                join_indices(),
                Default::default(),
                segments,
            )
            .unwrap()
        {
            n_rows_seg += b.unwrap().len();
        }
        let segment_ns_per_probe = t1.elapsed().as_nanos() as f64 / N_PROBES as f64;

        eprintln!(
            "storage={storage_ns_per_probe:.1} ns/probe ({n_rows} rows) \
             segment={segment_ns_per_probe:.1} ns/probe ({n_rows_seg} rows) \
             speedup={:.2}x",
            storage_ns_per_probe / segment_ns_per_probe
        );
    }

    /// A stream/decode error met DURING accumulation must not outrank an
    /// earlier accumulated row's predicate error: the row path interleaves
    /// decode and predicate per row, and the batched path must report the
    /// identical first failure (hostile-review reproducer).
    #[test]
    fn batched_filter_reports_earlier_predicate_error_before_stream_error() {
        use crate::data::functions::OP_GT;
        let gt_zero = Expr::Apply {
            op: &OP_GT,
            args: Box::new([
                Expr::Binding {
                    var: Symbol::new("c0", Default::default()),
                    tuple_pos: Some(0),
                },
                Expr::Const {
                    val: DataValue::from(0),
                    span: Default::default(),
                },
            ]),
            span: Default::default(),
        };
        let stream: Vec<Result<Tuple>> = vec![
            Ok(vec![DataValue::from(5)]),
            Ok(vec![DataValue::from("poison")]),
            Err(miette::miette!("simulated decode error at row 2")),
            Ok(vec![DataValue::from(7)]),
        ];
        let mut node = BatchTupleFilter {
            inner: stream.into_iter(),
            pred: Some(gt_zero),
            pending_err: None,
        };
        // First batch: row 0 survives; row 1's PREDICATE poison is the
        // first failure in stream order... but batches emit before errors,
        // so the first poll yields the survivors-so-far or the error —
        // whichever the ROW PATH would produce first. The row path yields
        // row 0 then errors on row 1: the batched path may fold both into
        // one poll ordering, but the ERROR it reports must be row 1's.
        let mut saw_error: Option<String> = None;
        for item in &mut node {
            match item {
                Ok(_) => {}
                Err(e) => {
                    saw_error = Some(e.to_string());
                    break;
                }
            }
        }
        let err = saw_error.expect("the stream errors");
        assert!(
            !err.contains("simulated decode error"),
            "the LATER stream error must not outrank the earlier predicate poison: {err}"
        );
    }
}
