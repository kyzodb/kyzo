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
 * Pure NNF → DNF → binding-safety well-ordering. Faithful ports of upstream
 * `logical.rs` and `reorder.rs`. Index-search atoms resolve through
 * `exec/plan/search.rs`; named-field errors live in `exec/plan/magic.rs`.
 * Session read surface and fixed-rule adapter live elsewhere
 * (`session/db.rs`, `rules/contract.rs`).
 *
 * Law-5 notes against the originals: logical.rs's `unreachable!` dispatch
 * (negation arms) is a typed invariant error; reorder.rs's `unreachable!`s
 * are typed invariant errors; `head_indices[k]`-style panics have no
 * descendants.
 */

//! Negation normal form, disjunctive normal form, and binding-safety
//! well-ordering for rule bodies — pure plan-tier machinery with no
//! session types.

use itertools::Itertools;
use miette::{Diagnostic, Result, bail, miette};
use thiserror::Error;

use crate::exec::plan::program::{
    NormalFormAtom, NormalFormInlineRule, NormalFormRelationApplyAtom, NormalFormRuleApplyAtom,
};
use kyzo_model::SourceSpan;
use kyzo_model::program::expr::{BindingPos, Expr};
use kyzo_model::program::rule::{
    InputAtom, InputNamedFieldRelationApplyAtom, InputRelationApplyAtom, InputRuleApplyAtom,
    TempSymbGen, Unification, ValidityClause,
};
use kyzo_model::program::symbol::{Symbol, SymbolKind};
use kyzo_model::schema::StoredRelationMetadata;

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

/// A cross-tier invariant that construction should have made impossible.
#[derive(Debug, Error, Diagnostic)]
#[error("query compilation invariant violated: {0}")]
#[diagnostic(code(compile::invariant), help("This is a bug. Please report it."))]
struct CompileInvariantError(&'static str);

// ─────────────────────────────────────────────────────────────────────────
// NNF → DNF → well-ordering
// ─────────────────────────────────────────────────────────────────────────

/// Negation normal form: push `not` down to atoms, De Morgan through
/// conjunction/disjunction, cancel double negation. (Upstream logical.rs.)
pub(crate) fn negation_normal_form(atom: InputAtom) -> Result<InputAtom> {
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
                bail!(crate::exec::plan::search::NegatedSearchUnsupported(
                    inner.span
                ))
            }
        },
    })
}

pub(crate) type SchemaLookup<'f> = dyn Fn(&str) -> Result<StoredRelationMetadata> + 'f;

/// Catalog lookup for search-atom resolution: the full [`RelationHandle`],
/// not just the schema (a search needs indices and manifests).
///
/// `RelationHandle` lives in `session::catalog` — search resolution needs
/// the live catalog handle, so this pure plan file carries that one session
/// type in the lookup closure signature.
pub(crate) type HandleLookup<'f> =
    dyn Fn(&str) -> Result<crate::session::catalog::RelationHandle> + 'f;

/// DNF conversion over an NNF atom. (Upstream logical.rs; the `unreachable!`
/// dispatch arm is a typed invariant error.)
pub(crate) fn do_disjunctive_normal_form(
    atom: InputAtom,
    symb_gen: &mut TempSymbGen,
    schema_of: &SchemaLookup<'_>,
    search_handle: &HandleLookup<'_>,
    cancel: &crate::rules::contract::CancelFlag,
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
            InputAtom::Predicate { .. }
            | InputAtom::Negation { .. }
            | InputAtom::Conjunction { .. }
            | InputAtom::Disjunction { .. }
            | InputAtom::Unification { .. }
            | InputAtom::Search { .. } => {
                bail!(CompileInvariantError("negation not in normal form"))
            }
        },
        InputAtom::Unification { inner } => vec![vec![NormalFormAtom::Unification(inner)]],
        InputAtom::Search { inner } => vec![vec![NormalFormAtom::Search(Box::new(
            crate::exec::plan::search::resolve_search(
                search_handle,
                inner,
                symb_gen,
                cancel.clone(),
            )?,
        ))]],
    })
}

/// Distribute conjunction over two disjunctions (De Morgan direction that
/// keeps DNF flat). Upstream `conjunctive_to_disjunctive_de_morgen`.
pub(crate) fn conjunct_disjunctions(
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
pub(crate) fn convert_named_field_relation(
    InputNamedFieldRelationApplyAtom {
        name,
        mut args,
        validity,
        span,
    }: InputNamedFieldRelationApplyAtom,
    symb_gen: &mut TempSymbGen,
    schema_of: &SchemaLookup<'_>,
) -> Result<InputRelationApplyAtom> {
    use crate::exec::plan::magic::NamedFieldNotFound;
    let metadata = schema_of(&name.name)?;
    let fields: std::collections::BTreeSet<_> = metadata
        .keys
        .iter()
        .chain(metadata.non_keys.iter())
        .map(|col| &col.name)
        .collect();
    for k in args.keys() {
        if !fields.contains(&k.name) {
            bail!(NamedFieldNotFound(name.clone(), k.clone(), span));
        }
    }
    let mut new_args = vec![];
    for col_def in metadata.keys.iter().chain(metadata.non_keys.iter()) {
        let arg = args
            .remove(&Symbol::new(col_def.name.clone(), span))
            .unwrap_or_else(|| Expr::Binding {
                var: symb_gen.next_ignored(span),
                tuple_pos: BindingPos::Unresolved,
            });
        new_args.push(arg);
    }
    Ok(InputRelationApplyAtom {
        name,
        args: new_args,
        validity,
        span,
    })
}

/// Shared shape of upstream's two `normalize` impls: expression arguments
/// become fresh bindings plus unifications; repeated variables become fresh
/// bindings plus equality unifications; ignored bindings become fresh
/// generated-ignored names.
pub(crate) fn normalize_args(
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
                            tuple_pos: BindingPos::Unresolved,
                        },
                        one_many_unif: false,
                        span: dup.span,
                    }));
                    out_args.push(dup);
                }
            }
            expr @ Expr::Const { .. }
            | expr @ Expr::Apply { .. }
            | expr @ Expr::UnboundApply { .. }
            | expr @ Expr::Cond { .. }
            | expr @ Expr::Lazy { .. } => {
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

pub(crate) fn normalize_rule_apply(
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

pub(crate) fn normalize_relation_apply(
    atom: InputRelationApplyAtom,
    is_negated: bool,
    symb_gen: &mut TempSymbGen,
) -> Vec<Vec<NormalFormAtom>> {
    let (args, mut ret) = normalize_args(atom.args, symb_gen);
    let apply = NormalFormRelationApplyAtom {
        name: atom.name,
        args,
        validity: atom.validity,
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
pub(crate) fn convert_to_well_ordered_rule(
    rule: NormalFormInlineRule,
) -> Result<NormalFormInlineRule> {
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
                if let Some(extra) = v.validity.as_ref().and_then(ValidityClause::extra_var) {
                    seen_variables.insert(extra.clone());
                }
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
                if let Some(extra) = v.validity.as_ref().and_then(ValidityClause::extra_var) {
                    seen_variables.insert(extra.clone());
                }
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
