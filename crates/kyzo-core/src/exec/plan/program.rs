/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): plan-tier IR (NORMAL / STRATIFIED / MAGIC) + BodyNormalizer seam,
 * re-homed from condemned `data/program.rs`. Input-tier vocabulary lives in
 * `kyzo_model::program::{rule,query}`; `Arc<dyn FixedRule>` binds here at
 * magic time via [`bind_fixed_impl`].
 */

//! Plan-tier program IR: normal / stratified / magic + the normalizer seam.
//!
//! Minted only by their transformations; possession is proof. The INPUT tier
//! (what a query *is*) lives in `kyzo_model`; this module owns the plan
//! artifacts the oracle never sees.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::fmt::{Debug, Display, Formatter};
use std::sync::Arc;

use miette::{Diagnostic, Result, bail};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use kyzo_model::program::expr::{BindingPos, Expr};
use kyzo_model::program::query::QueryOutOptions;
use kyzo_model::program::rule::{
    FixedRuleApply, FixedRuleHandle, HeadAggrSlot, InputAtom, InputInlineRulesOrFixed,
    InputProgram, NoEntry, Unification, ValidityClause,
};
use kyzo_model::program::symbol::{Symbol, SymbolKind};
use kyzo_model::program::span::SourceSpan;
use kyzo_model::value::AsOf;

use crate::rules::contract::{FixedRule, DEFAULT_FIXED_RULES};

/// Look up the live fixed-rule implementation by resolved name. Used when
/// minting [`MagicFixedRuleApply`] from a model [`FixedRuleApply`] (name +
/// arity declaration only — no trait object on the model side).
pub(crate) fn bind_fixed_impl(name: &str) -> Option<Arc<dyn FixedRule>> {
    DEFAULT_FIXED_RULES.get(name).cloned()
}

/// A tier invariant that construction should have made impossible. Returned
/// (never panicked) on the paths whose impossibility is proven elsewhere, so
/// corruption of that proof surfaces as an error, not an abort.
#[derive(Debug, Diagnostic, Error)]
#[error("Program tier invariant violated: {0}")]
#[diagnostic(code(compiler::tier_invariant))]
#[diagnostic(help("This is a bug. Please report it."))]
pub(crate) struct TierInvariantError(pub(crate) &'static str);

/// The transaction-facing half of normalization: DNF conversion (which
/// resolves index search atoms against the catalog) and binding-safety
/// reordering. The total desugar half lives in [`into_normalized_program`].
pub(crate) trait BodyNormalizer {
    /// Convert one rule body to disjunctive normal form: a disjunction
    /// (outer `Vec`) of flat conjunctions (inner `Vec`s) of normal-form
    /// atoms. Fallible — this is where search atoms resolve against the
    /// catalog and where malformed negation is rejected.
    fn disjunctive_normal_form(&mut self, body: InputAtom) -> Result<Vec<Vec<NormalFormAtom>>>;

    /// Reorder one flat rule so every atom's inputs are bound before use,
    /// rejecting unsafe rules (unbound head or negation variables).
    fn well_order(&mut self, rule: NormalFormInlineRule) -> Result<NormalFormInlineRule>;
}

/// Normalize: desugar every rule body to disjunctive normal form and
/// well-order the results, minting the [`NormalFormProgram`] tier.
///
/// The desugaring done *here* is total: head deduplication (a repeated
/// head variable becomes a fresh `***n` binding plus a unification atom
/// in every body), and the fan-out of each DNF conjunction into its own
/// flat rule. The fallible, catalog-facing parts enter through the
/// [`BodyNormalizer`] seam.
pub(crate) fn into_normalized_program(
    prog: InputProgram,
    normalizer: &mut impl BodyNormalizer,
) -> Result<(NormalFormProgram, QueryOutOptions)> {
    // InputProgram keeps normalize-facing fields private; clone via accessors.
    // (A consuming `into_parts` on the model seat would avoid the clone.)
    let entry = normalize_ruleset(prog.entry().clone(), normalizer)?;
    let mut rules: BTreeMap<Symbol, NormalFormRulesOrFixed> = Default::default();
    for (k, ruleset) in prog.rules() {
        let normalized = normalize_ruleset(ruleset.clone(), normalizer)?;
        rules.insert(k.clone(), normalized);
    }
    Ok((
        NormalFormProgram {
            entry_name: prog.entry_name().clone(),
            entry,
            rules,
            disable_magic_rewrite: prog.disable_magic_rewrite(),
        },
        prog.out_opts().clone(),
    ))
}

/// Normalize one definition: total desugar here, with disjunctive normal
/// form and well-ordering checks.
pub(crate) fn normalize_ruleset(
    ruleset: InputInlineRulesOrFixed,
    normalizer: &mut impl BodyNormalizer,
) -> Result<NormalFormRulesOrFixed> {
    match ruleset {
        InputInlineRulesOrFixed::Rules { rules } => {
            let mut collected_rules = vec![];
            for rule in rules {
                let normalized_body =
                    normalizer.disjunctive_normal_form(InputAtom::Conjunction {
                        inner: rule.body,
                        span: rule.span,
                    })?;
                // Deduplicate repeated head variables: `r[a, a]` becomes
                // `r[a, ***0]` plus a `***0 = a` unification in every body.
                let mut dup_counter: usize = 0;
                let mut new_head = Vec::with_capacity(rule.head.len());
                let mut seen: BTreeMap<&Symbol, Vec<Symbol>> = BTreeMap::default();
                for symb in rule.head.iter() {
                    match seen.entry(symb) {
                        Entry::Vacant(e) => {
                            e.insert(vec![]);
                            new_head.push(symb.clone());
                        }
                        Entry::Occupied(mut e) => {
                            // `***n` is `*`-prefixed: SymbolKind::Generated,
                            // so it cannot collide with a user name.
                            let new_symb = Symbol::new(format!("***{dup_counter}"), symb.span);
                            dup_counter += 1;
                            e.get_mut().push(new_symb.clone());
                            new_head.push(new_symb);
                        }
                    }
                }
                for mut body in normalized_body {
                    for (old_symb, new_symbs) in seen.iter() {
                        for new_symb in new_symbs.iter() {
                            body.push(NormalFormAtom::Unification(Unification {
                                binding: new_symb.clone(),
                                expr: Expr::Binding {
                                    var: (*old_symb).clone(),
                                    tuple_pos: BindingPos::Unresolved,
                                },
                                one_many_unif: false,
                                span: new_symb.span,
                            }))
                        }
                    }
                    let normalized_rule = NormalFormInlineRule {
                        head: new_head.clone(),
                        aggr: rule.aggr.clone(),
                        body,
                    };
                    collected_rules.push(normalizer.well_order(normalized_rule)?);
                }
            }
            Ok(NormalFormRulesOrFixed::Rules {
                rules: collected_rules,
            })
        }
        InputInlineRulesOrFixed::Fixed { fixed } => Ok(NormalFormRulesOrFixed::Fixed { fixed }),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Normal-form tier
// ─────────────────────────────────────────────────────────────────────────

/// One normalized rule: a flat, well-ordered conjunction body.
#[derive(Debug)]
pub(crate) struct NormalFormInlineRule {
    pub(crate) head: Vec<Symbol>,
    pub(crate) aggr: Vec<HeadAggrSlot>,
    pub(crate) body: Vec<NormalFormAtom>,
}

/// A normalized definition: flat rules, or a fixed-rule application
/// (which normalization passes through untouched).
///
/// `Fixed` holds the model [`FixedRuleApply`] (name + arity declaration —
/// no `Arc<dyn FixedRule>`). The live impl binds at magic time.
#[derive(Debug)]
pub(crate) enum NormalFormRulesOrFixed {
    Rules { rules: Vec<NormalFormInlineRule> },
    Fixed { fixed: FixedRuleApply },
}

impl NormalFormRulesOrFixed {
    pub(crate) fn rules(&self) -> Option<&[NormalFormInlineRule]> {
        match self {
            NormalFormRulesOrFixed::Rules { rules: r } => Some(r),
            NormalFormRulesOrFixed::Fixed { fixed: _ } => None,
        }
    }
}

/// A body atom in normal form: applications over plain symbols, predicates,
/// and unifications. Negation is atom-level only — DNF pushed it down.
#[derive(Debug, Clone)]
pub(crate) enum NormalFormAtom {
    Rule(NormalFormRuleApplyAtom),
    Relation(NormalFormRelationApplyAtom),
    NegatedRule(NormalFormRuleApplyAtom),
    NegatedRelation(NormalFormRelationApplyAtom),
    Predicate(Expr),
    Unification(Unification),
    /// A resolved index search (`~rel:idx{…}`): binds its `own_bindings`,
    /// requires its query expression's variables. Resolved against the
    /// catalog by the body normalizer (`exec::plan::search::resolve_search`).
    Search(Box<crate::exec::plan::search::SearchAtom>),
}

/// A rule application over plain symbols.
#[derive(Clone, Debug)]
pub(crate) struct NormalFormRuleApplyAtom {
    pub(crate) name: Symbol,
    pub(crate) args: Vec<Symbol>,
    pub(crate) span: SourceSpan,
}

/// A stored-relation application over plain symbols, optionally carrying a
/// [`ValidityClause`] (time travel, interval derivation, or diff).
#[derive(Clone, Debug)]
pub(crate) struct NormalFormRelationApplyAtom {
    pub(crate) name: Symbol,
    pub(crate) args: Vec<Symbol>,
    pub(crate) validity: Option<ValidityClause>,
    pub(crate) span: SourceSpan,
}

/// The normalized program: every body flat, deduplicated, well-ordered.
/// Minted only by [`into_normalized_program`], so possession is proof of
/// normalization — and of an entry, carried over as a field.
#[derive(Debug)]
pub(crate) struct NormalFormProgram {
    entry_name: Symbol,
    entry: NormalFormRulesOrFixed,
    rules: BTreeMap<Symbol, NormalFormRulesOrFixed>,
    disable_magic_rewrite: bool,
}

impl NormalFormProgram {
    /// The entry rule's name (`?`) with its real source span.
    pub(crate) fn entry_name(&self) -> &Symbol {
        &self.entry_name
    }

    /// What the entry is defined as.
    pub(crate) fn entry(&self) -> &NormalFormRulesOrFixed {
        &self.entry
    }

    /// The non-entry rules.
    pub(crate) fn rules(&self) -> &BTreeMap<Symbol, NormalFormRulesOrFixed> {
        &self.rules
    }

    /// Whether `::set_options` disabled the magic-sets rewrite for this
    /// query. Travels to [`StratifiedNormalFormProgram`] at stratification.
    pub(crate) fn disable_magic_rewrite(&self) -> bool {
        self.disable_magic_rewrite
    }

    /// Every definition, the entry included (under its own name): the
    /// dependency-graph walk of the stratifier sees one uniform view.
    pub(crate) fn iter_all(&self) -> impl Iterator<Item = (&Symbol, &NormalFormRulesOrFixed)> {
        self.rules
            .iter()
            .chain(std::iter::once((&self.entry_name, &self.entry)))
    }

    /// Consume into parts, for the stratifier's final distribution of rule
    /// sets into strata. Consumption, not construction: this cannot mint a
    /// new tier value.
    pub(crate) fn into_parts(
        self,
    ) -> (
        (Symbol, NormalFormRulesOrFixed),
        BTreeMap<Symbol, NormalFormRulesOrFixed>,
        bool,
    ) {
        (
            (self.entry_name, self.entry),
            self.rules,
            self.disable_magic_rewrite,
        )
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Stratified tier
// ─────────────────────────────────────────────────────────────────────────

/// One stratum: the definitions that evaluate together in one fixpoint.
#[derive(Debug, Default)]
pub(crate) struct NormalFormStratum {
    /// The stratifier distributes rule sets in here; by construction of
    /// [`StratifiedNormalFormProgram`] the final stratum holds the entry.
    pub(crate) rules: BTreeMap<Symbol, NormalFormRulesOrFixed>,
}

impl NormalFormStratum {
    fn holds_entry(&self) -> bool {
        self.rules.keys().any(|k| k.kind() == SymbolKind::Entry)
    }
}

/// The stratified program: strata stored **in execution order** — stratum
/// `0` evaluates first, the last stratum holds the entry and evaluates last.
#[derive(Debug)]
pub(crate) struct StratifiedNormalFormProgram {
    /// Execution order: `strata[0]` evaluates first.
    strata: Vec<NormalFormStratum>,
    disable_magic_rewrite: bool,
}

impl StratifiedNormalFormProgram {
    /// Mint the stratified tier from the stratifier's output, which is in
    /// reverse execution order (element `0` = evaluated last). The reversal
    /// happens here, once.
    pub(crate) fn from_reverse_execution_order(
        mut reversed_strata: Vec<NormalFormStratum>,
        disable_magic_rewrite: bool,
    ) -> Result<Self> {
        reversed_strata.reverse();
        let strata = reversed_strata;
        match strata.last() {
            None => bail!(NoEntry::spanless()),
            Some(last) if !last.holds_entry() => {
                if strata.iter().any(NormalFormStratum::holds_entry) {
                    bail!(TierInvariantError("entry rule is not in the final stratum"))
                } else {
                    bail!(NoEntry::spanless())
                }
            }
            Some(_) => {}
        }
        Ok(Self {
            strata,
            disable_magic_rewrite,
        })
    }

    /// The strata in execution order.
    pub(crate) fn strata(&self) -> &[NormalFormStratum] {
        &self.strata
    }

    /// Consume into execution-ordered strata plus the magic-rewrite flag,
    /// for the magic-sets rewrite.
    pub(crate) fn into_parts(self) -> (Vec<NormalFormStratum>, bool) {
        (self.strata, self.disable_magic_rewrite)
    }
}

/// For each named store, the **execution-order index of the last stratum
/// that reads it**. Produced by stratification alongside the stratified
/// program; consumed by evaluation, which drops a store before running
/// stratum `s` unless `last_use >= s` (see [`Self::is_live_at`]).
#[derive(Debug, Default)]
pub(crate) struct StoreLifetimes(BTreeMap<MagicSymbol, usize>);

impl StoreLifetimes {
    /// Record that `store` is read by the stratum at execution-order index
    /// `last_use`; keeps the maximum across calls.
    pub(crate) fn note_use(&mut self, store: MagicSymbol, last_use: usize) {
        match self.0.entry(store) {
            Entry::Vacant(e) => {
                e.insert(last_use);
            }
            Entry::Occupied(mut o) => {
                if last_use > *o.get() {
                    o.insert(last_use);
                }
            }
        }
    }

    /// Whether `store` must still exist when the stratum at execution-order
    /// index `stratum` runs. Unknown stores are not live: they were used
    /// only inside their own stratum.
    pub(crate) fn is_live_at(&self, store: &MagicSymbol, stratum: usize) -> bool {
        self.0.get(store).is_some_and(|last| *last >= stratum)
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Magic tier
// ─────────────────────────────────────────────────────────────────────────

/// One argument position in a demand pattern — bound or free, never a bool.
#[derive(Debug, Clone, Copy, Ord, PartialOrd, Eq, PartialEq)]
pub(crate) enum AdornmentMark {
    Bound,
    Free,
}

impl AdornmentMark {
    pub(crate) fn is_bound(self) -> bool {
        matches!(self, AdornmentMark::Bound)
    }

    pub(crate) fn from_bound(bound: bool) -> AdornmentMark {
        if bound {
            AdornmentMark::Bound
        } else {
            AdornmentMark::Free
        }
    }
}

/// An adornment: for each argument position, [`AdornmentMark::Bound`] or
/// [`AdornmentMark::Free`]. Rendered `b`/`f` in debug output.
pub(crate) type Adornment = Vec<AdornmentMark>;

/// A rule name after the magic-sets rewrite. The variants carry the demand
/// analysis in the name itself: evaluation of a magic program computes only
/// what the entry demands, and the names prove which role each store plays.
#[derive(Clone, Ord, PartialOrd, Eq, PartialEq)]
pub(crate) enum MagicSymbol {
    /// An unadorned rule, exempt from the rewrite (the entry always is).
    Muggle { inner: Symbol },
    /// An adorned rule: computes only rows matching the demand pattern.
    Magic { inner: Symbol, adornment: Adornment },
    /// The demand ("input") relation feeding an adorned rule.
    Input { inner: Symbol, adornment: Adornment },
    /// A supplementary relation carrying partial joins between body atoms
    /// of rule `rule_idx` at position `sup_idx`.
    Sup {
        inner: Symbol,
        adornment: Adornment,
        rule_idx: u16,
        sup_idx: u16,
    },
}

impl MagicSymbol {
    /// The underlying rule name, adornment stripped.
    pub(crate) fn as_plain_symbol(&self) -> &Symbol {
        match self {
            MagicSymbol::Muggle { inner, .. }
            | MagicSymbol::Magic { inner, .. }
            | MagicSymbol::Input { inner, .. }
            | MagicSymbol::Sup { inner, .. } => inner,
        }
    }
    pub(crate) fn magic_adornment(&self) -> &[AdornmentMark] {
        match self {
            MagicSymbol::Muggle { .. } => &[],
            MagicSymbol::Magic { adornment, .. }
            | MagicSymbol::Input { adornment, .. }
            | MagicSymbol::Sup { adornment, .. } => adornment,
        }
    }
    pub(crate) fn has_bound_adornment(&self) -> bool {
        self.magic_adornment().iter().any(|m| m.is_bound())
    }
    pub(crate) fn is_prog_entry(&self) -> bool {
        if let MagicSymbol::Muggle { inner } = self {
            inner.kind() == SymbolKind::Entry
        } else {
            false
        }
    }
}

impl Display for MagicSymbol {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl Debug for MagicSymbol {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            MagicSymbol::Muggle { inner } => write!(f, "{}", inner.name),
            MagicSymbol::Magic { inner, adornment } => {
                write!(f, "{}|M", inner.name)?;
                for m in adornment {
                    match m {
                        AdornmentMark::Bound => write!(f, "b")?,
                        AdornmentMark::Free => write!(f, "f")?,
                    }
                }
                Ok(())
            }
            MagicSymbol::Input { inner, adornment } => {
                write!(f, "{}|I", inner.name)?;
                for m in adornment {
                    match m {
                        AdornmentMark::Bound => write!(f, "b")?,
                        AdornmentMark::Free => write!(f, "f")?,
                    }
                }
                Ok(())
            }
            MagicSymbol::Sup {
                inner,
                adornment,
                rule_idx,
                sup_idx,
            } => {
                write!(f, "{}|S.{}.{}", inner.name, rule_idx, sup_idx)?;
                for m in adornment {
                    match m {
                        AdornmentMark::Bound => write!(f, "b")?,
                        AdornmentMark::Free => write!(f, "f")?,
                    }
                }
                Ok(())
            }
        }
    }
}

/// One rule after the magic rewrite.
#[derive(Debug)]
pub(crate) struct MagicInlineRule {
    pub(crate) head: Vec<Symbol>,
    pub(crate) aggr: Vec<HeadAggrSlot>,
    pub(crate) body: Vec<MagicAtom>,
}

/// A magic-tier definition: rewritten rules, or a fixed-rule application
/// with magic-renamed arguments.
#[derive(Debug)]
pub(crate) enum MagicRulesOrFixed {
    Rules { rules: Vec<MagicInlineRule> },
    Fixed { fixed: MagicFixedRuleApply },
}

impl Default for MagicRulesOrFixed {
    fn default() -> Self {
        Self::Rules { rules: vec![] }
    }
}

impl MagicRulesOrFixed {
    /// The output arity of this definition. Errors on a rule set that is
    /// still empty (the transient `Default` state).
    pub(crate) fn arity(&self) -> Result<usize> {
        match self {
            MagicRulesOrFixed::Rules { rules } => match rules.first() {
                Some(rule) => Ok(rule.head.len()),
                None => bail!(TierInvariantError("empty magic rule set has no arity")),
            },
            MagicRulesOrFixed::Fixed { fixed } => Ok(fixed.arity),
        }
    }

    pub(crate) fn mut_rules(&mut self) -> Option<&mut Vec<MagicInlineRule>> {
        match self {
            MagicRulesOrFixed::Rules { rules } => Some(rules),
            MagicRulesOrFixed::Fixed { fixed: _ } => None,
        }
    }
}

/// A fixed-rule application in the magic tier: in-memory arguments are now
/// named by [`MagicSymbol`]. Keeps the live [`FixedRule`] impl — engine bind.
pub(crate) struct MagicFixedRuleApply {
    pub(crate) fixed_handle: FixedRuleHandle,
    pub(crate) rule_args: Vec<MagicFixedRuleRuleArg>,
    pub(crate) options: Arc<BTreeMap<SmartString<LazyCompact>, Expr>>,
    pub(crate) span: SourceSpan,
    pub(crate) arity: usize,
    pub(crate) fixed_impl: Arc<dyn FixedRule>,
}

#[derive(Error, Diagnostic, Debug)]
#[error("Cannot find a required named option '{name}' for '{rule_name}'")]
#[diagnostic(code(fixed_rule::arg_not_found))]
pub(crate) struct FixedRuleOptionNotFoundError {
    pub(crate) name: Symbol,
    #[label]
    pub(crate) span: SourceSpan,
    pub(crate) rule_name: Symbol,
}

#[derive(Error, Diagnostic, Debug)]
#[error("Wrong value for option '{name}' of '{rule_name}'")]
#[diagnostic(code(fixed_rule::arg_wrong))]
pub(crate) struct WrongFixedRuleOptionError {
    pub(crate) name: Symbol,
    #[label]
    pub(crate) span: SourceSpan,
    pub(crate) rule_name: Symbol,
    #[help]
    pub(crate) help: WrongFixedRuleOptionHelp,
}

/// Named help for [`WrongFixedRuleOptionError`] — String identity is
/// unrepresentable; every construction site picks a variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WrongFixedRuleOptionHelp {
    StringRequired,
    IntegerRequired,
    PositiveIntegerRequired,
    PositiveIntegerFitsUsizeRequired,
    NonNegIntegerRequired,
    NonNegIntegerFitsUsizeRequired,
    FloatRequired,
    UnitIntervalRequired,
    BoolRequired,
    ListOfListsRequired,
    DelimiterSingleByte,
    OptionMustBeList,
    TypesNotColType,
}

impl Display for WrongFixedRuleOptionHelp {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StringRequired => write!(f, "a string is required"),
            Self::IntegerRequired => write!(f, "an integer is required"),
            Self::PositiveIntegerRequired => write!(f, "a positive integer is required"),
            Self::PositiveIntegerFitsUsizeRequired => {
                write!(f, "a positive integer fitting usize is required")
            }
            Self::NonNegIntegerRequired => write!(f, "a non-negative integer is required"),
            Self::NonNegIntegerFitsUsizeRequired => {
                write!(f, "a non-negative integer fitting usize is required")
            }
            Self::FloatRequired => write!(f, "a floating number is required"),
            Self::UnitIntervalRequired => write!(f, "a number between 0. and 1. is required"),
            Self::BoolRequired => write!(f, "a boolean value is required"),
            Self::ListOfListsRequired => write!(f, "a list of lists is required"),
            Self::DelimiterSingleByte => write!(f, "'delimiter' must be a single-byte string"),
            Self::OptionMustBeList => write!(f, "This option must evaluate to a list"),
            Self::TypesNotColType => {
                write!(f, "each element of 'types' must be a valid column type")
            }
        }
    }
}

impl MagicFixedRuleApply {
    pub(crate) fn relations_count(&self) -> usize {
        self.rule_args.len()
    }

    pub(crate) fn relation(&self, idx: usize) -> Result<&MagicFixedRuleRuleArg> {
        #[derive(Error, Diagnostic, Debug)]
        #[error("Cannot find a required positional argument at index {idx} for '{rule_name}'")]
        #[diagnostic(code(fixed_rule::not_enough_args))]
        pub(crate) struct FixedRuleNotEnoughRelationError {
            idx: usize,
            #[label]
            span: SourceSpan,
            rule_name: Symbol,
        }

        self.rule_args.get(idx).ok_or_else(|| {
            FixedRuleNotEnoughRelationError {
                idx,
                span: self.span,
                rule_name: self.fixed_handle.name.clone(),
            }
            .into()
        })
    }
}

impl Debug for MagicFixedRuleApply {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FixedRuleApply")
            .field("name", &self.fixed_handle.name)
            .field("rules", &self.rule_args)
            .field("options", &self.options)
            .finish()
    }
}

/// A fixed-rule argument in the magic tier.
#[derive(Debug)]
pub(crate) enum MagicFixedRuleRuleArg {
    InMem {
        name: MagicSymbol,
        bindings: Vec<Symbol>,
        span: SourceSpan,
    },
    Stored {
        name: Symbol,
        bindings: Vec<Symbol>,
        as_of: Option<AsOf>,
        span: SourceSpan,
    },
}

impl MagicFixedRuleRuleArg {
    pub(crate) fn bindings(&self) -> &[Symbol] {
        match self {
            MagicFixedRuleRuleArg::InMem { bindings, .. }
            | MagicFixedRuleRuleArg::Stored { bindings, .. } => bindings,
        }
    }

    pub(crate) fn span(&self) -> SourceSpan {
        match self {
            MagicFixedRuleRuleArg::InMem { span, .. }
            | MagicFixedRuleRuleArg::Stored { span, .. } => *span,
        }
    }

    pub(crate) fn get_binding_map(&self, starting: usize) -> BTreeMap<Symbol, usize> {
        let bindings = match self {
            MagicFixedRuleRuleArg::InMem { bindings, .. }
            | MagicFixedRuleRuleArg::Stored { bindings, .. } => bindings,
        };
        bindings
            .iter()
            .enumerate()
            .map(|(idx, symb)| (symb.clone(), idx + starting))
            .collect()
    }
}

/// A body atom in the magic tier. As with [`NormalFormAtom`], the resolved
/// index-search variants land with the index tier.
#[derive(Debug, Clone)]
pub(crate) enum MagicAtom {
    Rule(MagicRuleApplyAtom),
    Relation(MagicRelationApplyAtom),
    Predicate(Expr),
    NegatedRule(MagicRuleApplyAtom),
    NegatedRelation(MagicRelationApplyAtom),
    Unification(Unification),
    /// A resolved index search: adornment-inert (like `Relation`), passed
    /// through the magic rewrite with its dataflow facts intact.
    Search(Box<crate::exec::plan::search::SearchAtom>),
}

/// A rule application naming a [`MagicSymbol`].
#[derive(Clone, Debug)]
pub(crate) struct MagicRuleApplyAtom {
    pub(crate) name: MagicSymbol,
    pub(crate) args: Vec<Symbol>,
    pub(crate) span: SourceSpan,
}

/// A stored-relation application in the magic tier (stored relations are
/// never adorned; demand cannot restrict what is already materialized).
#[derive(Clone, Debug)]
pub(crate) struct MagicRelationApplyAtom {
    pub(crate) name: Symbol,
    pub(crate) args: Vec<Symbol>,
    pub(crate) validity: Option<ValidityClause>,
    pub(crate) span: SourceSpan,
}

/// One stratum after the magic rewrite.
#[derive(Debug, Default)]
pub(crate) struct MagicProgram {
    pub(crate) prog: BTreeMap<MagicSymbol, MagicRulesOrFixed>,
}

impl MagicProgram {
    fn holds_entry(&self) -> bool {
        self.prog.keys().any(MagicSymbol::is_prog_entry)
    }
}

/// The demand-rewritten program: strata **in execution order**.
#[derive(Debug)]
pub(crate) struct StratifiedMagicProgram {
    /// Execution order: `strata[0]` evaluates first.
    strata: Vec<MagicProgram>,
}

impl StratifiedMagicProgram {
    /// Mint the magic tier from the rewrite's per-stratum output, already in
    /// execution order. Proves the entry survived the rewrite unadorned and
    /// sits in the final stratum.
    pub(crate) fn from_execution_order(strata: Vec<MagicProgram>) -> Result<Self> {
        match strata.last() {
            None => bail!(NoEntry::spanless()),
            Some(last) if !last.holds_entry() => {
                if strata.iter().any(MagicProgram::holds_entry) {
                    bail!(TierInvariantError(
                        "magic entry rule is not in the final stratum"
                    ))
                } else {
                    bail!(NoEntry::spanless())
                }
            }
            Some(_) => {}
        }
        Ok(Self { strata })
    }

    /// The strata in execution order.
    pub(crate) fn strata(&self) -> &[MagicProgram] {
        &self.strata
    }

    /// Consume into execution-ordered strata for compilation.
    pub(crate) fn into_strata(self) -> Vec<MagicProgram> {
        self.strata
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kyzo_model::program::rule::{HeadAggrSlot, InputInlineRule, InputInlineRulesOrFixed};

    fn sym(name: &str) -> Symbol {
        Symbol::new(name, SourceSpan(0, 0))
    }

    fn rule(head: &[&str]) -> InputInlineRule {
        InputInlineRule {
            head: head.iter().map(|h| sym(h)).collect(),
            aggr: head.iter().map(|_| HeadAggrSlot::Plain).collect(),
            body: vec![],
            span: SourceSpan(0, 0),
            trivia: Default::default(),
        }
    }

    fn rules_def(rules: Vec<InputInlineRule>) -> InputInlineRulesOrFixed {
        InputInlineRulesOrFixed::Rules { rules }
    }

    /// A normalizer for exercising the total desugar in isolation: every
    /// body becomes a single empty conjunction, well-ordering is identity.
    struct TrivialNormalizer;

    impl BodyNormalizer for TrivialNormalizer {
        fn disjunctive_normal_form(
            &mut self,
            _body: InputAtom,
        ) -> Result<Vec<Vec<NormalFormAtom>>> {
            Ok(vec![vec![]])
        }
        fn well_order(&mut self, rule: NormalFormInlineRule) -> Result<NormalFormInlineRule> {
            Ok(rule)
        }
    }

    /// Normalization deduplicates repeated head variables: the duplicate
    /// becomes a generated `***n` binding plus a unification atom in the
    /// body, and the generated name classifies as Generated.
    #[test]
    fn normalization_deduplicates_head_variables() {
        let mut prog = BTreeMap::new();
        prog.insert(
            Symbol::prog_entry(SourceSpan(0, 1)),
            rules_def(vec![rule(&["a", "a"])]),
        );
        let p = InputProgram::new(prog, QueryOutOptions::default(), false).expect("valid program");
        let (normalized, _opts) =
            into_normalized_program(p, &mut TrivialNormalizer).expect("normalizes");
        let rules = normalized.entry().rules().expect("entry is rules");
        assert_eq!(rules.len(), 1);
        let entry_rule = &rules[0];
        assert_eq!(entry_rule.head[0].name.as_str(), "a");
        assert_eq!(entry_rule.head[1].name.as_str(), "***0");
        assert_eq!(entry_rule.head[1].kind(), SymbolKind::Generated);
        assert_eq!(entry_rule.body.len(), 1);
        match &entry_rule.body[0] {
            NormalFormAtom::Unification(u) => {
                assert_eq!(u.binding.name.as_str(), "***0");
                assert!(!u.one_many_unif);
                match &u.expr {
                    Expr::Binding { var, tuple_pos } => {
                        assert_eq!(var.name.as_str(), "a");
                        assert_eq!(*tuple_pos, BindingPos::Unresolved);
                    }
                    other => panic!("expected a binding, got {other:?}"),
                }
            }
            other => panic!("expected a unification, got {other:?}"),
        }
    }

    /// The normal-form tier keeps the entry as a field: normalization is
    /// the only mint, and the proof carries over.
    #[test]
    fn normalization_carries_the_entry_field() {
        let mut prog = BTreeMap::new();
        prog.insert(
            Symbol::prog_entry(SourceSpan(3, 1)),
            rules_def(vec![rule(&["x"])]),
        );
        prog.insert(sym("r"), rules_def(vec![rule(&["y"])]));
        let p = InputProgram::new(prog, QueryOutOptions::default(), true).expect("valid program");
        let (normalized, _) =
            into_normalized_program(p, &mut TrivialNormalizer).expect("normalizes");
        assert_eq!(normalized.entry_name().span, SourceSpan(3, 1));
        assert!(normalized.disable_magic_rewrite());
        assert_eq!(normalized.rules().len(), 1);
        assert_eq!(normalized.iter_all().count(), 2);
    }

    fn nf_stratum(names: &[&str]) -> NormalFormStratum {
        let mut stratum = NormalFormStratum::default();
        for name in names {
            let key = if *name == "?" {
                Symbol::prog_entry(SourceSpan(0, 1))
            } else {
                sym(name)
            };
            stratum.rules.insert(
                key,
                NormalFormRulesOrFixed::Rules {
                    rules: vec![NormalFormInlineRule {
                        head: vec![sym("x")],
                        aggr: vec![HeadAggrSlot::Plain],
                        body: vec![],
                    }],
                },
            );
        }
        stratum
    }

    /// The stratified tier stores execution order: the constructor takes the
    /// stratifier's reversed output and reverses it exactly once, and the
    /// entry ends up in the final stratum.
    #[test]
    fn stratified_tier_stores_execution_order() {
        let reversed = vec![nf_stratum(&["?"]), nf_stratum(&["r"])];
        let stratified = StratifiedNormalFormProgram::from_reverse_execution_order(reversed, false)
            .expect("stratifies");
        let strata = stratified.strata();
        assert_eq!(strata.len(), 2);
        assert!(
            !strata[0]
                .rules
                .keys()
                .any(|k| k.kind() == SymbolKind::Entry)
        );
        assert!(
            strata[1]
                .rules
                .keys()
                .any(|k| k.kind() == SymbolKind::Entry)
        );
    }

    /// Losing the entry across stratification is a construction error, not
    /// a later panic or a silent wrong answer.
    #[test]
    fn stratified_tier_requires_the_entry() {
        let err = StratifiedNormalFormProgram::from_reverse_execution_order(
            vec![nf_stratum(&["r"])],
            false,
        )
        .expect_err("entry-less strata must be refused");
        assert!(err.to_string().contains("no entry"), "got: {err}");

        let err = StratifiedNormalFormProgram::from_reverse_execution_order(vec![], false)
            .expect_err("no strata means no entry");
        assert!(err.to_string().contains("no entry"), "got: {err}");
    }

    /// An entry in a non-final stratum is a stratifier bug, reported as the
    /// tier-invariant error rather than accepted or panicked on.
    #[test]
    fn entry_must_sit_in_the_final_stratum() {
        let reversed = vec![nf_stratum(&["r"]), nf_stratum(&["?"])];
        let err = StratifiedNormalFormProgram::from_reverse_execution_order(reversed, false)
            .expect_err("misplaced entry must be refused");
        assert!(err.to_string().contains("invariant"), "got: {err}");
    }

    /// Store lifetimes: `note_use` keeps the maximum, `is_live_at` encodes
    /// evaluation's drop rule (`last_use >= stratum`), and unknown stores
    /// are never live across strata.
    #[test]
    fn store_lifetimes_semantics() {
        let name = MagicSymbol::Muggle { inner: sym("r") };
        let mut lifetimes = StoreLifetimes::default();
        lifetimes.note_use(name.clone(), 1);
        lifetimes.note_use(name.clone(), 3);
        lifetimes.note_use(name.clone(), 2);
        assert!(lifetimes.is_live_at(&name, 3));
        assert!(!lifetimes.is_live_at(&name, 4));
        let unknown = MagicSymbol::Muggle { inner: sym("s") };
        assert!(!lifetimes.is_live_at(&unknown, 0));
    }

    /// The magic tier's entry proof: execution-ordered strata whose final
    /// stratum holds the unadorned (Muggle) entry.
    #[test]
    fn magic_tier_requires_the_entry_in_the_final_stratum() {
        let mut entry_stratum = MagicProgram::default();
        entry_stratum.prog.insert(
            MagicSymbol::Muggle {
                inner: Symbol::prog_entry(SourceSpan(0, 1)),
            },
            MagicRulesOrFixed::Rules {
                rules: vec![MagicInlineRule {
                    head: vec![sym("x")],
                    aggr: vec![HeadAggrSlot::Plain],
                    body: vec![],
                }],
            },
        );
        let mut other = MagicProgram::default();
        other.prog.insert(
            MagicSymbol::Magic {
                inner: sym("r"),
                adornment: vec![AdornmentMark::Bound, AdornmentMark::Free],
            },
            MagicRulesOrFixed::default(),
        );

        let ok = StratifiedMagicProgram::from_execution_order(vec![other, entry_stratum])
            .expect("entry in final stratum is accepted");
        assert_eq!(ok.strata().len(), 2);
        assert!(ok.strata()[1].holds_entry());

        let mut lone = MagicProgram::default();
        lone.prog.insert(
            MagicSymbol::Magic {
                inner: sym("r"),
                adornment: vec![],
            },
            MagicRulesOrFixed::default(),
        );
        let err = StratifiedMagicProgram::from_execution_order(vec![lone])
            .expect_err("entry-less magic strata must be refused");
        assert!(err.to_string().contains("no entry"), "got: {err}");
    }

    /// An adorned name is never the entry, even over the `?` symbol; only
    /// the unadorned Muggle form is.
    #[test]
    fn adorned_entry_is_not_the_entry() {
        let muggle = MagicSymbol::Muggle {
            inner: Symbol::prog_entry(SourceSpan(0, 1)),
        };
        let magic = MagicSymbol::Magic {
            inner: Symbol::prog_entry(SourceSpan(0, 1)),
            adornment: vec![AdornmentMark::Bound],
        };
        assert!(muggle.is_prog_entry());
        assert!(!magic.is_prog_entry());
        assert!(magic.has_bound_adornment());
        assert_eq!(muggle.magic_adornment(), &[] as &[AdornmentMark]);
    }

    /// The magic-symbol debug rendering is load-bearing for logs and error
    /// messages: adornments render as `b`/`f` after the role marker.
    #[test]
    fn magic_symbol_debug_rendering() {
        let s = MagicSymbol::Sup {
            inner: sym("r"),
            adornment: vec![AdornmentMark::Bound, AdornmentMark::Free],
            rule_idx: 2,
            sup_idx: 5,
        };
        assert_eq!(format!("{s:?}"), "r|S.2.5bf");
        let m = MagicSymbol::Magic {
            inner: sym("r"),
            adornment: vec![AdornmentMark::Free, AdornmentMark::Bound],
        };
        assert_eq!(format!("{m:?}"), "r|Mfb");
        let i = MagicSymbol::Input {
            inner: sym("r"),
            adornment: vec![AdornmentMark::Bound],
        };
        assert_eq!(format!("{i:?}"), "r|Ib");
        let mu = MagicSymbol::Muggle { inner: sym("r") };
        assert_eq!(format!("{mu:?}"), "r");
    }

    /// The transient empty rule set (`MagicRulesOrFixed::default`) reports
    /// an error when asked for its arity — the original panicked.
    #[test]
    fn empty_magic_rule_set_arity_is_an_error() {
        let empty = MagicRulesOrFixed::default();
        assert!(empty.arity().is_err());
    }

    /// A missing positional fixed-rule argument is a diagnostic, not a
    /// panic, and the count accessor agrees.
    #[test]
    fn magic_fixed_rule_relation_lookup() {
        struct NoRule;
        impl FixedRule for NoRule {
            fn arity(
                &self,
                _options: &BTreeMap<SmartString<LazyCompact>, Expr>,
                _rule_head: &[Symbol],
                _span: SourceSpan,
            ) -> Result<usize> {
                Ok(1)
            }
            fn run(
                &self,
                _payload: crate::rules::contract::FixedRulePayload<'_>,
                _out: &mut crate::rules::contract::FixedRuleOutput,
                _cancel: crate::rules::contract::CancelFlag,
            ) -> Result<()> {
                unreachable!("test stub: never run")
            }
        }
        let apply = MagicFixedRuleApply {
            fixed_handle: FixedRuleHandle {
                name: sym("pagerank"),
            },
            rule_args: vec![MagicFixedRuleRuleArg::Stored {
                name: sym("edges"),
                bindings: vec![sym("a"), sym("b")],
                as_of: None,
                span: SourceSpan(0, 0),
            }],
            options: Arc::new(BTreeMap::new()),
            span: SourceSpan(0, 0),
            arity: 1,
            fixed_impl: Arc::new(NoRule),
        };
        assert_eq!(apply.relations_count(), 1);
        assert!(apply.relation(0).is_ok());
        let err = apply.relation(1).expect_err("out of range");
        assert!(
            err.to_string().contains("positional argument"),
            "got: {err}"
        );
        let map = apply.relation(0).expect("in range").get_binding_map(3);
        assert_eq!(map.get(&sym("a")), Some(&3));
        assert_eq!(map.get(&sym("b")), Some(&4));
    }
}
