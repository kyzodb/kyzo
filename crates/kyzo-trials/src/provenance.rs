// Copyright 2023 The Cozo Project Authors.
// Copyright 2026 The KyzoDB Authors.
//
// This Source Code Form is subject to the terms of the Mozilla Public License,
// v. 2.0. If a copy of the MPL was not distributed with this file, You can
// obtain one at https://mozilla.org/MPL/2.0/.

//! Capability 2 — provenance: reconstruct a proof, verify it independently.
//!
//! Relocated from condemned `kyzo-core::query::trials`. A recursive query
//! evaluated with first-witness recording on; the proof tree of a chosen
//! derived fact reconstructed from the witness table down to stored ground
//! facts; and an **independent** checker (importing no evaluator symbol)
//! that verifies every step is a valid rule instantiation. A four-corruption
//! negative control corrupts a step and watches the checker reject it —
//! including [`flip_interior_rule`] (sibling-rule mis-attribution).
//!
//! Shares the Cap1 ModelBody harness via [`crate::gauntlet`].

#![cfg(test)]

use std::collections::{BTreeMap, BTreeSet};

use kyzo::oracle_harness::{
    RowLimit, Witness, WitnessTable, collect_materialized, stratified_evaluate,
};
use kyzo_model::value::Tuple;
use kyzo_oracle::{Bindings, Literal, Program, Rel, Rule, unify};

use crate::gauntlet::{
    compile_for, fixed_arities_of, generous_budget, idb_of, lit, model_arities, real_eval, v, x, y,
    z,
};

// ════════════════════════════════════════════════════════════════════════
// CAPABILITY 2 — provenance: reconstruct a proof, verify it independently.
// ════════════════════════════════════════════════════════════════════════

/// A proof tree over the *model* alone: every node names a relation and a
/// tuple; a `Step` also names the rule (by its per-head index) that entailed
/// it and the proofs of its positive premises, in body order.
#[derive(Debug, Clone, PartialEq)]
enum Proof {
    /// A stored ground fact (leaf): the tuple is in the EDB.
    Ground { rel: Rel, tuple: Tuple },
    /// A derived fact: rule `rule_idx` of `rel`'s rules instantiated with
    /// these premises entails `tuple`.
    Step {
        rel: Rel,
        tuple: Tuple,
        rule_idx: usize,
        premises: Vec<Proof>,
    },
}

impl Proof {
    fn head(&self) -> (Rel, &Tuple) {
        match self {
            Proof::Ground { rel, tuple } | Proof::Step { rel, tuple, .. } => (rel.clone(), tuple),
        }
    }
}

/// Group a model's rules by head, preserving program order — the same
/// grouping `compile_for` builds, so a witness's per-head `rule_idx` resolves
/// to the same rule.
fn per_head_rules(model: &Program) -> BTreeMap<Rel, Vec<Rule>> {
    let mut per_head: BTreeMap<Rel, Vec<Rule>> = BTreeMap::new();
    for rule in &model.rules {
        per_head
            .entry(rule.head_rel.clone())
            .or_default()
            .push(rule.clone());
    }
    per_head
}

/// Reconstruct the proof of `(rel, tuple)` from the witness table. Uses the
/// evaluator's output (the witnesses) — the independent checker below does
/// not. Returns `None` at a boundary the first-witness table cannot expand
/// (a derivation-less admission: a normal-aggregation fold, a fixed-rule
/// output, or the meet identity row).
fn reconstruct(
    rel: Rel,
    tuple: &Tuple,
    witnesses: &BTreeMap<(String, Tuple), Witness>,
    per_head: &BTreeMap<Rel, Vec<Rule>>,
    idb: &BTreeSet<Rel>,
) -> Option<Proof> {
    if !idb.contains(&rel) {
        return Some(Proof::Ground {
            rel,
            tuple: tuple.clone(),
        });
    }
    let w = witnesses.get(&(rel.to_string(), tuple.clone()))?;
    let (rule_idx, premise_rows) = w.derivation.as_ref()?;
    let rule = &per_head[&rel][*rule_idx];
    let positives: Vec<&Literal> = rule.body.iter().filter(|l| !l.is_negated()).collect();
    if positives.len() != premise_rows.len() {
        return None;
    }
    let mut premises = Vec::new();
    for (l, row) in positives.iter().zip(premise_rows) {
        premises.push(reconstruct(l.rel.clone(), row, witnesses, per_head, idb)?);
    }
    Some(Proof::Step {
        rel,
        tuple: tuple.clone(),
        rule_idx: *rule_idx,
        premises,
    })
}

fn index_witnesses(table: &WitnessTable) -> BTreeMap<(String, Tuple), Witness> {
    let mut map = BTreeMap::new();
    for w in table.entries() {
        // First witness wins (admission order); later re-derivations ignored.
        map.entry((w.store.as_plain_symbol().name.to_string(), w.tuple.clone()))
            .or_insert_with(|| w.clone());
    }
    map
}

// ── The independent checker ──────────────────────────────────────────────
//
// `verify` imports no EVALUATOR symbol: only the model (`Rule`, `Literal`,
// `Term`), the shared reference-tier `unify` (`kyzo_oracle`), and plain
// data. It re-derives each step's binding from scratch, so a corrupted
// proof cannot pass by echoing eval's own reasoning.

/// Verify a proof tree. `Ok(())` iff every leaf is a genuine ground fact and
/// every step is a valid instantiation of the named rule whose positive
/// premises are exactly the child tuples. Rules with a negated premise are a
/// documented boundary — the checker refuses to bless them rather than pretend.
fn verify(
    proof: &Proof,
    per_head: &BTreeMap<Rel, Vec<Rule>>,
    facts: &BTreeMap<Rel, BTreeSet<Tuple>>,
) -> std::result::Result<(), String> {
    match proof {
        Proof::Ground { rel, tuple } => {
            if facts.get(rel).is_some_and(|s| s.contains(tuple)) {
                Ok(())
            } else {
                Err(format!("leaf {rel}{tuple:?} is not a stored ground fact"))
            }
        }
        Proof::Step {
            rel,
            tuple,
            rule_idx,
            premises,
        } => {
            let rules = per_head
                .get(rel)
                .ok_or_else(|| format!("no rules for head '{rel}'"))?;
            let rule = rules
                .get(*rule_idx)
                .ok_or_else(|| format!("rule index {rule_idx} out of range for '{rel}'"))?;
            if rule.head_rel != *rel {
                return Err(format!("rule head '{}' ≠ claimed '{rel}'", rule.head_rel));
            }
            if rule.body.iter().any(|l| l.is_negated()) {
                return Err(format!(
                    "boundary: rule for '{rel}' has a negated premise, not \
                     independently checkable from a proof tree"
                ));
            }
            let positives: Vec<&Literal> = rule.body.iter().filter(|l| !l.is_negated()).collect();
            if positives.len() != premises.len() {
                return Err(format!(
                    "'{rel}': {} premises for {} positive body literals",
                    premises.len(),
                    positives.len()
                ));
            }
            let mut bound: Bindings = Bindings::new();
            bound = match unify(&rule.head_args, tuple.as_slice(), &bound) {
                Some(b) => b,
                None => {
                    return Err(format!(
                        "head of rule {rule_idx} does not ground to {tuple:?}"
                    ));
                }
            };
            for (l, child) in positives.iter().zip(premises) {
                let (crel, ctuple) = child.head();
                if crel != l.rel {
                    return Err(format!(
                        "premise relation '{crel}' ≠ body literal '{}'",
                        l.rel
                    ));
                }
                bound = match unify(&l.args, ctuple.as_slice(), &bound) {
                    Some(b) => b,
                    None => {
                        return Err(format!(
                            "premise {crel}{ctuple:?} inconsistent with binding"
                        ));
                    }
                };
            }
            for child in premises {
                verify(child, per_head, facts)?;
            }
            Ok(())
        }
    }
}

/// The provenance fixture: a positive recursive program (transitive closure
/// joined onward through a second edge relation) with no negation or
/// aggregation in the proof-carrying relations, so the checker is complete
/// for it.
fn provenance_fixture() -> (Program, Rel) {
    let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
    facts.insert(
        "edge".into(),
        [(1, 2), (2, 3), (3, 4), (2, 5)]
            .iter()
            .map(|(a, b)| vec![v(*a), v(*b)])
            .map(Tuple::from_vec)
            .collect(),
    );
    facts.insert(
        "tag".into(),
        [(4, 100), (5, 200)]
            .iter()
            .map(|(a, b)| vec![v(*a), v(*b)])
            .map(Tuple::from_vec)
            .collect(),
    );
    let rules = vec![
        Rule::plain(
            "path",
            vec![x(), y()],
            vec![lit("edge", vec![x(), y()], false)],
        ),
        Rule::plain(
            "path",
            vec![x(), z()],
            vec![
                lit("edge", vec![x(), y()], false),
                lit("path", vec![y(), z()], false),
            ],
        ),
        Rule::plain(
            "labeled",
            vec![x(), z()],
            vec![
                lit("path", vec![x(), y()], false),
                lit("tag", vec![y(), z()], false),
            ],
        ),
    ];
    (Program::untimed(rules, vec![], facts), "labeled".into())
}

fn run_with_witnesses(
    model: &Program,
    entry: Rel,
    entry_arity: usize,
) -> (
    BTreeSet<Tuple>,
    BTreeMap<(String, Tuple), Witness>,
    BTreeMap<Rel, Vec<Rule>>,
    BTreeSet<Rel>,
) {
    let arities = model_arities(model);
    let fixed_arities = fixed_arities_of(model, &arities);
    let compiled = compile_for(model, entry.clone(), entry_arity, &fixed_arities);
    let mut table = WitnessTable::default();
    let outcome = stratified_evaluate(
        &compiled.program,
        &compiled.lifetimes,
        RowLimit::default(),
        &generous_budget(),
        Some(&mut table),
    )
    .expect("evaluates");
    let rows: BTreeSet<Tuple> =
        collect_materialized(outcome.store.all_iter().expect("harness: store iter"))
            .expect("harness: materialize")
            .into_iter()
            .collect();
    (
        rows,
        index_witnesses(&table),
        per_head_rules(model),
        idb_of(model),
    )
}

#[test]
fn provenance_reconstructs_and_verifies_every_derived_fact() {
    let (model, entry) = provenance_fixture();
    let (rows, witnesses, per_head, idb) = run_with_witnesses(&model, entry.clone(), 2);
    assert!(!rows.is_empty(), "the fixture derives facts");

    for tuple in &rows {
        let proof = reconstruct(entry.clone(), tuple, &witnesses, &per_head, &idb)
            .unwrap_or_else(|| panic!("reconstruct {entry}{tuple:?}"));
        assert_eq!(proof.head(), (entry.clone(), tuple));
        verify(&proof, &per_head, &model.facts)
            .unwrap_or_else(|e| panic!("checker rejected an honest proof of {tuple:?}: {e}"));
        assert!(all_leaves_ground(&proof, &model.facts));
    }

    let path_rows = real_eval(&model, "path", 2, &BTreeMap::new(), &generous_budget()).unwrap();
    for tuple in &path_rows {
        let proof = reconstruct("path".into(), tuple, &witnesses, &per_head, &idb)
            .unwrap_or_else(|| panic!("reconstruct path{tuple:?}"));
        verify(&proof, &per_head, &model.facts)
            .unwrap_or_else(|e| panic!("checker rejected honest path proof: {e}"));
    }
}

fn all_leaves_ground(proof: &Proof, facts: &BTreeMap<Rel, BTreeSet<Tuple>>) -> bool {
    match proof {
        Proof::Ground { rel, tuple } => facts.get(rel).is_some_and(|s| s.contains(tuple)),
        Proof::Step { premises, .. } => premises.iter().all(|p| all_leaves_ground(p, facts)),
    }
}

#[test]
fn provenance_negative_control_checker_rejects_corruption() {
    let (model, entry) = provenance_fixture();
    let (rows, witnesses, per_head, idb) = run_with_witnesses(&model, entry.clone(), 2);

    let target = rows
        .iter()
        .find(|t| {
            let proof = reconstruct(entry.clone(), t, &witnesses, &per_head, &idb).unwrap();
            proof_depth(&proof) >= 3
        })
        .expect("a multi-step labeled fact exists");
    let honest = reconstruct(entry.clone(), target, &witnesses, &per_head, &idb).unwrap();
    verify(&honest, &per_head, &model.facts).expect("honest proof verifies");

    // (a) Corrupt an interior premise tuple.
    let corrupt_premise = corrupt_first_step_premise(&honest);
    assert!(
        verify(&corrupt_premise, &per_head, &model.facts).is_err(),
        "checker must reject a corrupted premise tuple"
    );

    // (b) Corrupt the derived tuple of the root.
    let corrupt_head = match honest.clone() {
        Proof::Step {
            rel,
            mut tuple,
            rule_idx,
            premises,
        } => {
            tuple[0] = v(9999);
            Proof::Step {
                rel,
                tuple,
                rule_idx,
                premises,
            }
        }
        g => g,
    };
    assert!(
        verify(&corrupt_head, &per_head, &model.facts).is_err(),
        "checker must reject a corrupted conclusion"
    );

    // (c) Corrupt the rule index of the root to an out-of-range value.
    let corrupt_root_idx = match honest.clone() {
        Proof::Step {
            rel,
            tuple,
            premises,
            ..
        } => Proof::Step {
            rel,
            tuple,
            rule_idx: 999,
            premises,
        },
        g => g,
    };
    assert!(
        verify(&corrupt_root_idx, &per_head, &model.facts).is_err(),
        "checker must reject an out-of-range rule index"
    );

    // (d) Mis-attribute an interior step to the *sibling* rule of a
    //     multi-rule head (path has a base and a recursive rule).
    let corrupt_sibling = flip_interior_rule(&honest, &per_head);
    assert_ne!(
        corrupt_sibling, honest,
        "the fixture has an interior multi-rule step to flip"
    );
    assert!(
        verify(&corrupt_sibling, &per_head, &model.facts).is_err(),
        "checker must reject a step attributed to the wrong rule of its head"
    );
}

/// Flip the first interior `Step` whose head has more than one rule to a
/// different (valid) rule index of that head.
fn flip_interior_rule(proof: &Proof, per_head: &BTreeMap<Rel, Vec<Rule>>) -> Proof {
    match proof {
        Proof::Ground { .. } => proof.clone(),
        Proof::Step {
            rel,
            tuple,
            rule_idx,
            premises,
        } => {
            let n_rules = per_head.get(rel).map(|r| r.len()).unwrap_or(0);
            if n_rules > 1 {
                return Proof::Step {
                    rel: rel.clone(),
                    tuple: tuple.clone(),
                    rule_idx: (rule_idx + 1) % n_rules,
                    premises: premises.clone(),
                };
            }
            let mut premises = premises.clone();
            for p in premises.iter_mut() {
                let flipped = flip_interior_rule(p, per_head);
                if flipped != *p {
                    *p = flipped;
                    break;
                }
            }
            Proof::Step {
                rel: rel.clone(),
                tuple: tuple.clone(),
                rule_idx: *rule_idx,
                premises,
            }
        }
    }
}

fn proof_depth(proof: &Proof) -> usize {
    match proof {
        Proof::Ground { .. } => 1,
        Proof::Step { premises, .. } => 1 + premises.iter().map(proof_depth).max().unwrap_or(0),
    }
}

fn corrupt_first_step_premise(proof: &Proof) -> Proof {
    let Proof::Step {
        rel,
        tuple,
        rule_idx,
        premises,
    } = proof
    else {
        return proof.clone();
    };
    let mut premises = premises.clone();
    let pos = premises
        .iter()
        .position(|p| matches!(p, Proof::Step { .. }))
        .unwrap_or(0);
    if let Some(p) = premises.get_mut(pos) {
        *p = with_bumped_tuple(p);
    }
    Proof::Step {
        rel: rel.clone(),
        tuple: tuple.clone(),
        rule_idx: *rule_idx,
        premises,
    }
}

fn with_bumped_tuple(p: &Proof) -> Proof {
    match p {
        Proof::Ground { rel, tuple } => {
            let mut t = tuple.clone();
            t[0] = v(7777);
            Proof::Ground {
                rel: rel.clone(),
                tuple: t,
            }
        }
        Proof::Step {
            rel,
            tuple,
            rule_idx,
            premises,
        } => {
            let mut t = tuple.clone();
            t[0] = v(7777);
            Proof::Step {
                rel: rel.clone(),
                tuple: t,
                rule_idx: *rule_idx,
                premises: premises.clone(),
            }
        }
    }
}
