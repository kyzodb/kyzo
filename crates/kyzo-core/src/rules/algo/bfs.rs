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

//! Breadth-first search from starting nodes, collecting up to `limit`
//! nodes satisfying `condition`, with their routes.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

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

pub(crate) struct Bfs;

impl FixedRule for Bfs {
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
            // INVARIANT(bfs_start_col): `ensure_min_len(1)` proved a first column.
            let starting_node = &node_tuple[0];
            if visited.contains(starting_node) {
                continue;
            }
            visited.insert(starting_node.clone());

            let mut queue: VecDeque<DataValue> = VecDeque::default();
            queue.push_front(starting_node.clone());

            while let Some(candidate) = queue.pop_back() {
                for edge in edges.prefix_iter(&candidate)? {
                    // Polled at the top of the per-edge unit of work so no
                    // early exit (`continue` on visited, `break` on limit)
                    // can complete a run past a raised flag.
                    cancel.check()?;
                    let edge = edge?;
                    let to_node = &edge[1];
                    if visited.contains(to_node) {
                        continue;
                    }

                    visited.insert(to_node.clone());
                    backtrace.insert(to_node.clone(), candidate.clone());

                    let cand_tuple = if skip_query_nodes {
                        Tuple::from_vec(vec![to_node.clone()])
                    } else {
                        nodes
                            .prefix_iter(to_node)?
                            .next()
                            .ok_or_else(|| NodeNotFoundError {
                                missing: candidate.clone(),
                                span: nodes.span(),
                            })??
                    };

                    if crate::exec::expr::eval_pred(&condition, &cand_tuple)? {
                        found.push((starting_node.clone(), to_node.clone()));
                        if found.len() >= limit {
                            break 'outer;
                        }
                    }

                    queue.push_front(to_node.clone());
                }
            }
        }

        for (starting, ending) in found {
            let mut route = vec![];
            let mut current = ending.clone();
            while current != starting {
                route.push(current.clone());
                // INVARIANT(bfs_pred): `ending` was reached from `starting`,
                // and every visited node except the start got a backtrace
                // entry when it was discovered.
                current = backtrace_predecessor(&backtrace, &current, "bfs_pred")?;
            }
            route.push(starting.clone());
            route.reverse();
            let tuple = vec![starting, ending, DataValue::List(route)];
            out.put(Tuple::from_vec(tuple))?;
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

    /// VALUE ORACLE: exact BFS answer on the diamond a→{b,c}, b→d, c→d
    /// with `condition: true`, `limit: 10`.
    ///
    /// Hand trace (queue is FIFO; a node's edges scan in sorted order):
    ///   pop a: discover b (route a,b), discover c (route a,c)
    ///   pop b: discover d via b ⇒ route (a,b,d)
    ///   pop c: d already visited
    ///   pop d: no edges
    /// The start node itself is never tested against the condition
    /// (upstream-parity: only nodes reached over an edge are "found"), so
    /// exactly three rows, and d's route goes through b, not c.
    #[test]
    fn exact_traversal_and_routes() {
        let got = run_fixed_rule(
            &Bfs,
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
            Tuple::from_vec(vec![s("a"), s("b"), DataValue::List(vec![s("a"), s("b")])]),
            Tuple::from_vec(vec![s("a"), s("c"), DataValue::List(vec![s("a"), s("c")])]),
            Tuple::from_vec(vec![
                s("a"),
                s("d"),
                DataValue::List(vec![s("a"), s("b"), s("d")]),
            ]),
        ];
        assert_eq!(got, want);
    }
}
