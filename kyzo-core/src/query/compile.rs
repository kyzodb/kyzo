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
 *   (`runtime/relation.rs::get_relation`). `runtime/db.rs` threads the
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
 *   measurement (`kyzo-core/examples/fixpoint_mem_profile.rs`: 18-43×
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
 *   2. compile.rs:655  `ret_vars_set.difference(..).next().unwrap()` under
 *      an `ensure!` — restructured: the unbound symbol is matched, and the
 *      impossible empty-difference case is a typed
 *      [`PlanInvariantError`](crate::query::ra::PlanInvariantError).
 *   3. compile.rs:501,535,569 `debug_assert!(seen_variables.contains(..))`
 *      in the search arms — gone with the search seams (the index tier
 *      re-establishes them as typed checks when those atoms land).
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
 *   D4. Premises are never collected (`Premises::NotRequested`): the
 *       operator tree does not track which rows grounded a derivation.
 *       Witness tables still bind admissions; their `derivation` field is
 *       `None` for RA-backed rules until the operators grow provenance
 *       (SEAM, provenance tier — hooks only, per the eval design).
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

use crate::engines::segments::Segments;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;

use itertools::Itertools;
use miette::{Context, Diagnostic, Result, bail, ensure};
use thiserror::Error;

use crate::data::aggr::Aggregation;
use crate::data::expr::Expr;
use crate::data::program::{
    DeltaAxis, MagicAtom, MagicFixedRuleApply, MagicInlineRule, MagicRulesOrFixed, MagicSymbol,
    StratifiedMagicProgram, ValidityClause,
};
use crate::data::span::SourceSpan;
use crate::data::symb::{Symbol, SymbolKind};
use crate::data::value::DataValue;
use crate::query::eval::{
    AtomOccurrence, EvalDefinition, EvalProgram, EvalRuleSet, EvalStratum, FixedRuleEval, Premises,
    RuleBody,
};
use crate::query::levels::EpochStore;
use crate::query::ra::{PlanInvariantError, RelAlgebra, SearchRA};
use crate::query::temp_store::RegularTempStore;
use crate::runtime::relation::{
    AccessLevel, IndexKind, IndexPositionUse, InsufficientAccessLevel, RelationHandle, get_relation,
};
use crate::storage::ReadTx;

/// One head position's aggregation, if any — the same shape carried by
/// every program tier (`MagicInlineRule::aggr`) and by eval's rule sets.
type HeadAggr = Option<(Aggregation, Vec<DataValue>)>;

/// One compiled stratum: each rule store's definition, ready to bind to a
/// transaction and evaluate. A whole program is `Vec<CompiledProgram>` in
/// execution order.
pub(crate) type CompiledProgram = BTreeMap<MagicSymbol, CompiledRuleSet>;

/// A compiled definition: an inline rule set (plans), or a fixed-rule
/// application handed through to the fixed-rule tier unchanged.
#[derive(Debug)]
pub(crate) enum CompiledRuleSet {
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
pub(crate) struct CompiledInlineRules {
    /// The head's per-position aggregation signature, uniform across the
    /// set's rules.
    pub(crate) aggr: Vec<HeadAggr>,
    /// The rules' plans, in source order. Non-empty.
    pub(crate) rules: Vec<CompiledRule>,
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
pub(crate) struct RulesetHeadAggrMismatch(String, #[label] SourceSpan);

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
                RulesetHeadAggrMismatch(name.to_string(), name.as_plain_symbol().span)
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
pub(crate) struct CompiledRule {
    pub(crate) relation: RelAlgebra,
    pub(crate) contained_rules: BTreeMap<AtomOccurrence, MagicSymbol>,
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
    pub(crate) fn contained_rules(&self) -> BTreeMap<AtomOccurrence, MagicSymbol> {
        let mut coll = BTreeMap::new();
        for (occurrence, atom) in atom_occurrences(&self.body) {
            match atom {
                MagicAtom::Rule(rule) | MagicAtom::NegatedRule(rule) => {
                    coll.insert(occurrence, rule.name.clone());
                }
                _ => {}
            }
        }
        coll
    }
}

#[derive(Debug, Error, Diagnostic)]
#[error("Requested rule {0} not found")]
#[diagnostic(code(eval::rule_not_found))]
struct RuleNotFound(String, #[label] SourceSpan);

#[derive(Debug, Error, Diagnostic)]
#[error("Arity mismatch for rule application {0}")]
#[diagnostic(code(eval::rule_arity_mismatch))]
#[diagnostic(help("Required arity: {1}, number of arguments given: {2}"))]
struct ArityMismatch(String, usize, usize, #[label] SourceSpan);

#[derive(Debug, Error, Diagnostic)]
#[error("Symbol '{0}' in rule head is unbound")]
#[diagnostic(code(eval::unbound_symb_in_head))]
#[diagnostic(help(
    "Note that symbols occurring only in negated positions are not considered bound"
))]
struct UnboundSymbolInRuleHead(String, #[label] SourceSpan);

/// Compile every stratum of a proven program into executable plans.
///
/// The input tier is execution-ordered by construction; the output `Vec`
/// keeps that order (`compiled[0]` evaluates first).
pub(crate) fn stratified_magic_compile(
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
                    RuleNotFound(
                        rule_app.name.as_plain_symbol().to_string(),
                        rule_app.name.as_plain_symbol().span,
                    )
                })?;

                ensure!(
                    *store_arity == rule_app.args.len(),
                    ArityMismatch(
                        rule_app.name.as_plain_symbol().to_string(),
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
                debug_assert_eq!(prev_joiner_vars.len(), right_joiner_vars.len());
                ret = ret.join(right, prev_joiner_vars, right_joiner_vars, rule_app.span)?;
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
                        rel_app.name.to_string(),
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
                    _ => store.choose_index(&join_indices, rel_app.validity.is_some()),
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
                            delta.posting = Some(idx_store);
                        }
                        debug_assert_eq!(prev_joiner_vars.len(), right_joiner_vars.len());
                        ret = ret.join(right, prev_joiner_vars, right_joiner_vars, rel_app.span)?;
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
                            debug_assert_eq!(prev_joiner_vars.len(), right_joiner_vars.len());
                            ret =
                                ret.join(right, prev_joiner_vars, right_joiner_vars, rel_app.span)?;
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
                                ret = ret.join(relation, left_keys, right_keys, rel_app.span)?;
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
                                            tuple_pos: None,
                                        },
                                        Expr::Binding {
                                            var: right,
                                            tuple_pos: None,
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
                    RuleNotFound(
                        rule_app.name.as_plain_symbol().to_string(),
                        rule_app.name.as_plain_symbol().span,
                    )
                })?;
                ensure!(
                    *store_arity == rule_app.args.len(),
                    ArityMismatch(
                        rule_app.name.as_plain_symbol().to_string(),
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
                debug_assert_eq!(prev_joiner_vars.len(), right_joiner_vars.len());
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
                        rel_app.name.to_string(),
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
                    _ => store.choose_index(&join_indices, rel_app.validity.is_some()),
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
                            delta.posting = Some(idx_store);
                        }
                        debug_assert_eq!(prev_joiner_vars.len(), right_joiner_vars.len());
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
                        debug_assert_eq!(prev_joiner_vars.len(), right_joiner_vars.len());
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
                                    tuple_pos: None,
                                },
                                Expr::Binding {
                                    var: fresh.clone(),
                                    tuple_pos: None,
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
                    query_bytecode: vec![],
                    filter_bytecode: None,
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
                                    tuple_pos: None,
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
                                    tuple_pos: None,
                                },
                                u.expr.clone(),
                            ],
                            u.span,
                        )
                    };
                    ret = ret.filter(expr)?;
                } else {
                    seen_variables.insert(u.binding.clone());
                    ret = ret.unify(u.binding.clone(), u.expr.clone(), u.one_many_unif, u.span);
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
            Some(unbound) => bail!(UnboundSymbolInRuleHead(unbound.to_string(), unbound.span)),
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
/// - premises are `NotRequested` (deviation D4: the operators do not track
///   grounding rows);
/// - iteration order is a function of stores and plan alone — the
///   operators are order-preserving over canonical-order store scans and
///   memcmp-order relation scans.
pub(crate) struct CompiledRuleBody<'a, T> {
    plan: &'a CompiledRule,
    tx: &'a T,
    segments: Segments<'a>,
}

impl<T: ReadTx> RuleBody for CompiledRuleBody<'_, T> {
    fn for_each_derivation(
        &self,
        stores: &BTreeMap<MagicSymbol, EpochStore>,
        delta_from: Option<AtomOccurrence>,
        _want_premises: bool,
        f: &mut dyn FnMut(Cow<'_, [DataValue]>, Premises<'_>) -> Result<ControlFlow<()>>,
    ) -> Result<()> {
        for batch in self
            .plan
            .relation
            .iter_batched(self.tx, delta_from, stores, self.segments)?
        {
            // Rows cross the seam as borrowed slices into the
            // batch's flattened buffer: eval dedups and filters on
            // the slice and mints an owned row only on admission.
            let batch = batch?;
            for row in batch.iter_rows() {
                if f(Cow::Borrowed(row), Premises::NotRequested)?.is_break() {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn contained_rules(&self) -> &BTreeMap<AtomOccurrence, MagicSymbol> {
        &self.plan.contained_rules
    }
}

/// The fixed-rule evaluator of a program that HAS no fixed rules:
/// uninhabited, so "running" one is unrepresentable. Callers binding a
/// program proven fixed-rule-free (today: the tests; the parse tier
/// refuses unknown fixed rules much earlier) use this as `F`.
#[derive(Debug)]
pub(crate) enum NoFixedRules {}

impl FixedRuleEval for NoFixedRules {
    fn run(
        &self,
        _stores: &BTreeMap<MagicSymbol, EpochStore>,
        _out: &mut RegularTempStore,
        _budget: &crate::query::eval::Budget,
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
pub(crate) fn bind_for_eval<'a, T: ReadTx, F: FixedRuleEval>(
    compiled: &'a [CompiledProgram],
    tx: &'a T,
    segments: Segments<'a>,
    make_fixed: &mut dyn FnMut(&'a MagicFixedRuleApply) -> Result<F>,
) -> Result<EvalProgram<CompiledRuleBody<'a, T>, F>> {
    let mut strata = Vec::with_capacity(compiled.len());
    for stratum in compiled {
        let mut out: EvalStratum<CompiledRuleBody<'a, T>, F> = EvalStratum::default();
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::num::NonZeroU32;

    use smartstring::SmartString;

    use super::*;
    use crate::data::aggr::parse_aggr;
    use crate::data::program::{
        InputRelationHandle, MagicProgram, MagicRelationApplyAtom, MagicRuleApplyAtom,
        StoreLifetimes, Unification,
    };
    use crate::data::relation::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
    use crate::data::tuple::Tuple;
    use crate::query::eval::{Budget, RowLimit, stratified_evaluate};
    use crate::query::laws::{Literal, Program, Rel, Rule, Term, naive_eval};
    use crate::runtime::relation::KeyspaceKind;
    use crate::runtime::relation::{create_relation, set_access_level};
    use crate::storage::fjall::{FjallStorage, new_fjall_storage};
    use crate::storage::{Storage, WriteTx};

    // ── plumbing ─────────────────────────────────────────────────────────

    fn sp() -> SourceSpan {
        SourceSpan(0, 0)
    }
    fn sym(name: &str) -> Symbol {
        Symbol::new(name, sp())
    }
    fn v(i: i64) -> DataValue {
        DataValue::from(i)
    }
    fn muggle(rel: &str) -> MagicSymbol {
        MagicSymbol::Muggle { inner: sym(rel) }
    }
    fn entry_symbol() -> MagicSymbol {
        MagicSymbol::Muggle {
            inner: Symbol::prog_entry(sp()),
        }
    }
    fn generous_budget() -> Budget {
        // Arm the derived-tuple ceiling as well as the epoch ceiling: a
        // differential run against a MUTATED plan (e.g. an eliminate that
        // never fires) can diverge, and eval checks this dimension at the
        // epoch barrier (eval.rs, typed LimitExceeded{DerivedTuples}). Every
        // legitimate corpus here admits well under 100 tuples in total, so
        // 1_000 gives 10x headroom and never refuses a real run.
        //
        // The number is deliberately modest, not "astronomically large": a
        // divergence that disables column elimination *widens* the tuples
        // every epoch, so the process exhausts memory before the CUMULATIVE
        // admitted count reaches a large ceiling. Measured under the test
        // memory cap, a ceiling of 1_000 trips into a typed refusal while
        // 10_000+ still allocation-aborts. Keep this low.
        Budget::new(NonZeroU32::new(10_000).expect("nonzero")).with_derived_tuple_ceiling(1_000)
    }

    /// A bounded-but-larger budget for the batch-boundary equivalence tests,
    /// which deliberately build stores that straddle `BATCH_ROWS`=1024 (a
    /// chain's `path` store, a wide relation of a few thousand rows) and so
    /// legitimately need more than `generous_budget`'s intentionally-low
    /// 1_000. Sized just above those workloads' real derived-tuple spend
    /// (tens of thousands) and far below any OOM regime — the equivalence
    /// tests run CORRECT plans (plus one row-dropping mutation, which only
    /// shrinks the tuple set), so the OOM-before-ceiling hazard that keeps
    /// `generous_budget` low does not apply here.
    fn boundary_budget() -> Budget {
        Budget::new(NonZeroU32::new(10_000).expect("nonzero")).with_derived_tuple_ceiling(200_000)
    }

    fn col(name: &str) -> ColumnDef {
        ColumnDef {
            name: SmartString::from(name),
            typing: NullableColType {
                coltype: ColType::Any,
                nullable: false,
            },
            default_gen: None,
        }
    }

    /// Create an all-key-columns stored relation and fill it with rows.
    fn stored_relation(db: &FjallStorage, name: &str, arity: usize, rows: &[Tuple]) {
        let keys: Vec<ColumnDef> = (0..arity).map(|i| col(&format!("c{i}"))).collect();
        let key_bindings = keys.iter().map(|c| sym(&c.name)).collect();
        let input = InputRelationHandle {
            name: sym(name),
            metadata: StoredRelationMetadata {
                keys,
                non_keys: vec![],
            },
            key_bindings,
            dep_bindings: vec![],
            span: sp(),
        };
        let mut tx = db.write_tx().expect("write tx");
        let handle = create_relation(&mut tx, input, KeyspaceKind::Facts).expect("create relation");
        for row in rows {
            handle
                .put_fact(
                    &mut tx,
                    row,
                    crate::data::value::ValidityTs::from_raw(0),
                    sp(),
                )
                .expect("put row");
        }
        tx.commit().expect("commit");
    }

    // Body-atom builders.
    fn rule_atom(name: &str, args: &[Symbol]) -> MagicAtom {
        MagicAtom::Rule(MagicRuleApplyAtom {
            name: muggle(name),
            args: args.to_vec(),
            span: sp(),
        })
    }
    fn neg_rule_atom(name: &str, args: &[Symbol]) -> MagicAtom {
        MagicAtom::NegatedRule(MagicRuleApplyAtom {
            name: muggle(name),
            args: args.to_vec(),
            span: sp(),
        })
    }
    fn rel_atom(name: &str, args: &[Symbol]) -> MagicAtom {
        MagicAtom::Relation(MagicRelationApplyAtom {
            name: sym(name),
            args: args.to_vec(),
            validity: None,
            span: sp(),
        })
    }
    fn neg_rel_atom(name: &str, args: &[Symbol]) -> MagicAtom {
        MagicAtom::NegatedRelation(MagicRelationApplyAtom {
            name: sym(name),
            args: args.to_vec(),
            validity: None,
            span: sp(),
        })
    }
    fn unif(binding: Symbol, val: DataValue) -> MagicAtom {
        MagicAtom::Unification(Unification {
            binding,
            expr: Expr::Const { val, span: sp() },
            one_many_unif: false,
            span: sp(),
        })
    }

    fn plain_rule(head: &[Symbol], body: Vec<MagicAtom>) -> MagicInlineRule {
        MagicInlineRule {
            head: head.to_vec(),
            aggr: vec![None; head.len()],
            body,
        }
    }

    fn program_of(strata: Vec<Vec<(MagicSymbol, Vec<MagicInlineRule>)>>) -> StratifiedMagicProgram {
        let strata = strata
            .into_iter()
            .map(|defs| {
                let mut prog = MagicProgram::default();
                for (name, rules) in defs {
                    prog.prog.insert(name, MagicRulesOrFixed::Rules { rules });
                }
                prog
            })
            .collect();
        StratifiedMagicProgram::from_execution_order(strata).expect("entry in final stratum")
    }

    /// Lifetimes: every store lives to the end (fine for tests; the real
    /// map comes from the stratifier).
    fn immortal_lifetimes(compiled: &[CompiledProgram]) -> StoreLifetimes {
        let mut lifetimes = StoreLifetimes::default();
        let last = compiled.len().saturating_sub(1);
        for stratum in compiled {
            for name in stratum.keys() {
                lifetimes.note_use(name.clone(), last);
            }
        }
        lifetimes
    }

    /// Compile against a read snapshot and evaluate to the entry rows, on
    /// the classic iterator path.
    fn compile_and_run(db: &FjallStorage, prog: StratifiedMagicProgram) -> BTreeSet<Tuple> {
        compile_and_run_mode(db, prog)
    }

    /// [`compile_and_run`] over a chosen execution mode. The differential
    /// harness runs BOTH modes and asserts each equals the oracle, which is
    /// what proves the batched (vectorized) path equivalent.
    fn compile_and_run_mode(db: &FjallStorage, prog: StratifiedMagicProgram) -> BTreeSet<Tuple> {
        compile_and_run_mode_budget(db, prog, generous_budget())
    }

    /// [`compile_and_run_mode`] over an explicit budget — the batch-boundary
    /// tests pass [`boundary_budget`] because they exceed the deliberately
    /// low `generous_budget`.
    fn compile_and_run_mode_budget(
        db: &FjallStorage,
        prog: StratifiedMagicProgram,
        budget: Budget,
    ) -> BTreeSet<Tuple> {
        let rtx = db.read_tx().expect("read tx");
        let compiled = stratified_magic_compile(&rtx, prog).expect("compiles");
        let lifetimes = immortal_lifetimes(&compiled);
        let program = bind_for_eval::<_, NoFixedRules>(&compiled, &rtx, Segments::OFF, &mut |_| {
            panic!("test programs have no fixed rules")
        })
        .expect("binds");
        let outcome = stratified_evaluate(&program, &lifetimes, RowLimit::default(), &budget, None)
            .expect("evaluates");
        outcome.store.all_iter().map(|t| t.into_tuple()).collect()
    }

    fn rows(data: &[&[i64]]) -> BTreeSet<Tuple> {
        data.iter()
            .map(|r| r.iter().copied().map(v).collect())
            .collect()
    }

    // ── the upstream in-file test, ported ────────────────────────────────

    /// The original ra.rs's `test_mat_join`, driven through the compile
    /// tier over a real stored relation: `a = 3` binds `a` first, so
    /// `data[x, a]` joins on data's SECOND column — the materialized-join
    /// path.
    #[test]
    fn mat_join_reproduces_upstream_example() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        stored_relation(
            &db,
            "data",
            2,
            &[vec![v(1), v(2)], vec![v(1), v(3)], vec![v(2), v(3)]],
        );
        let (x, a) = (sym("x"), sym("a"));
        let prog = program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                std::slice::from_ref(&x),
                vec![unif(a.clone(), v(3)), rel_atom("data", &[x.clone(), a])],
            )],
        )]]);
        assert_eq!(compile_and_run(&db, prog), rows(&[&[1], &[2]]));
    }

    // ── the first real-storage recursive query ───────────────────────────

    /// Transitive closure through REAL RA operators against a stored
    /// relation on a real FjallStorage: semi-naive recursion with the
    /// delta threaded through `TempStoreRA`, base facts scanned from disk.
    #[test]
    fn transitive_closure_end_to_end_over_fjall() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        stored_relation(
            &db,
            "edge",
            2,
            &[
                vec![v(1), v(2)],
                vec![v(2), v(3)],
                vec![v(3), v(4)],
                vec![v(4), v(2)],
            ],
        );
        let (x, y, z) = (sym("x"), sym("y"), sym("z"));
        let prog = program_of(vec![
            vec![(
                muggle("path"),
                vec![
                    plain_rule(
                        &[x.clone(), y.clone()],
                        vec![rel_atom("edge", &[x.clone(), y.clone()])],
                    ),
                    plain_rule(
                        &[x.clone(), y.clone()],
                        vec![
                            rel_atom("edge", &[x.clone(), z.clone()]),
                            rule_atom("path", &[z.clone(), y.clone()]),
                        ],
                    ),
                ],
            )],
            vec![(
                entry_symbol(),
                vec![plain_rule(
                    &[x.clone(), y.clone()],
                    vec![rule_atom("path", &[x, y])],
                )],
            )],
        ]);
        // Reachability of 1→2→3→4→2 (cycle 2-3-4): from 1 everything but
        // 1; within the cycle every pair.
        assert_eq!(
            compile_and_run(&db, prog),
            rows(&[
                &[1, 2],
                &[1, 3],
                &[1, 4],
                &[2, 2],
                &[2, 3],
                &[2, 4],
                &[3, 2],
                &[3, 3],
                &[3, 4],
                &[4, 2],
                &[4, 3],
                &[4, 4],
            ])
        );
    }

    /// The head aligner emits a Reorder when the head order differs from
    /// the body's binding order.
    #[test]
    fn head_reorder_alignment() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        stored_relation(&db, "edge", 2, &[vec![v(1), v(2)]]);
        let (x, y) = (sym("x"), sym("y"));
        let prog = program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                &[y.clone(), x.clone()],
                vec![rel_atom("edge", &[x, y])],
            )],
        )]]);
        assert_eq!(compile_and_run(&db, prog), rows(&[&[2, 1]]));
    }

    // ── join-strategy paths ──────────────────────────────────────────────

    fn join_types_of(ra: &RelAlgebra, out: &mut Vec<&'static str>) {
        match ra {
            RelAlgebra::Join(j) => {
                join_types_of(&j.left, out);
                join_types_of(&j.right, out);
                out.push(j.join_type().expect("join type"));
            }
            RelAlgebra::NegJoin(j) => {
                join_types_of(&j.left, out);
                out.push(j.join_type().expect("neg join type"));
            }
            RelAlgebra::Reorder(r) => join_types_of(&r.relation, out),
            RelAlgebra::Filter(f) => join_types_of(&f.parent, out),
            RelAlgebra::Search(s) => join_types_of(&s.parent, out),
            RelAlgebra::Unification(u) => join_types_of(&u.parent, out),
            RelAlgebra::Fixed(_)
            | RelAlgebra::TempStore(_)
            | RelAlgebra::Stored(_)
            | RelAlgebra::StoredWithValidity(_)
            | RelAlgebra::Spans(_)
            | RelAlgebra::Delta(_) => {}
        }
    }

    fn compiled_entry_join_types(
        db: &FjallStorage,
        prog: StratifiedMagicProgram,
    ) -> Vec<&'static str> {
        let rtx = db.read_tx().unwrap();
        let compiled = stratified_magic_compile(&rtx, prog).expect("compiles");
        let entry = compiled
            .last()
            .and_then(|s| s.get(&entry_symbol()))
            .expect("entry compiled");
        let CompiledRuleSet::Rules(rules) = entry else {
            panic!("entry is an inline rule");
        };
        let mut types = vec![];
        join_types_of(&rules.rules[0].relation, &mut types);
        types
    }

    /// The second body atom joins the stored relation on its FIRST key
    /// column → prefix join; on its SECOND → materialized join. Both give
    /// exactly the expected rows.
    #[test]
    fn join_strategies_prefix_vs_materialized() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        stored_relation(
            &db,
            "edge",
            2,
            &[vec![v(1), v(2)], vec![v(2), v(3)], vec![v(3), v(1)]],
        );
        let (x, y, z) = (sym("x"), sym("y"), sym("z"));

        // ?[x, z] := *edge[x, y], *edge[y, z] — second scan joined on col 0.
        let prefix_prog = program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                &[x.clone(), z.clone()],
                vec![
                    rel_atom("edge", &[x.clone(), y.clone()]),
                    rel_atom("edge", &[y.clone(), z.clone()]),
                ],
            )],
        )]]);
        let types = compiled_entry_join_types(&db, prefix_prog);
        assert!(
            types.contains(&"stored_prefix_join"),
            "expected a stored prefix join, got {types:?}"
        );
        let prefix_prog = program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                &[x.clone(), z.clone()],
                vec![
                    rel_atom("edge", &[x.clone(), y.clone()]),
                    rel_atom("edge", &[y.clone(), z.clone()]),
                ],
            )],
        )]]);
        assert_eq!(
            compile_and_run(&db, prefix_prog),
            rows(&[&[1, 3], &[2, 1], &[3, 2]])
        );

        // ?[x, z] := *edge[x, y], *edge[z, y] — second scan joined on col 1.
        let mat_prog = || {
            program_of(vec![vec![(
                entry_symbol(),
                vec![plain_rule(
                    &[x.clone(), z.clone()],
                    vec![
                        rel_atom("edge", &[x.clone(), y.clone()]),
                        rel_atom("edge", &[z.clone(), y.clone()]),
                    ],
                )],
            )]])
        };
        let types = compiled_entry_join_types(&db, mat_prog());
        assert!(
            types.contains(&"stored_mat_join"),
            "expected a stored materialized join, got {types:?}"
        );
        assert_eq!(
            compile_and_run(&db, mat_prog()),
            rows(&[&[1, 1], &[2, 2], &[3, 3]])
        );
    }

    /// A join binding a stored relation's WHOLE key goes through the
    /// point-lookup specialization of the prefix join.
    #[test]
    fn join_strategy_point_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        stored_relation(&db, "edge", 2, &[vec![v(1), v(2)], vec![v(2), v(3)]]);
        stored_relation(&db, "cand", 2, &[vec![v(1), v(2)], vec![v(1), v(3)]]);
        let (x, y) = (sym("x"), sym("y"));
        // ?[x, y] := *cand[x, y], *edge[x, y] — edge joined on both keys.
        let prog = program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                &[x.clone(), y.clone()],
                vec![
                    rel_atom("cand", &[x.clone(), y.clone()]),
                    rel_atom("edge", &[x, y]),
                ],
            )],
        )]]);
        assert_eq!(compile_and_run(&db, prog), rows(&[&[1, 2]]));
    }

    /// Negation strategies: a negated stored relation joined on a key
    /// prefix (stored_neg_prefix_join) vs on a non-prefix column
    /// (stored_neg_mat_join); both semantics exact.
    #[test]
    fn neg_join_strategies() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        stored_relation(&db, "edge", 2, &[vec![v(1), v(2)], vec![v(2), v(3)]]);
        stored_relation(&db, "blocked", 2, &[vec![v(1), v(2)]]);
        stored_relation(&db, "sink", 2, &[vec![v(9), v(3)]]);
        let (x, y) = (sym("x"), sym("y"));

        // not *blocked[x, y]: negation joined on the full key prefix.
        let prefix_prog = || {
            program_of(vec![vec![(
                entry_symbol(),
                vec![plain_rule(
                    &[x.clone(), y.clone()],
                    vec![
                        rel_atom("edge", &[x.clone(), y.clone()]),
                        neg_rel_atom("blocked", &[x.clone(), y.clone()]),
                    ],
                )],
            )]])
        };
        let types = compiled_entry_join_types(&db, prefix_prog());
        assert!(
            types.contains(&"stored_neg_prefix_join"),
            "expected stored_neg_prefix_join, got {types:?}"
        );
        assert_eq!(compile_and_run(&db, prefix_prog()), rows(&[&[2, 3]]));

        // not *sink[w, y] with w fresh: negation joined on column 1 only —
        // the materialized (set-probe) negation.
        let w = sym("w");
        let mat_prog = || {
            program_of(vec![vec![(
                entry_symbol(),
                vec![plain_rule(
                    &[x.clone(), y.clone()],
                    vec![
                        rel_atom("edge", &[x.clone(), y.clone()]),
                        neg_rel_atom("sink", &[w.clone(), y.clone()]),
                    ],
                )],
            )]])
        };
        let types = compiled_entry_join_types(&db, mat_prog());
        assert!(
            types.contains(&"stored_neg_mat_join"),
            "expected stored_neg_mat_join, got {types:?}"
        );
        // (9, 3) blocks y = 3.
        assert_eq!(compile_and_run(&db, mat_prog()), rows(&[&[1, 2]]));
    }

    // ── typed refusals ───────────────────────────────────────────────────

    #[test]
    fn unknown_rule_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let x = sym("x");
        let prog = program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                std::slice::from_ref(&x),
                vec![rule_atom("ghost", std::slice::from_ref(&x))],
            )],
        )]]);
        let rtx = db.read_tx().unwrap();
        let err = stratified_magic_compile(&rtx, prog).unwrap_err();
        assert!(err.to_string().contains("not found"), "{err:?}");
    }

    #[test]
    fn rule_arity_mismatch_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        stored_relation(&db, "edge", 2, &[]);
        let x = sym("x");
        let prog = program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                std::slice::from_ref(&x),
                vec![rel_atom("edge", std::slice::from_ref(&x))],
            )],
        )]]);
        let rtx = db.read_tx().unwrap();
        let err = stratified_magic_compile(&rtx, prog).unwrap_err();
        assert!(err.to_string().contains("Arity mismatch"), "{err:?}");
    }

    #[test]
    fn unbound_head_symbol_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        stored_relation(&db, "edge", 2, &[]);
        let (x, y, q) = (sym("x"), sym("y"), sym("q"));
        // ?[x, q] := *edge[x, y] — q bound nowhere.
        let prog = program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(&[x.clone(), q], vec![rel_atom("edge", &[x, y])])],
        )]]);
        let rtx = db.read_tx().unwrap();
        let err = stratified_magic_compile(&rtx, prog).unwrap_err();
        assert!(
            err.to_string().contains("in rule head is unbound"),
            "{err:?}"
        );
    }

    /// Trap (c) of the reconciliation notes: arg-level aggregation
    /// signature equality is enforced at the compile tier too.
    #[test]
    fn head_aggr_mismatch_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        stored_relation(&db, "edge", 2, &[]);
        let (x, y) = (sym("x"), sym("y"));
        let min_aggr = parse_aggr("min").expect("min exists");
        let with_aggr = MagicInlineRule {
            head: vec![x.clone(), y.clone()],
            aggr: vec![None, Some((min_aggr, vec![]))],
            body: vec![rel_atom("edge", &[x.clone(), y.clone()])],
        };
        let without_aggr = plain_rule(
            &[x.clone(), y.clone()],
            vec![rel_atom("edge", &[x.clone(), y.clone()])],
        );
        let entry_reader = plain_rule(&[x.clone(), y.clone()], vec![rule_atom("m", &[x, y])]);
        let prog = program_of(vec![
            vec![(muggle("m"), vec![with_aggr, without_aggr])],
            vec![(entry_symbol(), vec![entry_reader])],
        ]);
        let rtx = db.read_tx().unwrap();
        let err = stratified_magic_compile(&rtx, prog).unwrap_err();
        assert!(
            err.downcast_ref::<RulesetHeadAggrMismatch>().is_some(),
            "expected RulesetHeadAggrMismatch, got {err:?}"
        );
    }

    /// A below-ReadOnly (hidden) relation cannot be read by a query.
    #[test]
    fn hidden_relation_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        stored_relation(&db, "secret", 1, &[vec![v(1)]]);
        let mut tx = db.write_tx().unwrap();
        set_access_level(&mut tx, &sym("secret"), AccessLevel::Hidden).unwrap();
        tx.commit().unwrap();

        let x = sym("x");
        let prog = program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                std::slice::from_ref(&x),
                vec![rel_atom("secret", std::slice::from_ref(&x))],
            )],
        )]]);
        let rtx = db.read_tx().unwrap();
        let err = stratified_magic_compile(&rtx, prog).unwrap_err();
        assert!(
            err.downcast_ref::<InsufficientAccessLevel>().is_some(),
            "expected InsufficientAccessLevel, got {err:?}"
        );
    }

    // ── the RA-vs-oracle differential ────────────────────────────────────
    //
    // This is the proof that seam implementation #2 (compiled RA plans)
    // equals implementation #1 (the oracle-model harness in eval's tests):
    // both are judged against the same sealed naive evaluator, on the same
    // corpus shapes. The model compiler below mirrors eval's test harness,
    // except that EDB relations become REAL stored relations on a real
    // FjallStorage and rule bodies become compiled operator trees.

    /// Stratum assignment for the model (duplicates the oracle's
    /// Bellman-Ford edge rules; the oracle's own strata are sealed).
    fn strata_of(program: &Program) -> HashMap<Rel, usize> {
        let mut classes: HashMap<Rel, (bool, bool)> = HashMap::new(); // (has_aggr, is_meet)
        {
            let mut per_head: HashMap<Rel, Vec<&Rule>> = HashMap::new();
            for rule in &program.rules {
                per_head.entry(rule.head_rel).or_default().push(rule);
            }
            for (rel, rules) in per_head {
                let has_aggr = rules.iter().any(|r| r.aggr.iter().any(|a| a.is_some()));
                let is_meet = has_aggr
                    && rules.iter().all(|r| {
                        r.aggr.iter().all(|a| match a {
                            None => true,
                            Some((aggregation, _)) => aggregation.is_meet(),
                        })
                    });
                classes.insert(rel, (has_aggr, is_meet));
            }
        }
        let is_meet = |rel: Rel| classes.get(rel).is_some_and(|c| c.1);
        let mut edges = Vec::new();
        for rule in &program.rules {
            let head = rule.head_rel;
            let (has_aggr, head_meet) = classes[&head];
            for l in &rule.body {
                let forcing = if has_aggr {
                    if head_meet && l.rel == head {
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
        let mut s: HashMap<Rel, usize> = HashMap::new();
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

    /// Convert one model literal into a magic atom (plus prepended
    /// unifications for constant arguments), mirroring what the normalize
    /// tier does for real programs.
    fn literal_atoms(
        l: &Literal,
        idb: &BTreeSet<Rel>,
        const_serial: &mut usize,
        out: &mut Vec<MagicAtom>,
    ) {
        let mut args = Vec::with_capacity(l.args.len());
        for t in &l.args {
            match t {
                Term::Var(name) => args.push(sym(name)),
                Term::Const(c) => {
                    let fresh = sym(&format!("*c{}", *const_serial));
                    *const_serial += 1;
                    out.push(unif(fresh.clone(), c.clone()));
                    args.push(fresh);
                }
            }
        }
        let atom = match (idb.contains(l.rel), l.negated) {
            (true, false) => rule_atom(l.rel, &args),
            (true, true) => neg_rule_atom(l.rel, &args),
            (false, false) => rel_atom(l.rel, &args),
            (false, true) => neg_rel_atom(l.rel, &args),
        };
        out.push(atom);
    }

    /// Evaluate `target` of the model through the REAL pipeline tail:
    /// stored EDB → compiled RA plans → semi-naive evaluation.
    fn ra_eval(model: &Program, target: Rel, target_arity: usize) -> BTreeSet<Tuple> {
        assert!(
            model.fixed.is_empty(),
            "RA differential corpus has no fixed rules"
        );
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let idb: BTreeSet<Rel> = model.rules.iter().map(|r| r.head_rel).collect();
        for (rel, facts) in &model.facts {
            assert!(!idb.contains(rel), "facts under a rule head");
            let arity = facts.iter().next().map(|t| t.len()).unwrap_or(1);
            let rows: Vec<Tuple> = facts.iter().cloned().collect();
            stored_relation(&db, rel, arity, &rows);
        }

        let strata_map = strata_of(model);
        let entry_stratum = strata_map.values().copied().max().unwrap_or(0) + 1;
        let mut strata: Vec<Vec<(MagicSymbol, Vec<MagicInlineRule>)>> =
            (0..=entry_stratum).map(|_| Vec::new()).collect();

        let mut per_head: BTreeMap<Rel, Vec<&Rule>> = BTreeMap::new();
        for rule in &model.rules {
            per_head.entry(rule.head_rel).or_default().push(rule);
        }
        let mut const_serial = 0usize;
        for (head, rules) in per_head {
            let stratum = strata_map[head];
            let magic_rules: Vec<MagicInlineRule> = rules
                .iter()
                .map(|r| {
                    let mut body = Vec::new();
                    // Positives first, then negatives: negation is safe
                    // only over bound variables (the reorder tier's job in
                    // the real pipeline).
                    for l in r.body.iter().filter(|l| !l.negated) {
                        literal_atoms(l, &idb, &mut const_serial, &mut body);
                    }
                    for l in r.body.iter().filter(|l| l.negated) {
                        literal_atoms(l, &idb, &mut const_serial, &mut body);
                    }
                    let head_syms: Vec<Symbol> = r
                        .head_args
                        .iter()
                        .map(|t| match t {
                            Term::Var(name) => sym(name),
                            Term::Const(_) => panic!("corpus heads are variables"),
                        })
                        .collect();
                    MagicInlineRule {
                        head: head_syms,
                        aggr: r.aggr.clone(),
                        body,
                    }
                })
                .collect();
            strata[stratum].push((muggle(head), magic_rules));
        }
        // The entry: ?[v0..vn] := target[v0..vn].
        let vars: Vec<Symbol> = ENTRY_VARS[..target_arity].iter().map(|s| sym(s)).collect();
        strata[entry_stratum].push((
            entry_symbol(),
            vec![plain_rule(&vars, vec![rule_atom(target, &vars)])],
        ));

        compile_and_run_mode(&db, program_of(strata))
    }

    /// THE differential: every IDB relation of the model, evaluated by the
    /// real compile+eval pipeline over real storage, must equal the sealed
    /// oracle's answer. A disagreement is a FINDING.
    ///
    /// Both execution modes are checked: the classic iterator path AND the
    /// batched (vectorized) path each equal the oracle. Because a shared
    /// oracle pins both, this simultaneously proves the batched path
    /// equal to the iterator path — the equivalence the vectorization ascent
    /// rests on.
    fn assert_ra_matches_oracle(model: &Program) {
        let oracle_db = naive_eval(model).expect("oracle accepts the program");
        let mut arities: BTreeMap<Rel, usize> = BTreeMap::new();
        for r in &model.rules {
            arities.insert(r.head_rel, r.head_args.len());
        }
        for rel in model
            .rules
            .iter()
            .map(|r| r.head_rel)
            .collect::<BTreeSet<_>>()
        {
            let oracle_rows = oracle_db.get(rel).cloned().unwrap_or_default();
            let ra_rows = ra_eval(model, rel, arities[rel]);
            assert_eq!(
                ra_rows, oracle_rows,
                "FINDING: RA-backed eval disagrees with the oracle on '{rel}'"
            );
        }
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
        if negated {
            Literal::neg(rel, args)
        } else {
            Literal::pos(rel, args)
        }
    }
    fn tx() -> Term {
        Term::Var("X")
    }
    fn ty() -> Term {
        Term::Var("Y")
    }
    fn tz() -> Term {
        Term::Var("Z")
    }

    #[test]
    fn differential_transitive_closure() {
        assert_ra_matches_oracle(&Program {
            rules: vec![
                Rule::plain(
                    "path",
                    vec![tx(), ty()],
                    vec![lit("edge", vec![tx(), ty()], false)],
                ),
                Rule::plain(
                    "path",
                    vec![tx(), ty()],
                    vec![
                        lit("edge", vec![tx(), tz()], false),
                        lit("path", vec![tz(), ty()], false),
                    ],
                ),
            ],
            facts: edge_facts(&[(1, 2), (2, 3), (3, 4), (4, 2)]),
            ..Program::default()
        });
    }

    /// TC by self-join: `path` twice in one body → multiplicity Many →
    /// the complete-re-run path of the delta discipline.
    #[test]
    fn differential_transitive_closure_self_join() {
        assert_ra_matches_oracle(&Program {
            rules: vec![
                Rule::plain(
                    "path",
                    vec![tx(), ty()],
                    vec![lit("edge", vec![tx(), ty()], false)],
                ),
                Rule::plain(
                    "path",
                    vec![tx(), tz()],
                    vec![
                        lit("path", vec![tx(), ty()], false),
                        lit("path", vec![ty(), tz()], false),
                    ],
                ),
            ],
            facts: edge_facts(&[(1, 2), (2, 3), (3, 1), (3, 4)]),
            ..Program::default()
        });
    }

    /// THREE occurrences of the same store in one body (`path` appears
    /// three times): the self-join scheme generalizes past two occurrences
    /// because every occurrence with a changed dependency gets its own
    /// independent delta pass — verified against the naive oracle through
    /// the real compiled pipeline.
    #[test]
    fn differential_three_way_self_join() {
        assert_ra_matches_oracle(&Program {
            rules: vec![
                Rule::plain(
                    "path",
                    vec![tx(), ty()],
                    vec![lit("edge", vec![tx(), ty()], false)],
                ),
                Rule::plain(
                    "path",
                    vec![tx(), Term::Var("W")],
                    vec![
                        lit("path", vec![tx(), ty()], false),
                        lit("path", vec![ty(), tz()], false),
                        lit("path", vec![tz(), Term::Var("W")], false),
                    ],
                ),
            ],
            facts: edge_facts(&[(1, 2), (2, 3), (3, 1), (3, 4), (4, 5)]),
            ..Program::default()
        });
    }

    /// Stratified negation: unreachable vertex pairs, negating a
    /// recursive rule's store (mem_neg join paths) across a stratum
    /// boundary.
    #[test]
    fn differential_stratified_negation() {
        assert_ra_matches_oracle(&Program {
            rules: vec![
                Rule::plain(
                    "vert",
                    vec![tx()],
                    vec![lit("edge", vec![tx(), ty()], false)],
                ),
                Rule::plain(
                    "vert",
                    vec![ty()],
                    vec![lit("edge", vec![tx(), ty()], false)],
                ),
                Rule::plain(
                    "path",
                    vec![tx(), ty()],
                    vec![lit("edge", vec![tx(), ty()], false)],
                ),
                Rule::plain(
                    "path",
                    vec![tx(), ty()],
                    vec![
                        lit("edge", vec![tx(), tz()], false),
                        lit("path", vec![tz(), ty()], false),
                    ],
                ),
                Rule::plain(
                    "unreach",
                    vec![tx(), ty()],
                    vec![
                        lit("vert", vec![tx()], false),
                        lit("vert", vec![ty()], false),
                        lit("path", vec![tx(), ty()], true),
                    ],
                ),
            ],
            facts: edge_facts(&[(1, 2), (2, 3), (4, 4)]),
            ..Program::default()
        });
    }

    /// The self-join shape (a store mentioned TWICE in one body) through a
    /// MEET-aggregation head, RA-BACKED (`compile_magic_rule_body` →
    /// `TempStoreRA`/`incremental_meet_eval`) rather than eval.rs's
    /// hand-rolled model harness (`differential_meet_self_join_many_
    /// multiplicity`) — the review of issue #68's fix flagged that the
    /// model-harness tests can't see bugs in the real compiled scan path
    /// at all (confirmed: mutating `TempStoreRA::iter_batched`'s
    /// `scan_epoch` test made this exact rule shape diverge from the
    /// oracle while the model-harness suite stayed green). `m` appears
    /// twice in the second rule's body — the case that used to collapse
    /// to `ContainedRuleMultiplicity::Many` (a full non-delta re-run every
    /// epoch) and now runs two independent per-occurrence delta passes.
    #[test]
    fn differential_meet_self_join_through_ra() {
        let named = |name: &str| Some((parse_aggr(name).expect("aggr exists"), vec![]));
        let mut facts = edge_facts(&[(1, 2), (2, 3), (3, 1)]);
        facts.insert(
            "seed",
            [(1, 5), (2, 7), (3, 9)]
                .iter()
                .map(|(k, l)| vec![v(*k), v(*l)])
                .collect(),
        );
        assert_ra_matches_oracle(&Program {
            rules: vec![
                Rule::aggregated(
                    "m",
                    vec![tx(), ty()],
                    vec![None, named("min")],
                    vec![lit("seed", vec![tx(), ty()], false)],
                ),
                // m(x, min w) :- m(x, _), m(w', w), edge(w', x): node x
                // adopts any predecessor's value; `m` appears twice.
                Rule::aggregated(
                    "m",
                    vec![tx(), tz()],
                    vec![None, named("min")],
                    vec![
                        lit("m", vec![tx(), ty()], false),
                        lit("m", vec![Term::Var("W"), tz()], false),
                        lit("edge", vec![Term::Var("W"), tx()], false),
                    ],
                ),
            ],
            facts,
            ..Program::default()
        });
    }

    /// Meet aggregation inside recursion: `min` folded epoch by epoch
    /// through the MeetAggrStore, RA-backed.
    #[test]
    fn differential_meet_aggregation_in_recursion() {
        let named = |name: &str| Some((parse_aggr(name).expect("aggr exists"), vec![]));
        let mut facts = edge_facts(&[(1, 2), (2, 3), (3, 1)]);
        facts.insert("seed", [vec![v(1), v(0)]].into_iter().collect());
        assert_ra_matches_oracle(&Program {
            rules: vec![
                Rule::aggregated(
                    "m",
                    vec![tx(), ty()],
                    vec![None, named("min")],
                    vec![lit("seed", vec![tx(), ty()], false)],
                ),
                Rule::aggregated(
                    "m",
                    vec![ty(), tz()],
                    vec![None, named("min")],
                    vec![
                        lit("edge", vec![tx(), ty()], false),
                        lit("m", vec![tx(), tz()], false),
                    ],
                ),
            ],
            facts,
            ..Program::default()
        });
    }

    /// Normal aggregation at a stratum boundary: `count` grouped by the
    /// first column, folded once over the fixpoint beneath.
    #[test]
    fn differential_normal_aggregation() {
        let named = |name: &str| Some((parse_aggr(name).expect("aggr exists"), vec![]));
        assert_ra_matches_oracle(&Program {
            rules: vec![
                Rule::plain(
                    "path",
                    vec![tx(), ty()],
                    vec![lit("edge", vec![tx(), ty()], false)],
                ),
                Rule::plain(
                    "path",
                    vec![tx(), ty()],
                    vec![
                        lit("edge", vec![tx(), tz()], false),
                        lit("path", vec![tz(), ty()], false),
                    ],
                ),
                Rule::aggregated(
                    "outdeg",
                    vec![tx(), ty()],
                    vec![None, named("count")],
                    vec![lit("path", vec![tx(), ty()], false)],
                ),
            ],
            facts: edge_facts(&[(1, 2), (2, 3), (3, 1), (1, 3)]),
            ..Program::default()
        });
    }

    /// Constant arguments in body literals (desugared to unifications, as
    /// the normalize tier does): filter and join paths together.
    #[test]
    fn differential_constant_arguments() {
        assert_ra_matches_oracle(&Program {
            rules: vec![
                Rule::plain(
                    "from_one",
                    vec![ty()],
                    vec![lit("edge", vec![Term::Const(v(1)), ty()], false)],
                ),
                Rule::plain(
                    "hop_from_one",
                    vec![tz()],
                    vec![
                        lit("from_one", vec![ty()], false),
                        lit("edge", vec![ty(), tz()], false),
                    ],
                ),
            ],
            facts: edge_facts(&[(1, 2), (2, 3), (3, 4), (1, 4)]),
            ..Program::default()
        });
    }

    /// `contained_rules` is keyed by OCCURRENCE (position among
    /// `Rule`/`NegatedRule` atoms), not by store name: a positive and a
    /// negated occurrence of the same store get distinct occurrence ids,
    /// and BOTH are entered into the map — this map is also
    /// `StoreLifetimes`'s dependency source (`eval.rs`'s `note_use`), and a
    /// store read only inside a negation is used just as much as one read
    /// positively (dropping it would let its lifetime end before a later
    /// stratum's negation reads it). Only the POSITIVE occurrence is ever
    /// actually selected for delta narrowing in practice — negation always
    /// reads totals, and stratification guarantees a negated dependency's
    /// delta is empty by the time this body runs.
    #[test]
    fn contained_rules_keys_by_occurrence_not_name() {
        let (x, y) = (sym("x"), sym("y"));
        let rule = MagicInlineRule {
            head: vec![x.clone()],
            aggr: vec![None],
            body: vec![
                rule_atom("a", &[x.clone(), y.clone()]),     // occurrence 0
                neg_rule_atom("a", &[y.clone(), x.clone()]), // occurrence 1 (negated)
                rule_atom("b", &[x.clone(), y.clone()]),     // occurrence 2
                rel_atom("edge", &[x, y]),                   // not Rule/NegatedRule: no occurrence
            ],
        };
        let contained = rule.contained_rules();
        assert_eq!(
            contained,
            BTreeMap::from([
                (AtomOccurrence(0), muggle("a")),
                (AtomOccurrence(1), muggle("a")),
                (AtomOccurrence(2), muggle("b")),
            ]),
            "occurrence 1 (the negated `a`) is numbered AND entered — distinct \
             from occurrence 0's positive `a`, but both name store `a`"
        );
    }

    /// The self-join shape (`pt(...), pt(...)` — Andersen's `load`/`store`
    /// rules, issue #68): the SAME store mentioned twice gets TWO
    /// occurrences, each independently delta-selectable — the predecessor
    /// name-keyed scheme collapsed these into one `Many` entry and lost
    /// the ability to narrow either occurrence to a delta at all.
    #[test]
    fn contained_rules_gives_repeated_store_two_independent_occurrences() {
        let (x, y, z) = (sym("x"), sym("y"), sym("z"));
        let rule = MagicInlineRule {
            head: vec![x.clone(), z.clone()],
            aggr: vec![None, None],
            body: vec![
                rule_atom("pt", &[x.clone(), y.clone()]),
                rule_atom("pt", &[y, z]),
            ],
        };
        let contained = rule.contained_rules();
        assert_eq!(
            contained,
            BTreeMap::from([
                (AtomOccurrence(0), muggle("pt")),
                (AtomOccurrence(1), muggle("pt")),
            ]),
            "two occurrences of `pt`, keyed independently — not collapsed to one entry"
        );
    }

    /// NegJoin's join_type surfaces on compiled plans (in-memory rule
    /// negation), completing the strategy-path coverage.
    #[test]
    fn neg_join_type_over_rule_store() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        stored_relation(&db, "edge", 2, &[vec![v(1), v(2)]]);
        let (x, y) = (sym("x"), sym("y"));
        let prog = program_of(vec![
            vec![(
                muggle("r"),
                vec![plain_rule(
                    &[x.clone(), y.clone()],
                    vec![rel_atom("edge", &[x.clone(), y.clone()])],
                )],
            )],
            vec![(
                entry_symbol(),
                vec![plain_rule(
                    &[x.clone(), y.clone()],
                    vec![
                        rel_atom("edge", &[x.clone(), y.clone()]),
                        neg_rule_atom("r", &[x.clone(), y.clone()]),
                    ],
                )],
            )],
        ]);
        let types = compiled_entry_join_types(&db, prog);
        assert!(
            types.contains(&"mem_neg_prefix_join"),
            "expected mem_neg_prefix_join, got {types:?}"
        );
    }

    /// Negation against a RULE store joined on a NON-prefix column — the
    /// set-probe anti-join (mem_neg_mat_join, TempStoreRA::neg_join's
    /// materialized branch). `?[x] := s2[x, y], not s(w, y)` with `w` fresh
    /// joins only on `y` (s's second column), so the probe set is s's
    /// column-1 values and a left row survives iff its `y` is NOT among
    /// them. The oracle's law-4 (fully-bound negated literals) cannot cover
    /// this shape, so this direct query-result test pins the `contains`
    /// sense of the probe: inverting it (`!contains`) yields the complement
    /// rows and fails here.
    #[test]
    fn neg_join_rule_store_non_prefix_set_probe() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        // s2's rows (via an EDB feeder); s's rows (feeder) have column-1
        // value 20 only.
        stored_relation(
            &db,
            "es2",
            2,
            &[vec![v(1), v(10)], vec![v(2), v(20)], vec![v(3), v(20)]],
        );
        stored_relation(&db, "es", 2, &[vec![v(7), v(20)]]);
        let (x, y, w) = (sym("x"), sym("y"), sym("w"));
        let prog = || {
            program_of(vec![
                vec![
                    (
                        muggle("s2"),
                        vec![plain_rule(
                            &[x.clone(), y.clone()],
                            vec![rel_atom("es2", &[x.clone(), y.clone()])],
                        )],
                    ),
                    (
                        muggle("s"),
                        vec![plain_rule(
                            &[w.clone(), y.clone()],
                            vec![rel_atom("es", &[w.clone(), y.clone()])],
                        )],
                    ),
                ],
                vec![(
                    entry_symbol(),
                    vec![plain_rule(
                        std::slice::from_ref(&x),
                        vec![
                            rule_atom("s2", &[x.clone(), y.clone()]),
                            neg_rule_atom("s", &[w.clone(), y.clone()]),
                        ],
                    )],
                )],
            ])
        };
        let types = compiled_entry_join_types(&db, prog());
        assert!(
            types.contains(&"mem_neg_mat_join"),
            "expected mem_neg_mat_join, got {types:?}"
        );
        // s's column-1 values = {20}; keep s2 rows whose y ∉ {20}: only the
        // (1, 10) row → x = 1. (An inverted probe would instead keep 2, 3.)
        assert_eq!(compile_and_run(&db, prog()), rows(&[&[1]]));
    }

    /// A materialized join whose RIGHT side is the recursive store itself:
    /// `r(x, y) :- edge(x, z), r(y, z)` joins `r` on its column 1 (a
    /// non-prefix column) → mem_mat_join with `r` as the right operand, so
    /// the delta of `r` is read through `TempStoreRA::iter`'s full-scan
    /// delta path (delta_all_iter). Emptying that path drops every
    /// recursively-derived-through-the-right fact, so this differential vs
    /// the sealed oracle fails under that mutation.
    #[test]
    fn differential_recursive_right_self_join() {
        let mut facts = edge_facts(&[(1, 2), (2, 3)]);
        facts.insert("base", [vec![v(5), v(2)]].into_iter().collect());
        assert_ra_matches_oracle(&Program {
            rules: vec![
                Rule::plain(
                    "r",
                    vec![tx(), ty()],
                    vec![lit("base", vec![tx(), ty()], false)],
                ),
                Rule::plain(
                    "r",
                    vec![tx(), ty()],
                    vec![
                        lit("edge", vec![tx(), tz()], false),
                        lit("r", vec![ty(), tz()], false),
                    ],
                ),
            ],
            facts,
            ..Program::default()
        });
    }

    // ── truncated stored rows are typed, never a slice panic (law 5) ──────
    //
    // `decode_tuple_from_kv`'s arity is a capacity hint only; a row decoded
    // from a truncated stored value is SHORTER than the relation's arity.
    // The join paths that index disk-decoded rows by position must surface
    // that as a typed error, not an out-of-bounds abort.

    /// Create a `num_keys`-key + `num_vals`-non-key relation and write ONE
    /// deliberately truncated row: a valid key with an EMPTY stored value,
    /// so it decodes to only its key columns (the non-key columns missing) —
    /// a row shorter than the declared arity. Hostile stored bytes, in the
    /// spirit of the storage tier's corruption tests.
    fn relation_with_truncated_row(
        db: &FjallStorage,
        name: &str,
        num_keys: usize,
        num_vals: usize,
        key_vals: &[DataValue],
    ) {
        let keys: Vec<ColumnDef> = (0..num_keys).map(|i| col(&format!("k{i}"))).collect();
        let non_keys: Vec<ColumnDef> = (0..num_vals).map(|i| col(&format!("nk{i}"))).collect();
        let key_bindings = keys.iter().map(|c| sym(&c.name)).collect();
        let dep_bindings = non_keys.iter().map(|c| sym(&c.name)).collect();
        let input = InputRelationHandle {
            name: sym(name),
            metadata: StoredRelationMetadata { keys, non_keys },
            key_bindings,
            dep_bindings,
            span: sp(),
        };
        let mut tx = db.write_tx().expect("write tx");
        let handle = create_relation(&mut tx, input, KeyspaceKind::Facts).expect("create relation");
        // An Assert row with a keys-only tuple: its payload is the empty
        // sequence, so the logical row decodes to `num_keys` columns —
        // `num_vals` short of the arity.
        handle
            .put_fact(
                &mut tx,
                key_vals,
                crate::data::value::ValidityTs::from_raw(0),
                sp(),
            )
            .expect("put truncated row");
        tx.commit().expect("commit");
    }

    /// Point-lookup join over a truncated row: `?[k, w] := *probe[k, w],
    /// *rel[k, w]` binds `rel`'s whole key AND its non-key column, so `rel`
    /// is reached by point lookup and the join then indexes the (missing)
    /// non-key column of the short row. Typed error, not a panic.
    #[test]
    fn point_lookup_join_short_row_is_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        stored_relation(&db, "probe", 2, &[vec![v(1), v(5)]]);
        // rel: one key column `k`, one non-key column `nk`; the stored row
        // for key 1 has no value, so it decodes to length 1.
        relation_with_truncated_row(&db, "rel", 1, 1, &[v(1)]);
        let (k, w) = (sym("k"), sym("w"));
        let prog = program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                &[k.clone(), w.clone()],
                vec![
                    rel_atom("probe", &[k.clone(), w.clone()]),
                    rel_atom("rel", &[k, w]),
                ],
            )],
        )]]);
        let rtx = db.read_tx().unwrap();
        let compiled = stratified_magic_compile(&rtx, prog).expect("compiles");
        let lifetimes = immortal_lifetimes(&compiled);
        let program = bind_for_eval::<_, NoFixedRules>(&compiled, &rtx, Segments::OFF, &mut |_| {
            panic!("no fixed rules")
        })
        .expect("binds");
        let err = stratified_evaluate(
            &program,
            &lifetimes,
            RowLimit::default(),
            &generous_budget(),
            None,
        )
        .unwrap_err();
        assert!(
            err.downcast_ref::<crate::query::ra::StoredRowTooShortError>()
                .is_some(),
            "expected StoredRowTooShortError, got {err:?}"
        );
    }

    /// Stored negation on a key prefix over a truncated row: `?[k, w] :=
    /// *src[k, w], not *blk[k, w]` joins the negated `blk` on `k` and `w`;
    /// the prefix anti-join scans `blk` by key and indexes its (missing)
    /// non-key column of the short row. Typed error, not a panic.
    #[test]
    fn stored_neg_prefix_join_short_row_is_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        stored_relation(&db, "src", 2, &[vec![v(1), v(5)]]);
        relation_with_truncated_row(&db, "blk", 1, 1, &[v(1)]);
        let (k, w) = (sym("k"), sym("w"));
        let prog = program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                &[k.clone(), w.clone()],
                vec![
                    rel_atom("src", &[k.clone(), w.clone()]),
                    neg_rel_atom("blk", &[k, w]),
                ],
            )],
        )]]);
        let rtx = db.read_tx().unwrap();
        let compiled = stratified_magic_compile(&rtx, prog).expect("compiles");
        let lifetimes = immortal_lifetimes(&compiled);
        let program = bind_for_eval::<_, NoFixedRules>(&compiled, &rtx, Segments::OFF, &mut |_| {
            panic!("no fixed rules")
        })
        .expect("binds");
        let err = stratified_evaluate(
            &program,
            &lifetimes,
            RowLimit::default(),
            &generous_budget(),
            None,
        )
        .unwrap_err();
        assert!(
            err.downcast_ref::<crate::query::ra::StoredRowTooShortError>()
                .is_some(),
            "expected StoredRowTooShortError, got {err:?}"
        );
    }

    // ── batched (vectorized) execution: the one machine ────────────────
    //
    // The seven `differential_*` tests above already assert BOTH modes equal
    // the oracle (see `assert_ra_matches_oracle`). These add what a
    // vectorized engine specifically lies about: batch-boundary arithmetic.
    // `BATCH_ROWS` (ra.rs) is 1024; a correct batched scan/filter must be
    // byte-identical to the iterator path at exactly the boundary, one
    // either side, an empty stream, a single row, and a whole rejected
    // batch. An off-by-one in the chunk loop or the filter's survivor count
    // shows up here and nowhere in a round-numbers corpus.

    /// `c1 > k` as a body predicate atom.
    fn pred_gt(col: Symbol, k: i64) -> MagicAtom {
        MagicAtom::Predicate(Expr::Apply {
            op: &crate::data::functions::OP_GT,
            args: Box::new([
                Expr::Binding {
                    var: col,
                    tuple_pos: None,
                },
                Expr::Const {
                    val: v(k),
                    span: sp(),
                },
            ]),
            span: sp(),
        })
    }

    /// `?[c0, c1] := *w[c0, c1], c1 > threshold` — the batched
    /// scan→filter→project pipeline end to end.
    /// Cross-mode differential for BATCHED UNIFICATION (the campaign
    /// generates no unify atoms, so this is its coverage): single and
    /// spread forms across batch boundaries, plus per-row error identity
    /// for a poison row landing mid-stream.
    #[test]
    fn batched_unification_matches_iterator() {
        use crate::data::functions::{OP_ADD, OP_LIST};
        let unify_prog = |multi: bool| -> StratifiedMagicProgram {
            let (c0, c1, w) = (sym("c0"), sym("c1"), sym("w"));
            let expr = if multi {
                Expr::Apply {
                    op: &OP_LIST,
                    args: Box::new([
                        Expr::Binding {
                            var: c0.clone(),
                            tuple_pos: None,
                        },
                        Expr::Binding {
                            var: c1.clone(),
                            tuple_pos: None,
                        },
                    ]),
                    span: sp(),
                }
            } else {
                Expr::Apply {
                    op: &OP_ADD,
                    args: Box::new([
                        Expr::Binding {
                            var: c0.clone(),
                            tuple_pos: None,
                        },
                        Expr::Binding {
                            var: c1.clone(),
                            tuple_pos: None,
                        },
                    ]),
                    span: sp(),
                }
            };
            program_of(vec![vec![(
                entry_symbol(),
                vec![plain_rule(
                    &[c0.clone(), c1.clone(), w.clone()],
                    vec![
                        rel_atom("w", &[c0, c1]),
                        MagicAtom::Unification(Unification {
                            binding: w,
                            expr,
                            one_many_unif: multi,
                            span: sp(),
                        }),
                    ],
                )],
            )]])
        };
        // 2049 rows: straddles the 1024 batch boundary twice.
        for multi in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let db = new_fjall_storage(dir.path()).unwrap();
            let rows: Vec<Tuple> = (0..2049i64).map(|i| vec![v(i), v(i * 3)]).collect();
            stored_relation(&db, "w", 2, &rows);
            let rows_out = compile_and_run_mode_budget(&db, unify_prog(multi), boundary_budget());
            assert_eq!(rows_out.len(), if multi { 2049 * 2 - 1 } else { 2049 });
        }
        // Error identity: a poison row (string in an arithmetic unify)
        // past the first batch boundary errors IDENTICALLY in both modes.
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let mut rows: Vec<Tuple> = (0..1500i64).map(|i| vec![v(i), v(i)]).collect();
        rows[1300][1] = DataValue::from("poison");
        stored_relation(&db, "w", 2, &rows);
        let run = || -> String {
            let rtx = db.read_tx().expect("read tx");
            let compiled = stratified_magic_compile(&rtx, unify_prog_err()).expect("compiles");
            let lifetimes = immortal_lifetimes(&compiled);
            let program =
                bind_for_eval::<_, NoFixedRules>(&compiled, &rtx, Segments::OFF, &mut |_| {
                    panic!("no fixed rules")
                })
                .expect("binds");
            stratified_evaluate(
                &program,
                &lifetimes,
                RowLimit::default(),
                &boundary_budget(),
                None,
            )
            .expect_err("poison row must error")
            .to_string()
        };
        fn unify_prog_err() -> StratifiedMagicProgram {
            use crate::data::functions::OP_ADD;
            let (c0, c1, w) = (sym("c0"), sym("c1"), sym("w"));
            program_of(vec![vec![(
                entry_symbol(),
                vec![plain_rule(
                    &[c0.clone(), w.clone()],
                    vec![
                        rel_atom("w", &[c0.clone(), c1.clone()]),
                        MagicAtom::Unification(Unification {
                            binding: w,
                            expr: Expr::Apply {
                                op: &OP_ADD,
                                args: Box::new([
                                    Expr::Binding {
                                        var: c0,
                                        tuple_pos: None,
                                    },
                                    Expr::Binding {
                                        var: c1,
                                        tuple_pos: None,
                                    },
                                ]),
                                span: sp(),
                            },
                            one_many_unif: false,
                            span: sp(),
                        }),
                    ],
                )],
            )]])
        }
        // One machine: the pin is determinism — two runs of the same
        // program yield the byte-identical refusal.
        assert_eq!(run(), run(), "error identity across runs");
    }

    fn scan_filter_prog(threshold: i64) -> StratifiedMagicProgram {
        let (c0, c1) = (sym("c0"), sym("c1"));
        program_of(vec![vec![(
            entry_symbol(),
            vec![plain_rule(
                &[c0.clone(), c1.clone()],
                vec![rel_atom("w", &[c0, c1.clone()]), pred_gt(c1, threshold)],
            )],
        )]])
    }

    /// Build a fresh fjall `w[c0, c1]` of `n` rows `[i, i]`, run the
    /// scan+filter program on BOTH modes, and assert they are byte-identical
    /// to each other and to the analytic answer (`i > threshold`). `n`
    /// straddles the batch boundary; the surviving count does too.
    fn assert_scan_filter_equiv(n: usize, threshold: i64) {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let rows: Vec<Tuple> = (0..n as i64).map(|i| vec![v(i), v(i)]).collect();
        stored_relation(&db, "w", 2, &rows);

        let batch_rows =
            compile_and_run_mode_budget(&db, scan_filter_prog(threshold), boundary_budget());
        let expected: BTreeSet<Tuple> = (0..n as i64)
            .filter(|&i| i > threshold)
            .map(|i| vec![v(i), v(i)])
            .collect();

        assert_eq!(
            batch_rows, expected,
            "batched scan+filter wrong result at n={n}, threshold={threshold}"
        );
    }

    #[test]
    fn batched_scan_filter_boundary_sizes() {
        // BATCH_ROWS is 1024. Straddle the scan chunk boundary with n, and
        // the *survivor* boundary with the threshold. threshold = -1 keeps
        // all n rows (survivors straddle 1024 with the same n); threshold
        // near n/2 makes the filter reject roughly half.
        for &n in &[0usize, 1, 2, 1023, 1024, 1025, 2047, 2048, 2049, 4096, 4097] {
            // keep-all: survivors = n, exercises the scan chunk boundary
            assert_scan_filter_equiv(n, -1);
            // reject-most: a single survivor from a full batch, then a whole
            // rejected leading batch when n > 1024
            if n > 0 {
                assert_scan_filter_equiv(n, n as i64 - 2);
            }
            // reject-all: empty output through the whole pipeline
            assert_scan_filter_equiv(n, n as i64);
        }
    }

    #[test]
    fn batched_recursion_boundary_sizes() {
        // A chain of n edges builds a `path` rule store of n*(n+1)/2 rows.
        // What must cross BATCH_ROWS=1024 is the STORE the batched scan
        // reads — so sizes are chosen for the store to straddle 1024 (n=44 →
        // 990 rows, n=45 → 1035, n=46 → 1081, n=64 → 2080, n=90 → 4095), not
        // for n itself. That crosses the boundary inside semi-naive
        // recursion (the entry rule projects the >1024-row `path` total)
        // while the derived-tuple spend stays far under the test budget.
        // Iterator ≡ batched at each size.
        for &n in &[1usize, 44, 45, 46, 64, 90] {
            let dir = tempfile::tempdir().unwrap();
            let db = new_fjall_storage(dir.path()).unwrap();
            let edges: Vec<Tuple> = (0..n as i64).map(|i| vec![v(i), v(i + 1)]).collect();
            stored_relation(&db, "edge", 2, &edges);
            let (x, y, z) = (sym("x"), sym("y"), sym("z"));
            let prog = || {
                program_of(vec![
                    vec![(
                        muggle("path"),
                        vec![
                            plain_rule(
                                &[x.clone(), y.clone()],
                                vec![rel_atom("edge", &[x.clone(), y.clone()])],
                            ),
                            plain_rule(
                                &[x.clone(), y.clone()],
                                vec![
                                    rel_atom("edge", &[x.clone(), z.clone()]),
                                    rule_atom("path", &[z.clone(), y.clone()]),
                                ],
                            ),
                        ],
                    )],
                    vec![(
                        entry_symbol(),
                        vec![plain_rule(
                            &[x.clone(), y.clone()],
                            vec![rule_atom("path", &[x.clone(), y.clone()])],
                        )],
                    )],
                ])
            };
            let rows_out = compile_and_run_mode_budget(&db, prog(), boundary_budget());
            // a chain of n edges has n*(n+1)/2 reachable pairs
            assert_eq!(
                rows_out.len(),
                n * (n + 1) / 2,
                "chain TC pair count at n={n}"
            );
        }
    }

    /// A tiny deterministic LCG — a seeded random-graph campaign without a
    /// proptest harness, so it runs in the always-on suite under caps.
    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *state >> 16
    }

    #[test]
    fn batched_random_program_campaign() {
        // 120 seeded random small graphs, each run through the transitive
        // closure program on BOTH modes and the oracle. Iterator ≡ batched ≡
        // oracle for every one. This is the mini-campaign the vectorization
        // ascent's mutation test sabotages.
        for seed in 0u64..120 {
            let mut st = seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(1);
            let n_verts = 3 + (lcg(&mut st) % 8) as i64; // 3..10 vertices
            let n_edges = 2 + (lcg(&mut st) % 14) as usize; // 2..15 edges
            let mut edge_set: BTreeSet<(i64, i64)> = BTreeSet::new();
            for _ in 0..n_edges {
                let a = (lcg(&mut st) as i64) % n_verts;
                let b = (lcg(&mut st) as i64) % n_verts;
                edge_set.insert((a, b));
            }
            let model = Program {
                rules: vec![
                    Rule::plain(
                        "path",
                        vec![tx(), ty()],
                        vec![lit("edge", vec![tx(), ty()], false)],
                    ),
                    Rule::plain(
                        "path",
                        vec![tx(), ty()],
                        vec![
                            lit("edge", vec![tx(), tz()], false),
                            lit("path", vec![tz(), ty()], false),
                        ],
                    ),
                ],
                facts: {
                    let mut f: BTreeMap<Rel, BTreeSet<Tuple>> = Default::default();
                    f.insert(
                        "edge",
                        edge_set.iter().map(|(a, b)| vec![v(*a), v(*b)]).collect(),
                    );
                    f
                },
                ..Program::default()
            };
            // assert_ra_matches_oracle runs BOTH modes vs the oracle.
            assert_ra_matches_oracle(&model);
        }
    }

    /// The seam contract is stronger than set equality: `for_each_derivation`
    /// Drives the entry rule's `CompiledRuleBody` directly and checks the
    /// survivor COUNT against the analytic answer at batch boundaries.
    /// (The row-vs-batch order comparison died with the iterator machine;
    /// ordering itself is pinned by the byte-identity trials.)
    #[test]
    fn batched_stream_survivor_count_is_analytic() {
        for &(n, threshold) in &[
            (1023usize, -1i64),
            (1024, -1),
            (1025, -1),
            (2049, 1024),
            (2049, -1),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let db = new_fjall_storage(dir.path()).unwrap();
            let rows: Vec<Tuple> = (0..n as i64).map(|i| vec![v(i), v(i)]).collect();
            stored_relation(&db, "w", 2, &rows);

            let rtx = db.read_tx().expect("read tx");
            let compiled =
                stratified_magic_compile(&rtx, scan_filter_prog(threshold)).expect("compiles");
            let entry = compiled
                .iter()
                .flat_map(|stratum| stratum.values())
                .find_map(|rs| match rs {
                    CompiledRuleSet::Rules(rules) => Some(&rules.rules[0]),
                    CompiledRuleSet::Fixed(_) => None,
                })
                .expect("an inline rule");

            let stores: BTreeMap<MagicSymbol, EpochStore> = BTreeMap::new();
            let body = CompiledRuleBody {
                plan: entry,
                tx: &rtx,
                segments: Segments::OFF,
            };
            let mut seen: Vec<Tuple> = Vec::new();
            body.for_each_derivation(&stores, None, false, &mut |t, _| {
                seen.push(t.into_owned());
                Ok(ControlFlow::Continue(()))
            })
            .expect("derives");
            let survivors = (threshold.max(-1) + 1..n as i64).count();
            assert_eq!(seen.len(), survivors, "survivor count at n={n}");
        }
    }
}
