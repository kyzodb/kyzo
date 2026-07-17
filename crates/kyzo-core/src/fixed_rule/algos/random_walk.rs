/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): LAW-5 FIX — the original called
 * `WeightedIndex::new(&weights).unwrap()`, which panics the engine when a
 * user's `weight` expression yields an unusable distribution (all zeros
 * being the reachable case, since negatives and NaN are already refused
 * above it); that is now a typed error naming the offending expression.
 * The unweighted `choose(..).unwrap()` is annotated as structural
 * (`candidate_steps` was checked non-empty). `rand` 0.8 APIs updated to
 * 0.9. Output rows flow through the arity-checked writer.
 * DETERMINISM FIX (deliberate, pinned vs upstream): the original seeded
 * `rand` from the OS entropy pool (`rand::rng()`), so which neighbor each
 * step picked — and thus the whole walk — varied run to run for the SAME
 * facts + query, violating the determinism law. The walk now draws from a
 * `seed` option (fixed default `SeededRng::DEFAULT_SEED`) through the
 * seed-reproducible `SeededRng`: same seed ⇒ byte-identical output, pinned
 * by the `run_twice_*` test below.
 */

//! Random walks over the edge relation: `iterations` walks of up to
//! `steps` steps from each starting node, optionally biased by a `weight`
//! expression over the node and edge tuples.

use std::collections::BTreeMap;

use itertools::Itertools;
use miette::{Result, bail, ensure};
use rand::distr::Distribution;
use rand::distr::weighted::WeightedIndex;
use rand::prelude::*;
use smartstring::{LazyCompact, SmartString};

use crate::data::expr::Expr;
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::{DataValue, Tuple};
use crate::fixed_rule::rng::SeededRng;
use crate::fixed_rule::{
    BadExprValueError, CancelFlag, FixedRule, FixedRuleOutput, FixedRulePayload, NodeNotFoundError,
};
use crate::data::value::data_value_any;

pub(crate) struct RandomWalk;

impl FixedRule for RandomWalk {
    fn run(
        &self,
        payload: FixedRulePayload<'_>,
        out: &mut FixedRuleOutput,
        cancel: CancelFlag,
    ) -> Result<()> {
        let edges = payload.get_input(0)?.ensure_min_len(2)?;
        let nodes = payload.get_input(1)?.ensure_min_len(1)?;
        let starting = payload.get_input(2)?.ensure_min_len(1)?;
        let iterations = payload.pos_integer_option("iterations", Some(1))?;
        let steps = payload.pos_integer_option("steps", None)?;
        // Determinism: each step's neighbor pick is seeded from this option
        // (fixed default), never from OS entropy.
        let seed = payload.integer_option("seed", Some(SeededRng::DEFAULT_SEED as i64))? as u64;

        let mut maybe_weight = payload.expr_option("weight", None).ok();
        if let Some(weight) = &mut maybe_weight {
            let mut nodes_binding = nodes.get_binding_map(0);
            let nodes_arity = nodes.arity()?;
            let edges_binding = edges.get_binding_map(nodes_arity);
            nodes_binding.extend(edges_binding);
            weight.fill_binding_indices(&nodes_binding)?;
        }

        let mut counter = 0i64;
        let mut rng = SeededRng::new(seed);
        for start_node in starting.iter()? {
            let start_node = start_node?;
            // Structural: `ensure_min_len(1)` proved every tuple has a
            // first column.
            let start_node_key = &start_node.as_slice()[0];
            let starting_tuple =
                nodes
                    .prefix_iter(start_node_key)?
                    .next()
                    .ok_or_else(|| NodeNotFoundError {
                        missing: start_node_key.clone(),
                        span: starting.span(),
                    })??;
            for _ in 0..iterations {
                counter += 1;
                let mut current_tuple = starting_tuple.clone();
                let mut path = vec![start_node_key.clone()];
                for _ in 0..steps {
                    // Structural: `nodes.ensure_min_len(1)` proved every
                    // `nodes` tuple (which `current_tuple` always is) has
                    // a first column.
                    let cur_node_key = &current_tuple.as_slice()[0];
                    let candidate_steps: Vec<_> = edges.prefix_iter(cur_node_key)?.try_collect()?;
                    if candidate_steps.is_empty() {
                        break;
                    }
                    let next_step = if let Some(weight_expr) = &maybe_weight {
                        let weights: Vec<_> = candidate_steps
                            .iter()
                            .map(|t| -> Result<f64> {
                                let mut cand = current_tuple.clone();
                                cand.extend(t.iter().cloned());
                                Ok(match weight_expr.eval(&cand)? {
                                    DataValue::Num(n) => {
                                        let f = n.to_f64();
                                        ensure!(
                                            f >= 0.,
                                            BadExprValueError(
                                                DataValue::from(f),
                                                weight_expr.span(),
                                                "'weight' must evaluate to a non-negative number"
                                                    .to_string()
                                            )
                                        );
                                        f
                                    }
                                    v @ (data_value_any!()) => bail!(BadExprValueError(
                                        v,
                                        weight_expr.span(),
                                        "'weight' must evaluate to a non-negative number"
                                            .to_string()
                                    )),
                                })
                            })
                            .try_collect()?;
                        // LAW-5: a user weight expression can legally yield
                        // all zeros; that is not a samplable distribution,
                        // and it must refuse, not panic (the original
                        // unwrapped here).
                        let dist = WeightedIndex::new(&weights).map_err(|err| {
                            BadExprValueError(
                                DataValue::List(
                                    weights.iter().map(|w| DataValue::from(*w)).collect(),
                                ),
                                weight_expr.span(),
                                format!(
                                    "'weight' must yield a samplable distribution \
                                     (at least one positive weight): {err}"
                                ),
                            )
                        })?;
                        &candidate_steps[dist.sample(&mut rng)]
                    } else {
                        // INVARIANT(walk_candidates): checked non-empty above.
                        candidate_steps.choose(&mut rng).expect(
                            "INVARIANT(walk_candidates): non-empty candidate_steps",
                        )
                    };
                    let next_node = &next_step.as_slice()[1];
                    path.push(next_node.clone());
                    current_tuple = nodes.prefix_iter(next_node)?.next().ok_or_else(|| {
                        NodeNotFoundError {
                            missing: next_node.clone(),
                            span: nodes.span(),
                        }
                    })??;
                    cancel.check()?;
                }
                out.put(Tuple::from_vec(vec![
                    DataValue::from(counter),
                    start_node_key.clone(),
                    DataValue::List(path),
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

    /// REGRESSION (law 5): an all-zero user weight distribution is a typed
    /// refusal, not an engine panic (the original's
    /// `WeightedIndex::new(..).unwrap()`).
    #[test]
    fn all_zero_weights_refuse_typed() {
        let options = BTreeMap::from([
            (
                SmartString::from("steps"),
                Expr::Const {
                    val: DataValue::from(3i64),
                    span: SourceSpan::default(),
                },
            ),
            (
                SmartString::from("weight"),
                Expr::Const {
                    val: DataValue::from(0.0f64),
                    span: SourceSpan::default(),
                },
            ),
        ]);
        let res = run_fixed_rule(
            &RandomWalk,
            vec![
                TestInput::new(
                    vec!["fr", "to"],
                    vec![
                        Tuple::from_vec(vec![s("a"), s("b")]),
                        Tuple::from_vec(vec![s("b"), s("a")]),
                    ],
                ),
                TestInput::new(
                    vec!["id"],
                    vec![Tuple::from_vec(vec![s("a")]), Tuple::from_vec(vec![s("b")])],
                ),
                TestInput::new(vec!["start"], vec![Tuple::from_vec(vec![s("a")])]),
            ],
            options,
            CancelFlag::default(),
        );
        let err = res.unwrap_err();
        assert!(err.to_string().contains("Unacceptable value"), "{err}");
    }

    /// The unweighted walk works and emits `steps + 1` path entries when
    /// edges never run out.
    #[test]
    fn unweighted_walk_runs() {
        let options = BTreeMap::from([(
            SmartString::from("steps"),
            Expr::Const {
                val: DataValue::from(4i64),
                span: SourceSpan::default(),
            },
        )]);
        let got = run_fixed_rule(
            &RandomWalk,
            vec![
                TestInput::new(
                    vec!["fr", "to"],
                    vec![
                        Tuple::from_vec(vec![s("a"), s("b")]),
                        Tuple::from_vec(vec![s("b"), s("a")]),
                    ],
                ),
                TestInput::new(
                    vec!["id"],
                    vec![Tuple::from_vec(vec![s("a")]), Tuple::from_vec(vec![s("b")])],
                ),
                TestInput::new(vec!["start"], vec![Tuple::from_vec(vec![s("a")])]),
            ],
            options,
            CancelFlag::default(),
        )
        .unwrap();
        assert_eq!(got.len(), 1);
        let path = got[0][2].get_slice().unwrap();
        assert_eq!(path.len(), 5);
    }

    /// VALUE ORACLE on the one graph where randomness has no choices: the
    /// directed path a→b→c→d (every node has at most one out-edge, so
    /// every "random" pick is forced). Independent of the seed, so this
    /// pins the walk mechanics; the seed's effect is pinned separately by
    /// `run_twice_default_seed_is_byte_identical` below:
    ///   - steps: 3 walks the whole path ⇒ exactly (1, a, [a,b,c,d]);
    ///   - steps: 2 respects the bound  ⇒ exactly (1, a, [a,b,c]);
    ///   - steps: 10 stops at the sink d ⇒ [a,b,c,d] again (4 < 11).
    ///
    /// (The all-zero-weights refusal is pinned separately above.)
    #[test]
    fn deterministic_single_path_walk() {
        let inputs = || {
            vec![
                TestInput::new(
                    vec!["fr", "to"],
                    vec![
                        Tuple::from_vec(vec![s("a"), s("b")]),
                        Tuple::from_vec(vec![s("b"), s("c")]),
                        Tuple::from_vec(vec![s("c"), s("d")]),
                    ],
                ),
                TestInput::new(
                    vec!["id"],
                    vec![
                        Tuple::from_vec(vec![s("a")]),
                        Tuple::from_vec(vec![s("b")]),
                        Tuple::from_vec(vec![s("c")]),
                        Tuple::from_vec(vec![s("d")]),
                    ],
                ),
                TestInput::new(vec!["start"], vec![Tuple::from_vec(vec![s("a")])]),
            ]
        };
        let steps_opt = |n: i64| {
            BTreeMap::from([(
                SmartString::from("steps"),
                Expr::Const {
                    val: DataValue::from(n),
                    span: SourceSpan::default(),
                },
            )])
        };
        for (steps, expected_path) in [
            (3, Tuple::from_vec(vec![s("a"), s("b"), s("c"), s("d")])),
            (2, Tuple::from_vec(vec![s("a"), s("b"), s("c")])),
            (10, Tuple::from_vec(vec![s("a"), s("b"), s("c"), s("d")])),
        ] {
            let got = run_fixed_rule(
                &RandomWalk,
                inputs(),
                steps_opt(steps),
                CancelFlag::default(),
            )
            .unwrap();
            let want: Vec<Tuple> = vec![Tuple::from_vec(vec![
                DataValue::from(1i64),
                s("a"),
                DataValue::List(expected_path.into_vec()),
            ])];
            assert_eq!(got, want, "steps = {steps}");
        }
    }

    /// DETERMINISM: a branching graph (every node has two out-edges) walked
    /// for many steps and iterations makes a long stream of genuine random
    /// choices. With the fixed default seed, two runs are byte-identical;
    /// a mutation back to `rand::rng()` (OS entropy) makes them diverge and
    /// fails this test.
    #[test]
    fn run_twice_default_seed_is_byte_identical() {
        let inputs = || {
            let n = 8usize;
            // A ring plus a chord from each node, so every node branches.
            let mut edges: Vec<Tuple> = vec![];
            for i in 0..n {
                edges.push(Tuple::from_vec(vec![
                    s(&format!("v{i}")),
                    s(&format!("v{}", (i + 1) % n)),
                ]));
                edges.push(Tuple::from_vec(vec![
                    s(&format!("v{i}")),
                    s(&format!("v{}", (i + 3) % n)),
                ]));
            }
            let nodes: Vec<Tuple> = (0..n)
                .map(|i| Tuple::from_vec(vec![s(&format!("v{i}"))]))
                .collect();
            let starts: Vec<Tuple> = (0..n)
                .map(|i| Tuple::from_vec(vec![s(&format!("v{i}"))]))
                .collect();
            vec![
                TestInput::new(vec!["fr", "to"], edges),
                TestInput::new(vec!["id"], nodes),
                TestInput::new(vec!["start"], starts),
            ]
        };
        let opts = || {
            BTreeMap::from([
                (
                    SmartString::from("steps"),
                    Expr::Const {
                        val: DataValue::from(20i64),
                        span: SourceSpan::default(),
                    },
                ),
                (
                    SmartString::from("iterations"),
                    Expr::Const {
                        val: DataValue::from(5i64),
                        span: SourceSpan::default(),
                    },
                ),
            ])
        };
        let first = run_fixed_rule(&RandomWalk, inputs(), opts(), CancelFlag::default()).unwrap();
        // A genuinely random walk: the paths are not all length-1 (choices
        // were actually made), so byte-identity is a real determinism claim.
        assert!(first.iter().any(|r| r[2].get_slice().unwrap().len() > 2));
        for _ in 0..8 {
            let again =
                run_fixed_rule(&RandomWalk, inputs(), opts(), CancelFlag::default()).unwrap();
            assert_eq!(first, again);
        }
    }

    /// DETERMINISM: an explicit `seed` is reproducible and actually steers
    /// the walk — different seeds on the branching graph produce different
    /// paths, so the seed is load-bearing, not decorative.
    #[test]
    fn explicit_seed_is_reproducible_and_load_bearing() {
        let inputs = || {
            let n = 8usize;
            let mut edges: Vec<Tuple> = vec![];
            for i in 0..n {
                edges.push(Tuple::from_vec(vec![
                    s(&format!("v{i}")),
                    s(&format!("v{}", (i + 1) % n)),
                ]));
                edges.push(Tuple::from_vec(vec![
                    s(&format!("v{i}")),
                    s(&format!("v{}", (i + 3) % n)),
                ]));
            }
            let nodes: Vec<Tuple> = (0..n)
                .map(|i| Tuple::from_vec(vec![s(&format!("v{i}"))]))
                .collect();
            vec![
                TestInput::new(vec!["fr", "to"], edges),
                TestInput::new(vec!["id"], nodes),
                TestInput::new(vec!["start"], vec![Tuple::from_vec(vec![s("v0")])]),
            ]
        };
        let opts = |seed: i64| {
            BTreeMap::from([
                (
                    SmartString::from("steps"),
                    Expr::Const {
                        val: DataValue::from(20i64),
                        span: SourceSpan::default(),
                    },
                ),
                (
                    SmartString::from("seed"),
                    Expr::Const {
                        val: DataValue::from(seed),
                        span: SourceSpan::default(),
                    },
                ),
            ])
        };
        let run = |seed: i64| {
            run_fixed_rule(&RandomWalk, inputs(), opts(seed), CancelFlag::default()).unwrap()
        };
        assert_eq!(run(1), run(1));
        let base = run(1);
        assert!(
            (2..40).any(|seed| run(seed) != base),
            "seed does not steer the walk"
        );
    }

    /// GOLDEN VECTOR (default seed): on a fixed branching graph the walk from
    /// the *default* seed produces this exact path — a literal. `run_twice`
    /// above only proves the output is *stable*; this proves it is the
    /// specific value `DEFAULT_SEED` (and the splitmix64 constants in
    /// `rng.rs`) determine, so drifting any of those constants changes the
    /// path and fails here loudly. Every node has two out-edges, so each of
    /// the four steps is a genuine seeded choice.
    #[test]
    fn default_seed_output_is_golden() {
        let n = 6usize;
        let mut edges: Vec<Tuple> = vec![];
        for i in 0..n {
            edges.push(Tuple::from_vec(vec![
                s(&format!("v{i}")),
                s(&format!("v{}", (i + 1) % n)),
            ]));
            edges.push(Tuple::from_vec(vec![
                s(&format!("v{i}")),
                s(&format!("v{}", (i + 2) % n)),
            ]));
        }
        let nodes: Vec<Tuple> = (0..n)
            .map(|i| Tuple::from_vec(vec![s(&format!("v{i}"))]))
            .collect();
        let inputs = vec![
            TestInput::new(vec!["fr", "to"], edges),
            TestInput::new(vec!["id"], nodes),
            TestInput::new(vec!["start"], vec![Tuple::from_vec(vec![s("v0")])]),
        ];
        let opts = BTreeMap::from([(
            SmartString::from("steps"),
            Expr::Const {
                val: DataValue::from(4i64),
                span: SourceSpan::default(),
            },
        )]);
        let got = run_fixed_rule(&RandomWalk, inputs, opts, CancelFlag::default()).unwrap();
        let want: Vec<Tuple> = vec![Tuple::from_vec(vec![
            DataValue::from(1i64),
            s("v0"),
            DataValue::List(vec![s("v0"), s("v1"), s("v3"), s("v4"), s("v0")]),
        ])];
        assert_eq!(got, want);
    }
}
