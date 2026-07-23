/*
 * Copyright 2023, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): seated as the pure-data FTS/search AST behind the crate wall.
 * Phrase / proximity / scoring nodes are ordinary program-AST data here;
 * posting walks, `eval_near`, and score materialization live in kyzo-core
 * (`project/text/fts.rs`). Analyzer-coupled tokenize is an engine extension
 * over these types (`project/text/ast.rs`) — kyzo-model never depends on
 * the engine.
 */

//! Pure-data FTS/search AST seat: what an FTS search string *means* once
//! parsed — phrase literals, proximity (`Near`), boolean structure, and
//! score boosters as ordinary AST data.
//!
//! # Crate wall (load-bearing)
//!
//! This module holds **data and total AST rewrites only**
//! ([`FtsExpr::flatten`], [`FtsExpr::is_empty`], mint doors). It must not
//! grow evaluation: no posting-list walks, no proximity matching, no
//! score aggregation, no analyzer coupling. Those verbs stay engine-side
//! in kyzo-core (`eval_near` / `eval_ast` in `project/text/fts.rs`;
//! tokenize in `project/text/ast.rs`). Crossing that wall here would
//! pull storage and analyzer truth into the model crate.
//!
//! # Depth invariant (load-bearing)
//!
//! The FTS query parser is the only non-test constructor, and it bounds
//! construction: group and `NOT` depth are counted against a nesting
//! ceiling, the total operator count against an ops ceiling, and
//! `AND`/`OR` chains are built as flat vectors — so **no `FtsExpr` deeper
//! than that nesting ceiling plus a small constant ever exists**. Every
//! recursive walk here (`flatten`, `is_empty`) and the compiler-generated
//! recursive `Drop`/`Clone`/`PartialEq`/`Hash` rely on that bound for
//! stack safety; they are recursive *because* the bound holds. (Bounding
//! at the parser is strictly stronger than rewriting `flatten`
//! iteratively: an iterative `flatten` would still leave the derived
//! `Drop` and friends recursing over an unbounded tree.) A new
//! constructor must either enforce the same bound or make every walk,
//! including `Drop`, iterative.

use std::hash::{Hash, Hasher};
use std::ops::Index;
use std::sync::OnceLock;

use miette::{Diagnostic, Result};
use pest::Parser;
use pest::error::InputLocation;
use pest::pratt_parser::{Assoc, Op, PrattParser};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::program::span::SourceSpan;

use super::expr::parse_string;
use super::{
    EmptyParseRoot, ExtractSpan, IntoChildren, KyzoScriptParser, Pair, ParseError, Rule, unexpected,
};

/// Score booster for one FTS literal (`^n`). Bit-identity Eq/Hash so NaN
/// boosters are representable without a float-order crate at the model wall.
#[derive(Debug, Clone, Copy)]
pub struct FtsBooster(pub f64);

impl PartialEq for FtsBooster {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}

impl Eq for FtsBooster {}

impl Hash for FtsBooster {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.to_bits().hash(state);
    }
}

impl From<f64> for FtsBooster {
    fn from(v: f64) -> Self {
        FtsBooster(v)
    }
}

impl From<f32> for FtsBooster {
    fn from(v: f32) -> Self {
        FtsBooster(f64::from(v))
    }
}

/// One search term: the text, whether it is a prefix search, and its
/// score booster.
///
/// Mint only through [`Self::new`]. Empty text is the canonical empty
/// node (`is_prefix == false`, `booster == 0`); searchable terms require
/// non-empty text and a finite positive booster.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FtsLiteral {
    value: SmartString<LazyCompact>,
    is_prefix: bool,
    booster: FtsBooster,
}

impl FtsLiteral {
    /// Sole mint. Empty value is only the canonical empty node; non-empty
    /// values require a finite positive booster.
    pub fn new(
        value: SmartString<LazyCompact>,
        is_prefix: bool,
        booster: impl Into<FtsBooster>,
    ) -> Option<Self> {
        let booster = booster.into();
        if value.is_empty() {
            if is_prefix || booster != FtsBooster(0.0) {
                return None;
            }
        } else if !(booster.0.is_finite() && booster.0 > 0.0) {
            return None;
        }
        Some(FtsLiteral {
            value,
            is_prefix,
            booster,
        })
    }

    /// Infallible mint for the canonical empty literal (empty text, non-prefix,
    /// zero booster). The sole construction path for [`FtsExpr::empty_node`].
    pub fn canonical_empty() -> Self {
        FtsLiteral {
            value: SmartString::new(),
            is_prefix: false,
            booster: FtsBooster(0.0),
        }
    }

    /// Non-empty term with unit booster — the common searchable literal.
    /// Refuses empty text (use [`canonical_empty`]).
    pub fn term(value: SmartString<LazyCompact>) -> Option<Self> {
        Self::new(value, false, 1.0)
    }

    pub fn value(&self) -> &str {
        self.value.as_str()
    }

    pub fn is_prefix(&self) -> bool {
        self.is_prefix
    }

    pub fn booster(&self) -> FtsBooster {
        self.booster
    }
}

/// Non-empty Near literals. Empty proximity is unrepresentable through
/// [`Self::admit`] / [`FtsExpr::near`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NonEmptyFtsLiterals {
    literals: Vec<FtsLiteral>,
}

impl NonEmptyFtsLiterals {
    pub fn admit(literals: Vec<FtsLiteral>) -> Option<Self> {
        if literals.is_empty() {
            None
        } else {
            Some(Self { literals })
        }
    }

    pub fn as_slice(&self) -> &[FtsLiteral] {
        &self.literals
    }

    pub fn len(&self) -> usize {
        self.literals.len()
    }

    pub fn into_vec(self) -> Vec<FtsLiteral> {
        self.literals
    }
}

/// A proximity group: literals that must occur within `distance` tokens.
/// Literals are non-empty by construction.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FtsNear {
    pub literals: NonEmptyFtsLiterals,
    pub distance: u32,
}

/// Non-empty And/Or children. Empty conjunction/disjunction is
/// unrepresentable through [`Self::admit`] / [`FtsExpr::and`] /
/// [`FtsExpr::or`]; [`FtsExpr::flatten`] never emits empty And/Or either
/// (collapses to [`FtsExpr::empty_node`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NonEmptyFtsExprs {
    children: Vec<FtsExpr>,
}

impl NonEmptyFtsExprs {
    pub fn admit(children: Vec<FtsExpr>) -> Option<Self> {
        if children.is_empty() {
            None
        } else {
            Some(Self { children })
        }
    }

    pub fn into_vec(self) -> Vec<FtsExpr> {
        self.children
    }

    pub fn as_slice(&self) -> &[FtsExpr] {
        &self.children
    }

    pub fn len(&self) -> usize {
        self.children.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &FtsExpr> {
        self.children.iter()
    }

    pub fn push(&mut self, e: FtsExpr) {
        self.children.push(e);
    }
}

impl Index<usize> for NonEmptyFtsExprs {
    type Output = FtsExpr;

    fn index(&self, index: usize) -> &Self::Output {
        &self.children[index]
    }
}

/// A parsed FTS query. See the module-level depth invariant.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FtsExpr {
    Literal(FtsLiteral),
    Near(FtsNear),
    And(NonEmptyFtsExprs),
    Or(NonEmptyFtsExprs),
    Not(Box<FtsExpr>, Box<FtsExpr>),
}

impl FtsExpr {
    /// Canonical empty node (empty literal). Used when And/Or would otherwise
    /// be empty after flatten/tokenize — empty And/Or is refused.
    pub fn empty_node() -> Self {
        FtsExpr::Literal(FtsLiteral::canonical_empty())
    }

    /// Conjunction door: refuses empty children (returns [`Self::empty_node`]).
    pub fn and(children: Vec<FtsExpr>) -> Self {
        match NonEmptyFtsExprs::admit(children) {
            Some(n) => FtsExpr::And(n),
            None => {
                // Empty children refuse — published empty FTS node.
                let empty_and = Self::empty_node();
                empty_and
            }
        }
    }

    /// Disjunction door: refuses empty children (returns [`Self::empty_node`]).
    pub fn or(children: Vec<FtsExpr>) -> Self {
        match NonEmptyFtsExprs::admit(children) {
            Some(n) => FtsExpr::Or(n),
            None => {
                let empty_or = Self::empty_node();
                empty_or
            }
        }
    }

    /// Proximity door: refuses empty literals (returns [`Self::empty_node`]).
    pub fn near(literals: Vec<FtsLiteral>, distance: u32) -> Self {
        match NonEmptyFtsLiterals::admit(literals) {
            Some(l) => FtsExpr::Near(FtsNear {
                literals: l,
                distance,
            }),
            None => {
                let empty_near = Self::empty_node();
                empty_near
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            FtsExpr::Literal(l) => l.value().is_empty(),
            FtsExpr::Near(_) => false,
            // NonEmptyFtsExprs is never empty by construction.
            FtsExpr::And(_) | FtsExpr::Or(_) => false,
            FtsExpr::Not(lhs, _) => lhs.is_empty(),
        }
    }

    /// Collapse nested conjunctions/disjunctions and drop empty subtrees.
    /// Never emits empty And/Or — all-empty collapses to [`Self::empty_node`].
    pub fn flatten(self) -> Self {
        match self {
            FtsExpr::And(exprs) => {
                let mut flattened = vec![];
                for e in exprs.into_vec() {
                    match e.flatten() {
                        FtsExpr::And(es) => flattened.extend(es.into_vec()),
                        e @ FtsExpr::Literal(_)
                        | e @ FtsExpr::Near(_)
                        | e @ FtsExpr::Or(_)
                        | e @ FtsExpr::Not(..) => {
                            if !e.is_empty() {
                                flattened.push(e)
                            }
                        }
                    }
                }
                match flattened.len() {
                    0 => Self::empty_node(),
                    1 => flattened.remove(0),
                    _other => FtsExpr::and(flattened),
                }
            }
            FtsExpr::Or(exprs) => {
                let mut flattened = vec![];
                for e in exprs.into_vec() {
                    match e.flatten() {
                        FtsExpr::Or(es) => flattened.extend(es.into_vec()),
                        e @ FtsExpr::Literal(_)
                        | e @ FtsExpr::Near(_)
                        | e @ FtsExpr::And(_)
                        | e @ FtsExpr::Not(..) => {
                            if !e.is_empty() {
                                flattened.push(e)
                            }
                        }
                    }
                }
                match flattened.len() {
                    0 => Self::empty_node(),
                    1 => flattened.remove(0),
                    _other => FtsExpr::or(flattened),
                }
            }
            FtsExpr::Not(lhs, rhs) => {
                let lhs = lhs.flatten();
                let rhs = rhs.flatten();
                if rhs.is_empty() {
                    lhs
                } else {
                    FtsExpr::Not(Box::new(lhs), Box::new(rhs))
                }
            }
            FtsExpr::Literal(l) => FtsExpr::Literal(l),
            FtsExpr::Near(n) => FtsExpr::Near(n),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// The FTS query parser (pure-data-in / AST-out door).
//
// Parallel to `parse_expressions` / `parse_type`: a *value* (the query
// string handed to an FTS search) becomes an [`FtsExpr`] AST. Distinct from
// KyzoScript proper — this grammar (`fts_doc`) applies to a search string at
// runtime, not to script text — but the same totality law holds: no query
// string may panic the parser, and no query shape may exhaust the stack.
// Group / `NOT` depth is counted against [`FTS_NESTING_CEILING`] and the
// total operator count against [`FTS_OPS_CEILING`]; together with the flat
// `And`/`Or` construction in `build_infix`, that is what guarantees the
// depth invariant documented on [`FtsExpr`].
// ─────────────────────────────────────────────────────────────────────────

/// Ceiling on `AND`/`OR`/`NOT`/group *depth* in one FTS query — the FTS
/// counterpart of the expression builder's nesting ceiling. A recursive
/// parse deeper than this is a native stack overflow (an uncatchable
/// process abort), so the language itself has a nesting ceiling. 64 is deep
/// enough for any legitimate search and low enough that the recursive walk
/// (and the derived recursive `Drop`) can never exhaust a default stack.
pub(crate) const FTS_NESTING_CEILING: usize = 64;

/// Ceiling on the total `AND`/`OR`/`NOT` operator *count* in one FTS query.
///
/// Depth is bounded separately ([`FTS_NESTING_CEILING`]); this bounds
/// *breadth*. A bracket-free `w AND w AND …` chain nests nothing, yet each
/// operator costs parse work, tree size, and downstream search work. 1024
/// is comfortably above every real query — and since `build_infix` builds
/// chains flat, the bound is a refusal of absurd inputs, not the only thing
/// standing between us and an abort.
pub(crate) const FTS_OPS_CEILING: usize = 1024;

/// An FTS query nesting deeper than [`FTS_NESTING_CEILING`]. Law-5
/// enforcement (refusal over resource death): raised by the depth counter
/// threaded through `parse_fts_expr` before the offending level recurses.
#[derive(Debug, Error, Diagnostic)]
#[error("FTS query nests {depth} levels deep, over the ceiling of {ceiling}")]
#[diagnostic(code(parser::fts_nesting_too_deep))]
#[diagnostic(help(
    "The nesting ceiling is a limit of the FTS query language, deep enough \
     for any legitimate search and low enough that a recursive parse can \
     never exhaust the stack. Flatten the query."
))]
pub(crate) struct FtsNestingTooDeep {
    depth: usize,
    ceiling: usize,
    #[label("nesting passes the ceiling here")]
    span: SourceSpan,
}

/// An FTS query with more operators than [`FTS_OPS_CEILING`] allows. The
/// breadth counterpart of [`FtsNestingTooDeep`]; raised by the flat-chain
/// operator count in `parse_fts_expr`, before any tree is built.
#[derive(Debug, Error, Diagnostic)]
#[error("FTS query has more than {ceiling} operators")]
#[diagnostic(code(parser::fts_too_many_ops))]
#[diagnostic(help(
    "The operator ceiling is a limit of the FTS query language, deep enough \
     for any legitimate search. Split the query, or drop the explicit \
     operators: a plain sequence of terms is already a conjunction."
))]
pub(crate) struct FtsTooManyOps {
    ceiling: usize,
    #[label("this operator passes the ceiling")]
    span: SourceSpan,
}

/// A numeric literal in an FTS query — a `NEAR` distance or a `^` booster —
/// that does not fit its target type, spanned at the offending digits.
#[derive(Debug, Error, Diagnostic)]
#[error("Invalid number in FTS query: {0}")]
#[diagnostic(code(parser::fts_bad_number))]
#[diagnostic(help(
    "An FTS `NEAR` distance or `^` booster must be a number that fits its \
     target type."
))]
struct BadFtsNumber(String, #[label("not a valid number here")] SourceSpan);

/// A malformed FTS literal reached the mint: empty text carrying a prefix or
/// booster, or a searchable term without a positive finite booster.
#[derive(Debug, Error, Diagnostic)]
#[error(
    "invalid FTS literal: empty text must not carry a prefix or booster, and \
     searchable terms require a positive finite booster"
)]
#[diagnostic(code(parser::fts_bad_literal))]
struct BadFtsLiteral(#[label] SourceSpan);

/// Parse an FTS search string into its [`FtsExpr`] AST. Pure-data-in /
/// AST-out, mirroring [`super::parse_expressions`]; the sole non-test
/// constructor of [`FtsExpr`], and the one that enforces the depth
/// invariant every recursive walk over the AST relies on.
pub fn parse_fts_query(q: &str) -> Result<FtsExpr> {
    // A grammar mismatch is the user's error in the FTS query text: label it
    // at pest's reported location (the same conversion the script door uses).
    let parsed = KyzoScriptParser::parse(Rule::fts_doc, q)
        .map_err(|err| {
            let span = match err.location {
                InputLocation::Pos(p) => SourceSpan(p, 0),
                InputLocation::Span((start, end)) => SourceSpan(start, end - start),
            };
            ParseError { span }
        })?
        .next()
        .ok_or_else(|| EmptyParseRoot {
            expected: "an fts_doc root",
        })?;
    // One operator budget for the whole query, threaded through every level.
    let mut ops_left = FTS_OPS_CEILING;
    let pairs = parsed
        .children()
        .filter(|r| r.as_rule() != Rule::EOI)
        .map(|p| parse_fts_expr(p, 0, &mut ops_left))
        .collect::<Result<Vec<_>>>()?;
    Ok(if pairs.len() == 1 {
        let mut pairs = pairs;
        // In bounds: length checked to be exactly 1.
        pairs.remove(0)
    } else {
        FtsExpr::and(pairs)
    })
}

/// Parse one `fts_expr` sitting `depth` levels deep, spending operators from
/// `ops_left`. The guards run on the *flat* child list before the recursive
/// work: `NOT` counts against the nesting ceiling (each `Not` boxes its
/// operands and adds a tree level); `AND`/`OR` count only against the
/// operator budget (`build_infix` extends their vectors in place — breadth,
/// not depth).
fn parse_fts_expr(pair: Pair<'_>, depth: usize, ops_left: &mut usize) -> Result<FtsExpr> {
    if pair.as_rule() != Rule::fts_expr {
        return Err(unexpected("an fts_expr", &pair));
    }

    let mut weight = depth + 1;
    if weight > FTS_NESTING_CEILING {
        return Err(FtsNestingTooDeep {
            depth: weight,
            ceiling: FTS_NESTING_CEILING,
            span: pair.extract_span(),
        }
        .into());
    }
    for child in pair.clone().children() {
        match child.as_rule() {
            Rule::fts_and | Rule::fts_or | Rule::fts_not => {
                if *ops_left == 0 {
                    return Err(FtsTooManyOps {
                        ceiling: FTS_OPS_CEILING,
                        span: child.extract_span(),
                    }
                    .into());
                }
                *ops_left -= 1;
                if child.as_rule() == Rule::fts_not {
                    weight += 1;
                    if weight > FTS_NESTING_CEILING {
                        return Err(FtsNestingTooDeep {
                            depth: weight,
                            ceiling: FTS_NESTING_CEILING,
                            span: child.extract_span(),
                        }
                        .into());
                    }
                }
            }
            _other => {}
        }
    }

    fts_pratt()
        .map_primary(|p| build_term(p, weight, ops_left))
        .map_infix(build_infix)
        .parse(pair.children())
}

fn build_infix(lhs: Result<FtsExpr>, op: Pair<'_>, rhs: Result<FtsExpr>) -> Result<FtsExpr> {
    let lhs = lhs?;
    let rhs = rhs?;
    // `And`/`Or` extend an existing vector instead of nesting a fresh
    // two-element node around it: the Pratt build is left-associative, so a
    // flat `w AND w AND …` chain arrives with `lhs` already the accumulated
    // `And`, and pushing keeps the tree one level deep. Semantically
    // identical to `flatten`'s collapse; this just never builds the deep form.
    Ok(match op.as_rule() {
        Rule::fts_and => match lhs {
            FtsExpr::And(mut es) => {
                es.push(rhs);
                FtsExpr::And(es)
            }
            lhs @ (FtsExpr::Literal(_) | FtsExpr::Near(_) | FtsExpr::Or(_) | FtsExpr::Not(..)) => {
                FtsExpr::and(vec![lhs, rhs])
            }
        },
        Rule::fts_or => match lhs {
            FtsExpr::Or(mut es) => {
                es.push(rhs);
                FtsExpr::Or(es)
            }
            lhs @ (FtsExpr::Literal(_) | FtsExpr::Near(_) | FtsExpr::And(_) | FtsExpr::Not(..)) => {
                FtsExpr::or(vec![lhs, rhs])
            }
        },
        Rule::fts_not => FtsExpr::Not(Box::new(lhs), Box::new(rhs)),
        _other => return Err(unexpected("an FTS operator", &op)),
    })
}

fn build_term(pair: Pair<'_>, depth: usize, ops_left: &mut usize) -> Result<FtsExpr> {
    Ok(match pair.as_rule() {
        Rule::fts_grouped => {
            let collected = pair
                .children()
                .map(|p| parse_fts_expr(p, depth, &mut *ops_left))
                .collect::<Result<Vec<_>>>()?;
            if collected.len() == 1 {
                let mut collected = collected;
                // In bounds: length checked to be exactly 1.
                collected.remove(0)
            } else {
                FtsExpr::and(collected)
            }
        }
        Rule::fts_near => {
            let mut literals = vec![];
            let mut distance = 10;
            for pair in pair.children() {
                match pair.as_rule() {
                    Rule::pos_int => {
                        let span = pair.extract_span();
                        let i = pair
                            .as_str()
                            .replace('_', "")
                            .parse::<i64>()
                            .map_err(|_| BadFtsNumber(pair.as_str().to_string(), span))?;
                        distance = u32::try_from(i)
                            .map_err(|_| BadFtsNumber(pair.as_str().to_string(), span))?;
                    }
                    _other => literals.push(build_phrase(pair)?),
                }
            }
            FtsExpr::near(literals, distance)
        }
        Rule::fts_phrase => FtsExpr::Literal(build_phrase(pair)?),
        _other => return Err(unexpected("an FTS term", &pair)),
    })
}

fn build_phrase(pair: Pair<'_>) -> Result<FtsLiteral> {
    let span = pair.extract_span();
    let mut inner = pair.children();
    let kernel = inner.need("the phrase kernel")?;
    let core_text = match kernel.as_rule() {
        Rule::fts_phrase_group => SmartString::from(kernel.as_str().trim()),
        Rule::quoted_string | Rule::s_quoted_string | Rule::raw_string => parse_string(kernel)?,
        _other => return Err(unexpected("a phrase kernel", &kernel)),
    };
    let mut is_prefix = false;
    let mut booster = 1.0;
    for pair in inner {
        match pair.as_rule() {
            Rule::fts_prefix_marker => is_prefix = true,
            Rule::fts_booster => {
                let boosted = pair.children().need("the booster value")?;
                match boosted.as_rule() {
                    Rule::dot_float => {
                        let span = boosted.extract_span();
                        booster = boosted
                            .as_str()
                            .replace('_', "")
                            .parse::<f64>()
                            .map_err(|_| BadFtsNumber(boosted.as_str().to_string(), span))?;
                    }
                    // An integer booster (`word^22`) is valid syntax.
                    Rule::pos_int => {
                        let span = boosted.extract_span();
                        let i = boosted
                            .as_str()
                            .replace('_', "")
                            .parse::<i64>()
                            .map_err(|_| BadFtsNumber(boosted.as_str().to_string(), span))?;
                        booster = f64::from(
                            i32::try_from(i).map_err(|_| {
                                BadFtsNumber(boosted.as_str().to_string(), span)
                            })?,
                        );
                    }
                    _other => return Err(unexpected("a booster value", &boosted)),
                }
            }
            _other => return Err(unexpected("a phrase modifier", &pair)),
        }
    }
    FtsLiteral::new(core_text, is_prefix, booster).ok_or_else(|| BadFtsLiteral(span).into())
}

/// The FTS boolean precedence: `NOT` binds tightest, then `AND`, then `OR`,
/// all left-associative — the same ladder the CozoDB original used.
fn fts_pratt() -> &'static PrattParser<Rule> {
    static PARSER: OnceLock<PrattParser<Rule>> = OnceLock::new();
    PARSER.get_or_init(|| {
        PrattParser::new()
            .op(Op::infix(Rule::fts_not, Assoc::Left))
            .op(Op::infix(Rule::fts_and, Assoc::Left))
            .op(Op::infix(Rule::fts_or, Assoc::Left))
    })
}

#[cfg(test)]
mod tests {
    use miette::{Result, bail, ensure};

    use super::*;

    fn lit(s: &str) -> Result<FtsExpr> {
        if s.is_empty() {
            return Ok(FtsExpr::empty_node());
        }
        let literal = FtsLiteral::new(s.into(), false, 1.0).ok_or_else(|| {
            miette::miette!("test lit mint refused for non-empty term with booster 1.0")
        })?;
        Ok(FtsExpr::Literal(literal))
    }

    #[test]
    fn is_empty_edge_cases() -> Result<()> {
        assert!(lit("")?.is_empty());
        assert!(FtsLiteral::new("hello".into(), false, 0.0).is_none());
        assert!(FtsExpr::empty_node().is_empty());
        assert!(NonEmptyFtsExprs::admit(vec![]).is_none());
        assert!(FtsExpr::and(vec![]).is_empty());
        assert!(FtsExpr::or(vec![]).is_empty());
        assert!(NonEmptyFtsLiterals::admit(vec![]).is_none());
        assert!(FtsExpr::near(vec![], 10).is_empty());
        assert!(FtsExpr::Not(Box::new(lit("")?), Box::new(lit("x")?)).is_empty());
        assert!(!FtsExpr::Not(Box::new(lit("x")?), Box::new(lit("")?)).is_empty());
        let shallow = FtsExpr::and(vec![lit("")?]);
        assert!(!shallow.is_empty());
        assert!(shallow.flatten().is_empty());
        Ok(())
    }

    #[test]
    fn flatten_collapses_nesting_and_drops_empties() -> Result<()> {
        let e = FtsExpr::and(vec![FtsExpr::and(vec![lit("a")?, lit("b")?]), lit("c")?]);
        match e.flatten() {
            FtsExpr::And(v) => assert_eq!(v.len(), 3),
            other @ FtsExpr::Literal(_)
            | other @ FtsExpr::Near(_)
            | other @ FtsExpr::Or(_)
            | other @ FtsExpr::Not(..) => bail!("expected And, got {other:?}"),
        }
        let e = FtsExpr::or(vec![
            FtsExpr::or(vec![lit("a")?, lit("b")?]),
            FtsExpr::or(vec![lit("c")?, lit("d")?]),
        ]);
        match e.flatten() {
            FtsExpr::Or(v) => assert_eq!(v.len(), 4),
            other @ FtsExpr::Literal(_)
            | other @ FtsExpr::Near(_)
            | other @ FtsExpr::And(_)
            | other @ FtsExpr::Not(..) => bail!("expected Or, got {other:?}"),
        }
        let e = FtsExpr::and(vec![lit("a")?, lit("")?]);
        assert_eq!(e.flatten(), lit("a")?);
        let e = FtsExpr::or(vec![lit("")?, lit("")?]);
        let flat = e.flatten();
        assert!(flat.is_empty());
        assert!(matches!(flat, FtsExpr::Literal(_)));
        let e = FtsExpr::Not(Box::new(lit("keep")?), Box::new(lit("")?));
        assert_eq!(e.flatten(), lit("keep")?);
        let e = FtsExpr::and(vec![FtsExpr::and(vec![FtsExpr::and(vec![FtsExpr::or(
            vec![lit("x")?],
        )])])]);
        assert_eq!(e.flatten(), lit("x")?);
        Ok(())
    }

    /// Drive the nesting ceiling through the sole non-test constructor
    /// ([`parse_fts_query`]), not a hand-built tree — the parser is what
    /// the depth invariant on [`FtsExpr`] relies on.
    #[test]
    fn parse_fts_query_refuses_nesting_over_ceiling() -> Result<()> {
        // Nested `w NOT (…)`: each NOT raises the weight passed into the
        // grouped child (see `parse_fts_expr` / `build_term`).
        let mut q = String::from("w");
        for _ in 0..FTS_NESTING_CEILING {
            q = format!("w NOT ({q})");
        }
        let Err(err) = parse_fts_query(&q) else {
            bail!("must refuse over nesting ceiling");
        };
        ensure!(
            err.downcast_ref::<FtsNestingTooDeep>().is_some(),
            "expected FtsNestingTooDeep, got {err:?}"
        );
        // Weight grows by ~2 per nested NOT; 16 NOTs stays well under the ceiling.
        let mut ok = String::from("w");
        for _ in 0..16 {
            ok = format!("w NOT ({ok})");
        }
        ensure!(
            parse_fts_query(&ok).is_ok(),
            "16 nested NOTs must still admit"
        );
        Ok(())
    }

    #[test]
    fn parse_fts_query_refuses_ops_over_ceiling() -> Result<()> {
        let terms: Vec<&str> = std::iter::repeat("w").take(FTS_OPS_CEILING + 2).collect();
        let q = terms.join(" AND ");
        let Err(err) = parse_fts_query(&q) else {
            bail!("must refuse over ops ceiling");
        };
        ensure!(
            err.downcast_ref::<FtsTooManyOps>().is_some(),
            "expected FtsTooManyOps, got {err:?}"
        );
        let ok_terms: Vec<&str> = std::iter::repeat("w").take(8).collect();
        ensure!(
            parse_fts_query(&ok_terms.join(" AND ")).is_ok(),
            "small AND query must admit"
        );
        Ok(())
    }
}
