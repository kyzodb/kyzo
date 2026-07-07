/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Maximum flow / minimum cut on a weighted directed graph.
//!
//! New in KyzoDB (no CozoDB precedent). Inputs: the weighted edge relation
//! (input 0, `[from, to, capacity]`; capacity defaults to `1.0`, and the
//! existing weighted-graph builder enforces **finite, non-negative**
//! capacities — a negative capacity is refused through the established typed
//! [`crate::fixed_rule::BadEdgeWeightError`] path), plus a single-source
//! relation (input 1) and a single-sink relation (input 2), each read like
//! the Dijkstra/Prim start relations: the first row's first column, required
//! to be a node of the graph.
//!
//! Output: one row `[from, to, flow]` per edge of a **minimum cut** — the
//! edges crossing from the source side to the sink side of the residual
//! graph, each saturated (`flow == capacity`). By the max-flow/min-cut
//! theorem the **maximum-flow value is exactly the sum of these rows'
//! flows**, so the scalar the caller usually wants is recovered by summing
//! the third column. Arity is 3.
//!
//! **Algorithm: Edmonds–Karp** (BFS shortest augmenting paths), `O(V E^2)`.
//! Chosen over Dinic deliberately:
//! - *Termination with float capacities.* Edmonds–Karp's augmentation count
//!   is `O(V E)` by the BFS-distance-monotonicity argument, which is purely
//!   combinatorial and independent of capacity magnitude or rationality — so
//!   it terminates on real-valued capacities where plain Ford–Fulkerson can
//!   loop forever. That property is worth more here than Dinic's better
//!   asymptotics.
//! - *Law 5 (no deep recursion).* Every phase is a BFS plus an iterative
//!   back-pointer walk — no recursion at all, so no stack to overflow on a
//!   large stored graph. Dinic's blocking-flow DFS would need a careful
//!   iterative rewrite to match that guarantee.
//!
//! **Parallel edges: capacities SUM.** Two parallel `u → v` arcs are two
//! independent conduits, so their combined throughput is the sum of their
//! capacities — the physically correct policy and the one every max-flow
//! reference assumes. Parallel input edges are therefore aggregated by sum
//! into a single residual arc (pinned by `parallel_edges_sum_capacity`).
//! Antiparallel arcs (`u → v` and `v → u`) stay independent, each with its
//! own residual reverse arc. Self-loops carry no s–t flow and are dropped.
//!
//! **Determinism.** The maximum-flow *value* is a graph invariant. The
//! specific cut returned is the source-side residual-reachable cut, and the
//! whole computation is deterministic: residual arcs are built in sorted
//! `(from, to)` order, BFS visits neighbors in that fixed adjacency order,
//! and the reported cut edges are enumerated in sorted order. Two runs are
//! byte-identical (pinned by `deterministic_across_runs`).

use std::collections::{BTreeMap, VecDeque};

use miette::{Diagnostic, Result, bail};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::expr::Expr;
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::fixed_rule::graph::DirectedCsrGraph;
use crate::fixed_rule::{
    CancelFlag, FixedRule, FixedRuleInputRelation, FixedRuleOutput, FixedRulePayload,
    NodeNotFoundError,
};

/// Residual-capacity threshold: values at or below this count as saturated.
/// It guards against floating-point residual "dust" (`cap - flow` landing at
/// a spurious tiny positive) driving vacuous augmentations. The threshold is
/// load-bearing for termination, so the contract is enforced rather than
/// assumed: any aggregated capacity in `(0, EPS]` is refused up front as a
/// typed error ([`SubEpsilonCapacityError`]) — inside the solver every
/// positive capacity is therefore strictly above `EPS` and can never be
/// mis-saturated from the start.
const EPS: f64 = 1e-9;

// A test-only observable: how many nodes the augmenting BFS has dequeued.
// Mirrors `shortest_path_bfs::BFS_NODES_EXPANDED`: the cancellation test
// asserts a deterministic, load-independent effect of the per-pop poll. In a
// non-test build the note fn is an empty inlined no-op.
#[cfg(test)]
thread_local! {
    static MAXFLOW_BFS_POPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_maxflow_bfs_pop() {
    MAXFLOW_BFS_POPS.with(|c| c.set(c.get() + 1));
}

/// Reset the counter and return what it held (for the cancellation test).
#[cfg(test)]
fn take_maxflow_bfs_pops() -> u64 {
    MAXFLOW_BFS_POPS.with(|c| c.replace(0))
}

#[cfg(not(test))]
#[inline(always)]
fn note_maxflow_bfs_pop() {}

pub(crate) struct MaxFlow;

impl FixedRule for MaxFlow {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let edges = payload.get_input(0)?;
        let source_rel = payload.get_input(1)?.ensure_min_len(1)?;
        let sink_rel = payload.get_input(2)?.ensure_min_len(1)?;

        // Directed, non-negative capacities (negatives refused by the builder).
        let (graph, indices, inv_indices) = edges.as_directed_weighted_graph(false, false)?;

        let source = single_node(&source_rel, &inv_indices, "source")?;
        let sink = single_node(&sink_rel, &inv_indices, "sink")?;
        if source == sink {
            bail!(SourceIsSinkError(source_rel.span()));
        }

        let mut net = ResidualNet::from_graph(&graph, edges.span())?;
        net.max_flow(source, sink, &cancel)?;

        for (from, to, flow) in net.min_cut_edges(source) {
            out.put(vec![
                indices[from as usize].clone(),
                indices[to as usize].clone(),
                DataValue::from(flow),
            ])?;
        }
        Ok(())
    }

    fn arity(
        &self,
        _options: &BTreeMap<SmartString<LazyCompact>, Expr>,
        _rule_head: &[Symbol],
        _span: SourceSpan,
    ) -> Result<usize> {
        Ok(3)
    }
}

/// The first row's first column of an endpoint relation, resolved to a graph
/// node id (mirrors the Dijkstra/Prim start-relation convention).
fn single_node(
    rel: &FixedRuleInputRelation<'_>,
    inv_indices: &BTreeMap<DataValue, u32>,
    which: &'static str,
) -> Result<u32> {
    let tuple = rel
        .iter()?
        .next()
        .ok_or_else(|| EmptyEndpointError(which, rel.span()))??;
    // Structural: the caller's `ensure_min_len(1)` on `rel` proved every
    // tuple has a first column (a nullary relation would otherwise yield
    // an empty tuple here despite the row-count check above).
    let dv = tuple.into_iter().next().unwrap();
    inv_indices.get(&dv).copied().ok_or_else(|| {
        NodeNotFoundError {
            missing: dv,
            span: rel.span(),
        }
        .into()
    })
}

/// One residual arc: `flow` may exceed neither `cap` (forward slack) nor push
/// the paired reverse arc below zero. `rev` indexes the paired arc within
/// `adj[to]`.
#[derive(Clone)]
struct ResidualArc {
    to: u32,
    cap: f64,
    flow: f64,
    rev: usize,
}

/// The residual network: an adjacency list of arcs, each with its paired
/// reverse arc, plus the list of *original* forward arcs (as `(from, index)`
/// into `adj`) so the min cut can be read back over real edges only.
struct ResidualNet {
    adj: Vec<Vec<ResidualArc>>,
    orig: Vec<(u32, usize)>,
}

impl ResidualNet {
    fn from_graph(graph: &DirectedCsrGraph<f32>, span: SourceSpan) -> Result<Self> {
        let n = graph.node_count() as usize;
        // Aggregate parallel arcs by SUM of capacity (see module docs). The
        // BTreeMap fixes a sorted `(from, to)` construction order, which the
        // determinism argument relies on.
        let mut agg: BTreeMap<(u32, u32), f64> = BTreeMap::new();
        for u in 0..graph.node_count() {
            for target in graph.out_neighbors_with_values(u) {
                if u == target.target {
                    continue; // self-loop: carries no s–t flow
                }
                *agg.entry((u, target.target)).or_default() += target.value as f64;
            }
        }
        let mut adj: Vec<Vec<ResidualArc>> = vec![Vec::new(); n];
        let mut orig = Vec::with_capacity(agg.len());
        for (&(u, v), &cap) in &agg {
            // The EPS contract (see the constant's doc): a positive capacity
            // at or below the saturation threshold would be silently treated
            // as zero by the solver, so it is refused instead.
            if cap > 0.0 && cap <= EPS {
                bail!(SubEpsilonCapacityError(u, v, cap, span));
            }
        }
        for ((u, v), cap) in agg {
            let fwd_idx = adj[u as usize].len();
            let rev_idx = adj[v as usize].len();
            adj[u as usize].push(ResidualArc {
                to: v,
                cap,
                flow: 0.0,
                rev: rev_idx,
            });
            adj[v as usize].push(ResidualArc {
                to: u,
                cap: 0.0,
                flow: 0.0,
                rev: fwd_idx,
            });
            orig.push((u, fwd_idx));
        }
        Ok(Self { adj, orig })
    }

    /// Edmonds–Karp: repeatedly BFS for a shortest augmenting path and push
    /// its bottleneck, until the source can no longer reach the sink. Returns
    /// the maximum-flow value (also `Σ` of the min-cut edge flows).
    fn max_flow(&mut self, source: u32, sink: u32, cancel: &CancelFlag) -> Result<f64> {
        let n = self.adj.len();
        let mut total = 0.0;
        loop {
            // BFS from source; `prev[v] = (u, arc index in adj[u])` records
            // the tree edge reaching v.
            let mut prev: Vec<Option<(u32, usize)>> = vec![None; n];
            let mut seen = vec![false; n];
            seen[source as usize] = true;
            let mut queue = VecDeque::new();
            queue.push_back(source);
            while let Some(u) = queue.pop_front() {
                note_maxflow_bfs_pop();
                cancel.check()?;
                if u == sink {
                    break;
                }
                for (ai, arc) in self.adj[u as usize].iter().enumerate() {
                    if arc.cap - arc.flow > EPS && !seen[arc.to as usize] {
                        seen[arc.to as usize] = true;
                        prev[arc.to as usize] = Some((u, ai));
                        queue.push_back(arc.to);
                    }
                }
            }
            if !seen[sink as usize] {
                break; // no augmenting path: flow is maximum
            }

            // Bottleneck residual along the found path.
            let mut bottleneck = f64::INFINITY;
            let mut v = sink;
            while v != source {
                // Structural: BFS set `prev` for every reached node but source.
                let (u, ai) = prev[v as usize].unwrap();
                let arc = &self.adj[u as usize][ai];
                bottleneck = bottleneck.min(arc.cap - arc.flow);
                v = u;
            }

            // Push it: raise forward flow, lower the paired reverse arc.
            let mut v = sink;
            while v != source {
                let (u, ai) = prev[v as usize].unwrap();
                let rev = self.adj[u as usize][ai].rev;
                self.adj[u as usize][ai].flow += bottleneck;
                self.adj[v as usize][rev].flow -= bottleneck;
                v = u;
            }
            total += bottleneck;
        }
        Ok(total)
    }

    /// The minimum cut: from the final residual graph, the set `S` of nodes
    /// still reachable from the source; the cut edges are the original arcs
    /// leaving `S`. Each such arc is saturated, so its `flow` equals its
    /// capacity. Returned in sorted `(from, to)` order (the `orig` order).
    fn min_cut_edges(&self, source: u32) -> Vec<(u32, u32, f64)> {
        let n = self.adj.len();
        let mut side = vec![false; n];
        side[source as usize] = true;
        let mut queue = VecDeque::new();
        queue.push_back(source);
        while let Some(u) = queue.pop_front() {
            for arc in &self.adj[u as usize] {
                if arc.cap - arc.flow > EPS && !side[arc.to as usize] {
                    side[arc.to as usize] = true;
                    queue.push_back(arc.to);
                }
            }
        }
        let mut cut = Vec::new();
        for &(from, idx) in &self.orig {
            let arc = &self.adj[from as usize][idx];
            if side[from as usize] && !side[arc.to as usize] {
                cut.push((from, arc.to, arc.flow));
            }
        }
        cut
    }
}

#[derive(Debug, Error, Diagnostic)]
#[error("The {0} relation for MaxFlow is empty")]
#[diagnostic(code(algo::max_flow_empty_endpoint))]
#[diagnostic(help("MaxFlow needs a source (input 1) and a sink (input 2), each one node"))]
struct EmptyEndpointError(&'static str, #[label] SourceSpan);

#[derive(Debug, Error, Diagnostic)]
#[error(
    "MaxFlow capacity {2:e} on edge {0}->{1} is positive but at or below the solver's saturation threshold (1e-9)"
)]
#[diagnostic(code(algo::max_flow_sub_epsilon_capacity))]
#[diagnostic(help(
    "capacities this small would be silently treated as zero, producing a wrong \
     max flow; rescale the edge weights so every positive capacity exceeds 1e-9"
))]
struct SubEpsilonCapacityError(u32, u32, f64, #[label] SourceSpan);

#[derive(Debug, Error, Diagnostic)]
#[error("MaxFlow source and sink are the same node")]
#[diagnostic(code(algo::max_flow_source_is_sink))]
#[diagnostic(help("The maximum flow from a node to itself is undefined; give distinct endpoints"))]
struct SourceIsSinkError(#[label] SourceSpan);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::value::Tuple;
    use crate::fixed_rule::tests_support::{TestInput, run_fixed_rule};

    fn s(v: &str) -> DataValue {
        DataValue::from(v)
    }

    /// Run MaxFlow over string-named nodes; returns the cut rows as
    /// `(from, to, flow)`.
    fn run_cut(
        edges: &[(&str, &str, f64)],
        source: &str,
        sink: &str,
    ) -> Vec<(String, String, f64)> {
        let erows = edges
            .iter()
            .map(|&(a, b, c)| vec![s(a), s(b), DataValue::from(c)])
            .collect::<Vec<Tuple>>();
        let got = run_fixed_rule(
            &MaxFlow,
            vec![
                TestInput::new(vec!["fr", "to", "cap"], erows),
                TestInput::new(vec!["src"], vec![vec![s(source)]]),
                TestInput::new(vec!["snk"], vec![vec![s(sink)]]),
            ],
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap();
        got.into_iter()
            .map(|r| {
                (
                    r[0].get_str().unwrap().to_string(),
                    r[1].get_str().unwrap().to_string(),
                    r[2].get_float().unwrap(),
                )
            })
            .collect()
    }

    /// The maximum-flow value = sum of the returned cut edges' flows.
    fn flow_value(edges: &[(&str, &str, f64)], source: &str, sink: &str) -> f64 {
        run_cut(edges, source, sink)
            .iter()
            .map(|(_, _, f)| *f)
            .sum()
    }

    /// INDEPENDENT ORACLE: the max-flow value equals the minimum s–t cut
    /// capacity, computed by brute force over all `2^(n-2)` vertex bipartitions
    /// (source-side must contain the source, exclude the sink). This shares no
    /// logic with augmenting-path search, so it is a genuine cross-check.
    /// Node ids here are the caller's own `0..n` labelling; the value is
    /// label-independent, so it compares directly with the rule's flow sum.
    fn brute_min_cut(n: usize, caps: &[(usize, usize, f64)], source: usize, sink: usize) -> f64 {
        let mut best = f64::INFINITY;
        for mask in 0u32..(1u32 << n) {
            let in_s = |i: usize| mask & (1 << i) != 0;
            if !in_s(source) || in_s(sink) {
                continue;
            }
            let mut c = 0.0;
            for &(u, v, cap) in caps {
                if in_s(u) && !in_s(v) {
                    c += cap;
                }
            }
            best = best.min(c);
        }
        best
    }

    /// VALUE ORACLE: the CLRS figure-26.1 network, whose maximum flow is the
    /// textbook 23. Exercises antiparallel arcs (`v1↔v2`) and flow
    /// cancellation through reverse arcs.
    #[test]
    fn clrs_network_max_flow_is_23() {
        let edges = [
            ("s", "v1", 16.0),
            ("s", "v2", 13.0),
            ("v1", "v2", 10.0),
            ("v2", "v1", 4.0),
            ("v1", "v3", 12.0),
            ("v3", "v2", 9.0),
            ("v2", "v4", 14.0),
            ("v4", "v3", 7.0),
            ("v3", "t", 20.0),
            ("v4", "t", 4.0),
        ];
        assert!((flow_value(&edges, "s", "t") - 23.0).abs() < 1e-6);
    }

    /// VALUE ORACLE: a unique-bottleneck graph pins the exact cut edge. The
    /// only way from s to t narrows through `a → t` (capacity 1), so the cut
    /// is exactly that one saturated edge and the value is 1.
    #[test]
    fn unique_bottleneck_cut_is_pinned() {
        let cut = run_cut(&[("s", "a", 10.0), ("a", "t", 1.0)], "s", "t");
        assert_eq!(cut, vec![("a".to_string(), "t".to_string(), 1.0)]);
    }

    /// PARALLEL-EDGE POLICY: two parallel `s → t` arcs (capacities 2 and 3)
    /// aggregate to a single conduit of capacity 5, so the max flow is 5 and
    /// the cut is the one summed edge. A `max`/`last-wins` policy would give
    /// 3; a mis-handled parallel arc would double-count endpoints.
    #[test]
    fn parallel_edges_sum_capacity() {
        let cut = run_cut(&[("s", "t", 2.0), ("s", "t", 3.0)], "s", "t");
        assert_eq!(cut, vec![("s".to_string(), "t".to_string(), 5.0)]);
    }

    /// A deterministic pseudo-random battery of small graphs, each checked
    /// against the brute-force min-cut oracle. This is where flow cancellation
    /// through reverse arcs is exercised broadly: breaking the reverse-arc
    /// update (`flow -= bottleneck`) makes many of these values wrong.
    fn random_graph(seed: u64, n: usize) -> Vec<(usize, usize, f64)> {
        let mut state = seed;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state
        };
        let mut caps: BTreeMap<(usize, usize), f64> = BTreeMap::new();
        let m = 3 * n;
        for _ in 0..m {
            let u = (next() >> 33) as usize % n;
            let v = (next() >> 33) as usize % n;
            let c = 1.0 + ((next() >> 40) % 9) as f64;
            if u != v {
                *caps.entry((u, v)).or_default() += c;
            }
        }
        caps.into_iter().map(|((u, v), c)| (u, v, c)).collect()
    }

    #[test]
    fn matches_brute_min_cut_oracle() {
        let n = 8usize;
        for seed in 0..120u64 {
            let caps = random_graph(0xC0FF_EE00 ^ seed, n);
            let edges: Vec<(String, String, f64)> = caps
                .iter()
                .map(|&(u, v, c)| (format!("{u}"), format!("{v}"), c))
                .collect();
            let eref: Vec<(&str, &str, f64)> = edges
                .iter()
                .map(|(u, v, c)| (u.as_str(), v.as_str(), *c))
                .collect();
            let source = 0usize;
            let sink = n - 1;
            let got = flow_value(&eref, &source.to_string(), &sink.to_string());
            let expected = brute_min_cut(n, &caps, source, sink);
            assert!(
                (got - expected).abs() < 1e-6,
                "seed {seed}: got {got}, brute min-cut {expected}, caps {caps:?}"
            );
        }
    }

    /// DETERMINISM: byte-identical cut across repeated runs on a random graph.
    #[test]
    fn deterministic_across_runs() {
        let caps = random_graph(0xD37E_C7ED_u64 ^ 0xABCD, 9);
        let edges: Vec<(String, String, f64)> = caps
            .iter()
            .map(|&(u, v, c)| (format!("{u}"), format!("{v}"), c))
            .collect();
        let eref: Vec<(&str, &str, f64)> = edges
            .iter()
            .map(|(u, v, c)| (u.as_str(), v.as_str(), *c))
            .collect();
        let first = run_cut(&eref, "0", "8");
        for _ in 0..6 {
            assert_eq!(run_cut(&eref, "0", "8"), first);
        }
    }

    /// A saturated-path scale/adversarial case: a long chain plus a fat
    /// bypass. Purely iterative BFS + back-pointer walk, no recursion — the
    /// point being there is no stack to overflow however long the chain.
    #[test]
    fn long_chain_bottleneck() {
        let k = 5_000u32;
        let mut edges: Vec<(String, String, f64)> = Vec::new();
        edges.push(("s".to_string(), "x0".to_string(), 1.0));
        for i in 0..k {
            edges.push((format!("x{i}"), format!("x{}", i + 1), 1.0));
        }
        edges.push((format!("x{k}"), "t".to_string(), 1.0));
        let eref: Vec<(&str, &str, f64)> = edges
            .iter()
            .map(|(u, v, c)| (u.as_str(), v.as_str(), *c))
            .collect();
        // The chain carries exactly capacity 1 end to end.
        assert!((flow_value(&eref, "s", "t") - 1.0).abs() < 1e-9);
    }

    /// Endpoint validation: an unknown source is refused, typed.
    #[test]
    fn unknown_source_is_refused() {
        let err = run_fixed_rule(
            &MaxFlow,
            vec![
                TestInput::new(
                    vec!["fr", "to", "cap"],
                    vec![vec![s("a"), s("b"), DataValue::from(1.0)]],
                ),
                TestInput::new(vec!["src"], vec![vec![s("nope")]]),
                TestInput::new(vec!["snk"], vec![vec![s("b")]]),
            ],
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    /// Source == sink is refused, typed.
    #[test]
    fn source_equals_sink_is_refused() {
        let err = run_fixed_rule(
            &MaxFlow,
            vec![
                TestInput::new(
                    vec!["fr", "to", "cap"],
                    vec![vec![s("a"), s("b"), DataValue::from(1.0)]],
                ),
                TestInput::new(vec!["src"], vec![vec![s("a")]]),
                TestInput::new(vec!["snk"], vec![vec![s("a")]]),
            ],
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("same node"), "{err}");
    }

    /// A negative capacity is refused through the existing weighted-graph
    /// builder path.
    #[test]
    fn negative_capacity_is_refused() {
        let err = run_fixed_rule(
            &MaxFlow,
            vec![
                TestInput::new(
                    vec!["fr", "to", "cap"],
                    vec![vec![s("a"), s("b"), DataValue::from(-1.0)]],
                ),
                TestInput::new(vec!["src"], vec![vec![s("a")]]),
                TestInput::new(vec!["snk"], vec![vec![s("b")]]),
            ],
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("edge weight"), "{err}");
    }

    /// CANCELLATION, inner-poll pinned (house exemplar:
    /// `shortest_path_bfs::honors_cancel_pins_inner_poll`). The baseline
    /// pushes one unit down a 60k-edge chain, so its first augmenting BFS
    /// dequeues every node; with a pre-raised flag the per-pop poll must
    /// refuse before dequeuing more than one. Deleting the poll inside the
    /// BFS loop makes the cancelled run walk the whole chain, so the `<= 1`
    /// bound fails. Load-independent: counts BFS pops, not wall-clock.
    #[test]
    fn honors_cancel_pins_inner_poll() {
        use crate::fixed_rule::tests_support::prepare_fixed_rule;

        let n: u32 = 60_000;
        let edges: Vec<Tuple> = (0..n - 1)
            .map(|i| {
                vec![
                    s(&format!("v{i}")),
                    s(&format!("v{}", i + 1)),
                    DataValue::from(1.0),
                ]
            })
            .collect();
        let inputs = vec![
            TestInput::new(vec!["fr", "to", "cap"], edges),
            TestInput::new(vec!["src"], vec![vec![s("v0")]]),
            TestInput::new(vec!["snk"], vec![vec![s(&format!("v{}", n - 1))]]),
        ];
        let prepared = prepare_fixed_rule(&MaxFlow, inputs, BTreeMap::new()).unwrap();

        // Baseline: no cancellation. The first BFS walks the whole chain.
        take_maxflow_bfs_pops(); // clear any leftover from a reused thread
        let full = prepared.run(&MaxFlow, CancelFlag::default());
        let full_pops = take_maxflow_bfs_pops();
        assert!(full.is_ok());
        assert!(
            full_pops >= u64::from(n),
            "baseline should dequeue the whole chain, got {full_pops}"
        );

        // Pre-set flag: the inner poll must refuse before walking the chain.
        let flag = CancelFlag::default();
        flag.cancel();
        let cancelled = prepared.run(&MaxFlow, flag);
        let cancel_pops = take_maxflow_bfs_pops();
        assert!(cancelled.unwrap_err().to_string().contains("killed"));
        assert!(
            cancel_pops <= 1,
            "inner poll did not refuse before walking the chain: dequeued \
             {cancel_pops} nodes (deleting the per-pop poll makes this ~60k)"
        );
    }

    /// F1 (hostile review): a positive capacity at or below the saturation
    /// threshold is refused as a typed error up front — never silently
    /// treated as saturated, which returned a wrong max flow of 0 for a
    /// 1e-12-capacity edge. Covers both a plain tiny value and an f32
    /// denormal.
    #[test]
    fn sub_epsilon_capacity_is_refused() {
        for tiny in [1e-12_f64, 1e-40] {
            let err = run_fixed_rule(
                &MaxFlow,
                vec![
                    TestInput::new(
                        vec!["fr", "to", "cap"],
                        vec![vec![s("s"), s("t"), DataValue::from(tiny)]],
                    ),
                    TestInput::new(vec!["src"], vec![vec![s("s")]]),
                    TestInput::new(vec!["snk"], vec![vec![s("t")]]),
                ],
                BTreeMap::new(),
                CancelFlag::default(),
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("saturation threshold"),
                "capacity {tiny:e} must be refused with the typed \
                 sub-epsilon error, got: {err}"
            );
        }
    }
}
