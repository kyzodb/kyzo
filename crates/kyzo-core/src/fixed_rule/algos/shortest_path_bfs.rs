/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): the original's file header was duplicated verbatim
 * (copy-paste); once is enough. The `into_iter().next().unwrap()` on
 * starting/ending rows is annotated as structural (`ensure_min_len(1)`
 * proved a first column exists); the route-reconstruction `unwrap`
 * likewise. Output rows flow through the arity-checked writer. The
 * original's in-file test drove a full `DbInstance`; it is ported onto
 * the payload harness (no runtime yet) with the same graph and the same
 * two assertions.
 * CANCELLATION FIX (deliberate, pinned vs upstream): the inner BFS
 * traversal polled nothing — the only cancel check sat in the outer
 * per-start loop, so a single start over a huge reachable set could not be
 * interrupted. The flag is now polled once per node dequeued; `check` only
 * reads it, so an unset flag leaves the routes unchanged. Pinned by
 * `honors_cancel_pins_inner_poll` below — which is constructed so that only
 * the inner poll (not the pre-existing per-start poll) can stop the run.
 */

//! Single-pair shortest paths by BFS: one path per (start, end) pair,
//! `Null` where unreachable.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use itertools::Itertools;
use miette::Result;
use smartstring::{LazyCompact, SmartString};

use crate::data::expr::Expr;
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::{DataValue, Tuple};
use crate::fixed_rule::{CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload};

// A test-only observable: how many nodes the inner BFS loop has dequeued.
// It lets `honors_cancel_pins_inner_poll` assert a *deterministic* effect of
// the per-node poll (nodes expanded under a pre-set flag) instead of a
// load-sensitive wall-clock ratio. `thread_local` so parallel test threads
// don't interfere. In a non-test build `note_bfs_node_expanded` is an empty
// inlined no-op, so the production loop is unchanged.
#[cfg(test)]
thread_local! {
    static BFS_NODES_EXPANDED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_bfs_node_expanded() {
    BFS_NODES_EXPANDED.with(|c| c.set(c.get() + 1));
}

/// Reset the counter and return what it held (for the cancellation test).
#[cfg(test)]
fn take_bfs_nodes_expanded() -> u64 {
    BFS_NODES_EXPANDED.with(|c| c.replace(0))
}

#[cfg(not(test))]
#[inline(always)]
fn note_bfs_node_expanded() {}

pub(crate) struct ShortestPathBFS;

impl FixedRule for ShortestPathBFS {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let edges = payload.get_input(0)?.ensure_min_len(2)?;
        let starting_nodes: Vec<_> = payload
            .get_input(1)?
            .ensure_min_len(1)?
            .iter()?
            // Structural: `ensure_min_len(1)` proved every tuple has a
            // first column.
            .map_ok(|n| n.into_iter().next().unwrap())
            .try_collect()?;
        let ending_nodes: BTreeSet<_> = payload
            .get_input(2)?
            .ensure_min_len(1)?
            .iter()?
            .map_ok(|n| n.into_iter().next().unwrap())
            .try_collect()?;

        for starting_node in starting_nodes.iter() {
            let mut pending: BTreeSet<_> = ending_nodes.clone();
            let mut visited: BTreeSet<DataValue> = Default::default();
            let mut backtrace: BTreeMap<DataValue, DataValue> = Default::default();

            visited.insert(starting_node.clone());

            let mut queue: VecDeque<DataValue> = VecDeque::default();
            queue.push_front(starting_node.clone());

            while let Some(candidate) = queue.pop_back() {
                // Count this dequeue (test-only observable; no-op in
                // production), BEFORE the poll — so a pre-set flag lets at
                // most this one node through before the poll refuses.
                note_bfs_node_expanded();
                // Poll once per node dequeued: the inner traversal is
                // unbounded in the reachable-set size, so a single huge start
                // was uninterruptible when the only poll sat in the outer
                // per-start loop. `check` only reads the flag, so an unset
                // flag leaves the discovered routes unchanged.
                cancel.check()?;
                for edge in edges.prefix_iter(&candidate)? {
                    let edge = edge?;
                    let to_node = &edge[1];
                    if visited.contains(to_node) {
                        continue;
                    }

                    visited.insert(to_node.clone());
                    backtrace.insert(to_node.clone(), candidate.clone());

                    pending.remove(to_node);

                    if pending.is_empty() {
                        break;
                    }

                    queue.push_front(to_node.clone());
                }
            }

            for ending_node in ending_nodes.iter() {
                if backtrace.contains_key(ending_node) {
                    let mut route = vec![];
                    let mut current = ending_node.clone();
                    while current != *starting_node {
                        route.push(current.clone());
                        // Structural: `ending_node` has a backtrace entry
                        // (checked above), and so does every predecessor
                        // back to the start.
                        current = backtrace.get(&current).unwrap().clone();
                    }
                    route.push(starting_node.clone());
                    route.reverse();
                    let tuple = vec![
                        starting_node.clone(),
                        ending_node.clone(),
                        DataValue::List(route),
                    ];
                    out.put(Tuple::from_vec(tuple))?;
                } else {
                    out.put(Tuple::from_vec(vec![
                        starting_node.clone(),
                        ending_node.clone(),
                        DataValue::Null,
                    ]))?
                }
            }
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
        Ok(3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::value::Tuple;
    use crate::fixed_rule::tests_support::{TestInput, run_fixed_rule};

    fn s(v: &str) -> DataValue {
        DataValue::from(v)
    }

    fn love_edges() -> Vec<Tuple> {
        [
            ("alice", "eve"),
            ("bob", "alice"),
            ("eve", "alice"),
            ("eve", "bob"),
            ("eve", "charlie"),
            ("charlie", "eve"),
            ("david", "george"),
            ("george", "george"),
        ]
        .into_iter()
        .map(|(a, b)| vec![s(a), s(b)])
        .collect()
    }

    /// The original's `test_bfs_path`, on the payload harness instead of a
    /// full database: alice → bob has a 3-node path; alice → george is
    /// unreachable and reports `Null`.
    #[test]
    fn test_bfs_path() {
        let got = run_fixed_rule(
            &ShortestPathBFS,
            vec![
                TestInput::new(vec!["loving", "loved"], love_edges()),
                TestInput::new(vec!["start"], vec![vec![s("alice")]]),
                TestInput::new(vec!["end"], vec![vec![s("bob")]]),
            ],
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap();
        assert_eq!(got[0][2].get_slice().unwrap().len(), 3);

        let got = run_fixed_rule(
            &ShortestPathBFS,
            vec![
                TestInput::new(vec!["loving", "loved"], love_edges()),
                TestInput::new(vec!["start"], vec![vec![s("alice")]]),
                TestInput::new(vec!["end"], vec![vec![s("george")]]),
            ],
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap();
        assert_eq!(got[0][2], DataValue::Null);
    }

    /// CANCELLATION: pins the *inner* per-node poll specifically (the
    /// `cancel.check()?` at the top of the `queue.pop_back()` loop), not the
    /// pre-existing per-start poll that runs after a start's whole BFS.
    ///
    /// The setup makes the inner poll the *only* thing that can stop the run
    /// early: ONE start over a long chain (250k nodes), with an end node that
    /// is absent from the graph so the frontier never empties and the whole
    /// chain would otherwise be traversed. The store is built once (via
    /// `prepare_fixed_rule`) so the O(rows) construction is out of the way.
    ///
    /// The assertion is a DETERMINISTIC observable, not a wall-clock time:
    /// `BFS_NODES_EXPANDED` counts nodes dequeued (incremented before the
    /// poll). The uncancelled baseline expands the whole chain (~250k); with
    /// a pre-set flag the inner poll refuses after at most the first dequeue,
    /// so the cancelled run expands ≤ 1 node. Load-independent — it holds on
    /// a fully-loaded machine. The reviewer's line-`cancel.check()?`-deletion
    /// mutant makes the cancelled run expand the whole chain (~250k), so the
    /// `≤ 1` bound fails; restoring the poll passes.
    #[test]
    fn honors_cancel_pins_inner_poll() {
        use crate::fixed_rule::tests_support::prepare_fixed_rule;

        let n: u32 = 250_000;
        let edges: Vec<Tuple> = (0..n - 1)
            .map(|i| vec![s(&format!("n{i}")), s(&format!("n{}", i + 1))])
            .collect();
        // An end node absent from the graph: the frontier never empties, so
        // without the inner poll the whole chain is walked.
        let inputs = vec![
            TestInput::new(vec!["fr", "to"], edges),
            TestInput::new(vec!["start"], vec![vec![s("n0")]]),
            TestInput::new(vec!["end"], vec![vec![s("absent")]]),
        ];
        let prepared = prepare_fixed_rule(&ShortestPathBFS, inputs, BTreeMap::new()).unwrap();

        // Baseline: no cancellation. The whole chain is expanded.
        take_bfs_nodes_expanded(); // clear any leftover from a reused thread
        let full = prepared.run(&ShortestPathBFS, CancelFlag::default());
        let full_expanded = take_bfs_nodes_expanded();
        assert!(full.is_ok());
        assert!(
            full_expanded > 200_000,
            "baseline should expand the whole chain, got {full_expanded}"
        );

        // Pre-set flag: the inner poll must refuse before expanding the graph.
        let flag = CancelFlag::default();
        flag.cancel();
        let cancelled = prepared.run(&ShortestPathBFS, flag);
        let cancel_expanded = take_bfs_nodes_expanded();
        assert!(cancelled.is_err());
        assert!(
            cancel_expanded <= 1,
            "inner poll did not refuse before expanding the graph: expanded \
             {cancel_expanded} nodes (deleting the per-node poll makes this ~250k)"
        );
    }

    /// VALUE ORACLE: the exact route, not just its length. On a→b, b→c,
    /// a→c, the BFS from a discovers both b and c at depth 1 while
    /// scanning a's edges, so c's backtrace entry is a itself: the
    /// shortest a→c path is the direct edge [a,c], never [a,b,c].
    #[test]
    fn exact_route_prefers_direct_edge() {
        let got = run_fixed_rule(
            &ShortestPathBFS,
            vec![
                TestInput::new(
                    vec!["fr", "to"],
                    vec![
                        vec![s("a"), s("b")],
                        vec![s("b"), s("c")],
                        vec![s("a"), s("c")],
                    ],
                ),
                TestInput::new(vec!["start"], vec![vec![s("a")]]),
                TestInput::new(vec!["end"], vec![vec![s("c")]]),
            ],
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap();
        let want: Vec<Tuple> = vec![vec![s("a"), s("c"), DataValue::List(vec![s("a"), s("c")])]];
        assert_eq!(got, want);
    }
}
