// Copyright 2023 The Cozo Project Authors.
// Copyright 2026 The KyzoDB Authors.
//
// This Source Code Form is subject to the terms of the Mozilla Public License,
// v. 2.0. If a copy of the MPL was not distributed with this file, You can
// obtain one at https://mozilla.org/MPL/2.0/.

//! Gauntlet seat — generator + Capability 1 (determinism campaign).
//!
//! Relocated from condemned `kyzo-core::query::trials` per
//! `docs/deprecated/storage-model/01-query-tree.json`. Drives the sanctioned
//! `pub(crate)` eval seams and the sealed oracle (`kyzo_oracle`) as an
//! outside caller would.
//!
//! **"Determinism as a law."** A seed-reproducible generator (splitmix64)
//! emits large stratified programs mixing linear and self-join recursion,
//! stratified negation, normal aggregation, and meet aggregation in every
//! positional layout — suffix, position-0, and interleaved — plus mutual
//! recursion, a non-self-healing two-delta join (`cross_join`), and opaque
//! fixed rules. Node identity payloads are seeded across [`PayloadKind`]
//! (Int / Vector / Geometry) so recursion, negation, and aggregation are
//! not Int-only. Per seed, under a finite [`Budget`]: the answer is
//! differentially checked against the oracle, and the answer set, the
//! witness table, and deliberately budget-exceeding refusals — including
//! mid-epoch [`BudgetDimension::InFlightDerivations`] — are asserted
//! byte-identical across 1/2/4/8 rayon threads (and twice at width 1).
//!
//! ## What this harness does and does not exercise
//!
//! The [`RuleBody`] seam is *post-stratification*. Driving it directly
//! exercises the whole semi-naive stratified fixpoint against the oracle.
//! It does **not** run the magic-set *demand rewriter*.
//!
//! # Carried obligation: demand-rewriter generative gap
//!
//! **Owner:** kyzo-trials gauntlet lane (carried_obligation
//! `demand-rewriter-generative-gap` from 01-query-tree.json).
//!
//! End-to-end demand-rewriter differential exists at the session seam
//! (fixed corpus). The remaining breadth — a **generative** corpus through
//! the public path (magic-vs-bypass over generated programs, the issue-#29
//! NoREC-analog) — stays open and named. See
//! [`demand_rewriter_generative_magic_vs_bypass`] (disclosed `#[ignore]`).
//!
//! The prior issue-#29 magic-sets NoREC gauntlet that occupied this seat
//! is not adapted here (session/`Db`/`parse` surfaces + `crate::` in-tree
//! paths); Cap1 from `query/trials.rs` owns the seat per the cut destiny.
//! When the generative demand arm lands, it reuses this generator's
//! vocabulary and the session public path — not a second parallel RNG.

#![cfg(test)]

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::ops::ControlFlow;
use std::sync::Arc;

use miette::Result;

use kyzo::oracle_harness::{
    AtomOccurrence, Budget, BudgetDimension, EpochStore, EvalDefinition, EvalProgram, EvalRuleSet,
    EvalStratum, FixedRuleEval, LimitExceeded, MagicSymbol, Premises, RegularTempStore, RowLimit,
    RuleBody, Sealed, StoreLifetimes, WitnessTable, collect_materialized, stratified_evaluate,
};
use kyzo_model::SourceSpan;
use kyzo_model::program::aggregate::parse_aggr;
use kyzo_model::program::rule::HeadAggrSlot;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::{DataValue, Geometry, Tuple, Vector};
use kyzo_oracle::{
    Bindings, FixedRule, HeadAggr, Literal, Program, Rel, Rule, Term, ground, head_classes,
    naive_eval, unify,
};

#[cfg(test)]
fn require<T, E: core::fmt::Debug>(r: Result<T, E>, msg: &str) -> T {
    match r {
        Ok(v) => v,
        Err(e) => {
            assert!(false, "{msg}: {e:?}");
            loop {}
        }
    }
}

#[cfg(test)]
fn require_some<T>(o: Option<T>, msg: &str) -> T {
    match o {
        Some(v) => v,
        None => {
            assert!(false, "{msg}");
            loop {}
        }
    }
}

// ════════════════════════════════════════════════════════════════════════
// Seeded RNG — splitmix64; one u64 seed, no ambient entropy.
// ════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod rng_door {
    use super::require;

    pub(crate) struct Rng {
        state: u64,
    }

    impl Rng {
        pub(crate) fn new(seed: u64) -> Self {
            Rng { state: seed }
        }
        pub(crate) fn next_u64(&mut self) -> u64 {
            // INVARIANT(splitmix64): modular mix per the splitmix64 contract; wrap is the PRNG.
            self.state = u64::wrapping_add(self.state, 0x9E37_79B9_7F4A_7C15);
            let mut z = self.state;
            z = u64::wrapping_mul(z ^ (z >> 30), 0xBF58_476D_1CE4_E5B9);
            z = u64::wrapping_mul(z ^ (z >> 27), 0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        /// A value in `[0, n)`. Modulo bias is irrelevant at test scales.
        pub(crate) fn below(&mut self, n: u64) -> u64 {
            assert!(n > 0);
            self.next_u64() % n
        }
        pub(crate) fn range(&mut self, lo: i64, hi: i64) -> i64 {
            assert!(hi > lo);
            let width = require(u64::try_from(hi - lo), "range width fits u64");
            lo + require(i64::try_from(self.below(width)), "range offset fits i64")
        }
        pub(crate) fn chance(&mut self, num: u64, den: u64) -> bool {
            self.below(den) < num
        }
        pub(crate) fn one_of<T: Copy>(&mut self, xs: &[T]) -> T {
            let len = require(u64::try_from(xs.len()), "slice len fits u64");
            assert!(len > 0, "one_of on empty slice");
            xs[require(usize::try_from(self.below(len)), "index fits usize")]
        }
    }
}
pub(crate) use rng_door::Rng;

// ════════════════════════════════════════════════════════════════════════
// Oracle-model RuleBody harness (stand-in for a compiled RA plan).
// ════════════════════════════════════════════════════════════════════════

pub(crate) fn muggle(rel: impl AsRef<str>) -> MagicSymbol {
    MagicSymbol::Muggle {
        inner: Symbol::new(rel.as_ref(), SourceSpan(0, 0)),
    }
}
pub(crate) fn entry_symbol() -> MagicSymbol {
    MagicSymbol::Muggle {
        inner: Symbol::prog_entry(SourceSpan(0, 0)),
    }
}

#[derive(Debug)]
pub(crate) struct ModelBody {
    head: Vec<Term>,
    body: Vec<Literal>,
    facts: Arc<BTreeMap<Rel, BTreeSet<Tuple>>>,
    idb: Arc<BTreeSet<Rel>>,
    contained: BTreeMap<AtomOccurrence, MagicSymbol>,
}

impl ModelBody {
    pub(crate) fn new(
        head: Vec<Term>,
        body: Vec<Literal>,
        facts: Arc<BTreeMap<Rel, BTreeSet<Tuple>>>,
        idb: Arc<BTreeSet<Rel>>,
    ) -> Self {
        let mut contained: BTreeMap<AtomOccurrence, MagicSymbol> = BTreeMap::new();
        for (i, l) in body.iter().enumerate() {
            if idb.contains(&l.rel) {
                contained.insert(AtomOccurrence(i), muggle(&l.rel));
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
        if self.idb.contains(&rel) {
            let store = require_some(stores.get(&muggle(&rel)), "harness: IDB store present");
            if use_delta {
                require(
                    collect_materialized(require(store.delta_all_iter(), "harness: store iter")),
                    "harness: materialize",
                )
            } else {
                require(
                    collect_materialized(require(store.all_iter(), "harness: store iter")),
                    "harness: materialize",
                )
            }
        } else {
            match self.facts.get(&rel) {
                Some(set) => set.iter().cloned().collect(),
                None => Vec::new(),
            }
        }
    }

    fn negated_probe_hits(
        &self,
        stores: &BTreeMap<MagicSymbol, EpochStore>,
        rel: Rel,
        probe: &Tuple,
    ) -> bool {
        if self.idb.contains(&rel) {
            let store = require_some(stores.get(&muggle(&rel)), "harness: IDB store present");
            require(store.prefix_iter(probe), "harness: store iter")
                .next()
                .is_some_and(|t| match t.try_into_tuple() {
                    Ok(tup) => &tup == probe,
                    Err(_) => false,
                })
        } else {
            self.facts.get(&rel).is_some_and(|set| set.contains(probe))
        }
    }
}

// Sealed seam: story-#80 style; external trials reach seal via `oracle_harness`.
impl Sealed for ModelBody {}

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
                    if !self.negated_probe_hits(stores, l.rel.clone(), &probe) {
                        next.push((bound.clone(), premises.clone()));
                    }
                }
            } else {
                let rows = self.rows_of(stores, l.rel.clone(), is_delta);
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

pub(crate) struct ModelFixed {
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
                    require(
                        collect_materialized(require(
                            require_some(
                                stores.get(&muggle(rel)),
                                "harness: fixed input store present",
                            )
                            .all_iter(),
                            "harness: store iter",
                        )),
                        "harness: materialize",
                    )
                    .into_iter()
                    .collect()
                } else {
                    match self.facts.get(rel) {
                        Some(set) => set.clone(),
                        None => BTreeSet::new(),
                    }
                }
            })
            .collect();
        for row in (self.eval)(&inputs) {
            out.put(row);
        }
        Ok(())
    }
}

fn dependency_edges(program: &Program) -> Vec<(Rel, Rel, bool)> {
    let classes = head_classes(program);
    let fixed_heads: BTreeSet<Rel> = program.fixed.iter().map(|f| f.head_rel.clone()).collect();
    let is_meet = |rel: &Rel| classes.get(rel).is_some_and(|c| c.is_meet);
    let mut edges = Vec::new();
    for rule in &program.rules {
        let head = rule.head_rel.clone();
        let class = &classes[&head];
        for l in &rule.body {
            let forcing = if class.has_aggr {
                if class.is_meet && l.rel == head {
                    l.is_negated()
                } else {
                    true
                }
            } else {
                l.is_negated() || fixed_heads.contains(&l.rel) || is_meet(&l.rel)
            };
            edges.push((head.clone(), l.rel.clone(), forcing));
        }
    }
    for f in &program.fixed {
        for dep in &f.inputs {
            edges.push((f.head_rel.clone(), dep.clone(), true));
        }
    }
    edges
}

fn strata_of(program: &Program) -> BTreeMap<Rel, usize> {
    let edges = dependency_edges(program);
    let mut s: BTreeMap<Rel, usize> = BTreeMap::new();
    for rule in &program.rules {
        s.insert(rule.head_rel.clone(), 0);
        for l in &rule.body {
            s.insert(l.rel.clone(), 0);
        }
    }
    for f in &program.fixed {
        s.insert(f.head_rel.clone(), 0);
        for i in &f.inputs {
            s.insert(i.clone(), 0);
        }
    }
    for rel in program.facts.keys() {
        s.insert(rel.clone(), 0);
    }
    let bound = s.len() + 1;
    for _ in 0..bound {
        let mut changed = false;
        for (head, dep, forcing) in &edges {
            let need = s[dep] + usize::from(*forcing);
            if s[head] < need {
                s.insert(head.clone(), need);
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

pub(crate) struct Compiled {
    pub(crate) program: EvalProgram<ModelBody, ModelFixed>,
    pub(crate) lifetimes: StoreLifetimes,
}

#[cfg(test)]
/// Oracle [`HeadAggr`] → engine [`HeadAggrSlot`] at the compile boundary
/// (AggrFold injection: name resolves through `parse_aggr`).
fn to_engine_aggr(slot: &HeadAggr) -> HeadAggrSlot {
    match slot {
        HeadAggr::Plain => HeadAggrSlot::Plain,
        HeadAggr::Aggregated { fold, args } => {
            let aggr = match parse_aggr(fold.name()) {
                Ok(Some(a)) => a,
                Ok(None) | Err(_) => {
                    assert!(
                        false,
                        "engine aggregation exists for oracle fold {}",
                        fold.name()
                    );
                    loop {}
                }
            };
            HeadAggrSlot::Aggregated {
                aggr,
                args: args.clone(),
            }
        }
    }
}

#[cfg(test)]
/// Compile the oracle model for evaluation with `target` as the entry rule.
pub(crate) fn compile_for(
    model: &Program,
    target: impl Into<Rel>,
    target_arity: usize,
    fixed_arities: &BTreeMap<Rel, usize>,
) -> Compiled {
    let target = target.into();
    let idb: Arc<BTreeSet<Rel>> = Arc::new(
        model
            .rules
            .iter()
            .map(|r| r.head_rel.clone())
            .chain(model.fixed.iter().map(|f| f.head_rel.clone()))
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
    let entry_stratum = match strata_map.values().copied().max() {
        Some(m) => m + 1,
        None => 1,
    };

    let mut strata: Vec<EvalStratum<ModelBody, ModelFixed>> = (0..=entry_stratum)
        .map(|_| EvalStratum {
            defs: BTreeMap::new(),
        })
        .collect();
    let mut lifetimes = StoreLifetimes::empty();

    let mut heads_in_order: Vec<Rel> = Vec::new();
    let mut per_head: BTreeMap<Rel, Vec<&Rule>> = BTreeMap::new();
    for rule in &model.rules {
        if !per_head.contains_key(&rule.head_rel) {
            heads_in_order.push(rule.head_rel.clone());
        }
        per_head
            .entry(rule.head_rel.clone())
            .or_default()
            .push(rule);
    }
    for head in heads_in_order {
        let rules = &per_head[&head];
        let stratum = strata_map[&head];
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
        let engine_aggr: Vec<HeadAggrSlot> = rules[0].aggr.iter().map(to_engine_aggr).collect();
        let rule_set = require(EvalRuleSet::new(engine_aggr, bodies), "well-shaped set");
        strata[stratum]
            .defs
            .insert(muggle(&head), EvalDefinition::Rules(rule_set));
    }
    for f in &model.fixed {
        let stratum = strata_map[&f.head_rel];
        for input in &f.inputs {
            if idb.contains(input) {
                lifetimes.note_use(muggle(input), stratum);
            }
        }
        strata[stratum].defs.insert(
            muggle(&f.head_rel),
            EvalDefinition::Fixed {
                arity: match fixed_arities.get(&f.head_rel).copied() {
                    Some(a) => a,
                    None => {
                        assert!(
                            false,
                            "fixed head {} missing from fixed_arities",
                            f.head_rel
                        );
                        loop {}
                    }
                },
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
        .map(|s| Term::Var(s.into()))
        .collect();
    let entry_body = ModelBody::new(
        vars.clone(),
        vec![Literal::pos(target.clone(), vars)],
        facts.clone(),
        idb.clone(),
    );
    lifetimes.note_use(muggle(&target), entry_stratum);
    let entry_set = require(
        EvalRuleSet::new(
            std::iter::repeat_n(HeadAggrSlot::Plain, target_arity).collect(),
            vec![entry_body],
        ),
        "entry rule set",
    );
    strata[entry_stratum]
        .defs
        .insert(entry_symbol(), EvalDefinition::Rules(entry_set));

    let program = require(EvalProgram::from_execution_order(strata), "entry in final stratum");
    Compiled { program, lifetimes }
}

#[cfg(test)]
/// Relation arities from the MODEL alone (never from oracle output).
pub(crate) fn model_arities(model: &Program) -> BTreeMap<Rel, usize> {
    fn note(arities: &mut BTreeMap<Rel, usize>, rel: Rel, n: usize) {
        match arities.entry(rel) {
            std::collections::btree_map::Entry::Vacant(e) => {
                e.insert(n);
            }
            std::collections::btree_map::Entry::Occupied(o) => {
                assert_eq!(*o.get(), n, "model uses '{}' at two arities", o.key());
            }
        }
    }
    let mut arities = BTreeMap::new();
    for r in &model.rules {
        note(&mut arities, r.head_rel.clone(), r.head_args.len());
        for l in &r.body {
            note(&mut arities, l.rel.clone(), l.args.len());
        }
    }
    for (rel, rows) in &model.facts {
        if let Some(t) = rows.first() {
            note(&mut arities, rel.clone(), t.len());
        }
    }
    arities
}

pub(crate) fn fixed_arities_of(
    model: &Program,
    arities: &BTreeMap<Rel, usize>,
) -> BTreeMap<Rel, usize> {
    model
        .fixed
        .iter()
        .map(|f| (f.head_rel.clone(), arities[&f.head_rel]))
        .collect()
}

pub(crate) fn idb_of(model: &Program) -> BTreeSet<Rel> {
    model
        .rules
        .iter()
        .map(|r| r.head_rel.clone())
        .chain(model.fixed.iter().map(|f| f.head_rel.clone()))
        .collect()
}

/// Evaluate `target` through the real engine, returning its rows.
pub(crate) fn real_eval(
    model: &Program,
    target: impl Into<Rel>,
    target_arity: usize,
    fixed_arities: &BTreeMap<Rel, usize>,
    budget: &Budget,
) -> Result<BTreeSet<Tuple>> {
    let target = target.into();
    let compiled = compile_for(model, target.clone(), target_arity, fixed_arities);
    let outcome = stratified_evaluate(
        &compiled.program,
        &compiled.lifetimes,
        RowLimit::unlimited(),
        budget,
        None,
    )?;
    Ok(collect_materialized(outcome.store.all_iter()?)?
        .into_iter()
        .collect())
}

pub(crate) fn generous_budget() -> Budget {
    Budget::new(require_some(NonZeroU32::new(10_000), "10_000 is non-zero"))
}

pub(crate) fn v(i: i64) -> DataValue {
    DataValue::from(i)
}

/// Seeded identity kind for nodes that flow through recursion / negation /
/// aggregation. Meet-folded and `sum` columns stay numeric/bool via
/// [`meet_value`] / [`v`] — those folds refuse Vector/Geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PayloadKind {
    Int,
    Vector,
    Geometry,
}

const PAYLOAD_KINDS: [PayloadKind; 3] =
    [PayloadKind::Int, PayloadKind::Vector, PayloadKind::Geometry];

pub(crate) fn choose_payload_kind(rng: &mut Rng) -> PayloadKind {
    rng.one_of(&PAYLOAD_KINDS)
}

/// Identity-stable payload for node index `i` under `kind` — joins unify.
pub(crate) fn node_payload(kind: PayloadKind, i: i64) -> DataValue {
    match kind {
        PayloadKind::Int => v(i),
        PayloadKind::Vector => {
            let fi = f64::from(require(i32::try_from(i), "node index fits i32"));
            let fj = f64::from(require(
                // INVARIANT(SeedMix): identity-stable payload scale; wrap is intentional diffusion.
                i32::try_from(i64::wrapping_mul(i, 3)),
                "scaled node index fits i32",
            ));
            DataValue::Vector(require_some(
                Vector::try_new(vec![fi, fj]),
                "gauntlet: vector dim fits u32",
            ))
        }
        PayloadKind::Geometry => {
            let lat = require(u32::try_from(i), "node index fits u32");
            // INVARIANT(SeedMix): identity-stable cell mix; wrap is intentional diffusion.
            let lon = u32::wrapping_add(u32::wrapping_mul(lat, 7), 1);
            DataValue::Geometry(Geometry::from_cells(lat, lon))
        }
    }
}

pub(crate) fn x() -> Term {
    Term::Var("X".into())
}
pub(crate) fn y() -> Term {
    Term::Var("Y".into())
}
pub(crate) fn z() -> Term {
    Term::Var("Z".into())
}
pub(crate) fn lit(rel: impl Into<Rel>, args: Vec<Term>, negated: bool) -> Literal {
    let rel = rel.into();
    if negated {
        Literal::neg(rel, args)
    } else {
        Literal::pos(rel, args)
    }
}
pub(crate) fn named(name: &str) -> HeadAggr {
    HeadAggr::named(name)
}

// ════════════════════════════════════════════════════════════════════════
// The generator: one u64 seed → one large, stratified, safe program.
// ════════════════════════════════════════════════════════════════════════

/// Meet lattices available through the oracle [`HeadAggr`] / [`builtin_fold`]
/// seam. DEVIATION from condemned trials.rs: `"union"` omitted until trials
/// injects the engine's `exec/fold/aggr` folds through [`kyzo_oracle::AggrFold`]
/// (oracle `builtin_fold` has no union).
pub(crate) const MEET_OPS: [&str; 4] = ["min", "max", "and", "or"];

#[derive(Debug, Clone)]
struct GenParams {
    n_nodes: i64,
    /// Node identity kind for edge/node/seed keys (recursion / negation /
    /// aggregation group keys). Not Int-only.
    payload_kind: PayloadKind,
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
    meet_pos0: bool,
    meet_interleaved: bool,
}

fn gen_params(rng: &mut Rng, payload_kind: PayloadKind) -> GenParams {
    GenParams {
        n_nodes: rng.range(6, 15),
        payload_kind,
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

#[cfg(test)]
fn meet_value(rng: &mut Rng, op: &str) -> DataValue {
    match op {
        "and" | "or" => DataValue::from(rng.chance(1, 2)),
        "min" | "max" => v(rng.range(-10, 10)),
        other => {
            assert!(false, "unknown meet op {other}");
            v(0)
        }
    }
}

/// A generated program plus the metadata the campaign and provenance code need.
pub(crate) struct Generated {
    pub(crate) program: Program,
    pub(crate) entry: Rel,
    pub(crate) entry_arity: usize,
}

pub(crate) fn generate(seed: u64) -> Generated {
    // Payload kind from an independent stream so Cap1's structural/sizing
    // draws (and the substantial-EDB law) stay seed-stable; kind still
    // varies across seeds and is what Cap1 actually materializes.
    let payload_kind = choose_payload_kind(&mut Rng::new(
        // INVARIANT(SeedMix): independent stream mix; wrap is the published seed contract.
        u64::wrapping_add(u64::wrapping_mul(seed, 0xA24B_AED4_96E9_F2D9), 1),
    ));
    let mut rng = Rng::new(seed);
    let p = gen_params(&mut rng, payload_kind);
    let n = p.n_nodes;
    let pk = p.payload_kind;

    let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();

    let n_edges = require(
        i64::try_from(rng.below(require(u64::try_from(n * 3), "n*3 fits u64"))),
        "edge count fits i64",
    ) + 1;
    let edges: BTreeSet<Tuple> = (0..n_edges)
        .map(|_| {
            vec![
                node_payload(pk, rng.range(0, n)),
                node_payload(pk, rng.range(0, n)),
            ]
        })
        .map(Tuple::from_vec)
        .collect();
    facts.insert("edge".into(), edges);
    facts.insert(
        "node".into(),
        (0..n)
            .map(|i| vec![node_payload(pk, i)])
            .map(Tuple::from_vec)
            .collect(),
    );

    let n_seeds = require(
        i64::try_from(rng.below(require(u64::try_from(n), "n fits u64"))),
        "seed count fits i64",
    ) + 1;
    let seeds: BTreeSet<Tuple> = (0..n_seeds)
        .map(|_| {
            vec![
                node_payload(pk, rng.range(0, n)),
                meet_value(&mut rng, p.meet_op),
            ]
        })
        .map(Tuple::from_vec)
        .collect();
    facts.insert("seed".into(), seeds);

    // `sum` refuses non-numeric payloads — item columns stay Int via `v`.
    let n_items = rng.range(800, 3000);
    let n_keys = rng.range(20, 100);
    let items: BTreeSet<Tuple> = (0..n_items)
        .map(|_| vec![v(rng.range(0, n_keys)), v(rng.range(0, 50))])
        .map(Tuple::from_vec)
        .collect();
    facts.insert("item".into(), items);

    let mut rules: Vec<Rule> = Vec::new();

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

    if p.meet_pos0 {
        rules.push(Rule::aggregated(
            "m",
            vec![y(), x()],
            vec![named(p.meet_op), HeadAggr::Plain],
            vec![lit("seed", vec![x(), y()], false)],
        ));
        rules.push(Rule::aggregated(
            "m",
            vec![z(), y()],
            vec![named(p.meet_op), HeadAggr::Plain],
            vec![
                lit("edge", vec![x(), y()], false),
                lit("m", vec![z(), x()], false),
            ],
        ));
    } else {
        rules.push(Rule::aggregated(
            "m",
            vec![x(), y()],
            vec![HeadAggr::Plain, named(p.meet_op)],
            vec![lit("seed", vec![x(), y()], false)],
        ));
        rules.push(Rule::aggregated(
            "m",
            vec![y(), z()],
            vec![HeadAggr::Plain, named(p.meet_op)],
            vec![
                lit("edge", vec![x(), y()], false),
                lit("m", vec![x(), z()], false),
            ],
        ));
    }

    if p.meet_interleaved {
        let n_s3 = require(
            i64::try_from(rng.below(require(u64::try_from(n), "n fits u64"))),
            "seed3 count fits i64",
        ) + 1;
        let seed3: BTreeSet<Tuple> = (0..n_s3)
            .map(|_| {
                vec![
                    node_payload(pk, rng.range(0, n)),
                    v(rng.range(-10, 10)),
                    v(rng.range(-10, 10)),
                ]
            })
            .map(Tuple::from_vec)
            .collect();
        facts.insert("seed3".into(), seed3);
        let (lo, k, hi, s, t) = (
            Term::Var("Lo".into()),
            Term::Var("K".into()),
            Term::Var("Hi".into()),
            Term::Var("S".into()),
            Term::Var("T".into()),
        );
        rules.push(Rule::aggregated(
            "mi",
            vec![lo.clone(), k.clone(), hi.clone()],
            vec![named("min"), HeadAggr::Plain, named("max")],
            vec![lit("seed3", vec![k.clone(), lo.clone(), hi.clone()], false)],
        ));
        rules.push(Rule::aggregated(
            "mi",
            vec![lo.clone(), t.clone(), hi.clone()],
            vec![named("min"), HeadAggr::Plain, named("max")],
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
        // Non-recursive join of TWO separately-derived recursive stores —
        // delta-discipline discriminator (no repair rule to mask the mutant).
        rules.push(Rule::plain(
            "j",
            vec![x(), z()],
            vec![
                lit("qa", vec![x(), y()], false),
                lit("qb", vec![y(), z()], false),
            ],
        ));
    }

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
            vec![HeadAggr::Plain, named("count")],
            vec![lit("path", vec![x(), y()], false)],
        ));
    }
    if p.bulk_aggr {
        rules.push(Rule::aggregated(
            "bulk_sum",
            vec![x(), y()],
            vec![HeadAggr::Plain, named("sum")],
            vec![lit("item", vec![x(), y()], false)],
        ));
    }

    let mut fixed = Vec::new();
    if p.fixed_rule {
        fixed.push(FixedRule {
            head_rel: "fx".into(),
            inputs: vec!["path".into()],
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
        entry: "path".into(),
        entry_arity: 2,
    }
}

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

/// Near-cross-product shaped so mid-epoch [`BudgetDimension::InFlightDerivations`]
/// fires before the barrier. Cap1's main generator tops out below the
/// mid-epoch interrupt stride (64) in edge cardinality, so InFlight is
/// unreachable there — this companion closes that structural hole with a
/// seeded payload kind.
pub(crate) fn generate_in_flight_probe(seed: u64) -> Generated {
    // Independent stream from [`generate`] so Cap1's main corpus RNG is
    // not coupled to the in-flight arm's sizing draws.
    // INVARIANT(SeedMix): independent stream mix; wrap is the published seed contract.
    let mut rng = Rng::new(u64::wrapping_add(u64::wrapping_mul(seed, 0xD2B7_44F4_5AD5_5A5A), 1));
    let kind = choose_payload_kind(&mut rng);
    // 70..=119 per side → product ≫ ceiling+stride; one join epoch trips
    // the mid-epoch guard (stride 64) before DerivedTuples at the barrier.
    let a_n = 70 + require(i64::try_from(rng.below(50)), "a_n offset fits i64");
    let b_n = 70 + require(i64::try_from(rng.below(50)), "b_n offset fits i64");
    let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = BTreeMap::new();
    facts.insert(
        "a".into(),
        (0..a_n)
            .map(|i| Tuple::from_vec(vec![node_payload(kind, i)]))
            .collect(),
    );
    facts.insert(
        "b".into(),
        (0..b_n)
            .map(|i| Tuple::from_vec(vec![node_payload(kind, i)]))
            .collect(),
    );
    let rules = vec![Rule::plain(
        "prod",
        vec![x(), y()],
        vec![lit("a", vec![x()], false), lit("b", vec![y()], false)],
    )];
    Generated {
        program: Program::untimed(rules, vec![], facts),
        entry: "prod".into(),
        entry_arity: 2,
    }
}

// ════════════════════════════════════════════════════════════════════════
// CAPABILITY 1 — the determinism campaign.
// ════════════════════════════════════════════════════════════════════════

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
fn at_thread_count<T: Send>(threads: usize, f: impl FnOnce() -> T + Send) -> T {
    let pool = require(
        rayon::ThreadPoolBuilder::new().num_threads(threads).build(),
        "thread pool",
    );
    pool.install(|| {
        assert_eq!(
            rayon::current_num_threads(),
            threads,
            "rayon pool width mismatch"
        );
        f()
    })
}

fn render_witnesses(table: &WitnessTable) -> Vec<String> {
    table.entries().iter().map(|w| format!("{w:?}")).collect()
}

fn differential(model: &Program) -> Option<String> {
    let oracle_db = match naive_eval(model) {
        Ok(db) => db,
        Err(rej) => return Some(format!("oracle refused a generated program: {rej:?}")),
    };
    let arities = model_arities(model);
    let fixed_arities = fixed_arities_of(model, &arities);
    for rel in idb_of(model) {
        let oracle_rows = match oracle_db.get(&rel) {
            Some(rows) => rows.clone(),
            None => BTreeSet::new(),
        };
        let arity = arities[&rel];
        let real_rows = match real_eval(
            model,
            rel.clone(),
            arity,
            &fixed_arities,
            &generous_budget(),
        ) {
            Ok(rows) => rows,
            Err(e) => {
                return Some(format!("real eval failed for '{rel}': {e}"));
            }
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

#[cfg(not(target_arch = "wasm32"))]
fn eval_fingerprint(g: &Generated, threads: usize) -> (BTreeSet<Tuple>, Vec<String>) {
    at_thread_count(threads, || {
        let arities = model_arities(&g.program);
        let fixed_arities = fixed_arities_of(&g.program, &arities);
        let compiled = compile_for(&g.program, g.entry.clone(), g.entry_arity, &fixed_arities);
        let mut table = WitnessTable::new();
        let outcome = require(
            stratified_evaluate(
                &compiled.program,
                &compiled.lifetimes,
                RowLimit::unlimited(),
                &generous_budget(),
                Some(&mut table),
            ),
            "evaluates",
        );
        let rows: BTreeSet<Tuple> = require(
            collect_materialized(require(outcome.store.all_iter(), "harness: store iter")),
            "harness: materialize",
        )
        .into_iter()
        .collect();
        (rows, render_witnesses(&table))
    })
}

/// Refusal identity: message, dimension, spend, ceiling, and (for mid-epoch
/// InFlight) the offending rule name + span — the fields most likely to
/// diverge when thread width changes the interrupt order.
type RefusalFp = (
    String,
    BudgetDimension,
    u64,
    u64,
    Option<String>,
    Option<SourceSpan>,
);

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
fn refusal_fingerprint(g: &Generated, budget: &Budget, threads: usize) -> RefusalFp {
    at_thread_count(threads, || {
        let arities = model_arities(&g.program);
        let fixed_arities = fixed_arities_of(&g.program, &arities);
        let compiled = compile_for(&g.program, g.entry.clone(), g.entry_arity, &fixed_arities);
        let err = match stratified_evaluate(
            &compiled.program,
            &compiled.lifetimes,
            RowLimit::unlimited(),
            budget,
            None,
        ) {
            Err(e) => e,
            Ok(_) => {
                assert!(false, "must refuse");
                loop {}
            }
        };
        let refusal: &LimitExceeded =
            require_some(err.downcast_ref(), "typed LimitExceeded");
        (
            err.to_string(),
            refusal.dimension,
            refusal.spent,
            refusal.ceiling,
            refusal
                .rule
                .as_ref()
                .map(|s| s.as_plain_symbol().name.to_string()),
            refusal.span,
        )
    })
}

/// Run the full battery for one seed.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn run_seed(seed: u64) -> std::result::Result<(), String> {
    let g = generate(seed);

    if let Some(disagreement) = differential(&g.program) {
        return Err(format!("differential: {disagreement}"));
    }

    let baseline = eval_fingerprint(&g, 1);
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

    let epoch_budget = Budget::new(require_some(NonZeroU32::new(1), "1 is non-zero"));
    check_refusal(&g, &epoch_budget, BudgetDimension::Epochs)?;

    let derived_budget = generous_budget().with_derived_tuple_ceiling(1);
    check_refusal(&g, &derived_budget, BudgetDimension::DerivedTuples)?;

    // InFlightDerivations: companion cross-product (main generator stays
    // below INTERRUPT_STRIDE). Same seed twice at width 1, then 2/4/8.
    let inflight = generate_in_flight_probe(seed);
    let inflight_budget = generous_budget().with_derived_tuple_ceiling(100);
    check_refusal(
        &inflight,
        &inflight_budget,
        BudgetDimension::InFlightDerivations,
    )?;

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
    // Same seed / width twice — catches non-thread ambient divergence
    // (the hazard dst multihead proved real for parallel seams).
    let again = refusal_fingerprint(g, budget, 1);
    if again != baseline {
        return Err(format!(
            "{expect:?} refusal diverged on repeat at 1 thread: {again:?} vs {baseline:?}"
        ));
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

fn seed_count() -> u64 {
    crate::campaign::env_u64("KYZO_TRIALS_SEEDS", 24)
}

fn seed_base() -> u64 {
    crate::campaign::env_u64("KYZO_TRIALS_BASE", 0)
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn determinism_campaign() {
    let count = seed_count();
    let failures = crate::campaign::run_seed_campaign(seed_base(), count, run_seed);
    assert!(
        failures.is_empty(),
        "determinism campaign FINDINGS ({} of {count}): {failures:?}",
        failures.len()
    );
}

#[test]
fn generator_is_seed_reproducible() {
    let a = generate(42);
    let b = generate(42);
    assert_eq!(a.program.facts, b.program.facts);
    assert_eq!(a.program.rules.len(), b.program.rules.len());
    assert_eq!(a.entry, b.entry);
    let total: usize = a.program.facts.values().map(|s| s.len()).sum();
    assert!(total > 800, "generated EDB is substantial: {total} tuples");
}

fn fact_has_kind(g: &Generated, pred: impl Fn(&DataValue) -> bool) -> bool {
    g.program
        .facts
        .values()
        .flat_map(|rows| rows.iter())
        .flat_map(|t| t.iter())
        .any(pred)
}

/// Anti-vacuity: Cap1's generator must actually emit Vector and Geometry
/// node payloads into edge/node facts (the recursion / negation / aggr keys)
/// for some seeds — not merely offer an unused chooser.
#[test]
fn generator_emits_vector_and_geometry_payloads_for_some_seeds() {
    let mut saw_vector = false;
    let mut saw_geometry = false;
    let mut saw_int = false;
    for i in 0..96u64 {
        // INVARIANT(SeedMix): property-test seed diffusion uses modular golden mix.
        let seed = Rng::new(u64::wrapping_mul(i, 0x9E37_79B9_7F4A_7C15)).next_u64();
        let g = generate(seed);
        // edge feeds path recursion; node feeds negation; seed keys feed meet.
        let edge: Rel = "edge".into();
        let node: Rel = "node".into();
        let edge_nodes = g
            .program
            .facts
            .get(&edge)
            .into_iter()
            .chain(g.program.facts.get(&node))
            .flat_map(|rows| rows.iter())
            .flat_map(|t| t.iter());
        for val in edge_nodes {
            match val {
                DataValue::Vector(_) => saw_vector = true,
                DataValue::Geometry(_) => saw_geometry = true,
                DataValue::Num(_) => saw_int = true,
                DataValue::Null
                | DataValue::Bool(_)
                | DataValue::Str(_)
                | DataValue::Bytes(_)
                | DataValue::Uuid(_)
                | DataValue::Regex(_)
                | DataValue::Json(_)
                | DataValue::List(_)
                | DataValue::Set(_)
                | DataValue::Validity(_)
                | DataValue::Interval(_) => {}
            }
        }
        if saw_vector && saw_geometry && saw_int {
            break;
        }
    }
    assert!(
        saw_int,
        "Int payload kind never appeared in edge/node facts"
    );
    assert!(
        saw_vector,
        "Vector payload kind never appeared in edge/node facts (chooser unused?)"
    );
    assert!(
        saw_geometry,
        "Geometry payload kind never appeared in edge/node facts (chooser unused?)"
    );
}

/// Anti-vacuity: the InFlight companion must refuse on that dimension (not
/// silently fall through to barrier DerivedTuples), and must carry Vector/
/// Geometry for some seeds (same chooser Cap1 uses).
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn in_flight_probe_refuses_in_flight_derivations() {
    let mut saw_vector = false;
    let mut saw_geometry = false;
    for i in 0..48u64 {
        let seed =
            // INVARIANT(SeedMix): property-test seed diffusion uses modular golden mix.
            Rng::new(u64::wrapping_mul(u64::wrapping_add(0x1F_u64, i), 0x9E37_79B9_7F4A_7C15)).next_u64();
        let g = generate_in_flight_probe(seed);
        saw_vector |= fact_has_kind(&g, |v| matches!(v, DataValue::Vector(_)));
        saw_geometry |= fact_has_kind(&g, |v| matches!(v, DataValue::Geometry(_)));
        let budget = generous_budget().with_derived_tuple_ceiling(100);
        let fp = refusal_fingerprint(&g, &budget, 1);
        assert_eq!(
            fp.1,
            BudgetDimension::InFlightDerivations,
            "seed {seed}: expected InFlightDerivations, got {:?}",
            fp.1
        );
        assert!(fp.4.is_some(), "InFlight refusal must name a rule");
        assert!(fp.5.is_some(), "InFlight refusal must carry a span");
    }
    assert!(
        saw_vector,
        "InFlight probe never carried Vector (chooser unused on companion?)"
    );
    assert!(
        saw_geometry,
        "InFlight probe never carried Geometry (chooser unused on companion?)"
    );
}

/// Generative magic-vs-bypass (demand rewriter) — standing disclosed red.
///
/// Cap1's [`RuleBody`] seam is post-stratification and does not exercise
/// magic-set demand rewriting. Fixed-corpus differential lives at the
/// session seam. Generative breadth needs either:
/// - `stratified_magic_compile` / adornment-rewrite exported on
///   `kyzo::oracle_harness`, or
/// - an Engine/`run_script` generative path that toggles magic vs bypass
///   over this lane's generator vocabulary.
#[test]
#[ignore = "unbuilt seat: kyzo::oracle_harness lacks stratified_magic_compile/magic_rewrite; generative magic-vs-bypass needs that public door or Engine script generative path (demand-rewriter-generative-gap)"]
fn demand_rewriter_generative_magic_vs_bypass() {
    let _seed_vocab = generate(0xDE_9A_7D);
    assert!(
        false,
        "demand-rewriter generative arm: wire oracle_harness magic compile \
         or Engine generative magic-vs-bypass before un-ignoring"
    );
}
