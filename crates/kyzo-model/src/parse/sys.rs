/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): seated in kyzo-model as the pure-data half of the sys-op lift.
 * The grammar walk, option validation, and constant folding land here; the
 * engine-typed second half — admitting a tokenizer name+args to an analyzer
 * config, and sealing an index configuration through its staged builder —
 * lives in kyzo-core (`crate::parse::sys`), which lifts this [`SysScript`]
 * into its `SysOp`.
 */

//! Parsing `::…` system scripts into pure-data syntax.
//!
//! A [`SysScript`] is one parsed, validated administrative command as pure
//! data: relation names as [`Symbol`], option values evaluated to
//! constants and range-checked, index-declaration configs as
//! [`HnswConfigSpec`] / [`FtsConfigSpec`] / [`LshConfigSpec`]. It carries
//! everything the grammar promised, with one deliberate seam:
//!
//! **The tokenizer seam.** A `tokenizer:` / `filters:` option names an
//! analyzer stage (`Simple`, `NGram(1, 3, false)`, …). The *analyzer* is an
//! engine object (`kyzo-core`'s `project/text`), which the model crate must
//! never import. So this zone carries the tokenizer as a [`TokenizerSpec`]
//! — the validated name and constant args — and leaves the admission of
//! that spec into an analyzer config to the engine-typed lift. Everything
//! else in an index config is pure data (dimensions, distances, extractor
//! [`Expr`]s), so it is fully built here; only the tokenizer name-proof
//! crosses the wall unresolved.

use std::collections::BTreeMap;

use miette::{Diagnostic, Result, bail, ensure};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::program::expr::Expr;
use crate::program::op::OP_LIST;
use crate::program::rule::InputProgram;
use crate::program::span::SourceSpan;
use crate::program::symbol::Symbol;
use crate::schema::column::VecElementType;
use crate::value::{DataValue, ValidityTs};

use super::expr::{build_expr, parse_string};
use super::query::parse_query;
use super::{ExtractSpan, IntoChildren, Pairs, Rule, unexpected};

/// How accessible a stored relation is to queries and mutations, as parsed.
/// The parse-tier twin of the catalog's access level (kyzo-core maps this
/// into its own `session::access::AccessLevel`); the two tiers keep
/// distinct types. **The `Ord` derive IS the semantics** — `Hidden <
/// ReadOnly < Protected < Normal`, each level permitting strictly more than
/// the one below. Do not reorder the variants.
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

/// The distance metric of an HNSW index, as declared. Pure data (the value
/// plane owns no metric); the engine's HNSW kernels and persisted manifest
/// consume it by path.
#[allow(missing_docs)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde_derive::Serialize, serde_derive::Deserialize,
)]
pub enum HnswDistance {
    L2,
    InnerProduct,
    Cosine,
}

/// Non-negative process id for `::kill` (P081). Negatives are unconstructible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessId(u64);

impl ProcessId {
    /// Admit a non-negative integer as a process id.
    pub fn try_from_i64(v: i64) -> std::result::Result<Self, NegativeProcessId> {
        u64::try_from(v).map(Self).map_err(|_| NegativeProcessId(v))
    }

    /// The underlying non-negative id.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Negative integer offered where a [`ProcessId`] is required.
#[derive(Debug, Error, Diagnostic)]
#[error("`::kill` process ID must be non-negative, got {0}")]
#[diagnostic(code(parser::kill_pid_negative))]
#[diagnostic(help("write a non-negative process ID"))]
pub struct NegativeProcessId(pub i64);

/// A named analyzer stage — a tokenizer or token filter — as parsed: the
/// stage name (`Simple`, `NGram`, …) and its constant arguments, with the
/// option's span. The engine-typed lift admits the name and range-checks
/// the args into an analyzer config; here it is a pure name-and-values
/// carrier that crosses the crate wall unresolved (see the module doc).
#[derive(Debug, Clone, PartialEq)]
pub struct TokenizerSpec {
    /// The analyzer stage name (`Simple`, `NGram`, `Lowercase`, …).
    pub name: SmartString<LazyCompact>,
    /// The stage's constant arguments, already evaluated.
    pub args: Vec<DataValue>,
    /// Where the option that named this stage was written.
    pub span: SourceSpan,
}

impl TokenizerSpec {
    /// The default `Simple` tokenizer used when no `tokenizer:` option is
    /// given (the engine admits `"Simple"` to its simple analyzer).
    fn simple(span: SourceSpan) -> Self {
        TokenizerSpec {
            name: SmartString::from("Simple"),
            args: vec![],
            span,
        }
    }
}

/// A declared HNSW vector index, as parsed. Every field is pure data:
/// `vec_dim` / `ef_construction` / `m_neighbours` are range-checked here,
/// `index_filter` is a parsed typed predicate (never source text). The
/// engine-typed lift seals this into its `HnswIndexConfig` through a staged
/// builder that re-proves completeness at compile time.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq)]
pub struct HnswConfigSpec {
    pub base_relation: SmartString<LazyCompact>,
    pub index_name: SmartString<LazyCompact>,
    pub vec_dim: usize,
    pub dtype: VecElementType,
    pub vec_fields: Vec<SmartString<LazyCompact>>,
    pub distance: HnswDistance,
    pub ef_construction: usize,
    pub m_neighbours: usize,
    pub index_filter: Option<Expr>,
    pub extend_candidates: bool,
    pub keep_pruned_connections: bool,
}

/// A declared FTS index, as parsed. The `extractor` is a parsed,
/// partial-evaluated typed [`Expr`] (an `extract_filter` is folded in as a
/// typed conditional, not a textual splice); `tokenizer` / `filters` are
/// [`TokenizerSpec`]s the engine-typed lift admits into analyzer configs.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq)]
pub struct FtsConfigSpec {
    pub base_relation: SmartString<LazyCompact>,
    pub index_name: SmartString<LazyCompact>,
    pub extractor: Expr,
    pub tokenizer: TokenizerSpec,
    pub filters: Vec<TokenizerSpec>,
}

/// A declared MinHash-LSH index, as parsed. Like [`FtsConfigSpec`] plus the
/// LSH numeric parameters, already range-checked and (for the two weights)
/// normalized to sum to one — exactly as the CozoDB original did at parse.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq)]
pub struct LshConfigSpec {
    pub base_relation: SmartString<LazyCompact>,
    pub index_name: SmartString<LazyCompact>,
    pub extractor: Expr,
    pub tokenizer: TokenizerSpec,
    pub filters: Vec<TokenizerSpec>,
    pub n_gram: usize,
    pub n_perm: usize,
    pub false_positive_weight: f64,
    pub false_negative_weight: f64,
    pub target_threshold: f64,
}

/// One parsed system operation, as pure data. The engine-typed lift
/// (`kyzo-core`'s `crate::parse::sys`) turns each variant into its `SysOp`
/// counterpart; the two enums are variant-for-variant identical except that
/// the three index-create variants carry [`*ConfigSpec`](HnswConfigSpec)
/// pure syntax here and sealed engine configs there.
#[allow(missing_docs)]
#[derive(Debug)]
pub enum SysScript {
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
    /// Trigger bodies as provenance source text (put / rm / replace),
    /// validation-parsed here and re-parsed once at the store boundary.
    SetTriggers(Symbol, Vec<String>, Vec<String>, Vec<String>),
    /// A named denial rule; the body is stored as raw source (same inherited
    /// convention as [`SysScript::SetTriggers`]).
    CreateConstraint(Symbol, String),
    RemoveConstraint(Symbol),
    ListConstraints,
    SetAccessLevel(Vec<Symbol>, AccessLevel),
    CreateIndex(Symbol, Symbol, Vec<Symbol>),
    CreateVectorIndex(HnswConfigSpec),
    CreateFtsIndex(FtsConfigSpec),
    CreateMinHashLshIndex(LshConfigSpec),
    RemoveIndex(Symbol, Symbol),
    /// Unreachable through the grammar (`sys_script` never includes
    /// `describe_relation_op`), faithfully ported so the engine-typed lift's
    /// `SysOp::DescribeRelation` consumer stays satisfied.
    DescribeRelation(Symbol, SmartString<LazyCompact>),
}

/// A rejected option in an `::hnsw`/`::fts`/`::lsh` index-DDL clause,
/// labelled at the offending option name or value. One typed carrier for
/// the whole option-validation family; the message is the specific
/// validation failure, the span points at the exact construct.
#[derive(Debug, Error, Diagnostic)]
#[error("{0}")]
#[diagnostic(code(parser::index_option))]
struct IndexOptionError(String, #[label("invalid index option")] SourceSpan);

/// A `::kill` argument that evaluates to a non-integer value.
#[derive(Debug, Error, Diagnostic)]
#[error("`::kill` needs a process ID, not this")]
#[diagnostic(code(parser::kill_pid_not_integer))]
#[diagnostic(help("write the process ID as an integer literal or a `$parameter` bound to one"))]
struct ProcessIdNotInteger(#[label("this must evaluate to an integer process ID")] SourceSpan);

/// Spanned refusal for a negative `::kill` process id.
#[derive(Debug, Error, Diagnostic)]
#[error("`::kill` process ID must be non-negative, got {0}")]
#[diagnostic(code(parser::kill_pid_negative))]
#[diagnostic(help("write a non-negative process ID"))]
struct NegativeProcessIdSpanned(i64, #[label("negative process ID")] SourceSpan);

/// Fold an optional `extract_filter` predicate into the row `extractor` as a
/// typed `if(filter, extractor)` conditional — the exact shape a two-arg
/// `if(cond, then)` builds (filter true yields the extractor, else `Null`),
/// but constructed from already-parsed sub-expressions rather than spliced
/// source text. A missing extractor is a typed refusal HERE: an index with
/// nothing to extract is a definition error surfaced at parse.
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

/// Walk the pairs of a `sys_script` into pure-data [`SysScript`] syntax.
pub(crate) fn parse_sys(
    mut src: Pairs<'_>,
    param_pool: &BTreeMap<String, DataValue>,
    cur_vld: ValidityTs,
) -> Result<SysScript> {
    #[derive(Debug, Error, Diagnostic)]
    #[error("parse-tree shape violates the grammar: sys_script has no operation")]
    #[diagnostic(code(parser::grammar_shape))]
    #[diagnostic(help("This is a bug: grammar.pest and its consumer disagree. Please report it."))]
    struct EmptySysScript;
    let inner = src.next().ok_or(EmptySysScript)?;
    Ok(match inner.as_rule() {
        Rule::compact_op => SysScript::Compact,
        Rule::running_op => SysScript::ListRunning,
        Rule::kill_op => {
            let i_expr = inner.children().expect("the process id expression")?;
            let span = i_expr.extract_span();
            let i_val = build_expr(i_expr, param_pool)?;
            let i_val = i_val.eval_to_const()?;
            let i_val = i_val.get_int().ok_or(ProcessIdNotInteger(span))?;
            let pid = ProcessId::try_from_i64(i_val)
                .map_err(|NegativeProcessId(v)| NegativeProcessIdSpanned(v, span))?;
            SysScript::KillRunning(pid)
        }
        Rule::explain_op => {
            let prog = parse_query(
                inner
                    .children()
                    .expect("the query to explain")?
                    .into_inner(),
                param_pool,
                cur_vld,
            )?;
            SysScript::Explain(Box::new(prog))
        }
        Rule::verify_op => {
            let prog = parse_query(
                inner.children().expect("the query to verify")?.into_inner(),
                param_pool,
                cur_vld,
            )?;
            SysScript::Verify(Box::new(prog))
        }
        Rule::describe_relation_op => {
            let mut inner = inner.children();
            let rels_p = inner.expect("the relation's name")?;
            let rel = Symbol::new(rels_p.as_str(), rels_p.extract_span());
            let description = match inner.next() {
                None => Default::default(),
                Some(desc_p) => parse_string(desc_p)?,
            };
            SysScript::DescribeRelation(rel, description)
        }
        Rule::list_relations_op => SysScript::ListRelations,
        Rule::remove_relations_op => {
            let rel = inner
                .into_inner()
                .map(|rels_p| Symbol::new(rels_p.as_str(), rels_p.extract_span()))
                .collect::<Vec<_>>();
            SysScript::RemoveRelation(rel)
        }
        Rule::list_columns_op => {
            let rels_p = inner.children().expect("the relation's name")?;
            let rel = Symbol::new(rels_p.as_str(), rels_p.extract_span());
            SysScript::ListColumns(rel)
        }
        Rule::list_indices_op => {
            let rels_p = inner.children().expect("the relation's name")?;
            let rel = Symbol::new(rels_p.as_str(), rels_p.extract_span());
            SysScript::ListIndices(rel)
        }
        Rule::rename_relations_op => {
            let rename_pairs = inner
                .into_inner()
                .map(|pair| -> Result<(Symbol, Symbol)> {
                    let [old_p, new_p] = pair
                        .children()
                        .expect_n(["the old relation name", "the new relation name"])?;
                    let rel = Symbol::new(old_p.as_str(), old_p.extract_span());
                    let new_rel = Symbol::new(new_p.as_str(), new_p.extract_span());
                    Ok((rel, new_rel))
                })
                .collect::<Result<Vec<_>>>()?;
            SysScript::RenameRelation(rename_pairs)
        }
        Rule::access_level_op => {
            let mut ps = inner.children();
            let level_p = ps.expect("the access level")?;
            let access_level = match level_p.as_str() {
                "normal" => AccessLevel::Normal,
                "protected" => AccessLevel::Protected,
                "read_only" => AccessLevel::ReadOnly,
                "hidden" => AccessLevel::Hidden,
                _other => return Err(unexpected("an access level", &level_p)),
            };
            let mut rels = vec![];
            for rel_p in ps {
                let rel = Symbol::new(rel_p.as_str(), rel_p.extract_span());
                rels.push(rel)
            }
            SysScript::SetAccessLevel(rels, access_level)
        }
        Rule::trigger_relation_show_op => {
            let rels_p = inner.children().expect("the relation's name")?;
            let rel = Symbol::new(rels_p.as_str(), rels_p.extract_span());
            SysScript::ShowTrigger(rel)
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
                // Validation parse only: the body is stored as source text and
                // re-parsed at fire time (inherited convention). Parameters
                // deliberately empty — the firing context supplies its own.
                parse_query(script.into_inner(), &Default::default(), cur_vld)?;
                match op.as_rule() {
                    Rule::trigger_put => puts.push(script_str.to_string()),
                    Rule::trigger_rm => rms.push(script_str.to_string()),
                    Rule::trigger_replace => replaces.push(script_str.to_string()),
                    _other => return Err(unexpected("a trigger kind", &op)),
                }
            }
            SysScript::SetTriggers(rel, puts, rms, replaces)
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
                    // convention; see `SysScript::SetTriggers`). Parameters
                    // deliberately empty — a constraint is a standing rule
                    // and binds no caller parameters.
                    parse_query(script.into_inner(), &Default::default(), cur_vld)?;
                    SysScript::CreateConstraint(name, script_str.to_string())
                }
                Rule::constraint_drop => {
                    let name_p = op.children().expect("the constraint's name")?;
                    SysScript::RemoveConstraint(Symbol::new(name_p.as_str(), name_p.extract_span()))
                }
                Rule::constraint_list => SysScript::ListConstraints,
                _other => return Err(unexpected("a constraint operation", &op)),
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
                    let mut tokenizer = TokenizerSpec::simple(create_span);
                    let mut extractor: Option<Expr> = None;
                    let mut extract_filter: Option<Expr> = None;
                    let mut n_gram = 1;
                    let mut n_perm = 200;
                    let mut target_threshold = 0.9;
                    let mut false_positive_weight = 1.0;
                    let mut false_negative_weight = 1.0;
                    // Spans of the offending option values, for the post-loop
                    // range checks: an out-of-range value is labelled where the
                    // user wrote it; an option left at its default falls back
                    // to the whole create clause.
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
                                n_gram = usize::try_from(v).map_err(|_| {
                                    IndexOptionError(
                                        "n_gram must be positive".to_string(),
                                        val_span,
                                    )
                                })?;
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
                                n_perm = usize::try_from(v).map_err(|_| {
                                    IndexOptionError(
                                        "n_perm must be positive".to_string(),
                                        val_span,
                                    )
                                })?;
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
                                tokenizer = parse_tokenizer_expr(expr)?;
                            }
                            "filters" => {
                                filters = parse_filters_expr(build_expr(opt_val, param_pool)?)?;
                            }
                            _other => {
                                return Err(IndexOptionError(
                                    format!("Unknown option {} for LSH index", opt_name.as_str()),
                                    name_span,
                                )
                                .into());
                            }
                        }
                    }
                    ensure!(
                        false_positive_weight.is_finite() && false_positive_weight > 0.,
                        IndexOptionError(
                            "false_positive_weight must be finite and positive".to_string(),
                            fpw_span,
                        )
                    );
                    ensure!(
                        false_negative_weight.is_finite() && false_negative_weight > 0.,
                        IndexOptionError(
                            "false_negative_weight must be finite and positive".to_string(),
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
                    // Inf+finite and large-finite overflow both yield a non-finite
                    // sum; dividing by that silently zeroes both normalized weights.
                    ensure!(
                        total_weights.is_finite() && total_weights > 0.,
                        IndexOptionError(
                            "false_positive_weight and false_negative_weight must have a finite positive sum".to_string(),
                            fpw_span,
                        )
                    );
                    false_positive_weight /= total_weights;
                    false_negative_weight /= total_weights;

                    let extractor = combine_extractor(
                        extractor,
                        extract_filter,
                        "MinHash-LSH",
                        name.extract_span(),
                    )?;
                    SysScript::CreateMinHashLshIndex(LshConfigSpec {
                        base_relation: SmartString::from(rel.as_str()),
                        index_name: SmartString::from(name.as_str()),
                        extractor,
                        tokenizer,
                        filters,
                        n_gram,
                        n_perm,
                        false_positive_weight,
                        false_negative_weight,
                        target_threshold,
                    })
                }
                Rule::index_drop => parse_index_drop(inner)?,
                _other => return Err(unexpected("an LSH index operation", &inner)),
            }
        }
        Rule::fts_idx_op => {
            let inner = inner.children().expect("the index operation")?;
            match inner.as_rule() {
                Rule::index_create_adv => {
                    let create_span = inner.extract_span();
                    let mut inner = inner.children();
                    let rel = inner.expect("the relation's name")?;
                    let name = inner.expect("the index's name")?;
                    let mut filters = vec![];
                    let mut tokenizer = TokenizerSpec::simple(create_span);
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
                                tokenizer = parse_tokenizer_expr(expr)?;
                            }
                            "filters" => {
                                filters = parse_filters_expr(build_expr(opt_val, param_pool)?)?;
                            }
                            _other => {
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
                    SysScript::CreateFtsIndex(FtsConfigSpec {
                        base_relation: SmartString::from(rel.as_str()),
                        index_name: SmartString::from(name.as_str()),
                        extractor,
                        tokenizer,
                        filters,
                    })
                }
                Rule::index_drop => parse_index_drop(inner)?,
                _other => return Err(unexpected("an FTS index operation", &inner)),
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
                    // The three required numeric fields are collected as
                    // `Option` (a user may omit them — that refusal is runtime)
                    // and validated below before the spec is built.
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
                                vec_dim = Some(usize::try_from(v).map_err(|_| {
                                    IndexOptionError(format!("Invalid vec_dim: {v}"), val_span)
                                })?);
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
                                        val_span
                                    )
                                );
                                ef_construction = Some(usize::try_from(v).map_err(|_| {
                                    IndexOptionError(
                                        format!("Invalid ef_construction: {v}"),
                                        val_span,
                                    )
                                })?);
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
                                // `m >= 2`: m=1 makes `1/ln(m)` infinite (the
                                // persisted MNeighbours newtype refuses it too).
                                ensure!(
                                    v >= 2,
                                    IndexOptionError(
                                        format!("Invalid m_neighbours: {v} (must be >= 2)"),
                                        val_span,
                                    )
                                );
                                m_neighbours = Some(usize::try_from(v).map_err(|_| {
                                    IndexOptionError(format!("Invalid m_neighbours: {v}"), val_span)
                                })?);
                            }
                            "dtype" => {
                                dtype = match opt_val.as_str() {
                                    "F32" | "Float" => VecElementType::F32,
                                    "F64" | "Double" => VecElementType::F64,
                                    _other => {
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
                                    _other => {
                                        return Err(IndexOptionError(
                                            format!("Invalid distance: {}", opt_val.as_str()),
                                            val_span,
                                        )
                                        .into());
                                    }
                                }
                            }
                            "filter" => {
                                let mut ex = build_expr(opt_val, param_pool)?;
                                ex.partial_eval()?;
                                index_filter = Some(ex);
                            }
                            "extend_candidates" => {
                                extend_candidates = opt_val.as_str().trim() == "true";
                            }
                            "keep_pruned_connections" => {
                                keep_pruned_connections = opt_val.as_str().trim() == "true";
                            }
                            _other => {
                                return Err(IndexOptionError(
                                    format!("Invalid option: {}", opt_name.as_str()),
                                    name_span,
                                )
                                .into());
                            }
                        }
                    }
                    // User omission of a required field is the one runtime
                    // refusal; the engine-typed lift's staged builder proves the
                    // rest at compile time.
                    let (Some(vec_dim), Some(ef_construction), Some(m_neighbours)) =
                        (vec_dim, ef_construction, m_neighbours)
                    else {
                        bail!(IndexOptionError(
                            "an HNSW index requires `dim`, `ef_construction`, and `m_neighbours`"
                                .to_string(),
                            create_span,
                        ));
                    };
                    SysScript::CreateVectorIndex(HnswConfigSpec {
                        base_relation: SmartString::from(rel.as_str()),
                        index_name: SmartString::from(name.as_str()),
                        vec_dim,
                        dtype,
                        vec_fields,
                        distance,
                        ef_construction,
                        m_neighbours,
                        index_filter,
                        extend_candidates,
                        keep_pruned_connections,
                    })
                }
                Rule::index_drop => parse_index_drop(inner)?,
                _other => return Err(unexpected("an HNSW index operation", &inner)),
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
                        .collect::<Vec<_>>();

                    #[derive(Debug, Diagnostic, Error)]
                    #[error("`::index create` needs at least one column")]
                    #[diagnostic(code(parser::empty_index))]
                    #[diagnostic(help(
                        "name the columns to index, e.g. `::index create rel:idx {{col1, col2}}`"
                    ))]
                    struct EmptyIndex(#[label] SourceSpan);

                    ensure!(!cols.is_empty(), EmptyIndex(span));
                    SysScript::CreateIndex(
                        Symbol::new(rel.as_str(), rel.extract_span()),
                        Symbol::new(name.as_str(), name.extract_span()),
                        cols,
                    )
                }
                Rule::index_drop => parse_index_drop(inner)?,
                _other => return Err(unexpected("an index operation", &inner)),
            }
        }
        Rule::list_fixed_rules => SysScript::ListFixedRules,
        _other => return Err(unexpected("a system operation", &inner)),
    })
}

/// The shared `drop rel:idx` shape of every index family.
fn parse_index_drop(inner: super::Pair<'_>) -> Result<SysScript> {
    let [rel, name] = inner
        .children()
        .expect_n(["the relation's name", "the index's name"])?;
    Ok(SysScript::RemoveIndex(
        Symbol::new(rel.as_str(), rel.extract_span()),
        Symbol::new(name.as_str(), name.extract_span()),
    ))
}

/// A `tokenizer: …` option value: a bare name (`Simple`) or a call with
/// constant arguments (`NGram(1, 3, false)`), lifted to a pure
/// [`TokenizerSpec`] (the analyzer admission is the engine-typed lift's job).
fn parse_tokenizer_expr(expr: Expr) -> Result<TokenizerSpec> {
    // Captured before the match consumes `expr`: the offending option value is
    // labelled where the user wrote it.
    let span = expr.span();
    match expr {
        Expr::UnboundApply { op, args, .. } => {
            let mut targs = vec![];
            for arg in args.iter() {
                targs.push(arg.clone().eval_to_const()?);
            }
            Ok(TokenizerSpec {
                name: op,
                args: targs,
                span,
            })
        }
        Expr::Binding { var, .. } => Ok(TokenizerSpec {
            name: var.name,
            args: vec![],
            span,
        }),
        Expr::Const { .. } | Expr::Apply { .. } | Expr::Cond { .. } | Expr::Lazy { .. } => {
            Err(IndexOptionError(
                "Tokenizer must be a symbol or a call for an existing tokenizer".to_string(),
                span,
            )
            .into())
        }
    }
}

/// A `filters: […]` option value: a list of tokenizer expressions.
fn parse_filters_expr(mut expr: Expr) -> Result<Vec<TokenizerSpec>> {
    expr.partial_eval()?;
    // Captured before the match consumes `expr`: a non-list `filters:` value is
    // labelled where the user wrote it.
    let span = expr.span();
    match expr {
        Expr::Apply { op, args, .. } => {
            if op.name != OP_LIST.name {
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
        Expr::Binding { .. }
        | Expr::Const { .. }
        | Expr::UnboundApply { .. }
        | Expr::Cond { .. }
        | Expr::Lazy { .. } => {
            Err(IndexOptionError("Filters must be a list of filters".to_string(), span).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::parse::parse_sys;
    use crate::value::{DataValue, ValidityTs};

    use super::SysScript;

    fn parse_lsh(src: &str, params: BTreeMap<String, DataValue>) -> miette::Result<SysScript> {
        parse_sys(src, &params, ValidityTs::from_raw(0))
    }

    fn lsh_create(fp: &str, fn_: &str) -> String {
        format!(
            "::lsh create docs:sim {{extractor: body, tokenizer: Simple, \
             false_positive_weight: {fp}, false_negative_weight: {fn_}}}"
        )
    }

    /// Inf/NaN pass a bare `> 0` check (Inf) or must be named at the sum;
    /// both must refuse with IndexOptionError — never normalize to 0/0.
    #[test]
    fn lsh_weights_refuse_non_finite() {
        let cases: &[(&str, f64, f64)] = &[
            ("inf fp", f64::INFINITY, 1.0),
            ("inf fn", 1.0, f64::INFINITY),
            ("nan fp", f64::NAN, 1.0),
            ("nan fn", 1.0, f64::NAN),
            ("both inf", f64::INFINITY, f64::INFINITY),
        ];
        for (label, fp, fn_) in cases {
            let mut params = BTreeMap::new();
            params.insert("fp".into(), DataValue::from(*fp));
            params.insert("fn".into(), DataValue::from(*fn_));
            let err = parse_lsh(&lsh_create("$fp", "$fn"), params)
                .expect_err(&format!("{label}: non-finite weight must refuse"));
            let msg = format!("{err:?}");
            assert!(
                msg.contains("finite") || msg.contains("index_option"),
                "{label}: expected IndexOptionError about finite weights, got: {msg}"
            );
        }
    }

    /// Finite positive weights normalize to a unit sum (the Cozo parse contract).
    #[test]
    fn lsh_weights_normalize_when_finite() {
        let mut params = BTreeMap::new();
        params.insert("fp".into(), DataValue::from(1.0));
        params.insert("fn".into(), DataValue::from(3.0));
        let op = parse_lsh(&lsh_create("$fp", "$fn"), params).expect("finite weights parse");
        let SysScript::CreateMinHashLshIndex(cfg) = op else {
            panic!("expected CreateMinHashLshIndex");
        };
        assert!((cfg.false_positive_weight - 0.25).abs() < 1e-12);
        assert!((cfg.false_negative_weight - 0.75).abs() < 1e-12);
        assert!((cfg.false_positive_weight + cfg.false_negative_weight - 1.0).abs() < 1e-12);
    }

    /// Two huge finites whose sum overflows to Inf must refuse at the sum gate.
    #[test]
    fn lsh_weights_refuse_infinite_sum_of_finites() {
        let mut params = BTreeMap::new();
        params.insert("fp".into(), DataValue::from(f64::MAX));
        params.insert("fn".into(), DataValue::from(f64::MAX));
        let err = parse_lsh(&lsh_create("$fp", "$fn"), params)
            .expect_err("finite weights with Inf sum must refuse");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("finite positive sum") || msg.contains("index_option"),
            "expected sum IndexOptionError, got: {msg}"
        );
    }
}
