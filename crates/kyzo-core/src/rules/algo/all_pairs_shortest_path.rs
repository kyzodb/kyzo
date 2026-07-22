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

use crate::rules::algo::dijkstra::dijkstra_keep_ties;
use crate::rules::contract::par_try_map;
use crate::rules::contract::{CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload};
use crate::rules::graph_view::DirectedCsrGraph;
use kyzo_model::SourceSpan;
use kyzo_model::program::rule::FixedRuleOptions;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::{DataValue, Tuple};

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
            par_try_map((0..n).collect(), |start| -> Result<BTreeMap<u32, f64>> {
                let res_for_start =
                    dijkstra_keep_ties(&graph, start, &(), &(), &(), cancel.clone())?;
                let mut ret: BTreeMap<u32, f64> = Default::default();
                let grouped = res_for_start.into_iter().chunk_by(|(n, _, _)| *n);
                for (_, grp) in grouped.into_iter() {
                    let grp = grp.collect_vec();
                    let l = f64::from(crate::rules::convert::u32_from_usize(grp.len())?);
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
        let mut centrality: Vec<f64> = vec![0.; crate::rules::convert::usize_from_u32(n)];
        for m in centrality_segs {
            for (k, v) in m {
                centrality[crate::rules::convert::usize_from_u32(k)] += v;
            }
        }

        for (i, s) in centrality.into_iter().enumerate() {
            let node = indices[i].clone();
            out.put(Tuple::from_vec(vec![node, (s).into()]))?;
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
        let res: Vec<_> = par_try_map((0..n).collect(), |start| -> Result<f64> {
            let distances = dijkstra_cost_only(&graph, start, cancel.clone())?;
            let total_dist: f64 = distances.iter().filter(|d| d.is_finite()).cloned().sum();
            let nc_usize = distances.iter().filter(|d| d.is_finite()).count();
            let nc = f64::from(crate::rules::convert::u32_from_usize(nc_usize)?);
            let denom = f64::from(n - 1);
            Ok(nc * nc / total_dist / denom)
        })?;
        for (idx, centrality) in res.into_iter().enumerate() {
            out.put(Tuple::from_vec(vec![
                indices[idx].clone(),
                DataValue::from(centrality),
            ]))?;
            cancel.check()?;
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

pub(crate) fn dijkstra_cost_only(
    edges: &DirectedCsrGraph<f64>,
    start: u32,
    cancel: CancelFlag,
) -> Result<Vec<f64>> {
    use std::cmp::Reverse;

    use ordered_float::OrderedFloat;
    use priority_queue::PriorityQueue;

    let mut distance = vec![f64::INFINITY; crate::rules::convert::usize_from_u32(edges.node_count())];
    let mut pq = PriorityQueue::new();
    distance[crate::rules::convert::usize_from_u32(start)] = 0.;
    pq.push(start, Reverse(OrderedFloat(0.)));

    // Cost-only Dijkstra: no predecessor table (P078 — no `u32::MAX` sentinel).
    while let Some((node, Reverse(OrderedFloat(cost)))) = pq.pop() {
        if cost > distance[crate::rules::convert::usize_from_u32(node)] {
            continue;
        }

        for target in edges.out_neighbors_with_values(node) {
            let nxt_node = target.target;
            let path_weight = target.value;

            let nxt_cost = cost + path_weight;
            if nxt_cost < distance[crate::rules::convert::usize_from_u32(nxt_node)] {
                pq.push_increase(nxt_node, Reverse(OrderedFloat(nxt_cost)));
                distance[crate::rules::convert::usize_from_u32(nxt_node)] = nxt_cost;
            }
        }
        cancel.check()?;
    }

    Ok(distance)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::contract::tests_support::{TestInput, opts_map, run_fixed_rule};
    use kyzo_model::SourceSpan;
    use kyzo_model::program::expr::Expr;
    use kyzo_model::value::Tuple;

    #[test]
    #[ignore = "timing-evidence rig; run explicitly with --ignored --nocapture to print APSP timings"]
    fn zz_timing_evidence() {
        let n = 400u32;
        let mut state = 0x0bad_c0de_dead_beefu64;
        let mut next = || {
            // INVARIANT(lcg64): Knuth LCG step is defined wrapping on u64.
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state
        };
        let mut rows: Vec<Tuple> = vec![];
        for _ in 0..6000 {
            let a = crate::rules::convert::u32_low(next() >> 33) % n;
            let b = crate::rules::convert::u32_low(next() >> 33) % n;
            let w = 1.0 + f64::from(crate::rules::convert::u32_low(next() >> 40) % 97);
            if a != b {
                rows.push(Tuple::from_vec(vec![
                    DataValue::from(format!("n{a}").as_str()),
                    DataValue::from(format!("n{b}").as_str()),
                    DataValue::from(w),
                ]));
            }
        }
        rows.push(Tuple::from_vec(vec![
            DataValue::from(format!("n{}", n - 1).as_str()),
            DataValue::from("n0"),
            DataValue::from(1.0),
        ]));
        let opt = || {
            opts_map(BTreeMap::from([(
                smartstring::SmartString::from("undirected"),
                Expr::Const {
                    val: DataValue::from(true),
                    span: SourceSpan::default(),
                },
            )]))
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
            vec![
                Tuple::from_vec(vec![s("a"), s("b")]),
                Tuple::from_vec(vec![s("b"), s("c")]),
            ],
        )
    }

    fn undirected_opt() -> FixedRuleOptions {
        opts_map(BTreeMap::from([(
            smartstring::SmartString::from("undirected"),
            Expr::Const {
                val: DataValue::from(true),
                span: SourceSpan::default(),
            },
        )]))
    }

    /// A deterministic pseudo-random weighted graph (LCG), large enough that
    /// the per-start Dijkstra map splits across rayon workers.
    fn pseudo_random_edges() -> TestInput {
        let n = 60u32;
        let mut state = 0x0bad_c0de_dead_beefu64;
        let mut next = || {
            // INVARIANT(lcg64): Knuth LCG step is defined wrapping on u64.
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state
        };
        let mut rows: Vec<Tuple> = vec![];
        for _ in 0..400 {
            let a = crate::rules::convert::u32_low(next() >> 33) % n;
            let b = crate::rules::convert::u32_low(next() >> 33) % n;
            let w = 1.0 + f64::from(crate::rules::convert::u32_low(next() >> 40) % 97);
            if a != b {
                rows.push(Tuple::from_vec(vec![
                    s(&format!("n{a}")),
                    s(&format!("n{b}")),
                    DataValue::from(w),
                ]));
            }
        }
        rows.push(Tuple::from_vec(vec![
            s(&format!("n{}", n - 1)),
            s("n0"),
            DataValue::from(1.0),
        ]));
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
            Tuple::from_vec(vec![s("a"), DataValue::from(1.5)]),
            Tuple::from_vec(vec![s("b"), DataValue::from(2.25)]),
            Tuple::from_vec(vec![s("c"), DataValue::from(1.5)]),
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
            Tuple::from_vec(vec![s("a"), DataValue::from(0.0)]),
            Tuple::from_vec(vec![s("b"), DataValue::from(2.0)]),
            Tuple::from_vec(vec![s("c"), DataValue::from(0.0)]),
        ];
        assert_eq!(got, want);
    }
}
