/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): lifts query/rule syntax into program IR — FixedRuleHandle
 * declaration-only (no Arc<dyn FixedRule>), HeadAggrSlot, ValidityClause,
 * WriteValidity, InputProgram::new door.
 */

//! Lifts query / rule syntax into program IR.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{Display, Formatter};

use miette::{Diagnostic, LabeledSpan, Report, Result, bail, ensure};
use thiserror::Error;

use crate::program::aggregate::parse_aggr;
use crate::program::expr::{BindingPos, Expr};
use crate::program::query::{
    InputRelationHandle, QueryAssertion, QueryOutOptions, RelationOp, ReturnMutation, SortDir,
    WriteValidity,
};
use crate::program::rule::{
    FixedRuleApply, FixedRuleArg, FixedRuleHandle, FixedRuleOptions, HeadAggrSlot, InputAtom,
    InputInlineRule, InputInlineRulesOrFixed, InputNamedFieldRelationApplyAtom, InputProgram,
    InputRelationApplyAtom, InputRuleApplyAtom, SearchInput, Trivia, Unification, ValidityClause,
};
use crate::program::span::SourceSpan;
use crate::program::symbol::{Symbol, SymbolKind};
use crate::schema::column::{ColType, ColumnDef, NullableColType};
use crate::schema::relation::StoredRelationMetadata;
use crate::value::{AsOf, DataValue, ValidityTs};
use crate::value::validity_coerce::data_value_to_vld_spec;

use super::expr::build_expr;
use super::schema::parse_schema;
use super::{ExtractSpan, Pair, Pairs, Rule, UnexpectedRule};

#[derive(Error, Diagnostic, Debug)]
#[error("Query option {0} is not constant")]
#[diagnostic(code(parser::option_not_constant))]
struct OptionNotConstantError(&'static str, #[label] SourceSpan, #[related] [Report; 1]);

#[derive(Error, Diagnostic, Debug)]
#[error("Query option {0} requires a non-negative integer")]
#[diagnostic(code(parser::option_not_non_neg))]
struct OptionNotNonNegIntError(&'static str, #[label] SourceSpan);

#[derive(Error, Diagnostic, Debug)]
#[error("Query option {0} requires a positive integer")]
#[diagnostic(code(parser::option_not_pos))]
struct OptionNotPosIntError(&'static str, #[label] SourceSpan);

#[derive(Error, Diagnostic, Debug)]
#[error("Query option {0} requires a boolean")]
#[diagnostic(code(parser::option_not_bool))]
struct OptionNotBoolError(&'static str, #[label] SourceSpan);

#[derive(Debug)]
struct MultipleRuleDefinitionError(String, Vec<SourceSpan>);

#[derive(Debug, Error, Diagnostic)]
#[error("Multiple query output assertions defined")]
#[diagnostic(code(parser::multiple_out_assert))]
struct DuplicateQueryAssertion(#[label] SourceSpan);

impl Error for MultipleRuleDefinitionError {}

impl Display for MultipleRuleDefinitionError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The rule '{0}' cannot have multiple definitions since it contains non-Horn clauses",
            self.0
        )
    }
}

impl Diagnostic for MultipleRuleDefinitionError {
    fn code<'a>(&'a self) -> Option<Box<dyn Display + 'a>> {
        Some(Box::new("parser::multiple_rule_def"))
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        Some(Box::new(
            self.1
                .iter()
                .map(|s| LabeledSpan::new_with_span(None, *s)),
        ))
    }
}

fn merge_spans(symbs: &[Symbol]) -> SourceSpan {
    let mut fst = symbs.first().unwrap().span;
    for nxt in symbs.iter().skip(1) {
        fst = fst.merge(nxt.span);
    }
    fst
}

fn get_non_neg_int(v: &DataValue) -> Option<i64> {
    let i = v.get_int()?;
    (i >= 0).then_some(i)
}

enum StoredRelationBuild {
    NameOnly {
        name: Symbol,
        span: SourceSpan,
        op: RelationOp,
    },
    WithSchema {
        handle: InputRelationHandle,
        op: RelationOp,
    },
}

/// Lift a query_script's pairs into an [`InputProgram`].
pub(crate) fn parse_query(
    src: Pairs<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    cur_vld: ValidityTs,
) -> Result<InputProgram> {
    let mut progs: BTreeMap<Symbol, InputInlineRulesOrFixed> = Default::default();
    let mut out_opts: QueryOutOptions = Default::default();
    let mut disable_magic_rewrite = false;

    let mut stored_relation: Option<StoredRelationBuild> = None;
    let mut returning_mutation = ReturnMutation::NotReturning;

    for pair in src {
        match pair.as_rule() {
            Rule::rule => {
                let (name, rule) = parse_rule(pair, param_pool, cur_vld)?;

                match progs.entry(name) {
                    Entry::Vacant(e) => {
                        e.insert(InputInlineRulesOrFixed::Rules { rules: vec![rule] });
                    }
                    Entry::Occupied(mut e) => {
                        let key = e.key().to_string();
                        match e.get_mut() {
                            InputInlineRulesOrFixed::Rules { rules: rs } => {
                                #[derive(Debug, Error, Diagnostic)]
                                #[error("Rule {0} has multiple definitions with conflicting heads")]
                                #[diagnostic(code(parser::head_aggr_mismatch))]
                                #[diagnostic(help(
                                    "The arity of each rule head must match. In addition, any aggregation \
                                     applied must be the same."
                                ))]
                                struct RuleHeadMismatch(
                                    String,
                                    #[label] SourceSpan,
                                    #[label] SourceSpan,
                                );
                                let prev = rs.first().unwrap();
                                ensure!(prev.aggr == rule.aggr, {
                                    RuleHeadMismatch(
                                        key,
                                        merge_spans(&prev.head),
                                        merge_spans(&rule.head),
                                    )
                                });

                                rs.push(rule);
                            }
                            InputInlineRulesOrFixed::Fixed { fixed } => {
                                let fixed_span = fixed.span;
                                bail!(MultipleRuleDefinitionError(
                                    e.key().name.to_string(),
                                    vec![rule.span, fixed_span]
                                ))
                            }
                        }
                    }
                }
            }
            Rule::fixed_rule => {
                let rule_span = pair.extract_span();
                let (name, apply) = parse_fixed_rule(pair, param_pool, cur_vld)?;

                match progs.entry(name) {
                    Entry::Vacant(e) => {
                        e.insert(InputInlineRulesOrFixed::Fixed { fixed: apply });
                    }
                    Entry::Occupied(e) => {
                        let found_name = e.key().name.to_string();
                        let mut found_span = match e.get() {
                            InputInlineRulesOrFixed::Rules { rules } => {
                                rules.iter().map(|r| r.span).collect()
                            }
                            InputInlineRulesOrFixed::Fixed { fixed } => vec![fixed.span],
                        };
                        found_span.push(rule_span);
                        bail!(MultipleRuleDefinitionError(found_name, found_span));
                    }
                }
            }
            Rule::const_rule => {
                let span = pair.extract_span();
                let mut src = pair.into_inner();
                let (name, mut head, aggr) = parse_rule_head(src.next().unwrap(), param_pool)?;

                if let Some(found) = progs.get(&name) {
                    let mut found_span = match found {
                        InputInlineRulesOrFixed::Rules { rules } => {
                            rules.iter().map(|r| r.span).collect()
                        }
                        InputInlineRulesOrFixed::Fixed { fixed } => {
                            vec![fixed.span]
                        }
                    };
                    found_span.push(span);
                    bail!(MultipleRuleDefinitionError(
                        name.name.to_string(),
                        found_span
                    ));
                }

                #[derive(Debug, Error, Diagnostic)]
                #[error("Constant rules cannot have aggregation application")]
                #[diagnostic(code(parser::aggr_in_const_rule))]
                struct AggrInConstRuleError(#[label] SourceSpan);

                for (a, v) in aggr.iter().zip(head.iter()) {
                    ensure!(!a.is_aggregated(), AggrInConstRuleError(v.span));
                }
                let data_part = src.next().unwrap();
                let entry_param_head = extract_entry_param_head(data_part.clone());
                let data = build_expr(data_part, param_pool)?;
                let options = FixedRuleOptions::from_entries([(
                    Symbol::new("data", span),
                    data.clone(),
                )])?;
                let handle = FixedRuleHandle {
                    name: Symbol::new("Constant", span),
                };

                let arity = match data.clone().eval_to_const() {
                    Ok(DataValue::List(rows)) => rows
                        .first()
                        .and_then(|r| r.get_slice())
                        .map(|s| s.len())
                        .unwrap_or(0),
                    _ => head.len(),
                };

                ensure!(arity != 0 || !head.is_empty(), EmptyRowForConstRule(span));
                if !head.is_empty() {
                    ensure!(
                        arity == head.len(),
                        FixedRuleHeadArityMismatch(arity, head.len(), span)
                    );
                }
                if head.is_empty() && matches!(name.kind(), SymbolKind::Entry) {
                    if let Some(params) = entry_param_head {
                        head.extend(params);
                    }
                }
                let arity = if head.is_empty() { arity } else { head.len() };
                progs.insert(
                    name,
                    InputInlineRulesOrFixed::Fixed {
                        fixed: FixedRuleApply {
                            fixed_handle: handle,
                            rule_args: vec![],
                            options,
                            head,
                            arity,
                            span,
                            trivia: Trivia::default(),
                        },
                    },
                );
            }
            Rule::timeout_option => {
                let pair = pair.into_inner().next().unwrap();
                let span = pair.extract_span();
                let timeout = build_expr(pair, param_pool)?
                    .eval_to_const()
                    .map_err(|err| OptionNotConstantError("timeout", span, [err]))?
                    .get_float()
                    .ok_or(OptionNotNonNegIntError("timeout", span))?;
                if timeout > 0. {
                    out_opts.timeout = Some(timeout);
                } else {
                    out_opts.timeout = None;
                }
            }
            Rule::sleep_option => {
                #[cfg(target_arch = "wasm32")]
                bail!(":sleep is not supported under WASM");

                #[cfg(not(target_arch = "wasm32"))]
                {
                    let pair = pair.into_inner().next().unwrap();
                    let span = pair.extract_span();
                    let sleep = build_expr(pair, param_pool)?
                        .eval_to_const()
                        .map_err(|err| OptionNotConstantError("sleep", span, [err]))?
                        .get_float()
                        .ok_or(OptionNotNonNegIntError("sleep", span))?;
                    ensure!(sleep > 0., OptionNotPosIntError("sleep", span));
                    out_opts.sleep = Some(sleep);
                }
            }
            Rule::limit_option => {
                let pair = pair.into_inner().next().unwrap();
                let span = pair.extract_span();
                let limit = get_non_neg_int(
                    &build_expr(pair, param_pool)?
                        .eval_to_const()
                        .map_err(|err| OptionNotConstantError("limit", span, [err]))?,
                )
                .ok_or(OptionNotNonNegIntError("limit", span))?;
                out_opts.limit = Some(limit as usize);
            }
            Rule::offset_option => {
                let pair = pair.into_inner().next().unwrap();
                let span = pair.extract_span();
                let offset = get_non_neg_int(
                    &build_expr(pair, param_pool)?
                        .eval_to_const()
                        .map_err(|err| OptionNotConstantError("offset", span, [err]))?,
                )
                .ok_or(OptionNotNonNegIntError("offset", span))?;
                out_opts.offset = Some(offset as usize);
            }
            Rule::sort_option => {
                for part in pair.into_inner() {
                    let mut var = "";
                    let mut dir = SortDir::Asc;
                    let mut span = part.extract_span();
                    for a in part.into_inner() {
                        match a.as_rule() {
                            Rule::out_arg => {
                                var = a.as_str();
                                span = a.extract_span();
                            }
                            Rule::sort_asc => dir = SortDir::Asc,
                            Rule::sort_desc => dir = SortDir::Dsc,
                            _ => bail!(UnexpectedRule(a.extract_span())),
                        }
                    }
                    out_opts.sorters.push((Symbol::new(var, span), dir));
                }
            }
            Rule::returning_option => {
                returning_mutation = ReturnMutation::Returning;
            }
            Rule::relation_option => {
                let span = pair.extract_span();
                let mut args = pair.into_inner();
                let op_pair = args.next().unwrap();
                let op = match op_pair.as_rule() {
                    Rule::relation_create => RelationOp::Create,
                    Rule::relation_replace => RelationOp::Replace,
                    Rule::relation_put => RelationOp::Put,
                    Rule::relation_insert => RelationOp::Insert,
                    Rule::relation_update => RelationOp::Update,
                    Rule::relation_rm => RelationOp::Rm,
                    Rule::relation_delete => RelationOp::Delete,
                    Rule::relation_ensure => RelationOp::Ensure,
                    Rule::relation_ensure_not => RelationOp::EnsureNot,
                    _ => bail!(UnexpectedRule(op_pair.extract_span())),
                };

                let name_p = args.next().unwrap();
                let name = Symbol::new(name_p.as_str(), name_p.extract_span());
                match args.next() {
                    None => {
                        stored_relation = Some(StoredRelationBuild::NameOnly { name, span, op })
                    }
                    Some(schema_p) => {
                        let (mut metadata, mut key_bindings, mut dep_bindings) =
                            parse_schema(schema_p)?;
                        if !matches!(op, RelationOp::Create | RelationOp::Replace) {
                            key_bindings.extend(dep_bindings);
                            dep_bindings = vec![];
                            metadata.keys.extend(metadata.non_keys);
                            metadata.non_keys = vec![];
                        }
                        stored_relation = Some(StoredRelationBuild::WithSchema {
                            handle: InputRelationHandle {
                                name,
                                metadata,
                                key_bindings,
                                dep_bindings,
                                span,
                            },
                            op,
                        })
                    }
                }
            }
            Rule::assert_none_option => {
                ensure!(
                    out_opts.assertion.is_none(),
                    DuplicateQueryAssertion(pair.extract_span())
                );
                out_opts.assertion = Some(QueryAssertion::AssertNone(pair.extract_span()))
            }
            Rule::assert_some_option => {
                ensure!(
                    out_opts.assertion.is_none(),
                    DuplicateQueryAssertion(pair.extract_span())
                );
                out_opts.assertion = Some(QueryAssertion::AssertSome(pair.extract_span()))
            }
            Rule::disable_magic_rewrite_option => {
                let pair = pair.into_inner().next().unwrap();
                let span = pair.extract_span();
                let val = build_expr(pair, param_pool)?
                    .eval_to_const()
                    .map_err(|err| OptionNotConstantError("disable_magic_rewrite", span, [err]))?
                    .get_bool()
                    .ok_or(OptionNotBoolError("disable_magic_rewrite", span))?;
                disable_magic_rewrite = val;
            }
            Rule::EOI => break,
            _ => bail!(UnexpectedRule(pair.extract_span())),
        }
    }

    if progs.is_empty() {
        if let Some(StoredRelationBuild::WithSchema {
            handle:
                InputRelationHandle {
                    key_bindings,
                    dep_bindings,
                    ..
                },
            op: RelationOp::Create,
        }) = &stored_relation
        {
            let mut bindings = key_bindings.clone();
            bindings.extend_from_slice(dep_bindings);
            insert_empty_const_rule(&mut progs, &bindings);
        }
    }

    match stored_relation {
        None => {}
        Some(StoredRelationBuild::NameOnly { name, span, op }) => {
            // Need an entry to derive head — ensure Constant placeholder if empty.
            if progs.is_empty() {
                insert_empty_const_rule(&mut progs, &[]);
            }
            let mut prog = InputProgram::new(progs, out_opts, disable_magic_rewrite)?;
            let head = prog.get_entry_out_head()?;
            for symb in &head {
                symb.ensure_valid_field()?;
            }

            let metadata = StoredRelationMetadata {
                keys: head
                    .iter()
                    .map(|s| ColumnDef {
                        name: s.name.clone(),
                        typing: NullableColType::optional(ColType::Any),
                        default_gen: None,
                    })
                    .collect(),
                non_keys: vec![],
            };

            let handle = InputRelationHandle {
                name,
                metadata,
                key_bindings: head,
                dep_bindings: vec![],
                span,
            };
            prog.out_opts_mut().store_relation =
                Some((handle, op, returning_mutation, WriteValidity::Now));
            return finalize_program(prog);
        }
        Some(StoredRelationBuild::WithSchema { handle, op }) => {
            if progs.is_empty() && matches!(op, RelationOp::Create) {
                let mut bindings = handle.dep_bindings.clone();
                bindings.extend_from_slice(&handle.key_bindings);
                insert_empty_const_rule(&mut progs, &bindings);
            }
            out_opts.store_relation =
                Some((handle, op, returning_mutation, WriteValidity::Now));
        }
    }

    let prog = InputProgram::new(progs, out_opts, disable_magic_rewrite)?;
    finalize_program(prog)
}

fn finalize_program(mut prog: InputProgram) -> Result<InputProgram> {
    if !prog.out_opts().sorters.is_empty() {
        #[derive(Debug, Error, Diagnostic)]
        #[error("Sort key '{0}' not found")]
        #[diagnostic(code(parser::sort_key_not_found))]
        struct SortKeyNotFound(String, #[label] SourceSpan);

        let head_args = prog.get_entry_out_head()?;

        for (sorter, _) in &prog.out_opts().sorters {
            ensure!(
                head_args.contains(sorter),
                SortKeyNotFound(sorter.to_string(), sorter.span)
            )
        }
    }

    #[derive(Debug, Error, Diagnostic)]
    #[error("Input relation '{0}' has no keys")]
    #[diagnostic(code(parser::relation_has_no_keys))]
    struct RelationHasNoKeys(String, #[label] SourceSpan);

    let empty_mutation_head = match &prog.out_opts().store_relation {
        None => None,
        Some((handle, _, _, _)) => {
            if handle.key_bindings.is_empty() {
                if handle.dep_bindings.is_empty() {
                    Some((handle.name.to_string(), handle.span))
                } else {
                    bail!(RelationHasNoKeys(handle.name.to_string(), handle.span));
                }
            } else {
                None
            }
        }
    };

    if let Some((name, span)) = empty_mutation_head {
        let head_args = prog.get_entry_out_head()?;
        let Some((handle, _, _, _)) = prog.out_opts_mut().store_relation.as_mut() else {
            bail!(RelationHasNoKeys(name, span));
        };
        if head_args.is_empty() {
            bail!(RelationHasNoKeys(handle.name.to_string(), handle.span));
        }
        handle.key_bindings = head_args.clone();
        handle.metadata.keys = head_args
            .iter()
            .map(|s| ColumnDef {
                name: s.name.clone(),
                typing: NullableColType::optional(ColType::Any),
                default_gen: None,
            })
            .collect();
    }

    Ok(prog)
}

fn parse_rule(
    src: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    cur_vld: ValidityTs,
) -> Result<(Symbol, InputInlineRule)> {
    let span = src.extract_span();
    let mut src = src.into_inner();
    let head = src.next().unwrap();
    let head_span = head.extract_span();
    let (name, head, aggr) = parse_rule_head(head, param_pool)?;

    #[derive(Debug, Error, Diagnostic)]
    #[error("Horn-clause rule cannot have empty rule head")]
    #[diagnostic(code(parser::empty_horn_rule_head))]
    struct EmptyRuleHead(#[label] SourceSpan);

    ensure!(!head.is_empty(), EmptyRuleHead(head_span));
    let body = src.next().unwrap();
    let mut body_clauses = vec![];
    let mut ignored_counter = 0;
    for atom_src in body.into_inner() {
        body_clauses.push(parse_disjunction(
            atom_src,
            param_pool,
            cur_vld,
            &mut ignored_counter,
        )?)
    }

    Ok((
        name,
        InputInlineRule {
            head,
            aggr,
            body: body_clauses,
            span,
            trivia: Trivia::default(),
        },
    ))
}

fn parse_disjunction(
    pair: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    cur_vld: ValidityTs,
    ignored_counter: &mut u32,
) -> Result<InputAtom> {
    let span = pair.extract_span();
    let mut res = Vec::new();
    for v in pair.into_inner() {
        match v.as_rule() {
            Rule::or_op => {}
            _ => res.push(parse_atom(v, param_pool, cur_vld, ignored_counter)?),
        }
    }
    Ok(if res.len() == 1 {
        res.into_iter().next().unwrap()
    } else {
        InputAtom::Disjunction { inner: res, span }
    })
}

fn parse_atom(
    src: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    cur_vld: ValidityTs,
    ignored_counter: &mut u32,
) -> Result<InputAtom> {
    Ok(match src.as_rule() {
        Rule::rule_body => {
            let span = src.extract_span();
            let mut grouped = Vec::new();
            for v in src.into_inner() {
                grouped.push(parse_disjunction(v, param_pool, cur_vld, ignored_counter)?);
            }
            InputAtom::Conjunction {
                inner: grouped,
                span,
            }
        }
        Rule::disjunction => parse_disjunction(src, param_pool, cur_vld, ignored_counter)?,
        Rule::negation => {
            let span = src.extract_span();
            let mut src = src.into_inner();
            src.next().unwrap();
            let inner = parse_atom(src.next().unwrap(), param_pool, cur_vld, ignored_counter)?;
            InputAtom::Negation {
                inner: inner.into(),
                span,
            }
        }
        Rule::expr => {
            let expr = build_expr(src, param_pool)?;
            InputAtom::Predicate { inner: expr }
        }
        Rule::unify => {
            let span = src.extract_span();
            let mut src = src.into_inner();
            let var = src.next().unwrap();
            let mut symb = Symbol::new(var.as_str(), var.extract_span());
            if matches!(symb.kind(), SymbolKind::Ignored) {
                symb.name = format!("*^*{}", *ignored_counter).into();
                *ignored_counter += 1;
            }
            let expr = build_expr(src.next().unwrap(), param_pool)?;
            InputAtom::Unification {
                inner: Unification {
                    binding: symb,
                    expr,
                    one_many_unif: false,
                    span,
                },
            }
        }
        Rule::unify_multi => {
            let span = src.extract_span();
            let mut src = src.into_inner();
            let var = src.next().unwrap();
            let mut symb = Symbol::new(var.as_str(), var.extract_span());
            if matches!(symb.kind(), SymbolKind::Ignored) {
                symb.name = format!("*^*{}", *ignored_counter).into();
                *ignored_counter += 1;
            }
            src.next().unwrap();
            let expr = build_expr(src.next().unwrap(), param_pool)?;
            InputAtom::Unification {
                inner: Unification {
                    binding: symb,
                    expr,
                    one_many_unif: true,
                    span,
                },
            }
        }
        Rule::rule_apply => {
            let span = src.extract_span();
            let mut src = src.into_inner();
            let name = src.next().unwrap();
            let mut args = Vec::new();
            for v in src.next().unwrap().into_inner() {
                args.push(build_expr(v, param_pool)?);
            }
            InputAtom::Rule {
                inner: InputRuleApplyAtom {
                    name: Symbol::new(name.as_str(), name.extract_span()),
                    args,
                    span,
                },
            }
        }
        Rule::relation_apply => {
            let span = src.extract_span();
            let mut src = src.into_inner();
            let name = src.next().unwrap();
            let mut args = Vec::new();
            for v in src.next().unwrap().into_inner() {
                args.push(build_expr(v, param_pool)?);
            }
            let validity = match src.next() {
                None => None,
                Some(vld_clause) => {
                    let vld_expr = build_expr(vld_clause.into_inner().next().unwrap(), param_pool)?;
                    Some(ValidityClause::At(AsOf::current(expr2vld_spec(
                        vld_expr, cur_vld,
                    )?)))
                }
            };
            InputAtom::Relation {
                inner: InputRelationApplyAtom {
                    name: Symbol::new(&name.as_str()[1..], name.extract_span()),
                    args,
                    validity,
                    span,
                },
            }
        }
        Rule::search_apply => {
            let span = src.extract_span();
            let mut src = src.into_inner();
            let name_p = src.next().unwrap();
            let name_segs: Vec<&str> = name_p.as_str().split(':').collect();

            #[derive(Debug, Error, Diagnostic)]
            #[error("Search head must be of the form `relation_name:index_name`")]
            #[diagnostic(code(parser::invalid_search_head))]
            struct InvalidSearchHead(#[label] SourceSpan);

            ensure!(
                name_segs.len() == 2,
                InvalidSearchHead(name_p.extract_span())
            );
            let relation = Symbol::new(name_segs[0], name_p.extract_span());
            let index = Symbol::new(name_segs[1], name_p.extract_span());
            let mut bindings = BTreeMap::new();
            for arg in src.next().unwrap().into_inner() {
                let (k, v) = extract_named_apply_arg(arg, param_pool)?;
                bindings.insert(k, v);
            }
            let mut parameters = BTreeMap::new();
            for arg in src {
                let (k, v) = extract_named_apply_arg(arg, param_pool)?;
                parameters.insert(k, v);
            }

            InputAtom::Search {
                inner: SearchInput::from_named_parts(
                    relation,
                    index,
                    bindings,
                    parameters,
                    span,
                )
                .map_err(miette::Report::new)?,
            }
        }
        Rule::relation_named_apply => {
            let span = src.extract_span();
            let mut src = src.into_inner();
            let name_p = src.next().unwrap();
            let name = Symbol::new(&name_p.as_str()[1..], name_p.extract_span());
            let mut args = BTreeMap::new();
            for arg in src.next().unwrap().into_inner() {
                let (k, v) = extract_named_apply_arg(arg, param_pool)?;
                args.insert(k, v);
            }
            let validity = match src.next() {
                None => None,
                Some(vld_clause) => {
                    let vld_expr = build_expr(vld_clause.into_inner().next().unwrap(), param_pool)?;
                    Some(ValidityClause::At(AsOf::current(expr2vld_spec(
                        vld_expr, cur_vld,
                    )?)))
                }
            };
            InputAtom::NamedFieldRelation {
                inner: InputNamedFieldRelationApplyAtom {
                    name,
                    args,
                    validity,
                    span,
                },
            }
        }
        _ => bail!(UnexpectedRule(src.extract_span())),
    })
}

fn extract_named_apply_arg(
    pair: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
) -> Result<(Symbol, Expr)> {
    let mut inner = pair.into_inner();
    let name_p = inner.next().unwrap();
    let name = Symbol::new(name_p.as_str(), name_p.extract_span());
    let arg = match inner.next() {
        Some(a) => build_expr(a, param_pool)?,
        None => Expr::Binding {
            var: name.clone(),
            tuple_pos: BindingPos::Unresolved,
        },
    };
    Ok((name, arg))
}

fn parse_rule_head(
    src: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
) -> Result<(Symbol, Vec<Symbol>, Vec<HeadAggrSlot>)> {
    let mut src = src.into_inner();
    let name = src.next().unwrap();
    let mut args = vec![];
    let mut aggrs = vec![];
    for p in src {
        let (arg, aggr) = parse_rule_head_arg(p, param_pool)?;
        args.push(arg);
        aggrs.push(aggr);
    }
    Ok((Symbol::new(name.as_str(), name.extract_span()), args, aggrs))
}

#[derive(Error, Diagnostic, Debug)]
#[diagnostic(code(parser::aggr_not_found))]
#[error("Aggregation '{0}' not found")]
struct AggrNotFound(String, #[label] SourceSpan);

fn parse_rule_head_arg(
    src: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
) -> Result<(Symbol, HeadAggrSlot)> {
    let src = src.into_inner().next().unwrap();
    Ok(match src.as_rule() {
        Rule::var => (Symbol::new(src.as_str(), src.extract_span()), HeadAggrSlot::Plain),
        Rule::aggr_arg => {
            let mut inner = src.into_inner();
            let aggr_p = inner.next().unwrap();
            let aggr_name = aggr_p.as_str();
            let var = inner.next().unwrap();
            let mut args = Vec::new();
            for v in inner {
                args.push(build_expr(v, param_pool)?.eval_to_const()?);
            }
            let aggr = parse_aggr(aggr_name)
                .map_err(|e| miette::Report::new(e))?
                .ok_or_else(|| AggrNotFound(aggr_name.to_string(), aggr_p.extract_span()))?;
            (
                Symbol::new(var.as_str(), var.extract_span()),
                HeadAggrSlot::Aggregated { aggr, args },
            )
        }
        _ => bail!(UnexpectedRule(src.extract_span())),
    })
}

fn parse_fixed_rule(
    src: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    cur_vld: ValidityTs,
) -> Result<(Symbol, FixedRuleApply)> {
    let mut src = src.into_inner();
    let (out_symbol, head, aggr) = parse_rule_head(src.next().unwrap(), param_pool)?;

    #[derive(Debug, Error, Diagnostic)]
    #[error("fixed rule cannot be combined with aggregation")]
    #[diagnostic(code(parser::fixed_aggr_conflict))]
    struct AggrInfixedError(#[label] SourceSpan);

    #[derive(Debug, Error, Diagnostic)]
    #[error("fixed rule cannot have duplicate bindings")]
    #[diagnostic(code(parser::duplicate_bindings_for_fixed_rule))]
    struct DuplicateBindingError(#[label] SourceSpan);

    for (a, v) in aggr.iter().zip(head.iter()) {
        ensure!(!a.is_aggregated(), AggrInfixedError(v.span))
    }

    let mut seen_bindings = BTreeSet::new();
    let mut binding_gen_id = 0;

    let name_pair = src.next().unwrap();
    let fixed_name = name_pair.as_str();
    let mut rule_args: Vec<FixedRuleArg> = vec![];
    let mut options = FixedRuleOptions::empty();
    let args_list = src.next().unwrap();
    let args_list_span = args_list.extract_span();

    for nxt in args_list.into_inner() {
        match nxt.as_rule() {
            Rule::fixed_rel => {
                let inner = nxt.into_inner().next().unwrap();
                let span = inner.extract_span();
                match inner.as_rule() {
                    Rule::fixed_rule_rel => {
                        let mut els = inner.into_inner();
                        let name = els.next().unwrap();
                        let mut bindings = Vec::new();
                        for v in els {
                            let s = v.as_str();
                            if s == "_" {
                                let symb =
                                    Symbol::new(format!("*_*{binding_gen_id}"), v.extract_span());
                                binding_gen_id += 1;
                                bindings.push(symb);
                            } else {
                                if !seen_bindings.insert(s) {
                                    bail!(DuplicateBindingError(v.extract_span()))
                                }
                                bindings.push(Symbol::new(s, v.extract_span()));
                            }
                        }
                        rule_args.push(FixedRuleArg::InMem {
                            name: Symbol::new(name.as_str(), name.extract_span()),
                            bindings,
                            span,
                        })
                    }
                    Rule::fixed_relation_rel => {
                        let mut els = inner.into_inner();
                        let name = els.next().unwrap();
                        let mut bindings = vec![];
                        let mut as_of = None;
                        for v in els {
                            match v.as_rule() {
                                Rule::var => {
                                    let s = v.as_str();
                                    if s == "_" {
                                        let symb = Symbol::new(
                                            format!("*_*{binding_gen_id}"),
                                            v.extract_span(),
                                        );
                                        binding_gen_id += 1;
                                        bindings.push(symb);
                                    } else {
                                        if !seen_bindings.insert(s) {
                                            bail!(DuplicateBindingError(v.extract_span()))
                                        }
                                        bindings.push(Symbol::new(v.as_str(), v.extract_span()))
                                    }
                                }
                                Rule::validity_clause => {
                                    let vld_inner = v.into_inner().next().unwrap();
                                    let vld_expr = build_expr(vld_inner, param_pool)?;
                                    as_of = Some(AsOf::current(expr2vld_spec(vld_expr, cur_vld)?))
                                }
                                _ => bail!(UnexpectedRule(v.extract_span())),
                            }
                        }
                        rule_args.push(FixedRuleArg::Stored {
                            name: Symbol::new(
                                name.as_str().strip_prefix('*').unwrap(),
                                name.extract_span(),
                            ),
                            bindings,
                            as_of,
                            span,
                        })
                    }
                    Rule::fixed_named_relation_rel => {
                        let mut els = inner.into_inner();
                        let name = els.next().unwrap();
                        let mut bindings = BTreeMap::new();
                        let mut as_of = None;
                        for p in els {
                            match p.as_rule() {
                                Rule::fixed_named_relation_arg_pair => {
                                    let mut vs = p.into_inner();
                                    let kp = vs.next().unwrap();
                                    let k = Symbol::new(kp.as_str(), kp.extract_span());
                                    let v = match vs.next() {
                                        Some(vp) => {
                                            if !seen_bindings.insert(vp.as_str()) {
                                                bail!(DuplicateBindingError(vp.extract_span()))
                                            }
                                            Symbol::new(vp.as_str(), vp.extract_span())
                                        }
                                        None => {
                                            if !seen_bindings.insert(kp.as_str()) {
                                                bail!(DuplicateBindingError(kp.extract_span()))
                                            }
                                            k.clone()
                                        }
                                    };
                                    bindings.insert(k, v);
                                }
                                Rule::validity_clause => {
                                    let vld_inner = p.into_inner().next().unwrap();
                                    let vld_expr = build_expr(vld_inner, param_pool)?;
                                    as_of = Some(AsOf::current(expr2vld_spec(vld_expr, cur_vld)?))
                                }
                                _ => bail!(UnexpectedRule(p.extract_span())),
                            }
                        }

                        rule_args.push(FixedRuleArg::NamedStored {
                            name: Symbol::new(
                                name.as_str().strip_prefix('*').unwrap(),
                                name.extract_span(),
                            ),
                            bindings,
                            as_of,
                            span,
                        })
                    }
                    _ => bail!(UnexpectedRule(inner.extract_span())),
                }
            }
            Rule::fixed_opt_pair => {
                let mut inner = nxt.into_inner();
                let name_p = inner.next().unwrap();
                let name = Symbol::new(name_p.as_str(), name_p.extract_span());
                let val = inner.next().unwrap();
                let val = build_expr(val, param_pool)?;
                options.insert(name, val)?;
            }
            _ => bail!(UnexpectedRule(nxt.extract_span())),
        }
    }

    let fixed = FixedRuleHandle::new(fixed_name, name_pair.extract_span());
    // Declaration arity: head length is the authority when present; empty
    // head defers (arity 0) — the engine binds the live impl later.
    let arity = head.len();

    Ok((
        out_symbol,
        FixedRuleApply {
            fixed_handle: fixed,
            rule_args,
            options,
            head,
            arity,
            span: args_list_span,
            trivia: Trivia::default(),
        },
    ))
}

#[derive(Debug, Error, Diagnostic)]
#[error("Fixed rule head arity mismatch")]
#[diagnostic(code(parser::fixed_rule_head_arity_mismatch))]
#[diagnostic(help("Expected arity: {0}, number of arguments given: {1}"))]
struct FixedRuleHeadArityMismatch(usize, usize, #[label] SourceSpan);

#[derive(Debug, Error, Diagnostic)]
#[error("Encountered empty row for constant rule")]
#[diagnostic(code(parser::const_rule_empty_row))]
struct EmptyRowForConstRule(#[label] SourceSpan);

fn insert_empty_const_rule(
    progs: &mut BTreeMap<Symbol, InputInlineRulesOrFixed>,
    bindings: &[Symbol],
) {
    let entry_symbol = Symbol::prog_entry(Default::default());
    let options = FixedRuleOptions::from_entries([(
        Symbol::new("data", Default::default()),
        Expr::Const {
            val: DataValue::List(vec![]),
            span: Default::default(),
        },
    )])
    .expect("data is a declared fixed-rule option");
    progs.insert(
        entry_symbol,
        InputInlineRulesOrFixed::Fixed {
            fixed: FixedRuleApply {
                fixed_handle: FixedRuleHandle {
                    name: Symbol::new("Constant", Default::default()),
                },
                rule_args: vec![],
                options,
                head: bindings.to_vec(),
                arity: bindings.len(),
                span: Default::default(),
                trivia: Trivia::default(),
            },
        },
    );
}

/// Derive `?[] <- [[$a, $b]]` head bindings from the already-parsed data
/// expr pairs — same shape as grammar `param_list`, without re-entering pest.
fn extract_entry_param_head(data_part: Pair<'_>) -> Option<Vec<Symbol>> {
    let outer_list = sole_expr_term(data_part)?;
    if outer_list.as_rule() != Rule::list {
        return None;
    }
    let mut outer_elems: Vec<_> = outer_list.into_inner().collect();
    if outer_elems.len() != 1 {
        return None;
    }
    let inner_list = sole_expr_term(outer_elems.pop()?)?;
    if inner_list.as_rule() != Rule::list {
        return None;
    }
    let mut head = Vec::new();
    for elem in inner_list.into_inner() {
        let param = sole_expr_term(elem)?;
        if param.as_rule() != Rule::param {
            return None;
        }
        let name = param.as_str().strip_prefix('$')?;
        head.push(Symbol::new(name, Default::default()));
    }
    Some(head)
}

/// Bare expr child with no unary ops or infix chain — the param_list shape.
fn sole_expr_term(expr: Pair<'_>) -> Option<Pair<'_>> {
    if expr.as_rule() != Rule::expr {
        return None;
    }
    let mut inner = expr.into_inner();
    let first = inner.next()?;
    if matches!(first.as_rule(), Rule::minus | Rule::negate) {
        return None;
    }
    if inner.next().is_some() {
        return None;
    }
    Some(first)
}

fn expr2vld_spec(expr: Expr, cur_vld: ValidityTs) -> Result<ValidityTs> {
    let vld_span = expr.span();
    data_value_to_vld_spec(expr.eval_to_const()?, vld_span, cur_vld)
}
