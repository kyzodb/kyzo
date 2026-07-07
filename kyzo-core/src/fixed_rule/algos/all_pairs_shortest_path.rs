/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): the per-start-node Dijkstra fan-out ran under `rayon`
 * (`into_par_iter`). SEAM(parallelism) closed: the per-start map runs on
 * `rayon` via `par_try_map` (each start's Dijkstra is independent and
 * reads only the shared CSR). Determinism is preserved because the map is
 * order-preserving AND the only cross-start float reduction — betweenness'
 * accumulation into `centrality` — is left as a sequential fold over the
 * ordered per-start segments, so the summation order is fixed. Closeness
 * has no cross-start reduction. `itertools`' `group_by` is now `chunk_by`;
 * output rows flow through the arity-checked writer.
 */

//! Closeness and betweenness centrality, both computed from all-pairs
//! shortest paths (one Dijkstra per node).

use std::collections::BTreeMap;

use itertools::Itertools;
use miette::Result;
use smartstring::{LazyCompact, SmartString};

use crate::data::expr::Expr;
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::fixed_rule::algos::shortest_path_dijkstra::dijkstra_keep_ties;
use crate::fixed_rule::graph::DirectedCsrGraph;
use crate::fixed_rule::parallel::par_try_map;
use crate::fixed_rule::{CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload};

pub(crate) struct BetweennessCentrality;

impl FixedRule for BetweennessCentrality {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let edges = payload.get_input(0)?;
        let undirected = payload.bool_option("undirected", Some(false))?;

        let (graph, indices, _inv_indices) = edges.as_directed_weighted_graph(undirected, false)?;

        let n = graph.node_count();
        if n == 0 {
            return Ok(());
        }

        // SEAM(parallelism) closed: each start's Dijkstra + accumulation into
        // its own `BTreeMap` is independent, so the map runs on `rayon` via
        // the order-preserving `par_try_map`. The cross-start reduction below
        // stays a sequential fold over the ordered segments, fixing the float
        // summation order — parallel and sequential runs are byte-identical.
        let centrality_segs: Vec<_> =
            par_try_map((0..n).collect(), |start| -> Result<BTreeMap<u32, f32>> {
                let res_for_start =
                    dijkstra_keep_ties(&graph, start, &(), &(), &(), cancel.clone())?;
                let mut ret: BTreeMap<u32, f32> = Default::default();
                let grouped = res_for_start.into_iter().chunk_by(|(n, _, _)| *n);
                for (_, grp) in grouped.into_iter() {
                    let grp = grp.collect_vec();
                    let l = grp.len() as f32;
                    for (_, _, path) in grp {
                        if path.len() < 3 {
                            continue;
                        }
                        for middle in path.iter().take(path.len() - 1).skip(1) {
                            let entry = ret.entry(*middle).or_default();
                            *entry += 1. / l;
                        }
                    }
                }
                Ok(ret)
            })?;
        let mut centrality: Vec<f32> = vec![0.; n as usize];
        for m in centrality_segs {
            for (k, v) in m {
                centrality[k as usize] += v;
            }
        }

        for (i, s) in centrality.into_iter().enumerate() {
            let node = indices[i].clone();
            out.put(vec![node, (s as f64).into()].into())?;
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

pub(crate) struct ClosenessCentrality;

impl FixedRule for ClosenessCentrality {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let edges = payload.get_input(0)?;
        let undirected = payload.bool_option("undirected", Some(false))?;

        let (graph, indices, _inv_indices) = edges.as_directed_weighted_graph(undirected, false)?;

        let n = graph.node_count();
        if n == 0 {
            return Ok(());
        }
        // SEAM(parallelism) closed: each start's closeness is an independent
        // scalar (its `total_dist` sum is over that one start's distances,
        // computed sequentially inside the closure), so the per-start map
        // runs on `rayon` via the order-preserving `par_try_map`. There is no
        // cross-start reduction, so the output is byte-identical to the
        // sequential map.
        let res: Vec<_> = par_try_map((0..n).collect(), |start| -> Result<f32> {
            let distances = dijkstra_cost_only(&graph, start, cancel.clone())?;
            let total_dist: f32 = distances.iter().filter(|d| d.is_finite()).cloned().sum();
            let nc: f32 = distances.iter().filter(|d| d.is_finite()).count() as f32;
            Ok(nc * nc / total_dist / (n - 1) as f32)
        })?;
        for (idx, centrality) in res.into_iter().enumerate() {
            out.put(vec![indices[idx].clone(), DataValue::from(centrality as f64)].into())?;
            cancel.check()?;
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

pub(crate) fn dijkstra_cost_only(
    edges: &DirectedCsrGraph<f32>,
    start: u32,
    cancel: CancelFlag,
) -> Result<Vec<f32>> {
    use std::cmp::Reverse;

    use ordered_float::OrderedFloat;
    use priority_queue::PriorityQueue;

    let mut distance = vec![f32::INFINITY; edges.node_count() as usize];
    let mut pq = PriorityQueue::new();
    let mut back_pointers = vec![u32::MAX; edges.node_count() as usize];
    distance[start as usize] = 0.;
    pq.push(start, Reverse(OrderedFloat(0.)));

    while let Some((node, Reverse(OrderedFloat(cost)))) = pq.pop() {
        if cost > distance[node as usize] {
            continue;
        }

        for target in edges.out_neighbors_with_values(node) {
            let nxt_node = target.target;
            let path_weight = target.value;

            let nxt_cost = cost + path_weight;
            if nxt_cost < distance[nxt_node as usize] {
                pq.push_increase(nxt_node, Reverse(OrderedFloat(nxt_cost)));
                distance[nxt_node as usize] = nxt_cost;
                back_pointers[nxt_node as usize] = node;
            }
        }
        cancel.check()?;
    }

    Ok(distance)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::expr::Expr;
    use crate::data::span::SourceSpan;
    use crate::data::value::Tuple;
    use crate::fixed_rule::tests_support::{TestInput, run_fixed_rule};

    #[test]
    #[ignore]
    fn zz_timing_evidence() {
        let n = 400u32;
        let mut state = 0x0bad_c0de_dead_beefu64;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state
        };
        let mut rows: Vec<Tuple> = vec![];
        for _ in 0..6000 {
            let a = (next() >> 33) as u32 % n;
            let b = (next() >> 33) as u32 % n;
            let w = 1.0 + ((next() >> 40) as u32 % 97) as f64;
            if a != b {
                rows.push(
                    vec![
                        DataValue::from(format!("n{a}").as_str()),
                        DataValue::from(format!("n{b}").as_str()),
                        DataValue::from(w),
                    ]
                    .into(),
                );
            }
        }
        rows.push(
            vec![
                DataValue::from(format!("n{}", n - 1).as_str()),
                DataValue::from("n0"),
                DataValue::from(1.0),
            ]
            .into(),
        );
        let opt = || {
            BTreeMap::from([(
                smartstring::SmartString::from("undirected"),
                Expr::Const {
                    val: DataValue::from(true),
                    span: SourceSpan::default(),
                },
            )])
        };
        let run = || {
            run_fixed_rule(
                &BetweennessCentrality,
                vec![TestInput::new(vec!["fr", "to", "w"], rows.clone())],
                opt(),
                CancelFlag::default(),
            )
            .unwrap()
        };
        let single = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let t0 = std::time::Instant::now();
        let seq = single.install(run);
        let seq_t = t0.elapsed();
        let t1 = std::time::Instant::now();
        let par = run();
        let par_t = t1.elapsed();
        assert_eq!(seq, par);
        let threads = rayon::current_num_threads();
        eprintln!(
            "betweenness n={n}: 1-thread {seq_t:?}, default({threads} threads) {par_t:?}, speedup {:.2}x",
            seq_t.as_secs_f64() / par_t.as_secs_f64()
        );
    }

    fn s(v: &str) -> DataValue {
        DataValue::from(v)
    }

    fn path_graph() -> TestInput {
        // The undirected path a—b—c, unit weights.
        TestInput::new(
            vec!["fr", "to"],
            vec![vec![s("a"), s("b")].into(), vec![s("b"), s("c")].into()],
        )
    }

    fn undirected_opt() -> BTreeMap<smartstring::SmartString<smartstring::LazyCompact>, Expr> {
        BTreeMap::from([(
            smartstring::SmartString::from("undirected"),
            Expr::Const {
                val: DataValue::from(true),
                span: SourceSpan::default(),
            },
        )])
    }

    /// A deterministic pseudo-random weighted graph (LCG), large enough that
    /// the per-start Dijkstra map splits across rayon workers.
    fn pseudo_random_edges() -> TestInput {
        let n = 60u32;
        let mut state = 0x0bad_c0de_dead_beefu64;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state
        };
        let mut rows: Vec<Tuple> = vec![];
        for _ in 0..400 {
            let a = (next() >> 33) as u32 % n;
            let b = (next() >> 33) as u32 % n;
            let w = 1.0 + ((next() >> 40) as u32 % 97) as f64;
            if a != b {
                rows.push(
                    vec![s(&format!("n{a}")), s(&format!("n{b}")), DataValue::from(w)].into(),
                );
            }
        }
        rows.push(vec![s(&format!("n{}", n - 1)), s("n0"), DataValue::from(1.0)].into());
        TestInput::new(vec!["fr", "to", "w"], rows)
    }

    /// DETERMINISM: betweenness (whose only cross-start reduction is the
    /// sequential fold into `centrality`) is byte-identical on a single- and
    /// multi-thread rayon pool, across repeated runs. This is the site where
    /// a parallel float reduction would bite; the fold is kept sequential, so
    /// it does not.
    #[test]
    fn betweenness_parallel_matches_single_thread() {
        let single = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let seq = single.install(|| {
            run_fixed_rule(
                &BetweennessCentrality,
                vec![pseudo_random_edges()],
                undirected_opt(),
                CancelFlag::default(),
            )
            .unwrap()
        });
        for _ in 0..8 {
            let par = run_fixed_rule(
                &BetweennessCentrality,
                vec![pseudo_random_edges()],
                undirected_opt(),
                CancelFlag::default(),
            )
            .unwrap();
            assert_eq!(seq, par);
        }
    }

    /// DETERMINISM: closeness (independent per-start scalars, no cross-start
    /// reduction) is byte-identical on a single- and multi-thread pool.
    #[test]
    fn closeness_parallel_matches_single_thread() {
        let single = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let seq = single.install(|| {
            run_fixed_rule(
                &ClosenessCentrality,
                vec![pseudo_random_edges()],
                undirected_opt(),
                CancelFlag::default(),
            )
            .unwrap()
        });
        for _ in 0..8 {
            let par = run_fixed_rule(
                &ClosenessCentrality,
                vec![pseudo_random_edges()],
                undirected_opt(),
                CancelFlag::default(),
            )
            .unwrap();
            assert_eq!(seq, par);
        }
    }

    /// VALUE ORACLE for closeness as implemented: nc²/Σd/(n−1) over the
    /// finite distances (nc counts reachable nodes including self).
    ///
    /// Hand computation on a—b—c (n = 3):
    ///   a: distances (0,1,2) ⇒ 3²/3/2 = 1.5
    ///   b: distances (1,0,1) ⇒ 3²/2/2 = 2.25
    ///   c: symmetric to a    ⇒ 1.5
    /// (All exact in f32, so the f64 rows compare exactly.)
    #[test]
    fn closeness_on_path_graph() {
        let got = run_fixed_rule(
            &ClosenessCentrality,
            vec![path_graph()],
            undirected_opt(),
            CancelFlag::default(),
        )
        .unwrap();
        let want: Vec<Tuple> = vec![
            vec![s("a"), DataValue::from(1.5)].into(),
            vec![s("b"), DataValue::from(2.25)].into(),
            vec![s("c"), DataValue::from(1.5)].into(),
        ];
        assert_eq!(got, want);
    }

    /// VALUE ORACLE for betweenness as implemented (unnormalized, over
    /// all ordered pairs): on a—b—c only the length-3 paths a→c = [a,b,c]
    /// and c→a = [c,b,a] have an interior node, each contributing 1 to b
    /// (one tied path per pair, so the 1/ties factor is 1).
    ///   ⇒ a: 0, b: 2, c: 0.
    #[test]
    fn betweenness_on_path_graph() {
        let got = run_fixed_rule(
            &BetweennessCentrality,
            vec![path_graph()],
            undirected_opt(),
            CancelFlag::default(),
        )
        .unwrap();
        let want: Vec<Tuple> = vec![
            vec![s("a"), DataValue::from(0.0)].into(),
            vec![s("b"), DataValue::from(2.0)].into(),
            vec![s("c"), DataValue::from(0.0)].into(),
        ];
        assert_eq!(got, want);
    }
}
