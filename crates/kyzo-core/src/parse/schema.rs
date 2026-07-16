/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): grammar-shape `unwrap`s and `unreachable!` dispatch arms go
 * through the typed-accessor layer; `VecElementType` comes from the
 * value model (`data/value.rs`), where it lives in KyzoDB.
 */

//! Parsing declared schemas: the `{k1: Type, … => v1: Type, …}` clause of a
//! `:create`/`:replace`, and column-type expressions.
//!
//! What is proven here: every column name is unique within its relation,
//! every written type is a real [`ColType`], and every list length in a
//! type is a non-negative constant. The output is the *declared* schema
//! ([`StoredRelationMetadata`]) plus which head bindings feed each column —
//! the contract that `coerce` later applies to every fact at the data
//! boundary.

use std::collections::BTreeSet;

use itertools::Itertools;
use miette::{Diagnostic, Result, bail, ensure};
use smartstring::SmartString;
use thiserror::Error;

use crate::data::relation::VecElementType;
use crate::data::relation::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::parse::expr::build_expr;
use crate::parse::{ExtractSpan, IntoChildren, Pair, Rule, unexpected};

pub(crate) fn parse_schema(
    pair: Pair<'_>,
) -> Result<(StoredRelationMetadata, Vec<Symbol>, Vec<Symbol>)> {
    let mut src = pair.children();
    let mut keys = vec![];
    let mut dependents = vec![];
    let mut key_bindings = vec![];
    let mut dep_bindings = vec![];
    let mut seen_names = BTreeSet::new();

    #[derive(Debug, Error, Diagnostic)]
    #[error("column `{0}` is declared twice")]
    #[diagnostic(code(parser::dup_name_in_cols))]
    #[diagnostic(help("every column — key or dependent — needs a name unique in the relation"))]
    struct DuplicateNameInCols(String, #[label] SourceSpan);
    for p in src.expect("the key columns")?.into_inner() {
        let span = p.extract_span();
        let (col, ident) = parse_col(p)?;
        if !seen_names.insert(col.name.clone()) {
            bail!(DuplicateNameInCols(col.name.to_string(), span));
        }
        keys.push(col);
        key_bindings.push(ident)
    }
    if let Some(ps) = src.next() {
        for p in ps.into_inner() {
            let span = p.extract_span();
            let (col, ident) = parse_col(p)?;
            if !seen_names.insert(col.name.clone()) {
                bail!(DuplicateNameInCols(col.name.to_string(), span));
            }
            dependents.push(col);
            dep_bindings.push(ident)
        }
    }

    Ok((
        StoredRelationMetadata {
            keys,
            non_keys: dependents,
        },
        key_bindings,
        dep_bindings,
    ))
}

fn parse_col(pair: Pair<'_>) -> Result<(ColumnDef, Symbol)> {
    let mut src = pair.children();
    let name_p = src.expect("the column's name")?;
    let name = SmartString::from(name_p.as_str());
    let mut typing = NullableColType {
        coltype: ColType::Any,
        nullable: true,
    };
    let mut default_gen = None;
    let mut binding_candidate = None;
    for nxt in src {
        match nxt.as_rule() {
            Rule::col_type => typing = parse_nullable_type(nxt)?,
            Rule::expr => default_gen = Some(build_expr(nxt, &Default::default())?),
            Rule::out_arg => {
                binding_candidate = Some(Symbol::new(nxt.as_str(), nxt.extract_span()))
            }
            Rule::EOI | Rule::script | Rule::query_script | Rule::query_script_inner | Rule::query_script_inner_no_bracket | Rule::imperative_script | Rule::sys_script | Rule::sys_script_inner | Rule::index_op | Rule::vec_idx_op | Rule::fts_idx_op | Rule::lsh_idx_op | Rule::index_create | Rule::index_create_adv | Rule::index_drop | Rule::compact_op | Rule::merkle_root_op | Rule::list_fixed_rules | Rule::running_op | Rule::kill_op | Rule::explain_op | Rule::verify_op | Rule::list_relations_op | Rule::list_columns_op | Rule::list_indices_op | Rule::describe_relation_op | Rule::remove_relations_op | Rule::rename_relations_op | Rule::access_level_op | Rule::access_level | Rule::trigger_relation_show_op | Rule::trigger_relation_op | Rule::trigger_clause | Rule::trigger_put | Rule::trigger_rm | Rule::trigger_replace | Rule::constraint_op | Rule::constraint_create | Rule::constraint_drop | Rule::constraint_list | Rule::rename_pair | Rule::from_clause | Rule::to_clause | Rule::index_opt_field | Rule::WHITESPACE | Rule::BLOCK_COMMENT | Rule::LINE_COMMENT | Rule::COMMENT | Rule::prog_entry | Rule::var | Rule::param | Rule::ident | Rule::underscore_ident | Rule::definitely_underscore_ident | Rule::relation_ident | Rule::search_index_ident | Rule::compound_ident | Rule::compound_or_index_ident | Rule::rule | Rule::const_rule | Rule::fixed_rule | Rule::fixed_args_list | Rule::rule_head | Rule::head_arg | Rule::aggr_arg | Rule::fixed_arg | Rule::fixed_opt_pair | Rule::fixed_rel | Rule::fixed_rule_rel | Rule::fixed_relation_rel | Rule::fixed_named_relation_rel | Rule::fixed_named_relation_arg_pair | Rule::validity_clause | Rule::spans_kw | Rule::delta_sys_kw | Rule::delta_kw | Rule::spans_clause | Rule::delta_sys_clause | Rule::delta_clause | Rule::read_validity_clause | Rule::rule_body | Rule::rule_apply | Rule::relation_named_apply | Rule::relation_apply | Rule::search_apply | Rule::disjunction | Rule::or_op | Rule::atom | Rule::unify | Rule::unify_multi | Rule::in_op | Rule::negation | Rule::not_op | Rule::apply | Rule::apply_args | Rule::named_apply_args | Rule::named_apply_pair | Rule::grouped | Rule::operation | Rule::op_or | Rule::op_and | Rule::op_concat | Rule::op_add | Rule::op_field_access | Rule::op_sub | Rule::op_mul | Rule::op_div | Rule::op_mod | Rule::op_eq | Rule::op_ne | Rule::op_gt | Rule::op_lt | Rule::op_ge | Rule::op_le | Rule::op_pow | Rule::op_coalesce | Rule::unary_op | Rule::minus | Rule::negate | Rule::term | Rule::object | Rule::object_pair | Rule::list | Rule::grouping | Rule::option | Rule::disable_magic_rewrite_option | Rule::limit_option | Rule::offset_option | Rule::sort_option | Rule::returning_option | Rule::relation_option | Rule::relation_op | Rule::relation_create | Rule::relation_replace | Rule::relation_insert | Rule::relation_delete | Rule::relation_put | Rule::relation_update | Rule::relation_rm | Rule::relation_ensure | Rule::relation_ensure_not | Rule::timeout_option | Rule::sleep_option | Rule::sort_arg | Rule::sort_dir | Rule::sort_asc | Rule::sort_desc | Rule::assert_none_option | Rule::assert_some_option | Rule::quoted_string | Rule::quoted_string_inner | Rule::char | Rule::s_quoted_string | Rule::s_quoted_string_inner | Rule::s_char | Rule::raw_string | Rule::raw_string_inner | Rule::string | Rule::boolean | Rule::null | Rule::pos_int | Rule::hex_pos_int | Rule::octo_pos_int | Rule::bin_pos_int | Rule::int | Rule::dot_float | Rule::sci_float | Rule::float | Rule::number | Rule::literal | Rule::table_schema | Rule::table_cols | Rule::table_col | Rule::col_type_with_term | Rule::any_type | Rule::int_type | Rule::float_type | Rule::string_type | Rule::bytes_type | Rule::uuid_type | Rule::bool_type | Rule::json_type | Rule::validity_type | Rule::list_type | Rule::tuple_type | Rule::vec_type | Rule::vec_el_type | Rule::imperative_stmt | Rule::imperative_sysop | Rule::imperative_clause | Rule::imperative_condition | Rule::if_chain | Rule::if_not_chain | Rule::imperative_block | Rule::break_stmt | Rule::ignore_error_script | Rule::continue_stmt | Rule::return_stmt | Rule::loop_block | Rule::temp_swap | Rule::debug_stmt | Rule::fts_doc | Rule::fts_phrase_simple | Rule::fts_phrase_group | Rule::fts_prefix_marker | Rule::fts_booster | Rule::fts_phrase | Rule::fts_near | Rule::fts_term | Rule::fts_grouped | Rule::fts_expr | Rule::fts_op | Rule::fts_and | Rule::fts_or | Rule::fts_not | Rule::expression_script | Rule::param_list => return Err(unexpected("a column type, default, or binding", &nxt)),
        }
    }
    let binding =
        binding_candidate.unwrap_or_else(|| Symbol::new(&name as &str, name_p.extract_span()));
    Ok((
        ColumnDef {
            name,
            typing,
            default_gen,
        },
        binding,
    ))
}

pub(crate) fn parse_nullable_type(pair: Pair<'_>) -> Result<NullableColType> {
    let nullable = pair.as_str().ends_with('?');
    let coltype = parse_type_inner(pair.children().expect("the inner type")?)?;
    Ok(NullableColType { coltype, nullable })
}

fn parse_type_inner(pair: Pair<'_>) -> Result<ColType> {
    Ok(match pair.as_rule() {
        Rule::any_type => ColType::Any,
        Rule::bool_type => ColType::Bool,
        Rule::int_type => ColType::Int,
        Rule::float_type => ColType::Float,
        Rule::string_type => ColType::String,
        Rule::bytes_type => ColType::Bytes,
        Rule::uuid_type => ColType::Uuid,
        Rule::json_type => ColType::Json,
        Rule::validity_type => ColType::Validity,
        Rule::list_type => {
            let mut inner = pair.children();
            let eltype = parse_nullable_type(inner.expect("the element type")?)?;
            let len = match inner.next() {
                None => None,
                Some(len_p) => {
                    let span = len_p.extract_span();
                    let expr = build_expr(len_p, &Default::default())?;
                    let dv = expr.eval_to_const()?;

                    #[derive(Debug, Error, Diagnostic)]
                    #[error("a list type's length must be a non-negative integer, got {0:?}")]
                    #[diagnostic(code(parser::bad_list_len_in_type))]
                    #[diagnostic(help(
                        "write the fixed length as a plain integer, e.g. `[Int; 3]`, or omit \
                         it for a variable-length list, e.g. `[Int]`"
                    ))]
                    struct BadListLenSpec(DataValue, #[label] SourceSpan);

                    let n = dv.get_int().ok_or(BadListLenSpec(dv, span))?;
                    ensure!(n >= 0, BadListLenSpec(DataValue::from(n), span));
                    Some(n as usize)
                }
            };
            ColType::List {
                eltype: eltype.into(),
                len,
            }
        }
        Rule::vec_type => {
            let mut inner = pair.children();
            let eltype_p = inner.expect("the vector element type")?;
            let eltype = match eltype_p.as_str() {
                "F32" | "Float" => VecElementType::F32,
                "F64" | "Double" => VecElementType::F64,
                _ => return Err(unexpected("a vector element type", &eltype_p)),
            };
            let len_p = inner.expect("the vector length")?;
            let span = len_p.extract_span();

            #[derive(Debug, Error, Diagnostic)]
            #[error("Invalid vector dimension: {0}")]
            #[diagnostic(code(parser::bad_vec_dimension))]
            #[diagnostic(help(
                "A vector's dimension must be a non-negative integer that fits in a \
                 machine word."
            ))]
            struct BadVecDimension(String, #[label("not a valid vector dimension")] SourceSpan);

            let len = len_p
                .as_str()
                .replace('_', "")
                .parse::<usize>()
                .map_err(|_| BadVecDimension(len_p.as_str().to_string(), span))?;
            ColType::Vec { eltype, len }
        }
        Rule::tuple_type => {
            ColType::Tuple(pair.into_inner().map(parse_nullable_type).try_collect()?)
        }
        Rule::EOI | Rule::script | Rule::query_script | Rule::query_script_inner | Rule::query_script_inner_no_bracket | Rule::imperative_script | Rule::sys_script | Rule::sys_script_inner | Rule::index_op | Rule::vec_idx_op | Rule::fts_idx_op | Rule::lsh_idx_op | Rule::index_create | Rule::index_create_adv | Rule::index_drop | Rule::compact_op | Rule::merkle_root_op | Rule::list_fixed_rules | Rule::running_op | Rule::kill_op | Rule::explain_op | Rule::verify_op | Rule::list_relations_op | Rule::list_columns_op | Rule::list_indices_op | Rule::describe_relation_op | Rule::remove_relations_op | Rule::rename_relations_op | Rule::access_level_op | Rule::access_level | Rule::trigger_relation_show_op | Rule::trigger_relation_op | Rule::trigger_clause | Rule::trigger_put | Rule::trigger_rm | Rule::trigger_replace | Rule::constraint_op | Rule::constraint_create | Rule::constraint_drop | Rule::constraint_list | Rule::rename_pair | Rule::from_clause | Rule::to_clause | Rule::index_opt_field | Rule::WHITESPACE | Rule::BLOCK_COMMENT | Rule::LINE_COMMENT | Rule::COMMENT | Rule::prog_entry | Rule::var | Rule::param | Rule::ident | Rule::underscore_ident | Rule::definitely_underscore_ident | Rule::relation_ident | Rule::search_index_ident | Rule::compound_ident | Rule::compound_or_index_ident | Rule::rule | Rule::const_rule | Rule::fixed_rule | Rule::fixed_args_list | Rule::rule_head | Rule::head_arg | Rule::aggr_arg | Rule::fixed_arg | Rule::fixed_opt_pair | Rule::fixed_rel | Rule::fixed_rule_rel | Rule::fixed_relation_rel | Rule::fixed_named_relation_rel | Rule::fixed_named_relation_arg_pair | Rule::validity_clause | Rule::spans_kw | Rule::delta_sys_kw | Rule::delta_kw | Rule::spans_clause | Rule::delta_sys_clause | Rule::delta_clause | Rule::read_validity_clause | Rule::rule_body | Rule::rule_apply | Rule::relation_named_apply | Rule::relation_apply | Rule::search_apply | Rule::disjunction | Rule::or_op | Rule::atom | Rule::unify | Rule::unify_multi | Rule::in_op | Rule::negation | Rule::not_op | Rule::apply | Rule::apply_args | Rule::named_apply_args | Rule::named_apply_pair | Rule::grouped | Rule::expr | Rule::operation | Rule::op_or | Rule::op_and | Rule::op_concat | Rule::op_add | Rule::op_field_access | Rule::op_sub | Rule::op_mul | Rule::op_div | Rule::op_mod | Rule::op_eq | Rule::op_ne | Rule::op_gt | Rule::op_lt | Rule::op_ge | Rule::op_le | Rule::op_pow | Rule::op_coalesce | Rule::unary_op | Rule::minus | Rule::negate | Rule::term | Rule::object | Rule::object_pair | Rule::list | Rule::grouping | Rule::option | Rule::out_arg | Rule::disable_magic_rewrite_option | Rule::limit_option | Rule::offset_option | Rule::sort_option | Rule::returning_option | Rule::relation_option | Rule::relation_op | Rule::relation_create | Rule::relation_replace | Rule::relation_insert | Rule::relation_delete | Rule::relation_put | Rule::relation_update | Rule::relation_rm | Rule::relation_ensure | Rule::relation_ensure_not | Rule::timeout_option | Rule::sleep_option | Rule::sort_arg | Rule::sort_dir | Rule::sort_asc | Rule::sort_desc | Rule::assert_none_option | Rule::assert_some_option | Rule::quoted_string | Rule::quoted_string_inner | Rule::char | Rule::s_quoted_string | Rule::s_quoted_string_inner | Rule::s_char | Rule::raw_string | Rule::raw_string_inner | Rule::string | Rule::boolean | Rule::null | Rule::pos_int | Rule::hex_pos_int | Rule::octo_pos_int | Rule::bin_pos_int | Rule::int | Rule::dot_float | Rule::sci_float | Rule::float | Rule::number | Rule::literal | Rule::table_schema | Rule::table_cols | Rule::table_col | Rule::col_type | Rule::col_type_with_term | Rule::vec_el_type | Rule::imperative_stmt | Rule::imperative_sysop | Rule::imperative_clause | Rule::imperative_condition | Rule::if_chain | Rule::if_not_chain | Rule::imperative_block | Rule::break_stmt | Rule::ignore_error_script | Rule::continue_stmt | Rule::return_stmt | Rule::loop_block | Rule::temp_swap | Rule::debug_stmt | Rule::fts_doc | Rule::fts_phrase_simple | Rule::fts_phrase_group | Rule::fts_prefix_marker | Rule::fts_booster | Rule::fts_phrase | Rule::fts_near | Rule::fts_term | Rule::fts_grouped | Rule::fts_expr | Rule::fts_op | Rule::fts_and | Rule::fts_or | Rule::fts_not | Rule::expression_script | Rule::param_list => return Err(unexpected("a column type", &pair)),
    })
}
