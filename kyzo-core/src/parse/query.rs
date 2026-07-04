/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): `parse_query` collects the whole rule map — synthesizing the
 * constant entry for a body-less `:create` — BEFORE calling
 * `InputProgram::new` exactly once (the original built a bare struct and
 * injected the synthetic `?` afterwards; KyzoDB's constructor refuses
 * entry-less programs). The original's first `make_empty_const_rule` site
 * was dead code (it tested `out_opts.store_relation` before anything set
 * it) and has no descendant. Fixed-rule named-relation arguments strip the
 * `*` sigil their grammar rule actually carries (the original stripped `:`
 * and panicked on every `rule(*rel{…})` argument). Grammar-shape `unwrap`s
 * and `unreachable!`s go through the typed-accessor layer. The parse-time
 * surface of the `Constant` fixed rule lives here behind a seam until the
 * fixed-rule tier lands. `parse_aggr` returns `Aggregation` by value
 * (`Copy`); fixed-rule implementations are `Arc<dyn FixedRule>`.
 */

//! Parsing one query: rules, options, and the proofs that bind them.
//!
//! This is where a query's map of definitions is assembled and where
//! [`InputProgram::new`] is called — **exactly once, after the map is
//! complete** — so "every program has an entry" is proven at construction,
//! never patched up afterwards. Along the way: rule-head consistency across
//! multiple definitions, option constancy (`:limit 1+1` is folded, `:limit
//! x` is refused), aggregation and fixed-rule resolution, and validity
//! specs (`@ 'NOW'`) evaluated against the query's one clock reading.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::Arc;

use itertools::Itertools;
use miette::{Diagnostic, LabeledSpan, Report, Result, bail, ensure};
use pest::Parser;
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::aggr::{Aggregation, parse_aggr};
use crate::data::expr::Expr;
use crate::data::program::{
    DeltaAxis, FixedRule, FixedRuleApply, FixedRuleArg, FixedRuleHandle, InputAtom,
    InputInlineRule, InputInlineRulesOrFixed, InputNamedFieldRelationApplyAtom, InputProgram,
    InputRelationApplyAtom, InputRelationHandle, InputRuleApplyAtom, QueryAssertion,
    QueryOutOptions, RelationOp, ReturnMutation, SearchInput, SortDir, Unification, ValidityClause,
    WriteValidity,
};
use crate::data::relation::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
use crate::data::span::SourceSpan;
use crate::data::symb::{Symbol, SymbolKind};
use crate::data::value::{AsOf, DataValue, MAX_VALIDITY_TS, ValidityTs};
use crate::parse::expr::build_expr;
use crate::parse::schema::parse_schema;
use crate::parse::{
    ExtractSpan, IntoChildren, Pair, Pairs, Rule, ScriptParser, strip_sigil, unexpected,
};

// The fixed-rule tier has landed: the `Constant` parse-time surface that
// lived here behind a seam re-homed to `fixed_rule/utilities/constant.rs`
// (and grew its `run`), `FixedRuleNotFoundError` to `fixed_rule/mod.rs`.
use crate::fixed_rule::FixedRuleNotFoundError;
use crate::fixed_rule::utilities::Constant;

// ─────────────────────────────────────────────────────────────────────────
// Option errors
// ─────────────────────────────────────────────────────────────────────────

#[derive(Error, Diagnostic, Debug)]
#[error("`:{0}` must evaluate to a constant")]
#[diagnostic(code(parser::option_not_constant))]
#[diagnostic(help(
    "options are evaluated once, at parse time, before any row exists — the expression can't \
     reference row variables or aggregations; see the attached cause for what stopped it \
     from folding to a constant"
))]
struct OptionNotConstantError(&'static str, #[label] SourceSpan, #[related] [Report; 1]);

#[derive(Error, Diagnostic, Debug)]
#[error("`:{0}` needs a non-negative integer")]
#[diagnostic(code(parser::option_not_non_neg))]
#[diagnostic(help("`:{0}` takes an integer that is 0 or greater"))]
struct OptionNotNonNegIntError(&'static str, #[label] SourceSpan);

#[derive(Error, Diagnostic, Debug)]
#[error("`:{0}` needs a positive integer")]
#[diagnostic(code(parser::option_not_pos))]
#[diagnostic(help("`:{0}` takes an integer greater than 0"))]
struct OptionNotPosIntError(&'static str, #[label] SourceSpan);

#[derive(Error, Diagnostic, Debug)]
#[error("`:{0}` needs a boolean")]
#[diagnostic(code(parser::option_not_bool))]
#[diagnostic(help("write `:{0} true` or `:{0} false`"))]
struct OptionNotBoolError(&'static str, #[label] SourceSpan);

#[derive(Debug)]
struct MultipleRuleDefinitionError(String, Vec<SourceSpan>);

#[derive(Debug, Error, Diagnostic)]
#[error("this query asserts its output relation more than once")]
#[diagnostic(code(parser::multiple_out_assert))]
#[diagnostic(help(
    "a script has exactly one entry point — one `?[...] := …` (or `<-`/`<~`) — pick one and \
     fold the rest into rules the entry calls"
))]
struct DuplicateQueryAssertion(#[label] SourceSpan);

impl Error for MultipleRuleDefinitionError {}

impl Display for MultipleRuleDefinitionError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "`{0}` is defined more than once, and at least one definition isn't a plain \
             Horn clause (it has aggregation, or is a fixed rule)",
            self.0
        )
    }
}

impl Diagnostic for MultipleRuleDefinitionError {
    fn code<'a>(&'a self) -> Option<Box<dyn Display + 'a>> {
        Some(Box::new("parser::mult_rule_def"))
    }
    fn help<'a>(&'a self) -> Option<Box<dyn Display + 'a>> {
        Some(Box::new(format!(
            "only plain rules (`{0}[...] := body`, no aggregation) may share a name across \
             multiple definitions — a fixed rule or an aggregating rule must be the only \
             definition of `{0}`",
            self.0
        )))
    }
    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        Some(Box::new(
            self.1.iter().map(|s| LabeledSpan::new_with_span(None, s)),
        ))
    }
}

/// The union of the symbols' spans; `fallback` for an empty slice (the
/// original `unwrap`ped the first element instead).
fn merge_spans(symbs: &[Symbol], fallback: SourceSpan) -> SourceSpan {
    symbs
        .iter()
        .map(|s| s.span)
        .reduce(SourceSpan::merge)
        .unwrap_or(fallback)
}

pub(crate) fn parse_query(
    src: Pairs<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    fixed_rules: &BTreeMap<String, Arc<dyn FixedRule>>,
    cur_vld: ValidityTs,
) -> Result<InputProgram> {
    let mut progs: BTreeMap<Symbol, InputInlineRulesOrFixed> = Default::default();
    let mut out_opts: QueryOutOptions = Default::default();
    let mut disable_magic_rewrite = false;

    let mut stored_relation = None;
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
                                // Non-empty by construction: every `Rules`
                                // entry is created with one rule and only
                                // ever appended to.
                                if let Some(prev) = rs.first() {
                                    ensure!(prev.aggr == rule.aggr, {
                                        RuleHeadMismatch(
                                            key,
                                            merge_spans(&prev.head, prev.span),
                                            merge_spans(&rule.head, rule.span),
                                        )
                                    });
                                }
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
                let (name, apply) = parse_fixed_rule(pair, param_pool, fixed_rules, cur_vld)?;

                match progs.entry(name) {
                    Entry::Vacant(e) => {
                        e.insert(InputInlineRulesOrFixed::Fixed { fixed: apply });
                    }
                    Entry::Occupied(e) => {
                        let found_name = e.key().name.to_string();
                        let mut found_span = match e.get() {
                            InputInlineRulesOrFixed::Rules { rules } => {
                                rules.iter().map(|r| r.span).collect_vec()
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
                let (name, mut head, aggr) =
                    parse_rule_head(src.expect("the const rule's head")?, param_pool)?;

                if let Some(found) = progs.get(&name) {
                    let mut found_span = match found {
                        InputInlineRulesOrFixed::Rules { rules } => {
                            rules.iter().map(|r| r.span).collect_vec()
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
                #[error("a constant rule's head can't apply an aggregation")]
                #[diagnostic(code(parser::aggr_in_const_rule))]
                #[diagnostic(help(
                    "`head <- value` binds `value`'s rows verbatim — there is nothing to \
                     aggregate over; drop the aggregation from the head, or use a real rule \
                     (`head := body`) if you need it"
                ))]
                struct AggrInConstRuleError(#[label] SourceSpan);

                for (a, v) in aggr.iter().zip(head.iter()) {
                    ensure!(a.is_none(), AggrInConstRuleError(v.span));
                }
                let data_part = src.expect("the const rule's data expression")?;
                let data_part_str = data_part.as_str();
                let data = build_expr(data_part.clone(), param_pool)?;
                let mut options = BTreeMap::new();
                options.insert(SmartString::from("data"), data);
                let handle = FixedRuleHandle {
                    name: Symbol::new("Constant", span),
                };
                let fixed_impl = Constant;
                fixed_impl.init_options(&mut options, span)?;
                let arity = fixed_impl.arity(&options, &head, span)?;

                ensure!(arity != 0, EmptyRowForConstRule(span));
                ensure!(
                    head.is_empty() || arity == head.len(),
                    FixedRuleHeadArityMismatch(arity, head.len(), span)
                );
                if head.is_empty()
                    && name.kind() == SymbolKind::Entry
                    && let Ok(datalist) = ScriptParser::parse(Rule::param_list, data_part_str)
                    && let Ok(datalist) =
                        crate::parse::single(datalist, "the parsed param_list", Rule::param_list)
                {
                    for s in datalist.into_inner() {
                        if s.as_rule() == Rule::param {
                            head.push(Symbol::new(strip_sigil(&s, '$')?, Default::default()));
                        }
                    }
                }
                progs.insert(
                    name,
                    InputInlineRulesOrFixed::Fixed {
                        fixed: FixedRuleApply {
                            fixed_handle: handle,
                            rule_args: vec![],
                            options: Arc::new(options),
                            head,
                            arity,
                            span,
                            fixed_impl: Arc::new(Constant),
                        },
                    },
                );
            }
            Rule::timeout_option => {
                let pair = pair.children().expect("the timeout expression")?;
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
                {
                    #[derive(Debug, Error, Diagnostic)]
                    #[error("`:sleep` is not supported under WASM")]
                    #[diagnostic(code(parser::sleep_unsupported_wasm))]
                    #[diagnostic(help(
                        "a WASM build has no thread to block on: drop the `:sleep` option \
                         from this script, or don't run it under wasm32"
                    ))]
                    struct SleepUnsupportedOnWasm(#[label] SourceSpan);
                    bail!(SleepUnsupportedOnWasm(pair.extract_span()));
                }

                #[cfg(not(target_arch = "wasm32"))]
                {
                    let pair = pair.children().expect("the sleep expression")?;
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
                let pair = pair.children().expect("the limit expression")?;
                let span = pair.extract_span();
                let limit = build_expr(pair, param_pool)?
                    .eval_to_const()
                    .map_err(|err| OptionNotConstantError("limit", span, [err]))?
                    .get_non_neg_int()
                    .ok_or(OptionNotNonNegIntError("limit", span))?;
                out_opts.limit = Some(limit as usize);
            }
            Rule::offset_option => {
                let pair = pair.children().expect("the offset expression")?;
                let span = pair.extract_span();
                let offset = build_expr(pair, param_pool)?
                    .eval_to_const()
                    .map_err(|err| OptionNotConstantError("offset", span, [err]))?
                    .get_non_neg_int()
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
                            _ => return Err(unexpected("a sort direction or key", &a)),
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
                let op_p = args.expect("the relation operation")?;
                let op = match op_p.as_rule() {
                    Rule::relation_create => RelationOp::Create,
                    Rule::relation_replace => RelationOp::Replace,
                    Rule::relation_put => RelationOp::Put,
                    Rule::relation_insert => RelationOp::Insert,
                    Rule::relation_update => RelationOp::Update,
                    Rule::relation_rm => RelationOp::Rm,
                    Rule::relation_delete => RelationOp::Delete,
                    Rule::relation_ensure => RelationOp::Ensure,
                    Rule::relation_ensure_not => RelationOp::EnsureNot,
                    _ => return Err(unexpected("a relation operation", &op_p)),
                };

                let name_p = args.expect("the output relation's name")?;
                let name = Symbol::new(name_p.as_str(), name_p.extract_span());

                // What's left is `table_schema? ~ validity_clause?`: zero,
                // one, or two more children. Sorted by their own rule, not
                // position, since either may be absent.
                let mut schema_p = None;
                let mut validity_p = None;
                for rest in args {
                    match rest.as_rule() {
                        Rule::table_schema => schema_p = Some(rest),
                        Rule::validity_clause => validity_p = Some(rest),
                        _ => return Err(unexpected("a table schema or `@` clause", &rest)),
                    }
                }

                let raw_write_vld = parse_write_validity_clause(validity_p, op, param_pool)?;

                match schema_p {
                    None => {
                        stored_relation = Some(StagedRelation::Unnamed {
                            name,
                            span,
                            op,
                            raw_write_vld,
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
                        stored_relation = Some(StagedRelation::WithSchema {
                            handle: InputRelationHandle {
                                name,
                                metadata,
                                key_bindings,
                                dep_bindings,
                                span,
                            },
                            op,
                            raw_write_vld,
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
                let pair = pair.children().expect("the option's boolean expression")?;
                let span = pair.extract_span();
                let val = build_expr(pair, param_pool)?
                    .eval_to_const()
                    .map_err(|err| OptionNotConstantError("disable_magic_rewrite", span, [err]))?
                    .get_bool()
                    .ok_or(OptionNotBoolError("disable_magic_rewrite", span))?;
                disable_magic_rewrite = val;
            }
            Rule::EOI => break,
            _ => return Err(unexpected("a rule or query option", &pair)),
        }
    }

    // A body-less `:create` gets its synthetic constant entry HERE, while
    // the rule map is still open — `InputProgram::new` (below, called
    // exactly once) refuses entry-less programs. The CozoDB original built
    // a bare `InputProgram{}` first and injected the `?` afterwards
    // (`make_empty_const_rule`); its earlier injection site was dead code —
    // it tested `out_opts.store_relation` before anything had set it — so
    // this one synthesis point is the whole behavior. Binding order (deps
    // before keys) is the original's live site, preserved; only the arity
    // matters, since the data is empty.
    if progs.is_empty()
        && let Some(StagedRelation::WithSchema { handle, op, .. }) = &stored_relation
        && *op == RelationOp::Create
    {
        let mut bindings = handle.dep_bindings.clone();
        bindings.extend_from_slice(&handle.key_bindings);
        insert_empty_const_entry(&mut progs, &bindings, handle.span);
    }

    // The one construction: proves the program has an entry and no rule
    // set is empty. Every use of `prog` below rides on that proof.
    let mut prog = InputProgram::new(progs, out_opts, disable_magic_rewrite)?;

    match stored_relation {
        None => {}
        Some(StagedRelation::Unnamed {
            name,
            span,
            op,
            raw_write_vld,
        }) => {
            let head = prog.get_entry_out_head()?;
            for symb in &head {
                symb.ensure_valid_field()?;
            }
            let write_vld = resolve_write_validity(raw_write_vld, &prog, cur_vld)?;

            let metadata = StoredRelationMetadata {
                keys: head
                    .iter()
                    .map(|s| ColumnDef {
                        name: s.name.clone(),
                        typing: NullableColType {
                            coltype: ColType::Any,
                            nullable: true,
                        },
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
            prog.out_opts_mut().store_relation = Some((handle, op, returning_mutation, write_vld))
        }
        Some(StagedRelation::WithSchema {
            handle,
            op,
            raw_write_vld,
        }) => {
            let write_vld = resolve_write_validity(raw_write_vld, &prog, cur_vld)?;
            prog.out_opts_mut().store_relation = Some((handle, op, returning_mutation, write_vld))
        }
    }

    if !prog.out_opts().sorters.is_empty() {
        #[derive(Debug, Error, Diagnostic)]
        #[error("`:sort`/`:order` names `{0}`, which isn't a head column")]
        #[diagnostic(code(parser::sort_key_not_found))]
        #[diagnostic(help("a sort key must be one of the entry rule's own head variables"))]
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
    #[error("`{0}` declares dependent columns but no key column")]
    #[diagnostic(code(parser::relation_has_no_keys))]
    #[diagnostic(help(
        "every stored relation needs at least one key column before `=>`, e.g. \
         `{0} {{id => value}}`, not `{0} {{=> value}}`"
    ))]
    struct RelationHasNoKeys(String, #[label] SourceSpan);

    let empty_mutation_head = match &prog.out_opts().store_relation {
        None => false,
        Some((handle, _, _, _)) if handle.key_bindings.is_empty() => {
            if handle.dep_bindings.is_empty() {
                true
            } else {
                bail!(RelationHasNoKeys(handle.name.to_string(), handle.span));
            }
        }
        Some(_) => false,
    };

    if empty_mutation_head {
        let head_args = prog.get_entry_out_head()?;
        // `empty_mutation_head` is only true when `store_relation` matched
        // `Some` just above; if that proof is ever broken this is a no-op,
        // not an abort (the original had `else { unreachable!() }`).
        if let Some((handle, _, _, _)) = &mut prog.out_opts_mut().store_relation {
            if head_args.is_empty() {
                bail!(RelationHasNoKeys(handle.name.to_string(), handle.span));
            }
            handle.key_bindings = head_args.clone();
            handle.metadata.keys = head_args
                .iter()
                .map(|s| ColumnDef {
                    name: s.name.clone(),
                    typing: NullableColType {
                        coltype: ColType::Any,
                        nullable: true,
                    },
                    default_gen: None,
                })
                .collect();
        }
    }

    Ok(prog)
}

/// A `:create`/`:put`/… clause as staged during the option loop, before the
/// program exists to hang it on. (The original used
/// `Either<(Symbol, SourceSpan, RelationOp), (InputRelationHandle, RelationOp)>`.)
enum StagedRelation {
    /// `:put name` — no schema given; columns come from the entry head.
    Unnamed {
        name: Symbol,
        span: SourceSpan,
        op: RelationOp,
        raw_write_vld: RawWriteValidity,
    },
    /// `:create name {…}` — schema written out.
    WithSchema {
        handle: InputRelationHandle,
        op: RelationOp,
        raw_write_vld: RawWriteValidity,
    },
}

/// A write-time `@` clause as staged during the option loop: refused shapes
/// (two coordinates, or any `@` on `:ensure`/`:ensure_not`) are already
/// rejected by this point, but whether the one surviving coordinate is a
/// parse-time constant or a per-row column reference isn't decided until
/// the entry rule's head is known — which happens only after
/// [`InputProgram::new`], since a `:put` line may parse before its entry
/// rule does.
enum RawWriteValidity {
    /// No `@` clause: every row lands at the transaction's system stamp.
    Now,
    /// `@ <expr>`: the one legal write-side coordinate, not yet resolved
    /// against the entry head.
    OneCoord(Expr),
}

/// The write side's `@` clause cannot set the system coordinate: system
/// time is always the committing transaction's own engine-minted stamp, so
/// a script that could choose it would let a writer forge when the
/// database "learned" a fact.
#[derive(Debug, Error, Diagnostic)]
#[error(
    "a write's `@` clause takes exactly one coordinate (the valid instant); system time is never script-settable"
)]
#[diagnostic(code(parser::write_validity_sets_system))]
#[diagnostic(help("write `@ instant` (one coordinate), not `@ system, instant`"))]
struct WriteValiditySetsSystemTime(#[label] SourceSpan);

/// `:ensure`/`:ensure_not` only read current state; they perform no
/// bitemporal write, so a `@` clause on them would silently do nothing.
#[derive(Debug, Error, Diagnostic)]
#[error("`@` has no effect on `{0}`, which checks current state and writes nothing")]
#[diagnostic(code(parser::write_validity_on_non_write_op))]
#[diagnostic(help("drop the `@` clause; `{0}` performs no bitemporal write for it to date"))]
struct WriteValidityOnNonWriteOp(&'static str, #[label] SourceSpan);

/// `valid = i64::MAX` (`'END'`, or the literal microsecond itself) is the
/// reserved terminal tick every open-end sentinel depends on being
/// unwritable (issue #62's ruling: the temporal oracle and the Interval
/// `DataValue` both read "no stored event governs past here" as "still
/// open" — a fact actually stored AT that instant would collide with that
/// reading and derive as a zero-width interval). `@ 'END'` stays legal on
/// the READ side (`data_value_to_vld_spec`'s "as of the end of time"); this
/// refusal is write-only, at the one coordinate a `@` clause resolves to a
/// parse-time constant.
#[derive(Debug, Error, Diagnostic)]
#[error(
    "the valid instant `i64::MAX` (`'END'`) is reserved as the open-end sentinel and cannot be written to; name a concrete instant, or omit `@` (every row lands at the transaction's own stamp)"
)]
#[diagnostic(code(parser::write_validity_at_terminal_instant))]
struct WriteValidityAtTerminalInstant(#[label] SourceSpan);

/// Stage a `relation_option`'s optional trailing `validity_clause`: refuse
/// the two-coordinate form and any clause on `:ensure`/`:ensure_not` here
/// (both are op/shape checks, not head-dependent), leaving only the head
/// resolution ([`resolve_write_validity`]) for after construction.
fn parse_write_validity_clause(
    clause: Option<Pair<'_>>,
    op: RelationOp,
    param_pool: &BTreeMap<String, DataValue>,
) -> Result<RawWriteValidity> {
    let Some(clause) = clause else {
        return Ok(RawWriteValidity::Now);
    };
    let span = clause.extract_span();
    if let RelationOp::Ensure | RelationOp::EnsureNot = op {
        let name = if op == RelationOp::Ensure {
            ":ensure"
        } else {
            ":ensure_not"
        };
        bail!(WriteValidityOnNonWriteOp(name, span));
    }
    let mut coords = clause.children();
    let first = build_expr(coords.expect("the write's as-of expression")?, param_pool)?;
    if coords.next().is_some() {
        bail!(WriteValiditySetsSystemTime(span));
    }
    Ok(RawWriteValidity::OneCoord(first))
}

/// Resolve a staged `@` clause: a fully constant expression is one instant
/// for every row (parity with the read side's single-coordinate `@`,
/// folded once here); an expression that still names a free variable must
/// name one of the mutation's own output columns, and becomes a per-row
/// extractor exactly like any other column (`runtime::mutate`'s
/// `DataExtractor`) — the primary backfill/import use case, where every
/// row carries its own timestamp.
///
/// The entry's output head is fetched lazily (only in the per-row branch):
/// unlike `StagedRelation::Unnamed`, `WithSchema` mutations never otherwise
/// needed it, and some legal entries (an unnamed fixed-rule head) don't
/// have one — a `@ <constant>` or headless mutation must not regress by
/// suddenly requiring one.
fn resolve_write_validity(
    raw: RawWriteValidity,
    prog: &InputProgram,
    cur_vld: ValidityTs,
) -> Result<WriteValidity> {
    match raw {
        RawWriteValidity::Now => Ok(WriteValidity::Now),
        RawWriteValidity::OneCoord(expr) => {
            if expr.bindings()?.is_empty() {
                let span = expr.span();
                let vld = crate::data::functions::data_value_to_vld_spec(
                    expr.eval_to_const()?,
                    span,
                    cur_vld,
                )?;
                if vld == crate::data::value::MAX_VALIDITY_TS {
                    bail!(WriteValidityAtTerminalInstant(span));
                }
                Ok(WriteValidity::Fixed(vld))
            } else {
                let head = prog.get_entry_out_head()?;
                let frame: BTreeMap<Symbol, usize> = head
                    .iter()
                    .enumerate()
                    .map(|(i, s)| (s.clone(), i))
                    .collect();
                let mut expr = expr;
                expr.fill_binding_indices(&frame)?;
                Ok(WriteValidity::PerRow(expr))
            }
        }
    }
}

fn parse_rule(
    src: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    cur_vld: ValidityTs,
) -> Result<(Symbol, InputInlineRule)> {
    let span = src.extract_span();
    let mut src = src.children();
    let head = src.expect("the rule head")?;
    let head_span = head.extract_span();
    let (name, head, aggr) = parse_rule_head(head, param_pool)?;

    #[derive(Debug, Error, Diagnostic)]
    #[error("a rule head needs at least one column")]
    #[diagnostic(code(parser::empty_horn_rule_head))]
    #[diagnostic(help("name at least one output variable, e.g. `name[x] := …`"))]
    struct EmptyRuleHead(#[label] SourceSpan);

    ensure!(!head.is_empty(), EmptyRuleHead(head_span));
    let body = src.expect("the rule body")?;
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
    let mut res: Vec<_> = pair
        .into_inner()
        .filter_map(|v| match v.as_rule() {
            Rule::or_op => None,
            _ => Some(parse_atom(v, param_pool, cur_vld, ignored_counter)),
        })
        .try_collect()?;
    Ok(if res.len() == 1 {
        // In bounds: length checked to be exactly 1.
        res.remove(0)
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
            let grouped: Vec<_> = src
                .into_inner()
                .map(|v| parse_disjunction(v, param_pool, cur_vld, ignored_counter))
                .try_collect()?;
            InputAtom::Conjunction {
                inner: grouped,
                span,
            }
        }
        Rule::disjunction => parse_disjunction(src, param_pool, cur_vld, ignored_counter)?,
        Rule::negation => {
            let span = src.extract_span();
            let mut src = src.children();
            src.expect("the `not` operator")?;
            let inner = parse_atom(
                src.expect("the negated atom")?,
                param_pool,
                cur_vld,
                ignored_counter,
            )?;
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
            let var = src.expect("the binding")?;
            let symb = unify_binding_symbol(&var, ignored_counter);
            let expr = build_expr(src.expect("the unified expression")?, param_pool)?;
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
            let var = src.expect("the binding")?;
            let symb = unify_binding_symbol(&var, ignored_counter);
            src.expect("the `in` operator")?;
            let expr = build_expr(src.expect("the unified expression")?, param_pool)?;
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
            let name = src.expect("the applied rule's name")?;
            let args: Vec<_> = src
                .expect("the argument list")?
                .into_inner()
                .map(|v| build_expr(v, param_pool))
                .try_collect()?;
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
            let name = src.expect("the applied relation's name")?;
            let args: Vec<_> = src
                .expect("the argument list")?
                .into_inner()
                .map(|v| build_expr(v, param_pool))
                .try_collect()?;
            let validity = parse_validity_clause(src.next(), param_pool, cur_vld, ignored_counter)?;
            InputAtom::Relation {
                inner: InputRelationApplyAtom {
                    name: Symbol::new(strip_sigil(&name, '*')?, name.extract_span()),
                    args,
                    validity,
                    span,
                },
            }
        }
        Rule::search_apply => {
            let span = src.extract_span();
            let mut src = src.children();
            let name_p = src.expect("the `relation:index` head")?;
            let name_segs = name_p.as_str().split(':').collect_vec();

            #[derive(Debug, Error, Diagnostic)]
            #[error("`~{0}` isn't `relation:index`")]
            #[diagnostic(code(parser::invalid_search_head))]
            #[diagnostic(help(
                "a search atom names the base relation and the index together, e.g. \
                 `~doc:emb{{…}}` for the `emb` index on `doc`"
            ))]
            struct InvalidSearchHead(String, #[label] SourceSpan);

            ensure!(
                name_segs.len() == 2,
                InvalidSearchHead(name_p.as_str().to_string(), name_p.extract_span())
            );
            let relation = Symbol::new(name_segs[0], name_p.extract_span());
            let index = Symbol::new(name_segs[1], name_p.extract_span());
            let bindings: BTreeMap<SmartString<LazyCompact>, Expr> = src
                .expect("the named bindings")?
                .into_inner()
                .map(|arg| extract_named_apply_arg(arg, param_pool))
                .try_collect()?;
            let parameters: BTreeMap<SmartString<LazyCompact>, Expr> = src
                .map(|arg| extract_named_apply_arg(arg, param_pool))
                .try_collect()?;

            let opts = SearchInput {
                relation,
                index,
                bindings,
                span,
                parameters,
            };

            InputAtom::Search { inner: opts }
        }
        Rule::relation_named_apply => {
            let span = src.extract_span();
            let mut src = src.children();
            let name_p = src.expect("the applied relation's name")?;
            let name = Symbol::new(strip_sigil(&name_p, '*')?, name_p.extract_span());
            let args = src
                .expect("the named arguments")?
                .into_inner()
                .map(|arg| extract_named_apply_arg(arg, param_pool))
                .try_collect()?;
            let validity = parse_validity_clause(src.next(), param_pool, cur_vld, ignored_counter)?;
            InputAtom::NamedFieldRelation {
                inner: InputNamedFieldRelationApplyAtom {
                    name,
                    args,
                    span,
                    validity,
                },
            }
        }
        _ => return Err(unexpected("a body atom", &src)),
    })
}

/// The binding of a unification, with `_` (bind-nothing) replaced by a
/// fresh generated name so each occurrence stays independent.
fn unify_binding_symbol(var: &Pair<'_>, ignored_counter: &mut u32) -> Symbol {
    let symb = Symbol::new(var.as_str(), var.extract_span());
    if symb.kind() == SymbolKind::Ignored {
        // `*^*n` is `*`-prefixed: SymbolKind::Generated, so it can never
        // collide with a user-written name.
        let fresh = Symbol::new(format!("*^*{ignored_counter}"), symb.span);
        *ignored_counter += 1;
        fresh
    } else {
        symb
    }
}

/// The plain `@ expr` / `@ system, valid` clause (`Rule::validity_clause`),
/// resolved to its one bitemporal coordinate. Shared by
/// [`parse_validity_clause`] (the stored-atom seat, where it is one of
/// several alternatives) and the fixed-rule relation-argument productions
/// (`fixed_relation_rel`/`fixed_named_relation_rel`), which reference
/// `validity_clause` directly and never the temporal alternatives — a
/// fixed rule's input is a complete relation, not a row-multiplying read.
fn parse_at_expr_clause(
    clause: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    cur_vld: ValidityTs,
) -> Result<AsOf> {
    let mut coords = clause.children();
    let first = expr2vld_spec(
        build_expr(coords.expect("the as-of expression")?, param_pool)?,
        cur_vld,
    )?;
    Ok(match coords.next() {
        // `@ valid`: the record's current belief about that instant.
        None => AsOf::current(first),
        // `@ system, valid`: what the record said at `system` about the
        // world at `valid`.
        Some(second) => AsOf {
            sys: first,
            valid: expr2vld_spec(build_expr(second, param_pool)?, cur_vld)?,
        },
    })
}

/// An optional trailing `@` clause on a stored-relation atom: the
/// pre-existing point-in-time read (`@ expr`, unchanged), or one of story
/// #62's derivation/diff clauses (`@spans`/`@delta`/`@delta_sys`) — all one
/// grammar seat (`read_validity_clause` in `kyzoscript.pest`), dispatched
/// here by which alternative matched.
fn parse_validity_clause(
    clause: Option<Pair<'_>>,
    param_pool: &BTreeMap<String, DataValue>,
    cur_vld: ValidityTs,
    ignored_counter: &mut u32,
) -> Result<Option<ValidityClause>> {
    let Some(clause) = clause else {
        return Ok(None);
    };
    match clause.as_rule() {
        Rule::validity_clause => Ok(Some(ValidityClause::At(parse_at_expr_clause(
            clause, param_pool, cur_vld,
        )?))),
        Rule::spans_clause => {
            let mut children = clause.children();
            children.expect("the `@spans` keyword")?; // spans_kw, discarded
            let var_pair = children.expect("`@spans`'s bound interval variable")?;
            let var = unify_binding_symbol(&var_pair, ignored_counter);
            let sys = match children.next() {
                // Default: the record's current belief (every stored
                // system version is visible).
                None => MAX_VALIDITY_TS,
                Some(sys_expr) => expr2vld_spec(build_expr(sys_expr, param_pool)?, cur_vld)?,
            };
            Ok(Some(ValidityClause::Spans { sys, var }))
        }
        Rule::delta_clause | Rule::delta_sys_clause => {
            let axis = if clause.as_rule() == Rule::delta_sys_clause {
                DeltaAxis::Sys
            } else {
                DeltaAxis::Valid
            };
            let mut children = clause.children();
            children.expect("the `@delta`/`@delta_sys` keyword")?; // discarded
            let from = expr2vld_spec(
                build_expr(children.expect("`@delta`'s FROM instant")?, param_pool)?,
                cur_vld,
            )?;
            let to = expr2vld_spec(
                build_expr(children.expect("`@delta`'s TO instant")?, param_pool)?,
                cur_vld,
            )?;
            let var_pair = children.expect("`@delta`'s bound sign variable")?;
            let var = unify_binding_symbol(&var_pair, ignored_counter);
            Ok(Some(ValidityClause::Delta {
                axis,
                from,
                to,
                var,
            }))
        }
        _ => Err(unexpected("a validity/temporal clause", &clause)),
    }
}

fn extract_named_apply_arg(
    pair: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
) -> Result<(SmartString<LazyCompact>, Expr)> {
    let mut inner = pair.children();
    let name_p = inner.expect("the field name")?;
    let name = SmartString::from(name_p.as_str());
    let arg = match inner.next() {
        Some(a) => build_expr(a, param_pool)?,
        None => Expr::Binding {
            var: Symbol::new(name.clone(), name_p.extract_span()),
            tuple_pos: None,
        },
    };
    Ok((name, arg))
}

fn parse_rule_head(
    src: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
) -> Result<(
    Symbol,
    Vec<Symbol>,
    Vec<Option<(Aggregation, Vec<DataValue>)>>,
)> {
    let mut src = src.children();
    let name = src.expect("the rule's name")?;
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
#[error("`{0}` isn't a known aggregation")]
struct AggrNotFound(String, #[label] SourceSpan, #[help] Option<String>);

/// The common built-in aggregations, for the "did you mean" hint on
/// [`AggrNotFound`] only. `parse_aggr` (`data/aggr.rs`) plus the sketch
/// aggregations it defers to are the actual source of truth for what a
/// script may name; if this list ever drifts from that, the failure mode is
/// a weaker hint, never a wrong refusal.
const COMMON_AGGR_NAMES: &[&str] = &[
    "count",
    "count_unique",
    "sum",
    "product",
    "mean",
    "variance",
    "std_dev",
    "min",
    "max",
    "unique",
    "collect",
    "group_count",
    "union",
    "intersection",
    "choice",
    "choice_rand",
    "shortest",
    "min_cost",
    "bit_and",
    "bit_or",
    "bit_xor",
    "latest_by",
    "smallest_by",
    "and",
    "or",
];

/// The closest [`COMMON_AGGR_NAMES`] entry to `name`, offered only when it's
/// close enough to plausibly be a typo rather than an unrelated word.
fn suggest_aggr(name: &str) -> Option<String> {
    COMMON_AGGR_NAMES
        .iter()
        .map(|&candidate| (candidate, edit_distance(name, candidate)))
        .filter(|&(_, distance)| distance <= 2)
        .min_by_key(|&(_, distance)| distance)
        .map(|(candidate, _)| format!("did you mean `{candidate}`?"))
}

/// Levenshtein edit distance, for typo suggestions only (no crate pulled in
/// for one small function over short identifiers).
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, &ca) in a.iter().enumerate() {
        let mut cur = vec![0usize; b.len() + 1];
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            cur[j + 1] = if ca == cb {
                prev[j]
            } else {
                1 + prev[j].min(prev[j + 1]).min(cur[j])
            };
        }
        prev = cur;
    }
    prev[b.len()]
}

fn parse_rule_head_arg(
    src: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
) -> Result<(Symbol, Option<(Aggregation, Vec<DataValue>)>)> {
    let src = src.children().expect("the head argument")?;
    Ok(match src.as_rule() {
        Rule::var => (Symbol::new(src.as_str(), src.extract_span()), None),
        Rule::aggr_arg => {
            let mut inner = src.children();
            let aggr_p = inner.expect("the aggregation's name")?;
            let aggr_name = aggr_p.as_str();
            let var = inner.expect("the aggregated binding")?;
            let args: Vec<_> = inner
                .map(|v| -> Result<DataValue> { build_expr(v, param_pool)?.eval_to_const() })
                .try_collect()?;
            (
                Symbol::new(var.as_str(), var.extract_span()),
                Some((
                    parse_aggr(aggr_name).ok_or_else(|| {
                        AggrNotFound(
                            aggr_name.to_string(),
                            aggr_p.extract_span(),
                            suggest_aggr(aggr_name),
                        )
                    })?,
                    args,
                )),
            )
        }
        _ => return Err(unexpected("a head argument", &src)),
    })
}

fn parse_fixed_rule(
    src: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    fixed_rules: &BTreeMap<String, Arc<dyn FixedRule>>,
    cur_vld: ValidityTs,
) -> Result<(Symbol, FixedRuleApply)> {
    let mut src = src.children();
    let (out_symbol, head, aggr) =
        parse_rule_head(src.expect("the fixed rule's head")?, param_pool)?;

    #[derive(Debug, Error, Diagnostic)]
    #[error("a fixed rule's head can't apply an aggregation")]
    #[diagnostic(code(parser::fixed_aggr_conflict))]
    #[diagnostic(help(
        "a fixed rule (`head <~ Algo(...)`) names its own output columns; wrap its result in \
         a further rule if you need to aggregate over them"
    ))]
    struct AggrInFixedError(#[label] SourceSpan);

    #[derive(Debug, Error, Diagnostic)]
    #[error("`{0}` is bound twice in this fixed-rule invocation")]
    #[diagnostic(code(parser::duplicate_bindings_for_fixed_rule))]
    #[diagnostic(help(
        "each argument relation's bindings share one namespace across the whole invocation; \
         rename one occurrence, or bind it as `_` if the value doesn't matter there"
    ))]
    struct DuplicateBindingError(String, #[label] SourceSpan);

    for (a, v) in aggr.iter().zip(head.iter()) {
        ensure!(a.is_none(), AggrInFixedError(v.span))
    }

    let mut seen_bindings = BTreeSet::new();
    let mut binding_gen_id = 0;

    let name_pair = src.expect("the invoked rule's name")?;
    let fixed_name = &name_pair.as_str();
    let mut rule_args: Vec<FixedRuleArg> = vec![];
    let mut options: BTreeMap<SmartString<LazyCompact>, Expr> = Default::default();
    let args_list = src.expect("the argument list")?;
    let args_list_span = args_list.extract_span();

    for nxt in args_list.into_inner() {
        match nxt.as_rule() {
            Rule::fixed_rel => {
                let inner = nxt.children().expect("the relation argument")?;
                let span = inner.extract_span();
                match inner.as_rule() {
                    Rule::fixed_rule_rel => {
                        let mut els = inner.children();
                        let name = els.expect("the rule name")?;
                        let mut bindings = Vec::with_capacity(els.size_hint().1.unwrap_or(4));
                        for v in els {
                            let s = v.as_str();
                            if s == "_" {
                                let symb =
                                    Symbol::new(format!("*_*{binding_gen_id}"), v.extract_span());
                                binding_gen_id += 1;
                                bindings.push(symb);
                            } else {
                                if !seen_bindings.insert(s) {
                                    bail!(DuplicateBindingError(s.to_string(), v.extract_span()))
                                }
                                let symb = Symbol::new(s, v.extract_span());
                                bindings.push(symb);
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
                        let name = els.expect("the relation name")?;
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
                                            bail!(DuplicateBindingError(
                                                s.to_string(),
                                                v.extract_span()
                                            ))
                                        }
                                        bindings.push(Symbol::new(v.as_str(), v.extract_span()))
                                    }
                                }
                                Rule::validity_clause => {
                                    as_of = Some(parse_at_expr_clause(v, param_pool, cur_vld)?);
                                }
                                _ => {
                                    return Err(unexpected("a binding or validity clause", &v));
                                }
                            }
                        }
                        rule_args.push(FixedRuleArg::Stored {
                            name: Symbol::new(strip_sigil(&name, '*')?, name.extract_span()),
                            bindings,
                            as_of,
                            span,
                        })
                    }
                    Rule::fixed_named_relation_rel => {
                        let mut els = inner.children();
                        let name = els.expect("the relation name")?;
                        let mut bindings = BTreeMap::new();
                        let mut as_of = None;
                        for p in els {
                            match p.as_rule() {
                                Rule::fixed_named_relation_arg_pair => {
                                    let mut vs = p.children();
                                    let kp = vs.expect("the field name")?;
                                    let k = SmartString::from(kp.as_str());
                                    let v = match vs.next() {
                                        Some(vp) => {
                                            if !seen_bindings.insert(vp.as_str()) {
                                                bail!(DuplicateBindingError(
                                                    vp.as_str().to_string(),
                                                    vp.extract_span()
                                                ))
                                            }
                                            Symbol::new(vp.as_str(), vp.extract_span())
                                        }
                                        None => {
                                            if !seen_bindings.insert(kp.as_str()) {
                                                bail!(DuplicateBindingError(
                                                    kp.as_str().to_string(),
                                                    kp.extract_span()
                                                ))
                                            }
                                            Symbol::new(k.clone(), kp.extract_span())
                                        }
                                    };
                                    bindings.insert(k, v);
                                }
                                Rule::validity_clause => {
                                    as_of = Some(parse_at_expr_clause(p, param_pool, cur_vld)?);
                                }
                                _ => {
                                    return Err(unexpected("a field pair or validity clause", &p));
                                }
                            }
                        }

                        rule_args.push(FixedRuleArg::NamedStored {
                            // The `*` sigil, matching `relation_ident` in
                            // the grammar. The CozoDB original stripped `:`
                            // here and panicked on every named-relation
                            // fixed-rule argument.
                            name: Symbol::new(strip_sigil(&name, '*')?, name.extract_span()),
                            bindings,
                            as_of,
                            span,
                        })
                    }
                    _ => return Err(unexpected("a fixed-rule relation argument", &inner)),
                }
            }
            Rule::fixed_opt_pair => {
                let [name, val] = nxt
                    .children()
                    .expect_n(["the option's name", "the option's value"])?;
                let val = build_expr(val, param_pool)?;
                options.insert(SmartString::from(name.as_str()), val);
            }
            _ => return Err(unexpected("a fixed-rule argument", &nxt)),
        }
    }

    let fixed = FixedRuleHandle {
        name: Symbol::new(*fixed_name, name_pair.extract_span()),
    };

    let fixed_impl = fixed_rules
        .get(&fixed.name as &str)
        .ok_or_else(|| FixedRuleNotFoundError(fixed.name.to_string(), name_pair.extract_span()))?;
    fixed_impl.init_options(&mut options, args_list_span)?;
    let arity = fixed_impl.arity(&options, &head, name_pair.extract_span())?;

    ensure!(
        head.is_empty() || arity == head.len(),
        FixedRuleHeadArityMismatch(arity, head.len(), args_list_span)
    );

    Ok((
        out_symbol,
        FixedRuleApply {
            fixed_handle: fixed,
            rule_args,
            options: Arc::new(options),
            head,
            arity,
            span: args_list_span,
            fixed_impl: fixed_impl.clone(),
        },
    ))
}

#[derive(Debug, Error, Diagnostic)]
#[error("the rule head names {1} column(s), but the fixed rule produces {0}")]
#[diagnostic(code(parser::fixed_rule_head_arity_mismatch))]
#[diagnostic(help(
    "either write exactly {0} head column(s) (`head[c1, c2, …]`), or omit the head columns \
     entirely and let the fixed rule name them"
))]
struct FixedRuleHeadArityMismatch(usize, usize, #[label] SourceSpan);

#[derive(Debug, Error, Diagnostic)]
#[error("a constant rule's rows can't be zero columns wide")]
#[diagnostic(code(parser::const_rule_empty_row))]
#[diagnostic(help("`head <- value` needs `value` to be rows of at least one column each"))]
struct EmptyRowForConstRule(#[label] SourceSpan);

/// The synthetic entry of a body-less `:create`: a `Constant` rule with no
/// rows, headed by the declared columns. Inserted into the still-open rule
/// map — this must run before `InputProgram::new`, which refuses entry-less
/// programs.
fn insert_empty_const_entry(
    progs: &mut BTreeMap<Symbol, InputInlineRulesOrFixed>,
    bindings: &[Symbol],
    span: SourceSpan,
) {
    let entry_symbol = Symbol::prog_entry(span);
    let mut options = BTreeMap::new();
    options.insert(
        SmartString::from("data"),
        Expr::Const {
            val: DataValue::List(vec![]),
            span,
        },
    );
    progs.insert(
        entry_symbol,
        InputInlineRulesOrFixed::Fixed {
            fixed: FixedRuleApply {
                fixed_handle: FixedRuleHandle {
                    name: Symbol::new("Constant", span),
                },
                rule_args: vec![],
                options: Arc::new(options),
                head: bindings.to_vec(),
                arity: bindings.len(),
                span,
                fixed_impl: Arc::new(Constant),
            },
        },
    );
}

fn expr2vld_spec(expr: Expr, cur_vld: ValidityTs) -> Result<ValidityTs> {
    let vld_span = expr.span();
    crate::data::functions::data_value_to_vld_spec(expr.eval_to_const()?, vld_span, cur_vld)
}

#[cfg(test)]
mod tests {
    use std::cmp::Reverse;

    use super::*;
    use crate::parse::parse_script;

    fn vld() -> ValidityTs {
        ValidityTs(Reverse(0))
    }

    fn parse_single(src: &str) -> Result<InputProgram> {
        parse_script(src, &Default::default(), &Default::default(), vld())?.get_single_program()
    }

    /// THE LANDMINE: a body-less `:create` must parse. The constant `?`
    /// entry is synthesized into the rule map BEFORE `InputProgram::new`
    /// runs, because the constructor refuses entry-less programs (the
    /// CozoDB original injected it after building a bare struct).
    #[test]
    fn bodyless_create_synthesizes_the_entry() {
        let prog = parse_single(":create t {k: Int => v}").unwrap();
        assert_eq!(prog.get_entry_arity().unwrap(), 2);
        match prog.entry() {
            InputInlineRulesOrFixed::Fixed { fixed } => {
                assert_eq!(&fixed.fixed_handle.name as &str, "Constant");
                assert_eq!(fixed.arity, 2);
            }
            other => panic!("expected a synthesized Constant entry, got {other:?}"),
        }
        // And the staged relation landed in the options.
        let (handle, op, _, _) = prog.out_opts().store_relation.as_ref().unwrap();
        assert_eq!(&handle.name as &str, "t");
        assert_eq!(*op, RelationOp::Create);
    }

    /// A body-less `:create` without a schema has no columns to synthesize
    /// an entry from: it is the same no-entry error any entry-less program
    /// gets, raised at the single `InputProgram::new` construction site.
    #[test]
    fn bodyless_create_without_schema_is_no_entry() {
        let err = parse_single(":create t").unwrap_err();
        assert!(err.to_string().contains("entry"), "got: {err}");
    }

    /// A body-less `:put` (not `:create`) gets no synthesized entry either.
    #[test]
    fn bodyless_put_is_no_entry() {
        assert!(parse_single(":put t {k => v}").is_err());
    }

    /// A const rule (`<-`) parses into a Constant fixed application.
    #[test]
    fn const_rule_parses() {
        let prog = parse_single("?[a, b] <- [[1, 2], [3, 4]]").unwrap();
        assert_eq!(prog.get_entry_arity().unwrap(), 2);
    }

    /// Aggregations in rule heads resolve by value (`Aggregation` is Copy).
    #[test]
    fn aggregations_resolve() {
        let prog = parse_single("?[count(a)] := a in [1, 2, 3]").unwrap();
        assert_eq!(prog.get_entry_arity().unwrap(), 1);
        let head = prog.get_entry_out_head().unwrap();
        assert_eq!(&head[0] as &str, "count(a)");
    }

    /// An unknown aggregation is a spanned error.
    #[test]
    fn unknown_aggregation_errors() {
        assert!(parse_single("?[frobnicate(a)] := a in [1, 2]").is_err());
    }

    // ─────────────────────────────────────────────────────────────────
    // Write-time `@`: the mutation surface's own validity clause.
    // ─────────────────────────────────────────────────────────────────

    fn write_vld(src: &str) -> WriteValidity {
        let prog = parse_single(src).unwrap();
        let (_, _, _, write_vld) = prog.out_opts().store_relation.clone().unwrap();
        write_vld
    }

    /// No `@` clause at all: `WriteValidity::Now`, byte-for-byte the
    /// pre-`@` behavior (every row lands at the transaction's stamp).
    #[test]
    fn put_without_at_clause_is_now() {
        assert_eq!(
            write_vld("?[k, v] <- [[1, 'a']] :put t {k => v}"),
            WriteValidity::Now
        );
    }

    /// `@ <constant>` folds once at parse time into `WriteValidity::Fixed`,
    /// exactly like the read side's single-coordinate `@`.
    #[test]
    fn put_at_constant_is_fixed() {
        assert_eq!(
            write_vld("?[k, v] <- [[1, 'a']] :put t {k => v} @ 12345"),
            WriteValidity::Fixed(ValidityTs(Reverse(12345)))
        );
    }

    /// `@ 'NOW'` resolves through the same sentinel coercion the read side
    /// uses (`data_value_to_vld_spec`); `@ 'END'` resolves to the same
    /// `i64::MAX` coordinate but is then REFUSED — issue #62's ruling
    /// reserves the terminal tick as non-writable, since it is the instant
    /// every open-end sentinel (the temporal oracle, the Interval
    /// `DataValue`) reads as "still open." (This test once pinned `@ 'END'`
    /// resolving to `WriteValidity::Fixed(MAX_VALIDITY_TS)`; a hostile
    /// review of that behavior showed it stored a fact AT the terminal
    /// instant, which derives as a zero-width interval — see
    /// `put_at_end_sentinel_is_refused` below and
    /// `WriteValidityAtTerminalInstant`.)
    #[test]
    fn put_at_now_sentinel_resolves() {
        assert_eq!(
            write_vld("?[k, v] <- [[1, 'a']] :put t {k => v} @ 'NOW'"),
            WriteValidity::Fixed(vld())
        );
    }

    /// REFUSED: `@ 'END'` on a write names the reserved terminal tick
    /// (issue #62's ruling) rather than resolving to `WriteValidity::Fixed`.
    #[test]
    fn put_at_end_sentinel_is_refused() {
        let err = parse_single("?[k, v] <- [[1, 'a']] :put t {k => v} @ 'END'").unwrap_err();
        assert!(err.to_string().contains("reserved"), "got: {err}");
    }

    /// REFUSED: the same reservation applies to the literal microsecond
    /// value, not just the `'END'` spelling — a script that spells out
    /// `9223372036854775807` is refused identically.
    #[test]
    fn put_at_literal_max_is_refused() {
        let err = parse_single(&format!(
            "?[k, v] <- [[1, 'a']] :put t {{k => v}} @ {}",
            i64::MAX
        ))
        .unwrap_err();
        assert!(err.to_string().contains("reserved"), "got: {err}");
    }

    /// `@ <var>` naming one of the entry's own output columns becomes a
    /// per-row extractor — the backfill/import case, one instant per row.
    #[test]
    fn put_at_output_column_is_per_row() {
        match write_vld("?[k, v, ts] <- [[1, 'a', 999]] :put t {k => v} @ ts") {
            WriteValidity::PerRow(expr) => {
                assert_eq!(expr.get_binding().map(|s| s.name.as_str()), Some("ts"));
            }
            other => panic!("expected PerRow, got {other:?}"),
        }
    }

    /// `:rm` gets the identical `@` surface as `:put`.
    #[test]
    fn rm_accepts_at_clause() {
        assert_eq!(
            write_vld("?[k] <- [[1]] :rm t {k} @ 777"),
            WriteValidity::Fixed(ValidityTs(Reverse(777)))
        );
    }

    /// REFUSED: two coordinates on a write would let a script pick the
    /// system stamp — the one thing that must always be engine-minted.
    #[test]
    fn put_at_two_coordinates_is_refused() {
        let err = parse_single("?[k, v] <- [[1, 'a']] :put t {k => v} @ 1, 2").unwrap_err();
        assert!(err.to_string().contains("one coordinate"), "got: {err}");
    }

    /// REFUSED: `@` has no effect on `:ensure` (no bitemporal write
    /// happens), so it is rejected rather than silently ignored.
    #[test]
    fn ensure_with_at_clause_is_refused() {
        let err = parse_single("?[k, v] <- [[1, 'a']] :ensure t {k => v} @ 100").unwrap_err();
        assert!(err.to_string().contains("no effect"), "got: {err}");
    }

    /// REFUSED: same for `:ensure_not`.
    #[test]
    fn ensure_not_with_at_clause_is_refused() {
        let err = parse_single("?[k, v] <- [[1, 'a']] :ensure_not t {k => v} @ 100").unwrap_err();
        assert!(err.to_string().contains("no effect"), "got: {err}");
    }

    /// REFUSED: `@` naming something that is not one of the mutation's own
    /// output columns — a per-row clause must bind to a real column, not a
    /// stray identifier.
    #[test]
    fn put_at_unbound_name_is_refused() {
        assert!(parse_single("?[k, v] <- [[1, 'a']] :put t {k => v} @ nonexistent_var").is_err());
    }
}
