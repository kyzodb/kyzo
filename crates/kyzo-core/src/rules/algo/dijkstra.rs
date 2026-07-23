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
 * inline one in `fixed_rule/graph.rs`; the multi-start fan-out ran under
 * `rayon` (`into_par_iter`). SEAM(parallelism) closed: the per-start map
 * runs on `rayon` via `par_try_map` — each start's Dijkstra is
 * independent. The start list is the sorted BTreeSet and the map is
 * order-preserving, so rows land in the sequential order (which tied path
 * wins *within* a start is priority-queue pop order, already documented as
 * not pinnable and unaffected by this axis);
 * `SmallVec<[u32; 1]>` back-pointer lists are plain `Vec<u32>` (dropping
 * the `smallvec` dependency); the tie-path reconstruction `unwrap` is
 * annotated as structural; output rows flow through the arity-checked
 * writer. The `ForbiddenEdge`/`ForbiddenNode`/`Goal` capability traits are
 * unchanged. The original's `HeapState` struct was referenced nowhere in
 * the file or the workspace (dead code) and is dropped.
 * CANCELLATION FIX (deliberate, pinned vs upstream): the plain `dijkstra`
 * core took no cancel flag and never polled, so a `keep_ties=false` search
 * — and every Yen spur, which drives this core — was uninterruptible on a
 * large graph (the ratified budget/deadline design could not stop it). It
 * now takes a `CancelFlag` and polls once per node popped. `dijkstra_keep_ties`
 * polls at the same site — unconditional top-of-pop — not inside the out-edge
 * scan (a sink / all-forbidden hub never entered that scan). `check` only
 * reads the flag, so an unset flag leaves every result byte-identical; pinned
 * by `plain_dijkstra_honors_cancel` and
 * `keep_ties_honors_cancel_on_all_forbidden_hub` below.
 */

//! Dijkstra shortest paths from starting nodes to optional termination
//! sets, with optional tie-keeping; the search core is shared by Yen's
//! k-shortest-paths and the centralities.

use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::iter;

use itertools::Itertools;
use miette::Result;
use ordered_float::OrderedFloat;
use priority_queue::PriorityQueue;

use crate::rules::contract::par_try_map;
use crate::rules::contract::{
    CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload, GraphAlgorithmInvariantError,
    btree_set_only_element, path_predecessor,
};
use crate::rules::graph_view::DirectedCsrGraph;
use kyzo_model::SourceSpan;
use kyzo_model::program::rule::FixedRuleOptions;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::{DataValue, Tuple};

/// One Dijkstra/Yen output row: `(start, end, cost, path)` with dense ids
/// remapped through the algorithm's `indices` table.
pub(crate) fn weighted_path_out_row(
    indices: &[DataValue],
    start: u32,
    end: u32,
    cost: f64,
    path: impl IntoIterator<Item = u32>,
) -> Tuple {
    Tuple::from_vec(vec![
        indices[crate::rules::convert::usize_from_u32(start)].clone(),
        indices[crate::rules::convert::usize_from_u32(end)].clone(),
        DataValue::from(cost),
        DataValue::List(
            path.into_iter()
                .map(|u| indices[crate::rules::convert::usize_from_u32(u)].clone())
                .collect(),
        ),
    ])
}

#[cfg(test)]
use crate::rules::contract::{CancelAuthority, Cancelled};
#[cfg(test)]
use kyzo_model::program::expr::Expr;
#[cfg(test)]
use smartstring::SmartString;
#[cfg(test)]
use std::collections::BTreeMap;
pub(crate) struct ShortestPathDijkstra;

impl FixedRule for ShortestPathDijkstra {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let edges = payload.get_input(0)?;
        let starting = payload.get_input(1)?.ensure_min_len(1)?;
        let termination = payload.get_input(2).map(|t| t.ensure_min_len(1));
        let undirected = payload.bool_option("undirected", Some(false))?;
        let keep_ties = payload.bool_option("keep_ties", Some(false))?;

        let (graph, indices, inv_indices) = edges.as_directed_weighted_graph(undirected, false)?;

        let mut starting_nodes = BTreeSet::new();
        for tuple in starting.iter()? {
            let tuple = tuple?;
            // INVARIANT(dijkstra_start_col): `ensure_min_len(1)` proved a first column.
            let node = &tuple.as_slice()[0];
            if let Some(idx) = inv_indices.get(node) {
                starting_nodes.insert(*idx);
            }
        }
        let termination_nodes = match termination {
            Err(_) => None,
            Ok(t) => {
                let t = t?;
                let mut tn = BTreeSet::new();
                for tuple in t.iter()? {
                    let tuple = tuple?;
                    // INVARIANT(dijkstra_term_col): `ensure_min_len(1)` proved a first column.
                    let node = &tuple.as_slice()[0];
                    if let Some(idx) = inv_indices.get(node) {
                        tn.insert(*idx);
                    }
                }
                Some(tn)
            }
        };

        // The original forked here: a sequential path for a single start and
        // a rayon-parallel one for many. SEAM(parallelism) closed: the
        // per-start map runs on `rayon` via the order-preserving
        // `par_try_map` (start list = the sorted BTreeSet), so rows land in
        // the sequential order. `out.put` stays on this thread — the writer
        // is not shared.
        let starts: Vec<u32> = starting_nodes.into_iter().collect();
        let rows_per_start = par_try_map(starts, |start| -> Result<Vec<Tuple>> {
            let res = if let Some(tn) = &termination_nodes {
                if tn.len() == 1 {
                    // INVARIANT(single_goal): `tn.len() == 1` so the set has
                    // exactly one element.
                    let single = Some(btree_set_only_element(tn, "single_goal")?);
                    if keep_ties {
                        dijkstra_keep_ties(&graph, start, &single, &(), &(), cancel.clone())?
                    } else {
                        dijkstra(&graph, start, &single, &(), &(), cancel.clone())?
                    }
                } else if keep_ties {
                    dijkstra_keep_ties(&graph, start, tn, &(), &(), cancel.clone())?
                } else {
                    dijkstra(&graph, start, tn, &(), &(), cancel.clone())?
                }
            } else {
                dijkstra(&graph, start, &(), &(), &(), cancel.clone())?
            };
            Ok(res
                .into_iter()
                .map(|(target, cost, path)| {
                    weighted_path_out_row(&indices, start, target, cost, path)
                })
                .collect())
        })?;
        for rows in rows_per_start {
            for t in rows {
                out.put(t)?;
            }
        }

        Ok(())
    }

    fn arity(
        &self,
        _options: &FixedRuleOptions,
        _rule_head: &[Symbol],
        _span: SourceSpan,
    ) -> Result<usize> {
        Ok(4)
    }
}

pub(crate) trait ForbiddenEdge {
    fn is_forbidden(&self, src: u32, dst: u32) -> bool;
}

impl ForbiddenEdge for () {
    fn is_forbidden(&self, _src: u32, _dst: u32) -> bool {
        false
    }
}

impl ForbiddenEdge for BTreeSet<(u32, u32)> {
    fn is_forbidden(&self, src: u32, dst: u32) -> bool {
        self.contains(&(src, dst))
    }
}

pub(crate) trait ForbiddenNode {
    fn is_forbidden(&self, node: u32) -> bool;
}

impl ForbiddenNode for () {
    fn is_forbidden(&self, _node: u32) -> bool {
        false
    }
}

impl ForbiddenNode for BTreeSet<u32> {
    fn is_forbidden(&self, node: u32) -> bool {
        self.contains(&node)
    }
}

pub(crate) trait Goal {
    fn is_exhausted(&self) -> bool;
    /// Consuming visit: drain this goal of `node` and return the residual (P084).
    fn visit(self, node: u32) -> Self;
    fn iter(&self, total: u32) -> Box<dyn Iterator<Item = u32> + '_>;
}

impl Goal for () {
    fn is_exhausted(&self) -> bool {
        false
    }

    fn visit(self, _node: u32) -> Self {}

    fn iter(&self, total: u32) -> Box<dyn Iterator<Item = u32> + '_> {
        Box::new(0..total)
    }
}

impl Goal for Option<u32> {
    fn is_exhausted(&self) -> bool {
        self.is_none()
    }

    fn visit(self, node: u32) -> Self {
        match self {
            Some(u) if u == node => None,
            other => other,
        }
    }

    fn iter(&self, _total: u32) -> Box<dyn Iterator<Item = u32> + '_> {
        if let Some(u) = self {
            Box::new(iter::once(*u))
        } else {
            Box::new(iter::empty())
        }
    }
}

impl Goal for BTreeSet<u32> {
    fn is_exhausted(&self) -> bool {
        self.is_empty()
    }

    fn visit(mut self, node: u32) -> Self {
        self.remove(&node);
        self
    }

    fn iter(&self, _total: u32) -> Box<dyn Iterator<Item = u32> + '_> {
        Box::new(self.iter().cloned())
    }
}

pub(crate) fn dijkstra<FE: ForbiddenEdge, FN: ForbiddenNode, G: Goal + Clone>(
    edges: &DirectedCsrGraph<f64>,
    start: u32,
    goals: &G,
    forbidden_edges: &FE,
    forbidden_nodes: &FN,
    cancel: CancelFlag,
) -> Result<Vec<(u32, f64, Vec<u32>)>> {
    let graph_size = edges.node_count();
    let mut distance = vec![f64::INFINITY; crate::rules::convert::usize_from_u32(graph_size)];
    let mut pq = PriorityQueue::new();
    // Absence is `None` — never a reserved node id (P078).
    let mut back_pointers: Vec<Option<u32>> =
        vec![None; crate::rules::convert::usize_from_u32(graph_size)];
    distance[crate::rules::convert::usize_from_u32(start)] = 0.;
    pq.push(start, Reverse(OrderedFloat(0.)));
    let mut goals_remaining = goals.clone();

    while let Some((node, Reverse(OrderedFloat(cost)))) = pq.pop() {
        // Poll once per node popped — the unbounded work here is the pop
        // loop, so a raised flag refuses within one node's out-edge scan.
        // `check` only reads the flag, so an unset flag never changes the
        // result: the plain and Yen-spur searches stay byte-identical.
        cancel.check()?;
        if cost > distance[crate::rules::convert::usize_from_u32(node)] {
            continue;
        }

        for target in edges.out_neighbors_with_values(node) {
            let nxt_node = target.target;
            let path_weight = target.value;

            if forbidden_nodes.is_forbidden(nxt_node) {
                continue;
            }
            if forbidden_edges.is_forbidden(node, nxt_node) {
                continue;
            }
            let nxt_cost = cost + path_weight;
            if nxt_cost < distance[crate::rules::convert::usize_from_u32(nxt_node)] {
                pq.push_increase(nxt_node, Reverse(OrderedFloat(nxt_cost)));
                distance[crate::rules::convert::usize_from_u32(nxt_node)] = nxt_cost;
                back_pointers[crate::rules::convert::usize_from_u32(nxt_node)] = Some(node);
            }
        }

        goals_remaining = goals_remaining.visit(node);
        if goals_remaining.is_exhausted() {
            break;
        }
    }

    let mut results = Vec::new();
    for target in goals.iter(edges.node_count()) {
        let cost = distance[crate::rules::convert::usize_from_u32(target)];
        if !cost.is_finite() {
            results.push((target, cost, vec![]));
        } else {
            let mut path = vec![];
            let mut current = target;
            while current != start {
                path.push(current);
                current = path_predecessor(&back_pointers, current, "dijkstra_pred")?;
            }
            path.push(start);
            path.reverse();
            results.push((target, cost, path));
        }
    }
    Ok(results)
}

pub(crate) fn dijkstra_keep_ties<FE: ForbiddenEdge, FN: ForbiddenNode, G: Goal + Clone>(
    edges: &DirectedCsrGraph<f64>,
    start: u32,
    goals: &G,
    forbidden_edges: &FE,
    forbidden_nodes: &FN,
    cancel: CancelFlag,
) -> Result<Vec<(u32, f64, Vec<u32>)>> {
    let mut distance =
        vec![f64::INFINITY; crate::rules::convert::usize_from_u32(edges.node_count())];
    let mut pq = PriorityQueue::new();
    let mut back_pointers: Vec<Vec<u32>> =
        vec![vec![]; crate::rules::convert::usize_from_u32(edges.node_count())];
    distance[crate::rules::convert::usize_from_u32(start)] = 0.;
    pq.push(start, Reverse(OrderedFloat(0.)));
    let mut goals_remaining = goals.clone();

    while let Some((node, Reverse(OrderedFloat(cost)))) = pq.pop() {
        // Unconditional top-of-pop — same site as plain `dijkstra`. A hub
        // whose every out-edge is forbidden (or a sink) never enters the
        // scan body; polling only there left those pops uninterruptible.
        cancel.check()?;
        if cost > distance[crate::rules::convert::usize_from_u32(node)] {
            continue;
        }

        for target in edges.out_neighbors_with_values(node) {
            let nxt_node = target.target;
            let path_weight = target.value;

            if forbidden_nodes.is_forbidden(nxt_node) {
                continue;
            }
            if forbidden_edges.is_forbidden(node, nxt_node) {
                continue;
            }
            let nxt_cost = cost + path_weight;
            if nxt_cost < distance[crate::rules::convert::usize_from_u32(nxt_node)] {
                pq.push_increase(nxt_node, Reverse(OrderedFloat(nxt_cost)));
                distance[crate::rules::convert::usize_from_u32(nxt_node)] = nxt_cost;
                back_pointers[crate::rules::convert::usize_from_u32(nxt_node)].clear();
                back_pointers[crate::rules::convert::usize_from_u32(nxt_node)].push(node);
            } else if nxt_cost == distance[crate::rules::convert::usize_from_u32(nxt_node)] {
                pq.push_increase(nxt_node, Reverse(OrderedFloat(nxt_cost)));
                back_pointers[crate::rules::convert::usize_from_u32(nxt_node)].push(node);
            }
        }

        goals_remaining = goals_remaining.visit(node);
        if goals_remaining.is_exhausted() {
            break;
        }
    }

    let mut ret = Vec::new();
    for target in goals.iter(edges.node_count()) {
        let cost = distance[crate::rules::convert::usize_from_u32(target)];
        if !cost.is_finite() {
            ret.push((target, cost, vec![]));
        } else {
            struct CollectPath {
                collected: Vec<(u32, f64, Vec<u32>)>,
            }

            impl CollectPath {
                fn collect(
                    &mut self,
                    chain: &[u32],
                    start: u32,
                    target: u32,
                    cost: f64,
                    back_pointers: &[Vec<u32>],
                ) -> Result<()> {
                    let last = chain
                        .last()
                        .copied()
                        .ok_or_else(|| GraphAlgorithmInvariantError::refuse("keep_ties_chain"))?;
                    let prevs = &back_pointers[crate::rules::convert::usize_from_u32(last)];
                    for &nxt in prevs {
                        let mut ret = chain.to_vec();
                        ret.push(nxt);
                        if nxt == start {
                            ret.reverse();
                            self.collected.push((target, cost, ret));
                        } else {
                            self.collect(&ret, start, target, cost, back_pointers)?;
                        }
                    }
                    Ok(())
                }
            }
            let mut cp = CollectPath { collected: vec![] };
            cp.collect(&[target], start, target, cost, &back_pointers)?;
            ret.extend(cp.collected);
        }
    }

    Ok(ret)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::contract::tests_support::{
        TestInput, assert_parallel_matches_single_thread, empty_opts, opts_map, run_fixed_rule,
    };

    use miette::{Result, miette};
    fn s(v: &str) -> DataValue {
        DataValue::from(v)
    }

    fn e(a: &str, b: &str, w: f64) -> Tuple {
        Tuple::from_vec(vec![s(a), s(b), DataValue::from(w)])
    }

    /// A deterministic pseudo-random weighted graph plus a many-node start
    /// set, so the per-start Dijkstra map splits across rayon workers.
    fn pseudo_random_inputs() -> Vec<TestInput> {
        let n = 60u32;
        let mut state = 0xfeed_face_cafe_babeu64;
        let mut next = || {
            // INVARIANT(lcg64): Knuth LCG step is defined wrapping on u64.
            state = (std::num::Wrapping(state) * std::num::Wrapping(6364136223846793005)
                + std::num::Wrapping(1442695040888963407))
            .0;
            state
        };
        let mut edges: Vec<Tuple> = vec![];
        for _ in 0..400 {
            let a = crate::rules::convert::u32_low(next() >> 33) % n;
            let b = crate::rules::convert::u32_low(next() >> 33) % n;
            let w = 1.0 + f64::from(crate::rules::convert::u32_low(next() >> 40) % 97);
            if a != b {
                edges.push(e(&format!("n{a}"), &format!("n{b}"), w));
            }
        }
        edges.push(e(&format!("n{}", n - 1), "n0", 1.0));
        let starts: Vec<Tuple> = (0..n)
            .map(|i| Tuple::from_vec(vec![s(&format!("n{i}"))]))
            .collect();
        let ends: Vec<Tuple> = (0..n)
            .step_by(7)
            .map(|i| Tuple::from_vec(vec![s(&format!("n{i}"))]))
            .collect();
        vec![
            TestInput::new(vec!["fr", "to", "w"], edges),
            TestInput::new(vec!["start"], starts),
            TestInput::new(vec!["end"], ends),
        ]
    }

    /// DETERMINISM: the per-start Dijkstra map is byte-identical on a single-
    /// and multi-thread rayon pool, across repeated runs. Weights are
    /// distinct enough that shortest-path costs are unique, so no tie
    /// resolution (the one documented not-pinnable axis) is exercised.
    #[test]
    fn parallel_matches_single_thread() -> Result<()> {
        assert_parallel_matches_single_thread(|| {
            run_fixed_rule(
                &ShortestPathDijkstra,
                pseudo_random_inputs(),
                empty_opts(),
                CancelFlag::inert(),
            )
        })
    }

    /// End-to-end over the payload: the cheap two-hop route beats the
    /// expensive direct edge, and the output is (start, goal, cost, path).
    #[test]
    fn picks_cheaper_route() -> Result<()> {
        let got = run_fixed_rule(
            &ShortestPathDijkstra,
            vec![
                TestInput::new(
                    vec!["fr", "to", "w"],
                    vec![e("a", "b", 10.0), e("a", "c", 1.0), e("c", "b", 1.0)],
                ),
                TestInput::new(vec!["start"], vec![Tuple::from_vec(vec![s("a")])]),
                TestInput::new(vec!["end"], vec![Tuple::from_vec(vec![s("b")])]),
            ],
            empty_opts(),
            CancelFlag::inert(),
        )?;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0][2], DataValue::from(2.0));
        assert_eq!(got[0][3], DataValue::List(vec![s("a"), s("c"), s("b")]));
        Ok(())
    }

    /// The tie graph: an expensive direct edge plus two equally cheap
    /// two-hop routes.
    ///
    ///   a→d: 3 (direct decoy)
    ///   a→b: 1, b→d: 1   (cost 2)
    ///   a→c: 1, c→d: 1   (cost 2)
    ///
    /// Hand computation: dist(d) relaxes 3 (via a), then improves to
    /// 1 + 1 = 2 via b; c's relaxation ties at 2. The shortest cost is 2,
    /// never 3. A max-heap mutant pops d at cost 3 first (not stale:
    /// 3 == dist(d)), reaches the goal, and answers 3 — wrong.
    fn tie_graph() -> TestInput {
        TestInput::new(
            vec!["fr", "to", "w"],
            vec![
                e("a", "d", 3.0),
                e("a", "b", 1.0),
                e("b", "d", 1.0),
                e("a", "c", 1.0),
                e("c", "d", 1.0),
            ],
        )
    }

    /// VALUE ORACLE: `keep_ties: true` returns *every* cost-2 path — both
    /// [a,b,d] and [a,c,d], exactly, and not the cost-3 direct decoy.
    /// (Rows come back store-sorted; [a,b,d] < [a,c,d].)
    #[test]
    fn keep_ties_returns_all_tied_shortest_paths() -> Result<()> {
        let got = run_fixed_rule(
            &ShortestPathDijkstra,
            vec![
                tie_graph(),
                TestInput::new(vec!["start"], vec![Tuple::from_vec(vec![s("a")])]),
                TestInput::new(vec!["end"], vec![Tuple::from_vec(vec![s("d")])]),
            ],
            opts_map(BTreeMap::from([(
                SmartString::from("keep_ties"),
                Expr::Const {
                    val: DataValue::from(true),
                    span: SourceSpan::empty(),
                },
            )]))?,
            CancelFlag::inert(),
        )?;
        let want: Vec<Tuple> = vec![
            Tuple::from_vec(vec![
                s("a"),
                s("d"),
                DataValue::from(2.0),
                DataValue::List(vec![s("a"), s("b"), s("d")]),
            ]),
            Tuple::from_vec(vec![
                s("a"),
                s("d"),
                DataValue::from(2.0),
                DataValue::List(vec![s("a"), s("c"), s("d")]),
            ]),
        ];
        assert_eq!(got, want);
        Ok(())
    }

    /// VALUE ORACLE: `keep_ties` with MULTIPLE termination nodes (the
    /// `BTreeSet` goal branch): goals {b, d} on the tie graph.
    ///
    /// Hand computation: b is reached only directly, cost 1, path [a,b];
    /// d keeps both tied cost-2 routes as above. The search must not stop
    /// at the first goal it visits — b (cost 1) is visited before d's
    /// relaxations finish, and d's rows must still be complete.
    #[test]
    fn keep_ties_with_multiple_goals() -> Result<()> {
        let got = run_fixed_rule(
            &ShortestPathDijkstra,
            vec![
                tie_graph(),
                TestInput::new(vec!["start"], vec![Tuple::from_vec(vec![s("a")])]),
                TestInput::new(
                    vec!["end"],
                    vec![Tuple::from_vec(vec![s("b")]), Tuple::from_vec(vec![s("d")])],
                ),
            ],
            opts_map(BTreeMap::from([(
                SmartString::from("keep_ties"),
                Expr::Const {
                    val: DataValue::from(true),
                    span: SourceSpan::empty(),
                },
            )]))?,
            CancelFlag::inert(),
        )?;
        let want: Vec<Tuple> = vec![
            Tuple::from_vec(vec![
                s("a"),
                s("b"),
                DataValue::from(1.0),
                DataValue::List(vec![s("a"), s("b")]),
            ]),
            Tuple::from_vec(vec![
                s("a"),
                s("d"),
                DataValue::from(2.0),
                DataValue::List(vec![s("a"), s("b"), s("d")]),
            ]),
            Tuple::from_vec(vec![
                s("a"),
                s("d"),
                DataValue::from(2.0),
                DataValue::List(vec![s("a"), s("c"), s("d")]),
            ]),
        ];
        assert_eq!(got, want);
        Ok(())
    }

    /// VALUE ORACLE: without `keep_ties` the same graph yields exactly one
    /// row at cost 2. Which of the two tied routes wins depends on
    /// priority-queue pop order among equal costs (hash-seeded, not
    /// pinnable), so the path is asserted structurally: a → (b|c) → d.
    #[test]
    fn single_result_without_keep_ties() -> Result<()> {
        let got = run_fixed_rule(
            &ShortestPathDijkstra,
            vec![
                tie_graph(),
                TestInput::new(vec!["start"], vec![Tuple::from_vec(vec![s("a")])]),
                TestInput::new(vec!["end"], vec![Tuple::from_vec(vec![s("d")])]),
            ],
            empty_opts(),
            CancelFlag::inert(),
        )?;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0][2], DataValue::from(2.0));
        let path = got[0][3]
            .get_slice()
            .ok_or_else(|| miette!("test expected Some"))?;
        assert_eq!(path.len(), 3);
        assert_eq!(path[0], s("a"));
        assert!(path[1] == s("b") || path[1] == s("c"), "{:?}", path);
        assert_eq!(path[2], s("d"));
        Ok(())
    }

    /// CANCELLATION: the plain `dijkstra` core refuses once its flag is
    /// raised (a search that used to be uninterruptible), and an unset flag
    /// leaves the result identical to the always-default runs above — so the
    /// poll never changes an answer. Pins the fix that threaded the flag in.
    #[test]
    fn plain_dijkstra_honors_cancel() -> Result<()> {
        let graph = DirectedCsrGraph::from_edges([(0u32, 1u32, 1.0f64), (1, 2, 1.0), (2, 3, 1.0)])?;
        // Unset flag: the search completes, path 0→3 costs 3.
        let ok = dijkstra(&graph, 0, &Some(3u32), &(), &(), CancelFlag::inert())?;
        assert_eq!(ok, vec![(3, 3.0, vec![0, 1, 2, 3])]);
        // Spent authority: the very first pop refuses.
        let (auth, flag) = CancelAuthority::arm();
        let Cancelled = auth.cancel();
        assert!(dijkstra(&graph, 0, &Some(3u32), &(), &(), flag).is_err());
        Ok(())
    }

    /// CANCELLATION: pins unconditional top-of-pop in `dijkstra_keep_ties`.
    /// Hub 0's only out-edge is forbidden, so the out-edge scan body never
    /// reaches a mid-scan poll — under the bug a pre-raised flag still
    /// returns Ok (unreachable goal); under the fix the first pop refuses.
    #[test]
    fn keep_ties_honors_cancel_on_all_forbidden_hub() -> Result<()> {
        let graph = DirectedCsrGraph::from_edges([(0u32, 1u32, 1.0f64)])?;
        let forbidden: BTreeSet<(u32, u32)> = BTreeSet::from([(0, 1)]);
        // Unset flag: hub is a dead end; goal 1 stays unreachable.
        let ok = dijkstra_keep_ties(&graph, 0, &Some(1u32), &forbidden, &(), CancelFlag::inert())?;
        assert_eq!(ok.len(), 1);
        assert!(!ok[0].1.is_finite());
        assert!(ok[0].2.is_empty());
        // Spent authority: must refuse on the hub pop itself.
        let (auth, flag) = CancelAuthority::arm();
        let Cancelled = auth.cancel();
        assert!(
            dijkstra_keep_ties(&graph, 0, &Some(1u32), &forbidden, &(), flag).is_err(),
            "all-forbidden hub pop must poll cancel (Ok under mid-scan-only poll)"
        );
        Ok(())
    }
}
