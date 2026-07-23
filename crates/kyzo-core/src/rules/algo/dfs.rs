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
 * panic); the route-reconstruction `unwrap` is annotated as structural;
 * output rows flow through the arity-checked writer.
 */

//! Depth-first search from starting nodes, collecting up to `limit`
//! nodes satisfying `condition`, with their routes.

use std::collections::{BTreeMap, BTreeSet};

use miette::Result;

use crate::rules::contract::{
    CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload, admit_conditioned_traversal,
    emit_backtrace_routes, traversal_node_tuple,
};
use kyzo_model::SourceSpan;
use kyzo_model::program::rule::FixedRuleOptions;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::{DataValue, Tuple};

pub(crate) struct Dfs;

impl FixedRule for Dfs {
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
            // INVARIANT(dfs_start_col): `ensure_min_len(1)` proved a first column.
            let starting_node = &node_tuple[0];
            if visited.contains(starting_node) {
                continue;
            }

            let mut to_visit_stack: Vec<DataValue> = vec![];
            to_visit_stack.push(starting_node.clone());

            while let Some(candidate) = to_visit_stack.pop() {
                // Polled at the top of the per-node unit of work so no
                // early exit (`continue` on visited, `break` on limit) can
                // complete a run past a raised flag.
                cancel.check()?;
                if visited.contains(&candidate) {
                    continue;
                }

                let cand_tuple =
                    traversal_node_tuple(t.nodes, &candidate, t.skip_query_nodes, &candidate)?;

                if crate::exec::expr::eval_pred(&t.condition, &cand_tuple)? {
                    found.push((starting_node.clone(), candidate.clone()));
                    if found.len() >= t.limit {
                        break 'outer;
                    }
                }

                visited.insert(candidate.clone());

                for edge in t.edges.prefix_iter(&candidate)? {
                    let edge = edge?;
                    let to_node = &edge[1];
                    if visited.contains(to_node) {
                        continue;
                    }
                    backtrace.insert(to_node.clone(), candidate.clone());
                    to_visit_stack.push(to_node.clone());
                }
            }
        }

        emit_backtrace_routes(found, &backtrace, out, "dfs_pred", Some(&cancel))
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

    /// VALUE ORACLE: exact DFS answer on the same diamond BFS is tested
    /// on (a→{b,c}, b→d, c→d; `condition: true`, `limit: 10`) — the two
    /// traversals are distinguished by their exact outputs.
    ///
    /// Hand trace (explicit LIFO stack; a node's edges push in sorted
    /// order, so the LAST-sorted neighbor pops first):
    ///   pop a: found (a,a,[a]) — DFS tests the popped node itself, so
    ///          unlike BFS the start appears; push b, push c
    ///   pop c: found (a,c,[a,c]); push d (backtrace d→c)
    ///   pop d: found (a,d,[a,c,d]) — via c, where BFS went via b
    ///   pop b: found (a,b,[a,b]); d already visited
    /// ⇒ exactly four rows, with d's route through c.
    #[test]
    fn exact_traversal_and_routes() -> Result<()> {
        assert_diamond_traversal_routes(
            &Dfs,
            &[
                ("a", "a", &["a"]),
                ("a", "b", &["a", "b"]),
                ("a", "c", &["a", "c"]),
                ("a", "d", &["a", "c", "d"]),
            ],
        )
    }
}
