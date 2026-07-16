/*
 * Copyright 2023, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): the FTS query AST (`FtsExpr`/`FtsLiteral`/`FtsNear`) lives in
 * `fts/ast.rs`, as in the original; integer boosters (`word^22`) parse
 * instead of panicking (the original matched the silent `int` rule, which
 * never appears in the tree, and hit `unreachable!` on `pos_int`); the
 * remaining `unwrap`s, `panic!`s and `unreachable!`s go through the
 * typed-accessor layer; the Pratt table is a `std::sync::LazyLock` (no
 * `lazy_static` dependency); the build is depth- and operator-bounded and
 * `AND`/`OR` chains construct flat vectors (the original built a
 * left-nested spine one level deep per operator, so a long bracket-free
 * `w AND w AND …` chain aborted the process by stack overflow when the
 * tree was flattened or dropped).
 */

//! Parsing full-text-search queries: the mini-language inside an FTS
//! search's query string (`AND`/`OR`/`NOT`, `NEAR(...)`, phrase quoting,
//! `*` prefix markers, `^` boosters).
//!
//! Distinct from KyzoScript proper: this grammar is applied to a *value*
//! (the query string handed to an FTS index search) at runtime, not to the
//! script text. The same law applies — no query string can panic the
//! parser, and no query shape can exhaust the stack: group/`NOT` depth is
//! counted against the same [`NESTING_CEILING`] as the main expression
//! builder, and the total operator count against [`FTS_OPS_CEILING`].
//! Together with the flat `And`/`Or` construction in `build_infix`, this
//! is what guarantees the depth invariant documented on [`FtsExpr`].

use std::sync::LazyLock;

use itertools::Itertools;
use miette::{Diagnostic, Result, ensure};
use pest::Parser;
use pest::pratt_parser::{Op, PrattParser};
use smartstring::SmartString;
use thiserror::Error;

use crate::data::span::SourceSpan;
use crate::engines::text::ast::{FtsExpr, FtsLiteral, FtsNear};
use crate::parse::expr::parse_string;
use crate::parse::{
    ExtractSpan, IntoChildren, NESTING_CEILING, NestingTooDeep, Pair, ParseError, Rule,
    ScriptParser, single, unexpected,
};

/// Ceiling on the total `AND`/`OR`/`NOT` operator count in one FTS query.
///
/// Depth is bounded separately against [`NESTING_CEILING`]; this bounds
/// *breadth*. A bracket-free `w AND w AND …` chain nests nothing, so the
/// nesting ceiling never sees it, yet each operator costs parse work,
/// tree size, and downstream search work. Legitimate hand- or
/// machine-written queries run to a few hundred terms; the pre-fix abort
/// (left-nested spine, one stack frame per operator on flatten/drop) was
/// observed from ~15k operators up. 1024 is comfortably above every real
/// query and an order of magnitude below the old failure region — and
/// since `build_infix` now builds chains flat, the bound is a refusal of
/// absurd inputs, not the only thing standing between us and an abort.
pub(crate) const FTS_OPS_CEILING: usize = 1024;

/// An FTS query with more operators than the language allows. The breadth
/// counterpart of [`NestingTooDeep`], law-5 enforcement (refusal over
/// resource death): raised by the flat-chain operator count in
/// `parse_fts_expr`, before any tree is built.
#[derive(Debug, Error, Diagnostic)]
#[error("FTS query has more than {ceiling} operators")]
#[diagnostic(code(parser::fts_too_many_ops))]
#[diagnostic(help(
    "The operator ceiling is a limit of the FTS query language, deep enough \
     for any legitimate search. Split the query, or drop the explicit \
     operators: a plain sequence of terms is already a conjunction."
))]
pub(crate) struct FtsTooManyOps {
    /// The ceiling it crossed: [`FTS_OPS_CEILING`].
    pub(crate) ceiling: usize,
    /// The first operator past the ceiling.
    #[label("this operator passes the ceiling")]
    pub(crate) span: SourceSpan,
}

/// A numeric literal in an FTS query — a `NEAR` distance or a `^` booster —
/// that does not fit its target type. Spanned at the offending digits,
/// replacing the CozoDB original's span-less `into_diagnostic` passthrough.
#[derive(Debug, Error, Diagnostic)]
#[error("Invalid number in FTS query: {0}")]
#[diagnostic(code(parser::fts_bad_number))]
#[diagnostic(help(
    "An FTS `NEAR` distance or `^` booster must be a number that fits its \
     target type."
))]
struct BadFtsNumber(String, #[label("not a valid number here")] SourceSpan);

pub(crate) fn parse_fts_query(q: &str) -> Result<FtsExpr> {
    // An FTS query string is user text hitting the same recursive parser
    // (`fts_grouped` recurses through parens): same nesting ceiling.
    crate::parse::reject_excessive_nesting(q)?;
    // A grammar mismatch is the user's error in the FTS query text: label it
    // at pest's reported location (the same conversion the script parser
    // uses) instead of passing the span-less pest rendering through
    // `into_diagnostic`.
    let parsed = ScriptParser::parse(Rule::fts_doc, q).map_err(|e| ParseError::from_pest(e, q))?;
    let pairs = single(parsed, "the parsed fts_doc", Rule::fts_doc)?.into_inner();
    // One operator budget for the whole query, threaded through every
    // nesting level.
    let mut ops_left = FTS_OPS_CEILING;
    let pairs: Vec<_> = pairs
        .filter(|r| r.as_rule() != Rule::EOI)
        .map(|p| parse_fts_expr(p, 0, &mut ops_left))
        .try_collect()?;
    Ok(if pairs.len() == 1 {
        let mut pairs = pairs;
        // In bounds: length checked to be exactly 1.
        pairs.remove(0)
    } else {
        FtsExpr::And(pairs)
    })
}

/// Parse one `fts_expr` sitting `depth` levels deep, spending operators
/// from `ops_left`. Mirrors `build_expr_bounded` (`parse/expr.rs`): the
/// guards run on `fts_expr`'s *flat* child list, before the recursive
/// work. `NOT` counts against the nesting ceiling because each one adds a
/// tree level (`Not` boxes its operands and cannot be flattened); `AND`/
/// `OR` count only against the operator budget because `build_infix`
/// extends their vectors in place — a chain adds breadth, not depth.
fn parse_fts_expr(pair: Pair<'_>, depth: usize, ops_left: &mut usize) -> Result<FtsExpr> {
    if pair.as_rule() != Rule::fts_expr {
        // The original made this a `debug_assert!`; drift is checked in
        // every build here.
        return Err(unexpected("an fts_expr", &pair));
    }

    let mut weight = depth + 1;
    ensure!(
        weight <= NESTING_CEILING,
        NestingTooDeep {
            depth: weight,
            ceiling: NESTING_CEILING,
            span: pair.extract_span(),
        }
    );
    for child in pair.clone().into_inner() {
        match child.as_rule() {
            Rule::fts_and | Rule::fts_or | Rule::fts_not => {
                ensure!(
                    *ops_left > 0,
                    FtsTooManyOps {
                        ceiling: FTS_OPS_CEILING,
                        span: child.extract_span(),
                    }
                );
                *ops_left -= 1;
                if child.as_rule() == Rule::fts_not {
                    weight += 1;
                    ensure!(
                        weight <= NESTING_CEILING,
                        NestingTooDeep {
                            depth: weight,
                            ceiling: NESTING_CEILING,
                            span: child.extract_span(),
                        }
                    );
                }
            }
            Rule::EOI | Rule::script | Rule::query_script | Rule::query_script_inner | Rule::query_script_inner_no_bracket | Rule::imperative_script | Rule::sys_script | Rule::sys_script_inner | Rule::index_op | Rule::vec_idx_op | Rule::fts_idx_op | Rule::lsh_idx_op | Rule::index_create | Rule::index_create_adv | Rule::index_drop | Rule::compact_op | Rule::merkle_root_op | Rule::list_fixed_rules | Rule::running_op | Rule::kill_op | Rule::explain_op | Rule::verify_op | Rule::list_relations_op | Rule::list_columns_op | Rule::list_indices_op | Rule::describe_relation_op | Rule::remove_relations_op | Rule::rename_relations_op | Rule::access_level_op | Rule::access_level | Rule::trigger_relation_show_op | Rule::trigger_relation_op | Rule::trigger_clause | Rule::trigger_put | Rule::trigger_rm | Rule::trigger_replace | Rule::constraint_op | Rule::constraint_create | Rule::constraint_drop | Rule::constraint_list | Rule::rename_pair | Rule::from_clause | Rule::to_clause | Rule::index_opt_field | Rule::WHITESPACE | Rule::BLOCK_COMMENT | Rule::LINE_COMMENT | Rule::COMMENT | Rule::prog_entry | Rule::var | Rule::param | Rule::ident | Rule::underscore_ident | Rule::definitely_underscore_ident | Rule::relation_ident | Rule::search_index_ident | Rule::compound_ident | Rule::compound_or_index_ident | Rule::rule | Rule::const_rule | Rule::fixed_rule | Rule::fixed_args_list | Rule::rule_head | Rule::head_arg | Rule::aggr_arg | Rule::fixed_arg | Rule::fixed_opt_pair | Rule::fixed_rel | Rule::fixed_rule_rel | Rule::fixed_relation_rel | Rule::fixed_named_relation_rel | Rule::fixed_named_relation_arg_pair | Rule::validity_clause | Rule::spans_kw | Rule::delta_sys_kw | Rule::delta_kw | Rule::spans_clause | Rule::delta_sys_clause | Rule::delta_clause | Rule::read_validity_clause | Rule::rule_body | Rule::rule_apply | Rule::relation_named_apply | Rule::relation_apply | Rule::search_apply | Rule::disjunction | Rule::or_op | Rule::atom | Rule::unify | Rule::unify_multi | Rule::in_op | Rule::negation | Rule::not_op | Rule::apply | Rule::apply_args | Rule::named_apply_args | Rule::named_apply_pair | Rule::grouped | Rule::expr | Rule::operation | Rule::op_or | Rule::op_and | Rule::op_concat | Rule::op_add | Rule::op_field_access | Rule::op_sub | Rule::op_mul | Rule::op_div | Rule::op_mod | Rule::op_eq | Rule::op_ne | Rule::op_gt | Rule::op_lt | Rule::op_ge | Rule::op_le | Rule::op_pow | Rule::op_coalesce | Rule::unary_op | Rule::minus | Rule::negate | Rule::term | Rule::object | Rule::object_pair | Rule::list | Rule::grouping | Rule::option | Rule::out_arg | Rule::disable_magic_rewrite_option | Rule::limit_option | Rule::offset_option | Rule::sort_option | Rule::returning_option | Rule::relation_option | Rule::relation_op | Rule::relation_create | Rule::relation_replace | Rule::relation_insert | Rule::relation_delete | Rule::relation_put | Rule::relation_update | Rule::relation_rm | Rule::relation_ensure | Rule::relation_ensure_not | Rule::timeout_option | Rule::sleep_option | Rule::sort_arg | Rule::sort_dir | Rule::sort_asc | Rule::sort_desc | Rule::assert_none_option | Rule::assert_some_option | Rule::quoted_string | Rule::quoted_string_inner | Rule::char | Rule::s_quoted_string | Rule::s_quoted_string_inner | Rule::s_char | Rule::raw_string | Rule::raw_string_inner | Rule::string | Rule::boolean | Rule::null | Rule::pos_int | Rule::hex_pos_int | Rule::octo_pos_int | Rule::bin_pos_int | Rule::int | Rule::dot_float | Rule::sci_float | Rule::float | Rule::number | Rule::literal | Rule::table_schema | Rule::table_cols | Rule::table_col | Rule::col_type | Rule::col_type_with_term | Rule::any_type | Rule::int_type | Rule::float_type | Rule::string_type | Rule::bytes_type | Rule::uuid_type | Rule::bool_type | Rule::json_type | Rule::validity_type | Rule::list_type | Rule::tuple_type | Rule::vec_type | Rule::vec_el_type | Rule::imperative_stmt | Rule::imperative_sysop | Rule::imperative_clause | Rule::imperative_condition | Rule::if_chain | Rule::if_not_chain | Rule::imperative_block | Rule::break_stmt | Rule::ignore_error_script | Rule::continue_stmt | Rule::return_stmt | Rule::loop_block | Rule::temp_swap | Rule::debug_stmt | Rule::fts_doc | Rule::fts_phrase_simple | Rule::fts_phrase_group | Rule::fts_prefix_marker | Rule::fts_booster | Rule::fts_phrase | Rule::fts_near | Rule::fts_term | Rule::fts_grouped | Rule::fts_expr | Rule::fts_op | Rule::expression_script | Rule::param_list => {}
        }
    }

    PRATT_PARSER
        .map_primary(|p| build_term(p, weight, ops_left))
        .map_infix(build_infix)
        .parse(pair.into_inner())
}

fn build_infix(lhs: Result<FtsExpr>, op: Pair<'_>, rhs: Result<FtsExpr>) -> Result<FtsExpr> {
    let lhs = lhs?;
    let rhs = rhs?;
    // `And`/`Or` extend an existing vector instead of nesting a fresh
    // two-element node around it: the Pratt build is left-associative, so
    // a flat `w AND w AND …` chain arrives here with `lhs` already the
    // accumulated `And`, and pushing keeps the tree one level deep where
    // the original grew a spine as deep as the chain was long.
    // Semantically identical: `flatten` collapses `And(And(a,b),c)` to
    // `And(a,b,c)` anyway; this just never builds the deep form.
    Ok(match op.as_rule() {
        Rule::fts_and => match lhs {
            FtsExpr::And(mut es) => {
                es.push(rhs);
                FtsExpr::And(es)
            }
            lhs @ FtsExpr::Literal(_) | lhs @ FtsExpr::Near(_) | lhs @ FtsExpr::Or(_) | lhs @ FtsExpr::Not(..) => FtsExpr::And(vec![lhs, rhs]),
        },
        Rule::fts_or => match lhs {
            FtsExpr::Or(mut es) => {
                es.push(rhs);
                FtsExpr::Or(es)
            }
            lhs @ FtsExpr::Literal(_) | lhs @ FtsExpr::Near(_) | lhs @ FtsExpr::And(_) | lhs @ FtsExpr::Not(..) => FtsExpr::Or(vec![lhs, rhs]),
        },
        Rule::fts_not => FtsExpr::Not(Box::new(lhs), Box::new(rhs)),
        Rule::EOI | Rule::script | Rule::query_script | Rule::query_script_inner | Rule::query_script_inner_no_bracket | Rule::imperative_script | Rule::sys_script | Rule::sys_script_inner | Rule::index_op | Rule::vec_idx_op | Rule::fts_idx_op | Rule::lsh_idx_op | Rule::index_create | Rule::index_create_adv | Rule::index_drop | Rule::compact_op | Rule::merkle_root_op | Rule::list_fixed_rules | Rule::running_op | Rule::kill_op | Rule::explain_op | Rule::verify_op | Rule::list_relations_op | Rule::list_columns_op | Rule::list_indices_op | Rule::describe_relation_op | Rule::remove_relations_op | Rule::rename_relations_op | Rule::access_level_op | Rule::access_level | Rule::trigger_relation_show_op | Rule::trigger_relation_op | Rule::trigger_clause | Rule::trigger_put | Rule::trigger_rm | Rule::trigger_replace | Rule::constraint_op | Rule::constraint_create | Rule::constraint_drop | Rule::constraint_list | Rule::rename_pair | Rule::from_clause | Rule::to_clause | Rule::index_opt_field | Rule::WHITESPACE | Rule::BLOCK_COMMENT | Rule::LINE_COMMENT | Rule::COMMENT | Rule::prog_entry | Rule::var | Rule::param | Rule::ident | Rule::underscore_ident | Rule::definitely_underscore_ident | Rule::relation_ident | Rule::search_index_ident | Rule::compound_ident | Rule::compound_or_index_ident | Rule::rule | Rule::const_rule | Rule::fixed_rule | Rule::fixed_args_list | Rule::rule_head | Rule::head_arg | Rule::aggr_arg | Rule::fixed_arg | Rule::fixed_opt_pair | Rule::fixed_rel | Rule::fixed_rule_rel | Rule::fixed_relation_rel | Rule::fixed_named_relation_rel | Rule::fixed_named_relation_arg_pair | Rule::validity_clause | Rule::spans_kw | Rule::delta_sys_kw | Rule::delta_kw | Rule::spans_clause | Rule::delta_sys_clause | Rule::delta_clause | Rule::read_validity_clause | Rule::rule_body | Rule::rule_apply | Rule::relation_named_apply | Rule::relation_apply | Rule::search_apply | Rule::disjunction | Rule::or_op | Rule::atom | Rule::unify | Rule::unify_multi | Rule::in_op | Rule::negation | Rule::not_op | Rule::apply | Rule::apply_args | Rule::named_apply_args | Rule::named_apply_pair | Rule::grouped | Rule::expr | Rule::operation | Rule::op_or | Rule::op_and | Rule::op_concat | Rule::op_add | Rule::op_field_access | Rule::op_sub | Rule::op_mul | Rule::op_div | Rule::op_mod | Rule::op_eq | Rule::op_ne | Rule::op_gt | Rule::op_lt | Rule::op_ge | Rule::op_le | Rule::op_pow | Rule::op_coalesce | Rule::unary_op | Rule::minus | Rule::negate | Rule::term | Rule::object | Rule::object_pair | Rule::list | Rule::grouping | Rule::option | Rule::out_arg | Rule::disable_magic_rewrite_option | Rule::limit_option | Rule::offset_option | Rule::sort_option | Rule::returning_option | Rule::relation_option | Rule::relation_op | Rule::relation_create | Rule::relation_replace | Rule::relation_insert | Rule::relation_delete | Rule::relation_put | Rule::relation_update | Rule::relation_rm | Rule::relation_ensure | Rule::relation_ensure_not | Rule::timeout_option | Rule::sleep_option | Rule::sort_arg | Rule::sort_dir | Rule::sort_asc | Rule::sort_desc | Rule::assert_none_option | Rule::assert_some_option | Rule::quoted_string | Rule::quoted_string_inner | Rule::char | Rule::s_quoted_string | Rule::s_quoted_string_inner | Rule::s_char | Rule::raw_string | Rule::raw_string_inner | Rule::string | Rule::boolean | Rule::null | Rule::pos_int | Rule::hex_pos_int | Rule::octo_pos_int | Rule::bin_pos_int | Rule::int | Rule::dot_float | Rule::sci_float | Rule::float | Rule::number | Rule::literal | Rule::table_schema | Rule::table_cols | Rule::table_col | Rule::col_type | Rule::col_type_with_term | Rule::any_type | Rule::int_type | Rule::float_type | Rule::string_type | Rule::bytes_type | Rule::uuid_type | Rule::bool_type | Rule::json_type | Rule::validity_type | Rule::list_type | Rule::tuple_type | Rule::vec_type | Rule::vec_el_type | Rule::imperative_stmt | Rule::imperative_sysop | Rule::imperative_clause | Rule::imperative_condition | Rule::if_chain | Rule::if_not_chain | Rule::imperative_block | Rule::break_stmt | Rule::ignore_error_script | Rule::continue_stmt | Rule::return_stmt | Rule::loop_block | Rule::temp_swap | Rule::debug_stmt | Rule::fts_doc | Rule::fts_phrase_simple | Rule::fts_phrase_group | Rule::fts_prefix_marker | Rule::fts_booster | Rule::fts_phrase | Rule::fts_near | Rule::fts_term | Rule::fts_grouped | Rule::fts_expr | Rule::fts_op | Rule::expression_script | Rule::param_list => return Err(unexpected("an FTS operator", &op)),
    })
}

fn build_term(pair: Pair<'_>, depth: usize, ops_left: &mut usize) -> Result<FtsExpr> {
    Ok(match pair.as_rule() {
        Rule::fts_grouped => {
            let collected: Vec<_> = pair
                .into_inner()
                .map(|p| parse_fts_expr(p, depth, &mut *ops_left))
                .try_collect()?;
            if collected.len() == 1 {
                let mut collected = collected;
                // In bounds: length checked to be exactly 1.
                collected.remove(0)
            } else {
                FtsExpr::And(collected)
            }
        }
        Rule::fts_near => {
            let mut literals = vec![];
            let mut distance = 10;
            for pair in pair.into_inner() {
                match pair.as_rule() {
                    Rule::pos_int => {
                        let span = pair.extract_span();
                        let i = pair
                            .as_str()
                            .replace('_', "")
                            .parse::<i64>()
                            .map_err(|_| BadFtsNumber(pair.as_str().to_string(), span))?;
                        distance = i as u32;
                    }
                    Rule::EOI | Rule::script | Rule::query_script | Rule::query_script_inner | Rule::query_script_inner_no_bracket | Rule::imperative_script | Rule::sys_script | Rule::sys_script_inner | Rule::index_op | Rule::vec_idx_op | Rule::fts_idx_op | Rule::lsh_idx_op | Rule::index_create | Rule::index_create_adv | Rule::index_drop | Rule::compact_op | Rule::merkle_root_op | Rule::list_fixed_rules | Rule::running_op | Rule::kill_op | Rule::explain_op | Rule::verify_op | Rule::list_relations_op | Rule::list_columns_op | Rule::list_indices_op | Rule::describe_relation_op | Rule::remove_relations_op | Rule::rename_relations_op | Rule::access_level_op | Rule::access_level | Rule::trigger_relation_show_op | Rule::trigger_relation_op | Rule::trigger_clause | Rule::trigger_put | Rule::trigger_rm | Rule::trigger_replace | Rule::constraint_op | Rule::constraint_create | Rule::constraint_drop | Rule::constraint_list | Rule::rename_pair | Rule::from_clause | Rule::to_clause | Rule::index_opt_field | Rule::WHITESPACE | Rule::BLOCK_COMMENT | Rule::LINE_COMMENT | Rule::COMMENT | Rule::prog_entry | Rule::var | Rule::param | Rule::ident | Rule::underscore_ident | Rule::definitely_underscore_ident | Rule::relation_ident | Rule::search_index_ident | Rule::compound_ident | Rule::compound_or_index_ident | Rule::rule | Rule::const_rule | Rule::fixed_rule | Rule::fixed_args_list | Rule::rule_head | Rule::head_arg | Rule::aggr_arg | Rule::fixed_arg | Rule::fixed_opt_pair | Rule::fixed_rel | Rule::fixed_rule_rel | Rule::fixed_relation_rel | Rule::fixed_named_relation_rel | Rule::fixed_named_relation_arg_pair | Rule::validity_clause | Rule::spans_kw | Rule::delta_sys_kw | Rule::delta_kw | Rule::spans_clause | Rule::delta_sys_clause | Rule::delta_clause | Rule::read_validity_clause | Rule::rule_body | Rule::rule_apply | Rule::relation_named_apply | Rule::relation_apply | Rule::search_apply | Rule::disjunction | Rule::or_op | Rule::atom | Rule::unify | Rule::unify_multi | Rule::in_op | Rule::negation | Rule::not_op | Rule::apply | Rule::apply_args | Rule::named_apply_args | Rule::named_apply_pair | Rule::grouped | Rule::expr | Rule::operation | Rule::op_or | Rule::op_and | Rule::op_concat | Rule::op_add | Rule::op_field_access | Rule::op_sub | Rule::op_mul | Rule::op_div | Rule::op_mod | Rule::op_eq | Rule::op_ne | Rule::op_gt | Rule::op_lt | Rule::op_ge | Rule::op_le | Rule::op_pow | Rule::op_coalesce | Rule::unary_op | Rule::minus | Rule::negate | Rule::term | Rule::object | Rule::object_pair | Rule::list | Rule::grouping | Rule::option | Rule::out_arg | Rule::disable_magic_rewrite_option | Rule::limit_option | Rule::offset_option | Rule::sort_option | Rule::returning_option | Rule::relation_option | Rule::relation_op | Rule::relation_create | Rule::relation_replace | Rule::relation_insert | Rule::relation_delete | Rule::relation_put | Rule::relation_update | Rule::relation_rm | Rule::relation_ensure | Rule::relation_ensure_not | Rule::timeout_option | Rule::sleep_option | Rule::sort_arg | Rule::sort_dir | Rule::sort_asc | Rule::sort_desc | Rule::assert_none_option | Rule::assert_some_option | Rule::quoted_string | Rule::quoted_string_inner | Rule::char | Rule::s_quoted_string | Rule::s_quoted_string_inner | Rule::s_char | Rule::raw_string | Rule::raw_string_inner | Rule::string | Rule::boolean | Rule::null | Rule::hex_pos_int | Rule::octo_pos_int | Rule::bin_pos_int | Rule::int | Rule::dot_float | Rule::sci_float | Rule::float | Rule::number | Rule::literal | Rule::table_schema | Rule::table_cols | Rule::table_col | Rule::col_type | Rule::col_type_with_term | Rule::any_type | Rule::int_type | Rule::float_type | Rule::string_type | Rule::bytes_type | Rule::uuid_type | Rule::bool_type | Rule::json_type | Rule::validity_type | Rule::list_type | Rule::tuple_type | Rule::vec_type | Rule::vec_el_type | Rule::imperative_stmt | Rule::imperative_sysop | Rule::imperative_clause | Rule::imperative_condition | Rule::if_chain | Rule::if_not_chain | Rule::imperative_block | Rule::break_stmt | Rule::ignore_error_script | Rule::continue_stmt | Rule::return_stmt | Rule::loop_block | Rule::temp_swap | Rule::debug_stmt | Rule::fts_doc | Rule::fts_phrase_simple | Rule::fts_phrase_group | Rule::fts_prefix_marker | Rule::fts_booster | Rule::fts_phrase | Rule::fts_near | Rule::fts_term | Rule::fts_grouped | Rule::fts_expr | Rule::fts_op | Rule::fts_and | Rule::fts_or | Rule::fts_not | Rule::expression_script | Rule::param_list => literals.push(build_phrase(pair)?),
                }
            }
            FtsExpr::Near(FtsNear { literals, distance })
        }
        Rule::fts_phrase => FtsExpr::Literal(build_phrase(pair)?),
        Rule::EOI | Rule::script | Rule::query_script | Rule::query_script_inner | Rule::query_script_inner_no_bracket | Rule::imperative_script | Rule::sys_script | Rule::sys_script_inner | Rule::index_op | Rule::vec_idx_op | Rule::fts_idx_op | Rule::lsh_idx_op | Rule::index_create | Rule::index_create_adv | Rule::index_drop | Rule::compact_op | Rule::merkle_root_op | Rule::list_fixed_rules | Rule::running_op | Rule::kill_op | Rule::explain_op | Rule::verify_op | Rule::list_relations_op | Rule::list_columns_op | Rule::list_indices_op | Rule::describe_relation_op | Rule::remove_relations_op | Rule::rename_relations_op | Rule::access_level_op | Rule::access_level | Rule::trigger_relation_show_op | Rule::trigger_relation_op | Rule::trigger_clause | Rule::trigger_put | Rule::trigger_rm | Rule::trigger_replace | Rule::constraint_op | Rule::constraint_create | Rule::constraint_drop | Rule::constraint_list | Rule::rename_pair | Rule::from_clause | Rule::to_clause | Rule::index_opt_field | Rule::WHITESPACE | Rule::BLOCK_COMMENT | Rule::LINE_COMMENT | Rule::COMMENT | Rule::prog_entry | Rule::var | Rule::param | Rule::ident | Rule::underscore_ident | Rule::definitely_underscore_ident | Rule::relation_ident | Rule::search_index_ident | Rule::compound_ident | Rule::compound_or_index_ident | Rule::rule | Rule::const_rule | Rule::fixed_rule | Rule::fixed_args_list | Rule::rule_head | Rule::head_arg | Rule::aggr_arg | Rule::fixed_arg | Rule::fixed_opt_pair | Rule::fixed_rel | Rule::fixed_rule_rel | Rule::fixed_relation_rel | Rule::fixed_named_relation_rel | Rule::fixed_named_relation_arg_pair | Rule::validity_clause | Rule::spans_kw | Rule::delta_sys_kw | Rule::delta_kw | Rule::spans_clause | Rule::delta_sys_clause | Rule::delta_clause | Rule::read_validity_clause | Rule::rule_body | Rule::rule_apply | Rule::relation_named_apply | Rule::relation_apply | Rule::search_apply | Rule::disjunction | Rule::or_op | Rule::atom | Rule::unify | Rule::unify_multi | Rule::in_op | Rule::negation | Rule::not_op | Rule::apply | Rule::apply_args | Rule::named_apply_args | Rule::named_apply_pair | Rule::grouped | Rule::expr | Rule::operation | Rule::op_or | Rule::op_and | Rule::op_concat | Rule::op_add | Rule::op_field_access | Rule::op_sub | Rule::op_mul | Rule::op_div | Rule::op_mod | Rule::op_eq | Rule::op_ne | Rule::op_gt | Rule::op_lt | Rule::op_ge | Rule::op_le | Rule::op_pow | Rule::op_coalesce | Rule::unary_op | Rule::minus | Rule::negate | Rule::term | Rule::object | Rule::object_pair | Rule::list | Rule::grouping | Rule::option | Rule::out_arg | Rule::disable_magic_rewrite_option | Rule::limit_option | Rule::offset_option | Rule::sort_option | Rule::returning_option | Rule::relation_option | Rule::relation_op | Rule::relation_create | Rule::relation_replace | Rule::relation_insert | Rule::relation_delete | Rule::relation_put | Rule::relation_update | Rule::relation_rm | Rule::relation_ensure | Rule::relation_ensure_not | Rule::timeout_option | Rule::sleep_option | Rule::sort_arg | Rule::sort_dir | Rule::sort_asc | Rule::sort_desc | Rule::assert_none_option | Rule::assert_some_option | Rule::quoted_string | Rule::quoted_string_inner | Rule::char | Rule::s_quoted_string | Rule::s_quoted_string_inner | Rule::s_char | Rule::raw_string | Rule::raw_string_inner | Rule::string | Rule::boolean | Rule::null | Rule::pos_int | Rule::hex_pos_int | Rule::octo_pos_int | Rule::bin_pos_int | Rule::int | Rule::dot_float | Rule::sci_float | Rule::float | Rule::number | Rule::literal | Rule::table_schema | Rule::table_cols | Rule::table_col | Rule::col_type | Rule::col_type_with_term | Rule::any_type | Rule::int_type | Rule::float_type | Rule::string_type | Rule::bytes_type | Rule::uuid_type | Rule::bool_type | Rule::json_type | Rule::validity_type | Rule::list_type | Rule::tuple_type | Rule::vec_type | Rule::vec_el_type | Rule::imperative_stmt | Rule::imperative_sysop | Rule::imperative_clause | Rule::imperative_condition | Rule::if_chain | Rule::if_not_chain | Rule::imperative_block | Rule::break_stmt | Rule::ignore_error_script | Rule::continue_stmt | Rule::return_stmt | Rule::loop_block | Rule::temp_swap | Rule::debug_stmt | Rule::fts_doc | Rule::fts_phrase_simple | Rule::fts_phrase_group | Rule::fts_prefix_marker | Rule::fts_booster | Rule::fts_term | Rule::fts_expr | Rule::fts_op | Rule::fts_and | Rule::fts_or | Rule::fts_not | Rule::expression_script | Rule::param_list => return Err(unexpected("an FTS term", &pair)),
    })
}

fn build_phrase(pair: Pair<'_>) -> Result<FtsLiteral> {
    let mut inner = pair.children();
    let kernel = inner.expect("the phrase kernel")?;
    let core_text = match kernel.as_rule() {
        Rule::fts_phrase_group => SmartString::from(kernel.as_str().trim()),
        Rule::quoted_string | Rule::s_quoted_string | Rule::raw_string => parse_string(kernel)?,
        Rule::EOI | Rule::script | Rule::query_script | Rule::query_script_inner | Rule::query_script_inner_no_bracket | Rule::imperative_script | Rule::sys_script | Rule::sys_script_inner | Rule::index_op | Rule::vec_idx_op | Rule::fts_idx_op | Rule::lsh_idx_op | Rule::index_create | Rule::index_create_adv | Rule::index_drop | Rule::compact_op | Rule::merkle_root_op | Rule::list_fixed_rules | Rule::running_op | Rule::kill_op | Rule::explain_op | Rule::verify_op | Rule::list_relations_op | Rule::list_columns_op | Rule::list_indices_op | Rule::describe_relation_op | Rule::remove_relations_op | Rule::rename_relations_op | Rule::access_level_op | Rule::access_level | Rule::trigger_relation_show_op | Rule::trigger_relation_op | Rule::trigger_clause | Rule::trigger_put | Rule::trigger_rm | Rule::trigger_replace | Rule::constraint_op | Rule::constraint_create | Rule::constraint_drop | Rule::constraint_list | Rule::rename_pair | Rule::from_clause | Rule::to_clause | Rule::index_opt_field | Rule::WHITESPACE | Rule::BLOCK_COMMENT | Rule::LINE_COMMENT | Rule::COMMENT | Rule::prog_entry | Rule::var | Rule::param | Rule::ident | Rule::underscore_ident | Rule::definitely_underscore_ident | Rule::relation_ident | Rule::search_index_ident | Rule::compound_ident | Rule::compound_or_index_ident | Rule::rule | Rule::const_rule | Rule::fixed_rule | Rule::fixed_args_list | Rule::rule_head | Rule::head_arg | Rule::aggr_arg | Rule::fixed_arg | Rule::fixed_opt_pair | Rule::fixed_rel | Rule::fixed_rule_rel | Rule::fixed_relation_rel | Rule::fixed_named_relation_rel | Rule::fixed_named_relation_arg_pair | Rule::validity_clause | Rule::spans_kw | Rule::delta_sys_kw | Rule::delta_kw | Rule::spans_clause | Rule::delta_sys_clause | Rule::delta_clause | Rule::read_validity_clause | Rule::rule_body | Rule::rule_apply | Rule::relation_named_apply | Rule::relation_apply | Rule::search_apply | Rule::disjunction | Rule::or_op | Rule::atom | Rule::unify | Rule::unify_multi | Rule::in_op | Rule::negation | Rule::not_op | Rule::apply | Rule::apply_args | Rule::named_apply_args | Rule::named_apply_pair | Rule::grouped | Rule::expr | Rule::operation | Rule::op_or | Rule::op_and | Rule::op_concat | Rule::op_add | Rule::op_field_access | Rule::op_sub | Rule::op_mul | Rule::op_div | Rule::op_mod | Rule::op_eq | Rule::op_ne | Rule::op_gt | Rule::op_lt | Rule::op_ge | Rule::op_le | Rule::op_pow | Rule::op_coalesce | Rule::unary_op | Rule::minus | Rule::negate | Rule::term | Rule::object | Rule::object_pair | Rule::list | Rule::grouping | Rule::option | Rule::out_arg | Rule::disable_magic_rewrite_option | Rule::limit_option | Rule::offset_option | Rule::sort_option | Rule::returning_option | Rule::relation_option | Rule::relation_op | Rule::relation_create | Rule::relation_replace | Rule::relation_insert | Rule::relation_delete | Rule::relation_put | Rule::relation_update | Rule::relation_rm | Rule::relation_ensure | Rule::relation_ensure_not | Rule::timeout_option | Rule::sleep_option | Rule::sort_arg | Rule::sort_dir | Rule::sort_asc | Rule::sort_desc | Rule::assert_none_option | Rule::assert_some_option | Rule::quoted_string_inner | Rule::char | Rule::s_quoted_string_inner | Rule::s_char | Rule::raw_string_inner | Rule::string | Rule::boolean | Rule::null | Rule::pos_int | Rule::hex_pos_int | Rule::octo_pos_int | Rule::bin_pos_int | Rule::int | Rule::dot_float | Rule::sci_float | Rule::float | Rule::number | Rule::literal | Rule::table_schema | Rule::table_cols | Rule::table_col | Rule::col_type | Rule::col_type_with_term | Rule::any_type | Rule::int_type | Rule::float_type | Rule::string_type | Rule::bytes_type | Rule::uuid_type | Rule::bool_type | Rule::json_type | Rule::validity_type | Rule::list_type | Rule::tuple_type | Rule::vec_type | Rule::vec_el_type | Rule::imperative_stmt | Rule::imperative_sysop | Rule::imperative_clause | Rule::imperative_condition | Rule::if_chain | Rule::if_not_chain | Rule::imperative_block | Rule::break_stmt | Rule::ignore_error_script | Rule::continue_stmt | Rule::return_stmt | Rule::loop_block | Rule::temp_swap | Rule::debug_stmt | Rule::fts_doc | Rule::fts_phrase_simple | Rule::fts_prefix_marker | Rule::fts_booster | Rule::fts_phrase | Rule::fts_near | Rule::fts_term | Rule::fts_grouped | Rule::fts_expr | Rule::fts_op | Rule::fts_and | Rule::fts_or | Rule::fts_not | Rule::expression_script | Rule::param_list => return Err(unexpected("a phrase kernel", &kernel)),
    };
    let mut is_quoted = false;
    let mut booster = 1.0;
    for pair in inner {
        match pair.as_rule() {
            Rule::fts_prefix_marker => is_quoted = true,
            Rule::fts_booster => {
                let boosted = pair.children().expect("the booster value")?;
                match boosted.as_rule() {
                    Rule::dot_float => {
                        let span = boosted.extract_span();
                        let f = boosted
                            .as_str()
                            .replace('_', "")
                            .parse::<f64>()
                            .map_err(|_| BadFtsNumber(boosted.as_str().to_string(), span))?;
                        booster = f;
                    }
                    // `pos_int`: the CozoDB original matched `Rule::int`
                    // here, a silent grammar rule that never appears in the
                    // tree, so every integer booster (`word^22`) hit its
                    // `unreachable!` and aborted.
                    Rule::pos_int => {
                        let span = boosted.extract_span();
                        let i = boosted
                            .as_str()
                            .replace('_', "")
                            .parse::<i64>()
                            .map_err(|_| BadFtsNumber(boosted.as_str().to_string(), span))?;
                        booster = i as f64;
                    }
                    Rule::EOI | Rule::script | Rule::query_script | Rule::query_script_inner | Rule::query_script_inner_no_bracket | Rule::imperative_script | Rule::sys_script | Rule::sys_script_inner | Rule::index_op | Rule::vec_idx_op | Rule::fts_idx_op | Rule::lsh_idx_op | Rule::index_create | Rule::index_create_adv | Rule::index_drop | Rule::compact_op | Rule::merkle_root_op | Rule::list_fixed_rules | Rule::running_op | Rule::kill_op | Rule::explain_op | Rule::verify_op | Rule::list_relations_op | Rule::list_columns_op | Rule::list_indices_op | Rule::describe_relation_op | Rule::remove_relations_op | Rule::rename_relations_op | Rule::access_level_op | Rule::access_level | Rule::trigger_relation_show_op | Rule::trigger_relation_op | Rule::trigger_clause | Rule::trigger_put | Rule::trigger_rm | Rule::trigger_replace | Rule::constraint_op | Rule::constraint_create | Rule::constraint_drop | Rule::constraint_list | Rule::rename_pair | Rule::from_clause | Rule::to_clause | Rule::index_opt_field | Rule::WHITESPACE | Rule::BLOCK_COMMENT | Rule::LINE_COMMENT | Rule::COMMENT | Rule::prog_entry | Rule::var | Rule::param | Rule::ident | Rule::underscore_ident | Rule::definitely_underscore_ident | Rule::relation_ident | Rule::search_index_ident | Rule::compound_ident | Rule::compound_or_index_ident | Rule::rule | Rule::const_rule | Rule::fixed_rule | Rule::fixed_args_list | Rule::rule_head | Rule::head_arg | Rule::aggr_arg | Rule::fixed_arg | Rule::fixed_opt_pair | Rule::fixed_rel | Rule::fixed_rule_rel | Rule::fixed_relation_rel | Rule::fixed_named_relation_rel | Rule::fixed_named_relation_arg_pair | Rule::validity_clause | Rule::spans_kw | Rule::delta_sys_kw | Rule::delta_kw | Rule::spans_clause | Rule::delta_sys_clause | Rule::delta_clause | Rule::read_validity_clause | Rule::rule_body | Rule::rule_apply | Rule::relation_named_apply | Rule::relation_apply | Rule::search_apply | Rule::disjunction | Rule::or_op | Rule::atom | Rule::unify | Rule::unify_multi | Rule::in_op | Rule::negation | Rule::not_op | Rule::apply | Rule::apply_args | Rule::named_apply_args | Rule::named_apply_pair | Rule::grouped | Rule::expr | Rule::operation | Rule::op_or | Rule::op_and | Rule::op_concat | Rule::op_add | Rule::op_field_access | Rule::op_sub | Rule::op_mul | Rule::op_div | Rule::op_mod | Rule::op_eq | Rule::op_ne | Rule::op_gt | Rule::op_lt | Rule::op_ge | Rule::op_le | Rule::op_pow | Rule::op_coalesce | Rule::unary_op | Rule::minus | Rule::negate | Rule::term | Rule::object | Rule::object_pair | Rule::list | Rule::grouping | Rule::option | Rule::out_arg | Rule::disable_magic_rewrite_option | Rule::limit_option | Rule::offset_option | Rule::sort_option | Rule::returning_option | Rule::relation_option | Rule::relation_op | Rule::relation_create | Rule::relation_replace | Rule::relation_insert | Rule::relation_delete | Rule::relation_put | Rule::relation_update | Rule::relation_rm | Rule::relation_ensure | Rule::relation_ensure_not | Rule::timeout_option | Rule::sleep_option | Rule::sort_arg | Rule::sort_dir | Rule::sort_asc | Rule::sort_desc | Rule::assert_none_option | Rule::assert_some_option | Rule::quoted_string | Rule::quoted_string_inner | Rule::char | Rule::s_quoted_string | Rule::s_quoted_string_inner | Rule::s_char | Rule::raw_string | Rule::raw_string_inner | Rule::string | Rule::boolean | Rule::null | Rule::hex_pos_int | Rule::octo_pos_int | Rule::bin_pos_int | Rule::int | Rule::sci_float | Rule::float | Rule::number | Rule::literal | Rule::table_schema | Rule::table_cols | Rule::table_col | Rule::col_type | Rule::col_type_with_term | Rule::any_type | Rule::int_type | Rule::float_type | Rule::string_type | Rule::bytes_type | Rule::uuid_type | Rule::bool_type | Rule::json_type | Rule::validity_type | Rule::list_type | Rule::tuple_type | Rule::vec_type | Rule::vec_el_type | Rule::imperative_stmt | Rule::imperative_sysop | Rule::imperative_clause | Rule::imperative_condition | Rule::if_chain | Rule::if_not_chain | Rule::imperative_block | Rule::break_stmt | Rule::ignore_error_script | Rule::continue_stmt | Rule::return_stmt | Rule::loop_block | Rule::temp_swap | Rule::debug_stmt | Rule::fts_doc | Rule::fts_phrase_simple | Rule::fts_phrase_group | Rule::fts_prefix_marker | Rule::fts_booster | Rule::fts_phrase | Rule::fts_near | Rule::fts_term | Rule::fts_grouped | Rule::fts_expr | Rule::fts_op | Rule::fts_and | Rule::fts_or | Rule::fts_not | Rule::expression_script | Rule::param_list => return Err(unexpected("a booster value", &boosted)),
                }
            }
            Rule::EOI | Rule::script | Rule::query_script | Rule::query_script_inner | Rule::query_script_inner_no_bracket | Rule::imperative_script | Rule::sys_script | Rule::sys_script_inner | Rule::index_op | Rule::vec_idx_op | Rule::fts_idx_op | Rule::lsh_idx_op | Rule::index_create | Rule::index_create_adv | Rule::index_drop | Rule::compact_op | Rule::merkle_root_op | Rule::list_fixed_rules | Rule::running_op | Rule::kill_op | Rule::explain_op | Rule::verify_op | Rule::list_relations_op | Rule::list_columns_op | Rule::list_indices_op | Rule::describe_relation_op | Rule::remove_relations_op | Rule::rename_relations_op | Rule::access_level_op | Rule::access_level | Rule::trigger_relation_show_op | Rule::trigger_relation_op | Rule::trigger_clause | Rule::trigger_put | Rule::trigger_rm | Rule::trigger_replace | Rule::constraint_op | Rule::constraint_create | Rule::constraint_drop | Rule::constraint_list | Rule::rename_pair | Rule::from_clause | Rule::to_clause | Rule::index_opt_field | Rule::WHITESPACE | Rule::BLOCK_COMMENT | Rule::LINE_COMMENT | Rule::COMMENT | Rule::prog_entry | Rule::var | Rule::param | Rule::ident | Rule::underscore_ident | Rule::definitely_underscore_ident | Rule::relation_ident | Rule::search_index_ident | Rule::compound_ident | Rule::compound_or_index_ident | Rule::rule | Rule::const_rule | Rule::fixed_rule | Rule::fixed_args_list | Rule::rule_head | Rule::head_arg | Rule::aggr_arg | Rule::fixed_arg | Rule::fixed_opt_pair | Rule::fixed_rel | Rule::fixed_rule_rel | Rule::fixed_relation_rel | Rule::fixed_named_relation_rel | Rule::fixed_named_relation_arg_pair | Rule::validity_clause | Rule::spans_kw | Rule::delta_sys_kw | Rule::delta_kw | Rule::spans_clause | Rule::delta_sys_clause | Rule::delta_clause | Rule::read_validity_clause | Rule::rule_body | Rule::rule_apply | Rule::relation_named_apply | Rule::relation_apply | Rule::search_apply | Rule::disjunction | Rule::or_op | Rule::atom | Rule::unify | Rule::unify_multi | Rule::in_op | Rule::negation | Rule::not_op | Rule::apply | Rule::apply_args | Rule::named_apply_args | Rule::named_apply_pair | Rule::grouped | Rule::expr | Rule::operation | Rule::op_or | Rule::op_and | Rule::op_concat | Rule::op_add | Rule::op_field_access | Rule::op_sub | Rule::op_mul | Rule::op_div | Rule::op_mod | Rule::op_eq | Rule::op_ne | Rule::op_gt | Rule::op_lt | Rule::op_ge | Rule::op_le | Rule::op_pow | Rule::op_coalesce | Rule::unary_op | Rule::minus | Rule::negate | Rule::term | Rule::object | Rule::object_pair | Rule::list | Rule::grouping | Rule::option | Rule::out_arg | Rule::disable_magic_rewrite_option | Rule::limit_option | Rule::offset_option | Rule::sort_option | Rule::returning_option | Rule::relation_option | Rule::relation_op | Rule::relation_create | Rule::relation_replace | Rule::relation_insert | Rule::relation_delete | Rule::relation_put | Rule::relation_update | Rule::relation_rm | Rule::relation_ensure | Rule::relation_ensure_not | Rule::timeout_option | Rule::sleep_option | Rule::sort_arg | Rule::sort_dir | Rule::sort_asc | Rule::sort_desc | Rule::assert_none_option | Rule::assert_some_option | Rule::quoted_string | Rule::quoted_string_inner | Rule::char | Rule::s_quoted_string | Rule::s_quoted_string_inner | Rule::s_char | Rule::raw_string | Rule::raw_string_inner | Rule::string | Rule::boolean | Rule::null | Rule::pos_int | Rule::hex_pos_int | Rule::octo_pos_int | Rule::bin_pos_int | Rule::int | Rule::dot_float | Rule::sci_float | Rule::float | Rule::number | Rule::literal | Rule::table_schema | Rule::table_cols | Rule::table_col | Rule::col_type | Rule::col_type_with_term | Rule::any_type | Rule::int_type | Rule::float_type | Rule::string_type | Rule::bytes_type | Rule::uuid_type | Rule::bool_type | Rule::json_type | Rule::validity_type | Rule::list_type | Rule::tuple_type | Rule::vec_type | Rule::vec_el_type | Rule::imperative_stmt | Rule::imperative_sysop | Rule::imperative_clause | Rule::imperative_condition | Rule::if_chain | Rule::if_not_chain | Rule::imperative_block | Rule::break_stmt | Rule::ignore_error_script | Rule::continue_stmt | Rule::return_stmt | Rule::loop_block | Rule::temp_swap | Rule::debug_stmt | Rule::fts_doc | Rule::fts_phrase_simple | Rule::fts_phrase_group | Rule::fts_phrase | Rule::fts_near | Rule::fts_term | Rule::fts_grouped | Rule::fts_expr | Rule::fts_op | Rule::fts_and | Rule::fts_or | Rule::fts_not | Rule::expression_script | Rule::param_list => return Err(unexpected("a phrase modifier", &pair)),
        }
    }
    Ok(FtsLiteral {
        value: core_text,
        is_prefix: is_quoted,
        booster: booster.into(),
    })
}

static PRATT_PARSER: LazyLock<PrattParser<Rule>> = LazyLock::new(|| {
    use pest::pratt_parser::Assoc::*;

    PrattParser::new()
        .op(Op::infix(Rule::fts_not, Left))
        .op(Op::infix(Rule::fts_and, Left))
        .op(Op::infix(Rule::fts_or, Left))
});

#[cfg(test)]
mod tests {
    use super::{FTS_OPS_CEILING, FtsTooManyOps, parse_fts_query};
    use crate::engines::text::ast::{FtsExpr, FtsNear};
    use crate::parse::{NESTING_CEILING, NestingTooDeep};

    #[test]
    fn test_parse() {
        let src = " hello world OR bye bye world";
        let res = parse_fts_query(src).unwrap().flatten();
        assert!(matches!(res, FtsExpr::Or(_)));
        let src = " hello world AND bye bye world";
        let res = parse_fts_query(src).unwrap().flatten();
        assert!(matches!(res, FtsExpr::And(_)));
        let src = " hello world NOT bye bye NOT 'ok, mates'";
        let res = parse_fts_query(src).unwrap().flatten();
        assert!(matches!(res, FtsExpr::Not(_, _)));
        let src = " NEAR(abc def \"ghi\"^22.8) ";
        let res = parse_fts_query(src).unwrap().flatten();
        assert!(matches!(res, FtsExpr::Near(FtsNear { distance: 10, .. })));
    }

    /// An integer booster is valid syntax; the CozoDB original panicked on
    /// it (its dispatch matched the silent `int` rule instead of the
    /// `pos_int` the tree actually carries).
    #[test]
    fn integer_booster_parses() {
        let res = parse_fts_query("hello^22").unwrap().flatten();
        match res {
            FtsExpr::Literal(l) => assert_eq!(l.booster.0, 22.0),
            r @ FtsExpr::Near(_) | r @ FtsExpr::And(_) | r @ FtsExpr::Or(_) | r @ FtsExpr::Not(..) => panic!("expected a literal, got {r:?}"),
        }
    }

    /// The reviewer's abort shape: a ~300 KiB bracket-free `w AND w AND …`
    /// chain. Brackets never nest, so the pre-parse scan can't see it; the
    /// CozoDB original built a left-nested spine one level per operator
    /// and aborted the process (stack overflow) flattening/dropping it.
    /// Now: a typed, spanned refusal, in linear time.
    #[test]
    fn huge_flat_and_chain_is_refused_not_aborted() {
        let src = "w AND ".repeat(50_000) + "w"; // ~300 KiB, 50k operators
        assert!(src.len() > 300_000);
        let err = parse_fts_query(&src).unwrap_err();
        assert!(
            err.downcast_ref::<FtsTooManyOps>().is_some(),
            "expected the typed FtsTooManyOps refusal, got: {err:?}"
        );
        // The same shape spelled with the other operators refuses too.
        for chain in ["w OR ".repeat(50_000) + "w", "w, ".repeat(50_000) + "w"] {
            let err = parse_fts_query(&chain).unwrap_err();
            assert!(err.downcast_ref::<FtsTooManyOps>().is_some());
        }
        // And the budget is shared across nesting levels: hiding the chain
        // inside a group changes nothing.
        let err = parse_fts_query(&format!("({})", "w AND ".repeat(50_000) + "w")).unwrap_err();
        assert!(err.downcast_ref::<FtsTooManyOps>().is_some());
    }

    /// `NOT` boxes its operands, so a `NOT` chain grows the tree one level
    /// per operator — it counts against the *nesting* ceiling, and a chain
    /// past it is the same typed refusal as everywhere else in the language.
    #[test]
    fn long_not_chain_is_refused_as_too_deep() {
        let src = vec!["w"; NESTING_CEILING + 2].join(" NOT ");
        let err = parse_fts_query(&src).unwrap_err();
        assert!(
            err.downcast_ref::<NestingTooDeep>().is_some(),
            "expected the typed NestingTooDeep refusal, got: {err:?}"
        );
    }

    /// The ceilings refuse the absurd, not the legitimate: a 100-operator
    /// chain (a long hand-assembled search) parses, and the flat `And`
    /// construction means it arrives one level deep with 101 children.
    #[test]
    fn legitimate_100_op_chain_parses() {
        let src = vec!["w"; 101].join(" AND ");
        match parse_fts_query(&src).unwrap() {
            FtsExpr::And(v) => assert_eq!(v.len(), 101),
            other @ FtsExpr::Literal(_) | other @ FtsExpr::Near(_) | other @ FtsExpr::Or(_) | other @ FtsExpr::Not(..) => panic!("expected a flat And, got {other:?}"),
        }
        let src = vec!["w"; 101].join(" OR ");
        match parse_fts_query(&src).unwrap() {
            FtsExpr::Or(v) => assert_eq!(v.len(), 101),
            other @ FtsExpr::Literal(_) | other @ FtsExpr::Near(_) | other @ FtsExpr::And(_) | other @ FtsExpr::Not(..) => panic!("expected a flat Or, got {other:?}"),
        }
        // Exactly at the operator ceiling still parses; one over refuses.
        let src = vec!["w"; FTS_OPS_CEILING + 1].join(" AND ");
        assert!(parse_fts_query(&src).is_ok());
        let src = vec!["w"; FTS_OPS_CEILING + 2].join(" AND ");
        assert!(parse_fts_query(&src).is_err());
    }

    /// Flat construction is semantically invisible: mixed operators still
    /// group by precedence and survive `flatten` unchanged in meaning.
    #[test]
    fn flat_construction_preserves_shape() {
        // Grouped chain: `(a AND b) AND c` is the same conjunction.
        let res = parse_fts_query("(a AND b) AND c").unwrap().flatten();
        match res {
            FtsExpr::And(v) => assert_eq!(v.len(), 3),
            other @ FtsExpr::Literal(_) | other @ FtsExpr::Near(_) | other @ FtsExpr::Or(_) | other @ FtsExpr::Not(..) => panic!("expected And, got {other:?}"),
        }
        // NOT still nests: `a NOT b AND c` keeps its binary shape.
        let res = parse_fts_query("a NOT b AND c").unwrap().flatten();
        assert!(matches!(res, FtsExpr::Not(_, _)));
    }
}
