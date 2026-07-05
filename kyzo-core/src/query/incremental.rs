/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Story #61's production incremental-maintenance engine: an
//! independently-written twin of `query/laws.rs`'s `incremental_eval` —
//! never a shared import, the same relationship `query/ra/temporal.rs`'s
//! `SignedFact`/`compose` have to `laws.rs`'s oracle versions, so a bug
//! cannot hide behind one implementation covering for the other. This
//! module's own test suite differentials [`incremental_eval`] against
//! the oracle on the same generated program shapes — the full chain is
//! then: production == oracle `incremental_eval` (this differential) ==
//! `naive_eval` recompute (`laws.rs`'s own differential), the transitive
//! proof issue #61's DoD demands.
//!
//! [`SignedFact`] is reused directly from `query::ra::temporal` — it is
//! ALREADY production code (story #62's `DeltaRA` fast path is its first
//! caller), not oracle-only, so there is no test/production boundary to
//! cross by depending on it here. `compose` is not: candidates-then-
//! verify (below) never composes two already-computed patches together —
//! each candidate's `Plus`/`Minus` comes directly from comparing its own
//! old-vs-new derivability, the same reason `laws::incremental_eval`
//! itself stopped calling `compose` once the multiset-vs-set bug was
//! found and fixed (see that module's doc).
//!
//! ## Scope
//!
//! Identical to `laws::incremental_eval`, for identical reasons (see its
//! module doc): RECURSION is refused outright (DRed — retraction through
//! a recursive derivation — is separate, harder scope); AGGREGATION is
//! refused for now (a follow-up unit, not silently dropped); FIXED RULES
//! have no representation here at all (this module's [`Rule`] has no
//! opaque-function variant) — there is nothing to refuse because nothing
//! constructs one, the same "unrepresentable, not merely refused" posture
//! the type system prefers over a runtime check where it can have it.
//!
//! ## The algorithm
//!
//! Two phases, per relation, in topological order — see `laws.rs`'s own
//! module doc for the full derivation and the multiset-vs-set-semantics
//! pitfall the oracle's differential caught on its first run (a `Program`
//! there has an in-memory `facts`/`histories` EDB and a full-recompute
//! reference to check against; a maintained standing query has neither —
//! its "EDB" is the caller-supplied `edb_patch` plus whatever
//! `MaintainedState` already holds, and there is no reference recompute
//! to check against at this layer, only the differential against the
//! oracle):
//!
//! 1. [`collect_candidates`] finds every grounded head tuple ANY rule of
//!    a relation could possibly have gained or lost a derivation for —
//!    delta-bounded, never a full scan.
//! 2. [`head_is_derivable`] verifies each candidate directly against the
//!    OLD state (`MaintainedState`, read-only) versus the NEW state
//!    (built up alongside it in the same topological pass) — only a real
//!    truth-value flip becomes a `Plus`/`Minus`.

use std::collections::{BTreeMap, BTreeSet};

use miette::{Error, Result};

use crate::data::symb::Symbol;
use crate::data::tuple::Tuple;
use crate::data::value::DataValue;
use crate::query::ra::temporal::SignedFact;

/// One rule-body argument: a bound value, or a variable to unify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Term {
    Const(DataValue),
    Var(Symbol),
}

/// One rule-body literal: a relation read, optionally negated.
#[derive(Debug, Clone)]
pub(crate) struct Literal {
    pub(crate) rel: Symbol,
    pub(crate) args: Vec<Term>,
    pub(crate) negated: bool,
}

/// One derivation rule: `head_rel(head_args) :- body`. No aggregation
/// signature and no fixed-rule variant — both are unrepresentable here
/// (see the module doc's scope section), not merely refused at runtime.
#[derive(Debug, Clone)]
pub(crate) struct Rule {
    pub(crate) head_rel: Symbol,
    pub(crate) head_args: Vec<Term>,
    pub(crate) body: Vec<Literal>,
}

/// A standing query's rule set. Unlike `laws::Program`, there is no
/// inline `facts` field: EDB content lives in the caller's
/// [`MaintainedState`], never inline in the program itself — a standing
/// query's whole point is that its EDB changes out from under it, commit
/// after commit.
#[derive(Debug, Clone, Default)]
pub(crate) struct IncrementalProgram {
    pub(crate) rules: Vec<Rule>,
}

/// Every relation's current fully-materialized row set — the persistent,
/// cross-commit state a standing query owns and this module reads and
/// updates. Unlike `EpochStore` (`query/levels.rs`), this is NOT ephemeral
/// per-query-run scaffolding: `EpochStore` is a monotone-only (assert-only,
/// no retraction), single-fixpoint-run structure with no `Clone` impl and
/// no way to answer "what was this relation's state before the last
/// patch" — exactly the two things a standing query needs forever. This
/// type is the production twin of `laws::naive_eval`'s return value
/// (`BTreeMap<Rel, BTreeSet<Tuple>>`), long-lived instead of one-shot.
pub(crate) type MaintainedState = BTreeMap<Symbol, BTreeSet<Tuple>>;

/// One literal's variable bindings so far, keyed by variable identity
/// (`Symbol`'s `Ord`/`Eq` is name-only, span-independent — see
/// `data/symb.rs`'s doc). `BTreeMap`, not `HashMap`: every consumer of a
/// `Bindings` here only ever looks up by key or grounds a fixed arg list
/// through it, never iterates it, so this costs nothing and removes any
/// doubt about hash-randomization affecting output order (the exact
/// question raised — and cleared by other means — against the oracle's
/// own generative differential).
pub(crate) type Bindings = BTreeMap<Symbol, DataValue>;

/// Unify one literal's argument list against a candidate tuple, extending
/// `bound`. Independently written from `laws::unify` (same shape,
/// different types) — see the module doc for why the two never share
/// code.
pub(crate) fn unify(args: &[Term], tuple: &[DataValue], bound: &Bindings) -> Option<Bindings> {
    if args.len() != tuple.len() {
        return None;
    }
    let mut out = bound.clone();
    for (t, v) in args.iter().zip(tuple) {
        match t {
            Term::Const(c) => {
                if c != v {
                    return None;
                }
            }
            Term::Var(name) => match out.get(name) {
                Some(existing) if existing != v => return None,
                Some(_) => {}
                None => {
                    out.insert(name.clone(), v.clone());
                }
            },
        }
    }
    Some(out)
}

/// Instantiate an argument list against a complete binding — the
/// [`unify`] counterpart.
pub(crate) fn ground(args: &[Term], bound: &Bindings) -> Tuple {
    args.iter()
        .map(|t| match t {
            Term::Const(c) => c.clone(),
            Term::Var(v) => bound[v].clone(),
        })
        .collect()
}

/// The rows a literal reading `lit.rel` sees in `state` — empty if the
/// relation has no entry at all (a relation with zero current rows, not
/// yet touched, is not an error).
fn literal_rows(state: &MaintainedState, lit: &Literal) -> BTreeSet<Tuple> {
    state.get(&lit.rel).cloned().unwrap_or_default()
}

/// Every relation this program treats as EDB: mentioned in some rule's
/// head or body, in `patched` (the incoming patch's own relation set —
/// a relation the caller is patching is EDB even when the CURRENT rule
/// set happens not to reference it, e.g. a standing query mid-edit, or
/// simply an unrelated relation in the same commit), but never a rule
/// HEAD itself. Mirrors `laws::edb_relations`'s reasoning exactly: a
/// relation with zero current rows (nothing in `MaintainedState` for it
/// yet) is still EDB, never misclassified as "IDB with zero matching
/// rules" (which would silently drop its own patch entries into an
/// empty delta) — the SAME bug this now guards against a second way
/// (this module's own differential caught a relation the rule set never
/// mentions being silently dropped entirely, not just misclassified).
fn edb_relations(program: &IncrementalProgram, patched: &BTreeSet<Symbol>) -> BTreeSet<Symbol> {
    let idb: BTreeSet<Symbol> = program.rules.iter().map(|r| r.head_rel.clone()).collect();
    let mentioned: BTreeSet<Symbol> = program
        .rules
        .iter()
        .flat_map(|r| {
            std::iter::once(r.head_rel.clone()).chain(r.body.iter().map(|l| l.rel.clone()))
        })
        .chain(patched.iter().cloned())
        .collect();
    mentioned.difference(&idb).cloned().collect()
}

/// A full topological order over every dependency edge, plus every
/// patched-but-otherwise-unreferenced EDB relation (sorted first — they
/// have no dependencies, since nothing in the rule set reads them) —
/// sound only because [`incremental_eval`] has already refused any
/// program with a cycle at all (see [`has_any_cycle`]).
fn topological_order(program: &IncrementalProgram, patched: &BTreeSet<Symbol>) -> Vec<Symbol> {
    let mut all_rels: BTreeSet<Symbol> = edb_relations(program, patched);
    for rule in &program.rules {
        all_rels.insert(rule.head_rel.clone());
        for lit in &rule.body {
            all_rels.insert(lit.rel.clone());
        }
    }
    let mut depends_on: BTreeMap<Symbol, BTreeSet<Symbol>> = BTreeMap::new();
    for rule in &program.rules {
        for lit in &rule.body {
            depends_on
                .entry(rule.head_rel.clone())
                .or_default()
                .insert(lit.rel.clone());
        }
    }
    let mut placed: BTreeSet<Symbol> = BTreeSet::new();
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

/// Does `program`'s dependency graph contain a cycle at all? Recursion is
/// refused unconditionally here (see the module doc's scope section),
/// not just illegal-for-stratification cycles.
fn has_any_cycle(program: &IncrementalProgram) -> bool {
    let mut adjacency: BTreeMap<Symbol, BTreeSet<Symbol>> = BTreeMap::new();
    for rule in &program.rules {
        for lit in &rule.body {
            adjacency
                .entry(rule.head_rel.clone())
                .or_default()
                .insert(lit.rel.clone());
        }
    }
    let reaches = |from: &Symbol, to: &Symbol| -> bool {
        let mut seen = BTreeSet::new();
        let mut stack = vec![from.clone()];
        while let Some(r) = stack.pop() {
            if &r == to {
                return true;
            }
            if seen.insert(r.clone())
                && let Some(deps) = adjacency.get(&r)
            {
                stack.extend(deps.iter().cloned());
            }
        }
        false
    };
    // For every dependency edge (head -> dep), the edge closes a cycle
    // iff `dep` can reach BACK to `head` through the rest of the graph —
    // checking `reaches(head, head)` directly would trivially be true on
    // the very first stack pop (the search starts AT `head`) regardless
    // of whether any real cycle exists, which is exactly the bug this
    // module's own test suite caught (every program, recursive or not,
    // was refused).
    program.rules.iter().any(|rule| {
        rule.body
            .iter()
            .any(|lit| reaches(&lit.rel, &rule.head_rel))
    })
}

/// Every grounded head tuple ONE rule could possibly have gained or lost
/// a derivation for this round. See `laws::collect_candidates`'s doc for
/// the full subset-expansion derivation — identical algorithm, different
/// types.
fn collect_candidates(
    rule: &Rule,
    state: &MaintainedState,
    rel_deltas: &BTreeMap<Symbol, BTreeSet<SignedFact>>,
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
        contribute_candidates_subset(rule, state, rel_deltas, &subset, candidates);
    }
}

/// One non-empty subset of body positions treated as this pass's
/// "drivers" (iterate their delta tuples' bindings, regardless of sign);
/// every other position is a plain join (positive) or gate (negated)
/// against `state`, the stable old state.
fn contribute_candidates_subset(
    rule: &Rule,
    state: &MaintainedState,
    rel_deltas: &BTreeMap<Symbol, BTreeSet<SignedFact>>,
    subset: &[usize],
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
                if let Some(b) = unify(&lit.args, tuple, bound) {
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
        .filter(|(i, l)| !subset.contains(i) && !l.negated)
        .map(|(_, l)| l);
    let remaining_negated = rule
        .body
        .iter()
        .enumerate()
        .filter(|(i, l)| !subset.contains(i) && l.negated)
        .map(|(_, l)| l);
    for lit in remaining_positive.chain(remaining_negated) {
        let rows = literal_rows(state, lit);
        let mut next = Vec::new();
        for bound in &frontier {
            if lit.negated {
                let probe = ground(&lit.args, bound);
                if !rows.contains(&probe) {
                    next.push(bound.clone());
                }
            } else {
                for tuple in &rows {
                    if let Some(b) = unify(&lit.args, tuple, bound) {
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

    for bound in &frontier {
        candidates.insert(ground(&rule.head_args, bound));
    }
}

/// Is `target` derivable from ANY of `rules` (every rule of one head),
/// evaluated against `state`? Seeds the search from `target`'s own
/// values unified against each rule's head arguments, then an ordinary
/// body join from there — bounded by one relation's body cost, never a
/// full relation re-derivation.
fn head_is_derivable(rules: &[&Rule], state: &MaintainedState, target: &Tuple) -> bool {
    rules.iter().any(|rule| {
        let Some(seed) = unify(&rule.head_args, target, &Bindings::new()) else {
            return false;
        };
        !body_bindings_from(rule, state, seed).is_empty()
    })
}

/// All satisfying bindings of a rule body against `state`, starting from
/// an already-known partial binding (`head_is_derivable`'s seed).
/// Positives first, so safety guarantees negated literals are fully
/// bound when probed — the same ordering `laws::body_bindings` uses.
fn body_bindings_from(rule: &Rule, state: &MaintainedState, initial: Bindings) -> Vec<Bindings> {
    let mut ordered: Vec<&Literal> = rule.body.iter().filter(|l| !l.negated).collect();
    ordered.extend(rule.body.iter().filter(|l| l.negated));

    let mut frontier: Vec<Bindings> = vec![initial];
    for lit in ordered {
        let rows = literal_rows(state, lit);
        let mut next = Vec::new();
        for bound in &frontier {
            if lit.negated {
                let probe = ground(&lit.args, bound);
                if !rows.contains(&probe) {
                    next.push(bound.clone());
                }
            } else {
                for tuple in &rows {
                    if let Some(b) = unify(&lit.args, tuple, bound) {
                        next.push(b);
                    }
                }
            }
        }
        frontier = next;
    }
    frontier
}

/// A program this module refuses outright (never a wrong answer instead).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub(crate) enum IncrementalRejection {
    #[error("incremental maintenance refuses any recursive dependency (DRed is separate scope)")]
    #[diagnostic(code(incremental::recursive))]
    Recursive,
}

/// The production incremental-maintenance law (issue #61): given a signed
/// patch to `program`'s EDB and its CURRENT [`MaintainedState`], the
/// signed patch every relation (EDB and IDB alike) undergoes, computed
/// WITHOUT re-evaluating the whole program — and the NEW state, for the
/// caller to persist as this round's [`MaintainedState`] going forward.
/// Refuses (never silently wrong) recursion; aggregation and fixed rules
/// have no representation in [`IncrementalProgram`] at all, so there is
/// nothing to refuse for them here — see the module doc's scope section.
pub(crate) fn incremental_eval(
    program: &IncrementalProgram,
    state: &MaintainedState,
    edb_patch: &BTreeMap<Symbol, BTreeSet<SignedFact>>,
) -> Result<(BTreeMap<Symbol, BTreeSet<SignedFact>>, MaintainedState)> {
    if has_any_cycle(program) {
        return Err(Error::from(IncrementalRejection::Recursive));
    }

    let patched: BTreeSet<Symbol> = edb_patch.keys().cloned().collect();
    let order = topological_order(program, &patched);
    let edb = edb_relations(program, &patched);
    let mut rel_deltas: BTreeMap<Symbol, BTreeSet<SignedFact>> = BTreeMap::new();
    let mut new_state: MaintainedState = BTreeMap::new();

    for rel in order {
        let old_rows = state.get(&rel).cloned().unwrap_or_default();
        let (delta, new_rows) = if edb.contains(&rel) {
            // A redundant patch entry (asserting an already-present fact,
            // retracting an absent one) is a no-op on the SET — filtered
            // out, not forwarded verbatim (the exact bug the oracle's
            // differential caught on its first run).
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
            let mut candidates = BTreeSet::new();
            for rule in &rules {
                collect_candidates(rule, state, &rel_deltas, &mut candidates);
            }
            let mut delta = BTreeSet::new();
            let mut new_rows = old_rows.clone();
            for candidate in candidates {
                let was = old_rows.contains(&candidate);
                let now = head_is_derivable(&rules, &new_state, &candidate);
                match (was, now) {
                    (false, true) => {
                        delta.insert(SignedFact::Plus(candidate.clone()));
                        new_rows.insert(candidate);
                    }
                    (true, false) => {
                        delta.insert(SignedFact::Minus(candidate.clone()));
                        new_rows.remove(&candidate);
                    }
                    _ => {}
                }
            }
            (delta, new_rows)
        };
        new_state.insert(rel.clone(), new_rows);
        rel_deltas.insert(rel, delta);
    }
    Ok((rel_deltas, new_state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::span::SourceSpan;
    use crate::data::value::Num;
    use crate::query::laws;

    fn sym(name: &str) -> Symbol {
        Symbol::new(name, SourceSpan::default())
    }
    fn v(i: i64) -> DataValue {
        DataValue::Num(Num::Int(i))
    }
    fn x() -> Term {
        Term::Var(sym("X"))
    }
    fn y() -> Term {
        Term::Var(sym("Y"))
    }
    fn lit(rel: &str, args: Vec<Term>, negated: bool) -> Literal {
        Literal {
            rel: sym(rel),
            args,
            negated,
        }
    }
    fn rule(head_rel: &str, head_args: Vec<Term>, body: Vec<Literal>) -> Rule {
        Rule {
            head_rel: sym(head_rel),
            head_args,
            body,
        }
    }
    fn state_of(entries: Vec<(&str, Vec<Tuple>)>) -> MaintainedState {
        entries
            .into_iter()
            .map(|(rel, rows)| (sym(rel), rows.into_iter().collect()))
            .collect()
    }
    fn patch_of(entries: Vec<(&str, SignedFact)>) -> BTreeMap<Symbol, BTreeSet<SignedFact>> {
        let mut out: BTreeMap<Symbol, BTreeSet<SignedFact>> = BTreeMap::new();
        for (rel, fact) in entries {
            out.entry(sym(rel)).or_default().insert(fact);
        }
        out
    }

    /// The hard corner, direct: `q(x) :- p(x), not r(x)`. Retracting
    /// `r(1)` while `p(1)` already holds must make `q(1)` newly true.
    #[test]
    fn retraction_through_negation_produces_a_new_fact() {
        let program = IncrementalProgram {
            rules: vec![rule(
                "q",
                vec![x()],
                vec![lit("p", vec![x()], false), lit("r", vec![x()], true)],
            )],
        };
        let state = state_of(vec![("p", vec![vec![v(1)]]), ("r", vec![vec![v(1)]])]);
        let patch = patch_of(vec![("r", SignedFact::Minus(vec![v(1)]))]);
        let (deltas, new_state) = incremental_eval(&program, &state, &patch).unwrap();
        assert_eq!(
            deltas[&sym("q")],
            [SignedFact::Plus(vec![v(1)])].into_iter().collect()
        );
        assert_eq!(new_state[&sym("q")], [vec![v(1)]].into_iter().collect());
    }

    /// The mirror: asserting into the negated relation retracts the
    /// dependent fact.
    #[test]
    fn assertion_into_negation_retracts_the_dependent_fact() {
        let program = IncrementalProgram {
            rules: vec![rule(
                "q",
                vec![x()],
                vec![lit("p", vec![x()], false), lit("r", vec![x()], true)],
            )],
        };
        // p(1) with r initially empty: not-r(1) holds, so q(1) is ALREADY
        // true before the patch — MaintainedState must say so, not just
        // the base relations, since this module never re-derives from
        // scratch to find out.
        let state = state_of(vec![
            ("p", vec![vec![v(1)]]),
            ("r", vec![]),
            ("q", vec![vec![v(1)]]),
        ]);
        let patch = patch_of(vec![("r", SignedFact::Plus(vec![v(1)]))]);
        let (deltas, new_state) = incremental_eval(&program, &state, &patch).unwrap();
        assert_eq!(
            deltas[&sym("q")],
            [SignedFact::Minus(vec![v(1)])].into_iter().collect()
        );
        assert!(new_state[&sym("q")].is_empty());
    }

    /// A tuple with TWO independent derivations: only ONE is touched by
    /// the patch, and the untouched one must still hold the fact up —
    /// exactly the multiset-vs-set bug the oracle's differential caught
    /// on its first run (`laws.rs`'s module doc). This module's own
    /// direct test for the same law.
    #[test]
    fn a_second_untouched_derivation_holds_the_fact_up() {
        let program = IncrementalProgram {
            rules: vec![rule(
                "q",
                vec![x()],
                vec![lit("p", vec![x(), y()], false), lit("r", vec![x()], true)],
            )],
        };
        // p(2,1) and p(2,3): two derivations of q(2), both already
        // reflected in q's own prior MaintainedState. Only p(2,1) is
        // retracted; p(2,3) still supports q(2) unchanged.
        let state = state_of(vec![
            ("p", vec![vec![v(2), v(1)], vec![v(2), v(3)]]),
            ("r", vec![]),
            ("q", vec![vec![v(2)]]),
        ]);
        let patch = patch_of(vec![("p", SignedFact::Minus(vec![v(2), v(1)]))]);
        let (deltas, new_state) = incremental_eval(&program, &state, &patch).unwrap();
        assert!(
            deltas.get(&sym("q")).is_none_or(BTreeSet::is_empty),
            "q(2) has a second, untouched derivation — must not change: {:?}",
            deltas.get(&sym("q"))
        );
        assert_eq!(new_state[&sym("q")], [vec![v(2)]].into_iter().collect());
    }

    #[test]
    fn recursion_is_refused() {
        let program = IncrementalProgram {
            rules: vec![
                rule(
                    "path",
                    vec![x(), y()],
                    vec![lit("edge", vec![x(), y()], false)],
                ),
                rule(
                    "path",
                    vec![x(), y()],
                    vec![
                        lit("edge", vec![x(), Term::Var(sym("Z"))], false),
                        lit("path", vec![Term::Var(sym("Z")), y()], false),
                    ],
                ),
            ],
        };
        let state = state_of(vec![("edge", vec![vec![v(1), v(2)]])]);
        let patch = patch_of(vec![("edge", SignedFact::Plus(vec![v(2), v(3)]))]);
        let err = incremental_eval(&program, &state, &patch).unwrap_err();
        assert!(err.to_string().contains("recursive"));
    }

    // ── The production-vs-oracle differential (issue #61's non-
    // negotiable gate): every case laws.rs's own generative campaign
    // proves against full recompute, converted into this module's real
    // types and run through THIS module's algorithm, must agree with
    // the oracle's `incremental_eval` byte-for-byte. ───────────────────

    fn conv_term(t: &laws::Term) -> Term {
        match t {
            laws::Term::Const(c) => Term::Const(c.clone()),
            laws::Term::Var(name) => Term::Var(sym(name)),
        }
    }
    fn conv_literal(l: &laws::Literal) -> Literal {
        Literal {
            rel: sym(l.rel),
            args: l.args.iter().map(conv_term).collect(),
            negated: l.negated,
        }
    }
    fn conv_rule(r: &laws::Rule) -> Rule {
        Rule {
            head_rel: sym(r.head_rel),
            head_args: r.head_args.iter().map(conv_term).collect(),
            body: r.body.iter().map(conv_literal).collect(),
        }
    }
    fn conv_program(p: &laws::Program) -> IncrementalProgram {
        IncrementalProgram {
            rules: p.rules.iter().map(conv_rule).collect(),
        }
    }
    fn conv_facts(facts: &BTreeMap<laws::Rel, BTreeSet<Tuple>>) -> MaintainedState {
        facts.iter().map(|(k, v)| (sym(k), v.clone())).collect()
    }
    fn conv_signed(fact: &laws::SignedFact) -> SignedFact {
        match fact {
            laws::SignedFact::Plus(t) => SignedFact::Plus(t.clone()),
            laws::SignedFact::Minus(t) => SignedFact::Minus(t.clone()),
        }
    }
    fn conv_patch(
        patch: &BTreeMap<laws::Rel, BTreeSet<laws::SignedFact>>,
    ) -> BTreeMap<Symbol, BTreeSet<SignedFact>> {
        patch
            .iter()
            .map(|(k, facts)| (sym(k), facts.iter().map(conv_signed).collect()))
            .collect()
    }

    /// One case: build the oracle `Program`/EDB/patch, run
    /// `laws::incremental_eval`, convert everything to this module's
    /// types, run THIS module's `incremental_eval`, and assert the two
    /// deltas agree relation-by-relation (a relation absent from one
    /// side means the same as an empty delta on the other).
    fn assert_matches_oracle(
        oracle_program: &laws::Program,
        oracle_facts: &BTreeMap<laws::Rel, BTreeSet<Tuple>>,
        oracle_patch: &BTreeMap<laws::Rel, BTreeSet<laws::SignedFact>>,
        ctx: &str,
    ) {
        let full_oracle_program = laws::Program::untimed(
            oracle_program.rules.clone(),
            oracle_program.fixed.clone(),
            oracle_facts.clone(),
        );
        let oracle_out = laws::incremental_eval(&full_oracle_program, oracle_patch)
            .expect("oracle incremental_eval succeeds");
        // `MaintainedState` must start as the FULL old total (every IDB
        // relation's own prior derivation, not just the raw EDB facts) —
        // a standing query maintains that state itself; it has no way to
        // re-derive it from scratch each round. `naive_eval` on the
        // OLD (pre-patch) program is exactly that full old total.
        let old_total = laws::naive_eval(&full_oracle_program).expect("old program evaluates");

        let production_program = conv_program(oracle_program);
        let production_state = conv_facts(&old_total);
        let production_patch = conv_patch(oracle_patch);
        let (production_out, _new_state) =
            incremental_eval(&production_program, &production_state, &production_patch)
                .expect("production incremental_eval succeeds");

        let rel_names: BTreeSet<&str> = oracle_out
            .keys()
            .copied()
            .chain(oracle_facts.keys().copied())
            .collect();
        for rel in rel_names {
            let expected: BTreeSet<SignedFact> = oracle_out
                .get(rel)
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(conv_signed)
                .collect();
            let got = production_out.get(&sym(rel)).cloned().unwrap_or_default();
            assert_eq!(expected, got, "{ctx}: mismatch on relation '{rel}'");
        }
    }

    #[test]
    fn production_matches_oracle_generatively() {
        fn shape_a() -> Vec<laws::Rule> {
            vec![laws::Rule::plain(
                "q",
                vec![laws::Term::Var("X")],
                vec![
                    laws::Literal::pos("p", vec![laws::Term::Var("X"), laws::Term::Var("Y")]),
                    laws::Literal::neg("r", vec![laws::Term::Var("X")]),
                ],
            )]
        }
        fn shape_b() -> Vec<laws::Rule> {
            vec![
                laws::Rule::plain(
                    "mid",
                    vec![laws::Term::Var("X")],
                    vec![
                        laws::Literal::pos("p", vec![laws::Term::Var("X"), laws::Term::Var("Y")]),
                        laws::Literal::neg("r", vec![laws::Term::Var("X")]),
                    ],
                ),
                laws::Rule::plain(
                    "q",
                    vec![laws::Term::Var("X")],
                    vec![
                        laws::Literal::pos("mid", vec![laws::Term::Var("X")]),
                        laws::Literal::neg("s", vec![laws::Term::Var("X")]),
                    ],
                ),
            ]
        }
        fn shape_c() -> Vec<laws::Rule> {
            vec![laws::Rule::plain(
                "q",
                vec![laws::Term::Var("X"), laws::Term::Var("Y")],
                vec![
                    laws::Literal::pos("p", vec![laws::Term::Var("X"), laws::Term::Var("Y")]),
                    laws::Literal::pos("r2", vec![laws::Term::Var("X"), laws::Term::Var("Y")]),
                ],
            )]
        }
        let shapes: [fn() -> Vec<laws::Rule>; 3] = [shape_a, shape_b, shape_c];

        let mut state: u64 = 0xFEED_FACE_C0FF_EE01;
        let mut next_u64 = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut next_range = |n: u64| next_u64() % n;

        let mut cases = 0;
        for shape in shapes {
            for _ in 0..60 {
                let rules = shape();
                let mut facts: BTreeMap<laws::Rel, BTreeSet<Tuple>> = BTreeMap::new();
                for rel in ["p", "r", "r2", "s"] {
                    let n = next_range(6);
                    let mut set = BTreeSet::new();
                    for _ in 0..n {
                        let a = v(next_range(4) as i64);
                        if rel == "p" || rel == "r2" {
                            set.insert(vec![a, v(next_range(4) as i64)]);
                        } else {
                            set.insert(vec![a]);
                        }
                    }
                    facts.insert(rel, set);
                }

                let mut patch: BTreeMap<laws::Rel, BTreeSet<laws::SignedFact>> = BTreeMap::new();
                let all = ["p", "r", "r2", "s"];
                let k = 1 + next_range(2) as usize;
                let mut chosen = Vec::new();
                while chosen.len() < k {
                    let rel = all[next_range(4) as usize];
                    if !chosen.contains(&rel) {
                        chosen.push(rel);
                    }
                }
                for rel in chosen {
                    let existing: Vec<Tuple> = facts[rel].iter().cloned().collect();
                    if !existing.is_empty() && next_range(2) == 0 {
                        let victim = existing[next_range(existing.len() as u64) as usize].clone();
                        patch
                            .entry(rel)
                            .or_default()
                            .insert(laws::SignedFact::Minus(victim));
                    } else {
                        let a = v(next_range(4) as i64);
                        let t = if rel == "p" || rel == "r2" {
                            vec![a, v(next_range(4) as i64)]
                        } else {
                            vec![a]
                        };
                        patch
                            .entry(rel)
                            .or_default()
                            .insert(laws::SignedFact::Plus(t));
                    }
                }
                if patch.values().all(BTreeSet::is_empty) {
                    continue;
                }

                let oracle_program = laws::Program::untimed(rules, vec![], BTreeMap::new());
                assert_matches_oracle(&oracle_program, &facts, &patch, &format!("case {cases}"));
                cases += 1;
            }
        }
        assert!(
            cases > 100,
            "expected a rich production-vs-oracle campaign, ran {cases}"
        );
    }
}
