/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): the grammar file is `kyzoscript.pest` (the language is
 * KyzoScript) and `CozoScript`/`CozoScriptParser` are `Script`/
 * `ScriptParser`; `SourceSpan` is imported from the data layer instead of
 * defined here; a typed-accessor layer over pest pairs (`GrammarChildren`,
 * `single`, `unexpected`, `strip_sigil`) replaces the grammar-shape
 * `unwrap`s and `unreachable!`s, so grammar/consumer drift surfaces as a
 * spanned error naming the rule, never an abort; the two orientations of
 * `either::Either` in the imperative AST are one named sum type
 * (`QueryOrRelation`), dropping the `either` dependency; fixed-rule
 * implementations are threaded as `Arc<dyn FixedRule>` (not
 * `Arc<Box<dyn FixedRule>>`); nesting depth is bounded by a pre-parse
 * structural scan (`reject_excessive_nesting`/`NestingTooDeep`) because
 * the recursive-descent parse would otherwise overflow the native stack
 * — an uncatchable abort — on adversarially deep input.
 */

//! The parse tier: claimed text becomes proven syntax.
//!
//! Everything below this module's boundary is a *claim* — a `&str` that says
//! it is a KyzoScript program. Everything above it is *proof*: a
//! [`Script`] whose values were constructed only by code that checked them,
//! with every value carrying the [`SourceSpan`] of the text it came from, so
//! any later stage can point a diagnostic at the exact characters
//! responsible.
//!
//! The language is one grammar (`kyzoscript.pest`) with three script
//! species, and [`Script`] is that genus:
//!
//! - **Query** — a single Datalog program ([`InputProgram`]); parsing proves
//!   it has an entry (`?`), that its options are well-formed constants, and
//!   that parameters, aggregations and fixed rules all resolve.
//! - **Imperative** — a chain of queries under control flow (`%if`,
//!   `%loop`, `%return`, …), each clause a proven program.
//! - **Sys** — one system operation ([`SysOp`]).
//!
//! Two laws hold throughout the tier:
//!
//! 1. **Grammar-shape trust is typed.** The pest grammar guarantees the
//!    shape of the parse tree, and the code consumes that shape through the
//!    typed accessors below ([`GrammarChildren`], [`single`],
//!    [`unexpected`], [`strip_sigil`]) instead of `unwrap`/`unreachable!`.
//!    If the grammar and its consumer ever drift apart, the result is a
//!    spanned [`GrammarShapeError`] naming the rule — diagnosable, not an
//!    abort. The few remaining `unwrap`-free shortcuts each carry a comment
//!    naming their structural guarantee.
//! 2. **No user text can panic the parser** — and no user text can hang it
//!    or overflow the stack: every literal, option and sigil path is
//!    fallible, malformed input is an error value, nesting is bounded
//!    ([`NestingTooDeep`], enforced by [`reject_excessive_nesting`] before
//!    any recursive work and again inside the expression builder), and the
//!    literal grammar is backtracking-free (see the [SEQ] proofs in
//!    `kyzoscript.pest`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use miette::{Diagnostic, IntoDiagnostic, Result, bail};
use pest::Parser;
use pest::error::InputLocation;
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::expr::Expr;
use crate::data::program::{FixedRule, InputProgram};
use crate::data::relation::NullableColType;
use crate::data::span::SourceSpan;
use crate::data::value::{DataValue, ValidityTs};
use crate::parse::expr::build_expr;
use crate::parse::imperative::parse_imperative_block;
use crate::parse::query::parse_query;
use crate::parse::schema::parse_nullable_type;
use crate::parse::sys::{SysOp, parse_sys};

pub(crate) mod expr;
pub(crate) mod fts;
#[cfg(test)]
mod fuzz_tests;
pub(crate) mod imperative;
pub(crate) mod query;
pub(crate) mod schema;
pub(crate) mod sys;

/// The pest parser for KyzoScript. The grammar file is the *other half of
/// this tier's proofs*: every typed accessor's "the grammar guarantees"
/// claim points at a rule in `kyzoscript.pest`.
#[derive(pest_derive::Parser)]
#[grammar = "kyzoscript.pest"]
pub(crate) struct ScriptParser;

pub(crate) type Pair<'a> = pest::iterators::Pair<'a, Rule>;
pub(crate) type Pairs<'a> = pest::iterators::Pairs<'a, Rule>;

// ─────────────────────────────────────────────────────────────────────────
// Typed access to the parse tree.
//
// pest already proved the input matches the grammar; these accessors carry
// that proof into Rust instead of re-asserting it with `unwrap`. Each one
// produces a spanned error naming the grammar rule when the tree's shape
// disagrees with what the consumer expects — which can only happen if
// `kyzoscript.pest` and the consuming code have drifted apart (an internal
// bug, but a *diagnosable* one).
// ─────────────────────────────────────────────────────────────────────────

/// The parse tree lacks a child the grammar promises. Reachable only through
/// grammar/consumer drift; an error, never an abort.
#[derive(Debug, Error, Diagnostic)]
#[error("parse-tree shape violates the grammar: rule `{rule:?}` promised {expected}")]
#[diagnostic(code(parser::grammar_shape))]
#[diagnostic(help("This is a bug: kyzoscript.pest and its consumer disagree. Please report it."))]
pub(crate) struct GrammarShapeError {
    /// What the consumer expected to find, in grammar terms.
    expected: &'static str,
    /// The grammar rule whose children were being consumed.
    rule: Rule,
    #[label]
    span: SourceSpan,
}

/// The parse tree contains a rule the consumer has no arm for. Replaces the
/// original's `unreachable!` dispatch arms; reachable only through
/// grammar/consumer drift.
#[derive(Debug, Error, Diagnostic)]
#[error("parse-tree shape violates the grammar: `{found:?}` cannot appear in {context}")]
#[diagnostic(code(parser::grammar_shape))]
#[diagnostic(help("This is a bug: kyzoscript.pest and its consumer disagree. Please report it."))]
struct UnexpectedRuleError {
    found: Rule,
    context: &'static str,
    #[label]
    span: SourceSpan,
}

/// A rule the grammar cannot put here appeared anyway: the typed
/// replacement for an `unreachable!` dispatch arm. Use as
/// `r => bail!(unexpected("a body atom", &pair))` — or return it directly.
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
    /// (Not named `take`: `GrammarChildren` is an `Iterator`, and
    /// `Iterator::take` would shadow-fight it.)
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
/// as `(head_arg ~ ",")*`).
impl<'a> Iterator for GrammarChildren<'a> {
    type Item = Pair<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

/// The single pair a successful top-level `ScriptParser::parse` always
/// yields (every top-level grammar rule is `SOI ~ … ~ EOI` with non-empty
/// body). Typed so the guarantee is checked, not asserted.
pub(crate) fn single<'a>(
    mut pairs: Pairs<'a>,
    expected: &'static str,
    top_rule: Rule,
) -> Result<Pair<'a>> {
    pairs.next().ok_or_else(|| {
        GrammarShapeError {
            expected,
            rule: top_rule,
            span: SourceSpan(0, 0),
        }
        .into()
    })
}

/// Strip the sigil the grammar mandates on this token (`$` on `param`, `*`
/// on `relation_ident`, …). Its absence is grammar/consumer drift, reported
/// as a spanned error. (The sigils themselves stay strings-with-prefixes in
/// the grammar; the *types* built from them — `Expr::Const` for `$params`,
/// `InputRelationApplyAtom` for `*relations`, option fields for `:options`
/// — already carry the distinction, so this boundary is where each sigil is
/// looked at for the last time.)
pub(crate) fn strip_sigil<'a>(pair: &Pair<'a>, sigil: char) -> Result<&'a str> {
    match pair.as_str().strip_prefix(sigil) {
        Some(rest) => Ok(rest),
        None => bail!(GrammarShapeError {
            expected: "a leading sigil on this token",
            rule: pair.as_rule(),
            span: pair.extract_span(),
        }),
    }
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

// ─────────────────────────────────────────────────────────────────────────
// The script genus
// ─────────────────────────────────────────────────────────────────────────

/// This represents a full KyzoScript script, as you'd pass to `run_script`:
/// the genus over the language's three species.
#[derive(Debug)]
pub enum Script {
    /// Boxed: a program is hundreds of bytes and the other species are
    /// pointer-sized (clippy::large_enum_variant).
    #[allow(missing_docs)]
    Single(Box<InputProgram>),
    #[allow(missing_docs)]
    Imperative(ImperativeProgram),
    #[allow(missing_docs)]
    Sys(SysOp),
}

/// One query inside an imperative script, with the optional temp relation
/// (`as _name`) its result is stored under.
#[allow(missing_docs)]
#[derive(Debug)]
pub struct ImperativeStmtClause {
    pub prog: InputProgram,
    pub store_as: Option<SmartString<LazyCompact>>,
}

/// One system operation inside an imperative script, with the optional temp
/// relation its result is stored under.
#[allow(missing_docs)]
#[derive(Debug)]
pub struct ImperativeSysop {
    pub sysop: SysOp,
    pub store_as: Option<SmartString<LazyCompact>>,
}

/// A value source in an imperative script: an inline query, or the name of
/// a temporary relation holding an earlier result. (The CozoDB original
/// used `either::Either` here, with *opposite* orientations at its two use
/// sites — conditions were `Left(name)`, returns were `Left(clause)`; one
/// named type removes both the dependency and the trap.)
#[allow(missing_docs)]
#[derive(Debug)]
pub enum QueryOrRelation {
    /// Boxed: a clause holds a whole program (clippy::large_enum_variant).
    Query(Box<ImperativeStmtClause>),
    Relation(SmartString<LazyCompact>),
}

/// The condition of an `%if`/`%if_not`: a temp relation tested for
/// non-emptiness, or an inline query.
pub(crate) type ImperativeCondition = QueryOrRelation;

/// One statement of an imperative script.
#[allow(missing_docs)]
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

/// This is a chained query: a series of `{}` queries possibly with
/// imperative directives like `%if` and `%loop`.
pub type ImperativeProgram = Vec<ImperativeStmt>;

impl ImperativeStmt {
    pub(crate) fn needs_write_locks(&self, collector: &mut BTreeSet<SmartString<LazyCompact>>) {
        match self {
            ImperativeStmt::Program { prog, .. }
            | ImperativeStmt::IgnoreErrorProgram { prog, .. } => {
                if let Some(name) = prog.prog.needs_write_lock() {
                    collector.insert(name);
                }
            }
            ImperativeStmt::Return { returns, .. } => {
                for ret in returns {
                    if let QueryOrRelation::Query(prog) = ret
                        && let Some(name) = prog.prog.needs_write_lock()
                    {
                        collector.insert(name);
                    }
                }
            }
            ImperativeStmt::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                if let QueryOrRelation::Query(prog) = condition
                    && let Some(name) = prog.prog.needs_write_lock()
                {
                    collector.insert(name);
                }
                for prog in then_branch.iter().chain(else_branch.iter()) {
                    prog.needs_write_locks(collector);
                }
            }
            ImperativeStmt::Loop { body, .. } => {
                for prog in body {
                    prog.needs_write_locks(collector);
                }
            }
            ImperativeStmt::TempDebug { .. }
            | ImperativeStmt::Break { .. }
            | ImperativeStmt::Continue { .. }
            | ImperativeStmt::TempSwap { .. } => {}
            ImperativeStmt::SysOp { sysop } => match &sysop.sysop {
                SysOp::RemoveRelation(rels) => {
                    for rel in rels {
                        collector.insert(rel.name.clone());
                    }
                }
                SysOp::RenameRelation(renames) => {
                    for (old, new) in renames {
                        collector.insert(old.name.clone());
                        collector.insert(new.name.clone());
                    }
                }
                SysOp::CreateIndex(symb, subs, _) => {
                    collector.insert(symb.name.clone());
                    collector.insert(SmartString::from(format!("{}:{}", symb.name, subs.name)));
                }
                SysOp::CreateVectorIndex(m) => {
                    collector.insert(m.base_relation.clone());
                    collector.insert(SmartString::from(format!(
                        "{}:{}",
                        m.base_relation, m.index_name
                    )));
                }
                SysOp::CreateFtsIndex(m) => {
                    collector.insert(m.base_relation.clone());
                    collector.insert(SmartString::from(format!(
                        "{}:{}",
                        m.base_relation, m.index_name
                    )));
                }
                SysOp::CreateMinHashLshIndex(m) => {
                    collector.insert(m.base_relation.clone());
                    collector.insert(SmartString::from(format!(
                        "{}:{}",
                        m.base_relation, m.index_name
                    )));
                }
                SysOp::RemoveIndex(rel, idx) => {
                    collector.insert(SmartString::from(format!("{}:{}", rel.name, idx.name)));
                }
                _ => {}
            },
        }
    }
}

impl Script {
    pub(crate) fn get_single_program(self) -> Result<InputProgram> {
        #[derive(Debug, Error, Diagnostic)]
        #[error("expect script to contain only a single program")]
        #[diagnostic(code(parser::expect_singleton))]
        struct ExpectSingleProgram;
        match self {
            Script::Single(s) => Ok(*s),
            Script::Imperative(_) | Script::Sys(_) => {
                bail!(ExpectSingleProgram)
            }
        }
    }
}

/// The input did not match the grammar at all: the user's error, labeled at
/// the offending position. (Distinct from [`GrammarShapeError`], which is
/// ours.) This is the single funnel every syntax mistake in KyzoScript
/// passes through, so it is the highest-leverage diagnostic in the
/// language: `summary`/`label` translate pest's expected-rule set into
/// KyzoScript's own vocabulary via [`describe_expected`] (never a bare
/// `Rule::foo` debug print), and `help` — when the mistake reads as
/// SQL — points at the KyzoScript idiom instead of just refusing it
/// (`sql_refugee_hint`): SQL muscle memory is the most common on-ramp
/// error for a Datalog newcomer.
#[derive(thiserror::Error, Diagnostic, Debug)]
#[error("{summary}")]
#[diagnostic(code(parser::pest))]
pub(crate) struct ParseError {
    summary: String,
    #[label("{label}")]
    pub(crate) span: SourceSpan,
    label: String,
    #[help]
    help: Option<String>,
}

impl ParseError {
    /// Convert pest's location + expected-rule report into our spanned,
    /// designed error. `src` is the whole program text: needed both to
    /// quote the offending token and to scan for a SQL-shaped mistake.
    fn from_pest(err: pest::error::Error<Rule>, src: &str) -> Self {
        let start = match err.location {
            InputLocation::Pos(p) => p,
            InputLocation::Span((start, _)) => start,
        };
        let end = match err.location {
            InputLocation::Pos(p) => p,
            InputLocation::Span((_, end)) => end,
        };
        let span = SourceSpan(start, end - start);
        let expected = match &err.variant {
            pest::error::ErrorVariant::ParsingError { positives, .. } => {
                describe_expected(positives)
            }
            pest::error::ErrorVariant::CustomError { message } => Some(message.clone()),
        };
        let offending = src.get(start..end.max(start).min(src.len())).unwrap_or("");
        let offending = offending.trim();
        let summary = match (offending.is_empty(), &expected) {
            (true, Some(e)) => format!("the query ends here, but {e} was expected next"),
            (true, None) => "the query ends where more input was expected".to_string(),
            (false, Some(e)) => format!("unexpected `{offending}` — expected {e}"),
            (false, None) => format!("unexpected `{offending}`"),
        };
        let label = match &expected {
            Some(e) => format!("expected {e} here"),
            None => "unexpected input here".to_string(),
        };
        let help = sql_refugee_hint(src, start);
        ParseError {
            summary,
            span,
            label,
            help,
        }
    }
}

/// Turn pest's list of grammar rules it was still willing to accept at the
/// failure point into one clause in KyzoScript's own vocabulary — e.g. "a
/// rule head, a query option, or a fixed-rule application" — deduplicated
/// and capped so a deeply ambiguous position doesn't produce an unreadable
/// wall of alternatives. `None` for an empty list (nothing was expected:
/// pest itself only raises this for a custom error, handled separately).
fn describe_expected(positives: &[Rule]) -> Option<String> {
    const MAX_ALTERNATIVES: usize = 5;
    if positives.is_empty() {
        return None;
    }
    // Dedup on the rendered phrase, not the `Rule`: several distinct rules
    // (every operator token) deliberately render to the same phrase, and
    // that collapse only happens here.
    let mut seen = BTreeSet::new();
    let mut phrases = Vec::new();
    for &r in positives {
        let phrase = describe_rule(r);
        if seen.insert(phrase.clone()) {
            phrases.push(phrase);
        }
    }
    if phrases.len() > MAX_ALTERNATIVES {
        phrases.truncate(MAX_ALTERNATIVES);
        phrases.push("other constructs".to_string());
    }
    Some(match phrases.as_slice() {
        [one] => one.clone(),
        [a, b] => format!("{a} or {b}"),
        many => {
            let (last, rest) = many.split_last().expect("checked non-empty above");
            format!("{}, or {last}", rest.join(", "))
        }
    })
}

/// One grammar rule's meaning, in words a KyzoScript user would recognize.
/// The rules that actually show up as "expected" at a real parse failure —
/// the top-level script forms, rule/atom/expression shapes, options,
/// literals — get a hand-written phrase naming KyzoScript syntax. Every
/// other rule (the grammar has ~200) falls back to its own pest name with
/// underscores turned to spaces, so this can never bottom out in a bare
/// `Rule::foo` debug print, even for a rule nobody hand-wrote a phrase for.
fn describe_rule(r: Rule) -> String {
    let phrase = match r {
        Rule::script | Rule::query_script => "a query (a rule head, e.g. `?[x] := …`)",
        Rule::sys_script => "a `::` system operation",
        Rule::imperative_script => "a `%`-imperative script",
        Rule::rule_head => "a rule head, e.g. `?[x, y]`",
        Rule::rule => "a rule (`head := body`)",
        Rule::const_rule => "a constant rule (`head <- value`)",
        Rule::fixed_rule => "a fixed-rule application (`head <~ Algo(...)`)",
        Rule::option => "a query option (e.g. `:limit 10`, `:order x`)",
        Rule::atom => "a rule-body atom (a relation, a condition, or `not …`)",
        Rule::relation_apply => "a relation application, e.g. `rel[x, y]`",
        Rule::relation_named_apply => "a named relation application, e.g. `rel{x, y}`",
        Rule::rule_apply => "a rule application",
        Rule::negation => "a negated atom (`not …`)",
        Rule::unify | Rule::unify_multi => "a binding (`x = expr` or `x in expr`)",
        Rule::expr | Rule::term => "an expression",
        Rule::grouped => "a parenthesized expression `(…)`",
        Rule::literal | Rule::number | Rule::boolean | Rule::null => "a literal value",
        Rule::string | Rule::quoted_string | Rule::raw_string => "a string literal",
        Rule::ident | Rule::compound_ident | Rule::relation_ident => "an identifier",
        Rule::var => "a variable (starts with a letter or `_`)",
        Rule::param => "a `$parameter`",
        Rule::prog_entry => "the entry marker `?`",
        Rule::list => "a list `[…]`",
        Rule::object => "an object `{…}`",
        Rule::validity_type => "the `Validity` column type",
        Rule::spans_kw => "an `@spans` clause",
        Rule::delta_kw => "an `@delta` clause",
        Rule::delta_sys_kw => "an `@delta_sys` clause",
        Rule::head_arg => "a head argument",
        Rule::apply_args | Rule::named_apply_args => "relation arguments",
        Rule::table_schema => "a schema (`{col: Type, …}`)",
        Rule::table_col => "a column definition",
        Rule::col_type => "a column type (`Int`, `Float`, `String`, …)",
        Rule::sort_arg => "a sort key",
        Rule::relation_op => {
            "a relation operation (`:create`, `:put`, `:insert`, `:update`, `:rm`)"
        }
        Rule::validity_clause | Rule::read_validity_clause => {
            "a validity clause (`@ 'NOW'`, `@ instant`)"
        }
        Rule::fixed_args_list | Rule::fixed_arg => "fixed-rule arguments",
        Rule::index_op => "an index operation (`::index create`, …)",
        Rule::EOI => "the end of the query",
        // Every binary/unary operator token collapses to one phrase: pest
        // reports each operator symbol it tried as its own `Rule`, and
        // listing "op or, op and, op concat, …" fifteen times over is noise
        // a real continuation-of-expression failure would otherwise drown
        // in — the dedup in `describe_expected` then merges every one of
        // these into the single alternative below.
        Rule::op_or
        | Rule::op_and
        | Rule::op_concat
        | Rule::op_add
        | Rule::op_sub
        | Rule::op_mul
        | Rule::op_div
        | Rule::op_mod
        | Rule::op_pow
        | Rule::op_eq
        | Rule::op_ne
        | Rule::op_gt
        | Rule::op_lt
        | Rule::op_ge
        | Rule::op_le
        | Rule::op_coalesce
        | Rule::op_field_access
        | Rule::minus
        | Rule::negate
        | Rule::or_op
        | Rule::in_op
        | Rule::not_op => "an operator (`+`, `==`, `and`, `or`, `.`, …)",
        _ => return format!("{r:?}").replace('_', " "),
    };
    phrase.to_string()
}

/// SQL keywords mapped to the KyzoScript idiom that replaces them. Checked
/// only after a real parse failure — so a relation or column that merely
/// happens to be named `select` never triggers this, since well-formed
/// KyzoScript using that name as an identifier parses fine and never
/// reaches here. Ordered by how early each keyword would appear in a
/// translated-verbatim SQL query, so the first hit is usually the mistake
/// that actually caused the failure.
const SQL_KEYWORD_HINTS: &[(&str, &str)] = &[
    (
        "select",
        "KyzoScript has no SELECT: name the output columns directly in the rule head, \
         e.g. `?[name, age] := person{name, age}`.",
    ),
    (
        "from",
        "KyzoScript has no FROM: the source relation is a body atom, \
         e.g. `?[x] := relation_name{x}`.",
    ),
    (
        "where",
        "KyzoScript has no WHERE: conditions are just more atoms in the rule body, \
         e.g. `?[x] := person{x, age}, age > 18`.",
    ),
    (
        "join",
        "KyzoScript has no JOIN: joins fall out of sharing a variable across two body \
         atoms, e.g. `?[name, item] := person{id, name}, orders{id, item}`.",
    ),
    (
        "group",
        "KyzoScript has no GROUP BY: wrap the head variable in an aggregation instead, \
         e.g. `?[dept, count(name)] := employee{dept, name}`.",
    ),
    (
        "having",
        "KyzoScript has no HAVING: filter after aggregation with a second rule over the \
         aggregated one, e.g. `?[dept] := agg[dept, n := count(name)], n > 10`.",
    ),
    (
        "order",
        "Sorting is the `:order` (or `:sort`) query option: `?[x] := … :order x`.",
    ),
    (
        "insert",
        "Writes are a relation operation on a rule, e.g. \
         `?[x, y] <- [[1, 2]] :put relation_name {x, y}` (or `:insert`).",
    ),
    (
        "update",
        "Writes are a relation operation on a rule, e.g. \
         `?[x, y] <- [[1, 2]] :update relation_name {x, y}`.",
    ),
    (
        "delete",
        "Deletes are the `:rm` relation operation, e.g. \
         `?[key] := … :rm relation_name {key}`.",
    ),
    (
        "values",
        "There's no VALUES clause: the constant rows are the constant rule's own value, \
         e.g. `?[x, y] <- [[1, 2], [3, 4]]`.",
    ),
    (
        "create",
        "Schema definitions use `:create`, e.g. \
         `?[a] <- [] :create relation_name {key: Int, value: String}` — not `CREATE TABLE`.",
    ),
];

/// Does the query text contain one of [`SQL_KEYWORD_HINTS`] as a whole
/// word, close to the failure? Checked in a window around the offending
/// position first (the keyword that actually broke the parse is usually
/// right there — `SELECT` fails immediately), then over the whole text (a
/// SQL shape can fail several tokens after the keyword that gives it away,
/// e.g. `SELECT x FROM t` fails at the bare `x`, not at `SELECT`).
fn sql_refugee_hint(src: &str, near: usize) -> Option<String> {
    let lower = src.to_ascii_lowercase();
    let window_start = near.saturating_sub(32);
    let window_end = (near + 32).min(lower.len());
    let window = lower.get(window_start..window_end).unwrap_or("");
    for (keyword, hint) in SQL_KEYWORD_HINTS {
        if has_word(window, keyword) {
            return Some((*hint).to_string());
        }
    }
    for (keyword, hint) in SQL_KEYWORD_HINTS {
        if has_word(&lower, keyword) {
            return Some((*hint).to_string());
        }
    }
    None
}

/// Whole-word search: `word` must appear as its own token, delimited by
/// anything that isn't an identifier character, so `selected_at` never
/// matches `select`.
fn has_word(haystack: &str, word: &str) -> bool {
    haystack
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|token| token == word)
}

// ─────────────────────────────────────────────────────────────────────────
// The nesting ceiling.
//
// The parser is recursive descent (pest) feeding recursive builders, so
// input nesting spends native stack, and past some depth the process dies
// with an uncatchable stack-overflow abort. That violates law 5 of the
// engine (`query/mod.rs`): no query text may panic the process; errors
// are values. The enforcement is refusal over resource death: a single
// cheap pass over the raw text bounds structural nesting *before* any
// recursive work begins, so the ceiling is a language limit exactly like
// the range of an `i64` — generous beyond any legitimate query, and far
// below any overflow threshold.
// ─────────────────────────────────────────────────────────────────────────

/// The maximum structural nesting depth of KyzoScript text: brackets
/// (`()`/`[]`/`{}`), imperative blocks (`%if`/`%loop` … `%end`), nested
/// block comments, open `not` prefixes, and operators stacked within a
/// single expression all count against it. A language limit, not a tuning
/// knob, placed by measurement:
///
/// - the deepest legitimate script in the corpus nests under 10 levels;
/// - one level of nested-list parse costs ~2.5 KiB of stack in a release
///   build and ~11–12 KiB in a debug build (measured: on a 2 MiB thread —
///   Rust's spawned-thread default — the unguarded parse overflowed
///   between depth 768 and 1024 in release and between 160 and 192 in
///   debug).
///
/// At 64, the worst construct needs ≈ 0.8 MiB even in a debug build: safe
/// on any default-sized stack in both profiles, with the whole ceiling
/// still ~7x deeper than any real query.
pub(crate) const NESTING_CEILING: usize = 64;

/// User text nests deeper than the language allows. Law-5 enforcement
/// (refusal over resource death): unbounded nesting is a native stack
/// overflow — an uncatchable process abort — so the language itself has a
/// nesting ceiling, like the range of an `i64` has ends. Raised by the
/// pre-parse scan ([`reject_excessive_nesting`]) and by the expression
/// builder's own depth counter (`parse/expr.rs`), whichever sees the
/// depth first.
#[derive(Debug, Error, Diagnostic)]
#[error("nesting is {depth} levels deep, over the KyzoScript ceiling of {ceiling}")]
#[diagnostic(code(parser::nesting_too_deep))]
#[diagnostic(help(
    "The nesting ceiling is a limit of the language, like the range of an \
     integer: deep enough for any legitimate query, and low enough that a \
     recursive parse can never exhaust the stack and kill the process. \
     Flatten the query, or pass deeply nested data as a $parameter instead \
     of a literal."
))]
pub(crate) struct NestingTooDeep {
    /// The depth at the refused token (one past the ceiling).
    pub(crate) depth: usize,
    /// The ceiling it crossed: [`NESTING_CEILING`].
    pub(crate) ceiling: usize,
    /// The first token past the ceiling.
    #[label("nesting passes the ceiling here")]
    pub(crate) span: SourceSpan,
}

/// The pre-parse structural scan: one pass over the raw text, counting —
/// not parsing — the nesting the recursive parse would have to follow.
/// Counted against [`NESTING_CEILING`], jointly: bracket depth, `%if`/
/// `%loop` … `%end` block depth, nested block-comment depth (the
/// `BLOCK_COMMENT` rule is recursive), and *open* `not` prefixes (the
/// `negation` rule is recursive, needs no brackets, and interleaves with
/// them — `[not ([not (…` — so open negations are tracked per bracket
/// level and closed by the separators that end an atom: `,`, `;`, `or`,
/// or the level's closing bracket). Chains of `-`/`!` prefixes and
/// stacked binary operators recurse in the *Pratt builder*, not in pest,
/// and are bounded there (`parse/expr.rs`) — belt and suspenders around
/// the same ceiling.
///
/// The scan is a faithful mini-lexer for the only constructs that decide
/// what is structure and what is content: comments and string literals
/// (which is why `raw_string` must be atomic — divergence 2 in
/// `kyzoscript.pest`). Where the scan cannot know the grammar's intent
/// without truly parsing (`%if` in expression position, `not` as prose),
/// it over-counts, never under-counts: the failure mode is a spurious
/// refusal of pathological text, never an unscanned recursion.
pub(crate) fn reject_excessive_nesting(src: &str) -> Result<(), NestingTooDeep> {
    /// Could this byte continue an identifier/number/param token? `$` and
    /// `.` count because `param` and the float/compound rules greedily
    /// consume trailing `_`s, which must then not read as a raw-string
    /// sigil. Any non-ASCII byte counts (over-approximating XID_CONTINUE
    /// in the safe, over-counting direction).
    fn wordish(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'$' || b >= 0x80
    }
    /// One-character span at byte `i`, labeling the refused token.
    fn span_at(src: &str, i: usize) -> SourceSpan {
        let len = src[i..].chars().next().map_or(0, char::len_utf8);
        SourceSpan(i, len)
    }
    /// Skip a raw string body: opener `"` at `b[i]`, `sigils` leading
    /// underscores already seen; the terminator is a `"` followed by that
    /// many underscores (mirrors the atomic `raw_string` rule, `PEEK`
    /// included). Returns the index just past the token (or the end: an
    /// unterminated string is pest's error to report).
    fn skip_raw_string(b: &[u8], mut i: usize, sigils: usize) -> usize {
        i += 1;
        while i < b.len() {
            if b[i] == b'"'
                && b.get(i + 1..i + 1 + sigils)
                    .is_some_and(|tail| tail.iter().all(|&c| c == b'_'))
            {
                return i + 1 + sigils;
            }
            i += 1;
        }
        b.len()
    }
    /// Does the word `word` start at `b[i]`, as a whole token?
    fn word_at(b: &[u8], i: usize, word: &[u8]) -> bool {
        b[i..].starts_with(word) && !b.get(i + word.len()).copied().is_some_and(wordish)
    }

    let too_deep = |i: usize| NestingTooDeep {
        depth: NESTING_CEILING + 1,
        ceiling: NESTING_CEILING,
        span: span_at(src, i),
    };

    let b = src.as_bytes();
    let n = b.len();
    let mut i = 0;
    // Bracket + %-block depth (block comments count via a local).
    let mut depth = 0usize;
    // Open `not` prefixes, per bracket level; `open_nots` is their sum.
    // The recursion the parse will perform is bounded by
    // `depth + open_nots`, so that is what the ceiling applies to.
    let mut nots_at_level: Vec<usize> = vec![0];
    let mut open_nots = 0usize;
    // Whether the previous byte can continue a token: a `"` after `a___`
    // is a plain string (the `_`s belong to the identifier); after a
    // token boundary, `___"` opens a raw string.
    let mut prev_wordish = false;

    while i < n {
        match b[i] {
            b' ' | b'\t' | b'\r' | b'\n' => {
                prev_wordish = false;
                i += 1;
            }
            b'#' => {
                // Line comment: a token separator, like whitespace.
                prev_wordish = false;
                while i < n && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                // Block comment; nesting recurses in pest, so it counts.
                prev_wordish = false;
                let mut comment_depth = 1usize;
                if depth + open_nots + comment_depth > NESTING_CEILING {
                    return Err(too_deep(i));
                }
                i += 2;
                while i < n && comment_depth > 0 {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        comment_depth += 1;
                        if depth + open_nots + comment_depth > NESTING_CEILING {
                            return Err(too_deep(i));
                        }
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        comment_depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            b'\'' => {
                // Single-quoted string: backslash escapes its next char.
                prev_wordish = false;
                i += 1;
                while i < n {
                    match b[i] {
                        b'\\' => i += 2,
                        b'\'' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
            }
            b'"' => {
                // Double-quoted string: raw (`raw_string` wins the ordered
                // choice), terminated by the bare quote.
                i = skip_raw_string(b, i, 0);
                prev_wordish = false;
            }
            b'_' if !prev_wordish => {
                // A token-initial underscore run: raw-string sigil if a
                // quote follows, otherwise an identifier.
                let start = i;
                while i < n && b[i] == b'_' {
                    i += 1;
                }
                if i < n && b[i] == b'"' {
                    i = skip_raw_string(b, i, i - start);
                    prev_wordish = false;
                } else {
                    prev_wordish = true;
                }
            }
            b'(' | b'[' | b'{' => {
                depth += 1;
                if depth + open_nots > NESTING_CEILING {
                    return Err(too_deep(i));
                }
                nots_at_level.push(0);
                prev_wordish = false;
                i += 1;
            }
            b')' | b']' | b'}' => {
                // Closing a level ends every negation opened inside it.
                depth = depth.saturating_sub(1);
                if nots_at_level.len() > 1 {
                    // In bounds: length checked to be at least 2.
                    open_nots -= nots_at_level.pop().unwrap_or(0);
                } else {
                    // A stray closer (a parse error later): stay counting.
                    open_nots -= std::mem::take(&mut nots_at_level[0]);
                }
                prev_wordish = false;
                i += 1;
            }
            b',' | b';' => {
                // Separators end any negation open at the current level
                // (a negated atom cannot span an unbracketed `,`/`;`).
                if let Some(last) = nots_at_level.last_mut() {
                    open_nots -= std::mem::take(last);
                }
                prev_wordish = false;
                i += 1;
            }
            b'%' => {
                // Imperative blocks nest: `%if`(`_not`)/`%loop` open one,
                // `%end` closes one. Prefix-matched exactly as pest
                // matches the literals; `%` as the modulus operator can
                // only over-count.
                let rest = &b[i..];
                if rest.starts_with(b"%loop") || rest.starts_with(b"%if") {
                    depth += 1;
                    if depth + open_nots > NESTING_CEILING {
                        return Err(too_deep(i));
                    }
                    i += if rest.starts_with(b"%loop") { 5 } else { 3 };
                    // `%if_not`'s tail must not read as a raw-string sigil.
                    prev_wordish = true;
                } else if rest.starts_with(b"%end") {
                    depth = depth.saturating_sub(1);
                    i += 4;
                    prev_wordish = true;
                } else {
                    prev_wordish = false;
                    i += 1;
                }
            }
            b'n' if !prev_wordish && word_at(b, i, b"not") => {
                // A `not` prefix: the `negation` grammar rule recurses.
                if let Some(last) = nots_at_level.last_mut() {
                    *last += 1;
                }
                open_nots += 1;
                if depth + open_nots > NESTING_CEILING {
                    return Err(too_deep(i));
                }
                prev_wordish = true;
                i += 3;
            }
            b'o' if !prev_wordish && word_at(b, i, b"or") => {
                // `or` separates disjuncts: like `,`, it ends any negation
                // open at the current level.
                if let Some(last) = nots_at_level.last_mut() {
                    open_nots -= std::mem::take(last);
                }
                prev_wordish = true;
                i += 2;
            }
            c => {
                prev_wordish = wordish(c);
                i += 1;
            }
        }
    }
    Ok(())
}

/// Parse a column type expression (`Int`, `[Float; 3]?`, …) on its own.
pub(crate) fn parse_type(src: &str) -> Result<NullableColType> {
    let parsed = single(
        ScriptParser::parse(Rule::col_type_with_term, src).into_diagnostic()?,
        "the parsed col_type_with_term",
        Rule::col_type_with_term,
    )?;
    parse_nullable_type(parsed.children().expect("the col_type child")?)
}

/// Parse a standalone expression, with `$params` substituted from
/// `param_pool`.
pub(crate) fn parse_expressions(
    src: &str,
    param_pool: &BTreeMap<String, DataValue>,
) -> Result<Expr> {
    reject_excessive_nesting(src)?;
    let parsed = single(
        ScriptParser::parse(Rule::expression_script, src)
            .map_err(|e| ParseError::from_pest(e, src))?,
        "the parsed expression_script",
        Rule::expression_script,
    )?;

    build_expr(parsed.children().expect("the expression")?, param_pool)
}

/// This parses a text script into the AST used by KyzoDB.
///
/// Note! This is an unstable interface, the signature may change between
/// releases. Depend on it at your own risk.
///
/// * `src` - the script to parse
///
/// * `param_pool` - the list of parameters to execute the script with.
///   These are substituted into the syntax tree during parsing.
///
/// * `fixed_rules` - a mapping of fixed rule names to their
///   implementations. These are substituted into the syntax tree during
///   parsing.
///
/// * `cur_vld` - the current timestamp, substituted into expressions where
///   validity is relevant.
pub fn parse_script(
    src: &str,
    param_pool: &BTreeMap<String, DataValue>,
    fixed_rules: &BTreeMap<String, Arc<dyn FixedRule>>,
    cur_vld: ValidityTs,
) -> Result<Script> {
    reject_excessive_nesting(src)?;
    let parsed = single(
        ScriptParser::parse(Rule::script, src).map_err(|e| ParseError::from_pest(e, src))?,
        "the parsed script",
        Rule::script,
    )?;
    Ok(match parsed.as_rule() {
        Rule::query_script => {
            let q = parse_query(parsed.into_inner(), param_pool, fixed_rules, cur_vld)?;
            Script::Single(Box::new(q))
        }
        Rule::imperative_script => {
            let p = parse_imperative_block(parsed, param_pool, fixed_rules, cur_vld)?;
            Script::Imperative(p)
        }
        Rule::sys_script => Script::Sys(parse_sys(
            parsed.into_inner(),
            param_pool,
            fixed_rules,
            cur_vld,
        )?),
        _ => return Err(unexpected("a script species", &parsed)),
    })
}

#[cfg(test)]
mod tests {
    use std::cmp::Reverse;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::*;
    use crate::data::symb::Symbol;

    fn vld() -> ValidityTs {
        ValidityTs(Reverse(0))
    }

    fn parse(src: &str) -> Result<Script> {
        parse_script(src, &Default::default(), &Default::default(), vld())
    }

    /// A registered stub so fixed-rule syntax can be exercised without the
    /// fixed-rule tier.
    struct StubRule(usize);

    impl crate::data::program::FixedRule for StubRule {
        fn arity(
            &self,
            _options: &BTreeMap<SmartString<LazyCompact>, Expr>,
            _rule_head: &[Symbol],
            _span: SourceSpan,
        ) -> Result<usize> {
            Ok(self.0)
        }
        fn run(
            &self,
            _payload: crate::fixed_rule::FixedRulePayload<'_>,
            _out: &mut crate::fixed_rule::FixedRuleOutput,
            _cancel: crate::fixed_rule::CancelFlag,
        ) -> Result<()> {
            unreachable!("test stub: never run")
        }
    }

    /// Representative scripts of all three species parse. Each entry is a
    /// (description, script) pair so a failure names the case.
    #[test]
    fn smoke_corpus_parses() {
        let corpus: &[(&str, &str)] = &[
            ("const entry", "?[] <- [[1, 2, 3]]"),
            (
                "unification and arithmetic",
                "?[a, b] := a in [1, 2], b = a * 2",
            ),
            (
                "recursion over a const relation",
                r#"
                parent[c, p] <- [['a', 'b'], ['b', 'c']]
                anc[a, b] := parent[a, b]
                anc[a, b] := parent[a, c], anc[c, b]
                ?[a] := anc[a, 'c']
                "#,
            ),
            (
                "aggregation with argument",
                "?[a, collect(b, 5)] := a in [1], b in [2, 3]",
            ),
            (
                "negation and disjunction",
                "?[a] := a in [1, 2, 3], (not a == 2) or a == 3",
            ),
            (
                "stored relation write",
                "?[a, b] := a in [1], b in [2] :put rel {a => b}",
            ),
            ("create with body", "?[a] <- [[1]] :create t {a}"),
            (
                "options fold to constants",
                "?[a] := a in [1, 2, 3] :limit 1 + 1 :offset 1 :timeout 10.5 :order -a",
            ),
            ("assertion", "?[a] := a in [1] :assert some"),
            ("named-field relation", "?[a] := *rel{f: a}"),
            ("time travel as-of", "?[a] := *rel[a, b @ 'NOW']"),
            (
                "time travel explicit",
                "?[a] := *rel[a, b @ '2020-01-01T00:00:00Z']",
            ),
            ("index search atom", "?[a] := ~rel:idx{a: q | k: 10}"),
            ("string escapes", r#"?[a] := a = "x\nA""#),
            ("json object literal", "?[a] := a = {'k': 1}"),
            ("sys: list relations", "::relations"),
            ("sys: columns", "::columns rel"),
            ("sys: remove", "::remove a, b"),
            ("sys: rename", "::rename a -> b"),
            ("sys: access level", "::access_level read_only a, b"),
            ("sys: explain", "::explain { ?[a] := a in [1] }"),
            ("sys: plain index", "::index create r:idx {a, b}"),
            (
                "sys: hnsw index",
                "::hnsw create r:i {dim: 128, m: 50, ef_construction: 20, dtype: F32, \
                 fields: [v], distance: Cosine, filter: k != 'foo', extend_candidates: false, \
                 keep_pruned_connections: false}",
            ),
            (
                "sys: fts index",
                "::fts create r:i {extractor: v, tokenizer: Simple, filters: [Lowercase]}",
            ),
            (
                "sys: lsh index",
                "::lsh create r:i {extractor: v, tokenizer: Simple, n_perm: 200, \
                 target_threshold: 0.7, n_gram: 3}",
            ),
            ("sys: index drop", "::index drop r:idx"),
            (
                "sys: triggers",
                "::set_triggers rel on put { ?[a, b] := _new[a, b] }",
            ),
            ("sys: merkle root (whole store)", "::merkle_root"),
            ("sys: merkle root (relation)", "::merkle_root rel"),
            ("sys: kill", "::running"),
            (
                "imperative: chain with temp store",
                "{ ?[a] <- [[1]] :replace _t {a} } %swap _t _t2 %return _t2",
            ),
            (
                "imperative: if and loop",
                "%loop { ?[a] := a in [1] } %if { ?[a] := a in [1] } %then %break %end %end",
            ),
            ("imperative: debug", "{ ?[a] <- [[1]] } %debug _x"),
        ];
        for (what, src) in corpus {
            if let Err(err) = parse(src) {
                panic!("{what}: failed to parse {src:?}: {err:?}");
            }
        }
    }

    /// The three species dispatch to the right variant.
    #[test]
    fn species_dispatch() {
        assert!(matches!(parse("?[] <- [[1]]").unwrap(), Script::Single(_)));
        assert!(matches!(parse("::relations").unwrap(), Script::Sys(_)));
        assert!(matches!(
            parse("{ ?[a] <- [[1]] } %return _x").unwrap(),
            Script::Imperative(_)
        ));
    }

    /// `::lsh create`'s `n_gram`/`n_perm` options used to cast `get_int()`'s
    /// `i64` straight to `usize` (`as usize`) BEFORE the `> 0` positivity
    /// `ensure!` ran: `n_gram: -1` wraps to a huge `usize` and sails through
    /// the later check, eventually reaching `Vec::with_capacity` in the LSH
    /// engine (`engines/lsh.rs::MinHashPermutations::new`) with an
    /// allocator-aborting size. `dim`/`ef_construction`/`m_neighbours`
    /// (`::hnsw create`) already validated the `i64` before casting; `n_gram`
    /// and `n_perm` now match that shape and refuse at parse time instead.
    #[test]
    fn lsh_negative_options_refuse_at_parse_not_wrap() {
        for (what, src) in [
            ("n_gram", "::lsh create r:i {n_gram: -1}"),
            ("n_perm", "::lsh create r:i {n_perm: -1}"),
        ] {
            let err = parse(src).expect_err(&format!("{what}: negative option must refuse"));
            let msg = err.to_string();
            assert!(
                msg.contains("must be positive"),
                "{what}: expected a positivity refusal, got: {msg}"
            );
        }
    }

    /// `::merkle_root` parses to the right variant: bare ⇒ whole keyspace
    /// (`None`), with a name ⇒ that relation (`Some`).
    #[test]
    fn merkle_root_op_parses_to_the_right_variant() {
        match parse("::merkle_root").unwrap() {
            Script::Sys(SysOp::MerkleRoot(None)) => {}
            other => panic!("bare ::merkle_root parsed as {other:?}"),
        }
        match parse("::merkle_root my_rel").unwrap() {
            Script::Sys(SysOp::MerkleRoot(Some(rel))) => {
                assert_eq!(rel.name.as_str(), "my_rel");
            }
            other => panic!("::merkle_root my_rel parsed as {other:?}"),
        }
    }

    /// Parameters substitute during parsing; a missing one is an error.
    #[test]
    fn params_substitute() {
        let mut pool = BTreeMap::new();
        pool.insert("p".to_string(), DataValue::from(7));
        assert!(parse_script("?[a] := a = $p", &pool, &Default::default(), vld()).is_ok());
        assert!(parse_script("?[a] := a = $q", &pool, &Default::default(), vld()).is_err());
    }

    /// Fixed-rule syntax resolves against the registry, including a
    /// named-field stored-relation argument — the CozoDB original panicked
    /// on that shape (it stripped `:` off a `*`-prefixed name).
    #[test]
    fn fixed_rules_resolve() {
        let mut rules: BTreeMap<String, Arc<dyn FixedRule>> = BTreeMap::new();
        rules.insert("Stub".to_string(), Arc::new(StubRule(2)));
        let parse = |src: &str| parse_script(src, &Default::default(), &rules, vld());
        assert!(parse("?[a, b] <~ Stub(r[x, y], k: 1)").is_ok());
        assert!(parse("?[a, b] <~ Stub(*sr[x, y])").is_ok());
        assert!(
            parse("?[a, b] <~ Stub(*sr{f: x})").is_ok(),
            "named stored-relation args must parse (upstream panicked)"
        );
        // Unknown rule: an error naming it.
        assert!(parse("?[a, b] <~ Nonexistent()").is_err());
        // Head arity mismatch against the implementation's arity.
        assert!(parse("?[a, b, c] <~ Stub()").is_err());
    }

    // ── Nesting: refusal over resource death (law 5) ────────────────────

    /// Run `parse` on a thread with `stack_kib` KiB of stack, so a
    /// would-be stack overflow fails the test instead of hiding under the
    /// runner's 8 MiB main-thread stack.
    fn parse_on_stack(src: String, stack_kib: usize) -> Result<Script> {
        std::thread::Builder::new()
            .stack_size(stack_kib * 1024)
            .spawn(move || parse(&src))
            .expect("spawning the sized-stack thread")
            .join()
            .expect("the parse thread must return, not die")
    }

    /// A refusal must arrive *before* the recursive work: 256 KiB is far
    /// below any default stack, so surviving here proves the headroom.
    fn assert_nesting_refusal(what: &str, src: String) {
        let err = parse_on_stack(src, 256).expect_err(what);
        assert!(
            err.downcast_ref::<NestingTooDeep>().is_some(),
            "{what}: expected the typed NestingTooDeep refusal, got: {err:?}"
        );
    }

    /// A parse within the ceiling must succeed on the stack size the
    /// ceiling is documented against: 2 MiB, Rust's spawned-thread
    /// default (the measured basis for [`NESTING_CEILING`]).
    fn assert_parses_on_default_stack(what: &str, src: String) {
        parse_on_stack(src, 2048).unwrap_or_else(|e| panic!("{what}: {e:?}"));
    }

    /// F1 regression (exponential backtracking): deeply nested literals
    /// parse, and parse fast. Under the upstream `(x ~ ",")* ~ x?` grammar
    /// shape each nesting level re-parsed its contents (~30x per +5
    /// levels: depth 20 took ~20s here; depth 40 would take centuries), so
    /// *terminating at all* within the wall guard is the structural
    /// assert.
    #[test]
    fn hostile_battery_nested_literals_parse_fast() {
        let start = std::time::Instant::now();
        for depth in [15usize, 20, 25, 40] {
            let list = format!("?[x] := x = {}1{}", "[".repeat(depth), "]".repeat(depth));
            parse(&list).unwrap_or_else(|e| panic!("depth-{depth} list: {e:?}"));
            let object = format!(
                "?[x] := x = {}1{}",
                "{'k':".repeat(depth),
                "}".repeat(depth)
            );
            parse(&object).unwrap_or_else(|e| panic!("depth-{depth} object: {e:?}"));
            let parens = format!("?[x] := x = {}1{}", "(".repeat(depth), ")".repeat(depth));
            parse(&parens).unwrap_or_else(|e| panic!("depth-{depth} parens: {e:?}"));
            let args = format!("?[x] := x = f{}1{}", "(g".repeat(depth), ")".repeat(depth));
            parse(&args).unwrap_or_else(|e| panic!("depth-{depth} apply args: {e:?}"));
        }
        // Generous wall guard: the whole battery is microseconds now; the
        // old grammar could not finish depth 25 in a minute.
        assert!(
            start.elapsed() < std::time::Duration::from_secs(30),
            "nested-literal parsing has regressed toward backtracking: {:?}",
            start.elapsed()
        );
    }

    /// F2 (stack overflow): structural nesting past the ceiling is a
    /// spanned, typed refusal — before any recursive work, proven by
    /// surviving on a small stack. The unterminated shapes ensure the
    /// scan itself refuses (a real parse would error later anyway).
    #[test]
    fn nesting_past_ceiling_is_refused_not_fatal() {
        for (what, src) in [
            ("deep parens", format!("?[x] := x = {}1", "(".repeat(300))),
            ("deep lists", format!("?[x] := x = {}1", "[".repeat(300))),
            (
                "deep objects",
                format!("?[x] := x = {}", "{'k':".repeat(300)),
            ),
            ("deep %loop chain", "%loop ".repeat(300)),
            (
                "deep block comments",
                format!("{}?[x] := x = 1", "/*".repeat(300)),
            ),
            (
                "deep not chain",
                format!("?[x] := {}x == 1", "not ".repeat(5000)),
            ),
            (
                "not chain glued by comments",
                format!("?[x] := {}x == 1", "not /*c*/ ".repeat(5000)),
            ),
            (
                // Brackets and negations interleave into one recursion, so
                // they count against one joint ceiling.
                "interleaved not/bracket nesting",
                format!("?[x] := x = {}1", "[not ([".repeat(100)),
            ),
        ] {
            assert_nesting_refusal(what, src);
        }

        // The refusal is labeled at the first token past the ceiling: with
        // the query's own `[`/`]` balanced out, that is open-paren number
        // NESTING_CEILING + 1.
        let prefix = "?[x] := x = ";
        let src = format!("{prefix}{}1", "(".repeat(300));
        let err = parse(&src).expect_err("300 parens must be refused");
        let refusal = err
            .downcast_ref::<NestingTooDeep>()
            .expect("typed NestingTooDeep");
        assert_eq!(refusal.ceiling, NESTING_CEILING);
        assert_eq!(refusal.depth, NESTING_CEILING + 1);
        assert_eq!(refusal.span, SourceSpan(prefix.len() + NESTING_CEILING, 1));

        // The ceiling is generous — shapes just under it parse — and it
        // holds on the default-sized stack it is documented against.
        let depth = NESTING_CEILING - 4;
        assert_parses_on_default_stack(
            "a list nested just under the ceiling",
            format!("?[x] := x = {}1{}", "[".repeat(depth), "]".repeat(depth)),
        );
        // Each `[not ([` unit opens three brackets and one negation.
        let units = NESTING_CEILING / 4 - 2;
        assert_parses_on_default_stack(
            "an interleaved shape just under the ceiling",
            format!(
                "?[x] := x = {}1{}",
                "[not ([".repeat(units),
                "])]".repeat(units)
            ),
        );
        // Scattered negations do not accumulate: only *open* ones nest.
        assert_parses_on_default_stack(
            "many sibling negations",
            format!("?[a] := a in [1]{}", ", not a == 2".repeat(500)),
        );
        assert_parses_on_default_stack(
            "many or-separated negations",
            format!("?[a] := not a == 0{}", " or not a == 2".repeat(500)),
        );
    }

    /// F2, bracketless shapes: operator chains recurse in the Pratt
    /// builder (and in dropping/evaluating the built tree) rather than in
    /// pest, so the expression builder's own depth counter must refuse
    /// them — same typed error, same small-stack proof.
    #[test]
    fn bracketless_operator_chains_are_refused_not_fatal() {
        for (what, src) in [
            (
                "unary-minus chain",
                format!("?[x] := x = {}1", "-".repeat(5000)),
            ),
            (
                "unary-negate chain",
                format!("?[x] := x = {}true", "!".repeat(5000)),
            ),
            (
                "right-associative power chain",
                format!("?[x] := x = 1{}", "^1".repeat(5000)),
            ),
        ] {
            assert_nesting_refusal(what, src);
        }
        // Under the ceiling, the same shapes parse (on the default stack).
        assert_parses_on_default_stack(
            "a modest minus chain",
            format!("?[x] := x = {}1", "-".repeat(NESTING_CEILING - 4)),
        );
    }

    /// The scan counts structure, not content: brackets inside strings and
    /// comments are data, not nesting, and must not trip the ceiling.
    #[test]
    fn nesting_scan_ignores_string_and_comment_content() {
        let deep = "(".repeat(1000);
        for (what, src) in [
            ("double-quoted", format!("?[x] := x = \"{deep}\"")),
            ("single-quoted", format!("?[x] := x = '{deep}'")),
            ("raw", format!("?[x] := x = ___\"{deep}\"___")),
            ("line comment", format!("?[x] := x = 1 # {deep}")),
            ("block comment", format!("?[x] := x = 1 /* {deep} */")),
        ] {
            parse(&src).unwrap_or_else(|e| panic!("{what}: {e:?}"));
        }
        // And conversely, quotes inside strings do not hide following
        // structure from the scan (the raw-string terminator is exact).
        assert_nesting_refusal(
            "brackets after a quote-bearing raw string",
            format!("?[x] := x = ___\"a\"b\"___ ; {}", "[".repeat(300)),
        );
    }

    /// Divergence 2 pins (`raw_string` is now atomic): comment characters
    /// inside strings stay string content, and a raw string is one
    /// contiguous token — the upstream acceptance of whitespace between
    /// the sigil and its quote is gone.
    #[test]
    fn raw_string_is_one_contiguous_token() {
        let eval = |src: &str| {
            parse_expressions(src, &Default::default())
                .and_then(|e| e.eval_to_const())
                .unwrap_or_else(|e| panic!("{src}: {e:?}"))
        };
        assert_eq!(eval(r##""a#b""##), DataValue::from("a#b"));
        assert_eq!(eval(r#""a/*x*/b""#), DataValue::from("a/*x*/b"));
        assert_eq!(
            eval(r#"___"raw " content"___"#),
            DataValue::from("raw \" content")
        );
        // Upstream parsed `__ "a"__` as a raw string across the gap.
        assert!(
            parse_expressions("__ \"a\"__", &Default::default()).is_err(),
            "a split raw-string sigil must not lex as a string"
        );
    }

    // ── F1 grammar-equivalence pins: empty / one / many / trailing ─────

    /// The [SEQ]/[SEQ1] rewrites accept exactly the upstream language:
    /// every separated-sequence rule pinned across its empty, one, many
    /// and trailing-comma cases — and, for [SEQ1] rules, pinned to keep
    /// *rejecting* trailing separators.
    #[test]
    fn separated_sequence_language_pinned() {
        let mut rules: BTreeMap<String, Arc<dyn FixedRule>> = BTreeMap::new();
        rules.insert("Stub".to_string(), Arc::new(StubRule(2)));
        let parse = |src: &str| parse_script(src, &Default::default(), &rules, vld());

        let accepted: &[(&str, &str)] = &[
            // list [SEQ]
            ("empty list", "?[x] := x = []"),
            ("one-item list", "?[x] := x = [1]"),
            ("one-item list, trailing", "?[x] := x = [1,]"),
            ("many-item list", "?[x] := x = [1, 2]"),
            ("many-item list, trailing", "?[x] := x = [1, 2,]"),
            // object [SEQ]
            ("empty object", "?[x] := x = {}"),
            ("one-pair object", "?[x] := x = {'a': 1}"),
            ("one-pair object, trailing", "?[x] := x = {'a': 1,}"),
            (
                "many-pair object, trailing",
                "?[x] := x = {'a': 1, 'b': 2,}",
            ),
            // apply_args [SEQ]
            ("empty args", "?[x] := x = f()"),
            ("one arg, trailing", "?[x] := x = f(1,)"),
            ("many args, trailing", "?[x] := x = f(1, 2,)"),
            // rule_head [SEQ]
            ("head, trailing", "?[a,] := a in [1]"),
            ("const head, trailing", "?[a,] <- [[1]]"),
            // rule_body [SEQ]
            ("body, trailing", "?[a] := a in [1],"),
            // fixed_args_list + fixed_rule_rel + fixed rels [SEQ]
            ("fixed args, trailing", "?[a, b] <~ Stub(k: 1,)"),
            ("fixed rule rel, trailing", "?[a, b] <~ Stub(r[x, y,])"),
            ("fixed stored rel, trailing", "?[a, b] <~ Stub(*sr[x, y,])"),
            (
                "fixed stored rel, trailing + validity",
                "?[a, b] <~ Stub(*sr[x, y, @ 'NOW'])",
            ),
            ("fixed named rel, trailing", "?[a, b] <~ Stub(*sr{f: x,})"),
            // relation_apply / named_apply_args [SEQ]
            ("relation args, trailing", "?[a] := *rel[a, b,]"),
            (
                "relation args, trailing + validity",
                "?[a] := *rel[a, b, @ 'NOW']",
            ),
            ("named relation args, empty", "?[a] := a in [1], *rel{}"),
            ("named relation args, trailing", "?[a] := *rel{f: a,}"),
            // search_apply parameters [SEQ]
            ("search params, trailing", "?[a] := ~rel:idx{a: q | k: 10,}"),
            // disjunction [SEQ1]
            ("disjunction", "?[a] := a in [1] or a in [2] or a in [3]"),
            // sort_option [SEQ1]
            ("sort, many", "?[a] := a in [1] :sort a, -a"),
            // table schema [SEQ]
            ("schema cols, trailing", "?[a] <- [[1]] :create t {a,}"),
            (
                "schema keys and values, trailing",
                "?[a, b] <- [[1, 2]] :create t {a, => b,}",
            ),
            (
                "tuple col type: empty, and trailing",
                "?[a, b] <- [[1, 2]] :create t {a: (), b: (Int, Float,)}",
            ),
            // sys-op sequences
            ("remove: one", "::remove a"),
            ("remove: many", "::remove a, b"),
            ("rename: many", "::rename a -> b, c -> d"),
            ("access level: many", "::access_level hidden a, b"),
            ("index create: trailing", "::index create r:idx {a, b,}"),
            (
                "index create adv: trailing",
                "::fts create r:i {extractor: v, tokenizer: Simple, filters: [Lowercase],}",
            ),
            // imperative %return [SEQ1]-optional
            ("return: empty", "{ ?[a] <- [[1]] } %return"),
            ("return: one", "{ ?[a] <- [[1]] } %return _t"),
            ("return: many", "{ ?[a] <- [[1]] } %return _t, _u"),
        ];
        for (what, src) in accepted {
            if let Err(err) = parse(src) {
                panic!("{what}: must parse, got: {err:?}");
            }
        }

        // [SEQ1] rules keep refusing trailing separators, exactly as
        // upstream did.
        let rejected: &[(&str, &str)] = &[
            ("remove: trailing comma", "::remove a,"),
            ("rename: trailing comma", "::rename a -> b,"),
            ("access level: trailing comma", "::access_level hidden a,"),
            ("sort: trailing comma", "?[a] := a in [1] :sort a,"),
            ("disjunction: trailing or", "?[a] := a in [1] or"),
            ("return: trailing comma", "{ ?[a] <- [[1]] } %return _t,"),
        ];
        for (what, src) in rejected {
            assert!(parse(src).is_err(), "{what}: must be refused");
        }

        // `::index create r:idx {}`: the empty column list passes the
        // *grammar* (as upstream) and is refused semantically — the error
        // is the typed empty-index refusal, not a parse error.
        let err = parse("::index create r:idx {}").expect_err("empty index refused semantically");
        assert!(
            format!("{err:?}").contains("at least one column"),
            "expected the semantic empty-index refusal, got: {err:?}"
        );

        // param_list [SEQ] (entered directly by the const-rule head
        // inference path).
        for src in ["[[]]", "[[$a]]", "[[$a,]]", "[[$a, $b,]]"] {
            assert!(
                ScriptParser::parse(Rule::param_list, src).is_ok(),
                "param_list must accept {src}"
            );
        }
    }

    /// Feeding a consumer the wrong grammar rule produces a spanned
    /// [`GrammarShapeError`]-class error, not a panic: the typed-accessor
    /// layer's whole point. (Simulates grammar/consumer drift.)
    #[test]
    fn grammar_drift_errors_instead_of_panicking() {
        // `parse_query` fed an expression's pairs (not a query's).
        let pairs = ScriptParser::parse(Rule::expression_script, "1 + 1").unwrap();
        let wrong = pairs.clone();
        let res = crate::parse::query::parse_query(
            wrong,
            &Default::default(),
            &Default::default(),
            vld(),
        );
        let err = res.expect_err("wrong-rule input must error");
        assert!(
            format!("{err:?}").contains("grammar"),
            "error should name the grammar drift: {err:?}"
        );

        // `parse_sys` fed an exhausted stream.
        let mut spent = pairs.clone();
        spent.next();
        let res =
            crate::parse::sys::parse_sys(spent, &Default::default(), &Default::default(), vld());
        assert!(res.is_err(), "exhausted input must error, not panic");
    }

    #[test]
    fn eyeball_diagnostics() {
        for src in [
            "SELECT name, age FROM person WHERE age > 18",
            "?[x] := *person[x educ",
            "?[x] := *person{x}, x >",
            "?[x] := *person{x} GROUP BY x",
            "DELETE FROM person WHERE x = 1",
            "?[cout(x)] := *person{x}",
        ] {
            let err = parse(src).expect_err("must fail to parse");
            println!("=== {src:?} ===\n{err:?}\n");
        }
    }
}
