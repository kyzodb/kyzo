/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): the engine-typed half of the sys-op lift. The pure-data grammar
 * walk and option validation live in kyzo-model (`kyzo_model::parse::sys`);
 * this seat holds `SysOp` and its index-declaration configs — the ones that
 * carry engine objects (`TokenizerConfig`) the model crate must not import —
 * and lifts kyzo-model's [`SysScript`] syntax into them. `AccessLevel`,
 * `HnswDistance`, and `ProcessId` are pure-data leaves re-exported from the
 * model zone so `crate::parse::sys::…` call sites resolve unchanged.
 */

//! The typed lift of a parsed `::…` system script into the engine-shaped
//! [`SysOp`].
//!
//! kyzo-model parses a system script into pure-data
//! [`kyzo_model::parse::sys::SysScript`] syntax — names, constants, extractor
//! `Expr`s, and tokenizer name+args. [`lift`] admits that syntax into a
//! [`SysOp`]: it seals each index-declaration config through its staged
//! builder (so a config missing a required field cannot be built) and
//! admits every tokenizer name into an analyzer [`TokenizerConfig`]. The
//! consumers of a `SysOp` are session-tier (`session/db.rs`'s `run_sys_op`);
//! the values are proven here at the parse boundary.

use std::marker::PhantomData;

use miette::{Diagnostic, Result};
use ordered_float::OrderedFloat;
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use kyzo_model::SourceSpan;
use kyzo_model::assert_not_impl;
use kyzo_model::parse::sys::{
    FtsConfigSpec, HnswConfigSpec, LshConfigSpec, SysScript, TokenizerSpec,
};
use kyzo_model::program::expr::Expr;
use kyzo_model::program::rule::InputProgram;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::schema::VecElementType;
use kyzo_model::typestate::{Set, Unset};
use kyzo_model::value::DataValue;

use crate::project::text::TokenizerConfig;

// The pure-data grammar leaves are owned by the model zone (the grammar's
// `access_level` / `distance` / `kill` productions live there); re-exported
// so `crate::parse::sys::AccessLevel` / `HnswDistance` / `ProcessId` resolve.
pub(crate) use kyzo_model::parse::sys::{AccessLevel, HnswDistance, ProcessId};

/// One parsed system operation, engine-shaped. Variant-for-variant identical
/// to [`SysScript`] except that the three index-create variants carry sealed
/// engine configs here where the syntax carried pure-data specs.
#[derive(Debug)]
pub(crate) enum SysOp {
    Compact,
    /// Whole-keyspace (`None`) or per-relation (`Some`) Merkle state root.
    MerkleRoot(Option<Symbol>),
    ListColumns(Symbol),
    ListIndices(Symbol),
    ListRelations,
    ListRunning,
    ListFixedRules,
    KillRunning(ProcessId),
    Explain(Box<InputProgram>),
    Verify(Box<InputProgram>),
    RemoveRelation(Vec<Symbol>),
    RenameRelation(Vec<(Symbol, Symbol)>),
    ShowTrigger(Symbol),
    SetTriggers(Symbol, Vec<String>, Vec<String>, Vec<String>),
    CreateConstraint(Symbol, String),
    RemoveConstraint(Symbol),
    ListConstraints,
    SetAccessLevel(Vec<Symbol>, AccessLevel),
    CreateIndex(Symbol, Symbol, Vec<Symbol>),
    CreateVectorIndex(HnswIndexConfig),
    CreateFtsIndex(FtsIndexConfig),
    CreateMinHashLshIndex(MinHashLshConfig),
    RemoveIndex(Symbol, Symbol),
    /// Unreachable through the grammar, faithfully carried (see
    /// [`SysScript::DescribeRelation`]).
    DescribeRelation(Symbol, SmartString<LazyCompact>),
}

/// A private witness that an index configuration was produced by its staged
/// builder. Only this module can name it, so `build()` is each config's sole
/// constructor — no struct literal elsewhere can assemble an incomplete,
/// unproven configuration. Its presence carries the completeness proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Built;

/// Configuration of an FTS (full-text search) index, as declared.
/// Constructible ONLY through [`FtsConfigBuilder`] (the private `_built`
/// witness seals it), so every value carries a proven `extractor`.
///
/// **Sole spelling.** This is the only `struct FtsIndexConfig` in the crate.
/// The session tier consumes it by path (`crate::parse::sys::FtsIndexConfig`);
/// the stored catalog form is [`crate::project::text::FtsIndexManifest`], a
/// different concept. Pinned by `fts_index_config_is_the_sole_spelling`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FtsIndexConfig {
    pub(crate) base_relation: SmartString<LazyCompact>,
    pub(crate) index_name: SmartString<LazyCompact>,
    /// The row-extraction expression as a PARSED, partial-evaluated typed
    /// substance — never source text re-parsed at build time.
    pub(crate) extractor: Expr,
    pub(crate) tokenizer: TokenizerConfig,
    pub(crate) filters: Vec<TokenizerConfig>,
    _built: Built,
}

/// Payload of an FTS staged builder — moved as one unit so typestate flips
/// (`extractor` / `build`) cannot become a second field-by-field authority.
struct FtsConfigFields {
    base_relation: SmartString<LazyCompact>,
    index_name: SmartString<LazyCompact>,
    extractor: Expr,
    tokenizer: TokenizerConfig,
    filters: Vec<TokenizerConfig>,
}

/// Staged builder for [`FtsIndexConfig`]. The one required field — the row
/// `extractor` — is a typestate marker, so [`BuildFtsConfig::build`] exists
/// only once it is set.
#[must_use = "an FTS config builder yields nothing until `.build()`, which \
              exists only once the extractor is set"]
pub(crate) struct FtsConfigBuilder<Ex> {
    fields: FtsConfigFields,
    _required: PhantomData<Ex>,
}

impl FtsConfigBuilder<Unset> {
    /// Begin an FTS config from the two always-known identity fields. The
    /// extractor starts `Unset` (its placeholder is never read in a built
    /// config); optional fields take their defaults.
    pub(crate) fn new(
        base_relation: SmartString<LazyCompact>,
        index_name: SmartString<LazyCompact>,
    ) -> Self {
        FtsConfigBuilder {
            fields: FtsConfigFields {
                base_relation,
                index_name,
                extractor: Expr::Const {
                    val: DataValue::Null,
                    span: SourceSpan(0, 0),
                },
                tokenizer: TokenizerConfig::simple(),
                filters: vec![],
            },
            _required: PhantomData,
        }
    }
}

impl<Ex> FtsConfigBuilder<Ex> {
    pub(crate) fn tokenizer(mut self, tokenizer: TokenizerConfig) -> Self {
        self.fields.tokenizer = tokenizer;
        self
    }
    pub(crate) fn filters(mut self, filters: Vec<TokenizerConfig>) -> Self {
        self.fields.filters = filters;
        self
    }
}

impl FtsConfigBuilder<Unset> {
    /// Supply the required row extractor, flipping its marker to `Set`.
    pub(crate) fn extractor(mut self, extractor: Expr) -> FtsConfigBuilder<Set> {
        self.fields.extractor = extractor;
        FtsConfigBuilder {
            fields: self.fields,
            _required: PhantomData,
        }
    }
}

/// The build door for an FTS configuration: implemented ONLY for the
/// extractor-set builder, so a config without an extractor cannot be built.
pub(crate) trait BuildFtsConfig {
    fn build(self) -> FtsIndexConfig;
}

impl BuildFtsConfig for FtsConfigBuilder<Set> {
    fn build(self) -> FtsIndexConfig {
        let FtsConfigFields {
            base_relation,
            index_name,
            extractor,
            tokenizer,
            filters,
        } = self.fields;
        FtsIndexConfig {
            base_relation,
            index_name,
            extractor,
            tokenizer,
            filters,
            _built: Built,
        }
    }
}

/// Configuration of a MinHash-LSH (locality-sensitive hashing) index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MinHashLshConfig {
    pub(crate) base_relation: SmartString<LazyCompact>,
    pub(crate) index_name: SmartString<LazyCompact>,
    pub(crate) extractor: Expr,
    pub(crate) tokenizer: TokenizerConfig,
    pub(crate) filters: Vec<TokenizerConfig>,
    pub(crate) n_gram: usize,
    pub(crate) n_perm: usize,
    pub(crate) false_positive_weight: OrderedFloat<f64>,
    pub(crate) false_negative_weight: OrderedFloat<f64>,
    pub(crate) target_threshold: OrderedFloat<f64>,
    _built: Built,
}

/// Payload of a MinHash-LSH staged builder — moved as one unit so typestate
/// flips cannot become a second field-by-field authority (copy_detector).
struct MinHashLshConfigFields {
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
}

/// Staged builder for [`MinHashLshConfig`]. The one required field — the row
/// `extractor` — is a typestate marker.
#[must_use = "a MinHash-LSH config builder yields nothing until `.build()`, \
              which exists only once the extractor is set"]
pub(crate) struct MinHashLshConfigBuilder<Ex> {
    fields: MinHashLshConfigFields,
    _required: PhantomData<Ex>,
}

impl MinHashLshConfigBuilder<Unset> {
    /// Begin a MinHash-LSH config from the two always-known identity fields.
    pub(crate) fn new(
        base_relation: SmartString<LazyCompact>,
        index_name: SmartString<LazyCompact>,
    ) -> Self {
        MinHashLshConfigBuilder {
            fields: MinHashLshConfigFields {
                base_relation,
                index_name,
                extractor: Expr::Const {
                    val: DataValue::Null,
                    span: SourceSpan(0, 0),
                },
                tokenizer: TokenizerConfig::simple(),
                filters: vec![],
                n_gram: 1,
                n_perm: 200,
                false_positive_weight: OrderedFloat(1.0),
                false_negative_weight: OrderedFloat(1.0),
                target_threshold: OrderedFloat(0.9),
            },
            _required: PhantomData,
        }
    }
}

impl<Ex> MinHashLshConfigBuilder<Ex> {
    pub(crate) fn tokenizer(mut self, tokenizer: TokenizerConfig) -> Self {
        self.fields.tokenizer = tokenizer;
        self
    }
    pub(crate) fn filters(mut self, filters: Vec<TokenizerConfig>) -> Self {
        self.fields.filters = filters;
        self
    }
    pub(crate) fn n_gram(mut self, n_gram: usize) -> Self {
        self.fields.n_gram = n_gram;
        self
    }
    pub(crate) fn n_perm(mut self, n_perm: usize) -> Self {
        self.fields.n_perm = n_perm;
        self
    }
    pub(crate) fn weights(mut self, false_positive: f64, false_negative: f64) -> Self {
        self.fields.false_positive_weight = OrderedFloat(false_positive);
        self.fields.false_negative_weight = OrderedFloat(false_negative);
        self
    }
    pub(crate) fn target_threshold(mut self, target_threshold: f64) -> Self {
        self.fields.target_threshold = OrderedFloat(target_threshold);
        self
    }
}

impl MinHashLshConfigBuilder<Unset> {
    /// Supply the required row extractor, flipping its marker to `Set`.
    pub(crate) fn extractor(mut self, extractor: Expr) -> MinHashLshConfigBuilder<Set> {
        self.fields.extractor = extractor;
        MinHashLshConfigBuilder {
            fields: self.fields,
            _required: PhantomData,
        }
    }
}

/// The build door for a MinHash-LSH configuration.
pub(crate) trait BuildMinHashLshConfig {
    fn build(self) -> MinHashLshConfig;
}

impl BuildMinHashLshConfig for MinHashLshConfigBuilder<Set> {
    fn build(self) -> MinHashLshConfig {
        let MinHashLshConfigFields {
            base_relation,
            index_name,
            extractor,
            tokenizer,
            filters,
            n_gram,
            n_perm,
            false_positive_weight,
            false_negative_weight,
            target_threshold,
        } = self.fields;
        MinHashLshConfig {
            base_relation,
            index_name,
            extractor,
            tokenizer,
            filters,
            n_gram,
            n_perm,
            false_positive_weight,
            false_negative_weight,
            target_threshold,
            _built: Built,
        }
    }
}

/// Configuration of an HNSW vector index. Constructible ONLY through
/// [`HnswConfigBuilder`] (the private `_built` witness seals it), so every
/// value is proven complete: `vec_dim`, `ef_construction`, and `m_neighbours`
/// are always present, never a sentinel checked after assembly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HnswIndexConfig {
    pub(crate) base_relation: SmartString<LazyCompact>,
    pub(crate) index_name: SmartString<LazyCompact>,
    pub(crate) vec_dim: usize,
    pub(crate) dtype: VecElementType,
    pub(crate) vec_fields: Vec<SmartString<LazyCompact>>,
    pub(crate) distance: HnswDistance,
    pub(crate) ef_construction: usize,
    pub(crate) m_neighbours: usize,
    pub(crate) index_filter: Option<Expr>,
    pub(crate) extend_candidates: bool,
    pub(crate) keep_pruned_connections: bool,
    _built: Built,
}

/// Payload of an HNSW staged builder — moved as one unit so typestate
/// `remark` / `build` cannot become a second field-by-field authority.
struct HnswConfigFields {
    base_relation: SmartString<LazyCompact>,
    index_name: SmartString<LazyCompact>,
    vec_dim: usize,
    ef_construction: usize,
    m_neighbours: usize,
    dtype: VecElementType,
    vec_fields: Vec<SmartString<LazyCompact>>,
    distance: HnswDistance,
    index_filter: Option<Expr>,
    extend_candidates: bool,
    keep_pruned_connections: bool,
}

/// Staged builder for [`HnswIndexConfig`]. Its three required numeric fields —
/// `dim`, `ef`, and `m` — are typestate markers, so [`BuildHnswConfig::build`]
/// exists only on the fully-`Set` instantiation: a config missing any of them
/// is a COMPILE error, not an `Option`/sentinel validated at run time.
#[must_use = "an HNSW config builder yields nothing until `.build()`, which \
              exists only once dim, ef, and m are all set"]
pub(crate) struct HnswConfigBuilder<Dim, Ef, M> {
    fields: HnswConfigFields,
    _required: PhantomData<(Dim, Ef, M)>,
}

impl HnswConfigBuilder<Unset, Unset, Unset> {
    /// Begin an HNSW config from the two always-known identity fields.
    pub(crate) fn new(
        base_relation: SmartString<LazyCompact>,
        index_name: SmartString<LazyCompact>,
    ) -> Self {
        HnswConfigBuilder {
            fields: HnswConfigFields {
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
            },
            _required: PhantomData,
        }
    }
}

impl<Dim, Ef, M> HnswConfigBuilder<Dim, Ef, M> {
    /// Retype the required-field markers without re-listing payload fields.
    fn remark<D2, E2, M2>(self) -> HnswConfigBuilder<D2, E2, M2> {
        HnswConfigBuilder {
            fields: self.fields,
            _required: PhantomData,
        }
    }

    pub(crate) fn dtype(mut self, dtype: VecElementType) -> Self {
        self.fields.dtype = dtype;
        self
    }
    pub(crate) fn distance(mut self, distance: HnswDistance) -> Self {
        self.fields.distance = distance;
        self
    }
    pub(crate) fn fields(mut self, vec_fields: Vec<SmartString<LazyCompact>>) -> Self {
        self.fields.vec_fields = vec_fields;
        self
    }
    pub(crate) fn filter(mut self, index_filter: Option<Expr>) -> Self {
        self.fields.index_filter = index_filter;
        self
    }
    pub(crate) fn extend_candidates(mut self, extend_candidates: bool) -> Self {
        self.fields.extend_candidates = extend_candidates;
        self
    }
    pub(crate) fn keep_pruned_connections(mut self, keep_pruned_connections: bool) -> Self {
        self.fields.keep_pruned_connections = keep_pruned_connections;
        self
    }
}

// Each required setter is callable only while its own field is `Unset` and
// flips exactly that field's marker to `Set`.
impl<Ef, M> HnswConfigBuilder<Unset, Ef, M> {
    /// Set vector dimension. Same law as `::hnsw` parse: `dim >= 1` (P092).
    pub(crate) fn dim(
        mut self,
        vec_dim: usize,
    ) -> std::result::Result<HnswConfigBuilder<Set, Ef, M>, HnswDimLawError> {
        if vec_dim < 1 {
            return Err(HnswDimLawError(vec_dim));
        }
        self.fields.vec_dim = vec_dim;
        Ok(self.remark())
    }
}
impl<Dim, M> HnswConfigBuilder<Dim, Unset, M> {
    pub(crate) fn ef(mut self, ef_construction: usize) -> HnswConfigBuilder<Dim, Set, M> {
        self.fields.ef_construction = ef_construction;
        self.remark()
    }
}
impl<Dim, Ef> HnswConfigBuilder<Dim, Ef, Unset> {
    /// Set `m` neighbours. Same law as `::hnsw` parse: `m >= 2` (P092).
    pub(crate) fn m(
        mut self,
        m_neighbours: usize,
    ) -> std::result::Result<HnswConfigBuilder<Dim, Ef, Set>, HnswMLawError> {
        if m_neighbours < 2 {
            return Err(HnswMLawError(m_neighbours));
        }
        self.fields.m_neighbours = m_neighbours;
        Ok(self.remark())
    }
}

/// `HnswConfigBuilder::dim` refused a non-positive dimension (P092).
#[derive(Debug, Error, Diagnostic)]
#[error("HNSW dim must be >= 1, got {0}")]
#[diagnostic(code(parser::hnsw_dim_law))]
pub(crate) struct HnswDimLawError(pub(crate) usize);

/// `HnswConfigBuilder::m` refused `m < 2` (P092).
#[derive(Debug, Error, Diagnostic)]
#[error("HNSW m must be >= 2, got {0}")]
#[diagnostic(code(parser::hnsw_m_law))]
pub(crate) struct HnswMLawError(pub(crate) usize);

/// The build door for an HNSW configuration: implemented ONLY for the
/// fully-set builder, so a config missing `dim`, `ef`, or `m` cannot be built.
pub(crate) trait BuildHnswConfig {
    fn build(self) -> HnswIndexConfig;
}

impl BuildHnswConfig for HnswConfigBuilder<Set, Set, Set> {
    fn build(self) -> HnswIndexConfig {
        let HnswConfigFields {
            base_relation,
            index_name,
            vec_dim,
            ef_construction,
            m_neighbours,
            dtype,
            vec_fields,
            distance,
            index_filter,
            extend_candidates,
            keep_pruned_connections,
        } = self.fields;
        HnswIndexConfig {
            base_relation,
            index_name,
            vec_dim,
            dtype,
            vec_fields,
            distance,
            ef_construction,
            m_neighbours,
            index_filter,
            extend_candidates,
            keep_pruned_connections,
            _built: Built,
        }
    }
}

// ── Compile-fail proofs ────────────────────────────────────────────────────
//
// `build()` — carried by each config's build trait — is ABSENT on every
// INCOMPLETE builder instantiation: "a config missing a required field cannot
// compile", witnessed mechanically. Each line fails to compile the moment the
// build trait leaks to an under-set typestate.

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

/// A tokenizer/filter stage the analyzer does not know, labelled at the
/// option that named it. The model zone validated the *shape* (a symbol or a
/// call); the engine owns the *name* proof ([`TokenizerConfig::admit`]).
#[derive(Debug, Error, Diagnostic)]
#[error("{0}")]
#[diagnostic(code(parser::index_option))]
struct UnknownTokenizerStage(String, #[label("no such tokenizer stage")] SourceSpan);

/// Admit one parsed [`TokenizerSpec`] into an analyzer [`TokenizerConfig`],
/// carrying the option span onto an unknown-stage refusal.
fn admit_tokenizer(spec: TokenizerSpec) -> Result<TokenizerConfig> {
    let span = spec.span;
    TokenizerConfig::admit(spec.name, spec.args)
        .map_err(|e| UnknownTokenizerStage(e.to_string(), span).into())
}

/// Admit a list of tokenizer specs (an index's `filters:`).
fn admit_tokenizers(specs: Vec<TokenizerSpec>) -> Result<Vec<TokenizerConfig>> {
    specs.into_iter().map(admit_tokenizer).collect()
}

/// Seal a parsed [`HnswConfigSpec`] into a proven [`HnswIndexConfig`]. The
/// numeric laws (`dim >= 1`, `m >= 2`) the model zone already checked are
/// re-proven by the staged builder — a redundant proof, not a second policy.
fn lift_hnsw(spec: HnswConfigSpec) -> Result<HnswIndexConfig> {
    Ok(HnswConfigBuilder::new(spec.base_relation, spec.index_name)
        .dtype(spec.dtype)
        .fields(spec.vec_fields)
        .distance(spec.distance)
        .filter(spec.index_filter)
        .extend_candidates(spec.extend_candidates)
        .keep_pruned_connections(spec.keep_pruned_connections)
        .dim(spec.vec_dim)?
        .ef(spec.ef_construction)
        .m(spec.m_neighbours)?
        .build())
}

/// Admit tokenizer + filters once for text-index lifts (copy_detector).
fn admit_text_stages(
    tokenizer: TokenizerSpec,
    filters: Vec<TokenizerSpec>,
) -> Result<(TokenizerConfig, Vec<TokenizerConfig>)> {
    Ok((admit_tokenizer(tokenizer)?, admit_tokenizers(filters)?))
}

/// Seal a parsed [`FtsConfigSpec`] into a proven [`FtsIndexConfig`].
fn lift_fts(spec: FtsConfigSpec) -> Result<FtsIndexConfig> {
    let (tokenizer, filters) = admit_text_stages(spec.tokenizer, spec.filters)?;
    Ok(FtsConfigBuilder::new(spec.base_relation, spec.index_name)
        .tokenizer(tokenizer)
        .filters(filters)
        .extractor(spec.extractor)
        .build())
}

/// Seal a parsed [`LshConfigSpec`] into a proven [`MinHashLshConfig`].
fn lift_lsh(spec: LshConfigSpec) -> Result<MinHashLshConfig> {
    let (tokenizer, filters) = admit_text_stages(spec.tokenizer, spec.filters)?;
    Ok(
        MinHashLshConfigBuilder::new(spec.base_relation, spec.index_name)
            .tokenizer(tokenizer)
            .filters(filters)
            .n_gram(spec.n_gram)
            .n_perm(spec.n_perm)
            .weights(spec.false_positive_weight, spec.false_negative_weight)
            .target_threshold(spec.target_threshold)
            .extractor(spec.extractor)
            .build(),
    )
}

/// Lift a pure-data [`SysScript`] into an engine-shaped [`SysOp`]: seal the
/// index-declaration configs and admit their tokenizers; every other variant
/// is a direct one-to-one carry of already-proven pure data.
pub(crate) fn lift(script: SysScript) -> Result<SysOp> {
    Ok(match script {
        SysScript::Compact => SysOp::Compact,
        SysScript::MerkleRoot(rel) => SysOp::MerkleRoot(rel),
        SysScript::ListColumns(rel) => SysOp::ListColumns(rel),
        SysScript::ListIndices(rel) => SysOp::ListIndices(rel),
        SysScript::ListRelations => SysOp::ListRelations,
        SysScript::ListRunning => SysOp::ListRunning,
        SysScript::ListFixedRules => SysOp::ListFixedRules,
        SysScript::KillRunning(pid) => SysOp::KillRunning(pid),
        SysScript::Explain(prog) => SysOp::Explain(prog),
        SysScript::Verify(prog) => SysOp::Verify(prog),
        SysScript::RemoveRelation(rels) => SysOp::RemoveRelation(rels),
        SysScript::RenameRelation(pairs) => SysOp::RenameRelation(pairs),
        SysScript::ShowTrigger(rel) => SysOp::ShowTrigger(rel),
        SysScript::SetTriggers(rel, puts, rms, replaces) => {
            SysOp::SetTriggers(rel, puts, rms, replaces)
        }
        SysScript::CreateConstraint(name, source) => SysOp::CreateConstraint(name, source),
        SysScript::RemoveConstraint(name) => SysOp::RemoveConstraint(name),
        SysScript::ListConstraints => SysOp::ListConstraints,
        SysScript::SetAccessLevel(rels, level) => SysOp::SetAccessLevel(rels, level),
        SysScript::CreateIndex(rel, name, cols) => SysOp::CreateIndex(rel, name, cols),
        SysScript::CreateVectorIndex(spec) => SysOp::CreateVectorIndex(lift_hnsw(spec)?),
        SysScript::CreateFtsIndex(spec) => SysOp::CreateFtsIndex(lift_fts(spec)?),
        SysScript::CreateMinHashLshIndex(spec) => SysOp::CreateMinHashLshIndex(lift_lsh(spec)?),
        SysScript::RemoveIndex(rel, idx) => SysOp::RemoveIndex(rel, idx),
        SysScript::DescribeRelation(rel, desc) => SysOp::DescribeRelation(rel, desc),
    })
}

#[cfg(test)]
mod tests {
    use miette::{IntoDiagnostic, Result};

    use super::*;

    /// The staged builder yields a fully-set HNSW config; `build()` is
    /// reachable only because dim, ef, and m are all supplied.
    #[test]
    fn hnsw_staged_builder_yields_a_complete_config() -> Result<()> {
        let cfg = HnswConfigBuilder::new("docs".into(), "by_vec".into())
            .dtype(VecElementType::F64)
            .distance(HnswDistance::Cosine)
            .dim(128)?
            .ef(64)
            .m(16)?
            .build();
        assert_eq!(cfg.vec_dim, 128);
        assert_eq!(cfg.ef_construction, 64);
        assert_eq!(cfg.m_neighbours, 16);
        assert_eq!(cfg.dtype, VecElementType::F64);
        assert_eq!(cfg.distance, HnswDistance::Cosine);
        Ok(())
    }

    /// Builder setters enforce the same dim/m laws as `::hnsw` parse (P092).
    #[test]
    fn hnsw_builder_refuses_illegal_dim_and_m() -> Result<()> {
        assert!(
            HnswConfigBuilder::new("docs".into(), "by_vec".into())
                .dim(0)
                .is_err()
        );
        assert!(
            HnswConfigBuilder::new("docs".into(), "by_vec".into())
                .dim(1)?
                .ef(8)
                .m(1)
                .is_err()
        );
        Ok(())
    }

    /// Negative process ids are unconstructible (P081).
    #[test]
    fn process_id_refuses_negatives() -> Result<()> {
        assert!(ProcessId::try_from_i64(-1).is_err());
        assert_eq!(ProcessId::try_from_i64(0)?.get(), 0);
        assert_eq!(ProcessId::try_from_i64(42)?.get(), 42);
        Ok(())
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

    /// One `struct FtsIndexConfig` spelling. The session tier names this
    /// module's type by path; a second definition must not exist anywhere
    /// under this crate's `src/`.
    #[test]
    fn fts_index_config_is_the_sole_spelling() -> Result<()> {
        fn session_tier_consumes(cfg: &crate::parse::sys::FtsIndexConfig) {
            fn takes_local(_: &FtsIndexConfig) {}
            takes_local(cfg);
        }
        match session_tier_consumes {
            f => core::mem::drop(f),
        }

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut defs = Vec::new();
        fn walk(dir: &std::path::Path, defs: &mut Vec<std::path::PathBuf>) -> Result<()> {
            for entry in std::fs::read_dir(dir).into_diagnostic()? {
                let entry = entry.into_diagnostic()?;
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, defs)?;
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let src = std::fs::read_to_string(&path).into_diagnostic()?;
                if src.contains("struct FtsIndexConfig") {
                    defs.push(path);
                }
            }
            Ok(())
        }
        walk(&root, &mut defs)?;
        assert_eq!(
            defs,
            [root.join("parse/sys.rs")],
            "exactly one `struct FtsIndexConfig` — parse/sys.rs owns the sole spelling"
        );
        Ok(())
    }
}
