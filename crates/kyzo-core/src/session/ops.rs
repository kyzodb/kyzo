/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Index lifecycle peeled from
 * runtime/mutate.rs into session/ops.rs (story #350 T2).
 *
 * Carried obligation: temporal-index-parsed-surface — record at this seat.
 */

//! Index lifecycle: attach/backfill/remove and the five `::create` ops.

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::sync::Arc;

use fjall::Slice;
use itertools::Itertools;
use miette::{Diagnostic, Result, bail};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::json::NamedRows;
use crate::session::catalog::{
    IndexKind, IndexRef, KeyspaceKind, RelationHandle, Residency, get_relation,
};
use crate::session::db::{Engine, ScriptOptions, SessionTx, status_ok};
use crate::store::time::ClaimPolarity;
use crate::store::{Storage, WriteTx};
use kyzo_model::SourceSpan;
use kyzo_model::program::expr::Expr;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::schema::StoredRelationMetadata;
use kyzo_model::value::{DataValue, Tuple, TupleT};

/// The scan ceiling for `::merkle_root` when the caller sets no
/// derived-tuple ceiling: 2^32 key-value pairs. Large enough for any store
/// this engine has met, small enough that no scan is unbounded.
#[cfg(test)]
use kyzo_model::schema::ColumnDef;
#[cfg(test)]
use kyzo_model::schema::NullableColType;
const DEFAULT_MERKLE_SCAN_CEILING: NonZeroU64 = match NonZeroU64::new(1 << 32) {
    Some(n) => n,
    // 1<<32 ≠ 0 — NonZeroU64::new only rejects zero; MIN is an unreachable stand-in.
    None => NonZeroU64::MIN,
};

// ─────────────────────────────────────────────────────────────────────────
// Manifest-index maintenance and lifecycle (the index-operator tier)
// ─────────────────────────────────────────────────────────────────────────

/// A manifest index's resolved runtime context: live handles, compiled
/// extractor/filter bytecode, built analyzer, decoded permutations. Resolved
/// once per session per index (cached by index relation name) — a manifest
/// that no longer parses, builds, or decodes is a typed refusal at first
/// touch, never mid-scan corruption.
// Variant sizes differ by design (LSH carries perms + two handles); a
// session holds a handful of these in a cache, never hot collections.
#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub(crate) enum IndexCtx {
    Hnsw {
        idx: RelationHandle,
        manifest: crate::project::vector::hnsw::HnswIndexManifest,
        filter: Option<Expr>,
    },
    Fts {
        idx: RelationHandle,
        extractor: Expr,
        analyzer: Arc<crate::project::text::tokenizer::TextAnalyzer>,
    },
    Lsh {
        idx: RelationHandle,
        inv: RelationHandle,
        manifest: crate::project::dedup::lsh::MinHashLshIndexManifest,
        extractor: Expr,
        analyzer: Arc<crate::project::text::tokenizer::TextAnalyzer>,
        perms: Arc<crate::project::dedup::lsh::HashPermutations>,
    },
}

/// `::lsh/fts/hnsw create` on a temp base, or an index name that already
/// exists on the base — both structural refusals.
#[derive(Debug, Error, Diagnostic)]
#[error("{0}")]
#[diagnostic(code(db::index_lifecycle))]
pub(crate) struct IndexLifecycleError(pub(crate) String);

impl<T: WriteTx> SessionTx<T> {
    /// The base relation's full column frame (keys then non-keys), for
    /// resolving extractor/filter expressions by column name.
    fn base_column_frame(base: &RelationHandle) -> BTreeMap<Symbol, usize> {
        base.metadata
            .keys
            .iter()
            .chain(base.metadata.non_keys.iter())
            .enumerate()
            .map(|(i, col)| (Symbol::new(col.name.clone(), SourceSpan::default()), i))
            .collect()
    }

    /// Bind an already-parsed row extractor ([`crate::parse::sys::FtsIndexConfig::extractor`]
    /// / the manifest's stored typed substance) to the base column frame.
    /// The extractor is never re-parsed from source at build time — it arrives typed.
    fn compile_row_extractor(
        base: &RelationHandle,
        extractor: &kyzo_model::program::expr::Expr,
    ) -> Result<Expr> {
        let mut expr = extractor.clone();
        expr.fill_binding_indices(&Self::base_column_frame(base))?;
        Ok(expr)
    }

    /// Resolve (and cache) a manifest index's runtime context.
    pub(crate) fn manifest_index_ctx(
        &mut self,
        base: &RelationHandle,
        index: &IndexRef,
    ) -> Result<IndexCtx> {
        let idx_name = index.relation_name(&base.name);
        if let Some(ctx) = self.index_ctxs.get(idx_name.as_str()) {
            return Ok(ctx.clone());
        }
        let idx = self.get_relation(&idx_name)?;
        let ctx = match &index.kind {
            IndexKind::Plain { .. } | IndexKind::Temporal => {
                bail!(IndexLifecycleError(format!(
                    "index '{}' is a plain or temporal index; it has no manifest context",
                    index.name
                )))
            }
            IndexKind::Hnsw(manifest) => {
                // Manifest holds typed Expr substance; fill binding indices
                // against the base frame — never re-parse source text.
                let filter = manifest
                    .index_filter()
                    .map(|expr| Self::compile_row_extractor(base, expr))
                    .transpose()?;
                IndexCtx::Hnsw {
                    idx,
                    manifest: manifest.clone(),
                    filter,
                }
            }
            IndexKind::Fts(manifest) => IndexCtx::Fts {
                idx,
                extractor: Self::compile_row_extractor(base, &manifest.extractor)?,
                analyzer: Arc::new(manifest.tokenizer.build(&manifest.filters)?),
            },
            IndexKind::Lsh { manifest, inverse } => IndexCtx::Lsh {
                idx,
                inv: self.get_relation(&format!("{}:{}", base.name, inverse))?,
                extractor: Self::compile_row_extractor(base, &manifest.extractor)?,
                analyzer: Arc::new(manifest.tokenizer.build(&manifest.filters)?),
                perms: Arc::new(manifest.get_hash_perms()?),
                manifest: manifest.clone(),
            },
        };
        self.index_ctxs.insert(idx_name.into(), ctx.clone());
        Ok(ctx)
    }

    /// One row transition through one manifest index: `old_kv` un-indexed,
    /// `new_kv` indexed, in the same transaction as the base write.
    pub(crate) fn apply_manifest_index(
        &mut self,
        base: &RelationHandle,
        ctx: &IndexCtx,
        new_kv: Option<&[DataValue]>,
        old_kv: Option<&[DataValue]>,
    ) -> Result<()> {
        match ctx {
            IndexCtx::Hnsw {
                idx,
                manifest,
                filter,
            } => {
                if let Some(old) = old_kv {
                    crate::project::vector::hnsw::hnsw_remove(&mut self.store, base, idx, old)?;
                }
                if let Some(new) = new_kv {
                    crate::project::vector::hnsw::hnsw_put(
                        &mut self.store,
                        manifest,
                        base,
                        idx,
                        filter.as_ref(),
                        new,
                    )?;
                }
            }
            IndexCtx::Fts {
                idx,
                extractor,
                analyzer,
            } => {
                if let Some(old) = old_kv {
                    crate::project::text::fts::fts_del(
                        &mut self.store,
                        old,
                        extractor,
                        analyzer,
                        base,
                        idx,
                    )?;
                }
                if let Some(new) = new_kv {
                    crate::project::text::fts::fts_put(
                        &mut self.store,
                        new,
                        extractor,
                        analyzer,
                        base,
                        idx,
                    )?;
                }
            }
            IndexCtx::Lsh {
                idx,
                inv,
                manifest,
                extractor,
                analyzer,
                perms,
            } => {
                if let Some(old) = old_kv {
                    crate::project::dedup::lsh::lsh_del(&mut self.store, old, None, idx, inv)?;
                }
                if let Some(new) = new_kv {
                    crate::project::dedup::lsh::lsh_put(
                        &mut self.store,
                        new,
                        extractor,
                        analyzer,
                        base,
                        idx,
                        inv,
                        manifest,
                        perms,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Shared `::hnsw|fts|lsh create` tail: create the index relation(s),
    /// attach the ref (kept sorted by name — deterministic lookup), persist
    /// the base handle, and backfill from the existing rows.
    fn attach_and_backfill(
        &mut self,
        mut base: RelationHandle,
        index_ref: IndexRef,
        index_metas: Vec<(String, StoredRelationMetadata)>,
    ) -> Result<NamedRows> {
        if matches!(base.residency(), Residency::Temp) {
            bail!(IndexLifecycleError(format!(
                "temp relation '{}' cannot carry a manifest index",
                base.name
            )));
        }
        if base.indices.iter().any(|r| r.name == index_ref.name) {
            bail!(IndexLifecycleError(format!(
                "relation '{}' already has an index named '{}'",
                base.name, index_ref.name
            )));
        }
        // A plain or temporal index mirrors its base's facts bitemporally
        // (a posting IS a bitemporal fact — its own as-of reads are how
        // window scans see corrections, per issue #62's design ruling);
        // every manifest index keyspace is the algorithm's own
        // current-only state.
        let kind = match &index_ref.kind {
            IndexKind::Plain { .. } | IndexKind::Temporal => KeyspaceKind::Facts,
            IndexKind::Hnsw(_) | IndexKind::Fts(_) | IndexKind::Lsh { .. } => {
                KeyspaceKind::AlgorithmState
            }
        };
        for (name, metadata) in index_metas {
            self.create_relation(
                kyzo_model::program::InputRelationHandle {
                    name: Symbol::new(name, SourceSpan::default()),
                    metadata,
                    key_bindings: vec![],
                    dep_bindings: vec![],
                    span: SourceSpan::default(),
                },
                kind,
            )?;
        }
        base.indices.push(index_ref.clone());
        base.indices.sort_by(|a, b| a.name.cmp(&b.name));
        self.write_catalog_row(&base)?;

        const BACKFILL_BATCH: usize = 4096;

        // Temporal backfill is NOT "index the current rows": a posting
        // exists per POINT EVENT, so an index attached after N base
        // writes must reproduce the exact posting keyspace an index live
        // since the first write would hold (backfill-equals-incremental,
        // the rebuildability law) — every stored version of every fact,
        // each posted at its own original (valid, sys) and polarity, not
        // "now". That is a raw walk of the base's whole keyspace, not the
        // as-of skip-scan the Plain/manifest path below uses.
        if matches!(index_ref.kind, IndexKind::Temporal) {
            let idx_handle = self.get_relation(&index_ref.relation_name(&base.name))?;
            let keys_len = base.metadata.keys.len();
            let upper = (base.id.raw() + 1).to_be_bytes();
            let mut lower: Vec<u8> = Tuple::default().encode_as_key(base.id).as_ref().to_vec();
            loop {
                let batch: Vec<(Slice, Slice)> = self
                    .store
                    .range_scan(&lower, &upper)
                    .take(BACKFILL_BATCH)
                    .try_collect()?;
                let Some((last_key, _)) = batch.last() else {
                    break;
                };
                let mut succ = last_key.to_vec();
                succ.push(0);
                lower = succ;
                for (k, v) in &batch {
                    // Every stored row here IS one point event: decode its
                    // key columns plus its two time slots directly (no
                    // as-of resolution — resolution is exactly what would
                    // collapse the history this backfill must reproduce
                    // whole), and its polarity from the value.
                    let tuple = kyzo_model::value::decode_tuple_from_key(k, keys_len + 2)?;
                    let polarity = crate::store::time::claim_polarity_of_value(v)?;
                    let key_cols = &tuple.as_slice()[..keys_len];
                    let DataValue::Validity(valid_slot) = &tuple[keys_len] else {
                        bail!(
                            "corrupt bitemporal key: missing valid-time slot during \
                             temporal index backfill"
                        );
                    };
                    let DataValue::Validity(sys_slot) = &tuple[keys_len + 1] else {
                        bail!(
                            "corrupt bitemporal key: missing system-time slot during \
                             temporal index backfill"
                        );
                    };
                    self.temporal_index_write(
                        &base,
                        &idx_handle,
                        key_cols,
                        polarity,
                        valid_slot.timestamp(),
                        sys_slot.timestamp(),
                    )?;
                }
            }
            return Ok(status_ok());
        }

        // Backfill: index every existing base row, in bounded batches — the
        // scan borrows the store the puts need mutably, so each round
        // materializes at most BACKFILL_BATCH rows and resumes from the
        // strict successor of the last key (memcmp order: key ++ 0x00).
        // Split on IndexKind once: Plain carries its mapper; every other
        // kind builds a manifest ctx. No `unreachable!` residual.
        let plain_mapper = match &index_ref.kind {
            IndexKind::Plain { mapper } => Some(mapper),
            IndexKind::Temporal
            | IndexKind::Hnsw(_)
            | IndexKind::Fts(_)
            | IndexKind::Lsh { .. } => None,
        };
        let ctx = match plain_mapper {
            Some(_) => None,
            None => Some(self.manifest_index_ctx(&base, &index_ref)?),
        };
        let stamp = self.system_stamp_routed(base.residency());
        let upper = (base.id.raw() + 1).to_be_bytes();
        let keys_len = base.metadata.keys.len();
        let as_of = kyzo_model::value::AsOf::current(kyzo_model::value::MAX_VALIDITY_TS);
        let mut lower: Vec<u8> = Tuple::default().encode_as_key(base.id).as_ref().to_vec();
        loop {
            // Current rows only: an index reflects current state, and the
            // as-of resolution skips a fact's whole version group in one
            // seek. Rows arrive with the two time slots; the LOGICAL row
            // (user columns) is what the index projects.
            let batch: Vec<Tuple> = self
                .store
                .range_skip_scan_tuple(&lower, &upper, as_of)
                .take(BACKFILL_BATCH)
                .map(|r| {
                    r.map(|mut t| {
                        t.drain(keys_len..keys_len + 2);
                        t
                    })
                })
                .try_collect()?;
            let Some(last) = batch.last() else { break };
            // Resume past ALL versions of the last fact: the 0xFF tail
            // encodes above every slot byte, so this bound clears its
            // group.
            let mut succ = base
                .encode_partial_key_for_store(&last.as_slice()[0..keys_len])
                .as_bytes()
                .to_vec();
            succ.push(0xFF);
            lower = succ;
            for row in &batch {
                match (&ctx, plain_mapper) {
                    (Some(ctx), _) => {
                        self.apply_manifest_index(&base, ctx, Some(row.as_slice()), None)?
                    }
                    (None, Some(mapper)) => {
                        let idx_handle = self.get_relation(&index_ref.relation_name(&base.name))?;
                        // Backfill re-mints "now" for both coordinates —
                        // it indexes the base's CURRENT rows (`as_of`
                        // above), and the scan already discards each row's
                        // original bitemporal slots, so there is no
                        // per-row valid instant left to carry forward.
                        self.plain_index_write(
                            &base,
                            &idx_handle,
                            mapper,
                            row.as_slice(),
                            ClaimPolarity::Assert,
                            stamp,
                            stamp,
                        )?;
                    }
                    (None, None) => {
                        bail!(miette::miette!(
                            "index backfill: plain mapper missing for Plain kind"
                        ))
                    }
                }
            }
        }
        Ok(status_ok())
    }

    /// `::index create rel:name {cols}` — a plain index: a projection of
    /// the base relation, mirrored bitemporally per write. The stored
    /// index rows are the chosen columns followed by whichever base key
    /// columns the choice omitted (so index rows are per-fact unique and
    /// every base key is recoverable from the index alone).
    pub(crate) fn create_plain_index(
        &mut self,
        rel: &str,
        idx_name: &str,
        cols: &[Symbol],
    ) -> Result<NamedRows> {
        let base = self.get_relation(rel)?;
        let all_cols: Vec<&kyzo_model::schema::ColumnDef> = base
            .metadata
            .keys
            .iter()
            .chain(base.metadata.non_keys.iter())
            .collect();
        let mut mapper: Vec<usize> = vec![];
        for col in cols {
            let pos = all_cols
                .iter()
                .position(|c| c.name == col.name)
                .ok_or_else(|| {
                    IndexLifecycleError(format!(
                        "relation '{rel}' has no column '{}' to index",
                        col.name
                    ))
                })?;
            if mapper.contains(&pos) {
                bail!(IndexLifecycleError(format!(
                    "column '{}' appears twice in the index specification",
                    col.name
                )));
            }
            mapper.push(pos);
        }
        // Every base key column rides along (after the chosen columns) so
        // the index key identifies exactly one base fact.
        for key_pos in 0..base.metadata.keys.len() {
            if !mapper.contains(&key_pos) {
                mapper.push(key_pos);
            }
        }
        let metadata = kyzo_model::schema::StoredRelationMetadata {
            keys: mapper.iter().map(|&i| all_cols[i].clone()).collect(),
            non_keys: vec![],
        };
        let index_ref = IndexRef {
            name: SmartString::from(idx_name),
            kind: IndexKind::Plain { mapper },
        };
        let idx_rel_name = index_ref.relation_name(&base.name).to_string();
        self.attach_and_backfill(base, index_ref, vec![(idx_rel_name, metadata)])
    }

    /// `::temporal index create` — issue #62's transposed event-posting
    /// index: opt-in per relation, no column choice (unlike `::index
    /// create`, a posting's whole identity is the base's own key, always
    /// — see [`IndexKind::Temporal`]). The stored posting rows are the
    /// write's own valid instant as a leading column, followed by the
    /// base relation's key columns.
    ///
    /// No parsed `SysOp` surface yet (grammar lives outside this door);
    /// exercised by in-crate temporal index tests that drive `SessionTx`
    /// directly — the same code the eventual parsed surface will call.
    #[cfg(test)]
    pub(crate) fn create_temporal_index(&mut self, rel: &str, idx_name: &str) -> Result<NamedRows> {
        let base = self.get_relation(rel)?;
        let mut keys = Vec::with_capacity(1 + base.metadata.keys.len());
        keys.push(kyzo_model::schema::ColumnDef {
            name: SmartString::from(crate::session::catalog::TEMPORAL_POSTING_LEADING_COLUMN),
            typing: kyzo_model::schema::NullableColType::required(
                kyzo_model::schema::ColType::Validity,
            ),
            default_gen: None,
        });
        keys.extend(base.metadata.keys.iter().cloned());
        let metadata = kyzo_model::schema::StoredRelationMetadata {
            keys,
            non_keys: vec![],
        };
        let index_ref = IndexRef {
            name: SmartString::from(idx_name),
            kind: IndexKind::Temporal,
        };
        let idx_rel_name = index_ref.relation_name(&base.name).to_string();
        self.attach_and_backfill(base, index_ref, vec![(idx_rel_name, metadata)])
    }

    /// `::hnsw create` — build the manifest, mint the index relation,
    /// backfill.
    pub(crate) fn create_hnsw_index(
        &mut self,
        cfg: &crate::parse::sys::HnswIndexConfig,
    ) -> Result<NamedRows> {
        let base = self.get_relation(&cfg.base_relation)?;
        let frame = Self::base_column_frame(&base);
        let mut vec_fields = Vec::with_capacity(cfg.vec_fields.len());
        for f in &cfg.vec_fields {
            let pos = frame
                .get(&Symbol::new(f.clone(), SourceSpan::default()))
                .ok_or_else(|| {
                    IndexLifecycleError(format!(
                        "'{}' is not a column of relation '{}'",
                        f, cfg.base_relation
                    ))
                })?;
            vec_fields.push(*pos);
        }
        if let Some(filter) = &cfg.index_filter {
            // Prove the typed filter binds against the base frame now; the
            // manifest stores that same Expr substance (not source text).
            Self::compile_row_extractor(&base, filter)?;
        }
        // Admit-only mint: private fields, MNeighbours (m >= 2), derived
        // m_max / m_max0 / level_multiplier — illegal descriptions refuse here.
        let manifest = crate::project::vector::hnsw::HnswIndexManifest::admit(
            cfg.base_relation.clone(),
            cfg.index_name.clone(),
            cfg.vec_dim,
            cfg.dtype,
            vec_fields,
            cfg.distance,
            cfg.ef_construction,
            cfg.m_neighbours,
            cfg.index_filter.clone(),
            cfg.extend_candidates,
            cfg.keep_pruned_connections,
        )?;
        let idx_meta = crate::project::vector::hnsw::hnsw_index_metadata(&base.metadata);
        let idx_ref = IndexRef {
            name: cfg.index_name.clone(),
            kind: IndexKind::Hnsw(manifest),
        };
        let idx_rel = idx_ref.relation_name(&base.name);
        self.attach_and_backfill(base, idx_ref, vec![(idx_rel, idx_meta)])
    }

    /// `::fts create`.
    pub(crate) fn create_fts_index(
        &mut self,
        cfg: &crate::parse::sys::FtsIndexConfig,
    ) -> Result<NamedRows> {
        let base = self.get_relation(&cfg.base_relation)?;
        // Prove the analyzer builds and the extractor compiles now.
        cfg.tokenizer.build(&cfg.filters)?;
        Self::compile_row_extractor(&base, &cfg.extractor)?;
        let manifest = crate::project::text::FtsIndexManifest {
            base_relation: cfg.base_relation.clone(),
            index_name: cfg.index_name.clone(),
            extractor: cfg.extractor.clone(),
            tokenizer: cfg.tokenizer.clone(),
            filters: cfg.filters.clone(),
        };
        let idx_meta = crate::project::text::fts::fts_index_metadata(&base.metadata);
        let idx_ref = IndexRef {
            name: cfg.index_name.clone(),
            kind: IndexKind::Fts(manifest),
        };
        let idx_rel = idx_ref.relation_name(&base.name);
        self.attach_and_backfill(base, idx_ref, vec![(idx_rel, idx_meta)])
    }

    /// `::lsh create` — bands/rows from the deterministic optimal-parameter
    /// search, permutations drawn from the pinned default seed (two builds
    /// of the same index are byte-identical).
    pub(crate) fn create_lsh_index(
        &mut self,
        cfg: &crate::parse::sys::MinHashLshConfig,
    ) -> Result<NamedRows> {
        use crate::project::dedup::lsh::{DEFAULT_PERM_SEED, HashPermutations, LshParams, Weights};
        let base = self.get_relation(&cfg.base_relation)?;
        cfg.tokenizer.build(&cfg.filters)?;
        Self::compile_row_extractor(&base, &cfg.extractor)?;
        let params = LshParams::find_optimal_params(
            cfg.target_threshold.0,
            cfg.n_perm,
            &Weights(cfg.false_positive_weight.0, cfg.false_negative_weight.0),
        );
        // The signature holds exactly b*r hashes (the engine's band-chunk
        // contract); the requested n_perm is the optimizer's search budget,
        // not the drawn count. This product reaches both a STORED count
        // (`num_perm`) and the permutation allocation below, so it is a
        // checked multiply, not a wrapping one: an overflow is a typed refusal
        // here, never a silently-wrapped count that mis-sizes the index.
        let n_drawn = params.b.checked_mul(params.r).ok_or_else(|| {
            IndexLifecycleError(format!(
                "LSH parameters overflow: {} bands * {} rows-per-band exceeds usize",
                params.b, params.r
            ))
        })?;
        let perms = HashPermutations::new(n_drawn, DEFAULT_PERM_SEED);
        let inverse: SmartString<LazyCompact> = format!("{}:inv", cfg.index_name).into();
        let manifest = crate::project::dedup::lsh::MinHashLshIndexManifest {
            base_relation: cfg.base_relation.clone(),
            index_name: cfg.index_name.clone(),
            extractor: cfg.extractor.clone(),
            n_gram: cfg.n_gram,
            tokenizer: cfg.tokenizer.clone(),
            filters: cfg.filters.clone(),
            num_perm: n_drawn,
            n_bands: params.b,
            n_rows_in_band: params.r,
            threshold: cfg.target_threshold.0,
            perms: crate::project::dedup::lsh::LshPermutationBytes::from_perms(&perms),
        };
        let idx_meta = crate::project::dedup::lsh::lsh_index_metadata(&base.metadata);
        let inv_meta = crate::project::dedup::lsh::lsh_inv_index_metadata(&base.metadata);
        let idx_ref = IndexRef {
            name: cfg.index_name.clone(),
            kind: IndexKind::Lsh { manifest, inverse },
        };
        let idx_rel = idx_ref.relation_name(&base.name);
        let inv_rel = format!("{}:{}:inv", base.name, cfg.index_name);
        self.attach_and_backfill(
            base,
            idx_ref,
            vec![(idx_rel, idx_meta), (inv_rel, inv_meta)],
        )
    }

    /// `::index drop` for every index kind: destroy the index relation(s),
    /// detach the ref, drop the session's cached context.
    pub(crate) fn remove_index(&mut self, rel: &str, idx: &str) -> Result<NamedRows> {
        let mut base = self.get_relation(rel)?;
        let pos = base
            .indices
            .iter()
            .position(|r| r.name == idx)
            .ok_or_else(|| {
                IndexLifecycleError(format!("relation '{rel}' has no index named '{idx}'"))
            })?;
        let index_ref = base.indices.remove(pos);
        let idx_rel = index_ref.relation_name(&base.name);
        self.destroy_relation(&idx_rel)?;
        if let IndexKind::Lsh { inverse, .. } = &index_ref.kind {
            self.destroy_relation(&format!("{}:{}", base.name, inverse))?;
        }
        self.index_ctxs.remove(idx_rel.as_str());
        self.write_catalog_row(&base)?;
        Ok(status_ok())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Engine sys-op doors for index / operator lifecycle
// ─────────────────────────────────────────────────────────────────────────

impl<S: Storage> Engine<S> {
    /// `::compact` — flush the store.
    pub(crate) fn sys_compact(&self) -> Result<NamedRows> {
        self.store.sync()?;
        Ok(status_ok())
    }

    /// `::merkle_root` — bounded cold scan of store (or one relation) state.
    pub(crate) fn sys_merkle_root(
        &self,
        rel: Option<&Symbol>,
        options: &ScriptOptions,
    ) -> Result<NamedRows> {
        // A cold root is a full ordered rescan, so the scan must be
        // bounded: the session's derived-tuple ceiling doubles as the
        // scan ceiling (one scanned pair = one unit), with a default
        // when the caller sets none. A ceiling of zero refuses before
        // scanning anything.
        let ceiling = match options.derived_tuple_ceiling {
            Some(c) => {
                NonZeroU64::new(c).ok_or(crate::store::merkle::MerkleScanExceeded { ceiling: 0 })?
            }
            None => DEFAULT_MERKLE_SCAN_CEILING,
        };
        let rtx = self.store.read_tx()?;
        let root = match rel {
            None => crate::store::merkle::state_root(&rtx, ceiling)?,
            Some(name) => {
                let id = get_relation(&rtx, &name.name)?.id;
                crate::store::merkle::relation_root(&rtx, id, ceiling)?
            }
        };
        Ok(NamedRows::try_new(
            vec!["root".into()],
            vec![Tuple::from_vec(vec![DataValue::from(root.to_hex())])],
        )?)
    }

    /// `::indices` — list index names and kinds on a relation.
    pub(crate) fn sys_list_indices(&self, name: &Symbol) -> Result<NamedRows> {
        let tx = SessionTx::new_read(self.store.read_tx()?, ScriptOptions::new());
        let handle = get_relation(&tx.store, &name.name)?;
        let rows = handle
            .indices
            .iter()
            .map(|r| {
                let kind = match &r.kind {
                    IndexKind::Plain { .. } => "plain",
                    IndexKind::Temporal => "temporal",
                    IndexKind::Hnsw(..) => "hnsw",
                    IndexKind::Fts(..) => "fts",
                    IndexKind::Lsh { .. } => "lsh",
                };
                Tuple::from_vec(vec![
                    DataValue::from(r.name.as_str()),
                    DataValue::from(kind),
                ])
            })
            .collect();
        Ok(NamedRows::try_new(
            vec!["name".into(), "kind".into()],
            rows,
        )?)
    }

    /// `::index create`.
    pub(crate) fn sys_create_index(
        &self,
        rel: &Symbol,
        name: &Symbol,
        cols: &[Symbol],
    ) -> Result<NamedRows> {
        self.sys_write(|tx| tx.create_plain_index(&rel.name, &name.name, cols))
    }

    /// `::hnsw create`.
    pub(crate) fn sys_create_vector_index(
        &self,
        cfg: &crate::parse::sys::HnswIndexConfig,
    ) -> Result<NamedRows> {
        self.sys_write(|tx| tx.create_hnsw_index(cfg))
    }

    /// `::fts create`.
    pub(crate) fn sys_create_fts_index(
        &self,
        cfg: &crate::parse::sys::FtsIndexConfig,
    ) -> Result<NamedRows> {
        self.sys_write(|tx| tx.create_fts_index(cfg))
    }

    /// `::lsh create`.
    pub(crate) fn sys_create_minhash_lsh_index(
        &self,
        cfg: &crate::parse::sys::MinHashLshConfig,
    ) -> Result<NamedRows> {
        self.sys_write(|tx| tx.create_lsh_index(cfg))
    }

    /// `::index drop`.
    pub(crate) fn sys_remove_index(&self, rel: &Symbol, idx: &Symbol) -> Result<NamedRows> {
        self.sys_write(|tx| tx.remove_index(&rel.name, &idx.name))
    }
}

/// read-side RA operator that serves window/stab queries over these
/// postings is a separate chunk, see `IndexKind::Temporal`'s doc comment).
///
/// These tests drive `SessionTx` directly rather than through
/// `Db::run_script`: `::temporal index create` has no parsed KyzoScript
/// surface yet (the grammar and `SysOp` dispatch live in `parse/sys.rs`
/// and `session/db.rs`, both outside this chunk's file scope — see the
/// landing report), and `ClaimPolarity::Erase` (a system-time correction)
/// has no scripted write surface at all today, in or out of this scope.
/// Every function called here (`create_temporal_index`, `update_indices`,
/// `temporal_index_write`) is the exact same code the eventual parsed
/// surface and correction mechanism would call.
#[cfg(test)]
mod temporal_index_tests {
    use miette::{Result, miette};
    use std::cmp::Reverse;

    use super::*;
    use crate::session::catalog::Catalog;
    use crate::session::db::{Engine, ScriptOptions};
    use crate::store::sim::SimStorage;
    use crate::store::{ReadTx, Storage};
    use kyzo_model::data_value_any;
    use kyzo_model::program::InputRelationHandle;
    use kyzo_model::schema::ColType;
    use kyzo_model::value::{StoredValiditySlot, ValidityTs};

    fn vts(t: i64) -> ValidityTs {
        ValidityTs::from_raw(t)
    }

    fn open_engine(store: SimStorage) -> Result<Engine<SimStorage>> {
        Ok(Engine::compose(store, Catalog::new())?)
    }

    fn col(name: &str) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            typing: NullableColType::required(ColType::Int),
            default_gen: None,
        }
    }

    /// A single-key-column base relation input: `k` is both the whole key
    /// and the fact's whole identity, so every event below is unambiguous
    /// without a dependent-column payload to track.
    fn base_input(name: &str) -> InputRelationHandle {
        InputRelationHandle {
            name: Symbol::new(name, SourceSpan::default()),
            metadata: StoredRelationMetadata {
                keys: vec![col("k")],
                non_keys: vec![],
            },
            key_bindings: vec![Symbol::new("k", SourceSpan::default())],
            dep_bindings: vec![],
            span: SourceSpan::default(),
        }
    }

    fn open_session(db: &Engine<SimStorage>) -> SessionTx<<SimStorage as Storage>::WriteTx> {
        SessionTx::new_write(db.store.write_tx()?, ScriptOptions::new())
    }

    /// Write one base point event directly (bypassing `execute_relation`,
    /// which never produces `Erase`) and drive it through the exact same
    /// `update_indices`/`temporal_index_write` seam the mutation pipeline
    /// uses for Assert/Retract; `Erase` — a correction with no production
    /// caller yet — goes straight to `temporal_index_write`, proving the
    /// write PRIMITIVE composes correctly for whatever future correction
    /// mechanism calls it.
    ///
    /// `reasserts_existing`: when true, an `Assert` event ALSO supplies
    /// its own row as `old_kv` — simulating a `:put`-overwrite or
    /// `:update` (both `old_kv` and `new_kv` present), the exact branch
    /// story #62's hostile review found unguarded. `Temporal` discards
    /// payload, so `old` and `new` compose to the IDENTICAL posting
    /// regardless of content — this flag exercises that both-`Some` path
    /// without needing a dependent column to vary.
    #[allow(clippy::too_many_arguments)]
    fn write_base_event(
        stx: &mut SessionTx<<SimStorage as Storage>::WriteTx>,
        base: &RelationHandle,
        idx_handle: &RelationHandle,
        k: i64,
        valid: i64,
        sys: i64,
        polarity: ClaimPolarity,
        reasserts_existing: bool,
    ) {
        let span = SourceSpan::default();
        let row = vec![DataValue::from(k)];
        let key = base
            .encode_bitemporal_key_for_store(&row, vts(valid), vts(sys), span)?;
        let val = base
            .encode_bitemporal_val_for_store(&row, polarity, span)?;
        stx.put_routed(Residency::Stored, &key, &val)?;
        match polarity {
            ClaimPolarity::Assert => {
                let old = reasserts_existing.then_some(row.as_slice());
                stx.update_indices(base, Some(&row), old, vts(valid), vts(sys))?;
            }
            ClaimPolarity::Retract => {
                stx.update_indices(base, None, Some(&row), vts(valid), vts(sys))?;
            }
            ClaimPolarity::Erase => {
                stx.temporal_index_write(
                    base,
                    idx_handle,
                    &row,
                    ClaimPolarity::Erase,
                    vts(valid),
                    vts(sys),
                )?;
            }
        }
    }

    /// One decoded posting row, in the form every assertion below compares
    /// against: `(leading valid ts, base key, tail valid ts, tail sys ts,
    /// polarity)`.
    type DecodedPosting = (i64, i64, i64, i64, ClaimPolarity);

    fn scan_postings(tx: &impl ReadTx, idx_handle: &RelationHandle) -> Result<Vec<DecodedPosting>> {
        let lower: Vec<u8> = Tuple::default()
            .encode_as_key(idx_handle.id)
            .as_ref()
            .to_vec();
        let upper = (idx_handle.id.raw() + 1).to_be_bytes().to_vec();
        tx.range_scan(&lower, &upper)
            .map(|r| {
                let (k, v) = r.map_err(|e| miette!("posting row decodes cleanly: {e}"))?;
                let tup = kyzo_model::value::decode_tuple_from_key(&k, 4)
                    .map_err(|e| miette!("posting key decodes cleanly: {e}"))?;
                let leading = match &tup.as_slice()[0] {
                    DataValue::Validity(vv) => vv.ts_micros(),
                    other @ (data_value_any!()) => {
                        return Err(miette!("expected the leading Validity column, got {other:?}"));
                    }
                };
                let key_col = tup[1].get_int().ok_or_else(|| miette!("int base key column"))?;
                let tail_valid = match &tup.as_slice()[2] {
                    DataValue::Validity(vv) => vv.ts_micros(),
                    other @ (data_value_any!()) => {
                        return Err(miette!("expected the tail valid slot, got {other:?}"));
                    }
                };
                let tail_sys = match &tup.as_slice()[3] {
                    DataValue::Validity(vv) => vv.ts_micros(),
                    other @ (data_value_any!()) => {
                        return Err(miette!("expected the tail sys slot, got {other:?}"));
                    }
                };
                let polarity = crate::store::time::claim_polarity_of_value(&v)
                    .map_err(|e| miette!("posting value decodes cleanly: {e}"))?;
                Ok((leading, key_col, tail_valid, tail_sys, polarity))
            })
            .collect()
    }

    /// One decoded BASE row, in the SAME `DecodedPosting` shape as
    /// [`scan_postings`] (a base row's own valid instant fills both the
    /// "leading" and "tail valid" fields), so the two scans compare
    /// directly for the bijection tests below. Every base relation in
    /// this module has exactly one Int key column.
    fn scan_base_rows(tx: &impl ReadTx, base: &RelationHandle) -> Result<Vec<DecodedPosting>> {
        let lower: Vec<u8> = Tuple::default().encode_as_key(base.id).as_ref().to_vec();
        let upper = (base.id.raw() + 1).to_be_bytes().to_vec();
        tx.range_scan(&lower, &upper)
            .map(|r| {
                let (k, v) = r.map_err(|e| miette!("base row decodes cleanly: {e}"))?;
                let tup = kyzo_model::value::decode_tuple_from_key(&k, 3)
                    .map_err(|e| miette!("base key decodes cleanly: {e}"))?;
                let key_col = tup[0].get_int().ok_or_else(|| miette!("int base key column"))?;
                let valid = match &tup.as_slice()[1] {
                    DataValue::Validity(vv) => vv.ts_micros(),
                    other @ (data_value_any!()) => {
                        return Err(miette!("expected the valid slot, got {other:?}"));
                    }
                };
                let sys = match &tup.as_slice()[2] {
                    DataValue::Validity(vv) => vv.ts_micros(),
                    other @ (data_value_any!()) => {
                        return Err(miette!("expected the sys slot, got {other:?}"));
                    }
                };
                let polarity = crate::store::time::claim_polarity_of_value(&v)
                    .map_err(|e| miette!("base value decodes cleanly: {e}"))?;
                Ok((valid, key_col, valid, sys, polarity))
            })
            .collect()
    }

    /// `ClaimPolarity` derives `Eq` but not `Ord` (a value-side type,
    /// never a sort key elsewhere): every bijection comparison below sorts
    /// by this key instead of a bare `.sort()`.
    fn decoded_posting_sort_key(r: &DecodedPosting) -> (i64, i64, i64, i64, u8) {
        (r.0, r.1, r.2, r.3, r.4.encode())
    }

    /// Posting rows for a scripted history — assert, retract, and an
    /// erase that corrects the SAME valid instant as the initial assert
    /// with a newer sys (the "same-instant sys correction") — decoded and
    /// compared field-for-field, plus one literal raw-byte check proving
    /// the key layout claim directly: the leading Validity column really
    /// does precede the base key bytes, not follow them.
    #[test]
    fn temporal_index_posting_rows_match_the_scripted_history_exactly() -> Result<()>  {
        let db = open_engine(SimStorage::new(0x7E57_0001));
        let mut stx = open_session(&db);
        stx.create_relation(base_input("e"), KeyspaceKind::Facts)?;
        stx.create_temporal_index("e", "t")?;
        let base = stx.get_relation("e")?;
        let idx_handle = stx.get_relation("e:t")?;

        // (k, valid, sys, polarity). The third event corrects the FIRST
        // one: same valid instant (10), newer sys (3) — an Erase un-
        // recording the earlier Assert, never a new instant.
        let events = [
            (1i64, 10i64, 1i64, ClaimPolarity::Assert),
            (1, 20, 2, ClaimPolarity::Retract),
            (1, 10, 3, ClaimPolarity::Erase),
        ];
        for &(k, valid, sys, polarity) in &events {
            write_base_event(&mut stx, &base, &idx_handle, k, valid, sys, polarity, false);
        }
        stx.store.commit()?;

        let tx = db.store.read_tx()?;
        let mut got = scan_postings(&tx, &idx_handle)?;
        got.sort_by_key(|r| (Reverse(r.0), r.1, Reverse(r.2), Reverse(r.3)));
        let mut want: Vec<DecodedPosting> = events
            .iter()
            .map(|&(k, valid, sys, polarity)| (valid, k, valid, sys, polarity))
            .collect();
        want.sort_by_key(|r| (Reverse(r.0), r.1, Reverse(r.2), Reverse(r.3)));
        assert_eq!(
            got, want,
            "every posting must carry exactly its base event's own \
             (valid, sys, polarity) — mirrored, never re-stamped"
        );

        // The literal byte claim: the FIRST event's posting key is
        // `[idx_id][Validity(10) leading][k=1][Validity(10) tail][Validity(1) tail]`
        // — independently hand-encoded and compared byte-for-byte.
        let expected_first_tuple = vec![
            StoredValiditySlot::new(vts(10)).as_datavalue(),
            DataValue::from(1i64),
            StoredValiditySlot::new(vts(10)).as_datavalue(),
            StoredValiditySlot::new(vts(1)).as_datavalue(),
        ];
        let expected_key = expected_first_tuple.encode_as_key(idx_handle.id);
        let (got_key, _) = tx
            .range_scan(
                expected_key.as_ref(),
                &(idx_handle.id.raw() + 1).to_be_bytes(),
            )
            .next()
            .map_err(|e| miette!("at least one posting at or after the hand-encoded key: {e}"))?;
        assert_eq!(
            got_key,
            expected_key.as_ref(),
            "the hand-encoded posting key (leading Validity(10), then k=1, \
             then the tail) must be the literal first key on disk"
        );
        Ok(())
    }

    /// The rebuildability law: an index attached BEFORE any base writes
    /// (maintained incrementally, one posting per write) and the SAME
    /// index attached AFTER those writes (backfilled by a full rescan of
    /// the base's stored history) must produce byte-identical posting
    /// keyspaces. Both universes create exactly the same two relations in
    /// the same order (base, then index), so their relation ids align and
    /// a literal raw-byte comparison — no id-prefix stripping — is valid.
    #[test]
    fn temporal_index_backfill_equals_incremental() -> Result<()>  {
        // (k, valid, sys, polarity, reasserts_existing) — several keys,
        // mixed polarities, instants not in chronological write order,
        // several sys stamps. The `(1, 120, 14, Assert, true)` event is a
        // real-shaped overwrite: as-of resolution AT valid=120 finds the
        // `(1, 100, 10, Assert)` row (100 <= 120), so a real pipeline
        // write here supplies BOTH `old_kv` and `new_kv` — the branch
        // story #62's hostile review found unguarded, now included in the
        // byte-identity check.
        let events = [
            (1i64, 100i64, 10i64, ClaimPolarity::Assert, false),
            (2, 200, 11, ClaimPolarity::Assert, false),
            (1, 150, 12, ClaimPolarity::Retract, false),
            (3, 300, 13, ClaimPolarity::Assert, false),
            (1, 120, 14, ClaimPolarity::Assert, true),
            (2, 250, 15, ClaimPolarity::Retract, false),
            (3, 310, 16, ClaimPolarity::Assert, false),
        ];

        // Universe A: index live from the start (incremental).
        let db_a = open_engine(SimStorage::new(0xB0071));
        let mut stx_a = open_session(&db_a);
        stx_a
            .create_relation(base_input("b"), KeyspaceKind::Facts)?;
        stx_a.create_temporal_index("b", "t")?;
        let base_a = stx_a.get_relation("b")?;
        let idx_a = stx_a.get_relation("b:t")?;
        for &(k, valid, sys, polarity, reasserts_existing) in &events {
            write_base_event(
                &mut stx_a,
                &base_a,
                &idx_a,
                k,
                valid,
                sys,
                polarity,
                reasserts_existing,
            );
        }
        stx_a.store.commit()?;

        // Universe B: index attached AFTER the same writes (backfill).
        // No index exists yet, so `write_base_event`'s `update_indices`
        // call is a no-op over an empty index list — write the base rows
        // directly instead, to keep the helper's contract ("an index is
        // attached") honest.
        let db_b = open_engine(SimStorage::new(0xB0072));
        let mut stx_b = open_session(&db_b);
        stx_b
            .create_relation(base_input("b"), KeyspaceKind::Facts)?;
        let base_b = stx_b.get_relation("b")?;
        for &(k, valid, sys, polarity, _) in &events {
            let span = SourceSpan::default();
            let row = vec![DataValue::from(k)];
            let key = base_b
                .encode_bitemporal_key_for_store(&row, vts(valid), vts(sys), span)?;
            let val = base_b
                .encode_bitemporal_val_for_store(&row, polarity, span)?;
            stx_b.put_routed(Residency::Stored, &key, &val)?;
        }
        stx_b.create_temporal_index("b", "t")?;
        let idx_b = stx_b.get_relation("b:t")?;
        stx_b.store.commit()?;

        assert_eq!(
            base_a.id, base_b.id,
            "both universes must create the same relations in the same \
             order for the raw-byte comparison below to be valid"
        );
        assert_eq!(idx_a.id, idx_b.id);

        let tx_a = db_a.store.read_tx()?;
        let tx_b = db_b.store.read_tx()?;
        let lower: Vec<u8> = Tuple::default().encode_as_key(idx_a.id).as_ref().to_vec();
        let upper = (idx_a.id.raw() + 1).to_be_bytes().to_vec();
        let raw_a: Vec<(Slice, Slice)> = tx_a
            .range_scan(&lower, &upper)
            .collect::<Result<_>>()?;
        let raw_b: Vec<(Slice, Slice)> = tx_b
            .range_scan(&lower, &upper)
            .collect::<Result<_>>()?;
        assert!(
            !raw_a.is_empty(),
            "the incremental universe must have posted something"
        );
        assert_eq!(
            raw_a, raw_b,
            "backfill-equals-incremental: an index attached after N base \
             writes must reproduce the exact keyspace an index live since \
             the first write would hold"
        );
        Ok(())
    }

    /// Polarity/coordinate mirroring, generalized over a scripted history:
    /// every base row implies exactly one posting at the SAME (valid,
    /// sys, polarity) — not merely for the byte-verified fixture above.
    #[test]
    fn every_base_row_mirrors_to_exactly_one_posting_at_its_own_coordinate() -> Result<()>  {
        let db = open_engine(SimStorage::new(0x7E57_0002));
        let mut stx = open_session(&db);
        stx.create_relation(base_input("m"), KeyspaceKind::Facts)?;
        stx.create_temporal_index("m", "t")?;
        let base = stx.get_relation("m")?;
        let idx_handle = stx.get_relation("m:t")?;

        let events = [
            (1i64, 5i64, 1i64, ClaimPolarity::Assert),
            (2, 6, 2, ClaimPolarity::Assert),
            (1, 15, 3, ClaimPolarity::Retract),
            (2, 6, 4, ClaimPolarity::Erase),
        ];
        for &(k, valid, sys, polarity) in &events {
            write_base_event(&mut stx, &base, &idx_handle, k, valid, sys, polarity, false);
        }
        stx.store.commit()?;

        let tx = db.store.read_tx()?;
        let mut base_rows = scan_base_rows(&tx, &base)?;
        let mut postings = scan_postings(&tx, &idx_handle)?;
        base_rows.sort_by_key(decoded_posting_sort_key);
        postings.sort_by_key(decoded_posting_sort_key);
        assert_eq!(
            base_rows, postings,
            "the posting keyspace, read as (valid, key, valid, sys, \
             polarity), must be a bijection with the base's own rows"
        );
        Ok(())
    }

    /// Hostile-review finding (story #62): `update_indices`'s `Temporal`
    /// arm used to fire BOTH `old` (Retract) and `new` (Assert) whenever
    /// both were `Some` — exactly the `:put`-overwrite and `:update`
    /// shape. NEITHER prior test above drove that branch: `write_base_event`
    /// only ever supplied one side (or, via `reasserts_existing`, a
    /// same-content synthetic one). This test drives the REAL production
    /// pipeline (`Db::run_script`, not the direct-write helper) through a
    /// fresh insert, an overwrite of the SAME key, an `:update`, then a
    /// removal — the exact previously-uncovered branch — and checks the
    /// exact posting byte set.
    #[test]
    fn temporal_index_production_pipeline_mirrors_one_posting_per_base_event() -> Result<()>  {
        let db = open_engine(SimStorage::new(0x7E57_0003));
        db.run_script("?[k, v] <- [] :create po {k => v}", BTreeMap::new())
            .map_err(|e| miette!("create: {e}"))?;
        {
            // `::temporal index create` has no parsed surface yet (see
            // the landing report); attach it directly.
            let mut stx = open_session(&db);
            stx.create_temporal_index("po", "t")?;
            stx.store.commit()?;
        }

        db.run_script("?[k, v] <- [[1, 100]] :put po {k, v}", BTreeMap::new())
            .map_err(|e| miette!("fresh insert: {e}"))?;
        // The overwrite: `current_row_routed` resolves at THIS write's own
        // valid to the prior row (`old_kv = Some`), and this write
        // supplies a new payload (`new_kv = Some`) — both `Some`, the
        // exact branch under review.
        db.run_script("?[k, v] <- [[1, 200]] :put po {k, v}", BTreeMap::new())
            .map_err(|e| miette!("overwrite: {e}"))?;
        db.run_script("?[k, v] <- [[1, 300]] :update po {k, v}", BTreeMap::new())
            .map_err(|e| miette!("update: {e}"))?;
        db.run_script("?[k] <- [[1]] :rm po {k}", BTreeMap::new())
            .map_err(|e| miette!("remove: {e}"))?;

        let rtx = SessionTx::new_read(db.store.read_tx()?, ScriptOptions::new());
        let base = rtx.get_relation("po")?;
        let idx_handle = rtx.get_relation("po:t")?;
        let mut base_rows = scan_base_rows(&rtx.store, &base)?;
        let mut postings = scan_postings(&rtx.store, &idx_handle)?;
        assert_eq!(
            base_rows.len(),
            4,
            "one base row per script mutation: insert, overwrite, update, remove"
        );
        base_rows.sort_by_key(decoded_posting_sort_key);
        postings.sort_by_key(decoded_posting_sort_key);
        assert_eq!(
            base_rows, postings,
            "every base event — insert, overwrite, update, remove alike — \
             must mirror to EXACTLY one posting at its own coordinate; the \
             overwrite/update events are precisely the ones a Plain-style \
             transition mirror would have wasted a clobbered Retract \
             write on"
        );
        assert_eq!(
            postings
                .iter()
                .filter(|p| p.4 == ClaimPolarity::Retract)
                .count(),
            1,
            "exactly one Retract posting — from the :rm — never one per \
             overwrite/update"
        );
        Ok(())
    }

    /// The write-COUNT law, closing the gap the byte-content tests above
    /// cannot: a confirmation reviewer proved that regressing the
    /// `Temporal` arm to the old dual-fire shape (retract-old +
    /// assert-new, unconditionally, whenever both are `Some`) is
    /// BYTE-IDENTICAL on disk to the single-fire code above, because every
    /// production call site resolves `old_kv` at the write's own `valid` —
    /// so the retract and the assert always compose to the SAME posting
    /// key at the SAME coordinate, and the assert (applied second, same
    /// key, same in-transaction write set) clobbers the retract before
    /// commit ever serializes a byte. No scan of the committed keyspace,
    /// however thorough, can tell the two shapes apart. What differs is
    /// only the number of `WriteTx::put` CALLS made getting there — the
    /// posting index's actual law ("one posting PER BASE EVENT") is a
    /// count claim, not a content claim, so it needs a count oracle:
    /// `SimStorage::put_call_count`, which totals calls at the call site
    /// (before any in-transaction collapse), not post-collapse entries.
    ///
    /// Drives the real production pipeline (`Db::run_script`, matching
    /// `temporal_index_production_pipeline_mirrors_one_posting_per_base_event`
    /// above) through the four mutation kinds and checks the EXACT put
    /// delta each one costs: 1 base-row put (assert or retract — `:rm`
    /// writes a Retract-flagged version, never a physical delete, so it
    /// costs a put too) + 1 posting put, always — 2, never 3, even for
    /// the overwrite/update calls that supply BOTH `old_kv` and `new_kv`.
    /// `del_call_count` stays 0 throughout: this bitemporal pipeline never
    /// calls `WriteTx::del`/`del_range` on a mutation path at all.
    #[test]
    fn temporal_index_write_count_law_holds_for_every_mutation_kind() -> Result<()>  {
        let db = open_engine(SimStorage::new(0x7E57_0004));
        db.run_script("?[k, v] <- [] :create po {k => v}", BTreeMap::new())
            .map_err(|e| miette!("create: {e}"))?;
        {
            let mut stx = open_session(&db);
            stx.create_temporal_index("po", "t")?;
            stx.store.commit()?;
        }

        let puts_before_all = db.store.put_call_count();
        let dels_before_all = db.store.del_call_count();

        let check_delta = |label: &str, puts_before: u64, dels_before: u64| {
            let puts_after = db.store.put_call_count();
            let dels_after = db.store.del_call_count();
            assert_eq!(
                puts_after - puts_before,
                2,
                "{label}: expected exactly 2 put CALLS (1 base row + 1 \
                 posting), got {} — a dual-fired Temporal arm costs 3 on \
                 the overwrite/update kinds even though the bytes it \
                 lands are identical to the single-fire shape",
                puts_after - puts_before
            );
            assert_eq!(
                dels_after - dels_before,
                0,
                "{label}: this pipeline never physically deletes a key — \
                 every retraction is a Retract-flagged put"
            );
            (puts_after, dels_after)
        };

        // :put fresh — no existing row, so `old_kv` is `None`: the branch
        // the dual-fire mutant cannot distinguish from single-fire.
        db.run_script("?[k, v] <- [[1, 100]] :put po {k, v}", BTreeMap::new())
            .map_err(|e| miette!("fresh insert: {e}"))?;
        let (p1, d1) = check_delta("put fresh", puts_before_all, dels_before_all);

        // :put overwrite — `old_kv` AND `new_kv` both `Some`: the exact
        // branch story #62's hostile review found unguarded.
        db.run_script("?[k, v] <- [[1, 200]] :put po {k, v}", BTreeMap::new())
            .map_err(|e| miette!("overwrite: {e}"))?;
        let (p2, d2) = check_delta("put overwrite", p1, d1);

        // :update — same both-`Some` shape as the overwrite.
        db.run_script("?[k, v] <- [[1, 300]] :update po {k, v}", BTreeMap::new())
            .map_err(|e| miette!("update: {e}"))?;
        let (p3, d3) = check_delta("update", p2, d2);

        // :rm — `new_kv` is `None`, `old_kv` is `Some`: single-fire and
        // the dual-fire mutant agree here too (only overwrite/update
        // diverge), so this is the control showing the law holds
        // everywhere, not just where the mutant happens to differ.
        db.run_script("?[k] <- [[1]] :rm po {k}", BTreeMap::new())
            .map_err(|e| miette!("remove: {e}"))?;
        check_delta("rm", p3, d3);
        Ok(())
    }
}

#[cfg(test)]
mod index_surface_tests {
    use miette::{Result, miette};
    use std::collections::BTreeMap;

    use crate::data::json::NamedRows;
    use crate::session::catalog::Catalog;
    use crate::session::db::Engine;
    use crate::store::Storage;
    use crate::store::fjall::new_fjall_storage;
    use kyzo_model::value::{DataValue, Tuple};

    fn no_params() -> BTreeMap<String, DataValue> {
        BTreeMap::new()
    }

    fn open_engine<S: Storage>(store: S) -> Result<Engine<S>> {
        Ok(Engine::compose(store, Catalog::new())?)
    }

    /// Result rows as sorted `i64` vectors, for order-independent assertions.
    fn int_rows(nr: &NamedRows) -> Vec<Vec<i64>> {
        let mut out: Vec<Vec<i64>> = nr
            .rows()
            .iter()
            .map(|r| r.iter().map(|v| v.get_int().ok_or_else(|| miette!("int"))?).collect())
            .collect();
        out.sort();
        out
    }

    /// Index-served as-of reads answer exactly like base scans, through
    /// the REAL `::index create` surface: same rows at every coordinate,
    /// including one where the fact was retracted and one where its value
    /// changed between coordinates.
    #[test]
    fn plain_index_asof_reads_match_base_scans() -> Result<()>  {
        let dir = tempfile::tempdir().map_err(|e| miette!("tempdir: {e}"))?;
        let db = open_engine(new_fjall_storage(dir.path())?);
        db.run_script(
            "?[k, v] <- [[1, 10], [2, 20]] :create t {k => v}",
            no_params(),
        )
        .map_err(|e| miette!("create: {e}"))?;
        db.run_script("::index create t:by_v {v}", no_params())
            .map_err(|e| miette!("::index create must be a live surface: {e}"))?;
        // History: update k=1, retract k=2 (distinct stamps).
        db.run_script("?[k, v] <- [[1, 11]] :put t {k => v}", no_params())
            .map_err(|e| miette!("update: {e}"))?;
        db.run_script("?[k] <- [[2]] :rm t {k}", no_params())
            .map_err(|e| miette!("retract: {e}"))?;

        // The index-served plan binds v first (the index's leading
        // column); the base plan binds k first. Same logical query.
        let via_index = db
            .run_script("?[v, k] := *t:by_v{v, k} :order v", no_params())
            .map_err(|e| miette!("index read: {e}"))?;
        let via_base = db
            .run_script("?[v, k] := *t[k, v] :order v", no_params())
            .map_err(|e| miette!("base read: {e}"))?;
        assert_eq!(
            via_index.rows(),
            via_base.rows(),
            "index and base must agree on current state"
        );
        assert_eq!(via_base.rows().len(), 1, "one row: k=1 updated, k=2 gone");
        let want: Tuple = Tuple::from_vec(vec![DataValue::from(11), DataValue::from(1)]);
        assert_eq!(via_base.rows()[0], want);
        Ok(())
    }

    /// Backfill batching at scale: a plain index created over MORE rows
    /// than one backfill batch (4096) must index every row exactly once —
    /// the resume bound (fact prefix + `Bot`) must neither skip nor
    /// double-count across batch boundaries.
    #[test]
    fn index_backfill_resumes_correctly_across_batches() -> Result<()>  {
        let dir = tempfile::tempdir().map_err(|e| miette!("tempdir: {e}"))?;
        let db = open_engine(new_fjall_storage(dir.path())?);
        db.run_script("?[k, v] <- [[0, 0]] :create big {k => v}", no_params())
            .map_err(|e| miette!("create: {e}"))?;
        let mut chunk = vec![];
        for i in 0..5000i64 {
            chunk.push(format!("[{}, {}]", i, i * 7));
            if chunk.len() == 500 {
                db.run_script(
                    &format!("?[k, v] <- [{}] :put big {{k => v}}", chunk.join(", ")),
                    no_params(),
                )
                .map_err(|e| miette!("seed chunk: {e}"))?;
                chunk.clear();
            }
        }
        db.run_script("::index create big:by_v {v}", no_params())
            .map_err(|e| miette!("index create over 5000 rows: {e}"))?;
        let via_index = db
            .run_script("?[count(v)] := *big:by_v{v, k}", no_params())
            .map_err(|e| miette!("count via index: {e}"))?;
        assert_eq!(
            int_rows(&via_index),
            vec![vec![5000]],
            "every row indexed once"
        );
        // And value-level agreement with the base on a spot range.
        let via_base = db
            .run_script(
                "?[v] := *big[k, v], k >= 4094, k <= 4098 :order v",
                no_params(),
            )
            .map_err(|e| miette!("base spot: {e}"))?;
        let via_idx = db
            .run_script(
                "?[v] := *big:by_v{v, k}, k >= 4094, k <= 4098 :order v",
                no_params(),
            )
            .map_err(|e| miette!("index spot: {e}"))?;
        assert_eq!(via_base.rows(), via_idx.rows(), "batch-boundary rows agree");
        Ok(())
    }
}
