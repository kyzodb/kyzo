/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Reference semantics: program model, checkers, naive stratified fixpoint.
//!
//! Deliberately naive — obviously correct by inspection. Aggregation folds
//! through [`crate::AggrFold`]; oracle budget is local ([`OracleBudget`]).

use std::borrow::Borrow;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::num::NonZeroU32;
use std::ops::Deref;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kyzo_model::value::{DataValue, Tuple};

use crate::temporal::{AsOf, Event, resolve_relation};
use crate::{AggrFold, MeetAccum, MeetOp, NormalAccum, fold_named};

/// Oracle relation / variable name. Content-eq `Arc<str>`.
#[derive(Clone, Debug)]
pub struct Name(Arc<str>);

impl Name {
    pub fn owned(s: impl AsRef<str>) -> Self {
        Self(Arc::from(s.as_ref()))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl From<&'static str> for Name {
    fn from(s: &'static str) -> Self {
        Self(Arc::from(s))
    }
}

impl PartialEq for Name {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}
impl Eq for Name {}
impl PartialOrd for Name {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Name {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}
impl Hash for Name {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}
impl Deref for Name {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}
impl AsRef<str> for Name {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}
impl Borrow<str> for Name {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}
impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl PartialEq<str> for Name {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}
impl PartialEq<&str> for Name {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

pub type Rel = Name;

#[derive(Clone, Debug, PartialEq)]
pub enum Term {
    Var(Name),
    Const(DataValue),
}

impl Term {
    pub fn var(name: impl Into<Name>) -> Self {
        Term::Var(name.into())
    }
}

/// Body-literal polarity: positive read vs negation gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Polarity {
    Positive,
    Negative,
}

#[derive(Clone, Debug)]
pub struct Literal {
    pub rel: Rel,
    pub args: Vec<Term>,
    pub polarity: Polarity,
    pub as_of: Option<AsOf>,
}

impl Literal {
    pub fn is_negated(&self) -> bool {
        matches!(self.polarity, Polarity::Negative)
    }

    pub fn pos(rel: impl Into<Rel>, args: Vec<Term>) -> Self {
        Literal {
            rel: rel.into(),
            args,
            polarity: Polarity::Positive,
            as_of: None,
        }
    }

    pub fn neg(rel: impl Into<Rel>, args: Vec<Term>) -> Self {
        Literal {
            rel: rel.into(),
            args,
            polarity: Polarity::Negative,
            as_of: None,
        }
    }

    pub fn pos_at(rel: impl Into<Rel>, args: Vec<Term>, at: AsOf) -> Self {
        Literal {
            rel: rel.into(),
            args,
            polarity: Polarity::Positive,
            as_of: Some(at),
        }
    }

    pub fn neg_at(rel: impl Into<Rel>, args: Vec<Term>, at: AsOf) -> Self {
        Literal {
            rel: rel.into(),
            args,
            polarity: Polarity::Negative,
            as_of: Some(at),
        }
    }
}

/// One head position's aggregation slot — oracle-local, keyed by [`AggrFold`].
#[derive(Clone)]
pub enum HeadAggr {
    Plain,
    Aggregated {
        fold: Arc<dyn AggrFold>,
        args: Vec<DataValue>,
    },
}

impl fmt::Debug for HeadAggr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeadAggr::Plain => write!(f, "Plain"),
            HeadAggr::Aggregated { fold, args } => {
                write!(
                    f,
                    "Aggregated {{ name: {:?}, args: {:?} }}",
                    fold.name(),
                    args
                )
            }
        }
    }
}

impl PartialEq for HeadAggr {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (HeadAggr::Plain, HeadAggr::Plain) => true,
            (
                HeadAggr::Aggregated { fold: a, args: aa },
                HeadAggr::Aggregated { fold: b, args: ba },
            ) => a.name() == b.name() && aa == ba,
            (HeadAggr::Plain, HeadAggr::Aggregated { .. })
            | (HeadAggr::Aggregated { .. }, HeadAggr::Plain) => false,
        }
    }
}
impl Eq for HeadAggr {}

impl HeadAggr {
    pub fn is_aggregated(&self) -> bool {
        matches!(self, HeadAggr::Aggregated { .. })
    }

    pub fn as_aggregated(&self) -> Option<(&dyn AggrFold, &[DataValue])> {
        match self {
            HeadAggr::Plain => None,
            HeadAggr::Aggregated { fold, args } => Some((fold.as_ref(), args)),
        }
    }

    /// Built-in fold by name (module tests / corpus). Unknown names construct
    /// a refuse-on-use fold — never panics; evaluation yields [`Rejection::AggrError`].
    pub fn named(name: &str) -> Self {
        HeadAggr::Aggregated {
            fold: fold_named(name),
            args: vec![],
        }
    }
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub head_rel: Rel,
    pub head_args: Vec<Term>,
    pub aggr: Vec<HeadAggr>,
    pub body: Vec<Literal>,
}

impl Rule {
    pub fn plain(head_rel: impl Into<Rel>, head_args: Vec<Term>, body: Vec<Literal>) -> Self {
        let aggr = (0..head_args.len()).map(|_| HeadAggr::Plain).collect();
        Self {
            head_rel: head_rel.into(),
            head_args,
            aggr,
            body,
        }
    }

    pub fn aggregated(
        head_rel: impl Into<Rel>,
        head_args: Vec<Term>,
        aggr: Vec<HeadAggr>,
        body: Vec<Literal>,
    ) -> Self {
        Self {
            head_rel: head_rel.into(),
            head_args,
            aggr,
            body,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FixedRule {
    pub head_rel: Rel,
    pub inputs: Vec<Rel>,
    pub eval: fn(&[BTreeSet<Tuple>]) -> BTreeSet<Tuple>,
}

#[derive(Clone, Debug)]
pub struct Program {
    pub rules: Vec<Rule>,
    pub fixed: Vec<FixedRule>,
    pub facts: BTreeMap<Rel, BTreeSet<Tuple>>,
    pub histories: BTreeMap<Rel, Vec<Event>>,
}

impl Program {
    /// Empty program — no rules, fixed rules, facts, or histories.
    pub fn empty() -> Self {
        Self {
            rules: Vec::new(),
            fixed: Vec::new(),
            facts: BTreeMap::new(),
            histories: BTreeMap::new(),
        }
    }

    pub fn untimed(
        rules: Vec<Rule>,
        fixed: Vec<FixedRule>,
        facts: BTreeMap<Rel, BTreeSet<Tuple>>,
    ) -> Self {
        Program {
            rules,
            fixed,
            facts,
            histories: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    Unsafe(Rel),
    Unstratifiable(Rel),
    Malformed(Rel),
    AggrError(String),
    BudgetExceeded(String),
}

fn aggr_err(e: String) -> Rejection {
    Rejection::AggrError(e)
}

/// Oracle-owned bounds vocabulary — never consumes exec's Budget.
#[derive(Debug, Clone)]
pub struct OracleBudget {
    epoch_ceiling: NonZeroU32,
    derived_tuple_ceiling: Option<u64>,
    deadline: Option<(Instant, Duration)>,
    killed: bool,
}

impl OracleBudget {
    pub fn new(epoch_ceiling: NonZeroU32) -> Self {
        Self {
            epoch_ceiling,
            derived_tuple_ceiling: None,
            deadline: None,
            killed: false,
        }
    }

    pub fn with_derived_tuple_ceiling(mut self, ceiling: u64) -> Self {
        self.derived_tuple_ceiling = Some(ceiling);
        self
    }

    pub fn with_timeout(mut self, allotted: Duration) -> Self {
        self.deadline = Some((Instant::now(), allotted));
        self
    }

    pub fn kill(&mut self) {
        self.killed = true;
    }

    pub fn epoch_ceiling(&self) -> NonZeroU32 {
        self.epoch_ceiling
    }

    pub fn derived_tuple_ceiling(&self) -> Option<u64> {
        self.derived_tuple_ceiling
    }

    pub fn check_interrupt(&self) -> Result<(), String> {
        if self.killed {
            return Err("query cancelled".into());
        }
        if let Some((started, allotted)) = self.deadline {
            let elapsed = started.elapsed();
            if elapsed > allotted {
                return Err(format!(
                    "query budget exceeded: deadline (ms) spent {} of ceiling {}",
                    elapsed.as_millis(),
                    allotted.as_millis()
                ));
            }
        }
        Ok(())
    }
}

fn literal_vars(l: &Literal) -> HashSet<&str> {
    l.args
        .iter()
        .filter_map(|t| match t {
            Term::Var(v) => Some(v.as_str()),
            Term::Const(_) => None,
        })
        .collect()
}

pub fn check_safety(program: &Program) -> Result<(), Rejection> {
    for rule in &program.rules {
        let positive_vars: HashSet<&str> = rule
            .body
            .iter()
            .filter(|l| !l.is_negated())
            .flat_map(literal_vars)
            .collect();
        for t in &rule.head_args {
            if let Term::Var(v) = t
                && !positive_vars.contains(v.as_str())
            {
                return Err(Rejection::Unsafe(rule.head_rel.clone()));
            }
        }
        for l in rule.body.iter().filter(|l| l.is_negated()) {
            if !literal_vars(l).is_subset(&positive_vars) {
                return Err(Rejection::Unsafe(rule.head_rel.clone()));
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub struct HeadClass {
    pub has_aggr: bool,
    pub is_meet: bool,
}

/// Per-head aggregation profile for stratification and naive eval.
///
/// Independent derivation from the engine's `aggregation_character`: that
/// helper runs `any`/`all` over a pre-bucketed ruleset; here we stream each
/// head slot once and demote meet-ness as non-meet folds appear. Same facts,
/// different shape — required by zone-oracle differential law.
pub fn head_classes(program: &Program) -> HashMap<Rel, HeadClass> {
    let mut classes: HashMap<Rel, HeadClass> = HashMap::new();
    for rule in &program.rules {
        let entry = classes
            .entry(rule.head_rel.clone())
            .or_insert(HeadClass {
                has_aggr: false,
                is_meet: false,
            });
        for slot in &rule.aggr {
            let Some((fold, _)) = slot.as_aggregated() else {
                continue;
            };
            if !entry.has_aggr {
                // First aggregated slot stamps the head: meet iff that fold is.
                entry.has_aggr = true;
                entry.is_meet = fold.is_meet();
            } else if entry.is_meet && !fold.is_meet() {
                // A single non-meet fold demotes the whole head permanently.
                entry.is_meet = false;
            }
        }
    }
    classes
}

pub fn dependency_edges(program: &Program) -> Vec<(Rel, Rel, bool)> {
    let classes = head_classes(program);
    let fixed_heads: HashSet<Rel> = program.fixed.iter().map(|f| f.head_rel.clone()).collect();
    let is_meet = |rel: &Rel| classes.get(rel).is_some_and(|c| c.is_meet);
    let mut edges = Vec::new();
    for rule in &program.rules {
        let head = rule.head_rel.clone();
        let class = classes[&head];
        for lit in &rule.body {
            let dep = lit.rel.clone();
            let forcing = if class.has_aggr {
                if class.is_meet && dep == head {
                    lit.is_negated()
                } else {
                    true
                }
            } else {
                lit.is_negated() || fixed_heads.contains(&dep) || is_meet(&dep)
            };
            edges.push((head.clone(), dep, forcing));
        }
    }
    for f in &program.fixed {
        for dep in &f.inputs {
            edges.push((f.head_rel.clone(), dep.clone(), true));
        }
    }
    edges
}

/// One oracle door for dependency back-edges.
///
/// - `forcing_only = true` — stratification: a forcing edge whose dep can
///   reach the head is illegal.
/// - `forcing_only = false` — any edge closing a walk is a cycle (incremental
///   refuses recursion even when stratifiable).
///
/// Shared because both questions are oracle-owned graph facts over the same
/// edge list; splitting them into twin DFS bodies was accidental duplication,
/// not a differential.
pub(crate) fn dependency_back_edge(program: &Program, forcing_only: bool) -> Option<Rel> {
    let edges = dependency_edges(program);
    let mut outs: HashMap<Rel, Vec<Rel>> = HashMap::new();
    for (head, dep, _) in &edges {
        outs.entry(head.clone()).or_default().push(dep.clone());
    }
    for (head, dep, forcing) in &edges {
        if forcing_only && !*forcing {
            continue;
        }
        if dep_can_reach(&outs, dep, head) {
            return Some(head.clone());
        }
    }
    None
}

fn dep_can_reach(outs: &HashMap<Rel, Vec<Rel>>, from: &Rel, target: &Rel) -> bool {
    let mut seen = HashSet::new();
    let mut work = vec![from.clone()];
    while let Some(cur) = work.pop() {
        if cur == *target {
            return true;
        }
        if !seen.insert(cur.clone()) {
            continue;
        }
        if let Some(next) = outs.get(&cur) {
            work.extend(next.iter().cloned());
        }
    }
    false
}

pub fn check_stratifiable(program: &Program) -> Result<(), Rejection> {
    match dependency_back_edge(program, true) {
        Some(head) => Err(Rejection::Unstratifiable(head)),
        None => Ok(()),
    }
}

/// One position that introduces a relation name into a namespace a
/// [`Program::histories`] entry could collide with.
#[derive(Clone, Debug)]
pub enum NameIntroduction {
    RuleHead(Rel),
    FixedHead(Rel),
    FixedInput { head: Rel, input: Rel },
}

impl NameIntroduction {
    pub fn name(&self) -> &Rel {
        match self {
            NameIntroduction::RuleHead(r) | NameIntroduction::FixedHead(r) => r,
            NameIntroduction::FixedInput { input, .. } => input,
        }
    }

    pub fn report(&self) -> Rel {
        match self {
            NameIntroduction::RuleHead(r) | NameIntroduction::FixedHead(r) => r.clone(),
            NameIntroduction::FixedInput { head, .. } => head.clone(),
        }
    }
}

fn name_introductions(program: &Program) -> Vec<NameIntroduction> {
    let mut out = Vec::new();
    for rule in &program.rules {
        out.push(NameIntroduction::RuleHead(rule.head_rel.clone()));
    }
    for f in &program.fixed {
        out.push(NameIntroduction::FixedHead(f.head_rel.clone()));
        for input in &f.inputs {
            out.push(NameIntroduction::FixedInput {
                head: f.head_rel.clone(),
                input: input.clone(),
            });
        }
    }
    out
}

fn check_no_historical_name_collision(program: &Program) -> Result<(), Rejection> {
    for intro in name_introductions(program) {
        if program.histories.contains_key(intro.name()) {
            return Err(Rejection::Malformed(intro.report()));
        }
    }
    Ok(())
}

pub fn check_wellformed(program: &Program) -> Result<(), Rejection> {
    let mut signatures: BTreeMap<Rel, &[HeadAggr]> = BTreeMap::new();
    for rule in &program.rules {
        if rule.aggr.len() != rule.head_args.len() {
            return Err(Rejection::Malformed(rule.head_rel.clone()));
        }
        match signatures.entry(rule.head_rel.clone()) {
            Entry::Occupied(prev) if *prev.get() != rule.aggr.as_slice() => {
                return Err(Rejection::Malformed(rule.head_rel.clone()));
            }
            Entry::Occupied(_) => {}
            Entry::Vacant(e) => {
                e.insert(&rule.aggr);
            }
        }
    }
    let mut fixed_heads = HashSet::new();
    for f in &program.fixed {
        if !fixed_heads.insert(f.head_rel.clone()) || program.facts.contains_key(&f.head_rel) {
            return Err(Rejection::Malformed(f.head_rel.clone()));
        }
    }
    for rule in &program.rules {
        if fixed_heads.contains(&rule.head_rel) {
            return Err(Rejection::Malformed(rule.head_rel.clone()));
        }
    }
    for (rel, class) in head_classes(program) {
        if class.has_aggr && program.facts.contains_key(&rel) {
            return Err(Rejection::Malformed(rel));
        }
    }
    for rel in program.histories.keys() {
        if program.facts.contains_key(rel) {
            return Err(Rejection::Malformed(rel.clone()));
        }
    }
    check_no_historical_name_collision(program)?;
    for (rel, history) in &program.histories {
        let key_arity = history.first().map(|e| e.key().len());
        for e in history {
            if Some(e.key().len()) != key_arity {
                return Err(Rejection::Malformed(rel.clone()));
            }
        }
        let payload_arity = history.iter().find_map(|e| e.payload().map(|p| p.len()));
        for e in history.iter().filter_map(Event::payload) {
            if Some(e.len()) != payload_arity {
                return Err(Rejection::Malformed(rel.clone()));
            }
        }
    }
    for rule in &program.rules {
        for lit in &rule.body {
            if lit.as_of.is_some() && !program.histories.contains_key(&lit.rel) {
                return Err(Rejection::Malformed(lit.rel.clone()));
            }
        }
    }
    let mut arities: HashMap<Rel, usize> = HashMap::new();
    let mut check_arity = |rel: Rel, arity: usize| -> Result<(), Rejection> {
        match arities.get(&rel) {
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
            check_arity(rel.clone(), t.len())?;
        }
    }
    for (rel, history) in &program.histories {
        if let (Some(k), Some(v)) = (
            history.first().map(|e| e.key().len()),
            history.iter().find_map(|e| e.payload().map(|p| p.len())),
        ) {
            check_arity(rel.clone(), k + v)?;
        }
    }
    for rule in &program.rules {
        check_arity(rule.head_rel.clone(), rule.head_args.len())?;
        for l in &rule.body {
            check_arity(l.rel.clone(), l.args.len())?;
        }
    }
    Ok(())
}

/// Bellman-Ford stratum assignment (assumes stratifiability).
pub fn strata(program: &Program) -> Result<HashMap<Rel, usize>, Rejection> {
    let edges = dependency_edges(program);
    let mut s: HashMap<Rel, usize> = HashMap::new();
    let rels: HashSet<Rel> = program
        .rules
        .iter()
        .flat_map(|r| {
            std::iter::once(r.head_rel.clone()).chain(r.body.iter().map(|l| l.rel.clone()))
        })
        .chain(program.facts.keys().cloned())
        .chain(program.histories.keys().cloned())
        .chain(
            program
                .fixed
                .iter()
                .flat_map(|f| std::iter::once(f.head_rel.clone()).chain(f.inputs.iter().cloned())),
        )
        .collect();
    for r in &rels {
        s.insert(r.clone(), 0);
    }
    let bound = rels.len() + 1;
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
            return Ok(s);
        }
    }
    Err(Rejection::Unstratifiable(
        "stratum assignment did not converge: program is not stratifiable".into(),
    ))
}

pub type Bindings = HashMap<Name, DataValue>;

/// Extend `bound` so every `args[i]` agrees with `tuple[i]`.
///
/// Position-indexed bind with a total `bind_slot` helper — deliberately not
/// the engine's zip/`match` ladder (zone-oracle differential).
pub fn unify(args: &[Term], tuple: &[DataValue], bound: &Bindings) -> Option<Bindings> {
    if args.len() != tuple.len() {
        return None;
    }
    let mut env = bound.clone();
    for i in 0..args.len() {
        if !bind_slot(&mut env, &args[i], &tuple[i]) {
            return None;
        }
    }
    Some(env)
}

fn bind_slot(env: &mut Bindings, term: &Term, value: &DataValue) -> bool {
    match term {
        Term::Const(c) => c == value,
        Term::Var(name) => match env.get(name.as_str()) {
            Some(prior) => prior == value,
            None => {
                env.insert(name.clone(), value.clone());
                true
            }
        },
    }
}

pub fn ground(args: &[Term], bound: &Bindings) -> Tuple {
    args.iter()
        .map(|t| match t {
            Term::Const(c) => c.clone(),
            Term::Var(v) => bound[v.as_str()].clone(),
        })
        .collect()
}

pub fn literal_rows(
    program: &Program,
    db: &BTreeMap<Rel, BTreeSet<Tuple>>,
    lit: &Literal,
    default_as_of: AsOf,
) -> BTreeSet<Tuple> {
    match program.histories.get(&lit.rel) {
        Some(history) => {
            let as_of = match lit.as_of {
                Some(a) => a,
                None => default_as_of,
            };
            resolve_relation(history, as_of)
        }
        None => match db.get(&lit.rel) {
            Some(rows) => rows.clone(),
            None => BTreeSet::new(),
        },
    }
}

fn body_bindings(
    rule: &Rule,
    program: &Program,
    db: &BTreeMap<Rel, BTreeSet<Tuple>>,
    default_as_of: AsOf,
) -> Vec<Bindings> {
    body_bindings_from(rule, program, db, default_as_of, Bindings::new())
}

/// All body-satisfying environments starting from `initial`.
///
/// Positives join first (safety: negated probes are fully ground), then
/// negated literals gate. Split into two helpers so the control flow is not
/// a twin of the engine's single ordered-literal frontier loop.
pub fn body_bindings_from(
    rule: &Rule,
    program: &Program,
    db: &BTreeMap<Rel, BTreeSet<Tuple>>,
    default_as_of: AsOf,
    initial: Bindings,
) -> Vec<Bindings> {
    let after_pos = join_positive_body(rule, program, db, default_as_of, initial);
    gate_negated_body(rule, program, db, default_as_of, after_pos)
}

fn join_positive_body(
    rule: &Rule,
    program: &Program,
    db: &BTreeMap<Rel, BTreeSet<Tuple>>,
    default_as_of: AsOf,
    initial: Bindings,
) -> Vec<Bindings> {
    let mut frontier = vec![initial];
    for lit in rule.body.iter().filter(|l| !l.is_negated()) {
        let rows = literal_rows(program, db, lit, default_as_of);
        frontier = frontier
            .into_iter()
            .flat_map(|bound| {
                rows.iter()
                    .filter_map(|tuple| unify(&lit.args, tuple.as_slice(), &bound))
                    .collect::<Vec<_>>()
            })
            .collect();
        if frontier.is_empty() {
            return frontier;
        }
    }
    frontier
}

fn gate_negated_body(
    rule: &Rule,
    program: &Program,
    db: &BTreeMap<Rel, BTreeSet<Tuple>>,
    default_as_of: AsOf,
    frontier: Vec<Bindings>,
) -> Vec<Bindings> {
    let mut live = frontier;
    for lit in rule.body.iter().filter(|l| l.is_negated()) {
        let rows = literal_rows(program, db, lit, default_as_of);
        live.retain(|bound| {
            let probe = ground(&lit.args, bound);
            !rows.contains(&probe)
        });
        if live.is_empty() {
            return live;
        }
    }
    live
}

pub fn derived_rows(
    rule: &Rule,
    program: &Program,
    db: &BTreeMap<Rel, BTreeSet<Tuple>>,
    default_as_of: AsOf,
) -> Vec<Tuple> {
    body_bindings(rule, program, db, default_as_of)
        .iter()
        .map(|b| ground(&rule.head_args, b))
        .collect()
}

fn eval_normal_aggr_head(
    rules: &[&Rule],
    program: &Program,
    db: &BTreeMap<Rel, BTreeSet<Tuple>>,
    default_as_of: AsOf,
) -> Result<BTreeSet<Tuple>, Rejection> {
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
    let fresh_ops = || -> Result<Vec<Box<dyn NormalAccum>>, Rejection> {
        val_positions
            .iter()
            .map(|(_, fold, args)| fold.fresh_normal(args).map_err(aggr_err))
            .collect()
    };

    let mut groups: BTreeMap<Tuple, Vec<Box<dyn NormalAccum>>> = BTreeMap::new();
    for rule in rules {
        for row in derived_rows(rule, program, db, default_as_of) {
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
        let mut row = Tuple::with_capacity(val_positions.len());
        for op in fresh_ops()? {
            row.push(op.get().map_err(aggr_err)?);
        }
        out.insert(row);
    }
    for (key, ops) in groups {
        let mut row = Tuple::from_vec(vec![DataValue::Null; signature.len()]);
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

struct MeetState {
    key_positions: Vec<usize>,
    val_positions: Vec<usize>,
    ops: Vec<Box<dyn MeetOp>>,
    arity: usize,
    acc: BTreeMap<Tuple, Vec<MeetAccum>>,
}

impl MeetState {
    fn new(signature: &[HeadAggr]) -> Result<Self, Rejection> {
        let key_positions = signature
            .iter()
            .enumerate()
            .filter(|(_, a)| !a.is_aggregated())
            .map(|(i, _)| i)
            .collect();
        let mut val_positions = Vec::new();
        let mut ops = Vec::new();
        for (i, a) in signature.iter().enumerate() {
            if let Some((fold, _)) = a.as_aggregated() {
                let op = fold.fresh_meet().ok_or(Rejection::Malformed(
                    "non-meet aggregation on a meet head".into(),
                ))?;
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

    fn meet_row(&mut self, row: &Tuple) -> Result<bool, Rejection> {
        let key: Tuple = self.key_positions.iter().map(|i| row[*i].clone()).collect();
        let incoming: Vec<MeetAccum> = self
            .val_positions
            .iter()
            .map(|i| MeetAccum::from_derived(row[*i].clone()))
            .collect();
        match self.acc.entry(key) {
            Entry::Vacant(e) => {
                let mut vals: Vec<MeetAccum> = self.ops.iter().map(|op| op.init_val()).collect();
                for (slot, op) in self.ops.iter().enumerate() {
                    op.update(&mut vals[slot], &incoming[slot])
                        .map_err(aggr_err)?;
                }
                e.insert(vals);
                Ok(true)
            }
            Entry::Occupied(mut e) => {
                let stored = e.get_mut();
                let mut changed = false;
                for (slot, op) in self.ops.iter().enumerate() {
                    changed |= op
                        .update(&mut stored[slot], &incoming[slot])
                        .map_err(aggr_err)?;
                }
                Ok(changed)
            }
        }
    }

    fn materialize(&self) -> BTreeSet<Tuple> {
        self.acc
            .iter()
            .map(|(key, vals)| {
                let mut row = Tuple::from_vec(vec![DataValue::Null; self.arity]);
                for (slot, i) in self.key_positions.iter().enumerate() {
                    row[*i] = key[slot].clone();
                }
                for (slot, i) in self.val_positions.iter().enumerate() {
                    row[*i] = vals[slot].to_value();
                }
                row
            })
            .collect()
    }
}

pub fn naive_eval(program: &Program) -> Result<BTreeMap<Rel, BTreeSet<Tuple>>, Rejection> {
    naive_eval_at(program, AsOf::current())
}

pub fn naive_eval_at(
    program: &Program,
    default_as_of: AsOf,
) -> Result<BTreeMap<Rel, BTreeSet<Tuple>>, Rejection> {
    naive_eval_at_impl(program, default_as_of, None)
}

pub fn naive_eval_at_budgeted(
    program: &Program,
    default_as_of: AsOf,
    budget: &OracleBudget,
) -> Result<BTreeMap<Rel, BTreeSet<Tuple>>, Rejection> {
    naive_eval_at_impl(program, default_as_of, Some(budget))
}

fn check_oracle_budget(
    budget: &OracleBudget,
    db: &BTreeMap<Rel, BTreeSet<Tuple>>,
    rounds: usize,
) -> Result<(), Rejection> {
    budget
        .check_interrupt()
        .map_err(Rejection::BudgetExceeded)?;

    // INVARIANT(u32_fits_usize): NonZeroU32::get always fits usize.
    let epoch_ceiling = usize::try_from(budget.epoch_ceiling().get())
        .expect("INVARIANT(u32_fits_usize): u32 fits usize");
    if rounds > epoch_ceiling {
        return Err(Rejection::BudgetExceeded(format!(
            "query budget exceeded: epochs spent {rounds} of ceiling {epoch_ceiling}"
        )));
    }
    if let Some(ceiling) = budget.derived_tuple_ceiling() {
        let spent: u64 = db
            .values()
            .map(|rows| {
                // INVARIANT(len_fits_u64): Vec/BTreeSet::len fits u64.
                u64::try_from(rows.len()).expect("INVARIANT(len_fits_u64): len fits u64")
            })
            .sum();
        if spent > ceiling {
            return Err(Rejection::BudgetExceeded(format!(
                "query budget exceeded: derived tuples spent {spent} of ceiling {ceiling}"
            )));
        }
    }
    Ok(())
}

fn naive_eval_at_impl(
    program: &Program,
    default_as_of: AsOf,
    budget: Option<&OracleBudget>,
) -> Result<BTreeMap<Rel, BTreeSet<Tuple>>, Rejection> {
    check_wellformed(program)?;
    check_safety(program)?;
    check_stratifiable(program)?;
    let classes = head_classes(program);
    let strata_of = strata(program)?;
    let max_stratum = strata_of.values().copied().fold(0, Ord::max);

    let mut db = program.facts.clone();

    for stratum in 0..=max_stratum {
        if let Some(b) = budget {
            check_oracle_budget(b, &db, 0)?;
        }
        for f in program
            .fixed
            .iter()
            .filter(|f| strata_of[&f.head_rel] == stratum)
        {
            let inputs: Vec<BTreeSet<Tuple>> = f
                .inputs
                .iter()
                .map(|r| match db.get(r) {
                    Some(rows) => rows.clone(),
                    None => BTreeSet::new(),
                })
                .collect();
            db.insert(f.head_rel.clone(), (f.eval)(&inputs));
        }

        let normal_heads: BTreeSet<Rel> = program
            .rules
            .iter()
            .filter(|r| strata_of[&r.head_rel] == stratum)
            .map(|r| r.head_rel.clone())
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
            let out = eval_normal_aggr_head(&head_rules, program, &db, default_as_of)?;
            db.insert(head.clone(), out);
        }

        let mut meets: BTreeMap<Rel, MeetState> = BTreeMap::new();
        for rule in program
            .rules
            .iter()
            .filter(|r| strata_of[&r.head_rel] == stratum && classes[&r.head_rel].is_meet)
        {
            if !meets.contains_key(&rule.head_rel) {
                meets.insert(rule.head_rel.clone(), MeetState::new(&rule.aggr)?);
            }
        }
        let mut rounds = 0usize;
        loop {
            rounds += 1;
            if rounds > 100_000 {
                return Err(Rejection::BudgetExceeded(
                    "fixpoint bound exceeded: non-termination".into(),
                ));
            }
            if let Some(b) = budget {
                check_oracle_budget(b, &db, rounds)?;
            }
            let mut changed = false;
            for rule in program.rules.iter().filter(|r| {
                strata_of[&r.head_rel] == stratum && !normal_heads.contains(&r.head_rel)
            }) {
                let rows = derived_rows(rule, program, &db, default_as_of);
                if let Some(state) = meets.get_mut(&rule.head_rel) {
                    for row in &rows {
                        changed |= state.meet_row(row)?;
                    }
                } else {
                    for row in rows {
                        changed |= db.entry(rule.head_rel.clone()).or_default().insert(row);
                    }
                }
            }
            if rounds == 1 {
                for state in meets.values_mut() {
                    if state.acc.is_empty()
                        && state.key_positions.is_empty()
                        && !state.ops.is_empty()
                    {
                        let identity: Vec<MeetAccum> =
                            state.ops.iter().map(|op| op.init_val()).collect();
                        state.acc.insert(Tuple::new(), identity);
                        changed = true;
                    }
                }
            }
            for (head, state) in &meets {
                db.insert(head.clone(), state.materialize());
            }
            if !changed {
                break;
            }
        }
    }
    Ok(db)
}

/// Corpus of programs the compiler must refuse.
pub fn unstratifiable_corpus() -> Vec<(&'static str, Program)> {
    fn lit(rel: impl Into<Rel>, args: Vec<Term>, negated: bool) -> Literal {
        if negated {
            Literal::neg(rel, args)
        } else {
            Literal::pos(rel, args)
        }
    }
    let x = || Term::var("X");
    let y = || Term::var("Y");
    vec![
        (
            "direct self-negation: p(X) :- d(X), not p(X)",
            Program {
                rules: vec![Rule::plain(
                    "p",
                    vec![x()],
                    vec![lit("d", vec![x()], false), lit("p", vec![x()], true)],
                )],
                ..Program::empty()
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
                ..Program::empty()
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
                ..Program::empty()
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
                ..Program::empty()
            },
        ),
        (
            "recursive normal aggregation: p(X, count(Y)) :- d(X,Y); p(X, count(Y)) :- p(X,Y)",
            Program {
                rules: vec![
                    Rule::aggregated(
                        "p",
                        vec![x(), y()],
                        vec![HeadAggr::Plain, HeadAggr::named("count")],
                        vec![lit("d", vec![x(), y()], false)],
                    ),
                    Rule::aggregated(
                        "p",
                        vec![x(), y()],
                        vec![HeadAggr::Plain, HeadAggr::named("count")],
                        vec![lit("p", vec![x(), y()], false)],
                    ),
                ],
                ..Program::empty()
            },
        ),
        (
            "mixed meet+normal aggregation on a recursive head: \
             q(X, min(Y), count(Z)) :- q(X,Y,Z)",
            Program {
                rules: vec![Rule::aggregated(
                    "q",
                    vec![x(), y(), Term::var("Z")],
                    vec![
                        HeadAggr::Plain,
                        HeadAggr::named("min"),
                        HeadAggr::named("count"),
                    ],
                    vec![lit("q", vec![x(), y(), Term::var("Z")], false)],
                )],
                ..Program::empty()
            },
        ),
        (
            "meet aggregation negating its own head: m(X, min(Y)) :- d(X,Y), not m(X,Y)",
            Program {
                rules: vec![Rule::aggregated(
                    "m",
                    vec![x(), y()],
                    vec![HeadAggr::Plain, HeadAggr::named("min")],
                    vec![
                        lit("d", vec![x(), y()], false),
                        lit("m", vec![x(), y()], true),
                    ],
                )],
                ..Program::empty()
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
                    head_rel: "f".into(),
                    inputs: vec!["r".into()],
                    eval: |_| BTreeSet::new(),
                }],
                ..Program::empty()
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(i: i64) -> DataValue {
        DataValue::from(i)
    }
    fn edge_facts(edges: &[(i64, i64)]) -> BTreeMap<Rel, BTreeSet<Tuple>> {
        let mut facts: BTreeMap<Rel, BTreeSet<Tuple>> = Default::default();
        facts.insert(
            "edge".into(),
            edges
                .iter()
                .map(|(a, b)| vec![v(*a), v(*b)])
                .map(Tuple::from_vec)
                .collect(),
        );
        facts
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
    fn z() -> Term {
        Term::var("Z")
    }
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

    #[test]
    fn law1_transitive_closure_exact() {
        let program = Program {
            rules: transitive_closure(),
            facts: edge_facts(&[(1, 2), (2, 3), (3, 4)]),
            ..Program::empty()
        };
        let db = naive_eval(&program).expect("well-formed corpus program evaluates");
        let want: BTreeSet<Tuple> = [(1, 2), (2, 3), (3, 4), (1, 3), (2, 4), (1, 4)]
            .into_iter()
            .map(|(a, b)| vec![v(a), v(b)])
            .map(Tuple::from_vec)
            .collect();
        assert_eq!(db[&Rel::from("path")], want);
    }

    #[test]
    fn law2_stratified_negation_evaluates_correctly() {
        let mut facts = edge_facts(&[(1, 2), (2, 3)]);
        facts.insert(
            "node".into(),
            (1..=3).map(|i| vec![v(i)]).map(Tuple::from_vec).collect(),
        );
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
            ..Program::empty()
        })
        .expect("stratified negation corpus evaluates");
        let want: BTreeSet<Tuple> = [(1, 1), (2, 1), (2, 2), (3, 1), (3, 2), (3, 3)]
            .into_iter()
            .map(|(a, b)| vec![v(a), v(b)])
            .map(Tuple::from_vec)
            .collect();
        assert_eq!(db[&Rel::from("unreachable")], want);
    }

    #[test]
    fn law2_unstratifiable_corpus_is_refused() {
        for (name, program) in unstratifiable_corpus() {
            assert!(
                matches!(
                    check_stratifiable(&program),
                    Err(Rejection::Unstratifiable(_))
                ),
                "corpus case must refuse: {name}"
            );
        }
    }

    #[test]
    fn law3_recursion_terminates_on_cyclic_data() {
        let program = Program {
            rules: transitive_closure(),
            facts: edge_facts(&[(1, 2), (2, 3), (3, 1)]),
            ..Program::empty()
        };
        let db = naive_eval(&program).expect("well-formed corpus program evaluates");
        assert_eq!(db[&Rel::from("path")].len(), 9);
    }

    #[test]
    fn law4_unsafe_rules_are_refused() {
        let program = Program {
            rules: vec![Rule::plain("p", vec![x()], vec![])],
            ..Program::empty()
        };
        assert!(matches!(check_safety(&program), Err(Rejection::Unsafe(_))));
    }

    #[test]
    fn budgeted_eval_refuses_under_a_starved_epoch_ceiling() {
        let program = Program {
            rules: transitive_closure(),
            facts: edge_facts(&[(1, 2), (2, 3), (3, 4)]),
            ..Program::empty()
        };
        let budget = OracleBudget::new(NonZeroU32::new(1).expect("literal 1 is nonzero"));
        let err = naive_eval_at_budgeted(&program, AsOf::current(), &budget)
            .expect_err("a ceiling of 1 must refuse a real recursive program");
        assert!(
            matches!(err, Rejection::BudgetExceeded(_)),
            "expected BudgetExceeded, got {err:?}"
        );
    }

    #[test]
    fn budgeted_eval_matches_the_unbudgeted_oracle_under_a_generous_budget() {
        let program = Program {
            rules: transitive_closure(),
            facts: edge_facts(&[(1, 2), (2, 3), (3, 4)]),
            ..Program::empty()
        };
        let budget = OracleBudget::new(NonZeroU32::new(1_000).expect("literal 1000 is nonzero"));
        let budgeted = naive_eval_at_budgeted(&program, AsOf::current(), &budget)
            .expect("a generous budget never refuses");
        let unbudgeted = naive_eval(&program).expect("the unbudgeted oracle always runs");
        assert_eq!(budgeted, unbudgeted);
    }

    #[test]
    fn normal_aggregation_groups_and_folds() {
        let mut facts = BTreeMap::new();
        facts.insert(
            "p".into(),
            [vec![v(1), v(10)], vec![v(1), v(20)], vec![v(2), v(5)]]
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
        let db = naive_eval(&program).expect("well-formed corpus program evaluates");
        let want: BTreeSet<Tuple> = [vec![v(1), v(30)], vec![v(2), v(5)]]
            .into_iter()
            .map(Tuple::from_vec)
            .collect();
        assert_eq!(db[&Rel::from("total")], want);
    }

    #[test]
    fn meet_aggregation_evaluates_inside_recursion() {
        let mut facts = edge_facts(&[(1, 2), (2, 3)]);
        facts.insert(
            "seed".into(),
            [vec![v(1), v(10)]]
                .into_iter()
                .map(Tuple::from_vec)
                .collect(),
        );
        let program = Program {
            rules: vec![
                Rule::aggregated(
                    "m",
                    vec![x(), y()],
                    vec![HeadAggr::Plain, HeadAggr::named("min")],
                    vec![lit("seed", vec![x(), y()], false)],
                ),
                Rule::aggregated(
                    "m",
                    vec![y(), z()],
                    vec![HeadAggr::Plain, HeadAggr::named("min")],
                    vec![
                        lit("edge", vec![x(), y()], false),
                        lit("m", vec![x(), z()], false),
                    ],
                ),
            ],
            facts,
            ..Program::empty()
        };
        let db = naive_eval(&program).expect("well-formed corpus program evaluates");
        assert!(db[&Rel::from("m")].contains(&Tuple::from_vec(vec![v(3), v(10)])));
    }

    #[test]
    fn constants_and_repeated_variables_unify_exactly() {
        let mut facts = BTreeMap::new();
        facts.insert(
            "e".into(),
            [vec![v(1), v(1)], vec![v(1), v(2)]]
                .into_iter()
                .map(Tuple::from_vec)
                .collect(),
        );
        let program = Program::untimed(
            vec![Rule::plain(
                "loop",
                vec![x()],
                vec![lit("e", vec![x(), x()], false)],
            )],
            vec![],
            facts,
        );
        let db = naive_eval(&program).expect("well-formed corpus program evaluates");
        assert_eq!(
            db[&Rel::from("loop")],
            [Tuple::from_vec(vec![v(1)])].into_iter().collect()
        );
    }
}
