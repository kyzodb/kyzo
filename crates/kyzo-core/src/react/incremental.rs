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
//! [`SignedFact`] is reused directly from `exec::op::temporal` — it is
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
//! a recursive derivation — is separate, harder scope); FIXED RULES have
//! no representation here at all (this module's [`Rule`] has no
//! opaque-function variant) — there is nothing to refuse because nothing
//! constructs one, the same "unrepresentable, not merely refused" posture
//! the type system prefers over a runtime check where it can have it.
//! AGGREGATION is fully covered, not refused — see
//! [`eval_aggregating_head_incremental`]'s doc for the algorithm, the
//! same group-level candidates-then-verify extension `laws.rs` proves
//! first (`eval_aggregating_head_incremental` there).
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
//!
//! An aggregating head extends this one level ([`collect_affected_groups`]
//! / [`eval_one_group`] / [`eval_aggregating_head_incremental`]): find
//! affected GROUPS instead of tuples, then fully re-derive each group's
//! aggregate row directly, rather than maintaining a per-kind signed
//! delta (which does not exist in general — see that function's doc).

use std::collections::{BTreeMap, BTreeSet};

use miette::{Error, Result, miette, ensure};

use crate::exec::fold::aggr::NormalAggr;
use crate::exec::op::temporal::SignedFact;
use kyzo_model::program::aggregate::Aggregation;
use kyzo_model::program::rule::HeadAggrSlot;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::DataValue;
use kyzo_model::value::Tuple;

/// One rule-body argument: a bound value, or a variable to unify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    Const(DataValue),
    Var(Symbol),
}

/// A rule-body literal's sign: read positively, or negated. Minted locally
/// rather than reused from the oracle's `eval::Polarity` — this module is an
/// independently-written twin of `laws::incremental_eval` (see the module
/// doc), and the oracle crate wall forbids kyzo-core from depending on
/// kyzo-oracle at all, test code included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Polarity {
    Positive,
    Negative,
}

/// One rule-body literal: a relation read, optionally negated.
#[derive(Debug, Clone)]
pub struct Literal {
    pub rel: Symbol,
    pub args: Vec<Term>,
    pub polarity: Polarity,
}

impl Literal {
    pub fn is_negated(&self) -> bool {
        matches!(self.polarity, Polarity::Negative)
    }
}

/// One head position's aggregation slot — the REAL landed
/// [`Aggregation`] from `data/aggr.rs`, the same type `laws::HeadAggr`
/// wraps: both tiers fold through exactly the code users get, never a
/// second hand-rolled implementation of "sum" or "min".
pub type HeadAggr = HeadAggrSlot;

/// One derivation rule: `head_rel(head_args) :- body`. `aggr` is always
/// the same length as `head_args`; all-`None` marks an ordinary
/// (non-aggregating) rule, matching `laws::Rule`'s own convention. No
/// fixed-rule variant: that stays unrepresentable (see the module doc's
/// scope section), not merely refused at runtime.
#[derive(Debug, Clone)]
pub struct Rule {
    pub head_rel: Symbol,
    pub head_args: Vec<Term>,
    pub body: Vec<Literal>,
    pub aggr: Vec<HeadAggr>,
}

/// A standing query's rule set. Unlike `laws::Program`, there is no
/// inline `facts` field: EDB content lives in the caller's
/// [`MaintainedState`], never inline in the program itself — a standing
/// query's whole point is that its EDB changes out from under it, commit
/// after commit.
#[derive(Debug, Clone)]
pub struct IncrementalProgram {
    pub rules: Vec<Rule>,
}

impl IncrementalProgram {
    /// Empty program — no rules.
    pub fn empty() -> Self {
        Self { rules: Vec::new() }
    }
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
pub type MaintainedState = BTreeMap<Symbol, BTreeSet<Tuple>>;

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
    match state.get(&lit.rel) {
        Some(rows) => rows.clone(),
        None => BTreeSet::new(),
    }
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
/// [`edb_relations`], for a caller with no patch yet (registration: the
/// static EDB set a compiled program's rules name, before any commit has
/// arrived to patch).
pub(crate) fn edb_relations_pub(program: &IncrementalProgram) -> BTreeSet<Symbol> {
    edb_relations(program, &BTreeSet::new())
}

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
        let rows = literal_rows(state, lit);
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
        let Some(seed) = unify(&rule.head_args, target.as_slice(), &Bindings::new()) else {
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
    let mut ordered: Vec<&Literal> = rule.body.iter().filter(|l| !l.is_negated()).collect();
    ordered.extend(rule.body.iter().filter(|l| l.is_negated()));

    let mut frontier: Vec<Bindings> = vec![initial];
    for lit in ordered {
        let rows = literal_rows(state, lit);
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

// ─────────────────────────────────────────────────────────────────────────
// Translation: a real compiled query -> this module's IR
// ─────────────────────────────────────────────────────────────────────────
//
// `data::program::MagicAtom` (the magic-set-rewritten tier, one stratum
// short of the final `RelAlgebra` lowering) is the right source, not
// `RelAlgebra` itself: by the time atoms reach `RelAlgebra`, negation has
// become `NegJoin`, every variable has become a resolved column INDEX,
// and `Symbol`s are gone — there is nothing left to translate. `MagicAtom`
// still names its variables by `Symbol` and already separates
// `Rule`/`NegatedRule` and `Relation`/`NegatedRelation` as distinct
// variants, matching this module's `Literal.is_negated()` directly.
//
// One real subtlety, not a free structural match: after the magic
// rewrite, a CONSTANT never appears inline in a `Relation`/`Rule` atom's
// argument list — it is hoisted into a separate `Unification{binding,
// expr: Expr::Const{..}}` atom instead. [`translate_rule`] below collects
// every constant-valued `Unification` atom into a substitution map
// first, then applies it to every other atom's (and the head's) `Symbol`
// arguments, folding `Term::Var` back into `Term::Const` wherever the
// rewrite split them apart.

use crate::exec::plan::program::{
    MagicAtom, MagicInlineRule, MagicRulesOrFixed, MagicSymbol, StratifiedMagicProgram,
};

/// A compiled query this module cannot translate — a typed refusal, never
/// a silently wrong or partial `IncrementalProgram`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum TranslationRejection {
    #[error("standing queries do not cover fixed rules (opaque graph algorithms)")]
    #[diagnostic(code(incremental::translate::fixed_rule))]
    FixedRule,
    #[error("standing queries do not cover {0} yet — refused, not silently wrong")]
    #[diagnostic(code(incremental::translate::unsupported))]
    Unsupported(&'static str),
}

/// The canonical `Symbol` identity of a `MagicSymbol` — its own `Debug`
/// rendering already encodes the adornment (`path`, `path|Mbf`,
/// `path|Ibf`, `path|S.0.1bf`, …) uniquely per distinct derived relation,
/// so reusing it (rather than inventing a second encoding) keeps two
/// differently-adorned versions of the same rule name the DISTINCT
/// relations they are.
fn magic_symbol_to_symbol(m: &MagicSymbol) -> Symbol {
    Symbol::new(format!("{m:?}"), m.as_plain_symbol().span)
}

/// Fold every constant-valued `Unification` atom in `body` into a
/// substitution map, keyed by the `Symbol` it binds. A non-constant
/// `Unification` (a computed expression, not a plain constant) has no
/// representation in this module's `Term` and is refused, named.
fn collect_const_substitutions(
    body: &[MagicAtom],
) -> Result<BTreeMap<Symbol, DataValue>, TranslationRejection> {
    let mut subst = BTreeMap::new();
    for atom in body {
        if let MagicAtom::Unification(u) = atom {
            if !u.is_const() {
                return Err(TranslationRejection::Unsupported(
                    "a non-constant unification",
                ));
            }
            let kyzo_model::program::expr::Expr::Const { val, .. } = &u.expr else {
                return Err(TranslationRejection::Unsupported(
                    "a non-constant unification (is_const disagreed with Expr shape)",
                ));
            };
            subst.insert(u.binding.clone(), val.clone());
        }
    }
    Ok(subst)
}

/// Apply a constant substitution to one `Symbol` argument, producing the
/// `Term` it should become in this module's IR.
fn substitute(v: &Symbol, subst: &BTreeMap<Symbol, DataValue>) -> Term {
    match subst.get(v) {
        Some(c) => Term::Const(c.clone()),
        None => Term::Var(v.clone()),
    }
}

/// Translate one magic-tier rule (for the head `head_sym`) into this
/// module's [`Rule`]. `MagicInlineRule::aggr` is already exactly this
/// module's `HeadAggr` shape ([`HeadAggrSlot`]) — carried straight through,
/// not re-derived.
fn translate_rule(
    head_sym: &MagicSymbol,
    rule: &MagicInlineRule,
) -> Result<Rule, TranslationRejection> {
    let subst = collect_const_substitutions(&rule.body)?;
    let mut body = Vec::new();
    for atom in &rule.body {
        let (rel, args, negated) = match atom {
            MagicAtom::Relation(r) => (r.name.clone(), &r.args, false),
            MagicAtom::NegatedRelation(r) => (r.name.clone(), &r.args, true),
            MagicAtom::Rule(r) => (magic_symbol_to_symbol(&r.name), &r.args, false),
            MagicAtom::NegatedRule(r) => (magic_symbol_to_symbol(&r.name), &r.args, true),
            MagicAtom::Unification(_) => continue, // already folded into `subst`
            MagicAtom::Predicate(_) => {
                return Err(TranslationRejection::Unsupported("a predicate filter"));
            }
            MagicAtom::Search(_) => {
                return Err(TranslationRejection::Unsupported("an index search"));
            }
        };
        body.push(Literal {
            rel,
            args: args.iter().map(|v| substitute(v, &subst)).collect(),
            polarity: if negated {
                Polarity::Negative
            } else {
                Polarity::Positive
            },
        });
    }
    let head_args = rule.head.iter().map(|v| substitute(v, &subst)).collect();
    Ok(Rule {
        head_rel: magic_symbol_to_symbol(head_sym),
        head_args,
        body,
        aggr: rule.aggr.clone(),
    })
}

/// Translate a real compiled, magic-set-rewritten query into this
/// module's [`IncrementalProgram`] — the missing piece between a real
/// KyzoScript query and [`incremental_eval`]: a caller no longer has to
/// hand-author a `Rule`/`Literal` set the way this module's own tests
/// do. `MagicInlineRule::aggr` carries straight through (it is already
/// this module's exact `HeadAggr` shape) — [`incremental_eval`] fully
/// covers aggregation, so there is nothing to refuse there. Still
/// refuses (never silently drops or mis-translates) fixed rules,
/// predicates, index searches, and non-constant unifications — every
/// one of those has no representation in this module's
/// `Rule`/`Literal`/`Term` today.
pub fn translate(
    program: StratifiedMagicProgram,
) -> Result<IncrementalProgram, TranslationRejection> {
    let mut rules = Vec::new();
    for stratum in program.into_strata() {
        for (head_sym, def) in stratum.prog {
            match def {
                MagicRulesOrFixed::Fixed { .. } => return Err(TranslationRejection::FixedRule),
                MagicRulesOrFixed::Rules { rules: magic_rules } => {
                    for magic_rule in &magic_rules {
                        rules.push(translate_rule(&head_sym, magic_rule)?);
                    }
                }
            }
        }
    }
    Ok(IncrementalProgram { rules })
}

/// For an aggregating head, the GROUP KEYS (projections onto the
/// non-aggregated head positions) any of `rules`'s candidate
/// re-derivations touch. Reuses [`collect_candidates`] UNCHANGED, same
/// reasoning as `laws::collect_affected_groups`: a "candidate" there is
/// a raw, pre-fold ground head row, so projecting it onto the key
/// positions is exactly "which group might have gained or lost a member
/// this round."
fn collect_affected_groups(
    rules: &[&Rule],
    state: &MaintainedState,
    rel_deltas: &BTreeMap<Symbol, BTreeSet<SignedFact>>,
    key_positions: &[usize],
) -> BTreeSet<Tuple> {
    let mut raw_candidates = BTreeSet::new();
    for rule in rules {
        collect_candidates(rule, state, rel_deltas, &mut raw_candidates);
    }
    raw_candidates
        .iter()
        .map(|row| key_positions.iter().map(|i| row[*i].clone()).collect())
        .collect()
}

/// Fully re-derive one group's aggregate row from CURRENT (post-patch)
/// state — the production twin of `laws::eval_one_group`, same
/// reasoning: bounded by one group's own body cost via a targeted join
/// seeded from the group's own key values, never a full relation
/// re-derivation. `None` means the group has no members left, UNLESS
/// `key_positions` is empty (a single global aggregate), which folds
/// zero rows into the identity row instead of vanishing.
fn eval_one_group(
    rules: &[&Rule],
    state: &MaintainedState,
    key_positions: &[usize],
    val_positions: &[(usize, &Aggregation, &[DataValue])],
    signature_len: usize,
    group_key: &Tuple,
) -> Result<Option<Tuple>> {
    let fresh_ops = || -> Result<Vec<NormalAggr>> {
        val_positions
            .iter()
            .map(|(_, aggr, args)| crate::exec::fold::aggr::normal_op(aggr, args))
            .collect()
    };
    let mut ops: Option<Vec<NormalAggr>> = None;
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
        for binding in body_bindings_from(rule, state, seed) {
            let row = ground(&rule.head_args, &binding);
            let ops = ops.get_or_insert(fresh_ops()?);
            for (op, (i, _, _)) in ops.iter_mut().zip(val_positions) {
                op.set(&row[*i])?;
            }
        }
    }
    match ops {
        None if key_positions.is_empty() => {
            let mut row: Tuple = Tuple::from_vec(vec![DataValue::Null; signature_len]);
            for (op, (i, _, _)) in fresh_ops()?.iter().zip(val_positions) {
                row[*i] = op.get()?;
            }
            Ok(Some(row))
        }
        None => Ok(None),
        Some(ops) => {
            let mut row: Tuple = Tuple::from_vec(vec![DataValue::Null; signature_len]);
            for (slot, &i) in key_positions.iter().enumerate() {
                row[i] = group_key[slot].clone();
            }
            for (op, (i, _, _)) in ops.iter().zip(val_positions) {
                row[*i] = op.get()?;
            }
            Ok(Some(row))
        }
    }
}

/// The incremental-maintenance law for an aggregating head, production
/// twin of `laws::eval_aggregating_head_incremental` — same algorithm
/// (candidates-then-verify extended one level: find affected GROUPS,
/// fully re-derive each one directly), same reason it is sound uniformly
/// across every aggregation kind without a per-kind delta formula (see
/// that function's doc). Reuses the REAL landed `Aggregation::normal_op`
/// directly, never a second hand-rolled fold.
fn eval_aggregating_head_incremental(
    rules: &[&Rule],
    state: &MaintainedState,
    new_state: &MaintainedState,
    rel_deltas: &BTreeMap<Symbol, BTreeSet<SignedFact>>,
    old_rows: &BTreeSet<Tuple>,
) -> Result<BTreeSet<SignedFact>> {
    let signature = &rules[0].aggr;
    let key_positions: Vec<usize> = signature
        .iter()
        .enumerate()
        .filter(|(_, a)| !a.is_aggregated())
        .map(|(i, _)| i)
        .collect();
    let val_positions: Vec<(usize, &Aggregation, &[DataValue])> = signature
        .iter()
        .enumerate()
        .filter_map(|(i, a)| a.as_aggregated().map(|(aggr, args)| (i, aggr, args)))
        .collect();

    let old_by_key: BTreeMap<Tuple, Tuple> = old_rows
        .iter()
        .map(|row| {
            let key: Tuple = key_positions.iter().map(|i| row[*i].clone()).collect();
            (key, row.clone())
        })
        .collect();

    let mut affected = collect_affected_groups(rules, state, rel_deltas, &key_positions);
    // The global (no GROUP BY) special case: a pre-existing global
    // aggregate must be re-checked whenever ANY dependency had ANY delta
    // at all, even with zero raw candidates — its last remaining member
    // could have just been retracted (`collect_candidates` DOES surface
    // that: a `Minus` is as valid a driver as a `Plus`), so this only
    // matters when NO dependency had a delta, in which case there is
    // nothing to re-check anyway.
    if key_positions.is_empty() && rel_deltas.values().any(|d| !d.is_empty()) {
        affected.insert(Tuple::new());
    }

    let mut delta = BTreeSet::new();
    for group_key in &affected {
        let new_row = eval_one_group(
            rules,
            new_state,
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
            (Some(_), Some(_)) => {
                // Same aggregate row — group unchanged.
            }
            (Some(old), None) => {
                delta.insert(SignedFact::Minus(old));
            }
            (None, Some(new)) => {
                delta.insert(SignedFact::Plus(new));
            }
            (None, None) => {
                // Group absent before and after.
            }
        }
    }
    Ok(delta)
}

/// The production incremental-maintenance law (issue #61): given a signed
/// patch to `program`'s EDB and its CURRENT [`MaintainedState`], the
/// signed patch every relation (EDB and IDB alike) undergoes, computed
/// WITHOUT re-evaluating the whole program — and the NEW state, for the
/// caller to persist as this round's [`MaintainedState`] going forward.
/// Refuses (never silently wrong) recursion; fixed rules have no
/// representation in [`IncrementalProgram`] at all, so there is nothing
/// to refuse for them here. Aggregation (normal or meet form) is fully
/// covered via [`eval_aggregating_head_incremental`] — see the module
/// doc's scope section.
pub fn incremental_eval(
    program: &IncrementalProgram,
    state: &MaintainedState,
    edb_patch: &BTreeMap<Symbol, BTreeSet<SignedFact>>,
) -> Result<(BTreeMap<Symbol, BTreeSet<SignedFact>>, MaintainedState)> {
    // A well-formed signed patch never claims BOTH a gain and a loss of the
    // SAME tuple in one round — that would mean "this fact just became true
    // AND just became false," which no caller can coherently mean. Callers
    // (`standing.rs::apply_pending`) are required to NET raw callback events
    // down to one sign per tuple before calling in here; this is the
    // 0.9.0-review bug's own invariant, checked at the one seam every caller
    // must cross, not re-derived at every call site.
    ensure!(
        edb_patch.values().all(|facts| {
            let pluses: BTreeSet<&Tuple> = facts
                .iter()
                .filter_map(|f| match f {
                    SignedFact::Plus(t) => Some(t),
                    SignedFact::Minus(_) => None,
                })
                .collect();
            let minuses: BTreeSet<&Tuple> = facts
                .iter()
                .filter_map(|f| match f {
                    SignedFact::Minus(t) => Some(t),
                    SignedFact::Plus(_) => None,
                })
                .collect();
            pluses.is_disjoint(&minuses)
        }),
        "incremental_eval received a patch with both Plus(t) and Minus(t) for the same t — \
         the caller must net raw events by tuple before calling in, exactly the bug \
         apply_pending's netting step (standing.rs) exists to prevent"
    );
    if has_any_cycle(program) {
        return Err(Error::from(IncrementalRejection::Recursive));
    }

    let patched: BTreeSet<Symbol> = edb_patch.keys().cloned().collect();
    let order = topological_order(program, &patched);
    let edb = edb_relations(program, &patched);
    let mut rel_deltas: BTreeMap<Symbol, BTreeSet<SignedFact>> = BTreeMap::new();
    let mut new_state: MaintainedState = BTreeMap::new();

    for rel in order {
        let old_rows = match state.get(&rel) {
            Some(rows) => rows.clone(),
            None => BTreeSet::new(),
        };
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
            let has_aggr = rules
                .iter()
                .any(|r| r.aggr.iter().any(|a| a.is_aggregated()));
            let delta = if has_aggr {
                eval_aggregating_head_incremental(
                    &rules,
                    state,
                    &new_state,
                    &rel_deltas,
                    &old_rows,
                )?
            } else {
                let mut candidates = BTreeSet::new();
                for rule in &rules {
                    collect_candidates(rule, state, &rel_deltas, &mut candidates);
                }
                let mut delta = BTreeSet::new();
                for candidate in candidates {
                    let was = old_rows.contains(&candidate);
                    let now = head_is_derivable(&rules, &new_state, &candidate);
                    match (was, now) {
                        (false, true) => {
                            delta.insert(SignedFact::Plus(candidate));
                        }
                        (true, false) => {
                            delta.insert(SignedFact::Minus(candidate));
                        }
                        (true, true) | (false, false) => {
                            // Truth value unchanged — not a delta.
                        }
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
        new_state.insert(rel.clone(), new_rows);
        rel_deltas.insert(rel, delta);
    }
    Ok((rel_deltas, new_state))
}

#[cfg(test)]
mod tests {
    use miette::{Result, miette};
    use super::*;
    use kyzo_model::SourceSpan;
    use kyzo_model::value::Num;

    fn sym(name: &str) -> Symbol {
        Symbol::new(name, SourceSpan::default())
    }
    fn v(i: i64) -> DataValue {
        DataValue::Num(Num::int(i))
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
            polarity: if negated {
                Polarity::Negative
            } else {
                Polarity::Positive
            },
        }
    }
    fn rule(head_rel: &str, head_args: Vec<Term>, body: Vec<Literal>) -> Rule {
        let aggr = (0..head_args.len()).map(|_| HeadAggrSlot::Plain).collect();
        Rule {
            head_rel: sym(head_rel),
            head_args,
            body,
            aggr,
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
    fn retraction_through_negation_produces_a_new_fact() -> Result<()>  {
        let program = IncrementalProgram {
            rules: vec![rule(
                "q",
                vec![x()],
                vec![lit("p", vec![x()], false), lit("r", vec![x()], true)],
            )],
        };
        let state = state_of(vec![
            ("p", vec![Tuple::from_vec(vec![v(1)])]),
            ("r", vec![Tuple::from_vec(vec![v(1)])]),
        ]);
        let patch = patch_of(vec![("r", SignedFact::Minus(Tuple::from_vec(vec![v(1)])))]);
        let (deltas, new_state) = incremental_eval(&program, &state, &patch)?;
        assert_eq!(
            deltas[&sym("q")],
            [SignedFact::Plus(Tuple::from_vec(vec![v(1)]))]
                .into_iter()
                .collect()
        );
        assert_eq!(
            new_state[&sym("q")],
            [vec![v(1)]].into_iter().map(Tuple::from_vec).collect()
        );
        Ok(())
    }

    /// The mirror: asserting into the negated relation retracts the
    /// dependent fact.
    #[test]
    fn assertion_into_negation_retracts_the_dependent_fact() -> Result<()>  {
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
            ("p", vec![Tuple::from_vec(vec![v(1)])]),
            ("r", vec![]),
            ("q", vec![Tuple::from_vec(vec![v(1)])]),
        ]);
        let patch = patch_of(vec![("r", SignedFact::Plus(Tuple::from_vec(vec![v(1)])))]);
        let (deltas, new_state) = incremental_eval(&program, &state, &patch)?;
        assert_eq!(
            deltas[&sym("q")],
            [SignedFact::Minus(Tuple::from_vec(vec![v(1)]))]
                .into_iter()
                .collect()
        );
        assert!(new_state[&sym("q")].is_empty());
        Ok(())
    }

    /// A tuple with TWO independent derivations: only ONE is touched by
    /// the patch, and the untouched one must still hold the fact up —
    /// exactly the multiset-vs-set bug the oracle's differential caught
    /// on its first run (`laws.rs`'s module doc). This module's own
    /// direct test for the same law.
    #[test]
    fn a_second_untouched_derivation_holds_the_fact_up() -> Result<()>  {
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
            (
                "p",
                vec![
                    Tuple::from_vec(vec![v(2), v(1)]),
                    Tuple::from_vec(vec![v(2), v(3)]),
                ],
            ),
            ("r", vec![]),
            ("q", vec![Tuple::from_vec(vec![v(2)])]),
        ]);
        let patch = patch_of(vec![(
            "p",
            SignedFact::Minus(Tuple::from_vec(vec![v(2), v(1)])),
        )]);
        let (deltas, new_state) = incremental_eval(&program, &state, &patch)?;
        assert!(
            deltas.get(&sym("q")).is_none_or(BTreeSet::is_empty),
            "q(2) has a second, untouched derivation — must not change: {:?}",
            deltas.get(&sym("q"))
        );
        assert_eq!(
            new_state[&sym("q")],
            [vec![v(2)]].into_iter().map(Tuple::from_vec).collect()
        );
        Ok(())
    }

    #[test]
    fn recursion_is_refused() -> Result<()>  {
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
        let state = state_of(vec![("edge", vec![Tuple::from_vec(vec![v(1), v(2)])])]);
        let patch = patch_of(vec![(
            "edge",
            SignedFact::Plus(Tuple::from_vec(vec![v(2), v(3)])),
        )]);
        let err = incremental_eval(&program, &state, &patch)
            .err()
            .ok_or_else(|| miette!("expected error"))?;
        assert!(err.to_string().contains("recursive"));
        Ok(())
    }

    // Oracle-differential incremental property corpus moved to
    // `kyzo-trials` (crate wall: kyzo-core must not import kyzo_oracle).
}
