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
 * arity-checked writer. Note, preserved from the original: on a cyclic
 * graph Kahn's algorithm silently omits the nodes of the cycles from the
 * output rather than erroring.
 */

//! Topological sort (Kahn's algorithm) of the edge relation.

use std::collections::BTreeMap;

use miette::Result;
use smartstring::{LazyCompact, SmartString};

use crate::data::expr::Expr;
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::fixed_rule::graph::DirectedCsrGraph;
use crate::fixed_rule::{CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload};

pub(crate) struct TopSort;

impl FixedRule for TopSort {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let edges = payload.get_input(0)?;

        let (graph, indices, _) = edges.as_directed_graph(false)?;

        let sorted = kahn_g(&graph, cancel)?;

        for (idx, val_id) in sorted.iter().enumerate() {
            // Structural: `kahn_g` only emits ids of the graph's nodes,
            // and `indices` has an entry per node.
            let val = indices.get(*val_id as usize).unwrap();
            let tuple = vec![DataValue::from(idx as i64), val.clone()];
            out.put(tuple)?;
        }

        Ok(())
    }

    fn arity(
        &self,
        _options: &BTreeMap<SmartString<LazyCompact>, Expr>,
        _rule_head: &[Symbol],
        _span: SourceSpan,
    ) -> Result<usize> {
        Ok(2)
    }
}

pub(crate) fn kahn_g(graph: &DirectedCsrGraph, cancel: CancelFlag) -> Result<Vec<u32>> {
    let graph_size = graph.node_count();
    let mut in_degree = vec![0; graph_size as usize];
    for tos in 0..graph_size {
        for to in graph.out_neighbors(tos) {
            in_degree[to as usize] += 1;
        }
    }
    let mut sorted = Vec::with_capacity(graph_size as usize);
    let mut pending = vec![];

    for (node, degree) in in_degree.iter().enumerate() {
        if *degree == 0 {
            pending.push(node as u32);
        }
    }

    while let Some(removed) = pending.pop() {
        sorted.push(removed);
        for nxt in graph.out_neighbors(removed) {
            in_degree[nxt as usize] -= 1;
            if in_degree[nxt as usize] == 0 {
                pending.push(nxt);
            }
        }
        cancel.check()?;
    }

    Ok(sorted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixed_rule::tests_support::{TestInput, run_fixed_rule};

    fn s(v: &str) -> DataValue {
        DataValue::from(v)
    }

    /// A diamond a→{b,c}→d sorts with a first and d last.
    #[test]
    fn sorts_a_diamond() {
        let got = run_fixed_rule(
            &TopSort,
            vec![TestInput::new(
                vec!["fr", "to"],
                vec![
                    vec![s("a"), s("b")],
                    vec![s("a"), s("c")],
                    vec![s("b"), s("d")],
                    vec![s("c"), s("d")],
                ],
            )],
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap();
        assert_eq!(got.len(), 4);
        let pos_of = |name: &str| -> i64 {
            got.iter().find(|t| t[1] == s(name)).unwrap()[0]
                .get_int()
                .unwrap()
        };
        assert_eq!(pos_of("a"), 0);
        assert_eq!(pos_of("d"), 3);
    }

    /// VALUE ORACLE: the exact order on the diamond, pinned modulo the
    /// documented tie rule. Kahn's here uses a LIFO stack of ready nodes,
    /// and a freed node's successors are pushed in sorted-adjacency
    /// order — so among nodes that become ready together, the LAST in
    /// sorted neighbor order pops first.
    ///
    /// Hand trace (a→b, a→c, b→d, c→d; interning: a=0, b=1, c=2, d=3):
    ///   ready {a}: pop a; free b, c (pushed b then c)
    ///   pop c (top of stack); d still has in-degree 1
    ///   pop b; d freed
    ///   pop d
    /// ⇒ a, c, b, d — reversing the adjacency segments would push c
    /// before b and give a, b, c, d instead, so this kills the
    /// reversed-CSR-sort mutant.
    #[test]
    fn exact_order_with_documented_tie_rule() {
        let got = run_fixed_rule(
            &TopSort,
            vec![TestInput::new(
                vec!["fr", "to"],
                vec![
                    vec![s("a"), s("b")],
                    vec![s("a"), s("c")],
                    vec![s("b"), s("d")],
                    vec![s("c"), s("d")],
                ],
            )],
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap();
        let i = |v: i64| DataValue::from(v);
        assert_eq!(
            got,
            vec![
                vec![i(0), s("a")],
                vec![i(1), s("c")],
                vec![i(2), s("b")],
                vec![i(3), s("d")],
            ]
        );
    }
}
