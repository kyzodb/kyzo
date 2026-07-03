/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): `expr2bytecode` is relocated to `data/expr.rs` (compiling an
 * expression is the expression's own domain) — this file is only the Pratt
 * builder; radix integer literals (`0x`/`0o`/`0b`) beyond `i64` are the
 * same `BadIntError` the decimal path raises instead of a panic; the
 * grammar-shape `unwrap`s and `unreachable!` dispatch arms go through the
 * typed-accessor layer in `parse/mod.rs`; the Pratt table is a
 * `std::sync::LazyLock` (no `lazy_static` dependency).
 */

//! Building [`Expr`]s from parsed text: the Pratt (operator-precedence)
//! builder and the literal parsers.
//!
//! The proofs established here, at construction: every `$param` resolved
//! against the parameter pool; every named function resolved (or was
//! deliberately kept as `UnboundApply` for later resolution); every
//! application satisfies its op's declared arity; every literal parsed
//! within its type's range. No user-supplied literal can panic the parser —
//! malformed numbers, escapes and codepoints are all spanned errors — and
//! no expression shape can overflow the stack: the builder counts its own
//! recursion depth against the same nesting ceiling the pre-parse scan
//! enforces ([`NestingTooDeep`]).

use std::collections::BTreeMap;
use std::sync::LazyLock;

use itertools::Itertools;
use miette::{Diagnostic, Result, bail, ensure};
use pest::pratt_parser::{Op, PrattParser};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::expr::{Expr, get_op};
use crate::data::functions::{
    OP_ADD, OP_AND, OP_COALESCE, OP_CONCAT, OP_DIV, OP_EQ, OP_GE, OP_GT, OP_JSON_OBJECT, OP_LE,
    OP_LIST, OP_LT, OP_MAYBE_GET, OP_MINUS, OP_MOD, OP_MUL, OP_NEGATE, OP_NEQ, OP_OR, OP_POW,
    OP_SUB,
};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::parse::{
    ExtractSpan, IntoChildren, NESTING_CEILING, NestingTooDeep, Pair, Rule, strip_sigil, unexpected,
};

static PRATT_PARSER: LazyLock<PrattParser<Rule>> = LazyLock::new(|| {
    use pest::pratt_parser::Assoc::*;

    PrattParser::new()
        .op(Op::infix(Rule::op_or, Left))
        .op(Op::infix(Rule::op_and, Left))
        .op(Op::infix(Rule::op_gt, Left)
            | Op::infix(Rule::op_lt, Left)
            | Op::infix(Rule::op_ge, Left)
            | Op::infix(Rule::op_le, Left))
        .op(Op::infix(Rule::op_eq, Left) | Op::infix(Rule::op_ne, Left))
        .op(Op::infix(Rule::op_mod, Left))
        .op(Op::infix(Rule::op_add, Left)
            | Op::infix(Rule::op_sub, Left)
            | Op::infix(Rule::op_concat, Left))
        .op(Op::infix(Rule::op_mul, Left) | Op::infix(Rule::op_div, Left))
        .op(Op::infix(Rule::op_pow, Right))
        .op(Op::infix(Rule::op_coalesce, Left))
        .op(Op::prefix(Rule::minus))
        .op(Op::prefix(Rule::negate))
        .op(Op::infix(Rule::op_field_access, Left))
});

#[derive(Debug, Error, Diagnostic)]
#[error("Invalid expression encountered")]
#[diagnostic(code(parser::invalid_expression))]
pub(crate) struct InvalidExpression(#[label] pub(crate) SourceSpan);

/// An integer literal that does not fit in `i64`. One error for every
/// radix: the CozoDB original raised it only on the decimal path and
/// *panicked* on `0x`/`0o`/`0b` overflow (`parse/expr.rs:427`).
#[derive(Error, Diagnostic, Debug)]
#[error("Cannot parse integer")]
#[diagnostic(code(parser::bad_pos_int))]
struct BadIntError(#[label] SourceSpan);

/// Is this pair one of `expr`'s operator children? Each one costs a level
/// of Pratt recursion and a level of the built [`Expr`] tree (prefix
/// chains and right-associative `^` stack the parser; every operator
/// stacks the tree that later evaluation and `Drop` recurse over).
fn is_operator(candidate: Rule) -> bool {
    matches!(
        candidate,
        Rule::minus
            | Rule::negate
            | Rule::op_or
            | Rule::op_and
            | Rule::op_concat
            | Rule::op_add
            | Rule::op_field_access
            | Rule::op_sub
            | Rule::op_mul
            | Rule::op_div
            | Rule::op_mod
            | Rule::op_eq
            | Rule::op_ne
            | Rule::op_gt
            | Rule::op_lt
            | Rule::op_ge
            | Rule::op_le
            | Rule::op_pow
            | Rule::op_coalesce
    )
}

pub(crate) fn build_expr(pair: Pair<'_>, param_pool: &BTreeMap<String, DataValue>) -> Result<Expr> {
    build_expr_bounded(pair, param_pool, 0)
}

/// [`build_expr`] with the depth this expression already sits at. Belt and
/// suspenders around the same [`NESTING_CEILING`] as the pre-parse scan
/// (law 5: refusal over resource death): the scan bounds what pest
/// recurses over (brackets, `not` chains), while this counter bounds the
/// shapes only the Pratt builder recurses over — bracketless operator
/// chains such as `----1` (pest matches `unary_op*` iteratively; *this*
/// tier would blow the stack building, and later dropping and evaluating,
/// the tree). The check runs before the recursive work, on `expr`'s flat
/// child list.
fn build_expr_bounded(
    pair: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    depth: usize,
) -> Result<Expr> {
    ensure!(
        pair.as_rule() == Rule::expr,
        InvalidExpression(pair.extract_span())
    );

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
        if is_operator(child.as_rule()) {
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

    PRATT_PARSER
        .map_primary(|v| build_term(v, param_pool, weight))
        .map_infix(build_expr_infix)
        .map_prefix(|op, rhs| {
            let rhs = rhs?;
            let rhs_span = rhs.span();
            Ok(match op.as_rule() {
                Rule::minus => Expr::Apply {
                    op: &OP_MINUS,
                    args: [rhs].into(),
                    span: op.extract_span().merge(rhs_span),
                },
                Rule::negate => Expr::Apply {
                    op: &OP_NEGATE,
                    args: [rhs].into(),
                    span: op.extract_span().merge(rhs_span),
                },
                _ => return Err(unexpected("a prefix operator", &op)),
            })
        })
        .parse(pair.into_inner())
}

fn build_expr_infix(lhs: Result<Expr>, op: Pair<'_>, rhs: Result<Expr>) -> Result<Expr> {
    let args = vec![lhs?, rhs?];
    let op = match op.as_rule() {
        Rule::op_add => &OP_ADD,
        Rule::op_sub => &OP_SUB,
        Rule::op_mul => &OP_MUL,
        Rule::op_div => &OP_DIV,
        Rule::op_mod => &OP_MOD,
        Rule::op_pow => &OP_POW,
        Rule::op_eq => &OP_EQ,
        Rule::op_ne => &OP_NEQ,
        Rule::op_gt => &OP_GT,
        Rule::op_ge => &OP_GE,
        Rule::op_lt => &OP_LT,
        Rule::op_le => &OP_LE,
        Rule::op_concat => &OP_CONCAT,
        Rule::op_or => &OP_OR,
        Rule::op_and => &OP_AND,
        Rule::op_coalesce => &OP_COALESCE,
        Rule::op_field_access => &OP_MAYBE_GET,
        _ => return Err(unexpected("an infix operator", &op)),
    };
    let span = args[0].span().merge(args[1].span());
    Ok(Expr::Apply {
        op,
        args: args.into(),
        span,
    })
}

fn build_term(
    pair: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    depth: usize,
) -> Result<Expr> {
    let span = pair.extract_span();
    let op = pair.as_rule();
    Ok(match op {
        Rule::var => Expr::Binding {
            var: Symbol::new(pair.as_str(), pair.extract_span()),
            tuple_pos: None,
        },
        Rule::param => {
            #[derive(Error, Diagnostic, Debug)]
            #[error("Required parameter {0} not found")]
            #[diagnostic(code(parser::param_not_found))]
            struct ParamNotFoundError(String, #[label] SourceSpan);

            let param_str = strip_sigil(&pair, '$')?;
            Expr::Const {
                val: param_pool
                    .get(param_str)
                    .ok_or_else(|| ParamNotFoundError(param_str.to_string(), span))?
                    .clone(),
                span,
            }
        }
        Rule::pos_int => {
            let i = pair
                .as_str()
                .replace('_', "")
                .parse::<i64>()
                .map_err(|_| BadIntError(span))?;
            Expr::Const {
                val: DataValue::from(i),
                span,
            }
        }
        Rule::hex_pos_int => Expr::Const {
            val: DataValue::from(parse_radix_int(pair.as_str(), 16, span)?),
            span,
        },
        Rule::octo_pos_int => Expr::Const {
            val: DataValue::from(parse_radix_int(pair.as_str(), 8, span)?),
            span,
        },
        Rule::bin_pos_int => Expr::Const {
            val: DataValue::from(parse_radix_int(pair.as_str(), 2, span)?),
            span,
        },
        Rule::dot_float | Rule::sci_float => {
            #[derive(Error, Diagnostic, Debug)]
            #[error("Cannot parse float")]
            #[diagnostic(code(parser::bad_float))]
            struct BadFloatError(#[label] SourceSpan);

            let f = pair
                .as_str()
                .replace('_', "")
                .parse::<f64>()
                .map_err(|_| BadFloatError(span))?;
            Expr::Const {
                val: DataValue::from(f),
                span,
            }
        }
        Rule::null => Expr::Const {
            val: DataValue::Null,
            span,
        },
        Rule::boolean => Expr::Const {
            val: DataValue::from(pair.as_str() == "true"),
            span,
        },
        Rule::quoted_string | Rule::s_quoted_string | Rule::raw_string => {
            let s = parse_string(pair)?;
            Expr::Const {
                val: DataValue::Str(s),
                span,
            }
        }
        Rule::list => {
            let mut collected = vec![];
            for p in pair.into_inner() {
                collected.push(build_expr_bounded(p, param_pool, depth)?)
            }
            Expr::Apply {
                op: &OP_LIST,
                args: collected.into(),
                span,
            }
        }
        Rule::object => {
            let mut args = vec![];
            for p in pair.into_inner() {
                let [k, v] = p
                    .children()
                    .expect_n(["an object key", "an object value"])?;
                let k = build_expr_bounded(k, param_pool, depth)?;
                let v = build_expr_bounded(v, param_pool, depth)?;
                args.push(k);
                args.push(v);
            }
            Expr::Apply {
                op: &OP_JSON_OBJECT,
                args: args.into(),
                span,
            }
        }
        Rule::apply => {
            let mut p = pair.children();
            let ident_p = p.expect("the applied function's name")?;
            let ident = ident_p.as_str();
            let mut args: Vec<_> = p
                .expect("the argument list")?
                .into_inner()
                .map(|v| build_expr_bounded(v, param_pool, depth))
                .try_collect()?;

            match ident {
                "cond" => {
                    if args.is_empty() {
                        #[derive(Error, Diagnostic, Debug)]
                        #[error("'cond' cannot have empty body")]
                        #[diagnostic(code(parser::empty_cond))]
                        struct EmptyCond(#[label] SourceSpan);
                        bail!(EmptyCond(span));
                    }
                    if args.len() & 1 == 1 {
                        // Non-empty: bailed on `args.is_empty()` above.
                        let last_span = args.last().map(Expr::span).unwrap_or(span);
                        args.insert(
                            args.len() - 1,
                            Expr::Const {
                                val: DataValue::Null,
                                span: last_span,
                            },
                        )
                    }
                    let mut clauses = args
                        .chunks(2)
                        .map(|pair| (pair[0].clone(), pair[1].clone()))
                        .collect_vec();
                    if let Some((cond, _)) = clauses.last() {
                        match cond {
                            Expr::Const {
                                val: DataValue::Bool(true),
                                ..
                            } => {}
                            _ => {
                                clauses.push((
                                    Expr::Const {
                                        val: DataValue::from(true),
                                        span,
                                    },
                                    Expr::Const {
                                        val: DataValue::Null,
                                        span,
                                    },
                                ));
                            }
                        }
                    }
                    Expr::Cond { clauses, span }
                }
                "if" => {
                    #[derive(Debug, Error, Diagnostic)]
                    #[error("wrong number of arguments to if: 2 or 3 required")]
                    #[diagnostic(code(parser::bad_if))]
                    struct WrongArgsToIf(#[label] SourceSpan);

                    let mut args = args.into_iter();
                    // "2 or 3 arguments" is proven by the shape of the code
                    // itself: the pattern match demands the first two, the
                    // trailing `ensure!` forbids a fourth — no counting
                    // check whose proof an `unwrap` then re-asserts.
                    let (cond, then) = match (args.next(), args.next()) {
                        (Some(cond), Some(then)) => (cond, then),
                        _ => bail!(WrongArgsToIf(span)),
                    };
                    let else_clause = args.next();
                    ensure!(args.next().is_none(), WrongArgsToIf(span));
                    let clauses = vec![
                        (cond, then),
                        (
                            Expr::Const {
                                val: DataValue::from(true),
                                span,
                            },
                            else_clause.unwrap_or(Expr::Const {
                                val: DataValue::Null,
                                span,
                            }),
                        ),
                    ];
                    Expr::Cond { clauses, span }
                }
                _ => match get_op(ident) {
                    None => Expr::UnboundApply {
                        op: ident.into(),
                        args: args.into(),
                        span,
                    },
                    Some(op) => {
                        op.post_process_args(&mut args);
                        #[derive(Error, Diagnostic, Debug)]
                        #[error("Wrong number of arguments for function '{0}'")]
                        #[diagnostic(code(parser::func_wrong_num_args))]
                        struct WrongNumArgsError(String, #[label] SourceSpan, #[help] String);

                        ensure!(
                            op.arity_matches(args.len()),
                            WrongNumArgsError(
                                ident.to_string(),
                                span,
                                format!("Need {} argument(s)", op.arity_requirement())
                            )
                        );
                        Expr::Apply {
                            op,
                            args: args.into(),
                            span,
                        }
                    }
                },
            }
        }
        Rule::grouping => build_expr_bounded(
            pair.children().expect("the grouped expression")?,
            param_pool,
            depth,
        )?,
        _ => return Err(unexpected("an expression term", &pair)),
    })
}

/// Parse a radix-prefixed integer literal (`0x…`, `0o…`, `0b…`). Total over
/// user text: overflow is the same [`BadIntError`] as on the decimal path.
/// (The CozoDB original `unwrap`ped here, so `0xFFFFFFFFFFFFFFFFF` aborted
/// the process from query text.)
pub(crate) fn parse_radix_int(s: &str, radix: u32, span: SourceSpan) -> Result<i64> {
    // The grammar guarantees the two-character prefix (`0x`/`0o`/`0b`, or
    // `\u` for string escapes) before the digits.
    let digits = s.get(2..).ok_or(BadIntError(span))?;
    i64::from_str_radix(&digits.replace('_', ""), radix).map_err(|_| BadIntError(span).into())
}

pub(crate) fn parse_string(pair: Pair<'_>) -> Result<SmartString<LazyCompact>> {
    match pair.as_rule() {
        Rule::quoted_string => Ok(parse_quoted_string(pair)?),
        Rule::s_quoted_string => Ok(parse_s_quoted_string(pair)?),
        Rule::raw_string => Ok(parse_raw_string(pair)?),
        Rule::ident => Ok(SmartString::from(pair.as_str())),
        _ => Err(unexpected("a string literal", &pair)),
    }
}

#[derive(Error, Diagnostic, Debug)]
#[error("invalid UTF8 code {0}")]
#[diagnostic(code(parser::invalid_utf8_code))]
struct InvalidUtf8Error(u32, #[label] SourceSpan);

#[derive(Error, Diagnostic, Debug)]
#[error("invalid escape sequence {0}")]
#[diagnostic(code(parser::invalid_escape_seq))]
struct InvalidEscapeSeqError(String, #[label] SourceSpan);

fn parse_quoted_string(pair: Pair<'_>) -> Result<SmartString<LazyCompact>> {
    let pairs = pair
        .children()
        .expect("the quoted_string_inner body")?
        .into_inner();
    let mut ret = SmartString::new();
    for pair in pairs {
        let s = pair.as_str();
        match s {
            r#"\""# => ret.push('"'),
            r"\\" => ret.push('\\'),
            r"\/" => ret.push('/'),
            r"\b" => ret.push('\x08'),
            r"\f" => ret.push('\x0c'),
            r"\n" => ret.push('\n'),
            r"\r" => ret.push('\r'),
            r"\t" => ret.push('\t'),
            s if s.starts_with(r"\u") => {
                let code = parse_radix_int(s, 16, pair.extract_span())? as u32;
                let ch = char::from_u32(code)
                    .ok_or_else(|| InvalidUtf8Error(code, pair.extract_span()))?;
                ret.push(ch);
            }
            s if s.starts_with('\\') => {
                bail!(InvalidEscapeSeqError(s.to_string(), pair.extract_span()))
            }
            s => ret.push_str(s),
        }
    }
    Ok(ret)
}

fn parse_s_quoted_string(pair: Pair<'_>) -> Result<SmartString<LazyCompact>> {
    let pairs = pair
        .children()
        .expect("the s_quoted_string_inner body")?
        .into_inner();
    let mut ret = SmartString::new();
    for pair in pairs {
        let s = pair.as_str();
        match s {
            r"\'" => ret.push('\''),
            r"\\" => ret.push('\\'),
            r"\/" => ret.push('/'),
            r"\b" => ret.push('\x08'),
            r"\f" => ret.push('\x0c'),
            r"\n" => ret.push('\n'),
            r"\r" => ret.push('\r'),
            r"\t" => ret.push('\t'),
            s if s.starts_with(r"\u") => {
                let code = parse_radix_int(s, 16, pair.extract_span())? as u32;
                let ch = char::from_u32(code)
                    .ok_or_else(|| InvalidUtf8Error(code, pair.extract_span()))?;
                ret.push(ch);
            }
            s if s.starts_with('\\') => {
                bail!(InvalidEscapeSeqError(s.to_string(), pair.extract_span()))
            }
            s => ret.push_str(s),
        }
    }
    Ok(ret)
}

fn parse_raw_string(pair: Pair<'_>) -> Result<SmartString<LazyCompact>> {
    Ok(SmartString::from(
        pair.children()
            .expect("the raw_string_inner body")?
            .as_str(),
    ))
}

#[cfg(test)]
mod tests {
    use crate::data::value::DataValue;
    use crate::parse::parse_expressions;

    fn eval_const(src: &str) -> miette::Result<DataValue> {
        parse_expressions(src, &Default::default())?.eval_to_const()
    }

    /// Radix literals evaluate to the same values their decimal spellings
    /// would, underscores included.
    #[test]
    fn radix_literals_parse() {
        for (src, expected) in [
            ("0xff", 255i64),
            ("0o17", 15),
            ("0b1010", 10),
            ("0xdead_beef", 0xdead_beef),
            ("0x7fffffffffffffff", i64::MAX),
            ("-0x10", -16),
        ] {
            assert_eq!(
                eval_const(src).unwrap(),
                DataValue::from(expected),
                "value of {src}"
            );
        }
    }

    /// A radix literal beyond `i64` is the same `BadIntError` the decimal
    /// path raises. The CozoDB original called
    /// `i64::from_str_radix(..).unwrap()` here (`parse/expr.rs:427`) and
    /// aborted the process on `0xFFFFFFFFFFFFFFFFF` in query text.
    #[test]
    fn radix_overflow_is_an_error_not_a_panic() {
        for src in [
            "0xFFFFFFFFFFFFFFFFF",
            "0x8000000000000000",
            "0o7777777777777777777777777",
            "0b11111111111111111111111111111111111111111111111111111111111111111",
            // The decimal path, for symmetry: same error class.
            "9223372036854775808",
        ] {
            let res = parse_expressions(src, &Default::default());
            assert!(res.is_err(), "{src} must error, not panic");
        }
    }
}
