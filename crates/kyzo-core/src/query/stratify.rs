/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): the stratifier feeds the typestate tiers of `data/program.rs`
 * — its output is minted through
 * `StratifiedNormalFormProgram::from_reverse_execution_order`, which
 * reverses to execution order once and proves the entry sits in the final
 * stratum (the original returned raw reversed strata that `compile.rs`
 * un-reversed by convention); the entry is reached through the program's
 * entry field, never by re-spelling `Symbol::new("?", …)` with a dummy
 * span; store lifetimes are the documented `StoreLifetimes` type fed in
 * execution-order indices directly (the original's bare
 * `BTreeMap<MagicSymbol, usize>` computed `n_strata - 1 - stratum` inline);
 * the reduced-graph `unwrap` is a typed internal error; the
 * unstratifiability refusal carries the source span of the atom that
 * *establishes* the poisoned edge (the dependency map keys by first
 * occurrence, so the first-read symbol's span would mislabel a later
 * negation); `generalized_kahn` is sized by the SCC count, not the node
 * count (the original passed the node count, harmlessly emitting phantom
 * component ids); the aggregation classification is computed by one
 * helper instead of twice inline; tests build programs through the tier
 * constructors instead of a live `DbInstance`.
 */

//! Stratification: the proof that a program's negations and aggregations
//! have a sound evaluation order — and the refusal of every program whose
//! don't. The refusal is the feature: a missed refusal here does not crash,
//! it silently yields *wrong answers* (a `not` read before its relation is
//! complete, an aggregate folded over a half-computed fixpoint).
//!
//! The dependency graph has an edge from each rule to every rule its bodies
//! read, and an edge is *poisoned* when the dependency must be **complete**
//! before the dependent may start:
//!
//! - negated dependencies (`not r[…]`) — negation reads absence, and
//!   absence is only meaningful of a finished relation;
//! - every dependency of a rule head that aggregates with a normal
//!   (non-meet) form — such a fold is only correct over the finished set;
//! - reads of a meet-aggregated or fixed-rule head by other rules — their
//!   stores hold folded/algorithmic results, meaningful only when complete.
//!
//! The one legal aggregation inside recursion is a head **all** of whose
//! rules aggregate with meet (semilattice) forms reading *itself*,
//! positively: a meet folds soundly while the fixpoint still grows.
//! Fixed rules (graph algorithms applied as rules) are always stratum
//! bounded: poisoned edges both to their inputs and from their readers.
//!
//! Tarjan's SCC then finds the recursive families; a poisoned edge inside
//! one is the unstratifiable refusal. The generalized Kahn's algorithm
//! (`query/graph.rs`) lays the SCC condensation out into strata with no
//! poisoned edge inside any stratum, and the result is minted as the
//! [`StratifiedNormalFormProgram`] tier — possession of that type *is* the
//! proof this module's checks passed. Alongside it the stratifier reports
//! each intermediate store's last use ([`StoreLifetimes`]), which is what
//! lets evaluation drop stores between strata.
//!
//! The reference semantics this must agree with — the naive stratification
//! checker and the refusal corpus — live in `query/laws.rs` (law 2); the
//! tests here run the corpus through this real stratifier.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use miette::{Diagnostic, Result, bail, ensure};
use thiserror::Error;

use crate::data::program::{
    FixedRuleArg, MagicSymbol, NormalFormAtom, NormalFormInlineRule, NormalFormProgram,
    NormalFormRulesOrFixed, NormalFormStratum, StoreLifetimes, StratifiedNormalFormProgram,
};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::query::graph::{
    Graph, StratifiedGraph, generalized_kahn, reachable_components, strongly_connected_components,
};

/// The refusal at the heart of this module: a negation, non-meet
/// aggregation, or fixed-rule application sits inside a recursive cycle, so
/// no evaluation order is sound. Accepting such a program would not fail —
/// it would answer wrongly.
#[derive(Debug, Error, Diagnostic)]
#[error("Query is unstratifiable")]
#[diagnostic(code(eval::unstratifiable))]
#[diagnostic(help(
    "The rule '{name}' is in the strongly connected component {scc:?},\n\
     and is involved in at least one forbidden dependency \n\
     (negation, non-meet aggregation, or algorithm-application)."
))]
struct UnStratifiableProgram {
    name: String,
    scc: Vec<String>,
    #[label("this dependency closes a cycle it may not be part of")]
    span: SourceSpan,
}

/// A stratifier invariant that its construction steps should have made
/// impossible. Returned (never panicked) so corruption of that proof
/// surfaces as an error, not an abort.
#[derive(Debug, Error, Diagnostic)]
#[error("Stratifier invariant violated: {0}")]
#[diagnostic(code(compiler::stratifier_invariant))]
#[diagnostic(help("This is a bug. Please report it."))]
struct StratifierInvariantError(&'static str);

impl NormalFormAtom {
    /// The rule names this atom reads, each mapped to whether the read is
    /// negated. Stored-relation reads, predicates and unifications depend
    /// on no rule. (The resolved index-search atoms land with the index
    /// tier; they read stored relations, so they too will contribute no
    /// rule dependency.)
    fn contained_rules(&self) -> BTreeMap<&Symbol, bool> {
        match self {
            NormalFormAtom::Relation(_)
            | NormalFormAtom::NegatedRelation(_)
            | NormalFormAtom::Predicate(_)
            | NormalFormAtom::Unification(_)
            // A search reads a stored relation and its index: no rule
            // dependency, exactly like Relation.
            | NormalFormAtom::Search(_) => Default::default(),
            NormalFormAtom::Rule(r) => BTreeMap::from([(&r.name, false)]),
            NormalFormAtom::NegatedRule(r) => BTreeMap::from([(&r.name, true)]),
        }
    }
}

/// How a rule set aggregates: `has_aggr` when any rule aggregates any head
/// position; `is_meet` when it aggregates and every aggregated position of
/// every rule uses a meet form — the one class whose self-recursion is
/// evaluable (the fold is a semilattice meet, sound while the fixpoint
/// still grows). Non-aggregated positions do not disqualify a meet head.
///
/// **Deliberately independent of `query/laws.rs::head_classes`, and this is
/// load-bearing (issue #89's ruling).** This is the ENGINE's classification
/// — it feeds the real stratifier that gates what the compiler will
/// actually evaluate — while `head_classes` is the ORACLE's twin, feeding
/// `naive_eval`. Story #89 consolidated the reference-tier triplication of
/// this same classification (`laws.rs`/`provenance.rs`/`trials.rs`, three
/// copies judging the engine, never each other, so sharing among them cost
/// nothing) but explicitly did NOT fold this one in with them: the whole
/// point of `the_oracle_refusal_corpus_is_refused` (this module's test
/// suite) is that the refusal BOUNDARY this function helps compute and the
/// oracle's independently-computed boundary agree despite being two
/// separately hand-maintained implementations. Sharing this function with
/// `head_classes` would collapse that differential into a tautology — a
/// bug in the shared logic would silently pass both "independent" checks at
/// once. Keep every future edit here hand-applied, never routed through the
/// oracle's copy.
fn aggregation_character(rules: &[NormalFormInlineRule]) -> (bool, bool) {
    let has_aggr = rules
        .iter()
        .any(|rule| rule.aggr.iter().any(|a| a.is_some()));
    let is_meet = has_aggr
        && rules.iter().all(|rule| {
            rule.aggr.iter().all(|v| match v {
                None => true,
                Some((v, _)) => v.is_meet(),
            })
        });
    (has_aggr, is_meet)
}

/// For each poisoned edge `(dependent, dependency)`, the source span of the
/// body atom that *established* the poison — the first atom whose read
/// poisoned the edge, either at insertion or by upgrading an existing
/// benign edge. The dependency map itself keys `&Symbol` by first
/// occurrence, so the map key's span may belong to an innocent positive
/// read; the refusal diagnostic must label the poisoning atom instead.
type PoisonSpans<'a> = BTreeMap<(&'a Symbol, &'a Symbol), SourceSpan>;

/// The program's dependency graph, edges labelled poisoned (`true`) when
/// the dependency forces a stratum boundary, together with the span of the
/// atom that established each poison (for the refusal diagnostic). The
/// rules are the module doc's; the shape is the original's, decision for
/// decision:
///
/// - a head with normal aggregation poisons **every** dependency;
/// - an all-meet head's read of *itself* is poisoned only if negated or a
///   fixed rule (i.e. meet self-recursion is the one exemption) — a meet
///   head reading a *different* rule is poisoned like any aggregation;
/// - a non-aggregating rule's read is poisoned when negated, or when the
///   dependency is a fixed rule or a (different) meet-aggregated head;
/// - a fixed rule poisons every in-memory rule it takes as input.
fn convert_normal_form_program_to_graph(
    nf_prog: &NormalFormProgram,
) -> (StratifiedGraph<&'_ Symbol>, PoisonSpans<'_>) {
    let meet_rules: BTreeSet<&Symbol> = nf_prog
        .iter_all()
        .filter_map(|(k, ruleset)| match ruleset {
            NormalFormRulesOrFixed::Rules { rules } => {
                let (_, is_meet) = aggregation_character(rules);
                if is_meet { Some(k) } else { None }
            }
            NormalFormRulesOrFixed::Fixed { fixed: _ } => None,
        })
        .collect();
    let fixed_rules: BTreeSet<&Symbol> = nf_prog
        .iter_all()
        .filter_map(|(k, ruleset)| match ruleset {
            NormalFormRulesOrFixed::Rules { rules: _ } => None,
            NormalFormRulesOrFixed::Fixed { fixed: _ } => Some(k),
        })
        .collect();
    let mut graph: StratifiedGraph<&Symbol> = BTreeMap::default();
    let mut poison_spans: PoisonSpans<'_> = BTreeMap::default();
    for (k, ruleset) in nf_prog.iter_all() {
        match ruleset {
            NormalFormRulesOrFixed::Rules { rules: ruleset } => {
                let mut ret: BTreeMap<&Symbol, bool> = BTreeMap::default();
                let (has_aggr, is_meet) = aggregation_character(ruleset);
                for rule in ruleset {
                    for atom in &rule.body {
                        let contained = atom.contained_rules();
                        for (found_key, is_negated) in contained {
                            let found_key_is_meet =
                                meet_rules.contains(found_key) && found_key != k;
                            let found_key_is_fixed_rule = fixed_rules.contains(found_key);
                            match ret.entry(found_key) {
                                Entry::Vacant(e) => {
                                    let poisoned = if has_aggr {
                                        if is_meet && k == found_key {
                                            found_key_is_fixed_rule || is_negated
                                        } else {
                                            true
                                        }
                                    } else {
                                        found_key_is_fixed_rule || found_key_is_meet || is_negated
                                    };
                                    if poisoned {
                                        poison_spans
                                            .entry((k, found_key))
                                            .or_insert(found_key.span);
                                    }
                                    e.insert(poisoned);
                                }
                                Entry::Occupied(mut e) => {
                                    let old = *e.get();
                                    let new_val = if has_aggr {
                                        if is_meet && k == found_key {
                                            found_key_is_fixed_rule
                                                || found_key_is_meet
                                                || is_negated
                                        } else {
                                            true
                                        }
                                    } else {
                                        found_key_is_fixed_rule || found_key_is_meet || is_negated
                                    };
                                    // This atom upgrades a benign edge to
                                    // poisoned: it is the establishing atom,
                                    // even though the map key (and its span)
                                    // stays the first occurrence's.
                                    if new_val && !old {
                                        poison_spans
                                            .entry((k, found_key))
                                            .or_insert(found_key.span);
                                    }
                                    e.insert(old || new_val);
                                }
                            }
                        }
                    }
                }
                graph.insert(k, ret);
            }
            NormalFormRulesOrFixed::Fixed { fixed } => {
                let mut ret: BTreeMap<&Symbol, bool> = BTreeMap::default();
                for rel in &fixed.rule_args {
                    match rel {
                        FixedRuleArg::InMem { name, .. } => {
                            ret.insert(name, true);
                            poison_spans.entry((k, name)).or_insert(name.span);
                        }
                        FixedRuleArg::Stored { .. } | FixedRuleArg::NamedStored { .. } => {}
                    }
                }
                graph.insert(k, ret);
            }
        }
    }
    (graph, poison_spans)
}

/// The dependency graph with the poison labels forgotten, for reachability
/// and SCC computation.
fn reduce_to_graph<'a>(g: &StratifiedGraph<&'a Symbol>) -> Graph<&'a Symbol> {
    g.iter()
        .map(|(k, s)| (*k, s.keys().copied().collect::<Vec<_>>()))
        .collect()
}

/// The refusal check: a poisoned edge whose two ends sit in the same
/// strongly connected component means a negation, non-meet aggregation, or
/// fixed rule inside recursion — unstratifiable. The span on the error is
/// the atom that *established* the poison (looked up in `poison_spans`),
/// so the diagnostic points at the negated/offending read, not at whichever
/// read of the same rule happened to occur first in the bodies.
fn verify_no_cycle(
    g: &StratifiedGraph<&'_ Symbol>,
    poison_spans: &PoisonSpans<'_>,
    sccs: &[BTreeSet<&Symbol>],
) -> Result<()> {
    for (k, vs) in g {
        for scc in sccs {
            if scc.contains(k) {
                for (v, negated) in vs {
                    ensure!(
                        !negated || !scc.contains(v),
                        UnStratifiableProgram {
                            name: v.to_string(),
                            scc: scc.iter().map(|v| v.to_string()).collect(),
                            span: poison_spans.get(&(*k, *v)).copied().unwrap_or(v.span),
                        }
                    );
                }
            }
        }
    }
    Ok(())
}

/// The condensation: collapse each SCC to one node (its index in `sccs`),
/// keeping inter-component edges and merging their poison labels (an edge
/// poisoned anywhere is poisoned in the condensation). Self-edges vanish —
/// [`verify_no_cycle`] has already proven none of them poisoned.
fn make_scc_reduced_graph(
    sccs: &[BTreeSet<&Symbol>],
    graph: &StratifiedGraph<&Symbol>,
) -> Result<(BTreeMap<Symbol, usize>, StratifiedGraph<usize>)> {
    let indices = sccs
        .iter()
        .enumerate()
        .flat_map(|(idx, scc)| scc.iter().map(move |k| ((*k).clone(), idx)))
        .collect::<BTreeMap<_, _>>();
    let mut ret: BTreeMap<usize, BTreeMap<usize, bool>> = Default::default();
    for (from, tos) in graph {
        let from_idx = *indices
            .get(*from)
            .ok_or(StratifierInvariantError("a graph node is in no SCC"))?;
        let cur_entry = ret.entry(from_idx).or_default();
        for (to, poisoned) in tos {
            let to_idx = match indices.get(*to) {
                Some(i) => *i,
                // A dependency on an undefined rule name: not a node,
                // resolved (or refused) by a later tier.
                None => continue,
            };
            if from_idx == to_idx {
                continue;
            }
            match cur_entry.entry(to_idx) {
                Entry::Vacant(e) => {
                    e.insert(*poisoned);
                }
                Entry::Occupied(mut e) => {
                    let old_p = *e.get();
                    e.insert(old_p || *poisoned);
                }
            }
        }
    }
    Ok((indices, ret))
}

impl NormalFormProgram {
    /// Stratify: prove this program's negations and aggregations have a
    /// sound evaluation order, or refuse it. On success, mint the
    /// [`StratifiedNormalFormProgram`] tier together with the
    /// [`StoreLifetimes`] evaluation uses to drop dead stores between
    /// strata. Rules unreachable from the entry are pruned here and never
    /// evaluated.
    ///
    /// Prerequisite: the program is already in disjunctive normal form
    /// (this type is proof of that).
    pub(crate) fn into_stratified_program(
        self,
    ) -> Result<(StratifiedNormalFormProgram, StoreLifetimes)> {
        // 0. build the labelled dependency graph of the program, plus the
        // span of the atom establishing each poisoned edge (diagnostics)
        let (stratified_graph, poison_spans) = convert_normal_form_program_to_graph(&self);
        let graph = reduce_to_graph(&stratified_graph);

        // 1. find reachable rules starting from the entry — the entry
        // field itself, real span and all, never a re-spelled `?` probe
        let entry = self.entry_name();
        let reachable: BTreeSet<Symbol> = reachable_components(&graph, &entry)
            .into_iter()
            .map(|k| (*k).clone())
            .collect();
        // 2. prune the graph of unreachable rules
        let stratified_graph: StratifiedGraph<&Symbol> = stratified_graph
            .into_iter()
            .filter(|(k, _)| reachable.contains(*k))
            .collect();
        let graph: Graph<&Symbol> = graph
            .into_iter()
            .filter(|(k, _)| reachable.contains(*k))
            .collect();
        // 3. find the SCCs — the recursive families
        let sccs: Vec<BTreeSet<&Symbol>> = strongly_connected_components(&graph)?
            .into_iter()
            .map(|scc| scc.into_iter().copied().collect())
            .collect();
        // 4. refuse if any SCC contains a poisoned edge: THE soundness gate
        verify_no_cycle(&stratified_graph, &poison_spans, &sccs)?;
        // 5. collapse the SCCs into the condensation DAG
        let (invert_indices, reduced_graph) = make_scc_reduced_graph(&sccs, &stratified_graph)?;
        // 6. topologically sort the condensation into strata. Kahn emits
        // them in REVERSE execution order (it starts from the entry's
        // component and walks toward the dependencies).
        let sort_result = generalized_kahn(&reduced_graph, sccs.len())?;

        // The same sort read in each of the two index spaces, by direct
        // enumeration — no `n_strata - 1 - i` arithmetic anywhere:
        // component → position in Kahn's (reversed) output, and
        // component → execution-order stratum index.
        let rev_stratum_of: BTreeMap<usize, usize> = sort_result
            .iter()
            .enumerate()
            .flat_map(|(rev_idx, comps)| comps.iter().map(move |comp| (*comp, rev_idx)))
            .collect();
        let exec_stratum_of: BTreeMap<usize, usize> = sort_result
            .iter()
            .rev()
            .enumerate()
            .flat_map(|(exec_idx, comps)| comps.iter().map(move |comp| (*comp, exec_idx)))
            .collect();

        // 7. store lifetimes: each dependency's store is read by its
        // dependents, so it must live to the last execution-order stratum
        // holding one; `note_use` keeps the maximum.
        let mut store_lifetimes = StoreLifetimes::default();
        for (fr, tos) in &stratified_graph {
            if let Some(fr_component) = invert_indices.get(*fr)
                && let Some(fr_exec_stratum) = exec_stratum_of.get(fr_component)
            {
                for to in tos.keys() {
                    // Stratum ordering, checked at the one place every
                    // dependency edge passes through: `to` (the dependency,
                    // read by `fr`'s body) must execute AT OR BEFORE `fr`
                    // (the dependent) — Kahn walked backward from the entry
                    // to its dependencies, so `to`'s execution-order index
                    // can never exceed `fr`'s. A violation here would mean a
                    // rule reading state that has not been computed yet.
                    debug_assert!(
                        invert_indices
                            .get(to)
                            .and_then(|to_component| exec_stratum_of.get(to_component))
                            .is_none_or(|to_exec_stratum| *to_exec_stratum <= *fr_exec_stratum),
                        "stratum ordering violated: dependency {to:?} (stratum {:?}) executes \
                         after its dependent {fr:?} (stratum {fr_exec_stratum})",
                        invert_indices.get(to).and_then(|c| exec_stratum_of.get(c))
                    );
                    store_lifetimes.note_use(
                        MagicSymbol::Muggle {
                            inner: (*to).clone(),
                        },
                        *fr_exec_stratum,
                    );
                }
            }
        }

        // 8. distribute the rule sets into strata, still in the reverse
        // execution order Kahn emitted (the tier constructor owns the one
        // reversal). Rules absent from `invert_indices` are the pruned,
        // unreachable ones; a reachable rule missing from the sort would
        // mean Kahn dropped a node, which its own checks refute.
        let mut reversed_strata: Vec<NormalFormStratum> = (0..sort_result.len())
            .map(|_| NormalFormStratum::default())
            .collect();
        let ((entry_name, entry_rules), rules, disable_magic_rewrite) = self.into_parts();
        for (name, ruleset) in rules
            .into_iter()
            .chain(std::iter::once((entry_name, entry_rules)))
        {
            let Some(component) = invert_indices.get(&name) else {
                continue; // unreachable from the entry: pruned
            };
            let Some(rev_idx) = rev_stratum_of.get(component) else {
                bail!(StratifierInvariantError("a reachable rule was not sorted"));
            };
            match reversed_strata.get_mut(*rev_idx) {
                Some(stratum) => {
                    stratum.rules.insert(name, ruleset);
                }
                None => bail!(StratifierInvariantError("stratum index out of range")),
            }
        }

        // 9. mint the stratified tier: the constructor reverses to
        // execution order and proves the entry sits in the final stratum.
        // That always holds for what this function feeds it: after pruning,
        // every node is reachable from the entry, so in the condensation
        // every non-entry component has an incoming edge (from its
        // predecessor on a path from the entry) while the entry's component
        // has none (an edge into it would come from a node it also reaches
        // — same component). The entry's component is therefore the only
        // possible Kahn starting point and lands in reversed stratum 0 —
        // the final execution stratum.
        let stratified = StratifiedNormalFormProgram::from_reverse_execution_order(
            reversed_strata,
            disable_magic_rewrite,
        )?;
        Ok((stratified, store_lifetimes))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::data::aggr::{Aggregation, parse_aggr};
    use crate::data::expr::Expr;
    use crate::data::program::{
        BodyNormalizer, FixedRule, FixedRuleApply, FixedRuleHandle, InputAtom, InputInlineRule,
        InputInlineRulesOrFixed, InputProgram, InputRuleApplyAtom, NormalFormRuleApplyAtom,
        QueryOutOptions, Trivia,
    };
    use crate::data::symb::SymbolKind;
    use crate::data::value::DataValue;
    use crate::query::laws;

    fn sym(name: &str) -> Symbol {
        Symbol::new(name, SourceSpan(0, 0))
    }

    /// A positive read of rule `name` in a body.
    fn dep(name: &str) -> InputAtom {
        InputAtom::Rule {
            inner: InputRuleApplyAtom {
                name: sym(name),
                args: vec![],
                span: SourceSpan(0, 0),
            },
        }
    }

    /// A negated read of rule `name` in a body.
    fn neg_dep(name: &str) -> InputAtom {
        InputAtom::Negation {
            inner: Box::new(dep(name)),
            span: SourceSpan(0, 0),
        }
    }

    /// A pass-through normalizer: the tests build bodies that are already
    /// flat conjunctions of rule reads, so DNF is the identity fan-in and
    /// well-ordering is the identity. (The real normalizer — DNF over the
    /// catalog plus binding-safety reordering — is the logical/reorder
    /// port; the stratifier only reads names, negation, and aggregations.)
    struct PassThrough;

    impl BodyNormalizer for PassThrough {
        fn disjunctive_normal_form(&mut self, body: InputAtom) -> Result<Vec<Vec<NormalFormAtom>>> {
            fn flatten(atom: InputAtom, out: &mut Vec<NormalFormAtom>) {
                match atom {
                    InputAtom::Conjunction { inner, .. } => {
                        for a in inner {
                            flatten(a, out);
                        }
                    }
                    InputAtom::Rule { inner } => {
                        out.push(NormalFormAtom::Rule(NormalFormRuleApplyAtom {
                            name: inner.name,
                            args: vec![],
                            span: inner.span,
                        }));
                    }
                    InputAtom::Negation { inner, .. } => match *inner {
                        InputAtom::Rule { inner } => {
                            out.push(NormalFormAtom::NegatedRule(NormalFormRuleApplyAtom {
                                name: inner.name,
                                args: vec![],
                                span: inner.span,
                            }));
                        }
                        InputAtom::NamedFieldRelation { .. } | InputAtom::Relation { .. } | InputAtom::Predicate { .. } | InputAtom::Negation { .. } | InputAtom::Conjunction { .. } | InputAtom::Disjunction { .. } | InputAtom::Unification { .. } | InputAtom::Search { .. } => panic!("test bodies negate rule reads only"),
                    },
                    InputAtom::NamedFieldRelation { .. } | InputAtom::Relation { .. } | InputAtom::Predicate { .. } | InputAtom::Disjunction { .. } | InputAtom::Unification { .. } | InputAtom::Search { .. } => panic!("test bodies contain rule reads only"),
                }
            }
            let mut out = vec![];
            flatten(body, &mut out);
            Ok(vec![out])
        }

        fn well_order(
            &mut self,
            rule: crate::data::program::NormalFormInlineRule,
        ) -> Result<crate::data::program::NormalFormInlineRule> {
            Ok(rule)
        }
    }

    /// A fixed rule whose runtime never runs: stratification only needs the
    /// *shape* (a fixed head plus its in-memory inputs).
    struct StubFixedRule;

    impl FixedRule for StubFixedRule {
        fn arity(
            &self,
            _options: &BTreeMap<smartstring::SmartString<smartstring::LazyCompact>, Expr>,
            _rule_head: &[Symbol],
            _span: SourceSpan,
        ) -> Result<usize> {
            Ok(1)
        }
        fn run(
            &self,
            _payload: crate::fixed_rule::FixedRulePayload<'_>,
            _out: &mut crate::fixed_rule::FixedRuleOutput,
            _cancel: crate::fixed_rule::CancelFlag,
        ) -> Result<()> {
            unreachable!("test stub: never run")
        }
    }

    /// A tiny program builder over the real tier constructors.
    #[derive(Default)]
    struct Prog {
        prog: BTreeMap<Symbol, InputInlineRulesOrFixed>,
    }

    impl Prog {
        /// Add one rule `head[…] := body…`, `aggrs` naming the per-position
        /// head aggregations (`None` = plain position).
        fn rule(self, head: &str, aggrs: &[Option<&str>], body: Vec<InputAtom>) -> Self {
            let aggr = aggrs
                .iter()
                .map(|a| {
                    a.map(|name| {
                        (
                            parse_aggr(name)
                                .unwrap_or_else(|| panic!("real aggregation exists: {name}")),
                            vec![],
                        )
                    })
                })
                .collect();
            self.rule_raw(head, aggr, body)
        }

        fn rule_raw(
            mut self,
            head: &str,
            aggr: Vec<Option<(Aggregation, Vec<DataValue>)>>,
            body: Vec<InputAtom>,
        ) -> Self {
            let head_syms: Vec<Symbol> = (0..aggr.len()).map(|i| sym(&format!("v{i}"))).collect();
            let rule = InputInlineRule {
                head: head_syms,
                aggr,
                body,
                span: SourceSpan(0, 0),
                trivia: Trivia::default(),
            };
            match self
                .prog
                .entry(sym(head))
                .or_insert_with(|| InputInlineRulesOrFixed::Rules { rules: vec![] })
            {
                InputInlineRulesOrFixed::Rules { rules } => rules.push(rule),
                InputInlineRulesOrFixed::Fixed { .. } => {
                    panic!("test program defines {head} as both rules and fixed")
                }
            }
            self
        }

        /// Define `head` as a fixed-rule application over in-memory inputs.
        fn fixed(mut self, head: &str, inputs: &[&str]) -> Self {
            let apply = FixedRuleApply {
                fixed_handle: FixedRuleHandle {
                    name: sym("StubAlgo"),
                },
                rule_args: inputs
                    .iter()
                    .map(|name| FixedRuleArg::InMem {
                        name: sym(name),
                        bindings: vec![],
                        span: SourceSpan(0, 0),
                    })
                    .collect(),
                options: Arc::new(BTreeMap::new()),
                head: vec![],
                arity: 1,
                span: SourceSpan(0, 0),
                fixed_impl: Arc::new(StubFixedRule),
                trivia: Trivia::default(),
            };
            let prev = self
                .prog
                .insert(sym(head), InputInlineRulesOrFixed::Fixed { fixed: apply });
            assert!(prev.is_none(), "test program redefines {head}");
            self
        }

        fn stratify(self) -> Result<(StratifiedNormalFormProgram, StoreLifetimes)> {
            let input = InputProgram::new(self.prog, QueryOutOptions::default(), false)?;
            let (normalized, _opts) = input.into_normalized_program(&mut PassThrough)?;
            normalized.into_stratified_program()
        }
    }

    /// The execution-order stratum index holding rule `name`, if any.
    fn stratum_of(program: &StratifiedNormalFormProgram, name: &str) -> Option<usize> {
        program
            .strata()
            .iter()
            .position(|s| s.rules.contains_key(&sym(name)))
    }

    fn assert_unstratifiable(err: &miette::Report, context: &str) {
        assert!(
            err.downcast_ref::<UnStratifiableProgram>().is_some(),
            "{context}: expected the unstratifiable refusal, got: {err:?}"
        );
    }

    /// The port of the original `test_dependencies` (which ran a script on
    /// a live `DbInstance` and asserted nothing): the same dependency
    /// shape, with the stratification actually asserted.
    ///
    ///     x       — fixed (the original's constant rule)
    ///     w := ∅ ; w := w            (plain self-recursion: legal)
    ///     y[count] := x ; y[count] := w
    ///     z[count] := y ; z[count] := y
    ///     ? := z ; ? := w
    #[test]
    fn dependencies_stratify_in_execution_order() {
        let (program, lifetimes) = Prog::default()
            .fixed("x", &[])
            .rule("w", &[None], vec![])
            .rule("w", &[None], vec![dep("w")])
            .rule("y", &[Some("count")], vec![dep("x")])
            .rule("y", &[Some("count")], vec![dep("w")])
            .rule("z", &[Some("count")], vec![dep("y")])
            .rule("z", &[Some("count")], vec![dep("y")])
            .rule("?", &[None], vec![dep("z")])
            .rule("?", &[None], vec![dep("w")])
            .stratify()
            .expect("the program is stratifiable");

        let w = stratum_of(&program, "w").expect("w is reachable");
        let x = stratum_of(&program, "x").expect("x is reachable");
        let y = stratum_of(&program, "y").expect("y is reachable");
        let z = stratum_of(&program, "z").expect("z is reachable");
        let entry = stratum_of(&program, "?").expect("the entry is always placed");

        // Aggregations force strict boundaries below their dependencies…
        assert!(x < y, "y counts over the fixed rule x");
        assert!(w < y, "y counts over w");
        assert!(y < z, "z counts over y");
        // …while plain reads may share a stratum with their dependent.
        assert!(z <= entry);
        assert!(w < entry);
        // The constructor proved it, but it is the point: entry runs last.
        assert_eq!(entry, program.strata().len() - 1);

        // Lifetimes are in execution-order stratum indices: a store lives
        // to the last stratum that reads it.
        let muggle = |n: &str| MagicSymbol::Muggle { inner: sym(n) };
        assert!(lifetimes.is_live_at(&muggle("y"), z), "z reads y");
        assert!(lifetimes.is_live_at(&muggle("z"), entry), "? reads z");
        assert!(lifetimes.is_live_at(&muggle("w"), entry), "? reads w");
        assert!(
            !lifetimes.is_live_at(&muggle("x"), z),
            "nothing after y reads x"
        );
        assert!(
            !lifetimes.is_live_at(&muggle("?"), 0),
            "nothing reads the entry"
        );
    }

    /// Law 2's refusal corpus, run through the *real* stratifier: every
    /// program the reference checker (`query/laws.rs`) refuses as
    /// unstratifiable, this module must refuse too — the two must never
    /// drift. The corpus covers direct/mutual/cycle-mediated negation,
    /// recursive normal aggregation, mixed meet+normal aggregation,
    /// meet self-negation, and a fixed rule inside recursion.
    #[test]
    fn the_oracle_refusal_corpus_is_refused() {
        for (name, oracle_program) in laws::unstratifiable_corpus() {
            let mut prog = Prog::default();
            // An entry reading every head, so nothing is pruned as
            // unreachable before the check.
            let mut heads: BTreeSet<&str> = BTreeSet::new();
            for rule in &oracle_program.rules {
                heads.insert(rule.head_rel);
            }
            for fixed in &oracle_program.fixed {
                heads.insert(fixed.head_rel);
            }
            prog = prog.rule("?", &[None], heads.iter().map(|h| dep(h)).collect());
            for rule in &oracle_program.rules {
                prog = prog.rule_raw(
                    rule.head_rel,
                    rule.aggr.clone(),
                    rule.body
                        .iter()
                        .map(|l| {
                            if l.negated {
                                neg_dep(l.rel)
                            } else {
                                dep(l.rel)
                            }
                        })
                        .collect(),
                );
            }
            for fixed in &oracle_program.fixed {
                prog = prog.fixed(fixed.head_rel, &fixed.inputs);
            }
            let err = prog.stratify().expect_err(&format!("must refuse: {name}"));
            assert_unstratifiable(&err, name);
        }
    }

    /// Recursion through a *normal* aggregation is refused: the fold is
    /// only correct over the finished set, and the set is never finished
    /// inside its own recursion. (Accepting this answers wrongly, it does
    /// not crash — the refusal is the feature.)
    #[test]
    fn recursive_normal_aggregation_is_refused() {
        let err = Prog::default()
            .rule("p", &[Some("count")], vec![dep("d")])
            .rule("p", &[Some("count")], vec![dep("p")])
            .rule("d", &[None], vec![])
            .rule("?", &[None], vec![dep("p")])
            .stratify()
            .expect_err("count through recursion must be refused");
        assert_unstratifiable(&err, "recursive count");
    }

    /// The one legal aggregation inside recursion: every rule of the head
    /// aggregates with meet forms, and the recursion reads the head itself,
    /// positively.
    #[test]
    fn all_meet_self_recursion_is_accepted() {
        let (program, _) = Prog::default()
            .rule("m", &[None, Some("min")], vec![dep("seed")])
            .rule("m", &[None, Some("min")], vec![dep("m")])
            .rule("seed", &[None], vec![])
            .rule("?", &[None], vec![dep("m")])
            .stratify()
            .expect("all-meet self-recursion is stratifiable");
        assert!(stratum_of(&program, "m").is_some());
    }

    /// Mixing a meet with a normal aggregation on a recursive head is
    /// refused: one non-meet position poisons the whole head's recursion.
    #[test]
    fn mixed_meet_and_normal_recursion_is_refused() {
        let err = Prog::default()
            .rule("q", &[None, Some("min"), Some("count")], vec![dep("q")])
            .rule("?", &[None], vec![dep("q")])
            .stratify()
            .expect_err("min+count through recursion must be refused");
        assert_unstratifiable(&err, "mixed meet+normal");
    }

    /// A meet head negating itself is refused: the exemption is for
    /// positive self-reads only.
    #[test]
    fn meet_negating_itself_is_refused() {
        let err = Prog::default()
            .rule("m", &[Some("min")], vec![dep("d"), neg_dep("m")])
            .rule("d", &[None], vec![])
            .rule("?", &[None], vec![dep("m")])
            .stratify()
            .expect_err("meet self-negation must be refused");
        assert_unstratifiable(&err, "meet self-negation");
    }

    /// The meet exemption is for a head reading *itself*: meet recursion
    /// routed through an intermediary rule is refused, exactly as the
    /// original decided it. (Deliberately preserved upstream behavior.)
    #[test]
    fn meet_recursion_through_an_intermediary_is_refused() {
        let err = Prog::default()
            .rule("p", &[Some("min")], vec![dep("q")])
            .rule("q", &[None], vec![dep("p")])
            .rule("?", &[None], vec![dep("p")])
            .stratify()
            .expect_err("indirect meet recursion must be refused");
        assert_unstratifiable(&err, "meet through intermediary");
    }

    /// Fixed rules are always stratum-bounded: inputs complete strictly
    /// before, readers start strictly after — but out of recursion they
    /// stratify fine.
    #[test]
    fn fixed_rules_are_stratum_bounded() {
        let (program, _) = Prog::default()
            .rule("base", &[None], vec![])
            .fixed("f", &["base"])
            .rule("p", &[None], vec![dep("f")])
            .rule("?", &[None], vec![dep("p")])
            .stratify()
            .expect("a fixed rule outside recursion stratifies");
        let base = stratum_of(&program, "base").expect("base placed");
        let f = stratum_of(&program, "f").expect("f placed");
        let p = stratum_of(&program, "p").expect("p placed");
        assert!(base < f, "inputs complete strictly before the fixed rule");
        assert!(f < p, "readers start strictly after the fixed rule");

        let err = Prog::default()
            .rule("r", &[None], vec![dep("f")])
            .fixed("f", &["r"])
            .rule("?", &[None], vec![dep("r")])
            .stratify()
            .expect_err("a fixed rule inside recursion must be refused");
        assert_unstratifiable(&err, "fixed rule in recursion");
    }

    /// Plain negation of a completed dependency is stratified negation —
    /// accepted, with the dependency strictly below.
    #[test]
    fn stratified_negation_is_accepted_with_a_boundary() {
        let (program, _) = Prog::default()
            .rule("r", &[None], vec![dep("s")])
            .rule("s", &[None], vec![])
            .rule("p", &[None], vec![dep("q"), neg_dep("r")])
            .rule("q", &[None], vec![])
            .rule("?", &[None], vec![dep("p")])
            .stratify()
            .expect("negation of a non-recursive dependency is fine");
        let r = stratum_of(&program, "r").expect("r placed");
        let p = stratum_of(&program, "p").expect("p placed");
        assert!(r < p, "the negated relation completes strictly below");
    }

    /// Rules unreachable from the entry are pruned before the check — even
    /// unstratifiable ones. Deliberately preserved upstream behavior: the
    /// soundness proof is about what will be *evaluated*.
    #[test]
    fn unreachable_rules_are_pruned_not_checked() {
        let (program, _) = Prog::default()
            .rule("a", &[None], vec![])
            .rule("orphan", &[None], vec![neg_dep("orphan")])
            .rule("?", &[None], vec![dep("a")])
            .stratify()
            .expect("the unreachable orphan is pruned, not refused");
        assert_eq!(stratum_of(&program, "orphan"), None, "orphan is pruned");
        assert!(stratum_of(&program, "a").is_some());
    }

    /// The refusal carries the source span of the body atom that closes
    /// the forbidden cycle, so the diagnostic can point at it.
    #[test]
    fn the_refusal_points_at_the_offending_dependency() {
        let offending = SourceSpan(42, 7);
        let err = Prog::default()
            .rule(
                "p",
                &[None],
                vec![
                    dep("d"),
                    InputAtom::Negation {
                        inner: Box::new(InputAtom::Rule {
                            inner: InputRuleApplyAtom {
                                name: Symbol::new("p", offending),
                                args: vec![],
                                span: offending,
                            },
                        }),
                        span: offending,
                    },
                ],
            )
            .rule("d", &[None], vec![])
            .rule("?", &[None], vec![dep("p")])
            .stratify()
            .expect_err("self-negation must be refused");
        let refusal = err
            .downcast_ref::<UnStratifiableProgram>()
            .expect("the unstratifiable refusal");
        assert_eq!(refusal.span, offending);
    }

    /// The refusal labels the atom that ESTABLISHES the poison, not the
    /// first occurrence of the dependency. In
    ///
    /// ```text
    /// p := p, d
    /// p := d, not p
    /// ```
    ///
    /// the dependency map keys `p` by its first occurrence — the legal
    /// positive self-read — and the `not p` in the second rule upgrades
    /// that edge to poisoned. The diagnostic must point at `not p` (the
    /// read that actually forbids stratification), not at the innocent
    /// positive read whose symbol happens to hold the map key.
    #[test]
    fn the_refusal_points_at_the_poisoning_atom_not_the_first_read() {
        let positive = SourceSpan(10, 1);
        let poisoning = SourceSpan(42, 7);
        let dep_at = |name: &str, span: SourceSpan| InputAtom::Rule {
            inner: InputRuleApplyAtom {
                name: Symbol::new(name, span),
                args: vec![],
                span,
            },
        };
        let err = Prog::default()
            .rule("p", &[None], vec![dep_at("p", positive), dep("d")])
            .rule(
                "p",
                &[None],
                vec![
                    dep("d"),
                    InputAtom::Negation {
                        inner: Box::new(dep_at("p", poisoning)),
                        span: poisoning,
                    },
                ],
            )
            .rule("d", &[None], vec![])
            .rule("?", &[None], vec![dep("p")])
            .stratify()
            .expect_err("negated self-read must be refused");
        let refusal = err
            .downcast_ref::<UnStratifiableProgram>()
            .expect("the unstratifiable refusal");
        assert_eq!(
            refusal.span, poisoning,
            "the label must land on `not p`, not the positive self-read"
        );
    }

    /// Law 5 at the stratifier: a ten-thousand-rule dependency chain
    /// stratifies on a deliberately small thread stack. The original's
    /// recursive Tarjan/reachability overflowed here.
    #[test]
    fn a_deep_rule_chain_stratifies_without_stack_overflow() {
        let handle = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                const N: usize = 10_000;
                let mut prog = Prog::default().rule("?", &[None], vec![dep("r0")]);
                for i in 0..N - 1 {
                    prog = prog.rule(&format!("r{i}"), &[None], vec![dep(&format!("r{}", i + 1))]);
                }
                prog = prog.rule(&format!("r{}", N - 1), &[None], vec![]);
                let (program, _) = prog.stratify().expect("a plain chain stratifies");
                // No poisoned edges anywhere: one stratum holds the chain.
                assert_eq!(program.strata().len(), 1);
                assert_eq!(program.strata()[0].rules.len(), N + 1);
            })
            .expect("spawn test thread");
        handle.join().expect("no stack overflow");
    }

    /// The entry field carries its real span into the pipeline (the
    /// original rebuilt `?` with a dummy span to look it up).
    #[test]
    fn a_minimal_program_stratifies_to_one_stratum() {
        let (program, lifetimes) = Prog::default()
            .rule("?", &[None], vec![])
            .stratify()
            .expect("the minimal program stratifies");
        assert_eq!(program.strata().len(), 1);
        assert!(
            program.strata()[0]
                .rules
                .keys()
                .any(|k| k.kind() == SymbolKind::Entry)
        );
        assert!(!lifetimes.is_live_at(&MagicSymbol::Muggle { inner: sym("?") }, 0));
    }
}
