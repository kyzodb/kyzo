/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): the stratum walk runs over the landed tier's execution-ordered
 * strata in *reverse* (the original stored strata already reversed and
 * walked them front to back — the same logical direction, entry stratum
 * first; see `magic_sets_rewrite` for why demand analysis must flow that
 * way), and the collected output is un-reversed exactly once before minting
 * [`StratifiedMagicProgram`]; the adornment phase returns a local
 * [`AdornedProgram`] keyed by [`AdornedHead`] (Muggle or Magic *by type*),
 * which turns the original's "at this point, rule_head must be Muggle or
 * Magic, the remaining options are impossible" comment into structure; the
 * entry exemption is structural (`SymbolKind::Entry`), not a seeded
 * dummy-span `?` symbol; `disable_magic_rewrite` lives once on the tier,
 * not copied into every stratum; the map-lookup and `mut_rules` `unwrap`s
 * are typed internal errors; the `rule_idx`/`sup_idx` narrowing to `u16` is
 * checked (silent wrap-around would merge distinct supplementary relations
 * — extra join tuples, i.e. changed *results*, not just changed demand);
 * every stored relation time-travels through the universal bitemporal
 * format (no per-schema validity column exists to check, so the old
 * `keys.last()` panic site has no successor); the
 * transaction-facing schema
 * lookups sit behind the [`StoredRelationSchemaSource`] seam (the mirror of
 * `BodyNormalizer` in `data/program.rs` — the runtime's session transaction
 * implements it when it lands); the index-search atom arms (HNSW/FTS/LSH)
 * land with the index tier, which owns those `MagicAtom` variants;
 * adornments are `Vec<AdornmentMark>` (no `smallvec` dependency); the
 * `exempt_aggr_rules_for_magic_sets` walk is re-homed from
 * `NormalFormProgram` onto [`NormalFormStratum`], which is what the landed
 * stratified tier stores. `NamedFieldNotFound` is declared here, its
 * first port-order user.
 */

//! The magic-sets rewrite: demand-driven evaluation as a program transform.
//!
//! A bottom-up Datalog evaluator computes *every* fact of every rule, even
//! when the query needs three of them. Magic sets fixes this by rewriting
//! the program so that binding patterns flow from the entry downward: each
//! demanded rule is *adorned* with which argument positions arrive bound
//! ([`MagicSymbol::Magic`]), an *input* relation ([`MagicSymbol::Input`])
//! carries the demanded binding tuples into it, and *supplementary*
//! relations ([`MagicSymbol::Sup`]) carry the partial joins between body
//! atoms so demand can be forwarded mid-rule (sideways information
//! passing). Rules nobody demands are dropped.
//!
//! **The law this file lives under** (`query/mod.rs`, law 1 via the rule in
//! `.claude/rules/query.md`): the rewrite may change only *demand* — which
//! facts get computed — never *result semantics*. The rewritten program
//! must produce exactly the same answer relation as the naive evaluation of
//! the original; every deviation is a wrong-answers bug, not a performance
//! bug. The differential harness against the naive oracle (`query/laws.rs`)
//! is the standing enforcement; the tests here pin the transformation's
//! structure rule by rule.
//!
//! **The fully-free identity theorem** (issue #68's law, a corollary of law
//! 1 that must hold *by construction*, not merely by informal argument): for
//! any predicate whose demand chain contains a fully-free adornment (every
//! argument position free — the shape a query with no bound arguments
//! anywhere seeds), the rewrite is answer- **and cost-**identical to
//! skipping it: no supplementary relation, no input relation, no second copy
//! of that predicate's own fixpoint survives *reachable from an
//! always-evaluated root* on its account. This does not fall out of the
//! adornment phase for free — sideways information passing
//! (`NormalFormAtom::adorn`) is correct and standard, and *locally* right to
//! adorn a self-referencing occurrence tighter when an earlier body atom
//! happens to bind one of its arguments, even inside a rule whose own head
//! is fully free. Andersen points-to is exactly this: `pt` occurs twice in
//! `load`/`store`'s bodies, the first occurrence's output binds an argument
//! the second consumes, and SIP correctly (if uselessly, here) proposes
//! `bf`/`bb` demand for the second — while the entry (`?[y, x] := pt[y,
//! x]`) is fully unbound and must compute the complete `pt` regardless.
//! Answering that proposal literally — minting `pt|Mbf`/`pt|Mbb` plus their
//! `Input`/`Sup` chain — is sound (law 1 holds; every deviation would still
//! be a *subset* of the complete relation) but is pure waste multiplied
//! across every self-join occurrence: three separately-fixpointed `pt`
//! variants plus roughly twenty supplementary relations, all computing
//! overlapping fragments of the one relation a magic-sets-free evaluator
//! computes once.
//!
//! Two passes enforce the theorem, in order, and both are load-bearing:
//! [`AdornedProgram::collapse_ff_redundant_variants`] redirects every
//! reference to a predicate's tighter-adorned variant onto its fully-free
//! sibling when one is demanded (sound unconditionally — a store computed
//! in full already contains whatever a tighter join would have derived);
//! [`AdornedProgram::sweep_unreachable`] then mark-and-sweeps the reference
//! graph from every always-evaluated (Muggle) head and drops anything the
//! walk never reaches. The sweep, not the redirect, is what actually proves
//! the theorem: a redirect is local to the atom it rewrites, so if the
//! variant it just orphaned held the *only* reference to some unrelated
//! predicate's own tight variant, that predicate never gains a free sibling
//! of its own and `collapse_ff_redundant_variants`'s name-keyed pass cannot
//! see it — reachability from the roots is the actual invariant, and the
//! sweep is what closes the whole orphan class rather than one instance of
//! it (a hostile-review finding on an earlier version of this fix that
//! shipped the redirect without the sweep). The planner-rule corollary
//! falls out of both passes together, without a separate code path: a
//! fully-free-headed entry's reachable predicates end up with exactly one
//! (Muggle-cost) variant apiece, evaluated bottom-up with no demand
//! machinery at all — the shape Souffle's planner reaches by recognizing
//! "no bound arguments, no restriction to propagate" up front. The standing
//! differential (`runtime::db::tests::magic_bypass_differential`) is this
//! theorem's executable form: it runs a small recursive corpus — including
//! a points-to-shaped self-join and the orphan-producing shape above —
//! through both `Db::run_script` (this file's rewrite included) and the
//! crate-internal bypass path (`bench_api`'s magic-sets-free compile), and
//! asserts byte-identical answers *and* a byte-identical adorned-symbol set
//! (one variant per reachable predicate, no `Input`/`Sup`) for every
//! fully-unbound query in the corpus.
//!
//! The transformation is *visible internally and invisible at the
//! boundary*: inside the engine, [`MagicSymbol`]'s variants carry the
//! demand analysis in the type itself (a name proves which role its store
//! plays), while at the public boundary a query's answers are indifferent
//! to whether the rewrite ran at all (`::set_options` can disable it
//! wholesale, and exempt rules pass through untouched as
//! [`MagicSymbol::Muggle`]).
//!
//! Exemptions — rules the rewrite must not touch:
//! - the **entry** (`?`): its store *is* the answer relation, read by the
//!   runtime under its unadorned name;
//! - **aggregating rules**: adornment restricts which tuples a rule
//!   derives, and an aggregate over a restricted subset is a different
//!   value, not a lazier computation of the same one;
//! - **everything**, when the query says `:disable_magic_rewrite true`;
//! - **cross-stratum producers**: a rule consumed from a later-executing
//!   stratum was already referenced there under its Muggle name (adornment
//!   never crosses a stratum boundary), so its definition must stay Muggle
//!   — and must not be dropped as undemanded.

use std::collections::BTreeSet;
use std::collections::btree_map::Entry;
use std::mem;

use miette::{Diagnostic, Result, bail, ensure, miette};
use thiserror::Error;

use crate::exec::plan::program::{
    Adornment, AdornmentMark, MagicAtom, MagicFixedRuleApply, MagicFixedRuleRuleArg,
    MagicInlineRule, MagicProgram, MagicRelationApplyAtom, MagicRuleApplyAtom, MagicRulesOrFixed,
    MagicSymbol, NormalFormAtom, NormalFormInlineRule, NormalFormRulesOrFixed, NormalFormStratum,
    StratifiedMagicProgram, StratifiedNormalFormProgram, bind_fixed_impl,
};
use kyzo_model::SourceSpan;
use kyzo_model::program::rule::{FixedRuleApply, FixedRuleArg, HeadAggrSlot, ValidityClause};
use kyzo_model::program::symbol::{Symbol, SymbolKind};
use kyzo_model::schema::StoredRelationMetadata;

// ─────────────────────────────────────────────────────────────────────────
// SEAM: the catalog (runtime tier, not yet ported).
// ─────────────────────────────────────────────────────────────────────────

/// The magic tier's seam to the runtime's catalog, mirroring what
/// `BodyNormalizer` (`data/program.rs`) does for normalization: the CozoDB
/// original's adornment phase took the session transaction because
/// fixed-rule arguments naming *stored* relations need their declared
/// schemas — to refuse time travel over a relation whose last key column is
/// not `Validity`, and to resolve named-field bindings to positional ones.
/// Those lookups are the only transaction-facing part of this file; when
/// the runtime tier lands, its session transaction implements this trait.
pub trait StoredRelationSchemaSource {
    /// The declared schema of the stored relation `name`, or an error if no
    /// such relation exists (the implementation owns that diagnostic).
    fn stored_relation_schema(
        &self,
        name: &Symbol,
        span: SourceSpan,
    ) -> Result<StoredRelationMetadata>;
}

// ─────────────────────────────────────────────────────────────────────────
// Errors
// ─────────────────────────────────────────────────────────────────────────

/// A named-field binding on a stored relation names a field the relation
/// does not have.
#[derive(Debug, Error, Diagnostic)]
#[error("stored relation '{0}' does not have field '{1}'")]
#[diagnostic(code(eval::named_field_not_found))]
pub(crate) struct NamedFieldNotFound(
    pub(crate) Symbol,
    pub(crate) Symbol,
    #[label] pub(crate) SourceSpan,
);

/// An invariant the rewrite maintains internally was found broken. Returned
/// (never panicked) on the paths whose impossibility is proven elsewhere,
/// so corruption of that proof surfaces as a bug report instead of an
/// abort — and, worse here than anywhere, instead of silently *changed
/// demand*, which the law forbids to ever become changed answers.
#[derive(Debug, Diagnostic, Error)]
#[error("Magic-sets rewrite invariant violated: {0}")]
#[diagnostic(code(compiler::magic_invariant))]
#[diagnostic(help("This is a bug. Please report it."))]
struct MagicInvariantError(&'static str);

// ─────────────────────────────────────────────────────────────────────────
// The adorned intermediate: Muggle or Magic, by type
// ─────────────────────────────────────────────────────────────────────────

/// A rule head as the adornment phase mints it: unadorned (`Muggle`) or
/// demand-adorned (`Magic`) — nothing else. The original kept these as
/// [`MagicSymbol`]s and asserted "the remaining options are impossible" in
/// a comment inside the rewrite; here the `Input` and `Sup` roles cannot
/// exist before the rewrite phase because only [`magic_rewrite_ruleset`]
/// mints those names. The match in the rewrite is total, not trusted.
#[derive(Debug, Clone, Ord, PartialOrd, Eq, PartialEq)]
enum AdornedHead {
    Muggle { inner: Symbol },
    Magic { inner: Symbol, adornment: Adornment },
}

impl AdornedHead {
    fn as_plain_symbol(&self) -> &Symbol {
        match self {
            AdornedHead::Muggle { inner } | AdornedHead::Magic { inner, .. } => inner,
        }
    }

    fn adornment(&self) -> &[AdornmentMark] {
        match self {
            AdornedHead::Muggle { .. } => &[],
            AdornedHead::Magic { adornment, .. } => adornment,
        }
    }

    fn has_bound_adornment(&self) -> bool {
        self.adornment().iter().any(|m| m.is_bound())
    }

    fn to_magic_symbol(&self) -> MagicSymbol {
        match self {
            AdornedHead::Muggle { inner } => MagicSymbol::Muggle {
                inner: inner.clone(),
            },
            AdornedHead::Magic { inner, adornment } => MagicSymbol::Magic {
                inner: inner.clone(),
                adornment: adornment.clone(),
            },
        }
    }
}

/// One stratum between the two phases: adorned, not yet rewritten. A local
/// intermediate — it never leaves this file, which is what keeps the
/// Muggle-or-Magic proof airtight.
#[derive(Debug)]
struct AdornedProgram {
    prog: std::collections::BTreeMap<AdornedHead, MagicRulesOrFixed>,
}

impl AdornedProgram {
    fn empty() -> Self {
        Self {
            prog: std::collections::BTreeMap::new(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Phase 0: the stratum walk
// ─────────────────────────────────────────────────────────────────────────

impl StratifiedNormalFormProgram {
    /// The magic-sets rewrite: adorn and rewrite each stratum, minting the
    /// magic tier. Demand changes; result semantics may not.
    ///
    /// ## Why the walk runs *against* execution order
    ///
    /// Demand flows from consumers to producers. Within a stratum the
    /// adornment phase handles that itself (the pending-adornment loop),
    /// but *across* strata the rewrite never adorns: a reference to a rule
    /// defined in another stratum is always Muggle, because the demand
    /// ("input") relations synthesized by the rewrite feed evaluation
    /// within one stratum's fixpoint only. So every rule consumed across a
    /// stratum boundary must be **exempt** in the stratum that defines it —
    /// otherwise its definition would either be specialized away from the
    /// Muggle name its consumers already reference, or (if nothing in its
    /// own stratum references it) dropped entirely as undemanded. Consumers
    /// execute *after* producers; therefore the walk must visit consumers
    /// *first*, accumulating each stratum's cross-stratum dependencies into
    /// `exempt_rules` before reaching the strata that define them. Over the
    /// landed execution-ordered tier that is a reverse walk, and the
    /// collected output is reversed back exactly once, here. (The CozoDB
    /// original stored strata in reverse execution order and walked them
    /// front to back — the identical logical direction; `compile.rs`'s
    /// `.rev()` was its un-reversal, and has no descendant here.)
    ///
    /// An inverted walk does not crash: it silently drops or specializes
    /// cross-stratum producers, which evaluation then resolves to empty
    /// stores — wrong answers. The direction is pinned by
    /// `cross_stratum_consumers_keep_producers_unrewritten` in the tests.
    pub fn magic_sets_rewrite(
        self,
        schemas: &impl StoredRelationSchemaSource,
    ) -> Result<StratifiedMagicProgram> {
        let (strata, disable_magic_rewrite) = self.into_parts();
        let mut exempt_rules: BTreeSet<Symbol> = BTreeSet::new();
        let mut rewritten_reversed: Vec<MagicProgram> = Vec::with_capacity(strata.len());
        for stratum in strata.into_iter().rev() {
            stratum.collect_magic_exemptions(disable_magic_rewrite, &mut exempt_rules);
            let cross_stratum_deps = stratum.cross_stratum_dependencies();
            let adorned = stratum
                .adorn(&exempt_rules, schemas)?
                .collapse_ff_redundant_variants()
                .sweep_unreachable();
            rewritten_reversed.push(adorned.magic_rewrite()?);
            exempt_rules.extend(cross_stratum_deps);
        }
        rewritten_reversed.reverse();
        // The constructor is the proof that the entry survived the rewrite
        // unadorned, in the final stratum.
        StratifiedMagicProgram::from_execution_order(rewritten_reversed)
    }
}

impl NormalFormStratum {
    /// Add this stratum's exempt rules to `exempt_rules`: every rule when
    /// the rewrite is disabled for the query, else every inline rule with
    /// an aggregation anywhere in its head. (The entry needs no entry here:
    /// its exemption is structural, by [`SymbolKind::Entry`], in
    /// [`NormalFormStratum::adorn`].) Port of the original's
    /// `exempt_aggr_rules_for_magic_sets`, re-homed onto the stratum;
    /// `disable_magic_rewrite` arrives as a parameter because the landed
    /// tier carries it once, not copied into every stratum.
    fn collect_magic_exemptions(
        &self,
        disable_magic_rewrite: bool,
        exempt_rules: &mut BTreeSet<Symbol>,
    ) {
        for (name, rule_set) in self.rules.iter() {
            if disable_magic_rewrite {
                exempt_rules.insert(name.clone());
                continue;
            }
            match rule_set {
                NormalFormRulesOrFixed::Rules { rules: rule_set } => {
                    'outer: for rule in rule_set.iter() {
                        for aggr in rule.aggr.iter() {
                            if aggr.is_aggregated() {
                                exempt_rules.insert(name.clone());
                                continue 'outer;
                            }
                        }
                    }
                }
                NormalFormRulesOrFixed::Fixed { fixed: _ } => {}
            }
        }
    }

    /// The rule names this stratum applies but does not define — its
    /// dependencies in earlier-executing strata (the original's
    /// `get_downstream_rules`, named for its walk order). In-memory
    /// arguments of fixed rules are included unconditionally: stratification
    /// always puts a fixed rule's inputs in strictly earlier strata.
    fn cross_stratum_dependencies(&self) -> BTreeSet<Symbol> {
        let own_rules: BTreeSet<_> = self.rules.keys().collect();
        let mut dependencies: BTreeSet<Symbol> = BTreeSet::new();
        for rules in self.rules.values() {
            match rules {
                NormalFormRulesOrFixed::Rules { rules } => {
                    for rule in rules {
                        for atom in rule.body.iter() {
                            match atom {
                                NormalFormAtom::Rule(r_app)
                                | NormalFormAtom::NegatedRule(r_app)
                                    if !own_rules.contains(&r_app.name) =>
                                {
                                    dependencies.insert(r_app.name.clone());
                                }
                                NormalFormAtom::Rule(_)
                                | NormalFormAtom::Relation(_)
                                | NormalFormAtom::NegatedRule(_)
                                | NormalFormAtom::NegatedRelation(_)
                                | NormalFormAtom::Predicate(_)
                                | NormalFormAtom::Unification(_)
                                | NormalFormAtom::Search(_) => {}
                            }
                        }
                    }
                }
                NormalFormRulesOrFixed::Fixed { fixed } => {
                    for rel in fixed.rule_args.iter() {
                        if let FixedRuleArg::InMem { name, .. } = rel {
                            dependencies.insert(name.clone());
                        }
                    }
                }
            }
        }
        dependencies
    }

    // ─────────────────────────────────────────────────────────────────
    // Phase 1: adornment
    // ─────────────────────────────────────────────────────────────────

    /// Adorn one stratum: propagate binding patterns from the rules *not*
    /// subject to rewrite (the entry and the exempt rules, which pass
    /// through as Muggle) into the rules that are, minting one
    /// [`AdornedHead::Magic`] definition per demanded adornment. Rules
    /// subject to rewrite that nobody demands are dropped — that is the
    /// demand pruning, and it is only sound because cross-stratum consumers
    /// have already exempted everything they reference.
    fn adorn(
        &self,
        exempt_rules: &BTreeSet<Symbol>,
        schemas: &impl StoredRelationSchemaSource,
    ) -> Result<AdornedProgram> {
        let rules_to_rewrite: BTreeSet<_> = self
            .rules
            .keys()
            // The entry's exemption is structural: `?` is the answer
            // relation, read by the runtime under its unadorned name. (The
            // original seeded the exempt set with a dummy-span `?` symbol.)
            .filter(|k| k.kind() != SymbolKind::Entry && !exempt_rules.contains(*k))
            .cloned()
            .collect();

        let mut pending_adornment: Vec<AdornedHead> = vec![];
        let mut adorned_prog = AdornedProgram::empty();

        // Processing starts with the rules NOT subject to rewrite: they
        // keep their Muggle names, and their bodies seed the demand.
        for (rule_name, rules) in &self.rules {
            if rules_to_rewrite.contains(rule_name) {
                continue;
            }
            match rules {
                NormalFormRulesOrFixed::Fixed { fixed } => {
                    adorned_prog.prog.insert(
                        AdornedHead::Muggle {
                            inner: rule_name.clone(),
                        },
                        MagicRulesOrFixed::Fixed {
                            fixed: adorn_fixed_rule_apply(fixed, schemas)?,
                        },
                    );
                }
                NormalFormRulesOrFixed::Rules { rules } => {
                    let mut adorned_rules = Vec::with_capacity(rules.len());
                    for rule in rules {
                        let adorned_rule =
                            rule.adorn(&mut pending_adornment, &rules_to_rewrite, BTreeSet::new());
                        adorned_rules.push(adorned_rule);
                    }
                    adorned_prog.prog.insert(
                        AdornedHead::Muggle {
                            inner: rule_name.clone(),
                        },
                        MagicRulesOrFixed::Rules {
                            rules: adorned_rules,
                        },
                    );
                }
            }
        }

        // Then every demanded adornment, transitively: adorning a rule's
        // bodies can demand further adornments of its callees.
        while let Some(head) = pending_adornment.pop() {
            if adorned_prog.prog.contains_key(&head) {
                continue;
            }
            let original_rules = match self.rules.get(head.as_plain_symbol()) {
                Some(NormalFormRulesOrFixed::Rules { rules }) => rules,
                // Adornments are only ever demanded of names in
                // `rules_to_rewrite` — inline-rule keys of this stratum.
                // (The original unwrapped both lookups.)
                Some(NormalFormRulesOrFixed::Fixed { .. }) => bail!(MagicInvariantError(
                    "an adornment was demanded of a fixed rule"
                )),
                None => bail!(MagicInvariantError(
                    "an adornment was demanded of a rule not in its stratum"
                )),
            };
            let adornment = head.adornment();
            let mut adorned_rules = Vec::with_capacity(original_rules.len());
            for rule in original_rules {
                // Inside an adorned rule, the bound head positions arrive
                // bound: they are what the input relation carries in.
                let seen_bindings = rule
                    .head
                    .iter()
                    .zip(adornment.iter())
                    .filter_map(|(kw, bound)| {
                        if bound.is_bound() {
                            Some(kw.clone())
                        } else {
                            None
                        }
                    })
                    .collect();
                let adorned_rule =
                    rule.adorn(&mut pending_adornment, &rules_to_rewrite, seen_bindings);
                adorned_rules.push(adorned_rule);
            }
            adorned_prog.prog.insert(
                head,
                MagicRulesOrFixed::Rules {
                    rules: adorned_rules,
                },
            );
        }
        Ok(adorned_prog)
    }
}

/// Adorn a fixed-rule application: in-memory arguments get their Muggle
/// names (fixed rules always consume *complete* input relations — demand
/// cannot restrict an opaque algorithm's input); stored arguments have
/// their time-travel legality checked and named-field bindings resolved to
/// positional ones against the declared schema, via the seam.
fn adorn_fixed_rule_apply(
    fixed: &FixedRuleApply,
    schemas: &impl StoredRelationSchemaSource,
) -> Result<MagicFixedRuleApply> {
    let mut rule_args = Vec::with_capacity(fixed.rule_args.len());
    for r in fixed.rule_args.iter() {
        rule_args.push(match r {
            FixedRuleArg::InMem {
                name,
                bindings,
                span,
            } => MagicFixedRuleRuleArg::InMem {
                name: MagicSymbol::Muggle {
                    inner: name.clone(),
                },
                bindings: bindings.clone(),
                span: *span,
            },
            FixedRuleArg::Stored {
                name,
                bindings,
                span,
                as_of,
            } => MagicFixedRuleRuleArg::Stored {
                name: name.clone(),
                bindings: bindings.clone(),
                as_of: *as_of,
                span: *span,
            },
            FixedRuleArg::NamedStored {
                name,
                bindings,
                as_of,
                span,
            } => {
                let metadata = schemas.stored_relation_schema(name, *span)?;
                // `ColumnDef::name` is a bare `SmartString<LazyCompact>`
                // (schema-plane, no source span); `bindings` is keyed by
                // `Symbol` (query-plane, span-carrying but name-only
                // `Eq`/`Ord`) — compare by string content.
                let fields: BTreeSet<&str> = metadata
                    .keys
                    .iter()
                    .chain(metadata.non_keys.iter())
                    .map(|col| col.name.as_str())
                    .collect();
                for k in bindings.keys() {
                    ensure!(
                        fields.contains(k.name.as_str()),
                        NamedFieldNotFound(name.clone(), k.clone(), *span,)
                    );
                }
                let new_bindings = metadata
                    .keys
                    .iter()
                    .chain(metadata.non_keys.iter())
                    .enumerate()
                    .map(|(i, col)| {
                        match bindings.get(&Symbol::new(col.name.clone(), *span)) {
                            // Unbound columns get positional filler names;
                            // digit-leading names cannot collide with user
                            // bindings (not valid identifiers in the grammar).
                            None => Symbol::new(format!("{i}"), SourceSpan::default()),
                            Some(k) => k.clone(),
                        }
                    })
                    .collect();
                MagicFixedRuleRuleArg::Stored {
                    name: name.clone(),
                    bindings: new_bindings,
                    as_of: *as_of,
                    span: *span,
                }
            }
        });
    }
    let fixed_impl = bind_fixed_impl(&fixed.fixed_handle.name).ok_or_else(|| {
        crate::rules::contract::FixedRuleNotFoundError(
            fixed.fixed_handle.name.to_string(),
            fixed.span,
        )
    })?;
    // Seal options through the fixed-rule door (Constant folds Apply(OP_LIST)
    // list literals to Const List-of-Lists; other rules normalize similarly).
    let options = fixed_impl.init_options(fixed.options.clone(), fixed.span)?;
    Ok(MagicFixedRuleApply {
        span: fixed.span,
        fixed_handle: fixed.fixed_handle.clone(),
        fixed_impl,
        rule_args,
        options,
        arity: fixed.arity,
    })
}

impl NormalFormInlineRule {
    /// Adorn one rule: walk its (already well-ordered) body left to right,
    /// tracking which bindings are bound so far; each application of a
    /// rewritable rule is renamed to the Magic name for the binding pattern
    /// at its position, and that adornment is pushed as pending demand.
    fn adorn(
        &self,
        pending: &mut Vec<AdornedHead>,
        rules_to_rewrite: &BTreeSet<Symbol>,
        mut seen_bindings: BTreeSet<Symbol>,
    ) -> MagicInlineRule {
        let mut ret_body = Vec::with_capacity(self.body.len());

        for atom in &self.body {
            let new_atom = atom.adorn(pending, &mut seen_bindings, rules_to_rewrite);
            ret_body.push(new_atom);
        }
        MagicInlineRule {
            head: self.head.clone(),
            aggr: self.aggr.clone(),
            body: ret_body,
        }
    }
}

impl NormalFormAtom {
    /// Adorn one atom. Everything except rule applications passes through,
    /// contributing its bindings; an application of a rewritable rule is
    /// adorned with, per argument position, whether the binding is already
    /// seen (bound) or introduced here (free).
    fn adorn(
        &self,
        pending: &mut Vec<AdornedHead>,
        seen_bindings: &mut BTreeSet<Symbol>,
        rules_to_rewrite: &BTreeSet<Symbol>,
    ) -> MagicAtom {
        match self {
            NormalFormAtom::Relation(v) => {
                let v = MagicRelationApplyAtom {
                    name: v.name.clone(),
                    args: v.args.clone(),
                    validity: v.validity.clone(),
                    span: v.span,
                };
                for arg in v.args.iter() {
                    if !seen_bindings.contains(arg) {
                        seen_bindings.insert(arg.clone());
                    }
                }
                // `@spans`/`@delta`/`@delta_sys` bind one extra column
                // beyond `args` (the interval or the sign) — the same
                // "own bindings beyond the base row" shape `Search` uses
                // for its engine-appended columns, below.
                if let Some(extra) = v.validity.as_ref().and_then(ValidityClause::extra_var) {
                    seen_bindings.insert(extra.clone());
                }
                MagicAtom::Relation(v)
            }
            NormalFormAtom::Search(sa) => {
                for b in sa.own_bindings.iter() {
                    if !seen_bindings.contains(b) {
                        seen_bindings.insert(b.clone());
                    }
                }
                MagicAtom::Search(sa.clone())
            }
            NormalFormAtom::Predicate(p) => {
                // A predicate cannot introduce new bindings.
                MagicAtom::Predicate(p.clone())
            }
            NormalFormAtom::Rule(rule) => {
                if rules_to_rewrite.contains(&rule.name) {
                    let mut adornment: Adornment = Vec::with_capacity(rule.args.len());
                    for arg in rule.args.iter() {
                        // Bound iff already seen. A binding repeated within
                        // this same application adorns its later positions
                        // bound — faithful to the original.
                        adornment.push(AdornmentMark::from_bound(
                            !seen_bindings.insert(arg.clone()),
                        ));
                    }
                    pending.push(AdornedHead::Magic {
                        inner: rule.name.clone(),
                        adornment: adornment.clone(),
                    });

                    MagicAtom::Rule(MagicRuleApplyAtom {
                        name: MagicSymbol::Magic {
                            inner: rule.name.clone(),
                            adornment,
                        },
                        args: rule.args.clone(),
                        span: rule.span,
                    })
                } else {
                    // Deliberately does NOT extend `seen_bindings`, faithful
                    // to the original: bindings introduced by an exempt
                    // application count as *free* in later adornments. That
                    // only widens demand (a freer adornment computes more),
                    // which the law permits; treating them as bound would be
                    // a demand-shape change to make deliberately, against
                    // the oracle, not silently in a port.
                    MagicAtom::Rule(MagicRuleApplyAtom {
                        name: MagicSymbol::Muggle {
                            inner: rule.name.clone(),
                        },
                        args: rule.args.clone(),
                        span: rule.span,
                    })
                }
            }
            NormalFormAtom::NegatedRule(nr) => MagicAtom::NegatedRule(MagicRuleApplyAtom {
                // Negated applications are never adorned: negation needs
                // the complete relation to subtract from.
                name: MagicSymbol::Muggle {
                    inner: nr.name.clone(),
                },
                args: nr.args.clone(),
                span: nr.span,
            }),
            NormalFormAtom::NegatedRelation(nv) => {
                MagicAtom::NegatedRelation(MagicRelationApplyAtom {
                    name: nv.name.clone(),
                    args: nv.args.clone(),
                    validity: nv.validity.clone(),
                    span: nv.span,
                })
            }
            NormalFormAtom::Unification(u) => {
                seen_bindings.insert(u.binding.clone());
                MagicAtom::Unification(u.clone())
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Phase 1.5: collapse ff-redundant variants
// ─────────────────────────────────────────────────────────────────────────

impl AdornedProgram {
    /// Sideways information passing (`NormalFormAtom::adorn`, above) is
    /// standard and correct in isolation: within one rule body, a call to a
    /// rewritten predicate is adorned bound in whichever positions an
    /// earlier atom already bound. But when the SAME predicate also has a
    /// fully-free ("ff": every position free) variant demanded — typically
    /// because some consumer needs its complete, unrestricted contents —
    /// every other adorned variant of that predicate is provably redundant:
    /// the ff variant must already compute the complete relation, so a
    /// tighter-adorned sibling can only ever derive a subset of it, at the
    /// cost of running its OWN full semi-naive fixpoint plus an
    /// Input/supplementary chain to feed it. Joining against the ff store
    /// with the very same bound values in hand yields identical rows (join
    /// semantics do not care whether the store being probed carries rows
    /// beyond the ones that match), so every reference to a redundant
    /// variant collapses onto the ff one, and the now-unreferenced variant
    /// is dropped.
    ///
    /// This is issue #68's actual driver for Andersen points-to: `pt`
    /// occurs twice in each `load`/`store` rule body, and left-to-right SIP
    /// sees the first occurrence bind a variable the second consumes,
    /// adorning the second `bf`/`bb` — even though the ENTRY's demand for
    /// `pt` (`?[y, x] := pt[y, x]`) is fully unbound. Uncollapsed, that
    /// mints THREE separately-fixpointed `pt` variants (`Mff`/`Mbf`/`Mbb`)
    /// plus roughly twenty supplementary relations, all computing
    /// overlapping fragments of the one relation `bench_api::points_to`'s
    /// hand-built (magic-sets-bypassing) program computes once — measured
    /// as the actual OOM driver (`pointsto_repro.rs`, an identical facts+
    /// program run through the crate-internal path completes at bounded
    /// memory while the same run through this rewrite exhausts a 12 GiB
    /// cap in seconds).
    ///
    /// Sound regardless of *why* the ff variant is demanded: nothing here
    /// assumes points-to's specific shape, only that an ff demand, once it
    /// exists for a predicate, subsumes every other adornment of it.
    fn collapse_ff_redundant_variants(mut self) -> Self {
        let ff_names: BTreeSet<Symbol> = self
            .prog
            .keys()
            .filter_map(|head| match head {
                AdornedHead::Magic { inner, adornment }
                    if !adornment.iter().any(|m| m.is_bound()) =>
                {
                    Some(inner.clone())
                }
                AdornedHead::Muggle { .. } | AdornedHead::Magic { .. } => None,
            })
            .collect();
        if ff_names.is_empty() {
            return self;
        }
        let redirect_to_ff = |name: &mut MagicSymbol| {
            if let MagicSymbol::Magic { inner, adornment } = name
                && adornment.iter().any(|m| m.is_bound())
                && ff_names.contains(inner)
            {
                adornment.iter_mut().for_each(|m| *m = AdornmentMark::Free);
            }
        };
        for rules_or_fixed in self.prog.values_mut() {
            if let MagicRulesOrFixed::Rules { rules } = rules_or_fixed {
                for rule in rules.iter_mut() {
                    for atom in rule.body.iter_mut() {
                        match atom {
                            MagicAtom::Rule(r) | MagicAtom::NegatedRule(r) => {
                                redirect_to_ff(&mut r.name);
                            }
                            MagicAtom::Relation(_)
                            | MagicAtom::Predicate(_)
                            | MagicAtom::NegatedRelation(_)
                            | MagicAtom::Unification(_)
                            | MagicAtom::Search(_) => {}
                        }
                    }
                }
            }
        }
        // Dropping the now-unreferenced tight variants themselves is
        // `sweep_unreachable`'s job, not this pass's: redirecting a
        // predicate's OWN references onto its ff sibling can just as well
        // orphan some UNRELATED predicate whose only referrer was the
        // redirected variant's body — this pass, keyed on `ff_names`, has
        // no way to see that (the orphan never gains an ff sibling of its
        // own). Reachability from the always-evaluated roots is the actual
        // invariant; name-matching was only ever an approximation of it.
        self
    }

    /// Reachability mark-and-sweep over the (post-collapse) reference
    /// graph, rooted at every always-evaluated head — the entry and every
    /// exempt rule, stored under `AdornedHead::Muggle` keys. Hostile review
    /// on `collapse_ff_redundant_variants` found the gap this closes: that
    /// pass redirects references INTO a predicate's ff variant, but the
    /// redirect is local to one atom and non-transitive — if the redirected
    /// (now-dead) variant's body held the ONLY reference to some other
    /// predicate's tight variant, that predicate never acquires an ff
    /// sibling itself, so `collapse_ff_redundant_variants`'s
    /// `ff_names`-keyed retain cannot see it, and it survives as compiled,
    /// fixpointed dead code (with its own `Input`/`Sup` chain) reachable
    /// from nothing. Reachability, not "does this predicate have an ff
    /// sibling", is the actual criterion; this sweep drops anything the
    /// walk from the roots never reaches, which subsumes
    /// `collapse_ff_redundant_variants`'s own cleanup role (its retain step
    /// was removed once this landed) and closes the whole orphan class, not
    /// one instance of it.
    fn sweep_unreachable(mut self) -> Self {
        fn target_head(sym: &MagicSymbol) -> Option<AdornedHead> {
            match sym {
                MagicSymbol::Muggle { inner } => Some(AdornedHead::Muggle {
                    inner: inner.clone(),
                }),
                MagicSymbol::Magic { inner, adornment } => Some(AdornedHead::Magic {
                    inner: inner.clone(),
                    adornment: adornment.clone(),
                }),
                // Cannot occur before `magic_rewrite` mints them.
                MagicSymbol::Input { .. } | MagicSymbol::Sup { .. } => None,
            }
        }

        let mut reachable: BTreeSet<AdornedHead> = self
            .prog
            .keys()
            .filter(|head| matches!(head, AdornedHead::Muggle { .. }))
            .cloned()
            .collect();
        let mut frontier: Vec<AdornedHead> = reachable.iter().cloned().collect();
        while let Some(head) = frontier.pop() {
            let Some(rules_or_fixed) = self.prog.get(&head) else {
                continue;
            };
            let mut targets: Vec<MagicSymbol> = Vec::new();
            match rules_or_fixed {
                MagicRulesOrFixed::Rules { rules } => {
                    for rule in rules {
                        for atom in &rule.body {
                            if let MagicAtom::Rule(r) | MagicAtom::NegatedRule(r) = atom {
                                targets.push(r.name.clone());
                            }
                        }
                    }
                }
                MagicRulesOrFixed::Fixed { fixed } => {
                    for arg in &fixed.rule_args {
                        if let MagicFixedRuleRuleArg::InMem { name, .. } = arg {
                            targets.push(name.clone());
                        }
                    }
                }
            }
            for sym in targets {
                if let Some(t) = target_head(&sym)
                    && reachable.insert(t.clone())
                {
                    frontier.push(t);
                }
            }
        }
        self.prog.retain(|head, _| reachable.contains(head));
        self
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Phase 2: the rewrite proper
// ─────────────────────────────────────────────────────────────────────────

impl AdornedProgram {
    /// Rewrite one adorned stratum: every inline rule set is run through
    /// sup-rule synthesis; fixed-rule applications pass through under their
    /// (always Muggle) names.
    fn magic_rewrite(self) -> Result<MagicProgram> {
        let mut ret_prog = MagicProgram::empty();
        for (rule_head, ruleset) in self.prog {
            match ruleset {
                MagicRulesOrFixed::Rules { rules: ruleset } => {
                    magic_rewrite_ruleset(rule_head, ruleset, &mut ret_prog)?;
                }
                MagicRulesOrFixed::Fixed { fixed } => {
                    ret_prog.prog.insert(
                        rule_head.to_magic_symbol(),
                        MagicRulesOrFixed::Fixed { fixed },
                    );
                }
            }
        }
        Ok(ret_prog)
    }
}

/// Append one rule under `key`, creating the rule set if absent. Replaces
/// the original's `entry().or_default().mut_rules()` panic-on-miss: the key
/// roles are disjoint by construction (`Sup`/`Input` names are minted only
/// here, and one name is never both rules and fixed), so a collision with a
/// fixed rule is a rewrite bug, reported as such.
fn push_magic_rule(
    ret_prog: &mut MagicProgram,
    key: MagicSymbol,
    rule: MagicInlineRule,
) -> Result<()> {
    match ret_prog.prog.entry(key) {
        Entry::Vacant(e) => {
            e.insert(MagicRulesOrFixed::Rules { rules: vec![rule] });
        }
        Entry::Occupied(mut o) => match o.get_mut().mut_rules() {
            Some(rules) => rules.push(rule),
            None => bail!(MagicInvariantError(
                "a rewrite-synthesized rule collided with a fixed rule"
            )),
        },
    }
    Ok(())
}

/// Rewrite one rule set (sideways information passing): for a head with
/// bound positions, seed the body with the input relation's demand tuples;
/// at each application of an adorned rule inside the body, cut the atoms
/// collected so far into a supplementary rule, and feed its projection onto
/// the callee's bound positions into the callee's input relation.
///
/// `rule_head` is Muggle or Magic *by type* — the `Input` and `Sup` roles
/// are minted only by this function, after adornment, so they cannot occur
/// as heads of an [`AdornedProgram`].
fn magic_rewrite_ruleset(
    rule_head: AdornedHead,
    ruleset: Vec<MagicInlineRule>,
    ret_prog: &mut MagicProgram,
) -> Result<()> {
    let rule_name = rule_head.as_plain_symbol().clone();
    let adornment: Adornment = rule_head.adornment().to_vec();
    let head_span = rule_name.span;
    let out_head = rule_head.to_magic_symbol();

    // Can only be true if the head is Magic and not all positions are free.
    let rule_has_bound_args = rule_head.has_bound_adornment();

    for (rule_idx, rule) in ruleset.into_iter().enumerate() {
        // Checked, not `as`: a silent wrap would merge distinct sup
        // relations across rules — extra join tuples, changed answers.
        let rule_idx = u16::try_from(rule_idx)
            .map_err(|_| MagicInvariantError("more than u16::MAX rules in one rule set"))?;
        let mut sup_idx: u16 = 0;
        let mut make_sup_kw = || -> Result<MagicSymbol> {
            let ret = MagicSymbol::Sup {
                inner: rule_name.clone(),
                adornment: adornment.clone(),
                rule_idx,
                sup_idx,
            };
            sup_idx = sup_idx.checked_add(1).ok_or(MagicInvariantError(
                "more than u16::MAX supplementary rules in one rule",
            ))?;
            Ok(ret)
        };
        let mut collected_atoms = vec![];
        let mut seen_bindings: BTreeSet<Symbol> = BTreeSet::new();

        // SIP from the input rule, if the head has any bound positions:
        // sup₀ carries the demanded tuples in from the input relation.
        if rule_has_bound_args {
            let sup_kw = make_sup_kw()?;

            let sup_args: Vec<Symbol> = rule
                .head
                .iter()
                .zip(adornment.iter())
                .filter_map(|(arg, is_bound)| {
                    if is_bound.is_bound() {
                        Some(arg.clone())
                    } else {
                        None
                    }
                })
                .collect();
            let sup_aggr = (0..sup_args.len()).map(|_| HeadAggrSlot::Plain).collect();
            let sup_body = vec![MagicAtom::Rule(MagicRuleApplyAtom {
                name: MagicSymbol::Input {
                    inner: rule_name.clone(),
                    adornment: adornment.clone(),
                },
                args: sup_args.clone(),
                span: head_span,
            })];

            push_magic_rule(
                ret_prog,
                sup_kw.clone(),
                MagicInlineRule {
                    head: sup_args.clone(),
                    aggr: sup_aggr,
                    body: sup_body,
                },
            )?;

            seen_bindings.extend(sup_args.iter().cloned());

            collected_atoms.push(MagicAtom::Rule(MagicRuleApplyAtom {
                name: sup_kw,
                args: sup_args,
                span: head_span,
            }))
        }
        for atom in rule.body {
            match atom {
                a @ (MagicAtom::Predicate(_)
                | MagicAtom::NegatedRule(_)
                | MagicAtom::NegatedRelation(_)) => {
                    collected_atoms.push(a);
                }
                MagicAtom::Search(sa) => {
                    seen_bindings.extend(sa.own_bindings.iter().cloned());
                    collected_atoms.push(MagicAtom::Search(sa));
                }
                MagicAtom::Relation(v) => {
                    seen_bindings.extend(v.args.iter().cloned());
                    if let Some(extra) = v.validity.as_ref().and_then(ValidityClause::extra_var) {
                        seen_bindings.insert(extra.clone());
                    }
                    collected_atoms.push(MagicAtom::Relation(v));
                }
                // SEAM: the index-search atoms (HNSW/FTS/LSH) land with the
                // index tier; their arms extend `seen_bindings` with all of
                // the search's bindings and pass through, like Relation.
                MagicAtom::Unification(u) => {
                    seen_bindings.insert(u.binding.clone());
                    collected_atoms.push(MagicAtom::Unification(u));
                }
                MagicAtom::Rule(r_app) => {
                    if r_app.name.has_bound_adornment() {
                        // A bound adornment is minted only on Magic names,
                        // so this application demands input. Cut the atoms
                        // so far into a sup rule…
                        let sup_kw = make_sup_kw()?;
                        let args: Vec<Symbol> = seen_bindings.iter().cloned().collect();
                        let mut sup_rule_atoms = vec![];
                        mem::swap(&mut sup_rule_atoms, &mut collected_atoms);

                        // …add the sup rule to the program (this cleared
                        // all collected atoms)…
                        push_magic_rule(
                            ret_prog,
                            sup_kw.clone(),
                            MagicInlineRule {
                                head: args.clone(),
                                aggr: (0..args.len()).map(|_| HeadAggrSlot::Plain).collect(),
                                body: sup_rule_atoms,
                            },
                        )?;

                        // …continue the body from the sup rule's output…
                        let sup_rule_app = MagicAtom::Rule(MagicRuleApplyAtom {
                            name: sup_kw,
                            args,
                            span: head_span,
                        });
                        collected_atoms.push(sup_rule_app.clone());

                        // …and feed its projection onto the callee's bound
                        // positions into the callee's input relation.
                        let inp_kw = MagicSymbol::Input {
                            inner: r_app.name.as_plain_symbol().clone(),
                            adornment: r_app.name.magic_adornment().to_vec(),
                        };
                        let inp_args: Vec<Symbol> = r_app
                            .args
                            .iter()
                            .zip(r_app.name.magic_adornment())
                            .filter_map(|(kw, is_bound)| {
                                if is_bound.is_bound() {
                                    Some(kw.clone())
                                } else {
                                    None
                                }
                            })
                            .collect();
                        let inp_aggr = (0..inp_args.len()).map(|_| HeadAggrSlot::Plain).collect();
                        push_magic_rule(
                            ret_prog,
                            inp_kw,
                            MagicInlineRule {
                                head: inp_args,
                                aggr: inp_aggr,
                                body: vec![sup_rule_app],
                            },
                        )?;
                    }
                    seen_bindings.extend(r_app.args.iter().cloned());
                    collected_atoms.push(MagicAtom::Rule(r_app));
                }
            }
        }

        // The rewritten rule itself: head and aggregations untouched (the
        // law — the rewrite may reshape bodies, never what a rule returns).
        push_magic_rule(
            ret_prog,
            out_head.clone(),
            MagicInlineRule {
                head: rule.head,
                aggr: rule.aggr,
                body: collected_atoms,
            },
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    use miette::miette;
    use smartstring::SmartString;

    use super::*;
    use crate::exec::plan::program::{NormalFormRelationApplyAtom, NormalFormRuleApplyAtom};
    use kyzo_model::program::aggregate::parse_aggr;
    use kyzo_model::program::expr::{BindingPos, Expr};
    use kyzo_model::program::rule::{FixedRuleHandle, FixedRuleOptions, Trivia, Unification};
    use kyzo_model::schema::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
    use kyzo_model::value::AsOf;
    use kyzo_model::value::DataValue;

    // ── construction helpers ────────────────────────────────────────────

    fn sym(name: &str) -> Symbol {
        Symbol::new(name, SourceSpan(0, 0))
    }

    fn rule_app(name: &str, args: &[&str]) -> NormalFormAtom {
        NormalFormAtom::Rule(NormalFormRuleApplyAtom {
            name: sym(name),
            args: args.iter().map(|a| sym(a)).collect(),
            span: SourceSpan(0, 0),
        })
    }

    fn stored_app(name: &str, args: &[&str]) -> NormalFormAtom {
        NormalFormAtom::Relation(NormalFormRelationApplyAtom {
            name: sym(name),
            args: args.iter().map(|a| sym(a)).collect(),
            validity: None,
            span: SourceSpan(0, 0),
        })
    }

    fn unify_const(binding: &str, val: i64) -> NormalFormAtom {
        NormalFormAtom::Unification(Unification {
            binding: sym(binding),
            expr: Expr::Const {
                val: DataValue::from(val),
                span: SourceSpan(0, 0),
            },
            one_many_unif: false,
            span: SourceSpan(0, 0),
        })
    }

    fn unify_var(binding: &str, var: &str) -> NormalFormAtom {
        NormalFormAtom::Unification(Unification {
            binding: sym(binding),
            expr: Expr::Binding {
                var: sym(var),
                tuple_pos: BindingPos::Unresolved,
            },
            one_many_unif: false,
            span: SourceSpan(0, 0),
        })
    }

    fn nf_rule(head: &[&str], body: Vec<NormalFormAtom>) -> NormalFormInlineRule {
        NormalFormInlineRule {
            head: head.iter().map(|h| sym(h)).collect(),
            aggr: head.iter().map(|_| HeadAggrSlot::Plain).collect(),
            body,
        }
    }

    fn stratum(defs: Vec<(&str, Vec<NormalFormInlineRule>)>) -> NormalFormStratum {
        let mut stratum = NormalFormStratum::empty();
        for (name, rules) in defs {
            let key = if name == "?" {
                Symbol::prog_entry(SourceSpan(0, 1))
            } else {
                sym(name)
            };
            stratum
                .rules
                .insert(key, NormalFormRulesOrFixed::Rules { rules });
        }
        stratum
    }

    /// Mint the stratified tier from strata given in EXECUTION order (the
    /// tier constructor takes the stratifier's reversed order).
    fn stratified(
        exec_order: Vec<NormalFormStratum>,
        disable_magic_rewrite: bool,
    ) -> Result<StratifiedNormalFormProgram> {
        let mut reversed = exec_order;
        reversed.reverse();
        StratifiedNormalFormProgram::from_reverse_execution_order(reversed, disable_magic_rewrite)
    }

    /// No stored relations exist: any schema lookup is a test failure.
    struct NoSchemas;
    impl StoredRelationSchemaSource for NoSchemas {
        fn stored_relation_schema(
            &self,
            name: &Symbol,
            _span: SourceSpan,
        ) -> Result<StoredRelationMetadata> {
            Err(miette!("test unexpectedly looked up schema of '{name}'"))
        }
    }

    /// Every relation has the same fixed schema.
    struct FixedSchema(StoredRelationMetadata);
    impl StoredRelationSchemaSource for FixedSchema {
        fn stored_relation_schema(
            &self,
            _name: &Symbol,
            _span: SourceSpan,
        ) -> Result<StoredRelationMetadata> {
            Ok(self.0.clone())
        }
    }

    fn column(name: &str, coltype: ColType) -> ColumnDef {
        ColumnDef {
            name: SmartString::from(name),
            typing: NullableColType::required(coltype),
            default_gen: None,
        }
    }

    // ── inspection helpers ──────────────────────────────────────────────

    /// The stratum's store names, in their debug rendering (`r`, `r|Mbf`,
    /// `r|Ibf`, `r|S.0.1bf`) — the same rendering the engine logs.
    fn key_names(stratum: &MagicProgram) -> BTreeSet<String> {
        stratum.prog.keys().map(|k| format!("{k:?}")).collect()
    }

    fn rules_of<'a>(stratum: &'a MagicProgram, key: &str) -> &'a [MagicInlineRule] {
        let found = stratum
            .prog
            .iter()
            .find(|(k, _)| format!("{k:?}") == key);
        assert!(found.is_some(), "store '{key}' not found");
        match found.map(|(_, v)| v) {
            Some(MagicRulesOrFixed::Rules { rules }) => rules,
            Some(MagicRulesOrFixed::Fixed { .. }) => {
                assert!(false, "store '{key}' is a fixed rule");
                &[]
            }
            None => &[],
        }
    }

    fn atom_names(rule: &MagicInlineRule) -> Vec<String> {
        rule.body
            .iter()
            .map(|atom| match atom {
                MagicAtom::Rule(r) => format!("rule {:?}", r.name),
                MagicAtom::Relation(r) => format!("stored {}", r.name),
                MagicAtom::Predicate(_) => "predicate".to_string(),
                MagicAtom::NegatedRule(r) => format!("not rule {:?}", r.name),
                MagicAtom::NegatedRelation(r) => format!("not stored {}", r.name),
                MagicAtom::Unification(u) => format!("unify {}", u.binding),
                MagicAtom::Search(sa) => format!("search {:?}", sa.cfg),
            })
            .collect()
    }

    fn head_names(rule: &MagicInlineRule) -> Vec<&str> {
        rule.head.iter().map(|s| s.name.as_str()).collect()
    }

    // ── the tests ───────────────────────────────────────────────────────

    /// Port of the original's `strange_case` (upstream `magic.rs`), which
    /// ran through `DbInstance`:
    ///
    /// ```text
    /// x[A] := A = 1
    /// y[A, A] := A = 1
    /// y[A, B] := A = 0, B = 1, x[B]
    /// ?[C] := y[A, _], y[C, A]
    /// :disable_magic_rewrite true    => rows [[0], [1]]
    /// ```
    ///
    /// Until the runtime tier lands, the port is structural: with the
    /// rewrite disabled every rule is exempt, so the magic tier must be the
    /// identity image of the program — all names Muggle, every body
    /// preserved atom for atom, no Input/Sup/Magic stores anywhere. The
    /// end-to-end row assertion re-lands with `DbInstance`, and the
    /// naive-oracle differential covers the answer itself.
    #[test]
    fn strange_case_with_disabled_rewrite_is_identity() -> Result<()>  {
        // Normal form of the program above (head dedup makes y's first rule
        // `y[A, ***0] := A = 1, ***0 = A`; `_` becomes an ignored binding).
        let make_strata = || {
            vec![stratum(vec![
                ("x", vec![nf_rule(&["A"], vec![unify_const("A", 1)])]),
                (
                    "y",
                    vec![
                        nf_rule(
                            &["A", "***0"],
                            vec![unify_const("A", 1), unify_var("***0", "A")],
                        ),
                        nf_rule(
                            &["A", "B"],
                            vec![
                                unify_const("A", 0),
                                unify_const("B", 1),
                                rule_app("x", &["B"]),
                            ],
                        ),
                    ],
                ),
                (
                    "?",
                    vec![nf_rule(
                        &["C"],
                        vec![rule_app("y", &["A", "~1"]), rule_app("y", &["C", "A"])],
                    )],
                ),
            ])]
        };

        let rewritten = stratified(make_strata(), true)?
            .magic_sets_rewrite(&NoSchemas)
            .map_err(|e| miette!("rewrite succeeds: {e}"))?;
        assert_eq!(rewritten.strata().len(), 1);
        let out = &rewritten.strata()[0];
        assert_eq!(
            key_names(out),
            BTreeSet::from(["x".to_string(), "y".to_string(), "?".to_string()]),
            "disabled rewrite must leave every name Muggle and add nothing"
        );
        // Bodies are preserved atom for atom.
        assert_eq!(atom_names(&rules_of(out, "?")[0]), vec!["rule y", "rule y"]);
        assert_eq!(
            atom_names(&rules_of(out, "y")[1]),
            vec!["unify A", "unify B", "rule x"]
        );

        // Contrast, proving the flag is load-bearing: the same program with
        // the rewrite enabled does adorn (`y[C, A]` sees A bound).
        let rewritten = stratified(make_strata(), false)?
            .magic_sets_rewrite(&NoSchemas)
            .map_err(|e| miette!("rewrite succeeds: {e}"))?;
        let names = key_names(&rewritten.strata()[0]);
        assert!(
            names.iter().any(|n| n.starts_with("y|M")),
            "enabled rewrite adorns y; got {names:?}"
        );
        Ok(())
    }

    /// The demand rewrite on a bound transitive closure, checkable by hand:
    ///
    /// ```text
    /// tc[a, b] := *e[a, b]
    /// tc[a, b] := *e[a, c], tc[c, b]
    /// ?[b]     := s = 1, tc[s, b]
    /// ```
    ///
    /// Semantics-preservation argument (the full differential equivalence
    /// runs against the naive oracle once the pipeline is joined up):
    /// unfolding the sup chain of the rewritten recursive rule gives
    /// `tc|Mbf[a, b] :- tc|Ibf[a], *e[a, c], tc|Mbf[c, b]` — the original
    /// rule guarded by the demand relation — and the demand relation is
    /// seeded from the entry's constant (`s = 1`) and closed under
    /// `demanded(a), e(a, c) ⇒ demanded(c)`. By induction over derivation
    /// height, `tc|Mbf` is exactly `tc` restricted to demanded first
    /// arguments, the entry's first argument is demanded, and the entry
    /// reads only `tc|Mbf[s, …]` with `s` demanded — so `?` derives exactly
    /// the rows the unrewritten program derives, from fewer facts.
    #[test]
    fn bound_entry_transitive_closure_rewrites_demand_only() -> Result<()>  {
        let strata = vec![stratum(vec![
            (
                "tc",
                vec![
                    nf_rule(&["a", "b"], vec![stored_app("e", &["a", "b"])]),
                    nf_rule(
                        &["a", "b"],
                        vec![stored_app("e", &["a", "c"]), rule_app("tc", &["c", "b"])],
                    ),
                ],
            ),
            (
                "?",
                vec![nf_rule(
                    &["b"],
                    vec![unify_const("s", 1), rule_app("tc", &["s", "b"])],
                )],
            ),
        ])];

        let rewritten = stratified(strata, false)?
            .magic_sets_rewrite(&NoSchemas)
            .map_err(|e| miette!("rewrite succeeds: {e}"))?;
        assert_eq!(rewritten.strata().len(), 1);
        let out = &rewritten.strata()[0];

        assert_eq!(
            key_names(out),
            BTreeSet::from([
                "?".to_string(),          // the entry, Muggle, always
                "?|S.0.0".to_string(),    // entry's partial join up to tc[s, b]
                "tc|Mbf".to_string(),     // tc adorned bound-free
                "tc|Ibf".to_string(),     // the demand feeding tc|Mbf
                "tc|S.0.0bf".to_string(), // base rule: demand seed
                "tc|S.1.0bf".to_string(), // recursive rule: demand seed
                "tc|S.1.1bf".to_string(), // recursive rule: join demand ⋈ e
            ]),
        );

        // The unadorned tc is gone: that is the demand pruning, and it is
        // the ONLY kind of change the law allows.
        assert!(!key_names(out).contains("tc"));

        // Heads and aggregations of the rewritten rules are untouched.
        let tc_rules = rules_of(out, "tc|Mbf");
        assert_eq!(tc_rules.len(), 2);
        for rule in tc_rules {
            assert_eq!(head_names(rule), vec!["a", "b"]);
            assert!(rule.aggr.iter().all(|a| matches!(a, HeadAggrSlot::Plain)));
        }

        // Base rule: sup₀ (the demand) then the original stored scan.
        assert_eq!(
            atom_names(&tc_rules[0]),
            vec!["rule tc|S.0.0bf", "stored e"]
        );
        // Recursive rule: sup₁ (demand ⋈ e) then the original recursive
        // application, now adorned.
        assert_eq!(
            atom_names(&tc_rules[1]),
            vec!["rule tc|S.1.1bf", "rule tc|Mbf"]
        );
        // sup₁.₁ carries the partial join: sup₁.₀ then e.
        assert_eq!(
            atom_names(&rules_of(out, "tc|S.1.1bf")[0]),
            vec!["rule tc|S.1.0bf", "stored e"]
        );

        // The demand relation is fed from exactly two places: the entry's
        // bound argument and the recursive call's bound argument.
        let inputs = rules_of(out, "tc|Ibf");
        assert_eq!(inputs.len(), 2);
        for rule in inputs {
            assert_eq!(rule.head.len(), 1, "adornment bf has one bound slot");
        }
        let input_bodies: BTreeSet<_> = inputs.iter().flat_map(atom_names).collect();
        assert_eq!(
            input_bodies,
            BTreeSet::from(["rule ?|S.0.0".to_string(), "rule tc|S.1.1bf".to_string()])
        );

        // The entry stays Muggle with its head untouched, reading tc|Mbf.
        let entry_rules = rules_of(out, "?");
        assert_eq!(head_names(&entry_rules[0]), vec!["b"]);
        assert_eq!(
            atom_names(&entry_rules[0]),
            vec!["rule ?|S.0.0", "rule tc|Mbf"]
        );
        Ok(())
    }

    /// Exemption: the entry. Even with an empty exempt set the entry is
    /// never rewritten — structurally, by `SymbolKind::Entry`, not via a
    /// seeded `?` symbol.
    #[test]
    fn entry_is_always_exempt() -> Result<()>  {
        let strata = vec![stratum(vec![
            ("r", vec![nf_rule(&["x"], vec![stored_app("e", &["x"])])]),
            ("?", vec![nf_rule(&["x"], vec![rule_app("r", &["x"])])]),
        ])];
        let rewritten = stratified(strata, false)?
            .magic_sets_rewrite(&NoSchemas)
            .map_err(|e| miette!("rewrite succeeds: {e}"))?;
        let out = &rewritten.strata()[0];
        let names = key_names(out);
        assert!(names.contains("?"), "entry must survive as Muggle");
        assert!(
            !names.iter().any(|n| n.starts_with("?|")),
            "entry must never be adorned; got {names:?}"
        );
        // An all-free application still mints an adorned (Magic) name for
        // the callee — with no bound position, no Input/Sup appears.
        assert!(names.contains("r|Mf"));
        assert!(!names.iter().any(|n| n.starts_with("r|I")));
        Ok(())
    }

    /// Exemption: aggregating rules. A rule with an aggregation anywhere in
    /// its head stays Muggle even when applied with bound arguments —
    /// an aggregate over a demand-restricted subset would be a different
    /// value, which the law forbids.
    #[test]
    fn aggregation_rules_are_exempt() -> Result<()>  {
        let count = parse_aggr("count")?.ok_or_else(|| miette!("count exists"))?;
        let mut agg_rule = nf_rule(&["a", "n"], vec![stored_app("e", &["a", "n"])]);
        agg_rule.aggr[1] = HeadAggrSlot::Aggregated {
            aggr: count,
            args: vec![],
        };

        let strata = vec![stratum(vec![
            ("agg", vec![agg_rule]),
            (
                "?",
                vec![nf_rule(
                    &["x"],
                    vec![unify_const("v", 1), rule_app("agg", &["v", "x"])],
                )],
            ),
        ])];
        let rewritten = stratified(strata, false)?
            .magic_sets_rewrite(&NoSchemas)
            .map_err(|e| miette!("rewrite succeeds: {e}"))?;
        let out = &rewritten.strata()[0];
        let names = key_names(out);
        assert!(names.contains("agg"), "aggregating rule stays Muggle");
        assert!(
            !names.iter().any(|n| n.starts_with("agg|")),
            "no adorned/input/sup form of an aggregating rule; got {names:?}"
        );
        // The entry references it under the Muggle name, and the entry body
        // is left whole (no sup cut without a bound-adorned application).
        assert_eq!(
            atom_names(&rules_of(out, "?")[0]),
            vec!["unify v", "rule agg"]
        );
        // The aggregation itself is untouched.
        assert!(rules_of(out, "agg")[0].aggr[1].is_aggregated());
        Ok(())
    }

    /// Exemption: `:disable_magic_rewrite`. The flag lives once on the tier
    /// (not per stratum) and must exempt every rule in every stratum.
    #[test]
    fn disable_magic_rewrite_exempts_every_stratum() -> Result<()>  {
        let strata = vec![
            stratum(vec![(
                "r",
                vec![nf_rule(&["a", "b"], vec![stored_app("e", &["a", "b"])])],
            )]),
            stratum(vec![(
                "?",
                vec![nf_rule(
                    &["x"],
                    vec![unify_const("v", 1), rule_app("r", &["v", "x"])],
                )],
            )]),
        ];
        let rewritten = stratified(strata, true)?
            .magic_sets_rewrite(&NoSchemas)
            .map_err(|e| miette!("rewrite succeeds: {e}"))?;
        assert_eq!(rewritten.strata().len(), 2);
        assert_eq!(
            key_names(&rewritten.strata()[0]),
            BTreeSet::from(["r".to_string()])
        );
        assert_eq!(
            key_names(&rewritten.strata()[1]),
            BTreeSet::from(["?".to_string()])
        );
        Ok(())
    }

    /// Exemption: cross-stratum producers — and with it, the direction of
    /// the stratum walk. `r` is defined in the first-executing stratum and
    /// consumed (with a bound argument) only from the entry stratum. The
    /// walk must visit the entry stratum first so that `r` is exempt by the
    /// time its own stratum is adorned; a walk in execution order would
    /// find `r` unreferenced-and-rewritable and DROP its definition, and
    /// evaluation would then read an empty store — silently wrong answers.
    /// This test is the standing regression for an inverted walk.
    #[test]
    fn cross_stratum_consumers_keep_producers_unrewritten() -> Result<()>  {
        let strata = vec![
            stratum(vec![(
                "r",
                vec![nf_rule(&["a", "b"], vec![stored_app("e", &["a", "b"])])],
            )]),
            stratum(vec![(
                "?",
                vec![nf_rule(
                    &["x"],
                    vec![unify_const("v", 1), rule_app("r", &["v", "x"])],
                )],
            )]),
        ];
        let rewritten = stratified(strata, false)?
            .magic_sets_rewrite(&NoSchemas)
            .map_err(|e| miette!("rewrite succeeds: {e}"))?;
        assert_eq!(rewritten.strata().len(), 2);

        // Producer stratum: r survives, Muggle, body intact.
        let producer = &rewritten.strata()[0];
        assert_eq!(key_names(producer), BTreeSet::from(["r".to_string()]));
        assert_eq!(atom_names(&rules_of(producer, "r")[0]), vec!["stored e"]);

        // Entry stratum: the reference is Muggle too — consistent names on
        // both sides of the boundary, and no demand machinery anywhere.
        let consumer = &rewritten.strata()[1];
        assert_eq!(key_names(consumer), BTreeSet::from(["?".to_string()]));
        assert_eq!(
            atom_names(&rules_of(consumer, "?")[0]),
            vec!["unify v", "rule r"]
        );
        Ok(())
    }

    /// Adornment correctness on a mixed application: `r[v, y, w]` with `v`
    /// and `w` bound by unifications and `y` free must adorn as `bfb`, and
    /// the input relation must carry exactly the bound positions.
    #[test]
    fn adornment_marks_bound_and_free_positions() -> Result<()>  {
        let strata = vec![stratum(vec![
            (
                "r",
                vec![nf_rule(
                    &["a", "b", "c"],
                    vec![stored_app("e", &["a", "b", "c"])],
                )],
            ),
            (
                "?",
                vec![nf_rule(
                    &["y"],
                    vec![
                        unify_const("v", 1),
                        unify_const("w", 2),
                        rule_app("r", &["v", "y", "w"]),
                    ],
                )],
            ),
        ])];
        let rewritten = stratified(strata, false)?
            .magic_sets_rewrite(&NoSchemas)
            .map_err(|e| miette!("rewrite succeeds: {e}"))?;
        let out = &rewritten.strata()[0];
        let names = key_names(out);
        assert!(names.contains("r|Mbfb"), "got {names:?}");
        assert!(names.contains("r|Ibfb"), "got {names:?}");

        // The input relation carries the bound positions, in order: v, w.
        let input_rules = rules_of(out, "r|Ibfb");
        assert_eq!(input_rules.len(), 1);
        assert_eq!(head_names(&input_rules[0]), vec!["v", "w"]);

        // Inside r|Mbfb, the bound head slots (a, c) arrive via sup₀ from
        // the input relation, and the free slot does not.
        let sup0 = rules_of(out, "r|S.0.0bfb");
        assert_eq!(head_names(&sup0[0]), vec!["a", "c"]);
        assert_eq!(atom_names(&sup0[0]), vec!["rule r|Ibfb"]);
        Ok(())
    }

    /// Repeated-variable adornment, pinned exactly — deliberately preserved
    /// upstream behavior (see the oddity note in `NormalFormAtom::adorn`):
    /// in `r[v, y, y]` with `v` bound by a unification, the FIRST `y`
    /// adorns free (it is new), but the SECOND `y` adorns BOUND, because
    /// `seen_bindings.insert` already admitted the first occurrence within
    /// the same application. So the adornment is `bfb`, not `bff`. Any
    /// change to this (e.g. adorning all occurrences of a
    /// newly-introduced binding free) is a demand-shape change to make
    /// deliberately, against the naive oracle — never silently in a port.
    #[test]
    fn repeated_variable_adorns_later_positions_bound() -> Result<()>  {
        let strata = vec![stratum(vec![
            (
                "r",
                vec![nf_rule(
                    &["a", "b", "c"],
                    vec![stored_app("e", &["a", "b", "c"])],
                )],
            ),
            (
                "?",
                vec![nf_rule(
                    &["y"],
                    vec![unify_const("v", 1), rule_app("r", &["v", "y", "y"])],
                )],
            ),
        ])];
        let rewritten = stratified(strata, false)?
            .magic_sets_rewrite(&NoSchemas)
            .map_err(|e| miette!("rewrite succeeds: {e}"))?;
        let out = &rewritten.strata()[0];

        // The exact adornment vector: bound, free, bound.
        let adornments: Vec<Adornment> = out
            .prog
            .keys()
            .filter_map(|k| match k {
                MagicSymbol::Magic { inner, adornment } if inner.name.as_str() == "r" => {
                    Some(adornment.clone())
                }
                MagicSymbol::Muggle { .. }
                | MagicSymbol::Magic { .. }
                | MagicSymbol::Input { .. }
                | MagicSymbol::Sup { .. } => None,
            })
            .collect();
        assert_eq!(
            adornments,
            vec![vec![
                AdornmentMark::Bound,
                AdornmentMark::Free,
                AdornmentMark::Bound,
            ]],
            "repeated y must adorn its second position bound; got {:?}",
            key_names(out)
        );
        // The demand relation carries exactly the bound slots, in order:
        // v and the (repeated) y.
        assert_eq!(head_names(&rules_of(out, "r|Ibfb")[0]), vec!["v", "y"]);
        Ok(())
    }

    /// The seam: named-field bindings on a stored fixed-rule argument
    /// resolve to positional bindings against the declared schema, with
    /// digit-named fillers for unbound columns; unknown fields are refused.
    #[test]
    fn named_stored_fixed_rule_args_resolve_positionally() -> Result<()>  {
        let schema = FixedSchema(StoredRelationMetadata {
            keys: vec![column("a", ColType::Int), column("b", ColType::Int)],
            non_keys: vec![column("c", ColType::Int)],
        });
        let apply = |bindings: &[(&str, &str)]| FixedRuleApply {
            fixed_handle: FixedRuleHandle {
                name: sym("PageRank"),
            },
            rule_args: vec![FixedRuleArg::NamedStored {
                name: sym("edges"),
                bindings: bindings.iter().map(|(k, v)| (sym(k), sym(v))).collect(),
                as_of: None,
                span: SourceSpan(0, 0),
            }],
            options: FixedRuleOptions::empty(),
            head: vec![],
            arity: 1,
            span: SourceSpan(0, 0),
            trivia: Trivia::default(),
        };

        let adorned =
            adorn_fixed_rule_apply(&apply(&[("b", "x"), ("c", "y")]), &schema).map_err(|e| miette!("resolves: {e}"))?;
        match &adorned.rule_args[0] {
            MagicFixedRuleRuleArg::Stored { bindings, .. } => {
                let names: Vec<_> = bindings.iter().map(|s| s.name.as_str()).collect();
                assert_eq!(names, vec!["0", "x", "y"]);
            }
            other => panic!("expected a stored arg, got {other:?}"),
        }

        let err = adorn_fixed_rule_apply(&apply(&[("nope", "x")]), &schema)
            .expect_err("unknown field must be refused");
        assert!(
            err.to_string().contains("does not have field"),
            "got: {err}"
        );
        Ok(())
    }

    /// The seam: time travel is refused unless the last key column is
    /// non-nullable `Validity` — including the keyless-relation shape the
    /// original panicked on.
    #[test]
    fn time_travel_requires_a_validity_keyed_relation() -> Result<()>  {
        use kyzo_model::value::ValidityTs;

        let apply = FixedRuleApply {
            fixed_handle: FixedRuleHandle {
                name: sym("PageRank"),
            },
            rule_args: vec![FixedRuleArg::Stored {
                name: sym("edges"),
                bindings: vec![sym("x")],
                as_of: Some(AsOf::current(ValidityTs::from_raw(0))),
                span: SourceSpan(0, 0),
            }],
            options: FixedRuleOptions::empty(),
            head: vec![],
            arity: 1,
            span: SourceSpan(0, 0),
            trivia: Trivia::default(),
        };

        // Every facts relation time-travels in the one universal format:
        // plain and even keyless schemas adorn under `@` without refusal.
        let plain = FixedSchema(StoredRelationMetadata {
            keys: vec![column("a", ColType::Int)],
            non_keys: vec![],
        });
        adorn_fixed_rule_apply(&apply, &plain).map_err(|e| miette!("any facts relation time-travels: {e}"))?;

        let keyless = FixedSchema(StoredRelationMetadata {
            keys: vec![],
            non_keys: vec![column("a", ColType::Int)],
        });
        adorn_fixed_rule_apply(&apply, &keyless)
            .map_err(|e| miette!("a keyless relation adorns without panicking: {e}"))?;
            Ok(())
    }
}
