//! Parse zone: text → typed IR with spans and refusals.
//!
//! Pest owns KyzoScript surface syntax ([`grammar.pest`]); lift modules
//! turn pairs into program / schema IR. The language door lives entirely
//! in this crate — never imports the engine.

/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): seated in kyzo-model; public `parse_script` is the LSP /
 * host language door (params only — fixed-rule binding is exec-tier);
 * sys / imperative scripts validate as syntax with deferred typed lift.
 */

use std::collections::BTreeMap;

use miette::{Diagnostic, Result, bail};
use pest::error::InputLocation;
use pest::Parser;
use thiserror::Error;

use crate::program::InputProgram;
use crate::program::span::SourceSpan;
use crate::value::{DataValue, ValidityTs, MAX_VALIDITY_TS};

pub mod expr;
pub mod query;
pub mod schema;
pub mod search;

pub use search::{
    FtsBooster, FtsExpr, FtsLiteral, FtsNear, NonEmptyFtsExprs, NonEmptyFtsLiterals,
};

#[derive(pest_derive::Parser)]
#[grammar = "parse/grammar.pest"]
pub(crate) struct KyzoScriptParser;

pub(crate) type Pair<'a> = pest::iterators::Pair<'a, Rule>;
pub(crate) type Pairs<'a> = pest::iterators::Pairs<'a, Rule>;

/// A parsed KyzoScript script: query IR, or a syntax-validated system /
/// imperative script whose typed lift seats land later (`parse/sys`,
/// `parse/script`).
#[derive(Debug)]
pub enum Script {
    /// One query program (`?[…] := …` / options / const / fixed rules).
    Query(InputProgram),
    /// `::…` system op — pest-validated; typed SysOp lift is a later seat.
    Sys { span: SourceSpan },
    /// `%…` imperative block — pest-validated; typed lift is a later seat.
    Imperative { span: SourceSpan },
}

impl Script {
    /// Refuse unless this script is a single query program.
    pub fn get_single_program(self) -> Result<InputProgram> {
        #[derive(Debug, Error, Diagnostic)]
        #[error("expect script to contain only a single program")]
        #[diagnostic(code(parser::expect_singleton))]
        struct ExpectSingleProgram;

        match self {
            Script::Query(s) => Ok(s),
            Script::Imperative { .. } | Script::Sys { .. } => bail!(ExpectSingleProgram),
        }
    }
}

#[derive(thiserror::Error, Diagnostic, Debug)]
#[error("The query parser has encountered unexpected input / end of input at {span}")]
#[diagnostic(code(parser::pest))]
pub struct ParseError {
    #[label]
    pub span: SourceSpan,
}

/// Pest delivered a rule the lift seat does not own — grammar/lift drift, not user input.
#[derive(Debug, Error, Diagnostic)]
#[error("unexpected grammar rule while lifting KyzoScript")]
#[diagnostic(code(parser::unexpected_rule))]
pub(crate) struct UnexpectedRule(#[label] pub(crate) SourceSpan);

/// Public language door: source text + param pool → typed [`Script`] with
/// spans, or a labeled refusal. Resolves `kyzo_model::parse::parse_script`
/// for kyzo-lsp validation.
///
/// Current validity for `@` clauses defaults to the open (latest) system
/// coordinate — hosts that need a session clock pass through a later
/// engine-facing wrapper; the LSP door is params-only.
pub fn parse_script(
    src: &str,
    param_pool: &BTreeMap<String, DataValue>,
) -> Result<Script> {
    let cur_vld = MAX_VALIDITY_TS;
    let parsed = KyzoScriptParser::parse(Rule::script, src)
        .map_err(|err| {
            let span = match err.location {
                InputLocation::Pos(p) => SourceSpan(p, 0),
                InputLocation::Span((start, end)) => SourceSpan(start, end - start),
            };
            ParseError { span }
        })?
        .next()
        .unwrap();
    Ok(match parsed.as_rule() {
        Rule::query_script => {
            let q = query::parse_query(parsed.into_inner(), param_pool, cur_vld)?;
            Script::Query(q)
        }
        Rule::imperative_script => Script::Imperative {
            span: parsed.extract_span(),
        },
        Rule::sys_script => Script::Sys {
            span: parsed.extract_span(),
        },
        _ => bail!(UnexpectedRule(parsed.extract_span())),
    })
}

/// Parse a standalone expression (no program wrapper).
pub fn parse_expressions(
    src: &str,
    param_pool: &BTreeMap<String, DataValue>,
) -> Result<crate::program::Expr> {
    let parsed = KyzoScriptParser::parse(Rule::expression_script, src)
        .map_err(|err| {
            let span = match err.location {
                InputLocation::Pos(p) => SourceSpan(p, 0),
                InputLocation::Span((start, end)) => SourceSpan(start, end - start),
            };
            ParseError { span }
        })?
        .next()
        .unwrap();
    expr::build_expr(parsed.into_inner().next().unwrap(), param_pool)
}

/// Parse a standalone column type string.
pub fn parse_type(src: &str) -> Result<crate::schema::NullableColType> {
    let parsed = KyzoScriptParser::parse(Rule::col_type_with_term, src)
        .map_err(|err| {
            let span = match err.location {
                InputLocation::Pos(p) => SourceSpan(p, 0),
                InputLocation::Span((start, end)) => SourceSpan(start, end - start),
            };
            ParseError { span }
        })?
        .next()
        .unwrap();
    schema::parse_nullable_type(parsed.into_inner().next().unwrap())
}

pub(crate) trait ExtractSpan {
    fn extract_span(&self) -> SourceSpan;
}

impl ExtractSpan for Pair<'_> {
    fn extract_span(&self) -> SourceSpan {
        let span = self.as_span();
        let start = span.start();
        let end = span.end();
        SourceSpan(start, end - start)
    }
}

/// Session clock for `@ NOW` — kept for internal lifts; public door uses
/// [`MAX_VALIDITY_TS`] until an engine wrapper threads the live stamp.
#[allow(dead_code)]
pub(crate) fn default_cur_vld() -> ValidityTs {
    MAX_VALIDITY_TS
}
