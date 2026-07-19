/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): the input-tier program vocabulary seated in kyzo-model;
 * FixedRuleApply carries name+arity declaration only (no Arc<dyn FixedRule>);
 * fixed-rule options are a typed FixedRuleOptions bag (Symbol keys, unknown
 * names unconstructible); named-field maps are Symbol-keyed;
 * one arity authority on the declaration field; NormalForm minting omitted
 * (normalize lives in exec/plan).
 */

//! The input-tier program IR: what a parsed query *is* before normalization.
//!
//! [`InputProgram`] proves an entry (`?`) at construction. Downstream tiers
//! (normal / stratified / magic) live in the engine plan seat.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::{Debug, Display, Formatter};

use miette::{Diagnostic, Result, bail};
use serde::de::Error as _;
use serde::{Deserialize, Serialize};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::SourceSpan;
use crate::program::aggregate::Aggregation;
use crate::program::expr::Expr;
use crate::program::op::{resolve_fixed_rule_option, resolve_search_modality_option};
use crate::program::query::QueryOutOptions;
use crate::program::symbol::Symbol;
use crate::value::{AsOf, DataValue, ValidityTs};

// ─────────────────────────────────────────────────────────────────────────
// Generated symbols
// ─────────────────────────────────────────────────────────────────────────

/// Mints fresh compiler-generated symbols. The prefixes are load-bearing:
/// `*` classifies as generated and `~` as generated-ignored, and neither is
/// a valid user identifier in the grammar, so generated names can never
/// collide with user names.
#[derive(Default)]
pub struct TempSymbGen {
    last_id: u32,
}

impl TempSymbGen {
    /// A fresh generated binding (`*n`).
    pub fn next(&mut self, span: SourceSpan) -> Symbol {
        self.last_id += 1;
        Symbol::new(format!("*{}", self.last_id), span)
    }

    /// A fresh generated *ignored* binding (`~n`): matches anything, binds
    /// nothing downstream.
    pub fn next_ignored(&mut self, span: SourceSpan) -> Symbol {
        self.last_id += 1;
        Symbol::new(format!("~{}", self.last_id), span)
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Program-shape errors
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Diagnostic, Error)]
#[error("Entry head not found")]
#[diagnostic(code(parser::no_entry_head))]
#[diagnostic(help("You need to explicitly name your entry arguments"))]
pub struct EntryHeadNotExplicitlyDefined(#[label] pub SourceSpan);

/// The query has no `?` rule. An entry-less [`InputProgram`] cannot exist.
///
/// The parse-reachable refusal ([`InputProgram::new`]) carries a span.
/// Later-tier defensive sites use [`NoEntry::spanless`].
#[derive(Debug, Diagnostic, Error)]
#[error("Program has no entry")]
#[diagnostic(code(parser::no_entry))]
#[diagnostic(help("You need to have one rule named '?'"))]
pub struct NoEntry(#[label("this program defines no `?` rule")] pub Option<SourceSpan>);

impl NoEntry {
    /// The span-less variant for later-tier defensive sites that reach this
    /// error only if the entry proven by [`InputProgram::new`] has been
    /// corrupted.
    pub fn spanless() -> Self {
        NoEntry(None)
    }
}

/// A named rule set with zero rules.
#[derive(Debug, Diagnostic, Error)]
#[error("The rule set for '{0}' contains no rules")]
#[diagnostic(code(parser::empty_rule_set))]
pub struct EmptyRuleSet(pub String, #[label] pub SourceSpan);

/// A construction invariant that should have been impossible.
#[derive(Debug, Diagnostic, Error)]
#[error("Program tier invariant violated: {0}")]
#[diagnostic(code(compiler::tier_invariant))]
#[diagnostic(help("This is a bug. Please report it."))]
struct TierInvariant(&'static str);

// ─────────────────────────────────────────────────────────────────────────
// Comment / trivia
// ─────────────────────────────────────────────────────────────────────────

/// One comment, captured verbatim from source text — delimiters included
/// (`#...` or `/* ... */`), so a consumer never re-synthesizes comment
/// syntax, only places the text back where it was attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub text: String,
    pub span: SourceSpan,
}

/// The comments attached to one position in the program: every comment
/// immediately preceding it (`leading`, in source order), and every
/// comment sharing the position's own last source line after it
/// (`trailing`, in source order). Attached once by
/// [`InputProgram::attach_comment_trivia`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trivia {
    pub leading: Vec<Comment>,
    pub trailing: Vec<Comment>,
}

// ─────────────────────────────────────────────────────────────────────────
// Head aggregation slots
// ─────────────────────────────────────────────────────────────────────────

/// Per-head-position aggregation — a structured slot, never an `Option` hole.
#[derive(Debug, Clone, PartialEq, Eq, serde_derive::Serialize, serde_derive::Deserialize)]
pub enum HeadAggrSlot {
    Plain,
    Aggregated {
        aggr: Aggregation,
        args: Vec<DataValue>,
    },
}

impl HeadAggrSlot {
    pub fn is_aggregated(&self) -> bool {
        matches!(self, HeadAggrSlot::Aggregated { .. })
    }

    pub fn as_aggregated(&self) -> Option<(&Aggregation, &[DataValue])> {
        match self {
            HeadAggrSlot::Plain => None,
            HeadAggrSlot::Aggregated { aggr, args } => Some((aggr, args)),
        }
    }
}

/// One head column: binding paired with its aggregation slot.
#[derive(Debug, Clone)]
pub struct HeadColumn {
    pub binding: Symbol,
    pub aggr: HeadAggrSlot,
}

/// Zip head bindings with aggregation slots, refusing length disagreement.
pub fn aligned_head(
    bindings: Vec<Symbol>,
    aggrs: Vec<HeadAggrSlot>,
) -> Result<(Vec<Symbol>, Vec<HeadAggrSlot>), HeadAggrLenMismatch> {
    if bindings.len() != aggrs.len() {
        return Err(HeadAggrLenMismatch(bindings.len(), aggrs.len()));
    }
    Ok((bindings, aggrs))
}

/// Split aligned [`HeadColumn`]s into the parallel head/aggr representation.
pub fn split_head_columns(columns: Vec<HeadColumn>) -> (Vec<Symbol>, Vec<HeadAggrSlot>) {
    let mut bindings = Vec::with_capacity(columns.len());
    let mut aggrs = Vec::with_capacity(columns.len());
    for HeadColumn { binding, aggr } in columns {
        bindings.push(binding);
        aggrs.push(aggr);
    }
    (bindings, aggrs)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadAggrLenMismatch(pub usize, pub usize);

impl std::fmt::Display for HeadAggrLenMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "head binding count {} disagrees with aggregation slot count {}",
            self.0, self.1
        )
    }
}

impl std::error::Error for HeadAggrLenMismatch {}

// ─────────────────────────────────────────────────────────────────────────
// Inline rules / fixed-rule apply
// ─────────────────────────────────────────────────────────────────────────

/// One parsed inline rule: head bindings with per-position [`HeadAggrSlot`]s.
#[derive(Debug, Clone, serde_derive::Serialize, serde_derive::Deserialize)]
pub struct InputInlineRule {
    pub head: Vec<Symbol>,
    pub aggr: Vec<HeadAggrSlot>,
    pub body: Vec<InputAtom>,
    #[serde(skip)]
    pub span: SourceSpan,
    #[serde(skip)]
    pub trivia: Trivia,
}

/// What a name is defined as in a program: a set of inline rules, or a
/// fixed-rule application.
#[derive(Debug, Clone, serde_derive::Serialize, serde_derive::Deserialize)]
pub enum InputInlineRulesOrFixed {
    Rules { rules: Vec<InputInlineRule> },
    Fixed { fixed: FixedRuleApply },
}

impl InputInlineRulesOrFixed {
    /// The span of the first clause, for labeling diagnostics. `None` only
    /// for an empty rule set, which [`InputProgram::new`] refuses.
    pub fn first_span(&self) -> Option<SourceSpan> {
        match self {
            InputInlineRulesOrFixed::Rules { rules, .. } => rules.first().map(|r| r.span),
            InputInlineRulesOrFixed::Fixed { fixed, .. } => Some(fixed.span),
        }
    }
}

/// Declaration handle for a fixed rule: the registered name only.
///
/// The live `FixedRule` impl binds at exec/plan time; the model cannot
/// import the rules contract.
#[derive(Clone, Debug, serde_derive::Serialize, serde_derive::Deserialize)]
pub struct FixedRuleHandle {
    pub name: Symbol,
}

impl FixedRuleHandle {
    pub fn new(name: &str, span: SourceSpan) -> Self {
        FixedRuleHandle {
            name: Symbol::new(name, span),
        }
    }
}

/// Unknown / misspelled fixed-rule option — refused at construction, never
/// deferred to bind-time panic.
#[derive(Debug, Clone, PartialEq, Eq, Error, Diagnostic)]
#[error("unknown fixed-rule option '{name}'")]
#[diagnostic(code(parser::unknown_fixed_rule_option))]
#[diagnostic(help("Option names are a closed vocabulary; unknown names are unconstructible"))]
pub struct UnknownFixedRuleOption {
    pub name: String,
    #[label]
    pub span: SourceSpan,
}

/// Proven fixed-rule options bag: every key resolved through
/// [`resolve_fixed_rule_option`]. Identity keys are [`Symbol`]; an unknown
/// name cannot enter this type.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FixedRuleOptions {
    entries: BTreeMap<Symbol, Expr>,
}

impl FixedRuleOptions {
    pub fn empty() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Build from Symbol-keyed pairs; refuses any key that does not resolve.
    pub fn from_entries(
        entries: impl IntoIterator<Item = (Symbol, Expr)>,
    ) -> std::result::Result<Self, UnknownFixedRuleOption> {
        let mut opts = Self::empty();
        for (name, value) in entries {
            opts.insert(name, value)?;
        }
        Ok(opts)
    }

    /// Insert one option. Unknown names are unconstructible here.
    pub fn insert(
        &mut self,
        name: Symbol,
        value: Expr,
    ) -> std::result::Result<(), UnknownFixedRuleOption> {
        if resolve_fixed_rule_option(&name).is_none() {
            return Err(UnknownFixedRuleOption {
                name: name.name.to_string(),
                span: name.span,
            });
        }
        self.entries.insert(name, value);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&Expr> {
        self.entries.get(&Symbol::new(name, SourceSpan::default()))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Symbol, &Expr)> {
        self.entries.iter()
    }

    pub fn as_map(&self) -> &BTreeMap<Symbol, Expr> {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Serialize for FixedRuleOptions {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        self.entries.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for FixedRuleOptions {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let raw = BTreeMap::<Symbol, Expr>::deserialize(deserializer)?;
        Self::from_entries(raw).map_err(D::Error::custom)
    }
}

/// A fixed rule applied in rule position: name + arity declaration.
///
/// One arity authority: the declaration field. The engine checks the live
/// impl against it at bind time. No `Arc<dyn FixedRule>`. Options are a
/// typed [`FixedRuleOptions`] bag — unknown names unconstructible.
#[derive(Clone, serde_derive::Serialize, serde_derive::Deserialize)]
pub struct FixedRuleApply {
    pub fixed_handle: FixedRuleHandle,
    pub rule_args: Vec<FixedRuleArg>,
    pub options: FixedRuleOptions,
    pub head: Vec<Symbol>,
    /// Declaration arity — the one authority; [`Self::arity`] returns it.
    pub arity: usize,
    #[serde(skip)]
    pub span: SourceSpan,
    #[serde(skip)]
    pub trivia: Trivia,
}

impl FixedRuleApply {
    /// The declaration arity — one authority, never recomputed from an impl.
    pub fn arity(&self) -> Result<usize> {
        Ok(self.arity)
    }
}

impl Debug for FixedRuleApply {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FixedRuleApply")
            .field("name", &self.fixed_handle.name)
            .field("rules", &self.rule_args)
            .field("options", &self.options)
            .field("arity", &self.arity)
            .finish()
    }
}

/// A positional argument to a fixed rule: an in-memory rule, a stored
/// relation, or a stored relation addressed by named fields.
#[derive(Clone, serde_derive::Serialize, serde_derive::Deserialize)]
pub enum FixedRuleArg {
    InMem {
        name: Symbol,
        bindings: Vec<Symbol>,
        #[serde(skip)]
        span: SourceSpan,
    },
    Stored {
        name: Symbol,
        bindings: Vec<Symbol>,
        as_of: Option<AsOf>,
        #[serde(skip)]
        span: SourceSpan,
    },
    NamedStored {
        name: Symbol,
        bindings: BTreeMap<Symbol, Symbol>,
        as_of: Option<AsOf>,
        #[serde(skip)]
        span: SourceSpan,
    },
}

impl Debug for FixedRuleArg {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self}")
    }
}

impl Display for FixedRuleArg {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            FixedRuleArg::InMem { name, bindings, .. } => {
                write!(f, "{name}")?;
                f.debug_list().entries(bindings).finish()?;
            }
            FixedRuleArg::Stored { name, bindings, .. } => {
                write!(f, ":{name}")?;
                f.debug_list().entries(bindings).finish()?;
            }
            FixedRuleArg::NamedStored { name, bindings, .. } => {
                write!(f, "*")?;
                let mut sf = f.debug_struct(name);
                for (k, v) in bindings {
                    sf.field(k, v);
                }
                sf.finish()?;
            }
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Atoms
// ─────────────────────────────────────────────────────────────────────────

/// A body atom as parsed: still sugared (conjunctions, disjunctions,
/// negations, named-field relations, index searches). Normalization lives
/// in exec/plan.
#[derive(Clone, serde_derive::Serialize, serde_derive::Deserialize)]
pub enum InputAtom {
    Rule {
        inner: InputRuleApplyAtom,
    },
    NamedFieldRelation {
        inner: InputNamedFieldRelationApplyAtom,
    },
    Relation {
        inner: InputRelationApplyAtom,
    },
    Predicate {
        inner: Expr,
    },
    Negation {
        inner: Box<InputAtom>,
        #[serde(skip)]
        span: SourceSpan,
    },
    Conjunction {
        inner: Vec<InputAtom>,
        #[serde(skip)]
        span: SourceSpan,
    },
    Disjunction {
        inner: Vec<InputAtom>,
        #[serde(skip)]
        span: SourceSpan,
    },
    /// `x = y` or `x in y`
    Unification {
        inner: Unification,
    },
    /// An index search (`~rel:idx{...}`) as an ordinary relation on the
    /// Candidates&lt;K&gt; seam (seat 14): joinable, filterable, and
    /// **negatable-shaped** — it sits under [`InputAtom::Negation`] like any
    /// other body atom. Engine-side `NegatedSearchUnsupported` retirement is
    /// [OPEN] to #209; spatial/sparse `SearchConfig` variants [OPEN] to
    /// #207/#209.
    Search {
        inner: SearchInput,
    },
}

impl InputAtom {
    pub fn span(&self) -> SourceSpan {
        match self {
            InputAtom::Negation { span, .. }
            | InputAtom::Conjunction { span, .. }
            | InputAtom::Disjunction { span, .. } => *span,
            InputAtom::Rule { inner, .. } => inner.span,
            InputAtom::NamedFieldRelation { inner, .. } => inner.span,
            InputAtom::Relation { inner, .. } => inner.span,
            InputAtom::Predicate { inner, .. } => inner.span(),
            InputAtom::Unification { inner, .. } => inner.span,
            InputAtom::Search { inner, .. } => inner.span,
        }
    }
}

impl Debug for InputAtom {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self}")
    }
}

impl Display for InputAtom {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            InputAtom::Rule {
                inner: InputRuleApplyAtom { name, args, .. },
            } => {
                write!(f, "{name}")?;
                f.debug_list().entries(args).finish()?;
            }
            InputAtom::NamedFieldRelation {
                inner: InputNamedFieldRelationApplyAtom { name, args, .. },
            } => {
                f.write_str("*")?;
                let mut sf = f.debug_struct(name);
                for (k, v) in args {
                    sf.field(k, v);
                }
                sf.finish()?;
            }
            InputAtom::Relation {
                inner: InputRelationApplyAtom { name, args, .. },
            } => {
                write!(f, ":{name}")?;
                f.debug_list().entries(args).finish()?;
            }
            InputAtom::Search { inner } => {
                write!(f, "~{}:{}{{", inner.relation, inner.index)?;
                for (binding, expr) in &inner.bindings {
                    write!(f, "{binding}: {expr}, ")?;
                }
                write!(f, "| query: {}", inner.query)?;
                match &inner.filter {
                    SearchFilter::Unfiltered => {}
                    SearchFilter::Pred(pred) => write!(f, ", filter: {pred}")?,
                }
                for (k, v) in inner.modality.iter() {
                    write!(f, ", {k}: {v}")?;
                }
                write!(f, "}}")?;
            }
            InputAtom::Predicate { inner } => {
                write!(f, "{inner}")?;
            }
            InputAtom::Negation { inner, .. } => {
                write!(f, "not {inner}")?;
            }
            InputAtom::Conjunction { inner, .. } => {
                for (i, a) in inner.iter().enumerate() {
                    if i > 0 {
                        write!(f, " and ")?;
                    }
                    write!(f, "({a})")?;
                }
            }
            InputAtom::Disjunction { inner, .. } => {
                for (i, a) in inner.iter().enumerate() {
                    if i > 0 {
                        write!(f, " or ")?;
                    }
                    write!(f, "({a})")?;
                }
            }
            InputAtom::Unification {
                inner:
                    Unification {
                        binding,
                        expr,
                        one_many_unif,
                        ..
                    },
            } => {
                write!(f, "{binding}")?;
                if *one_many_unif {
                    write!(f, " in ")?;
                } else {
                    write!(f, " = ")?;
                }
                write!(f, "{expr}")?;
            }
        }
        Ok(())
    }
}

/// Residual filter over search hits — named absence, not `Option<Expr>`.
#[derive(Clone, Debug, PartialEq, Eq, serde_derive::Serialize, serde_derive::Deserialize)]
pub enum SearchFilter {
    /// No residual predicate — every Candidates hit is admitted.
    Unfiltered,
    /// Predicate over the full output row (parent ++ own bindings).
    Pred(Expr),
}

impl SearchFilter {
    pub fn as_expr(&self) -> Option<&Expr> {
        match self {
            SearchFilter::Unfiltered => None,
            SearchFilter::Pred(e) => Some(e),
        }
    }
}

/// Search atom missing its required `query` argument.
#[derive(Debug, Clone, PartialEq, Eq, Error, Diagnostic)]
#[error("search atom requires a `query` argument")]
#[diagnostic(code(parser::search_query_required))]
#[diagnostic(help(
    "supply `query: <expr>` — any body-bound Expr is legal (correlated \
     subquery), not only a script `$param`"
))]
pub struct SearchQueryRequired {
    #[label]
    pub span: SourceSpan,
}

/// Unknown / misspelled search modality option — refused at construction.
#[derive(Debug, Clone, PartialEq, Eq, Error, Diagnostic)]
#[error("unknown search modality option '{name}'")]
#[diagnostic(code(parser::unknown_search_modality_option))]
#[diagnostic(help(
    "Modality option names are an extensible closed vocabulary; unknown \
     names are unconstructible. `query` and `filter` are first-class fields, \
     not modality options. Spatial/sparse option names are [OPEN] to #207/#209."
))]
pub struct UnknownSearchModalityOption {
    pub name: String,
    #[label]
    pub span: SourceSpan,
}

/// Proven search modality options bag: every key resolved through
/// [`resolve_search_modality_option`]. Identity keys are [`Symbol`]; an
/// unknown name cannot enter this type.
///
/// Extensible by appending `SEARCH_OPT_*` in `program/op.rs` — spatial /
/// sparse names [OPEN] to #207/#209 when engine `SearchConfig` grows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchModalityOptions {
    entries: BTreeMap<Symbol, Expr>,
}

impl SearchModalityOptions {
    pub fn empty() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Build from Symbol-keyed pairs; refuses any key that does not resolve.
    pub fn from_entries(
        entries: impl IntoIterator<Item = (Symbol, Expr)>,
    ) -> std::result::Result<Self, UnknownSearchModalityOption> {
        let mut opts = Self::empty();
        for (name, value) in entries {
            opts.insert(name, value)?;
        }
        Ok(opts)
    }

    /// Insert one option. Unknown names are unconstructible here.
    pub fn insert(
        &mut self,
        name: Symbol,
        value: Expr,
    ) -> std::result::Result<(), UnknownSearchModalityOption> {
        if resolve_search_modality_option(&name).is_none() {
            return Err(UnknownSearchModalityOption {
                name: name.name.to_string(),
                span: name.span,
            });
        }
        self.entries.insert(name, value);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&Expr> {
        self.entries.get(&Symbol::new(name, SourceSpan::default()))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Symbol, &Expr)> {
        self.entries.iter()
    }

    pub fn as_map(&self) -> &BTreeMap<Symbol, Expr> {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn into_map(self) -> BTreeMap<Symbol, Expr> {
        self.entries
    }
}

impl Serialize for SearchModalityOptions {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        self.entries.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SearchModalityOptions {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let raw = BTreeMap::<Symbol, Expr>::deserialize(deserializer)?;
        Self::from_entries(raw).map_err(D::Error::custom)
    }
}

/// Construction refusal for [`SearchInput::from_named_parts`].
#[derive(Debug, Clone, PartialEq, Eq, Error, Diagnostic)]
pub enum SearchAtomConstructRefuse {
    #[error(transparent)]
    #[diagnostic(transparent)]
    QueryRequired(#[from] SearchQueryRequired),
    #[error(transparent)]
    #[diagnostic(transparent)]
    UnknownModality(#[from] UnknownSearchModalityOption),
}

/// Index search atom as an ordinary relation on the Candidates&lt;K&gt; seam
/// (seat 14).
///
/// - **query** — first-class body-bound [`Expr`] (correlated subquery;
///   `$param`-only shape retired)
/// - **modality** — extensible closed vocabulary ([`SearchModalityOptions`]);
///   spatial/sparse names [OPEN] to #207/#209 (engine `SearchConfig` variants)
/// - **negatable-shaped** — sits under [`InputAtom::Negation`] like any
///   relation; engine `NegatedSearchUnsupported` retirement [OPEN] to #209
#[derive(Clone, serde_derive::Serialize, serde_derive::Deserialize)]
pub struct SearchInput {
    pub relation: Symbol,
    pub index: Symbol,
    pub bindings: BTreeMap<Symbol, Expr>,
    /// Body-bound query argument (correlated subquery allowed).
    pub query: Expr,
    pub filter: SearchFilter,
    pub modality: SearchModalityOptions,
    #[serde(skip)]
    pub span: SourceSpan,
}

impl SearchInput {
    /// Lift from a named-parameter bag (parser surface): peels required
    /// `query` and optional `filter` into first-class fields; remaining keys
    /// enter the modality bag (unknown names unconstructible).
    ///
    /// Construction site: `parse/query.rs` `search_apply`.
    pub fn from_named_parts(
        relation: Symbol,
        index: Symbol,
        bindings: BTreeMap<Symbol, Expr>,
        mut parameters: BTreeMap<Symbol, Expr>,
        span: SourceSpan,
    ) -> std::result::Result<Self, SearchAtomConstructRefuse> {
        let query = parameters.remove(&Symbol::new("query", span)).ok_or(
            SearchAtomConstructRefuse::QueryRequired(SearchQueryRequired { span }),
        )?;
        let filter = match parameters.remove(&Symbol::new("filter", span)) {
            None => SearchFilter::Unfiltered,
            Some(e) => SearchFilter::Pred(e),
        };
        let modality = SearchModalityOptions::from_entries(parameters)
            .map_err(SearchAtomConstructRefuse::UnknownModality)?;
        Ok(Self {
            relation,
            index,
            bindings,
            query,
            filter,
            modality,
            span,
        })
    }
}

/// A rule application in a parsed body: `name[args…]` with expression args.
#[derive(Clone, Debug, serde_derive::Serialize, serde_derive::Deserialize)]
pub struct InputRuleApplyAtom {
    pub name: Symbol,
    pub args: Vec<Expr>,
    #[serde(skip)]
    pub span: SourceSpan,
}

/// Which axis an [`ValidityClause::Delta`] varies, the other held at the
/// record's current belief.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde_derive::Serialize, serde_derive::Deserialize)]
pub enum DeltaAxis {
    /// `@delta(a, b)`: valid-time net diff at the current system snapshot.
    Valid,
    /// `@delta_sys(a, b)`: system-time net diff at the current valid instant.
    Sys,
}

/// A stored-relation atom's trailing `@` clause, in the ONE grammar seat.
#[derive(Clone, Debug, serde_derive::Serialize, serde_derive::Deserialize)]
pub enum ValidityClause {
    /// `@ expr` (one or two coordinates): resolve at this bitemporal coordinate.
    At(AsOf),
    /// `@spans var[, sys_expr]`: derive maximal equal-payload half-open runs.
    Spans { sys: ValidityTs, var: Symbol },
    /// `@delta(a, b) var` / `@delta_sys(a, b) var`: axis-parameterized net diff.
    Delta {
        axis: DeltaAxis,
        from: ValidityTs,
        to: ValidityTs,
        var: Symbol,
    },
}

impl ValidityClause {
    /// The one extra binding this clause produces beyond the atom's own
    /// `args` — `None` for the plain point-in-time read.
    pub fn extra_var(&self) -> Option<&Symbol> {
        match self {
            ValidityClause::At(_) => None,
            ValidityClause::Spans { var, .. } | ValidityClause::Delta { var, .. } => Some(var),
        }
    }
}

/// A stored-relation application addressed by named fields:
/// `*name{field: expr, …}`. Field names are [`Symbol`] keys.
#[derive(Clone, Debug, serde_derive::Serialize, serde_derive::Deserialize)]
pub struct InputNamedFieldRelationApplyAtom {
    pub name: Symbol,
    pub args: BTreeMap<Symbol, Expr>,
    pub validity: Option<ValidityClause>,
    #[serde(skip)]
    pub span: SourceSpan,
}

/// A stored-relation application with positional args: `*name[args…]`.
#[derive(Clone, Debug, serde_derive::Serialize, serde_derive::Deserialize)]
pub struct InputRelationApplyAtom {
    pub name: Symbol,
    pub args: Vec<Expr>,
    pub validity: Option<ValidityClause>,
    #[serde(skip)]
    pub span: SourceSpan,
}

/// `binding = expr` (or `binding in expr` when `one_many_unif`).
#[derive(Clone, Debug, serde_derive::Serialize, serde_derive::Deserialize)]
pub struct Unification {
    pub binding: Symbol,
    pub expr: Expr,
    /// If false, `=`; if true, `in`.
    pub one_many_unif: bool,
    #[serde(skip)]
    pub span: SourceSpan,
}

impl Unification {
    pub fn is_const(&self) -> bool {
        matches!(self.expr, Expr::Const { .. })
    }
    pub fn bindings_in_expr(&self) -> Result<BTreeSet<Symbol>> {
        self.expr.bindings()
    }
}

// ─────────────────────────────────────────────────────────────────────────
// InputProgram
// ─────────────────────────────────────────────────────────────────────────

/// A single query as parsed. The entry rule `?` is a **field**: constructing
/// an [`InputProgram`] proves the query has an answer relation.
///
/// Normalization (`into_normalized_program`) lives in exec/plan — omitted here.
#[derive(Debug, Clone, serde_derive::Serialize)]
pub struct InputProgram {
    entry_name: Symbol,
    entry: InputInlineRulesOrFixed,
    rules: BTreeMap<Symbol, InputInlineRulesOrFixed>,
    out_opts: QueryOutOptions,
    disable_magic_rewrite: bool,
    #[serde(skip)]
    pub leading_trivia: Vec<Comment>,
    #[serde(skip)]
    pub trailing_trivia: Vec<Comment>,
}

#[derive(serde_derive::Deserialize)]
struct InputProgramWire {
    entry_name: Symbol,
    entry: InputInlineRulesOrFixed,
    rules: BTreeMap<Symbol, InputInlineRulesOrFixed>,
    out_opts: QueryOutOptions,
    disable_magic_rewrite: bool,
}

impl<'de> serde::Deserialize<'de> for InputProgram {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        use serde::de::Error as _;
        let wire = InputProgramWire::deserialize(deserializer)?;
        let mut rules = wire.rules;
        rules.insert(wire.entry_name, wire.entry);
        InputProgram::new(rules, wire.out_opts, wire.disable_magic_rewrite)
            .map_err(D::Error::custom)
    }
}

/// Every rule clause in `ruleset`, pushed onto `anchors` as its span start
/// paired with a mutable handle to where its trivia lives.
pub fn collect_trivia_anchors<'a>(
    ruleset: &'a mut InputInlineRulesOrFixed,
    anchors: &mut Vec<(usize, &'a mut Trivia)>,
) {
    match ruleset {
        InputInlineRulesOrFixed::Rules { rules } => {
            for rule in rules {
                anchors.push((rule.span.0, &mut rule.trivia));
            }
        }
        InputInlineRulesOrFixed::Fixed { fixed } => {
            anchors.push((fixed.span.0, &mut fixed.trivia));
        }
    }
}

/// Does `offset` share its source line with real content already written
/// before it — scanning backward from `offset` over spaces/tabs only.
pub fn shares_a_line_with_preceding_content(src: &str, offset: usize) -> bool {
    matches!(
        src[..offset.min(src.len())]
            .bytes()
            .rev()
            .find(|&b| b != b' ' && b != b'\t'),
        Some(b) if b != b'\n'
    )
}

impl InputProgram {
    pub fn insert_rule(&mut self, name: Symbol, def: InputInlineRulesOrFixed) {
        self.rules.insert(name, def);
    }

    /// The one way to make a program. Proves an entry (`?`) and that no
    /// rule set is empty.
    pub fn new(
        mut prog: BTreeMap<Symbol, InputInlineRulesOrFixed>,
        out_opts: QueryOutOptions,
        disable_magic_rewrite: bool,
    ) -> Result<Self> {
        for (name, ruleset) in prog.iter() {
            if let InputInlineRulesOrFixed::Rules { rules } = ruleset
                && rules.is_empty()
            {
                bail!(EmptyRuleSet(name.to_string(), name.span));
            }
        }
        let (entry_name, entry) = prog
            .remove_entry(&Symbol::prog_entry(SourceSpan::default()))
            .ok_or_else(|| NoEntry(Some(prog.keys().next().map(|s| s.span).unwrap_or_default())))?;
        Ok(Self {
            entry_name,
            entry,
            rules: prog,
            out_opts,
            disable_magic_rewrite,
            leading_trivia: Vec::new(),
            trailing_trivia: Vec::new(),
        })
    }

    /// Attach every comment found in the same source text this program was
    /// parsed from — the one place "leading" and "trailing" are decided.
    ///
    /// Anchors are ordered by START only: pest optional trailing `";"?`
    /// probing can stretch a clause span past a same-line comment.
    pub fn attach_comment_trivia(&mut self, src: &str, comments: Vec<Comment>) {
        if comments.is_empty() {
            return;
        }
        let mut anchors: Vec<(usize, &mut Trivia)> = Vec::new();
        collect_trivia_anchors(&mut self.entry, &mut anchors);
        for ruleset in self.rules.values_mut() {
            collect_trivia_anchors(ruleset, &mut anchors);
        }
        anchors.sort_by_key(|(start, _)| *start);

        for comment in comments {
            if shares_a_line_with_preceding_content(src, comment.span.0) {
                if let Some(idx) = anchors
                    .iter()
                    .rposition(|(start, _)| *start <= comment.span.0)
                {
                    anchors[idx].1.trailing.push(comment);
                    continue;
                }
                self.trailing_trivia.push(comment);
                continue;
            }
            match anchors
                .iter()
                .position(|(start, _)| *start > comment.span.0)
            {
                Some(idx) => anchors[idx].1.leading.push(comment),
                None => self.trailing_trivia.push(comment),
            }
        }
    }

    pub fn entry_name(&self) -> &Symbol {
        &self.entry_name
    }

    pub fn entry(&self) -> &InputInlineRulesOrFixed {
        &self.entry
    }

    pub fn rules(&self) -> &BTreeMap<Symbol, InputInlineRulesOrFixed> {
        &self.rules
    }

    /// Every definition in the program, the entry included.
    pub fn iter_all(&self) -> impl Iterator<Item = (&Symbol, &InputInlineRulesOrFixed)> {
        self.rules
            .iter()
            .chain(std::iter::once((&self.entry_name, &self.entry)))
    }

    pub fn out_opts(&self) -> &QueryOutOptions {
        &self.out_opts
    }

    pub fn out_opts_mut(&mut self) -> &mut QueryOutOptions {
        &mut self.out_opts
    }

    pub fn disable_magic_rewrite(&self) -> bool {
        self.disable_magic_rewrite
    }

    /// The stored relation this query needs a write lock on, if any.
    pub fn needs_write_lock(&self) -> Option<SmartString<LazyCompact>> {
        if let Some((h, _, _, _)) = &self.out_opts.store_relation {
            if !h.name.is_temp_relation_name() {
                Some(h.name.name.clone())
            } else {
                None
            }
        } else {
            None
        }
    }

    /// The entry's output arity.
    pub fn get_entry_arity(&self) -> Result<usize> {
        match &self.entry {
            InputInlineRulesOrFixed::Rules { rules } => match rules.last() {
                Some(rule) => Ok(rule.head.len()),
                None => bail!(TierInvariant("entry rule set is empty")),
            },
            InputInlineRulesOrFixed::Fixed { fixed } => fixed.arity(),
        }
    }

    /// The entry's output header, or `_0.._n` defaults when it has none.
    pub fn get_entry_out_head_or_default(&self) -> Result<Vec<Symbol>> {
        match self.get_entry_out_head() {
            Ok(r) => Ok(r),
            Err(_) => {
                let arity = self.get_entry_arity()?;
                Ok((0..arity)
                    .map(|i| Symbol::new(format!("_{i}"), self.entry_name.span))
                    .collect())
            }
        }
    }

    /// The entry's output header. Aggregated positions render as `aggr(binding)`.
    pub fn get_entry_out_head(&self) -> Result<Vec<Symbol>> {
        match &self.entry {
            InputInlineRulesOrFixed::Rules { rules } => {
                let last_rule = match rules.last() {
                    Some(rule) => rule,
                    None => bail!(TierInvariant("entry rule set is empty")),
                };
                let mut ret = Vec::with_capacity(last_rule.head.len());
                for (symb, aggr) in last_rule.head.iter().zip(last_rule.aggr.iter()) {
                    if let Some((aggr, _)) = aggr.as_aggregated() {
                        ret.push(Symbol::new(format!("{}({})", aggr.name, symb), symb.span))
                    } else {
                        ret.push(symb.clone())
                    }
                }
                Ok(ret)
            }
            InputInlineRulesOrFixed::Fixed { fixed } => {
                if fixed.head.is_empty() {
                    bail!(EntryHeadNotExplicitlyDefined(
                        self.entry.first_span().unwrap_or(self.entry_name.span)
                    ))
                } else {
                    Ok(fixed.head.to_vec())
                }
            }
        }
    }

    // `into_normalized_program` omitted — NormalForm types and BodyNormalizer
    // live in exec/plan; this seat is input-tier only.
}

impl Display for InputProgram {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        for (name, rules) in self.iter_all() {
            match rules {
                InputInlineRulesOrFixed::Rules { rules, .. } => {
                    for InputInlineRule {
                        head, aggr, body, ..
                    } in rules
                    {
                        write!(f, "{name}[")?;

                        for (i, (h, a)) in head.iter().zip(aggr).enumerate() {
                            if i > 0 {
                                write!(f, ", ")?;
                            }
                            if let Some((aggr, aggr_args)) = a.as_aggregated() {
                                write!(f, "{}({}", aggr.name, h)?;
                                for aga in aggr_args {
                                    write!(f, ", {aga}")?;
                                }
                                write!(f, ")")?;
                            } else {
                                write!(f, "{h}")?;
                            }
                        }
                        write!(f, "] := ")?;
                        for (i, atom) in body.iter().enumerate() {
                            if i > 0 {
                                write!(f, ", ")?;
                            }
                            write!(f, "{atom}")?;
                        }
                        writeln!(f, ";")?;
                    }
                }
                InputInlineRulesOrFixed::Fixed {
                    fixed:
                        FixedRuleApply {
                            fixed_handle: handle,
                            rule_args,
                            options,
                            head,
                            ..
                        },
                } => {
                    write!(f, "{name}")?;
                    f.debug_list().entries(head).finish()?;
                    write!(f, " <~ ")?;
                    write!(f, "{}(", handle.name)?;
                    let mut first = true;
                    for rule_arg in rule_args {
                        if first {
                            first = false;
                        } else {
                            write!(f, ", ")?;
                        }
                        write!(f, "{rule_arg}")?;
                    }
                    for (k, v) in options.iter() {
                        if first {
                            first = false;
                        } else {
                            write!(f, ", ")?;
                        }
                        write!(f, "{k}: {v}")?;
                    }
                    writeln!(f, ");")?;
                }
            }
        }
        write!(f, "{}", self.out_opts)?;
        Ok(())
    }
}
