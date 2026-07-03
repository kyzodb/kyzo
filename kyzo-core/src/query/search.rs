/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The resolved index-search atom: `~rel:idx{ bindings | parameters }` after
//! the catalog has proven it.
//!
//! Below this tier's boundary a search is a *claim* ([`SearchInput`]: names
//! and raw expressions, purely syntactic); above it, *proof*: a
//! [`SearchAtom`] holds the live base and index relation handles, the
//! decoded manifest, the engine's parameter struct, and the exact output
//! frame (`own_bindings`) the RA tier will append to each parent row. One
//! resolution site ([`resolve_search`]), called from the body normalizer,
//! where the catalog is available — a bad relation name, a missing index, a
//! mistyped parameter, or a binding that is not a plain variable is a typed,
//! spanned refusal at that boundary, never a downstream surprise.
//!
//! A search atom evaluates as "a search is a join": for each parent row the
//! query expression is evaluated against that row, the engine's pure search
//! function runs once, and each result row (the full base row plus the
//! engine's appended columns, in the engine's fixed order) extends the
//! parent row. The atom *binds* `own_bindings` and *requires* the variables
//! of its query expression — the well-ordering pass places it exactly like a
//! unification with those dataflow facts.

use std::collections::BTreeMap;
use std::sync::Arc;

use miette::{Diagnostic, Result, bail};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::expr::Expr;
use crate::data::program::{SearchInput, TempSymbGen};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::fts::tokenizer::TextAnalyzer;
use crate::runtime::fts_index::{FtsScoreKind, FtsSearchParams};
use crate::runtime::hnsw::HnswKnnParams;
use crate::runtime::minhash_lsh::{HashPermutations, LshSearchParams};
use crate::runtime::relation::{IndexKind, RelationHandle};

/// A search atom the catalog has proven: live handles, decoded manifest,
/// engine parameters, and the output frame. Carried by
/// `NormalFormAtom::Search` and `MagicAtom::Search`; compiled into
/// `RelAlgebra::Search`.
#[derive(Clone, Debug)]
pub(crate) struct SearchAtom {
    pub(crate) cfg: SearchConfig,
    /// The query expression, evaluated against each PARENT row. Its
    /// variables are this atom's dataflow inputs.
    pub(crate) query: Expr,
    /// The columns this atom appends to each parent row, in order: one
    /// symbol per base-relation column (bound name or a generated ignored
    /// binding), then the engine's appended columns that were asked for.
    pub(crate) own_bindings: Vec<Symbol>,
    /// Residual predicate over the FULL output row (parent ++ own), pushed
    /// into the engine's candidate walk where the engine supports it.
    pub(crate) filter: Option<Expr>,
    /// The session's cooperative kill flag (design Q5): the RA node polls
    /// it once per search invocation, and the engines poll it per scanned
    /// node inside a single search — both refuse with the typed
    /// cancellation error, never a silent short read.
    pub(crate) cancel: crate::fixed_rule::CancelFlag,
    pub(crate) span: SourceSpan,
}

/// The engine-specific half of a resolved search atom.
#[derive(Clone)]
pub(crate) enum SearchConfig {
    Hnsw(HnswSearch),
    Fts(FtsSearch),
    Lsh(LshSearch),
}

impl SearchConfig {
    pub(crate) fn base(&self) -> &RelationHandle {
        match self {
            SearchConfig::Hnsw(c) => &c.base,
            SearchConfig::Fts(c) => &c.base,
            SearchConfig::Lsh(c) => &c.base,
        }
    }
}

impl std::fmt::Debug for SearchConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (kind, base, idx) = match self {
            SearchConfig::Hnsw(c) => ("hnsw", &c.base.name, &c.idx.name),
            SearchConfig::Fts(c) => ("fts", &c.base.name, &c.idx.name),
            SearchConfig::Lsh(c) => ("lsh", &c.base.name, &c.idx.name),
        };
        write!(f, "SearchConfig::{kind}({base}:{idx})")
    }
}

/// A resolved HNSW k-nearest-neighbours search.
#[derive(Clone)]
pub(crate) struct HnswSearch {
    pub(crate) base: RelationHandle,
    pub(crate) idx: RelationHandle,
    pub(crate) manifest: crate::runtime::hnsw::HnswIndexManifest,
    pub(crate) params: HnswKnnParams,
}

/// A resolved full-text search. The analyzer is built once, at resolution —
/// a manifest that no longer builds is a refusal here, not mid-scan.
#[derive(Clone)]
pub(crate) struct FtsSearch {
    pub(crate) base: RelationHandle,
    pub(crate) idx: RelationHandle,
    pub(crate) params: FtsSearchParams,
    pub(crate) analyzer: Arc<TextAnalyzer>,
}

/// A resolved MinHash-LSH candidate search (a candidate SET, not a
/// similarity ranking — see the engine's module docs).
#[derive(Clone)]
pub(crate) struct LshSearch {
    pub(crate) base: RelationHandle,
    pub(crate) idx: RelationHandle,
    pub(crate) manifest: crate::runtime::minhash_lsh::MinHashLshIndexManifest,
    pub(crate) params: LshSearchParams,
    pub(crate) analyzer: Arc<TextAnalyzer>,
    pub(crate) perms: Arc<HashPermutations>,
}

// ─────────────────────────────────────────────────────────────────────────
// Typed refusals
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Error, Diagnostic)]
#[error("relation '{0}' has no index named '{1}'")]
#[diagnostic(code(query::search_index_not_found))]
pub(crate) struct SearchIndexNotFound(
    pub(crate) String,
    pub(crate) String,
    #[label] pub(crate) SourceSpan,
);

#[derive(Debug, Error, Diagnostic)]
#[error("index '{0}' is a plain projection index; the search atom serves HNSW/FTS/LSH")]
#[diagnostic(code(query::search_over_plain_index))]
#[diagnostic(help("plain indices are chosen automatically by the planner; query the relation"))]
pub(crate) struct SearchOverPlainIndex(pub(crate) String, #[label] pub(crate) SourceSpan);

#[derive(Debug, Error, Diagnostic)]
#[error("search binding for '{0}' must be a plain variable")]
#[diagnostic(code(query::search_binding_not_variable))]
pub(crate) struct SearchBindingNotVariable(pub(crate) String, #[label] pub(crate) SourceSpan);

#[derive(Debug, Error, Diagnostic)]
#[error("'{0}' is not a column of relation '{1}'")]
#[diagnostic(code(query::search_column_not_found))]
pub(crate) struct SearchColumnNotFound(
    pub(crate) String,
    pub(crate) String,
    #[label] pub(crate) SourceSpan,
);

#[derive(Debug, Error, Diagnostic)]
#[error("search parameter '{0}' is required")]
#[diagnostic(code(query::search_param_required))]
pub(crate) struct SearchParamRequired(pub(crate) &'static str, #[label] pub(crate) SourceSpan);

#[derive(Debug, Error, Diagnostic)]
#[error("search parameter '{0}' {1}")]
#[diagnostic(code(query::search_param_invalid))]
pub(crate) struct SearchParamInvalid(
    pub(crate) String,
    pub(crate) &'static str,
    #[label] pub(crate) SourceSpan,
);

#[derive(Debug, Error, Diagnostic)]
#[error("a search atom cannot be negated")]
#[diagnostic(code(query::negated_search_unsupported))]
#[diagnostic(help(
    "bind the search results and negate on the bound rows instead: a search \
     is a ranked/candidate join, and 'not near' has no single sound meaning"
))]
pub(crate) struct NegatedSearchUnsupported(#[label] pub(crate) SourceSpan);

// ─────────────────────────────────────────────────────────────────────────
// Resolution
// ─────────────────────────────────────────────────────────────────────────

/// Extract a plain variable from a binding/parameter expression, refusing
/// anything computed.
fn expr_as_var(what: &str, e: &Expr, span: SourceSpan) -> Result<Symbol> {
    match e {
        Expr::Binding { var, .. } => Ok(var.clone()),
        _ => bail!(SearchBindingNotVariable(what.to_string(), span)),
    }
}

fn take_const(
    params: &mut BTreeMap<SmartString<LazyCompact>, Expr>,
    name: &'static str,
    span: SourceSpan,
) -> Result<Option<DataValue>> {
    match params.remove(name) {
        None => Ok(None),
        Some(e) => {
            let v = e
                .eval_to_const()
                .map_err(|_| SearchParamInvalid(name.to_string(), "must be a constant", span))?;
            Ok(Some(v))
        }
    }
}

fn take_pos_int(
    params: &mut BTreeMap<SmartString<LazyCompact>, Expr>,
    name: &'static str,
    span: SourceSpan,
) -> Result<Option<usize>> {
    match take_const(params, name, span)? {
        None => Ok(None),
        Some(DataValue::Num(n)) => {
            let i = n.get_int().ok_or(SearchParamInvalid(
                name.to_string(),
                "must be a positive integer",
                span,
            ))?;
            if i <= 0 {
                bail!(SearchParamInvalid(
                    name.to_string(),
                    "must be a positive integer",
                    span
                ));
            }
            Ok(Some(i as usize))
        }
        Some(_) => bail!(SearchParamInvalid(
            name.to_string(),
            "must be a positive integer",
            span
        )),
    }
}

fn take_var(
    params: &mut BTreeMap<SmartString<LazyCompact>, Expr>,
    name: &'static str,
    span: SourceSpan,
) -> Result<Option<Symbol>> {
    match params.remove(name) {
        None => Ok(None),
        Some(e) => Ok(Some(expr_as_var(name, &e, span)?)),
    }
}

/// The base relation's output frame: one symbol per column (keys then
/// non-keys), taking the user's variable where the column is bound and a
/// generated ignored binding where it is not. Consumes `bindings`; a
/// leftover key names a column that does not exist — refused.
fn base_frame(
    base: &RelationHandle,
    mut bindings: BTreeMap<SmartString<LazyCompact>, Expr>,
    symb_gen: &mut TempSymbGen,
    span: SourceSpan,
) -> Result<Vec<Symbol>> {
    let mut frame = Vec::new();
    for col in base
        .metadata
        .keys
        .iter()
        .chain(base.metadata.non_keys.iter())
    {
        match bindings.remove(&col.name) {
            Some(e) => frame.push(expr_as_var(&col.name, &e, span)?),
            None => frame.push(symb_gen.next_ignored(span)),
        }
    }
    if let Some((name, _)) = bindings.into_iter().next() {
        bail!(SearchColumnNotFound(
            name.to_string(),
            base.name.to_string(),
            span
        ));
    }
    Ok(frame)
}

/// Resolve a parsed search atom against the catalog. `handle` looks a
/// relation up by name (the body normalizer's session view).
pub(crate) fn resolve_search(
    handle: &dyn Fn(&str) -> Result<RelationHandle>,
    inp: SearchInput,
    symb_gen: &mut TempSymbGen,
    cancel: crate::fixed_rule::CancelFlag,
) -> Result<SearchAtom> {
    let span = inp.span;
    let base = handle(&inp.relation.name)?;
    let idx_ref = base
        .indices
        .iter()
        .find(|r| r.name == inp.index.name)
        .ok_or_else(|| {
            SearchIndexNotFound(base.name.to_string(), inp.index.name.to_string(), span)
        })?
        .clone();
    let idx = handle(&idx_ref.relation_name(&base.name))?;

    let mut params = inp.parameters;
    let query = params
        .remove("query")
        .ok_or(SearchParamRequired("query", span))?;
    let filter = params.remove("filter");
    let mut own_bindings = base_frame(&base, inp.bindings, symb_gen, span)?;

    let cfg = match idx_ref.kind {
        IndexKind::Plain { .. } => {
            bail!(SearchOverPlainIndex(inp.index.name.to_string(), span))
        }
        IndexKind::Hnsw(manifest) => {
            let k = take_pos_int(&mut params, "k", span)?.ok_or(SearchParamRequired("k", span))?;
            let ef = take_pos_int(&mut params, "ef", span)?.unwrap_or(k).max(k);
            let radius = match take_const(&mut params, "radius", span)? {
                None => None,
                Some(v) => Some(v.get_float().ok_or(SearchParamInvalid(
                    "radius".to_string(),
                    "must be a number",
                    span,
                ))?),
            };
            let bind_field = take_var(&mut params, "bind_field", span)?;
            let bind_field_idx = take_var(&mut params, "bind_field_idx", span)?;
            let bind_distance = take_var(&mut params, "bind_distance", span)?;
            let bind_vector = take_var(&mut params, "bind_vector", span)?;
            let p = HnswKnnParams {
                k,
                ef,
                radius,
                bind_field: bind_field.is_some(),
                bind_field_idx: bind_field_idx.is_some(),
                bind_distance: bind_distance.is_some(),
                bind_vector: bind_vector.is_some(),
            };
            // The engine appends these IN THIS ORDER (hnsw_knn's contract).
            own_bindings.extend(
                [bind_field, bind_field_idx, bind_distance, bind_vector]
                    .into_iter()
                    .flatten(),
            );
            SearchConfig::Hnsw(HnswSearch {
                base,
                idx,
                manifest,
                params: p,
            })
        }
        IndexKind::Fts(manifest) => {
            let k = take_pos_int(&mut params, "k", span)?.ok_or(SearchParamRequired("k", span))?;
            let score_kind = match take_const(&mut params, "score_kind", span)? {
                None => FtsScoreKind::TfIdf,
                Some(DataValue::Str(s)) if s == "tf_idf" => FtsScoreKind::TfIdf,
                Some(DataValue::Str(s)) if s == "tf" => FtsScoreKind::Tf,
                Some(_) => bail!(SearchParamInvalid(
                    "score_kind".to_string(),
                    "must be 'tf' or 'tf_idf'",
                    span
                )),
            };
            let bind_score = take_var(&mut params, "bind_score", span)?;
            let analyzer = manifest.tokenizer.build(&manifest.filters).map(Arc::new)?;
            let p = FtsSearchParams {
                k,
                score_kind,
                bind_score: bind_score.is_some(),
            };
            own_bindings.extend(bind_score);
            SearchConfig::Fts(FtsSearch {
                base,
                idx,
                params: p,
                analyzer,
            })
        }
        IndexKind::Lsh { manifest, .. } => {
            let k = take_pos_int(&mut params, "k", span)?;
            let analyzer = manifest.tokenizer.build(&manifest.filters).map(Arc::new)?;
            let perms = Arc::new(manifest.get_hash_perms()?);
            own_bindings // LSH appends nothing beyond the base row.
                .shrink_to_fit();
            SearchConfig::Lsh(LshSearch {
                base,
                idx,
                manifest,
                params: LshSearchParams { k },
                analyzer,
                perms,
            })
        }
    };

    if let Some((name, _)) = params.into_iter().next() {
        bail!(SearchParamInvalid(
            name.to_string(),
            "is not a parameter of this index kind",
            span
        ));
    }

    Ok(SearchAtom {
        cfg,
        query,
        own_bindings,
        filter,
        cancel,
        span,
    })
}
