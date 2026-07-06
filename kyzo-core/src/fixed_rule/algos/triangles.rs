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
 * inline one in `fixed_rule/graph.rs`; the per-node triangle count ran
 * under `rayon` when that feature was on. SEAM(parallelism) closed: the
 * per-node map runs on `rayon` via `par_try_map` — each node reads only
 * the shared CSR, `n_triangles` is an order-independent integer sum, and
 * `cc` is a pure function of per-node integers, so the result is
 * byte-identical to the sequential map. Output rows flow through the
 * arity-checked writer.
 */

//! Clustering coefficients: per node, the triangle count over the
//! (undirected) neighborhood and the resulting coefficient.

use std::collections::BTreeMap;

use itertools::Itertools;
use miette::Result;
use smartstring::{LazyCompact, SmartString};

use crate::data::expr::Expr;
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::fixed_rule::graph::DirectedCsrGraph;
use crate::fixed_rule::parallel::par_try_map;
use crate::fixed_rule::{CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload};

pub(crate) struct ClusteringCoefficients;

impl FixedRule for ClusteringCoefficients {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let edges = payload.get_input(0)?;
        let (graph, indices, _) = edges.as_directed_graph(true)?;
        let coefficients = clustering_coefficients(&graph, cancel)?;
        for (idx, (cc, n_triangles, degree)) in coefficients.into_iter().enumerate() {
            out.put(
                vec![
                    indices[idx].clone(),
                    DataValue::from(cc),
                    DataValue::from(n_triangles as i64),
                    DataValue::from(degree as i64),
                ]
                .into(),
            )?;
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

fn clustering_coefficients(
    graph: &DirectedCsrGraph,
    cancel: CancelFlag,
) -> Result<Vec<(f64, usize, usize)>> {
    let node_size = graph.node_count();

    // SEAM(parallelism) closed: the per-node map is order-preserving through
    // `par_try_map`, so parallel and sequential runs are byte-identical.
    // `cancel.check()` is polled once per (degree ≥ 2) node, unchanged from
    // the sequential body.
    par_try_map(
        (0..node_size).collect(),
        |node_idx| -> Result<(f64, usize, usize)> {
            let edges = graph.out_neighbors(node_idx).collect_vec();
            let degree = edges.len();
            if degree < 2 {
                Ok((0., 0, degree))
            } else {
                let n_triangles = edges
                    .iter()
                    .map(|e_src| {
                        edges
                            .iter()
                            .filter(|e_dst| {
                                if e_src <= e_dst {
                                    return false;
                                }
                                for nb in graph.out_neighbors(*e_src) {
                                    if nb == **e_dst {
                                        return true;
                                    }
                                }
                                false
                            })
                            .count()
                    })
                    .sum();
                let cc = 2. * n_triangles as f64 / ((degree as f64) * ((degree as f64) - 1.));
                cancel.check()?;
                Ok((cc, n_triangles, degree))
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::tuple::Tuple;
    use crate::fixed_rule::tests_support::{TestInput, run_fixed_rule};

    fn s(v: &str) -> DataValue {
        DataValue::from(v)
    }

    /// A dense-ish deterministic pseudo-random directed graph (LCG), large
    /// enough that the per-node map splits across rayon workers.
    fn pseudo_random_graph() -> DirectedCsrGraph {
        let n = 200u32;
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) as u32) % n
        };
        let mut edges = vec![];
        for _ in 0..3000 {
            let (a, b) = (next(), next());
            if a != b {
                edges.push((a, b, ()));
            }
        }
        edges.push((n - 1, 0, ())); // pin the node count at n
        DirectedCsrGraph::from_edges(edges).unwrap()
    }

    /// DETERMINISM: the per-node clustering-coefficient map is byte-identical
    /// on a single-thread rayon pool and the default (multi-thread) pool,
    /// across repeated runs. `clustering_coefficients` returns an ordered
    /// `Vec`, so this pins both value AND order.
    #[test]
    fn parallel_matches_single_thread() {
        let graph = pseudo_random_graph();
        let single = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let seq =
            single.install(|| clustering_coefficients(&graph, CancelFlag::default()).unwrap());
        for _ in 0..8 {
            let par = clustering_coefficients(&graph, CancelFlag::default()).unwrap();
            assert_eq!(seq, par);
        }
    }

    /// VALUE ORACLE: exact triangle counts and coefficients on the known
    /// graph of the triangle {a,b,c} plus d attached to a and b — two
    /// triangles total, (a,b,c) and (a,b,d).
    ///
    /// Hand computation (undirected degrees / per-node triangle counts):
    ///   a: neighbors {b,c,d}, deg 3; pairs closed: (b,c) ✓, (b,d) ✓,
    ///      (c,d) ✗ ⇒ 2 triangles, cc = 2·2/(3·2) = 2/3
    ///   b: symmetric to a ⇒ 2 triangles, cc = 2/3
    ///   c: neighbors {a,b}, deg 2; (a,b) ✓ ⇒ 1 triangle, cc = 2·1/(2·1) = 1
    ///   d: neighbors {a,b}, deg 2 ⇒ 1 triangle, cc = 1
    #[test]
    fn counts_triangles_on_known_graph() {
        let i = |v: i64| DataValue::from(v);
        let got = run_fixed_rule(
            &ClusteringCoefficients,
            vec![TestInput::new(
                vec!["fr", "to"],
                vec![
                    vec![s("a"), s("b")].into(),
                    vec![s("a"), s("c")].into(),
                    vec![s("b"), s("c")].into(),
                    vec![s("a"), s("d")].into(),
                    vec![s("b"), s("d")].into(),
                ],
            )],
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap();
        let two_thirds = DataValue::from(2.0 * 2.0 / (3.0 * 2.0));
        let want: Vec<Tuple> = vec![
            vec![s("a"), two_thirds.clone(), i(2), i(3)].into(),
            vec![s("b"), two_thirds, i(2), i(3)].into(),
            vec![s("c"), DataValue::from(1.0), i(1), i(2)].into(),
            vec![s("d"), DataValue::from(1.0), i(1), i(2)].into(),
        ];
        assert_eq!(got, want);
    }
}
