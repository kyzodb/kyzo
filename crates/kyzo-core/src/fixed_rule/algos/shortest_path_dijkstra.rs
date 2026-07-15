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
 * now takes a `CancelFlag` and polls once per node popped, the same
 * granularity as `dijkstra_cost_only` and `dijkstra_keep_ties`. `check`
 * only reads the flag, so an unset flag leaves every result byte-identical;
 * pinned by `plain_dijkstra_honors_cancel` below.
 */

//! Dijkstra shortest paths from starting nodes to optional termination
//! sets, with optional tie-keeping; the search core is shared by Yen's
//! k-shortest-paths and the centralities.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::iter;

use itertools::Itertools;
use miette::Result;
use ordered_float::OrderedFloat;
use priority_queue::PriorityQueue;
use smartstring::{LazyCompact, SmartString};

use crate::data::expr::Expr;
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::data::value::Tuple;
use crate::fixed_rule::graph::DirectedCsrGraph;
use crate::fixed_rule::parallel::par_try_map;
use crate::fixed_rule::{CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload};

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
            // Structural: `ensure_min_len(1)` proved every tuple has a
            // first column.
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
                    // Structural: `ensure_min_len(1)` proved every tuple
                    // has a first column.
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
                    // Structural: `tn.len() == 1`.
                    let single = Some(*tn.iter().next().unwrap());
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
                    Tuple::from_vec(vec![
                        indices[start as usize].clone(),
                        indices[target as usize].clone(),
                        DataValue::from(cost as f64),
                        DataValue::List(
                            path.into_iter()
                                .map(|u| indices[u as usize].clone())
                                .collect_vec(),
                        ),
                    ])
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
        _options: &BTreeMap<SmartString<LazyCompact>, Expr>,
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
    fn visit(&mut self, node: u32);
    fn iter(&self, total: u32) -> Box<dyn Iterator<Item = u32> + '_>;
}

impl Goal for () {
    fn is_exhausted(&self) -> bool {
        false
    }

    fn visit(&mut self, _node: u32) {}

    fn iter(&self, total: u32) -> Box<dyn Iterator<Item = u32> + '_> {
        Box::new(0..total)
    }
}

impl Goal for Option<u32> {
    fn is_exhausted(&self) -> bool {
        self.is_none()
    }

    fn visit(&mut self, node: u32) {
        if let Some(u) = &self
            && *u == node
        {
            self.take();
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

    fn visit(&mut self, node: u32) {
        self.remove(&node);
    }

    fn iter(&self, _total: u32) -> Box<dyn Iterator<Item = u32> + '_> {
        Box::new(self.iter().cloned())
    }
}

pub(crate) fn dijkstra<FE: ForbiddenEdge, FN: ForbiddenNode, G: Goal + Clone>(
    edges: &DirectedCsrGraph<f32>,
    start: u32,
    goals: &G,
    forbidden_edges: &FE,
    forbidden_nodes: &FN,
    cancel: CancelFlag,
) -> Result<Vec<(u32, f32, Vec<u32>)>> {
    let graph_size = edges.node_count();
    let mut distance = vec![f32::INFINITY; graph_size as usize];
    let mut pq = PriorityQueue::new();
    let mut back_pointers = vec![u32::MAX; graph_size as usize];
    distance[start as usize] = 0.;
    pq.push(start, Reverse(OrderedFloat(0.)));
    let mut goals_remaining = goals.clone();

    while let Some((node, Reverse(OrderedFloat(cost)))) = pq.pop() {
        // Poll once per node popped — the unbounded work here is the pop
        // loop, so a raised flag refuses within one node's out-edge scan.
        // `check` only reads the flag, so an unset flag never changes the
        // result: the plain and Yen-spur searches stay byte-identical.
        cancel.check()?;
        if cost > distance[node as usize] {
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
            if nxt_cost < distance[nxt_node as usize] {
                pq.push_increase(nxt_node, Reverse(OrderedFloat(nxt_cost)));
                distance[nxt_node as usize] = nxt_cost;
                back_pointers[nxt_node as usize] = node;
            }
        }

        goals_remaining.visit(node);
        if goals_remaining.is_exhausted() {
            break;
        }
    }

    Ok(goals
        .iter(edges.node_count())
        .map(|target| {
            let cost = distance[target as usize];
            if !cost.is_finite() {
                (target, cost, vec![])
            } else {
                let mut path = vec![];
                let mut current = target;
                while current != start {
                    path.push(current);
                    current = back_pointers[current as usize];
                }
                path.push(start);
                path.reverse();
                (target, cost, path)
            }
        })
        .collect_vec())
}

pub(crate) fn dijkstra_keep_ties<FE: ForbiddenEdge, FN: ForbiddenNode, G: Goal + Clone>(
    edges: &DirectedCsrGraph<f32>,
    start: u32,
    goals: &G,
    forbidden_edges: &FE,
    forbidden_nodes: &FN,
    cancel: CancelFlag,
) -> Result<Vec<(u32, f32, Vec<u32>)>> {
    let mut distance = vec![f32::INFINITY; edges.node_count() as usize];
    let mut pq = PriorityQueue::new();
    let mut back_pointers: Vec<Vec<u32>> = vec![vec![]; edges.node_count() as usize];
    distance[start as usize] = 0.;
    pq.push(start, Reverse(OrderedFloat(0.)));
    let mut goals_remaining = goals.clone();

    while let Some((node, Reverse(OrderedFloat(cost)))) = pq.pop() {
        if cost > distance[node as usize] {
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
            if nxt_cost < distance[nxt_node as usize] {
                pq.push_increase(nxt_node, Reverse(OrderedFloat(nxt_cost)));
                distance[nxt_node as usize] = nxt_cost;
                back_pointers[nxt_node as usize].clear();
                back_pointers[nxt_node as usize].push(node);
            } else if nxt_cost == distance[nxt_node as usize] {
                pq.push_increase(nxt_node, Reverse(OrderedFloat(nxt_cost)));
                back_pointers[nxt_node as usize].push(node);
            }
            cancel.check()?;
        }

        goals_remaining.visit(node);
        if goals_remaining.is_exhausted() {
            break;
        }
    }

    let ret = goals
        .iter(edges.node_count())
        .flat_map(|target| {
            let cost = distance[target as usize];
            if !cost.is_finite() {
                vec![(target, cost, vec![])]
            } else {
                struct CollectPath {
                    collected: Vec<(u32, f32, Vec<u32>)>,
                }

                impl CollectPath {
                    fn collect(
                        &mut self,
                        chain: &[u32],
                        start: u32,
                        target: u32,
                        cost: f32,
                        back_pointers: &[Vec<u32>],
                    ) {
                        // Structural: `chain` starts as `[target]` and only
                        // grows.
                        let last = chain.last().unwrap();
                        let prevs = &back_pointers[*last as usize];
                        for nxt in prevs {
                            let mut ret = chain.to_vec();
                            ret.push(*nxt);
                            if *nxt == start {
                                ret.reverse();
                                self.collected.push((target, cost, ret));
                            } else {
                                self.collect(&ret, start, target, cost, back_pointers)
                            }
                        }
                    }
                }
                let mut cp = CollectPath { collected: vec![] };
                cp.collect(&[target], start, target, cost, &back_pointers);
                cp.collected
            }
        })
        .collect_vec();

    Ok(ret)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixed_rule::tests_support::{TestInput, run_fixed_rule};

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
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state
        };
        let mut edges: Vec<Tuple> = vec![];
        for _ in 0..400 {
            let a = (next() >> 33) as u32 % n;
            let b = (next() >> 33) as u32 % n;
            let w = 1.0 + ((next() >> 40) as u32 % 97) as f64;
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
    fn parallel_matches_single_thread() {
        let single = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let seq = single.install(|| {
            run_fixed_rule(
                &ShortestPathDijkstra,
                pseudo_random_inputs(),
                BTreeMap::new(),
                CancelFlag::default(),
            )
            .unwrap()
        });
        for _ in 0..8 {
            let par = run_fixed_rule(
                &ShortestPathDijkstra,
                pseudo_random_inputs(),
                BTreeMap::new(),
                CancelFlag::default(),
            )
            .unwrap();
            assert_eq!(seq, par);
        }
    }

    /// End-to-end over the payload: the cheap two-hop route beats the
    /// expensive direct edge, and the output is (start, goal, cost, path).
    #[test]
    fn picks_cheaper_route() {
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
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0][2], DataValue::from(2.0));
        assert_eq!(got[0][3], DataValue::List(vec![s("a"), s("c"), s("b")]));
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
    fn keep_ties_returns_all_tied_shortest_paths() {
        let got = run_fixed_rule(
            &ShortestPathDijkstra,
            vec![
                tie_graph(),
                TestInput::new(vec!["start"], vec![Tuple::from_vec(vec![s("a")])]),
                TestInput::new(vec!["end"], vec![Tuple::from_vec(vec![s("d")])]),
            ],
            BTreeMap::from([(
                SmartString::from("keep_ties"),
                Expr::Const {
                    val: DataValue::from(true),
                    span: SourceSpan::default(),
                },
            )]),
            CancelFlag::default(),
        )
        .unwrap();
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
    }

    /// VALUE ORACLE: `keep_ties` with MULTIPLE termination nodes (the
    /// `BTreeSet` goal branch): goals {b, d} on the tie graph.
    ///
    /// Hand computation: b is reached only directly, cost 1, path [a,b];
    /// d keeps both tied cost-2 routes as above. The search must not stop
    /// at the first goal it visits — b (cost 1) is visited before d's
    /// relaxations finish, and d's rows must still be complete.
    #[test]
    fn keep_ties_with_multiple_goals() {
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
            BTreeMap::from([(
                SmartString::from("keep_ties"),
                Expr::Const {
                    val: DataValue::from(true),
                    span: SourceSpan::default(),
                },
            )]),
            CancelFlag::default(),
        )
        .unwrap();
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
    }

    /// VALUE ORACLE: without `keep_ties` the same graph yields exactly one
    /// row at cost 2. Which of the two tied routes wins depends on
    /// priority-queue pop order among equal costs (hash-seeded, not
    /// pinnable), so the path is asserted structurally: a → (b|c) → d.
    #[test]
    fn single_result_without_keep_ties() {
        let got = run_fixed_rule(
            &ShortestPathDijkstra,
            vec![
                tie_graph(),
                TestInput::new(vec!["start"], vec![Tuple::from_vec(vec![s("a")])]),
                TestInput::new(vec!["end"], vec![Tuple::from_vec(vec![s("d")])]),
            ],
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0][2], DataValue::from(2.0));
        let path = got[0][3].get_slice().unwrap();
        assert_eq!(path.len(), 3);
        assert_eq!(path[0], s("a"));
        assert!(path[1] == s("b") || path[1] == s("c"), "{:?}", path);
        assert_eq!(path[2], s("d"));
    }

    /// CANCELLATION: the plain `dijkstra` core refuses once its flag is
    /// raised (a search that used to be uninterruptible), and an unset flag
    /// leaves the result identical to the always-default runs above — so the
    /// poll never changes an answer. Pins the fix that threaded the flag in.
    #[test]
    fn plain_dijkstra_honors_cancel() {
        let graph =
            DirectedCsrGraph::from_edges([(0u32, 1u32, 1.0f32), (1, 2, 1.0), (2, 3, 1.0)]).unwrap();
        // Unset flag: the search completes, path 0→3 costs 3.
        let ok = dijkstra(&graph, 0, &Some(3u32), &(), &(), CancelFlag::default()).unwrap();
        assert_eq!(ok, vec![(3, 3.0, vec![0, 1, 2, 3])]);
        // Raised flag: the very first pop refuses.
        let flag = CancelFlag::default();
        flag.cancel();
        assert!(dijkstra(&graph, 0, &Some(3u32), &(), &(), flag).is_err());
    }
}
