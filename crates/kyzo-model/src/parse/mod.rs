//! Parse zone: text → typed IR with spans and refusals.
//!
//! Pest owns KyzoScript surface syntax ([`grammar.pest`]); lift modules
//! turn pairs into program / schema IR. The language door lives entirely
//! in this crate — never imports the engine.
//!
//! [`search`] is the pure-data FTS/search AST seat (phrase / proximity /
//! scoring nodes as ordinary AST). Evaluation (`eval_near`, scoring,
//! analyzer tokenize) stays in kyzo-core — see that module's crate-wall
//! note.

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
pub mod sys;

pub use search::{
    FtsBooster, FtsExpr, FtsLiteral, FtsNear, NonEmptyFtsExprs, NonEmptyFtsLiterals,
    parse_fts_query,
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

/// Engine-facing language door for `::…` system scripts: source text +
/// param pool → pure-data [`sys::SysScript`] syntax, or a labeled refusal.
///
/// The pure-data half of the sys-op lift. The grammar walk, option
/// validation, and constant folding land here (parse zone, no engine
/// types); the engine-typed second half — admitting tokenizers to analyzer
/// configs and sealing index configurations — lives in kyzo-core's
/// `parse::sys`, which lifts this `SysScript` into its `SysOp`. See
/// [`sys`]'s module doc for the seam.
///
/// Current validity for embedded `@` clauses defaults to the open (latest)
/// coordinate, exactly as [`parse_script`] does; fixed-rule binding is
/// exec-tier and never happens here.
pub fn parse_sys(
    src: &str,
    param_pool: &BTreeMap<String, DataValue>,
) -> Result<sys::SysScript> {
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
    match parsed.as_rule() {
        Rule::sys_script => sys::parse_sys(parsed.into_inner(), param_pool, cur_vld),
        _ => bail!(UnexpectedRule(parsed.extract_span())),
    }
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

// ─────────────────────────────────────────────────────────────────────────
// Typed access to the parse tree (restored for the sys-op lift).
//
// pest already proved the input matches the grammar; these accessors carry
// that proof into Rust instead of re-asserting it with `unwrap`. Each one
// produces a spanned error naming the grammar rule when the tree's shape
// disagrees with what the consumer expects — which can only happen if
// `grammar.pest` and the consuming code have drifted apart (an internal
// bug, but a *diagnosable* one).
// ─────────────────────────────────────────────────────────────────────────

/// The parse tree lacks a child the grammar promises. Reachable only through
/// grammar/consumer drift; an error, never an abort.
#[derive(Debug, Error, Diagnostic)]
#[error("parse-tree shape violates the grammar: rule `{rule:?}` promised {expected}")]
#[diagnostic(code(parser::grammar_shape))]
#[diagnostic(help("This is a bug: grammar.pest and its consumer disagree. Please report it."))]
pub(crate) struct GrammarShapeError {
    /// What the consumer expected to find, in grammar terms.
    expected: &'static str,
    /// The grammar rule whose children were being consumed.
    rule: Rule,
    #[label]
    span: SourceSpan,
}

/// The parse tree contains a rule the consumer has no arm for. The typed
/// replacement for an `unreachable!` dispatch arm; reachable only through
/// grammar/consumer drift.
#[derive(Debug, Error, Diagnostic)]
#[error("parse-tree shape violates the grammar: `{found:?}` cannot appear in {context}")]
#[diagnostic(code(parser::grammar_shape))]
#[diagnostic(help("This is a bug: grammar.pest and its consumer disagree. Please report it."))]
pub(crate) struct UnexpectedRuleError {
    found: Rule,
    context: &'static str,
    #[label]
    span: SourceSpan,
}

/// A rule the grammar cannot put here appeared anyway: the typed
/// replacement for an `unreachable!` dispatch arm. Use as
/// `r => return Err(unexpected("a body atom", &pair))`.
pub(crate) fn unexpected(context: &'static str, pair: &Pair<'_>) -> miette::Report {
    UnexpectedRuleError {
        found: pair.as_rule(),
        context,
        span: pair.extract_span(),
    }
    .into()
}

/// A pair's children, remembering the parent rule and span so a missing
/// child is a spanned error naming the grammar rule. The typed replacement
/// for `pair.into_inner()` + `next().unwrap()`.
pub(crate) struct GrammarChildren<'a> {
    rule: Rule,
    span: SourceSpan,
    inner: Pairs<'a>,
}

impl<'a> GrammarChildren<'a> {
    /// The next child, which the grammar guarantees to exist here.
    /// `expected` names it for the drift diagnostic.
    pub(crate) fn expect(&mut self, expected: &'static str) -> Result<Pair<'a>> {
        self.inner.next().ok_or_else(|| {
            GrammarShapeError {
                expected,
                rule: self.rule,
                span: self.span,
            }
            .into()
        })
    }

    /// The next `N` children at once, each named for diagnostics:
    /// `let [k, v] = pair.children().expect_n(["a key", "a value"])?;`
    pub(crate) fn expect_n<const N: usize>(
        &mut self,
        expected: [&'static str; N],
    ) -> Result<[Pair<'a>; N]> {
        let mut out = Vec::with_capacity(N);
        for what in expected {
            out.push(self.expect(what)?);
        }
        match out.try_into() {
            Ok(arr) => Ok(arr),
            // In bounds: the loop above pushed exactly N elements.
            Err(_) => bail!(GrammarShapeError {
                expected: "an exact child count",
                rule: self.rule,
                span: self.span,
            }),
        }
    }
}

/// Remaining children iterate as plain pairs (grammar-repeated tails such
/// as `(compound_ident ~ ",")*`).
impl<'a> Iterator for GrammarChildren<'a> {
    type Item = Pair<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

/// Entry point of the typed-accessor layer: consume a pair's children with
/// the parent's identity retained for diagnostics.
pub(crate) trait IntoChildren<'a> {
    fn children(self) -> GrammarChildren<'a>;
}

impl<'a> IntoChildren<'a> for Pair<'a> {
    fn children(self) -> GrammarChildren<'a> {
        let parent_rule = self.as_rule();
        let span = self.extract_span();
        GrammarChildren {
            rule: parent_rule,
            span,
            inner: self.into_inner(),
        }
    }
}

/// Session clock for `@ NOW` — kept for internal lifts; public door uses
/// [`MAX_VALIDITY_TS`] until an engine wrapper threads the live stamp.
#[allow(dead_code)]
pub(crate) fn default_cur_vld() -> ValidityTs {
    MAX_VALIDITY_TS
}
