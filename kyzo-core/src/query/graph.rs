/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): Tarjan's SCC and the reachability walk run on explicit work
 * stacks (the original recursed once per graph edge, so a rule chain a few
 * thousand deep overflowed the thread stack); `generalized_kahn`'s
 * in-degree bookkeeping is checked in every build with a typed internal
 * error (the original guarded it with a `debug_assert_eq!` that release
 * builds skipped, so a cyclic or corrupted reduced graph would silently
 * yield a truncated stratification — wrong answers, not a refusal); node
 * indices arriving as data are validated up front instead of trusted; the
 * unused `Debug` bounds are dropped; the cooperative-cancellation
 * (`Poison`) check inside the SCC driver returns with the runtime tier,
 * which owns that substance.
 */

//! Graph algorithms under the stratifier: strongly connected components
//! (Tarjan), reachability, and a generalized topological sort (Kahn).
//!
//! These are the mechanical half of stratification (`query/stratify.rs`):
//! Tarjan finds the recursive families of a program's dependency graph, and
//! the generalized Kahn's algorithm lays their condensation out into strata
//! such that no "poisoned" (stratum-forcing) edge sits inside a stratum.
//! Rule dependency graphs are user-shaped — a generated program can be one
//! chain ten thousand rules deep — so both traversals here run on explicit
//! work stacks, never on the thread stack.

use std::cmp::min;
use std::collections::{BTreeMap, BTreeSet};

use miette::{Diagnostic, Result, ensure};
use thiserror::Error;

/// A directed graph as an adjacency map: node → successors.
pub(crate) type Graph<T> = BTreeMap<T, Vec<T>>;

/// A directed graph whose edges carry a poison flag: node → (successor →
/// poisoned). A poisoned edge is one that [`generalized_kahn`] must not
/// leave inside a single stratum.
pub(crate) type StratifiedGraph<T> = BTreeMap<T, BTreeMap<T, bool>>;

/// An invariant these algorithms maintain internally was found broken.
/// Returned (never panicked, and never only debug-asserted) so corrupt
/// bookkeeping surfaces as a bug report instead of a silently wrong
/// stratification.
#[derive(Debug, Diagnostic, Error)]
#[error("Graph algorithm invariant violated: {0}")]
#[diagnostic(code(compiler::graph_invariant))]
#[diagnostic(help("This is a bug. Please report it."))]
struct GraphInvariantError(&'static str);

/// The strongly connected components of `graph`, computed by an iterative
/// Tarjan's algorithm. Edges pointing outside the key set are ignored (the
/// stratifier's graphs mention undefined names; those resolve elsewhere).
///
/// Components are returned keyed by their root's DFS discovery order, each
/// component's members in ascending node order — the same output, member
/// for member, as the CozoDB original's recursive formulation.
pub(crate) fn strongly_connected_components<T: Ord>(graph: &Graph<T>) -> Result<Vec<Vec<&T>>> {
    let indices: Vec<&T> = graph.keys().collect();
    let invert_indices: BTreeMap<&T, usize> = indices
        .iter()
        .enumerate()
        .map(|(idx, k)| (*k, idx))
        .collect();
    let idx_graph: Vec<Vec<usize>> = graph
        .values()
        .map(|vs| {
            vs.iter()
                .filter_map(|v| invert_indices.get(v).copied())
                .collect()
        })
        .collect();
    let mut ret = Vec::new();
    for group in TarjanScc::new(&idx_graph)?.run() {
        let mut component = Vec::with_capacity(group.len());
        for i in group {
            component.push(
                *indices
                    .get(i)
                    .ok_or(GraphInvariantError("SCC member index out of range"))?,
            );
        }
        ret.push(component);
    }
    Ok(ret)
}

/// Every node reachable from `start` (including `start` itself), walked
/// iteratively on a worklist. Nodes absent from the key set contribute no
/// successors.
pub(crate) fn reachable_components<'a, T: Ord>(
    graph: &'a Graph<T>,
    start: &'a T,
) -> BTreeSet<&'a T> {
    let mut collected = BTreeSet::from([start]);
    let mut pending = vec![start];
    while let Some(at) = pending.pop() {
        if let Some(children) = graph.get(at) {
            for el in children {
                if collected.insert(el) {
                    pending.push(el);
                }
            }
        }
    }
    collected
}

/// For this generalized Kahn's algorithm, graph edges can be labelled
/// 'poisoned', so that no stratum contains any poisoned edges within it.
/// The returned vector of vectors is simultaneously a topological ordering
/// and a stratification, which is greedy with respect to the starting node.
///
/// Node identities must be exactly `0..num_nodes`, and every id appearing
/// in `graph` must be below `num_nodes`; the ids and edge counts are
/// validated, and the in-degree bookkeeping is checked at every step and at
/// exit. A graph that is not a DAG (the condensation the stratifier feeds
/// in always is) is therefore a typed error here, never a silently
/// truncated result — the CozoDB original checked this only with a
/// `debug_assert_eq!` that release builds compiled out.
pub(crate) fn generalized_kahn(
    graph: &StratifiedGraph<usize>,
    num_nodes: usize,
) -> Result<Vec<Vec<usize>>> {
    let mut in_degree = vec![0usize; num_nodes];
    for (fr, tos) in graph {
        ensure!(
            *fr < num_nodes,
            GraphInvariantError("Kahn edge source out of range")
        );
        for to in tos.keys() {
            let d = in_degree
                .get_mut(*to)
                .ok_or(GraphInvariantError("Kahn edge target out of range"))?;
            *d += 1;
        }
    }
    let mut ret = vec![];
    let mut current_stratum = vec![];
    // Nodes with no unprocessed dependents, placeable in the current
    // stratum; and nodes reached over a poisoned edge from the current
    // stratum, which must wait for the next one.
    let mut safe_pending = vec![];
    let mut unsafe_nodes: BTreeSet<usize> = BTreeSet::new();

    for (node, degree) in in_degree.iter().enumerate() {
        if *degree == 0 {
            safe_pending.push(node);
        }
    }

    loop {
        if safe_pending.is_empty() && !unsafe_nodes.is_empty() {
            // Stratum boundary: everything placeable is placed, and only
            // poison-blocked nodes remain. Close the stratum and release
            // the blocked nodes whose dependents are all processed.
            ret.push(std::mem::take(&mut current_stratum));
            for node in &unsafe_nodes {
                // In range: `unsafe_nodes` only ever holds validated edge
                // targets.
                if in_degree[*node] == 0 {
                    safe_pending.push(*node);
                }
            }
            unsafe_nodes.clear();
        }
        let removed = match safe_pending.pop() {
            Some(node) => node,
            None => {
                if !current_stratum.is_empty() {
                    ret.push(current_stratum);
                }
                break;
            }
        };
        current_stratum.push(removed);
        if let Some(edges) = graph.get(&removed) {
            for (nxt, poisoned) in edges {
                let d = in_degree
                    .get_mut(*nxt)
                    .ok_or(GraphInvariantError("Kahn edge target out of range"))?;
                *d = d
                    .checked_sub(1)
                    .ok_or(GraphInvariantError("Kahn in-degree underflow"))?;
                if *poisoned {
                    unsafe_nodes.insert(*nxt);
                }
                if *d == 0 && !unsafe_nodes.contains(nxt) {
                    safe_pending.push(*nxt);
                }
            }
        }
    }
    // The original's graph.rs:129 `debug_assert_eq!`, as a real invariant:
    // every edge must have been consumed, or some node was never emitted.
    ensure!(
        in_degree.iter().all(|d| *d == 0),
        GraphInvariantError("generalized Kahn did not consume every edge (cyclic input?)")
    );
    Ok(ret)
}

/// Tarjan's strongly-connected-components state over an index graph.
///
/// The traversal is the classic recursive DFS re-expressed on an explicit
/// frame stack: each frame is `(node, cursor)` where `cursor` is the next
/// child edge to examine. Opening a node assigns its discovery id; closing
/// it (cursor exhausted) performs the SCC-root check and then propagates
/// its low-link to its parent frame — exactly the work the recursive
/// version does after each nested call returns.
struct TarjanScc<'a> {
    graph: &'a [Vec<usize>],
    id: usize,
    ids: Vec<Option<usize>>,
    low: Vec<usize>,
    on_stack: Vec<bool>,
    stack: Vec<usize>,
}

impl<'a> TarjanScc<'a> {
    /// Validates every edge target up front, so all index arithmetic in the
    /// traversal below is proven in-range once, here.
    fn new(graph: &'a [Vec<usize>]) -> Result<Self> {
        for tos in graph {
            for to in tos {
                ensure!(
                    *to < graph.len(),
                    GraphInvariantError("SCC edge target out of range")
                );
            }
        }
        Ok(Self {
            graph,
            id: 0,
            ids: vec![None; graph.len()],
            low: vec![0; graph.len()],
            on_stack: vec![false; graph.len()],
            stack: vec![],
        })
    }

    fn run(mut self) -> Vec<Vec<usize>> {
        // SEAM: runtime tier. The original checked the session's `Poison`
        // (cooperative query cancellation) after each DFS root here; that
        // substance lives in `runtime/db.rs` and returns with it.
        for i in 0..self.graph.len() {
            if self.ids[i].is_none() {
                self.dfs(i);
            }
        }

        // After every DFS completes, `low` holds each node's component
        // label (its root's discovery id): grouping by it yields the SCCs.
        let mut low_map: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (idx, grp) in self.low.into_iter().enumerate() {
            low_map.entry(grp).or_default().push(idx);
        }

        low_map.into_values().collect()
    }

    /// Assign `at` its discovery id and put it on the component stack.
    fn open(&mut self, at: usize) {
        self.stack.push(at);
        self.on_stack[at] = true;
        self.id += 1;
        self.ids[at] = Some(self.id);
        self.low[at] = self.id;
    }

    /// One DFS from `root`, on an explicit frame stack. All indexing below
    /// is in-range: nodes come from `0..graph.len()` and from edge targets
    /// validated in [`Self::new`].
    fn dfs(&mut self, root: usize) {
        self.open(root);
        let mut frames: Vec<(usize, usize)> = vec![(root, 0)];
        while let Some(&(at, cursor)) = frames.last() {
            match self.graph[at].get(cursor) {
                Some(&to) => {
                    if let Some(frame) = frames.last_mut() {
                        frame.1 += 1;
                    }
                    if self.ids[to].is_none() {
                        // The recursive call: descend into `to`; the
                        // post-return low propagation happens when its
                        // frame closes below.
                        self.open(to);
                        frames.push((to, 0));
                    } else if self.on_stack[to] {
                        self.low[at] = min(self.low[at], self.low[to]);
                    }
                }
                None => {
                    // All children examined: close this frame.
                    frames.pop();
                    if self.ids[at] == Some(self.low[at]) {
                        // `at` roots its component: pop the component off
                        // the stack, labelling every member with the root's
                        // id (`low[at]` equals it here).
                        let label = self.low[at];
                        while let Some(node) = self.stack.pop() {
                            self.on_stack[node] = false;
                            self.low[node] = label;
                            if node == at {
                                break;
                            }
                        }
                    }
                    // What the recursive version does right after `dfs(to)`
                    // returns: if the child is still on the component
                    // stack, its low-link constrains the parent's.
                    if let Some(&(parent, _)) = frames.last()
                        && self.on_stack[at]
                    {
                        self.low[parent] = min(self.low[parent], self.low[at]);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn graph_of(edges: &[(usize, usize)], n: usize) -> Graph<usize> {
        let mut g: Graph<usize> = (0..n).map(|i| (i, vec![])).collect();
        for (fr, to) in edges {
            if let Some(children) = g.get_mut(fr) {
                children.push(*to);
            }
        }
        g
    }

    /// SCCs as an order-free partition, for comparing against the oracle.
    fn scc_partition(graph: &Graph<usize>) -> BTreeSet<BTreeSet<usize>> {
        strongly_connected_components(graph)
            .expect("valid graph")
            .into_iter()
            .map(|c| c.into_iter().copied().collect())
            .collect()
    }

    /// The obviously-correct SCC oracle: `u` and `v` share a component iff
    /// each reaches the other, via a boolean transitive closure.
    fn naive_scc(graph: &Graph<usize>, n: usize) -> BTreeSet<BTreeSet<usize>> {
        let mut reach = vec![vec![false; n]; n];
        for (fr, tos) in graph {
            reach[*fr][*fr] = true;
            for to in tos {
                reach[*fr][*to] = true;
            }
        }
        for (i, row) in reach.iter_mut().enumerate() {
            row[i] = true;
        }
        for k in 0..n {
            for i in 0..n {
                for j in 0..n {
                    if reach[i][k] && reach[k][j] {
                        reach[i][j] = true;
                    }
                }
            }
        }
        (0..n)
            .map(|u| {
                (0..n)
                    .filter(|&v| reach[u][v] && reach[v][u])
                    .collect::<BTreeSet<usize>>()
            })
            .collect()
    }

    /// The CozoDB original's *recursive* Tarjan, kept verbatim (minus the
    /// `Poison` plumbing) as a test oracle: the iterative rewrite must
    /// produce identical output — same components, same component order,
    /// same member order. Only run on small graphs, where its recursion is
    /// safe.
    struct RecursiveTarjan<'a> {
        graph: &'a [Vec<usize>],
        id: usize,
        ids: Vec<Option<usize>>,
        low: Vec<usize>,
        on_stack: Vec<bool>,
        stack: Vec<usize>,
    }

    impl<'a> RecursiveTarjan<'a> {
        fn new(graph: &'a [Vec<usize>]) -> Self {
            Self {
                graph,
                id: 0,
                ids: vec![None; graph.len()],
                low: vec![0; graph.len()],
                on_stack: vec![false; graph.len()],
                stack: vec![],
            }
        }
        fn run(mut self) -> Vec<Vec<usize>> {
            for i in 0..self.graph.len() {
                if self.ids[i].is_none() {
                    self.dfs(i);
                }
            }
            let mut low_map: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
            for (idx, grp) in self.low.into_iter().enumerate() {
                low_map.entry(grp).or_default().push(idx);
            }
            low_map.into_values().collect()
        }
        fn dfs(&mut self, at: usize) {
            self.stack.push(at);
            self.on_stack[at] = true;
            self.id += 1;
            self.ids[at] = Some(self.id);
            self.low[at] = self.id;
            for to in &self.graph[at] {
                let to = *to;
                if self.ids[to].is_none() {
                    self.dfs(to);
                }
                if self.on_stack[to] {
                    self.low[at] = min(self.low[at], self.low[to]);
                }
            }
            if self.ids[at] == Some(self.low[at]) {
                while let Some(node) = self.stack.pop() {
                    self.on_stack[node] = false;
                    self.low[node] = self.low[at];
                    if node == at {
                        break;
                    }
                }
            }
        }
    }

    #[test]
    fn scc_on_a_known_graph() {
        // Two 2-cycles bridged by a one-way edge, plus a free-standing node.
        let g = graph_of(&[(0, 1), (1, 0), (1, 2), (2, 3), (3, 2)], 5);
        let want: BTreeSet<BTreeSet<usize>> = [
            BTreeSet::from([0, 1]),
            BTreeSet::from([2, 3]),
            BTreeSet::from([4]),
        ]
        .into_iter()
        .collect();
        assert_eq!(scc_partition(&g), want);
    }

    #[test]
    fn scc_single_node_with_and_without_self_loop() {
        // A self-loop and its absence both yield the singleton component:
        // the stratifier distinguishes them by the edge, not the SCC.
        assert_eq!(
            scc_partition(&graph_of(&[(0, 0)], 1)),
            scc_partition(&graph_of(&[], 1))
        );
    }

    #[test]
    fn scc_ignores_edges_to_undefined_nodes() {
        // Node 0 points at a name that is not a key (the stratifier's
        // graphs mention undefined rules); it must simply not count.
        let mut g: Graph<usize> = BTreeMap::from([(0, vec![7]), (1, vec![0])]);
        g.entry(1).or_default();
        let want: BTreeSet<BTreeSet<usize>> = [BTreeSet::from([0]), BTreeSet::from([1])]
            .into_iter()
            .collect();
        assert_eq!(scc_partition(&g), want);
    }

    proptest! {
        /// The iterative Tarjan agrees with the naive reachability oracle
        /// on random graphs: identical component partitions.
        #[test]
        fn scc_matches_naive_reachability_oracle(
            n in 1usize..12,
            edges in proptest::collection::vec((0usize..12, 0usize..12), 0..60)
        ) {
            let edges: Vec<(usize, usize)> = edges
                .into_iter()
                .map(|(a, b)| (a % n, b % n))
                .collect();
            let g = graph_of(&edges, n);
            prop_assert_eq!(scc_partition(&g), naive_scc(&g, n));
        }

        /// The iterative Tarjan is *output-identical* to the original
        /// recursive formulation: same components, same order, same member
        /// order — not merely the same partition.
        #[test]
        fn iterative_tarjan_is_output_identical_to_recursive(
            n in 1usize..12,
            edges in proptest::collection::vec((0usize..12, 0usize..12), 0..60)
        ) {
            let mut idx_graph: Vec<Vec<usize>> = vec![vec![]; n];
            for (a, b) in edges {
                idx_graph[a % n].push(b % n);
            }
            let iterative = TarjanScc::new(&idx_graph)
                .expect("validated targets")
                .run();
            let recursive = RecursiveTarjan::new(&idx_graph).run();
            prop_assert_eq!(iterative, recursive);
        }

        /// Kahn's output on a random poisoned DAG is a stratification:
        /// every node exactly once; every edge points into the same or a
        /// later stratum; every poisoned edge into a strictly later one.
        #[test]
        fn kahn_output_is_a_stratification(
            n in 1usize..12,
            edges in proptest::collection::vec((0usize..12, 0usize..12, any::<bool>()), 0..60)
        ) {
            // Orient every edge low → high so the graph is a DAG.
            let mut g: StratifiedGraph<usize> = BTreeMap::new();
            for (a, b, poisoned) in edges {
                let (a, b) = (a % n, b % n);
                if a == b {
                    continue;
                }
                let (fr, to) = (a.min(b), a.max(b));
                let e = g.entry(fr).or_default().entry(to).or_insert(false);
                *e = *e || poisoned;
            }
            let strata = generalized_kahn(&g, n).expect("a DAG stratifies");
            let mut stratum_of: BTreeMap<usize, usize> = BTreeMap::new();
            for (idx, stratum) in strata.iter().enumerate() {
                for node in stratum {
                    prop_assert!(
                        stratum_of.insert(*node, idx).is_none(),
                        "node emitted twice"
                    );
                }
            }
            prop_assert_eq!(stratum_of.len(), n, "every node emitted");
            for (fr, tos) in &g {
                for (to, poisoned) in tos {
                    prop_assert!(stratum_of[to] >= stratum_of[fr]);
                    if *poisoned {
                        prop_assert!(stratum_of[to] > stratum_of[fr]);
                    }
                }
            }
        }
    }

    #[test]
    fn kahn_poisoned_edges_split_strata() {
        // 0 → 1 poisoned, 0 → 2 clean: 1 must wait a stratum, 2 need not.
        let g: StratifiedGraph<usize> =
            BTreeMap::from([(0, BTreeMap::from([(1, true), (2, false)]))]);
        let strata = generalized_kahn(&g, 3).expect("stratifies");
        assert_eq!(strata, vec![vec![0, 2], vec![1]]);
    }

    #[test]
    fn kahn_refuses_a_cyclic_graph() {
        // The condensation the stratifier feeds in is always a DAG; a cycle
        // here means corrupt bookkeeping. The original debug_assert let a
        // release build return a truncated stratification for this input.
        let g: StratifiedGraph<usize> = BTreeMap::from([
            (0, BTreeMap::from([(1, false)])),
            (1, BTreeMap::from([(0, false)])),
        ]);
        let err = generalized_kahn(&g, 2).expect_err("cycle must be refused");
        assert!(err.to_string().contains("invariant"), "got: {err}");
    }

    #[test]
    fn kahn_refuses_out_of_range_nodes() {
        let g: StratifiedGraph<usize> = BTreeMap::from([(0, BTreeMap::from([(9, false)]))]);
        assert!(generalized_kahn(&g, 2).is_err());
        let g: StratifiedGraph<usize> = BTreeMap::from([(9, BTreeMap::new())]);
        assert!(generalized_kahn(&g, 2).is_err());
    }

    /// The reason these traversals are iterative: a deep chain must not
    /// overflow the stack. Run in a deliberately small-stack thread — the
    /// original recursive formulations overflow it.
    #[test]
    fn deep_chain_does_not_overflow_the_stack() {
        let handle = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                const N: usize = 50_000;
                let edges: Vec<(usize, usize)> = (0..N - 1).map(|i| (i, i + 1)).collect();
                let g = graph_of(&edges, N);
                let sccs = strongly_connected_components(&g).expect("chain is valid");
                assert_eq!(sccs.len(), N, "a chain is all singletons");
                let reached = reachable_components(&g, &0);
                assert_eq!(reached.len(), N);
            })
            .expect("spawn test thread");
        handle.join().expect("no stack overflow");
    }
}
