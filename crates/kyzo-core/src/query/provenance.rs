/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The provenance trials: semiring provenance proven against independent
//! references. Test-only; touches no engine source. It drives the
//! `pub(crate)` eval seams ([`stratified_evaluate_with_stores`],
//! [`provenance_graph`]) and the semiring solver exactly as the runtime
//! tier will, and judges them against:
//!
//! - **the semiring axioms**, asserted on randomized values;
//! - **the sealed oracle** ([`crate::query::laws::naive_eval`]): the
//!   boolean semiring's support must be byte-identical to set semantics;
//! - **an independent shortest-derivation reference** (naive Bellman
//!   iteration written from the model alone — no solver, no graph, no
//!   evaluator symbol): tropical min-costs must agree exactly;
//! - **an independent certificate checker** (rule instantiations
//!   re-derived from scratch over the model): extracted min-cost proofs
//!   must verify, and corrupted ones must be rejected;
//! - **the determinism law**: annotations and certificates byte-identical
//!   at 1/2/4/8 rayon threads;
//! - **the typed refusals**: unattributed bodies, un-retained stores, the
//!   enumeration ceiling, the solver pass ceiling, tropical overflow.
//!
//! The model harness (`ModelBody`, `compile_for`, the stratum assignment)
//! is the shape of the trials harness: a faithful stand-in for a compiled
//! RA plan, driving the post-stratification [`RuleBody`] seam.

#![cfg(test)]

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::num::{NonZeroU32, NonZeroU64};
use std::ops::ControlFlow;
use std::sync::Arc;

use miette::Result;

use crate::data::aggr::parse_aggr;
use crate::data::program::{MagicSymbol, StoreLifetimes};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::data::value::Tuple;
use crate::query::eval::{
    AtomOccurrence, Budget, EvalDefinition, EvalProgram, EvalRuleSet, EvalStratum, FixedRuleEval,
    PremiseSource, Premises, ProvNode, ProvenanceUnsupported, RowLimit, RuleBody, provenance_graph,
    stratified_evaluate_with_stores,
};
use crate::query::laws::{
    Bindings, HeadAggr, Literal, Program, Rel, Rule, Term, ground, head_classes, naive_eval, unify,
};
use crate::query::levels::EpochStore;
use crate::query::semiring::{
    Annotation, Cost, Derivation, DerivationGraph, ProofNode, ProvenanceLimitExceeded, Semiring,
    SemiringOverflow, SolverBudget, as_cost_map, extract_min_cost_proof, solve, verify_proof,
};
use crate::query::temp_store::{RegularTempStore, TupleInIter};

// ════════════════════════════════════════════════════════════════════════
// Seeded RNG — splitmix64, one u64 seed and nothing else (replayable).
// ════════════════════════════════════════════════════════════════════════

struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        Rng { state: seed }
    }
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// A value in `[0, n)`. Modulo bias is irrelevant at test scales.
    fn below(&mut self, n: u64) -> u64 {
        debug_assert!(n > 0);
        self.next_u64() % n
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        debug_assert!(hi > lo);
        lo + self.below((hi - lo) as u64) as i64
    }
    fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }
}

// ════════════════════════════════════════════════════════════════════════
// The oracle-model RuleBody harness (the trials shape): one `laws::Rule`
// body evaluated by naive nested-loop unification against the live
// EpochStore map — the stand-in for a compiled RA plan. Here it also
// attributes its premises (`premise_sources`), which is exactly what a
// compiled plan will do from its literal manifest.
// ════════════════════════════════════════════════════════════════════════

fn muggle(rel: &str) -> MagicSymbol {
    MagicSymbol::Muggle {
        inner: Symbol::new(rel, SourceSpan(0, 0)),
    }
}
fn entry_symbol() -> MagicSymbol {
    MagicSymbol::Muggle {
        inner: Symbol::prog_entry(SourceSpan(0, 0)),
    }
}

// `Bindings`, `unify`, and `ground` are the shared reference-tier helpers
// from `query/laws.rs` (issue #89) — this harness used to hand-copy them.

struct ModelBody {
    head: Vec<Term>,
    body: Vec<Literal>,
    facts: Arc<BTreeMap<Rel, BTreeSet<Tuple>>>,
    idb: Arc<BTreeSet<Rel>>,
    /// Occurrence key = this literal's position in `body` — one entry per
    /// idb literal, positive OR negated: this map is also the
    /// lifetime-tracking dependency source (`note_use`), and a store read
    /// only inside a negation is used just as much as one read positively.
    /// A relation mentioned twice gets two independent, independently
    /// delta-selectable occurrences (matches `compile.rs::contained_rules`'s
    /// numbering over the real engine's `MagicInlineRule::body`) — though
    /// `for_each_derivation` never selects a negated occurrence's delta.
    contained: BTreeMap<AtomOccurrence, MagicSymbol>,
}

impl ModelBody {
    fn new(
        head: Vec<Term>,
        body: Vec<Literal>,
        facts: Arc<BTreeMap<Rel, BTreeSet<Tuple>>>,
        idb: Arc<BTreeSet<Rel>>,
    ) -> Self {
        let mut contained: BTreeMap<AtomOccurrence, MagicSymbol> = BTreeMap::new();
        for (i, l) in body.iter().enumerate() {
            if idb.contains(l.rel) {
                contained.insert(AtomOccurrence(i), muggle(l.rel));
            }
        }
        Self {
            head,
            body,
            facts,
            idb,
            contained,
        }
    }

    fn rows_of(
        &self,
        stores: &BTreeMap<MagicSymbol, EpochStore>,
        rel: Rel,
        use_delta: bool,
    ) -> Vec<Tuple> {
        if self.idb.contains(rel) {
            let store = stores
                .get(&muggle(rel))
                .expect("harness: IDB store present");
            if use_delta {
                store
                    .delta_all_iter()
                    .map(TupleInIter::into_tuple)
                    .collect()
            } else {
                store.all_iter().map(TupleInIter::into_tuple).collect()
            }
        } else {
            self.facts
                .get(rel)
                .map(|set| set.iter().cloned().collect())
                .unwrap_or_default()
        }
    }

    fn negated_probe_hits(
        &self,
        stores: &BTreeMap<MagicSymbol, EpochStore>,
        rel: Rel,
        probe: &Tuple,
    ) -> bool {
        if self.idb.contains(rel) {
            let store = stores
                .get(&muggle(rel))
                .expect("harness: IDB store present");
            store
                .prefix_iter(probe)
                .next()
                .is_some_and(|t| t.into_tuple() == *probe)
        } else {
            self.facts.get(rel).is_some_and(|set| set.contains(probe))
        }
    }
}

impl crate::query::eval::seal::Sealed for ModelBody {}

impl RuleBody for ModelBody {
    fn for_each_derivation(
        &self,
        stores: &BTreeMap<MagicSymbol, EpochStore>,
        delta_from: Option<AtomOccurrence>,
        want_premises: bool,
        f: &mut dyn FnMut(Cow<'_, [DataValue]>, Premises<'_>) -> Result<ControlFlow<()>>,
    ) -> Result<()> {
        let mut ordered: Vec<(usize, &Literal)> = self
            .body
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.negated)
            .collect();
        ordered.extend(self.body.iter().enumerate().filter(|(_, l)| l.negated));

        let mut frontier: Vec<(Bindings, Vec<Tuple>)> = vec![(Bindings::new(), Vec::new())];
        for (body_pos, l) in ordered {
            // This literal's OWN occurrence (its position in the original
            // body, stable across the positive/negated reordering above)
            // must match `delta_from` exactly.
            let is_delta = !l.negated && delta_from == Some(AtomOccurrence(body_pos));
            let mut next = Vec::new();
            if l.negated {
                for (bound, premises) in &frontier {
                    let probe = ground(&l.args, bound);
                    if !self.negated_probe_hits(stores, l.rel, &probe) {
                        next.push((bound.clone(), premises.clone()));
                    }
                }
            } else {
                let rows = self.rows_of(stores, l.rel, is_delta);
                for (bound, premises) in &frontier {
                    for row in &rows {
                        if let Some(b) = unify(&l.args, row.as_slice(), bound) {
                            let mut p = premises.clone();
                            if want_premises {
                                p.push(row.clone());
                            }
                            next.push((b, p));
                        }
                    }
                }
            }
            frontier = next;
        }
        for (bound, premises) in frontier {
            let head = ground(&self.head, &bound);
            let arg = if want_premises {
                Premises::Rows(&premises)
            } else {
                Premises::NotRequested
            };
            if f(Cow::Owned(head.into_vec()), arg)?.is_break() {
                return Ok(());
            }
        }
        Ok(())
    }

    fn contained_rules(&self) -> &BTreeMap<AtomOccurrence, MagicSymbol> {
        &self.contained
    }

    /// The attribution a compiled plan will produce from its literal
    /// manifest: one source per positive literal, in body order.
    fn premise_sources(&self) -> Option<Vec<PremiseSource>> {
        Some(
            self.body
                .iter()
                .filter(|l| !l.negated)
                .map(|l| {
                    if self.idb.contains(l.rel) {
                        PremiseSource::Rule(muggle(l.rel))
                    } else {
                        PremiseSource::Fact(l.rel.to_string())
                    }
                })
                .collect(),
        )
    }
}

/// A body that refuses to attribute: the negative control for the typed
/// [`ProvenanceUnsupported`] refusal. Delegates evaluation to the model.
struct UnattributedBody(ModelBody);

impl crate::query::eval::seal::Sealed for UnattributedBody {}

impl RuleBody for UnattributedBody {
    fn for_each_derivation(
        &self,
        stores: &BTreeMap<MagicSymbol, EpochStore>,
        delta_from: Option<AtomOccurrence>,
        want_premises: bool,
        f: &mut dyn FnMut(Cow<'_, [DataValue]>, Premises<'_>) -> Result<ControlFlow<()>>,
    ) -> Result<()> {
        self.0
            .for_each_derivation(stores, delta_from, want_premises, f)
    }
    fn contained_rules(&self) -> &BTreeMap<AtomOccurrence, MagicSymbol> {
        self.0.contained_rules()
    }
    // premise_sources deliberately left at the default `None`.
}

/// A fixed-rule type for the `EvalProgram` parameter; never constructed
/// (these trials use inline rules only).
struct NoFixed;

impl FixedRuleEval for NoFixed {
    fn run(
        &self,
        _stores: &BTreeMap<MagicSymbol, EpochStore>,
        _out: &mut RegularTempStore,
        _budget: &Budget,
        _baseline: u64,
    ) -> Result<()> {
        unreachable!("NoFixed is never installed in a program")
    }
}

// ── stratum assignment: the oracle's Bellman-Ford, transcribed (the
// oracle's own `strata` is private; any valid stratification yields the
// oracle's fixpoint) ─────────────────────────────────────────────────────

// `HeadClass`/`head_classes` are the shared reference-tier items from
// `query/laws.rs` (issue #89) — this harness used to hand-copy them too.

fn dependency_edges(program: &Program) -> Vec<(Rel, Rel, bool)> {
    let classes = head_classes(program);
    let is_meet = |rel: Rel| classes.get(rel).is_some_and(|c| c.is_meet);
    let mut edges = Vec::new();
    for rule in &program.rules {
        let head = rule.head_rel;
        let class = &classes[&head];
        for l in &rule.body {
            let forcing = if class.has_aggr {
                if class.is_meet && l.rel == head {
                    l.negated
                } else {
                    true
                }
            } else {
                l.negated || is_meet(l.rel)
            };
            edges.push((head, l.rel, forcing));
        }
    }
    edges
}

fn strata_of(program: &Program) -> BTreeMap<Rel, usize> {
    let edges = dependency_edges(program);
    let mut s: BTreeMap<Rel, usize> = BTreeMap::new();
    for rule in &program.rules {
        s.insert(rule.head_rel, 0);
        for l in &rule.body {
            s.insert(l.rel, 0);
        }
    }
    for rel in program.facts.keys() {
        s.insert(rel, 0);
    }
    let bound = s.len() + 1;
    for _ in 0..bound {
        let mut changed = false;
        for (head, dep, forcing) in &edges {
            let need = s[dep] + usize::from(*forcing);
            if s[head] < need {
                s.insert(head, need);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    s
}

const ENTRY_VARS: [&str; 8] = ["v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7"];

struct Compiled {
    program: EvalProgram<ModelBody, NoFixed>,
    lifetimes: StoreLifetimes,
}

/// Compile the oracle model for evaluation with `target` as the entry
/// rule `?[vars…] := target[vars…]`. When `retain_all` is set, every rule
/// store's lifetime is extended to the final stratum — the provenance
/// requirement documented on [`stratified_evaluate_with_stores`].
fn compile_for(model: &Program, target: Rel, target_arity: usize, retain_all: bool) -> Compiled {
    let idb: Arc<BTreeSet<Rel>> = Arc::new(model.rules.iter().map(|r| r.head_rel).collect());
    for rel in idb.iter() {
        assert!(
            !model.facts.contains_key(rel),
            "harness limitation: facts under rule head {rel}"
        );
    }
    let facts = Arc::new(model.facts.clone());
    let strata_map = strata_of(model);
    let entry_stratum = strata_map.values().copied().max().unwrap_or(0) + 1;

    let mut strata: Vec<EvalStratum<ModelBody, NoFixed>> = (0..=entry_stratum)
        .map(|_| EvalStratum::default())
        .collect();
    let mut lifetimes = StoreLifetimes::default();

    let mut heads_in_order: Vec<Rel> = Vec::new();
    let mut per_head: BTreeMap<Rel, Vec<&Rule>> = BTreeMap::new();
    for rule in &model.rules {
        if !per_head.contains_key(rule.head_rel) {
            heads_in_order.push(rule.head_rel);
        }
        per_head.entry(rule.head_rel).or_default().push(rule);
    }
    for head in heads_in_order {
        let rules = &per_head[head];
        let stratum = strata_map[head];
        let bodies: Vec<ModelBody> = rules
            .iter()
            .map(|r| {
                ModelBody::new(
                    r.head_args.clone(),
                    r.body.clone(),
                    facts.clone(),
                    idb.clone(),
                )
            })
            .collect();
        for body in &bodies {
            for dep in body.contained_rules().values() {
                lifetimes.note_use(dep.clone(), stratum);
            }
        }
        if retain_all {
            lifetimes.note_use(muggle(head), entry_stratum);
        }
        let rule_set = EvalRuleSet::new(rules[0].aggr.clone(), bodies).expect("well-shaped set");
        strata[stratum]
            .defs
            .insert(muggle(head), EvalDefinition::Rules(rule_set));
    }
    let vars: Vec<Term> = ENTRY_VARS[..target_arity]
        .iter()
        .copied()
        .map(Term::Var)
        .collect();
    let entry_body = ModelBody::new(
        vars.clone(),
        vec![Literal::pos(target, vars)],
        facts.clone(),
        idb.clone(),
    );
    lifetimes.note_use(muggle(target), entry_stratum);
    let entry_set = EvalRuleSet::new(
        std::iter::repeat_n(None, target_arity).collect(),
        vec![entry_body],
    )
    .expect("entry rule set");
    strata[entry_stratum]
        .defs
        .insert(entry_symbol(), EvalDefinition::Rules(entry_set));

    let program = EvalProgram::from_execution_order(strata).expect("entry in final stratum");
    Compiled { program, lifetimes }
}

fn idb_of(model: &Program) -> BTreeSet<Rel> {
    model.rules.iter().map(|r| r.head_rel).collect()
}

fn generous_budget() -> Budget {
    Budget::new(NonZeroU32::new(10_000).unwrap())
}

fn generous_ceiling() -> NonZeroU64 {
    NonZeroU64::new(50_000_000).unwrap()
}

fn generous_solver() -> SolverBudget {
    SolverBudget::new(NonZeroU32::new(100_000).unwrap())
}

fn unit_weight(_sym: &MagicSymbol, _idx: usize) -> NonZeroU64 {
    NonZeroU64::new(1).unwrap()
}

#[cfg(not(target_arch = "wasm32"))]
fn at_thread_count<T: Send>(threads: usize, f: impl FnOnce() -> T + Send) -> T {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .expect("thread pool")
        .install(|| {
            // A 1-thread "8-thread" run would prove nothing.
            assert_eq!(
                rayon::current_num_threads(),
                threads,
                "rayon pool width mismatch"
            );
            f()
        })
}

// ════════════════════════════════════════════════════════════════════════
// The engine pipeline under test: evaluate, retain stores, enumerate the
// derivation graph. (The semiring solve runs on top, per test.)
// ════════════════════════════════════════════════════════════════════════

#[derive(Debug)]
struct PipelineOutput {
    /// Rows per rule store (the engine's set-semantics fixpoint).
    rows: BTreeMap<Rel, BTreeSet<Tuple>>,
    graph: DerivationGraph<ProvNode>,
}

fn run_pipeline(
    model: &Program,
    target: Rel,
    target_arity: usize,
    ceiling: NonZeroU64,
    weights: &dyn Fn(&MagicSymbol, usize) -> NonZeroU64,
) -> Result<PipelineOutput> {
    let compiled = compile_for(model, target, target_arity, true);
    let budget = generous_budget();
    let (_outcome, stores) = stratified_evaluate_with_stores(
        &compiled.program,
        &compiled.lifetimes,
        RowLimit::default(),
        &budget,
        None,
    )?;
    let graph = provenance_graph(&compiled.program, &stores, &budget, ceiling, weights)?;
    let mut rows: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
    for rel in idb_of(model) {
        let store = stores.get(&muggle(rel)).expect("store retained");
        rows.insert(rel, store.all_iter().map(TupleInIter::into_tuple).collect());
    }
    Ok(PipelineOutput { rows, graph })
}

fn rule_node(rel: Rel, tuple: &Tuple) -> ProvNode {
    (PremiseSource::Rule(muggle(rel)), tuple.clone())
}

// ════════════════════════════════════════════════════════════════════════
// Generated positive programs (the fragment full provenance is claimed
// for: plain rules, recursion, joins; no negation/aggregation).
// ════════════════════════════════════════════════════════════════════════

fn v(i: i64) -> DataValue {
    DataValue::from(i)
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
fn lit(rel: Rel, args: Vec<Term>, negated: bool) -> Literal {
    if negated {
        Literal::neg(rel, args)
    } else {
        Literal::pos(rel, args)
    }
}
fn named(name: &str) -> HeadAggr {
    Some((parse_aggr(name).expect("real aggregation"), vec![]))
}

fn gen_positive(seed: u64, small: bool) -> Program {
    let mut rng = Rng::new(seed);
    let n = if small {
        rng.range(4, 8)
    } else {
        rng.range(5, 12)
    };
    let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
    let n_edges = rng.below((n * 2) as u64) as i64 + 1;
    let edges: BTreeSet<Tuple> = (0..n_edges)
        .map(|_| vec![v(rng.range(0, n)), v(rng.range(0, n))])
        .map(Tuple::from_vec)
        .collect();
    facts.insert("edge", edges);

    let mut rules: Vec<Rule> = Vec::new();
    rules.push(Rule::plain(
        "path",
        vec![x(), y()],
        vec![lit("edge", vec![x(), y()], false)],
    ));
    if rng.chance(1, 2) {
        // Self-join transitive closure: two IDB premises per derivation.
        rules.push(Rule::plain(
            "path",
            vec![x(), z()],
            vec![
                lit("path", vec![x(), y()], false),
                lit("path", vec![y(), z()], false),
            ],
        ));
    } else {
        rules.push(Rule::plain(
            "path",
            vec![x(), z()],
            vec![
                lit("edge", vec![x(), y()], false),
                lit("path", vec![y(), z()], false),
            ],
        ));
    }
    if rng.chance(1, 2) {
        // Mutual recursion.
        rules.push(Rule::plain(
            "qa",
            vec![x(), y()],
            vec![lit("edge", vec![x(), y()], false)],
        ));
        rules.push(Rule::plain(
            "qa",
            vec![x(), z()],
            vec![
                lit("qb", vec![x(), y()], false),
                lit("edge", vec![y(), z()], false),
            ],
        ));
        rules.push(Rule::plain(
            "qb",
            vec![x(), z()],
            vec![
                lit("qa", vec![x(), y()], false),
                lit("edge", vec![y(), z()], false),
            ],
        ));
        if rng.chance(1, 2) {
            // A join over two separately-derived recursive stores.
            rules.push(Rule::plain(
                "j",
                vec![x(), z()],
                vec![
                    lit("qa", vec![x(), y()], false),
                    lit("qb", vec![y(), z()], false),
                ],
            ));
        }
    }
    if rng.chance(1, 2) {
        rules.push(Rule::plain(
            "hop2",
            vec![x(), z()],
            vec![
                lit("path", vec![x(), y()], false),
                lit("edge", vec![y(), z()], false),
            ],
        ));
    }
    Program::untimed(rules, vec![], facts)
}

/// Randomized per-(head, rule-index) weights in `1..=8`, plus the map the
/// independent reference consumes.
fn gen_weights(model: &Program, seed: u64) -> BTreeMap<(Rel, usize), u64> {
    let mut rng = Rng::new(seed ^ 0xDEAD_BEEF_CAFE_F00D);
    let mut per_head_counts: BTreeMap<Rel, usize> = BTreeMap::new();
    let mut weights = BTreeMap::new();
    for rule in &model.rules {
        let idx = per_head_counts.entry(rule.head_rel).or_default();
        weights.insert((rule.head_rel, *idx), rng.below(8) + 1);
        *idx += 1;
    }
    weights
}

fn engine_weight_fn(
    weights: &BTreeMap<(Rel, usize), u64>,
) -> impl Fn(&MagicSymbol, usize) -> NonZeroU64 + '_ {
    move |sym, idx| {
        let name = sym.as_plain_symbol().name.to_string();
        let w = weights
            .iter()
            .find(|((rel, i), _)| **rel == name && *i == idx)
            .map(|(_, w)| *w)
            .unwrap_or(1);
        NonZeroU64::new(w).expect("weights are 1..=8")
    }
}

// ════════════════════════════════════════════════════════════════════════
// The independent tropical reference: naive Bellman iteration over the
// oracle's fixpoint, written from the model alone. Imports no evaluator,
// graph, or solver machinery — its only shared vocabulary is the model
// types and this file's local `unify`/`ground`.
// ════════════════════════════════════════════════════════════════════════

/// All grounded instantiations of one positive-only rule over `db`
/// (IDB rows) and the model facts: `(head, premises-in-body-order)`.
fn rule_instantiations(
    rule: &Rule,
    facts: &BTreeMap<Rel, BTreeSet<Tuple>>,
    idb: &BTreeSet<Rel>,
    db: &BTreeMap<Rel, BTreeSet<Tuple>>,
) -> Vec<(Tuple, Vec<(Rel, Tuple)>)> {
    assert!(
        rule.body.iter().all(|l| !l.negated),
        "reference covers the positive fragment only"
    );
    let mut frontier: Vec<(Bindings, Vec<(Rel, Tuple)>)> = vec![(Bindings::new(), Vec::new())];
    for l in &rule.body {
        let rows: Vec<Tuple> = if idb.contains(l.rel) {
            db.get(l.rel)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default()
        } else {
            facts
                .get(l.rel)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default()
        };
        let mut next = Vec::new();
        for (bound, premises) in &frontier {
            for row in &rows {
                if let Some(b) = unify(&l.args, row.as_slice(), bound) {
                    let mut p = premises.clone();
                    p.push((l.rel, row.clone()));
                    next.push((b, p));
                }
            }
        }
        frontier = next;
    }
    frontier
        .into_iter()
        .map(|(bound, premises)| (ground(&rule.head_args, &bound), premises))
        .collect()
}

/// The reference min-cost map: iterate
/// `cost(t) = min over instantiations of (w + Σ cost(premises))`
/// to its fixpoint over the oracle database (facts cost 0). Naive and
/// obviously correct; termination because costs strictly decrease in a
/// well-founded order.
fn reference_min_costs(
    model: &Program,
    db: &BTreeMap<Rel, BTreeSet<Tuple>>,
    weights: &BTreeMap<(Rel, usize), u64>,
) -> BTreeMap<(Rel, Tuple), u64> {
    let idb = idb_of(model);
    let mut per_head_counts: BTreeMap<Rel, usize> = BTreeMap::new();
    let indexed_rules: Vec<(usize, &Rule)> = model
        .rules
        .iter()
        .map(|r| {
            let idx = per_head_counts.entry(r.head_rel).or_default();
            let i = *idx;
            *idx += 1;
            (i, r)
        })
        .collect();
    let mut costs: BTreeMap<(Rel, Tuple), u64> = BTreeMap::new();
    // The fixpoint bound: each improving round lowers at least one cost,
    // and all costs are bounded; a generous round cap guards the loop.
    for _round in 0..100_000 {
        let mut changed = false;
        for (idx, rule) in &indexed_rules {
            let w = weights[&(rule.head_rel, *idx)];
            for (head, premises) in rule_instantiations(rule, &model.facts, &idb, db) {
                let mut total = Some(w);
                for (prel, prow) in &premises {
                    let pc = if idb.contains(prel) {
                        costs.get(&(prel, prow.clone())).copied()
                    } else {
                        Some(0)
                    };
                    total = match (total, pc) {
                        (Some(t), Some(c)) => t.checked_add(c),
                        _ => None,
                    };
                }
                if let Some(total) = total {
                    let key = (rule.head_rel, head);
                    let better = costs.get(&key).is_none_or(|old| total < *old);
                    if better {
                        costs.insert(key, total);
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            return costs;
        }
    }
    panic!("reference fixpoint did not stabilize (bug in the reference)");
}

// ════════════════════════════════════════════════════════════════════════
// The independent certificate checker: every step re-derived from scratch
// over the model. Imports no evaluator or solver logic — [`ProofNode`]
// and [`PremiseSource`] are consumed as plain data.
// ════════════════════════════════════════════════════════════════════════

fn per_head_rules(model: &Program) -> BTreeMap<Rel, Vec<Rule>> {
    let mut per_head: BTreeMap<Rel, Vec<Rule>> = BTreeMap::new();
    for rule in &model.rules {
        per_head
            .entry(rule.head_rel)
            .or_default()
            .push(rule.clone());
    }
    per_head
}

fn node_rel_name(node: &ProvNode) -> String {
    match &node.0 {
        PremiseSource::Rule(sym) => sym.as_plain_symbol().name.to_string(),
        PremiseSource::Fact(name) => name.clone(),
    }
}

/// Verify a proof tree against the model: leaves are genuine stored
/// facts, every step is a valid instantiation of the named rule whose
/// positive premises are exactly the child tuples, and every claimed cost
/// is the rule weight plus the children's costs. Returns the root cost.
fn verify_model_proof(
    proof: &ProofNode<ProvNode>,
    per_head: &BTreeMap<Rel, Vec<Rule>>,
    facts: &BTreeMap<Rel, BTreeSet<Tuple>>,
    weights: &BTreeMap<(Rel, usize), u64>,
) -> std::result::Result<u64, String> {
    match proof {
        ProofNode::Fact { node } => {
            let (src, tuple) = node;
            match src {
                PremiseSource::Fact(name) => {
                    let is_fact = facts
                        .iter()
                        .any(|(rel, rows)| *rel == name.as_str() && rows.contains(tuple));
                    if is_fact {
                        Ok(0)
                    } else {
                        Err(format!("leaf {name}{tuple:?} is not a stored ground fact"))
                    }
                }
                PremiseSource::Rule(_) => Err(format!(
                    "boundary: leaf {node:?} grounds in an opaque store, not \
                     independently checkable from the model"
                )),
            }
        }
        ProofNode::Step {
            node,
            label,
            cost,
            premises,
            ..
        } => {
            let rel_name = node_rel_name(node);
            let (rel, rules) = per_head
                .iter()
                .find(|(rel, _)| **rel == rel_name)
                .ok_or_else(|| format!("no rules for head '{rel_name}'"))?;
            let rule = rules
                .get(*label)
                .ok_or_else(|| format!("rule index {label} out of range for '{rel_name}'"))?;
            if rule.body.iter().any(|l| l.negated) {
                return Err(format!(
                    "boundary: rule {label} of '{rel_name}' has a negated premise"
                ));
            }
            let positives: Vec<&Literal> = rule.body.iter().filter(|l| !l.negated).collect();
            if positives.len() != premises.len() {
                return Err(format!(
                    "'{rel_name}': {} premises for {} positive body literals",
                    premises.len(),
                    positives.len()
                ));
            }
            // One binding must satisfy the head and every positive premise.
            let mut bound = Bindings::new();
            let head_ok = {
                let mut ok = rule.head_args.len() == node.1.len();
                if ok {
                    match unify(&rule.head_args, node.1.as_slice(), &bound) {
                        Some(b) => bound = b,
                        None => ok = false,
                    }
                }
                ok
            };
            if !head_ok {
                return Err(format!(
                    "head of rule {label} does not ground to {:?}",
                    node.1
                ));
            }
            let mut total = weights[&(*rel, *label)];
            for (l, child) in positives.iter().zip(premises) {
                let child_node = child.node();
                if node_rel_name(child_node) != l.rel {
                    return Err(format!(
                        "premise relation mismatch: rule wants '{}', proof has '{}'",
                        l.rel,
                        node_rel_name(child_node)
                    ));
                }
                match unify(&l.args, child_node.1.as_slice(), &bound) {
                    Some(b) => bound = b,
                    None => {
                        return Err(format!(
                            "premise {:?} does not unify with literal {l:?}",
                            child_node.1
                        ));
                    }
                }
                let child_cost = verify_model_proof(child, per_head, facts, weights)?;
                total = total
                    .checked_add(child_cost)
                    .ok_or_else(|| "cost arithmetic overflow".to_string())?;
            }
            if total != *cost {
                return Err(format!(
                    "claimed cost {cost} ≠ re-derived cost {total} at {node:?}"
                ));
            }
            Ok(total)
        }
    }
}

// ════════════════════════════════════════════════════════════════════════
// TRIAL 1 — the semiring axioms, on randomized values.
// ════════════════════════════════════════════════════════════════════════

fn random_cost(rng: &mut Rng) -> Cost {
    if rng.chance(1, 8) {
        Cost::Infinite
    } else {
        // Below 2^31 so triple sums stay far from u64 overflow: the axiom
        // trial tests laws, the overflow trial tests the refusal.
        Cost::Finite(rng.below(1 << 31))
    }
}

fn assert_axioms(semiring: Semiring, a: &Annotation, b: &Annotation, c: &Annotation) {
    let t =
        |x: &Annotation, y: &Annotation| semiring.times(x, y).expect("no overflow in axiom trial");
    let p = |x: &Annotation, y: &Annotation| semiring.plus(x, y);
    let zero = semiring.zero();
    let one = semiring.one();
    // ⊕: associative, commutative, identity, idempotent.
    assert_eq!(p(&p(a, b), c), p(a, &p(b, c)), "⊕ associativity");
    assert_eq!(p(a, b), p(b, a), "⊕ commutativity");
    assert_eq!(p(a, &zero), a.clone(), "⊕ identity");
    assert_eq!(p(a, a), a.clone(), "⊕ idempotency (solver contract)");
    // ⊗: associative, commutative, identity, annihilator.
    assert_eq!(t(&t(a, b), c), t(a, &t(b, c)), "⊗ associativity");
    assert_eq!(t(a, b), t(b, a), "⊗ commutativity");
    assert_eq!(t(a, &one), a.clone(), "⊗ identity");
    assert_eq!(t(a, &zero), zero.clone(), "⊗ annihilator");
    // Distributivity.
    assert_eq!(t(a, &p(b, c)), p(&t(a, b), &t(a, c)), "distributivity");
}

#[test]
fn semiring_axioms_hold_on_randomized_values() {
    let mut rng = Rng::new(0x5EED_u64 ^ 0x51_3141);
    for _ in 0..2000 {
        let (a, b, c) = (
            Annotation::Boolean(rng.chance(1, 2)),
            Annotation::Boolean(rng.chance(1, 2)),
            Annotation::Boolean(rng.chance(1, 2)),
        );
        assert_axioms(Semiring::Boolean, &a, &b, &c);
    }
    for _ in 0..2000 {
        let (a, b, c) = (
            Annotation::Tropical(random_cost(&mut rng)),
            Annotation::Tropical(random_cost(&mut rng)),
            Annotation::Tropical(random_cost(&mut rng)),
        );
        assert_axioms(Semiring::Tropical, &a, &b, &c);
    }
}

#[test]
fn tropical_overflow_is_a_typed_refusal() {
    let err = Semiring::Tropical
        .times(
            &Annotation::Tropical(Cost::Finite(u64::MAX)),
            &Annotation::Tropical(Cost::Finite(1)),
        )
        .expect_err("must refuse");
    assert_eq!(
        err,
        SemiringOverflow {
            left: u64::MAX,
            right: 1
        }
    );
    // Infinity absorbs without overflow: ∞ ⊗ MAX is lawful.
    assert_eq!(
        Semiring::Tropical
            .times(
                &Annotation::Tropical(Cost::Infinite),
                &Annotation::Tropical(Cost::Finite(u64::MAX)),
            )
            .unwrap(),
        Annotation::Tropical(Cost::Infinite)
    );
    // And the solver surfaces the refusal typed, not stringly.
    let graph: DerivationGraph<u32> = DerivationGraph {
        facts: BTreeSet::from([0u32]),
        derivations: vec![
            Derivation {
                head: 1,
                label: 0,
                weight: NonZeroU64::new(u64::MAX).unwrap(),
                premises: vec![0],
            },
            Derivation {
                head: 2,
                label: 0,
                weight: NonZeroU64::new(2).unwrap(),
                premises: vec![1, 1],
            },
        ],
    };
    let err = solve(Semiring::Tropical, &graph, &generous_solver()).expect_err("must refuse");
    let refusal: &SemiringOverflow = err.downcast_ref().expect("typed SemiringOverflow");
    assert_eq!(refusal.right, u64::MAX);
}

// ════════════════════════════════════════════════════════════════════════
// TRIAL 2 — boolean semiring ≡ set semantics, by differential.
// ════════════════════════════════════════════════════════════════════════

#[test]
fn boolean_annotation_matches_naive_eval_byte_identical() {
    for i in 0..24u64 {
        let seed = Rng::new(i.wrapping_mul(0x9E37_79B9_7F4A_7C15)).next_u64();
        let model = gen_positive(seed, false);
        let oracle = naive_eval(&model).expect("oracle accepts the positive fragment");
        let out = run_pipeline(&model, "path", 2, generous_ceiling(), &unit_weight)
            .expect("pipeline runs");
        let ann = solve(Semiring::Boolean, &out.graph, &generous_solver()).expect("solver runs");
        for rel in idb_of(&model) {
            let oracle_rows = oracle.get(rel).cloned().unwrap_or_default();
            let engine_rows = &out.rows[rel];
            // The engine's fixpoint equals the oracle — byte-identical.
            assert_eq!(
                format!("{engine_rows:?}"),
                format!("{oracle_rows:?}"),
                "seed {seed}: '{rel}' fixpoint differs from the oracle"
            );
            // Every fixpoint row is annotated ⊤ (a derivation exists and
            // the enumeration found it)…
            for row in engine_rows {
                assert_eq!(
                    ann.get(&rule_node(rel, row)),
                    Some(&Annotation::Boolean(true)),
                    "seed {seed}: '{rel}' row {row:?} not boolean-derivable"
                );
            }
            // …and nothing outside the fixpoint is annotated ⊤.
            for (node, value) in &ann {
                if let (PremiseSource::Rule(sym), tuple) = node
                    && sym.as_plain_symbol().name == rel
                {
                    assert_eq!(
                        *value,
                        Annotation::Boolean(oracle_rows.contains(tuple)),
                        "seed {seed}: '{rel}' node {tuple:?} annotation disagrees with the oracle"
                    );
                }
            }
        }
    }
}

// ════════════════════════════════════════════════════════════════════════
// TRIAL 3 — tropical min-cost vs the independent reference.
// ════════════════════════════════════════════════════════════════════════

fn check_tropical_against_reference(seed: u64, unit: bool) {
    let model = gen_positive(seed, true);
    let weights = if unit {
        // All-ones, through the same map machinery.
        gen_weights(&model, seed)
            .into_keys()
            .map(|k| (k, 1))
            .collect()
    } else {
        gen_weights(&model, seed)
    };
    let oracle = naive_eval(&model).expect("oracle accepts");
    let reference = reference_min_costs(&model, &oracle, &weights);
    let weight_fn = engine_weight_fn(&weights);
    let out =
        run_pipeline(&model, "path", 2, generous_ceiling(), &weight_fn).expect("pipeline runs");
    let costs = as_cost_map(
        &solve(Semiring::Tropical, &out.graph, &generous_solver()).expect("solver runs"),
    )
    .expect("tropical annotations");
    for rel in idb_of(&model) {
        for row in &out.rows[rel] {
            let want = reference
                .get(&(rel, row.clone()))
                .copied()
                .unwrap_or_else(|| {
                    panic!("seed {seed}: reference has no cost for derivable {rel}{row:?}")
                });
            assert_eq!(
                costs.get(&rule_node(rel, row)),
                Some(&Cost::Finite(want)),
                "seed {seed}: '{rel}' row {row:?} min-cost disagrees with the reference"
            );
        }
    }
    // Ground facts cost 0.
    for (node, cost) in &costs {
        if matches!(node.0, PremiseSource::Fact(_)) {
            assert_eq!(*cost, Cost::Finite(0), "fact {node:?} must cost 0");
        }
    }
}

#[test]
fn tropical_min_cost_matches_independent_reference_unit_weights() {
    for i in 0..12u64 {
        let seed = Rng::new(i.wrapping_mul(0x517C_C1B7_2722_0A95)).next_u64();
        check_tropical_against_reference(seed, true);
    }
}

#[test]
fn tropical_min_cost_matches_independent_reference_random_weights() {
    for i in 0..12u64 {
        let seed = Rng::new(i.wrapping_mul(0x2545_F491_4F6C_DD1D)).next_u64();
        check_tropical_against_reference(seed, false);
    }
}

// ════════════════════════════════════════════════════════════════════════
// TRIAL 4 — certificates: extract, verify twice (structurally against the
// graph, semantically against the model), and reject corruption.
// ════════════════════════════════════════════════════════════════════════

/// A fixed, non-trivial program: TC over a 4-cycle plus a chord, so
/// `path` has genuinely multi-step cheapest derivations.
fn certificate_model() -> Program {
    let edges: BTreeSet<Tuple> = [(0, 1), (1, 2), (2, 3), (3, 0), (0, 2)]
        .iter()
        .map(|(a, b)| vec![v(*a), v(*b)])
        .map(Tuple::from_vec)
        .collect();
    Program::untimed(
        vec![
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
        ],
        vec![],
        BTreeMap::from([("edge", edges)]),
    )
}

struct CertificateFixture {
    model: Program,
    weights: BTreeMap<(Rel, usize), u64>,
    graph: DerivationGraph<ProvNode>,
    costs: BTreeMap<ProvNode, Cost>,
    target: ProvNode,
    proof: ProofNode<ProvNode>,
}

fn certificate_fixture() -> CertificateFixture {
    let model = certificate_model();
    let weights = gen_weights(&model, 7);
    let weight_fn = engine_weight_fn(&weights);
    let out =
        run_pipeline(&model, "path", 2, generous_ceiling(), &weight_fn).expect("pipeline runs");
    // `weight_fn` borrows `weights`; release the borrow before the fixture
    // takes ownership of `weights` (the checker re-derives weights from it).
    drop(weight_fn);
    let costs = as_cost_map(
        &solve(Semiring::Tropical, &out.graph, &generous_solver()).expect("solver runs"),
    )
    .expect("tropical annotations");
    // The most expensive derivable path row: the deepest certificate.
    let target = out.rows["path"]
        .iter()
        .map(|row| rule_node("path", row))
        .max_by_key(|node| match costs[node] {
            Cost::Finite(c) => c,
            Cost::Infinite => unreachable!("derivable rows are finite"),
        })
        .expect("path is non-empty");
    let proof = extract_min_cost_proof(&out.graph, &costs, &target).expect("certificate extracts");
    CertificateFixture {
        model,
        weights,
        graph: out.graph,
        costs,
        target,
        proof,
    }
}

#[test]
fn certificate_extracts_and_verifies_both_ways() {
    let fx = certificate_fixture();
    let solved = match fx.costs[&fx.target] {
        Cost::Finite(c) => c,
        Cost::Infinite => unreachable!(),
    };
    assert!(solved > 0, "the deepest path row must cost something");
    // Structural verification against the graph.
    let structural = verify_proof(&fx.proof, &fx.graph).expect("structural check passes");
    assert_eq!(structural, solved, "certificate cost = solved cost");
    // Independent semantic verification against the model.
    let per_head = per_head_rules(&fx.model);
    let semantic = verify_model_proof(&fx.proof, &per_head, &fx.model.facts, &fx.weights)
        .expect("independent checker accepts");
    assert_eq!(semantic, solved, "independent cost = solved cost");
    // The proof is genuinely a tree of steps grounding in edge facts.
    assert!(
        matches!(&fx.proof, ProofNode::Step { .. }),
        "path rows are derived, not ground"
    );
}

#[test]
fn corrupted_certificates_are_rejected() {
    let fx = certificate_fixture();
    let per_head = per_head_rules(&fx.model);
    let check_both = |proof: &ProofNode<ProvNode>| -> (bool, bool) {
        (
            verify_proof(proof, &fx.graph).is_ok(),
            verify_model_proof(proof, &per_head, &fx.model.facts, &fx.weights).is_ok(),
        )
    };
    assert_eq!(check_both(&fx.proof), (true, true), "control: intact proof");

    // (a) Lie about the total cost: both checkers reject.
    let mut corrupt = fx.proof.clone();
    if let ProofNode::Step { cost, .. } = &mut corrupt {
        *cost += 1;
    }
    assert_eq!(check_both(&corrupt), (false, false), "cost lie");

    // (b) Swap a ground leaf for a non-fact: both reject.
    let mut corrupt = fx.proof.clone();
    fn first_leaf<K>(p: &mut ProofNode<K>) -> &mut ProofNode<K> {
        // Rust's borrow checker dislikes returning from a loop over
        // children here; recurse on the first premise instead (every
        // Step in a well-formed proof has at least one).
        match p {
            ProofNode::Fact { .. } => p,
            ProofNode::Step { premises, .. } => first_leaf(
                premises
                    .first_mut()
                    .expect("every step in this fixture has a premise"),
            ),
        }
    }
    if let ProofNode::Fact { node } = first_leaf(&mut corrupt) {
        node.1 = Tuple::from_vec(vec![v(96), v(97)]);
    }
    assert_eq!(check_both(&corrupt), (false, false), "forged leaf");

    // (c) Claim the wrong rule: the recursive step relabeled as the base
    // rule (premise count 2 vs 1) — both reject.
    let mut corrupt = fx.proof.clone();
    if let ProofNode::Step { label, .. } = &mut corrupt {
        assert_eq!(
            *label, 1,
            "deepest path row is derived by the recursive rule"
        );
        *label = 0;
    }
    assert_eq!(check_both(&corrupt), (false, false), "wrong rule");

    // (d) Drop a premise: both reject.
    let mut corrupt = fx.proof.clone();
    if let ProofNode::Step { premises, .. } = &mut corrupt {
        premises.pop();
    }
    assert_eq!(check_both(&corrupt), (false, false), "dropped premise");
}

#[test]
fn underivable_targets_refuse_certificate_extraction() {
    let fx = certificate_fixture();
    let ghost = rule_node("path", &Tuple::from_vec(vec![v(40), v(41)]));
    let err = extract_min_cost_proof(&fx.graph, &fx.costs, &ghost).expect_err("must refuse");
    assert!(
        err.downcast_ref::<crate::query::semiring::NoDerivation>()
            .is_some(),
        "typed NoDerivation, got: {err:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════
// TRIAL 5 — determinism: annotations and certificates byte-identical
// across 1/2/4/8 rayon threads.
// ════════════════════════════════════════════════════════════════════════

#[cfg(not(target_arch = "wasm32"))]
fn provenance_fingerprint(seed: u64, threads: usize) -> String {
    at_thread_count(threads, || {
        let model = gen_positive(seed, true);
        let weights = gen_weights(&model, seed);
        let weight_fn = engine_weight_fn(&weights);
        let out =
            run_pipeline(&model, "path", 2, generous_ceiling(), &weight_fn).expect("pipeline runs");
        let bool_ann =
            solve(Semiring::Boolean, &out.graph, &generous_solver()).expect("boolean solves");
        let costs = as_cost_map(
            &solve(Semiring::Tropical, &out.graph, &generous_solver()).expect("tropical solves"),
        )
        .expect("tropical annotations");
        let proof = out.rows["path"].iter().next().map(|row| {
            extract_min_cost_proof(&out.graph, &costs, &rule_node("path", row))
                .expect("certificate extracts")
        });
        format!("{bool_ann:?}\n{costs:?}\n{proof:?}")
    })
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn provenance_is_deterministic_across_thread_counts() {
    for i in 0..3u64 {
        let seed = Rng::new(0xD1CE ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15)).next_u64();
        let baseline = provenance_fingerprint(seed, 1);
        assert!(!baseline.is_empty());
        for threads in [2, 4, 8] {
            assert_eq!(
                provenance_fingerprint(seed, threads),
                baseline,
                "seed {seed}: provenance differs at {threads} threads"
            );
        }
    }
}

// ════════════════════════════════════════════════════════════════════════
// TRIAL 6 — the collapse boundary: aggregated stores ground out.
// ════════════════════════════════════════════════════════════════════════

#[test]
fn aggregation_boundary_collapses_to_ground_facts() {
    // seed(node, val); m(K, min(V)) folded inside recursion along edges;
    // out(K, V) a plain reader above — the PA4 collapse: m's tuples are
    // ground (cost 0), out's derivations are costed above them.
    let model = Program::untimed(
        vec![
            Rule::aggregated(
                "m",
                vec![x(), y()],
                vec![None, named("min")],
                vec![lit("seed", vec![x(), y()], false)],
            ),
            Rule::aggregated(
                "m",
                vec![y(), z()],
                vec![None, named("min")],
                vec![
                    lit("edge", vec![x(), y()], false),
                    lit("m", vec![x(), z()], false),
                ],
            ),
            Rule::plain("out", vec![x(), y()], vec![lit("m", vec![x(), y()], false)]),
        ],
        vec![],
        BTreeMap::from([
            (
                "edge",
                [(0, 1), (1, 2), (2, 0)]
                    .iter()
                    .map(|(a, b)| vec![v(*a), v(*b)])
                    .map(Tuple::from_vec)
                    .collect(),
            ),
            (
                "seed",
                [(0, 5), (1, 9), (2, 3)]
                    .iter()
                    .map(|(a, b)| vec![v(*a), v(*b)])
                    .map(Tuple::from_vec)
                    .collect(),
            ),
        ]),
    );
    let oracle = naive_eval(&model).expect("oracle accepts");
    let out =
        run_pipeline(&model, "out", 2, generous_ceiling(), &unit_weight).expect("pipeline runs");
    assert_eq!(
        format!("{:?}", out.rows["out"]),
        format!("{:?}", oracle["out"]),
        "the reader's fixpoint equals the oracle"
    );
    let costs = as_cost_map(
        &solve(Semiring::Tropical, &out.graph, &generous_solver()).expect("solver runs"),
    )
    .expect("tropical annotations");
    assert!(!out.rows["m"].is_empty());
    for row in &out.rows["m"] {
        // The meet store's tuples enter the graph as ground facts…
        assert!(
            out.graph.facts.contains(&rule_node("m", row)),
            "meet row {row:?} must be a collapse-boundary ground fact"
        );
        assert_eq!(costs[&rule_node("m", row)], Cost::Finite(0));
    }
    for row in &out.rows["out"] {
        // …and the plain rule above them is costed normally.
        assert_eq!(
            costs[&rule_node("out", row)],
            Cost::Finite(1),
            "one unit-weight rule application above the boundary"
        );
    }
}

// ════════════════════════════════════════════════════════════════════════
// TRIAL 7 — the typed refusals.
// ════════════════════════════════════════════════════════════════════════

#[test]
fn unattributed_body_is_refused_typed() {
    // A one-rule program whose body declines to attribute its premises.
    let facts: Arc<BTreeMap<Rel, BTreeSet<Tuple>>> = Arc::new(BTreeMap::from([(
        "edge",
        BTreeSet::from([Tuple::from_vec(vec![v(1), v(2)])]),
    )]));
    let idb: Arc<BTreeSet<Rel>> = Arc::new(BTreeSet::new());
    let body = UnattributedBody(ModelBody::new(
        vec![x(), y()],
        vec![lit("edge", vec![x(), y()], false)],
        facts,
        idb,
    ));
    let entry_set = EvalRuleSet::new(vec![None, None], vec![body]).expect("well-shaped rule set");
    let mut stratum = EvalStratum::default();
    stratum.defs.insert(
        entry_symbol(),
        EvalDefinition::<_, NoFixed>::Rules(entry_set),
    );
    let program = EvalProgram::from_execution_order(vec![stratum]).expect("entry present");
    let budget = generous_budget();
    let (_outcome, stores) = stratified_evaluate_with_stores(
        &program,
        &StoreLifetimes::default(),
        RowLimit::default(),
        &budget,
        None,
    )
    .expect("evaluation itself is fine");
    let err = provenance_graph(&program, &stores, &budget, generous_ceiling(), &unit_weight)
        .expect_err("must refuse");
    let refusal: &ProvenanceUnsupported = err.downcast_ref().expect("typed ProvenanceUnsupported");
    assert_eq!(
        refusal.reason,
        "a rule body does not attribute its premises"
    );
}

#[test]
fn unretained_store_is_refused_typed() {
    // Without retain_all, `path`'s store dies after its last reader's
    // stratum and provenance must refuse — typed, not a silent gap.
    let model = certificate_model();
    let compiled = compile_for(&model, "path", 2, false);
    let budget = generous_budget();
    let (_outcome, stores) = stratified_evaluate_with_stores(
        &compiled.program,
        &compiled.lifetimes,
        RowLimit::default(),
        &budget,
        None,
    )
    .expect("evaluates");
    // The entry consumed `path` in the final stratum, so `path` survives —
    // but the recursive rule also premises `path`, and nothing else does.
    // To exercise the refusal deterministically, drop a store explicitly:
    // the map a caller passes is the contract surface.
    let mut stores = stores;
    stores.remove(&muggle("path"));
    let err = provenance_graph(
        &compiled.program,
        &stores,
        &budget,
        generous_ceiling(),
        &unit_weight,
    )
    .expect_err("must refuse");
    let refusal: &ProvenanceUnsupported = err.downcast_ref().expect("typed ProvenanceUnsupported");
    assert_eq!(
        refusal.reason,
        "a premised store was not retained to the final stratum"
    );
}

#[test]
fn enumeration_ceiling_is_a_typed_refusal() {
    let model = certificate_model();
    let err = run_pipeline(&model, "path", 2, NonZeroU64::new(1).unwrap(), &unit_weight)
        .expect_err("must refuse");
    let refusal: &ProvenanceLimitExceeded =
        err.downcast_ref().expect("typed ProvenanceLimitExceeded");
    assert_eq!(refusal.dimension, "enumerated derivations");
    assert_eq!(refusal.ceiling, 1);
    assert_eq!(refusal.spent, 2);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn enumeration_ceiling_refusal_is_deterministic_across_threads() {
    let render = |threads: usize| {
        at_thread_count(threads, || {
            let model = certificate_model();
            let err = run_pipeline(&model, "path", 2, NonZeroU64::new(1).unwrap(), &unit_weight)
                .expect_err("must refuse");
            let refusal: &ProvenanceLimitExceeded = err.downcast_ref().expect("typed");
            format!("{}|{refusal:?}", err)
        })
    };
    let baseline = render(1);
    for threads in [2, 4, 8] {
        assert_eq!(
            render(threads),
            baseline,
            "refusal differs at {threads} threads"
        );
    }
}

#[test]
fn solver_pass_ceiling_is_a_typed_refusal() {
    // A 5-link chain listed in reverse order: each pass propagates one
    // link, so 5 passes are needed (plus one to observe quiescence).
    let chain: Vec<Derivation<u32>> = (1..=5u32)
        .rev()
        .map(|i| Derivation {
            head: i,
            label: 0,
            weight: NonZeroU64::new(1).unwrap(),
            premises: vec![i - 1],
        })
        .collect();
    let graph = DerivationGraph {
        facts: BTreeSet::from([0u32]),
        derivations: chain,
    };
    let err = solve(
        Semiring::Tropical,
        &graph,
        &SolverBudget::new(NonZeroU32::new(2).unwrap()),
    )
    .expect_err("must refuse");
    let refusal: &ProvenanceLimitExceeded =
        err.downcast_ref().expect("typed ProvenanceLimitExceeded");
    assert_eq!(refusal.dimension, "solver passes");
    assert_eq!(refusal.ceiling, 2);
    // With enough passes the same graph solves exactly.
    let costs = as_cost_map(
        &solve(
            Semiring::Tropical,
            &graph,
            &SolverBudget::new(NonZeroU32::new(6).unwrap()),
        )
        .expect("solves"),
    )
    .expect("tropical annotations");
    for i in 0..=5u32 {
        assert_eq!(costs[&i], Cost::Finite(u64::from(i)));
    }
}

#[test]
fn open_graph_is_refused_by_the_closure_check() {
    // A premise that is neither a fact nor any derivation's head would
    // silently annotate to 0; check_closed turns that into a loud error.
    let graph = DerivationGraph {
        facts: BTreeSet::from([0u32]),
        derivations: vec![Derivation {
            head: 1,
            label: 0,
            weight: NonZeroU64::new(1).unwrap(),
            premises: vec![99],
        }],
    };
    assert!(graph.check_closed().is_err());
}
