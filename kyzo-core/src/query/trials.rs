// Copyright 2023 The Cozo Project Authors.
// Copyright 2026 The KyzoDB Authors.
//
// This Source Code Form is subject to the terms of the Mozilla Public License,
// v. 2.0. If a copy of the MPL was not distributed with this file, You can
// obtain one at https://mozilla.org/MPL/2.0/.

//! Trials: two README claims, demonstrated at scale against the sealed oracle.
//!
//! This is a *test-only* harness. It touches no engine source — it consumes
//! the `pub(crate)` evaluation seams ([`stratified_evaluate`], [`Budget`],
//! [`WitnessTable`], the [`RuleBody`]/[`FixedRuleEval`] traits and the
//! [`EvalProgram`] tier) and the sealed reference oracle
//! ([`crate::query::laws`]) exactly as an outside caller would.
//!
//! Two capabilities, mapped to two README lines under *The engine keeps its
//! word*:
//!
//! - **"Determinism as a law."** A seed-reproducible generator (splitmix64,
//!   the [`crate::storage::sim::SimRng`] pattern — no ambient entropy)
//!   emits large stratified programs mixing linear and self-join recursion,
//!   stratified negation, normal aggregation, and meet aggregation in every
//!   positional layout the landed [`crate::query::eval::EvalRuleSet`] now
//!   accepts — suffix, position-0, and interleaved (grouping columns between
//!   meet columns), the capability that replaced the retired `MeetNotSuffix`
//!   refusal — plus mutual recursion, a non-self-healing two-delta join, and
//!   opaque fixed rules, over generated fact sets in the thousands. Per seed,
//!   under a **finite**
//!   [`Budget`] (an unbudgeted random recursive program can legitimately
//!   explode; a ceiling turns explosion into a *typed refusal*): the answer
//!   is differentially checked against the oracle, and the answer set, the
//!   witness table, and — for deliberately budget-exceeding variants — the
//!   refusal are all asserted byte-identical across 1/2/4/8 rayon threads.
//!
//! - **"Answers that show their work."** A recursive query (transitive
//!   closure joined onward) evaluated with first-witness recording on; the
//!   proof tree of a chosen derived fact reconstructed from the witness
//!   table down to stored ground facts; and an **independent** checker
//!   (below, importing no evaluator symbol) that verifies every step is a
//!   valid rule instantiation over ground-or-previously-proven premises. A
//!   negative control corrupts a step and watches the checker reject it.
//!
//! ## What this harness does and does not exercise
//!
//! The [`RuleBody`] seam is *post-stratification*: it is the surface the
//! compile tier's relational-algebra plans plug into. Driving it directly
//! (as the landed `eval.rs` differential tests do) exercises the whole
//! semi-naive stratified fixpoint — delta discipline, meet folding inside
//! recursion, normal-aggregation and negation on stratum boundaries, fixed
//! rules run once, the admission seam, and the budget check sites — against
//! the oracle. It does **not** run the magic-set *demand rewriter*: that is
//! a compile-tier transform reached only from a parsed `InputProgram`, and
//! with no script front door wired yet there is no from-scratch program
//! builder to feed it. **This is a stated open gap: the demand rewriter has
//! no end-to-end differential anywhere today** (`compile.rs` and `magic.rs`
//! tests are structural — they never evaluate a rewritten program against the
//! oracle). Closing it is scheduled at the session tier: the `runtime/db.rs`
//! wave is to carry a demand-transform differential once a program can be
//! parsed and run end to end. Boundary stated, not smuggled.

#![cfg(test)]

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::ops::ControlFlow;
use std::sync::Arc;

use miette::Result;

use crate::data::aggr::parse_aggr;
use crate::data::program::{MagicSymbol, StoreLifetimes};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::tuple::Tuple;
use crate::data::value::DataValue;
use crate::query::eval::{
    AtomOccurrence, Budget, BudgetDimension, EvalDefinition, EvalProgram, EvalRuleSet, EvalStratum,
    FixedRuleEval, LimitExceeded, Premises, RowLimit, RuleBody, Witness, WitnessTable,
    stratified_evaluate,
};
use crate::query::laws::{FixedRule, HeadAggr, Literal, Program, Rel, Rule, Term, naive_eval};
use crate::query::levels::EpochStore;
use crate::query::temp_store::{RegularTempStore, TupleInIter};

// ════════════════════════════════════════════════════════════════════════
// Seeded RNG — the splitmix64 of storage/sim.rs, transcribed so the campaign
// depends on one u64 seed and nothing else (no ambient entropy, replayable).
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
    fn one_of<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len() as u64) as usize]
    }
}

// ════════════════════════════════════════════════════════════════════════
// The oracle-model RuleBody harness.
//
// A faithful, self-contained reimplementation of the seam the landed eval
// tests drive: one `laws::Rule` body evaluated by naive nested-loop
// unification against the live EpochStore map (positives in order, then
// negatives; IDB literals read totals, or the delta of `delta_from`). It is
// the stand-in for a compiled RA plan. It lives here rather than being
// imported because eval.rs's copy is private to eval.rs's own test module,
// and this patch does not touch eval.rs.
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

type Bindings = BTreeMap<&'static str, DataValue>;

fn unify(args: &[Term], tuple: &[DataValue], bound: &Bindings) -> Option<Bindings> {
    if args.len() != tuple.len() {
        return None;
    }
    let mut out = bound.clone();
    for (t, val) in args.iter().zip(tuple) {
        match t {
            Term::Const(c) => {
                if c != val {
                    return None;
                }
            }
            Term::Var(name) => match out.get(name) {
                Some(existing) if existing != val => return None,
                Some(_) => {}
                None => {
                    out.insert(name, val.clone());
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
            Term::Var(var) => bound[var].clone(),
        })
        .collect()
}

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
                        if let Some(b) = unify(&l.args, row, bound) {
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
            if f(Cow::Owned(head), arg)?.is_break() {
                return Ok(());
            }
        }
        Ok(())
    }

    fn contained_rules(&self) -> &BTreeMap<AtomOccurrence, MagicSymbol> {
        &self.contained
    }
}

struct ModelFixed {
    inputs: Vec<Rel>,
    eval: fn(&[BTreeSet<Tuple>]) -> BTreeSet<Tuple>,
    facts: Arc<BTreeMap<Rel, BTreeSet<Tuple>>>,
    idb: Arc<BTreeSet<Rel>>,
}

impl FixedRuleEval for ModelFixed {
    fn run(
        &self,
        stores: &BTreeMap<MagicSymbol, EpochStore>,
        out: &mut RegularTempStore,
        _budget: &Budget,
        _baseline: u64,
    ) -> Result<()> {
        let inputs: Vec<BTreeSet<Tuple>> = self
            .inputs
            .iter()
            .map(|rel| {
                if self.idb.contains(rel) {
                    stores
                        .get(&muggle(rel))
                        .expect("harness: fixed input store present")
                        .all_iter()
                        .map(TupleInIter::into_tuple)
                        .collect()
                } else {
                    self.facts.get(rel).cloned().unwrap_or_default()
                }
            })
            .collect();
        for row in (self.eval)(&inputs) {
            out.put(row);
        }
        Ok(())
    }
}

// ── stratum assignment: the oracle's Bellman-Ford, transcribed ──────────
//
// The oracle is sealed (its `strata` is private), and this scaffolding must
// not lean on the judge's internals. Any *valid* stratification yields the
// oracle's fixpoint, so this recomputes one from the same edge rules.

struct HeadClass {
    has_aggr: bool,
    is_meet: bool,
}

fn head_classes(program: &Program) -> BTreeMap<Rel, HeadClass> {
    let mut per_head: BTreeMap<Rel, Vec<&Rule>> = BTreeMap::new();
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
                        Some((aggregation, _)) => aggregation.is_meet(),
                    })
                });
            (rel, HeadClass { has_aggr, is_meet })
        })
        .collect()
}

fn dependency_edges(program: &Program) -> Vec<(Rel, Rel, bool)> {
    let classes = head_classes(program);
    let fixed_heads: BTreeSet<Rel> = program.fixed.iter().map(|f| f.head_rel).collect();
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
                l.negated || fixed_heads.contains(l.rel) || is_meet(l.rel)
            };
            edges.push((head, l.rel, forcing));
        }
    }
    for f in &program.fixed {
        for dep in &f.inputs {
            edges.push((f.head_rel, *dep, true));
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
    for f in &program.fixed {
        s.insert(f.head_rel, 0);
        for i in &f.inputs {
            s.insert(i, 0);
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
    program: EvalProgram<ModelBody, ModelFixed>,
    lifetimes: StoreLifetimes,
}

/// Compile the oracle model for evaluation with `target` as the entry rule
/// `?[vars…] := target[vars…]`.
fn compile_for(
    model: &Program,
    target: Rel,
    target_arity: usize,
    fixed_arities: &BTreeMap<Rel, usize>,
) -> Compiled {
    let idb: Arc<BTreeSet<Rel>> = Arc::new(
        model
            .rules
            .iter()
            .map(|r| r.head_rel)
            .chain(model.fixed.iter().map(|f| f.head_rel))
            .collect(),
    );
    for rel in idb.iter() {
        assert!(
            !model.facts.contains_key(rel),
            "harness limitation: facts under rule head {rel}"
        );
    }
    let facts = Arc::new(model.facts.clone());
    let strata_map = strata_of(model);
    let entry_stratum = strata_map.values().copied().max().unwrap_or(0) + 1;

    let mut strata: Vec<EvalStratum<ModelBody, ModelFixed>> = (0..=entry_stratum)
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
        let rule_set = EvalRuleSet::new(rules[0].aggr.clone(), bodies).expect("well-shaped set");
        strata[stratum]
            .defs
            .insert(muggle(head), EvalDefinition::Rules(rule_set));
    }
    for f in &model.fixed {
        let stratum = strata_map[f.head_rel];
        for input in &f.inputs {
            if idb.contains(input) {
                lifetimes.note_use(muggle(input), stratum);
            }
        }
        strata[stratum].defs.insert(
            muggle(f.head_rel),
            EvalDefinition::Fixed {
                arity: fixed_arities.get(f.head_rel).copied().unwrap_or_else(|| {
                    panic!("fixed head {} missing from fixed_arities", f.head_rel)
                }),
                rule: ModelFixed {
                    inputs: f.inputs.clone(),
                    eval: f.eval,
                    facts: facts.clone(),
                    idb: idb.clone(),
                },
            },
        );
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

/// Relation arities from the MODEL alone (never from oracle output — an
/// oracle-empty relation must still carry a real arity, or an over-derivation
/// into it would be an invisible vacuous pass).
fn model_arities(model: &Program) -> BTreeMap<Rel, usize> {
    fn note(arities: &mut BTreeMap<Rel, usize>, rel: Rel, n: usize) {
        match arities.entry(rel) {
            std::collections::btree_map::Entry::Vacant(e) => {
                e.insert(n);
            }
            std::collections::btree_map::Entry::Occupied(o) => {
                assert_eq!(*o.get(), n, "model uses '{rel}' at two arities");
            }
        }
    }
    let mut arities = BTreeMap::new();
    for r in &model.rules {
        note(&mut arities, r.head_rel, r.head_args.len());
        for l in &r.body {
            note(&mut arities, l.rel, l.args.len());
        }
    }
    for (rel, rows) in &model.facts {
        if let Some(t) = rows.first() {
            note(&mut arities, rel, t.len());
        }
    }
    arities
}

fn fixed_arities_of(model: &Program, arities: &BTreeMap<Rel, usize>) -> BTreeMap<Rel, usize> {
    model
        .fixed
        .iter()
        .map(|f| (f.head_rel, arities[f.head_rel]))
        .collect()
}

fn idb_of(model: &Program) -> BTreeSet<Rel> {
    model
        .rules
        .iter()
        .map(|r| r.head_rel)
        .chain(model.fixed.iter().map(|f| f.head_rel))
        .collect()
}

/// Evaluate `target` through the real engine, returning its rows.
fn real_eval(
    model: &Program,
    target: Rel,
    target_arity: usize,
    fixed_arities: &BTreeMap<Rel, usize>,
    budget: &Budget,
) -> Result<BTreeSet<Tuple>> {
    let compiled = compile_for(model, target, target_arity, fixed_arities);
    let outcome = stratified_evaluate(
        &compiled.program,
        &compiled.lifetimes,
        RowLimit::default(),
        budget,
        None,
    )?;
    Ok(outcome
        .store
        .all_iter()
        .map(TupleInIter::into_tuple)
        .collect())
}

fn generous_budget() -> Budget {
    Budget::new(NonZeroU32::new(10_000).unwrap())
}

// ════════════════════════════════════════════════════════════════════════
// Term / literal builders for the generated programs.
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

// ════════════════════════════════════════════════════════════════════════
// The generator: one u64 seed → one large, stratified, safe program.
//
// Everything is safe-by-construction: readers that negate, normal-aggregate,
// read a meet relation, or consume a fixed rule sit in strictly higher
// strata than the recursive cluster they observe, so both the oracle and the
// real compiler accept, and the program terminates (bounded lattices, bounded
// closures, single-pass folds).
// ════════════════════════════════════════════════════════════════════════

/// The five meet lattices, and the value shape each seed row carries.
const MEET_OPS: [&str; 5] = ["min", "max", "and", "or", "union"];

#[derive(Debug, Clone)]
struct GenParams {
    n_nodes: i64,
    self_join: bool,
    mutual: bool,
    two_dep: bool,
    /// A non-self-healing two-delta join `j(x,z) :- qa(x,y), qb(y,z)` over the
    /// separately-derived recursive pair qa/qb, with no recursive repair rule.
    /// This discriminates a delta-discipline mutant that threads only the
    /// first contained store's delta (which `pj`'s repair rule masks).
    cross_join: bool,
    negation: bool,
    normal_aggr: bool,
    bulk_aggr: bool,
    fixed_rule: bool,
    meet_op: &'static str,
    /// Emit the 2-column meet `m` with its aggregated column at position 0
    /// (grouping node at position 1) — a non-suffix layout, exercising the
    /// positional MeetAggrStore that replaced the retired `MeetNotSuffix`
    /// refusal. `false` keeps the classic suffix layout.
    meet_pos0: bool,
    /// Also emit a 3-column *interleaved* meet `mi(min(Lo), K, max(Hi))`: two
    /// meet columns split apart by a grouping column at position 1.
    meet_interleaved: bool,
}

fn gen_params(rng: &mut Rng) -> GenParams {
    GenParams {
        n_nodes: rng.range(6, 15),
        self_join: rng.chance(1, 2),
        mutual: rng.chance(1, 2),
        two_dep: rng.chance(2, 5),
        cross_join: rng.chance(3, 5),
        negation: rng.chance(3, 5),
        normal_aggr: rng.chance(3, 5),
        bulk_aggr: rng.chance(4, 5),
        fixed_rule: rng.chance(1, 2),
        meet_op: rng.one_of(&MEET_OPS),
        meet_pos0: rng.chance(1, 2),
        meet_interleaved: rng.chance(1, 2),
    }
}

fn meet_value(rng: &mut Rng, op: &str) -> DataValue {
    match op {
        "and" | "or" => DataValue::from(rng.chance(1, 2)),
        "union" => {
            let n = rng.below(3);
            let set: BTreeSet<DataValue> = (0..n).map(|_| v(rng.range(0, 4))).collect();
            DataValue::Set(set)
        }
        _ => v(rng.range(-10, 10)),
    }
}

/// A generated program plus the metadata the campaign and provenance code
/// need: the entry relation to evaluate and the facts (for the checker).
struct Generated {
    program: Program,
    entry: Rel,
    entry_arity: usize,
}

fn generate(seed: u64) -> Generated {
    let mut rng = Rng::new(seed);
    let p = gen_params(&mut rng);
    let n = p.n_nodes;

    // ── EDB, sized into the thousands via `item` ──
    let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();

    // A moderate graph: closure stays oracle-tractable (≤ n²).
    let n_edges = rng.below((n * 3) as u64) as i64 + 1;
    let edges: BTreeSet<Tuple> = (0..n_edges)
        .map(|_| vec![v(rng.range(0, n)), v(rng.range(0, n))])
        .collect();
    facts.insert("edge", edges);
    facts.insert("node", (0..n).map(|i| vec![v(i)]).collect());

    // Meet seeds, typed to the chosen lattice.
    let n_seeds = rng.below(n as u64) as i64 + 1;
    let seeds: BTreeSet<Tuple> = (0..n_seeds)
        .map(|_| vec![v(rng.range(0, n)), meet_value(&mut rng, p.meet_op)])
        .collect();
    facts.insert("seed", seeds);

    // The bulk relation: hundreds-to-thousands of rows over many keys.
    let n_items = rng.range(800, 3000);
    let n_keys = rng.range(20, 100);
    let items: BTreeSet<Tuple> = (0..n_items)
        .map(|_| vec![v(rng.range(0, n_keys)), v(rng.range(0, 50))])
        .collect();
    facts.insert("item", items);

    let mut rules: Vec<Rule> = Vec::new();

    // ── stratum 0: the recursive cluster ──
    // path: transitive closure, linear or self-join.
    rules.push(Rule::plain(
        "path",
        vec![x(), y()],
        vec![lit("edge", vec![x(), y()], false)],
    ));
    if p.self_join {
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

    // Meet recursion `m` over `seed(node, val)`, propagating the folded value
    // along edges. The same fixpoint in two head layouts, chosen by seed:
    //   suffix — head m(group, agg), aggr [None, meet]  (classic)
    //   pos0   — head m(agg, group), aggr [meet, None]  (non-suffix; the
    //            positional MeetAggrStore that replaced MeetNotSuffix)
    if p.meet_pos0 {
        rules.push(Rule::aggregated(
            "m",
            vec![y(), x()],
            vec![named(p.meet_op), None],
            vec![lit("seed", vec![x(), y()], false)],
        ));
        rules.push(Rule::aggregated(
            "m",
            vec![z(), y()],
            vec![named(p.meet_op), None],
            vec![
                lit("edge", vec![x(), y()], false),
                lit("m", vec![z(), x()], false),
            ],
        ));
    } else {
        rules.push(Rule::aggregated(
            "m",
            vec![x(), y()],
            vec![None, named(p.meet_op)],
            vec![lit("seed", vec![x(), y()], false)],
        ));
        rules.push(Rule::aggregated(
            "m",
            vec![y(), z()],
            vec![None, named(p.meet_op)],
            vec![
                lit("edge", vec![x(), y()], false),
                lit("m", vec![x(), z()], false),
            ],
        ));
    }

    // Interleaved meet `mi(min(Lo), K, max(Hi))`: two meet columns split by a
    // grouping column at position 1, seeded and relaxed along edges. Each hop
    // carries the source group's folded (min, max) to the target node.
    if p.meet_interleaved {
        let n_s3 = rng.below(n as u64) as i64 + 1;
        let seed3: BTreeSet<Tuple> = (0..n_s3)
            .map(|_| {
                vec![
                    v(rng.range(0, n)),
                    v(rng.range(-10, 10)),
                    v(rng.range(-10, 10)),
                ]
            })
            .collect();
        facts.insert("seed3", seed3);
        let (lo, k, hi, s, t) = (
            Term::Var("Lo"),
            Term::Var("K"),
            Term::Var("Hi"),
            Term::Var("S"),
            Term::Var("T"),
        );
        rules.push(Rule::aggregated(
            "mi",
            vec![lo.clone(), k.clone(), hi.clone()],
            vec![named("min"), None, named("max")],
            vec![lit("seed3", vec![k.clone(), lo.clone(), hi.clone()], false)],
        ));
        rules.push(Rule::aggregated(
            "mi",
            vec![lo.clone(), t.clone(), hi.clone()],
            vec![named("min"), None, named("max")],
            vec![
                lit("edge", vec![s.clone(), t], false),
                lit("mi", vec![lo, s, hi], false),
            ],
        ));
    }

    if p.mutual || p.two_dep || p.cross_join {
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
    }
    if p.two_dep {
        // pj joins TWO delta-carrying stores (path and qa change together).
        rules.push(Rule::plain(
            "pj",
            vec![x(), z()],
            vec![
                lit("path", vec![x(), y()], false),
                lit("qa", vec![y(), z()], false),
            ],
        ));
        rules.push(Rule::plain(
            "pj",
            vec![x(), z()],
            vec![
                lit("pj", vec![x(), y()], false),
                lit("qa", vec![y(), z()], false),
            ],
        ));
    }
    if p.cross_join {
        // A single, non-recursive rule joining TWO separately-derived
        // recursive stores (qa and qb both carry deltas across epochs). It
        // has no recursive repair rule, so a delta-discipline mutant that
        // threads only the first contained store's delta silently drops the
        // tuples that only `total(qa) × delta(qb)` would produce — a wrong
        // answer the differential catches. (Unlike `pj`, whose repair rule
        // re-derives and masks the same mutant.)
        rules.push(Rule::plain(
            "j",
            vec![x(), z()],
            vec![
                lit("qa", vec![x(), y()], false),
                lit("qb", vec![y(), z()], false),
            ],
        ));
    }

    // ── stratum ≥ 1: readers above the cluster ──
    // out reads the completed meet relation.
    rules.push(Rule::plain(
        "out",
        vec![x(), y()],
        vec![lit("m", vec![x(), y()], false)],
    ));
    if p.negation {
        rules.push(Rule::plain(
            "unreachable",
            vec![x(), y()],
            vec![
                lit("node", vec![x()], false),
                lit("node", vec![y()], false),
                lit("path", vec![x(), y()], true),
            ],
        ));
    }
    if p.normal_aggr {
        rules.push(Rule::aggregated(
            "reach_count",
            vec![x(), y()],
            vec![None, named("count")],
            vec![lit("path", vec![x(), y()], false)],
        ));
    }
    if p.bulk_aggr {
        rules.push(Rule::aggregated(
            "bulk_sum",
            vec![x(), y()],
            vec![None, named("sum")],
            vec![lit("item", vec![x(), y()], false)],
        ));
    }

    let mut fixed = Vec::new();
    if p.fixed_rule {
        // Opaque endpoints of the completed closure, and a reader above it.
        fixed.push(FixedRule {
            head_rel: "fx",
            inputs: vec!["path"],
            eval: fixed_endpoints,
        });
        rules.push(Rule::plain(
            "fr",
            vec![x(), y()],
            vec![lit("fx", vec![x()], false), lit("node", vec![y()], false)],
        ));
    }

    let program = Program::untimed(rules, fixed, facts);
    Generated {
        program,
        entry: "path",
        entry_arity: 2,
    }
}

/// A fixed rule: the set of endpoints appearing in a binary relation.
fn fixed_endpoints(inputs: &[BTreeSet<Tuple>]) -> BTreeSet<Tuple> {
    let mut out = BTreeSet::new();
    for t in &inputs[0] {
        if t.len() == 2 {
            out.insert(vec![t[0].clone()]);
            out.insert(vec![t[1].clone()]);
        }
    }
    out
}

// ════════════════════════════════════════════════════════════════════════
// CAPABILITY 1 — the determinism campaign.
// ════════════════════════════════════════════════════════════════════════

#[cfg(not(target_arch = "wasm32"))]
fn at_thread_count<T: Send>(threads: usize, f: impl FnOnce() -> T + Send) -> T {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .expect("thread pool")
        .install(|| {
            // Guard: the determinism claim is only meaningful if the pool has
            // the width we asked for (a 1-thread "8-thread" run proves nothing).
            assert_eq!(
                rayon::current_num_threads(),
                threads,
                "rayon pool width mismatch"
            );
            f()
        })
}

/// One admitted tuple, rendered so witness tables compare byte-for-byte
/// across runs and thread counts.
fn render_witnesses(table: &WitnessTable) -> Vec<String> {
    table.entries().iter().map(|w| format!("{w:?}")).collect()
}

/// The differential: every IDB relation of the generated model, through the
/// real semi-naive engine, must equal the sealed oracle. Returns the first
/// disagreement (relation name), if any.
fn differential(model: &Program) -> Option<String> {
    let oracle_db = match naive_eval(model) {
        Ok(db) => db,
        Err(rej) => return Some(format!("oracle refused a generated program: {rej:?}")),
    };
    let arities = model_arities(model);
    let fixed_arities = fixed_arities_of(model, &arities);
    for rel in idb_of(model) {
        let oracle_rows = oracle_db.get(rel).cloned().unwrap_or_default();
        let arity = arities[rel];
        let real_rows = match real_eval(model, rel, arity, &fixed_arities, &generous_budget()) {
            Ok(rows) => rows,
            Err(e) => return Some(format!("real eval failed for '{rel}': {e}")),
        };
        if real_rows != oracle_rows {
            return Some(format!(
                "'{rel}': real {} rows vs oracle {} rows",
                real_rows.len(),
                oracle_rows.len()
            ));
        }
    }
    None
}

/// Evaluate the entry with witnesses on, at one thread count.
#[cfg(not(target_arch = "wasm32"))]
fn eval_fingerprint(g: &Generated, threads: usize) -> (BTreeSet<Tuple>, Vec<String>) {
    at_thread_count(threads, || {
        let arities = model_arities(&g.program);
        let fixed_arities = fixed_arities_of(&g.program, &arities);
        let compiled = compile_for(&g.program, g.entry, g.entry_arity, &fixed_arities);
        let mut table = WitnessTable::default();
        let outcome = stratified_evaluate(
            &compiled.program,
            &compiled.lifetimes,
            RowLimit::default(),
            &generous_budget(),
            Some(&mut table),
        )
        .expect("evaluates");
        let rows = outcome
            .store
            .all_iter()
            .map(TupleInIter::into_tuple)
            .collect();
        (rows, render_witnesses(&table))
    })
}

/// A budget-exceeding refusal, rendered for cross-thread comparison.
#[cfg(not(target_arch = "wasm32"))]
fn refusal_fingerprint(
    g: &Generated,
    budget: &Budget,
    threads: usize,
) -> (String, BudgetDimension, u64, u64) {
    at_thread_count(threads, || {
        let arities = model_arities(&g.program);
        let fixed_arities = fixed_arities_of(&g.program, &arities);
        let compiled = compile_for(&g.program, g.entry, g.entry_arity, &fixed_arities);
        let err = stratified_evaluate(
            &compiled.program,
            &compiled.lifetimes,
            RowLimit::default(),
            budget,
            None,
        )
        .expect_err("must refuse");
        let refusal: &LimitExceeded = err.downcast_ref().expect("typed LimitExceeded");
        (
            err.to_string(),
            refusal.dimension,
            refusal.spent,
            refusal.ceiling,
        )
    })
}

/// Run the full battery for one seed. `Ok(())` means every claim held; the
/// `Err` string names the failing check (a campaign pins it against its seed).
#[cfg(not(target_arch = "wasm32"))]
fn run_seed(seed: u64) -> std::result::Result<(), String> {
    let g = generate(seed);

    // (1) correctness: differential against the oracle.
    if let Some(disagreement) = differential(&g.program) {
        return Err(format!("differential: {disagreement}"));
    }

    // (2) determinism of answers + witnesses across thread counts.
    let baseline = eval_fingerprint(&g, 1);
    // Guard: an empty witness table would make the cross-thread witness
    // comparison vacuous. Every non-empty program records admissions.
    if baseline.1.is_empty() {
        return Err("baseline witness table is empty (recording path not exercised)".into());
    }
    for threads in [2, 4, 8] {
        let got = eval_fingerprint(&g, threads);
        if got.0 != baseline.0 {
            return Err(format!("result set differs at {threads} threads"));
        }
        if got.1 != baseline.1 {
            return Err(format!("witness table differs at {threads} threads"));
        }
    }

    // (3) determinism of refusals across thread counts, for two deliberately
    //     budget-exceeding variants: the epoch ceiling and the derived-tuple
    //     ceiling. `epoch_ceiling = 1` refuses every deriving stratum (two
    //     epochs are needed to observe convergence); a low derived ceiling
    //     refuses at the first barrier that crosses it. Both are barrier-only
    //     deterministic dimensions, so the spend is exact.
    let epoch_budget = Budget::new(NonZeroU32::new(1).unwrap());
    check_refusal(&g, &epoch_budget, BudgetDimension::Epochs)?;

    // A ceiling of 1 is crossed at the first barrier by every non-empty
    // program (the recursive cluster admits `path` ≥ 1 and `m` ≥ 1), so the
    // refusal fires deterministically with an exact spend.
    let derived_budget = generous_budget().with_derived_tuple_ceiling(1);
    check_refusal(&g, &derived_budget, BudgetDimension::DerivedTuples)?;

    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn check_refusal(
    g: &Generated,
    budget: &Budget,
    expect: BudgetDimension,
) -> std::result::Result<(), String> {
    let baseline = refusal_fingerprint(g, budget, 1);
    if baseline.1 != expect {
        return Err(format!("expected {expect:?} refusal, got {:?}", baseline.1));
    }
    for threads in [2, 4, 8] {
        let got = refusal_fingerprint(g, budget, threads);
        if got != baseline {
            return Err(format!(
                "{expect:?} refusal differs at {threads} threads: {got:?} vs {baseline:?}"
            ));
        }
    }
    Ok(())
}

/// How many seeds to sweep. Bounded by default (seconds); the campaign run
/// scales it up via the environment (the `PROPTEST_CASES` pattern).
fn seed_count() -> u64 {
    std::env::var("KYZO_TRIALS_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(24)
}

/// The base seed, so a campaign can walk a different region of the space.
fn seed_base() -> u64 {
    std::env::var("KYZO_TRIALS_BASE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn determinism_campaign() {
    let base = seed_base();
    let count = seed_count();
    let mut failures: Vec<(u64, String)> = Vec::new();
    for i in 0..count {
        // splitmix64 the index so consecutive seeds are unrelated.
        let seed = Rng::new(base ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15)).next_u64();
        if let Err(f) = run_seed(seed) {
            failures.push((seed, format!("{f:?}")));
        }
    }
    assert!(
        failures.is_empty(),
        "determinism campaign FINDINGS ({} of {count}): {failures:?}",
        failures.len()
    );
}

// Regression pins for seeds a campaign has surfaced go here, each as a named
// test asserting `run_seed(SEED).is_ok()`. None to date.

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
            Proof::Ground { rel, tuple } | Proof::Step { rel, tuple, .. } => (rel, tuple),
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
            .entry(rule.head_rel)
            .or_default()
            .push(rule.clone());
    }
    per_head
}

/// Reconstruct the proof of `(rel, tuple)` from the witness table. Uses the
/// evaluator's output (the witnesses) — the independent checker below does
/// not. Returns `None` at a boundary the first-witness table cannot expand
/// (a derivation-less admission: a normal-aggregation fold, a fixed-rule
/// output, or the meet identity row), documented on `WitnessTable`.
fn reconstruct(
    rel: Rel,
    tuple: &Tuple,
    witnesses: &BTreeMap<(String, Tuple), Witness>,
    per_head: &BTreeMap<Rel, Vec<Rule>>,
    idb: &BTreeSet<Rel>,
) -> Option<Proof> {
    if !idb.contains(rel) {
        return Some(Proof::Ground {
            rel,
            tuple: tuple.clone(),
        });
    }
    let w = witnesses.get(&(rel.to_string(), tuple.clone()))?;
    let (rule_idx, premise_rows) = w.derivation.as_ref()?;
    let rule = &per_head[rel][*rule_idx];
    let positives: Vec<&Literal> = rule.body.iter().filter(|l| !l.negated).collect();
    // The witness records exactly one premise row per positive literal, in
    // body order — the order `ModelBody` grounds them.
    if positives.len() != premise_rows.len() {
        return None;
    }
    let mut premises = Vec::new();
    for (l, row) in positives.iter().zip(premise_rows) {
        premises.push(reconstruct(l.rel, row, witnesses, per_head, idb)?);
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
// This function imports NO evaluator symbol: only the model (`Rule`,
// `Literal`, `Term`) and plain data. It re-derives each step's binding from
// scratch, so a corrupted proof cannot pass by echoing eval's own reasoning.
// Its inputs are the rules (grouped per head), the ground facts, and the
// proof; nothing else.

fn check_unify(
    args: &[Term],
    tuple: &[DataValue],
    bound: &mut BTreeMap<&'static str, DataValue>,
) -> bool {
    if args.len() != tuple.len() {
        return false;
    }
    for (t, val) in args.iter().zip(tuple) {
        match t {
            Term::Const(c) => {
                if c != val {
                    return false;
                }
            }
            Term::Var(name) => match bound.get(name) {
                Some(existing) if existing != val => return false,
                Some(_) => {}
                None => {
                    bound.insert(name, val.clone());
                }
            },
        }
    }
    true
}

/// Verify a proof tree. `Ok(())` iff every leaf is a genuine ground fact and
/// every step is a valid instantiation of the named rule whose positive
/// premises are exactly the child tuples. Rules with a negated premise are a
/// documented boundary (a closed-world non-derivability check is not carried
/// in the proof) — the checker refuses to bless them rather than pretend.
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
            if rule.body.iter().any(|l| l.negated) {
                return Err(format!(
                    "boundary: rule for '{rel}' has a negated premise, not \
                     independently checkable from a proof tree"
                ));
            }
            let positives: Vec<&Literal> = rule.body.iter().filter(|l| !l.negated).collect();
            if positives.len() != premises.len() {
                return Err(format!(
                    "'{rel}': {} premises for {} positive body literals",
                    premises.len(),
                    positives.len()
                ));
            }
            // One binding must satisfy the head and every positive premise.
            let mut bound: BTreeMap<&'static str, DataValue> = BTreeMap::new();
            if !check_unify(&rule.head_args, tuple, &mut bound) {
                return Err(format!(
                    "head of rule {rule_idx} does not ground to {tuple:?}"
                ));
            }
            for (l, child) in positives.iter().zip(premises) {
                let (crel, ctuple) = child.head();
                if crel != l.rel {
                    return Err(format!(
                        "premise relation '{crel}' ≠ body literal '{}'",
                        l.rel
                    ));
                }
                if !check_unify(&l.args, ctuple, &mut bound) {
                    return Err(format!(
                        "premise {crel}{ctuple:?} inconsistent with binding"
                    ));
                }
            }
            // Premises independently valid.
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
        "edge",
        [(1, 2), (2, 3), (3, 4), (2, 5)]
            .iter()
            .map(|(a, b)| vec![v(*a), v(*b)])
            .collect(),
    );
    facts.insert(
        "tag",
        [(4, 100), (5, 200)]
            .iter()
            .map(|(a, b)| vec![v(*a), v(*b)])
            .collect(),
    );
    let rules = vec![
        // path: transitive closure of edge (linear recursion).
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
        // labeled: join the closure onward to a tag.
        Rule::plain(
            "labeled",
            vec![x(), z()],
            vec![
                lit("path", vec![x(), y()], false),
                lit("tag", vec![y(), z()], false),
            ],
        ),
    ];
    (Program::untimed(rules, vec![], facts), "labeled")
}

/// Evaluate the fixture with recording on and return (entry rows, witness
/// index, per-head rules, idb).
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
    let compiled = compile_for(model, entry, entry_arity, &fixed_arities);
    let mut table = WitnessTable::default();
    let outcome = stratified_evaluate(
        &compiled.program,
        &compiled.lifetimes,
        RowLimit::default(),
        &generous_budget(),
        Some(&mut table),
    )
    .expect("evaluates");
    let rows = outcome
        .store
        .all_iter()
        .map(TupleInIter::into_tuple)
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
    let (rows, witnesses, per_head, idb) = run_with_witnesses(&model, entry, 2);
    assert!(!rows.is_empty(), "the fixture derives facts");

    // Every entry fact reconstructs, and the independent checker blesses it.
    for tuple in &rows {
        let proof = reconstruct(entry, tuple, &witnesses, &per_head, &idb)
            .unwrap_or_else(|| panic!("reconstruct {entry}{tuple:?}"));
        assert_eq!(proof.head(), (entry, tuple));
        verify(&proof, &per_head, &model.facts)
            .unwrap_or_else(|e| panic!("checker rejected an honest proof of {tuple:?}: {e}"));
        // The proof must bottom out only in genuine ground facts.
        assert!(all_leaves_ground(&proof, &model.facts));
    }

    // And every *intermediate* derived fact (the closure itself) too.
    let path_rows = real_eval(&model, "path", 2, &BTreeMap::new(), &generous_budget()).unwrap();
    for tuple in &path_rows {
        let proof = reconstruct("path", tuple, &witnesses, &per_head, &idb)
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
    let (rows, witnesses, per_head, idb) = run_with_witnesses(&model, entry, 2);

    // Take an honest, deep proof (a labeled fact whose path premise is itself
    // derived, so there is an interior step to corrupt).
    let target = rows
        .iter()
        .find(|t| {
            let proof = reconstruct(entry, t, &witnesses, &per_head, &idb).unwrap();
            proof_depth(&proof) >= 3
        })
        .expect("a multi-step labeled fact exists");
    let honest = reconstruct(entry, target, &witnesses, &per_head, &idb).unwrap();
    verify(&honest, &per_head, &model.facts).expect("honest proof verifies");

    // (a) Corrupt an interior premise tuple: the parent step's unification
    //     against the body literal must now fail.
    let corrupt_premise = corrupt_first_step_premise(&honest);
    assert!(
        verify(&corrupt_premise, &per_head, &model.facts).is_err(),
        "checker must reject a corrupted premise tuple"
    );

    // (b) Corrupt the derived tuple of the root: the head no longer grounds.
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

    // (c) Corrupt the rule index of the root to an out-of-range value: the
    //     checker's range guard must reject it.
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
    //     multi-rule head (path has a base and a recursive rule): the premise
    //     count or the head unification must then fail.
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
                    rel,
                    tuple: tuple.clone(),
                    rule_idx: (rule_idx + 1) % n_rules,
                    premises: premises.clone(),
                };
            }
            // Otherwise recurse into premises, flipping the first eligible one.
            let mut premises = premises.clone();
            for p in premises.iter_mut() {
                let flipped = flip_interior_rule(p, per_head);
                if flipped != *p {
                    *p = flipped;
                    break;
                }
            }
            Proof::Step {
                rel,
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

/// Corrupt a premise tuple of the root: prefer a derived (interior) premise
/// so the falsified row is one the parent step claims to have joined against.
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
        rel,
        tuple: tuple.clone(),
        rule_idx: *rule_idx,
        premises,
    }
}

/// The same proof node with its first tuple value replaced by a bogus one.
fn with_bumped_tuple(p: &Proof) -> Proof {
    match p {
        Proof::Ground { rel, tuple } => {
            let mut t = tuple.clone();
            t[0] = v(7777);
            Proof::Ground { rel, tuple: t }
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
                rel,
                tuple: t,
                rule_idx: *rule_idx,
                premises: premises.clone(),
            }
        }
    }
}

// ── A tiny end-to-end sanity on the generator itself ─────────────────────

#[test]
fn generator_is_seed_reproducible() {
    // Same seed → byte-identical program (no ambient entropy).
    let a = generate(42);
    let b = generate(42);
    assert_eq!(a.program.facts, b.program.facts);
    assert_eq!(a.program.rules.len(), b.program.rules.len());
    assert_eq!(a.entry, b.entry);
    // And a program of real size: thousands of EDB tuples.
    let total: usize = a.program.facts.values().map(|s| s.len()).sum();
    assert!(total > 800, "generated EDB is substantial: {total} tuples");
}
