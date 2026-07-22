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
 * host language door (params + session stamp — fixed-rule binding is
 * exec-tier); sys / imperative scripts lift to typed pure-data IR.
 */

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

use std::collections::BTreeMap;

use miette::{Diagnostic, Result, bail};
use pest::Parser;
use pest::error::InputLocation;
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::program::InputProgram;
use crate::program::span::SourceSpan;
use crate::value::{DataValue, ValidityTs};

pub mod expr;
pub mod query;
pub mod schema;
pub mod script;
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

/// A parsed KyzoScript script: one of the language's three genera.
#[derive(Debug)]
pub enum Script {
    /// One query program (`?[…] := …` / options / const / fixed rules).
    Query(InputProgram),
    /// `::…` system op — pure-data [`sys::SysScript`] (engine `SysOp` lift
    /// lives in kyzo-core).
    Sys(sys::SysScript),
    /// `%…` imperative block — composition of proven programs + control flow.
    Imperative(ImperativeProgram),
}

/// One query inside an imperative script, with the optional temp relation
/// (`as _name`) its result is stored under.
#[derive(Debug)]
pub struct ImperativeStmtClause {
    pub prog: InputProgram,
    pub store_as: Option<SmartString<LazyCompact>>,
}

/// One system operation inside an imperative script, with the optional temp
/// relation its result is stored under. Carries pure-data [`sys::SysScript`];
/// the engine admits it to `SysOp` at the kyzo-core seam.
#[derive(Debug)]
pub struct ImperativeSysop {
    pub sysop: sys::SysScript,
    pub store_as: Option<SmartString<LazyCompact>>,
}

/// A value source in an imperative script: an inline query, or the name of
/// a temporary relation holding an earlier result. (The CozoDB original
/// used `either::Either` here, with *opposite* orientations at its two use
/// sites — conditions were `Left(name)`, returns were `Left(clause)`; one
/// named type removes both the dependency and the trap.)
#[derive(Debug)]
pub enum QueryOrRelation {
    /// Boxed: a clause holds a whole program (clippy::large_enum_variant).
    Query(Box<ImperativeStmtClause>),
    Relation(SmartString<LazyCompact>),
}

/// The condition of an `%if`/`%if_not`: a temp relation tested for
/// non-emptiness, or an inline query.
pub type ImperativeCondition = QueryOrRelation;

/// One statement of an imperative script.
#[derive(Debug)]
pub enum ImperativeStmt {
    Break {
        target: Option<SmartString<LazyCompact>>,
        span: SourceSpan,
    },
    Continue {
        target: Option<SmartString<LazyCompact>>,
        span: SourceSpan,
    },
    Return {
        returns: Vec<QueryOrRelation>,
    },
    Program {
        prog: ImperativeStmtClause,
    },
    SysOp {
        sysop: ImperativeSysop,
    },
    IgnoreErrorProgram {
        prog: ImperativeStmtClause,
    },
    If {
        condition: ImperativeCondition,
        then_branch: ImperativeProgram,
        else_branch: ImperativeProgram,
        negated: bool,
    },
    Loop {
        label: Option<SmartString<LazyCompact>>,
        body: ImperativeProgram,
    },
    TempSwap {
        left: SmartString<LazyCompact>,
        right: SmartString<LazyCompact>,
    },
    TempDebug {
        temp: SmartString<LazyCompact>,
    },
}

/// A chained query: a series of `{}` queries possibly with imperative
/// directives like `%if` and `%loop`.
pub type ImperativeProgram = Vec<ImperativeStmt>;

impl Script {
    /// Refuse unless this script is a single query program.
    pub fn get_single_program(self) -> Result<InputProgram> {
        #[derive(Debug, Error, Diagnostic)]
        #[error("expect script to contain only a single program")]
        #[diagnostic(code(parser::expect_singleton))]
        struct ExpectSingleProgram;

        match self {
            Script::Query(s) => Ok(s),
            Script::Imperative(_) | Script::Sys(_) => bail!(ExpectSingleProgram),
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

/// Map a pest parse failure into the spanned [`ParseError`].
fn pest_to_parse_error(err: pest::error::Error<Rule>) -> ParseError {
    let span = match err.location {
        InputLocation::Pos(p) => SourceSpan(p, 0),
        InputLocation::Span((start, end)) => SourceSpan(start, end - start),
    };
    ParseError { span }
}

/// Top-level pest match produced no root pair — grammar/consumer drift.
#[derive(Debug, Error, Diagnostic)]
#[error(
    "parse-tree shape violates the grammar: top-level parse produced no root pair ({expected})"
)]
#[diagnostic(code(parser::grammar_shape))]
#[diagnostic(help("This is a bug: grammar.pest and its consumer disagree. Please report it."))]
struct EmptyParseRoot {
    expected: &'static str,
}

fn expect_root_pair<'a>(mut pairs: Pairs<'a>, expected: &'static str) -> Result<Pair<'a>> {
    pairs
        .next()
        .ok_or_else(|| EmptyParseRoot { expected }.into())
}

/// Public language door: source text + param pool + session stamp → typed
/// [`Script`] with spans, or a labeled refusal.
///
/// `cur_vld` is the live session coordinate for `@` / `@ NOW` clauses —
/// hosts must pass the real stamp; there is no open-end default on this door.
pub fn parse_script(
    src: &str,
    param_pool: &BTreeMap<String, DataValue>,
    cur_vld: ValidityTs,
) -> Result<Script> {
    let parsed = expect_root_pair(
        KyzoScriptParser::parse(Rule::script, src).map_err(pest_to_parse_error)?,
        "a script root",
    )?;
    Ok(match parsed.as_rule() {
        Rule::query_script => {
            let q = query::parse_query(parsed.into_inner(), param_pool, cur_vld)?;
            Script::Query(q)
        }
        Rule::imperative_script => {
            let prog = script::parse_imperative_block(parsed, param_pool, cur_vld)?;
            Script::Imperative(prog)
        }
        Rule::sys_script => {
            let op = sys::parse_sys(parsed.into_inner(), param_pool, cur_vld)?;
            Script::Sys(op)
        }
        _other => bail!(UnexpectedRule(parsed.extract_span())),
    })
}

/// Parse a standalone expression (no program wrapper).
pub fn parse_expressions(
    src: &str,
    param_pool: &BTreeMap<String, DataValue>,
) -> Result<crate::program::Expr> {
    let parsed = expect_root_pair(
        KyzoScriptParser::parse(Rule::expression_script, src).map_err(pest_to_parse_error)?,
        "an expression_script root",
    )?;
    let expr_pair = parsed.children().expect("the expression")?;
    expr::build_expr(expr_pair, param_pool)
}

/// Engine-facing language door for `::…` system scripts: source text +
/// param pool + session stamp → pure-data [`sys::SysScript`] syntax, or a
/// labeled refusal.
///
/// The pure-data half of the sys-op lift. The grammar walk, option
/// validation, and constant folding land here (parse zone, no engine
/// types); the engine-typed second half — admitting tokenizers to analyzer
/// configs and sealing index configurations — lives in kyzo-core's
/// `parse::sys`, which lifts this `SysScript` into its `SysOp`. See
/// [`sys`]'s module doc for the seam.
///
/// `cur_vld` is the live session coordinate for embedded `@` clauses —
/// same contract as [`parse_script`]. Fixed-rule binding is exec-tier.
pub fn parse_sys(
    src: &str,
    param_pool: &BTreeMap<String, DataValue>,
    cur_vld: ValidityTs,
) -> Result<sys::SysScript> {
    let parsed = expect_root_pair(
        KyzoScriptParser::parse(Rule::script, src).map_err(pest_to_parse_error)?,
        "a script root",
    )?;
    match parsed.as_rule() {
        Rule::sys_script => sys::parse_sys(parsed.into_inner(), param_pool, cur_vld),
        _other => bail!(UnexpectedRule(parsed.extract_span())),
    }
}

/// Parse a standalone column type string.
pub fn parse_type(src: &str) -> Result<crate::schema::NullableColType> {
    let parsed = expect_root_pair(
        KyzoScriptParser::parse(Rule::col_type_with_term, src).map_err(pest_to_parse_error)?,
        "a col_type_with_term root",
    )?;
    let type_pair = parsed.children().expect("the column type")?;
    schema::parse_nullable_type(type_pair)
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
