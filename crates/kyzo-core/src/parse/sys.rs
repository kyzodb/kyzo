/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): grammar-shape `unwrap`s and `unreachable!` dispatch arms go
 * through the typed-accessor layer; `TokenizerConfig` lives in `fts/` and
 * is imported from there; `AccessLevel` (from the runtime tier) is a seam
 * declaration here until its owning tier lands; `VecElementType` is
 * imported from the value model; fixed-rule implementations are
 * `Arc<dyn FixedRule>`.
 */

//! Parsing system operations: the `::` scripts that administer the
//! database rather than query it.
//!
//! A [`SysOp`] is one parsed, validated administrative command. Everything
//! here is proven at parse time from pure data — even index configurations
//! ([`HnswIndexConfig`], [`FtsIndexConfig`], [`MinHashLshConfig`]) are
//! config values, constructed only after their options were evaluated to
//! constants and range-checked. The *consumers* of these ops are
//! runtime-tier; the ops themselves are parse-tier substance.

use std::collections::BTreeMap;
use std::sync::Arc;

use itertools::Itertools;
use miette::{Diagnostic, Result, bail, ensure};
use ordered_float::OrderedFloat;
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::expr::Expr;
use crate::data::program::{FixedRule, InputProgram};
use crate::data::relation::VecElementType;
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::{DataValue, ValidityTs};
use crate::engines::text::TokenizerConfig;
use crate::parse::expr::{build_expr, parse_string};
use crate::parse::query::parse_query;
use crate::parse::{ExtractSpan, IntoChildren, Pairs, Rule, unexpected};
use crate::data::value::proofs::assert_not_impl;
use crate::typestate::{Set, Unset};
use std::marker::PhantomData;

// ─────────────────────────────────────────────────────────────────────────
// SEAM: runtime tier (not yet ported).
//
// `AccessLevel` re-homes to `runtime/relation.rs` when the runtime tier
// lands. Its `Ord` derive IS its semantics — `Hidden < ReadOnly <
// Protected < Normal`, each level permitting strictly more operations than
// the one below — a landed type-driven win to preserve as-is.
// ─────────────────────────────────────────────────────────────────────────

/// How accessible a stored relation is to queries and mutations.
/// *Seam declaration* — see the note above.
#[allow(missing_docs)]
#[derive(
    Debug,
    Clone,
    Copy,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Default,
    serde_derive::Serialize,
    serde_derive::Deserialize,
)]
pub enum AccessLevel {
    Hidden,
    ReadOnly,
    Protected,
    #[default]
    Normal,
}

impl std::fmt::Display for AccessLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AccessLevel::Normal => f.write_str("normal"),
            AccessLevel::Protected => f.write_str("protected"),
            AccessLevel::ReadOnly => f.write_str("read_only"),
            AccessLevel::Hidden => f.write_str("hidden"),
        }
    }
}

/// One parsed system operation. The consumers are runtime-tier; the values
/// are parse-tier data.
#[allow(missing_docs)]
#[derive(Debug)]
pub enum SysOp {
    Compact,
    /// Compute a deterministic Merkle state root: `None` over the whole
    /// keyspace, `Some(rel)` over one relation's contiguous key range. A
    /// pure function of the committed `(k,v)` set in canonical memcmp
    /// order — the federation content-address (see `storage/merkle.rs`).
    /// Parse-tier data; the runtime dispatcher that runs the scan lands
    /// with `runtime/db.rs`, the same status as [`SysOp::Compact`].
    MerkleRoot(Option<Symbol>),
    ListColumns(Symbol),
    ListIndices(Symbol),
    ListRelations,
    ListRunning,
    ListFixedRules,
    KillRunning(u64),
    Explain(Box<InputProgram>),
    /// `::verify { <query> }` (story #80): run the query through both the
    /// production evaluator and the sealed naive oracle over one shared
    /// snapshot. Parsed identically to [`SysOp::Explain`] — same grammar
    /// shape, `runtime/verify.rs::Db::verify_input_program` is the
    /// dispatcher.
    Verify(Box<InputProgram>),
    RemoveRelation(Vec<Symbol>),
    RenameRelation(Vec<(Symbol, Symbol)>),
    ShowTrigger(Symbol),
    /// Trigger bodies as their provenance source text (put/rm/replace). The
    /// parse tier carries the sources; the runtime tier's
    /// `set_relation_triggers` lifts each ONCE into a parsed `Trigger`
    /// substance at the store boundary, so a source that fails its own parse
    /// is refused there and never stored — the substance, not the source, is
    /// what fires (never re-parsed at fire time).
    SetTriggers(Symbol, Vec<String>, Vec<String>, Vec<String>),
    /// Declare an integrity constraint: a named denial rule. The body is a
    /// pure query stored as raw source (the same inherited convention as
    /// [`SysOp::SetTriggers`] — parsed substances in the catalog are the
    /// Phase C end state); a non-empty result at commit time denies the
    /// transaction. Validation-parsed here; the runtime tier re-validates
    /// purity and computes the read-set.
    CreateConstraint(Symbol, String),
    /// Remove the named constraint from every relation it is attached to.
    RemoveConstraint(Symbol),
    /// List every constraint: name, attached relation, body source.
    ListConstraints,
    SetAccessLevel(Vec<Symbol>, AccessLevel),
    CreateIndex(Symbol, Symbol, Vec<Symbol>),
    CreateVectorIndex(HnswIndexConfig),
    CreateFtsIndex(FtsIndexConfig),
    CreateMinHashLshIndex(MinHashLshConfig),
    RemoveIndex(Symbol, Symbol),
    /// Unreachable through the grammar, faithfully ported: the CozoDB
    /// grammar defines `describe_relation_op` but never includes it in
    /// `sys_script`, so `::describe` cannot parse. Kept so the runtime
    /// tier's consumer ports cleanly; wiring the grammar rule in is a
    /// deliberate language decision to make separately.
    DescribeRelation(Symbol, SmartString<LazyCompact>),
}

/// A private witness that an index configuration was produced by its staged
/// builder. Only this module can name it, so `build()` is each config's sole
/// constructor — no struct literal elsewhere can assemble an incomplete,
/// unproven configuration. Its presence carries the completeness proof: to
/// hold one of these configs is to hold a fully-set one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Built;

/// Configuration of an FTS (full-text search) index, as declared.
/// Constructible ONLY through [`FtsConfigBuilder`] (the private `_built`
/// witness seals it), so every value carries a proven `extractor`.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtsIndexConfig {
    pub base_relation: SmartString<LazyCompact>,
    pub index_name: SmartString<LazyCompact>,
    /// The row-extraction expression as a PARSED, partial-evaluated typed
    /// substance — never source text re-parsed at build time. When an
    /// `extract_filter` is given it is folded in here as a typed
    /// `if(filter, extractor)` conditional, not a textual splice.
    pub extractor: Expr,
    pub tokenizer: TokenizerConfig,
    pub filters: Vec<TokenizerConfig>,
    _built: Built,
}

/// Staged builder for [`FtsIndexConfig`]. The one required field — the row
/// `extractor` — is tracked as a typestate marker, so
/// [`BuildFtsConfig::build`] exists only once it is set. Optional fields
/// carry their defaults.
#[must_use = "an FTS config builder yields nothing until `.build()`, which \
              exists only once the extractor is set"]
pub struct FtsConfigBuilder<Ex> {
    base_relation: SmartString<LazyCompact>,
    index_name: SmartString<LazyCompact>,
    extractor: Expr,
    tokenizer: TokenizerConfig,
    filters: Vec<TokenizerConfig>,
    _required: PhantomData<Ex>,
}

impl FtsConfigBuilder<Unset> {
    /// Begin an FTS config from the two always-known identity fields. The
    /// extractor starts `Unset` (its placeholder value is never read in a
    /// built config); optional fields take their defaults.
    pub fn new(
        base_relation: SmartString<LazyCompact>,
        index_name: SmartString<LazyCompact>,
    ) -> Self {
        FtsConfigBuilder {
            base_relation,
            index_name,
            extractor: Expr::Const {
                val: DataValue::Null,
                span: SourceSpan(0, 0),
            },
            tokenizer: TokenizerConfig {
                name: Default::default(),
                args: Default::default(),
            },
            filters: vec![],
            _required: PhantomData,
        }
    }
}

impl<Ex> FtsConfigBuilder<Ex> {
    // Optional fields: the extractor marker passes through unchanged.
    pub fn tokenizer(mut self, tokenizer: TokenizerConfig) -> Self {
        self.tokenizer = tokenizer;
        self
    }
    pub fn filters(mut self, filters: Vec<TokenizerConfig>) -> Self {
        self.filters = filters;
        self
    }
}

impl FtsConfigBuilder<Unset> {
    /// Supply the required row extractor, flipping its marker to `Set`.
    pub fn extractor(self, extractor: Expr) -> FtsConfigBuilder<Set> {
        FtsConfigBuilder {
            base_relation: self.base_relation,
            index_name: self.index_name,
            extractor,
            tokenizer: self.tokenizer,
            filters: self.filters,
            _required: PhantomData,
        }
    }
}

/// The build door for an FTS configuration: implemented ONLY for the
/// extractor-set builder, so a config without an extractor cannot be built.
pub trait BuildFtsConfig {
    fn build(self) -> FtsIndexConfig;
}

impl BuildFtsConfig for FtsConfigBuilder<Set> {
    fn build(self) -> FtsIndexConfig {
        FtsIndexConfig {
            base_relation: self.base_relation,
            index_name: self.index_name,
            extractor: self.extractor,
            tokenizer: self.tokenizer,
            filters: self.filters,
            _built: Built,
        }
    }
}

/// Configuration of a MinHash-LSH (locality-sensitive hashing) index, as
/// declared.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinHashLshConfig {
    pub base_relation: SmartString<LazyCompact>,
    pub index_name: SmartString<LazyCompact>,
    /// The row-extraction expression as a parsed typed substance (see
    /// [`FtsIndexConfig::extractor`]).
    pub extractor: Expr,
    pub tokenizer: TokenizerConfig,
    pub filters: Vec<TokenizerConfig>,
    pub n_gram: usize,
    pub n_perm: usize,
    pub false_positive_weight: OrderedFloat<f64>,
    pub false_negative_weight: OrderedFloat<f64>,
    pub target_threshold: OrderedFloat<f64>,
    _built: Built,
}

/// Staged builder for [`MinHashLshConfig`]. The one required field — the row
/// `extractor` — is a typestate marker, so [`BuildMinHashLshConfig::build`]
/// exists only once it is set. Optional fields carry their defaults (the same
/// values the grammar's option defaults use).
#[must_use = "a MinHash-LSH config builder yields nothing until `.build()`, \
              which exists only once the extractor is set"]
pub struct MinHashLshConfigBuilder<Ex> {
    base_relation: SmartString<LazyCompact>,
    index_name: SmartString<LazyCompact>,
    extractor: Expr,
    tokenizer: TokenizerConfig,
    filters: Vec<TokenizerConfig>,
    n_gram: usize,
    n_perm: usize,
    false_positive_weight: OrderedFloat<f64>,
    false_negative_weight: OrderedFloat<f64>,
    target_threshold: OrderedFloat<f64>,
    _required: PhantomData<Ex>,
}

impl MinHashLshConfigBuilder<Unset> {
    /// Begin a MinHash-LSH config from the two always-known identity fields.
    /// The extractor starts `Unset`; optional fields take their defaults.
    pub fn new(
        base_relation: SmartString<LazyCompact>,
        index_name: SmartString<LazyCompact>,
    ) -> Self {
        MinHashLshConfigBuilder {
            base_relation,
            index_name,
            extractor: Expr::Const {
                val: DataValue::Null,
                span: SourceSpan(0, 0),
            },
            tokenizer: TokenizerConfig {
                name: Default::default(),
                args: Default::default(),
            },
            filters: vec![],
            n_gram: 1,
            n_perm: 200,
            false_positive_weight: OrderedFloat(1.0),
            false_negative_weight: OrderedFloat(1.0),
            target_threshold: OrderedFloat(0.9),
            _required: PhantomData,
        }
    }
}

impl<Ex> MinHashLshConfigBuilder<Ex> {
    // Optional fields: the extractor marker passes through unchanged.
    pub fn tokenizer(mut self, tokenizer: TokenizerConfig) -> Self {
        self.tokenizer = tokenizer;
        self
    }
    pub fn filters(mut self, filters: Vec<TokenizerConfig>) -> Self {
        self.filters = filters;
        self
    }
    pub fn n_gram(mut self, n_gram: usize) -> Self {
        self.n_gram = n_gram;
        self
    }
    pub fn n_perm(mut self, n_perm: usize) -> Self {
        self.n_perm = n_perm;
        self
    }
    pub fn weights(mut self, false_positive: f64, false_negative: f64) -> Self {
        self.false_positive_weight = OrderedFloat(false_positive);
        self.false_negative_weight = OrderedFloat(false_negative);
        self
    }
    pub fn target_threshold(mut self, target_threshold: f64) -> Self {
        self.target_threshold = OrderedFloat(target_threshold);
        self
    }
}

impl MinHashLshConfigBuilder<Unset> {
    /// Supply the required row extractor, flipping its marker to `Set`.
    pub fn extractor(self, extractor: Expr) -> MinHashLshConfigBuilder<Set> {
        MinHashLshConfigBuilder {
            base_relation: self.base_relation,
            index_name: self.index_name,
            extractor,
            tokenizer: self.tokenizer,
            filters: self.filters,
            n_gram: self.n_gram,
            n_perm: self.n_perm,
            false_positive_weight: self.false_positive_weight,
            false_negative_weight: self.false_negative_weight,
            target_threshold: self.target_threshold,
            _required: PhantomData,
        }
    }
}

/// The build door for a MinHash-LSH configuration: implemented ONLY for the
/// extractor-set builder, so a config without an extractor cannot be built.
pub trait BuildMinHashLshConfig {
    fn build(self) -> MinHashLshConfig;
}

impl BuildMinHashLshConfig for MinHashLshConfigBuilder<Set> {
    fn build(self) -> MinHashLshConfig {
        MinHashLshConfig {
            base_relation: self.base_relation,
            index_name: self.index_name,
            extractor: self.extractor,
            tokenizer: self.tokenizer,
            filters: self.filters,
            n_gram: self.n_gram,
            n_perm: self.n_perm,
            false_positive_weight: self.false_positive_weight,
            false_negative_weight: self.false_negative_weight,
            target_threshold: self.target_threshold,
            _built: Built,
        }
    }
}

/// Configuration of an HNSW vector index, as declared. Constructible ONLY
/// through [`HnswConfigBuilder`] (the private `_built` witness seals it), so
/// every `HnswIndexConfig` value is proven complete: `vec_dim`,
/// `ef_construction`, and `m_neighbours` are always present, never a
/// sentinel checked after assembly.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HnswIndexConfig {
    pub base_relation: SmartString<LazyCompact>,
    pub index_name: SmartString<LazyCompact>,
    pub vec_dim: usize,
    pub dtype: VecElementType,
    pub vec_fields: Vec<SmartString<LazyCompact>>,
    pub distance: HnswDistance,
    pub ef_construction: usize,
    pub m_neighbours: usize,
    pub index_filter: Option<String>,
    pub extend_candidates: bool,
    pub keep_pruned_connections: bool,
    _built: Built,
}

/// Staged builder for [`HnswIndexConfig`]. Its three required numeric
/// fields — `dim`, `ef`, and `m` — are tracked as typestate markers, so
/// [`BuildHnswConfig::build`] exists only on the fully-`Set` instantiation:
/// a config missing any of them is a COMPILE error, not an `Option`/sentinel
/// validated at run time. Optional fields carry their defaults.
#[must_use = "an HNSW config builder yields nothing until `.build()`, which \
              exists only once dim, ef, and m are all set"]
pub struct HnswConfigBuilder<Dim, Ef, M> {
    base_relation: SmartString<LazyCompact>,
    index_name: SmartString<LazyCompact>,
    vec_dim: usize,
    ef_construction: usize,
    m_neighbours: usize,
    dtype: VecElementType,
    vec_fields: Vec<SmartString<LazyCompact>>,
    distance: HnswDistance,
    index_filter: Option<String>,
    extend_candidates: bool,
    keep_pruned_connections: bool,
    _required: PhantomData<(Dim, Ef, M)>,
}

impl HnswConfigBuilder<Unset, Unset, Unset> {
    /// Begin an HNSW config from the two always-known identity fields. Every
    /// required numeric field starts `Unset`; every optional field starts at
    /// its default.
    pub fn new(
        base_relation: SmartString<LazyCompact>,
        index_name: SmartString<LazyCompact>,
    ) -> Self {
        HnswConfigBuilder {
            base_relation,
            index_name,
            vec_dim: 0,
            ef_construction: 0,
            m_neighbours: 0,
            dtype: VecElementType::F32,
            vec_fields: vec![],
            distance: HnswDistance::L2,
            index_filter: None,
            extend_candidates: false,
            keep_pruned_connections: false,
            _required: PhantomData,
        }
    }
}

impl<Dim, Ef, M> HnswConfigBuilder<Dim, Ef, M> {
    /// Move every data field into a builder carrying different required-field
    /// markers. Private: its only callers are the required-field setters
    /// below, each of which writes its field's value BEFORE re-marking it, so
    /// no marker reaches `Set` without its value supplied.
    fn remark<D2, E2, M2>(self) -> HnswConfigBuilder<D2, E2, M2> {
        HnswConfigBuilder {
            base_relation: self.base_relation,
            index_name: self.index_name,
            vec_dim: self.vec_dim,
            ef_construction: self.ef_construction,
            m_neighbours: self.m_neighbours,
            dtype: self.dtype,
            vec_fields: self.vec_fields,
            distance: self.distance,
            index_filter: self.index_filter,
            extend_candidates: self.extend_candidates,
            keep_pruned_connections: self.keep_pruned_connections,
            _required: PhantomData,
        }
    }

    // Optional fields: required-field markers pass through unchanged, so they
    // may be set at any point in the chain, in any order.
    pub fn dtype(mut self, dtype: VecElementType) -> Self {
        self.dtype = dtype;
        self
    }
    pub fn distance(mut self, distance: HnswDistance) -> Self {
        self.distance = distance;
        self
    }
    pub fn fields(mut self, vec_fields: Vec<SmartString<LazyCompact>>) -> Self {
        self.vec_fields = vec_fields;
        self
    }
    pub fn filter(mut self, index_filter: Option<String>) -> Self {
        self.index_filter = index_filter;
        self
    }
    pub fn extend_candidates(mut self, extend_candidates: bool) -> Self {
        self.extend_candidates = extend_candidates;
        self
    }
    pub fn keep_pruned_connections(mut self, keep_pruned_connections: bool) -> Self {
        self.keep_pruned_connections = keep_pruned_connections;
        self
    }
}

// Each required setter is callable only while its own field is `Unset` (so it
// cannot be set twice) and flips exactly that field's marker to `Set`.
impl<Ef, M> HnswConfigBuilder<Unset, Ef, M> {
    pub fn dim(mut self, vec_dim: usize) -> HnswConfigBuilder<Set, Ef, M> {
        self.vec_dim = vec_dim;
        self.remark()
    }
}
impl<Dim, M> HnswConfigBuilder<Dim, Unset, M> {
    pub fn ef(mut self, ef_construction: usize) -> HnswConfigBuilder<Dim, Set, M> {
        self.ef_construction = ef_construction;
        self.remark()
    }
}
impl<Dim, Ef> HnswConfigBuilder<Dim, Ef, Unset> {
    pub fn m(mut self, m_neighbours: usize) -> HnswConfigBuilder<Dim, Ef, Set> {
        self.m_neighbours = m_neighbours;
        self.remark()
    }
}

/// The build door for an HNSW configuration: implemented ONLY for the
/// fully-set builder, so a config missing `dim`, `ef`, or `m` cannot be
/// built. The produced config is sealed, so this is its sole constructor.
pub trait BuildHnswConfig {
    fn build(self) -> HnswIndexConfig;
}

impl BuildHnswConfig for HnswConfigBuilder<Set, Set, Set> {
    fn build(self) -> HnswIndexConfig {
        HnswIndexConfig {
            base_relation: self.base_relation,
            index_name: self.index_name,
            vec_dim: self.vec_dim,
            dtype: self.dtype,
            vec_fields: self.vec_fields,
            distance: self.distance,
            ef_construction: self.ef_construction,
            m_neighbours: self.m_neighbours,
            index_filter: self.index_filter,
            extend_candidates: self.extend_candidates,
            keep_pruned_connections: self.keep_pruned_connections,
            _built: Built,
        }
    }
}

// ── Compile-fail proofs (enforced in EVERY build, like `data::value::proofs`)
//
// `build()` — carried by each config's build trait — is ABSENT on every
// INCOMPLETE builder instantiation: the DoD's "a config missing a required
// field cannot compile", witnessed mechanically rather than asserted in
// prose. Each line fails to compile the moment the build trait leaks to an
// under-set typestate. The positive direction (build present on the fully-set
// builder) is exercised by the parse paths above and the unit tests below.

// HNSW requires `dim`, `ef`, AND `m`: every instantiation short of all three
// `Set` lacks the build trait.
assert_not_impl!(HnswConfigBuilder<Unset, Unset, Unset>: BuildHnswConfig);
assert_not_impl!(HnswConfigBuilder<Set, Unset, Unset>: BuildHnswConfig);
assert_not_impl!(HnswConfigBuilder<Unset, Set, Unset>: BuildHnswConfig);
assert_not_impl!(HnswConfigBuilder<Unset, Unset, Set>: BuildHnswConfig);
assert_not_impl!(HnswConfigBuilder<Set, Set, Unset>: BuildHnswConfig);
assert_not_impl!(HnswConfigBuilder<Set, Unset, Set>: BuildHnswConfig);
assert_not_impl!(HnswConfigBuilder<Unset, Set, Set>: BuildHnswConfig);

// FTS and MinHash-LSH require the row `extractor`.
assert_not_impl!(FtsConfigBuilder<Unset>: BuildFtsConfig);
assert_not_impl!(MinHashLshConfigBuilder<Unset>: BuildMinHashLshConfig);

/// The distance metric of an HNSW index.
#[allow(missing_docs)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde_derive::Serialize, serde_derive::Deserialize,
)]
pub enum HnswDistance {
    L2,
    InnerProduct,
    Cosine,
}

/// A `::kill` argument that evaluates to a non-integer value. Spanned at the
/// argument expression, replacing the CozoDB original's span-less `miette!`.
#[derive(Debug, Error, Diagnostic)]
#[error("`::kill` needs a process ID, not this")]
#[diagnostic(code(parser::kill_pid_not_integer))]
#[diagnostic(help("write the process ID as an integer literal or a `$parameter` bound to one"))]
struct ProcessIdNotInteger(#[label("this must evaluate to an integer process ID")] SourceSpan);

/// A rejected option in an `::hnsw`/`::fts`/`::lsh` index-DDL clause,
/// labelled at the offending option name or value. One typed carrier for
/// the whole option-validation family, which the CozoDB original refused
/// with span-less `miette!`/`bail!`/`ensure!`. The message is the specific
/// validation failure; the span points at the exact construct.
#[derive(Debug, Error, Diagnostic)]
#[error("{0}")]
#[diagnostic(code(parser::index_option))]
struct IndexOptionError(String, #[label("invalid index option")] SourceSpan);

/// Fold an optional `extract_filter` predicate into the row `extractor` as a
/// typed `if(filter, extractor)` conditional — the exact shape the parser
/// builds for a two-argument `if(cond, then)` (filter true yields the
/// extractor, else `Null`), but constructed from the already-parsed
/// sub-expressions rather than spliced together as source text. A missing
/// extractor is a typed refusal HERE: an index with nothing to extract is a
/// definition error surfaced at parse, never a value deferred to build-time
/// compilation.
fn combine_extractor(
    extractor: Option<Expr>,
    extract_filter: Option<Expr>,
    kind: &str,
    span: SourceSpan,
) -> Result<Expr> {
    let extractor = extractor.ok_or_else(|| {
        miette::Report::from(IndexOptionError(
            format!("a {kind} index requires an `extractor` option"),
            span,
        ))
    })?;
    match extract_filter {
        None => Ok(extractor),
        Some(filter) => {
            let span = extractor.span();
            Ok(Expr::Cond {
                clauses: vec![
                    (filter, extractor),
                    (
                        Expr::Const {
                            val: DataValue::from(true),
                            span,
                        },
                        Expr::Const {
                            val: DataValue::Null,
                            span,
                        },
                    ),
                ],
                span,
            })
        }
    }
}

pub(crate) fn parse_sys(
    mut src: Pairs<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    fixed_rules: &BTreeMap<String, Arc<dyn FixedRule>>,
    cur_vld: ValidityTs,
) -> Result<SysOp> {
    #[derive(Debug, Error, Diagnostic)]
    #[error("parse-tree shape violates the grammar: sys_script has no operation")]
    #[diagnostic(code(parser::grammar_shape))]
    #[diagnostic(help(
        "This is a bug: kyzoscript.pest and its consumer disagree. Please report it."
    ))]
    struct EmptySysScript;
    let inner = src.next().ok_or(EmptySysScript)?;
    Ok(match inner.as_rule() {
        Rule::compact_op => SysOp::Compact,
        Rule::merkle_root_op => {
            // Optional relation name: present ⇒ a per-relation root, absent
            // ⇒ the whole-keyspace root.
            let rel = inner
                .into_inner()
                .next()
                .map(|rel_p| Symbol::new(rel_p.as_str(), rel_p.extract_span()));
            SysOp::MerkleRoot(rel)
        }
        Rule::running_op => SysOp::ListRunning,
        Rule::kill_op => {
            let i_expr = inner.children().expect("the process id expression")?;
            let span = i_expr.extract_span();
            let i_val = build_expr(i_expr, param_pool)?;
            let i_val = i_val.eval_to_const()?;
            let i_val = i_val.get_int().ok_or(ProcessIdNotInteger(span))?;
            SysOp::KillRunning(i_val as u64)
        }
        Rule::explain_op => {
            let prog = parse_query(
                inner
                    .children()
                    .expect("the query to explain")?
                    .into_inner(),
                param_pool,
                fixed_rules,
                cur_vld,
            )?;
            SysOp::Explain(Box::new(prog))
        }
        Rule::verify_op => {
            let prog = parse_query(
                inner.children().expect("the query to verify")?.into_inner(),
                param_pool,
                fixed_rules,
                cur_vld,
            )?;
            SysOp::Verify(Box::new(prog))
        }
        Rule::describe_relation_op => {
            let mut inner = inner.children();
            let rels_p = inner.expect("the relation's name")?;
            let rel = Symbol::new(rels_p.as_str(), rels_p.extract_span());
            let description = match inner.next() {
                None => Default::default(),
                Some(desc_p) => parse_string(desc_p)?,
            };
            SysOp::DescribeRelation(rel, description)
        }
        Rule::list_relations_op => SysOp::ListRelations,
        Rule::remove_relations_op => {
            let rel = inner
                .into_inner()
                .map(|rels_p| Symbol::new(rels_p.as_str(), rels_p.extract_span()))
                .collect_vec();

            SysOp::RemoveRelation(rel)
        }
        Rule::list_columns_op => {
            let rels_p = inner.children().expect("the relation's name")?;
            let rel = Symbol::new(rels_p.as_str(), rels_p.extract_span());
            SysOp::ListColumns(rel)
        }
        Rule::list_indices_op => {
            let rels_p = inner.children().expect("the relation's name")?;
            let rel = Symbol::new(rels_p.as_str(), rels_p.extract_span());
            SysOp::ListIndices(rel)
        }
        Rule::rename_relations_op => {
            let rename_pairs: Vec<_> = inner
                .into_inner()
                .map(|pair| -> Result<(Symbol, Symbol)> {
                    let [old_p, new_p] = pair
                        .children()
                        .expect_n(["the old relation name", "the new relation name"])?;
                    let rel = Symbol::new(old_p.as_str(), old_p.extract_span());
                    let new_rel = Symbol::new(new_p.as_str(), new_p.extract_span());
                    Ok((rel, new_rel))
                })
                .try_collect()?;
            SysOp::RenameRelation(rename_pairs)
        }
        Rule::access_level_op => {
            let mut ps = inner.children();
            let level_p = ps.expect("the access level")?;
            let access_level = match level_p.as_str() {
                "normal" => AccessLevel::Normal,
                "protected" => AccessLevel::Protected,
                "read_only" => AccessLevel::ReadOnly,
                "hidden" => AccessLevel::Hidden,
                _ => return Err(unexpected("an access level", &level_p)),
            };
            let mut rels = vec![];
            for rel_p in ps {
                let rel = Symbol::new(rel_p.as_str(), rel_p.extract_span());
                rels.push(rel)
            }
            SysOp::SetAccessLevel(rels, access_level)
        }
        Rule::trigger_relation_show_op => {
            let rels_p = inner.children().expect("the relation's name")?;
            let rel = Symbol::new(rels_p.as_str(), rels_p.extract_span());
            SysOp::ShowTrigger(rel)
        }
        Rule::trigger_relation_op => {
            let mut src = inner.children();
            let rels_p = src.expect("the relation's name")?;
            let rel = Symbol::new(rels_p.as_str(), rels_p.extract_span());
            let mut puts = vec![];
            let mut rms = vec![];
            let mut replaces = vec![];
            for clause in src {
                let [op, script] = clause
                    .children()
                    .expect_n(["the trigger kind", "the trigger body"])?;
                let script_str = script.as_str();
                // Validation parse only: the body is stored as source text
                // and re-parsed at fire time (inherited convention; see
                // `SysOp::SetTriggers`). Parameters deliberately empty —
                // the firing context supplies its own.
                parse_query(
                    script.into_inner(),
                    &Default::default(),
                    fixed_rules,
                    cur_vld,
                )?;
                match op.as_rule() {
                    Rule::trigger_put => puts.push(script_str.to_string()),
                    Rule::trigger_rm => rms.push(script_str.to_string()),
                    Rule::trigger_replace => replaces.push(script_str.to_string()),
                    _ => return Err(unexpected("a trigger kind", &op)),
                }
            }
            SysOp::SetTriggers(rel, puts, rms, replaces)
        }
        Rule::constraint_op => {
            let op = inner.children().expect("the constraint operation")?;
            match op.as_rule() {
                Rule::constraint_create => {
                    let [name_p, script] = op
                        .children()
                        .expect_n(["the constraint's name", "the constraint body"])?;
                    let name = Symbol::new(name_p.as_str(), name_p.extract_span());
                    let script_str = script.as_str();
                    // Validation parse only: the body is stored as source
                    // text and re-parsed at enforcement time (inherited
                    // convention; see `SysOp::SetTriggers`). Parameters
                    // deliberately empty — a constraint is a standing rule
                    // and binds no caller parameters.
                    parse_query(
                        script.into_inner(),
                        &Default::default(),
                        fixed_rules,
                        cur_vld,
                    )?;
                    SysOp::CreateConstraint(name, script_str.to_string())
                }
                Rule::constraint_drop => {
                    let name_p = op.children().expect("the constraint's name")?;
                    SysOp::RemoveConstraint(Symbol::new(name_p.as_str(), name_p.extract_span()))
                }
                Rule::constraint_list => SysOp::ListConstraints,
                _ => return Err(unexpected("a constraint operation", &op)),
            }
        }
        Rule::lsh_idx_op => {
            let inner = inner.children().expect("the index operation")?;
            match inner.as_rule() {
                Rule::index_create_adv => {
                    let create_span = inner.extract_span();
                    let mut inner = inner.children();
                    let rel = inner.expect("the relation's name")?;
                    let name = inner.expect("the index's name")?;
                    let mut filters = vec![];
                    let mut tokenizer = TokenizerConfig {
                        name: Default::default(),
                        args: Default::default(),
                    };
                    let mut extractor: Option<Expr> = None;
                    let mut extract_filter: Option<Expr> = None;
                    let mut n_gram = 1;
                    let mut n_perm = 200;
                    let mut target_threshold = 0.9;
                    let mut false_positive_weight = 1.0;
                    let mut false_negative_weight = 1.0;
                    // Spans of the offending option values, captured for the
                    // post-loop range checks below: an out-of-range value is
                    // labelled where the user wrote it; an option left at its
                    // default (and so never at fault) falls back to the whole
                    // create clause.
                    let mut fpw_span = create_span;
                    let mut fnw_span = create_span;
                    let mut n_gram_span = create_span;
                    let mut n_perm_span = create_span;
                    let mut threshold_span = create_span;
                    for opt_pair in inner {
                        let [opt_name, opt_val] = opt_pair
                            .children()
                            .expect_n(["the option's name", "the option's value"])?;
                        let name_span = opt_name.extract_span();
                        let val_span = opt_val.extract_span();
                        match opt_name.as_str() {
                            "false_positive_weight" => {
                                fpw_span = val_span;
                                let mut expr = build_expr(opt_val, param_pool)?;
                                expr.partial_eval()?;
                                let v = expr.eval_to_const()?;
                                false_positive_weight = v.get_float().ok_or_else(|| {
                                    IndexOptionError(
                                        "false_positive_weight must be a float".to_string(),
                                        val_span,
                                    )
                                })?;
                            }
                            "false_negative_weight" => {
                                fnw_span = val_span;
                                let mut expr = build_expr(opt_val, param_pool)?;
                                expr.partial_eval()?;
                                let v = expr.eval_to_const()?;
                                false_negative_weight = v.get_float().ok_or_else(|| {
                                    IndexOptionError(
                                        "false_negative_weight must be a float".to_string(),
                                        val_span,
                                    )
                                })?;
                            }
                            "n_gram" => {
                                n_gram_span = val_span;
                                let mut expr = build_expr(opt_val, param_pool)?;
                                expr.partial_eval()?;
                                let v = expr.eval_to_const()?;
                                let v = v.get_int().ok_or_else(|| {
                                    IndexOptionError(
                                        "n_gram must be an integer".to_string(),
                                        val_span,
                                    )
                                })?;
                                ensure!(
                                    v > 0,
                                    IndexOptionError(
                                        "n_gram must be positive".to_string(),
                                        val_span
                                    )
                                );
                                n_gram = v as usize;
                            }
                            "n_perm" => {
                                n_perm_span = val_span;
                                let mut expr = build_expr(opt_val, param_pool)?;
                                expr.partial_eval()?;
                                let v = expr.eval_to_const()?;
                                let v = v.get_int().ok_or_else(|| {
                                    IndexOptionError(
                                        "n_perm must be an integer".to_string(),
                                        val_span,
                                    )
                                })?;
                                ensure!(
                                    v > 0,
                                    IndexOptionError(
                                        "n_perm must be positive".to_string(),
                                        val_span
                                    )
                                );
                                n_perm = v as usize;
                            }
                            "target_threshold" => {
                                threshold_span = val_span;
                                let mut expr = build_expr(opt_val, param_pool)?;
                                expr.partial_eval()?;
                                let v = expr.eval_to_const()?;
                                target_threshold = v.get_float().ok_or_else(|| {
                                    IndexOptionError(
                                        "target_threshold must be a float".to_string(),
                                        val_span,
                                    )
                                })?;
                            }
                            "extractor" => {
                                let mut ex = build_expr(opt_val, param_pool)?;
                                ex.partial_eval()?;
                                extractor = Some(ex);
                            }
                            "extract_filter" => {
                                let mut ex = build_expr(opt_val, param_pool)?;
                                ex.partial_eval()?;
                                extract_filter = Some(ex);
                            }
                            "tokenizer" => {
                                let mut expr = build_expr(opt_val, param_pool)?;
                                expr.partial_eval()?;
                                let parsed = parse_tokenizer_expr(expr)?;
                                tokenizer = parsed;
                            }
                            "filters" => {
                                filters = parse_filters_expr(build_expr(opt_val, param_pool)?)?;
                            }
                            _ => {
                                return Err(IndexOptionError(
                                    format!("Unknown option {} for LSH index", opt_name.as_str()),
                                    name_span,
                                )
                                .into());
                            }
                        }
                    }
                    ensure!(
                        false_positive_weight > 0.,
                        IndexOptionError(
                            "false_positive_weight must be positive".to_string(),
                            fpw_span,
                        )
                    );
                    ensure!(
                        false_negative_weight > 0.,
                        IndexOptionError(
                            "false_negative_weight must be positive".to_string(),
                            fnw_span,
                        )
                    );
                    ensure!(
                        n_gram > 0,
                        IndexOptionError("n_gram must be positive".to_string(), n_gram_span)
                    );
                    ensure!(
                        n_perm > 0,
                        IndexOptionError("n_perm must be positive".to_string(), n_perm_span)
                    );
                    ensure!(
                        target_threshold > 0. && target_threshold < 1.,
                        IndexOptionError(
                            "target_threshold must be between 0 and 1".to_string(),
                            threshold_span,
                        )
                    );
                    let total_weights = false_positive_weight + false_negative_weight;
                    false_positive_weight /= total_weights;
                    false_negative_weight /= total_weights;

                    let extractor =
                        combine_extractor(extractor, extract_filter, "MinHash-LSH", name.extract_span())?;
                    let config = MinHashLshConfigBuilder::new(
                        SmartString::from(rel.as_str()),
                        SmartString::from(name.as_str()),
                    )
                    .tokenizer(tokenizer)
                    .filters(filters)
                    .n_gram(n_gram)
                    .n_perm(n_perm)
                    .weights(false_positive_weight, false_negative_weight)
                    .target_threshold(target_threshold)
                    .extractor(extractor)
                    .build();
                    SysOp::CreateMinHashLshIndex(config)
                }
                Rule::index_drop => parse_index_drop(inner)?,
                _ => return Err(unexpected("an LSH index operation", &inner)),
            }
        }
        Rule::fts_idx_op => {
            let inner = inner.children().expect("the index operation")?;
            match inner.as_rule() {
                Rule::index_create_adv => {
                    let mut inner = inner.children();
                    let rel = inner.expect("the relation's name")?;
                    let name = inner.expect("the index's name")?;
                    let mut filters = vec![];
                    let mut tokenizer = TokenizerConfig {
                        name: Default::default(),
                        args: Default::default(),
                    };
                    let mut extractor: Option<Expr> = None;
                    let mut extract_filter: Option<Expr> = None;
                    for opt_pair in inner {
                        let [opt_name, opt_val] = opt_pair
                            .children()
                            .expect_n(["the option's name", "the option's value"])?;
                        let name_span = opt_name.extract_span();
                        match opt_name.as_str() {
                            "extractor" => {
                                let mut ex = build_expr(opt_val, param_pool)?;
                                ex.partial_eval()?;
                                extractor = Some(ex);
                            }
                            "extract_filter" => {
                                let mut ex = build_expr(opt_val, param_pool)?;
                                ex.partial_eval()?;
                                extract_filter = Some(ex);
                            }
                            "tokenizer" => {
                                let mut expr = build_expr(opt_val, param_pool)?;
                                expr.partial_eval()?;
                                let parsed = parse_tokenizer_expr(expr)?;
                                tokenizer = parsed;
                            }
                            "filters" => {
                                filters = parse_filters_expr(build_expr(opt_val, param_pool)?)?;
                            }
                            _ => {
                                return Err(IndexOptionError(
                                    format!("Unknown option {} for FTS index", opt_name.as_str()),
                                    name_span,
                                )
                                .into());
                            }
                        }
                    }
                    let extractor =
                        combine_extractor(extractor, extract_filter, "FTS", name.extract_span())?;
                    let config = FtsConfigBuilder::new(
                        SmartString::from(rel.as_str()),
                        SmartString::from(name.as_str()),
                    )
                    .tokenizer(tokenizer)
                    .filters(filters)
                    .extractor(extractor)
                    .build();
                    SysOp::CreateFtsIndex(config)
                }
                Rule::index_drop => parse_index_drop(inner)?,
                _ => return Err(unexpected("an FTS index operation", &inner)),
            }
        }
        Rule::vec_idx_op => {
            let inner = inner.children().expect("the index operation")?;
            match inner.as_rule() {
                Rule::index_create_adv => {
                    let create_span = inner.extract_span();
                    let mut inner = inner.children();
                    let rel = inner.expect("the relation's name")?;
                    let name = inner.expect("the index's name")?;
                    // options: the three required fields are collected as
                    // `Option` (a user may omit them — that refusal is
                    // inherently runtime), then handed to the staged builder,
                    // which proves at compile time that all three were set.
                    let mut vec_dim: Option<usize> = None;
                    let mut dtype = VecElementType::F32;
                    let mut vec_fields = vec![];
                    let mut distance = HnswDistance::L2;
                    let mut ef_construction: Option<usize> = None;
                    let mut m_neighbours: Option<usize> = None;
                    let mut index_filter = None;
                    let mut extend_candidates = false;
                    let mut keep_pruned_connections = false;

                    for opt_pair in inner {
                        let [opt_name, opt_val] = opt_pair
                            .children()
                            .expect_n(["the option's name", "the option's value"])?;
                        let opt_val_str = opt_val.as_str();
                        let name_span = opt_name.extract_span();
                        let val_span = opt_val.extract_span();
                        match opt_name.as_str() {
                            "dim" => {
                                let v = build_expr(opt_val, param_pool)?
                                    .eval_to_const()?
                                    .get_int()
                                    .ok_or_else(|| {
                                        IndexOptionError(
                                            format!("Invalid vec_dim: {opt_val_str}"),
                                            val_span,
                                        )
                                    })?;
                                ensure!(
                                    v > 0,
                                    IndexOptionError(format!("Invalid vec_dim: {v}"), val_span)
                                );
                                vec_dim = Some(v as usize);
                            }
                            "ef_construction" | "ef" => {
                                let v = build_expr(opt_val, param_pool)?
                                    .eval_to_const()?
                                    .get_int()
                                    .ok_or_else(|| {
                                        IndexOptionError(
                                            format!("Invalid ef_construction: {opt_val_str}"),
                                            val_span,
                                        )
                                    })?;
                                ensure!(
                                    v > 0,
                                    IndexOptionError(
                                        format!("Invalid ef_construction: {v}"),
                                        val_span,
                                    )
                                );
                                ef_construction = Some(v as usize);
                            }
                            "m_neighbours" | "m" => {
                                let v = build_expr(opt_val, param_pool)?
                                    .eval_to_const()?
                                    .get_int()
                                    .ok_or_else(|| {
                                        IndexOptionError(
                                            format!("Invalid m_neighbours: {opt_val_str}"),
                                            val_span,
                                        )
                                    })?;
                                ensure!(
                                    v > 0,
                                    IndexOptionError(
                                        format!("Invalid m_neighbours: {v}"),
                                        val_span,
                                    )
                                );
                                m_neighbours = Some(v as usize);
                            }
                            "dtype" => {
                                dtype = match opt_val.as_str() {
                                    "F32" | "Float" => VecElementType::F32,
                                    "F64" | "Double" => VecElementType::F64,
                                    _ => {
                                        return Err(IndexOptionError(
                                            format!("Invalid dtype: {}", opt_val.as_str()),
                                            val_span,
                                        )
                                        .into());
                                    }
                                }
                            }
                            "fields" => {
                                let fields = build_expr(opt_val, &Default::default())?;
                                vec_fields = fields.to_var_list()?;
                            }
                            "distance" | "dist" => {
                                distance = match opt_val.as_str().trim() {
                                    "L2" => HnswDistance::L2,
                                    "IP" => HnswDistance::InnerProduct,
                                    "Cosine" => HnswDistance::Cosine,
                                    _ => {
                                        return Err(IndexOptionError(
                                            format!("Invalid distance: {}", opt_val.as_str()),
                                            val_span,
                                        )
                                        .into());
                                    }
                                }
                            }
                            "filter" => {
                                index_filter = Some(opt_val.as_str().to_string());
                            }
                            "extend_candidates" => {
                                extend_candidates = opt_val.as_str().trim() == "true";
                            }
                            "keep_pruned_connections" => {
                                keep_pruned_connections = opt_val.as_str().trim() == "true";
                            }
                            _ => {
                                return Err(IndexOptionError(
                                    format!("Invalid option: {}", opt_name.as_str()),
                                    name_span,
                                )
                                .into());
                            }
                        }
                    }
                    // User omission of a required field is the one runtime
                    // refusal; from here the staged builder proves — at
                    // compile time — that dim, ef, and m are all supplied
                    // before `build()` can be reached.
                    let (Some(vec_dim), Some(ef_construction), Some(m_neighbours)) =
                        (vec_dim, ef_construction, m_neighbours)
                    else {
                        bail!(IndexOptionError(
                            "an HNSW index requires `dim`, `ef_construction`, and \
                             `m_neighbours`"
                                .to_string(),
                            create_span,
                        ));
                    };
                    let config = HnswConfigBuilder::new(
                        SmartString::from(rel.as_str()),
                        SmartString::from(name.as_str()),
                    )
                    .dtype(dtype)
                    .fields(vec_fields)
                    .distance(distance)
                    .filter(index_filter)
                    .extend_candidates(extend_candidates)
                    .keep_pruned_connections(keep_pruned_connections)
                    .dim(vec_dim)
                    .ef(ef_construction)
                    .m(m_neighbours)
                    .build();
                    SysOp::CreateVectorIndex(config)
                }
                Rule::index_drop => parse_index_drop(inner)?,
                _ => return Err(unexpected("an HNSW index operation", &inner)),
            }
        }
        Rule::index_op => {
            let inner = inner.children().expect("the index operation")?;
            match inner.as_rule() {
                Rule::index_create => {
                    let span = inner.extract_span();
                    let mut inner = inner.children();
                    let rel = inner.expect("the relation's name")?;
                    let name = inner.expect("the index's name")?;
                    let cols = inner
                        .map(|p| Symbol::new(p.as_str(), p.extract_span()))
                        .collect_vec();

                    #[derive(Debug, Diagnostic, Error)]
                    #[error("`::index create` needs at least one column")]
                    #[diagnostic(code(parser::empty_index))]
                    #[diagnostic(help(
                        "name the columns to index, e.g. `::index create rel:idx {{col1, col2}}`"
                    ))]
                    struct EmptyIndex(#[label] SourceSpan);

                    ensure!(!cols.is_empty(), EmptyIndex(span));
                    SysOp::CreateIndex(
                        Symbol::new(rel.as_str(), rel.extract_span()),
                        Symbol::new(name.as_str(), name.extract_span()),
                        cols,
                    )
                }
                Rule::index_drop => parse_index_drop(inner)?,
                _ => return Err(unexpected("an index operation", &inner)),
            }
        }
        Rule::list_fixed_rules => SysOp::ListFixedRules,
        _ => return Err(unexpected("a system operation", &inner)),
    })
}

/// The shared `drop rel:idx` shape of every index family.
fn parse_index_drop(inner: crate::parse::Pair<'_>) -> Result<SysOp> {
    let [rel, name] = inner
        .children()
        .expect_n(["the relation's name", "the index's name"])?;
    Ok(SysOp::RemoveIndex(
        Symbol::new(rel.as_str(), rel.extract_span()),
        Symbol::new(name.as_str(), name.extract_span()),
    ))
}

/// A `tokenizer: …` option value: a bare name (`Simple`) or a call with
/// constant arguments (`NGram(1, 3, false)`).
fn parse_tokenizer_expr(expr: Expr) -> Result<TokenizerConfig> {
    // Captured before the match consumes `expr`: the offending option value
    // is labelled where the user wrote it.
    let span = expr.span();
    match expr {
        Expr::UnboundApply { op, args, .. } => {
            let mut targs = vec![];
            for arg in args.iter() {
                let v = arg.clone().eval_to_const()?;
                targs.push(v);
            }
            Ok(TokenizerConfig {
                name: op,
                args: targs,
            })
        }
        Expr::Binding { var, .. } => Ok(TokenizerConfig {
            name: var.name,
            args: vec![],
        }),
        _ => Err(IndexOptionError(
            "Tokenizer must be a symbol or a call for an existing tokenizer".to_string(),
            span,
        )
        .into()),
    }
}

/// A `filters: […]` option value: a list of tokenizer expressions.
fn parse_filters_expr(mut expr: Expr) -> Result<Vec<TokenizerConfig>> {
    expr.partial_eval()?;
    // Captured before the match consumes `expr`: a non-list `filters:` value
    // is labelled where the user wrote it.
    let span = expr.span();
    match expr {
        Expr::Apply { op, args, .. } => {
            if op.name != "OP_LIST" {
                return Err(IndexOptionError(
                    "Filters must be a list of filters".to_string(),
                    span,
                )
                .into());
            }
            let mut filters = vec![];
            for arg in args.iter() {
                filters.push(parse_tokenizer_expr(arg.clone())?);
            }
            Ok(filters)
        }
        _ => Err(IndexOptionError("Filters must be a list of filters".to_string(), span).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The staged builder yields a fully-set HNSW config; `build()` is
    /// reachable only because dim, ef, and m are all supplied (the absence
    /// proofs above witness that any short instantiation cannot reach it).
    #[test]
    fn hnsw_staged_builder_yields_a_complete_config() {
        let cfg = HnswConfigBuilder::new("docs".into(), "by_vec".into())
            .dtype(VecElementType::F64)
            .distance(HnswDistance::Cosine)
            .dim(128)
            .ef(64)
            .m(16)
            .build();
        assert_eq!(cfg.vec_dim, 128);
        assert_eq!(cfg.ef_construction, 64);
        assert_eq!(cfg.m_neighbours, 16);
        assert_eq!(cfg.dtype, VecElementType::F64);
        assert_eq!(cfg.distance, HnswDistance::Cosine);
    }

    /// The FTS and LSH staged builders carry their required extractor through
    /// to the sealed config, with optional fields defaulted or overridden.
    #[test]
    fn fts_and_lsh_staged_builders_carry_the_extractor() {
        let ex = || Expr::Const {
            val: DataValue::from(true),
            span: SourceSpan(0, 0),
        };
        let fts = FtsConfigBuilder::new("docs".into(), "by_text".into())
            .extractor(ex())
            .build();
        assert_eq!(fts.extractor, ex());

        let lsh = MinHashLshConfigBuilder::new("docs".into(), "by_lsh".into())
            .n_gram(3)
            .extractor(ex())
            .build();
        assert_eq!(lsh.extractor, ex());
        assert_eq!(lsh.n_gram, 3);
    }
}
