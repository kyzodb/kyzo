/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): lifts KyzoScript expression syntax into program IR —
 * `OpDecl` by value, `BindingPos::Unresolved`, `Expr::Lazy` for
 * short-circuit connectives, `resolve_decl` instead of a body table.
 */

//! Lifts KyzoScript expression syntax into program IR.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use miette::{Diagnostic, Result, bail, ensure};
use pest::pratt_parser::{Assoc, Op, PrattParser};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::program::expr::{BindingPos, Expr, LazyOp};
use crate::program::op::{
    OP_ADD, OP_CONCAT, OP_DIV, OP_EQ, OP_GE, OP_GT, OP_JSON_OBJECT, OP_LE, OP_LIST, OP_LT,
    OP_MAYBE_GET, OP_MINUS, OP_MOD, OP_MUL, OP_NEGATE, OP_NEQ, OP_POW, OP_SUB, resolve_decl,
};
use crate::program::span::SourceSpan;
use crate::program::symbol::Symbol;
use crate::value::DataValue;

use super::{ExtractSpan, Pair, Rule, UnexpectedRule};

fn pratt_parser() -> &'static PrattParser<Rule> {
    static PARSER: OnceLock<PrattParser<Rule>> = OnceLock::new();
    PARSER.get_or_init(|| {
        PrattParser::new()
            .op(Op::infix(Rule::op_or, Assoc::Left))
            .op(Op::infix(Rule::op_and, Assoc::Left))
            .op(Op::infix(Rule::op_gt, Assoc::Left)
                | Op::infix(Rule::op_lt, Assoc::Left)
                | Op::infix(Rule::op_ge, Assoc::Left)
                | Op::infix(Rule::op_le, Assoc::Left))
            .op(Op::infix(Rule::op_eq, Assoc::Left) | Op::infix(Rule::op_ne, Assoc::Left))
            .op(Op::infix(Rule::op_mod, Assoc::Left))
            .op(Op::infix(Rule::op_add, Assoc::Left)
                | Op::infix(Rule::op_sub, Assoc::Left)
                | Op::infix(Rule::op_concat, Assoc::Left))
            .op(Op::infix(Rule::op_mul, Assoc::Left) | Op::infix(Rule::op_div, Assoc::Left))
            .op(Op::infix(Rule::op_pow, Assoc::Right))
            .op(Op::infix(Rule::op_coalesce, Assoc::Left))
            .op(Op::prefix(Rule::minus))
            .op(Op::prefix(Rule::negate))
            .op(Op::infix(Rule::op_field_access, Assoc::Left))
    })
}

#[derive(Debug, Error, Diagnostic)]
#[error("Invalid expression encountered")]
#[diagnostic(code(parser::invalid_expression))]
pub(crate) struct InvalidExpression(#[label] pub(crate) SourceSpan);

/// Lift one pest `expr` pair into an [`Expr`].
pub(crate) fn build_expr(pair: Pair<'_>, param_pool: &BTreeMap<String, DataValue>) -> Result<Expr> {
    ensure!(
        pair.as_rule() == Rule::expr,
        InvalidExpression(pair.extract_span())
    );

    pratt_parser()
        .map_primary(|v| build_term(v, param_pool))
        .map_infix(build_expr_infix)
        .map_prefix(|op, rhs| {
            let rhs = rhs?;
            let rhs_span = rhs.span();
            Ok(match op.as_rule() {
                Rule::minus => Expr::Apply {
                    op: OP_MINUS,
                    args: [rhs].into(),
                    span: op.extract_span().merge(rhs_span),
                },
                Rule::negate => Expr::Apply {
                    op: OP_NEGATE,
                    args: [rhs].into(),
                    span: op.extract_span().merge(rhs_span),
                },
                _other => bail!(UnexpectedRule(op.extract_span())),
            })
        })
        .parse(pair.into_inner())
}

fn build_expr_infix(lhs: Result<Expr>, op: Pair<'_>, rhs: Result<Expr>) -> Result<Expr> {
    let args = [lhs?, rhs?];
    let start = args[0].span().0;
    let end = args[1].span().0 + args[1].span().1;
    let length = end - start;
    let span = SourceSpan(start, length);
    Ok(match op.as_rule() {
        Rule::op_or => Expr::Lazy {
            op: LazyOp::Or,
            args: args.into(),
            span,
        },
        Rule::op_and => Expr::Lazy {
            op: LazyOp::And,
            args: args.into(),
            span,
        },
        Rule::op_coalesce => Expr::Lazy {
            op: LazyOp::Coalesce,
            args: args.into(),
            span,
        },
        Rule::op_add => Expr::Apply {
            op: OP_ADD,
            args: args.into(),
            span,
        },
        Rule::op_sub => Expr::Apply {
            op: OP_SUB,
            args: args.into(),
            span,
        },
        Rule::op_mul => Expr::Apply {
            op: OP_MUL,
            args: args.into(),
            span,
        },
        Rule::op_div => Expr::Apply {
            op: OP_DIV,
            args: args.into(),
            span,
        },
        Rule::op_mod => Expr::Apply {
            op: OP_MOD,
            args: args.into(),
            span,
        },
        Rule::op_pow => Expr::Apply {
            op: OP_POW,
            args: args.into(),
            span,
        },
        Rule::op_eq => Expr::Apply {
            op: OP_EQ,
            args: args.into(),
            span,
        },
        Rule::op_ne => Expr::Apply {
            op: OP_NEQ,
            args: args.into(),
            span,
        },
        Rule::op_gt => Expr::Apply {
            op: OP_GT,
            args: args.into(),
            span,
        },
        Rule::op_ge => Expr::Apply {
            op: OP_GE,
            args: args.into(),
            span,
        },
        Rule::op_lt => Expr::Apply {
            op: OP_LT,
            args: args.into(),
            span,
        },
        Rule::op_le => Expr::Apply {
            op: OP_LE,
            args: args.into(),
            span,
        },
        Rule::op_concat => Expr::Apply {
            op: OP_CONCAT,
            args: args.into(),
            span,
        },
        Rule::op_field_access => Expr::Apply {
            op: OP_MAYBE_GET,
            args: args.into(),
            span,
        },
        _other => bail!(UnexpectedRule(op.extract_span())),
    })
}

fn build_term(pair: Pair<'_>, param_pool: &BTreeMap<String, DataValue>) -> Result<Expr> {
    let span = pair.extract_span();
    let op = pair.as_rule();
    Ok(match op {
        Rule::var => Expr::Binding {
            var: Symbol::new(pair.as_str(), pair.extract_span()),
            tuple_pos: BindingPos::Unresolved,
        },
        Rule::param => {
            #[derive(Error, Diagnostic, Debug)]
            #[error("Required parameter {0} not found")]
            #[diagnostic(code(parser::param_not_found))]
            struct ParamNotFoundError(String, #[label] SourceSpan);

            let param_str = pair.as_str().strip_prefix('$').unwrap();
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
        Rule::hex_pos_int => {
            let i = parse_int(pair.as_str(), 16, span)?;
            Expr::Const {
                val: DataValue::from(i),
                span,
            }
        }
        Rule::octo_pos_int => {
            let i = parse_int(pair.as_str(), 8, span)?;
            Expr::Const {
                val: DataValue::from(i),
                span,
            }
        }
        Rule::bin_pos_int => {
            let i = parse_int(pair.as_str(), 2, span)?;
            Expr::Const {
                val: DataValue::from(i),
                span,
            }
        }
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
                val: DataValue::Str(s.into()),
                span,
            }
        }
        Rule::list => {
            let mut collected = vec![];
            for p in pair.into_inner() {
                collected.push(build_expr(p, param_pool)?)
            }
            Expr::Apply {
                op: OP_LIST,
                args: collected.into(),
                span,
            }
        }
        Rule::object => {
            let mut args = vec![];
            for p in pair.into_inner() {
                let mut p = p.into_inner();
                let k = p.next().unwrap();
                let v = p.next().unwrap();
                args.push(build_expr(k, param_pool)?);
                args.push(build_expr(v, param_pool)?);
            }
            Expr::Apply {
                op: OP_JSON_OBJECT,
                args: args.into(),
                span,
            }
        }
        Rule::apply => {
            let mut p = pair.into_inner();
            let ident_p = p.next().unwrap();
            let ident = ident_p.as_str();
            let mut args: Vec<Expr> = Vec::new();
            for v in p.next().unwrap().into_inner() {
                args.push(build_expr(v, param_pool)?);
            }
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
                        let last_span = args.last().unwrap().span();
                        args.insert(
                            args.len() - 1,
                            Expr::Const {
                                val: DataValue::Null,
                                span: last_span,
                            },
                        )
                    }
                    let mut clauses: Vec<(Expr, Expr)> = args
                        .chunks(2)
                        .map(|pair| (pair[0].clone(), pair[1].clone()))
                        .collect();
                    if let Some((cond, _)) = clauses.last() {
                        match cond {
                            Expr::Const {
                                val: DataValue::Bool(true),
                                ..
                            } => {}
                            _other => {
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

                    ensure!(args.len() == 2 || args.len() == 3, WrongArgsToIf(span));

                    let mut clauses = vec![];
                    let mut args = args.into_iter();
                    let cond = args.next().unwrap();
                    let then = args.next().unwrap();
                    clauses.push((cond, then));
                    clauses.push((
                        Expr::Const {
                            val: DataValue::from(true),
                            span,
                        },
                        match args.next() {
                            Some(e) => e,
                            None => Expr::Const {
                                val: DataValue::Null,
                                span,
                            },
                        },
                    ));
                    Expr::Cond { clauses, span }
                }
                _other => match resolve_decl(ident) {
                    None => Expr::UnboundApply {
                        op: ident.into(),
                        args: args.into(),
                        span,
                    },
                    Some(op) => {
                        #[derive(Error, Diagnostic, Debug)]
                        #[error("Wrong number of arguments for function '{0}'")]
                        #[diagnostic(code(parser::func_wrong_num_args))]
                        struct WrongNumArgsError(String, #[label] SourceSpan, #[help] String);

                        if op.is_vararg() {
                            ensure!(
                                op.min_arity <= args.len(),
                                WrongNumArgsError(
                                    ident.to_string(),
                                    span,
                                    format!("Need at least {} argument(s)", op.min_arity)
                                )
                            );
                        } else {
                            ensure!(
                                op.min_arity == args.len(),
                                WrongNumArgsError(
                                    ident.to_string(),
                                    span,
                                    format!("Need exactly {} argument(s)", op.min_arity)
                                )
                            );
                        }
                        Expr::Apply {
                            op,
                            args: args.into(),
                            span,
                        }
                    }
                },
            }
        }
        Rule::grouping => build_expr(pair.into_inner().next().unwrap(), param_pool)?,
        _other => bail!(UnexpectedRule(pair.extract_span())),
    })
}

#[derive(Error, Diagnostic, Debug)]
#[error("Cannot parse integer")]
#[diagnostic(code(parser::bad_pos_int))]
struct BadIntError(#[label] SourceSpan);

/// Parse a prefixed radix literal (`0x` / `0o` / `0b` / `\u`) to `i64`.
/// Overflow and malformed digits refuse with spanned [`BadIntError`] — never abort.
pub(crate) fn parse_int(s: &str, radix: u32, span: SourceSpan) -> Result<i64> {
    Ok(i64::from_str_radix(&s[2..].replace('_', ""), radix).map_err(|_| BadIntError(span))?)
}

pub(crate) fn parse_string(pair: Pair<'_>) -> Result<SmartString<LazyCompact>> {
    match pair.as_rule() {
        Rule::quoted_string => Ok(parse_quoted_string(pair)?),
        Rule::s_quoted_string => Ok(parse_s_quoted_string(pair)?),
        Rule::raw_string => Ok(parse_raw_string(pair)?),
        Rule::ident => Ok(SmartString::from(pair.as_str())),
        _other => bail!(UnexpectedRule(pair.extract_span())),
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

/// One escape-decode loop for double- and single-quoted string literals.
/// Quote-escape token (`\"` vs `\'`) is the independence; the scaffold is shared.
fn parse_quoted_string_inner(
    pair: Pair<'_>,
    quote_escape: &str,
    quote_char: char,
) -> Result<SmartString<LazyCompact>> {
    let pairs = pair.into_inner().next().unwrap().into_inner();
    let mut ret = SmartString::new();
    for pair in pairs {
        let s = pair.as_str();
        if s == quote_escape {
            ret.push(quote_char);
            continue;
        }
        match s {
            r"\\" => ret.push('\\'),
            r"\/" => ret.push('/'),
            r"\b" => ret.push('\x08'),
            r"\f" => ret.push('\x0c'),
            r"\n" => ret.push('\n'),
            r"\r" => ret.push('\r'),
            r"\t" => ret.push('\t'),
            s if s.starts_with(r"\u") => {
                let esc_span = pair.extract_span();
                let code = parse_int(s, 16, esc_span)? as u32;
                let ch = char::from_u32(code).ok_or_else(|| InvalidUtf8Error(code, esc_span))?;
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

fn parse_quoted_string(pair: Pair<'_>) -> Result<SmartString<LazyCompact>> {
    parse_quoted_string_inner(pair, r#"\""#, '"')
}

fn parse_s_quoted_string(pair: Pair<'_>) -> Result<SmartString<LazyCompact>> {
    parse_quoted_string_inner(pair, r#"\'"#, '\'')
}

fn parse_raw_string(pair: Pair<'_>) -> Result<SmartString<LazyCompact>> {
    Ok(SmartString::from(
        pair.into_inner().next().unwrap().as_str(),
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::parse::parse_expressions;

    /// Overflowing radix literals must refuse with BadIntError — never panic/abort.
    fn assert_radix_overflow_refuses(src: &str) {
        let err = parse_expressions(src, &BTreeMap::new())
            .expect_err("overflowing int literal must refuse, not succeed");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("Cannot parse integer") || msg.contains("bad_pos_int"),
            "expected BadIntError for {src:?}, got: {msg}"
        );
    }

    #[test]
    fn hex_literal_overflow_is_bad_int_error() {
        assert_radix_overflow_refuses("0xFFFFFFFFFFFFFFFF");
        assert_radix_overflow_refuses("0x1_0000_0000_0000_0000");
    }

    #[test]
    fn octal_literal_overflow_is_bad_int_error() {
        // 2^63 in octal exceeds i64::MAX
        assert_radix_overflow_refuses("0o1000000000000000000000");
    }

    #[test]
    fn binary_literal_overflow_is_bad_int_error() {
        assert_radix_overflow_refuses(&format!("0b{}", "1".repeat(64)));
        assert_radix_overflow_refuses(&format!("0b{}", "1".repeat(80)));
    }

    #[test]
    fn radix_literal_in_bounds_parses() {
        let e = parse_expressions("0x7FFFFFFFFFFFFFFF", &BTreeMap::new()).unwrap();
        assert_eq!(e.get_const().and_then(|v| v.get_int()), Some(i64::MAX));
        let e = parse_expressions("0o777", &BTreeMap::new()).unwrap();
        assert_eq!(e.get_const().and_then(|v| v.get_int()), Some(0o777));
        let e = parse_expressions("0b1010", &BTreeMap::new()).unwrap();
        assert_eq!(e.get_const().and_then(|v| v.get_int()), Some(0b1010));
    }

    #[test]
    fn fuzzish_radix_overflow_corpus_refuses_without_panic() {
        let corpus = [
            "0xFFFFFFFFFFFFFFFF",
            "0xffffffffffffffff",
            "0x8000000000000000",
            "0o1777777777777777777777",
            "0o2000000000000000000000",
            "0b1111111111111111111111111111111111111111111111111111111111111111",
            "0b1000000000000000000000000000000000000000000000000000000000000000",
            "0xFFFF_FFFF_FFFF_FFFF",
            "0o1_000_000_000_000_000_000_000",
        ];
        for src in corpus {
            let err = parse_expressions(src, &BTreeMap::new())
                .expect_err("overflowing radix literal must refuse, not succeed or abort");
            let msg = format!("{err:?}");
            assert!(
                msg.contains("Cannot parse integer") || msg.contains("bad_pos_int"),
                "expected BadIntError for {src:?}, got: {msg}"
            );
        }
    }
}
