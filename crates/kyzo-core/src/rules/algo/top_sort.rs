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

use miette::Result;

use crate::rules::contract::{
    CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload, graph_node_value,
};
use crate::rules::graph_view::DirectedCsrGraph;
use kyzo_model::SourceSpan;
use kyzo_model::program::rule::FixedRuleOptions;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::{DataValue, Tuple};

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
            // INVARIANT(top_sort_index): `kahn_g` only emits graph node ids,
            // and `indices` has an entry per node.
            let val = graph_node_value(&indices, *val_id)?.clone();
            let tuple = Tuple::from_vec(vec![
                DataValue::from(crate::rules::convert::i64_from_usize(idx)?),
                val.clone(),
            ]);
            out.put(tuple)?;
        }

        Ok(())
    }

    fn arity(
        &self,
        _options: &FixedRuleOptions,
        _rule_head: &[Symbol],
        _span: SourceSpan,
    ) -> Result<usize> {
        Ok(2)
    }
}

pub(crate) fn kahn_g(graph: &DirectedCsrGraph, cancel: CancelFlag) -> Result<Vec<u32>> {
    let graph_size = graph.node_count();
    let mut in_degree = vec![0; crate::rules::convert::usize_from_u32(graph_size)];
    for tos in 0..graph_size {
        for to in graph.out_neighbors(tos) {
            in_degree[crate::rules::convert::usize_from_u32(to)] += 1;
        }
    }
    let mut sorted = Vec::with_capacity(crate::rules::convert::usize_from_u32(graph_size));
    let mut pending = vec![];

    for (node, degree) in in_degree.iter().enumerate() {
        if *degree == 0 {
            pending.push(u32::try_from(node).map_err(|_| crate::rules::graph_view::GraphTooLargeError)?);
        }
    }

    while let Some(removed) = pending.pop() {
        sorted.push(removed);
        for nxt in graph.out_neighbors(removed) {
            in_degree[crate::rules::convert::usize_from_u32(nxt)] -= 1;
            if in_degree[crate::rules::convert::usize_from_u32(nxt)] == 0 {
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
    use crate::rules::contract::tests_support::{TestInput, empty_opts, run_fixed_rule};
    use kyzo_model::value::Tuple;

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
                    Tuple::from_vec(vec![s("a"), s("b")]),
                    Tuple::from_vec(vec![s("a"), s("c")]),
                    Tuple::from_vec(vec![s("b"), s("d")]),
                    Tuple::from_vec(vec![s("c"), s("d")]),
                ],
            )],
            empty_opts(),
            CancelFlag::inert(),
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
                    Tuple::from_vec(vec![s("a"), s("b")]),
                    Tuple::from_vec(vec![s("a"), s("c")]),
                    Tuple::from_vec(vec![s("b"), s("d")]),
                    Tuple::from_vec(vec![s("c"), s("d")]),
                ],
            )],
            empty_opts(),
            CancelFlag::inert(),
        )
        .unwrap();
        let i = |v: i64| DataValue::from(v);
        let want: Vec<Tuple> = vec![
            Tuple::from_vec(vec![i(0), s("a")]),
            Tuple::from_vec(vec![i(1), s("c")]),
            Tuple::from_vec(vec![i(2), s("b")]),
            Tuple::from_vec(vec![i(3), s("d")]),
        ];
        assert_eq!(got, want);
    }
}
