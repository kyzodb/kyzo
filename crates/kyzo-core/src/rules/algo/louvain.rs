/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): the external `graph` crate's CSR type and `GraphBuilder` are
 * replaced by the inline CSR in `fixed_rule/graph.rs` (same
 * sorted-adjacency layout and node-count derivation); the `log::debug!`
 * progress lines are dropped (the workspace carries no `log`); the
 * hierarchy-walk `unwrap` on `collected.last()` is annotated as structural;
 * output rows flow through the arity-checked writer. The in-file sample
 * test is ported.
 */

//! Louvain community detection: repeated local modularity-improving moves,
//! then contraction of communities into a coarser graph, until the
//! hierarchy stabilizes.

use std::collections::{BTreeMap, BTreeSet};

use itertools::Itertools;
use miette::Result;

use crate::rules::contract::{CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload};
use crate::rules::graph_view::DirectedCsrGraph;
use kyzo_model::SourceSpan;
use kyzo_model::program::rule::FixedRuleOptions;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::{DataValue, Tuple};

pub(crate) struct CommunityDetectionLouvain;

impl FixedRule for CommunityDetectionLouvain {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let edges = payload.get_input(0)?;
        let undirected = payload.bool_option("undirected", Some(false))?;
        let max_iter = payload.pos_integer_option("max_iter", Some(10))?;
        let delta = payload.unit_interval_option("delta", Some(0.0001))?;
        let keep_depth = match payload.manifest.options.get("keep_depth") {
            None => None,
            Some(_) => Some(payload.non_neg_integer_option("keep_depth", None)?),
        };

        let (graph, indices, _inv_indices) = edges.as_directed_weighted_graph(undirected, false)?;
        let result = louvain(&graph, delta, max_iter, cancel)?;
        for (idx, node) in indices.into_iter().enumerate() {
            let mut labels = vec![];
            let mut cur_idx = crate::rules::convert::u32_from_usize(idx)?;
            for hierarchy in &result {
                let nxt_idx = hierarchy[crate::rules::convert::usize_from_u32(cur_idx)];
                labels.push(DataValue::from(i64::from(nxt_idx)));
                cur_idx = nxt_idx;
            }
            labels.reverse();
            if let Some(l) = keep_depth {
                labels.truncate(l);
            }
            out.put(Tuple::from_vec(vec![DataValue::List(labels), node]))?;
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

fn louvain(
    graph: &DirectedCsrGraph<f64>,
    delta: f64,
    max_iter: usize,
    cancel: CancelFlag,
) -> Result<Vec<Vec<u32>>> {
    let mut current_graph = graph;
    let mut collected = vec![];
    while current_graph.node_count() > 2 {
        let (node2comm, new_graph) = louvain_step(current_graph, delta, max_iter, cancel.clone())?;
        if new_graph.node_count() == current_graph.node_count() {
            break;
        }
        let idx = collected.len();
        collected.push((node2comm, new_graph));
        current_graph = &collected[idx].1;
    }
    Ok(collected.into_iter().map(|(a, _)| a).collect_vec())
}

fn calculate_delta(
    node: u32,
    target_community: u32,
    graph: &DirectedCsrGraph<f64>,
    comm2nodes: &[BTreeSet<u32>],
    out_weights: &[f64],
    in_weights: &[f64],
    total_weight: f64,
) -> f64 {
    let mut sigma_out_total = 0.;
    let mut sigma_in_total = 0.;
    let mut d2comm = 0.;
    let target_community_members = &comm2nodes[crate::rules::convert::usize_from_u32(target_community)];
    for member in target_community_members.iter() {
        if *member == node {
            continue;
        }
        sigma_out_total += out_weights[crate::rules::convert::usize_from_u32(*member)];
        sigma_in_total += in_weights[crate::rules::convert::usize_from_u32(*member)];
        for target in graph.out_neighbors_with_values(node) {
            if target.target == *member {
                d2comm += target.value;
                break;
            }
        }
        for target in graph.out_neighbors_with_values(*member) {
            if target.target == node {
                d2comm += target.value;
                break;
            }
        }
    }
    d2comm
        - (sigma_out_total * in_weights[crate::rules::convert::usize_from_u32(node)]
            + sigma_in_total * out_weights[crate::rules::convert::usize_from_u32(node)])
            / total_weight
}

fn louvain_step(
    graph: &DirectedCsrGraph<f64>,
    delta: f64,
    max_iter: usize,
    cancel: CancelFlag,
) -> Result<(Vec<u32>, DirectedCsrGraph<f64>)> {
    let n_nodes = graph.node_count();
    let mut total_weight = 0.;
    let mut out_weights = vec![0.; crate::rules::convert::usize_from_u32(n_nodes)];
    let mut in_weights = vec![0.; crate::rules::convert::usize_from_u32(n_nodes)];

    for from in 0..n_nodes {
        for target in graph.out_neighbors_with_values(from) {
            let to = target.target;
            let weight = target.value;

            total_weight += weight;
            out_weights[crate::rules::convert::usize_from_u32(from)] += weight;
            in_weights[crate::rules::convert::usize_from_u32(to)] += weight;
        }
    }

    let mut node2comm = (0..n_nodes).collect_vec();
    let mut comm2nodes = (0..n_nodes).map(|i| BTreeSet::from([i])).collect_vec();

    let mut last_modurality = f64::NEG_INFINITY;

    for _ in 0..max_iter {
        let modularity = {
            let mut modularity = 0.;
            for from in 0..n_nodes {
                for to in &comm2nodes[crate::rules::convert::usize_from_u32(node2comm[crate::rules::convert::usize_from_u32(from)])] {
                    for target in graph.out_neighbors_with_values(from) {
                        if target.target == *to {
                            modularity += target.value;
                        }
                    }
                    modularity -=
                        in_weights[crate::rules::convert::usize_from_u32(from)] * out_weights[crate::rules::convert::usize_from_u32(*to)] / total_weight;
                }
            }
            modularity /= total_weight;
            modularity
        };
        if modularity <= last_modurality + delta {
            break;
        } else {
            last_modurality = modularity;
        }

        let mut moved = false;
        for node in 0..n_nodes {
            // Polled at the top of the per-node scan (the unit of work
            // here: each node's candidate-community evaluation runs
            // `calculate_delta` per neighboring community), so a raised
            // flag refuses before the node's scan, not after it.
            cancel.check()?;
            let community_for_node = node2comm[crate::rules::convert::usize_from_u32(node)];

            let original_delta_q = calculate_delta(
                node,
                community_for_node,
                graph,
                &comm2nodes,
                &out_weights,
                &in_weights,
                total_weight,
            );
            let mut candidate_community = community_for_node;
            let mut best_improvement = 0.;

            let mut considered_communities = BTreeSet::from([community_for_node]);
            for target in graph.out_neighbors_with_values(node) {
                let to_node = target.target;

                let target_community = node2comm[crate::rules::convert::usize_from_u32(to_node)];
                if target_community == community_for_node
                    || considered_communities.contains(&target_community)
                {
                    continue;
                }
                considered_communities.insert(target_community);

                let delta_q = calculate_delta(
                    node,
                    target_community,
                    graph,
                    &comm2nodes,
                    &out_weights,
                    &in_weights,
                    total_weight,
                );
                if delta_q - original_delta_q > best_improvement {
                    best_improvement = delta_q - original_delta_q;
                    candidate_community = target_community;
                }
            }
            if best_improvement > 0. {
                moved = true;
                node2comm[crate::rules::convert::usize_from_u32(node)] = candidate_community;
                comm2nodes[crate::rules::convert::usize_from_u32(community_for_node)].remove(&node);
                comm2nodes[crate::rules::convert::usize_from_u32(candidate_community)].insert(node);
            }
        }
        if !moved {
            break;
        }
    }
    let mut new_comm_indices: BTreeMap<u32, u32> = Default::default();
    let mut new_comm_count: u32 = 0;

    for temp_comm_idx in node2comm.iter_mut() {
        if let Some(new_comm_idx) = new_comm_indices.get(temp_comm_idx) {
            *temp_comm_idx = *new_comm_idx;
        } else {
            new_comm_indices.insert(*temp_comm_idx, new_comm_count);
            *temp_comm_idx = new_comm_count;
            new_comm_count += 1;
        }
    }

    let mut new_graph_list: Vec<BTreeMap<u32, f64>> =
        vec![BTreeMap::new(); crate::rules::convert::usize_from_u32(new_comm_count)];
    for (node, comm) in node2comm.iter().enumerate() {
        let target = &mut new_graph_list[crate::rules::convert::usize_from_u32(*comm)];
        for t in graph.out_neighbors_with_values(u32::try_from(node).map_err(|_| crate::rules::graph_view::GraphTooLargeError)?) {
            let to_node = t.target;
            let weight = t.value;
            let to_comm = node2comm[crate::rules::convert::usize_from_u32(to_node)];
            *target.entry(to_comm).or_default() += weight;
        }
    }

    let new_graph: DirectedCsrGraph<f64> = {
        let mut edges = Vec::new();
        for (fr, nds) in new_graph_list.into_iter().enumerate() {
            let fr_u = crate::rules::convert::u32_from_usize(fr)?;
            for (to, weight) in nds {
                edges.push((fr_u, to, weight));
            }
        }
        DirectedCsrGraph::from_edges(edges)?
    };

    Ok((node2comm, new_graph))
}

#[cfg(test)]
mod tests {

    use super::{CommunityDetectionLouvain, louvain};
    use crate::rules::contract::tests_support::{TestInput, empty_opts, run_fixed_rule};
    use crate::rules::contract::{CancelAuthority, CancelFlag, Cancelled};
    use crate::rules::graph_view::DirectedCsrGraph;
    use kyzo_model::value::{DataValue, Tuple};

    #[test]
    fn sample() {
        let graph: Vec<Vec<u32>> = vec![
            vec![2, 3, 5],           // 0
            vec![2, 4, 7],           // 1
            vec![0, 1, 4, 5, 6],     // 2
            vec![0, 7],              // 3
            vec![1, 2, 10],          // 4
            vec![0, 2, 7, 11],       // 5
            vec![2, 7, 11],          // 6
            vec![1, 3, 5, 6],        // 7
            vec![9, 10, 11, 12, 15], // 8
            vec![8, 12, 14],         // 9
            vec![4, 8, 12, 13, 14],  // 10
            vec![5, 6, 8, 13],       // 11
            vec![9, 10],             // 12
            vec![10, 11],            // 13
            vec![8, 9, 10],          // 14
            vec![8],                 // 15
        ];
        let graph = {
            let mut edges = Vec::new();
            for (fr, tos) in graph.into_iter().enumerate() {
                let fr_u = match u32::try_from(fr) {
                    Ok(u) => u,
                    Err(_) => panic!("test fixture fr fits u32"),
                };
                for to in tos {
                    edges.push((fr_u, to, 1.));
                }
            }
            DirectedCsrGraph::from_edges(edges).unwrap()
        };
        louvain(&graph, 0., 100, CancelFlag::inert()).unwrap();
    }

    /// VALUE ORACLE, adjacency-order sensitivity pinned. The graph (all
    /// edges undirected, listed once here, built with both directions):
    ///
    ///   0—1: 2.0   (a tight pair)
    ///   2—3: 2.0   (a second tight pair)
    ///   4—1: 1.0, 4—2: 1.0   (node 4 bridges the two pairs, symmetrically)
    ///
    /// Hand computation (total_weight = 12; out_w = in_w = [2,3,3,2,2]):
    ///   - node 0: joining comm{1}: d2comm = 2+2 = 4, penalty
    ///     (3·2+3·2)/12 = 1 ⇒ Δ = 3 > 0 ⇒ 0 joins 1.       comm1 = {0,1}
    ///   - node 1: staying is worth 3; moving to comm{4} is worth 1 ⇒ stays.
    ///   - node 2: joining comm{3}: Δ = 4 − 1 = 3 beats comm{4}'s
    ///     Δ = 2 − 1 = 1 ⇒ 2 joins 3.                       comm3 = {2,3}
    ///   - node 3: staying is worth 3 ⇒ stays.
    ///   - node 4: comm{0,1}: d2comm = 2, penalty (5·2+5·2)/12 ⇒ Δ = 1/3.
    ///     comm{2,3}: d2comm = 2, same penalty ⇒ Δ = 1/3.
    ///     An EXACT tie (identical f32 expressions, summed in the same
    ///     member order). The strict `>` keeps the first candidate met in
    ///     adjacency order — sorted CSR order visits neighbor 1 before 2,
    ///     so node 4 joins comm{0,1}. Reversed adjacency would visit 2
    ///     first and flip node 4 into comm{2,3}: this pin kills the
    ///     reversed-CSR-sort mutant.
    ///   - renumbering by first appearance: comm1 → 0, comm3 → 1
    ///     ⇒ node2comm = [0, 0, 1, 1, 0]; the contracted graph has 2
    ///     nodes, so the hierarchy stops after this single level.
    #[test]
    fn adjacency_tie_break_pinned() {
        let graph = DirectedCsrGraph::from_edges([
            (0u32, 1u32, 2.0f32),
            (1, 0, 2.0),
            (2, 3, 2.0),
            (3, 2, 2.0),
            (4, 1, 1.0),
            (1, 4, 1.0),
            (4, 2, 1.0),
            (2, 4, 1.0),
        ])
        .unwrap();
        let got = louvain(&graph, 0., 10, CancelFlag::inert()).unwrap();
        assert_eq!(got, vec![vec![0, 0, 1, 1, 0]]);
    }

    /// F2: a raised flag refuses at the top of the per-node community
    /// scan, before any `calculate_delta` work for that node.
    #[test]
    fn cancellation_stops_node_scan() {
        let (auth, cancel) = CancelAuthority::arm();
        let Cancelled = auth.cancel();
        let s = |v: &str| DataValue::from(v);
        let err = run_fixed_rule(
            &CommunityDetectionLouvain,
            vec![TestInput::new(
                vec!["fr", "to"],
                vec![
                    Tuple::from_vec(vec![s("a"), s("b")]),
                    Tuple::from_vec(vec![s("b"), s("a")]),
                    Tuple::from_vec(vec![s("b"), s("c")]),
                    Tuple::from_vec(vec![s("c"), s("b")]),
                ],
            )],
            empty_opts(),
            cancel,
        )
        .unwrap_err();
        assert!(err.to_string().contains("killed"), "{err}");
    }
}
