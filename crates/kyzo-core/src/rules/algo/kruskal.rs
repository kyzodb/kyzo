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
 * arity-checked writer. Union-find (path compression, union by size)
 * unchanged.
 * MULTIGRAPH FIX (deliberate, pinned vs upstream): the priority queue is
 * keyed by the endpoint pair `(from, to)`, so parallel edges between the
 * same pair collide on one key. Plain `pq.push` overwrites the priority,
 * leaving the LAST-seen parallel edge's weight — a bug on a multigraph,
 * where a minimum spanning forest must take the CHEAPEST parallel edge.
 * `pq.push_increase` keeps the greater priority, and priority is
 * `Reverse(cost)`, so the surviving weight is the minimum — the correct
 * choice (this is `prim.rs`'s in-file precedent). Pinned by
 * `parallel_edges_take_cheapest` below.
 */

//! Kruskal's minimum spanning forest over the (undirected, negative-weight
//! permitting) edge relation.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use itertools::Itertools;
use miette::Result;
use ordered_float::OrderedFloat;
use priority_queue::PriorityQueue;
use smartstring::{LazyCompact, SmartString};

use crate::rules::contract::{CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload};
use crate::rules::graph_view::DirectedCsrGraph;
use kyzo_model::SourceSpan;
use kyzo_model::program::expr::Expr;
use kyzo_model::program::rule::FixedRuleOptions;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::{DataValue, Tuple};

pub(crate) struct MinimumSpanningForestKruskal;

impl FixedRule for MinimumSpanningForestKruskal {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let edges = payload.get_input(0)?;
        let (graph, indices, _) = edges.as_directed_weighted_graph(true, true)?;
        if graph.node_count() == 0 {
            return Ok(());
        }
        let msp = kruskal(&graph, cancel)?;
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

fn kruskal(edges: &DirectedCsrGraph<f32>, cancel: CancelFlag) -> Result<Vec<(u32, u32, f32)>> {
    let mut pq = PriorityQueue::new();
    let mut uf = UnionFind::new(edges.node_count());
    let mut mst = Vec::with_capacity((edges.node_count() - 1) as usize);
    for from in 0..edges.node_count() {
        for target in edges.out_neighbors_with_values(from) {
            let to = target.target;
            let cost = target.value;
            // Multigraph: the pq key is the endpoint pair, so parallel
            // edges collide. `push_increase` keeps the greater priority
            // (`Reverse(cost)` ⇒ the smaller cost), i.e. the cheapest
            // parallel edge — a plain `push` would keep whichever was seen
            // last. See `prim.rs` for the same pattern.
            pq.push_increase((from, to), Reverse(OrderedFloat(cost)));
        }
    }
    while let Some(((from, to), Reverse(OrderedFloat(cost)))) = pq.pop() {
        if uf.connected(from, to) {
            continue;
        }
        uf.union(from, to);

        mst.push((from, to, cost));
        if uf.szs[0] == edges.node_count() {
            break;
        }
        cancel.check()?;
    }
    Ok(mst)
}

struct UnionFind {
    ids: Vec<u32>,
    szs: Vec<u32>,
}

impl UnionFind {
    fn new(n: u32) -> Self {
        Self {
            ids: (0..n).collect_vec(),
            szs: vec![1; n as usize],
        }
    }
    fn union(&mut self, p: u32, q: u32) {
        let root1 = self.find(p);
        let root2 = self.find(q);
        if root1 != root2 {
            if self.szs[root1 as usize] < self.szs[root2 as usize] {
                self.szs[root2 as usize] += self.szs[root1 as usize];
                self.ids[root1 as usize] = root2;
            } else {
                self.szs[root1 as usize] += self.szs[root2 as usize];
                self.ids[root2 as usize] = root1;
            }
        }
    }
    fn find(&mut self, mut p: u32) -> u32 {
        let mut root = p;
        while root != self.ids[root as usize] {
            root = self.ids[root as usize];
        }
        while p != root {
            let next = self.ids[p as usize];
            self.ids[p as usize] = root;
            p = next;
        }
        root
    }
    fn connected(&mut self, p: u32, q: u32) -> bool {
        self.find(p) == self.find(q)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::contract::tests_support::{TestInput, empty_opts, run_fixed_rule};
    use kyzo_model::value::Tuple;

    fn s(v: &str) -> DataValue {
        DataValue::from(v)
    }

    /// The edge relation is undirected (each input edge is doubled), so
    /// which direction of an MST edge pops first from the priority queue
    /// is a tie among equal priorities — hash-seeded, not pinnable.
    /// Normalize each row to (min endpoint, max endpoint, weight) and
    /// sort; the SET of tree edges and their weights are the semantics.
    fn normalized(rows: Vec<Tuple>) -> Vec<(DataValue, DataValue, DataValue)> {
        let mut out: Vec<_> = rows
            .into_iter()
            .map(|r| {
                let (a, b) = if r[0] <= r[1] {
                    (r[0].clone(), r[1].clone())
                } else {
                    (r[1].clone(), r[0].clone())
                };
                (a, b, r[2].clone())
            })
            .collect();
        out.sort();
        out
    }

    /// VALUE ORACLE: distinct weights make the MST unique, so the edge
    /// set is fully pinned.
    ///
    /// Graph: a-b: 1, b-c: 2, a-c: 4, c-d: 3.
    /// Hand computation (Kruskal takes edges cheapest-first, skipping
    /// cycle-closers): a-b (1) ✓, b-c (2) ✓, c-d (3) ✓, a-c (4) closes a
    /// cycle ⇒ tree {a-b:1, b-c:2, c-d:3}, total 6.
    #[test]
    fn unique_mst_edge_set() {
        let got = run_fixed_rule(
            &MinimumSpanningForestKruskal,
            vec![TestInput::new(
                vec!["fr", "to", "w"],
                vec![
                    Tuple::from_vec(vec![s("a"), s("b"), DataValue::from(1.0)]),
                    Tuple::from_vec(vec![s("b"), s("c"), DataValue::from(2.0)]),
                    Tuple::from_vec(vec![s("a"), s("c"), DataValue::from(4.0)]),
                    Tuple::from_vec(vec![s("c"), s("d"), DataValue::from(3.0)]),
                ],
            )],
            empty_opts(),
            CancelFlag::default(),
        )
        .unwrap();
        assert_eq!(
            normalized(got),
            vec![
                (s("a"), s("b"), DataValue::from(1.0)),
                (s("b"), s("c"), DataValue::from(2.0)),
                (s("c"), s("d"), DataValue::from(3.0)),
            ]
        );
    }

    /// MULTIGRAPH: two parallel a-b edges (weights 5 and 2) plus b-c: 3.
    /// The minimum spanning forest must take the CHEAPEST parallel edge, so
    /// the a-b tree edge weighs 2, not 5. `from_edges` keeps parallel edges
    /// in input order (stable sort), so the expensive edge is the one a
    /// plain `pq.push` would leave on the key — this test fails against that
    /// mutant and passes with `push_increase`.
    #[test]
    fn parallel_edges_take_cheapest() {
        let got = run_fixed_rule(
            &MinimumSpanningForestKruskal,
            vec![TestInput::new(
                vec!["fr", "to", "w"],
                vec![
                    Tuple::from_vec(vec![s("a"), s("b"), DataValue::from(2.0)]),
                    Tuple::from_vec(vec![s("a"), s("b"), DataValue::from(5.0)]),
                    Tuple::from_vec(vec![s("b"), s("c"), DataValue::from(3.0)]),
                ],
            )],
            empty_opts(),
            CancelFlag::default(),
        )
        .unwrap();
        assert_eq!(
            normalized(got),
            vec![
                (s("a"), s("b"), DataValue::from(2.0)),
                (s("b"), s("c"), DataValue::from(3.0)),
            ]
        );
    }

    /// TIE BEHAVIOR: equal weights on a path a-b: 1, b-c: 1 force both
    /// edges into the forest regardless of pop order among the ties — the
    /// pinnable half of tie behavior. (Where tied weights admit multiple
    /// equally-minimal trees, the choice follows priority-queue pop order
    /// among equal priorities, which is hash-seeded and deliberately not
    /// pinned.)
    #[test]
    fn equal_weight_path_keeps_all_edges() {
        let got = run_fixed_rule(
            &MinimumSpanningForestKruskal,
            vec![TestInput::new(
                vec!["fr", "to", "w"],
                vec![
                    Tuple::from_vec(vec![s("a"), s("b"), DataValue::from(1.0)]),
                    Tuple::from_vec(vec![s("b"), s("c"), DataValue::from(1.0)]),
                ],
            )],
            empty_opts(),
            CancelFlag::default(),
        )
        .unwrap();
        assert_eq!(
            normalized(got),
            vec![
                (s("a"), s("b"), DataValue::from(1.0)),
                (s("b"), s("c"), DataValue::from(1.0)),
            ]
        );
    }
}
