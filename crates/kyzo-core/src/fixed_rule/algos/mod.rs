/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): the `graph-algo` feature gate is gone — the ported algorithms
 * are dependency-free pure Rust and always compiled.
 */

//! The built-in graph algorithms, each a [`crate::fixed_rule::FixedRule`]:
//! traversals (BFS/DFS), shortest paths (BFS, Dijkstra, A*, Yen's k-),
//! centralities (degree, closeness, betweenness, PageRank), components
//! (SCC/CC, topological sort), community structure (Louvain, label
//! propagation, triangles/clustering coefficients), spanning trees (Prim,
//! Kruskal), and random walks.

pub(crate) mod all_pairs_shortest_path;
pub(crate) mod astar;
pub(crate) mod bfs;
pub(crate) mod degree_centrality;
pub(crate) mod dfs;
pub(crate) mod k_core;
pub(crate) mod kruskal;
pub(crate) mod label_propagation;
pub(crate) mod louvain;
pub(crate) mod max_flow;
pub(crate) mod maximal_cliques;
pub(crate) mod pagerank;
pub(crate) mod prim;
pub(crate) mod random_walk;
pub(crate) mod shortest_path_bfs;
pub(crate) mod shortest_path_dijkstra;
pub(crate) mod strongly_connected_components;
pub(crate) mod top_sort;
pub(crate) mod triangles;
pub(crate) mod yen;

pub(crate) use all_pairs_shortest_path::{BetweennessCentrality, ClosenessCentrality};
pub(crate) use astar::ShortestPathAStar;
pub(crate) use bfs::Bfs;
pub(crate) use degree_centrality::DegreeCentrality;
pub(crate) use dfs::Dfs;
pub(crate) use k_core::KCoreDecomposition;
pub(crate) use kruskal::MinimumSpanningForestKruskal;
pub(crate) use label_propagation::LabelPropagation;
pub(crate) use louvain::CommunityDetectionLouvain;
pub(crate) use max_flow::MaxFlow;
pub(crate) use maximal_cliques::MaximalCliques;
pub(crate) use pagerank::PageRank;
pub(crate) use prim::MinimumSpanningTreePrim;
pub(crate) use random_walk::RandomWalk;
pub(crate) use shortest_path_bfs::ShortestPathBFS;
pub(crate) use shortest_path_dijkstra::ShortestPathDijkstra;
pub(crate) use strongly_connected_components::StronglyConnectedComponent;
pub(crate) use top_sort::TopSort;
pub(crate) use triangles::ClusteringCoefficients;
pub(crate) use yen::KShortestPathYen;
