/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0). The transformations, each per the ratified designs (story #3):
 *
 * - **Free functions over the kernel's read species, not `SessionTx`
 *   methods.** The original hung compilation off `impl SessionTx`; here
 *   [`stratified_magic_compile`] and [`compile_magic_rule_body`] take
 *   `&impl ReadTx` and resolve stored relations through the landed catalog
 *   (`session/catalog.rs::get_relation`). `session/db.rs` threads the
 *   real transaction when it lands; a `WriteTx` is a `ReadTx`, so either
 *   species works (SEAM, db tier). Temp (`_`-prefixed) relations resolve
 *   through the session's temp store — `get_relation` refuses them typed
 *   until that router lands (SEAM, session tier).
 * - **Strata arrive in execution order.** The landed
 *   `StratifiedMagicProgram` is execution-ordered by construction; the
 *   original's `.rev()` (over conventionally-reversed strata) has no
 *   descendant here.
 * - **The ruleset invariants are constructor proofs.** The original's
 *   `CompiledRuleSet::arity`/`aggr_kind` indexed `rules[0]` and trusted
 *   "the ruleset agrees on arity/aggr" as an unspoken convention.
 *   [`CompiledInlineRules::new`] refuses an empty set and enforces
 *   **arg-level** head-aggregation signature equality across the set's
 *   rules (the mirror of the parser's `parser::head_aggr_mismatch` check —
 *   the magic tier sits between the two, so the proof is re-established
 *   where the signatures collapse into one). The signature then travels
 *   once, on the set, which is exactly the shape
 *   `query/eval.rs::EvalRuleSet::new` consumes.
 * - **`AggrKind` is not re-declared here.** The original's
 *   `compile::AggrKind` (None/Normal/Meet) classified a whole head; that
 *   classification lives once, in `query/eval.rs::HeadAggrKind`, minted by
 *   `EvalRuleSet::new`. One concept, one name.
 * - **`contained_rules` is re-homed here** (the compile tier owns the
 *   dependency map): upstream declared it on `MagicInlineRule` in
 *   `data/program.rs`; the landed data tier deliberately omitted it.
 *   Upstream's original numbered a body's dependencies by STORE NAME,
 *   collapsing repeated occurrences of the same store into a `Many` that
 *   forced a complete naive re-join every epoch — correct, but with no
 *   delta narrowing at all for a body mentioning one store twice (the
 *   self-join shape: `tc(x,z), tc(z,y)`; Andersen's `load`/`store` rules,
 *   each mentioning `pt` twice). That shape's memory blowup is issue #68's
 *   dominant driver, confirmed both structurally (this collapse) and by
 *   measurement (`crates/kyzo-core/examples/fixpoint_mem_profile.rs`: 18-43×
 *   more allocations per output row than an equivalent single-occurrence
 *   rule, growing super-linearly with scale where the single-occurrence
 *   case stays flat). The fix here numbers by OCCURRENCE — this body's
 *   position in `MagicInlineRule::body`, one id per `Rule`/`NegatedRule`
 *   atom — so two occurrences of the same store get two independent
 *   [`AtomOccurrence`]s and each is delta-selectable on its own: the
 *   standard semi-naive self-join rewrite, `Δ(P⋈P) = (ΔP⋈P) ∪ (P⋈ΔP)`, one
 *   pass per occurrence. [`AtomOccurrence`] stays declared in
 *   `query/eval.rs` (its one consumer, the delta discipline) — ONE
 *   definition; the seam trait `RuleBody::contained_rules` names it in its
 *   signature.
 * - **Compiled rules implement the evaluator's seam.**
 *   [`CompiledRuleBody`] binds a compiled plan to a transaction and
 *   implements `query/eval.rs::RuleBody`: `for_each_derivation` walks the
 *   plan's `TupleIter` with `delta_from` threaded to the rule-store scans
 *   (the ONE occurrence it names deltas; every other occurrence, including
 *   another of the same store, reads its total; negation always reads
 *   totals — the operators enforce it, see `query/ra.rs`). [`bind_for_eval`] assembles the
 *   `EvalProgram` stratum by stratum, with the fixed-rule evaluator
 *   injected by the caller (SEAM, fixed-rule wiring in db.rs; tests use
 *   the uninhabited [`NoFixedRules`]).
 *
 * Upstream panic-site audit (Law 5) for this file:
 *   1. compile.rs:46,55  `rs[0]`/`rules[0]` indexing (arity, aggr_kind) —
 *      structurally removed by [`CompiledInlineRules::new`]'s non-empty
 *      proof; the signature is a field, not an index expression.
 *   2. compile.rs:655  `ret_vars_set.difference(..).next()` under
 *      an `ensure!` — restructured: the unbound symbol is matched, and the
 *      impossible empty-difference case is a typed
 *      [`PlanInvariantError`](crate::exec::op::PlanInvariantError).
 *   3. compile.rs search-arm seen_variables containment checks — gone
 *      with the search seams (the index tier re-establishes them as
 *      typed checks when those atoms land).
 *
 * Other deviations from the original, documented:
 *   D1. Index selection speaks the landed catalog shape: `choose_index`
 *       returns an `(IndexRef, requires_back_join)` pair and the index
 *       relation's handle is resolved by name through the catalog
 *       (`IndexRef::relation_name` + `get_relation`) — the original
 *       embedded full index-handle copies in the parent handle. A chosen
 *       ref that is not a `Plain` projection is a typed invariant error
 *       (choose_index never picks manifest-backed kinds).
 *   D2. The original's `right_joiner_vars_pos` vectors (pushed, never
 *       read) are dropped.
 *   D3. `budget`/interrupts: rule iteration is interrupt-checked by eval's
 *       per-derivation ticker through the seam callback, so the operators
 *       take no `Budget`. When db.rs wires *fixed rules*, eval's
 *       `Budget::check_interrupt`/`ticker` must go `pub(crate)` (per the
 *       reconciliation notes) — never solved by re-adding `Poison`.
 *   D4. RETIRED. Premises are collected when `want_premises` is true: each
 *       positive body literal's grounding row rides a side channel on
 *       [`Batch`], appended at capturing joins / search; negation contributes
 *       none. `CompiledRuleBody::premise_sources` attributes those rows.
 */

//! The plan compiler: from a proven program to an executable plan.
//!
//! **Essence**: compilation turns a *proven program* (the
//! [`StratifiedMagicProgram`] tier — stratified, demand-rewritten, entry
//! proven present) into an *executable plan* — for each rule, a left-deep
//! tree of relational-algebra operators (`query/ra.rs`) whose iteration IS
//! the rule's evaluation. [`compile_magic_rule_body`] walks the rule's
//! body atoms in order, growing the tree left-to-right: each atom either
//! joins a new row source onto everything bound so far (rule stores,
//! stored relations, index scans), filters (predicates, negation), or
//! appends a computed column (unification). Variables seen earlier join by
//! name; fresh right-side symbols are generated for repeats so the joiner
//! is always positional underneath. After the walk, dead columns are
//! eliminated, the head's symbols are proven bound (an unbound head symbol
//! is a typed refusal), and the columns are reordered to the head — so the
//! plan's output frame equals the rule head, position for position.
//!
//! Index selection: a stored-relation atom consults
//! `RelationHandle::choose_index` with each argument position's use
//! ([`IndexPositionUse`]). A covering index replaces the base scan; a
//! non-covering one is joined index→base by the base's key prefix, with
//! residual equality filters for join columns the index could not bind.
//!
//! The output tier ([`CompiledProgram`], per stratum) is consumed by
//! [`bind_for_eval`], which binds a transaction and yields the
//! `EvalProgram` that `query/eval.rs::stratified_evaluate` runs.

use crate::project::current::Segments;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;

use itertools::Itertools;
use miette::{Context, Diagnostic, Result, bail, ensure};
use thiserror::Error;

use crate::exec::fixpoint::delta_store::EpochStore;
use crate::exec::fixpoint::delta_store::RegularTempStore;
use crate::exec::fixpoint::eval::{
    AtomOccurrence, EvalDefinition, EvalProgram, EvalRuleSet, EvalStratum, FixedRuleEval,
    PremiseSource, Premises, RuleBody,
};
use crate::exec::op::{PlanInvariantError, RelAlgebra, SearchRA};
use crate::exec::plan::program::{
    MagicAtom, MagicFixedRuleApply, MagicInlineRule, MagicRulesOrFixed, MagicSymbol,
    StratifiedMagicProgram,
};
use crate::session::access::{AccessLevel, InsufficientAccessLevel};
use crate::session::catalog::{IndexKind, IndexRef, RelationHandle, get_relation};
use crate::store::ReadTx;
use kyzo_model::SourceSpan;
use kyzo_model::program::expr::{BindingPos, Expr};
use kyzo_model::program::rule::{DeltaAxis, HeadAggrSlot, ValidityClause};
use kyzo_model::program::symbol::{Symbol, SymbolKind};
use kyzo_model::value::DataValue;

/// How the compile tier uses each argument position of a stored-relation
/// atom, for index selection. Owns [`RelationHandle::choose_index`] so the
/// catalog seat does not import this type (avoids a session↔exec cycle).
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum IndexPositionUse {
    /// The position is bound and can seed an index prefix scan.
    Join,
    /// The position is needed later but not bound for the scan.
    BindForLater,
    /// The position is not used at all.
    Ignored,
}

impl RelationHandle {
    /// Choose the plain index whose column mapper matches the longest
    /// prefix of bound (`Join`) argument positions. Returns the chosen
    /// index reference and whether a back-join to the base relation is
    /// still required (some needed position is not covered by the index).
    ///
    /// By reference: the caller resolves the index relation's handle via
    /// [`IndexRef::relation_name`] and [`get_relation`] — this handle does
    /// not embed a copy of it. Manifest-backed indices (HNSW/FTS/LSH) are
    /// never chosen here; their own operators own their access paths.
    pub fn choose_index(
        &self,
        arg_uses: &[IndexPositionUse],
        validity_query: bool,
    ) -> Option<(IndexRef, bool)> {
        // Law 5: the original `unwrap`ped `first()`; a zero-arity atom
        // simply has no index to choose.
        let first = arg_uses.first()?;
        if *first == IndexPositionUse::Join {
            // The base relation's own key prefix is already usable.
            return None;
        }
        let required_positions: Vec<usize> = arg_uses
            .iter()
            .enumerate()
            .filter_map(|(i, pos_use)| (*pos_use != IndexPositionUse::Ignored).then_some(i))
            .collect();
        let mut max_prefix_len = 0usize;
        let mut chosen = None;
        for index in &self.indices {
            let IndexKind::Plain { mapper } = &index.kind else {
                continue;
            };
            // As-of queries use plain indexes freely: every plain index
            // row carries the base row's bitemporal coordinate and
            // polarity (the mutation tier mirrors them), so an index scan
            // resolves at any coordinate exactly like the base.
            drop(validity_query);
            let mut cur_prefix_len = 0usize;
            for i in mapper {
                // A mapper position beyond the argument list would mean a
                // stale catalog row; it ends the usable prefix rather than
                // panicking (law 5: the original indexed unchecked).
                match arg_uses.get(*i) {
                    Some(IndexPositionUse::Join) => cur_prefix_len += 1,
                    Some(IndexPositionUse::BindForLater)
                    | Some(IndexPositionUse::Ignored)
                    | None => break,
                }
            }
            if cur_prefix_len > max_prefix_len {
                max_prefix_len = cur_prefix_len;
                let requires_back_join =
                    required_positions.iter().any(|need| !mapper.contains(need));
                chosen = Some((index.clone(), requires_back_join));
            }
        }
        chosen
    }
}

/// One head position's aggregation slot — the same shape carried by
/// every program tier (`MagicInlineRule::aggr`) and by eval's rule sets.
type HeadAggr = HeadAggrSlot;

/// One compiled stratum: each rule store's definition, ready to bind to a
/// transaction and evaluate. A whole program is `Vec<CompiledProgram>` in
/// execution order.
pub type CompiledProgram = BTreeMap<MagicSymbol, CompiledRuleSet>;

/// A compiled definition: an inline rule set (plans), or a fixed-rule
/// application handed through to the fixed-rule tier unchanged.
#[derive(Debug)]
pub enum CompiledRuleSet {
    Rules(CompiledInlineRules),
    Fixed(MagicFixedRuleApply),
}

impl CompiledRuleSet {
    /// Output arity. Total by construction: an inline set proves itself
    /// non-empty and signature-uniform at [`CompiledInlineRules::new`]
    /// (the original indexed `rules[0]` here).
    pub(crate) fn arity(&self) -> usize {
        match self {
            CompiledRuleSet::Rules(rules) => rules.aggr.len(),
            CompiledRuleSet::Fixed(fixed) => fixed.arity,
        }
    }
}

/// The compiled rules of one head. Construction proves the set is
/// non-empty and that every rule aggregates identically — per position,
/// **arguments included** — so the signature travels once, on the set.
#[derive(Debug)]
pub struct CompiledInlineRules {
    /// The head's per-position aggregation signature, uniform across the
    /// set's rules.
    pub(crate) aggr: Vec<HeadAggr>,
    /// The rules' plans, in source order. Non-empty.
    pub rules: Vec<CompiledRule>,
}

/// A rule set that disagrees with itself about its head aggregations.
/// The parser refuses this per definition site
/// (`parser::head_aggr_mismatch`); this is the same law re-proven where
/// the per-rule signatures collapse into the per-set one, so no tier
/// between parse and eval can smuggle a disagreement through.
#[derive(Debug, Error, Diagnostic)]
#[error("Rule '{0}' has definitions with conflicting head aggregations")]
#[diagnostic(code(compile::head_aggr_mismatch))]
#[diagnostic(help(
    "Every definition of a rule must apply the same aggregation (with the \
     same arguments) to each head position."
))]
pub struct RulesetHeadAggrMismatch(MagicSymbol, #[label] SourceSpan);

impl CompiledInlineRules {
    /// Mint from `(signature, plan)` pairs, refusing an empty set and any
    /// signature disagreement.
    pub(crate) fn new(
        name: &MagicSymbol,
        sets: Vec<(Vec<HeadAggr>, CompiledRule)>,
    ) -> Result<Self> {
        let mut iter = sets.into_iter();
        let Some((aggr, first)) = iter.next() else {
            bail!(PlanInvariantError(
                "a compiled rule set must contain at least one rule"
            ));
        };
        let mut rules = vec![first];
        for (other_aggr, rule) in iter {
            ensure!(
                other_aggr == aggr,
                RulesetHeadAggrMismatch(name.clone(), name.as_plain_symbol().span)
            );
            rules.push(rule);
        }
        Ok(Self { aggr, rules })
    }
}

/// One compiled rule: the executable plan of one body, plus the rule
/// stores it reads (the delta discipline's map). The head-aggregation
/// signature lives on the set ([`CompiledInlineRules::aggr`]), not here —
/// it is a per-head fact, proven uniform.
#[derive(Debug)]
pub struct CompiledRule {
    pub relation: RelAlgebra,
    pub contained_rules: BTreeMap<AtomOccurrence, MagicSymbol>,
    /// Source of each **positive** body literal, in body order — aligned
    /// with the premise rows [`CompiledRuleBody`] passes when
    /// `want_premises` is true.
    pub(crate) premise_sources: Vec<PremiseSource>,
}

/// Positive body literals' provenance sources, in body order. Negation,
/// predicates, and unifications contribute none.
pub(crate) fn positive_premise_sources(body: &[MagicAtom]) -> Vec<PremiseSource> {
    body.iter()
        .filter_map(|atom| match atom {
            MagicAtom::Rule(r) => Some(PremiseSource::Rule(r.name.clone())),
            MagicAtom::Relation(r) => Some(PremiseSource::Fact(r.name.clone())),
            MagicAtom::Search(s) => Some(PremiseSource::Fact(Symbol::new(
                s.cfg.base().name.clone(),
                s.span,
            ))),
            MagicAtom::Predicate(_)
            | MagicAtom::NegatedRule(_)
            | MagicAtom::NegatedRelation(_)
            | MagicAtom::Unification(_) => None,
        })
        .collect()
}

/// The atom-occurrence numbering shared by [`MagicInlineRule::contained_rules`]
/// and every `TempStoreRA`-constructing call site in
/// `compile_magic_rule_body` — both walk `body: &[MagicAtom]` left to right
/// and must agree on which occurrence id names which atom, so the numbering
/// itself lives in exactly one place. One id per `Rule`/`NegatedRule` atom,
/// in body order; every other atom kind is not delta-selectable and consumes
/// no id.
pub(crate) fn atom_occurrences(
    body: &[MagicAtom],
) -> impl Iterator<Item = (AtomOccurrence, &MagicAtom)> {
    body.iter()
        .filter(|atom| matches!(atom, MagicAtom::Rule(_) | MagicAtom::NegatedRule(_)))
        .enumerate()
        .map(|(i, atom)| (AtomOccurrence(i), atom))
}

impl MagicInlineRule {
    /// Every in-memory rule store this body reads (positively OR
    /// negatively), keyed by occurrence — re-homed from the original's
    /// `data/program.rs` (the compile tier owns the dependency map; the
    /// data tier deliberately dropped it). Both `Rule` and `NegatedRule`
    /// atoms are entered: this map is also `StoreLifetimes`' dependency
    /// source (`note_use`, in `eval.rs`), and a store read only inside a
    /// negation is used just as much as one read positively — dropping it
    /// here would let its lifetime end before a later stratum's negation
    /// reads it (`eval::invariant`: "a referenced rule has no store").
    ///
    /// A store mentioned twice gets two entries (occurrence-keyed, not
    /// name-keyed): the self-join shape (`tc(x,z), tc(z,y)`; Andersen's
    /// `load`/`store` rules, each mentioning `pt` twice) is exactly the
    /// case the predecessor name-keyed scheme collapsed into a `Many` that
    /// forced a complete naive re-join every epoch, no delta narrowing at
    /// all — issue #68's dominant memory-blowup driver. Numbering by
    /// occurrence lets each be delta-selected independently.
    ///
    /// A negated occurrence's entry is never actually selected for delta
    /// narrowing in practice — negation always reads totals
    /// (`NegJoin::iter_batched` never consults `delta_from` for its own
    /// right side), and stratification guarantees a negated dependency is
    /// complete strictly below, so its delta is empty by the time this
    /// body runs and `eval.rs`'s dispatch loop skips it (`!has_delta()`).
    /// If it ever DID fire regardless, the result would still be sound —
    /// no real `TempStoreRA` reads this occurrence's delta, so every
    /// occurrence in the tree would read its total, the harmless
    /// (wasteful, never-taken) equivalent of the predecessor's `Many`
    /// fallback.
    pub fn contained_rules(&self) -> BTreeMap<AtomOccurrence, MagicSymbol> {
        let mut coll = BTreeMap::new();
        for (occurrence, atom) in atom_occurrences(&self.body) {
            match atom {
                MagicAtom::Rule(rule) | MagicAtom::NegatedRule(rule) => {
                    coll.insert(occurrence, rule.name.clone());
                }
                MagicAtom::Relation(_)
                | MagicAtom::Predicate(_)
                | MagicAtom::NegatedRelation(_)
                | MagicAtom::Unification(_)
                | MagicAtom::Search(_) => {}
            }
        }
        coll
    }
}

#[derive(Debug, Error, Diagnostic)]
#[error("Requested rule {0} not found")]
#[diagnostic(code(eval::rule_not_found))]
struct RuleNotFound(MagicSymbol, #[label] SourceSpan);

#[derive(Debug, Error, Diagnostic)]
#[error("Arity mismatch for rule application {0}")]
#[diagnostic(code(eval::rule_arity_mismatch))]
#[diagnostic(help("Required arity: {1}, number of arguments given: {2}"))]
struct ArityMismatch(Symbol, usize, usize, #[label] SourceSpan);

#[derive(Debug, Error, Diagnostic)]
#[error("Symbol '{0}' in rule head is unbound")]
#[diagnostic(code(eval::unbound_symb_in_head))]
#[diagnostic(help(
    "Note that symbols occurring only in negated positions are not considered bound"
))]
struct UnboundSymbolInRuleHead(Symbol, #[label] SourceSpan);

/// Compile every stratum of a proven program into executable plans.
///
/// The input tier is execution-ordered by construction; the output `Vec`
/// keeps that order (`compiled[0]` evaluates first).
pub fn stratified_magic_compile(
    tx: &impl ReadTx,
    prog: StratifiedMagicProgram,
) -> Result<Vec<CompiledProgram>> {
    // Every store's arity, across ALL strata: a rule body may reference
    // stores of earlier strata.
    let mut store_arities: BTreeMap<MagicSymbol, usize> = Default::default();
    for stratum in prog.strata() {
        for (name, ruleset) in &stratum.prog {
            store_arities.insert(name.clone(), ruleset.arity()?);
        }
    }

    prog.into_strata()
        .into_iter()
        .map(|cur_prog| -> Result<CompiledProgram> {
            cur_prog
                .prog
                .into_iter()
                .map(|(k, body)| -> Result<(MagicSymbol, CompiledRuleSet)> {
                    match body {
                        MagicRulesOrFixed::Rules { rules: body } => {
                            let mut collected = Vec::with_capacity(body.len());
                            for rule in body.iter() {
                                let header = &rule.head;
                                let mut relation =
                                    compile_magic_rule_body(tx, rule, &k, &store_arities, header)?;
                                relation.fill_binding_indices_and_compile().with_context(
                                    || {
                                        format!(
                                            "error encountered when filling binding indices for {relation:#?}"
                                        )
                                    },
                                )?;
                                collected.push((
                                    rule.aggr.clone(),
                                    CompiledRule {
                                        relation,
                                        contained_rules: rule.contained_rules(),
                                        premise_sources: positive_premise_sources(&rule.body),
                                    },
                                ));
                            }
                            Ok((
                                k.clone(),
                                CompiledRuleSet::Rules(CompiledInlineRules::new(&k, collected)?),
                            ))
                        }
                        MagicRulesOrFixed::Fixed { fixed } => {
                            Ok((k, CompiledRuleSet::Fixed(fixed)))
                        }
                    }
                })
                .try_collect()
        })
        .try_collect()
}

/// Resolve the `IndexKind::Temporal` posting index for a `Valid`-axis
/// `@delta` clause, if the base relation has one attached — story #62
/// chunk 4's read-side seam. `None` for every other clause shape
/// (`@delta_sys`, `@spans`, a plain read, or a `Valid`-axis `@delta` whose
/// base relation has no posting index yet): `DeltaRA::iter_batched` falls
/// back to the naive full-snapshot diff in every one of those cases, so
/// returning `None` here is never a correctness gap, only a missed
/// acceleration.
fn resolve_delta_posting_index(
    tx: &impl ReadTx,
    store: &RelationHandle,
    validity: &Option<ValidityClause>,
) -> Result<Option<RelationHandle>> {
    let Some(ValidityClause::Delta {
        axis: DeltaAxis::Valid,
        ..
    }) = validity
    else {
        return Ok(None);
    };
    store
        .indices
        .iter()
        .find(|idx_ref| matches!(idx_ref.kind, IndexKind::Temporal))
        .map(|idx_ref| get_relation(tx, &idx_ref.relation_name(&store.name)))
        .transpose()
}

/// Compile one rule body into its operator tree. `ret_vars` is the rule's
/// head: the plan's output frame is proven equal to it (unbound head
/// symbols refused, columns reordered to match).
pub(crate) fn compile_magic_rule_body(
    tx: &impl ReadTx,
    rule: &MagicInlineRule,
    rule_name: &MagicSymbol,
    store_arities: &BTreeMap<MagicSymbol, usize>,
    ret_vars: &[Symbol],
) -> Result<RelAlgebra> {
    let mut ret = RelAlgebra::unit(rule_name.as_plain_symbol().span);
    let mut seen_variables = BTreeSet::new();
    let mut serial_id = 0;
    let mut gen_symb = |span| {
        let ret = Symbol::new(format!("**{serial_id}"), span);
        serial_id += 1;
        ret
    };
    // One id per `Rule`/`NegatedRule` atom, in body order — the exact
    // numbering `MagicInlineRule::contained_rules` (via `atom_occurrences`)
    // assigns over this same `rule.body`, so a `TempStoreRA` built here and
    // the occurrence key `eval.rs`'s delta discipline selects it by always
    // agree.
    let mut occurrence_counter = 0usize;
    let mut next_occurrence = move || {
        let occ = AtomOccurrence(occurrence_counter);
        occurrence_counter += 1;
        occ
    };
    for atom in &rule.body {
        match atom {
            MagicAtom::Rule(rule_app) => {
                let occurrence = next_occurrence();
                let store_arity = store_arities.get(&rule_app.name).ok_or_else(|| {
                    RuleNotFound(rule_app.name.clone(), rule_app.name.as_plain_symbol().span)
                })?;

                ensure!(
                    *store_arity == rule_app.args.len(),
                    ArityMismatch(
                        rule_app.name.as_plain_symbol().clone(),
                        *store_arity,
                        rule_app.args.len(),
                        rule_app.span
                    )
                );
                let mut prev_joiner_vars = vec![];
                let mut right_joiner_vars = vec![];
                let mut right_vars = vec![];

                for var in &rule_app.args {
                    if seen_variables.contains(var) {
                        prev_joiner_vars.push(var.clone());
                        let rk = gen_symb(var.span);
                        right_vars.push(rk.clone());
                        right_joiner_vars.push(rk);
                    } else {
                        seen_variables.insert(var.clone());
                        right_vars.push(var.clone());
                    }
                }

                let right = RelAlgebra::derived(
                    right_vars,
                    rule_app.name.clone(),
                    occurrence,
                    rule_app.span,
                );
                ensure!(prev_joiner_vars.len() == right_joiner_vars.len(), "join key arity mismatch between sides");
                ret = ret.join_capturing_premise(
                    right,
                    prev_joiner_vars,
                    right_joiner_vars,
                    rule_app.span,
                )?;
            }
            MagicAtom::Relation(rel_app) => {
                let store = get_relation(tx, &rel_app.name)?;
                if store.access_level < AccessLevel::ReadOnly {
                    bail!(InsufficientAccessLevel(
                        store.name.to_string(),
                        "reading rows".to_string(),
                        store.access_level
                    ));
                }
                ensure!(
                    store.arity() == rel_app.args.len(),
                    ArityMismatch(
                        rel_app.name.clone(),
                        store.arity(),
                        rel_app.args.len(),
                        rel_app.span
                    )
                );
                // already existing vars
                let mut prev_joiner_vars = vec![];
                // vars introduced by right and joined
                let mut right_joiner_vars = vec![];
                // used to find the right joiner var from the tuple position
                let mut right_joiner_vars_pos_rev = vec![None; rel_app.args.len()];
                // vars introduced by right, regardless of joining
                let mut right_vars = vec![];
                // used for choosing indices
                let mut join_indices = vec![];

                for (i, var) in rel_app.args.iter().enumerate() {
                    if seen_variables.contains(var) {
                        prev_joiner_vars.push(var.clone());
                        let rk = gen_symb(var.span);
                        right_vars.push(rk.clone());
                        right_joiner_vars.push(rk);
                        right_joiner_vars_pos_rev[i] = Some(right_joiner_vars.len() - 1);
                        join_indices.push(IndexPositionUse::Join)
                    } else {
                        seen_variables.insert(var.clone());
                        right_vars.push(var.clone());
                        if var.kind() == SymbolKind::GeneratedIgnored {
                            join_indices.push(IndexPositionUse::Ignored)
                        } else {
                            join_indices.push(IndexPositionUse::BindForLater)
                        }
                    }
                }

                // `@spans`/`@delta`/`@delta_sys` always scan the base
                // relation directly: they need its own keyspace (the raw
                // multi-version history, or the plain as-of resolution
                // `skip_scan_all` already gives), never a plain index's
                // mirrored one. A `Valid`-axis `@delta` gets its OWN
                // acceleration below instead (story #62 chunk 4's posting
                // index) — a different mechanism from `choose_index`,
                // since a posting index has no `Plain` mapper shape.
                let chosen_index = match &rel_app.validity {
                    Some(ValidityClause::Spans { .. } | ValidityClause::Delta { .. }) => None,
                    Some(ValidityClause::At(_)) | None => {
                        store.choose_index(&join_indices, rel_app.validity.is_some())
                    }
                };

                match chosen_index {
                    None => {
                        // scan the base relation
                        let delta_posting =
                            resolve_delta_posting_index(tx, &store, &rel_app.validity)?;
                        let mut right = RelAlgebra::relation(
                            right_vars,
                            store,
                            rel_app.span,
                            rel_app.validity.clone(),
                        )?;
                        if let (RelAlgebra::Delta(delta), Some(idx_store)) =
                            (&mut right, delta_posting)
                        {
                            delta.scan = crate::exec::op::temporal::DeltaScan::Accelerated {
                                posting: idx_store,
                            };
                        }
                        ensure!(prev_joiner_vars.len() == right_joiner_vars.len(), "join key arity mismatch between sides");
                        ret = ret.join_capturing_premise(
                            right,
                            prev_joiner_vars,
                            right_joiner_vars,
                            rel_app.span,
                        )?;
                    }
                    Some((idx_ref, requires_back_join)) => {
                        // Resolve the chosen index by reference (D1).
                        let IndexKind::Plain { mapper } = &idx_ref.kind else {
                            bail!(PlanInvariantError(
                                "choose_index picked a manifest-backed index"
                            ));
                        };
                        let mapper = mapper.clone();
                        let idx_store = get_relation(tx, &idx_ref.relation_name(&store.name))?;

                        if !requires_back_join {
                            // index-only: the index covers every needed column
                            let new_right_vars = mapper
                                .iter()
                                .map(|i| {
                                    right_vars.get(*i).cloned().ok_or(PlanInvariantError(
                                        "index mapper column beyond the relation's arity",
                                    ))
                                })
                                .collect::<Result<Vec<_>, _>>()?;
                            let right = RelAlgebra::relation(
                                new_right_vars,
                                idx_store,
                                rel_app.span,
                                rel_app.validity.clone(),
                            )?;
                            ensure!(prev_joiner_vars.len() == right_joiner_vars.len(), "join key arity mismatch between sides");
                            // Covering index: the scanned row *is* the
                            // relation atom's grounding (projected).
                            ret = ret.join_capturing_premise(
                                right,
                                prev_joiner_vars,
                                right_joiner_vars,
                                rel_app.span,
                            )?;
                        } else {
                            // index-with-back-join: join the index by the
                            // bound prefix, then join back to the base
                            // relation by its keys, then re-check any join
                            // columns the index could not bind.
                            let mut not_bound = vec![true; prev_joiner_vars.len()];
                            let mut index_vars = vec![];
                            {
                                let mut left_keys = vec![];
                                let mut right_keys = vec![];
                                for &orig_idx in mapper.iter() {
                                    let orig_var =
                                        right_vars.get(orig_idx).ok_or(PlanInvariantError(
                                            "index mapper column beyond the relation's arity",
                                        ))?;
                                    // A fresh symbol for the index column.
                                    let tv = gen_symb(orig_var.span);
                                    // If the column is a joiner, join the
                                    // index on it and mark it bound.
                                    if let Some(join_idx) = right_joiner_vars_pos_rev[orig_idx] {
                                        not_bound[join_idx] = false;
                                        left_keys.push(prev_joiner_vars[join_idx].clone());
                                        right_keys.push(tv.clone());
                                    }
                                    index_vars.push(tv);
                                }
                                let index = RelAlgebra::relation(
                                    index_vars.clone(),
                                    idx_store,
                                    rel_app.span,
                                    rel_app.validity.clone(),
                                )?;
                                ret = ret.join(index, left_keys, right_keys, rel_app.span)?;
                            }
                            // Join the index back to the base relation.
                            {
                                let mut left_keys = Vec::with_capacity(store.metadata.keys.len());
                                let mut right_keys = Vec::with_capacity(store.metadata.keys.len());
                                for (index_idx, &orig_idx) in mapper.iter().enumerate() {
                                    if orig_idx < store.metadata.keys.len() {
                                        left_keys.push(index_vars[index_idx].clone());
                                        right_keys.push(right_vars[orig_idx].clone());
                                    }
                                }
                                let relation = RelAlgebra::relation(
                                    right_vars,
                                    store,
                                    rel_app.span,
                                    rel_app.validity.clone(),
                                )?;
                                // Base relation row is the premise; the
                                // preceding index join does not capture.
                                ret = ret.join_capturing_premise(
                                    relation,
                                    left_keys,
                                    right_keys,
                                    rel_app.span,
                                )?;
                            }
                            // Re-check join columns not bound via the index.
                            for (i, nb) in not_bound.into_iter().enumerate() {
                                if !nb {
                                    continue;
                                }
                                let (left, right) =
                                    (prev_joiner_vars[i].clone(), right_joiner_vars[i].clone());
                                ret = ret.filter(Expr::build_equate(
                                    vec![
                                        Expr::Binding {
                                            var: left,
                                            tuple_pos: BindingPos::Unresolved,
                                        },
                                        Expr::Binding {
                                            var: right,
                                            tuple_pos: BindingPos::Unresolved,
                                        },
                                    ],
                                    rel_app.span,
                                ))?;
                            }
                        }
                    }
                }
            }
            MagicAtom::NegatedRule(rule_app) => {
                // Consumes an occurrence id (keeps numbering in lockstep
                // with `MagicInlineRule::contained_rules`) but the id is
                // never selected for delta narrowing — negation always
                // reads totals (`NegJoin::iter_batched` never threads
                // `delta_from` to its own right side).
                let negated_occurrence = next_occurrence();
                let store_arity = store_arities.get(&rule_app.name).ok_or_else(|| {
                    RuleNotFound(rule_app.name.clone(), rule_app.name.as_plain_symbol().span)
                })?;
                ensure!(
                    *store_arity == rule_app.args.len(),
                    ArityMismatch(
                        rule_app.name.as_plain_symbol().clone(),
                        *store_arity,
                        rule_app.args.len(),
                        rule_app.span
                    )
                );

                let mut prev_joiner_vars = vec![];
                let mut right_joiner_vars = vec![];
                let mut right_vars = vec![];

                for var in &rule_app.args {
                    if seen_variables.contains(var) {
                        prev_joiner_vars.push(var.clone());
                        let rk = gen_symb(var.span);
                        right_vars.push(rk.clone());
                        right_joiner_vars.push(rk);
                    } else {
                        right_vars.push(var.clone());
                    }
                }

                let right = RelAlgebra::derived(
                    right_vars,
                    rule_app.name.clone(),
                    negated_occurrence,
                    rule_app.span,
                );
                ensure!(prev_joiner_vars.len() == right_joiner_vars.len(), "join key arity mismatch between sides");
                ret = ret.neg_join(right, prev_joiner_vars, right_joiner_vars, rule_app.span)?;
            }
            MagicAtom::NegatedRelation(rel_app) => {
                let store = get_relation(tx, &rel_app.name)?;
                if store.access_level < AccessLevel::ReadOnly {
                    bail!(InsufficientAccessLevel(
                        store.name.to_string(),
                        "reading rows".to_string(),
                        store.access_level
                    ));
                }
                ensure!(
                    store.arity() == rel_app.args.len(),
                    ArityMismatch(
                        rel_app.name.clone(),
                        store.arity(),
                        rel_app.args.len(),
                        rel_app.span
                    )
                );

                let mut prev_joiner_vars = vec![];
                let mut right_joiner_vars = vec![];
                let mut right_vars = vec![];
                let mut join_indices = vec![];

                for var in rel_app.args.iter() {
                    if seen_variables.contains(var) {
                        prev_joiner_vars.push(var.clone());
                        let rk = gen_symb(var.span);
                        right_vars.push(rk.clone());
                        right_joiner_vars.push(rk);
                        join_indices.push(IndexPositionUse::Join)
                    } else {
                        seen_variables.insert(var.clone());
                        right_vars.push(var.clone());
                        if var.kind() == SymbolKind::GeneratedIgnored {
                            join_indices.push(IndexPositionUse::Ignored)
                        } else {
                            join_indices.push(IndexPositionUse::BindForLater)
                        }
                    }
                }

                // Same short-circuit as the positive arm: a temporal
                // clause always scans the base relation directly — an
                // index mirrors only the current-state keyspace, never a
                // `Spans`/`Delta` derivation, negated or not (unrelated to
                // `neg_join` below, which serves all three time-travel
                // shapes as a negation right side since story #86).
                let chosen_index = match &rel_app.validity {
                    Some(ValidityClause::Spans { .. } | ValidityClause::Delta { .. }) => None,
                    Some(ValidityClause::At(_)) | None => {
                        store.choose_index(&join_indices, rel_app.validity.is_some())
                    }
                };

                match chosen_index {
                    // No usable index, or one that would need a back-join
                    // (useless under negation: the anti-join needs the
                    // base rows themselves): scan the base relation.
                    None | Some((_, true)) => {
                        let delta_posting =
                            resolve_delta_posting_index(tx, &store, &rel_app.validity)?;
                        let mut right = RelAlgebra::relation(
                            right_vars,
                            store,
                            rel_app.span,
                            rel_app.validity.clone(),
                        )?;
                        if let (RelAlgebra::Delta(delta), Some(idx_store)) =
                            (&mut right, delta_posting)
                        {
                            delta.scan = crate::exec::op::temporal::DeltaScan::Accelerated {
                                posting: idx_store,
                            };
                        }
                        ensure!(prev_joiner_vars.len() == right_joiner_vars.len(), "join key arity mismatch between sides");
                        ret =
                            ret.neg_join(right, prev_joiner_vars, right_joiner_vars, rel_app.span)?;
                    }
                    Some((idx_ref, false)) => {
                        // index-only
                        let IndexKind::Plain { mapper } = &idx_ref.kind else {
                            bail!(PlanInvariantError(
                                "choose_index picked a manifest-backed index"
                            ));
                        };
                        let idx_store = get_relation(tx, &idx_ref.relation_name(&store.name))?;
                        let new_right_vars = mapper
                            .iter()
                            .map(|i| {
                                right_vars.get(*i).cloned().ok_or(PlanInvariantError(
                                    "index mapper column beyond the relation's arity",
                                ))
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        let right = RelAlgebra::relation(
                            new_right_vars,
                            idx_store,
                            rel_app.span,
                            rel_app.validity.clone(),
                        )?;
                        ensure!(prev_joiner_vars.len() == right_joiner_vars.len(), "join key arity mismatch between sides");
                        ret =
                            ret.neg_join(right, prev_joiner_vars, right_joiner_vars, rel_app.span)?;
                    }
                }
            }
            MagicAtom::Predicate(p) => {
                ret = ret.filter(p.clone())?;
            }
            MagicAtom::Search(sa) => {
                // Join semantics for an already-bound output column: the
                // search emits into a fresh variable and an equality filter
                // joins it to the existing binding — the same discipline as
                // a repeated variable in a relation application.
                let mut sa = sa.clone();
                let mut post_filters = vec![];
                for b in sa.own_bindings.iter_mut() {
                    if seen_variables.contains(b) {
                        let fresh = gen_symb(sa.span);
                        post_filters.push(Expr::build_equate(
                            vec![
                                Expr::Binding {
                                    var: b.clone(),
                                    tuple_pos: BindingPos::Unresolved,
                                },
                                Expr::Binding {
                                    var: fresh.clone(),
                                    tuple_pos: BindingPos::Unresolved,
                                },
                            ],
                            sa.span,
                        ));
                        *b = fresh;
                    } else {
                        seen_variables.insert(b.clone());
                    }
                }
                ret = RelAlgebra::Search(Box::new(SearchRA {
                    parent: Box::new(ret),
                    atom: *sa,
                }));
                for f in post_filters {
                    ret = ret.filter(f)?;
                }
            }
            MagicAtom::Unification(u) => {
                if seen_variables.contains(&u.binding) {
                    let expr = if u.one_many_unif {
                        Expr::build_is_in(
                            vec![
                                Expr::Binding {
                                    var: u.binding.clone(),
                                    tuple_pos: BindingPos::Unresolved,
                                },
                                u.expr.clone(),
                            ],
                            u.span,
                        )
                    } else {
                        Expr::build_equate(
                            vec![
                                Expr::Binding {
                                    var: u.binding.clone(),
                                    tuple_pos: BindingPos::Unresolved,
                                },
                                u.expr.clone(),
                            ],
                            u.span,
                        )
                    };
                    ret = ret.filter(expr)?;
                } else {
                    seen_variables.insert(u.binding.clone());
                    ret = ret.unify(
                        u.binding.clone(),
                        u.expr.clone(),
                        if u.one_many_unif {
                            crate::exec::op::UnificationKind::Spread
                        } else {
                            crate::exec::op::UnificationKind::Single
                        },
                        u.span,
                    );
                }
            }
        }
    }

    let ret_vars_set: BTreeSet<Symbol> = ret_vars.iter().cloned().collect();
    ret.eliminate_temp_vars(&ret_vars_set)?;
    let cur_ret_set: BTreeSet<_> = ret.bindings_after_eliminate().into_iter().collect();
    if cur_ret_set != ret_vars_set {
        let ret_span = ret.span();
        ret = ret.cartesian_join(RelAlgebra::unit(ret_span), ret_span)?;
        ret.eliminate_temp_vars(&ret_vars_set)?;
    }

    let cur_ret_set: BTreeSet<_> = ret.bindings_after_eliminate().into_iter().collect();
    if cur_ret_set != ret_vars_set {
        // The plan's frame is always a subset of the head after the
        // eliminations above, so the difference names the unbound head
        // symbol; an empty difference would mean the subset invariant
        // broke (the original `unwrap`ped it away).
        match ret_vars_set.difference(&cur_ret_set).next() {
            Some(unbound) => bail!(UnboundSymbolInRuleHead(unbound.clone(), unbound.span)),
            None => bail!(PlanInvariantError(
                "plan frame disagrees with the rule head without an unbound head symbol"
            )),
        }
    }
    let cur_ret_bindings = ret.bindings_after_eliminate();
    if ret_vars != cur_ret_bindings {
        ret = ret.reorder(ret_vars.to_vec());
    }

    Ok(ret)
}

// ─────────────────────────────────────────────────────────────────────────
// SEAM IMPLEMENTATION: compiled plans as the evaluator's RuleBody
// ─────────────────────────────────────────────────────────────────────────

/// A compiled rule bound to a transaction: implementation #2 of the
/// `RuleBody` seam (implementation #1 is the oracle-model harness in
/// `query/eval.rs`'s tests — the differential tests in this file prove
/// them equal through the shared oracle).
///
/// The seam contract, and where each clause is discharged:
/// - `delta_from: Some(k)` deltas EVERY occurrence of `k` — enforced by
///   `TempStoreRA` (scan and prefix-join alike) in `query/ra.rs`;
/// - negated occurrences always read totals — enforced by
///   `TempStoreRA::neg_join`;
/// - `ControlFlow::Break` stops iteration and returns `Ok(())` — handled
///   here;
/// - when `want_premises` is true, positive-literal grounding rows ride the
///   batch premise channel (capturing joins / search); when false, the
///   channel stays `None` and callbacks see [`Premises::NotRequested`];
/// - iteration order is a function of stores and plan alone — the
///   operators are order-preserving over canonical-order store scans and
///   memcmp-order relation scans.
pub struct CompiledRuleBody<'a, T> {
    pub plan: &'a CompiledRule,
    pub tx: &'a T,
    pub segments: Segments<'a>,
}

impl<T: ReadTx> crate::exec::fixpoint::eval::seal::Sealed for CompiledRuleBody<'_, T> {}

impl<T: ReadTx> RuleBody for CompiledRuleBody<'_, T> {
    fn for_each_derivation(
        &self,
        stores: &BTreeMap<MagicSymbol, EpochStore>,
        delta_from: Option<AtomOccurrence>,
        want_premises: bool,
        f: &mut dyn FnMut(Cow<'_, [DataValue]>, Premises<'_>) -> Result<ControlFlow<()>>,
    ) -> Result<()> {
        for batch in self.plan.relation.iter_batched(
            self.tx,
            delta_from,
            stores,
            self.segments,
            want_premises,
        )? {
            // Rows cross the seam as borrowed slices into the
            // batch's flattened buffer: eval dedups and filters on
            // the slice and mints an owned row only on admission.
            let batch = batch?;
            if want_premises {
                let premises = match batch.premises() {
                    Some(p) => p,
                    None => &[],
                };
                for (i, row) in batch.iter_rows().enumerate() {
                    let row_premises = match premises.get(i).map(Vec::as_slice) {
                        Some(p) => p,
                        None => &[],
                    };
                    if f(Cow::Borrowed(row), Premises::Rows(row_premises))?.is_break() {
                        return Ok(());
                    }
                }
            } else {
                for row in batch.iter_rows() {
                    if f(Cow::Borrowed(row), Premises::NotRequested)?.is_break() {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    fn contained_rules(&self) -> &BTreeMap<AtomOccurrence, MagicSymbol> {
        &self.plan.contained_rules
    }

    fn premise_sources(&self) -> Option<Vec<PremiseSource>> {
        Some(self.plan.premise_sources.clone())
    }
}

/// The fixed-rule evaluator of a program that HAS no fixed rules:
/// uninhabited, so "running" one is unrepresentable. Callers binding a
/// program proven fixed-rule-free (today: the tests; the parse tier
/// refuses unknown fixed rules much earlier) use this as `F`.
#[derive(Debug)]
pub enum NoFixedRules {}

impl FixedRuleEval for NoFixedRules {
    fn run(
        &self,
        _stores: &BTreeMap<MagicSymbol, EpochStore>,
        _out: &mut RegularTempStore,
        _budget: &crate::exec::fixpoint::eval::Budget,
        _baseline: u64,
    ) -> Result<()> {
        match *self {}
    }
}

/// Bind a compiled program to a transaction, yielding the evaluable tier.
///
/// `make_fixed` injects the fixed-rule evaluator per application — the
/// fixed-rule wiring seam (`runtime/db.rs` supplies the real one, built on
/// `MagicFixedRuleApply::fixed_impl`; a fixed-rule-free caller passes a
/// refusing closure with `F = NoFixedRules`).
pub fn bind_for_eval<'a, T: ReadTx, F: FixedRuleEval>(
    compiled: &'a [CompiledProgram],
    tx: &'a T,
    segments: Segments<'a>,
    make_fixed: &mut dyn FnMut(&'a MagicFixedRuleApply) -> Result<F>,
) -> Result<EvalProgram<CompiledRuleBody<'a, T>, F>> {
    let mut strata = Vec::with_capacity(compiled.len());
    for stratum in compiled {
        let mut out: EvalStratum<CompiledRuleBody<'a, T>, F> = EvalStratum::empty();
        for (name, rule_set) in stratum {
            let def = match rule_set {
                CompiledRuleSet::Rules(rules) => {
                    let bodies = rules
                        .rules
                        .iter()
                        .map(|plan| CompiledRuleBody { plan, tx, segments })
                        .collect();
                    EvalDefinition::Rules(
                        EvalRuleSet::new(rules.aggr.clone(), bodies)
                            .map_err(miette::Report::new)?,
                    )
                }
                CompiledRuleSet::Fixed(fixed) => EvalDefinition::Fixed {
                    arity: fixed.arity,
                    rule: make_fixed(fixed)?,
                },
            };
            out.defs.insert(name.clone(), def);
        }
        strata.push(out);
    }
    EvalProgram::from_execution_order(strata)
}

// ═════════════════════════════════════════════════════════════════════════
// Tests: the first real-storage queries in the project — compile-then-eval
// over FjallStorage, the RA-vs-oracle differentials, and the strategy and
// refusal paths. (The original compile.rs had no in-file tests; the one
// test of the original ra.rs, `test_mat_join`, is ported here where the
// pipeline to drive it lives.)
// ═════════════════════════════════════════════════════════════════════════

// Oracle-differential compile unit corpus: see kyzo-trials gauntlet
// (crate wall forbids kyzo_oracle inside kyzo-core).
