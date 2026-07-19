/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): the external `graph` crate's CSR type is replaced by the
 * inline one in `fixed_rule/graph.rs`; output rows flow through the
 * arity-checked writer. Algorithm unchanged.
 */

//! Prim's minimum spanning tree from an optional starting node.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use miette::{Diagnostic, Result};
use ordered_float::OrderedFloat;
use priority_queue::PriorityQueue;
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::rules::contract::{CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload};
use crate::rules::graph_view::DirectedCsrGraph;
use kyzo_model::SourceSpan;
use kyzo_model::program::expr::Expr;
use kyzo_model::program::rule::FixedRuleOptions;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::{DataValue, Tuple};

pub(crate) struct MinimumSpanningTreePrim;

impl FixedRule for MinimumSpanningTreePrim {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let edges = payload.get_input(0)?;
        let (graph, indices, inv_indices) = edges.as_directed_weighted_graph(true, true)?;
        if graph.node_count() == 0 {
            return Ok(());
        }
        let starting = match payload.get_input(1) {
            Err(_) => 0,
            Ok(rel) => {
                let rel = rel.ensure_min_len(1)?;
                let tuple = rel.iter()?.next().ok_or_else(|| {
                    #[derive(Debug, Error, Diagnostic)]
                    #[error("The provided starting nodes relation is empty")]
                    #[diagnostic(code(algo::empty_starting))]
                    struct EmptyStarting(#[label] SourceSpan);

                    EmptyStarting(rel.span())
                })??;
                // INVARIANT(prim_start_col): `ensure_min_len(1)` proved a first column.
                let dv = &tuple.as_slice()[0];
                *inv_indices.get(dv).ok_or_else(|| {
                    #[derive(Debug, Error, Diagnostic)]
                    #[error("The requested starting node {0:?} is not found")]
                    #[diagnostic(code(algo::starting_node_not_found))]
                    struct StartingNodeNotFound(DataValue, #[label] SourceSpan);

                    StartingNodeNotFound(dv.clone(), rel.span())
                })?
            }
        };
        let msp = prim(&graph, starting, cancel)?;
        for (src, dst, cost) in msp {
            out.put(Tuple::from_vec(vec![
                indices[src as usize].clone(),
                indices[dst as usize].clone(),
                DataValue::from(cost as f64),
            ]))?;
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

fn prim(
    graph: &DirectedCsrGraph<f32>,
    starting: u32,
    cancel: CancelFlag,
) -> Result<Vec<(u32, u32, f32)>> {
    let mut visited = vec![false; graph.node_count() as usize];
    let mut mst_edges = Vec::with_capacity((graph.node_count() - 1) as usize);
    let mut pq = PriorityQueue::new();

    let mut relax_edges_at_node = |node: u32, pq: &mut PriorityQueue<_, _>| {
        visited[node as usize] = true;
        for target in graph.out_neighbors_with_values(node) {
            let to_node = target.target;
            let cost = target.value;
            if visited[to_node as usize] {
                continue;
            }
            pq.push_increase(to_node, (Reverse(OrderedFloat(cost)), node));
        }
    };

    relax_edges_at_node(starting, &mut pq);

    while let Some((to_node, (Reverse(OrderedFloat(cost)), from_node))) = pq.pop() {
        if mst_edges.len() == (graph.node_count() - 1) as usize {
            break;
        }
        mst_edges.push((from_node, to_node, cost));
        relax_edges_at_node(to_node, &mut pq);
        cancel.check()?;
    }

    Ok(mst_edges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::contract::tests_support::{TestInput, empty_opts, run_fixed_rule};
    use kyzo_model::value::Tuple;

    fn s(v: &str) -> DataValue {
        DataValue::from(v)
    }

    /// VALUE ORACLE: distinct weights make the MST unique AND Prim's
    /// output directions deterministic (each row is (tree node that
    /// claimed it, new node, weight)), so the rows are pinned exactly.
    ///
    /// Graph: a-b: 1, b-c: 2, a-c: 4, c-d: 3, start a.
    /// Hand trace: frontier from a = {b:1 via a, c:4 via a};
    ///   take b (1) ⇒ (a,b,1); b improves c to 2 via b;
    ///   take c (2) ⇒ (b,c,2); c offers d at 3;
    ///   take d (3) ⇒ (c,d,3).
    #[test]
    fn unique_mst_exact_rows() {
        let got = run_fixed_rule(
            &MinimumSpanningTreePrim,
            vec![
                TestInput::new(
                    vec!["fr", "to", "w"],
                    vec![
                        Tuple::from_vec(vec![s("a"), s("b"), DataValue::from(1.0)]),
                        Tuple::from_vec(vec![s("b"), s("c"), DataValue::from(2.0)]),
                        Tuple::from_vec(vec![s("a"), s("c"), DataValue::from(4.0)]),
                        Tuple::from_vec(vec![s("c"), s("d"), DataValue::from(3.0)]),
                    ],
                ),
                TestInput::new(vec!["start"], vec![Tuple::from_vec(vec![s("a")])]),
            ],
            empty_opts(),
            CancelFlag::default(),
        )
        .unwrap();
        let want: Vec<Tuple> = vec![
            Tuple::from_vec(vec![s("a"), s("b"), DataValue::from(1.0)]),
            Tuple::from_vec(vec![s("b"), s("c"), DataValue::from(2.0)]),
            Tuple::from_vec(vec![s("c"), s("d"), DataValue::from(3.0)]),
        ];
        assert_eq!(got, want);
    }

    /// TIE BEHAVIOR: equal weights on the chain a-b: 1, b-c: 1 from a.
    /// Only b is on the frontier first, then only c, so both edges and
    /// their directions are forced — the pinnable half of tie behavior.
    /// (Simultaneous equal-cost frontier edges from DIFFERENT tree nodes
    /// pop by the (cost, from-node) priority tuple — larger from-node id
    /// first; exact ties beyond that follow hash-seeded queue order and
    /// are deliberately not pinned.)
    #[test]
    fn equal_weight_chain_exact_rows() {
        let got = run_fixed_rule(
            &MinimumSpanningTreePrim,
            vec![
                TestInput::new(
                    vec!["fr", "to", "w"],
                    vec![
                        Tuple::from_vec(vec![s("a"), s("b"), DataValue::from(1.0)]),
                        Tuple::from_vec(vec![s("b"), s("c"), DataValue::from(1.0)]),
                    ],
                ),
                TestInput::new(vec!["start"], vec![Tuple::from_vec(vec![s("a")])]),
            ],
            empty_opts(),
            CancelFlag::default(),
        )
        .unwrap();
        let want: Vec<Tuple> = vec![
            Tuple::from_vec(vec![s("a"), s("b"), DataValue::from(1.0)]),
            Tuple::from_vec(vec![s("b"), s("c"), DataValue::from(1.0)]),
        ];
        assert_eq!(got, want);
    }
}
