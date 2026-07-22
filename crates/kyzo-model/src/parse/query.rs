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

use miette::{Diagnostic, LabeledSpan, Report, Result, bail, ensure, miette};
use thiserror::Error;

use crate::program::aggregate::parse_aggr;
use crate::program::expr::{BindingPos, Expr};
use crate::program::op::OP_LIST;
use crate::program::query::{
    InputRelationHandle, QueryAssertion, QueryOutOptions, RelationOp, ReturnMutation, SortDir,
    WriteValidity,
};
use crate::program::rule::{
    DeltaAxis, FixedRuleApply, FixedRuleArg, FixedRuleHandle, FixedRuleOptions, HeadAggrSlot,
    InputAtom, InputInlineRule, InputInlineRulesOrFixed, InputNamedFieldRelationApplyAtom,
    InputProgram, InputRelationApplyAtom, InputRuleApplyAtom, SearchInput, Trivia, Unification,
    ValidityClause,
};
use crate::program::span::SourceSpan;
use crate::program::symbol::{Symbol, SymbolKind};
use crate::schema::column::{ColType, ColumnDef, NullableColType};
use crate::schema::relation::StoredRelationMetadata;
use crate::value::validity_coerce::data_value_to_vld_spec;
use crate::value::{AsOf, DataValue, MAX_VALIDITY_TS, ValidityTs};

use super::expr::build_expr;
use super::schema::parse_schema;
use super::{ExtractSpan, IntoChildren, Pair, Pairs, Rule, UnexpectedRule};

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

/// The write side's `@` clause cannot set the system coordinate: system
/// time is always the committing transaction's own engine-minted stamp.
#[derive(Debug, Error, Diagnostic)]
#[error(
    "a write's `@` clause takes exactly one coordinate (the valid instant); system time is never script-settable"
)]
#[diagnostic(code(parser::write_validity_sets_system))]
struct WriteValiditySetsSystemTime(#[label] SourceSpan);

/// `:ensure`/`:ensure_not` only read current state; they perform no
/// bitemporal write, so a `@` clause on them would silently do nothing.
#[derive(Debug, Error, Diagnostic)]
#[error("`@` has no effect on `{0}`, which checks current state and writes nothing")]
#[diagnostic(code(parser::write_validity_on_non_write_op))]
struct WriteValidityOnNonWriteOp(&'static str, #[label] SourceSpan);

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
            self.1.iter().map(|s| LabeledSpan::new_with_span(None, *s)),
        ))
    }
}

fn merge_spans(symbs: &[Symbol], fallback: SourceSpan) -> SourceSpan {
    let Some((first, rest)) = symbs.split_first() else {
        return fallback;
    };
    let mut fst = first.span;
    for nxt in rest {
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
        write_vld: WriteValidity,
    },
    WithSchema {
        handle: InputRelationHandle,
        op: RelationOp,
        write_vld: WriteValidity,
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
                                let Some(prev) = rs.first() else {
                                    bail!(UnexpectedRule(rule.span));
                                };
                                ensure!(prev.aggr == rule.aggr, {
                                    RuleHeadMismatch(
                                        key,
                                        merge_spans(&prev.head, rule.span),
                                        merge_spans(&rule.head, rule.span),
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
                let mut src = pair.children();
                let (name, mut head, aggr) = parse_rule_head(src.need("a child")?, param_pool)?;

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
                let data_part = src.need("a child")?;
                let entry_param_head = extract_entry_param_head(data_part.clone());
                let data = build_expr(data_part, param_pool)?;
                let options =
                    FixedRuleOptions::from_entries([(Symbol::new("data", span), data.clone())])?;
                let handle = FixedRuleHandle {
                    name: Symbol::new("Constant", span),
                };

                // Model `eval_to_const` does not fold deterministic Applies
                // (`OP_LIST` stays Apply until the engine apply door). Const
                // rules with empty heads (`start[] <- [['FRA']]`) must still
                // learn arity from the list-of-lists shape at parse time.
                let arity = match const_rule_data_arity(&data) {
        Some(a) => a,
        None => head.len(),
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
                let pair = pair.children().need("a child")?;
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
                    let pair = pair.children().need("a child")?;
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
                let pair = pair.children().need("a child")?;
                let span = pair.extract_span();
                let limit = get_non_neg_int(
                    &build_expr(pair, param_pool)?
                        .eval_to_const()
                        .map_err(|err| OptionNotConstantError("limit", span, [err]))?,
                )
                .ok_or(OptionNotNonNegIntError("limit", span))?;
                out_opts.limit = Some(
                    usize::try_from(limit).map_err(|_| OptionNotNonNegIntError("limit", span))?,
                );
            }
            Rule::offset_option => {
                let pair = pair.children().need("a child")?;
                let span = pair.extract_span();
                let offset = get_non_neg_int(
                    &build_expr(pair, param_pool)?
                        .eval_to_const()
                        .map_err(|err| OptionNotConstantError("offset", span, [err]))?,
                )
                .ok_or(OptionNotNonNegIntError("offset", span))?;
                out_opts.offset = Some(
                    usize::try_from(offset).map_err(|_| OptionNotNonNegIntError("offset", span))?,
                );
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
                            _other => bail!(UnexpectedRule(a.extract_span())),
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
                let mut args = pair.children();
                let op_pair = args.need("a child")?;
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
                    _other => bail!(UnexpectedRule(op_pair.extract_span())),
                };

                let name_p = args.need("a child")?;
                let name = Symbol::new(name_p.as_str(), name_p.extract_span());
                let schema_or_vld = args.next();
                let (schema_p, vld_p) = match schema_or_vld {
                    None => (None, None),
                    Some(p) if p.as_rule() == Rule::validity_clause => (None, Some(p)),
                    Some(p) => {
                        let vld = args.next();
                        (Some(p), vld)
                    }
                };
                let write_vld = match vld_p {
                    None => WriteValidity::Now,
                    Some(p) => {
                        ensure!(
                            p.as_rule() == Rule::validity_clause,
                            UnexpectedRule(p.extract_span())
                        );
                        let span = p.extract_span();
                        if let RelationOp::Ensure | RelationOp::EnsureNot = op {
                            let name = if op == RelationOp::Ensure {
                                ":ensure"
                            } else {
                                ":ensure_not"
                            };
                            bail!(WriteValidityOnNonWriteOp(name, span));
                        }
                        let mut coords = p.children();
                        let vld_expr = build_expr(coords.need("a child")?, param_pool)?;
                        if coords.next().is_some() {
                            bail!(WriteValiditySetsSystemTime(span));
                        }
                        match vld_expr.clone().eval_to_const() {
                            Ok(val) => {
                                let span = vld_expr.span();
                                let vld = data_value_to_vld_spec(val, span, cur_vld)?;
                                let vld = ValidityTs::for_assertion(vld.raw()).ok_or_else(|| {
                                    miette::miette!(
                                        labels = vec![miette::LabeledSpan::underline(span)],
                                        "a write validity cannot be the reserved terminal tick (i64::MAX / 'END')"
                                    )
                                })?;
                                WriteValidity::Fixed(vld)
                            }
                            Err(_) => WriteValidity::PerRow(vld_expr),
                        }
                    }
                };
                match schema_p {
                    None => {
                        stored_relation = Some(StoredRelationBuild::NameOnly {
                            name,
                            span,
                            op,
                            write_vld,
                        })
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
                            write_vld,
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
                let pair = pair.children().need("a child")?;
                let span = pair.extract_span();
                let val = build_expr(pair, param_pool)?
                    .eval_to_const()
                    .map_err(|err| OptionNotConstantError("disable_magic_rewrite", span, [err]))?
                    .get_bool()
                    .ok_or(OptionNotBoolError("disable_magic_rewrite", span))?;
                disable_magic_rewrite = val;
            }
            Rule::EOI => break,
            _other => bail!(UnexpectedRule(pair.extract_span())),
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
            ..
        }) = &stored_relation
        {
            let mut bindings = key_bindings.clone();
            bindings.extend_from_slice(dep_bindings);
            insert_empty_const_rule(&mut progs, &bindings)?;
        }
    }

    match stored_relation {
        None => {}
        Some(StoredRelationBuild::NameOnly {
            name,
            span,
            op,
            write_vld,
        }) => {
            // Need an entry to derive head — ensure Constant placeholder if empty.
            if progs.is_empty() {
                insert_empty_const_rule(&mut progs, &[])?;
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
            prog.out_opts_mut().store_relation = Some((handle, op, returning_mutation, write_vld));
            return finalize_program(prog);
        }
        Some(StoredRelationBuild::WithSchema {
            handle,
            op,
            write_vld,
        }) => {
            if progs.is_empty() && matches!(op, RelationOp::Create) {
                let mut bindings = handle.dep_bindings.clone();
                bindings.extend_from_slice(&handle.key_bindings);
                insert_empty_const_rule(&mut progs, &bindings)?;
            }
            out_opts.store_relation = Some((handle, op, returning_mutation, write_vld));
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

    resolve_per_row_write_validity(&mut prog)?;
    Ok(prog)
}

/// Bind a `WriteValidity::PerRow` expression against the mutation's entry
/// head: `@ ts` names an output column, resolved to a tuple index so
/// `resolve_write_validity` can read it per row.
fn resolve_per_row_write_validity(prog: &mut InputProgram) -> Result<()> {
    let Some((handle, op, ret, write_vld)) = prog.out_opts_mut().store_relation.take() else {
        return Ok(());
    };
    let write_vld = match write_vld {
        WriteValidity::PerRow(mut expr) => {
            let head = prog.get_entry_out_head()?;
            let frame: BTreeMap<Symbol, usize> = head
                .iter()
                .enumerate()
                .map(|(i, s)| (s.clone(), i))
                .collect();
            expr.fill_binding_indices(&frame)?;
            WriteValidity::PerRow(expr)
        }
        other => other,
    };
    prog.out_opts_mut().store_relation = Some((handle, op, ret, write_vld));
    Ok(())
}

fn parse_rule(
    src: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    cur_vld: ValidityTs,
) -> Result<(Symbol, InputInlineRule)> {
    let span = src.extract_span();
    let mut src = src.children();
    let head = src.need("a child")?;
    let head_span = head.extract_span();
    let (name, head, aggr) = parse_rule_head(head, param_pool)?;

    #[derive(Debug, Error, Diagnostic)]
    #[error("Horn-clause rule cannot have empty rule head")]
    #[diagnostic(code(parser::empty_horn_rule_head))]
    struct EmptyRuleHead(#[label] SourceSpan);

    ensure!(!head.is_empty(), EmptyRuleHead(head_span));
    let body = src.need("a child")?;
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
            _other => res.push(parse_atom(v, param_pool, cur_vld, ignored_counter)?),
        }
    }
    Ok(if res.len() == 1 {
        res.into_iter()
            .next()
            .ok_or_else(|| UnexpectedRule(span))?
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
            let mut src = src.children();
            src.need("a child")?;
            let inner = parse_atom(src.need("a child")?, param_pool, cur_vld, ignored_counter)?;
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
            let mut src = src.children();
            let var = src.need("a child")?;
            let mut symb = Symbol::new(var.as_str(), var.extract_span());
            if matches!(symb.kind(), SymbolKind::Ignored) {
                symb.name = format!("*^*{}", *ignored_counter).into();
                *ignored_counter += 1;
            }
            let expr = build_expr(src.need("a child")?, param_pool)?;
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
            let mut src = src.children();
            let var = src.need("a child")?;
            let mut symb = Symbol::new(var.as_str(), var.extract_span());
            if matches!(symb.kind(), SymbolKind::Ignored) {
                symb.name = format!("*^*{}", *ignored_counter).into();
                *ignored_counter += 1;
            }
            src.need("a child")?;
            let expr = build_expr(src.need("a child")?, param_pool)?;
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
            let mut src = src.children();
            let name = src.need("a child")?;
            let mut args = Vec::new();
            for v in src.need("a child")?.into_inner() {
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
            let mut src = src.children();
            let name = src.need("a child")?;
            let mut args = Vec::new();
            for v in src.need("a child")?.into_inner() {
                args.push(build_expr(v, param_pool)?);
            }
            let validity =
                parse_read_validity_clause(src.next(), param_pool, cur_vld, ignored_counter)?;
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
            let mut src = src.children();
            let name_p = src.need("a child")?;
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
            for arg in src.need("a child")?.into_inner() {
                let (k, v) = extract_named_apply_arg(arg, param_pool)?;
                bindings.insert(k, v);
            }
            let mut parameters = BTreeMap::new();
            for arg in src {
                let (k, v) = extract_named_apply_arg(arg, param_pool)?;
                parameters.insert(k, v);
            }

            InputAtom::Search {
                inner: SearchInput::from_named_parts(relation, index, bindings, parameters, span)
                    .map_err(miette::Report::new)?,
            }
        }
        Rule::relation_named_apply => {
            let span = src.extract_span();
            let mut src = src.children();
            let name_p = src.need("a child")?;
            let name = Symbol::new(&name_p.as_str()[1..], name_p.extract_span());
            let mut args = BTreeMap::new();
            for arg in src.need("a child")?.into_inner() {
                let (k, v) = extract_named_apply_arg(arg, param_pool)?;
                args.insert(k, v);
            }
            let validity =
                parse_read_validity_clause(src.next(), param_pool, cur_vld, ignored_counter)?;
            InputAtom::NamedFieldRelation {
                inner: InputNamedFieldRelationApplyAtom {
                    name,
                    args,
                    validity,
                    span,
                },
            }
        }
        _other => bail!(UnexpectedRule(src.extract_span())),
    })
}

fn extract_named_apply_arg(
    pair: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
) -> Result<(Symbol, Expr)> {
    let mut inner = pair.children();
    let name_p = inner.need("a child")?;
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
    let mut src = src.children();
    let name = src.need("a child")?;
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
    let src = src.children().need("a child")?;
    Ok(match src.as_rule() {
        Rule::var => (
            Symbol::new(src.as_str(), src.extract_span()),
            HeadAggrSlot::Plain,
        ),
        Rule::aggr_arg => {
            let mut inner = src.children();
            let aggr_p = inner.need("a child")?;
            let aggr_name = aggr_p.as_str();
            let var = inner.need("a child")?;
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
        _other => bail!(UnexpectedRule(src.extract_span())),
    })
}

fn parse_fixed_rule(
    src: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    cur_vld: ValidityTs,
) -> Result<(Symbol, FixedRuleApply)> {
    let mut src = src.children();
    let (out_symbol, head, aggr) = parse_rule_head(src.need("a child")?, param_pool)?;

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

    let name_pair = src.need("a child")?;
    let fixed_name = name_pair.as_str();
    let mut rule_args: Vec<FixedRuleArg> = vec![];
    let mut options = FixedRuleOptions::empty();
    let args_list = src.need("a child")?;
    let args_list_span = args_list.extract_span();

    for nxt in args_list.into_inner() {
        match nxt.as_rule() {
            Rule::fixed_rel => {
                let inner = nxt.children().need("a child")?;
                let span = inner.extract_span();
                match inner.as_rule() {
                    Rule::fixed_rule_rel => {
                        let mut els = inner.children();
                        let name = els.need("a child")?;
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
                        let mut els = inner.children();
                        let name = els.need("a child")?;
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
                                    as_of = Some(parse_at_clause(v, param_pool, cur_vld)?)
                                }
                                _other => bail!(UnexpectedRule(v.extract_span())),
                            }
                        }
                        rule_args.push(FixedRuleArg::Stored {
                            name: Symbol::new(
                                name.as_str()
                                    .strip_prefix('*')
                                    .ok_or_else(|| UnexpectedRule(name.extract_span()))?,
                                name.extract_span(),
                            ),
                            bindings,
                            as_of,
                            span,
                        })
                    }
                    Rule::fixed_named_relation_rel => {
                        let mut els = inner.children();
                        let name = els.need("a child")?;
                        let mut bindings = BTreeMap::new();
                        let mut as_of = None;
                        for p in els {
                            match p.as_rule() {
                                Rule::fixed_named_relation_arg_pair => {
                                    let mut vs = p.children();
                                    let kp = vs.need("a child")?;
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
                                    as_of = Some(parse_at_clause(p, param_pool, cur_vld)?)
                                }
                                _other => bail!(UnexpectedRule(p.extract_span())),
                            }
                        }

                        rule_args.push(FixedRuleArg::NamedStored {
                            name: Symbol::new(
                                name.as_str()
                                    .strip_prefix('*')
                                    .ok_or_else(|| UnexpectedRule(name.extract_span()))?,
                                name.extract_span(),
                            ),
                            bindings,
                            as_of,
                            span,
                        })
                    }
                    _other => bail!(UnexpectedRule(inner.extract_span())),
                }
            }
            Rule::fixed_opt_pair => {
                let mut inner = nxt.children();
                let name_p = inner.need("a child")?;
                let name = Symbol::new(name_p.as_str(), name_p.extract_span());
                let val = inner.need("a child")?;
                let val = build_expr(val, param_pool)?;
                options.insert(name, val)?;
            }
            _other => bail!(UnexpectedRule(nxt.extract_span())),
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
) -> Result<()> {
    let entry_symbol = Symbol::prog_entry(Default::default());
    let options = FixedRuleOptions::from_entries([(
        Symbol::new("data", Default::default()),
        Expr::Const {
            val: DataValue::List(vec![]),
            span: Default::default(),
        },
    )])?;
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
    Ok(())
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
    let mut inner = expr.children();
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

/// Width of a const-rule `data` expression: first row length of a
/// list-of-lists, whether already folded to [`DataValue::List`] or still
/// an [`OP_LIST`] Apply tree (model parse cannot fold Applies).
///
/// Empty data (`[]`) returns [`None`] so the caller falls back to the head
/// length — `?[k, v] <- [] :create …` is legal and inherits arity from the
/// head. A non-empty list whose first element is not a row returns [`None`]
/// as well (engine `Constant::init_options` will refuse the shape later).
fn const_rule_data_arity(data: &Expr) -> Option<usize> {
    match data {
        Expr::Const {
            val: DataValue::List(rows),
            ..
        } => {
            if rows.is_empty() {
                return None;
            }
            rows.first().and_then(|r| r.get_slice()).map(|s| s.len())
        }
        Expr::Apply { op, args, .. } if op.name == OP_LIST.name => {
            if args.is_empty() {
                return None;
            }
            match args.first() {
                Some(Expr::Apply {
                    op: inner_op,
                    args: cols,
                    ..
                }) if inner_op.name == OP_LIST.name => Some(cols.len()),
                Some(Expr::Const {
                    val: DataValue::List(cols),
                    ..
                }) => Some(cols.len()),
                _other => None,
            }
        }
        _other => match data.clone().eval_to_const() {
            Ok(v) => match v {
                DataValue::List(rows) if rows.is_empty() => None,
                DataValue::List(rows) => rows.first().and_then(|r| r.get_slice()).map(|s| s.len()),
                _other_val => {
                    core::mem::drop(_other_val);
                    None
                }
            },
            Err(_eval) => None,
        },
    }
}

/// `@ valid` or `@ system, valid` — system coordinate first when two are given.
fn parse_at_clause(
    vld_clause: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    cur_vld: ValidityTs,
) -> Result<AsOf> {
    let mut coords = vld_clause.children();
    let first = expr2vld_spec(build_expr(coords.need("a child")?, param_pool)?, cur_vld)?;
    Ok(match coords.next() {
        None => AsOf::current(first),
        Some(second) => AsOf::at(
            first,
            expr2vld_spec(build_expr(second, param_pool)?, cur_vld)?,
        ),
    })
}

/// Optional trailing `@` clause on a stored-relation atom: point-in-time
/// (`@ expr`), or story #62's `@spans` / `@delta` / `@delta_sys` — one
/// grammar seat (`read_validity_clause`), dispatched by which alternative
/// matched. Silent in the grammar, so the pair is the matched alternative.
fn parse_read_validity_clause(
    clause: Option<Pair<'_>>,
    param_pool: &BTreeMap<String, DataValue>,
    cur_vld: ValidityTs,
    ignored_counter: &mut u32,
) -> Result<Option<ValidityClause>> {
    let Some(clause) = clause else {
        return Ok(None);
    };
    Ok(Some(match clause.as_rule() {
        Rule::validity_clause => ValidityClause::At(parse_at_clause(clause, param_pool, cur_vld)?),
        Rule::spans_clause => {
            let mut children = clause.children();
            children.need("a child")?; // spans_kw
            let var_pair = children.need("a child")?;
            let mut var = Symbol::new(var_pair.as_str(), var_pair.extract_span());
            if matches!(var.kind(), SymbolKind::Ignored) {
                var.name = format!("*^*{}", *ignored_counter).into();
                *ignored_counter += 1;
            }
            let sys = match children.next() {
                None => MAX_VALIDITY_TS,
                Some(sys_expr) => expr2vld_spec(build_expr(sys_expr, param_pool)?, cur_vld)?,
            };
            ValidityClause::Spans { sys, var }
        }
        Rule::delta_clause | Rule::delta_sys_clause => {
            let axis = if clause.as_rule() == Rule::delta_sys_clause {
                DeltaAxis::Sys
            } else {
                DeltaAxis::Valid
            };
            let mut children = clause.children();
            children.need("a child")?; // delta_kw / delta_sys_kw
            let from = expr2vld_spec(build_expr(children.need("a child")?, param_pool)?, cur_vld)?;
            let to = expr2vld_spec(build_expr(children.need("a child")?, param_pool)?, cur_vld)?;
            let var_pair = children.need("a child")?;
            let mut var = Symbol::new(var_pair.as_str(), var_pair.extract_span());
            if matches!(var.kind(), SymbolKind::Ignored) {
                var.name = format!("*^*{}", *ignored_counter).into();
                *ignored_counter += 1;
            }
            ValidityClause::Delta {
                axis,
                from,
                to,
                var,
            }
        }
        _other => bail!(UnexpectedRule(clause.extract_span())),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{Script, parse_script};
    use crate::program::rule::InputInlineRulesOrFixed;
    use crate::value::ValidityTs;
    use std::collections::BTreeMap;

    fn parse_q(src: &str) -> Result<InputProgram> {
        let cur = ValidityTs::of_micros(0);
        match parse_script(src, &BTreeMap::new(), cur)? {
            Script::Query(p) => Ok(p),
            other => bail!("expected Query script, got {other:?}"),
        }
    }

    #[test]
    fn parse_query_const_entry_is_fixed_constant_with_head_arity_two() -> Result<()> {
        // `<- [[…]]` lifts as the Constant fixed rule, not inline Rules.
        let prog = parse_q("?[a, b] <- [[1, 2]]")?;
        assert_eq!(prog.entry_name().to_string(), "?");
        match prog.entry() {
            InputInlineRulesOrFixed::Fixed { fixed } => {
                assert_eq!(fixed.head.len(), 2);
                assert_eq!(fixed.head[0].to_string(), "a");
                assert_eq!(fixed.head[1].to_string(), "b");
                assert_eq!(fixed.arity()?, 2);
                assert_eq!(fixed.fixed_handle.name.to_string(), "Constant");
            }
            InputInlineRulesOrFixed::Rules { .. } => {
                bail!("const `<-` entry must be Fixed(Constant), got Rules")
            }
        }
        Ok(())
    }

    #[test]
    fn parse_query_refuses_empty_and_no_entry() -> Result<()> {
        let cur = ValidityTs::of_micros(0);
        ensure!(
            parse_script("", &BTreeMap::new(), cur).is_err(),
            "empty script must refuse"
        );
        // A rule without `?` entry cannot become an InputProgram.
        match parse_script("r[x] := *s[x]", &BTreeMap::new(), cur) {
            Ok(_) => bail!("no-entry query must refuse"),
            Err(err) => {
                let msg = format!("{err:?}");
                ensure!(
                    msg.contains("entry") || msg.contains("Entry") || msg.contains("?"),
                    "no-entry query must refuse, got {msg}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn parse_query_relation_apply_atom_lifts() -> Result<()> {
        let prog = parse_q("?[x] := *r[x]")?;
        match prog.entry() {
            InputInlineRulesOrFixed::Rules { rules } => {
                assert_eq!(rules.len(), 1);
                assert_eq!(rules[0].head.len(), 1);
                ensure!(
                    !rules[0].body.is_empty(),
                    "body must carry the relation apply atom"
                );
            }
            InputInlineRulesOrFixed::Fixed { .. } => bail!("expected Rules entry"),
        }
        Ok(())
    }
}
