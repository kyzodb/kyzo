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
 * inline one in `fixed_rule/graph.rs` (its `out_neighbors` yields values,
 * so the original's `.cloned()` is gone); dead imports and redundant
 * `graph-algo` cfg gates dropped; output rows flow through the
 * arity-checked writer.
 * LAW-5 FIX (deliberate, pinned vs upstream): `TarjanSccG::dfs` was
 * recursive — one stack frame per graph edge, so a stored component a few
 * hundred thousand nodes deep overflowed the thread stack and aborted the
 * whole process (not a typed refusal, a crash). The DFS now runs on an
 * explicit frame stack — the exact iterative Tarjan proven in
 * `query/graph.rs` (`(node, cursor)` frames; open on descent, root-check
 * and low-propagation on close), which produces byte-identical component
 * labels to the recursive version. The cancel flag is polled inside the
 * DFS loop (the original only polled once per DFS root). Pinned by
 * `deep_chain_does_not_overflow` and `cancellation_inside_dfs` below.
 */

//! Strongly connected components (Tarjan); registered as
//! `ConnectedComponents` too, where the graph is built undirected so the
//! SCCs are the weakly connected components.

use std::cmp::min;
use std::collections::BTreeMap;

use itertools::Itertools;
use miette::Result;
use smartstring::{LazyCompact, SmartString};

use crate::data::expr::Expr;
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::{DataValue, Tuple};
use crate::fixed_rule::graph::DirectedCsrGraph;
use crate::fixed_rule::{
    CancelAuthority, CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload,
};

pub(crate) struct StronglyConnectedComponent {
    strong: bool,
}

impl StronglyConnectedComponent {
    pub(crate) fn new(strong: bool) -> Self {
        Self { strong }
    }
}

impl FixedRule for StronglyConnectedComponent {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let edges = payload.get_input(0)?;

        let (graph, indices, mut inv_indices) = edges.as_directed_graph(!self.strong)?;

        let tarjan = TarjanSccG::new(graph).run(cancel)?;
        for (grp_id, cc) in tarjan.iter().enumerate() {
            for idx in cc {
                // Structural: Tarjan only emits node ids the graph handed
                // it, and `indices` has an entry per graph node.
                let val = indices.get(*idx as usize).unwrap();
                let tuple = vec![val.clone(), DataValue::from(grp_id as i64)];
                out.put(Tuple::from_vec(tuple))?;
            }
        }

        let mut counter = tarjan.len() as i64;

        if let Ok(nodes) = payload.get_input(1) {
            // A missing (unbound) nodes relation is the "not provided" case
            // above and skips this block entirely; a PROVIDED nullary
            // relation is a real error, not something to silently ignore —
            // propagate it instead of letting the `unwrap` below panic.
            let nodes = nodes.ensure_min_len(1)?;
            for tuple in nodes.iter()? {
                let tuple = tuple?;
                // Structural: `ensure_min_len(1)` proved every tuple has a
                // first column.
                let node = tuple.into_iter().next().unwrap();
                if !inv_indices.contains_key(&node) {
                    inv_indices.insert(node.clone(), u32::MAX);
                    let tuple = vec![node, DataValue::from(counter)];
                    out.put(Tuple::from_vec(tuple))?;
                    counter += 1;
                }
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
        Ok(2)
    }
}

pub(crate) struct TarjanSccG {
    graph: DirectedCsrGraph,
    id: u32,
    ids: Vec<Option<u32>>,
    low: Vec<u32>,
    on_stack: Vec<bool>,
    stack: Vec<u32>,
}

impl TarjanSccG {
    pub(crate) fn new(graph: DirectedCsrGraph) -> Self {
        let graph_size = graph.node_count();
        Self {
            graph,
            id: 0,
            ids: vec![None; graph_size as usize],
            low: vec![0; graph_size as usize],
            on_stack: vec![false; graph_size as usize],
            stack: vec![],
        }
    }
    pub(crate) fn run(mut self, cancel: CancelFlag) -> Result<Vec<Vec<u32>>> {
        for i in 0..self.graph.node_count() {
            if self.ids[i as usize].is_none() {
                self.dfs(i, &cancel)?;
            }
        }

        let mut low_map: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (idx, grp) in self.low.into_iter().enumerate() {
            low_map.entry(grp).or_default().push(idx as u32);
        }

        Ok(low_map.into_values().collect_vec())
    }

    /// Assign `at` its discovery id and put it on the component stack.
    fn open(&mut self, at: u32) {
        self.stack.push(at);
        self.on_stack[at as usize] = true;
        self.id += 1;
        self.ids[at as usize] = Some(self.id);
        self.low[at as usize] = self.id;
    }

    /// One DFS from `root`, on an explicit `(node, cursor)` frame stack —
    /// the same shape as `query::graph::TarjanScc::dfs`, so a deep graph
    /// spends heap, not thread stack. Byte-identical component labels to the
    /// former recursive version: a fresh child is opened and its low-link
    /// propagates to the parent when its frame closes (guarded by
    /// `on_stack`), exactly what the recursive `if on_stack[to]` after the
    /// nested call did. The cancel flag is polled once per frame step.
    fn dfs(&mut self, root: u32, cancel: &CancelFlag) -> Result<()> {
        self.open(root);
        let mut frames: Vec<(u32, u32)> = vec![(root, 0)];
        while let Some(&(at, cursor)) = frames.last() {
            cancel.check()?;
            // Neighbors are the CSR out-adjacency in target-sorted order —
            // the same sequence the recursive version iterated; O(1) indexed
            // so the cursor walk is linear, not quadratic, in degree.
            match self.graph.out_neighbor(at, cursor) {
                Some(to) => {
                    // Structural: a `last()` that matched above still exists.
                    frames.last_mut().unwrap().1 += 1;
                    if self.ids[to as usize].is_none() {
                        self.open(to);
                        frames.push((to, 0));
                    } else if self.on_stack[to as usize] {
                        self.low[at as usize] = min(self.low[at as usize], self.low[to as usize]);
                    }
                }
                None => {
                    frames.pop();
                    // Structural: `ids[at]` was set to `Some` by `open`.
                    if self.ids[at as usize] == Some(self.low[at as usize]) {
                        let label = self.low[at as usize];
                        while let Some(node) = self.stack.pop() {
                            self.on_stack[node as usize] = false;
                            self.low[node as usize] = label;
                            if node == at {
                                break;
                            }
                        }
                    }
                    // The recursive version's post-return step: if the just-
                    // closed child is still on the component stack, its
                    // low-link constrains the parent's.
                    if let Some(&(parent, _)) = frames.last()
                        && self.on_stack[at as usize]
                    {
                        self.low[parent as usize] =
                            min(self.low[parent as usize], self.low[at as usize]);
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixed_rule::tests_support::{TestInput, run_fixed_rule};

    fn s(v: &str) -> DataValue {
        DataValue::from(v)
    }

    /// a↔b form one SCC; c (reachable but not returning) is its own; the
    /// isolated node from the second input gets a fresh group id.
    #[test]
    fn scc_groups() {
        let got = run_fixed_rule(
            &StronglyConnectedComponent::new(true),
            vec![
                TestInput::new(
                    vec!["fr", "to"],
                    vec![
                        Tuple::from_vec(vec![s("a"), s("b")]),
                        Tuple::from_vec(vec![s("b"), s("a")]),
                        Tuple::from_vec(vec![s("b"), s("c")]),
                    ],
                ),
                TestInput::new(vec!["id"], vec![Tuple::from_vec(vec![s("lonely")])]),
            ],
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap();
        let group_of = |name: &str| -> i64 {
            got.iter().find(|t| t[0] == s(name)).unwrap()[1]
                .get_int()
                .unwrap()
        };
        assert_eq!(group_of("a"), group_of("b"));
        assert_ne!(group_of("a"), group_of("c"));
        assert_ne!(group_of("lonely"), group_of("a"));
        assert_ne!(group_of("lonely"), group_of("c"));
        assert_eq!(got.len(), 4);
    }

    /// LAW-5: a single cycle 0→1→…→(n−1)→0 is one SCC whose DFS descends to
    /// depth n and, on close, unwinds a component stack of all n nodes. At
    /// n = 300_000 the former recursive `dfs` (one stack frame per edge)
    /// overflowed the 8 MiB thread stack and aborted the process; the
    /// iterative frame-stack version spends heap and returns. The exact
    /// answer (one component of every node) also proves the iterative
    /// low-link propagation is correct at depth, not merely non-crashing.
    #[test]
    fn deep_chain_does_not_overflow() {
        let n: u32 = 300_000;
        let edges = (0..n).map(|i| (i, (i + 1) % n, ()));
        let graph = DirectedCsrGraph::from_edges(edges).unwrap();
        let sccs = TarjanSccG::new(graph).run(CancelFlag::default()).unwrap();
        assert_eq!(sccs.len(), 1);
        assert_eq!(sccs[0].len(), n as usize);
    }

    /// CANCELLATION: `run` no longer polls outside the DFS (the recursive
    /// version's per-root `cancel.check()` is gone), so interruptibility now
    /// depends entirely on the poll inside `dfs`. A raised flag over a single
    /// SCC — one DFS call spanning many frame steps — must still refuse;
    /// removing the in-DFS poll makes this run complete `Ok` and fail here.
    #[test]
    fn cancellation_inside_dfs() {
        let graph = DirectedCsrGraph::from_edges([(0u32, 1u32, ()), (1, 0, ())]).unwrap();
        let (auth, flag) = CancelAuthority::arm();
        let _ = auth.cancel();
        assert!(TarjanSccG::new(graph).run(flag).is_err());
    }
}
