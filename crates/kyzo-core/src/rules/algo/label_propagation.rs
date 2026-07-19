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
 * inline one in `fixed_rule/graph.rs`; `rand` 0.8 APIs updated to 0.9
 * (`thread_rng` → `rng`, trait re-homes); the tie-break `choose(..)
 * .unwrap()` is annotated as structural (the candidate list is non-empty
 * by construction); output rows flow through the arity-checked writer.
 * DETERMINISM FIX (deliberate, pinned vs upstream): the original seeded
 * `rand` from the OS entropy pool (`rand::rng()`), so the shuffled scan
 * order and the random tie-break made the SAME facts + query answer
 * differently run to run — a determinism-law violation. Randomness now
 * flows from a `seed` option (fixed default [`SeededRng::DEFAULT_SEED`])
 * through the seed-reproducible [`SeededRng`]: same seed ⇒ byte-identical
 * output, pinned by `run_twice_*` tests below.
 */

//! Community detection by label propagation: nodes repeatedly adopt the
//! weight-heaviest label among their neighbors (ties broken at random)
//! until stable or `max_iter`.

use std::collections::BTreeMap;

use itertools::Itertools;
use miette::Result;
use rand::prelude::*;
use smartstring::{LazyCompact, SmartString};

use kyzo_model::program::expr::Expr;
use kyzo_model::program::rule::FixedRuleOptions;
use kyzo_model::SourceSpan;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::{DataValue, Tuple};
use crate::rules::graph_view::DirectedCsrGraph;
use crate::rules::rng::SeededRng;
use crate::rules::contract::{
    GraphAlgorithmInvariantError, CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload,
};

pub(crate) struct LabelPropagation;

impl FixedRule for LabelPropagation {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let edges = payload.get_input(0)?;
        let undirected = payload.bool_option("undirected", Some(false))?;
        let max_iter = payload.pos_integer_option("max_iter", Some(10))?;
        // Determinism: the shuffled scan order and random tie-break are
        // seeded from this option (fixed default), never from OS entropy.
        let seed = SeededRng::seed_from_i64(
            payload.integer_option("seed", Some(SeededRng::DEFAULT_SEED as i64))?,
        );
        let (graph, indices, _inv_indices) = edges.as_directed_weighted_graph(undirected, true)?;
        let labels = label_propagation(&graph, max_iter, seed, cancel)?;
        for (idx, label) in labels.into_iter().enumerate() {
            let node = indices[idx].clone();
            out.put(Tuple::from_vec(vec![DataValue::from(label as i64), node]))?;
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

fn label_propagation(
    graph: &DirectedCsrGraph<f32>,
    max_iter: usize,
    seed: u64,
    cancel: CancelFlag,
) -> Result<Vec<u32>> {
    let n_nodes = graph.node_count();
    let mut labels = (0..n_nodes).collect_vec();
    let mut rng = SeededRng::new(seed);
    let mut iter_order = (0..n_nodes).collect_vec();
    for _ in 0..max_iter {
        iter_order.shuffle(&mut rng);
        let mut changed = false;
        for node in &iter_order {
            let mut labels_for_node: BTreeMap<u32, f32> = BTreeMap::new();
            for edge in graph.out_neighbors_with_values(*node) {
                let label = labels[edge.target as usize];
                *labels_for_node.entry(label).or_default() += edge.value;
            }
            if labels_for_node.is_empty() {
                continue;
            }
            let mut labels_by_score = labels_for_node.into_iter().collect_vec();
            labels_by_score.sort_by(|a, b| a.1.total_cmp(&b.1).reverse());
            let max_score = labels_by_score[0].1;
            let candidate_labels = labels_by_score
                .into_iter()
                .take_while(|(_, score)| *score == max_score)
                .map(|(l, _)| l)
                .collect_vec();
            // INVARIANT(label_candidates): `take_while` keeps at least the
            // first element (score equals `max_score`), so never empty.
            let new_label = *candidate_labels
                .choose(&mut rng)
                .ok_or_else(|| GraphAlgorithmInvariantError::refuse("label_candidates"))?;
            if new_label != labels[*node as usize] {
                changed = true;
                labels[*node as usize] = new_label;
            }
            cancel.check()?;
        }
        if !changed {
            break;
        }
    }
    Ok(labels)
}

#[cfg(test)]
mod tests {
    use kyzo_model::program::symbol::Symbol;
    use super::*;
    use kyzo_model::value::Tuple;
    use crate::rules::contract::tests_support::{TestInput, run_fixed_rule, empty_opts, opts_map};

    fn s(v: &str) -> DataValue {
        DataValue::from(v)
    }

    /// An undirected ring a0..a11 — every node has two equal-weight
    /// neighbors, so every sweep faces genuine label ties and the shuffled
    /// scan order matters. Under OS-entropy seeding two runs would (almost
    /// surely) diverge; the fixed seed makes them identical.
    fn ring_inputs() -> Vec<TestInput> {
        let n = 12usize;
        let edges: Vec<Tuple> = (0..n)
            .map(|i| Tuple::from_vec(vec![s(&format!("a{i}")), s(&format!("a{}", (i + 1) % n))]))
            .collect();
        vec![TestInput::new(vec!["fr", "to"], edges)]
    }

    fn undirected_opt() -> FixedRuleOptions {
        opts_map(BTreeMap::from([(
            SmartString::from("undirected"),
            Expr::Const {
                val: DataValue::from(true),
                span: SourceSpan::default(),
            },
        )]))
    }

    fn seed_opt(seed: i64) -> FixedRuleOptions {
        let mut o = undirected_opt();
        o.insert(
            Symbol::new("seed", SourceSpan::default()),
            Expr::Const {
                val: DataValue::from(seed),
                span: SourceSpan::default(),
            },
        )
        .expect("seed is a known fixed-rule option");
        o
    }

    /// DETERMINISM: with the fixed default seed, the ring — where ties and
    /// shuffle order are exercised on every sweep — answers byte-identically
    /// across repeated runs. This is the test that a mutation back to
    /// `rand::rng()` (OS entropy) fails.
    #[test]
    fn run_twice_default_seed_is_byte_identical() {
        let first = run_fixed_rule(
            &LabelPropagation,
            ring_inputs(),
            undirected_opt(),
            CancelFlag::default(),
        )
        .unwrap();
        for _ in 0..8 {
            let again = run_fixed_rule(
                &LabelPropagation,
                ring_inputs(),
                undirected_opt(),
                CancelFlag::default(),
            )
            .unwrap();
            assert_eq!(first, again);
        }
    }

    /// DETERMINISM: an explicit `seed` is honored and reproducible; two
    /// different seeds explore different tie-break streams, so on this
    /// symmetric ring they need not agree — proving the seed actually
    /// steers the randomness (not a constant that ignores it).
    #[test]
    fn explicit_seed_is_reproducible_and_load_bearing() {
        let run = |seed: i64| {
            run_fixed_rule(
                &LabelPropagation,
                ring_inputs(),
                seed_opt(seed),
                CancelFlag::default(),
            )
            .unwrap()
        };
        assert_eq!(run(1), run(1));
        assert_eq!(run(999), run(999));
        // Across many seed pairs at least one disagreement proves the seed
        // reaches the tie-break; a constant-ignoring-seed mutant makes every
        // seed agree and fails this.
        let base = run(1);
        assert!(
            (2..40).any(|seed| run(seed) != base),
            "seed does not steer the tie-break"
        );
    }

    /// VALUE ORACLE on the only deterministic ground label propagation
    /// offers: a graph where no decision ever has more than one candidate,
    /// so neither the shuffled scan order nor the random tie-break can
    /// change the outcome.
    ///
    /// Two directed stars: b→a, c→a and y→x, z→x. The harness store sorts
    /// input rows ([b,a] < [c,a] < [y,x] < [z,x]), so interning assigns
    ///   b=0, a=1, c=2, y=3, x=4, z=5.
    /// Hubs a and x have no out-neighbors ⇒ they keep their own labels
    /// (1 and 4). Every leaf sees exactly one neighbor label (its hub's,
    /// which never changes) ⇒ adopts it in the first sweep; stable after.
    ///   ⇒ rows exactly: (1,a) (1,b) (1,c) (4,x) (4,y) (4,z).
    ///
    /// On adjacency ORDER: this algorithm accumulates neighbor-label
    /// weights into a `BTreeMap` keyed by label, so reversing the CSR
    /// adjacency segments cannot change its result on any graph (only f32
    /// rounding of the per-label sums could, never exactly). The
    /// reversed-CSR mutant is killed by the CSR's own test, `top_sort`,
    /// and `louvain::adjacency_tie_break_pinned` instead.
    #[test]
    fn star_hubs_labels_are_canonical() {
        let got = run_fixed_rule(
            &LabelPropagation,
            vec![TestInput::new(
                vec!["fr", "to"],
                vec![
                    Tuple::from_vec(vec![s("b"), s("a")]),
                    Tuple::from_vec(vec![s("c"), s("a")]),
                    Tuple::from_vec(vec![s("y"), s("x")]),
                    Tuple::from_vec(vec![s("z"), s("x")]),
                ],
            )],
            empty_opts(),
            CancelFlag::default(),
        )
        .unwrap();
        let one = DataValue::from(1i64);
        let four = DataValue::from(4i64);
        let want: Vec<Tuple> = vec![
            Tuple::from_vec(vec![one.clone(), s("a")]),
            Tuple::from_vec(vec![one.clone(), s("b")]),
            Tuple::from_vec(vec![one, s("c")]),
            Tuple::from_vec(vec![four.clone(), s("x")]),
            Tuple::from_vec(vec![four.clone(), s("y")]),
            Tuple::from_vec(vec![four, s("z")]),
        ];
        assert_eq!(got, want);
    }
}
