/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB originals
 * (`query/logical.rs`, `query/reorder.rs`, and the role of
 * `query/compile.rs` + `query/ra.rs`, all MPL-2.0).
 *
 * This file is the session's query-side seam — the glue `runtime/db.rs`
 * needs to drive the landed relational-algebra compile/eval tiers
 * (`query/compile.rs`, `query/ra.rs`, `query/eval.rs`). It has three parts:
 *
 * 1. The **normalizer** ([`SessionNormalizer`]) — faithful ports of
 *    upstream `logical.rs` (negation normal form + DNF) and `reorder.rs`
 *    (binding-safety well-ordering), implementing the landed
 *    [`BodyNormalizer`] seam. At a later landing these re-home to
 *    `query/logical.rs` / `query/reorder.rs`; nothing about them is interim.
 *    Index-search atoms are a typed refusal until the operator tier lands
 *    ([`SearchNotLanded`]); the landed `NormalFormAtom` has no search
 *    variants yet, so the search arms of upstream reorder.rs have no
 *    descendant here (they return with the operator tier).
 * 2. The **session view** ([`SessionView`]) — the read surface the query
 *    tier consumes: catalog lookups routed store/temp, scans, and the
 *    schema and fixed-rule-input source seams the magic and eval tiers read.
 * 3. The **fixed-rule evaluation adapter** ([`SessionFixedRule`]) — the
 *    concrete `FixedRuleEval` that `query/compile.rs::bind_for_eval`'s
 *    `make_fixed` factory produces. It assembles a rule's payload (in-memory
 *    rule inputs from the epoch stores, stored-relation inputs through the
 *    session view), brands the output with the manifest arity, and shares
 *    the budget's kill flag as the rule's [`CancelFlag`]. This is what lets
 *    a query APPLY a fixed rule, including the `Constant` rule behind every
 *    `<- [[…]]` inline datum.
 *
 * (An earlier draft of this file carried an interim nested-loop interpreter
 * of eval's `RuleBody`/`FixedRuleEval` seams; the landed RA engine
 * (`ra.rs` + `eval.rs`) superseded it, so only the seams above remain.)
 *
 * Law-5 notes against the originals: logical.rs's `unreachable!` dispatch
 * (negation arms) is a typed invariant error; reorder.rs's `unreachable!`s
 * are typed invariant errors; `head_indices[k]`-style panics have no
 * descendants.
 */

//! The session's query-side seam: the normalizer, the read surface, and the
//! fixed-rule evaluation adapter that let `runtime/db.rs` drive the landed
//! relational-algebra compile and semi-naive eval tiers over live session
//! state — stored relations through the kernel transaction, temp relations
//! through the session's scratch store.

use std::collections::BTreeMap;

use itertools::Itertools;
use miette::{Diagnostic, Result, bail, miette};
use thiserror::Error;

use crate::data::expr::Expr;
use crate::data::program::{
    BodyNormalizer, InputAtom, InputNamedFieldRelationApplyAtom, InputRelationApplyAtom,
    InputRuleApplyAtom, MagicFixedRuleApply, MagicSymbol, NormalFormAtom, NormalFormInlineRule,
    NormalFormRelationApplyAtom, NormalFormRuleApplyAtom, TempSymbGen, Unification,
};
use crate::data::relation::StoredRelationMetadata;
use crate::data::span::SourceSpan;
use crate::data::symb::{Symbol, SymbolKind};
use crate::data::tuple::Tuple;
use crate::data::value::{AsOf, DataValue};
use crate::fixed_rule::{
    CancelFlag, FixedRuleOutput, FixedRulePayload, StoredInputSource, TupleIter,
};
use crate::query::eval::{Budget, FixedRuleEval};
use crate::query::levels::EpochStore;
use crate::query::magic::StoredRelationSchemaSource;
use crate::query::temp_store::RegularTempStore;
use crate::runtime::relation::{RelationHandle, get_relation};
use crate::storage::ReadTx;
use crate::storage::temp::TempTx;

// ─────────────────────────────────────────────────────────────────────────
// Errors
// ─────────────────────────────────────────────────────────────────────────

/// Ported from upstream `reorder.rs` verbatim (same code, same help).
#[derive(Diagnostic, Debug, Error)]
#[error("Encountered unsafe negation, or empty rule definition")]
#[diagnostic(code(eval::unsafe_negation))]
#[diagnostic(help(
    "Only rule applications that are partially bounded, \
     or expressions / unifications that are completely bounded, can be safely negated. \
     You may also encounter this error if your rule can never produce any rows."
))]
pub(crate) struct UnsafeNegation(#[label] pub(crate) SourceSpan);

/// Ported from upstream `reorder.rs` verbatim.
#[derive(Diagnostic, Debug, Error)]
#[error("Atom contains unbound variable, or rule contains no variable at all")]
#[diagnostic(code(eval::unbound_variable))]
pub(crate) struct UnboundVariable(#[label] pub(crate) SourceSpan);

/// SEAM(operator tier): index search atoms (`~rel:idx{…}`) resolve against
/// index manifests, which land with the HNSW/FTS/LSH operators.
#[derive(Diagnostic, Debug, Error)]
#[error("index search is not available yet: the index-operator tier has not landed")]
#[diagnostic(code(eval::search_not_landed))]
pub(crate) struct SearchNotLanded(#[label] pub(crate) SourceSpan);

/// A cross-tier invariant that construction should have made impossible.
#[derive(Debug, Error, Diagnostic)]
#[error("query compilation invariant violated: {0}")]
#[diagnostic(code(compile::invariant), help("This is a bug. Please report it."))]
struct CompileInvariantError(&'static str);

// ─────────────────────────────────────────────────────────────────────────
// The session view: what the query tier reads from a session
// ─────────────────────────────────────────────────────────────────────────

/// The read surface of one session, as the query tier consumes it: the
/// kernel transaction for stored relations, the scratch store for temp
/// relations, and name-routed catalog access over both. `Copy` by design —
/// it is two references.
pub(crate) struct SessionView<'a, T> {
    pub(crate) store: &'a T,
    pub(crate) temp: &'a TempTx,
}

impl<T> Clone for SessionView<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for SessionView<'_, T> {}

impl<'a, T: ReadTx> SessionView<'a, T> {
    /// Catalog lookup, routed by the relation-name namespace: `_`-prefixed
    /// names resolve in the session's temp catalog, everything else in the
    /// persistent catalog.
    pub(crate) fn handle(&self, name: &str) -> Result<RelationHandle> {
        if name.starts_with('_') {
            get_relation(self.temp, name)
        } else {
            get_relation(self.store, name)
        }
    }

    /// Scan every row of a relation through the routed reader, as-of
    /// `as_of` when time travel is requested.
    pub(crate) fn scan_all(&self, handle: &RelationHandle, as_of: Option<AsOf>) -> TupleIter<'a> {
        match (handle.is_temp, as_of) {
            (true, None) => handle.scan_all(self.temp),
            (true, Some(vld)) => handle.skip_scan_all(self.temp, vld),
            (false, None) => handle.scan_all(self.store),
            (false, Some(vld)) => handle.skip_scan_all(self.store, vld),
        }
    }

    /// Prefix scan through the routed reader.
    pub(crate) fn scan_prefix(
        &self,
        handle: &RelationHandle,
        prefix: &Tuple,
        as_of: Option<AsOf>,
    ) -> TupleIter<'a> {
        match (handle.is_temp, as_of) {
            (true, None) => handle.scan_prefix(self.temp, prefix),
            (true, Some(vld)) => handle.skip_scan_prefix(self.temp, prefix, vld),
            (false, None) => handle.scan_prefix(self.store, prefix),
            (false, Some(vld)) => handle.skip_scan_prefix(self.store, prefix, vld),
        }
    }
}

/// The magic tier's schema seam, served by the session view.
impl<T: ReadTx> StoredRelationSchemaSource for SessionView<'_, T> {
    fn stored_relation_schema(
        &self,
        name: &Symbol,
        _span: SourceSpan,
    ) -> Result<StoredRelationMetadata> {
        Ok(self.handle(&name.name)?.metadata)
    }
}

/// The fixed-rule payload's stored-input seam, served by the session view.
impl<T: ReadTx> StoredInputSource for SessionView<'_, T> {
    fn stored_arity(&self, name: &Symbol) -> Result<usize> {
        Ok(self.handle(&name.name)?.arity())
    }

    fn stored_scan_all<'b>(&'b self, name: &Symbol, as_of: Option<AsOf>) -> Result<TupleIter<'b>> {
        let handle = self.handle(&name.name)?;
        Ok(self.scan_all(&handle, as_of))
    }

    fn stored_scan_prefix<'b>(
        &'b self,
        name: &Symbol,
        prefix: &DataValue,
        as_of: Option<AsOf>,
    ) -> Result<TupleIter<'b>> {
        let handle = self.handle(&name.name)?;
        Ok(self.scan_prefix(&handle, &vec![prefix.clone()], as_of))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// The normalizer: NNF → DNF → well-ordering (ports of logical.rs/reorder.rs)
// ─────────────────────────────────────────────────────────────────────────

/// The session's [`BodyNormalizer`]: DNF conversion (which resolves
/// named-field relation atoms against the catalog) plus binding-safety
/// well-ordering. LANDING NOTE: re-homes to `query/logical.rs` +
/// `query/reorder.rs` when the compile tier lands; the logic is final.
pub(crate) struct SessionNormalizer<'a, T> {
    pub(crate) view: SessionView<'a, T>,
    cancel: CancelFlag,
    symb_gen: TempSymbGen,
}

impl<'a, T> SessionNormalizer<'a, T> {
    pub(crate) fn new(view: SessionView<'a, T>, cancel: CancelFlag) -> Self {
        Self {
            view,
            cancel,
            symb_gen: TempSymbGen::default(),
        }
    }
}

impl<T: ReadTx> BodyNormalizer for SessionNormalizer<'_, T> {
    fn disjunctive_normal_form(&mut self, body: InputAtom) -> Result<Vec<Vec<NormalFormAtom>>> {
        let nnf = negation_normal_form(body)?;
        let disjunction = do_disjunctive_normal_form(
            nnf,
            &mut self.symb_gen,
            &|name| self.view.handle(name).map(|h| h.metadata),
            &|name| self.view.handle(name),
            &self.cancel,
        )?;
        Ok(disjunction)
    }

    fn well_order(&mut self, rule: NormalFormInlineRule) -> Result<NormalFormInlineRule> {
        convert_to_well_ordered_rule(rule)
    }
}

/// Negation normal form: push `not` down to atoms, De Morgan through
/// conjunction/disjunction, cancel double negation. (Upstream logical.rs.)
fn negation_normal_form(atom: InputAtom) -> Result<InputAtom> {
    Ok(match atom {
        a @ (InputAtom::Rule { .. }
        | InputAtom::NamedFieldRelation { .. }
        | InputAtom::Predicate { .. }
        | InputAtom::Relation { .. }) => a,
        InputAtom::Conjunction { inner, span } => InputAtom::Conjunction {
            inner: inner.into_iter().map(negation_normal_form).try_collect()?,
            span,
        },
        InputAtom::Disjunction { inner, span } => InputAtom::Disjunction {
            inner: inner.into_iter().map(negation_normal_form).try_collect()?,
            span,
        },
        InputAtom::Unification { inner } => InputAtom::Unification { inner },
        a @ InputAtom::Search { .. } => a,
        InputAtom::Negation { inner, span } => match *inner {
            a @ (InputAtom::Rule { .. }
            | InputAtom::NamedFieldRelation { .. }
            | InputAtom::Relation { .. }) => InputAtom::Negation {
                inner: Box::new(a),
                span,
            },
            InputAtom::Predicate { inner: p } => InputAtom::Predicate {
                inner: p.negate(span),
            },
            InputAtom::Negation { inner, .. } => negation_normal_form(*inner)?,
            InputAtom::Conjunction { inner, span } => InputAtom::Disjunction {
                inner: inner
                    .into_iter()
                    .map(|a| {
                        let span = a.span();
                        negation_normal_form(InputAtom::Negation {
                            inner: Box::new(a),
                            span,
                        })
                    })
                    .try_collect()?,
                span,
            },
            InputAtom::Disjunction { inner, span } => InputAtom::Conjunction {
                inner: inner
                    .into_iter()
                    .map(|a| {
                        let span = a.span();
                        negation_normal_form(InputAtom::Negation {
                            inner: Box::new(a),
                            span,
                        })
                    })
                    .try_collect()?,
                span,
            },
            InputAtom::Unification { inner } => bail!(UnsafeNegation(inner.span)),
            InputAtom::Search { inner } => {
                bail!(crate::query::search::NegatedSearchUnsupported(inner.span))
            }
        },
    })
}

type SchemaLookup<'f> = dyn Fn(&str) -> Result<StoredRelationMetadata> + 'f;

/// Catalog lookup for search-atom resolution: the full [`RelationHandle`],
/// not just the schema (a search needs indices and manifests).
type HandleLookup<'f> = dyn Fn(&str) -> Result<crate::runtime::relation::RelationHandle> + 'f;

/// DNF conversion over an NNF atom. (Upstream logical.rs; the `unreachable!`
/// dispatch arm is a typed invariant error.)
fn do_disjunctive_normal_form(
    atom: InputAtom,
    symb_gen: &mut TempSymbGen,
    schema_of: &SchemaLookup<'_>,
    search_handle: &HandleLookup<'_>,
    cancel: &crate::fixed_rule::CancelFlag,
) -> Result<Vec<Vec<NormalFormAtom>>> {
    Ok(match atom {
        InputAtom::Disjunction { inner, .. } => {
            let mut ret = vec![];
            for arg in inner {
                ret.extend(do_disjunctive_normal_form(
                    arg,
                    symb_gen,
                    schema_of,
                    search_handle,
                    cancel,
                )?);
            }
            ret
        }
        InputAtom::Conjunction { inner, .. } => {
            let mut args = inner
                .into_iter()
                .map(|a| do_disjunctive_normal_form(a, symb_gen, schema_of, search_handle, cancel));
            let mut result = args.next().ok_or_else(|| miette!("empty conjunction"))??;
            for a in args {
                result = conjunct_disjunctions(result, a?);
            }
            result
        }
        InputAtom::Rule { inner } => normalize_rule_apply(inner, false, symb_gen),
        InputAtom::NamedFieldRelation { inner } => {
            let r = convert_named_field_relation(inner, symb_gen, schema_of)?;
            normalize_relation_apply(r, false, symb_gen)
        }
        InputAtom::Relation { inner } => normalize_relation_apply(inner, false, symb_gen),
        InputAtom::Predicate { inner: mut p } => {
            p.partial_eval()?;
            vec![vec![NormalFormAtom::Predicate(p)]]
        }
        InputAtom::Negation { inner, .. } => match *inner {
            InputAtom::Rule { inner } => normalize_rule_apply(inner, true, symb_gen),
            InputAtom::Relation { inner } => normalize_relation_apply(inner, true, symb_gen),
            InputAtom::NamedFieldRelation { inner } => {
                let r = convert_named_field_relation(inner, symb_gen, schema_of)?;
                normalize_relation_apply(r, true, symb_gen)
            }
            // NNF proved negation sits only on applications.
            _ => bail!(CompileInvariantError("negation not in normal form")),
        },
        InputAtom::Unification { inner } => vec![vec![NormalFormAtom::Unification(inner)]],
        InputAtom::Search { inner } => vec![vec![NormalFormAtom::Search(Box::new(
            crate::query::search::resolve_search(search_handle, inner, symb_gen, cancel.clone())?,
        ))]],
    })
}

/// Distribute conjunction over two disjunctions (De Morgan direction that
/// keeps DNF flat). Upstream `conjunctive_to_disjunctive_de_morgen`.
fn conjunct_disjunctions(
    left: Vec<Vec<NormalFormAtom>>,
    right: Vec<Vec<NormalFormAtom>>,
) -> Vec<Vec<NormalFormAtom>> {
    let mut ret = Vec::with_capacity(left.len() * right.len());
    for l in &left {
        for r in &right {
            let mut current = l.clone();
            current.extend_from_slice(r);
            ret.push(current);
        }
    }
    ret
}

/// Resolve a named-field relation atom (`*rel{field: expr, …}`) to a
/// positional one against the declared schema; unnamed columns get fresh
/// ignored bindings. (Upstream logical.rs.)
fn convert_named_field_relation(
    InputNamedFieldRelationApplyAtom {
        name,
        mut args,
        as_of,
        span,
    }: InputNamedFieldRelationApplyAtom,
    symb_gen: &mut TempSymbGen,
    schema_of: &SchemaLookup<'_>,
) -> Result<InputRelationApplyAtom> {
    use crate::query::magic::NamedFieldNotFound;
    let metadata = schema_of(&name.name)?;
    let fields: std::collections::BTreeSet<_> = metadata
        .keys
        .iter()
        .chain(metadata.non_keys.iter())
        .map(|col| &col.name)
        .collect();
    for k in args.keys() {
        if !fields.contains(k) {
            bail!(NamedFieldNotFound(name.to_string(), k.to_string(), span));
        }
    }
    let mut new_args = vec![];
    for col_def in metadata.keys.iter().chain(metadata.non_keys.iter()) {
        let arg = args.remove(&col_def.name).unwrap_or_else(|| Expr::Binding {
            var: symb_gen.next_ignored(span),
            tuple_pos: None,
        });
        new_args.push(arg);
    }
    Ok(InputRelationApplyAtom {
        name,
        args: new_args,
        as_of,
        span,
    })
}

/// Shared shape of upstream's two `normalize` impls: expression arguments
/// become fresh bindings plus unifications; repeated variables become fresh
/// bindings plus equality unifications; ignored bindings become fresh
/// generated-ignored names.
fn normalize_args(
    args: Vec<Expr>,
    symb_gen: &mut TempSymbGen,
) -> (Vec<Symbol>, Vec<NormalFormAtom>) {
    let mut unifs = Vec::new();
    let mut out_args = Vec::with_capacity(args.len());
    let mut seen_variables = std::collections::BTreeSet::new();
    for arg in args {
        match arg {
            Expr::Binding { var, .. } => {
                if matches!(
                    var.kind(),
                    SymbolKind::Ignored | SymbolKind::GeneratedIgnored
                ) {
                    out_args.push(symb_gen.next_ignored(var.span));
                } else if seen_variables.insert(var.clone()) {
                    out_args.push(var);
                } else {
                    let dup = symb_gen.next(var.span);
                    unifs.push(NormalFormAtom::Unification(Unification {
                        binding: dup.clone(),
                        expr: Expr::Binding {
                            var,
                            tuple_pos: None,
                        },
                        one_many_unif: false,
                        span: dup.span,
                    }));
                    out_args.push(dup);
                }
            }
            expr => {
                let span = expr.span();
                let kw = symb_gen.next(span);
                out_args.push(kw.clone());
                unifs.push(NormalFormAtom::Unification(Unification {
                    binding: kw,
                    expr,
                    one_many_unif: false,
                    span,
                }));
            }
        }
    }
    (out_args, unifs)
}

fn normalize_rule_apply(
    atom: InputRuleApplyAtom,
    is_negated: bool,
    symb_gen: &mut TempSymbGen,
) -> Vec<Vec<NormalFormAtom>> {
    let (args, mut ret) = normalize_args(atom.args, symb_gen);
    let apply = NormalFormRuleApplyAtom {
        name: atom.name,
        args,
        span: atom.span,
    };
    ret.push(if is_negated {
        NormalFormAtom::NegatedRule(apply)
    } else {
        NormalFormAtom::Rule(apply)
    });
    vec![ret]
}

fn normalize_relation_apply(
    atom: InputRelationApplyAtom,
    is_negated: bool,
    symb_gen: &mut TempSymbGen,
) -> Vec<Vec<NormalFormAtom>> {
    let (args, mut ret) = normalize_args(atom.args, symb_gen);
    let apply = NormalFormRelationApplyAtom {
        name: atom.name,
        args,
        as_of: atom.as_of,
        span: atom.span,
    };
    ret.push(if is_negated {
        NormalFormAtom::NegatedRelation(apply)
    } else {
        NormalFormAtom::Relation(apply)
    });
    vec![ret]
}

/// Binding-safety well-ordering: positive applications bind, then pending
/// negations/predicates/unifications are inserted as soon as their inputs
/// are bound; anything still pending at the end is refused. Faithful port
/// of upstream `reorder.rs` (its `unreachable!`s are typed invariants).
fn convert_to_well_ordered_rule(rule: NormalFormInlineRule) -> Result<NormalFormInlineRule> {
    let mut seen_variables = std::collections::BTreeSet::new();
    let mut round_1_collected = vec![];
    let mut pending = vec![];

    for atom in rule.body {
        match atom {
            NormalFormAtom::Unification(u) => {
                if u.is_const() {
                    seen_variables.insert(u.binding.clone());
                    round_1_collected.push(NormalFormAtom::Unification(u));
                } else {
                    let unif_vars = u.bindings_in_expr()?;
                    if unif_vars.is_subset(&seen_variables) {
                        seen_variables.insert(u.binding.clone());
                        round_1_collected.push(NormalFormAtom::Unification(u));
                    } else {
                        pending.push(NormalFormAtom::Unification(u));
                    }
                }
            }
            NormalFormAtom::Rule(r) => {
                seen_variables.extend(r.args.iter().cloned());
                round_1_collected.push(NormalFormAtom::Rule(r));
            }
            NormalFormAtom::Relation(v) => {
                seen_variables.extend(v.args.iter().cloned());
                round_1_collected.push(NormalFormAtom::Relation(v));
            }
            NormalFormAtom::Search(sa) => {
                let mut needed = std::collections::BTreeSet::new();
                sa.query.collect_bindings(&mut needed)?;
                if needed.is_subset(&seen_variables) {
                    seen_variables.extend(sa.own_bindings.iter().cloned());
                    round_1_collected.push(NormalFormAtom::Search(sa));
                } else {
                    pending.push(NormalFormAtom::Search(sa));
                }
            }
            a @ (NormalFormAtom::NegatedRule(_)
            | NormalFormAtom::NegatedRelation(_)
            | NormalFormAtom::Predicate(_)) => pending.push(a),
        }
    }

    let mut collected = vec![];
    seen_variables.clear();
    let mut last_pending = vec![];
    for atom in round_1_collected {
        std::mem::swap(&mut last_pending, &mut pending);
        pending.clear();
        match atom {
            NormalFormAtom::Rule(r) => {
                seen_variables.extend(r.args.iter().cloned());
                collected.push(NormalFormAtom::Rule(r));
            }
            NormalFormAtom::Relation(v) => {
                seen_variables.extend(v.args.iter().cloned());
                collected.push(NormalFormAtom::Relation(v));
            }
            NormalFormAtom::Unification(u) => {
                seen_variables.insert(u.binding.clone());
                collected.push(NormalFormAtom::Unification(u));
            }
            NormalFormAtom::Search(sa) => {
                seen_variables.extend(sa.own_bindings.iter().cloned());
                collected.push(NormalFormAtom::Search(sa));
            }
            NormalFormAtom::NegatedRule(_)
            | NormalFormAtom::NegatedRelation(_)
            | NormalFormAtom::Predicate(_) => {
                bail!(CompileInvariantError(
                    "round-1 collection admitted a non-binding atom"
                ))
            }
        }
        for atom in last_pending.iter() {
            match atom {
                NormalFormAtom::Rule(_) | NormalFormAtom::Relation(_) => {
                    bail!(CompileInvariantError(
                        "a positive application was left pending"
                    ))
                }
                NormalFormAtom::NegatedRule(r) => {
                    if r.args.iter().all(|a| seen_variables.contains(a)) {
                        collected.push(NormalFormAtom::NegatedRule(r.clone()));
                    } else {
                        pending.push(NormalFormAtom::NegatedRule(r.clone()));
                    }
                }
                NormalFormAtom::NegatedRelation(v) => {
                    if v.args.iter().all(|a| seen_variables.contains(a)) {
                        collected.push(NormalFormAtom::NegatedRelation(v.clone()));
                    } else {
                        pending.push(NormalFormAtom::NegatedRelation(v.clone()));
                    }
                }
                NormalFormAtom::Predicate(p) => {
                    if p.bindings()?.is_subset(&seen_variables) {
                        collected.push(NormalFormAtom::Predicate(p.clone()));
                    } else {
                        pending.push(NormalFormAtom::Predicate(p.clone()));
                    }
                }
                NormalFormAtom::Unification(u) => {
                    if u.bindings_in_expr()?.is_subset(&seen_variables) {
                        collected.push(NormalFormAtom::Unification(u.clone()));
                    } else {
                        pending.push(NormalFormAtom::Unification(u.clone()));
                    }
                }
                NormalFormAtom::Search(sa) => {
                    let mut needed = std::collections::BTreeSet::new();
                    sa.query.collect_bindings(&mut needed)?;
                    if needed.is_subset(&seen_variables) {
                        seen_variables.extend(sa.own_bindings.iter().cloned());
                        collected.push(NormalFormAtom::Search(sa.clone()));
                    } else {
                        pending.push(NormalFormAtom::Search(sa.clone()));
                    }
                }
            }
        }
    }

    if !pending.is_empty() {
        for atom in pending {
            match atom {
                NormalFormAtom::Rule(_) | NormalFormAtom::Relation(_) => {
                    bail!(CompileInvariantError(
                        "a positive application was left pending"
                    ))
                }
                NormalFormAtom::NegatedRule(r) => {
                    if r.args.iter().any(|a| seen_variables.contains(a)) {
                        collected.push(NormalFormAtom::NegatedRule(r.clone()));
                    } else {
                        bail!(UnsafeNegation(r.span));
                    }
                }
                NormalFormAtom::NegatedRelation(v) => {
                    if v.args.iter().any(|a| seen_variables.contains(a)) {
                        collected.push(NormalFormAtom::NegatedRelation(v.clone()));
                    } else {
                        bail!(UnsafeNegation(v.span));
                    }
                }
                NormalFormAtom::Predicate(p) => bail!(UnboundVariable(p.span())),
                NormalFormAtom::Unification(u) => bail!(UnboundVariable(u.span)),
                NormalFormAtom::Search(sa) => bail!(UnboundVariable(sa.span)),
            }
        }
    }

    Ok(NormalFormInlineRule {
        head: rule.head,
        aggr: rule.aggr,
        body: collected,
    })
}

// ─────────────────────────────────────────────────────────────────────────
// The fixed-rule evaluation adapter
// ─────────────────────────────────────────────────────────────────────────

/// Bridges one `MagicFixedRuleApply` to `FixedRule::run` at evaluation time.
/// It assembles the payload (in-memory rule inputs from the epoch stores,
/// stored-relation inputs through the session view), brands the output store
/// with the manifest arity (never a caller-supplied one), and shares the
/// budget's kill flag as the rule's [`CancelFlag`] so a cancelled query stops
/// the rule too. This is the concrete `F` that `bind_for_eval`'s `make_fixed`
/// factory produces — the seam that lets a stored/derived query APPLY a fixed
/// rule (including the `Constant` rule behind every `<- [[…]]` inline datum).
pub(crate) struct SessionFixedRule<'a, T> {
    apply: &'a MagicFixedRuleApply,
    view: SessionView<'a, T>,
    cancel: CancelFlag,
}

impl<'a, T: ReadTx> SessionFixedRule<'a, T> {
    pub(crate) fn new(
        apply: &'a MagicFixedRuleApply,
        view: SessionView<'a, T>,
        cancel: CancelFlag,
    ) -> Self {
        Self {
            apply,
            view,
            cancel,
        }
    }
}

impl<T: ReadTx> FixedRuleEval for SessionFixedRule<'_, T> {
    fn run(
        &self,
        stores: &BTreeMap<MagicSymbol, EpochStore>,
        out: &mut RegularTempStore,
        budget: &Budget,
        baseline: u64,
    ) -> Result<()> {
        let payload = FixedRulePayload {
            manifest: self.apply,
            stores,
            stored: &self.view,
        };
        // Armed with the query's derived-tuple ceiling and the true global
        // admitted total as of this stratum's epoch-0 barrier, so a
        // row-amplifying algorithm refuses mid-run — counting every prior
        // admission, not just this writer's own rows — instead of
        // materializing unbounded output.
        let mut output = FixedRuleOutput::new_budgeted(
            self.apply.arity,
            self.apply.span,
            baseline,
            budget.derived_tuple_ceiling(),
        );
        self.apply
            .fixed_impl
            .clone()
            .run(payload, &mut output, self.cancel.clone())?;
        // Replace eval's fresh epoch-0 store with the branded output wholesale.
        *out = output.into_store();
        Ok(())
    }
}
