/*
 *  Copyright 2023, The Cozo Project Authors.
 *
 *  This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 *  If a copy of the MPL was not distributed with this file,
 *  You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): grammar-shape unwraps and unreachable! dispatch arms go
 * through the typed-accessor layer; either::Either is replaced by the
 * named [`QueryOrRelation`] sum type; ImperativeSysop carries
 * [`SysScript`] (crate-wall pure data), not engine SysOp; fixed-rule
 * binding stays exec-tier — this door is params + session stamp only.
 */

//! Parsing imperative scripts: queries chained under control flow.
//!
//! Each embedded `{…}` query parses through the same [`parse_query`] proof
//! as a standalone script — an imperative program is a *composition of
//! proven programs*, plus control structure (`%if`, `%loop`, `%return`,
//! `%swap`, `%debug`) over named temporary relations.

use std::collections::BTreeMap;

use miette::Result;
use smartstring::SmartString;

use crate::parse::query::parse_query;
use crate::parse::sys::parse_sys;
use crate::parse::{
    ExtractSpan, ImperativeProgram, ImperativeStmt, ImperativeStmtClause, ImperativeSysop,
    IntoChildren, Pair, QueryOrRelation, Rule, unexpected,
};
use crate::value::{DataValue, ValidityTs};

/// Walk an `imperative_script` / `imperative_block` into typed IR.
pub(crate) fn parse_imperative_block(
    src: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    cur_vld: ValidityTs,
) -> Result<ImperativeProgram> {
    let mut collected = Vec::new();

    for pair in src.into_inner() {
        if pair.as_rule() == Rule::EOI {
            break;
        }
        collected.push(parse_imperative_stmt(pair, param_pool, cur_vld)?);
    }

    Ok(collected)
}

fn parse_imperative_stmt(
    pair: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    cur_vld: ValidityTs,
) -> Result<ImperativeStmt> {
    Ok(match pair.as_rule() {
        Rule::break_stmt => {
            let span = pair.extract_span();
            let target = pair
                .into_inner()
                .next()
                .map(|p| SmartString::from(p.as_str()));
            ImperativeStmt::Break { target, span }
        }
        Rule::continue_stmt => {
            let span = pair.extract_span();
            let target = pair
                .into_inner()
                .next()
                .map(|p| SmartString::from(p.as_str()));
            ImperativeStmt::Continue { target, span }
        }
        Rule::return_stmt => {
            let mut rets = Vec::new();
            for p in pair.into_inner() {
                match p.as_rule() {
                    Rule::ident | Rule::underscore_ident => {
                        let rel = SmartString::from(p.as_str());
                        rets.push(QueryOrRelation::Relation(rel));
                    }
                    Rule::query_script_inner => {
                        let mut src = p.children();
                        let prog = parse_query(
                            src.expect("the returned query")?.into_inner(),
                            param_pool,
                            cur_vld,
                        )?;
                        let store_as = src.next().map(|p| SmartString::from(p.as_str().trim()));
                        rets.push(QueryOrRelation::Query(Box::new(ImperativeStmtClause {
                            prog,
                            store_as,
                        })))
                    }
                    _other => return Err(unexpected("a returned query or relation name", &p)),
                }
            }
            ImperativeStmt::Return { returns: rets }
        }
        Rule::if_chain | Rule::if_not_chain => {
            let negated = pair.as_rule() == Rule::if_not_chain;
            let mut inner = pair.children();
            let condition = inner.expect("the condition")?;
            let cond = match condition.as_rule() {
                Rule::underscore_ident => {
                    QueryOrRelation::Relation(SmartString::from(condition.as_str()))
                }
                Rule::imperative_clause => {
                    let mut src = condition.children();
                    let prog = parse_query(
                        src.expect("the condition query")?.into_inner(),
                        param_pool,
                        cur_vld,
                    )?;
                    let store_as = src.next().map(|p| SmartString::from(p.as_str().trim()));
                    QueryOrRelation::Query(Box::new(ImperativeStmtClause { prog, store_as }))
                }
                _other => return Err(unexpected("an if-condition", &condition)),
            };
            let then_branch =
                parse_stmt_children(inner.expect("the then-branch")?, param_pool, cur_vld)?;
            let else_branch = match inner.next() {
                None => Vec::new(),
                Some(rest) => parse_stmt_children(rest, param_pool, cur_vld)?,
            };
            ImperativeStmt::If {
                condition: cond,
                then_branch,
                else_branch,
                negated,
            }
        }
        Rule::loop_block => {
            let mut inner = pair.children();
            let mut mark = None;
            let mut nxt = inner.expect("the loop label or body")?;
            if nxt.as_rule() == Rule::ident {
                mark = Some(SmartString::from(nxt.as_str()));
                nxt = inner.expect("the loop body")?;
            }
            let body = parse_imperative_block(nxt, param_pool, cur_vld)?;
            ImperativeStmt::Loop { label: mark, body }
        }
        Rule::temp_swap => {
            let [left, right] = pair
                .children()
                .expect_n(["the left relation", "the right relation"])?;
            ImperativeStmt::TempSwap {
                left: SmartString::from(left.as_str()),
                right: SmartString::from(right.as_str()),
            }
        }
        Rule::debug_stmt => {
            let name_p = pair.children().expect("the relation to debug")?;
            ImperativeStmt::TempDebug {
                temp: SmartString::from(name_p.as_str()),
            }
        }
        Rule::imperative_sysop => {
            let mut src = pair.children();
            let sysop = parse_sys(
                src.expect("the system operation")?.into_inner(),
                param_pool,
                cur_vld,
            )?;
            let store_as = src.next().map(|p| SmartString::from(p.as_str().trim()));
            ImperativeStmt::SysOp {
                sysop: ImperativeSysop { sysop, store_as },
            }
        }
        Rule::imperative_clause => {
            let mut src = pair.children();
            let prog = parse_query(src.expect("the query")?.into_inner(), param_pool, cur_vld)?;
            let store_as = src.next().map(|p| SmartString::from(p.as_str().trim()));
            ImperativeStmt::Program {
                prog: ImperativeStmtClause { prog, store_as },
            }
        }
        Rule::ignore_error_script => {
            let pair = pair.children().expect("the guarded clause")?;
            let mut src = pair.children();
            let prog = parse_query(src.expect("the query")?.into_inner(), param_pool, cur_vld)?;
            let store_as = src.next().map(|p| SmartString::from(p.as_str().trim()));
            ImperativeStmt::IgnoreErrorProgram {
                prog: ImperativeStmtClause { prog, store_as },
            }
        }
        _other => return Err(unexpected("an imperative statement", &pair)),
    })
}

fn parse_stmt_children(
    block: Pair<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    cur_vld: ValidityTs,
) -> Result<ImperativeProgram> {
    let mut out = Vec::new();
    for p in block.into_inner() {
        out.push(parse_imperative_stmt(p, param_pool, cur_vld)?);
    }
    Ok(out)
}
