/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! K-core decomposition: the **core number** (coreness) of every node — the
//! largest `k` such that the node survives in the maximal subgraph where
//! every vertex has degree at least `k`.
//!
//! New in KyzoDB (no CozoDB precedent). The output is one row `[node,
//! core_number]` per node: *all* core numbers in a single pass, via the
//! standard Batagelj–Zaversnik bucket peeling (`O(V + E)`), rather than a
//! separate run per `k`.
//!
//! **Graph interpretation.** Coreness is a simple-undirected-graph
//! invariant (this matches every reference implementation, e.g. NetworkX
//! `core_number`, which requires an undirected graph and ignores self-loops
//! and edge multiplicity). The directed input edge relation is therefore
//! read *undirected* — each input edge contributes to both endpoints'
//! degree — and the working degree counts **distinct neighbors**: parallel
//! edges and self-loops do not inflate it. This is a deliberate departure
//! from the CSR's parallel-edge retention (`fixed_rule/graph.rs`), justified
//! because coreness is defined on the simple graph; it is documented and
//! test-pinned (`self_loops_and_parallel_edges_ignored`).
//!
//! **Determinism.** The core number is a graph invariant — it does not
//! depend on peel order at all — so the output is deterministic by
//! definition. On top of that the peel order itself is fixed: vertices are
//! bucketed by current degree and, within a degree, processed in ascending
//! node id. The algorithm is fully iterative (no recursion; law 5 is not
//! even in play), so an arbitrarily large stored graph peels without touching
//! the thread stack.

use miette::Result;

use crate::rules::contract::{CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload};
use crate::rules::graph_view::DirectedCsrGraph;
use kyzo_model::SourceSpan;
use kyzo_model::program::rule::FixedRuleOptions;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::{DataValue, Tuple};

// A test-only observable: how many vertices the peel loop has processed.
// Mirrors `shortest_path_bfs::BFS_NODES_EXPANDED` (see that comment for the
// rationale): `honors_cancel_pins_inner_poll` asserts a deterministic,
// load-independent effect of the per-vertex poll instead of wall-clock. In a
// non-test build the note fn is an empty inlined no-op.
#[cfg(test)]
#[cfg(test)]
use crate::rules::contract::{CancelAuthority, Cancelled};
thread_local! {
    static KCORE_VERTS_PEELED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_kcore_vertex_peeled() {
    KCORE_VERTS_PEELED.with(|c| c.set(c.get() + 1));
}

/// Reset the counter and return what it held (for the cancellation test).
#[cfg(test)]
fn take_kcore_verts_peeled() -> u64 {
    KCORE_VERTS_PEELED.with(|c| c.replace(0))
}

#[cfg(not(test))]
#[inline(always)]
fn note_kcore_vertex_peeled() {}

pub(crate) struct KCoreDecomposition;

impl FixedRule for KCoreDecomposition {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let edges = payload.get_input(0)?;
        // Undirected: coreness is a simple-undirected-graph invariant.
        let (graph, indices, _inv_indices) = edges.as_directed_graph(true)?;
        let n = graph.node_count();
        if n == 0 {
            return Ok(());
        }
        let core = core_numbers(&graph, &cancel)?;
        for (i, c) in core.into_iter().enumerate() {
            out.put(Tuple::from_vec(vec![
                indices[i].clone(),
                DataValue::from(i64::from(c)),
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
        Ok(2)
    }
}

/// The simple undirected adjacency: for each node, its distinct neighbors
/// (parallel edges collapsed) with self-loops dropped. Built from the
/// undirected CSR, whose out-adjacency is already target-sorted, so a single
/// `dedup` after the self-filter suffices.
fn simple_adjacency(graph: &DirectedCsrGraph) -> Result<Vec<Vec<u32>>> {
    let n_u32 = graph.node_count();
    let n = crate::rules::convert::usize_from_u32(n_u32);
    let mut adj = Vec::with_capacity(n);
    for v in 0..n_u32 {
        let mut nbrs: Vec<u32> = graph.out_neighbors(v).filter(|&u| u != v).collect();
        nbrs.dedup(); // already sorted by the CSR; collapse parallel edges
        adj.push(nbrs);
    }
    Ok(adj)
}

/// Batagelj–Zaverznik core decomposition (`O(V + E)`): bucket vertices by
/// degree, then repeatedly take the minimum-degree vertex, fix its core
/// number to its current degree, and decrement each higher-degree neighbor
/// (sliding it one bucket down). The returned vector holds every node's core
/// number, indexed by node id.
///
/// The load-bearing step is the neighbor decrement + bucket slide: without
/// it the "degree" never falls and the result collapses to raw degree, which
/// is wrong wherever coreness differs from degree (pinned by
/// `coreness_differs_from_degree`).
fn core_numbers(graph: &DirectedCsrGraph, cancel: &CancelFlag) -> Result<Vec<u32>> {
    let n = crate::rules::convert::usize_from_u32(graph.node_count());
    let adj = simple_adjacency(graph)?;
    let mut deg: Vec<u32> = {
        let mut out = Vec::with_capacity(adj.len());
        for a in &adj {
            out.push(crate::rules::convert::u32_from_usize(a.len())?);
        }
        out
    };
    let max_deg = deg
        .iter()
        .copied()
        .map(crate::rules::convert::usize_from_u32)
        .fold(0, Ord::max);

    // `bin[d]`: first the count of degree-`d` vertices, then (in place) the
    // start offset of the degree-`d` block within `vert`.
    let mut bin = vec![0usize; max_deg + 1];
    for &d in &deg {
        bin[crate::rules::convert::usize_from_u32(d)] += 1;
    }
    let mut start = 0usize;
    for slot in bin.iter_mut() {
        let count = *slot;
        *slot = start;
        start += count;
    }

    // `vert`: vertices ordered by degree (ties by ascending id, since `v`
    // ascends); `pos[v]`: v's index in `vert`.
    let mut pos = vec![0usize; n];
    let mut vert = vec![0u32; n];
    for v in 0..n {
        pos[v] = bin[crate::rules::convert::usize_from_u32(deg[v])];
        vert[pos[v]] = crate::rules::convert::u32_from_usize(v)?;
        bin[crate::rules::convert::usize_from_u32(deg[v])] += 1;
    }
    // Filling shifted every offset up by its block size; slide them back so
    // `bin[d]` again points at the start of the degree-`d` block.
    for d in (1..=max_deg).rev() {
        bin[d] = bin[d - 1];
    }
    bin[0] = 0;

    // Peel in `vert` order. When `v` is reached its `deg[v]` has been
    // decremented to its final core number; each still-higher-degree neighbor
    // is slid one bucket lower.
    for i in 0..n {
        let v = vert[i];
        note_kcore_vertex_peeled();
        cancel.check()?;
        // The loop mutates `vert`/`pos`/`deg`/`bin` but never `adj`, so
        // iterating `v`'s neighbor slice by reference is sound.
        for &u in &adj[crate::rules::convert::usize_from_u32(v)] {
            if deg[crate::rules::convert::usize_from_u32(u)]
                > deg[crate::rules::convert::usize_from_u32(v)]
            {
                let du = crate::rules::convert::usize_from_u32(
                    deg[crate::rules::convert::usize_from_u32(u)],
                );
                let pu = pos[crate::rules::convert::usize_from_u32(u)];
                let pw = bin[du];
                let w = vert[pw];
                if u != w {
                    vert[pu] = w;
                    pos[crate::rules::convert::usize_from_u32(w)] = pu;
                    vert[pw] = u;
                    pos[crate::rules::convert::usize_from_u32(u)] = pw;
                }
                bin[du] += 1;
                deg[crate::rules::convert::usize_from_u32(u)] -= 1;
            }
        }
    }

    Ok(deg)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::rules::contract::tests_support::{TestInput, empty_opts, run_fixed_rule};

    use miette::{IntoDiagnostic, Result, miette};
    fn s(v: &str) -> DataValue {
        DataValue::from(v)
    }

    /// Independent, obviously-correct reference: repeatedly remove a
    /// minimum-degree vertex (smallest id on ties), recording the running
    /// maximum degree-at-removal as its core number (Matula–Beck). `O(V^2)`,
    /// no bucket trickery — a different implementation from the one under
    /// test. Keyed by node name so it is independent of interning order.
    fn naive_coreness(
        nodes: &BTreeSet<String>,
        adj: &BTreeMap<String, BTreeSet<String>>,
    ) -> Result<BTreeMap<String, u32>> {
        use crate::rules::contract::GraphAlgorithmInvariantError;

        let mut deg: BTreeMap<String, i64> = nodes
            .iter()
            .map(|v| -> Result<_> {
                Ok((
                    v.clone(),
                    match adj.get(v) {
                        None => {
                            // Published floor for this absence.
                            0
                        },
                        Some(a) => crate::rules::convert::i64_from_usize(a.len())?,
                    },
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let mut removed: BTreeSet<String> = BTreeSet::new();
        let mut core: BTreeMap<String, u32> = BTreeMap::new();
        let mut k = 0u32;
        while removed.len() < nodes.len() {
            let v = nodes
                .iter()
                .filter(|v| !removed.contains(*v))
                .min_by_key(|v| (deg[*v], (*v).clone()))
                .ok_or_else(|| GraphAlgorithmInvariantError::refuse("k_core_remaining"))?
                .clone();
            let dv = deg[&v].max(0);
            let dv_u32 = match u32::try_from(dv) {
                Ok(x) => x,
                Err(_) => {
                    return Err(GraphAlgorithmInvariantError::refuse("k_core_deg_u32").into());
                }
            };
            k = k.max(dv_u32);
            core.insert(v.clone(), k);
            removed.insert(v.clone());
            if let Some(nbrs) = adj.get(&v) {
                for u in nbrs {
                    if !removed.contains(u) {
                        let d = deg
                            .get_mut(u)
                            .ok_or_else(|| GraphAlgorithmInvariantError::refuse("k_core_deg"))?;
                        *d -= 1;
                    }
                }
            }
        }
        Ok(core)
    }

    /// Build the undirected simple adjacency (keyed by node name) from an
    /// edge list, matching the rule's interpretation: undirected, distinct
    /// neighbors, no self-loops.
    fn adjacency(edges: &[(&str, &str)]) -> (BTreeSet<String>, BTreeMap<String, BTreeSet<String>>) {
        let mut nodes = BTreeSet::new();
        let mut adj: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for &(a, b) in edges {
            nodes.insert(a.to_string());
            nodes.insert(b.to_string());
            if a != b {
                adj.entry(a.to_string()).or_default().insert(b.to_string());
                adj.entry(b.to_string()).or_default().insert(a.to_string());
            }
        }
        (nodes, adj)
    }

    fn run(edges: &[(&str, &str)]) -> Result<BTreeMap<String, u32>> {
        let rows = edges
            .iter()
            .map(|&(a, b)| Tuple::from_vec(vec![s(a), s(b)]))
            .collect::<Vec<_>>();
        let got = run_fixed_rule(
            &KCoreDecomposition,
            vec![TestInput::new(vec!["fr", "to"], rows)],
            empty_opts(),
            CancelFlag::inert(),
        )?;
        got.into_iter()
            .map(|r| -> Result<_> {
                let core = match r[1].get_int() {
                    Some(i) => {
                        u32::try_from(i).map_err(|_| miette!("test core number fits u32"))?
                    }
                    None => return Err(miette!("test core column must be int")),
                };
                Ok((
                    r[0].get_str()
                        .ok_or_else(|| miette!("test expected Some"))?
                        .to_string(),
                    core,
                ))
            })
            .collect()
    }

    /// VALUE ORACLE: a triangle {a,b,c} with a pendant d hanging off a. By
    /// hand: d has degree 1 ⇒ core 1; a,b,c form a 2-core ⇒ core 2. Crucially
    /// a's *degree* is 3 but its *coreness* is 2, so this pins that the peel
    /// actually decrements degrees rather than reporting raw degree.
    #[test]
    fn coreness_differs_from_degree() -> Result<()> {
        let got = run(&[("a", "b"), ("b", "c"), ("a", "c"), ("a", "d")])?;
        assert_eq!(got[&"a".to_string()], 2);
        assert_eq!(got[&"b".to_string()], 2);
        assert_eq!(got[&"c".to_string()], 2);
        assert_eq!(got[&"d".to_string()], 1);
        Ok(())
    }

    /// VALUE ORACLE vs the naive reference on a hand-picked graph with a
    /// 3-core (K4 on {a,b,c,d}), a 2-core rim, and 1-core tails.
    #[test]
    fn matches_naive_reference() -> Result<()> {
        let edges = [
            ("a", "b"),
            ("a", "c"),
            ("a", "d"),
            ("b", "c"),
            ("b", "d"),
            ("c", "d"), // K4 ⇒ core 3
            ("d", "e"),
            ("e", "f"),
            ("f", "d"), // triangle d-e-f ⇒ e,f in a 2-core
            ("f", "g"), // pendant ⇒ g core 1
            ("a", "h"), // pendant ⇒ h core 1
        ];
        let (nodes, adj) = adjacency(&edges);
        let expected = naive_coreness(&nodes, &adj)?;
        assert_eq!(run(&edges)?, expected);
        Ok(())
    }

    /// The undirected/simple interpretation: a self-loop on a node and a
    /// doubled parallel edge must not change any core number. Two runs — one
    /// clean, one with a self-loop and a parallel edge added — agree.
    #[test]
    fn self_loops_and_parallel_edges_ignored() -> Result<()> {
        let clean = run(&[("a", "b"), ("b", "c"), ("a", "c")])?;
        let noisy = run(&[
            ("a", "b"),
            ("b", "c"),
            ("a", "c"),
            ("a", "a"), // self-loop
            ("a", "b"), // parallel edge
        ])?;
        assert_eq!(clean, noisy);
        // The clean triangle is a 2-core throughout.
        for v in ["a", "b", "c"] {
            assert_eq!(clean[&v.to_string()], 2);
        }
        Ok(())
    }

    /// DETERMINISM: coreness is an invariant, so the output is byte-identical
    /// across repeated runs (and, being an invariant, independent of any
    /// peel-order choice). A pseudo-random graph, run several times.
    #[test]
    fn deterministic_across_runs() -> Result<()> {
        let mut state = 0x51ed_2701_dead_c0deu64;
        let mut next = || {
            // INVARIANT(lcg64): Knuth LCG step is defined wrapping on u64.
            state = (std::num::Wrapping(state) * std::num::Wrapping(6364136223846793005)
                + std::num::Wrapping(1442695040888963407))
            .0;
            state
        };
        let owned: Vec<(String, String)> = (0..400)
            .filter_map(|_| {
                let a = (next() >> 33) % 40;
                let b = (next() >> 33) % 40;
                (a != b).then(|| (format!("n{a}"), format!("n{b}")))
            })
            .collect();
        let edges: Vec<(&str, &str)> = owned
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let first = run(&edges)?;
        for _ in 0..5 {
            assert_eq!(run(&edges)?, first);
        }
        Ok(())
    }

    /// ADVERSARIAL SHAPE / SCALE: a 200k-node cycle. Every node has exactly
    /// two distinct neighbors, so the whole graph is a 2-core: coreness 2
    /// everywhere. This is a large, purely iterative peel (no recursion, so
    /// unlike the DFS algorithms there is no stack to overflow) and confirms
    /// the bucket arithmetic holds at scale. The exact answer (all 2s) also
    /// proves correctness, not merely non-crashing.
    #[test]
    fn large_cycle_is_two_core() -> Result<()> {
        let n: u32 = 200_000;
        let owned: Vec<(String, String)> = (0..n)
            .map(|i| (format!("n{i}"), format!("n{}", (i + 1) % n)))
            .collect();
        let edges: Vec<(&str, &str)> = owned
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let got = run(&edges)?;
        assert_eq!(
            u32::try_from(got.len()).map_err(|_| miette!("test got.len fits u32"))?,
            n
        );
        assert!(got.values().all(|&c| c == 2));
        Ok(())
    }

    /// CANCELLATION, inner-poll pinned (house exemplar:
    /// `shortest_path_bfs::honors_cancel_pins_inner_poll`). The baseline run
    /// peels every vertex of a 60k-node path; with a pre-raised flag the
    /// per-vertex poll must refuse before peeling more than one. Deleting
    /// the poll inside the peel loop makes the cancelled run peel all ~60k,
    /// so the `<= 1` bound fails. Load-independent: counts peeled vertices,
    /// not wall-clock.
    #[test]
    fn honors_cancel_pins_inner_poll() -> Result<()> {
        use crate::rules::contract::tests_support::prepare_fixed_rule;

        let n: u32 = 60_000;
        let edges: Vec<_> = (0..n - 1)
            .map(|i| Tuple::from_vec(vec![s(&format!("v{i}")), s(&format!("v{}", i + 1))]))
            .collect();
        let inputs = vec![TestInput::new(vec!["fr", "to"], edges)];
        let prepared = prepare_fixed_rule(&KCoreDecomposition, inputs, empty_opts())?;

        // Baseline: no cancellation. Every vertex is peeled.
        take_kcore_verts_peeled(); // clear any leftover from a reused thread
        let full = prepared.run(&KCoreDecomposition, CancelFlag::inert())?;
        let full_peeled = take_kcore_verts_peeled();
        drop(full); // baseline completed
        assert!(
            full_peeled >= u64::from(n),
            "baseline should peel every vertex, got {full_peeled}"
        );

        // Spent authority: the inner poll must refuse before peeling the graph.
        let (auth, flag) = CancelAuthority::arm();
        let Cancelled = auth.cancel();
        let cancelled = prepared.run(&KCoreDecomposition, flag);
        let cancel_peeled = take_kcore_verts_peeled();
        assert!(cancelled.unwrap_err().to_string().contains("killed"));
        assert!(
            cancel_peeled <= 1,
            "inner poll did not refuse before peeling the graph: peeled \
             {cancel_peeled} vertices (deleting the per-vertex poll makes this ~60k)"
        );
        Ok(())
    }
}
