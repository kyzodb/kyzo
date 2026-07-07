/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): the original delegated to the `graph` crate's `page_rank`
 * (rayon-parallel, chunk-racing over a shared score array —
 * nondeterministic in the low-order float bits — and never polling the
 * poison). `page_rank` below is KyzoDB's own port of that routine, same
 * damped power iteration and same termination rule (sum of absolute score
 * changes below `tolerance`, or `iterations` reached), polling the cancel
 * flag per node.
 *
 * SEAM(parallelism) — CLOSED, with a DELIBERATE, PINNED SEMANTIC CHANGE.
 * The interim port ran a sequential Gauss-Seidel sweep (scores updated in
 * place, so a node read its predecessors' fresh values within the same
 * iteration) and refused parallelism because parallelizing that sweep would
 * reorder both the reads and a cross-node float reduction. This version
 * changes the iteration scheme to two-buffer JACOBI: every node's new score
 * reads only the PREVIOUS iteration's buffer (`prev`), so nodes are
 * independent within an iteration and the per-node update parallelizes
 * byte-deterministically through the order-preserving `par_try_map`. The
 * two order-dependent float computations are kept sequential and in a fixed
 * order: each node's in-neighbor sum is folded by a single worker in CSR
 * (ascending-source) order, and the per-iteration `Σ|Δ|` is folded by the
 * caller over the canonically ordered result `Vec` in node-index order —
 * never handed to a parallel reduction. The result is therefore identical
 * at any thread count and across runs.
 *
 * This is a deliberate divergence, pre-authorized by the maintainer, on two
 * fronts: (a) from upstream's `graph`-crate `page_rank`, whose chunk-raced
 * shared array is nondeterministic above ~16384 nodes; and (b) from this
 * fork's interim Gauss-Seidel port. Jacobi and Gauss-Seidel are the same
 * math family with the same fixpoint and the same termination rule, but
 * their iterates differ at a fixed iteration count, so the numbers change.
 * They agree once converged (pinned by a test). The original also carried a
 * dead `#[cfg(not(feature = "rayon"))]` fallback that referenced `nalgebra`
 * types it never imported (it could not have compiled); dropped.
 */

//! PageRank: damped power iteration over the (unweighted) edge graph.

use std::collections::BTreeMap;

use miette::Result;
use smartstring::{LazyCompact, SmartString};

use crate::data::expr::Expr;
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::fixed_rule::graph::DirectedCsrGraph;
use crate::fixed_rule::parallel::par_try_map;
use crate::fixed_rule::{CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload};

pub(crate) struct PageRank;

impl FixedRule for PageRank {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let edges = payload.get_input(0)?;
        let undirected = payload.bool_option("undirected", Some(false))?;
        let theta = payload.unit_interval_option("theta", Some(0.85))? as f32;
        let epsilon = payload.unit_interval_option("epsilon", Some(0.0001))? as f32;
        let iterations = payload.pos_integer_option("iterations", Some(10))?;

        let (graph, indices, _) = edges.as_directed_graph(undirected)?;

        if indices.is_empty() {
            return Ok(());
        }

        let ranks = page_rank(&graph, theta, epsilon as f64, iterations, cancel)?;

        for (idx, score) in ranks.iter().enumerate() {
            out.put(vec![indices[idx].clone(), DataValue::from(*score as f64)].into())?;
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

/// Damped power iteration, two-buffer Jacobi: scores start uniform at `1/n`;
/// each iteration recomputes every node's score as
/// `(1 - damping)/n + damping * Σ out_share(v)` over its in-neighbors `v`,
/// where `out_share(v)` is `v`'s score **from the previous iteration**
/// divided by its out-degree; iteration stops when the summed absolute
/// change drops below `tolerance` or after `max_iterations`.
///
/// Determinism at any thread count is structural: within an iteration every
/// node's new score is a pure function of the read-only `prev` buffer and
/// the shared CSR, so the per-node map is parallelized through the
/// order-preserving [`par_try_map`]. The two order-dependent float
/// computations stay sequential in a fixed order — each in-neighbor sum is
/// folded in CSR (ascending-source) order by the one worker owning that
/// node, and the per-iteration `Σ|Δ|` is folded here over the returned,
/// node-index-ordered `Vec`, never by a parallel reduction.
fn page_rank(
    graph: &DirectedCsrGraph,
    damping_factor: f32,
    tolerance: f64,
    max_iterations: usize,
    cancel: CancelFlag,
) -> Result<Vec<f32>> {
    let node_count = graph.node_count() as usize;
    let init_score = 1_f32 / node_count as f32;
    let base_score = (1.0_f32 - damping_factor) / node_count as f32;

    let mut prev = vec![init_score; node_count];

    for _ in 0..max_iterations {
        // Each node's contribution to each of its out-neighbors, from the
        // PREVIOUS iteration's scores — the Jacobi read. Computed once, up
        // front, so every node in this iteration sees the same frozen
        // `prev`. A sink node (out-degree 0) divides by zero into `inf`
        // here, exactly as the original did — the value is never read,
        // because a sink is nobody's in-neighbor.
        let out_scores: Vec<f32> = (0..node_count)
            .map(|node| prev[node] / graph.out_degree(node as u32) as f32)
            .collect();

        // Per-node Jacobi update, parallelized order-preservingly. Each node
        // reads only `out_scores`/`prev` (the frozen previous iteration) and
        // the shared CSR; nodes are independent within the iteration. The
        // in-neighbor sum is a sequential f32 fold in fixed CSR order, so the
        // whole map is byte-identical at any thread count. `cancel.check()`
        // is polled once per node.
        let updated = par_try_map(
            (0..node_count).collect::<Vec<_>>(),
            |u| -> Result<(f32, f64)> {
                let incoming_total = graph
                    .in_neighbors(u as u32)
                    .map(|v| out_scores[v as usize])
                    .sum::<f32>();
                let new_score = base_score + damping_factor * incoming_total;
                let delta = f64::abs((new_score - prev[u]) as f64);
                cancel.check()?;
                Ok((new_score, delta))
            },
        )?;

        // Cross-node reductions are order-dependent, so fold sequentially
        // over the canonically ordered `Vec` (never a parallel reduce):
        // install the new scores and sum `|Δ|` in node-index order.
        let mut error = 0_f64;
        for (u, (new_score, delta)) in updated.into_iter().enumerate() {
            prev[u] = new_score;
            error += delta;
        }

        if error < tolerance {
            break;
        }
    }
    Ok(prev)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::value::Tuple;

    /// The termination metric a Jacobi run stops on. `page_rank` uses
    /// [`Term::Sum`] (`Σ|Δ|`); [`Term::Max`] exists only so a test can pin
    /// that choice by showing the two metrics stop at different iterations.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Term {
        Sum,
        Max,
    }

    /// An obviously-correct naive Jacobi reference, deliberately independent
    /// of the CSR/`par_try_map` machinery: plain in-adjacency lists and
    /// out-degree counts over the raw edge list, iterated with the same
    /// `f32` arithmetic and the same fixed summation order (in-neighbors
    /// ascending by source, matching the CSR's `sort_unstable`d in-segments)
    /// as [`super::page_rank`]. It exists to DERIVE the expected values a
    /// value-oracle test asserts, so those values are computed, not
    /// hand-typed. Same math, same order, same termination metric ⇒
    /// byte-identical to `page_rank` (which is [`Term::Sum`]).
    fn naive_jacobi(
        node_count: usize,
        edges: &[(u32, u32)],
        damping: f32,
        tolerance: f64,
        max_iterations: usize,
        term: Term,
    ) -> Vec<f32> {
        let mut in_adj: Vec<Vec<u32>> = vec![vec![]; node_count];
        let mut out_degree: Vec<u32> = vec![0; node_count];
        for &(f, t) in edges {
            in_adj[t as usize].push(f);
            out_degree[f as usize] += 1;
        }
        // Fixed summation order: ascending source id, as the CSR keeps its
        // in-segments.
        for adj in &mut in_adj {
            adj.sort_unstable();
        }

        let init = 1_f32 / node_count as f32;
        let base = (1.0_f32 - damping) / node_count as f32;
        let mut prev = vec![init; node_count];
        for _ in 0..max_iterations {
            let out_share: Vec<f32> = (0..node_count)
                .map(|v| prev[v] / out_degree[v] as f32)
                .collect();
            let mut next = vec![0_f32; node_count];
            let mut sum_delta = 0_f64;
            let mut max_delta = 0_f64;
            for u in 0..node_count {
                let sum: f32 = in_adj[u].iter().map(|&v| out_share[v as usize]).sum();
                next[u] = base + damping * sum;
                let d = f64::abs((next[u] - prev[u]) as f64);
                sum_delta += d;
                max_delta = max_delta.max(d);
            }
            prev = next;
            let error = match term {
                Term::Sum => sum_delta,
                Term::Max => max_delta,
            };
            if error < tolerance {
                break;
            }
        }
        prev
    }

    /// A Gauss-Seidel reference: the interim (pre-parallel) scheme, scores
    /// updated in place so a node reads predecessors' fresh values within
    /// the iteration. Used only to demonstrate that Jacobi and Gauss-Seidel
    /// reach the same fixpoint — different path, same limit.
    fn naive_gauss_seidel(
        node_count: usize,
        edges: &[(u32, u32)],
        damping: f32,
        tolerance: f64,
        max_iterations: usize,
    ) -> Vec<f32> {
        let mut in_adj: Vec<Vec<u32>> = vec![vec![]; node_count];
        let mut out_degree: Vec<u32> = vec![0; node_count];
        for &(f, t) in edges {
            in_adj[t as usize].push(f);
            out_degree[f as usize] += 1;
        }
        for adj in &mut in_adj {
            adj.sort_unstable();
        }
        let init = 1_f32 / node_count as f32;
        let base = (1.0_f32 - damping) / node_count as f32;
        let mut scores = vec![init; node_count];
        let mut out_scores: Vec<f32> = (0..node_count)
            .map(|v| init / out_degree[v] as f32)
            .collect();
        for _ in 0..max_iterations {
            let mut error = 0_f64;
            for u in 0..node_count {
                let sum: f32 = in_adj[u].iter().map(|&v| out_scores[v as usize]).sum();
                let old = scores[u];
                let new = base + damping * sum;
                scores[u] = new;
                error += f64::abs((new - old) as f64);
                out_scores[u] = new / out_degree[u] as f32;
            }
            if error < tolerance {
                break;
            }
        }
        scores
    }

    /// A deterministic pseudo-random directed graph (LCG), large enough that
    /// the per-node Jacobi map splits across rayon workers.
    fn pseudo_random_edges(n: u32, m: usize) -> Vec<(u32, u32)> {
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) as u32) % n
        };
        let mut edges = vec![];
        for _ in 0..m {
            let (a, b) = (next(), next());
            if a != b {
                edges.push((a, b));
            }
        }
        edges.push((n - 1, 0)); // pin the node count at n
        edges
    }

    fn graph_of(edges: &[(u32, u32)]) -> DirectedCsrGraph {
        DirectedCsrGraph::from_edges(edges.iter().map(|&(f, t)| (f, t, ()))).unwrap()
    }

    /// Qualitative sanity, unchanged from the interim port: node 1 collects
    /// rank from 0 and 2, which keep the base score; all scores finite and
    /// positive. Holds under Jacobi as under Gauss-Seidel.
    #[test]
    fn ranks_sinks_and_sources() {
        // 0 → 1, 2 → 1: node 1 collects rank, 0 and 2 keep the base score.
        let graph = DirectedCsrGraph::from_edges([(0u32, 1u32, ()), (2, 1, ())]).unwrap();
        let ranks = page_rank(&graph, 0.85, 1e-7, 50, CancelFlag::default()).unwrap();
        assert_eq!(ranks.len(), 3);
        assert!(ranks[1] > ranks[0]);
        assert!((ranks[0] - ranks[2]).abs() < 1e-6);
        assert!(ranks.iter().all(|r| r.is_finite() && *r > 0.));
    }

    /// VALUE ORACLE: `page_rank` computes exactly the naive two-buffer
    /// Jacobi iterate. The expected values are DERIVED by the independent
    /// reference above (not hand-typed constants) and asserted
    /// byte-identical (same `f32` math, same fixed summation order). This is
    /// the pinned semantic change: were `page_rank` still Gauss-Seidel, this
    /// would fail at any iteration count where the two schemes' iterates
    /// differ.
    #[test]
    fn value_oracle_matches_naive_jacobi() {
        // A small graph with a hub, a sink, and a cycle, at several
        // iteration budgets (few enough that Jacobi and Gauss-Seidel
        // iterates are still distinct — see the divergence test below).
        let edges = [(0u32, 1u32), (0, 2), (1, 2), (2, 0), (3, 0), (1, 3)];
        let graph = graph_of(&edges);
        let n = graph.node_count() as usize;
        for &iters in &[1usize, 2, 5, 10] {
            let got = page_rank(&graph, 0.85, 0.0, iters, CancelFlag::default()).unwrap();
            let want = naive_jacobi(n, &edges, 0.85, 0.0, iters, Term::Sum);
            assert_eq!(
                got, want,
                "page_rank must equal the naive Jacobi reference at {iters} iterations"
            );
        }
    }

    /// VALUE ORACLE, WIDE HUB — pins the in-neighbor summation ORDER, not
    /// just the values. The small oracle above tops out at in-degree 2,
    /// where `f32` pair-addition is commutative, so it cannot see a fold
    /// that runs the in-neighbors in the wrong order. Here node 0 is a hub
    /// with 300 in-neighbors whose out-shares span a wide magnitude range
    /// (source `s` has out-degree `s`, so its share is `(1/n)/s`), which
    /// makes the `f32` sum genuinely non-associative: ascending-source order
    /// and its reverse differ in the low mantissa bits. `page_rank` folds in
    /// CSR ascending-source order and the reference matches it, so any drift
    /// in that order — a `graph.rs` in-segment sort change, or a reversed
    /// fold — breaks this equality. This is the executable proof of the
    /// docstring's "ascending CSR order" claim.
    #[test]
    fn value_oracle_wide_hub_pins_fold_order() {
        // Node 0: 300 in-neighbors (sources 1..=300). out_degree(s) = s,
        // realized as one edge (s, 0) plus s-1 parallel edges (s, 301).
        // Node 301 is a shared dummy sink so the node count stays at 302.
        let mut edges: Vec<(u32, u32)> = vec![];
        for s in 1u32..=300 {
            edges.push((s, 0));
            for _ in 1..s {
                edges.push((s, 301));
            }
        }
        let graph = graph_of(&edges);
        let n = graph.node_count() as usize;
        assert_eq!(n, 302, "hub graph should span nodes 0..=301");
        for &iters in &[1usize, 2, 3] {
            let got = page_rank(&graph, 0.85, 0.0, iters, CancelFlag::default()).unwrap();
            let want = naive_jacobi(n, &edges, 0.85, 0.0, iters, Term::Sum);
            assert_eq!(
                got, want,
                "wide-hub Jacobi must match the reference at {iters} iterations \
                 (fold order pinned to ascending CSR source)"
            );
        }
    }

    /// TERMINATION METRIC is `Σ|Δ|`, not `max|Δ|`. The tolerance-0 oracles
    /// never take the early-out, so they cannot see which metric guards it.
    /// At tolerance 0.1 on the irregular graph the two metrics diverge:
    /// `max|Δ|` first drops below 0.1 at iteration 2, but `Σ|Δ|` not until
    /// iteration 3 — so the two stop on DIFFERENT iterates. `page_rank` must
    /// return the `Σ|Δ|` result; the `assert_ne!` guards that the tolerance
    /// actually distinguishes the metrics (else the test would prove
    /// nothing).
    #[test]
    fn termination_metric_is_sum_not_max() {
        let edges = [(0u32, 1u32), (0, 2), (1, 2), (2, 0), (3, 0), (1, 3)];
        let graph = graph_of(&edges);
        let n = graph.node_count() as usize;
        let tol = 0.1;
        let got = page_rank(&graph, 0.85, tol, 50, CancelFlag::default()).unwrap();
        let want_sum = naive_jacobi(n, &edges, 0.85, tol, 50, Term::Sum);
        let want_max = naive_jacobi(n, &edges, 0.85, tol, 50, Term::Max);
        assert_ne!(
            want_sum, want_max,
            "at this tolerance Σ|Δ| and max|Δ| must stop at different iterations"
        );
        assert_eq!(
            got, want_sum,
            "page_rank must terminate on Σ|Δ| (stops one iteration later than max|Δ| here)"
        );
    }

    /// VALUE ORACLE through the full rule path (`run_fixed_rule` →
    /// `DataValue`): the emitted rows are the reference scores as `f64`,
    /// keyed by the interned node symbols in index order. Pins the plumbing
    /// (indices, output shape, f32→f64 widening) on top of the numeric
    /// oracle above.
    #[test]
    fn through_rule_matches_reference() {
        use crate::fixed_rule::tests_support::{TestInput, run_fixed_rule};

        let s = |v: &str| DataValue::from(v);
        // Symbols a,b,c,d intern to node ids 0,1,2,3 in first-seen order.
        let got = run_fixed_rule(
            &PageRank,
            vec![TestInput::new(
                vec!["fr", "to"],
                vec![
                    vec![s("a"), s("b")].into(),
                    vec![s("a"), s("c")].into(),
                    vec![s("b"), s("c")].into(),
                    vec![s("c"), s("a")].into(),
                    vec![s("d"), s("a")].into(),
                    vec![s("b"), s("d")].into(),
                ],
            )],
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap();

        let edges = [(0u32, 1u32), (0, 2), (1, 2), (2, 0), (3, 0), (1, 3)];
        // Defaults the rule applies: theta 0.85, epsilon 1e-4, iterations 10.
        let want_scores = naive_jacobi(4, &edges, 0.85, 1e-4, 10, Term::Sum);
        let want: Vec<Tuple> = ["a", "b", "c", "d"]
            .iter()
            .zip(want_scores.iter())
            .map(|(name, score)| vec![s(name), DataValue::from(*score as f64)].into())
            .collect();
        assert_eq!(got, want);
    }

    /// DETERMINISM (a): a single-thread rayon pool and the default
    /// (multi-thread) pool produce byte-identical scores, across 20 runs.
    /// `page_rank` returns an ordered `Vec`, so this pins both value AND
    /// order at every thread count.
    #[test]
    fn single_thread_matches_default_pool() {
        let edges = pseudo_random_edges(500, 4000);
        let graph = graph_of(&edges);
        let single = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let seq =
            single.install(|| page_rank(&graph, 0.85, 0.0, 30, CancelFlag::default()).unwrap());
        for _ in 0..20 {
            let par = page_rank(&graph, 0.85, 0.0, 30, CancelFlag::default()).unwrap();
            assert_eq!(
                seq, par,
                "single-thread and default-pool scores must be identical"
            );
        }
    }

    /// DETERMINISM (b): the default pool run twice is byte-identical (no
    /// run-to-run drift from scheduling).
    #[test]
    fn run_twice_identical() {
        let edges = pseudo_random_edges(500, 4000);
        let graph = graph_of(&edges);
        let a = page_rank(&graph, 0.85, 0.0, 30, CancelFlag::default()).unwrap();
        let b = page_rank(&graph, 0.85, 0.0, 30, CancelFlag::default()).unwrap();
        assert_eq!(a, b);
    }

    /// SAME FIXPOINT, DIFFERENT PATH: run both schemes to convergence (a
    /// tight tolerance, generous iteration budget) on a strongly-connected
    /// graph where both converge; the limits agree within a loose multiple
    /// of the tolerance. This is why the semantic change is safe: Jacobi and
    /// Gauss-Seidel differ only in the transient, not in what they converge
    /// to.
    #[test]
    fn converges_to_same_fixpoint_as_gauss_seidel() {
        // A strongly connected but degree-IRREGULAR graph, so PageRank does
        // not stay uniform and the two schemes' transients genuinely differ
        // (a regular graph would leave both trivially uniform and equal,
        // hiding the divergence). Out-degrees: 0→{1,2}, 1→{2,3}, 2→{0},
        // 3→{0}; every node both reaches and is reached from 0.
        let edges = [(0u32, 1u32), (0, 2), (1, 2), (2, 0), (3, 0), (1, 3)];
        let graph = graph_of(&edges);
        let n = graph.node_count() as usize;
        let tol = 1e-9;
        let jac = page_rank(&graph, 0.85, tol, 100_000, CancelFlag::default()).unwrap();
        let gs = naive_gauss_seidel(n, &edges, 0.85, tol, 100_000);
        for (a, b) in jac.iter().zip(gs.iter()) {
            assert!(
                (a - b).abs() < 1e-4,
                "Jacobi {a} and Gauss-Seidel {b} must agree at the fixpoint"
            );
        }
        // And Jacobi is not trivially equal to GS mid-transient (guards
        // against the reference collapsing into the implementation): at a
        // small iteration count the two schemes disagree somewhere.
        let jac_few = page_rank(&graph, 0.85, 0.0, 3, CancelFlag::default()).unwrap();
        let gs_few = naive_gauss_seidel(n, &edges, 0.85, 0.0, 3);
        assert!(
            jac_few.iter().zip(gs_few.iter()).any(|(a, b)| a != b),
            "Jacobi and Gauss-Seidel iterates should differ before convergence"
        );
    }

    /// Cancellation is honoured: a pre-cancelled flag refuses before any
    /// iteration completes.
    #[test]
    fn cancellation_refuses() {
        let graph = graph_of(&pseudo_random_edges(200, 1000));
        let cancel = CancelFlag::default();
        cancel.cancel();
        let res = page_rank(&graph, 0.85, 0.0, 30, cancel);
        assert!(res.is_err());
    }
}
