/*
 *  Copyright 2023, The Cozo Project Authors.
 *
 *  This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 *  If a copy of the MPL was not distributed with this file,
 *  You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): grammar-shape `unwrap`s and `unreachable!` dispatch arms go
 * through the typed-accessor layer; `either::Either` is replaced by the
 * named [`QueryOrRelation`] sum type (the original used opposite
 * `Left`/`Right` orientations at its two use sites).
 */

//! Parsing imperative scripts: queries chained under control flow.
//!
//! Each embedded `{…}` query parses through the same [`parse_query`] proof
//! as a standalone script — an imperative program is a *composition of
//! proven programs*, plus control structure (`%if`, `%loop`, `%return`,
//! `%swap`, `%debug`) over named temporary relations.

use std::collections::BTreeMap;
use std::sync::Arc;

use itertools::Itertools;
use miette::Result;
use smartstring::SmartString;

use crate::data::program::FixedRule;
use crate::data::value::{DataValue, ValidityTs};
use crate::parse::query::parse_query;
use crate::parse::sys::parse_sys;
use crate::parse::{
    ExtractSpan, ImperativeProgram, ImperativeStmt, ImperativeStmtClause, ImperativeSysop,
    IntoChildren, Pair, QueryOrRelation, Rule, unexpected,
};

pub(crate) fn parse_imperative_block(
    src: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    fixed_rules: &BTreeMap<String, Arc<dyn FixedRule>>,
    cur_vld: ValidityTs,
) -> Result<ImperativeProgram> {
    let mut collected = vec![];

    for pair in src.into_inner() {
        if pair.as_rule() == Rule::EOI {
            break;
        }
        collected.push(parse_imperative_stmt(
            pair,
            param_pool,
            fixed_rules,
            cur_vld,
        )?);
    }

    Ok(collected)
}

fn parse_imperative_stmt(
    pair: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    fixed_rules: &BTreeMap<String, Arc<dyn FixedRule>>,
    cur_vld: ValidityTs,
) -> Result<ImperativeStmt> {
    Ok(match pair.as_rule() {
        Rule::break_stmt => {
            let span = pair.extract_span();
            let target = pair
                .into_inner()
                .next()
                .map(|p| SmartString::from(p.as_str()));
            ImperativeStmt::Break { target, span }
        }
        Rule::continue_stmt => {
            let span = pair.extract_span();
            let target = pair
                .into_inner()
                .next()
                .map(|p| SmartString::from(p.as_str()));
            ImperativeStmt::Continue { target, span }
        }
        Rule::return_stmt => {
            let mut rets = vec![];
            for p in pair.into_inner() {
                match p.as_rule() {
                    Rule::ident | Rule::underscore_ident => {
                        let rel = SmartString::from(p.as_str());
                        rets.push(QueryOrRelation::Relation(rel));
                    }
                    Rule::query_script_inner => {
                        let mut src = p.children();
                        let prog = parse_query(
                            src.expect("the returned query")?.into_inner(),
                            param_pool,
                            fixed_rules,
                            cur_vld,
                        )?;
                        let store_as = src.next().map(|p| SmartString::from(p.as_str().trim()));
                        rets.push(QueryOrRelation::Query(Box::new(ImperativeStmtClause {
                            prog,
                            store_as,
                        })))
                    }
                    Rule::EOI | Rule::script | Rule::query_script | Rule::query_script_inner_no_bracket | Rule::imperative_script | Rule::sys_script | Rule::sys_script_inner | Rule::index_op | Rule::vec_idx_op | Rule::fts_idx_op | Rule::lsh_idx_op | Rule::index_create | Rule::index_create_adv | Rule::index_drop | Rule::compact_op | Rule::merkle_root_op | Rule::list_fixed_rules | Rule::running_op | Rule::kill_op | Rule::explain_op | Rule::verify_op | Rule::list_relations_op | Rule::list_columns_op | Rule::list_indices_op | Rule::describe_relation_op | Rule::remove_relations_op | Rule::rename_relations_op | Rule::access_level_op | Rule::access_level | Rule::trigger_relation_show_op | Rule::trigger_relation_op | Rule::trigger_clause | Rule::trigger_put | Rule::trigger_rm | Rule::trigger_replace | Rule::constraint_op | Rule::constraint_create | Rule::constraint_drop | Rule::constraint_list | Rule::rename_pair | Rule::from_clause | Rule::to_clause | Rule::index_opt_field | Rule::WHITESPACE | Rule::BLOCK_COMMENT | Rule::LINE_COMMENT | Rule::COMMENT | Rule::prog_entry | Rule::var | Rule::param | Rule::definitely_underscore_ident | Rule::relation_ident | Rule::search_index_ident | Rule::compound_ident | Rule::compound_or_index_ident | Rule::rule | Rule::const_rule | Rule::fixed_rule | Rule::fixed_args_list | Rule::rule_head | Rule::head_arg | Rule::aggr_arg | Rule::fixed_arg | Rule::fixed_opt_pair | Rule::fixed_rel | Rule::fixed_rule_rel | Rule::fixed_relation_rel | Rule::fixed_named_relation_rel | Rule::fixed_named_relation_arg_pair | Rule::validity_clause | Rule::spans_kw | Rule::delta_sys_kw | Rule::delta_kw | Rule::spans_clause | Rule::delta_sys_clause | Rule::delta_clause | Rule::read_validity_clause | Rule::rule_body | Rule::rule_apply | Rule::relation_named_apply | Rule::relation_apply | Rule::search_apply | Rule::disjunction | Rule::or_op | Rule::atom | Rule::unify | Rule::unify_multi | Rule::in_op | Rule::negation | Rule::not_op | Rule::apply | Rule::apply_args | Rule::named_apply_args | Rule::named_apply_pair | Rule::grouped | Rule::expr | Rule::operation | Rule::op_or | Rule::op_and | Rule::op_concat | Rule::op_add | Rule::op_field_access | Rule::op_sub | Rule::op_mul | Rule::op_div | Rule::op_mod | Rule::op_eq | Rule::op_ne | Rule::op_gt | Rule::op_lt | Rule::op_ge | Rule::op_le | Rule::op_pow | Rule::op_coalesce | Rule::unary_op | Rule::minus | Rule::negate | Rule::term | Rule::object | Rule::object_pair | Rule::list | Rule::grouping | Rule::option | Rule::out_arg | Rule::disable_magic_rewrite_option | Rule::limit_option | Rule::offset_option | Rule::sort_option | Rule::returning_option | Rule::relation_option | Rule::relation_op | Rule::relation_create | Rule::relation_replace | Rule::relation_insert | Rule::relation_delete | Rule::relation_put | Rule::relation_update | Rule::relation_rm | Rule::relation_ensure | Rule::relation_ensure_not | Rule::timeout_option | Rule::sleep_option | Rule::sort_arg | Rule::sort_dir | Rule::sort_asc | Rule::sort_desc | Rule::assert_none_option | Rule::assert_some_option | Rule::quoted_string | Rule::quoted_string_inner | Rule::char | Rule::s_quoted_string | Rule::s_quoted_string_inner | Rule::s_char | Rule::raw_string | Rule::raw_string_inner | Rule::string | Rule::boolean | Rule::null | Rule::pos_int | Rule::hex_pos_int | Rule::octo_pos_int | Rule::bin_pos_int | Rule::int | Rule::dot_float | Rule::sci_float | Rule::float | Rule::number | Rule::literal | Rule::table_schema | Rule::table_cols | Rule::table_col | Rule::col_type | Rule::col_type_with_term | Rule::any_type | Rule::int_type | Rule::float_type | Rule::string_type | Rule::bytes_type | Rule::uuid_type | Rule::bool_type | Rule::json_type | Rule::validity_type | Rule::list_type | Rule::tuple_type | Rule::vec_type | Rule::vec_el_type | Rule::imperative_stmt | Rule::imperative_sysop | Rule::imperative_clause | Rule::imperative_condition | Rule::if_chain | Rule::if_not_chain | Rule::imperative_block | Rule::break_stmt | Rule::ignore_error_script | Rule::continue_stmt | Rule::return_stmt | Rule::loop_block | Rule::temp_swap | Rule::debug_stmt | Rule::fts_doc | Rule::fts_phrase_simple | Rule::fts_phrase_group | Rule::fts_prefix_marker | Rule::fts_booster | Rule::fts_phrase | Rule::fts_near | Rule::fts_term | Rule::fts_grouped | Rule::fts_expr | Rule::fts_op | Rule::fts_and | Rule::fts_or | Rule::fts_not | Rule::expression_script | Rule::param_list => return Err(unexpected("a returned query or relation name", &p)),
                }
            }
            ImperativeStmt::Return { returns: rets }
        }
        Rule::if_chain | Rule::if_not_chain => {
            let negated = pair.as_rule() == Rule::if_not_chain;
            let mut inner = pair.children();
            let condition = inner.expect("the condition")?;
            let cond = match condition.as_rule() {
                Rule::underscore_ident => {
                    QueryOrRelation::Relation(SmartString::from(condition.as_str()))
                }
                Rule::imperative_clause => {
                    let mut src = condition.children();
                    let prog = parse_query(
                        src.expect("the condition query")?.into_inner(),
                        param_pool,
                        fixed_rules,
                        cur_vld,
                    )?;
                    let store_as = src.next().map(|p| SmartString::from(p.as_str().trim()));
                    QueryOrRelation::Query(Box::new(ImperativeStmtClause { prog, store_as }))
                }
                Rule::EOI | Rule::script | Rule::query_script | Rule::query_script_inner | Rule::query_script_inner_no_bracket | Rule::imperative_script | Rule::sys_script | Rule::sys_script_inner | Rule::index_op | Rule::vec_idx_op | Rule::fts_idx_op | Rule::lsh_idx_op | Rule::index_create | Rule::index_create_adv | Rule::index_drop | Rule::compact_op | Rule::merkle_root_op | Rule::list_fixed_rules | Rule::running_op | Rule::kill_op | Rule::explain_op | Rule::verify_op | Rule::list_relations_op | Rule::list_columns_op | Rule::list_indices_op | Rule::describe_relation_op | Rule::remove_relations_op | Rule::rename_relations_op | Rule::access_level_op | Rule::access_level | Rule::trigger_relation_show_op | Rule::trigger_relation_op | Rule::trigger_clause | Rule::trigger_put | Rule::trigger_rm | Rule::trigger_replace | Rule::constraint_op | Rule::constraint_create | Rule::constraint_drop | Rule::constraint_list | Rule::rename_pair | Rule::from_clause | Rule::to_clause | Rule::index_opt_field | Rule::WHITESPACE | Rule::BLOCK_COMMENT | Rule::LINE_COMMENT | Rule::COMMENT | Rule::prog_entry | Rule::var | Rule::param | Rule::ident | Rule::definitely_underscore_ident | Rule::relation_ident | Rule::search_index_ident | Rule::compound_ident | Rule::compound_or_index_ident | Rule::rule | Rule::const_rule | Rule::fixed_rule | Rule::fixed_args_list | Rule::rule_head | Rule::head_arg | Rule::aggr_arg | Rule::fixed_arg | Rule::fixed_opt_pair | Rule::fixed_rel | Rule::fixed_rule_rel | Rule::fixed_relation_rel | Rule::fixed_named_relation_rel | Rule::fixed_named_relation_arg_pair | Rule::validity_clause | Rule::spans_kw | Rule::delta_sys_kw | Rule::delta_kw | Rule::spans_clause | Rule::delta_sys_clause | Rule::delta_clause | Rule::read_validity_clause | Rule::rule_body | Rule::rule_apply | Rule::relation_named_apply | Rule::relation_apply | Rule::search_apply | Rule::disjunction | Rule::or_op | Rule::atom | Rule::unify | Rule::unify_multi | Rule::in_op | Rule::negation | Rule::not_op | Rule::apply | Rule::apply_args | Rule::named_apply_args | Rule::named_apply_pair | Rule::grouped | Rule::expr | Rule::operation | Rule::op_or | Rule::op_and | Rule::op_concat | Rule::op_add | Rule::op_field_access | Rule::op_sub | Rule::op_mul | Rule::op_div | Rule::op_mod | Rule::op_eq | Rule::op_ne | Rule::op_gt | Rule::op_lt | Rule::op_ge | Rule::op_le | Rule::op_pow | Rule::op_coalesce | Rule::unary_op | Rule::minus | Rule::negate | Rule::term | Rule::object | Rule::object_pair | Rule::list | Rule::grouping | Rule::option | Rule::out_arg | Rule::disable_magic_rewrite_option | Rule::limit_option | Rule::offset_option | Rule::sort_option | Rule::returning_option | Rule::relation_option | Rule::relation_op | Rule::relation_create | Rule::relation_replace | Rule::relation_insert | Rule::relation_delete | Rule::relation_put | Rule::relation_update | Rule::relation_rm | Rule::relation_ensure | Rule::relation_ensure_not | Rule::timeout_option | Rule::sleep_option | Rule::sort_arg | Rule::sort_dir | Rule::sort_asc | Rule::sort_desc | Rule::assert_none_option | Rule::assert_some_option | Rule::quoted_string | Rule::quoted_string_inner | Rule::char | Rule::s_quoted_string | Rule::s_quoted_string_inner | Rule::s_char | Rule::raw_string | Rule::raw_string_inner | Rule::string | Rule::boolean | Rule::null | Rule::pos_int | Rule::hex_pos_int | Rule::octo_pos_int | Rule::bin_pos_int | Rule::int | Rule::dot_float | Rule::sci_float | Rule::float | Rule::number | Rule::literal | Rule::table_schema | Rule::table_cols | Rule::table_col | Rule::col_type | Rule::col_type_with_term | Rule::any_type | Rule::int_type | Rule::float_type | Rule::string_type | Rule::bytes_type | Rule::uuid_type | Rule::bool_type | Rule::json_type | Rule::validity_type | Rule::list_type | Rule::tuple_type | Rule::vec_type | Rule::vec_el_type | Rule::imperative_stmt | Rule::imperative_sysop | Rule::imperative_condition | Rule::if_chain | Rule::if_not_chain | Rule::imperative_block | Rule::break_stmt | Rule::ignore_error_script | Rule::continue_stmt | Rule::return_stmt | Rule::loop_block | Rule::temp_swap | Rule::debug_stmt | Rule::fts_doc | Rule::fts_phrase_simple | Rule::fts_phrase_group | Rule::fts_prefix_marker | Rule::fts_booster | Rule::fts_phrase | Rule::fts_near | Rule::fts_term | Rule::fts_grouped | Rule::fts_expr | Rule::fts_op | Rule::fts_and | Rule::fts_or | Rule::fts_not | Rule::expression_script | Rule::param_list => return Err(unexpected("an if-condition", &condition)),
            };
            let body = inner
                .expect("the then-branch")?
                .into_inner()
                .map(|p| parse_imperative_stmt(p, param_pool, fixed_rules, cur_vld))
                .try_collect()?;
            let else_body = match inner.next() {
                None => vec![],
                Some(rest) => rest
                    .into_inner()
                    .map(|p| parse_imperative_stmt(p, param_pool, fixed_rules, cur_vld))
                    .try_collect()?,
            };
            ImperativeStmt::If {
                condition: cond,
                then_branch: body,
                else_branch: else_body,
                negated,
            }
        }
        Rule::loop_block => {
            let mut inner = pair.children();
            let mut mark = None;
            let mut nxt = inner.expect("the loop label or body")?;
            if nxt.as_rule() == Rule::ident {
                mark = Some(SmartString::from(nxt.as_str()));
                nxt = inner.expect("the loop body")?;
            }
            let body = parse_imperative_block(nxt, param_pool, fixed_rules, cur_vld)?;
            ImperativeStmt::Loop { label: mark, body }
        }
        Rule::temp_swap => {
            let [left, right] = pair
                .children()
                .expect_n(["the left relation", "the right relation"])?;
            ImperativeStmt::TempSwap {
                left: SmartString::from(left.as_str()),
                right: SmartString::from(right.as_str()),
            }
        }
        Rule::debug_stmt => {
            let name_p = pair.children().expect("the relation to debug")?;
            let name = name_p.as_str();

            ImperativeStmt::TempDebug {
                temp: SmartString::from(name),
            }
        }
        Rule::imperative_sysop => {
            let mut src = pair.children();
            let sysop = parse_sys(
                src.expect("the system operation")?.into_inner(),
                param_pool,
                fixed_rules,
                cur_vld,
            )?;
            let store_as = src.next().map(|p| SmartString::from(p.as_str().trim()));
            ImperativeStmt::SysOp {
                sysop: ImperativeSysop { sysop, store_as },
            }
        }
        Rule::imperative_clause => {
            let mut src = pair.children();
            let prog = parse_query(
                src.expect("the query")?.into_inner(),
                param_pool,
                fixed_rules,
                cur_vld,
            )?;
            let store_as = src.next().map(|p| SmartString::from(p.as_str().trim()));
            ImperativeStmt::Program {
                prog: ImperativeStmtClause { prog, store_as },
            }
        }
        Rule::ignore_error_script => {
            let pair = pair.children().expect("the guarded clause")?;
            let mut src = pair.children();
            let prog = parse_query(
                src.expect("the query")?.into_inner(),
                param_pool,
                fixed_rules,
                cur_vld,
            )?;
            let store_as = src.next().map(|p| SmartString::from(p.as_str().trim()));
            ImperativeStmt::IgnoreErrorProgram {
                prog: ImperativeStmtClause { prog, store_as },
            }
        }
        Rule::EOI | Rule::script | Rule::query_script | Rule::query_script_inner | Rule::query_script_inner_no_bracket | Rule::imperative_script | Rule::sys_script | Rule::sys_script_inner | Rule::index_op | Rule::vec_idx_op | Rule::fts_idx_op | Rule::lsh_idx_op | Rule::index_create | Rule::index_create_adv | Rule::index_drop | Rule::compact_op | Rule::merkle_root_op | Rule::list_fixed_rules | Rule::running_op | Rule::kill_op | Rule::explain_op | Rule::verify_op | Rule::list_relations_op | Rule::list_columns_op | Rule::list_indices_op | Rule::describe_relation_op | Rule::remove_relations_op | Rule::rename_relations_op | Rule::access_level_op | Rule::access_level | Rule::trigger_relation_show_op | Rule::trigger_relation_op | Rule::trigger_clause | Rule::trigger_put | Rule::trigger_rm | Rule::trigger_replace | Rule::constraint_op | Rule::constraint_create | Rule::constraint_drop | Rule::constraint_list | Rule::rename_pair | Rule::from_clause | Rule::to_clause | Rule::index_opt_field | Rule::WHITESPACE | Rule::BLOCK_COMMENT | Rule::LINE_COMMENT | Rule::COMMENT | Rule::prog_entry | Rule::var | Rule::param | Rule::ident | Rule::underscore_ident | Rule::definitely_underscore_ident | Rule::relation_ident | Rule::search_index_ident | Rule::compound_ident | Rule::compound_or_index_ident | Rule::rule | Rule::const_rule | Rule::fixed_rule | Rule::fixed_args_list | Rule::rule_head | Rule::head_arg | Rule::aggr_arg | Rule::fixed_arg | Rule::fixed_opt_pair | Rule::fixed_rel | Rule::fixed_rule_rel | Rule::fixed_relation_rel | Rule::fixed_named_relation_rel | Rule::fixed_named_relation_arg_pair | Rule::validity_clause | Rule::spans_kw | Rule::delta_sys_kw | Rule::delta_kw | Rule::spans_clause | Rule::delta_sys_clause | Rule::delta_clause | Rule::read_validity_clause | Rule::rule_body | Rule::rule_apply | Rule::relation_named_apply | Rule::relation_apply | Rule::search_apply | Rule::disjunction | Rule::or_op | Rule::atom | Rule::unify | Rule::unify_multi | Rule::in_op | Rule::negation | Rule::not_op | Rule::apply | Rule::apply_args | Rule::named_apply_args | Rule::named_apply_pair | Rule::grouped | Rule::expr | Rule::operation | Rule::op_or | Rule::op_and | Rule::op_concat | Rule::op_add | Rule::op_field_access | Rule::op_sub | Rule::op_mul | Rule::op_div | Rule::op_mod | Rule::op_eq | Rule::op_ne | Rule::op_gt | Rule::op_lt | Rule::op_ge | Rule::op_le | Rule::op_pow | Rule::op_coalesce | Rule::unary_op | Rule::minus | Rule::negate | Rule::term | Rule::object | Rule::object_pair | Rule::list | Rule::grouping | Rule::option | Rule::out_arg | Rule::disable_magic_rewrite_option | Rule::limit_option | Rule::offset_option | Rule::sort_option | Rule::returning_option | Rule::relation_option | Rule::relation_op | Rule::relation_create | Rule::relation_replace | Rule::relation_insert | Rule::relation_delete | Rule::relation_put | Rule::relation_update | Rule::relation_rm | Rule::relation_ensure | Rule::relation_ensure_not | Rule::timeout_option | Rule::sleep_option | Rule::sort_arg | Rule::sort_dir | Rule::sort_asc | Rule::sort_desc | Rule::assert_none_option | Rule::assert_some_option | Rule::quoted_string | Rule::quoted_string_inner | Rule::char | Rule::s_quoted_string | Rule::s_quoted_string_inner | Rule::s_char | Rule::raw_string | Rule::raw_string_inner | Rule::string | Rule::boolean | Rule::null | Rule::pos_int | Rule::hex_pos_int | Rule::octo_pos_int | Rule::bin_pos_int | Rule::int | Rule::dot_float | Rule::sci_float | Rule::float | Rule::number | Rule::literal | Rule::table_schema | Rule::table_cols | Rule::table_col | Rule::col_type | Rule::col_type_with_term | Rule::any_type | Rule::int_type | Rule::float_type | Rule::string_type | Rule::bytes_type | Rule::uuid_type | Rule::bool_type | Rule::json_type | Rule::validity_type | Rule::list_type | Rule::tuple_type | Rule::vec_type | Rule::vec_el_type | Rule::imperative_stmt | Rule::imperative_condition | Rule::imperative_block | Rule::fts_doc | Rule::fts_phrase_simple | Rule::fts_phrase_group | Rule::fts_prefix_marker | Rule::fts_booster | Rule::fts_phrase | Rule::fts_near | Rule::fts_term | Rule::fts_grouped | Rule::fts_expr | Rule::fts_op | Rule::fts_and | Rule::fts_or | Rule::fts_not | Rule::expression_script | Rule::param_list => return Err(unexpected("an imperative statement", &pair)),
    })
}
