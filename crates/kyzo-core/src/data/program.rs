/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): the entry rule (`?`) is a struct field proven at construction,
 * not a map-key convention reconstructed with dummy spans; the program tiers
 * have private fields and are minted only by their transformations; the
 * stratified tiers store strata in execution order (the original emitted
 * them reversed, un-reversed them in `compile.rs` with `.rev()`, and trusted
 * position in `eval.rs` — a three-file convention); store lifetimes are a
 * documented type, not a bare `BTreeMap<_, usize>`; normalization's
 * transaction-facing parts (DNF search resolution, well-ordering) sit behind
 * the [`BodyNormalizer`] seam so the total desugar is total; aggregation
 * names are used directly (the original's `strip_prefix("AGGR_").unwrap()`
 * would panic against the ported aggregations); the map-lookup and
 * empty-ruleset `unwrap`s are errors; `InputRelationHandle` is re-homed here
 * from the original's `runtime/relation.rs` (it is the *declared* output
 * relation of a query — parse-tier substance, all of its fields data-tier);
 * adornments are `Vec<bool>` (no `smallvec` dependency); the resolved index
 * search atoms (HNSW/FTS/LSH) and the fixed-rule trait's runtime surface
 * land with their owning tiers.
 * Port constraints for later tiers: the parser must synthesize the
 * constant entry rule for body-less `:create` scripts BEFORE calling
 * `InputProgram::new` (the original injected it after construction);
 * Display prints the entry last (the original's map order printed `?`
 * first — cosmetic, nothing re-parses it); `InputRelationHandle.span` is
 * deliberately not serialized (spans are never persisted in KyzoDB);
 * `MagicInlineRule::contained_rules` lands with the compile tier, which
 * owns the occurrence-keyed delta-selection map (`AtomOccurrence`, in
 * `query/eval.rs`) it returns. The original's name-keyed
 * `ContainedRuleMultiplicity` would collapse a store mentioned twice in one
 * body to one `Many` entry and lose all delta narrowing. This implementation
 * uses one entry per body position, so repeated occurrences of the same store
 * are each independently delta-selectable.
 */

//! The program tiers: what a query *is* at each stage of compilation.
//!
//! A KyzoScript query moves through a pipeline of representations, and each
//! representation is its own type family. The types are typestate: **a value
//! of a tier type is proof that its stage's checks passed**, because each
//! tier can only be minted by the transformation that performs those checks.
//! An unstratifiable program never becomes an evaluable value; a program
//! without an entry never becomes a program at all.
//!
//! - [`InputProgram`] — the query as parsed: sugared atoms (disjunctions,
//!   negations, named-field relations, index searches), rules under their
//!   names, and the entry rule `?` held as a **field** — constructing an
//!   `InputProgram` proves the query has an answer relation.
//! - [`NormalFormProgram`] — after desugaring and normalization: every rule
//!   body is a flat conjunction of [`NormalFormAtom`]s, well-ordered for
//!   binding safety. Minted only by
//!   [`InputProgram::into_normalized_program`].
//! - [`StratifiedNormalFormProgram`] — after stratification: the rules are
//!   split into strata stored **in execution order**, and negation or
//!   non-meet aggregation through a recursive cycle has been proven absent.
//!   Minted by the stratifier via
//!   [`StratifiedNormalFormProgram::from_reverse_execution_order`].
//! - [`StratifiedMagicProgram`] — after the magic-sets demand rewrite:
//!   rules are adorned ([`MagicSymbol`]) so evaluation computes only facts
//!   the entry actually demands. Demand changes; result semantics may not.
//!
//! Downstream, the compile tier turns the magic tier into relational algebra
//! and the runtime evaluates it; those stages consume these types but cannot
//! create them.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Debug, Display, Formatter};
use std::sync::Arc;

use miette::{Diagnostic, Result, bail};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::aggr::Aggregation;
use crate::data::expr::Expr;
use crate::data::relation::StoredRelationMetadata;
use crate::data::span::SourceSpan;
use crate::data::symb::{Symbol, SymbolKind};
use crate::data::value::{AsOf, DataValue, ValidityTs};

// The fixed-rule tier has landed: its trait and handle live in
// `fixed_rule/mod.rs` (the former seam declarations here re-homed there
// and the trait grew its runtime surface, `run`). Re-exported so the
// program tier's consumers keep their import path.
pub(crate) use crate::fixed_rule::{FixedRule, FixedRuleHandle};

// ─────────────────────────────────────────────────────────────────────────
// Query output options
// ─────────────────────────────────────────────────────────────────────────

/// A `:assert none` / `:assert some` clause: the query fails unless its
/// result set is empty / non-empty.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum QueryAssertion {
    AssertNone(SourceSpan),
    AssertSome(SourceSpan),
}

/// Whether a mutating query reports the mutated rows back (`:returning`).
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) enum ReturnMutation {
    NotReturning,
    Returning,
}

/// Sort direction in an `:order` clause.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) enum SortDir {
    Asc,
    Dsc,
}

/// What a query does to its output stored relation.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) enum RelationOp {
    Create,
    Replace,
    Put,
    Insert,
    Update,
    Rm,
    Delete,
    Ensure,
    EnsureNot,
}

/// The valid-time coordinate a mutation's rows are asserted at — the write
/// side's `@` clause. There is no system-time counterpart here by design:
/// the system coordinate is always the committing transaction's own
/// engine-minted stamp (`SessionTx::system_stamp_routed`); a script has no
/// syntax to set it, which is what keeps "system time" meaning "when the
/// database learned this" rather than something a writer can forge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WriteValidity {
    /// No `@` clause: every row lands at the transaction's own system
    /// stamp — byte-for-byte the pre-`@` behavior.
    Now,
    /// `@ <constant>`: one valid instant for every row this mutation
    /// writes, resolved once at parse time exactly like the read side's
    /// single-coordinate `@`.
    Fixed(ValidityTs),
    /// `@ <expr over one of this mutation's own output columns>`: each row
    /// supplies its own valid instant, extracted per row like any other
    /// column — the backfill/import case, where every row carries its own
    /// timestamp.
    PerRow(Expr),
}

impl WriteValidity {
    /// Resolve this mutation's valid coordinate for one row: `Now` is the
    /// transaction's own system stamp (untouched pre-`@` behavior), `Fixed`
    /// is the same instant for every row, and `PerRow` evaluates its
    /// expression against THIS row exactly like any other column
    /// extractor.
    pub(crate) fn resolve(
        &self,
        row: &[DataValue],
        stamp: ValidityTs,
        cur_vld: ValidityTs,
    ) -> Result<ValidityTs> {
        match self {
            WriteValidity::Now => Ok(stamp),
            WriteValidity::Fixed(v) => Ok(*v),
            WriteValidity::PerRow(expr) => {
                let span = expr.span();
                let val = expr.eval(row)?;
                let vld = crate::data::functions::data_value_to_vld_spec(val, span, cur_vld)?;
                // `parse::query::resolve_write_validity` proves the same
                // thing for the `Fixed` coordinate at parse time, but a
                // `PerRow` clause's instant comes out of THIS row's own
                // data — parse time only proved the expression names one
                // of the mutation's output columns, never what value that
                // column will hold for any given row. Re-prove it here,
                // per row, through the same smart constructor: a
                // user-asserted write validity can never be the reserved
                // terminal tick (`i64::MAX` / `'END'`), the instant every
                // open-end sentinel and derived interval reads as "still open."
                crate::data::value::ValidityTs::for_assertion(vld.raw()).ok_or_else(|| {
                    miette::miette!(
                        labels = vec![miette::LabeledSpan::underline(span)],
                        "a write validity cannot be the reserved terminal tick (i64::MAX / 'END')"
                    )
                })
            }
        }
    }
}

/// The output stored relation as the query *declares* it: name, declared
/// schema, and which head bindings feed the key and non-key columns.
///
/// Re-homed from the CozoDB original's `runtime/relation.rs`: this is
/// parse-tier substance (every field is data-tier), distinct from the
/// runtime's resolved relation handle.
#[derive(Debug, Clone, Eq, PartialEq, serde_derive::Serialize, serde_derive::Deserialize)]
pub(crate) struct InputRelationHandle {
    pub(crate) name: Symbol,
    pub(crate) metadata: StoredRelationMetadata,
    pub(crate) key_bindings: Vec<Symbol>,
    pub(crate) dep_bindings: Vec<Symbol>,
    #[serde(skip)]
    pub(crate) span: SourceSpan,
}

/// The `:option`s of a query: limit/offset, timeout, ordering, the output
/// relation (if the query writes one), and assertions.
///
/// Fields are `pub(crate)`: the parser assembles these incrementally and the
/// runtime reads them piecemeal; they carry no cross-field invariant that a
/// constructor could prove.
#[derive(Clone, PartialEq, Default)]
pub(crate) struct QueryOutOptions {
    pub(crate) limit: Option<usize>,
    pub(crate) offset: Option<usize>,
    /// Terminate query with an error if it exceeds this many seconds.
    pub(crate) timeout: Option<f64>,
    /// Sleep after performing the query for this number of seconds. Ignored in WASM.
    pub(crate) sleep: Option<f64>,
    pub(crate) sorters: Vec<(Symbol, SortDir)>,
    pub(crate) store_relation: Option<(
        InputRelationHandle,
        RelationOp,
        ReturnMutation,
        WriteValidity,
    )>,
    pub(crate) assertion: Option<QueryAssertion>,
}

impl Debug for QueryOutOptions {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self}")
    }
}

impl Display for QueryOutOptions {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if let Some(l) = self.limit {
            writeln!(f, ":limit {l};")?;
        }
        if let Some(l) = self.offset {
            writeln!(f, ":offset {l};")?;
        }
        if let Some(l) = self.timeout {
            writeln!(f, ":timeout {l};")?;
        }
        for (symb, dir) in &self.sorters {
            write!(f, ":order ")?;
            if *dir == SortDir::Dsc {
                write!(f, "-")?;
            }
            writeln!(f, "{symb};")?;
        }
        if let Some((
            InputRelationHandle {
                name,
                metadata: StoredRelationMetadata { keys, non_keys },
                key_bindings,
                dep_bindings,
                ..
            },
            op,
            return_mutation,
            write_vld,
        )) = &self.store_relation
        {
            if *return_mutation == ReturnMutation::Returning {
                writeln!(f, ":returning")?;
            }
            match op {
                RelationOp::Create => {
                    write!(f, ":create ")?;
                }
                RelationOp::Replace => {
                    write!(f, ":replace ")?;
                }
                RelationOp::Insert => {
                    write!(f, ":insert ")?;
                }
                RelationOp::Put => {
                    write!(f, ":put ")?;
                }
                RelationOp::Update => {
                    write!(f, ":update ")?;
                }
                RelationOp::Rm => {
                    write!(f, ":rm ")?;
                }
                RelationOp::Delete => {
                    write!(f, ":delete ")?;
                }
                RelationOp::Ensure => {
                    write!(f, ":ensure ")?;
                }
                RelationOp::EnsureNot => {
                    write!(f, ":ensure_not ")?;
                }
            }
            write!(f, "{name} {{")?;
            let mut is_first = true;
            for (col, bind) in keys.iter().zip(key_bindings) {
                if is_first {
                    is_first = false
                } else {
                    write!(f, ", ")?;
                }
                write!(f, "{}: {}", col.name, col.typing)?;
                if let Some(generator) = &col.default_gen {
                    write!(f, " default {generator}")?;
                } else {
                    write!(f, " = {bind}")?;
                }
            }
            write!(f, " => ")?;
            let mut is_first = true;
            for (col, bind) in non_keys.iter().zip(dep_bindings) {
                if is_first {
                    is_first = false
                } else {
                    write!(f, ", ")?;
                }
                write!(f, "{}: {}", col.name, col.typing)?;
                if let Some(generator) = &col.default_gen {
                    write!(f, " default {generator}")?;
                } else {
                    write!(f, " = {bind}")?;
                }
            }
            write!(f, "}}")?;
            match write_vld {
                WriteValidity::Now => {}
                WriteValidity::Fixed(ts) => write!(f, " @ {}", ts.raw())?,
                WriteValidity::PerRow(expr) => write!(f, " @ {expr}")?,
            }
            writeln!(f, ";")?;
        }

        if let Some(a) = &self.assertion {
            match a {
                QueryAssertion::AssertNone(_) => {
                    writeln!(f, ":assert none;")?;
                }
                QueryAssertion::AssertSome(_) => {
                    writeln!(f, ":assert some;")?;
                }
            }
        }

        Ok(())
    }
}

impl QueryOutOptions {
    /// How many rows evaluation must produce before it may stop early:
    /// `limit + offset` when both are given.
    pub(crate) fn num_to_take(&self) -> Option<usize> {
        match (self.limit, self.offset) {
            (None, _) => None,
            (Some(i), None) => Some(i),
            (Some(i), Some(j)) => Some(i + j),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Generated symbols
// ─────────────────────────────────────────────────────────────────────────

/// Mints fresh compiler-generated symbols. The prefixes are load-bearing:
/// `*` classifies as [`SymbolKind::Generated`] and `~` as
/// [`SymbolKind::GeneratedIgnored`], and neither is a valid user identifier
/// in the grammar, so generated names can never collide with user names.
#[derive(Default)]
pub(crate) struct TempSymbGen {
    last_id: u32,
}

impl TempSymbGen {
    /// A fresh generated binding (`*n`).
    pub(crate) fn next(&mut self, span: SourceSpan) -> Symbol {
        self.last_id += 1;
        Symbol::new(format!("*{}", self.last_id), span)
    }

    /// A fresh generated *ignored* binding (`~n`): matches anything, binds
    /// nothing downstream.
    pub(crate) fn next_ignored(&mut self, span: SourceSpan) -> Symbol {
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
struct EntryHeadNotExplicitlyDefinedError(#[label] SourceSpan);

/// The query has no `?` rule. In the CozoDB original this surfaced deep in
/// the pipeline (and again at evaluation); here it is primarily a
/// construction error — an entry-less [`InputProgram`] cannot exist.
///
/// The parse-reachable refusal ([`InputProgram::new`]) carries a span:
/// [`Some`] pointing at one of the program's actual rules (or, for an empty
/// program, the input start), so it names *where* an entry should have been.
/// The later-tier sites — stratification / magic-set rewriting in this file,
/// and evaluation in `query/eval.rs` — are defensive and structurally
/// unreachable once [`InputProgram::new`] has proven an entry exists; the
/// offending rules have been transformed away and no rule span survives, so
/// they use [`NoEntryError::spanless`].
#[derive(Debug, Diagnostic, Error)]
#[error("Program has no entry")]
#[diagnostic(code(parser::no_entry))]
#[diagnostic(help("You need to have one rule named '?'"))]
pub(crate) struct NoEntryError(#[label("this program defines no `?` rule")] Option<SourceSpan>);

impl NoEntryError {
    /// The span-less variant for the later-tier defensive sites
    /// (stratification / magic-set rewriting / evaluation), which reach this
    /// error only if the entry proven by [`InputProgram::new`] has been
    /// corrupted and so have no surviving rule span to point at. The
    /// parse-reachable refusal builds the spanned variant directly.
    pub(crate) fn spanless() -> Self {
        NoEntryError(None)
    }
}

/// A named rule set with zero rules. The original indexed `rules[0]` /
/// called `rules.last().unwrap()` on these; rejecting the shape at program
/// construction makes every later `first`/`last` structurally justified.
#[derive(Debug, Diagnostic, Error)]
#[error("The rule set for '{0}' contains no rules")]
#[diagnostic(code(parser::empty_rule_set))]
struct EmptyRuleSetError(String, #[label] SourceSpan);

/// A tier invariant that construction should have made impossible. Returned
/// (never panicked) on the paths whose impossibility is proven elsewhere, so
/// corruption of that proof surfaces as an error, not an abort.
#[derive(Debug, Diagnostic, Error)]
#[error("Program tier invariant violated: {0}")]
#[diagnostic(code(compiler::tier_invariant))]
#[diagnostic(help("This is a bug. Please report it."))]
struct TierInvariantError(&'static str);

// ─────────────────────────────────────────────────────────────────────────
// Input tier
// ─────────────────────────────────────────────────────────────────────────

/// One comment, captured verbatim from source text by `parse::
/// scan_comments` — delimiters included (`#...` or `/* ... */`), so a
/// consumer (the formatter) never re-synthesizes comment syntax, only
/// places the text back where it was attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Comment {
    pub(crate) text: String,
    pub(crate) span: SourceSpan,
}

/// The comments attached to one position in the program: every comment
/// immediately preceding it, each on its own source line with nothing but
/// other leading comments between it and the position (`leading`, in
/// source order), and every comment sharing the position's own last
/// source line, after it (`trailing`, in source order). Attached once, by
/// `InputProgram::attach_comment_trivia`, right after a program is fully
/// parsed — never recomputed by a consumer, so there is exactly one place
/// that decides what "leading" and "trailing" mean.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Trivia {
    pub(crate) leading: Vec<Comment>,
    pub(crate) trailing: Vec<Comment>,
}

/// One parsed inline rule: head bindings (with optional aggregations,
/// index-aligned with the head), and a body of sugared [`InputAtom`]s.
#[derive(Debug, Clone)]
pub(crate) struct InputInlineRule {
    pub(crate) head: Vec<Symbol>,
    pub(crate) aggr: Vec<Option<(Aggregation, Vec<DataValue>)>>,
    pub(crate) body: Vec<InputAtom>,
    pub(crate) span: SourceSpan,
    pub(crate) trivia: Trivia,
}

/// What a name is defined as in a program: a set of inline rules, or a
/// fixed-rule application.
#[derive(Debug, Clone)]
pub(crate) enum InputInlineRulesOrFixed {
    Rules { rules: Vec<InputInlineRule> },
    Fixed { fixed: FixedRuleApply },
}

impl InputInlineRulesOrFixed {
    /// The span of the first clause, for labeling diagnostics. `None` only
    /// for an empty rule set, which [`InputProgram::new`] refuses.
    pub(crate) fn first_span(&self) -> Option<SourceSpan> {
        match self {
            InputInlineRulesOrFixed::Rules { rules, .. } => rules.first().map(|r| r.span),
            InputInlineRulesOrFixed::Fixed { fixed, .. } => Some(fixed.span),
        }
    }
}

/// A fixed rule applied in rule position: `name[...] <~ rule_name(args, opts)`.
///
/// Carries the live implementation (`fixed_impl`) from parse time onward:
/// possession of a `FixedRuleApply` is proof the rule name resolved.
#[derive(Clone)]
pub(crate) struct FixedRuleApply {
    pub(crate) fixed_handle: FixedRuleHandle,
    pub(crate) rule_args: Vec<FixedRuleArg>,
    pub(crate) options: Arc<BTreeMap<SmartString<LazyCompact>, Expr>>,
    pub(crate) head: Vec<Symbol>,
    /// The arity recorded at parse time. [`Self::arity`] recomputes from the
    /// implementation; the CozoDB original carried both, and so does the
    /// port — reconciling them is a fixed-rule-tier decision.
    pub(crate) arity: usize,
    pub(crate) span: SourceSpan,
    pub(crate) fixed_impl: Arc<dyn FixedRule>,
    pub(crate) trivia: Trivia,
}

impl FixedRuleApply {
    pub(crate) fn arity(&self) -> Result<usize> {
        self.fixed_impl.arity(&self.options, &self.head, self.span)
    }
}

impl Debug for FixedRuleApply {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FixedRuleApply")
            .field("name", &self.fixed_handle.name)
            .field("rules", &self.rule_args)
            .field("options", &self.options)
            .finish()
    }
}

/// A positional argument to a fixed rule: an in-memory rule, a stored
/// relation, or a stored relation addressed by named fields.
#[derive(Clone)]
pub(crate) enum FixedRuleArg {
    InMem {
        name: Symbol,
        bindings: Vec<Symbol>,
        span: SourceSpan,
    },
    Stored {
        name: Symbol,
        bindings: Vec<Symbol>,
        as_of: Option<AsOf>,
        span: SourceSpan,
    },
    NamedStored {
        name: Symbol,
        bindings: BTreeMap<SmartString<LazyCompact>, Symbol>,
        as_of: Option<AsOf>,
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

/// A body atom as parsed: still sugared (conjunctions, disjunctions,
/// negations, named-field relations, index searches), normalized away by
/// [`InputProgram::into_normalized_program`].
#[derive(Clone)]
pub(crate) enum InputAtom {
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
        span: SourceSpan,
    },
    Conjunction {
        inner: Vec<InputAtom>,
        span: SourceSpan,
    },
    Disjunction {
        inner: Vec<InputAtom>,
        span: SourceSpan,
    },
    /// `x = y` or `x in y`
    Unification {
        inner: Unification,
    },
    /// An index search (`~rel:idx{...}`), still unresolved: resolution
    /// against the catalog happens behind the [`BodyNormalizer`] seam.
    Search {
        inner: SearchInput,
    },
}

impl InputAtom {
    pub(crate) fn span(&self) -> SourceSpan {
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
                write!(f, "| ")?;
                for (k, v) in inner.parameters.iter() {
                    write!(f, "{k}: {v}, ")?;
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

/// An index search atom as parsed: relation and index names, named-field
/// bindings, and raw parameters. Purely syntactic — the resolved forms
/// (HNSW/FTS/LSH searches holding live relation handles and manifests) are
/// index-tier substances and land with that tier.
#[derive(Clone)]
pub(crate) struct SearchInput {
    pub(crate) relation: Symbol,
    pub(crate) index: Symbol,
    pub(crate) bindings: BTreeMap<SmartString<LazyCompact>, Expr>,
    pub(crate) parameters: BTreeMap<SmartString<LazyCompact>, Expr>,
    pub(crate) span: SourceSpan,
}

/// A rule application in a parsed body: `name[args…]` with expression args.
#[derive(Clone, Debug)]
pub(crate) struct InputRuleApplyAtom {
    pub(crate) name: Symbol,
    pub(crate) args: Vec<Expr>,
    pub(crate) span: SourceSpan,
}

/// Which axis an [`ValidityClause::Delta`] varies, the other held at the
/// record's current belief — mirrors [`crate::query::laws::Axis`], the
/// oracle's own copy of this same distinction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeltaAxis {
    /// `@delta(a, b)`: valid-time net diff at the current system snapshot.
    Valid,
    /// `@delta_sys(a, b)`: system-time net diff at the current valid
    /// instant — "how did the record's belief about right now change".
    Sys,
}

/// A stored-relation atom's trailing `@` clause, in the ONE grammar seat.
/// Derivation and diff change row multiplicity: they ride the atom-level
/// clause that already changes what a stored atom yields, never a new
/// top-level construct or an expression function.
///
/// `At` is the pre-existing point-in-time read (unchanged, as it was
/// before time-travel semantics expanded). `Spans` and `Delta` each bind
/// one EXTRA trailing column
/// beyond the atom's own `args` — the interval or the signed-fact marker
/// — the same shape [`crate::query::search::SearchAtom::own_bindings`]
/// already uses for a search's engine-appended columns; the resolver
/// (`compile.rs`) appends it to the atom's bindings rather than folding it
/// into `args`, so relation arity checks against `args` stay exactly what
/// they were.
#[derive(Clone, Debug)]
pub(crate) enum ValidityClause {
    /// `@ expr` (one or two coordinates): resolve at this bitemporal
    /// coordinate. Unchanged surface and unchanged meaning.
    At(AsOf),
    /// `@spans var[, sys_expr]`: derive maximal equal-payload half-open
    /// runs along the valid axis at a fixed system snapshot (`sys`,
    /// default the record's current belief) — one output row per run,
    /// `var` bound to the produced [`crate::data::value::Interval`].
    Spans { sys: ValidityTs, var: Symbol },
    /// `@delta(a, b) var` / `@delta_sys(a, b) var`: the axis-parameterized
    /// net diff between two coordinates on `axis` (the other axis fixed at
    /// current) — one signed row per changed fact, `var` bound to the
    /// sign.
    Delta {
        axis: DeltaAxis,
        from: ValidityTs,
        to: ValidityTs,
        var: Symbol,
    },
}

impl ValidityClause {
    /// The one extra binding this clause produces beyond the atom's own
    /// `args` — `None` for the plain point-in-time read, which binds
    /// nothing new.
    pub(crate) fn extra_var(&self) -> Option<&Symbol> {
        match self {
            ValidityClause::At(_) => None,
            ValidityClause::Spans { var, .. } | ValidityClause::Delta { var, .. } => Some(var),
        }
    }
}

/// A stored-relation application addressed by named fields:
/// `*name{field: expr, …}`.
#[derive(Clone, Debug)]
pub(crate) struct InputNamedFieldRelationApplyAtom {
    pub(crate) name: Symbol,
    pub(crate) args: BTreeMap<SmartString<LazyCompact>, Expr>,
    pub(crate) validity: Option<ValidityClause>,
    pub(crate) span: SourceSpan,
}

/// A stored-relation application with positional args: `*name[args…]`.
#[derive(Clone, Debug)]
pub(crate) struct InputRelationApplyAtom {
    pub(crate) name: Symbol,
    pub(crate) args: Vec<Expr>,
    pub(crate) validity: Option<ValidityClause>,
    pub(crate) span: SourceSpan,
}

/// `binding = expr` (or `binding in expr` when `one_many_unif`: one row per
/// element of the list `expr` evaluates to).
#[derive(Clone, Debug)]
pub(crate) struct Unification {
    /// Symbol to bind expression to.
    pub(crate) binding: Symbol,
    pub(crate) expr: Expr,
    /// If false, `=`; if true, `in`.
    pub(crate) one_many_unif: bool,
    pub(crate) span: SourceSpan,
}

impl Unification {
    pub(crate) fn is_const(&self) -> bool {
        matches!(self.expr, Expr::Const { .. })
    }
    pub(crate) fn bindings_in_expr(&self) -> Result<BTreeSet<Symbol>> {
        self.expr.bindings()
    }
}

/// This is a single query, as you'd find between `{}` in a chained query
/// script or with no `{}` in a single query script.
///
/// The entry rule `?` is a **field**, not a key in the rule map: an
/// `InputProgram` cannot exist without one ([`InputProgram::new`] refuses),
/// so every downstream stage may rely on the entry structurally instead of
/// re-deriving `Symbol::new("?", …)` with a dummy span. The stored
/// `entry_name` keeps the real source span of the `?` the user wrote.
#[derive(Debug, Clone)]
pub(crate) struct InputProgram {
    /// The `?` symbol as written, with its real span.
    entry_name: Symbol,
    /// What `?` is defined as.
    entry: InputInlineRulesOrFixed,
    /// Every named rule except the entry. No key here is entry-kind.
    rules: BTreeMap<Symbol, InputInlineRulesOrFixed>,
    /// The query's `:option`s. `pub(crate)` access via [`Self::out_opts`] /
    /// [`Self::out_opts_mut`]: options are orthogonal to the entry/rules
    /// proof this type carries.
    out_opts: QueryOutOptions,
    disable_magic_rewrite: bool,
    /// Comments with no rule clause to attach to on the relevant side —
    /// the overflow `attach_comment_trivia` falls back to, not the common
    /// case: a comment before the textually-first rule clause instead
    /// becomes THAT clause's own `Trivia::leading` (there is always at
    /// least one clause to attach to — the entry is mandatory — so
    /// `leading_trivia` is reachable only by a future caller of
    /// `attach_comment_trivia` on a program with zero clauses, which
    /// `InputProgram::new` itself never produces). `trailing_trivia` is
    /// the one that matters in practice: every comment after the last
    /// rule clause in the source, not sharing its line. Filled by
    /// [`Self::attach_comment_trivia`]; empty (the default) until then.
    pub(crate) leading_trivia: Vec<Comment>,
    pub(crate) trailing_trivia: Vec<Comment>,
}

/// Every rule clause in `ruleset` (one per [`InputInlineRule`] if it's a
/// plain ruleset, the one [`FixedRuleApply`] if it's a fixed rule), pushed
/// onto `anchors` as its own span paired with a mutable handle to where
/// its trivia lives. A free function, not a method, so it can be called
/// once for `InputProgram::entry` and once per `InputProgram::rules`
/// value without holding two overlapping mutable borrows of `self`.
fn collect_trivia_anchors<'a>(
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
/// before it — scanning backward from `offset` over spaces/tabs only, the
/// first other byte found decides it: a newline means `offset` opens a
/// fresh line (`false`); anything else means there's content on this same
/// line before it (`true`); running off the start of `src` with nothing
/// but spaces/tabs behind `offset` also means `false` (nothing precedes it
/// on any line). This is the one fact [`InputProgram::attach_comment_
/// trivia`] actually needs and a clause's span end cannot reliably give it
/// (see that method's own doc for why).
fn shares_a_line_with_preceding_content(src: &str, offset: usize) -> bool {
    matches!(
        src[..offset.min(src.len())]
            .bytes()
            .rev()
            .find(|&b| b != b' ' && b != b'\t'),
        Some(b) if b != b'\n'
    )
}

impl InputProgram {
    pub(crate) fn insert_rule(&mut self, name: Symbol, def: InputInlineRulesOrFixed) {
        self.rules.insert(name, def);
    }

    /// The one way to make a program. Proves at construction that the query
    /// has an entry (`?`) and that no rule set is empty — the properties
    /// every accessor below relies on.
    pub(crate) fn new(
        mut prog: BTreeMap<Symbol, InputInlineRulesOrFixed>,
        out_opts: QueryOutOptions,
        disable_magic_rewrite: bool,
    ) -> Result<Self> {
        for (name, ruleset) in prog.iter() {
            if let InputInlineRulesOrFixed::Rules { rules } = ruleset
                && rules.is_empty()
            {
                bail!(EmptyRuleSetError(name.to_string(), name.span));
            }
        }
        // Identity of `Symbol` is the name alone, so a dummy-span probe
        // finds the parser's real `?`; `remove_entry` keeps the real span.
        // On failure the label points at one of the program's actual rules
        // (or, for an empty program, the input start — its only location),
        // so the refusal names where an entry should have been.
        let (entry_name, entry) = prog
            .remove_entry(&Symbol::prog_entry(SourceSpan::default()))
            .ok_or_else(|| {
                NoEntryError(Some(prog.keys().next().map(|s| s.span).unwrap_or_default()))
            })?;
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

    /// Attach every comment `parse::scan_comments` found in the same
    /// source text this program was parsed from — the one place "leading"
    /// and "trailing" are decided. `comments` must already be in source
    /// order (as `scan_comments` produces them).
    ///
    /// A comment attaches as *trailing* to the nearest rule clause whose
    /// span ends at or before it, if nothing but whitespace and the
    /// comment itself sits on that same source line (checked directly
    /// against `src`, which is why this takes it rather than working from
    /// spans alone). Otherwise it attaches as *leading* to the nearest
    /// clause whose span starts after it. A comment with no clause on the
    /// relevant side becomes whole-program `trailing_trivia`/
    /// `leading_trivia` instead.
    ///
    /// Every rule clause across the whole program — every [`InputInlineRule`]
    /// in every ruleset, the entry included, plus every [`FixedRuleApply`] —
    /// is a possible anchor; clauses are collected once and sorted by
    /// source position, since `rules`' `BTreeMap` key order is alphabetical
    /// by name, not the order they were written in.
    pub(crate) fn attach_comment_trivia(&mut self, src: &str, comments: Vec<Comment>) {
        if comments.is_empty() {
            return;
        }
        // Anchors are ordered and looked up by their START only, never
        // their end: `rule`/`const_rule`/`fixed_rule` all end in an
        // optional trailing `";"?`, and pest reports a sequence's span as
        // reaching to wherever it finished PROBING for a trailing optional
        // element that then didn't match -- which, across a silently-
        // skipped same-line comment, is past that comment, not before it.
        // (Verified directly: a two-rule pest grammar ending `~ ";"?`
        // reproduces a rule span that swallows a same-line trailing
        // comment whole.) A clause's start position has no such quirk.
        let mut anchors: Vec<(usize, &mut Trivia)> = Vec::new();
        collect_trivia_anchors(&mut self.entry, &mut anchors);
        for ruleset in self.rules.values_mut() {
            collect_trivia_anchors(ruleset, &mut anchors);
        }
        anchors.sort_by_key(|(start, _)| *start);

        for comment in comments {
            if shares_a_line_with_preceding_content(src, comment.span.0) {
                // Trailing: the nearest clause whose content starts this
                // same line (the largest start at or before the comment).
                if let Some(idx) = anchors
                    .iter()
                    .rposition(|(start, _)| *start <= comment.span.0)
                {
                    anchors[idx].1.trailing.push(comment);
                    continue;
                }
                // No clause precedes it at all (comment shares a line with
                // something that isn't a rule clause, e.g. a query
                // option): whole-program trailing is the honest fallback.
                self.trailing_trivia.push(comment);
                continue;
            }
            // Leading: the nearest clause whose content starts after this
            // comment's own line.
            match anchors
                .iter()
                .position(|(start, _)| *start > comment.span.0)
            {
                Some(idx) => anchors[idx].1.leading.push(comment),
                None => self.trailing_trivia.push(comment),
            }
        }
    }

    /// The entry rule's name (`?`) with its real source span.
    pub(crate) fn entry_name(&self) -> &Symbol {
        &self.entry_name
    }

    /// What the entry is defined as.
    pub(crate) fn entry(&self) -> &InputInlineRulesOrFixed {
        &self.entry
    }

    /// The non-entry rules.
    pub(crate) fn rules(&self) -> &BTreeMap<Symbol, InputInlineRulesOrFixed> {
        &self.rules
    }

    /// Every definition in the program, the entry included (under its own
    /// name). This is what replaces the original's "look up `?` in the map"
    /// convention for whole-program walks.
    pub(crate) fn iter_all(&self) -> impl Iterator<Item = (&Symbol, &InputInlineRulesOrFixed)> {
        self.rules
            .iter()
            .chain(std::iter::once((&self.entry_name, &self.entry)))
    }

    pub(crate) fn out_opts(&self) -> &QueryOutOptions {
        &self.out_opts
    }

    pub(crate) fn out_opts_mut(&mut self) -> &mut QueryOutOptions {
        &mut self.out_opts
    }

    /// Whether `:disable_magic_rewrite true` was set on this query. A
    /// formatter/renderer reads this to know whether the option needs
    /// re-emitting; [`NormalFormProgram::disable_magic_rewrite`] carries
    /// the same fact forward past normalization.
    pub(crate) fn disable_magic_rewrite(&self) -> bool {
        self.disable_magic_rewrite
    }

    /// The stored relation this query needs a write lock on, if any:
    /// its output relation, unless that is a temporary.
    pub(crate) fn needs_write_lock(&self) -> Option<SmartString<LazyCompact>> {
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
    pub(crate) fn get_entry_arity(&self) -> Result<usize> {
        match &self.entry {
            InputInlineRulesOrFixed::Rules { rules } => match rules.last() {
                Some(rule) => Ok(rule.head.len()),
                // Impossible after `new` (empty rule sets are refused);
                // surfaced as an error, never a panic.
                None => bail!(TierInvariantError("entry rule set is empty")),
            },
            InputInlineRulesOrFixed::Fixed { fixed } => fixed.arity(),
        }
    }

    /// The entry's output header, or `_0.._n` defaults when it has none
    /// (e.g. a fixed-rule entry without an explicit head).
    pub(crate) fn get_entry_out_head_or_default(&self) -> Result<Vec<Symbol>> {
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

    /// The entry's output header. Aggregated positions render as
    /// `aggr(binding)` — the aggregation's name is already the lowercase
    /// user-facing name (the original stripped an `AGGR_` prefix here, which
    /// would panic against the ported aggregations).
    pub(crate) fn get_entry_out_head(&self) -> Result<Vec<Symbol>> {
        match &self.entry {
            InputInlineRulesOrFixed::Rules { rules } => {
                let last_rule = match rules.last() {
                    Some(rule) => rule,
                    // Impossible after `new`; an error, never a panic.
                    None => bail!(TierInvariantError("entry rule set is empty")),
                };
                let mut ret = Vec::with_capacity(last_rule.head.len());
                for (symb, aggr) in last_rule.head.iter().zip(last_rule.aggr.iter()) {
                    if let Some((aggr, _)) = aggr {
                        ret.push(Symbol::new(format!("{}({})", aggr.name, symb), symb.span))
                    } else {
                        ret.push(symb.clone())
                    }
                }
                Ok(ret)
            }
            InputInlineRulesOrFixed::Fixed { fixed } => {
                if fixed.head.is_empty() {
                    bail!(EntryHeadNotExplicitlyDefinedError(
                        self.entry.first_span().unwrap_or(self.entry_name.span)
                    ))
                } else {
                    Ok(fixed.head.to_vec())
                }
            }
        }
    }

    /// Normalize: desugar every rule body to disjunctive normal form and
    /// well-order the results, minting the [`NormalFormProgram`] tier.
    ///
    /// The desugaring done *here* is total: head deduplication (a repeated
    /// head variable becomes a fresh `***n` binding plus a unification atom
    /// in every body), and the fan-out of each DNF conjunction into its own
    /// flat rule. The fallible, catalog-facing parts — resolving search
    /// atoms, DNF conversion itself, and binding-safety reordering — enter
    /// through the [`BodyNormalizer`] seam.
    pub(crate) fn into_normalized_program(
        self,
        normalizer: &mut impl BodyNormalizer,
    ) -> Result<(NormalFormProgram, QueryOutOptions)> {
        let entry = normalize_ruleset(self.entry, normalizer)?;
        let mut rules: BTreeMap<Symbol, NormalFormRulesOrFixed> = Default::default();
        for (k, ruleset) in self.rules {
            let normalized = normalize_ruleset(ruleset, normalizer)?;
            rules.insert(k, normalized);
        }
        Ok((
            NormalFormProgram {
                entry_name: self.entry_name,
                entry,
                rules,
                disable_magic_rewrite: self.disable_magic_rewrite,
            },
            self.out_opts,
        ))
    }
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
                            if let Some((aggr, aggr_args)) = a {
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
                    for (k, v) in options.as_ref() {
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

// ─────────────────────────────────────────────────────────────────────────
// SEAM: the normalization machinery of the query tier (not yet ported).
// ─────────────────────────────────────────────────────────────────────────

/// The program tier's seam to the query tier's normalization machinery.
///
/// The CozoDB original's `into_normalized_program` took the session
/// transaction, because DNF conversion (`query/logical.rs`) resolves index
/// search atoms against the catalog, and each flat rule is then reordered
/// for binding safety (`query/reorder.rs`). Those are the *fallible resolve*
/// half of normalization; the total desugar half lives in
/// [`InputProgram::into_normalized_program`]. When the query tier lands, its
/// tx-holding normalizer implements this trait; nothing in the program tier
/// touches a transaction.
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

/// Normalize one definition: total desugar here, with disjunctive normal
/// form and well-ordering checks. Implemented as a free function so
/// `into_normalized_program` can consume its input field by field.
fn normalize_ruleset(
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
                                    tuple_pos: None,
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
    pub(crate) aggr: Vec<Option<(Aggregation, Vec<DataValue>)>>,
    pub(crate) body: Vec<NormalFormAtom>,
}

/// A normalized definition: flat rules, or a fixed-rule application
/// (which normalization passes through untouched).
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
///
/// The resolved index-search atoms (HNSW/FTS/LSH) are index-tier substances:
/// they hold live relation handles and manifests, which do not exist before
/// the runtime tier. The index tier adds those variants when it lands;
/// until then a search atom exists only as [`InputAtom::Search`].
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
    /// catalog by the body normalizer (`query::search::resolve_search`).
    Search(Box<crate::query::search::SearchAtom>),
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
/// Minted only by [`InputProgram::into_normalized_program`], so possession
/// is proof of normalization — and of an entry, carried over as a field.
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
/// A *fragment* of a program — the entry lives in exactly one stratum, so
/// entry-as-field cannot apply here; the whole-program entry proof lives on
/// [`StratifiedNormalFormProgram`] instead.
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
///
/// The CozoDB original emitted strata in *reverse* execution order from
/// stratification, un-reversed them in `compile.rs` with `.rev()`, and let
/// `eval.rs` trust position — a convention spread over three files with
/// nothing enforcing it. Here the reversal happens exactly once, inside
/// [`Self::from_reverse_execution_order`], and every consumer reads
/// execution order. (The magic-sets rewrite analyses *demand*, which flows
/// against execution order: it walks `strata().iter().rev()`.)
#[derive(Debug)]
pub(crate) struct StratifiedNormalFormProgram {
    /// Execution order: `strata[0]` evaluates first.
    strata: Vec<NormalFormStratum>,
    disable_magic_rewrite: bool,
}

impl StratifiedNormalFormProgram {
    /// Mint the stratified tier from the stratifier's output, which is in
    /// reverse execution order (element `0` = evaluated last). The reversal
    /// happens here, once. Proves that the strata still contain the entry
    /// and that it sits in the final stratum (everything else is one of its
    /// dependencies, so anything after it would be unreachable).
    pub(crate) fn from_reverse_execution_order(
        mut reversed_strata: Vec<NormalFormStratum>,
        disable_magic_rewrite: bool,
    ) -> Result<Self> {
        reversed_strata.reverse();
        let strata = reversed_strata;
        match strata.last() {
            None => bail!(NoEntryError::spanless()),
            Some(last) if !last.holds_entry() => {
                if strata.iter().any(NormalFormStratum::holds_entry) {
                    bail!(TierInvariantError("entry rule is not in the final stratum"))
                } else {
                    bail!(NoEntryError::spanless())
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
///
/// The units were a three-file convention in the CozoDB original (a bare
/// `BTreeMap<MagicSymbol, usize>` computed as `n_strata - 1 - reversed_idx`
/// in `stratify.rs` and compared against execution positions in `eval.rs`);
/// this type pins them. Stores absent from the map — such as the
/// magic/supplementary stores the demand rewrite mints, which never cross a
/// stratum boundary — are dead after their own stratum.
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

/// An adornment: for each argument position of a rule, whether the demand
/// pattern binds it (`true` = bound, `false` = free). Rendered `b`/`f` in
/// debug output.
///
/// P054 done-when wants a Bound/Free sum (not `Vec<bool>`). That type
/// change breaks `query/magic.rs` (`Vec::with_capacity` assign, `&[bool]`
/// returns, `*b = false` mut iteration) which is outside this task's
/// allowlist — keep the alias until that file is on the allowlist.
pub(crate) type Adornment = Vec<bool>;

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
    pub(crate) fn magic_adornment(&self) -> &[bool] {
        match self {
            MagicSymbol::Muggle { .. } => &[],
            MagicSymbol::Magic { adornment, .. }
            | MagicSymbol::Input { adornment, .. }
            | MagicSymbol::Sup { adornment, .. } => adornment,
        }
    }
    pub(crate) fn has_bound_adornment(&self) -> bool {
        self.magic_adornment().iter().any(|b| *b)
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
                for b in adornment {
                    if *b { write!(f, "b")? } else { write!(f, "f")? }
                }
                Ok(())
            }
            MagicSymbol::Input { inner, adornment } => {
                write!(f, "{}|I", inner.name)?;
                for b in adornment {
                    if *b { write!(f, "b")? } else { write!(f, "f")? }
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
                for b in adornment {
                    if *b { write!(f, "b")? } else { write!(f, "f")? }
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
    pub(crate) aggr: Vec<Option<(Aggregation, Vec<DataValue>)>>,
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
    /// still empty (the transient `Default` state) — the original indexed
    /// `rules.first().unwrap()` here.
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
/// named by [`MagicSymbol`].
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
    pub(crate) name: String,
    #[label]
    pub(crate) span: SourceSpan,
    pub(crate) rule_name: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("Wrong value for option '{name}' of '{rule_name}'")]
#[diagnostic(code(fixed_rule::arg_wrong))]
pub(crate) struct WrongFixedRuleOptionError {
    pub(crate) name: String,
    #[label]
    pub(crate) span: SourceSpan,
    pub(crate) rule_name: String,
    #[help]
    pub(crate) help: String,
}

impl MagicFixedRuleApply {
    // The original's `relation_with_min_len` (an arity floor check on an
    // input relation) needs the session transaction and the epoch stores to
    // measure arity; it lands with the runtime tier as an impl there.

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
            rule_name: String,
        }

        self.rule_args.get(idx).ok_or_else(|| {
            FixedRuleNotEnoughRelationError {
                idx,
                span: self.span,
                rule_name: self.fixed_handle.name.to_string(),
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
    Search(Box<crate::query::search::SearchAtom>),
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

/// One stratum after the magic rewrite. Like [`NormalFormStratum`], a
/// program fragment: the rewrite mutates it map-first, so the field is
/// `pub(crate)` for the rewrite's use.
#[derive(Debug, Default)]
pub(crate) struct MagicProgram {
    pub(crate) prog: BTreeMap<MagicSymbol, MagicRulesOrFixed>,
}

impl MagicProgram {
    fn holds_entry(&self) -> bool {
        self.prog.keys().any(MagicSymbol::is_prog_entry)
    }
}

/// The demand-rewritten program: strata **in execution order** (the rewrite
/// preserves the stratified tier's order stratum by stratum). The final
/// stratum holds the entry as a [`MagicSymbol::Muggle`] — the entry is
/// always exempt from adornment, and construction proves it survived.
///
/// The compile tier consumes [`Self::into_strata`] front to back; the
/// original's `.rev()` in `compile.rs` has no descendant here.
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
            None => bail!(NoEntryError::spanless()),
            Some(last) if !last.holds_entry() => {
                if strata.iter().any(MagicProgram::holds_entry) {
                    bail!(TierInvariantError(
                        "magic entry rule is not in the final stratum"
                    ))
                } else {
                    bail!(NoEntryError::spanless())
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
    use crate::data::aggr::parse_aggr;

    fn sym(name: &str) -> Symbol {
        Symbol::new(name, SourceSpan(0, 0))
    }

    fn rule(head: &[&str]) -> InputInlineRule {
        InputInlineRule {
            head: head.iter().map(|h| sym(h)).collect(),
            aggr: head.iter().map(|_| None).collect(),
            body: vec![],
            span: SourceSpan(0, 0),
            trivia: Trivia::default(),
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

    /// The entry is required at construction: a program without `?` is
    /// refused with `NoEntryError`, not discovered mid-pipeline.
    #[test]
    fn entry_is_required_at_construction() {
        let mut prog = BTreeMap::new();
        prog.insert(sym("r"), rules_def(vec![rule(&["a"])]));
        let err = InputProgram::new(prog, QueryOutOptions::default(), false)
            .expect_err("entry-less program must be refused");
        assert!(err.to_string().contains("no entry"), "got: {err}");
    }

    /// The `?` definition is promoted out of the rule map into the entry
    /// field, keeping its real span.
    #[test]
    fn entry_is_promoted_to_a_field() {
        let mut prog = BTreeMap::new();
        prog.insert(
            Symbol::prog_entry(SourceSpan(7, 1)),
            rules_def(vec![rule(&["a", "b"])]),
        );
        prog.insert(sym("r"), rules_def(vec![rule(&["x"])]));
        let p = InputProgram::new(prog, QueryOutOptions::default(), false).expect("valid program");
        assert_eq!(p.entry_name().span, SourceSpan(7, 1));
        assert!(!p.rules().keys().any(|k| k.kind() == SymbolKind::Entry));
        assert_eq!(p.get_entry_arity().expect("arity"), 2);
        assert_eq!(p.iter_all().count(), 2);
    }

    /// Empty rule sets are refused at construction, which is what makes the
    /// later `first`/`last` accesses structurally sound.
    #[test]
    fn empty_rule_sets_are_refused() {
        let mut prog = BTreeMap::new();
        prog.insert(Symbol::prog_entry(SourceSpan(0, 1)), rules_def(vec![]));
        let err = InputProgram::new(prog, QueryOutOptions::default(), false)
            .expect_err("empty rule set must be refused");
        assert!(err.to_string().contains("no rules"), "got: {err}");
    }

    /// Aggregated entry heads render as `aggr(binding)` using the
    /// aggregation's user-facing name directly — the original stripped an
    /// `AGGR_` prefix that the ported aggregations never had.
    #[test]
    fn entry_out_head_uses_aggregation_names_directly() {
        let min = parse_aggr("min").expect("min exists");
        let mut prog = BTreeMap::new();
        prog.insert(
            Symbol::prog_entry(SourceSpan(0, 1)),
            rules_def(vec![InputInlineRule {
                head: vec![sym("g"), sym("x")],
                aggr: vec![None, Some((min, vec![]))],
                body: vec![],
                span: SourceSpan(0, 0),
                trivia: Trivia::default(),
            }]),
        );
        let p = InputProgram::new(prog, QueryOutOptions::default(), false).expect("valid program");
        let head = p.get_entry_out_head().expect("head");
        assert_eq!(head[0].name.as_str(), "g");
        assert_eq!(head[1].name.as_str(), "min(x)");
    }

    /// The generated-symbol prefixes must keep matching the symbol
    /// namespace classifier: `*n` is Generated, `~n` is GeneratedIgnored.
    #[test]
    fn temp_symb_gen_prefixes_match_symbol_kinds() {
        let mut generated = TempSymbGen::default();
        let g = generated.next(SourceSpan(0, 0));
        let i = generated.next_ignored(SourceSpan(0, 0));
        assert_eq!(g.kind(), SymbolKind::Generated);
        assert_eq!(i.kind(), SymbolKind::GeneratedIgnored);
        assert_ne!(g, i);
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
        let (normalized, _opts) = p
            .into_normalized_program(&mut TrivialNormalizer)
            .expect("normalizes");
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
                        assert!(tuple_pos.is_none());
                    }
                    other @ Expr::Const { .. } | other @ Expr::Apply { .. } | other @ Expr::UnboundApply { .. } | other @ Expr::Cond { .. } | other @ Expr::Lazy { .. } => panic!("expected a binding, got {other:?}"),
                }
            }
            other @ NormalFormAtom::Rule(_) | other @ NormalFormAtom::Relation(_) | other @ NormalFormAtom::NegatedRule(_) | other @ NormalFormAtom::NegatedRelation(_) | other @ NormalFormAtom::Predicate(_) | other @ NormalFormAtom::Search(_) => panic!("expected a unification, got {other:?}"),
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
        let (normalized, _) = p
            .into_normalized_program(&mut TrivialNormalizer)
            .expect("normalizes");
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
                        aggr: vec![None],
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
        // Reversed (stratifier) order: entry stratum first.
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
        // Reversed order: entry LAST here means entry would execute FIRST.
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
        lifetimes.note_use(name.clone(), 2); // must not lower the maximum
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
                    aggr: vec![None],
                    body: vec![],
                }],
            },
        );
        let mut other = MagicProgram::default();
        other.prog.insert(
            MagicSymbol::Magic {
                inner: sym("r"),
                adornment: vec![true, false],
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
            adornment: vec![true],
        };
        assert!(muggle.is_prog_entry());
        assert!(!magic.is_prog_entry());
        assert!(magic.has_bound_adornment());
        assert_eq!(muggle.magic_adornment(), &[] as &[bool]);
    }

    /// The magic-symbol debug rendering is load-bearing for logs and error
    /// messages: adornments render as `b`/`f` after the role marker.
    #[test]
    fn magic_symbol_debug_rendering() {
        let s = MagicSymbol::Sup {
            inner: sym("r"),
            adornment: vec![true, false],
            rule_idx: 2,
            sup_idx: 5,
        };
        assert_eq!(format!("{s:?}"), "r|S.2.5bf");
        let m = MagicSymbol::Magic {
            inner: sym("r"),
            adornment: vec![false, true],
        };
        assert_eq!(format!("{m:?}"), "r|Mfb");
        let i = MagicSymbol::Input {
            inner: sym("r"),
            adornment: vec![true],
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
                _payload: crate::fixed_rule::FixedRulePayload<'_>,
                _out: &mut crate::fixed_rule::FixedRuleOutput,
                _cancel: crate::fixed_rule::CancelFlag,
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

    /// `needs_write_lock` consults the relation-name namespace: temporary
    /// output relations (`_`-prefixed) take no lock.
    #[test]
    fn needs_write_lock_respects_temp_relations() {
        let handle = |name: &str| InputRelationHandle {
            name: sym(name),
            metadata: StoredRelationMetadata {
                keys: vec![],
                non_keys: vec![],
            },
            key_bindings: vec![],
            dep_bindings: vec![],
            span: SourceSpan(0, 0),
        };
        let mk = |name: &str| {
            let mut prog = BTreeMap::new();
            prog.insert(
                Symbol::prog_entry(SourceSpan(0, 1)),
                rules_def(vec![rule(&["x"])]),
            );
            let out_opts = QueryOutOptions {
                store_relation: Some((
                    handle(name),
                    RelationOp::Create,
                    ReturnMutation::NotReturning,
                    WriteValidity::Now,
                )),
                ..Default::default()
            };
            InputProgram::new(prog, out_opts, false).expect("valid program")
        };
        assert_eq!(
            mk("persisted").needs_write_lock().as_deref(),
            Some("persisted")
        );
        assert_eq!(mk("_scratch").needs_write_lock(), None);
    }
}
