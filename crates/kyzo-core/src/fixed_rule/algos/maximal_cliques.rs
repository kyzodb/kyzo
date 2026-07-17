/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Maximal clique enumeration (Bron–Kerbosch with pivoting, degeneracy
//! ordering).
//!
//! New in KyzoDB (no CozoDB precedent). Cliques are an undirected notion, so
//! the directed input edge relation (input 0) is read undirected, over the
//! simple graph — distinct neighbors, self-loops dropped — matching the
//! `k_core` interpretation. Output: one row `[clique_id, node]` per membership
//! (arity 2); a node in several maximal cliques appears in several rows.
//!
//! **Canonical clique numbering.** Each maximal clique's members are sorted
//! by node value; the cliques are then sorted lexicographically by that
//! member-value list, and `clique_id` is the index in that order. This is a
//! pure function of the *graph*, independent of input edge order or the
//! interning that assigns internal node ids — so the numbering is stable
//! across equivalent inputs (pinned by `deterministic_across_runs`).
//!
//! **Algorithm.** Tomita's pivoting Bron–Kerbosch, with the outer loop run in
//! **degeneracy order** (Eppstein–Löffler–Strash): near-optimal `O(d · n ·
//! 3^{d/3})` for a graph of degeneracy `d`. Both the pivot choice and the
//! degeneracy ordering are *efficiency* devices — neither changes the *set*
//! of maximal cliques, only how fast it is reached (so the value oracle,
//! which checks the set, would still pass if they were disabled; the
//! correctness-critical steps are the candidate/excluded intersections and
//! the maximality gate, both mutation-pinned below).
//!
//! **Law 5 (no deep recursion).** The recursion is run on an **explicit
//! heap stack**, not the call stack. Bron–Kerbosch recursion depth is bounded
//! by the size of the clique being built (≤ the largest clique), so a dense
//! stored graph with a large clique would drive a recursive formulation
//! arbitrarily deep and overflow; here those frames live on the heap
//! (`deep_clique_no_overflow`).
//!
//! **Bounded, budget-honest.** Clique enumeration is exponential in the worst
//! case (Moon–Ganguli: up to `3^{n/3}` maximal cliques). The count is never
//! unbounded: a `max_cliques` option (default [`DEFAULT_MAX_CLIQUES`]) caps
//! it, and exceeding the cap is a typed refusal ([`TooManyCliquesError`]),
//! never an unbounded allocation — the OOM lesson made law. Cancellation is
//! polled once per expansion step.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use miette::{Diagnostic, Result, bail};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::expr::Expr;
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::{DataValue, Tuple};
use crate::fixed_rule::graph::DirectedCsrGraph;
use crate::fixed_rule::{
    CancelAuthority, CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload,
};

/// The default ceiling on enumerated maximal cliques when the caller gives no
/// `max_cliques` option. Generous, but finite: enumeration must never be
/// unbounded (law).
pub(crate) const DEFAULT_MAX_CLIQUES: usize = 1 << 20;

// Test-only observables mirroring `shortest_path_bfs::BFS_NODES_EXPANDED`:
// deterministic, load-independent effects for the two cancellation polls
// (one per degeneracy removal, one per expansion step). The expansion hook
// can additionally raise a flag at a chosen step count, which is the only
// deterministic way to prove the *expansion* poll reads the flag — a
// pre-raised flag is always caught first by the degeneracy poll. In a
// non-test build the note fns are empty inlined no-ops.
#[cfg(test)]
thread_local! {
    static CLIQUE_DEGEN_REMOVALS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static CLIQUE_EXPANSION_STEPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static CANCEL_AT_EXPANSION_STEP: std::cell::RefCell<Option<(u64, CancelAuthority)>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn note_clique_degen_removal() {
    CLIQUE_DEGEN_REMOVALS.with(|c| c.set(c.get() + 1));
}

#[cfg(test)]
fn note_clique_expansion_step() {
    let now = CLIQUE_EXPANSION_STEPS.with(|c| {
        c.set(c.get() + 1);
        c.get()
    });
    CANCEL_AT_EXPANSION_STEP.with(|h| {
        let mut slot = h.borrow_mut();
        if let Some((at, _)) = slot.as_ref()
            && now == *at
            && let Some((_, auth)) = slot.take()
        {
            let _ = auth.cancel();
        }
    });
}

/// Reset both counters and return them as (degeneracy removals, expansion
/// steps) — for the cancellation tests.
#[cfg(test)]
fn take_clique_counters() -> (u64, u64) {
    (
        CLIQUE_DEGEN_REMOVALS.with(|c| c.replace(0)),
        CLIQUE_EXPANSION_STEPS.with(|c| c.replace(0)),
    )
}

#[cfg(not(test))]
#[inline(always)]
fn note_clique_degen_removal() {}

#[cfg(not(test))]
#[inline(always)]
fn note_clique_expansion_step() {}

pub(crate) struct MaximalCliques;

impl FixedRule for MaximalCliques {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let edges = payload.get_input(0)?;
        let max_cliques = payload.pos_integer_option("max_cliques", Some(DEFAULT_MAX_CLIQUES))?;
        let span = payload.span();

        // Undirected simple graph (as in k_core).
        let (graph, indices, _inv_indices) = edges.as_directed_graph(true)?;
        if graph.node_count() == 0 {
            return Ok(());
        }
        let adj = simple_adjacency(&graph);
        let cliques = enumerate_cliques(&adj, max_cliques, &cancel, span)?;

        // Canonicalize: sort members by value, sort cliques lexicographically
        // by member-value list, number by that order.
        let mut keyed: Vec<Vec<DataValue>> = cliques
            .into_iter()
            .map(|clique| {
                let mut members: Vec<DataValue> = clique
                    .into_iter()
                    .map(|id| indices[id as usize].clone())
                    .collect();
                members.sort();
                members
            })
            .collect();
        keyed.sort();

        for (clique_id, members) in keyed.into_iter().enumerate() {
            for member in members {
                out.put(Tuple::from_vec(vec![
                    DataValue::from(clique_id as i64),
                    member,
                ]))?;
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

/// Simple undirected adjacency: distinct neighbors, self-loops dropped, each
/// list sorted (the CSR out-adjacency is already target-sorted). Identical in
/// spirit to `k_core::simple_adjacency`; kept local so this file is a
/// self-contained deliverable.
fn simple_adjacency(graph: &DirectedCsrGraph) -> Vec<Vec<u32>> {
    let n = graph.node_count() as usize;
    let mut adj = Vec::with_capacity(n);
    for v in 0..n as u32 {
        let mut nbrs: Vec<u32> = graph.out_neighbors(v).filter(|&u| u != v).collect();
        nbrs.dedup();
        adj.push(nbrs);
    }
    adj
}

/// A degeneracy ordering (Matula–Beck smallest-last): repeatedly remove a
/// minimum-degree vertex; the removal order is returned. In this order every
/// vertex has at most `degeneracy` neighbors that come *later*, which bounds
/// the candidate set of each outer Bron–Kerbosch call. Ties break by ascending
/// id (the `BTreeSet` buckets iterate ascending), so the order is
/// deterministic.
fn degeneracy_order(adj: &[Vec<u32>], cancel: &CancelFlag) -> Result<Vec<u32>> {
    let n = adj.len();
    let mut deg: Vec<usize> = adj.iter().map(|a| a.len()).collect();
    let max_deg = deg.iter().copied().max().unwrap_or(0);
    let mut buckets: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); max_deg + 1];
    for (v, &d) in deg.iter().enumerate() {
        buckets[d].insert(v as u32);
    }
    let mut order = Vec::with_capacity(n);
    let mut removed = vec![false; n];
    for _ in 0..n {
        note_clique_degen_removal();
        cancel.check()?;
        // INVARIANT(degen_bucket): n removals leave a non-empty bucket until done.
        let d = buckets
            .iter()
            .position(|b| !b.is_empty())
            .expect("INVARIANT(degen_bucket): remaining vertex has a degree bucket");
        let v = *buckets[d]
            .iter()
            .next()
            .expect("INVARIANT(degen_bucket): chosen bucket is non-empty");
        buckets[d].remove(&v);
        removed[v as usize] = true;
        order.push(v);
        for &u in &adj[v as usize] {
            if !removed[u as usize] {
                buckets[deg[u as usize]].remove(&u);
                deg[u as usize] -= 1;
                buckets[deg[u as usize]].insert(u);
            }
        }
    }
    Ok(order)
}

/// One frame of the explicit Bron–Kerbosch stack: the growing clique `r`, the
/// live candidate set `p` and excluded set `x` (both kept sorted), the
/// pivot-reduced candidate list `cands` fixed at frame creation, and the
/// cursor into it.
struct Frame {
    r: Vec<u32>,
    p: Vec<u32>,
    x: Vec<u32>,
    cands: Vec<u32>,
    idx: usize,
}

/// The mutable context threaded through enumeration: the immutable adjacency,
/// the accumulating clique list, the ceiling, and the span for the refusal
/// error. Bundled into a struct so the recursion carries one `&mut self`
/// rather than a long argument list.
struct Enumeration<'a> {
    adj: &'a [Vec<u32>],
    cliques: Vec<Vec<u32>>,
    max_cliques: usize,
    span: SourceSpan,
}

/// Enumerate all maximal cliques. The outer loop walks the degeneracy order;
/// for each vertex `v` it runs a pivoting Bron–Kerbosch seeded with `v`'s
/// later-neighbors as candidates and earlier-neighbors as the excluded set,
/// which reports each maximal clique exactly once.
fn enumerate_cliques(
    adj: &[Vec<u32>],
    max_cliques: usize,
    cancel: &CancelFlag,
    span: SourceSpan,
) -> Result<Vec<Vec<u32>>> {
    let order = degeneracy_order(adj, cancel)?;
    let n = adj.len();
    let mut pos = vec![0usize; n];
    for (i, &v) in order.iter().enumerate() {
        pos[v as usize] = i;
    }

    let mut ctx = Enumeration {
        adj,
        cliques: Vec::new(),
        max_cliques,
        span,
    };
    for &v in &order {
        // Later-neighbors → candidates; earlier-neighbors → excluded. Both
        // inherit `adj[v]`'s sorted order.
        let p: Vec<u32> = adj[v as usize]
            .iter()
            .copied()
            .filter(|&u| pos[u as usize] > pos[v as usize])
            .collect();
        let x: Vec<u32> = adj[v as usize]
            .iter()
            .copied()
            .filter(|&u| pos[u as usize] < pos[v as usize])
            .collect();
        ctx.bron_kerbosch(vec![v], p, x, cancel)?;
    }
    Ok(ctx.cliques)
}

impl Enumeration<'_> {
    /// Iterative pivoting Bron–Kerbosch over an explicit stack. Reports every
    /// maximal clique reachable from the seed frame into `self.cliques`.
    fn bron_kerbosch(
        &mut self,
        r: Vec<u32>,
        p: Vec<u32>,
        x: Vec<u32>,
        cancel: &CancelFlag,
    ) -> Result<()> {
        let mut stack = vec![self.make_frame(r, p, x)?];
        while !stack.is_empty() {
            note_clique_expansion_step();
            cancel.check()?; // once per expansion step
            let top = stack.len() - 1;
            if stack[top].idx >= stack[top].cands.len() {
                stack.pop();
                continue;
            }
            // Next candidate `v`; advance the cursor.
            let v = stack[top].cands[stack[top].idx];
            stack[top].idx += 1;

            // Child sets from the CURRENT p/x (v ∉ N(v), so removing v from p
            // first would not change these): R∪{v}, P∩N(v), X∩N(v).
            let (child_r, child_p, child_x) = {
                let f = &stack[top];
                let mut cr = f.r.clone();
                cr.push(v);
                (
                    cr,
                    intersect(&f.p, &self.adj[v as usize]),
                    intersect(&f.x, &self.adj[v as usize]),
                )
            };

            // Advance this frame: move v from P to X for its remaining
            // candidates.
            {
                let f = &mut stack[top];
                if let Ok(i) = f.p.binary_search(&v) {
                    f.p.remove(i);
                }
                if let Err(i) = f.x.binary_search(&v) {
                    f.x.insert(i, v);
                }
            }

            let child = self.make_frame(child_r, child_p, child_x)?;
            stack.push(child);
        }
        Ok(())
    }

    /// Build a frame: if `P` and `X` are both empty, `R` is a maximal clique
    /// and is reported (honoring the `max_cliques` ceiling); otherwise choose a
    /// pivot and fix the candidate list `P \ N(pivot)`.
    ///
    /// The maximality gate is exactly `P.empty() && X.empty()`: dropping the
    /// `X` half would report non-maximal cliques (subsets of true maximal
    /// ones), pinned by `maximality_gate_rejects_subsets`.
    fn make_frame(&mut self, r: Vec<u32>, p: Vec<u32>, x: Vec<u32>) -> Result<Frame> {
        if p.is_empty() && x.is_empty() {
            self.cliques.push(r);
            if self.cliques.len() > self.max_cliques {
                bail!(TooManyCliquesError {
                    max_cliques: self.max_cliques,
                    span: self.span,
                });
            }
            return Ok(Frame {
                r: Vec::new(),
                p,
                x,
                cands: Vec::new(),
                idx: 0,
            });
        }
        let pivot = choose_pivot(&p, &x, self.adj);
        let pivot_nbrs = &self.adj[pivot as usize];
        let cands: Vec<u32> = p
            .iter()
            .copied()
            .filter(|&v| !contains(pivot_nbrs, v))
            .collect();
        Ok(Frame {
            r,
            p,
            x,
            cands,
            idx: 0,
        })
    }
}

/// Pivot choice (Tomita): the vertex of `P ∪ X` maximizing `|P ∩ N(u)|`, so
/// that `P \ N(pivot)` — the branch set — is as small as possible. Ties break
/// by smallest id for determinism. Called only on a non-terminal frame, so
/// `P ∪ X` is non-empty.
fn choose_pivot(p: &[u32], x: &[u32], adj: &[Vec<u32>]) -> u32 {
    let mut best: Option<(usize, u32)> = None; // (coverage, id)
    for &u in p.iter().chain(x.iter()) {
        let coverage = count_intersection(p, &adj[u as usize]);
        match best {
            None => best = Some((coverage, u)),
            Some((bc, bu)) => {
                if coverage > bc || (coverage == bc && u < bu) {
                    best = Some((coverage, u));
                }
            }
        }
    }
    // INVARIANT(clique_pivot): `p ∪ x` non-empty on a non-terminal frame.
    best.expect("INVARIANT(clique_pivot): non-empty p∪x yields a pivot").1
}

/// Sorted-set intersection of two ascending slices.
fn intersect(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

/// Count of common elements of two ascending slices.
fn count_intersection(a: &[u32], b: &[u32]) -> usize {
    let (mut i, mut j, mut c) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                c += 1;
                i += 1;
                j += 1;
            }
        }
    }
    c
}

#[inline]
fn contains(sorted: &[u32], v: u32) -> bool {
    sorted.binary_search(&v).is_ok()
}

#[derive(Debug, Error, Diagnostic)]
#[error("MaximalCliques exceeded the max_cliques ceiling of {max_cliques}")]
#[diagnostic(code(algo::too_many_cliques))]
#[diagnostic(help(
    "Maximal-clique enumeration is exponential in the worst case; raise \
     `max_cliques` deliberately if the graph really has this many"
))]
struct TooManyCliquesError {
    max_cliques: usize,
    #[label]
    span: SourceSpan,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::data::value::Tuple;
    use crate::fixed_rule::tests_support::{TestInput, run_fixed_rule};

    fn s(v: &str) -> DataValue {
        DataValue::from(v)
    }

    /// Run the rule and return the maximal cliques as a sorted set of sorted
    /// node-name lists — the canonical shape both the rule output and the
    /// brute-force oracle are compared in.
    fn run_cliques(edges: &[(&str, &str)], max_cliques: Option<i64>) -> Vec<Vec<String>> {
        let rows = edges
            .iter()
            .map(|&(a, b)| Tuple::from_vec(vec![s(a), s(b)]))
            .collect::<Vec<Tuple>>();
        let mut opts = BTreeMap::new();
        if let Some(mc) = max_cliques {
            opts.insert(
                SmartString::from("max_cliques"),
                Expr::Const {
                    val: DataValue::from(mc),
                    span: SourceSpan::default(),
                },
            );
        }
        let got = run_fixed_rule(
            &MaximalCliques,
            vec![TestInput::new(vec!["fr", "to"], rows)],
            opts,
            CancelFlag::default(),
        )
        .unwrap();
        // Group rows by clique_id, collect member names.
        let mut by_id: BTreeMap<i64, Vec<String>> = BTreeMap::new();
        for r in got {
            by_id
                .entry(r[0].get_int().unwrap())
                .or_default()
                .push(r[1].get_str().unwrap().to_string());
        }
        let mut cliques: Vec<Vec<String>> = by_id
            .into_values()
            .map(|mut m| {
                m.sort();
                m
            })
            .collect();
        cliques.sort();
        cliques
    }

    /// INDEPENDENT ORACLE: exhaustively test every vertex subset. A subset is
    /// a clique iff all its pairs are adjacent; it is maximal iff no outside
    /// vertex is adjacent to all of it. Feasible for ≤ ~16 nodes. Shares no
    /// logic with Bron–Kerbosch.
    fn brute_maximal_cliques(edges: &[(&str, &str)]) -> Vec<Vec<String>> {
        let mut names: BTreeSet<String> = BTreeSet::new();
        for &(a, b) in edges {
            names.insert(a.to_string());
            names.insert(b.to_string());
        }
        let names: Vec<String> = names.into_iter().collect();
        let n = names.len();
        let idx = |name: &str| names.iter().position(|x| x == name).unwrap();
        let mut adjm = vec![vec![false; n]; n];
        for &(a, b) in edges {
            if a != b {
                let (ia, ib) = (idx(a), idx(b));
                adjm[ia][ib] = true;
                adjm[ib][ia] = true;
            }
        }
        let mut out = Vec::new();
        for mask in 1u32..(1u32 << n) {
            let members: Vec<usize> = (0..n).filter(|&i| mask & (1 << i) != 0).collect();
            let is_clique = members
                .iter()
                .all(|&a| members.iter().all(|&b| a == b || adjm[a][b]));
            if !is_clique {
                continue;
            }
            let maximal = (0..n)
                .filter(|&ext| mask & (1 << ext) == 0)
                .all(|ext| !members.iter().all(|&a| adjm[a][ext]));
            if maximal {
                let mut clique: Vec<String> = members.iter().map(|&i| names[i].clone()).collect();
                clique.sort();
                out.push(clique);
            }
        }
        out.sort();
        out
    }

    /// VALUE ORACLE: two triangles sharing a vertex, plus a pendant. By hand
    /// the maximal cliques are {a,b,c}, {c,d,e}, {e,f}. Pinned exactly.
    #[test]
    fn two_triangles_sharing_a_vertex() {
        let edges = [
            ("a", "b"),
            ("b", "c"),
            ("a", "c"),
            ("c", "d"),
            ("d", "e"),
            ("c", "e"),
            ("e", "f"),
        ];
        assert_eq!(
            run_cliques(&edges, None),
            vec![
                vec!["a".to_string(), "b".to_string(), "c".to_string()],
                vec!["c".to_string(), "d".to_string(), "e".to_string()],
                vec!["e".to_string(), "f".to_string()],
            ]
        );
    }

    /// VALUE ORACLE vs the exhaustive reference on several hand-picked and
    /// pseudo-random graphs (≤ 14 nodes), including overlapping cliques and a
    /// bipartite graph (only edges are maximal cliques).
    #[test]
    fn matches_brute_reference() {
        // A 5-wheel: hub h joined to a 4-cycle a-b-c-d.
        let wheel = [
            ("a", "b"),
            ("b", "c"),
            ("c", "d"),
            ("d", "a"),
            ("h", "a"),
            ("h", "b"),
            ("h", "c"),
            ("h", "d"),
        ];
        assert_eq!(run_cliques(&wheel, None), brute_maximal_cliques(&wheel));

        // K4 with an extra pendant triangle.
        let g = [
            ("a", "b"),
            ("a", "c"),
            ("a", "d"),
            ("b", "c"),
            ("b", "d"),
            ("c", "d"),
            ("d", "e"),
            ("e", "f"),
            ("d", "f"),
        ];
        assert_eq!(run_cliques(&g, None), brute_maximal_cliques(&g));

        // Pseudo-random graphs.
        for seed in 0..40u64 {
            // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
            let mut state = 0x9E37_79B9_7F4A_7C15u64.wrapping_mul(seed + 1);
            let mut next = || {
                // INVARIANT(lcg64): Knuth LCG step is defined wrapping on u64.
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                state
            };
            let n = 12u64;
            let owned: Vec<(String, String)> = (0..30)
                .filter_map(|_| {
                    let a = (next() >> 33) % n;
                    let b = (next() >> 33) % n;
                    (a != b).then(|| (format!("n{a}"), format!("n{b}")))
                })
                .collect();
            let edges: Vec<(&str, &str)> = owned
                .iter()
                .map(|(a, b)| (a.as_str(), b.as_str()))
                .collect();
            assert_eq!(
                run_cliques(&edges, None),
                brute_maximal_cliques(&edges),
                "seed {seed}"
            );
        }
    }

    /// MUTATION PIN — the maximality gate. On two triangles sharing a vertex,
    /// the subsets {a,b}, {b,c}, ... are cliques but NOT maximal. If the gate
    /// weakened from `P.empty() && X.empty()` to just `P.empty()`, these
    /// subsets would be reported. The exact expected set (three maximal
    /// cliques, no 2-subsets of the triangles) fails under that mutant.
    #[test]
    fn maximality_gate_rejects_subsets() {
        let got = run_cliques(
            &[
                ("a", "b"),
                ("b", "c"),
                ("a", "c"),
                ("c", "d"),
                ("d", "e"),
                ("c", "e"),
            ],
            None,
        );
        assert_eq!(
            got,
            vec![
                vec!["a".to_string(), "b".to_string(), "c".to_string()],
                vec!["c".to_string(), "d".to_string(), "e".to_string()],
            ]
        );
        // No reported clique is a strict subset of another (the maximality
        // property), and none is a bare triangle-edge.
        assert!(got.iter().all(|c| c.len() == 3));
    }

    /// DETERMINISM: the canonical numbering is a pure function of the graph.
    /// Shuffling the input edge order (which changes interning) must not
    /// change the output.
    #[test]
    fn deterministic_across_runs() {
        let edges = [
            ("a", "b"),
            ("b", "c"),
            ("a", "c"),
            ("c", "d"),
            ("d", "e"),
            ("c", "e"),
            ("e", "f"),
        ];
        let mut shuffled = edges;
        shuffled.reverse();
        let first = run_cliques(&edges, None);
        assert_eq!(run_cliques(&shuffled, None), first);
        for _ in 0..4 {
            assert_eq!(run_cliques(&edges, None), first);
        }
    }

    /// BUDGET HONESTY: a graph with more maximal cliques than the cap is a
    /// typed refusal, not an unbounded allocation. The Moon–Moser graph
    /// (complete tripartite-style `K_{3,3,...}` complement) has many cliques;
    /// here a set of disjoint triangles has one clique each, so `t` triangles
    /// give `t` cliques — cap below `t` must refuse.
    #[test]
    fn max_cliques_ceiling_refuses() {
        // 5 disjoint triangles ⇒ 5 maximal cliques.
        let mut edges: Vec<(String, String)> = Vec::new();
        for t in 0..5 {
            let (a, b, c) = (format!("a{t}"), format!("b{t}"), format!("c{t}"));
            edges.push((a.clone(), b.clone()));
            edges.push((b.clone(), c.clone()));
            edges.push((a.clone(), c.clone()));
        }
        let eref: Vec<(&str, &str)> = edges
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        // Cap of 3 < 5 ⇒ refusal.
        let rows = eref
            .iter()
            .map(|&(a, b)| Tuple::from_vec(vec![s(a), s(b)]))
            .collect::<Vec<Tuple>>();
        let err = run_fixed_rule(
            &MaximalCliques,
            vec![TestInput::new(vec!["fr", "to"], rows)],
            BTreeMap::from([(
                SmartString::from("max_cliques"),
                Expr::Const {
                    val: DataValue::from(3i64),
                    span: SourceSpan::default(),
                },
            )]),
            CancelFlag::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("max_cliques"), "{err}");
        // A cap of 5 is exactly enough.
        assert_eq!(run_cliques(&eref, Some(5)).len(), 5);
    }

    /// ADVERSARIAL SHAPE (law 5): a complete graph K_m has a single maximal
    /// clique of all m nodes, reached by a Bron–Kerbosch recursion m levels
    /// deep. On the explicit heap stack this is fine; a recursive formulation
    /// would nest m native frames. The exact single-clique answer also proves
    /// correctness at depth. (Clique depth is inherently bounded by clique
    /// size, hence by the ~m^2/2 edges a clique needs, so m is moderate — the
    /// structural point is the explicit stack, not raw depth.)
    #[test]
    fn deep_clique_no_overflow() {
        let m: u32 = 2_000;
        let mut edges: Vec<(u32, u32)> = Vec::new();
        for i in 0..m {
            for j in (i + 1)..m {
                edges.push((i, j));
            }
        }
        // Build interned rows directly as strings.
        let owned: Vec<(String, String)> = edges
            .iter()
            .map(|&(i, j)| (format!("{i}"), format!("{j}")))
            .collect();
        let rows = owned
            .iter()
            .map(|(a, b)| Tuple::from_vec(vec![s(a), s(b)]))
            .collect::<Vec<Tuple>>();
        let got = run_fixed_rule(
            &MaximalCliques,
            vec![TestInput::new(vec!["fr", "to"], rows)],
            BTreeMap::new(),
            CancelFlag::default(),
        )
        .unwrap();
        // One clique (id 0) containing all m nodes ⇒ m membership rows.
        assert_eq!(got.len(), m as usize);
        assert!(got.iter().all(|r| r[0].get_int().unwrap() == 0));
    }

    /// CANCELLATION, degeneracy poll pinned (house exemplar:
    /// `shortest_path_bfs::honors_cancel_pins_inner_poll`). The baseline
    /// removes every vertex of a 60k-node path during degeneracy ordering;
    /// with a pre-raised flag the per-removal poll must refuse before
    /// removing more than one. Deleting that poll makes the cancelled run
    /// perform all ~60k removals, so the `<= 1` bound fails.
    #[test]
    fn honors_cancel_pins_degeneracy_poll() {
        use crate::fixed_rule::tests_support::prepare_fixed_rule;

        let n: u32 = 60_000;
        let edges: Vec<Tuple> = (0..n - 1)
            .map(|i| Tuple::from_vec(vec![s(&format!("v{i}")), s(&format!("v{}", i + 1))]))
            .collect();
        let inputs = vec![TestInput::new(vec!["fr", "to"], edges)];
        let prepared = prepare_fixed_rule(&MaximalCliques, inputs, BTreeMap::new()).unwrap();

        // Baseline: no cancellation. Every vertex is removed.
        take_clique_counters(); // clear any leftover from a reused thread
        let full = prepared.run(&MaximalCliques, CancelFlag::default());
        let (full_removals, _) = take_clique_counters();
        assert!(full.is_ok());
        assert!(
            full_removals >= u64::from(n),
            "baseline should remove every vertex, got {full_removals}"
        );

        // Spent authority: the degeneracy poll must refuse before ordering.
        let (auth, flag) = CancelAuthority::arm();
        let _ = auth.cancel();
        let cancelled = prepared.run(&MaximalCliques, flag);
        let (cancel_removals, cancel_steps) = take_clique_counters();
        assert!(cancelled.unwrap_err().to_string().contains("killed"));
        assert!(
            cancel_removals <= 1 && cancel_steps == 0,
            "degeneracy poll did not refuse up front: {cancel_removals} \
             removals, {cancel_steps} expansion steps (deleting the \
             per-removal poll makes removals ~60k)"
        );
    }

    /// CANCELLATION, expansion poll pinned. A pre-raised flag can never
    /// reach the expansion loop (the degeneracy poll refuses first), so the
    /// flag is raised BY the test hook at expansion step 5: the per-step
    /// poll must refuse at that step, not enumerate the remaining graph.
    /// Deleting the expansion poll makes the run complete (no other poll
    /// exists past degeneracy), so `is_err()` fails; weakening it to
    /// per-frame-push makes the step count overshoot the bound.
    #[test]
    fn honors_cancel_pins_expansion_poll() {
        use crate::fixed_rule::tests_support::prepare_fixed_rule;

        // A 2k-node path: ~2k expansion steps, far above the trip point.
        let n: u32 = 2_000;
        let edges: Vec<Tuple> = (0..n - 1)
            .map(|i| Tuple::from_vec(vec![s(&format!("v{i}")), s(&format!("v{}", i + 1))]))
            .collect();
        let inputs = vec![TestInput::new(vec!["fr", "to"], edges)];
        let prepared = prepare_fixed_rule(&MaximalCliques, inputs, BTreeMap::new()).unwrap();

        let (auth, flag) = CancelAuthority::arm();
        take_clique_counters();
        CANCEL_AT_EXPANSION_STEP.with(|h| *h.borrow_mut() = Some((5, auth)));
        let cancelled = prepared.run(&MaximalCliques, flag);
        CANCEL_AT_EXPANSION_STEP.with(|h| *h.borrow_mut() = None);
        let (removals, steps) = take_clique_counters();

        assert!(
            cancelled.unwrap_err().to_string().contains("killed"),
            "raising the flag mid-expansion must refuse the run"
        );
        assert!(
            removals >= u64::from(n),
            "degeneracy ordering should have completed first, got {removals}"
        );
        assert!(
            steps == 5,
            "expansion poll must refuse exactly at the step that raised the \
             flag: got {steps} steps (deleting the per-step poll lets the \
             run complete instead)"
        );
    }
}
