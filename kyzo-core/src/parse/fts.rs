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
            _ => {}
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
            lhs => FtsExpr::And(vec![lhs, rhs]),
        },
        Rule::fts_or => match lhs {
            FtsExpr::Or(mut es) => {
                es.push(rhs);
                FtsExpr::Or(es)
            }
            lhs => FtsExpr::Or(vec![lhs, rhs]),
        },
        Rule::fts_not => FtsExpr::Not(Box::new(lhs), Box::new(rhs)),
        _ => return Err(unexpected("an FTS operator", &op)),
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
                    _ => literals.push(build_phrase(pair)?),
                }
            }
            FtsExpr::Near(FtsNear { literals, distance })
        }
        Rule::fts_phrase => FtsExpr::Literal(build_phrase(pair)?),
        _ => return Err(unexpected("an FTS term", &pair)),
    })
}

fn build_phrase(pair: Pair<'_>) -> Result<FtsLiteral> {
    let mut inner = pair.children();
    let kernel = inner.expect("the phrase kernel")?;
    let core_text = match kernel.as_rule() {
        Rule::fts_phrase_group => SmartString::from(kernel.as_str().trim()),
        Rule::quoted_string | Rule::s_quoted_string | Rule::raw_string => parse_string(kernel)?,
        _ => return Err(unexpected("a phrase kernel", &kernel)),
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
                    _ => return Err(unexpected("a booster value", &boosted)),
                }
            }
            _ => return Err(unexpected("a phrase modifier", &pair)),
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
            r => panic!("expected a literal, got {r:?}"),
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
            other => panic!("expected a flat And, got {other:?}"),
        }
        let src = vec!["w"; 101].join(" OR ");
        match parse_fts_query(&src).unwrap() {
            FtsExpr::Or(v) => assert_eq!(v.len(), 101),
            other => panic!("expected a flat Or, got {other:?}"),
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
            other => panic!("expected And, got {other:?}"),
        }
        // NOT still nests: `a NOT b AND c` keeps its binary shape.
        let res = parse_fts_query("a NOT b AND c").unwrap().flatten();
        assert!(matches!(res, FtsExpr::Not(_, _)));
    }
}
