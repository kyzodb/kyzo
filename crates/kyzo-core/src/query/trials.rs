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
//!   meet columns) — plus mutual recursion, a non-self-healing two-delta join, and
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

use crate::data::aggr::{MeetAccum, MeetAggr, parse_aggr};
use crate::data::bitemporal::ClaimPolarity;
use crate::data::program::{HeadAggrSlot, MagicSymbol, StoreLifetimes};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::data::value::Tuple;
use crate::query::eval::{
    AtomOccurrence, Budget, BudgetDimension, EvalDefinition, EvalProgram, EvalRuleSet, EvalStratum,
    FixedRuleEval, LimitExceeded, Premises, RowLimit, RuleBody, Witness, WitnessTable,
    stratified_evaluate,
};
use crate::query::laws::{
    AsOf, Axis, Bindings, Event, FixedRule, HeadAggr, Interval, Literal, OPEN_END, Program, Rel,
    Rule, Term, compose, derive_intervals, diff, ground, head_classes, naive_eval, naive_eval_at,
    resolve, resolve_relation, unify,
};
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
        // INVARIANT(splitmix64): modular mix per the splitmix64 contract; wrap is the PRNG.
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
            .filter(|(_, l)| !l.is_negated())
            .collect();
        ordered.extend(self.body.iter().enumerate().filter(|(_, l)| l.is_negated()));

        let mut frontier: Vec<(Bindings, Vec<Tuple>)> = vec![(Bindings::new(), Vec::new())];
        for (body_pos, l) in ordered {
            let is_delta = !l.is_negated() && delta_from == Some(AtomOccurrence(body_pos));
            let mut next = Vec::new();
            if l.is_negated() {
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

// `HeadClass`/`head_classes` are the shared reference-tier items from
// `query/laws.rs` (issue #89) — this harness used to hand-copy them too.

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
                    l.is_negated()
                } else {
                    true
                }
            } else {
                l.is_negated() || fixed_heads.contains(l.rel) || is_meet(l.rel)
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
    HeadAggrSlot::Aggregated {
        aggr: parse_aggr(name).expect("real aggregation"),
        args: vec![],
    }
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
    /// (grouping node at position 1) — a non-suffix positional layout.
    /// `false` keeps the classic suffix layout.
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
        .map(Tuple::from_vec)
        .collect();
    facts.insert("edge", edges);
    facts.insert(
        "node",
        (0..n).map(|i| vec![v(i)]).map(Tuple::from_vec).collect(),
    );

    // Meet seeds, typed to the chosen lattice.
    let n_seeds = rng.below(n as u64) as i64 + 1;
    let seeds: BTreeSet<Tuple> = (0..n_seeds)
        .map(|_| vec![v(rng.range(0, n)), meet_value(&mut rng, p.meet_op)])
        .map(Tuple::from_vec)
        .collect();
    facts.insert("seed", seeds);

    // The bulk relation: hundreds-to-thousands of rows over many keys.
    let n_items = rng.range(800, 3000);
    let n_keys = rng.range(20, 100);
    let items: BTreeSet<Tuple> = (0..n_items)
        .map(|_| vec![v(rng.range(0, n_keys)), v(rng.range(0, 50))])
        .map(Tuple::from_vec)
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
    //   pos0   — head m(agg, group), aggr [meet, None]  (non-suffix positional)
    if p.meet_pos0 {
        rules.push(Rule::aggregated(
            "m",
            vec![y(), x()],
            vec![named(p.meet_op), HeadAggrSlot::Plain],
            vec![lit("seed", vec![x(), y()], false)],
        ));
        rules.push(Rule::aggregated(
            "m",
            vec![z(), y()],
            vec![named(p.meet_op), HeadAggrSlot::Plain],
            vec![
                lit("edge", vec![x(), y()], false),
                lit("m", vec![z(), x()], false),
            ],
        ));
    } else {
        rules.push(Rule::aggregated(
            "m",
            vec![x(), y()],
            vec![HeadAggrSlot::Plain, named(p.meet_op)],
            vec![lit("seed", vec![x(), y()], false)],
        ));
        rules.push(Rule::aggregated(
            "m",
            vec![y(), z()],
            vec![HeadAggrSlot::Plain, named(p.meet_op)],
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
            .map(Tuple::from_vec)
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
            vec![named("min"), HeadAggrSlot::Plain, named("max")],
            vec![lit("seed3", vec![k.clone(), lo.clone(), hi.clone()], false)],
        ));
        rules.push(Rule::aggregated(
            "mi",
            vec![lo.clone(), t.clone(), hi.clone()],
            vec![named("min"), HeadAggrSlot::Plain, named("max")],
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
            vec![HeadAggrSlot::Plain, named("count")],
            vec![lit("path", vec![x(), y()], false)],
        ));
    }
    if p.bulk_aggr {
        rules.push(Rule::aggregated(
            "bulk_sum",
            vec![x(), y()],
            vec![HeadAggrSlot::Plain, named("sum")],
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
            out.insert(Tuple::from_vec(vec![t[0].clone()]));
            out.insert(Tuple::from_vec(vec![t[1].clone()]));
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
        // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
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
    let positives: Vec<&Literal> = rule.body.iter().filter(|l| !l.is_negated()).collect();
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
// `verify` imports no EVALUATOR symbol: only the model (`Rule`, `Literal`,
// `Term`), the shared reference-tier `unify` (`query/laws.rs`, issue #89),
// and plain data. It re-derives each step's binding from scratch, so a
// corrupted proof cannot pass by echoing eval's own reasoning. Its inputs
// are the rules (grouped per head), the ground facts, and the proof;
// nothing else.
//
// This used to hand-roll its own `check_unify` — a mutate-in-place,
// bool-returning variant, never independent of `unify` in the oracle-vs-
// judge sense (it never checked `unify` itself; it existed only because
// `verify` was written before this module imported the shared helper). It
// was drift, not a deliberate second algorithm: every call site already
// has one candidate binding to confirm, never a branching search over
// several, so `unify`'s clone-and-return-`Option` shape costs nothing here
// and the bespoke in-place variant bought no real independence. Removed
// (issue #89); `verify` below folds through `unify` the same way
// `provenance.rs`'s own independent checker (`verify_model_proof`) already
// did.

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
            // One binding must satisfy the head and every positive premise.
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
            .map(Tuple::from_vec)
            .collect(),
    );
    facts.insert(
        "tag",
        [(4, 100), (5, 200)]
            .iter()
            .map(|(a, b)| vec![v(*a), v(*b)])
            .map(Tuple::from_vec)
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

// ════════════════════════════════════════════════════════════════════════
// CAPABILITY 3 — sys-axis generative coverage: the unified temporal
// oracle's OWN internal consistency, generatively.
//
// Story #62 unified three disjoint temporal oracles into one (`laws.rs`):
// point events, per-literal `AsOf`, `derive_intervals`, `diff`/`compose`.
// But the generator above (`GenParams`/`generate`) only ever emits UNTIMED
// programs — `Program::facts`, never `Program::histories` — so every
// campaign in CAPABILITY 1 exercises the untimed half of the oracle only.
// This section is that generator's temporal twin.
//
// It does NOT drive the real engine: `ModelBody`/`ModelFixed` above
// (`rows_of`, `negated_probe_hits`) read `self.facts` only and have no
// notion of `Program::histories` or per-literal `AsOf` at all — that seam
// (the derivation/diff RA operators) is a later chunk of story #62, built
// elsewhere. Every check below is ORACLE-VS-ORACLE, within `laws.rs`'s own
// machinery, over seeded, deterministic event histories and programs
// richer than any hand fixture (multiple relations, multiple keys,
// negative coordinates on both axes, same-valid-instant system-version
// corrections) — but "oracle-vs-oracle" proves different things per check,
// worth being precise about: (a) `resolve`'s direct point resolution
// against `derive_intervals`'s interval reconstruction are genuinely TWO
// independent algorithms over the same events, so this differentials
// resolution correctness itself; (b) `diff`/`compose`'s compositionality
// law is a mathematical identity over `diff`'s own outputs, not an
// independence claim; (c) `naive_eval_at`'s per-literal coordinate
// pushdown is checked against `resolve_relation` called directly per
// coordinate then hand-joined — this proves the PUSHDOWN WIRING (each
// literal occurrence resolves at its OWN coordinate), not
// `resolve_relation`'s resolution algebra, which (a) already covers.
// ════════════════════════════════════════════════════════════════════════

/// Parameters governing one generated temporal fixture: how many
/// historical relations, how many keys each carries, how many events per
/// key, and the half-width of the coordinate range those events are drawn
/// from — always CENTERED ON ZERO, so every fixture straddles negative and
/// positive coordinates on both axes rather than treating negative
/// coordinates as a rare edge case.
#[derive(Debug, Clone)]
struct TemporalGenParams {
    n_relations: usize,
    keys_per_relation: i64,
    events_per_key: i64,
    coord_span: i64,
}

fn gen_temporal_params(rng: &mut Rng) -> TemporalGenParams {
    TemporalGenParams {
        n_relations: rng.range(1, 4) as usize,
        keys_per_relation: rng.range(1, 4),
        events_per_key: rng.range(2, 8),
        coord_span: rng.range(4, 20),
    }
}

const TEMPORAL_POLARITIES: [ClaimPolarity; 3] = [
    ClaimPolarity::Assert,
    ClaimPolarity::Retract,
    ClaimPolarity::Erase,
];

/// One key's event history: `events_per_key` events at coordinates drawn
/// from `[-coord_span, coord_span]` on BOTH axes (never the reserved
/// terminal tick — `coord_span` keeps every draw far below `i64::MAX`), a
/// genuine mix of all three polarities, and — with even odds per event — a
/// SAME-VALID-INSTANT system-version correction: a second event at the
/// identical `valid` but a strictly later `sys`, the exact shape the
/// governing-version skip-scan (`data/bitemporal.rs::check_key_for_bitemporal`,
/// mirrored by `resolve_events`) exists to arbitrate.
fn gen_temporal_history(rng: &mut Rng, key: &Tuple, p: &TemporalGenParams) -> Vec<Event> {
    let mut history = Vec::new();
    for _ in 0..p.events_per_key {
        let valid = rng.range(-p.coord_span, p.coord_span);
        let sys = rng.range(-p.coord_span, p.coord_span);
        push_temporal_event(&mut history, rng, key, valid, sys);
        if rng.chance(2, 5) {
            let correction_sys = sys + rng.range(1, 5);
            push_temporal_event(&mut history, rng, key, valid, correction_sys);
        }
    }
    history
}

fn push_temporal_event(history: &mut Vec<Event>, rng: &mut Rng, key: &Tuple, valid: i64, sys: i64) {
    let event = match rng.one_of(&TEMPORAL_POLARITIES) {
        ClaimPolarity::Assert => Event::assert(
            key.clone(),
            Tuple::from_vec(vec![v(rng.range(0, 5))]),
            valid,
            sys,
        ),
        ClaimPolarity::Retract => Event::retract(key.clone(), valid, sys),
        ClaimPolarity::Erase => Event::erase(key.clone(), valid, sys),
    };
    history.push(event.expect("coord_span keeps every draw far below the reserved terminal tick"));
}

/// A bundle of several historical relations, each with several keys'
/// independently generated histories — the raw EDB material a generated
/// temporal `Program` is built from.
struct TemporalHistories {
    per_relation: BTreeMap<Rel, BTreeMap<Tuple, Vec<Event>>>,
}

/// Relation names available to the generator, in a fixed order so seed
/// reproducibility never depends on `BTreeMap` iteration order deciding
/// which subset gets used.
const HIST_RELS: [&str; 3] = ["ha", "hb", "hc"];

fn gen_temporal_histories(rng: &mut Rng, p: &TemporalGenParams) -> TemporalHistories {
    let mut per_relation = BTreeMap::new();
    for &rel in HIST_RELS.iter().take(p.n_relations) {
        let mut per_key = BTreeMap::new();
        for i in 0..p.keys_per_relation {
            let key: Tuple = Tuple::from_vec(vec![v(i)]);
            per_key.insert(key.clone(), gen_temporal_history(rng, &key, p));
        }
        per_relation.insert(rel, per_key);
    }
    TemporalHistories { per_relation }
}

impl TemporalHistories {
    /// One relation's whole event history, every key's events flattened
    /// together — exactly the shape `Program::histories` stores.
    fn flat(&self, rel: Rel) -> Vec<Event> {
        self.per_relation[rel].values().flatten().cloned().collect()
    }
}

/// Permutes a generated rule's body literals in place, seeded by `rng`
/// (Fisher-Yates) — deliberate coverage of a SEMANTIC property, not noise.
///
/// `body_bindings` (`laws.rs`, ~1120) explicitly reorders positives before
/// negatives regardless of the rule's OWN source order — implementing the
/// invariant that a rule's answer set never depends on its body's literal
/// order. A hostile review's "Mutant C" (reverting that reorder to plain
/// source order) survived every generative campaign, because every
/// generator here — like every hand fixture — happened to always emit
/// positives before negatives already, so the property was pinned by only
/// ONE named regression test (`negation_over_as_of_is_correct_even_when_
/// the_negated_literal_precedes_its_binder_in_source_order`) instead of
/// being hunted at scale. `check_safety` and `check_wellformed` verify a
/// rule per-literal or over the body as a SET, never positionally, so any
/// permutation of a well-formed body is itself well-formed — a full
/// shuffle is always legal here, not merely a special case.
fn shuffle_body(rng: &mut Rng, body: &mut [Literal]) {
    for i in (1..body.len()).rev() {
        let j = rng.below((i + 1) as u64) as usize;
        body.swap(i, j);
    }
}

/// A generated temporal `Program`: every relation of `hist` stored as a
/// historical EDB, one plain union rule per relation reading it untimed
/// (`out(K,V) :- rel(K,V)`), plus — when at least two relations were
/// generated — a genuine cross-relation join (`joined(K,V1,V2) :- ha(K,V1),
/// hb(K,V2)`), all non-recursive and negation-free so evaluation is a
/// single pass no fixpoint iteration is needed to reach. Every rule body is
/// shuffled (`shuffle_body`) before being stored, so the grid differential
/// run over this program hunts body-order sensitivity, not only the
/// union/join wiring it was written to prove.
fn temporal_program(rng: &mut Rng, hist: &TemporalHistories) -> Program {
    let mut histories: BTreeMap<Rel, Vec<Event>> = BTreeMap::new();
    for &rel in hist.per_relation.keys() {
        histories.insert(rel, hist.flat(rel));
    }
    let mut rules: Vec<Rule> = histories
        .keys()
        .map(|&rel| Rule::plain("out", vec![x(), y()], vec![lit(rel, vec![x(), y()], false)]))
        .collect();
    if histories.contains_key("ha") && histories.contains_key("hb") {
        rules.push(Rule::plain(
            "joined",
            vec![x(), y(), z()],
            vec![
                lit("ha", vec![x(), y()], false),
                lit("hb", vec![x(), z()], false),
            ],
        ));
    }
    for rule in &mut rules {
        shuffle_body(rng, &mut rule.body);
    }
    Program {
        rules,
        histories,
        ..Program::default()
    }
}

/// Every distinct stored coordinate of `history` on `axis`, ± one tick,
/// plus the extremes — `laws::tests::grid`'s complete-grid pattern
/// (private to that module's own tests), reconstructed here so this
/// section's campaigns can apply it to GENERATED histories bundled inside
/// a generated program, not only to one hand-picked key.
fn program_grid(history: &[Event], axis: Axis) -> Vec<i64> {
    let mut pts: Vec<i64> = history
        .iter()
        .flat_map(|e| {
            let c = match axis {
                Axis::Valid => e.valid(),
                Axis::Sys => e.sys(),
            };
            [c - 1, c, c + 1]
        })
        .collect();
    pts.push(i64::MIN);
    // Not `i64::MAX` itself: it is `OPEN_END`/`AsOf::current()`'s shared
    // sentinel, never a real stored coordinate (`Event::assert` et al.
    // refuse it) — probing one tick short is still the complete-grid
    // extreme for a query coordinate.
    pts.push(i64::MAX - 1);
    pts.sort_unstable();
    pts.dedup();
    pts
}

// ── (a) Snapshot-equivalence grid, generalized from one key/one history
//    (the sealed pattern) to a generated PROGRAM's whole historical EDB —
//    every relation, every key, both axes. ───────────────────────────────

#[test]
fn grid_differential_over_generated_temporal_programs() {
    let mut cases = 0usize;
    let seeds = 400u64;
    for seed in 0..seeds {
        // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
        let mut rng = Rng::new(0x7E57_A105_u64 ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let params = gen_temporal_params(&mut rng);
        let hist = gen_temporal_histories(&mut rng, &params);
        let program = temporal_program(&mut rng, &hist);

        for (&rel, history) in &program.histories {
            let keys: BTreeSet<&Tuple> = history.iter().map(|e| e.key()).collect();
            for key in keys {
                let valid_grid = program_grid(history, Axis::Valid);
                let sys_grid = program_grid(history, Axis::Sys);
                for &sys_pt in &sys_grid {
                    let ivs = derive_intervals(history, key, Axis::Valid, sys_pt);
                    for &valid_pt in &valid_grid {
                        let direct = resolve(
                            history,
                            key,
                            AsOf {
                                valid: valid_pt,
                                sys: sys_pt,
                            },
                        );
                        let via_intervals = ivs
                            .iter()
                            .find(|iv| iv.start <= valid_pt && valid_pt < iv.end)
                            .map(|iv| iv.tuple.clone());
                        assert_eq!(
                            direct, via_intervals,
                            "seed {seed} rel={rel} key={key:?}: valid axis \
                             valid={valid_pt} sys={sys_pt}"
                        );
                        cases += 1;
                    }
                }
                for &fixed_valid in &[history.first().map(|e| e.valid()).unwrap_or(0), 0] {
                    let ivs = derive_intervals(history, key, Axis::Sys, fixed_valid);
                    for &sys_pt in &sys_grid {
                        let direct = resolve(
                            history,
                            key,
                            AsOf {
                                valid: fixed_valid,
                                sys: sys_pt,
                            },
                        );
                        let via_intervals = ivs
                            .iter()
                            .find(|iv| iv.start <= sys_pt && sys_pt < iv.end)
                            .map(|iv| iv.tuple.clone());
                        assert_eq!(
                            direct, via_intervals,
                            "seed {seed} rel={rel} key={key:?}: sys axis \
                             fixed_valid={fixed_valid} sys={sys_pt}"
                        );
                        cases += 1;
                    }
                }
            }
        }

        // Program-level wiring: the whole-program answer at "current",
        // through the real evaluator (`naive_eval_at` -> `literal_rows` ->
        // `resolve_relation`), must equal the union/join hand-computed
        // directly from each relation's `resolve_relation` snapshot — the
        // generated union and join rules compose the same way the raw
        // histories do above.
        let db = naive_eval_at(&program, AsOf::current()).expect("well-formed generated program");
        let mut expected_out: BTreeSet<Tuple> = BTreeSet::new();
        for history in program.histories.values() {
            expected_out.extend(resolve_relation(history, AsOf::current()));
        }
        assert_eq!(
            db.get("out").cloned().unwrap_or_default(),
            expected_out,
            "seed {seed}: union wiring"
        );
        cases += 1;
        if let (Some(ha), Some(hb)) = (program.histories.get("ha"), program.histories.get("hb")) {
            let snap_a = resolve_relation(ha, AsOf::current());
            let snap_b = resolve_relation(hb, AsOf::current());
            let mut expected_joined: BTreeSet<Tuple> = BTreeSet::new();
            for row_a in &snap_a {
                for row_b in &snap_b {
                    if row_a[0] == row_b[0] {
                        expected_joined.insert(Tuple::from_vec(vec![
                            row_a[0].clone(),
                            row_a[1].clone(),
                            row_b[1].clone(),
                        ]));
                    }
                }
            }
            assert_eq!(
                db.get("joined").cloned().unwrap_or_default(),
                expected_joined,
                "seed {seed}: join wiring"
            );
            cases += 1;
        }
    }
    assert!(
        cases > 5000,
        "expected a rich grid campaign over generated programs, ran {cases}"
    );
}

// ── (b) Diff-composition law, over generated histories, with RANDOMIZED
//    a<b<c bounds on both axes (the sealed `diff_composition_law_holds_
//    across_axes` in laws.rs pins fixed bounds 0/3/6 — this generalizes to
//    arbitrary, seeded, negative-inclusive bounds). ───────────────────────

/// Three distinct coordinates, ascending, drawn from a range wide enough
/// to straddle the generated history's own span on both sides — `diff`
/// composes over ANY `a<b<c`, not only bounds that happen to be stored
/// coordinates.
fn ordered_triple(rng: &mut Rng, span: i64) -> (i64, i64, i64) {
    let lo = -(span * 2) - 5;
    let hi = span * 2 + 5;
    loop {
        let mut xs = [rng.range(lo, hi), rng.range(lo, hi), rng.range(lo, hi)];
        xs.sort_unstable();
        if xs[0] < xs[1] && xs[1] < xs[2] {
            return (xs[0], xs[1], xs[2]);
        }
    }
}

#[test]
fn diff_composition_law_holds_with_randomized_bounds_over_generated_histories() {
    let mut cases = 0usize;
    let seeds = 400u64;
    for seed in 0..seeds {
        // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
        let mut rng = Rng::new(0xD1FF_5EED_u64 ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let params = gen_temporal_params(&mut rng);
        let key: Tuple = Tuple::from_vec(vec![v(0)]);
        let history = gen_temporal_history(&mut rng, &key, &params);

        let sys_now = AsOf::current().sys;
        let (av, bv, cv) = ordered_triple(&mut rng, params.coord_span);
        let a = AsOf {
            valid: av,
            sys: sys_now,
        };
        let b = AsOf {
            valid: bv,
            sys: sys_now,
        };
        let c = AsOf {
            valid: cv,
            sys: sys_now,
        };
        let ab = diff(&history, a, b);
        let bc = diff(&history, b, c);
        let ac = diff(&history, a, c);
        assert_eq!(
            compose(&ab, &bc).expect("unit net"),
            ac,
            "seed {seed}: valid axis a={av} b={bv} c={cv}"
        );
        cases += 1;

        let fixed_valid = history.first().map(|e| e.valid()).unwrap_or(0);
        let (asys, bsys, csys) = ordered_triple(&mut rng, params.coord_span);
        let a = AsOf {
            valid: fixed_valid,
            sys: asys,
        };
        let b = AsOf {
            valid: fixed_valid,
            sys: bsys,
        };
        let c = AsOf {
            valid: fixed_valid,
            sys: csys,
        };
        let ab = diff(&history, a, b);
        let bc = diff(&history, b, c);
        let ac = diff(&history, a, c);
        assert_eq!(
            compose(&ab, &bc).expect("unit net"),
            ac,
            "seed {seed}: sys axis a={asys} b={bsys} c={csys}"
        );
        cases += 1;
    }
    assert!(
        cases >= 500,
        "expected hundreds of randomized-bounds composition cases, ran {cases}"
    );
}

// ── (c) Per-literal `AsOf` pushdown consistency: a generated program with
//    TWO literals reading the SAME historical relation at two different
//    coordinates, checked against resolving each coordinate on its own and
//    hand-joining — the exact reading `literal_rows`'s doc comment claims
//    ("each literal sees its own snapshot"). Precisely: this proves the
//    PUSHDOWN wiring — that two occurrences of one literal, each carrying
//    its own `as_of`, are each resolved at THEIR OWN coordinate rather
//    than some shared/confused one — not `resolve_relation`'s own
//    resolution algebra (both sides call it; that correctness is
//    `laws.rs`'s grid differential/diff-composition law's job). ─────────

fn lit_at(rel: Rel, args: Vec<Term>, at: AsOf) -> Literal {
    Literal::pos_at(rel, args, at)
}

/// A coordinate a tick or two off a real stored event — never the reserved
/// terminal tick. `Event`'s own constructors already keep every STORED
/// coordinate far below it; this only has to keep the QUERY coordinate
/// from coincidentally landing on it too (a legitimate "read current"
/// query bound this fixture has no interest in probing).
fn near_coordinate(rng: &mut Rng, history: &[Event]) -> AsOf {
    if history.is_empty() {
        return AsOf { valid: 0, sys: 0 };
    }
    let events: Vec<&Event> = history.iter().collect();
    let e = rng.one_of(&events);
    AsOf {
        valid: nudge(rng, e.valid()),
        sys: nudge(rng, e.sys()),
    }
}

fn nudge(rng: &mut Rng, coordinate: i64) -> i64 {
    let out = coordinate.saturating_add(rng.range(-2, 3));
    if out == i64::MAX { out - 1 } else { out }
}

#[test]
fn per_literal_asof_pushdown_matches_independent_single_coordinate_resolution() {
    let mut cases = 0usize;
    let seeds = 400u64;
    for seed in 0..seeds {
        // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
        let mut rng = Rng::new(0x9A5D_6E1B_u64 ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let params = gen_temporal_params(&mut rng);
        let mut history = Vec::new();
        for i in 0..params.keys_per_relation {
            history.extend(gen_temporal_history(
                &mut rng,
                &Tuple::from_vec(vec![v(i)]),
                &params,
            ));
        }

        // Two distinct query coordinates near real generated events — the
        // two literal-level `AsOf`s this rule's two occurrences of `hx`
        // will each carry.
        let c1 = near_coordinate(&mut rng, &history);
        let c2 = near_coordinate(&mut rng, &history);

        let mut histories: BTreeMap<Rel, Vec<Event>> = BTreeMap::new();
        histories.insert("hx", history);
        // out(K, V1, V2) :- hx(K, V1) @c1, hx(K, V2) @c2 — the same
        // relation, read at two different coordinates, joined on the
        // shared key.
        let program = Program {
            rules: vec![Rule::plain(
                "out",
                vec![x(), y(), z()],
                vec![
                    lit_at("hx", vec![x(), y()], c1),
                    lit_at("hx", vec![x(), z()], c2),
                ],
            )],
            histories,
            ..Program::default()
        };

        let got = naive_eval(&program)
            .expect("well-formed generated program")
            .get("out")
            .cloned()
            .unwrap_or_default();

        // Expectation: resolve each coordinate's snapshot ON ITS OWN,
        // called directly rather than through the program/evaluator, then
        // hand-join on the shared key — this checks the PUSHDOWN wiring
        // against those snapshots, not `resolve_relation` itself.
        let hx = &program.histories["hx"];
        let snap1 = resolve_relation(hx, c1);
        let snap2 = resolve_relation(hx, c2);
        let mut expected: BTreeSet<Tuple> = BTreeSet::new();
        for row1 in &snap1 {
            for row2 in &snap2 {
                if row1[0] == row2[0] {
                    expected.insert(Tuple::from_vec(vec![
                        row1[0].clone(),
                        row1[1].clone(),
                        row2[1].clone(),
                    ]));
                }
            }
        }
        assert_eq!(got, expected, "seed {seed}: c1={c1:?} c2={c2:?}");
        cases += 1;
    }
    assert!(
        cases >= 300,
        "expected hundreds of pushdown-consistency cases, ran {cases}"
    );
}

// ── Hand mutants: three deliberate weakenings of the generator/differential
//    code above, each shown to blind a campaign to a companion
//    deliberately-broken oracle twin that the REAL (unweakened) generator
//    or grid catches. ──────────────────────────────────────────────────────

// Mutant 1 — dropping Erase from generation.

/// The buggy twin: `Erase` settles the fact ABSENT, like `Retract`,
/// instead of falling through to the next older instant — a bug shaped
/// exactly like the one `data/bitemporal.rs`'s own governing-version sweep
/// must avoid (`Erase` is transparent, never terminal). Built from
/// `resolve`, so only the polarity handling differs from the real oracle.
fn resolve_erase_as_retract_bug(history: &[Event], key: &Tuple, at: AsOf) -> Option<Tuple> {
    let mut instants: Vec<i64> = history
        .iter()
        .filter(|e| e.key() == key && e.valid() <= at.valid)
        .map(|e| e.valid())
        .collect();
    instants.sort_unstable();
    instants.dedup();
    for instant in instants.into_iter().rev() {
        let governing = history
            .iter()
            .filter(|e| e.key() == key && e.valid() == instant && e.sys() <= at.sys)
            .max_by_key(|e| e.sys());
        match governing {
            Some(Event::Assert {
                key: k, payload, ..
            }) => {
                let mut t = k.clone();
                t.extend(payload.iter().cloned());
                return Some(t);
            }
            // BUG: Erase should fall through (continue the loop, `{}`),
            // not settle absent like Retract.
            Some(Event::Retract { .. }) | Some(Event::Erase { .. }) => return None,
            None => {}
        }
    }
    None
}

/// The mutated generator: only `Assert`/`Retract`, never `Erase`.
fn gen_temporal_history_no_erase(rng: &mut Rng, key: &Tuple, p: &TemporalGenParams) -> Vec<Event> {
    let mut history = Vec::new();
    for _ in 0..p.events_per_key {
        let valid = rng.range(-p.coord_span, p.coord_span);
        let sys = rng.range(-p.coord_span, p.coord_span);
        let event = if rng.chance(1, 2) {
            Event::assert(
                key.clone(),
                Tuple::from_vec(vec![v(rng.range(0, 5))]),
                valid,
                sys,
            )
        } else {
            Event::retract(key.clone(), valid, sys)
        };
        history
            .push(event.expect("coord_span keeps every draw far below the reserved terminal tick"));
        if rng.chance(2, 5) {
            let correction_sys = sys + rng.range(1, 5);
            history.push(
                Event::assert(
                    key.clone(),
                    Tuple::from_vec(vec![v(rng.range(0, 5))]),
                    valid,
                    correction_sys,
                )
                .expect("coord_span keeps every draw far below the reserved terminal tick"),
            );
        }
    }
    history
}

/// Does the buggy twin disagree with the real oracle anywhere on `history`'s
/// own grid?
fn erase_bug_manifests(history: &[Event], key: &Tuple) -> bool {
    for &valid in &program_grid(history, Axis::Valid) {
        for &sys in &program_grid(history, Axis::Sys) {
            let at = AsOf { valid, sys };
            if resolve(history, key, at) != resolve_erase_as_retract_bug(history, key, at) {
                return true;
            }
        }
    }
    false
}

#[test]
fn mutant_dropping_erase_from_generation_blinds_the_campaign() {
    let seeds = 300u64;
    let key: Tuple = Tuple::from_vec(vec![v(0)]);

    let mut caught_without_erase = false;
    for seed in 0..seeds {
        // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
        let mut rng = Rng::new(0xE1A5_E000_u64 ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let params = gen_temporal_params(&mut rng);
        let history = gen_temporal_history_no_erase(&mut rng, &key, &params);
        caught_without_erase |= erase_bug_manifests(&history, &key);
    }
    assert!(
        !caught_without_erase,
        "without Erase in generation, the erase-mishandling bug is structurally \
         unreachable (Retract and the bug's mishandled Erase branch are \
         identical) — nothing for {seeds} seeds to catch"
    );

    let mut caught_with_erase = false;
    for seed in 0..seeds {
        // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
        let mut rng = Rng::new(0xE1A5_E000_u64 ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let params = gen_temporal_params(&mut rng);
        let history = gen_temporal_history(&mut rng, &key, &params); // the real generator
        caught_with_erase |= erase_bug_manifests(&history, &key);
    }
    assert!(
        caught_with_erase,
        "the real generator (with Erase) must expose the erase-mishandling \
         bug somewhere in {seeds} seeds"
    );
}

// Mutant 2 — skipping negative coordinates.

/// The buggy twin: sorts derived-interval breakpoints by ABSOLUTE VALUE
/// instead of ascending — a plausible "smallest magnitude first" slip that
/// is silently correct whenever every coordinate is non-negative (the two
/// orders coincide) and silently wrong the instant a negative coordinate
/// appears alongside a positive one.
fn derive_intervals_abs_sort_bug(
    history: &[Event],
    key: &Tuple,
    axis: Axis,
    fixed: i64,
) -> Vec<Interval> {
    let mut breaks: Vec<i64> = history
        .iter()
        .filter(|e| e.key() == key)
        .map(|e| match axis {
            Axis::Valid => e.valid(),
            Axis::Sys => e.sys(),
        })
        .collect();
    breaks.sort_unstable_by_key(|b| b.unsigned_abs()); // BUG: magnitude, not ascending
    breaks.dedup();
    let coordinate = |pt: i64| -> AsOf {
        match axis {
            Axis::Valid => AsOf {
                valid: pt,
                sys: fixed,
            },
            Axis::Sys => AsOf {
                valid: fixed,
                sys: pt,
            },
        }
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < breaks.len() {
        let start = breaks[i];
        let Some(tuple) = resolve(history, key, coordinate(start)) else {
            i += 1;
            continue;
        };
        let mut j = i;
        while j + 1 < breaks.len()
            && resolve(history, key, coordinate(breaks[j + 1])) == Some(tuple.clone())
        {
            j += 1;
        }
        let end = if j + 1 < breaks.len() {
            breaks[j + 1]
        } else {
            OPEN_END
        };
        out.push(Interval { start, end, tuple });
        i = j + 1;
    }
    out
}

/// The mutated generator: coordinates drawn only from `[0, coord_span]`.
fn gen_temporal_history_nonneg(rng: &mut Rng, key: &Tuple, p: &TemporalGenParams) -> Vec<Event> {
    let mut history = Vec::new();
    for _ in 0..p.events_per_key {
        let valid = rng.range(0, p.coord_span.max(1));
        let sys = rng.range(0, p.coord_span.max(1));
        push_temporal_event(&mut history, rng, key, valid, sys);
        if rng.chance(2, 5) {
            let correction_sys = sys + rng.range(1, 5);
            push_temporal_event(&mut history, rng, key, valid, correction_sys);
        }
    }
    history
}

fn abs_sort_bug_manifests(history: &[Event], key: &Tuple) -> bool {
    let ivs = derive_intervals_abs_sort_bug(history, key, Axis::Valid, AsOf::current().sys);
    for &valid in &program_grid(history, Axis::Valid) {
        let at = AsOf {
            valid,
            sys: AsOf::current().sys,
        };
        let direct = resolve(history, key, at);
        let via = ivs
            .iter()
            .find(|iv| iv.start <= valid && valid < iv.end)
            .map(|iv| iv.tuple.clone());
        if direct != via {
            return true;
        }
    }
    false
}

#[test]
fn mutant_skipping_negative_coordinates_blinds_the_campaign() {
    let seeds = 300u64;
    let key: Tuple = Tuple::from_vec(vec![v(0)]);

    let mut caught_nonneg_only = false;
    for seed in 0..seeds {
        // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
        let mut rng = Rng::new(0xA65_5169_u64 ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let params = gen_temporal_params(&mut rng);
        let history = gen_temporal_history_nonneg(&mut rng, &key, &params);
        caught_nonneg_only |= abs_sort_bug_manifests(&history, &key);
    }
    assert!(
        !caught_nonneg_only,
        "without negative coordinates in generation, magnitude-order and \
         ascending-order sorts coincide — the abs-sort bug is unreachable, \
         nothing for {seeds} seeds to catch"
    );

    let mut caught_with_negatives = false;
    for seed in 0..seeds {
        // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
        let mut rng = Rng::new(0xA65_5169_u64 ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let params = gen_temporal_params(&mut rng);
        let history = gen_temporal_history(&mut rng, &key, &params); // the real generator
        caught_with_negatives |= abs_sort_bug_manifests(&history, &key);
    }
    assert!(
        caught_with_negatives,
        "the real generator (spanning negative and positive coordinates) \
         must expose the abs-sort bug somewhere in {seeds} seeds"
    );
}

// Mutant 3 — weakening the grid to stored coordinates only, without ±1.

/// The buggy twin: every INTERIOR interval boundary closes one tick short
/// (`breaks[j+1] - 1` instead of `breaks[j+1]`). Silent at every stored
/// coordinate (each one is still covered by SOME interval — its own
/// defining breakpoint, not the shortened neighbor), and wrong only at the
/// single coordinate strictly between two stored breakpoints that differ
/// by more than one tick — precisely the point a coordinates-only grid
/// never probes.
fn derive_intervals_short_end_bug(
    history: &[Event],
    key: &Tuple,
    axis: Axis,
    fixed: i64,
) -> Vec<Interval> {
    let mut breaks: Vec<i64> = history
        .iter()
        .filter(|e| e.key() == key)
        .map(|e| match axis {
            Axis::Valid => e.valid(),
            Axis::Sys => e.sys(),
        })
        .collect();
    breaks.sort_unstable();
    breaks.dedup();
    let coordinate = |pt: i64| -> AsOf {
        match axis {
            Axis::Valid => AsOf {
                valid: pt,
                sys: fixed,
            },
            Axis::Sys => AsOf {
                valid: fixed,
                sys: pt,
            },
        }
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < breaks.len() {
        let start = breaks[i];
        let Some(tuple) = resolve(history, key, coordinate(start)) else {
            i += 1;
            continue;
        };
        let mut j = i;
        while j + 1 < breaks.len()
            && resolve(history, key, coordinate(breaks[j + 1])) == Some(tuple.clone())
        {
            j += 1;
        }
        let end = if j + 1 < breaks.len() {
            breaks[j + 1] - 1 // BUG: one tick short
        } else {
            OPEN_END
        };
        out.push(Interval { start, end, tuple });
        i = j + 1;
    }
    out
}

fn short_end_bug_manifests(history: &[Event], key: &Tuple, grid: &[i64]) -> bool {
    let ivs = derive_intervals_short_end_bug(history, key, Axis::Valid, AsOf::current().sys);
    for &valid in grid {
        let at = AsOf {
            valid,
            sys: AsOf::current().sys,
        };
        let direct = resolve(history, key, at);
        let via = ivs
            .iter()
            .find(|iv| iv.start <= valid && valid < iv.end)
            .map(|iv| iv.tuple.clone());
        if direct != via {
            return true;
        }
    }
    false
}

#[test]
fn mutant_weakening_the_grid_to_stored_coordinates_only_blinds_it_to_a_short_end_boundary_bug() {
    let seeds = 300u64;
    let key: Tuple = Tuple::from_vec(vec![v(0)]);

    // Counted, not merely booleaned: a coordinates-only grid CAN still
    // catch this bug when two stored breakpoints happen to be exactly one
    // tick apart (the shortened end then excludes the very breakpoint that
    // should still be its own interval's start) — a real but incidental
    // overlap, not the general case. The honest claim is comparative: the
    // ±1-tick grid catches strictly MORE seeds than the coordinates-only
    // grid, because the bug's general failure mode — wrong at the single
    // coordinate strictly between two breakpoints more than one tick
    // apart — is exactly what only ±1 probes.
    let mut caught_without_ticks = 0usize;
    let mut caught_with_ticks = 0usize;
    for seed in 0..seeds {
        // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
        let mut rng = Rng::new(0x9BAD_E1D0_u64 ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let params = gen_temporal_params(&mut rng);
        let history = gen_temporal_history(&mut rng, &key, &params);

        // The mutated grid: stored coordinates only, no ±1, no extremes.
        let mut stored_only: Vec<i64> = history
            .iter()
            .filter(|e| *e.key() == key)
            .map(|e| e.valid())
            .collect();
        stored_only.sort_unstable();
        stored_only.dedup();
        if short_end_bug_manifests(&history, &key, &stored_only) {
            caught_without_ticks += 1;
        }

        // The sealed grid: stored coordinates ± one tick, plus extremes.
        let full_grid = program_grid(&history, Axis::Valid);
        if short_end_bug_manifests(&history, &key, &full_grid) {
            caught_with_ticks += 1;
        }
    }
    assert!(
        caught_with_ticks > 0,
        "the ±1-tick grid must catch the short-end-boundary bug in at \
         least one of {seeds} seeds"
    );
    assert!(
        caught_without_ticks < caught_with_ticks,
        "a coordinates-only grid must catch the short-end-boundary bug in \
         STRICTLY FEWER seeds than the ±1-tick grid (without-ticks: \
         {caught_without_ticks}, with-ticks: {caught_with_ticks} of \
         {seeds}) — the general failure (wrong strictly between two \
         breakpoints more than one tick apart) is exactly what only ±1 \
         probes; any without-ticks hits are the incidental \
         adjacent-breakpoint overlap"
    );
}

// ════════════════════════════════════════════════════════════════════════
// CAPABILITY 4 — the refusal lift's generator coverage: temporal programs
// through negation, recursion, and both aggregation families, uniformly.
//
// Story #62 lifted `NegationOverTimeTravelError` in the oracle (`laws.rs`):
// negation over a historical relation at a FIXED as-of coordinate is legal
// and well-defined (see `laws.rs`'s module doc, "the time-travel negation
// lift"). This section's generator combines that lift with the three
// OTHER dimensions the untimed generator (CAPABILITY 1) already covers
// untimed — recursion, negation, and both aggregation families — but now
// reading a HISTORICAL graph/seed relation at generated as-of coordinates
// instead of a plain EDB.
//
// What this proves, precisely: that recursion, negation, and both
// aggregation families are each correctly WIRED to whatever
// `resolve_relation` returns at the fixture's chosen coordinates — every
// check below calls `resolve_relation` itself to build `edge_snapshot`/
// `seed_snapshot`, then reasons from there with an independently written
// reference algorithm (brute-force transitive closure, plain set
// complement, group-and-count, meet-propagation via the real landed
// `Aggregation` meet ops — never a re-derivation of the Datalog evaluator
// itself). So a bug PRIVATE to that RA-shape wiring (recursion over a
// historical base, the lifted negation, either aggregation family reading
// a historical literal) is exactly what this catches; a bug in
// `resolve_relation`/`resolve_events`'s own resolution algebra would be
// shared by both the program under test and this campaign's snapshots,
// and is proven independently elsewhere (`laws.rs`'s
// `grid_differential_derived_intervals_equal_maximal_runs`,
// `diff_composition_law_holds_across_axes`, and the real-kernel
// cross-check `asof_mirror_matches_bitemporal_kernel_on_a_shared_
// fixture`).
// ════════════════════════════════════════════════════════════════════════

/// [`gen_temporal_history`]'s existential twin: EMPTY payload, so the
/// governing tuple is the key alone — a graph edge `(a,b)` needs no
/// payload column, and a negation probe on the SAME two columns the
/// positive reads use needs the tuple shape to match exactly.
fn gen_temporal_existential_history(
    rng: &mut Rng,
    key: &Tuple,
    p: &TemporalGenParams,
) -> Vec<Event> {
    let mut history = Vec::new();
    for _ in 0..p.events_per_key {
        let valid = rng.range(-p.coord_span, p.coord_span);
        let sys = rng.range(-p.coord_span, p.coord_span);
        let event = match rng.one_of(&TEMPORAL_POLARITIES) {
            ClaimPolarity::Assert => {
                Event::assert(key.clone(), Tuple::from_vec(vec![]), valid, sys)
            }
            ClaimPolarity::Retract => Event::retract(key.clone(), valid, sys),
            ClaimPolarity::Erase => Event::erase(key.clone(), valid, sys),
        };
        history
            .push(event.expect("coord_span keeps every draw far below the reserved terminal tick"));
        if rng.chance(2, 5) {
            let correction_sys = sys + rng.range(1, 5);
            history.push(
                Event::assert(key.clone(), Tuple::from_vec(vec![]), valid, correction_sys)
                    .expect("coord_span keeps every draw far below the reserved terminal tick"),
            );
        }
    }
    history
}

/// A negated body literal at its own bitemporal coordinate — [`lit_at`]'s
/// negated twin, now legal post-lift.
fn neg_lit_at(rel: Rel, args: Vec<Term>, at: AsOf) -> Literal {
    Literal::neg_at(rel, args, at)
}

/// A generated "temporal reachability" fixture: a historical, existential
/// graph (`hedge`) and a historical meet-seed relation (`hseed`), each with
/// several keys' generated event histories on both axes, plus fixed as-of
/// coordinates for each (drawn near real generated event coordinates, the
/// same discipline [`near_coordinate`] already uses) and the meet lattice
/// this fixture's seed values are typed to.
struct ReachabilityFixture {
    edge_history: Vec<Event>,
    seed_history: Vec<Event>,
    nodes: Vec<i64>,
    c_edge: AsOf,
    c_seed: AsOf,
    meet_op: &'static str,
}

fn gen_reachability_fixture(rng: &mut Rng) -> ReachabilityFixture {
    let n = rng.range(3, 8);
    let nodes: Vec<i64> = (0..n).collect();
    let params = gen_temporal_params(rng);

    // A random directed graph, EXISTENTIAL (empty payload): every (a,b)
    // pair gets its own independently generated event history.
    let n_edges = rng.range(1, n * 2);
    let mut edge_history = Vec::new();
    for _ in 0..n_edges {
        let a = rng.range(0, n);
        let b = rng.range(0, n);
        edge_history.extend(gen_temporal_existential_history(
            rng,
            &Tuple::from_vec(vec![v(a), v(b)]),
            &params,
        ));
    }

    // A seed value per node (not every node — some nodes are reachable
    // only by propagation, never directly seeded), typed to the chosen
    // meet lattice.
    let meet_op = rng.one_of(&MEET_OPS);
    let mut seed_history = Vec::new();
    for &node in &nodes {
        if rng.chance(2, 3) {
            let key: Tuple = Tuple::from_vec(vec![v(node)]);
            for _ in 0..rng.range(1, 4) {
                let valid = rng.range(-params.coord_span, params.coord_span);
                let sys = rng.range(-params.coord_span, params.coord_span);
                seed_history.push(
                    Event::assert(
                        key.clone(),
                        Tuple::from_vec(vec![meet_value(rng, meet_op)]),
                        valid,
                        sys,
                    )
                    .expect("coord_span keeps every draw far below the reserved terminal tick"),
                );
            }
        }
    }

    let c_edge = near_coordinate(rng, &edge_history);
    let c_seed = near_coordinate(rng, &seed_history);
    ReachabilityFixture {
        edge_history,
        seed_history,
        nodes,
        c_edge,
        c_seed,
        meet_op,
    }
}

/// The generated program: `path` (recursion over `hedge@c_edge`),
/// `unreachable` (negation over `hedge@c_edge` — the LIFTED shape),
/// `deg` (normal aggregation over `hedge@c_edge`), and `m` (meet
/// aggregation recursing `hseed@c_seed` along `hedge@c_edge`) — all four
/// reading the SAME two historical relations, at the SAME two fixed
/// coordinates, uniformly. Every rule body is shuffled (`shuffle_body`)
/// before being stored — `unreachable`'s body is written positives-then-
/// negative in source order below (`node(X), node(Y), NOT hedge(X,Y)`)
/// purely for readability; the shuffle is what actually exercises
/// `body_bindings`'s reordering across the mix this fixture is built to
/// prove (negation, recursion, both aggregation families) instead of
/// leaving it pinned to the one hand-written case in `laws.rs`.
fn reachability_program(rng: &mut Rng, fx: &ReachabilityFixture) -> Program {
    let mut histories: BTreeMap<Rel, Vec<Event>> = BTreeMap::new();
    histories.insert("hedge", fx.edge_history.clone());
    histories.insert("hseed", fx.seed_history.clone());
    let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
    facts.insert(
        "node",
        fx.nodes
            .iter()
            .map(|&n| vec![v(n)])
            .map(Tuple::from_vec)
            .collect(),
    );

    let mut rules = vec![
        // path(X,Y) :- hedge(X,Y) @c_edge.
        Rule::plain(
            "path",
            vec![x(), y()],
            vec![lit_at("hedge", vec![x(), y()], fx.c_edge)],
        ),
        // path(X,Z) :- path(X,Y), hedge(Y,Z) @c_edge.
        Rule::plain(
            "path",
            vec![x(), z()],
            vec![
                lit("path", vec![x(), y()], false),
                lit_at("hedge", vec![y(), z()], fx.c_edge),
            ],
        ),
        // unreachable(X,Y) :- node(X), node(Y), NOT hedge(X,Y) @c_edge —
        // the exact shape `NegationOverTimeTravelError` used to refuse.
        Rule::plain(
            "unreachable",
            vec![x(), y()],
            vec![
                lit("node", vec![x()], false),
                lit("node", vec![y()], false),
                neg_lit_at("hedge", vec![x(), y()], fx.c_edge),
            ],
        ),
        // deg(X, count(Y)) :- hedge(X,Y) @c_edge.
        Rule::aggregated(
            "deg",
            vec![x(), y()],
            vec![HeadAggrSlot::Plain, named("count")],
            vec![lit_at("hedge", vec![x(), y()], fx.c_edge)],
        ),
        // m(X,V) :- hseed(X,V) @c_seed.
        Rule::aggregated(
            "m",
            vec![x(), y()],
            vec![HeadAggrSlot::Plain, named(fx.meet_op)],
            vec![lit_at("hseed", vec![x(), y()], fx.c_seed)],
        ),
        // m(Y,Z) :- hedge(X,Y) @c_edge, m(X,Z).
        Rule::aggregated(
            "m",
            vec![y(), z()],
            vec![HeadAggrSlot::Plain, named(fx.meet_op)],
            vec![
                lit_at("hedge", vec![x(), y()], fx.c_edge),
                lit("m", vec![x(), z()], false),
            ],
        ),
    ];
    for rule in &mut rules {
        shuffle_body(rng, &mut rule.body);
    }
    Program {
        rules,
        facts,
        histories,
        ..Program::default()
    }
}

/// Transitive closure over `edges` (already-asserted `[a,b]` pairs), via
/// naive fixpoint iteration — independent of any Datalog evaluator, the
/// reference for `path`.
fn brute_force_closure(edges: &BTreeSet<Tuple>) -> BTreeSet<Tuple> {
    let mut closure = edges.clone();
    loop {
        let mut additions = Vec::new();
        for e1 in &closure {
            for e2 in &closure {
                if e1[1] == e2[0] {
                    let candidate: Tuple = Tuple::from_vec(vec![e1[0].clone(), e2[1].clone()]);
                    if !closure.contains(&candidate) {
                        additions.push(candidate);
                    }
                }
            }
        }
        if additions.is_empty() {
            break;
        }
        closure.extend(additions);
    }
    closure
}

/// The reference for `unreachable`: every `(a,b)` pair over `nodes` NOT in
/// `edges` — the plain set complement negation-over-as_of computes.
fn expected_unreachable(nodes: &[i64], edges: &BTreeSet<Tuple>) -> BTreeSet<Tuple> {
    let mut out = BTreeSet::new();
    for &a in nodes {
        for &b in nodes {
            let t: Tuple = Tuple::from_vec(vec![v(a), v(b)]);
            if !edges.contains(&t) {
                out.insert(t);
            }
        }
    }
    out
}

/// The reference for `deg`: for each source column, how many rows share
/// it.
fn expected_degree(edges: &BTreeSet<Tuple>) -> BTreeSet<Tuple> {
    let mut counts: BTreeMap<DataValue, i64> = BTreeMap::new();
    for e in edges {
        *counts.entry(e[0].clone()).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .map(|(k, c)| vec![k, v(c)])
        .map(Tuple::from_vec)
        .collect()
}

/// The reference for `m`: seed values propagated along `edges` to a
/// fixpoint, folding with the REAL landed meet operator (`meet_op`) at
/// every hop — reusing the exact aggregation semantics production code
/// runs (per this module's own header doc: a bug in an aggregation must
/// never hide behind a parallel test-only reimplementation), while the
/// PROPAGATION LOOP itself is independent of `laws.rs`'s `MeetState`.
fn expected_meet(
    edges: &BTreeSet<Tuple>,
    seeds: &BTreeSet<Tuple>,
    meet_op: &str,
) -> BTreeSet<Tuple> {
    let aggregation = parse_aggr(meet_op).expect("real aggregation");
    let mut acc: BTreeMap<DataValue, MeetAccum> = BTreeMap::new();
    for row in seeds {
        acc.insert(row[0].clone(), MeetAccum::from_derived(row[1].clone()));
    }
    let mut steps = 0usize;
    loop {
        steps += 1;
        assert!(
            steps <= 4 * edges.len() + 4,
            "meet propagation failed to terminate"
        );
        let mut changed = false;
        for edge in edges {
            let (a, b) = (&edge[0], &edge[1]);
            let Some(val) = acc.get(a).cloned() else {
                continue;
            };
            match acc.get(b).cloned() {
                None => {
                    acc.insert(b.clone(), val);
                    changed = true;
                }
                Some(mut cur) => {
                    let op: MeetAggr = aggregation.meet_op().expect("meet-capable aggregation");
                    if op.update(&mut cur, &val).expect("meet update") {
                        acc.insert(b.clone(), cur);
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    acc.into_iter()
        .map(|(k, val)| vec![k, val.to_value()])
        .map(Tuple::from_vec)
        .collect()
}

#[test]
fn temporal_negation_recursion_and_both_aggregation_families_match_independent_references() {
    let mut cases = 0usize;
    let seeds = 400u64;
    for seed in 0..seeds {
        // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
        let mut rng = Rng::new(0xF00D_BA11_u64 ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let fx = gen_reachability_fixture(&mut rng);
        let program = reachability_program(&mut rng, &fx);
        let db = naive_eval(&program).expect(
            "negation over a fixed as-of historical relation is legal (the lift); \
             recursion and both aggregation families over historical leaves are well-formed",
        );

        let edge_snapshot = resolve_relation(&fx.edge_history, fx.c_edge);
        let seed_snapshot = resolve_relation(&fx.seed_history, fx.c_seed);

        let expected_path = brute_force_closure(&edge_snapshot);
        assert_eq!(
            db.get("path").cloned().unwrap_or_default(),
            expected_path,
            "seed {seed}: path (recursion over a historical base)"
        );
        cases += 1;

        let expected_unreach = expected_unreachable(&fx.nodes, &edge_snapshot);
        assert_eq!(
            db.get("unreachable").cloned().unwrap_or_default(),
            expected_unreach,
            "seed {seed}: unreachable (the lifted negation-over-as_of shape)"
        );
        cases += 1;

        let expected_deg = expected_degree(&edge_snapshot);
        assert_eq!(
            db.get("deg").cloned().unwrap_or_default(),
            expected_deg,
            "seed {seed}: deg (normal aggregation over a historical base)"
        );
        cases += 1;

        let expected_m = expected_meet(&edge_snapshot, &seed_snapshot, fx.meet_op);
        assert_eq!(
            db.get("m").cloned().unwrap_or_default(),
            expected_m,
            "seed {seed}: m (meet aggregation, op={}, recursing a historical base)",
            fx.meet_op
        );
        cases += 1;
    }
    assert!(
        cases >= 800,
        "expected hundreds of temporal negation/recursion/aggregation cases, ran {cases}"
    );
}
