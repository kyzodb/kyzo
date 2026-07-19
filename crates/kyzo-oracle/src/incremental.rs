/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Story-#61 incremental reference: candidates-then-verify over a DAG.
//!
//! Recursion and fixed rules are refused (never silently wrong). Aggregation
//! is fully covered by group re-derivation.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use kyzo_model::value::{DataValue, Tuple};

use crate::eval::{
    body_bindings_from, check_safety, check_stratifiable, check_wellformed, dependency_edges,
    ground, head_classes, literal_rows, naive_eval, unify, Bindings, HeadAggr, Program, Rejection,
    Rel, Rule, Term,
};
use crate::temporal::{AsOf, SignedFact};
use crate::{AggrFold, NormalAccum};

/// Every relation this program treats as EDB.
pub fn edb_relations(program: &Program) -> BTreeSet<Rel> {
    let idb: BTreeSet<Rel> = program
        .rules
        .iter()
        .map(|r| r.head_rel.clone())
        .chain(program.fixed.iter().map(|f| f.head_rel.clone()))
        .collect();
    let mentioned: BTreeSet<Rel> = program
        .facts
        .keys()
        .cloned()
        .chain(program.histories.keys().cloned())
        .chain(program.rules.iter().flat_map(|r| {
            std::iter::once(r.head_rel.clone()).chain(r.body.iter().map(|l| l.rel.clone()))
        }))
        .chain(program.fixed.iter().flat_map(|f| {
            std::iter::once(f.head_rel.clone()).chain(f.inputs.iter().cloned())
        }))
        .collect();
    mentioned.difference(&idb).cloned().collect()
}

/// Full topological order over every dependency edge.
pub fn topological_order(program: &Program) -> Vec<Rel> {
    let edges = dependency_edges(program);
    let mut all_rels: BTreeSet<Rel> = edb_relations(program);
    for rule in &program.rules {
        all_rels.insert(rule.head_rel.clone());
        for lit in &rule.body {
            all_rels.insert(lit.rel.clone());
        }
    }
    for f in &program.fixed {
        all_rels.insert(f.head_rel.clone());
        for input in &f.inputs {
            all_rels.insert(input.clone());
        }
    }
    let mut depends_on: HashMap<Rel, HashSet<Rel>> = HashMap::new();
    for (head, dep, _) in &edges {
        depends_on
            .entry(head.clone())
            .or_default()
            .insert(dep.clone());
    }
    let mut placed: BTreeSet<Rel> = BTreeSet::new();
    let mut order = Vec::with_capacity(all_rels.len());
    while placed.len() < all_rels.len() {
        let mut progressed = false;
        for rel in &all_rels {
            if placed.contains(rel) {
                continue;
            }
            let ready = depends_on
                .get(rel)
                .is_none_or(|deps| deps.iter().all(|d| placed.contains(d)));
            if ready {
                order.push(rel.clone());
                placed.insert(rel.clone());
                progressed = true;
            }
        }
        assert!(
            progressed,
            "topological_order called on a cyclic program: incremental_eval must refuse first"
        );
    }
    order
}

fn has_any_cycle(program: &Program) -> bool {
    let edges = dependency_edges(program);
    let mut adjacency: HashMap<Rel, HashSet<Rel>> = HashMap::new();
    for (head, dep, _) in &edges {
        adjacency
            .entry(head.clone())
            .or_default()
            .insert(dep.clone());
    }
    let reaches = |from: &Rel, to: &Rel| -> bool {
        let mut seen = HashSet::new();
        let mut stack = vec![from.clone()];
        while let Some(r) = stack.pop() {
            if r == *to {
                return true;
            }
            if seen.insert(r.clone()) {
                stack.extend(adjacency.get(&r).into_iter().flatten().cloned());
            }
        }
        false
    };
    edges.iter().any(|(head, dep, _)| reaches(dep, head))
}

fn collect_candidates(
    rule: &Rule,
    program: &Program,
    total: &BTreeMap<Rel, BTreeSet<Tuple>>,
    rel_deltas: &BTreeMap<Rel, BTreeSet<SignedFact>>,
    default_as_of: AsOf,
    candidates: &mut BTreeSet<Tuple>,
) {
    let varying: Vec<usize> = rule
        .body
        .iter()
        .enumerate()
        .filter(|(_, l)| rel_deltas.get(&l.rel).is_some_and(|d| !d.is_empty()))
        .map(|(i, _)| i)
        .collect();
    if varying.is_empty() {
        return;
    }
    let n = varying.len();
    for mask in 1u32..(1u32 << n) {
        let subset: Vec<usize> = (0..n)
            .filter(|b| mask & (1 << b) != 0)
            .map(|b| varying[b])
            .collect();
        contribute_candidates_subset(
            rule,
            program,
            total,
            rel_deltas,
            &subset,
            default_as_of,
            candidates,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn contribute_candidates_subset(
    rule: &Rule,
    program: &Program,
    total: &BTreeMap<Rel, BTreeSet<Tuple>>,
    rel_deltas: &BTreeMap<Rel, BTreeSet<SignedFact>>,
    subset: &[usize],
    default_as_of: AsOf,
    candidates: &mut BTreeSet<Tuple>,
) {
    let mut frontier: Vec<Bindings> = vec![Bindings::new()];
    for &pos in subset {
        let lit = &rule.body[pos];
        let deltas = &rel_deltas[&lit.rel];
        let mut next = Vec::new();
        for bound in &frontier {
            for fact in deltas {
                let tuple = match fact {
                    SignedFact::Plus(t) | SignedFact::Minus(t) => t,
                };
                if let Some(b) = unify(&lit.args, tuple.as_slice(), bound) {
                    next.push(b);
                }
            }
        }
        frontier = next;
        if frontier.is_empty() {
            return;
        }
    }

    let remaining_positive = rule
        .body
        .iter()
        .enumerate()
        .filter(|(i, l)| !subset.contains(i) && !l.is_negated())
        .map(|(_, l)| l);
    let remaining_negated = rule
        .body
        .iter()
        .enumerate()
        .filter(|(i, l)| !subset.contains(i) && l.is_negated())
        .map(|(_, l)| l);
    for lit in remaining_positive.chain(remaining_negated) {
        let rows = literal_rows(program, total, lit, default_as_of);
        let mut next = Vec::new();
        for bound in &frontier {
            if lit.is_negated() {
                let probe = ground(&lit.args, bound);
                if !rows.contains(&probe) {
                    next.push(bound.clone());
                }
            } else {
                for tuple in &rows {
                    if let Some(b) = unify(&lit.args, tuple.as_slice(), bound) {
                        next.push(b);
                    }
                }
            }
        }
        frontier = next;
        if frontier.is_empty() {
            return;
        }
    }

    for bound in frontier {
        candidates.insert(ground(&rule.head_args, &bound));
    }
}

/// Is `target` derivable from any of `rules` against `db`?
pub fn head_is_derivable(
    rules: &[&Rule],
    program: &Program,
    db: &BTreeMap<Rel, BTreeSet<Tuple>>,
    default_as_of: AsOf,
    target: &Tuple,
) -> bool {
    rules.iter().any(|rule| {
        let Some(seed) = unify(&rule.head_args, target.as_slice(), &Bindings::new()) else {
            return false;
        };
        !body_bindings_from(rule, program, db, default_as_of, seed).is_empty()
    })
}

fn collect_affected_groups(
    rules: &[&Rule],
    program: &Program,
    total: &BTreeMap<Rel, BTreeSet<Tuple>>,
    rel_deltas: &BTreeMap<Rel, BTreeSet<SignedFact>>,
    default_as_of: AsOf,
    key_positions: &[usize],
) -> BTreeSet<Tuple> {
    let mut raw_candidates = BTreeSet::new();
    for rule in rules {
        collect_candidates(
            rule,
            program,
            total,
            rel_deltas,
            default_as_of,
            &mut raw_candidates,
        );
    }
    raw_candidates
        .iter()
        .map(|row| key_positions.iter().map(|i| row[*i].clone()).collect())
        .collect()
}

fn aggr_err(e: String) -> Rejection {
    Rejection::AggrError(e)
}

#[allow(clippy::too_many_arguments)]
fn eval_one_group(
    rules: &[&Rule],
    program: &Program,
    db: &BTreeMap<Rel, BTreeSet<Tuple>>,
    default_as_of: AsOf,
    key_positions: &[usize],
    val_positions: &[(usize, &dyn AggrFold, &[DataValue])],
    signature_len: usize,
    group_key: &Tuple,
) -> Result<Option<Tuple>, Rejection> {
    let fresh_ops = || -> Result<Vec<Box<dyn NormalAccum>>, Rejection> {
        val_positions
            .iter()
            .map(|(_, fold, args)| fold.fresh_normal(args).map_err(aggr_err))
            .collect()
    };
    let mut ops: Option<Vec<Box<dyn NormalAccum>>> = None;
    for rule in rules {
        let mut seed = Bindings::new();
        let mut consistent = true;
        for (slot, &pos) in key_positions.iter().enumerate() {
            match &rule.head_args[pos] {
                Term::Const(c) => {
                    if *c != group_key[slot] {
                        consistent = false;
                        break;
                    }
                }
                Term::Var(name) => {
                    seed.insert(name.clone(), group_key[slot].clone());
                }
            }
        }
        if !consistent {
            continue;
        }
        for binding in body_bindings_from(rule, program, db, default_as_of, seed) {
            let row = ground(&rule.head_args, &binding);
            let ops = ops.get_or_insert_with(|| fresh_ops().expect("infallible fresh fold"));
            for (op, (i, _, _)) in ops.iter_mut().zip(val_positions) {
                op.set(&row[*i]).map_err(aggr_err)?;
            }
        }
    }
    match ops {
        None if key_positions.is_empty() => {
            let mut row = Tuple::from_vec(vec![DataValue::Null; signature_len]);
            for (op, (i, _, _)) in fresh_ops()?.iter().zip(val_positions) {
                row[*i] = op.get().map_err(aggr_err)?;
            }
            Ok(Some(row))
        }
        None => Ok(None),
        Some(ops) => {
            let mut row = Tuple::from_vec(vec![DataValue::Null; signature_len]);
            for (slot, &i) in key_positions.iter().enumerate() {
                row[i] = group_key[slot].clone();
            }
            for (op, (i, _, _)) in ops.iter().zip(val_positions) {
                row[*i] = op.get().map_err(aggr_err)?;
            }
            Ok(Some(row))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn eval_aggregating_head_incremental(
    rules: &[&Rule],
    program: &Program,
    old_total: &BTreeMap<Rel, BTreeSet<Tuple>>,
    new_total: &BTreeMap<Rel, BTreeSet<Tuple>>,
    rel_deltas: &BTreeMap<Rel, BTreeSet<SignedFact>>,
    default_as_of: AsOf,
    old_rows: &BTreeSet<Tuple>,
) -> Result<BTreeSet<SignedFact>, Rejection> {
    let signature = &rules[0].aggr;
    let key_positions: Vec<usize> = signature
        .iter()
        .enumerate()
        .filter(|(_, a)| !a.is_aggregated())
        .map(|(i, _)| i)
        .collect();
    let val_positions: Vec<(usize, &dyn AggrFold, &[DataValue])> = signature
        .iter()
        .enumerate()
        .filter_map(|(i, a)| a.as_aggregated().map(|(fold, args)| (i, fold, args)))
        .collect();

    let old_by_key: BTreeMap<Tuple, Tuple> = old_rows
        .iter()
        .map(|row| {
            let key: Tuple = key_positions.iter().map(|i| row[*i].clone()).collect();
            (key, row.clone())
        })
        .collect();

    let mut affected = collect_affected_groups(
        rules,
        program,
        old_total,
        rel_deltas,
        default_as_of,
        &key_positions,
    );
    if key_positions.is_empty() && rel_deltas.values().any(|d| !d.is_empty()) {
        affected.insert(Tuple::new());
    }

    let mut delta = BTreeSet::new();
    for group_key in &affected {
        let new_row = eval_one_group(
            rules,
            program,
            new_total,
            default_as_of,
            &key_positions,
            &val_positions,
            signature.len(),
            group_key,
        )?;
        let old_row = old_by_key.get(group_key).cloned();
        match (old_row, new_row) {
            (Some(old), Some(new)) if old != new => {
                delta.insert(SignedFact::Minus(old));
                delta.insert(SignedFact::Plus(new));
            }
            (Some(old), None) => {
                delta.insert(SignedFact::Minus(old));
            }
            (None, Some(new)) => {
                delta.insert(SignedFact::Plus(new));
            }
            _ => {}
        }
    }
    Ok(delta)
}

/// Reference incremental maintenance: signed EDB patch → signed relation deltas.
pub fn incremental_eval(
    program: &Program,
    edb_patch: &BTreeMap<Rel, BTreeSet<SignedFact>>,
) -> Result<BTreeMap<Rel, BTreeSet<SignedFact>>, Rejection> {
    check_wellformed(program)?;
    check_safety(program)?;
    check_stratifiable(program)?;
    if has_any_cycle(program) {
        return Err(Rejection::Unstratifiable(
            "incremental maintenance refuses any recursive dependency, not just the \
             stratification-illegal ones — retraction through recursion is DRed territory, \
             out of this story's scope"
                .into(),
        ));
    }
    let classes = head_classes(program);
    if !program.fixed.is_empty() {
        return Err(Rejection::Malformed(
            "incremental maintenance does not cover fixed rules (opaque graph algorithms) — \
             refused, not silently wrong; recompute this program in full instead"
                .into(),
        ));
    }

    let old_total = naive_eval(program)?;
    let order = topological_order(program);
    let edb = edb_relations(program);
    let mut rel_deltas: BTreeMap<Rel, BTreeSet<SignedFact>> = BTreeMap::new();
    let mut new_total: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();

    for rel in order {
        let old_rows = old_total.get(&rel).cloned().unwrap_or_default();
        let (delta, new_rows) = if edb.contains(&rel) {
            let filtered: BTreeSet<SignedFact> = edb_patch
                .get(&rel)
                .into_iter()
                .flatten()
                .filter(|fact| match fact {
                    SignedFact::Plus(t) => !old_rows.contains(t),
                    SignedFact::Minus(t) => old_rows.contains(t),
                })
                .cloned()
                .collect();
            let mut new_rows = old_rows.clone();
            for fact in &filtered {
                match fact {
                    SignedFact::Plus(t) => {
                        new_rows.insert(t.clone());
                    }
                    SignedFact::Minus(t) => {
                        new_rows.remove(t);
                    }
                }
            }
            (filtered, new_rows)
        } else {
            let rules: Vec<&Rule> = program.rules.iter().filter(|r| r.head_rel == rel).collect();
            let delta = if classes[&rel].has_aggr {
                eval_aggregating_head_incremental(
                    &rules,
                    program,
                    &old_total,
                    &new_total,
                    &rel_deltas,
                    AsOf::current(),
                    &old_rows,
                )?
            } else {
                let mut candidates = BTreeSet::new();
                for rule in &rules {
                    collect_candidates(
                        rule,
                        program,
                        &old_total,
                        &rel_deltas,
                        AsOf::current(),
                        &mut candidates,
                    );
                }
                let mut delta = BTreeSet::new();
                for candidate in candidates {
                    let was = old_rows.contains(&candidate);
                    let now =
                        head_is_derivable(&rules, program, &new_total, AsOf::current(), &candidate);
                    match (was, now) {
                        (false, true) => {
                            delta.insert(SignedFact::Plus(candidate));
                        }
                        (true, false) => {
                            delta.insert(SignedFact::Minus(candidate));
                        }
                        _ => {}
                    }
                }
                delta
            };
            let mut new_rows = old_rows.clone();
            for fact in &delta {
                match fact {
                    SignedFact::Plus(t) => {
                        new_rows.insert(t.clone());
                    }
                    SignedFact::Minus(t) => {
                        new_rows.remove(t);
                    }
                }
            }
            (delta, new_rows)
        };
        new_total.insert(rel.clone(), new_rows);
        rel_deltas.insert(rel, delta);
    }
    Ok(rel_deltas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::{Literal, Rule};

    fn v(i: i64) -> DataValue {
        DataValue::from(i)
    }
    fn lit(rel: impl Into<Rel>, args: Vec<Term>, negated: bool) -> Literal {
        if negated {
            Literal::neg(rel, args)
        } else {
            Literal::pos(rel, args)
        }
    }
    fn x() -> Term {
        Term::var("X")
    }
    fn y() -> Term {
        Term::var("Y")
    }
    fn patch_of(
        entries: Vec<(&str, SignedFact)>,
    ) -> BTreeMap<Rel, BTreeSet<SignedFact>> {
        let mut out: BTreeMap<Rel, BTreeSet<SignedFact>> = BTreeMap::new();
        for (rel, fact) in entries {
            out.entry(rel.into()).or_default().insert(fact);
        }
        out
    }
    fn assert_incremental_matches_recompute(
        program: &Program,
        patch: &BTreeMap<Rel, BTreeSet<SignedFact>>,
        label: &str,
    ) {
        let old = naive_eval(program).unwrap();
        let mut patched_facts = program.facts.clone();
        for (rel, delta) in patch {
            let rows = patched_facts.entry(rel.clone()).or_default();
            for fact in delta {
                match fact {
                    SignedFact::Plus(t) => {
                        rows.insert(t.clone());
                    }
                    SignedFact::Minus(t) => {
                        rows.remove(t);
                    }
                }
            }
        }
        let patched = Program::untimed(program.rules.clone(), program.fixed.clone(), patched_facts);
        let new = naive_eval(&patched).unwrap();
        let got = incremental_eval(program, patch).unwrap();
        let all_rels: BTreeSet<_> = old.keys().chain(new.keys()).cloned().collect();
        for rel in all_rels {
            let o = old.get(&rel).cloned().unwrap_or_default();
            let n = new.get(&rel).cloned().unwrap_or_default();
            let mut want = BTreeSet::new();
            for t in o.difference(&n) {
                want.insert(SignedFact::Minus(t.clone()));
            }
            for t in n.difference(&o) {
                want.insert(SignedFact::Plus(t.clone()));
            }
            let g = got.get(&rel).cloned().unwrap_or_default();
            assert_eq!(g, want, "{label}: mismatch on {rel}");
        }
    }

    #[test]
    fn incremental_refuses_recursion_even_when_perfectly_stratifiable() {
        let program = Program::untimed(
            vec![
                Rule::plain(
                    "path",
                    vec![x(), y()],
                    vec![lit("edge", vec![x(), y()], false)],
                ),
                Rule::plain(
                    "path",
                    vec![x(), y()],
                    vec![
                        lit("edge", vec![x(), Term::var("Z")], false),
                        lit("path", vec![Term::var("Z"), y()], false),
                    ],
                ),
            ],
            vec![],
            {
                let mut facts = BTreeMap::new();
                facts.insert(
                    "edge".into(),
                    [vec![v(1), v(2)], vec![v(2), v(3)]]
                        .into_iter()
                        .map(Tuple::from_vec)
                        .collect(),
                );
                facts
            },
        );
        let patch = patch_of(vec![(
            "edge",
            SignedFact::Plus(Tuple::from_vec(vec![v(3), v(4)])),
        )]);
        let err = incremental_eval(&program, &patch).unwrap_err();
        assert!(matches!(err, Rejection::Unstratifiable(_)));
    }

    #[test]
    fn incremental_aggregation_sum_grows_on_assertion() {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "p".into(),
            [vec![v(1), v(10)], vec![v(1), v(20)]]
                .into_iter()
                .map(Tuple::from_vec)
                .collect(),
        );
        let program = Program::untimed(
            vec![Rule::aggregated(
                "total",
                vec![x(), y()],
                vec![HeadAggr::Plain, HeadAggr::named("sum")],
                vec![lit("p", vec![x(), y()], false)],
            )],
            vec![],
            facts,
        );
        let patch = patch_of(vec![(
            "p",
            SignedFact::Plus(Tuple::from_vec(vec![v(1), v(30)])),
        )]);
        assert_incremental_matches_recompute(&program, &patch, "aggregation sum grows");
        let got = incremental_eval(&program, &patch).unwrap();
        assert_eq!(
            got[&Rel::from("total")],
            [
                SignedFact::Minus(Tuple::from_vec(vec![v(1), v(30)])),
                SignedFact::Plus(Tuple::from_vec(vec![v(1), v(60)])),
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn incremental_aggregation_min_rescans_on_retracting_the_current_min() {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "p".into(),
            [vec![v(1), v(10)], vec![v(1), v(20)]]
                .into_iter()
                .map(Tuple::from_vec)
                .collect(),
        );
        let program = Program::untimed(
            vec![Rule::aggregated(
                "total",
                vec![x(), y()],
                vec![HeadAggr::Plain, HeadAggr::named("min")],
                vec![lit("p", vec![x(), y()], false)],
            )],
            vec![],
            facts,
        );
        let patch = patch_of(vec![(
            "p",
            SignedFact::Minus(Tuple::from_vec(vec![v(1), v(10)])),
        )]);
        assert_incremental_matches_recompute(&program, &patch, "min rescans on retract");
        let got = incremental_eval(&program, &patch).unwrap();
        assert_eq!(
            got[&Rel::from("total")],
            [
                SignedFact::Minus(Tuple::from_vec(vec![v(1), v(10)])),
                SignedFact::Plus(Tuple::from_vec(vec![v(1), v(20)])),
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn incremental_plain_join_assertion() {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "p".into(),
            [vec![v(1)]].into_iter().map(Tuple::from_vec).collect(),
        );
        facts.insert(
            "q".into(),
            [vec![v(1), v(10)]].into_iter().map(Tuple::from_vec).collect(),
        );
        let program = Program::untimed(
            vec![Rule::plain(
                "r",
                vec![x(), y()],
                vec![
                    lit("p", vec![x()], false),
                    lit("q", vec![x(), y()], false),
                ],
            )],
            vec![],
            facts,
        );
        let patch = patch_of(vec![(
            "p",
            SignedFact::Plus(Tuple::from_vec(vec![v(2)])),
        )]);
        // q has no (2, _), so r unchanged — but still matches recompute
        assert_incremental_matches_recompute(&program, &patch, "plain join");
    }

    #[test]
    fn incremental_filters_redundant_edb_patch() {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "p".into(),
            [vec![v(1)]].into_iter().map(Tuple::from_vec).collect(),
        );
        let program = Program::untimed(
            vec![Rule::plain(
                "q",
                vec![x()],
                vec![lit("p", vec![x()], false)],
            )],
            vec![],
            facts,
        );
        let patch = patch_of(vec![(
            "p",
            SignedFact::Plus(Tuple::from_vec(vec![v(1)])),
        )]);
        let got = incremental_eval(&program, &patch).unwrap();
        assert!(
            got.get(&Rel::from("p")).map(|d| d.is_empty()).unwrap_or(true),
            "redundant Plus must filter out"
        );
        assert!(
            got.get(&Rel::from("q")).map(|d| d.is_empty()).unwrap_or(true),
            "no IDB change from redundant patch"
        );
    }

    #[test]
    fn zero_rows_still_edb() {
        let program = Program::untimed(
            vec![Rule::plain(
                "q",
                vec![x()],
                vec![lit("p", vec![x()], false)],
            )],
            vec![],
            BTreeMap::new(),
        );
        assert!(edb_relations(&program).contains(&Rel::from("p")));
        let patch = patch_of(vec![(
            "p",
            SignedFact::Plus(Tuple::from_vec(vec![v(1)])),
        )]);
        assert_incremental_matches_recompute(&program, &patch, "zero-rows-still-EDB");
    }
}
