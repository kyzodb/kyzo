/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): `binding_indices` now returns `Result` (typed instead of a
 * panic on an unresolved binding); the route-reconstruction `unwrap` is
 * annotated as structural; output rows flow through the arity-checked
 * writer.
 */

//! Breadth-first search from starting nodes, collecting up to `limit`
//! nodes satisfying `condition`, with their routes.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use miette::Result;

use crate::rules::contract::{
    CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload, admit_conditioned_traversal,
    emit_backtrace_routes, traversal_node_tuple,
};
use kyzo_model::SourceSpan;
use kyzo_model::program::rule::FixedRuleOptions;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::{DataValue, Tuple};

pub(crate) struct Bfs;

impl FixedRule for Bfs {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let t = admit_conditioned_traversal(payload)?;
        let mut visited: BTreeSet<DataValue> = Default::default();
        let mut backtrace: BTreeMap<DataValue, DataValue> = Default::default();
        let mut found: Vec<(DataValue, DataValue)> = vec![];

        'outer: for node_tuple in t.starting_nodes.iter()? {
            let node_tuple = node_tuple?;
            // INVARIANT(bfs_start_col): `ensure_min_len(1)` proved a first column.
            let starting_node = &node_tuple[0];
            if visited.contains(starting_node) {
                continue;
            }
            visited.insert(starting_node.clone());

            let mut queue: VecDeque<DataValue> = VecDeque::default();
            queue.push_front(starting_node.clone());

            while let Some(candidate) = queue.pop_back() {
                for edge in t.edges.prefix_iter(&candidate)? {
                    // Polled at the top of the per-edge unit of work so no
                    // early exit (`continue` on visited, `break` on limit)
                    // can complete a run past a raised flag.
                    cancel.check()?;
                    let edge = edge?;
                    let to_node = &edge[1];
                    if visited.contains(to_node) {
                        continue;
                    }

                    visited.insert(to_node.clone());
                    backtrace.insert(to_node.clone(), candidate.clone());

                    let cand_tuple =
                        traversal_node_tuple(t.nodes, to_node, t.skip_query_nodes, &candidate)?;

                    if crate::exec::expr::eval_pred(&t.condition, &cand_tuple)? {
                        found.push((starting_node.clone(), to_node.clone()));
                        if found.len() >= t.limit {
                            break 'outer;
                        }
                    }

                    queue.push_front(to_node.clone());
                }
            }
        }

        emit_backtrace_routes(found, &backtrace, out, "bfs_pred", None)
    }

    fn arity(
        &self,
        _options: &FixedRuleOptions,
        _rule_head: &[Symbol],
        _span: SourceSpan,
    ) -> Result<usize> {
        Ok(3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::contract::tests_support::assert_diamond_traversal_routes;

    use miette::Result;

    /// VALUE ORACLE: exact BFS answer on the diamond a→{b,c}, b→d, c→d
    /// with `condition: true`, `limit: 10`.
    ///
    /// Hand trace (queue is FIFO; a node's edges scan in sorted order):
    ///   pop a: discover b (route a,b), discover c (route a,c)
    ///   pop b: discover d via b ⇒ route (a,b,d)
    ///   pop c: d already visited
    ///   pop d: no edges
    /// The start node itself is never tested against the condition
    /// (upstream-parity: only nodes reached over an edge are "found"), so
    /// exactly three rows, and d's route goes through b, not c.
    #[test]
    fn exact_traversal_and_routes() -> Result<()> {
        assert_diamond_traversal_routes(
            &Bfs,
            &[
                ("a", "b", &["a", "b"]),
                ("a", "c", &["a", "c"]),
                ("a", "d", &["a", "b", "d"]),
            ],
        )
    }
}
