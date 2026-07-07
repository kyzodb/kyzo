/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): the back-trace reconstruction `unwrap` is annotated as
 * structural (every non-start node on a reconstructed path was inserted
 * into `back_trace` when it was first relaxed); output rows flow through
 * the arity-checked writer. `run` and the search are otherwise unchanged.
 */

//! A* shortest path over relation-shaped graphs: edges and nodes stay as
//! tuples (no CSR interning) so the user-supplied `heuristic` expression
//! can read node attributes.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use miette::{Result, ensure};
use ordered_float::OrderedFloat;
use priority_queue::PriorityQueue;
use smartstring::{LazyCompact, SmartString};

use crate::data::expr::{Expr, eval_bytecode};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::data::value::Tuple;
use crate::fixed_rule::{
    BadExprValueError, CancelFlag, FixedRule, FixedRuleInputRelation, FixedRuleOutput,
    FixedRulePayload, NodeNotFoundError,
};

pub(crate) struct ShortestPathAStar;

impl FixedRule for ShortestPathAStar {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let edges = payload.get_input(0)?.ensure_min_len(2)?;
        let nodes = payload.get_input(1)?;
        let starting = payload.get_input(2)?.ensure_min_len(1)?;
        let goals = payload.get_input(3)?.ensure_min_len(1)?;
        let mut heuristic = payload.expr_option("heuristic", None)?;

        let mut binding_map = nodes.get_binding_map(0);
        let goal_binding_map = goals.get_binding_map(nodes.arity()?);
        binding_map.extend(goal_binding_map);
        heuristic.fill_binding_indices(&binding_map)?;
        for start in starting.iter()? {
            let start = start?;
            for goal in goals.iter()? {
                let goal = goal?;
                let (cost, path) = astar(&start, &goal, edges, nodes, &heuristic, cancel.clone())?;
                // Structural: `ensure_min_len(1)` on `starting`/`goals`
                // proved every tuple has a first column.
                out.put(
                    vec![
                        start[0].clone(),
                        goal[0].clone(),
                        DataValue::from(cost),
                        DataValue::List(path),
                    ]
                    .into(),
                )?;
            }
        }

        Ok(())
    }

    fn arity(
        &self,
        _options: &BTreeMap<SmartString<LazyCompact>, Expr>,
        _rule_head: &[Symbol],
        _span: SourceSpan,
    ) -> Result<usize> {
        Ok(4)
    }
}

fn astar(
    starting: &Tuple,
    goal: &Tuple,
    edges: FixedRuleInputRelation<'_>,
    nodes: FixedRuleInputRelation<'_>,
    heuristic: &Expr,
    cancel: CancelFlag,
) -> Result<(f64, Vec<DataValue>)> {
    // Structural: the caller's `ensure_min_len(1)` on `starting`/`goals`
    // proved every tuple has a first column.
    let start_node = &starting[0];
    let goal_node = &goal[0];
    let heuristic_bytecode = heuristic.compile()?;
    let mut stack = vec![];
    let mut eval_heuristic = |node: &Tuple| -> Result<f64> {
        let mut v = node.clone();
        v.extend(goal.iter().cloned());
        let t = v;
        let cost_val = eval_bytecode(&heuristic_bytecode, &t, &mut stack)?;
        let cost = cost_val.get_float().ok_or_else(|| {
            BadExprValueError(
                cost_val,
                heuristic.span(),
                "a number is required".to_string(),
            )
        })?;
        ensure!(
            !cost.is_nan(),
            BadExprValueError(
                DataValue::from(cost),
                heuristic.span(),
                "a number is required".to_string(),
            )
        );
        Ok(cost)
    };
    let mut back_trace: BTreeMap<DataValue, DataValue> = Default::default();
    let mut g_score: BTreeMap<DataValue, f64> = BTreeMap::from([(start_node.clone(), 0.)]);
    let mut open_set: PriorityQueue<DataValue, (Reverse<OrderedFloat<f64>>, usize)> =
        PriorityQueue::new();
    open_set.push(start_node.clone(), (Reverse(OrderedFloat(0.)), 0));
    let mut sub_priority: usize = 0;
    while let Some((node, (Reverse(OrderedFloat(cost)), _))) = open_set.pop() {
        if node == *goal_node {
            let mut current = node;
            let mut ret = vec![];
            while current != *start_node {
                // Structural: every non-start node popped from the open set
                // was inserted into `back_trace` when it was first relaxed,
                // so walking predecessors from the goal cannot miss.
                let prev = back_trace.get(&current).unwrap().clone();
                ret.push(current);
                current = prev;
            }
            ret.push(current);
            ret.reverse();
            return Ok((cost, ret));
        }

        for edge in edges.prefix_iter(&node)? {
            let edge = edge?;
            let edge_dst = &edge[1];
            let edge_cost = match edge.get(2) {
                None => 1.,
                Some(cost) => cost.get_float().ok_or_else(|| {
                    BadExprValueError(
                        edge_dst.clone(),
                        edges.span(),
                        "edge cost must be a number".to_string(),
                    )
                })?,
            };
            ensure!(
                !edge_cost.is_nan(),
                BadExprValueError(
                    edge_dst.clone(),
                    edges.span(),
                    "edge cost must be a number".to_string(),
                )
            );

            let cost_to_src = g_score.get(&node).cloned().unwrap_or(f64::INFINITY);
            let tentative_cost_to_dst = cost_to_src + edge_cost;
            let prev_cost_to_dst = g_score.get(edge_dst).cloned().unwrap_or(f64::INFINITY);
            if tentative_cost_to_dst < prev_cost_to_dst {
                back_trace.insert(edge_dst.clone(), node.clone());
                g_score.insert(edge_dst.clone(), tentative_cost_to_dst);

                let edge_dst_tuple =
                    nodes
                        .prefix_iter(edge_dst)?
                        .next()
                        .ok_or_else(|| NodeNotFoundError {
                            missing: edge_dst.clone(),
                            span: nodes.span(),
                        })??;

                let heuristic_cost = eval_heuristic(&edge_dst_tuple)?;
                sub_priority += 1;
                open_set.push_increase(
                    edge_dst.clone(),
                    (
                        Reverse(OrderedFloat(tentative_cost_to_dst + heuristic_cost)),
                        sub_priority,
                    ),
                );
            }
            cancel.check()?;
        }
    }
    Ok((f64::INFINITY, vec![]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixed_rule::tests_support::{TestInput, run_fixed_rule};

    fn s(v: &str) -> DataValue {
        DataValue::from(v)
    }

    /// VALUE ORACLE: A* with a real (admissible) heuristic read off the
    /// node relation's second column.
    ///
    /// Graph: a→b: 1, b→c: 1, a→c: 3. Heuristic column h = remaining
    /// distance to c: h(a)=2, h(b)=1, h(c)=0.
    ///
    /// Hand trace (f = g + h):
    ///   pop a: relax b (g=1, f=1+1=2), relax c (g=3, f=3+0=3)
    ///   pop b (f=2 < 3): relax c to g=2, f=2+0=2 (priority raised)
    ///   pop c (f=2): goal ⇒ cost 2, route c→b→a reversed = [a,b,c]
    /// The direct a→c edge (cost 3) loses to the guided two-hop route.
    #[test]
    fn heuristic_guided_route() {
        let h_binding = Expr::Binding {
            var: Symbol::new("h", SourceSpan::default()),
            tuple_pos: None,
        };
        let got = run_fixed_rule(
            &ShortestPathAStar,
            vec![
                TestInput::new(
                    vec!["fr", "to", "w"],
                    vec![
                        vec![s("a"), s("b"), DataValue::from(1.0)].into(),
                        vec![s("b"), s("c"), DataValue::from(1.0)].into(),
                        vec![s("a"), s("c"), DataValue::from(3.0)].into(),
                    ],
                ),
                TestInput::new(
                    vec!["id", "h"],
                    vec![
                        vec![s("a"), DataValue::from(2.0)].into(),
                        vec![s("b"), DataValue::from(1.0)].into(),
                        vec![s("c"), DataValue::from(0.0)].into(),
                    ],
                ),
                TestInput::new(vec!["start"], vec![vec![s("a")].into()]),
                TestInput::new(vec!["goal"], vec![vec![s("c")].into()]),
            ],
            BTreeMap::from([(SmartString::from("heuristic"), h_binding)]),
            CancelFlag::default(),
        )
        .unwrap();
        let want: Vec<Tuple> = vec![
            vec![
                s("a"),
                s("c"),
                DataValue::from(2.0),
                DataValue::List(vec![s("a"), s("b"), s("c")]),
            ]
            .into(),
        ];
        assert_eq!(got, want);
    }
}
