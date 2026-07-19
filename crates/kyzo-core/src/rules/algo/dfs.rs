/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): `binding_indices` now returns `Result` (typed instead of a
 * panic on an unresolved binding); the route-reconstruction `unwrap` is
 * annotated as structural; output rows flow through the arity-checked
 * writer.
 */

//! Depth-first search from starting nodes, collecting up to `limit` nodes
//! satisfying `condition`, with their routes. (Iterative — an explicit
//! to-visit stack, not recursion.)

use std::collections::{BTreeMap, BTreeSet};

use miette::Result;
use smartstring::{LazyCompact, SmartString};

use kyzo_model::program::expr::Expr;
use kyzo_model::program::rule::FixedRuleOptions;
use kyzo_model::SourceSpan;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::{DataValue, Tuple};
use crate::rules::contract::{
    backtrace_predecessor, CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload,
    NodeNotFoundError,
};

pub(crate) struct Dfs;

impl FixedRule for Dfs {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let edges = payload.get_input(0)?.ensure_min_len(2)?;
        let nodes = payload.get_input(1)?;
        let starting_nodes = payload.get_input(2).unwrap_or(nodes).ensure_min_len(1)?;
        let limit = payload.pos_integer_option("limit", Some(1))?;
        let mut condition = payload.expr_option("condition", None)?;
        let binding_map = nodes.get_binding_map(0);
        condition.fill_binding_indices(&binding_map)?;
        let binding_indices = condition.binding_indices()?;
        let skip_query_nodes = binding_indices.is_subset(&BTreeSet::from([0]));

        let mut visited: BTreeSet<DataValue> = Default::default();
        let mut backtrace: BTreeMap<DataValue, DataValue> = Default::default();
        let mut found: Vec<(DataValue, DataValue)> = vec![];

        'outer: for node_tuple in starting_nodes.iter()? {
            let node_tuple = node_tuple?;
            // INVARIANT(dfs_start_col): `ensure_min_len(1)` proved a first column.
            let starting_node = &node_tuple[0];
            if visited.contains(starting_node) {
                continue;
            }

            let mut to_visit_stack: Vec<DataValue> = vec![];
            to_visit_stack.push(starting_node.clone());

            while let Some(candidate) = to_visit_stack.pop() {
                // Polled at the top of the per-node unit of work so no
                // early exit (`continue` on visited, `break` on limit) can
                // complete a run past a raised flag.
                cancel.check()?;
                if visited.contains(&candidate) {
                    continue;
                }

                let cand_tuple = if skip_query_nodes {
                    Tuple::from_vec(vec![candidate.clone()])
                } else {
                    nodes
                        .prefix_iter(&candidate)?
                        .next()
                        .ok_or_else(|| NodeNotFoundError {
                            missing: candidate.clone(),
                            span: nodes.span(),
                        })??
                };

                if crate::exec::expr::eval_pred(&condition, &cand_tuple)? {
                    found.push((starting_node.clone(), candidate.clone()));
                    if found.len() >= limit {
                        break 'outer;
                    }
                }

                visited.insert(candidate.clone());

                for edge in edges.prefix_iter(&candidate)? {
                    let edge = edge?;
                    let to_node = &edge[1];
                    if visited.contains(to_node) {
                        continue;
                    }
                    backtrace.insert(to_node.clone(), candidate.clone());
                    to_visit_stack.push(to_node.clone());
                }
            }
        }

        for (starting, ending) in found {
            let mut route = vec![];
            let mut current = ending.clone();
            while current != starting {
                route.push(current.clone());
                // INVARIANT(dfs_pred): every discovered non-start node
                // received a backtrace entry before it was pushed to visit.
                current = backtrace_predecessor(&backtrace, &current, "dfs_pred")?;
            }
            route.push(starting.clone());
            route.reverse();
            let tuple = vec![starting, ending, DataValue::List(route)];
            out.put(Tuple::from_vec(tuple))?;
            cancel.check()?;
        }
        Ok(())
    }

    fn arity(
        &self,
        _options: &FixedRuleOptions,
        _rule_head: &[Symbol],
        _span: SourceSpan,
    ) -> Result<usize> {
        Ok(3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kyzo_model::value::Tuple;
    use crate::rules::contract::tests_support::{TestInput, run_fixed_rule, opts_map};

    fn s(v: &str) -> DataValue {
        DataValue::from(v)
    }

    /// VALUE ORACLE: exact DFS answer on the same diamond BFS is tested
    /// on (a→{b,c}, b→d, c→d; `condition: true`, `limit: 10`) — the two
    /// traversals are distinguished by their exact outputs.
    ///
    /// Hand trace (explicit LIFO stack; a node's edges push in sorted
    /// order, so the LAST-sorted neighbor pops first):
    ///   pop a: found (a,a,[a]) — DFS tests the popped node itself, so
    ///          unlike BFS the start appears; push b, push c
    ///   pop c: found (a,c,[a,c]); push d (backtrace d→c)
    ///   pop d: found (a,d,[a,c,d]) — via c, where BFS went via b
    ///   pop b: found (a,b,[a,b]); d already visited
    /// ⇒ exactly four rows, with d's route through c.
    #[test]
    fn exact_traversal_and_routes() {
        let got = run_fixed_rule(
            &Dfs,
            vec![
                TestInput::new(
                    vec!["fr", "to"],
                    vec![
                        Tuple::from_vec(vec![s("a"), s("b")]),
                        Tuple::from_vec(vec![s("a"), s("c")]),
                        Tuple::from_vec(vec![s("b"), s("d")]),
                        Tuple::from_vec(vec![s("c"), s("d")]),
                    ],
                ),
                TestInput::new(
                    vec!["id"],
                    vec![
                        Tuple::from_vec(vec![s("a")]),
                        Tuple::from_vec(vec![s("b")]),
                        Tuple::from_vec(vec![s("c")]),
                        Tuple::from_vec(vec![s("d")]),
                    ],
                ),
                TestInput::new(vec!["start"], vec![Tuple::from_vec(vec![s("a")])]),
            ],
            opts_map(BTreeMap::from([
                (
                    SmartString::from("condition"),
                    Expr::Const {
                        val: DataValue::from(true),
                        span: SourceSpan::default(),
                    },
                ),
                (
                    SmartString::from("limit"),
                    Expr::Const {
                        val: DataValue::from(10i64),
                        span: SourceSpan::default(),
                    },
                ),
            ])),
            CancelFlag::default(),
        )
        .unwrap();
        let want: Vec<Tuple> = vec![
            Tuple::from_vec(vec![s("a"), s("a"), DataValue::List(vec![s("a")])]),
            Tuple::from_vec(vec![s("a"), s("b"), DataValue::List(vec![s("a"), s("b")])]),
            Tuple::from_vec(vec![s("a"), s("c"), DataValue::List(vec![s("a"), s("c")])]),
            Tuple::from_vec(vec![
                s("a"),
                s("d"),
                DataValue::List(vec![s("a"), s("c"), s("d")]),
            ]),
        ];
        assert_eq!(got, want);
    }
}
