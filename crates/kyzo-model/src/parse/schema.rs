/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): lifts schema declaration syntax into schema types —
 * `NullableColType` doors, `ColLen` newtype.
 */

//! Lifts schema declaration syntax into schema types.

use std::collections::BTreeSet;

use miette::{Diagnostic, Result, bail, ensure};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::program::span::SourceSpan;
use crate::program::symbol::Symbol;
use crate::schema::column::{ColLen, ColType, ColumnDef, NullableColType, VecElementType};
use crate::schema::relation::StoredRelationMetadata;
use crate::value::DataValue;

use super::expr::build_expr;
use super::{ExtractSpan, Pair, Rule, UnexpectedRule};

/// Parse a `{ keys => dependents }` table schema pair.
pub(crate) fn parse_schema(
    pair: Pair<'_>,
) -> Result<(StoredRelationMetadata, Vec<Symbol>, Vec<Symbol>)> {
    let mut src = pair.into_inner();
    let mut keys = vec![];
    let mut dependents = vec![];
    let mut key_bindings = vec![];
    let mut dep_bindings = vec![];
    let mut seen_names = BTreeSet::new();

    #[derive(Debug, Error, Diagnostic)]
    #[error("Column {0} is defined multiple times")]
    #[diagnostic(code(parser::dup_name_in_cols))]
    struct DuplicateNameInCols(String, #[label] SourceSpan);

    for p in src.next().unwrap().into_inner() {
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
    let mut src = pair.into_inner();
    let name_p = src.next().unwrap();
    let name = SmartString::<LazyCompact>::from(name_p.as_str());
    let mut typing = NullableColType::optional(ColType::Any);
    let mut default_gen = None;
    let mut binding_candidate = None;
    for nxt in src {
        match nxt.as_rule() {
            Rule::col_type => typing = parse_nullable_type(nxt)?,
            Rule::expr => default_gen = Some(build_expr(nxt, &Default::default())?),
            Rule::out_arg => {
                binding_candidate = Some(Symbol::new(nxt.as_str(), nxt.extract_span()))
            }
            _other => bail!(UnexpectedRule(nxt.extract_span())),
        }
    }
    let binding =
        match binding_candidate {
        Some(b) => b,
        None => Symbol::new(&name as &str, name_p.extract_span()),
    };
    Ok((
        ColumnDef {
            name,
            typing,
            default_gen,
        },
        binding,
    ))
}

/// Parse a nullable column type pair.
pub(crate) fn parse_nullable_type(pair: Pair<'_>) -> Result<NullableColType> {
    let nullable = pair.as_str().ends_with('?');
    let coltype = parse_type_inner(pair.into_inner().next().unwrap())?;
    Ok(if nullable {
        NullableColType::optional(coltype)
    } else {
        NullableColType::required(coltype)
    })
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
            let mut inner = pair.into_inner();
            let eltype = parse_nullable_type(inner.next().unwrap())?;
            let len = match inner.next() {
                None => None,
                Some(len_p) => {
                    let span = len_p.extract_span();
                    let expr = build_expr(len_p, &Default::default())?;
                    let dv = expr.eval_to_const()?;

                    #[derive(Debug, Error, Diagnostic)]
                    #[error("Bad specification of list length in type: {0:?}")]
                    #[diagnostic(code(parser::bad_list_len_in_type))]
                    struct BadListLenSpec(DataValue, #[label] SourceSpan);

                    let n = dv.get_int().ok_or(BadListLenSpec(dv, span))?;
                    ensure!(n >= 0, BadListLenSpec(DataValue::from(n), span));
                    Some(ColLen::new(n as usize))
                }
            };
            ColType::List {
                eltype: eltype.into(),
                len,
            }
        }
        Rule::vec_type => {
            let mut inner = pair.into_inner();
            let el_p = inner.next().unwrap();
            let eltype = match el_p.as_str() {
                "F32" | "Float" => VecElementType::F32,
                "F64" | "Double" => VecElementType::F64,
                other => {
                    #[derive(Debug, Error, Diagnostic)]
                    #[error("unknown vector element type '{0}'")]
                    #[diagnostic(code(parser::bad_vec_eltype))]
                    struct BadVecElType(String, #[label] SourceSpan);

                    bail!(BadVecElType(other.to_string(), el_p.extract_span()))
                }
            };
            let len = inner.next().unwrap();
            let len: usize = len
                .as_str()
                .replace('_', "")
                .parse()
                .map_err(|e| miette::miette!("{e}"))?;
            ColType::Vec {
                eltype,
                len: ColLen::new(len),
            }
        }
        Rule::tuple_type => {
            let mut cols = Vec::new();
            for p in pair.into_inner() {
                cols.push(parse_nullable_type(p)?);
            }
            ColType::Tuple(cols)
        }
        _other => bail!(UnexpectedRule(pair.extract_span())),
    })
}
