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
            _ => return Err(unexpected("a column type, default, or binding", &nxt)),
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
        _ => return Err(unexpected("a column type", &pair)),
    })
}
