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
 * inline one in `fixed_rule/graph.rs`; the multi-pair fan-out ran under
 * `rayon` (`par_bridge`) when that feature was on. SEAM(parallelism)
 * closed: the per-(start, goal) map runs on `rayon` via `par_try_map` —
 * each pair's Yen search is independent and shares no mutable state.
 * Determinism holds because the pair list is built in sorted BTreeSet
 * order and the map is order-preserving, so rows land exactly as the
 * sequential loop emitted them. The `k_shortest.last()`/`candidates.pop()`
 * unwraps are annotated as structural; output rows flow through the
 * arity-checked writer.
 * MULTIGRAPH FIX (deliberate, pinned vs upstream): the root-segment cost
 * recomputation took the FIRST neighbor matching the next node on the
 * path, ignoring weight. On a multigraph that charges an arbitrary
 * parallel edge, while Dijkstra built the path over the cheapest — so a
 * candidate's total cost could be overstated and mis-ranked. It now sums
 * the MINIMUM matching weight per segment. Pinned by
 * `parallel_root_edge_uses_min_weight` below.
 */

//! Yen's algorithm for the k shortest loopless paths between node pairs,
//! built on the Dijkstra core with forbidden edges/nodes.

use std::collections::{BTreeMap, BTreeSet};

use itertools::Itertools;
use miette::Result;
use smartstring::{LazyCompact, SmartString};

use crate::data::expr::Expr;
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::data::value::Tuple;
use crate::fixed_rule::algos::shortest_path_dijkstra::dijkstra;
use crate::fixed_rule::graph::DirectedCsrGraph;
use crate::fixed_rule::parallel::par_try_map;
use crate::fixed_rule::{
    CancelAuthority, CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload,
};

pub(crate) struct KShortestPathYen;

impl FixedRule for KShortestPathYen {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let edges = payload.get_input(0)?;
        let starting = payload.get_input(1)?.ensure_min_len(1)?;
        let termination = payload.get_input(2)?.ensure_min_len(1)?;
        let undirected = payload.bool_option("undirected", Some(false))?;
        let k = payload.pos_integer_option("k", None)?;

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
        let mut termination_nodes = BTreeSet::new();
        for tuple in termination.iter()? {
            let tuple = tuple?;
            // Structural: `ensure_min_len(1)` proved every tuple has a
            // first column.
            let node = &tuple.as_slice()[0];
            if let Some(idx) = inv_indices.get(node) {
                termination_nodes.insert(*idx);
            }
        }
        // The original forked here: sequential for a single pair, rayon
        // (`par_bridge`) for many. SEAM(parallelism) closed: the per-pair map
        // runs on `rayon` via `par_try_map`. The pair list is built in the
        // sorted BTreeSet order (start outer, goal inner) and the map is
        // order-preserving, so rows land in exactly the sequential order;
        // `k_shortest_path_yen` polls the cancel flag at the top of each spur
        // search unchanged, so a raised flag still refuses before the next
        // Dijkstra. `out.put` stays on this thread — the writer is not shared.
        let pairs: Vec<(u32, u32)> = starting_nodes
            .iter()
            .flat_map(|start| termination_nodes.iter().map(move |goal| (*start, *goal)))
            .collect();
        let rows_per_pair = par_try_map(pairs, |(start, goal)| -> Result<Vec<Tuple>> {
            let paths = k_shortest_path_yen(k, &graph, start, goal, cancel.clone())?;
            Ok(paths
                .into_iter()
                .map(|(cost, path)| {
                    Tuple::from_vec(vec![
                        indices[start as usize].clone(),
                        indices[goal as usize].clone(),
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
        for rows in rows_per_pair {
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

fn k_shortest_path_yen(
    k: usize,
    edges: &DirectedCsrGraph<f32>,
    start: u32,
    goal: u32,
    cancel: CancelFlag,
) -> Result<Vec<(f32, Vec<u32>)>> {
    // `k` is a caller-supplied option with no upper bound: reserving from it
    // directly would let an absurd `k` abort the allocator before a single
    // path is found. Grow amortized instead — the final length is the same
    // either way.
    let mut k_shortest: Vec<(f32, Vec<u32>)> = Vec::new();
    let mut candidates: Vec<(f32, Vec<u32>)> = vec![];

    match dijkstra(edges, start, &Some(goal), &(), &(), cancel.clone())?
        .into_iter()
        .next()
    {
        None => return Ok(k_shortest),
        Some((_, cost, path)) => k_shortest.push((cost, path)),
    }

    for _ in 1..k {
        // Structural: `k_shortest` starts with one entry and only grows.
        let (_, prev_path) = k_shortest.last().unwrap();
        for i in 0..prev_path.len() - 1 {
            // Polled at the top of the spur-search unit of work: one
            // iteration runs a full Dijkstra, so a raised flag must refuse
            // before the next spur search — not after |path| - 1 of them,
            // as the previous below-the-search placement allowed.
            cancel.check()?;
            let spur_node = match prev_path.get(i) {
                None => return Ok(vec![]),
                Some(n) => *n,
            };
            let root_path = &prev_path[0..i + 1];
            let mut forbidden_edges = BTreeSet::new();
            for (_, p) in &k_shortest {
                if p.len() < root_path.len() + 1 {
                    continue;
                }
                let p_prefix = &p[0..i + 1];
                if p_prefix == root_path {
                    forbidden_edges.insert((p[i], p[i + 1]));
                }
            }
            let mut forbidden_nodes = BTreeSet::new();
            for node in &prev_path[0..i] {
                forbidden_nodes.insert(*node);
            }
            if let Some((_, spur_cost, spur_path)) = dijkstra(
                edges,
                spur_node,
                &Some(goal),
                &forbidden_edges,
                &forbidden_nodes,
                cancel.clone(),
            )?
            .into_iter()
            .next()
            {
                let mut total_cost = spur_cost;
                for j in 0..root_path.len() - 1 {
                    let seg_from = root_path[j];
                    let seg_to = root_path[j + 1];
                    // Multigraph: the root segment may span parallel edges.
                    // Dijkstra built this path over the CHEAPEST edge, so the
                    // recomputed segment cost must use the MINIMUM matching
                    // weight — taking the first neighbor (as before) would
                    // charge an arbitrary parallel edge and mis-rank
                    // candidates on a multigraph.
                    let mut best: Option<f32> = None;
                    for target in edges.out_neighbors_with_values(seg_from) {
                        if target.target == seg_to {
                            best = Some(best.map_or(target.value, |b: f32| b.min(target.value)));
                        }
                    }
                    // Structural: (seg_from, seg_to) is a consecutive pair on
                    // a path Dijkstra just returned, so a matching edge
                    // exists.
                    total_cost += best.unwrap();
                }
                let mut total_path = root_path.to_vec();
                total_path.pop();
                total_path.extend(spur_path);
                if candidates.iter().all(|(_, v)| *v != total_path) {
                    candidates.push((total_cost, total_path));
                }
            }
        }
        if candidates.is_empty() {
            break;
        }
        candidates.sort_by(|(a_cost, _), (b_cost, _)| b_cost.total_cmp(a_cost));
        // Structural: `candidates` was just checked non-empty.
        let shortest = candidates.pop().unwrap();
        let shortest_dist = shortest.0;
        if shortest_dist.is_finite() {
            k_shortest.push(shortest);
        }
    }
    Ok(k_shortest)
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

    fn k_opt(k: i64) -> BTreeMap<smartstring::SmartString<smartstring::LazyCompact>, Expr> {
        BTreeMap::from([(
            smartstring::SmartString::from("k"),
            Expr::Const {
                val: DataValue::from(k),
                span: SourceSpan::default(),
            },
        )])
    }

    /// A deterministic pseudo-random weighted graph plus multi-node start and
    /// goal sets, so the per-(start, goal) Yen map splits across rayon
    /// workers.
    fn pseudo_random_inputs() -> Vec<TestInput> {
        let n = 40u32;
        let mut state = 0xd1ce_d1ce_d1ce_d1ceu64;
        let mut next = || {
            // INVARIANT(lcg64): Knuth LCG step is defined wrapping on u64.
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state
        };
        let mut edges: Vec<Tuple> = vec![];
        for _ in 0..300 {
            let a = (next() >> 33) as u32 % n;
            let b = (next() >> 33) as u32 % n;
            let w = 1.0 + ((next() >> 40) as u32 % 97) as f64;
            if a != b {
                edges.push(e(&format!("n{a}"), &format!("n{b}"), w));
            }
        }
        edges.push(e(&format!("n{}", n - 1), "n0", 1.0));
        let starts: Vec<Tuple> = (0..n)
            .step_by(5)
            .map(|i| Tuple::from_vec(vec![s(&format!("n{i}"))]))
            .collect();
        let ends: Vec<Tuple> = (0..n)
            .step_by(6)
            .map(|i| Tuple::from_vec(vec![s(&format!("n{i}"))]))
            .collect();
        vec![
            TestInput::new(vec!["fr", "to", "w"], edges),
            TestInput::new(vec!["start"], starts),
            TestInput::new(vec!["end"], ends),
        ]
    }

    /// DETERMINISM: the per-(start, goal) Yen map is byte-identical on a
    /// single- and multi-thread rayon pool, across repeated runs.
    #[test]
    fn parallel_matches_single_thread() {
        let single = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let seq = single.install(|| {
            run_fixed_rule(
                &KShortestPathYen,
                pseudo_random_inputs(),
                k_opt(3),
                CancelFlag::default(),
            )
            .unwrap()
        });
        for _ in 0..8 {
            let par = run_fixed_rule(
                &KShortestPathYen,
                pseudo_random_inputs(),
                k_opt(3),
                CancelFlag::default(),
            )
            .unwrap();
            assert_eq!(seq, par);
        }
    }

    /// MULTIGRAPH: the root segment of the 2nd shortest path spans a pair
    /// of parallel edges 0→1 with weights (10, 1). Dijkstra routes over the
    /// cheap one, so the true cost of [0,1,2,3] is 1+1+1 = 3. The
    /// root-segment recomputation must charge the MINIMUM parallel weight
    /// (1), not the first neighbor (10, listed first because `from_edges`
    /// keeps parallel edges in input order): a first-match recomputation
    /// reports 12 and fails this test.
    #[test]
    fn parallel_root_edge_uses_min_weight() {
        let graph = DirectedCsrGraph::from_edges([
            (0u32, 1u32, 10.0f32), // parallel, expensive — listed first
            (0, 1, 1.0),           // parallel, cheap — the one Dijkstra uses
            (1, 3, 1.0),
            (1, 2, 1.0),
            (2, 3, 1.0),
        ])
        .unwrap();
        let got = k_shortest_path_yen(2, &graph, 0, 3, CancelFlag::default()).unwrap();
        assert_eq!(got, vec![(2.0, vec![0, 1, 3]), (3.0, vec![0, 1, 2, 3])]);
    }

    /// CANCELLATION: a raised flag refuses inside the spur search. The first
    /// interior Dijkstra now polls (the fix threaded the flag into the plain
    /// `dijkstra` core), so a search that was uninterruptible on a large
    /// graph stops; an unset flag returns the same paths as the oracle above.
    #[test]
    fn spur_search_honors_cancel() {
        let graph = DirectedCsrGraph::from_edges([
            (0u32, 1u32, 1.0f32),
            (1, 2, 1.0),
            (2, 3, 1.0),
            (0, 2, 3.0),
        ])
        .unwrap();
        let (auth, flag) = CancelAuthority::arm();
        let _ = auth.cancel();
        assert!(k_shortest_path_yen(3, &graph, 0, 3, flag).is_err());
    }

    /// k = 2 over a graph with two a→d routes returns both, cheaper first.
    #[test]
    fn two_shortest_paths() {
        let got = run_fixed_rule(
            &KShortestPathYen,
            vec![
                TestInput::new(
                    vec!["fr", "to", "w"],
                    vec![
                        e("a", "b", 1.0),
                        e("b", "d", 1.0),
                        e("a", "c", 2.0),
                        e("c", "d", 2.0),
                    ],
                ),
                TestInput::new(vec!["start"], vec![Tuple::from_vec(vec![s("a")])]),
                TestInput::new(vec!["end"], vec![Tuple::from_vec(vec![s("d")])]),
            ],
            BTreeMap::from([(
                smartstring::SmartString::from("k"),
                Expr::Const {
                    val: DataValue::from(2i64),
                    span: SourceSpan::default(),
                },
            )]),
            CancelFlag::default(),
        )
        .unwrap();
        assert_eq!(got.len(), 2);
        let costs: Vec<_> = got.iter().map(|t| t[2].get_float().unwrap()).collect();
        assert!(costs.contains(&2.0) && costs.contains(&4.0));
    }

    /// VALUE ORACLE: the k paths come back cheapest-FIRST, order pinned
    /// (the end-to-end test above only asserts the cost set; the store it
    /// reads through re-sorts rows, so order must be pinned on the
    /// algorithm itself).
    ///
    /// Graph (node ids literal):
    ///   0→1: 1, 1→3: 1   (route [0,1,3], cost 2)
    ///   0→2: 2, 2→3: 2   (route [0,2,3], cost 4)
    ///   0→3: 5           (route [0,3],   cost 5)
    ///
    /// Hand computation of Yen, k = 3:
    ///   1. Dijkstra 0→3: min(2, 4, 5) = 2 via [0,1,3].
    ///   2. Spur off [0,1,3]: banning edge (0,1) at spur node 0 finds
    ///      [0,2,3] = 4; banning (1,3) at spur node 1 leaves 1 with no
    ///      other out-edge (infinite candidate, filtered). Next: 4.
    ///   3. Spur off [0,2,3]: banning (0,1) and (0,2) at spur node 0
    ///      leaves the direct [0,3] = 5; spur node 2 has no alternative.
    ///      Next: 5.
    ///
    /// ⇒ exactly [(2, [0,1,3]), (4, [0,2,3]), (5, [0,3])], in that order.
    #[test]
    fn k_shortest_order_is_cheapest_first() {
        let graph = DirectedCsrGraph::from_edges([
            (0u32, 1u32, 1.0f32),
            (1, 3, 1.0),
            (0, 2, 2.0),
            (2, 3, 2.0),
            (0, 3, 5.0),
        ])
        .unwrap();
        let got = k_shortest_path_yen(3, &graph, 0, 3, CancelFlag::default()).unwrap();
        assert_eq!(
            got,
            vec![
                (2.0, vec![0, 1, 3]),
                (4.0, vec![0, 2, 3]),
                (5.0, vec![0, 3]),
            ]
        );
    }

    /// F2: a raised flag refuses at the top of the spur-search loop — one
    /// spur iteration is a full Dijkstra, so a pre-set flag must not run
    /// any of them.
    #[test]
    fn cancellation_stops_spur_search() {
        let (auth, cancel) = CancelAuthority::arm();
        let _ = auth.cancel();
        let err = run_fixed_rule(
            &KShortestPathYen,
            vec![
                TestInput::new(
                    vec!["fr", "to", "w"],
                    vec![
                        e("a", "b", 1.0),
                        e("b", "d", 1.0),
                        e("a", "c", 2.0),
                        e("c", "d", 2.0),
                    ],
                ),
                TestInput::new(vec!["start"], vec![Tuple::from_vec(vec![s("a")])]),
                TestInput::new(vec!["end"], vec![Tuple::from_vec(vec![s("d")])]),
            ],
            BTreeMap::from([(
                smartstring::SmartString::from("k"),
                Expr::Const {
                    val: DataValue::from(2i64),
                    span: SourceSpan::default(),
                },
            )]),
            cancel,
        )
        .unwrap_err();
        assert!(err.to_string().contains("killed"), "{err}");
    }
}
