/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The reference semantics of stratified Datalog, as executable law.
//!
//! Everything here is deliberately naive: no indexes, no deltas, no rewrites
//! — just the textbook fixpoint, written to be *obviously* correct. The real
//! engine's optimized evaluation must produce byte-identical answer sets to
//! this oracle on every program the differential tests generate. The oracle
//! is judge, never production code (`cfg(test)` only).
//!
//! The abstract program form is minimal on purpose — relation symbols,
//! variables, `DataValue` constants, optional negation, optional head
//! aggregations, opaque fixed rules — so it can outlive any concrete AST
//! the engine uses. The aggregations themselves are the *real* landed
//! [`Aggregation`] values from `data/aggr.rs`: the oracle folds through
//! exactly the code users get, so a bug in an aggregation cannot hide
//! behind a parallel test-only reimplementation.
//!
//! ## Aggregation semantics, as law
//!
//! - A **normal aggregation** head is evaluated once, at the fixpoint of
//!   everything beneath it: group the rule set's derived rows by the
//!   non-aggregated head positions, fold each group through the normal
//!   form, one output row per group. Rows are counted per distinct binding
//!   of the body's variables (the bodies join sets, so that multiset is
//!   well-defined without any plan-dependent notion of duplicates).
//! - A **meet aggregation** head whose rules are *all* meet forms may be
//!   self-recursive: each derived row meets into an accumulator keyed by
//!   the non-aggregated positions, *during* the fixpoint, and the
//!   accumulated rows are what the recursive body reads back. Naive
//!   iteration simply re-derives everything until no accumulated value
//!   changes.
//! - An aggregation head with **every position aggregated** always has a
//!   row. For normal forms, no input rows yield the single empty-fold
//!   row. For meet forms, if the first round — where the recursive reads
//!   see the empty store — derives nothing, the identity row
//!   (`init_val`s) is inserted as a real fact the rest of the recursion
//!   builds on; if anything was derived, the identity row never exists
//!   (exposing it alongside real rows would let its value join into rule
//!   bodies and derive facts outside the least fixpoint). With a grouping
//!   position, no rows yield no rows.
//! - A **fixed rule** is an opaque function from complete input relations
//!   to an output relation; it always sits on a stratum boundary (inputs
//!   strictly below, readers strictly above), never inside recursion.
//!
//! Three deliberate divergences from upstream cozo, all in the oracle's
//! favor: upstream `compile.rs::aggr_kind` silently demoted a meet
//! signature whose aggregated positions were not a suffix to a *normal*
//! aggregation — which its evaluator then froze after epoch 0, silently
//! dropping recursive derivations — while the oracle groups by position
//! and evaluates meets inside recursion wherever they appear; the
//! order-dependent aggregations (`choice`, `collect`, `min_cost`/
//! `shortest`/`latest_by`/`smallest_by` ties, `choice_rand`) are
//! deterministic here (sorted-set derivation order) but their tie-breaks
//! are arrival-order artifacts, so differential harnesses must avoid or
//! canonicalize them; and the abstract [`Program`] has no entry symbol,
//! so the oracle judges the *whole* program — upstream prunes rules
//! unreachable from the entry before both checking and evaluation (dead
//! rules are neither refused nor computed), while the oracle checks and
//! evaluates everything, so differential harnesses must feed
//! entry-reachable programs.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::data::aggr::{Aggregation, MeetAggrObj, NormalAggrObj};
use crate::data::tuple::Tuple;
use crate::data::value::DataValue;

pub(crate) type Rel = &'static str;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Term {
    Var(&'static str),
    Const(DataValue),
}

#[derive(Clone, Debug)]
pub(crate) struct Literal {
    pub rel: Rel,
    pub args: Vec<Term>,
    pub negated: bool,
}

/// One head position's aggregation, if any: the real landed [`Aggregation`]
/// plus its compile-time arguments (only `collect` takes one today).
pub(crate) type HeadAggr = Option<(Aggregation, Vec<DataValue>)>;

#[derive(Clone, Debug)]
pub(crate) struct Rule {
    pub head_rel: Rel,
    pub head_args: Vec<Term>,
    /// Per-head-position aggregations, same length as `head_args`.
    pub aggr: Vec<HeadAggr>,
    pub body: Vec<Literal>,
}

impl Rule {
    /// A rule with no aggregations.
    pub(crate) fn plain(head_rel: Rel, head_args: Vec<Term>, body: Vec<Literal>) -> Self {
        let aggr = vec![None; head_args.len()];
        Self {
            head_rel,
            head_args,
            aggr,
            body,
        }
    }

    /// A rule with per-position head aggregations.
    pub(crate) fn aggregated(
        head_rel: Rel,
        head_args: Vec<Term>,
        aggr: Vec<HeadAggr>,
        body: Vec<Literal>,
    ) -> Self {
        Self {
            head_rel,
            head_args,
            aggr,
            body,
        }
    }
}

/// A fixed rule, modeled abstractly: an opaque function from its complete
/// input relations to an output relation. Stratification always puts it on
/// a stratum boundary — inputs strictly below, readers strictly above — so
/// it can never sit inside recursion; evaluation runs it exactly once.
#[derive(Clone, Debug)]
pub(crate) struct FixedRule {
    pub head_rel: Rel,
    pub inputs: Vec<Rel>,
    /// Receives the input relations in `inputs` order.
    pub eval: fn(&[BTreeSet<Tuple>]) -> BTreeSet<Tuple>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Program {
    pub rules: Vec<Rule>,
    pub fixed: Vec<FixedRule>,
    pub facts: BTreeMap<Rel, BTreeSet<Tuple>>,
}

/// Why a program is refused, or an evaluation failed. The real compiler
/// must refuse the same programs, for the same reasons; evaluation errors
/// are values (law 5), never panics.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Rejection {
    /// A head variable is not bound by any positive body literal, or a
    /// negated literal uses a variable no positive literal binds.
    Unsafe(&'static str),
    /// A stratum-forcing dependency (negation, non-meet aggregation, a
    /// read of a meet-aggregated or fixed relation) occurs inside a
    /// recursive cycle.
    Unstratifiable(&'static str),
    /// The program shape is ill-formed: an aggregation vector whose length
    /// differs from the head's, rules of one head disagreeing on their
    /// aggregation signature (upstream refuses this at parse as
    /// `parser::head_aggr_mismatch`), a fixed head that is also a rule
    /// head, duplicated, or seeded with facts, facts under an aggregated
    /// head, or a relation used at two different arities.
    Malformed(&'static str),
    /// An aggregation failed at evaluation time (e.g. a type error inside
    /// a fold); carried as a value, never a panic.
    AggrError(String),
}

fn literal_vars(l: &Literal) -> HashSet<&'static str> {
    l.args
        .iter()
        .filter_map(|t| match t {
            Term::Var(v) => Some(*v),
            Term::Const(_) => None,
        })
        .collect()
}

/// Law 4 (rule safety), reference form.
pub(crate) fn check_safety(program: &Program) -> Result<(), Rejection> {
    for rule in &program.rules {
        let positive_vars: HashSet<&str> = rule
            .body
            .iter()
            .filter(|l| !l.negated)
            .flat_map(literal_vars)
            .collect();
        for t in &rule.head_args {
            if let Term::Var(v) = t
                && !positive_vars.contains(v)
            {
                return Err(Rejection::Unsafe(rule.head_rel));
            }
        }
        for l in rule.body.iter().filter(|l| l.negated) {
            if !literal_vars(l).is_subset(&positive_vars) {
                return Err(Rejection::Unsafe(rule.head_rel));
            }
        }
    }
    Ok(())
}

/// How a head relation aggregates, across *all* of its rules — the
/// classification upstream `stratify.rs` derives per rule set.
#[derive(Clone, Copy)]
struct HeadClass {
    /// Some rule of this head aggregates some position.
    has_aggr: bool,
    /// It aggregates, and every aggregated position of every rule is a
    /// meet form — the only class allowed to recurse through itself.
    is_meet: bool,
}

fn head_classes(program: &Program) -> HashMap<Rel, HeadClass> {
    let mut per_head: HashMap<Rel, Vec<&Rule>> = HashMap::new();
    for rule in &program.rules {
        per_head.entry(rule.head_rel).or_default().push(rule);
    }
    per_head
        .into_iter()
        .map(|(rel, rules)| {
            let has_aggr = rules.iter().any(|r| r.aggr.iter().any(|a| a.is_some()));
            let is_meet = has_aggr
                && rules.iter().all(|r| {
                    r.aggr.iter().all(|a| match a {
                        None => true,
                        Some((aggr, _)) => aggr.is_meet(),
                    })
                });
            (rel, HeadClass { has_aggr, is_meet })
        })
        .collect()
}

/// The dependency graph, one edge per body literal or fixed-rule input:
/// head → dependency, with `forcing` true when the dependency must be
/// complete strictly below the head. Mirrors the "poisoned" edges of
/// upstream `stratify.rs` (`convert_normal_form_program_to_graph`):
///
/// - an aggregating head's only non-forcing dependency is a meet head
///   reading *itself*, positively — every other dependency of an
///   aggregating head forces a stratum;
/// - a non-aggregating rule forces a stratum on negated dependencies and
///   on any read of a meet-aggregated or fixed relation;
/// - a fixed rule forces a stratum on every input.
fn dependency_edges(program: &Program) -> Vec<(Rel, Rel, bool)> {
    let classes = head_classes(program);
    let fixed_heads: HashSet<Rel> = program.fixed.iter().map(|f| f.head_rel).collect();
    let is_meet = |rel: Rel| classes.get(rel).is_some_and(|c| c.is_meet);
    let mut edges = Vec::new();
    for rule in &program.rules {
        let head = rule.head_rel;
        let class = classes[&head];
        for lit in &rule.body {
            let dep = lit.rel;
            let forcing = if class.has_aggr {
                if class.is_meet && dep == head {
                    // The one legal aggregation inside recursion: a meet
                    // head folding its own positive derivations.
                    lit.negated
                } else {
                    true
                }
            } else {
                lit.negated || fixed_heads.contains(dep) || is_meet(dep)
            };
            edges.push((head, dep, forcing));
        }
    }
    for f in &program.fixed {
        for dep in &f.inputs {
            edges.push((f.head_rel, *dep, true));
        }
    }
    edges
}

/// Law 2 (stratification), reference form: a program is unstratifiable iff
/// some dependency cycle contains a stratum-forcing edge. With aggregation
/// this is exactly upstream `stratify.rs`'s rule: self-recursion is legal
/// only when all rules of the head aggregate with meet forms; normal
/// aggregation over any dependency, negation in a cycle, and fixed rules
/// in a cycle are refused.
pub(crate) fn check_stratifiable(program: &Program) -> Result<(), Rejection> {
    let edges = dependency_edges(program);
    let mut adjacency: HashMap<Rel, HashSet<Rel>> = HashMap::new();
    for (head, dep, _) in &edges {
        adjacency.entry(*head).or_default().insert(*dep);
    }
    let reaches = |from: Rel, to: Rel| -> bool {
        let mut seen = HashSet::new();
        let mut stack = vec![from];
        while let Some(r) = stack.pop() {
            if r == to {
                return true;
            }
            if seen.insert(r) {
                stack.extend(adjacency.get(r).into_iter().flatten().copied());
            }
        }
        false
    };
    for (head, dep, forcing) in &edges {
        if *forcing && reaches(dep, head) {
            return Err(Rejection::Unstratifiable(head));
        }
    }
    Ok(())
}

fn aggr_err(e: miette::Report) -> Rejection {
    Rejection::AggrError(e.to_string())
}

/// Program-shape validation the real compiler performs at parse/compile
/// time; see [`Rejection::Malformed`] for the refused shapes.
pub(crate) fn check_wellformed(program: &Program) -> Result<(), Rejection> {
    let mut signatures: BTreeMap<Rel, &[HeadAggr]> = BTreeMap::new();
    for rule in &program.rules {
        if rule.aggr.len() != rule.head_args.len() {
            return Err(Rejection::Malformed(rule.head_rel));
        }
        match signatures.entry(rule.head_rel) {
            Entry::Occupied(prev) if *prev.get() != rule.aggr.as_slice() => {
                return Err(Rejection::Malformed(rule.head_rel));
            }
            Entry::Occupied(_) => {}
            Entry::Vacant(e) => {
                e.insert(&rule.aggr);
            }
        }
    }
    let mut fixed_heads = HashSet::new();
    for f in &program.fixed {
        if !fixed_heads.insert(f.head_rel) || program.facts.contains_key(f.head_rel) {
            return Err(Rejection::Malformed(f.head_rel));
        }
    }
    for rule in &program.rules {
        if fixed_heads.contains(rule.head_rel) {
            return Err(Rejection::Malformed(rule.head_rel));
        }
    }
    for (rel, class) in head_classes(program) {
        if class.has_aggr && program.facts.contains_key(rel) {
            return Err(Rejection::Malformed(rel));
        }
    }
    // One arity per relation, across facts, rule heads, and body literals
    // (the real compiler refuses arity clashes at compile time). A fixed
    // head's *output* arity is opaque to the model — its `eval` may emit
    // tuples of any length — but its readers must at least agree among
    // themselves, and they are its only arity sources here (fixed heads
    // can be neither rule heads nor fact relations, checked above).
    let mut arities: HashMap<Rel, usize> = HashMap::new();
    let mut check_arity = |rel: Rel, arity: usize| -> Result<(), Rejection> {
        match arities.get(rel) {
            Some(known) if *known != arity => Err(Rejection::Malformed(rel)),
            Some(_) => Ok(()),
            None => {
                arities.insert(rel, arity);
                Ok(())
            }
        }
    };
    for (rel, tuples) in &program.facts {
        for t in tuples {
            check_arity(rel, t.len())?;
        }
    }
    for rule in &program.rules {
        check_arity(rule.head_rel, rule.head_args.len())?;
        for l in &rule.body {
            check_arity(l.rel, l.args.len())?;
        }
    }
    Ok(())
}

/// Assign strata: a relation sits strictly above every stratum-forcing
/// dependency, and at least as high as its other dependencies. Assumes
/// `check_stratifiable` passed.
fn strata(program: &Program) -> HashMap<Rel, usize> {
    let edges = dependency_edges(program);
    let mut s: HashMap<Rel, usize> = HashMap::new();
    let rels: HashSet<Rel> = program
        .rules
        .iter()
        .flat_map(|r| std::iter::once(r.head_rel).chain(r.body.iter().map(|l| l.rel)))
        .chain(program.facts.keys().copied())
        .chain(
            program
                .fixed
                .iter()
                .flat_map(|f| std::iter::once(f.head_rel).chain(f.inputs.iter().copied())),
        )
        .collect();
    for r in &rels {
        s.insert(r, 0);
    }
    // Bellman-Ford over ≤ |rels| levels: any simple dependency path has
    // fewer than |rels| edges, so |rels| passes settle every level and one
    // more observes no change.
    let bound = rels.len() + 1;
    for _ in 0..bound {
        let mut changed = false;
        for (head, dep, forcing) in &edges {
            let need = s[dep] + usize::from(*forcing);
            if s[head] < need {
                s.insert(*head, need);
                changed = true;
            }
        }
        if !changed {
            return s;
        }
    }
    unreachable!("stratum assignment must converge on stratifiable programs");
}

type Bindings = HashMap<&'static str, DataValue>;

fn unify(args: &[Term], tuple: &Tuple, bound: &Bindings) -> Option<Bindings> {
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
                    out.insert(name, v.clone());
                }
            },
        }
    }
    Some(out)
}

fn ground(args: &[Term], bound: &Bindings) -> Tuple {
    args.iter()
        .map(|t| match t {
            Term::Const(c) => c.clone(),
            Term::Var(v) => bound[v].clone(),
        })
        .collect()
}

/// All satisfying bindings of a rule body against the current database,
/// one per distinct binding of the body's variables. Positives first, so
/// safety guarantees negated literals are fully bound when probed.
fn body_bindings(rule: &Rule, db: &BTreeMap<Rel, BTreeSet<Tuple>>) -> Vec<Bindings> {
    let empty = BTreeSet::new();
    let mut ordered: Vec<&Literal> = rule.body.iter().filter(|l| !l.negated).collect();
    ordered.extend(rule.body.iter().filter(|l| l.negated));

    let mut frontier: Vec<Bindings> = vec![Bindings::new()];
    for lit in ordered {
        let mut next = Vec::new();
        for bound in &frontier {
            if lit.negated {
                let probe = ground(&lit.args, bound);
                if !db.get(lit.rel).unwrap_or(&empty).contains(&probe) {
                    next.push(bound.clone());
                }
            } else {
                for tuple in db.get(lit.rel).unwrap_or(&empty) {
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

/// The rule's derived head rows, one per body binding. Distinct bindings
/// can ground to the same row; the multiplicity is what normal
/// aggregations fold over, so it is preserved.
fn derived_rows(rule: &Rule, db: &BTreeMap<Rel, BTreeSet<Tuple>>) -> Vec<Tuple> {
    body_bindings(rule, db)
        .iter()
        .map(|b| ground(&rule.head_args, b))
        .collect()
}

/// Evaluate one normal-aggregation head, once, over the fixpoint of
/// everything beneath it (stratification guarantees all its dependencies
/// are complete): group every rule's derived rows by the non-aggregated
/// head positions, fold each group through the normal forms — matching
/// upstream `eval.rs::initial_rule_aggr_eval`, shared groups across the
/// head's rules included. No rows with every position aggregated yields
/// the single empty-fold row.
fn eval_normal_aggr_head(
    rules: &[&Rule],
    db: &BTreeMap<Rel, BTreeSet<Tuple>>,
) -> Result<BTreeSet<Tuple>, Rejection> {
    // Well-formedness guarantees every rule of the head shares this
    // signature.
    let signature = &rules[0].aggr;
    let key_positions: Vec<usize> = signature
        .iter()
        .enumerate()
        .filter(|(_, a)| a.is_none())
        .map(|(i, _)| i)
        .collect();
    let val_positions: Vec<(usize, &Aggregation, &[DataValue])> = signature
        .iter()
        .enumerate()
        .filter_map(|(i, a)| a.as_ref().map(|(aggr, args)| (i, aggr, args.as_slice())))
        .collect();
    let fresh_ops = || -> Result<Vec<Box<dyn NormalAggrObj>>, Rejection> {
        val_positions
            .iter()
            .map(|(_, aggr, args)| aggr.normal_op(args).map_err(aggr_err))
            .collect()
    };

    let mut groups: BTreeMap<Tuple, Vec<Box<dyn NormalAggrObj>>> = BTreeMap::new();
    for rule in rules {
        for row in derived_rows(rule, db) {
            let key: Tuple = key_positions.iter().map(|i| row[*i].clone()).collect();
            let ops = match groups.entry(key) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => e.insert(fresh_ops()?),
            };
            for (op, (i, _, _)) in ops.iter_mut().zip(&val_positions) {
                op.set(&row[*i]).map_err(aggr_err)?;
            }
        }
    }

    let mut out = BTreeSet::new();
    if groups.is_empty() && key_positions.is_empty() && !val_positions.is_empty() {
        let mut row = Vec::with_capacity(val_positions.len());
        for op in fresh_ops()? {
            row.push(op.get().map_err(aggr_err)?);
        }
        out.insert(row);
    }
    for (key, ops) in groups {
        let mut row = vec![DataValue::Null; signature.len()];
        for (slot, i) in key_positions.iter().enumerate() {
            row[*i] = key[slot].clone();
        }
        for (op, (i, _, _)) in ops.iter().zip(&val_positions) {
            row[*i] = op.get().map_err(aggr_err)?;
        }
        out.insert(row);
    }
    Ok(out)
}

/// The running state of one meet-aggregated head during its stratum's
/// fixpoint: an accumulator keyed by the non-aggregated head positions,
/// updated in place through the real landed meet ops.
struct MeetState {
    key_positions: Vec<usize>,
    val_positions: Vec<usize>,
    ops: Vec<Box<dyn MeetAggrObj>>,
    arity: usize,
    acc: BTreeMap<Tuple, Tuple>,
}

impl MeetState {
    fn new(signature: &[HeadAggr]) -> Result<Self, Rejection> {
        let key_positions = signature
            .iter()
            .enumerate()
            .filter(|(_, a)| a.is_none())
            .map(|(i, _)| i)
            .collect();
        let mut val_positions = Vec::new();
        let mut ops = Vec::new();
        for (i, a) in signature.iter().enumerate() {
            if let Some((aggr, _)) = a {
                // Total by classification (`is_meet` heads only), never a
                // panic: a non-meet form here is a malformed program.
                let op = aggr
                    .meet_op()
                    .ok_or(Rejection::Malformed("non-meet aggregation on a meet head"))?;
                val_positions.push(i);
                ops.push(op);
            }
        }
        Ok(Self {
            key_positions,
            val_positions,
            ops,
            arity: signature.len(),
            acc: BTreeMap::new(),
        })
    }

    /// Meet one derived row into the accumulator; true iff any accumulated
    /// value changed (a fresh key always counts).
    fn meet_row(&mut self, row: &Tuple) -> Result<bool, Rejection> {
        let key: Tuple = self.key_positions.iter().map(|i| row[*i].clone()).collect();
        let vals: Tuple = self.val_positions.iter().map(|i| row[*i].clone()).collect();
        match self.acc.entry(key) {
            Entry::Vacant(e) => {
                e.insert(vals);
                Ok(true)
            }
            Entry::Occupied(mut e) => {
                let stored = e.get_mut();
                let mut changed = false;
                for (slot, op) in self.ops.iter().enumerate() {
                    changed |= op
                        .update(&mut stored[slot], &vals[slot])
                        .map_err(aggr_err)?;
                }
                Ok(changed)
            }
        }
    }

    /// The accumulated rows, re-interleaved into head-position order —
    /// this is the relation the recursive body (and everything above)
    /// reads.
    fn materialize(&self) -> BTreeSet<Tuple> {
        self.acc
            .iter()
            .map(|(key, vals)| {
                let mut row = vec![DataValue::Null; self.arity];
                for (slot, i) in self.key_positions.iter().enumerate() {
                    row[*i] = key[slot].clone();
                }
                for (slot, i) in self.val_positions.iter().enumerate() {
                    row[*i] = vals[slot].clone();
                }
                row
            })
            .collect()
    }
}

/// Naive stratified fixpoint evaluation: the textbook algorithm extended
/// with the aggregation and fixed-rule semantics in the module docs — the
/// oracle for Laws 1 and 3. Validates shape, safety, and stratifiability
/// first.
pub(crate) fn naive_eval(program: &Program) -> Result<BTreeMap<Rel, BTreeSet<Tuple>>, Rejection> {
    check_wellformed(program)?;
    check_safety(program)?;
    check_stratifiable(program)?;
    let classes = head_classes(program);
    let strata_of = strata(program);
    let max_stratum = strata_of.values().copied().max().unwrap_or(0);

    let mut db = program.facts.clone();

    for stratum in 0..=max_stratum {
        // Fixed rules run first and exactly once: stratification forces
        // their inputs strictly below (complete) and their readers
        // strictly above.
        for f in program
            .fixed
            .iter()
            .filter(|f| strata_of[f.head_rel] == stratum)
        {
            let inputs: Vec<BTreeSet<Tuple>> = f
                .inputs
                .iter()
                .map(|r| db.get(r).cloned().unwrap_or_default())
                .collect();
            db.insert(f.head_rel, (f.eval)(&inputs));
        }

        // Normal-aggregation heads run once, next: stratification forces
        // every dependency strictly below, so the rows they fold are
        // already the fixpoint of the strata beneath.
        let normal_heads: BTreeSet<Rel> = program
            .rules
            .iter()
            .filter(|r| strata_of[r.head_rel] == stratum)
            .map(|r| r.head_rel)
            .filter(|rel| {
                let c = classes[rel];
                c.has_aggr && !c.is_meet
            })
            .collect();
        for head in &normal_heads {
            let head_rules: Vec<&Rule> = program
                .rules
                .iter()
                .filter(|r| r.head_rel == *head)
                .collect();
            let out = eval_normal_aggr_head(&head_rules, &db)?;
            db.insert(head, out);
        }

        // Meet-aggregation heads of this stratum accumulate during the
        // fixpoint below; plain heads insert as ever.
        let mut meets: BTreeMap<Rel, MeetState> = BTreeMap::new();
        for rule in program
            .rules
            .iter()
            .filter(|r| strata_of[r.head_rel] == stratum && classes[r.head_rel].is_meet)
        {
            if !meets.contains_key(rule.head_rel) {
                meets.insert(rule.head_rel, MeetState::new(&rule.aggr)?);
            }
        }
        // Law 3's embodiment: over finite data with no invented values the
        // fixpoint is reached in finitely many rounds; the generous bound
        // turns non-termination into a loud test failure.
        let mut rounds = 0usize;
        loop {
            rounds += 1;
            assert!(
                rounds <= 100_000,
                "fixpoint bound exceeded: non-termination"
            );
            let mut changed = false;
            for rule in program
                .rules
                .iter()
                .filter(|r| strata_of[r.head_rel] == stratum && !normal_heads.contains(r.head_rel))
            {
                let rows = derived_rows(rule, &db);
                if let Some(state) = meets.get_mut(rule.head_rel) {
                    for row in &rows {
                        changed |= state.meet_row(row)?;
                    }
                } else {
                    for row in rows {
                        changed |= db.entry(rule.head_rel).or_default().insert(row);
                    }
                }
            }
            // Upstream's epoch-0 identity rule, transcribed
            // (`eval.rs::initial_rule_meet_eval`): an all-aggregated meet
            // head whose first round — where the recursive reads saw the
            // empty store, exactly epoch 0 — derived nothing gets the
            // identity row, a real fact the rest of the recursion builds
            // on. Once any row exists the identity is never inserted:
            // exposing it alongside real derivations would let its value
            // (e.g. `min`'s Null) join into rule bodies and derive facts
            // outside the least fixpoint.
            if rounds == 1 {
                for state in meets.values_mut() {
                    if state.acc.is_empty()
                        && state.key_positions.is_empty()
                        && !state.ops.is_empty()
                    {
                        let identity: Tuple = state.ops.iter().map(|op| op.init_val()).collect();
                        state.acc.insert(Vec::new(), identity);
                        changed = true;
                    }
                }
            }
            // Republish the accumulated meet relations so the next round's
            // derivations (the recursive reads) see this round's meets.
            for (head, state) in &meets {
                db.insert(head, state.materialize());
            }
            if !changed {
                break;
            }
        }
    }
    Ok(db)
}

/// The corpus of programs the compiler must refuse — shared between the
/// reference checker's self-tests and (as they land) the real compiler's.
pub(crate) fn unstratifiable_corpus() -> Vec<(&'static str, Program)> {
    fn lit(rel: Rel, args: Vec<Term>, negated: bool) -> Literal {
        Literal { rel, args, negated }
    }
    fn named(name: &'static str) -> (Aggregation, Vec<DataValue>) {
        let aggr = crate::data::aggr::parse_aggr(name)
            .unwrap_or_else(|| panic!("corpus uses only real aggregations, missing: {name}"));
        (aggr, vec![])
    }
    let x = || Term::Var("X");
    let y = || Term::Var("Y");
    vec![
        (
            "direct self-negation: p(X) :- d(X), not p(X)",
            Program {
                rules: vec![Rule::plain(
                    "p",
                    vec![x()],
                    vec![lit("d", vec![x()], false), lit("p", vec![x()], true)],
                )],
                ..Program::default()
            },
        ),
        (
            "mutual negation: p :- d, not q; q :- d, not p",
            Program {
                rules: vec![
                    Rule::plain(
                        "p",
                        vec![x()],
                        vec![lit("d", vec![x()], false), lit("q", vec![x()], true)],
                    ),
                    Rule::plain(
                        "q",
                        vec![x()],
                        vec![lit("d", vec![x()], false), lit("p", vec![x()], true)],
                    ),
                ],
                ..Program::default()
            },
        ),
        (
            "win-move game: win(X) :- move(X,Y), not win(Y)",
            Program {
                rules: vec![Rule::plain(
                    "win",
                    vec![x()],
                    vec![
                        lit("move", vec![x(), y()], false),
                        lit("win", vec![y()], true),
                    ],
                )],
                ..Program::default()
            },
        ),
        (
            "negation through a positive cycle: a :- d, not b; b :- a",
            Program {
                rules: vec![
                    Rule::plain(
                        "a",
                        vec![x()],
                        vec![lit("d", vec![x()], false), lit("b", vec![x()], true)],
                    ),
                    Rule::plain("b", vec![x()], vec![lit("a", vec![x()], false)]),
                ],
                ..Program::default()
            },
        ),
        (
            "recursive normal aggregation: p(X, count(Y)) :- d(X,Y); p(X, count(Y)) :- p(X,Y)",
            Program {
                rules: vec![
                    Rule::aggregated(
                        "p",
                        vec![x(), y()],
                        vec![None, Some(named("count"))],
                        vec![lit("d", vec![x(), y()], false)],
                    ),
                    Rule::aggregated(
                        "p",
                        vec![x(), y()],
                        vec![None, Some(named("count"))],
                        vec![lit("p", vec![x(), y()], false)],
                    ),
                ],
                ..Program::default()
            },
        ),
        (
            "mixed meet+normal aggregation on a recursive head: \
             q(X, min(Y), count(Z)) :- q(X,Y,Z)",
            Program {
                rules: vec![Rule::aggregated(
                    "q",
                    vec![x(), y(), Term::Var("Z")],
                    vec![None, Some(named("min")), Some(named("count"))],
                    vec![lit("q", vec![x(), y(), Term::Var("Z")], false)],
                )],
                ..Program::default()
            },
        ),
        (
            "meet aggregation negating its own head: m(X, min(Y)) :- d(X,Y), not m(X,Y)",
            Program {
                rules: vec![Rule::aggregated(
                    "m",
                    vec![x(), y()],
                    vec![None, Some(named("min"))],
                    vec![
                        lit("d", vec![x(), y()], false),
                        lit("m", vec![x(), y()], true),
                    ],
                )],
                ..Program::default()
            },
        ),
        (
            "fixed rule inside recursion: r(X) :- f(X), with fixed f over input r",
            Program {
                rules: vec![Rule::plain(
                    "r",
                    vec![x()],
                    vec![lit("f", vec![x()], false)],
                )],
                fixed: vec![FixedRule {
                    head_rel: "f",
                    inputs: vec!["r"],
                    eval: |_| BTreeSet::new(),
                }],
                ..Program::default()
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::data::aggr::parse_aggr;

    fn v(i: i64) -> DataValue {
        DataValue::from(i)
    }
    fn edge_facts(edges: &[(i64, i64)]) -> BTreeMap<Rel, BTreeSet<Tuple>> {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = Default::default();
        facts.insert(
            "edge",
            edges.iter().map(|(a, b)| vec![v(*a), v(*b)]).collect(),
        );
        facts
    }
    fn lit(rel: Rel, args: Vec<Term>, negated: bool) -> Literal {
        Literal { rel, args, negated }
    }
    fn x() -> Term {
        Term::Var("X")
    }
    fn y() -> Term {
        Term::Var("Y")
    }
    fn z() -> Term {
        Term::Var("Z")
    }
    /// A real landed aggregation by name, with no arguments.
    fn named(name: &str) -> HeadAggr {
        Some((
            parse_aggr(name).unwrap_or_else(|| panic!("real aggregation exists: {name}")),
            vec![],
        ))
    }

    /// path(X,Y) :- edge(X,Y); path(X,Y) :- edge(X,Z), path(Z,Y).
    fn transitive_closure() -> Vec<Rule> {
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
                    lit("edge", vec![x(), z()], false),
                    lit("path", vec![z(), y()], false),
                ],
            ),
        ]
    }

    /// The meet-reachability shape shared by the recursion tests and the
    /// property/differential harnesses:
    ///   m(X, aggr(V)) :- seed(X, V).
    ///   m(Y, aggr(V)) :- edge(X, Y), m(X, V).
    fn meet_reach_rules(aggr_name: &str) -> Vec<Rule> {
        vec![
            Rule::aggregated(
                "m",
                vec![x(), y()],
                vec![None, named(aggr_name)],
                vec![lit("seed", vec![x(), y()], false)],
            ),
            Rule::aggregated(
                "m",
                vec![y(), z()],
                vec![None, named(aggr_name)],
                vec![
                    lit("edge", vec![x(), y()], false),
                    lit("m", vec![x(), z()], false),
                ],
            ),
        ]
    }

    #[test]
    fn law1_transitive_closure_exact() {
        let program = Program {
            rules: transitive_closure(),
            facts: edge_facts(&[(1, 2), (2, 3), (3, 4)]),
            ..Program::default()
        };
        let db = naive_eval(&program).unwrap();
        let want: BTreeSet<Tuple> = [(1, 2), (2, 3), (3, 4), (1, 3), (2, 4), (1, 4)]
            .into_iter()
            .map(|(a, b)| vec![v(a), v(b)])
            .collect();
        assert_eq!(db["path"], want);
    }

    #[test]
    fn law3_recursion_terminates_on_cyclic_data() {
        let program = Program {
            rules: transitive_closure(),
            facts: edge_facts(&[(1, 2), (2, 3), (3, 1)]),
            ..Program::default()
        };
        let db = naive_eval(&program).unwrap();
        // Full 3×3 closure on a cycle.
        assert_eq!(db["path"].len(), 9);
    }

    #[test]
    fn law2_stratified_negation_evaluates_correctly() {
        // unreachable(X,Y) :- node(X), node(Y), not path(X,Y).
        let mut facts = edge_facts(&[(1, 2), (2, 3)]);
        facts.insert("node", (1..=3).map(|i| vec![v(i)]).collect());
        let mut rules = transitive_closure();
        rules.push(Rule::plain(
            "unreachable",
            vec![x(), y()],
            vec![
                lit("node", vec![x()], false),
                lit("node", vec![y()], false),
                lit("path", vec![x(), y()], true),
            ],
        ));
        let db = naive_eval(&Program {
            rules,
            facts,
            ..Program::default()
        })
        .unwrap();
        let want: BTreeSet<Tuple> = [(1, 1), (2, 1), (2, 2), (3, 1), (3, 2), (3, 3)]
            .into_iter()
            .map(|(a, b)| vec![v(a), v(b)])
            .collect();
        assert_eq!(db["unreachable"], want);
    }

    #[test]
    fn law2_unstratifiable_corpus_is_refused() {
        for (name, program) in unstratifiable_corpus() {
            assert!(
                matches!(
                    check_stratifiable(&program),
                    Err(Rejection::Unstratifiable(_))
                ),
                "must refuse: {name}"
            );
            assert!(naive_eval(&program).is_err(), "eval must refuse: {name}");
        }
    }

    #[test]
    fn law4_unsafe_rules_are_refused() {
        // Head variable unbound by any positive literal.
        let unbound_head = Program {
            rules: vec![Rule::plain(
                "p",
                vec![x()],
                vec![lit("q", vec![y()], false)],
            )],
            ..Program::default()
        };
        assert_eq!(check_safety(&unbound_head), Err(Rejection::Unsafe("p")));

        // Negated literal over a variable no positive literal binds.
        let unbound_negation = Program {
            rules: vec![Rule::plain(
                "p",
                vec![x()],
                vec![lit("q", vec![x()], false), lit("r", vec![z()], true)],
            )],
            ..Program::default()
        };
        assert_eq!(check_safety(&unbound_negation), Err(Rejection::Unsafe("p")));
    }

    #[test]
    fn constants_and_repeated_variables_unify_exactly() {
        // same(X) :- edge(X, X).  eq3(X) :- edge(3, X).
        let mut facts = edge_facts(&[(1, 1), (1, 2), (3, 5)]);
        facts.get_mut("edge").unwrap().insert(vec![v(4), v(4)]);
        let program = Program {
            rules: vec![
                Rule::plain("same", vec![x()], vec![lit("edge", vec![x(), x()], false)]),
                Rule::plain(
                    "eq3",
                    vec![x()],
                    vec![lit("edge", vec![Term::Const(v(3)), x()], false)],
                ),
            ],
            facts,
            ..Program::default()
        };
        let db = naive_eval(&program).unwrap();
        assert_eq!(db["same"], [vec![v(1)], vec![v(4)]].into_iter().collect());
        assert_eq!(db["eq3"], [vec![v(5)]].into_iter().collect());
    }

    /// Normal aggregation: group by the non-aggregated head positions and
    /// fold each group — groups shared across every rule of the head, sums
    /// exact `Int`s (the landed semantics, not upstream's f64 fold).
    #[test]
    fn normal_aggregation_groups_and_folds() {
        // total(D, sum(A), count(A)) :- sale(D, A); ... :- bonus(D, A).
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "sale",
            [(1, 10), (1, 20), (2, 5)]
                .iter()
                .map(|(d, a)| vec![v(*d), v(*a)])
                .collect(),
        );
        facts.insert("bonus", [vec![v(1), v(40)]].into_iter().collect());
        let rule = |rel| {
            Rule::aggregated(
                "total",
                vec![x(), y(), y()],
                vec![None, named("sum"), named("count")],
                vec![lit(rel, vec![x(), y()], false)],
            )
        };
        let program = Program {
            rules: vec![rule("sale"), rule("bonus")],
            facts,
            ..Program::default()
        };
        let db = naive_eval(&program).unwrap();
        let want: BTreeSet<Tuple> = [(1, 70, 3), (2, 5, 1)]
            .into_iter()
            .map(|(d, s, c)| vec![v(d), v(s), v(c)])
            .collect();
        assert_eq!(db["total"], want);
    }

    /// Aggregation over no rows: every position aggregated yields the
    /// single empty-fold row; a grouping position yields no rows at all.
    #[test]
    fn normal_aggregation_over_no_rows() {
        let all_aggregated = Program {
            rules: vec![Rule::aggregated(
                "c",
                vec![x(), x()],
                vec![named("count"), named("sum")],
                vec![lit("nothing", vec![x()], false)],
            )],
            ..Program::default()
        };
        let db = naive_eval(&all_aggregated).unwrap();
        assert_eq!(db["c"], [vec![v(0), v(0)]].into_iter().collect());

        let keyed = Program {
            rules: vec![Rule::aggregated(
                "t",
                vec![x(), y()],
                vec![None, named("count")],
                vec![lit("nothing", vec![x(), y()], false)],
            )],
            ..Program::default()
        };
        let db = naive_eval(&keyed).unwrap();
        assert!(db.get("t").is_none_or(|s| s.is_empty()));
    }

    /// Normal aggregation runs at the fixpoint: it folds the *complete*
    /// transitive closure computed in the stratum beneath it.
    #[test]
    fn normal_aggregation_folds_the_fixpoint_of_recursion() {
        // reach_count(X, count(Y)) :- path(X, Y).
        let mut rules = transitive_closure();
        rules.push(Rule::aggregated(
            "reach_count",
            vec![x(), y()],
            vec![None, named("count")],
            vec![lit("path", vec![x(), y()], false)],
        ));
        let program = Program {
            rules,
            facts: edge_facts(&[(1, 2), (2, 3), (3, 4)]),
            ..Program::default()
        };
        let db = naive_eval(&program).unwrap();
        let want: BTreeSet<Tuple> = [(1, 3), (2, 2), (3, 1)]
            .into_iter()
            .map(|(n, c)| vec![v(n), v(c)])
            .collect();
        assert_eq!(db["reach_count"], want);
    }

    /// The corpus counterpart: a self-recursive all-meet head is accepted
    /// and evaluated *inside* the fixpoint — here `min` labels flowing
    /// through a graph with a cycle, so termination is the meet's doing.
    #[test]
    fn meet_aggregation_evaluates_inside_recursion() {
        let mut facts = edge_facts(&[(1, 2), (2, 3), (3, 1), (3, 4)]);
        facts.insert(
            "seed",
            [(1, 5), (4, 1)]
                .iter()
                .map(|(k, l)| vec![v(*k), v(*l)])
                .collect(),
        );
        let program = Program {
            rules: meet_reach_rules("min"),
            facts,
            ..Program::default()
        };
        assert_eq!(check_stratifiable(&program), Ok(()));
        let db = naive_eval(&program).unwrap();
        let want: BTreeSet<Tuple> = [(1, 5), (2, 5), (3, 5), (4, 1)]
            .into_iter()
            .map(|(n, l)| vec![v(n), v(l)])
            .collect();
        assert_eq!(db["m"], want);
    }

    /// A meet head with every position aggregated and no derivations
    /// yields the single identity row of its meets.
    #[test]
    fn meet_aggregation_over_no_rows_yields_the_identity_row() {
        let program = Program {
            rules: vec![Rule::aggregated(
                "g",
                vec![x(), y()],
                vec![named("min"), named("or")],
                vec![lit("nothing", vec![x(), y()], false)],
            )],
            ..Program::default()
        };
        let db = naive_eval(&program).unwrap();
        assert_eq!(
            db["g"],
            [vec![DataValue::Null, DataValue::from(false)]]
                .into_iter()
                .collect()
        );
    }

    /// Review finding 1 (fix wave): the identity row of an all-aggregated
    /// meet head is a *real fact during recursion* — upstream meets it
    /// into the store at epoch 0, and derivations build on it. Here
    /// `m(or(W)) :- seed(W); m(or(W)) :- edge(V, W), m(V)` with no seeds:
    /// the identity `false` matches `edge(false, true)` and derives
    /// `true`; an oracle that only appended the identity after the
    /// fixpoint would answer `false`.
    #[test]
    fn meet_identity_row_feeds_recursion() {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "edge",
            [vec![DataValue::from(false), DataValue::from(true)]]
                .into_iter()
                .collect(),
        );
        let rules = vec![
            Rule::aggregated(
                "m",
                vec![x()],
                vec![named("or")],
                vec![lit("seed", vec![x()], false)],
            ),
            Rule::aggregated(
                "m",
                vec![y()],
                vec![named("or")],
                vec![
                    lit("edge", vec![x(), y()], false),
                    lit("m", vec![x()], false),
                ],
            ),
        ];
        let program = Program {
            rules,
            facts,
            ..Program::default()
        };
        let db = naive_eval(&program).unwrap();
        assert_eq!(db["m"], [vec![DataValue::from(true)]].into_iter().collect());
    }

    /// Review finding 1, second wave: the identity row must be *invisible*
    /// when derivations exist — upstream inserts it only when epoch 0
    /// derives nothing. `and`/`or` cannot tell (two-point lattices where
    /// the identity absorbs), but any larger lattice can: here `min`'s
    /// `Null` identity, if leaked into round-one recursion, would join
    /// `edge(Null, 1)` and derive a spurious 1, answering {1} instead of
    /// the least fixpoint {5}.
    #[test]
    fn meet_identity_row_is_invisible_when_derivations_exist() {
        // m(min(W)) :- seed(W);  m(min(W)) :- edge(V, W), m(V).
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert("seed", [vec![v(5)]].into_iter().collect());
        facts.insert("edge", [vec![DataValue::Null, v(1)]].into_iter().collect());
        let rules = vec![
            Rule::aggregated(
                "m",
                vec![x()],
                vec![named("min")],
                vec![lit("seed", vec![x()], false)],
            ),
            Rule::aggregated(
                "m",
                vec![y()],
                vec![named("min")],
                vec![
                    lit("edge", vec![x(), y()], false),
                    lit("m", vec![x()], false),
                ],
            ),
        ];
        let program = Program {
            rules,
            facts,
            ..Program::default()
        };
        let db = naive_eval(&program).unwrap();
        assert_eq!(db["m"], [vec![v(5)]].into_iter().collect());
    }

    /// Negation over a meet-aggregated relation forces a stratum, so the
    /// negating rule reads the *completed* accumulated relation.
    #[test]
    fn negation_reads_the_completed_meet_relation() {
        // unseeded(X) :- node(X), not m(X, true).
        let mut facts = edge_facts(&[(1, 2)]);
        facts.insert(
            "seed",
            [vec![v(1), DataValue::from(true)]].into_iter().collect(),
        );
        facts.insert("node", (1..=3).map(|i| vec![v(i)]).collect());
        let mut rules = meet_reach_rules("or");
        rules.push(Rule::plain(
            "unseeded",
            vec![x()],
            vec![
                lit("node", vec![x()], false),
                lit("m", vec![x(), Term::Const(DataValue::from(true))], true),
            ],
        ));
        let program = Program {
            rules,
            facts,
            ..Program::default()
        };
        let db = naive_eval(&program).unwrap();
        // m accumulates {(1,true),(2,true)}; node 3 has no m row at all.
        assert_eq!(db["unseeded"], [vec![v(3)]].into_iter().collect());
    }

    /// Fixed rules are opaque relation transformers on stratum boundaries:
    /// a constant one feeds recursion from below, a projecting one
    /// consumes the completed closure from above, and plain rules read its
    /// output one stratum higher still.
    #[test]
    fn fixed_rules_sit_on_stratum_boundaries() {
        let constant_edges = FixedRule {
            head_rel: "edge",
            inputs: vec![],
            eval: |_| {
                [(1, 2), (2, 3)]
                    .iter()
                    .map(|(a, b)| vec![v(*a), v(*b)])
                    .collect()
            },
        };
        let path_sources = FixedRule {
            head_rel: "sources",
            inputs: vec!["path"],
            eval: |inputs| inputs[0].iter().map(|t| vec![t[0].clone()]).collect(),
        };
        let mut rules = transitive_closure();
        rules.push(Rule::plain(
            "out",
            vec![x()],
            vec![lit("sources", vec![x()], false)],
        ));
        let program = Program {
            rules,
            fixed: vec![constant_edges, path_sources],
            ..Program::default()
        };
        let s = strata(&program);
        assert!(
            s["path"] > s["edge"],
            "readers sit strictly above a fixed rule"
        );
        assert!(
            s["sources"] > s["path"],
            "a fixed rule sits strictly above its inputs"
        );
        assert!(s["out"] > s["sources"]);
        let db = naive_eval(&program).unwrap();
        assert_eq!(db["path"].len(), 3);
        let want: BTreeSet<Tuple> = [vec![v(1)], vec![v(2)]].into_iter().collect();
        assert_eq!(db["sources"], want);
        assert_eq!(db["out"], want);
    }

    /// Law 5 at the oracle: aggregation type errors surface as values,
    /// through both the meet path and the normal path.
    #[test]
    fn aggregation_type_errors_are_values_not_panics() {
        // min meeting a Bool into a Bool.
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "seed",
            [
                vec![v(1), DataValue::from(false)],
                vec![v(1), DataValue::from(true)],
            ]
            .into_iter()
            .collect(),
        );
        facts.insert("edge", BTreeSet::new());
        let program = Program {
            rules: meet_reach_rules("min"),
            facts,
            ..Program::default()
        };
        assert!(matches!(naive_eval(&program), Err(Rejection::AggrError(_))));

        // sum folding a Bool.
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert(
            "d",
            [vec![v(1), DataValue::from(true)]].into_iter().collect(),
        );
        let program = Program {
            rules: vec![Rule::aggregated(
                "t",
                vec![x(), y()],
                vec![None, named("sum")],
                vec![lit("d", vec![x(), y()], false)],
            )],
            facts,
            ..Program::default()
        };
        assert!(matches!(naive_eval(&program), Err(Rejection::AggrError(_))));
    }

    /// The ill-formed shapes the real compiler refuses at parse/compile
    /// time (upstream `parser::head_aggr_mismatch` among them) are refused
    /// here as values.
    #[test]
    fn malformed_programs_are_refused_not_evaluated() {
        // Aggregation vector shorter than the head.
        let short = Program {
            rules: vec![Rule::aggregated(
                "p",
                vec![x(), y()],
                vec![named("min")],
                vec![lit("d", vec![x(), y()], false)],
            )],
            ..Program::default()
        };
        assert!(matches!(naive_eval(&short), Err(Rejection::Malformed("p"))));

        // Rules of one head disagreeing on the aggregation signature.
        let mismatch = Program {
            rules: vec![
                Rule::aggregated(
                    "p",
                    vec![x(), y()],
                    vec![None, named("min")],
                    vec![lit("d", vec![x(), y()], false)],
                ),
                Rule::aggregated(
                    "p",
                    vec![x(), y()],
                    vec![None, named("count")],
                    vec![lit("d", vec![x(), y()], false)],
                ),
            ],
            ..Program::default()
        };
        assert!(matches!(
            naive_eval(&mismatch),
            Err(Rejection::Malformed("p"))
        ));

        // A fixed head that is also a rule head.
        let clash = Program {
            rules: vec![Rule::plain(
                "f",
                vec![x()],
                vec![lit("d", vec![x()], false)],
            )],
            fixed: vec![FixedRule {
                head_rel: "f",
                inputs: vec![],
                eval: |_| BTreeSet::new(),
            }],
            ..Program::default()
        };
        assert!(matches!(naive_eval(&clash), Err(Rejection::Malformed("f"))));

        // Facts under an aggregated head.
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
        facts.insert("m", [vec![v(1), v(1)]].into_iter().collect());
        let seeded = Program {
            rules: meet_reach_rules("min"),
            facts,
            ..Program::default()
        };
        assert!(matches!(
            naive_eval(&seeded),
            Err(Rejection::Malformed("m"))
        ));

        // Duplicate fixed heads.
        let dup = Program {
            fixed: vec![
                FixedRule {
                    head_rel: "f",
                    inputs: vec![],
                    eval: |_| BTreeSet::new(),
                },
                FixedRule {
                    head_rel: "f",
                    inputs: vec![],
                    eval: |_| BTreeSet::new(),
                },
            ],
            ..Program::default()
        };
        assert!(matches!(naive_eval(&dup), Err(Rejection::Malformed("f"))));

        // A relation used at two different arities.
        let clash = Program {
            rules: vec![Rule::plain(
                "p",
                vec![x()],
                vec![lit("edge", vec![x()], false)],
            )],
            facts: edge_facts(&[(1, 2)]),
            ..Program::default()
        };
        assert!(matches!(
            naive_eval(&clash),
            Err(Rejection::Malformed("edge"))
        ));
    }

    /// Which changed-flag the delta machinery believes.
    #[derive(Clone, Copy)]
    enum FlagMode {
        /// The landed contract: true iff the stored value changed.
        Landed,
        /// Upstream's inverted `and`/`or` flag (`old == *l`): believe the
        /// opposite of what happened.
        UpstreamInverted,
    }

    /// A transcription of upstream's semi-naive meet evaluation for the
    /// [`meet_reach_rules`] shape (`eval.rs::initial_rule_meet_eval` /
    /// `incremental_rule_meet_eval` joining against the delta, plus
    /// `temp_store.rs::MeetAggrStore::merge_in`'s flag-gated delta): per
    /// epoch, the recursive rule derives only from the previous delta,
    /// rows meet into the running total, and a key re-enters the delta
    /// only when the changed-flag says its accumulated value moved. The
    /// flag is therefore load-bearing: lie once and propagation stops.
    fn semi_naive_meet_reach(
        edges: &BTreeSet<(i64, i64)>,
        seeds: &BTreeMap<i64, DataValue>,
        op: &dyn MeetAggrObj,
        mode: FlagMode,
    ) -> BTreeMap<i64, DataValue> {
        let mut total: BTreeMap<i64, DataValue> = BTreeMap::new();
        // Epoch 0: only the seed rule fires — the recursive store is empty.
        let mut epoch_rows: Vec<(i64, DataValue)> =
            seeds.iter().map(|(k, val)| (*k, val.clone())).collect();
        for _epoch in 0..100_000 {
            // The epoch's own meet store: rows meet together before merging.
            let mut fresh: BTreeMap<i64, DataValue> = BTreeMap::new();
            for (k, val) in epoch_rows {
                match fresh.entry(k) {
                    Entry::Vacant(e) => {
                        e.insert(val);
                    }
                    Entry::Occupied(mut e) => {
                        op.update(e.get_mut(), &val).expect("meet update");
                    }
                }
            }
            // merge_in: flag-gated delta discovery.
            let mut delta: BTreeMap<i64, DataValue> = BTreeMap::new();
            for (k, val) in fresh {
                match total.entry(k) {
                    Entry::Vacant(e) => {
                        delta.insert(k, val.clone());
                        e.insert(val);
                    }
                    Entry::Occupied(mut e) => {
                        let really_changed = op.update(e.get_mut(), &val).expect("meet update");
                        let believed = match mode {
                            FlagMode::Landed => really_changed,
                            FlagMode::UpstreamInverted => !really_changed,
                        };
                        if believed {
                            delta.insert(k, e.get().clone());
                        }
                    }
                }
            }
            if delta.is_empty() {
                return total;
            }
            // Next epoch: the recursive rule joined against the delta only.
            let mut next = Vec::new();
            for (from, val) in &delta {
                for (a, b) in edges {
                    if a == from {
                        next.push((*b, val.clone()));
                    }
                }
            }
            epoch_rows = next;
        }
        panic!("semi-naive simulator failed to converge");
    }

    /// The upstream `and`/`or` premature-fixpoint bug, as a differential.
    /// Upstream's `MeetAggrAnd`/`MeetAggrOr` returned `old == *l` — the
    /// inversion of the changed-flag contract — so the one update that
    /// flips an accumulated value announced "unchanged", the key never
    /// re-entered the delta, and recursion stopped one hop short. The
    /// naive oracle computes the correct fixpoint; the same semi-naive
    /// machinery run with the inverted flag reproduces exactly what
    /// upstream would have returned, and the two must differ.
    #[test]
    fn and_or_inverted_flag_reaches_a_premature_fixpoint() {
        let edges: BTreeSet<(i64, i64)> = [(1, 2), (2, 3)].into_iter().collect();
        // or: truth must propagate 1 → 2 → 3; and: falsity must.
        for (name, seed_of, fixpoint) in [
            ("or", [true, false, false], true),
            ("and", [false, true, true], false),
        ] {
            let seeds: BTreeMap<i64, DataValue> = (1..=3)
                .map(|k| (k, DataValue::from(seed_of[(k - 1) as usize])))
                .collect();
            let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
            facts.insert(
                "edge",
                edges.iter().map(|(a, b)| vec![v(*a), v(*b)]).collect(),
            );
            facts.insert(
                "seed",
                seeds
                    .iter()
                    .map(|(k, val)| vec![v(*k), val.clone()])
                    .collect(),
            );
            let program = Program {
                rules: meet_reach_rules(name),
                facts,
                ..Program::default()
            };
            let db = naive_eval(&program).unwrap();
            let correct: BTreeMap<i64, DataValue> =
                (1..=3).map(|k| (k, DataValue::from(fixpoint))).collect();
            let oracle: BTreeMap<i64, DataValue> = db["m"]
                .iter()
                .map(|t| (t[0].get_int().expect("int key"), t[1].clone()))
                .collect();
            assert_eq!(oracle, correct, "oracle fixpoint for {name}");

            let op = parse_aggr(name)
                .expect("real aggregation")
                .meet_op()
                .expect("meet form");
            // The honest flag reaches the oracle's fixpoint...
            let honest = semi_naive_meet_reach(&edges, &seeds, op.as_ref(), FlagMode::Landed);
            assert_eq!(
                honest, oracle,
                "honest semi-naive equals the oracle for {name}"
            );
            // ...the inverted flag stops early: node 2's flip is applied
            // to the store but never re-enters the delta, so node 3 keeps
            // its seed value.
            let buggy =
                semi_naive_meet_reach(&edges, &seeds, op.as_ref(), FlagMode::UpstreamInverted);
            assert_ne!(
                buggy, oracle,
                "the upstream inversion must be observable for {name}"
            );
            assert_eq!(
                buggy[&2],
                DataValue::from(fixpoint),
                "node 2's stored value did move"
            );
            assert_eq!(
                buggy[&3],
                DataValue::from(!fixpoint),
                "node 3 is stranded at its seed: the premature fixpoint for {name}"
            );
        }
    }

    #[derive(Clone, Debug)]
    struct MeetCase {
        aggr_name: &'static str,
        edges: BTreeSet<(i64, i64)>,
        seeds: BTreeMap<i64, DataValue>,
    }

    fn case_for(name: &'static str, value: BoxedStrategy<DataValue>) -> BoxedStrategy<MeetCase> {
        (1i64..=5)
            .prop_flat_map(move |n| {
                let value = value.clone();
                (
                    prop::collection::btree_set((0..n, 0..n), 0..8),
                    prop::collection::btree_map(0..n, value, 0..=(n as usize)),
                )
            })
            .prop_map(move |(edges, seeds)| MeetCase {
                aggr_name: name,
                edges,
                seeds,
            })
            .boxed()
    }

    /// Random small meet-recursive programs over the commutative meets;
    /// values are typed per aggregation (`union` seeds are `Set`s, the
    /// canonical accumulator representation).
    fn arb_meet_case() -> BoxedStrategy<MeetCase> {
        let bool_val = || any::<bool>().prop_map(DataValue::from).boxed();
        let int_val = || (-10i64..10).prop_map(DataValue::from).boxed();
        let set_val = prop::collection::btree_set((0i64..4).prop_map(DataValue::from), 0..3)
            .prop_map(DataValue::Set)
            .boxed();
        prop_oneof![
            case_for("or", bool_val()),
            case_for("and", bool_val()),
            case_for("min", int_val()),
            case_for("max", int_val()),
            case_for("union", set_val),
        ]
        .boxed()
    }

    proptest! {
        /// Oracle self-consistency: on randomly generated meet-recursive
        /// programs, naive re-derivation-to-fixpoint equals the
        /// upstream-shaped semi-naive strategy driven by the landed
        /// changed-flags, and a plain rule one stratum up reads exactly
        /// the accumulated meet relation.
        #[test]
        fn naive_meet_fixpoint_matches_semi_naive(case in arb_meet_case()) {
            let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
            facts.insert(
                "edge",
                case.edges.iter().map(|(a, b)| vec![v(*a), v(*b)]).collect(),
            );
            facts.insert(
                "seed",
                case.seeds.iter().map(|(k, val)| vec![v(*k), val.clone()]).collect(),
            );
            let mut rules = meet_reach_rules(case.aggr_name);
            rules.push(Rule::plain(
                "out",
                vec![x(), y()],
                vec![lit("m", vec![x(), y()], false)],
            ));
            let program = Program { rules, facts, ..Program::default() };
            let db = naive_eval(&program).expect("stratifiable meet program");
            let m = db.get("m").cloned().unwrap_or_default();

            let op = parse_aggr(case.aggr_name)
                .expect("real aggregation")
                .meet_op()
                .expect("meet form");
            let semi_naive: BTreeSet<Tuple> =
                semi_naive_meet_reach(&case.edges, &case.seeds, op.as_ref(), FlagMode::Landed)
                    .into_iter()
                    .map(|(k, val)| vec![v(k), val])
                    .collect();
            prop_assert_eq!(&m, &semi_naive);
            prop_assert_eq!(db.get("out").cloned().unwrap_or_default(), m);
        }
    }
}
