/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The in-memory graph the fixed-rule algorithms run on: a directed graph
//! in compressed-sparse-row form, plus the relation→graph builders that
//! intern edge endpoints to dense `u32` ids.
//!
//! New in KyzoDB. The CozoDB original used the external `graph` crate
//! (v0.3.1) for this type; that crate is effectively dormant (last release
//! 2023) and drags a ~20-crate subtree (`rayon` mandatorily, `dashmap`,
//! `memmap2`, `page_size`→`libc`, `parking_lot`, `num`, …) plus heavy
//! `unsafe`, for what the algorithms actually consume: CSR construction
//! from an edge list and neighbor iteration. This file replaces exactly
//! that surface, dependency-free and safe.
//!
//! Behavioral parity notes, verified against the original's usage:
//! - Node count is derived from the edges (max endpoint id + 1; `0` for an
//!   empty edge list), as the original `GraphBuilder` did — except that the
//!   `+ 1` is checked: an endpoint of `u32::MAX` refuses with the typed
//!   [`GraphTooLargeError`] where the original overflowed.
//! - Adjacency segments are sorted by target (the original always built
//!   with `CsrLayout::Sorted`); parallel edges are kept, not deduplicated.
//! - Both out- and in-adjacency are materialized, as in the original
//!   `DirectedCsrGraph` (`in_neighbors` is load-bearing for PageRank).
//! - `out_neighbors` yields `u32` by value where the original yielded
//!   `&u32`; call sites were adjusted (mechanical only).

use std::collections::BTreeMap;

use miette::{Diagnostic, Result, bail, ensure};
use thiserror::Error;

use kyzo_model::SourceSpan;
use kyzo_model::value::DataValue;

use crate::rules::contract::FixedRuleInputRelation;

/// Refusal at the graph-size bound: node ids are dense `u32`s, so a
/// fixed-rule graph holds at most 2^32 - 1 nodes (the `max_id + 1`
/// count must fit in `u32`; predecessor absence uses `Option`, not a
/// reserved id). The CozoDB original truncated silently (`len as u32`
/// wraps); KyzoDB refuses, typed.
///
/// Honesty note on testability: hitting this bound for real needs ~4
/// billion distinct nodes, which no test allocates. The guard is pure
/// arithmetic, factored into [`checked_node_count`] (here) and
/// [`checked_node_id`] (the intern site) precisely so
/// unit tests can pin the boundary math without the allocation; the
/// end-to-end path is exercised only up to the type level (the error
/// exists, is typed, and is returned by the factored checks at the
/// boundary values).
#[derive(Debug, Error, Diagnostic)]
#[error("Graph too large: fixed-rule graphs are limited to 2^32 - 1 nodes")]
#[diagnostic(code(algo::graph_too_large))]
#[diagnostic(help(
    "Node values are interned to dense `u32` ids; this input relation has \
     too many distinct nodes to index"
))]
pub(crate) struct GraphTooLargeError;

/// The node-count derivation `max endpoint id + 1` (`0` for an empty edge
/// list), guarded: an endpoint id of `u32::MAX` would overflow the `u32`
/// node count, so it refuses with [`GraphTooLargeError`] instead of
/// wrapping. Factored out of [`DirectedCsrGraph::from_edges`] so the
/// boundary math is unit-testable without 4-billion-node allocations.
pub(crate) fn checked_node_count(max_endpoint: Option<u32>) -> Result<u32> {
    match max_endpoint {
        None => Ok(0),
        Some(m) => {
            let count = m.checked_add(1);
            ensure!(count.is_some(), GraphTooLargeError);
            Ok(count.ok_or(GraphTooLargeError)?)
        }
    }
}

/// One outgoing edge as seen from a source node: the field names mirror
/// the original crate's `Target` so the algorithm bodies read unchanged.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct Target<W> {
    pub(crate) target: u32,
    pub(crate) value: W,
}

/// A directed graph over dense `u32` node ids in CSR form, with one weight
/// of type `W` per edge (`()` for unweighted graphs — a zero-sized slot).
#[derive(Debug)]
pub(crate) struct DirectedCsrGraph<W: Copy = ()> {
    /// `out_offsets[n]..out_offsets[n + 1]` indexes `n`'s slice of
    /// `out_edges`; length is `node_count + 1`. Same shape for `in_*`.
    out_offsets: Vec<usize>,
    out_edges: Vec<(u32, W)>,
    in_offsets: Vec<usize>,
    in_edges: Vec<u32>,
    node_count: u32,
}

impl<W: Copy> DirectedCsrGraph<W> {
    /// Build from an edge list. Endpoint ids are dense indices minted by
    /// the caller (the payload's interning builders), so `max id + 1` is
    /// the node count — the same derivation the original crate used, but
    /// guarded: an endpoint of `u32::MAX` refuses with
    /// [`GraphTooLargeError`] instead of overflowing (the original's `+ 1`
    /// wrapped in release builds).
    pub(crate) fn from_edges(edges: impl IntoIterator<Item = (u32, u32, W)>) -> Result<Self> {
        let edges: Vec<(u32, u32, W)> = edges.into_iter().collect();
        let node_count = checked_node_count(edges.iter().map(|(f, t, _)| (*f).max(*t)).max())?;
        let n = crate::rules::convert::usize_from_u32(node_count);

        // Counting sort into CSR: degree pass, prefix sums, then placement.
        let mut out_offsets = vec![0usize; n + 1];
        let mut in_offsets = vec![0usize; n + 1];
        for (f, t, _) in &edges {
            out_offsets[crate::rules::convert::usize_from_u32(*f) + 1] += 1;
            in_offsets[crate::rules::convert::usize_from_u32(*t) + 1] += 1;
        }
        for i in 0..n {
            out_offsets[i + 1] += out_offsets[i];
            in_offsets[i + 1] += in_offsets[i];
        }
        // Placement via per-node write cursors. Seed out_edges with the first
        // edge's weight as a overwriteable placeholder so every slot is a real
        // `(target, weight)` — no Option intermediate, no unwrap. Empty edge
        // lists stay empty; non-empty lists overwrite every slot exactly once
        // (cursors advance one per edge within disjoint segments).
        let mut out_edges: Vec<(u32, W)> = match edges.first() {
            None => Vec::new(),
            Some((_, _, w)) => vec![(0, *w); edges.len()],
        };
        let mut in_edges: Vec<u32> = vec![0; edges.len()];
        let mut out_cursor = out_offsets.clone();
        let mut in_cursor = in_offsets.clone();
        for (f, t, w) in &edges {
            out_edges[out_cursor[crate::rules::convert::usize_from_u32(*f)]] = (*t, *w);
            out_cursor[crate::rules::convert::usize_from_u32(*f)] += 1;
            in_edges[in_cursor[crate::rules::convert::usize_from_u32(*t)]] = *f;
            in_cursor[crate::rules::convert::usize_from_u32(*t)] += 1;
        }

        // `CsrLayout::Sorted` parity: each adjacency segment sorted by
        // target id; parallel edges kept.
        for node in 0..n {
            out_edges[out_offsets[node]..out_offsets[node + 1]].sort_by_key(|(t, _)| *t);
            in_edges[in_offsets[node]..in_offsets[node + 1]].sort_unstable();
        }

        Ok(Self {
            out_offsets,
            out_edges,
            in_offsets,
            in_edges,
            node_count,
        })
    }

    pub(crate) fn node_count(&self) -> u32 {
        self.node_count
    }

    pub(crate) fn out_degree(&self, node: u32) -> usize {
        let lo = self.out_offsets[crate::rules::convert::usize_from_u32(node)];
        let hi = self.out_offsets[crate::rules::convert::usize_from_u32(node) + 1];
        hi - lo
    }

    pub(crate) fn out_neighbors(&self, node: u32) -> impl Iterator<Item = u32> + '_ {
        self.out_edges[self.out_offsets[crate::rules::convert::usize_from_u32(node)]
            ..self.out_offsets[crate::rules::convert::usize_from_u32(node) + 1]]
            .iter()
            .map(|(t, _)| *t)
    }

    /// The `idx`-th out-neighbor of `node`, in the same target-sorted order
    /// as [`Self::out_neighbors`], or `None` once `idx` reaches the
    /// out-degree. O(1) — for cursor-driven iterative DFS (see the iterative
    /// Tarjan in `algos/strongly_connected_components.rs`), where repeatedly
    /// re-scanning the adjacency with `nth` would be quadratic in degree.
    pub(crate) fn out_neighbor(&self, node: u32, idx: u32) -> Option<u32> {
        self.out_edges[self.out_offsets[crate::rules::convert::usize_from_u32(node)]
            ..self.out_offsets[crate::rules::convert::usize_from_u32(node) + 1]]
            .get(crate::rules::convert::usize_from_u32(idx))
            .map(|(t, _)| *t)
    }

    pub(crate) fn out_neighbors_with_values(
        &self,
        node: u32,
    ) -> impl Iterator<Item = Target<W>> + '_ {
        self.out_edges[self.out_offsets[crate::rules::convert::usize_from_u32(node)]
            ..self.out_offsets[crate::rules::convert::usize_from_u32(node) + 1]]
            .iter()
            .map(|(t, w)| Target {
                target: *t,
                value: *w,
            })
    }

    pub(crate) fn in_neighbors(&self, node: u32) -> impl Iterator<Item = u32> + '_ {
        self.in_edges[self.in_offsets[crate::rules::convert::usize_from_u32(node)]
            ..self.in_offsets[crate::rules::convert::usize_from_u32(node) + 1]]
            .iter()
            .copied()
    }
}

#[derive(Error, Diagnostic, Debug)]
#[error("The relation cannot be interpreted as an edge")]
#[diagnostic(code(algo::not_an_edge))]
#[diagnostic(help("Edge relation requires tuples of length at least two"))]
struct NotAnEdgeError(#[label] SourceSpan);

#[derive(Error, Diagnostic, Debug)]
#[error(
    "The value {0:?} at the third position in the relation cannot be interpreted as edge weights"
)]
#[diagnostic(code(algo::invalid_edge_weight))]
#[diagnostic(help(
    "Edge weights must be finite numbers. Some algorithm also requires positivity."
))]
struct BadEdgeWeightError(DataValue, #[label] SourceSpan);

/// Mints the next dense node id at the intern site, refusing with the
/// typed [`GraphTooLargeError`] at the 2^32-node bound — the CozoDB
/// original's `indices.len() as u32` silently truncated there, aliasing
/// the 2^32-th node onto id 0. The cap is `u32::MAX` mintable ids
/// (`0..=u32::MAX - 1`); predecessor absence uses `Option` (P078), so
/// `u32::MAX` is no longer reserved as a sentinel.
///
/// The bound is untestable at scale (it would take ~4 billion interned
/// values); it is factored into this function precisely so a unit test
/// can pin the boundary arithmetic without the allocation. See the
/// honesty note on [`GraphTooLargeError`].
pub(crate) fn checked_node_id(interned_so_far: usize) -> Result<u32> {
    ensure!(
        interned_so_far < crate::rules::convert::usize_from_u32(u32::MAX),
        GraphTooLargeError
    );
    u32::try_from(interned_so_far).map_err(|_| GraphTooLargeError.into())
}

/// The first two columns of each tuple as an edge, interning the node
/// values to dense `u32` ids. Shared skeleton of the two graph builders
/// below; errors flow straight out (the original collected them into a
/// captured `Option<Report>` inside a `filter_map` and re-raised after
/// the build). Minting is guarded by [`checked_node_id`].
pub(crate) fn intern_edges<'a, W: Copy>(
    rel: &FixedRuleInputRelation<'a>,
    mut weight: impl FnMut(Option<&DataValue>) -> Result<W>,
    undirected: bool,
) -> Result<(Vec<(u32, u32, W)>, Vec<DataValue>, BTreeMap<DataValue, u32>)> {
    let mut indices: Vec<DataValue> = vec![];
    let mut inv_indices: BTreeMap<DataValue, u32> = Default::default();
    let mut edges: Vec<(u32, u32, W)> = vec![];
    for tuple in rel.iter()? {
        let mut tuple = tuple?.into_iter();
        let from = tuple.next().ok_or_else(|| NotAnEdgeError(rel.span()))?;
        let to = tuple.next().ok_or_else(|| NotAnEdgeError(rel.span()))?;
        let mut intern = |val: DataValue| -> Result<u32> {
            Ok(match inv_indices.get(&val) {
                Some(idx) => *idx,
                None => {
                    let idx = checked_node_id(indices.len())?;
                    inv_indices.insert(val.clone(), idx);
                    indices.push(val);
                    idx
                }
            })
        };
        let from_idx = intern(from)?;
        let to_idx = intern(to)?;
        let w = weight(tuple.next().as_ref())?;
        edges.push((from_idx, to_idx, w));
        if undirected {
            edges.push((to_idx, from_idx, w));
        }
    }
    Ok((edges, indices, inv_indices))
}

/// Convert an input relation into a directed graph.
/// If `undirected` is true, then each edge in the input relation is treated
/// as a pair of edges, one for each direction.
pub(crate) fn as_directed_graph(
    rel: &FixedRuleInputRelation<'_>,
    undirected: bool,
) -> Result<(DirectedCsrGraph, Vec<DataValue>, BTreeMap<DataValue, u32>)> {
    let (edges, indices, inv_indices) = intern_edges(rel, |_| Ok(()), undirected)?;
    Ok((DirectedCsrGraph::from_edges(edges)?, indices, inv_indices))
}

/// Convert an input relation into a directed weighted graph, the weight
/// taken from the third column (`1.0` when absent). Weights must be finite
/// numbers, and non-negative unless `allow_negative_weights`.
pub(crate) fn as_directed_weighted_graph(
    rel: &FixedRuleInputRelation<'_>,
    undirected: bool,
    allow_negative_weights: bool,
    weight_span: SourceSpan,
) -> Result<(
    DirectedCsrGraph<f64>,
    Vec<DataValue>,
    BTreeMap<DataValue, u32>,
)> {
    let (edges, indices, inv_indices) = intern_edges(
        rel,
        |d| -> Result<f64> {
            let d = match d {
                None => return Ok(1.0),
                Some(d) => d,
            };
            let f = d
                .get_float()
                .ok_or_else(|| BadEdgeWeightError(d.clone(), weight_span))?;
            if !f.is_finite() || (f < 0. && !allow_negative_weights) {
                bail!(BadEdgeWeightError(d.clone(), weight_span));
            }
            Ok(f)
        },
        undirected,
    )?;
    Ok((DirectedCsrGraph::from_edges(edges)?, indices, inv_indices))
}

#[cfg(test)]
mod tests {
    use super::*;

    use miette::{IntoDiagnostic, Result, miette};
    #[test]
    fn csr_shape_and_iteration() -> Result<()> {
        // 0→1, 0→2 (parallel ×2), 2→0; node 1 is a sink.
        let g: DirectedCsrGraph<f64> =
            DirectedCsrGraph::from_edges([(0, 2, 1.0), (0, 1, 2.0), (2, 0, 3.0), (0, 2, 4.0)])?;
        assert_eq!(g.node_count(), 3);
        assert_eq!(g.out_degree(0), 3);
        assert_eq!(g.out_degree(1), 0);
        // Sorted by target, parallel edges kept.
        let n0: Vec<_> = g.out_neighbors(0).collect();
        assert_eq!(n0, vec![1, 2, 2]);
        let w0: Vec<_> = g.out_neighbors_with_values(0).map(|t| t.value).collect();
        assert_eq!(w0[0], 2.0);
        assert_eq!({ w0[1] + w0[2] }, 5.0);
        assert_eq!(g.out_neighbors(1).count(), 0);
        // In-adjacency.
        let in2: Vec<_> = g.in_neighbors(2).collect();
        assert_eq!(in2, vec![0, 0]);
        assert_eq!(g.in_neighbors(0).collect::<Vec<_>>(), vec![2]);
        Ok(())
    }

    #[test]
    fn empty_graph() -> Result<()> {
        let g: DirectedCsrGraph = DirectedCsrGraph::from_edges([])?;
        assert_eq!(g.node_count(), 0);
        Ok(())
    }

    /// F3: the node-count derivation refuses, typed, at the 2^32 bound
    /// instead of overflowing `max(*t) + 1`. The bound itself cannot be
    /// reached end-to-end in a test (it needs ~4B nodes), so this pins the
    /// boundary math of the factored check:
    ///   - no edges            → 0 nodes
    ///   - max id 0            → 1 node
    ///   - max id u32::MAX - 1 → u32::MAX nodes (the last representable)
    ///   - max id u32::MAX     → GraphTooLargeError (the `+ 1` would wrap)
    #[test]
    fn node_count_refuses_at_u32_bound() -> Result<()> {
        assert_eq!(checked_node_count(None)?, 0);
        assert_eq!(checked_node_count(Some(0))?, 1);
        assert_eq!(checked_node_count(Some(u32::MAX - 1))?, u32::MAX);
        let err = checked_node_count(Some(u32::MAX)).unwrap_err();
        assert!(
            err.downcast_ref::<GraphTooLargeError>().is_some(),
            "expected the typed GraphTooLargeError, got: {err}"
        );
        assert!(err.to_string().contains("2^32"), "{err}");

        // The same refusal surfaces through `from_edges` itself.
        let err = DirectedCsrGraph::<()>::from_edges([(0, u32::MAX, ())]).unwrap_err();
        assert!(err.downcast_ref::<GraphTooLargeError>().is_some(), "{err}");
        Ok(())
    }

    /// F3: the intern site refuses, typed, at the 2^32-node bound instead
    /// of truncating `indices.len() as u32`.
    #[test]
    fn intern_site_refuses_at_u32_bound() -> Result<()> {
        assert_eq!(checked_node_id(0)?, 0);
        assert_eq!(
            checked_node_id(crate::rules::convert::usize_from_u32(u32::MAX - 1))?,
            u32::MAX - 1
        );
        let err = checked_node_id(crate::rules::convert::usize_from_u32(u32::MAX)).unwrap_err();
        assert!(
            err.downcast_ref::<GraphTooLargeError>().is_some(),
            "expected the typed GraphTooLargeError, got: {err}"
        );
        assert!(err.to_string().contains("2^32"), "{err}");
        Ok(())
    }
}
