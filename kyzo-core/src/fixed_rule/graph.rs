/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The in-memory graph the fixed-rule algorithms run on: a directed graph
//! in compressed-sparse-row form, nodes interned to dense `u32` ids by the
//! payload's graph builders (`fixed_rule/mod.rs`).
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

use miette::{Diagnostic, Result, ensure};
use thiserror::Error;

/// Refusal at the graph-size bound: node ids are dense `u32`s, so a
/// fixed-rule graph holds at most 2^32 - 1 nodes (`u32::MAX` itself is
/// reserved — the Dijkstra core uses it as the "no back-pointer"
/// sentinel). The CozoDB original truncated silently (`len as u32`
/// wraps); KyzoDB refuses, typed.
///
/// Honesty note on testability: hitting this bound for real needs ~4
/// billion distinct nodes, which no test allocates. The guard is pure
/// arithmetic, factored into [`checked_node_count`] (here) and
/// `checked_node_id` (the intern site in `fixed_rule/mod.rs`) precisely so
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
            // Structural: the `ensure!` above proved `is_some`.
            Ok(count.unwrap())
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
        let n = node_count as usize;

        // Counting sort into CSR: degree pass, prefix sums, then placement.
        let mut out_offsets = vec![0usize; n + 1];
        let mut in_offsets = vec![0usize; n + 1];
        for (f, t, _) in &edges {
            out_offsets[*f as usize + 1] += 1;
            in_offsets[*t as usize + 1] += 1;
        }
        for i in 0..n {
            out_offsets[i + 1] += out_offsets[i];
            in_offsets[i + 1] += in_offsets[i];
        }
        // Placement via per-node write cursors.
        let mut out_edges: Vec<Option<(u32, W)>> = vec![None; edges.len()];
        let mut in_edges: Vec<u32> = vec![0; edges.len()];
        let mut out_cursor = out_offsets.clone();
        let mut in_cursor = in_offsets.clone();
        for (f, t, w) in &edges {
            out_edges[out_cursor[*f as usize]] = Some((*t, *w));
            out_cursor[*f as usize] += 1;
            in_edges[in_cursor[*t as usize]] = *f;
            in_cursor[*t as usize] += 1;
        }
        // Placement fills every slot exactly once (cursors advance one per
        // edge within disjoint segments) — the `None`s cannot survive.
        let mut out_edges: Vec<(u32, W)> = out_edges.into_iter().map(|e| e.unwrap()).collect();

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

    pub(crate) fn out_degree(&self, node: u32) -> u32 {
        (self.out_offsets[node as usize + 1] - self.out_offsets[node as usize]) as u32
    }

    pub(crate) fn out_neighbors(&self, node: u32) -> impl Iterator<Item = u32> + '_ {
        self.out_edges[self.out_offsets[node as usize]..self.out_offsets[node as usize + 1]]
            .iter()
            .map(|(t, _)| *t)
    }

    /// The `idx`-th out-neighbor of `node`, in the same target-sorted order
    /// as [`Self::out_neighbors`], or `None` once `idx` reaches the
    /// out-degree. O(1) — for cursor-driven iterative DFS (see the iterative
    /// Tarjan in `algos/strongly_connected_components.rs`), where repeatedly
    /// re-scanning the adjacency with `nth` would be quadratic in degree.
    pub(crate) fn out_neighbor(&self, node: u32, idx: u32) -> Option<u32> {
        self.out_edges[self.out_offsets[node as usize]..self.out_offsets[node as usize + 1]]
            .get(idx as usize)
            .map(|(t, _)| *t)
    }

    pub(crate) fn out_neighbors_with_values(
        &self,
        node: u32,
    ) -> impl Iterator<Item = Target<W>> + '_ {
        self.out_edges[self.out_offsets[node as usize]..self.out_offsets[node as usize + 1]]
            .iter()
            .map(|(t, w)| Target {
                target: *t,
                value: *w,
            })
    }

    pub(crate) fn in_neighbors(&self, node: u32) -> impl Iterator<Item = u32> + '_ {
        self.in_edges[self.in_offsets[node as usize]..self.in_offsets[node as usize + 1]]
            .iter()
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csr_shape_and_iteration() {
        // 0→1, 0→2 (parallel ×2), 2→0; node 1 is a sink.
        let g: DirectedCsrGraph<f32> =
            DirectedCsrGraph::from_edges([(0, 2, 1.0), (0, 1, 2.0), (2, 0, 3.0), (0, 2, 4.0)])
                .unwrap();
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
    }

    #[test]
    fn empty_graph() {
        let g: DirectedCsrGraph = DirectedCsrGraph::from_edges([]).unwrap();
        assert_eq!(g.node_count(), 0);
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
    fn node_count_refuses_at_u32_bound() {
        assert_eq!(checked_node_count(None).unwrap(), 0);
        assert_eq!(checked_node_count(Some(0)).unwrap(), 1);
        assert_eq!(checked_node_count(Some(u32::MAX - 1)).unwrap(), u32::MAX);
        let err = checked_node_count(Some(u32::MAX)).unwrap_err();
        assert!(
            err.downcast_ref::<GraphTooLargeError>().is_some(),
            "expected the typed GraphTooLargeError, got: {err}"
        );
        assert!(err.to_string().contains("2^32"), "{err}");

        // The same refusal surfaces through `from_edges` itself.
        let err = DirectedCsrGraph::<()>::from_edges([(0, u32::MAX, ())]).unwrap_err();
        assert!(err.downcast_ref::<GraphTooLargeError>().is_some(), "{err}");
    }
}
