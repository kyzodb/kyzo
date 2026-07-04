/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (`query/stored.rs`, MPL-2.0), re-architected for the KyzoDB session:
 *
 * - Mutation lives on `SessionTx<T> where T: WriteTx` — running it against
 *   a read session does not compile (the session-species law, story #3).
 * - **The cleanups machinery is gone.** The original returned byte ranges
 *   (`Vec<(Vec<u8>, Vec<u8>)>`) from every mutation for a deferred
 *   post-commit `del_range_from_persisted` pass (a RocksDB-era shape).
 *   The kernel's `del_range` deletes inside the transaction — both
 *   snapshot data and the transaction's own writes — so `:replace` and
 *   `::remove` are atomic with the query and an abort rolls them back.
 * - **Triggers are parsed once per session** ([`SessionTx::parsed_trigger`]):
 *   the stored form is still raw source (the catalog's landed wire format),
 *   but each source string is parsed exactly once per session and the
 *   cached `InputProgram` is cloned per firing. This is sound because a
 *   session has ONE `cur_vld` (parse substitutes the current validity), and
 *   it makes the ratified "parsed substances in the catalog" end state a
 *   wire-format decision rather than a performance one — FLAG(catalog
 *   tier): stored parsed substances with provenance remain the end state.
 * - **Index maintenance is a typed seam.** Plain projection indices
 *   (`IndexKind::Plain`) are maintained here, resolved BY REFERENCE
 *   through the catalog (the landed `IndexRef` model — no embedded handle
 *   copies). The manifest kinds (HNSW/FTS/LSH) are unreachable until the
 *   operator tier lands (`create_relation` cannot attach them) and hit the
 *   typed [`ManifestIndexNotLanded`] refusal, never silent corruption:
 *   when the operator tier lands, it replaces that arm with the real
 *   put/del hooks (upstream's `update_in_hnsw`/`put_in_fts`/`put_in_lsh`).
 * - Law 5: the original's `rmp_serde::from_slice(..).unwrap()` on the old
 *   value in `update_in_relation` is a fallible decode; `unreachable!()`
 *   on collected tuples is a typed invariant error.
 * - `Db::run_query` returns `NamedRows` alone (no cleanup ranges), so the
 *   trigger-recursion call sites here simplify accordingly.
 */

//! The mutation pipeline: how a query's result set changes a stored
//! relation.
//!
//! `execute_relation` receives the evaluated rows and the `:put`/`:rm`/…
//! operation, coerces each row through the relation's declared column
//! types (the landed `data/relation.rs` coercions), writes through the
//! session — temp relations to the scratch store, stored relations to the
//! kernel transaction — maintains plain indices, and collects old/new rows
//! for triggers (executed inside the same transaction) and callbacks
//! (delivered after commit).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use itertools::Itertools;
use miette::{Diagnostic, Result, WrapErr, bail};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::bitemporal::ClaimPolarity;
use crate::data::expr::Expr;
use crate::data::program::{
    FixedRuleApply, InputInlineRulesOrFixed, InputProgram, InputRelationHandle, RelationOp,
    WriteValidity,
};
use crate::data::relation::{ColumnDef, NullableColType, StoredRelationMetadata};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::tuple::{Tuple, TupleT};
use crate::data::value::{DataValue, ValidityTs};
use crate::fixed_rule::utilities::Constant;
use crate::fixed_rule::{FixedRule, FixedRuleHandle, NamedRows};
use crate::runtime::callback::{CallbackCollector, CallbackOp};
use crate::runtime::db::{Db, SessionTx};
use crate::runtime::relation::{
    AccessLevel, IndexKind, IndexRef, InsufficientAccessLevel, KeyspaceKind, RelationHandle,
};
use crate::storage::{Storage, WriteTx};

#[derive(Debug, Error, Diagnostic)]
#[error("Assertion failure for {key:?} of {relation}: {notice}")]
#[diagnostic(code(transact::assertion_failure))]
pub(crate) struct TransactAssertionFailure {
    relation: String,
    key: Vec<DataValue>,
    notice: String,
}

/// SEAM(operator tier): a manifest-backed index (HNSW/FTS/LSH) appeared on
/// a relation before the operator tier that maintains it has landed. This
/// is unreachable today — nothing can attach one — and the refusal exists
/// so that WHEN they land, forgetting to replace this arm is a loud typed
/// error on the first mutation, never a silently stale index.
#[derive(Debug, Error, Diagnostic)]
#[error("index '{0}' on relation '{1}' needs the index-operator tier, which has not landed")]
#[diagnostic(code(tx::manifest_index_not_landed))]
pub(crate) struct ManifestIndexNotLanded(pub(crate) String, pub(crate) String);

#[derive(Debug, Error, Diagnostic)]
#[error("replace op in trigger is not allowed: {0}")]
#[diagnostic(code(eval::replace_in_trigger))]
struct ReplaceInTrigger(String);

/// The ceiling on trigger cascade depth. Triggers cascade — a mutation made
/// by a trigger fires the target relation's own triggers — but boundedly:
/// a cascade about to exceed this depth is a typed refusal that aborts the
/// whole transaction. Never silent truncation (the mutation would land but
/// its triggers would not fire) and never an unbounded loop (a trigger
/// writing its own relation would otherwise recurse forever).
pub(crate) const MAX_TRIGGER_CASCADE_DEPTH: usize = 32;

/// A trigger cascade reached [`MAX_TRIGGER_CASCADE_DEPTH`]. A cascade this
/// deep is almost certainly a trigger cycle (a trigger writing to its own
/// relation, or a loop of relations firing each other).
#[derive(Debug, Error, Diagnostic)]
#[error("trigger cascade on relation '{0}' exceeded the depth ceiling of {1}")]
#[diagnostic(code(tx::trigger_cascade_too_deep))]
#[diagnostic(help(
    "the transaction was aborted whole; restructure the triggers so they \
     do not form a cycle"
))]
pub(crate) struct TriggerCascadeTooDeep(pub(crate) String, pub(crate) usize);

#[derive(Debug, Error, Diagnostic)]
#[error("cannot replace relation {0} since it has indices")]
#[diagnostic(code(eval::replace_rel_with_indices))]
struct ReplaceRelationWithIndices(String);

impl<T: WriteTx> SessionTx<T> {
    /// Execute a mutation against a stored (or temp) relation with the
    /// query's result rows. The `force_collect` name forces old/new
    /// collection for `:returning` even when no trigger or callback wants
    /// it (upstream's convention, kept).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn execute_relation<S: Storage<WriteTx = T>>(
        &mut self,
        db: &Db<S>,
        res_iter: impl Iterator<Item = Tuple>,
        op: RelationOp,
        meta: &InputRelationHandle,
        headers: &[Symbol],
        cur_vld: ValidityTs,
        write_vld: WriteValidity,
        callback_targets: &BTreeSet<SmartString<LazyCompact>>,
        callback_collector: &mut CallbackCollector,
        trigger_depth: usize,
        force_collect: &str,
    ) -> Result<()> {
        let mut replaced_old_triggers = None;
        if op == RelationOp::Replace {
            if trigger_depth > 0 {
                bail!(ReplaceInTrigger(meta.name.to_string()))
            }
            if let Ok(old_handle) = self.get_relation(&meta.name.name) {
                if !old_handle.has_no_index() {
                    bail!(ReplaceRelationWithIndices(old_handle.name.to_string()))
                }
                if old_handle.access_level < AccessLevel::Normal {
                    bail!(InsufficientAccessLevel(
                        old_handle.name.to_string(),
                        "relation replacement".to_string(),
                        old_handle.access_level
                    ));
                }
                if old_handle.has_triggers() {
                    replaced_old_triggers = Some((
                        old_handle.put_triggers.clone(),
                        old_handle.rm_triggers.clone(),
                    ));
                }
                for trigger in &old_handle.replace_triggers {
                    let program = self.parsed_trigger(trigger, &db.fixed_rules(), cur_vld)?;
                    db.run_query(
                        self,
                        program,
                        cur_vld,
                        callback_targets,
                        callback_collector,
                        trigger_depth + 1,
                    )
                    .map_err(|err| {
                        if err.source_code().is_some() {
                            err
                        } else {
                            err.with_source_code(trigger.clone())
                        }
                    })?;
                }
                // In-transaction destruction: catalog row and keyspace go
                // together; an abort rolls both back (no deferred ranges).
                self.destroy_relation(&meta.name.name)?;
            }
        }
        let mut relation_store = if op == RelationOp::Replace || op == RelationOp::Create {
            self.create_relation(meta.clone(), KeyspaceKind::Facts)?
        } else {
            self.get_relation(&meta.name.name)?
        };
        if let Some((old_put, old_retract)) = replaced_old_triggers {
            relation_store.put_triggers = old_put;
            relation_store.rm_triggers = old_retract;
            self.write_relation_row(&relation_store)?;
        }
        // Register the touched relation's integrity constraints for the
        // pre-commit denial check (deduped by name across the transaction).
        // `Ensure`/`EnsureNot` only read; every other op mutates. Trigger
        // recursion funnels through here too, so a trigger's writes are
        // subject to constraints exactly like the user's.
        if !matches!(op, RelationOp::Ensure | RelationOp::EnsureNot) {
            self.note_constraints(&relation_store);
            // Segment soundness: every mutated relation's id is drained
            // into a watermark bump BEFORE the commit (runtime/db.rs).
            self.touched_relations.insert(relation_store.id);
        }
        let InputRelationHandle {
            metadata,
            key_bindings,
            dep_bindings,
            span,
            ..
        } = meta;

        match op {
            RelationOp::Rm | RelationOp::Delete => self.remove_from_relation(
                db,
                res_iter,
                headers,
                cur_vld,
                &write_vld,
                callback_targets,
                callback_collector,
                trigger_depth,
                &relation_store,
                metadata,
                key_bindings,
                op == RelationOp::Delete,
                force_collect,
                *span,
            )?,
            RelationOp::Ensure => self.ensure_in_relation(
                res_iter,
                headers,
                cur_vld,
                &relation_store,
                metadata,
                key_bindings,
                *span,
            )?,
            RelationOp::EnsureNot => self.ensure_not_in_relation(
                res_iter,
                headers,
                cur_vld,
                &relation_store,
                metadata,
                key_bindings,
                *span,
            )?,
            RelationOp::Update => self.update_in_relation(
                db,
                res_iter,
                headers,
                cur_vld,
                &write_vld,
                callback_targets,
                callback_collector,
                trigger_depth,
                &relation_store,
                metadata,
                key_bindings,
                force_collect,
                *span,
            )?,
            RelationOp::Create | RelationOp::Replace | RelationOp::Put | RelationOp::Insert => self
                .put_into_relation(
                    db,
                    res_iter,
                    headers,
                    cur_vld,
                    &write_vld,
                    callback_targets,
                    callback_collector,
                    trigger_depth,
                    &relation_store,
                    metadata,
                    key_bindings,
                    dep_bindings,
                    op == RelationOp::Insert,
                    force_collect,
                    *span,
                )?,
        };

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn put_into_relation<S: Storage<WriteTx = T>>(
        &mut self,
        db: &Db<S>,
        res_iter: impl Iterator<Item = Tuple>,
        headers: &[Symbol],
        cur_vld: ValidityTs,
        write_vld: &WriteValidity,
        callback_targets: &BTreeSet<SmartString<LazyCompact>>,
        callback_collector: &mut CallbackCollector,
        trigger_depth: usize,
        relation_store: &RelationHandle,
        metadata: &StoredRelationMetadata,
        key_bindings: &[Symbol],
        dep_bindings: &[Symbol],
        is_insert: bool,
        force_collect: &str,
        span: SourceSpan,
    ) -> Result<()> {
        let is_callback_target =
            callback_targets.contains(&relation_store.name) || force_collect == relation_store.name;

        if relation_store.access_level < AccessLevel::Protected {
            bail!(InsufficientAccessLevel(
                relation_store.name.to_string(),
                "row insertion".to_string(),
                relation_store.access_level
            ));
        }

        let mut key_extractors = make_extractors(
            &relation_store.metadata.keys,
            &metadata.keys,
            key_bindings,
            headers,
        )?;

        let need_to_collect = !force_collect.is_empty()
            || (!relation_store.is_temp
                && (is_callback_target || !relation_store.put_triggers.is_empty()));
        let has_indices = !relation_store.has_no_index();
        let mut new_tuples: Vec<Tuple> = vec![];
        let mut old_tuples: Vec<Tuple> = vec![];

        let val_extractors = if metadata.non_keys.is_empty() {
            make_extractors(
                &relation_store.metadata.non_keys,
                &metadata.keys,
                key_bindings,
                headers,
            )?
        } else {
            make_extractors(
                &relation_store.metadata.non_keys,
                &metadata.non_keys,
                dep_bindings,
                headers,
            )?
        };
        key_extractors.extend(val_extractors);

        // The system coordinate: engine-owned and unconditional — every
        // row this mutation writes lands in the SAME transaction, so it
        // gets the SAME system stamp regardless of what valid instant it
        // asserts.
        let stamp = self.system_stamp_routed(relation_store.is_temp);
        for tuple in res_iter {
            // The valid coordinate: an unspecified `@` defaults to the
            // transaction's own system stamp — snapshot-monotone, so a
            // retrying writer can never land its update at an instant an
            // already-committed writer has shadowed (wall-clock script
            // time is NOT monotone across retries; the stamp is). A
            // `@`-carrying mutation instead asserts the row at the
            // instant its own clause names, per row if the clause names
            // one of this row's own columns.
            let valid = write_vld.resolve(&tuple, stamp, cur_vld)?;
            let extracted: Tuple = key_extractors
                .iter()
                .map(|ex| ex.extract_data(&tuple, cur_vld))
                .try_collect()?;

            let key =
                relation_store.encode_bitemporal_key_for_store(&extracted, valid, stamp, span)?;

            // The probe below is load-bearing under SSI and UNCONDITIONAL:
            // bitemporal version keys are distinct per transaction stamp,
            // so two writers of the same fact never collide on written
            // keys — the fact-range READ this probe conflict-tracks is
            // the only thing that makes a same-fact race abort one racer
            // instead of losing an update. It also asserts absence for
            // insertion and yields the transition's old row for indices
            // and triggers — resolved AT THIS WRITE'S OWN `valid`, not
            // "ever": what this write supersedes is whatever governed the
            // instant it targets, never an unrelated later instant.
            let current = self.current_row_routed(relation_store, &extracted, valid, span)?;

            if is_insert && current.is_some() {
                bail!(TransactAssertionFailure {
                    relation: relation_store.name.to_string(),
                    key: extracted,
                    notice: "key exists in database".to_string()
                });
            }

            let val = relation_store.encode_bitemporal_val_for_store(
                &extracted,
                ClaimPolarity::Assert,
                span,
            )?;

            if need_to_collect || has_indices {
                match current {
                    Some(tup) => {
                        if has_indices && extracted != tup {
                            self.update_indices(
                                relation_store,
                                Some(&extracted),
                                Some(&tup),
                                valid,
                                stamp,
                            )?;
                        }
                        if need_to_collect {
                            old_tuples.push(tup);
                        }
                    }
                    None => {
                        if has_indices {
                            self.update_indices(
                                relation_store,
                                Some(&extracted),
                                None,
                                valid,
                                stamp,
                            )?;
                        }
                    }
                }

                if need_to_collect {
                    new_tuples.push(extracted.clone());
                }
            }

            self.put_routed(relation_store.is_temp, &key, &val)?;
        }

        if need_to_collect && !new_tuples.is_empty() {
            self.collect_mutations(
                db,
                cur_vld,
                callback_targets,
                callback_collector,
                trigger_depth,
                relation_store,
                is_callback_target,
                new_tuples,
                old_tuples,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn update_in_relation<S: Storage<WriteTx = T>>(
        &mut self,
        db: &Db<S>,
        res_iter: impl Iterator<Item = Tuple>,
        headers: &[Symbol],
        cur_vld: ValidityTs,
        write_vld: &WriteValidity,
        callback_targets: &BTreeSet<SmartString<LazyCompact>>,
        callback_collector: &mut CallbackCollector,
        trigger_depth: usize,
        relation_store: &RelationHandle,
        metadata: &StoredRelationMetadata,
        key_bindings: &[Symbol],
        force_collect: &str,
        span: SourceSpan,
    ) -> Result<()> {
        let is_callback_target =
            callback_targets.contains(&relation_store.name) || force_collect == relation_store.name;

        if relation_store.access_level < AccessLevel::Protected {
            bail!(InsufficientAccessLevel(
                relation_store.name.to_string(),
                "row update".to_string(),
                relation_store.access_level
            ));
        }

        let key_extractors = make_extractors(
            &relation_store.metadata.keys,
            &metadata.keys,
            key_bindings,
            headers,
        )?;

        let need_to_collect = !force_collect.is_empty()
            || (!relation_store.is_temp
                && (is_callback_target || !relation_store.put_triggers.is_empty()));
        let has_indices = !relation_store.has_no_index();
        let mut new_tuples: Vec<Tuple> = vec![];
        let mut old_tuples: Vec<Tuple> = vec![];

        let val_extractors = make_update_extractors(
            &relation_store.metadata.non_keys,
            &metadata.keys,
            key_bindings,
            headers,
        )?;

        let stamp = self.system_stamp_routed(relation_store.is_temp);
        for tuple in res_iter {
            let valid = write_vld.resolve(&tuple, stamp, cur_vld)?;
            let mut new_kv: Tuple = key_extractors
                .iter()
                .map(|ex| ex.extract_data(&tuple, cur_vld))
                .try_collect()?;

            let key =
                relation_store.encode_bitemporal_key_for_store(&new_kv, valid, stamp, span)?;
            // The row being updated must already exist AT THIS WRITE'S
            // OWN `valid`: a bitemporal point read of the fact, resolved
            // at that instant, yielding its logical row — the value an
            // unspecified (non-key) column carries forward is whatever
            // held at THAT instant, never a later write's belief.
            let old_kv: Tuple =
                match self.current_row_routed(relation_store, &new_kv, valid, span)? {
                    None => {
                        bail!(TransactAssertionFailure {
                            relation: relation_store.name.to_string(),
                            key: new_kv,
                            notice: "key to update does not exist".to_string()
                        })
                    }
                    Some(row) => row,
                };
            let original_val: Tuple = old_kv[relation_store.metadata.keys.len()..].to_vec();
            new_kv.reserve_exact(relation_store.arity());
            for (i, extractor) in val_extractors.iter().enumerate() {
                match extractor {
                    None => {
                        let carried = original_val.get(i).cloned().ok_or_else(|| {
                            TransactAssertionFailure {
                                relation: relation_store.name.to_string(),
                                key: new_kv.clone(),
                                notice: "stored row shorter than its schema".to_string(),
                            }
                        })?;
                        new_kv.push(carried);
                    }
                    Some(ex) => {
                        new_kv.push(ex.extract_data(&tuple, cur_vld)?);
                    }
                }
            }
            let new_val = relation_store.encode_bitemporal_val_for_store(
                &new_kv,
                ClaimPolarity::Assert,
                span,
            )?;

            if need_to_collect || has_indices {
                if has_indices {
                    self.update_indices(
                        relation_store,
                        Some(&new_kv),
                        Some(&old_kv),
                        valid,
                        stamp,
                    )?;
                }
                if need_to_collect {
                    old_tuples.push(old_kv);
                    new_tuples.push(new_kv.clone());
                }
            }

            self.put_routed(relation_store.is_temp, &key, &new_val)?;
        }

        if need_to_collect && !new_tuples.is_empty() {
            self.collect_mutations(
                db,
                cur_vld,
                callback_targets,
                callback_collector,
                trigger_depth,
                relation_store,
                is_callback_target,
                new_tuples,
                old_tuples,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn remove_from_relation<S: Storage<WriteTx = T>>(
        &mut self,
        db: &Db<S>,
        res_iter: impl Iterator<Item = Tuple>,
        headers: &[Symbol],
        cur_vld: ValidityTs,
        write_vld: &WriteValidity,
        callback_targets: &BTreeSet<SmartString<LazyCompact>>,
        callback_collector: &mut CallbackCollector,
        trigger_depth: usize,
        relation_store: &RelationHandle,
        metadata: &StoredRelationMetadata,
        key_bindings: &[Symbol],
        check_exists: bool,
        force_collect: &str,
        span: SourceSpan,
    ) -> Result<()> {
        let is_callback_target =
            callback_targets.contains(&relation_store.name) || force_collect == relation_store.name;

        if relation_store.access_level < AccessLevel::Protected {
            bail!(InsufficientAccessLevel(
                relation_store.name.to_string(),
                "row removal".to_string(),
                relation_store.access_level
            ));
        }
        let key_extractors = make_extractors(
            &relation_store.metadata.keys,
            &metadata.keys,
            key_bindings,
            headers,
        )?;

        let need_to_collect = !force_collect.is_empty()
            || (!relation_store.is_temp
                && (is_callback_target || !relation_store.rm_triggers.is_empty()));
        let has_indices = !relation_store.has_no_index();
        let mut new_tuples: Vec<Tuple> = vec![];
        let mut old_tuples: Vec<Tuple> = vec![];

        let stamp = self.system_stamp_routed(relation_store.is_temp);
        for tuple in res_iter {
            let valid = write_vld.resolve(&tuple, stamp, cur_vld)?;
            let extracted: Tuple = key_extractors
                .iter()
                .map(|ex| ex.extract_data(&tuple, cur_vld))
                .try_collect()?;
            let key =
                relation_store.encode_bitemporal_key_for_store(&extracted, valid, stamp, span)?;
            // Resolved AT THIS RETRACTION'S OWN `valid`: what it retracts
            // is whatever governed the instant it targets.
            let current = self.current_row_routed(relation_store, &extracted, valid, span)?;
            if check_exists && current.is_none() {
                bail!(TransactAssertionFailure {
                    relation: relation_store.name.to_string(),
                    key: extracted,
                    notice: "key does not exist in database".to_string()
                });
            }
            if need_to_collect || has_indices {
                if let Some(tup) = current {
                    if has_indices {
                        self.update_indices(relation_store, None, Some(&tup), valid, stamp)?;
                    }
                    if need_to_collect {
                        old_tuples.push(tup);
                    }
                }
                if need_to_collect {
                    new_tuples.push(extracted.clone());
                }
            }
            // Retraction is revision, not erasure: a Retract row at the
            // coordinate, never a physical delete.
            let val = relation_store.encode_bitemporal_val_for_store(
                &extracted,
                ClaimPolarity::Retract,
                span,
            )?;
            self.put_routed(relation_store.is_temp, &key, &val)?;
        }

        // Triggers and callbacks. Note the asymmetry preserved from the
        // original: `_new` for rm triggers carries KEY columns only.
        if need_to_collect && !new_tuples.is_empty() {
            let k_bindings = relation_store
                .metadata
                .keys
                .iter()
                .map(|k| Symbol::new(k.name.clone(), SourceSpan::default()))
                .collect_vec();
            let mut kv_bindings = k_bindings.clone();
            kv_bindings.extend(
                relation_store
                    .metadata
                    .non_keys
                    .iter()
                    .map(|k| Symbol::new(k.name.clone(), SourceSpan::default())),
            );

            if !relation_store.rm_triggers.is_empty() {
                // Cascade, bounded: firing at the ceiling is a typed
                // refusal that aborts the transaction whole — never a
                // silent stop with the mutation kept.
                if trigger_depth >= MAX_TRIGGER_CASCADE_DEPTH {
                    bail!(TriggerCascadeTooDeep(
                        relation_store.name.to_string(),
                        MAX_TRIGGER_CASCADE_DEPTH
                    ));
                }
                for trigger in &relation_store.rm_triggers {
                    let mut program = self.parsed_trigger(trigger, &db.fixed_rules(), cur_vld)?;

                    make_const_rule(&mut program, "_new", k_bindings.clone(), &new_tuples)?;
                    make_const_rule(&mut program, "_old", kv_bindings.clone(), &old_tuples)?;

                    db.run_query(
                        self,
                        program,
                        cur_vld,
                        callback_targets,
                        callback_collector,
                        trigger_depth + 1,
                    )
                    .map_err(|err| {
                        if err.source_code().is_some() {
                            err
                        } else {
                            err.with_source_code(format!("{trigger} "))
                        }
                    })?;
                }
            }

            if is_callback_target {
                let target_collector = callback_collector
                    .entry(relation_store.name.clone())
                    .or_default();
                target_collector.push((
                    CallbackOp::Rm,
                    NamedRows::new(
                        k_bindings.into_iter().map(|k| k.name.to_string()).collect(),
                        new_tuples,
                    ),
                    NamedRows::new(
                        kv_bindings
                            .into_iter()
                            .map(|k| k.name.to_string())
                            .collect(),
                        old_tuples,
                    ),
                ))
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn ensure_in_relation(
        &mut self,
        res_iter: impl Iterator<Item = Tuple>,
        headers: &[Symbol],
        cur_vld: ValidityTs,
        relation_store: &RelationHandle,
        metadata: &StoredRelationMetadata,
        key_bindings: &[Symbol],
        span: SourceSpan,
    ) -> Result<()> {
        if relation_store.access_level < AccessLevel::ReadOnly {
            bail!(InsufficientAccessLevel(
                relation_store.name.to_string(),
                "row check".to_string(),
                relation_store.access_level
            ));
        }

        let mut key_extractors = make_extractors(
            &relation_store.metadata.keys,
            &metadata.keys,
            key_bindings,
            headers,
        )?;
        let val_extractors = make_extractors(
            &relation_store.metadata.non_keys,
            &metadata.keys,
            key_bindings,
            headers,
        )?;
        key_extractors.extend(val_extractors);

        for tuple in res_iter {
            let extracted: Tuple = key_extractors
                .iter()
                .map(|ex| ex.extract_data(&tuple, cur_vld))
                .try_collect()?;

            match self.current_row_routed(
                relation_store,
                &extracted,
                crate::data::value::MAX_VALIDITY_TS,
                span,
            )? {
                None => {
                    bail!(TransactAssertionFailure {
                        relation: relation_store.name.to_string(),
                        key: extracted,
                        notice: "key does not exist in database".to_string()
                    })
                }
                Some(row) => {
                    // Logical-row comparison: the ensure asserts the fact's
                    // CURRENT columns, not any particular stored version.
                    // `:ensure` can never carry a `@` clause (refused at
                    // parse time), so "current" here always means the
                    // newest instant ever recorded, unconditionally.
                    if row != extracted {
                        bail!(TransactAssertionFailure {
                            relation: relation_store.name.to_string(),
                            key: extracted,
                            notice: "key exists in database, but value does not match".to_string()
                        })
                    }
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn ensure_not_in_relation(
        &mut self,
        res_iter: impl Iterator<Item = Tuple>,
        headers: &[Symbol],
        cur_vld: ValidityTs,
        relation_store: &RelationHandle,
        metadata: &StoredRelationMetadata,
        key_bindings: &[Symbol],
        span: SourceSpan,
    ) -> Result<()> {
        if relation_store.access_level < AccessLevel::ReadOnly {
            bail!(InsufficientAccessLevel(
                relation_store.name.to_string(),
                "row check".to_string(),
                relation_store.access_level
            ));
        }

        let key_extractors = make_extractors(
            &relation_store.metadata.keys,
            &metadata.keys,
            key_bindings,
            headers,
        )?;

        for tuple in res_iter {
            let extracted: Tuple = key_extractors
                .iter()
                .map(|ex| ex.extract_data(&tuple, cur_vld))
                .try_collect()?;
            // `:ensure_not` can never carry a `@` clause (refused at
            // parse time): "current" always means the newest instant
            // ever recorded, unconditionally.
            if self
                .current_row_routed(
                    relation_store,
                    &extracted,
                    crate::data::value::MAX_VALIDITY_TS,
                    span,
                )?
                .is_some()
            {
                bail!(TransactAssertionFailure {
                    relation: relation_store.name.to_string(),
                    key: extracted,
                    notice: "key exists in database".to_string()
                })
            }
        }
        Ok(())
    }

    /// Fire put-triggers and collect callback rows after a put/update
    /// mutation. Triggers run inside THIS transaction (atomic with the
    /// mutation); callbacks are only collected here and delivered by the
    /// `Db` after commit.
    #[allow(clippy::too_many_arguments)]
    fn collect_mutations<S: Storage<WriteTx = T>>(
        &mut self,
        db: &Db<S>,
        cur_vld: ValidityTs,
        callback_targets: &BTreeSet<SmartString<LazyCompact>>,
        callback_collector: &mut CallbackCollector,
        trigger_depth: usize,
        relation_store: &RelationHandle,
        is_callback_target: bool,
        new_tuples: Vec<Tuple>,
        old_tuples: Vec<Tuple>,
    ) -> Result<()> {
        let mut kv_bindings = relation_store
            .metadata
            .keys
            .iter()
            .map(|k| Symbol::new(k.name.clone(), SourceSpan::default()))
            .collect_vec();
        kv_bindings.extend(
            relation_store
                .metadata
                .non_keys
                .iter()
                .map(|k| Symbol::new(k.name.clone(), SourceSpan::default())),
        );

        if !relation_store.put_triggers.is_empty() {
            // Cascade, bounded: firing at the ceiling is a typed refusal
            // that aborts the transaction whole — never a silent stop with
            // the mutation kept.
            if trigger_depth >= MAX_TRIGGER_CASCADE_DEPTH {
                bail!(TriggerCascadeTooDeep(
                    relation_store.name.to_string(),
                    MAX_TRIGGER_CASCADE_DEPTH
                ));
            }
            for trigger in &relation_store.put_triggers {
                let mut program = self.parsed_trigger(trigger, &db.fixed_rules(), cur_vld)?;

                make_const_rule(&mut program, "_new", kv_bindings.clone(), &new_tuples)?;
                make_const_rule(&mut program, "_old", kv_bindings.clone(), &old_tuples)?;

                db.run_query(
                    self,
                    program,
                    cur_vld,
                    callback_targets,
                    callback_collector,
                    trigger_depth + 1,
                )
                .map_err(|err| {
                    if err.source_code().is_some() {
                        err
                    } else {
                        err.with_source_code(format!("{trigger} "))
                    }
                })?;
            }
        }

        if is_callback_target {
            let target_collector = callback_collector
                .entry(relation_store.name.clone())
                .or_default();
            let headers: Vec<String> = kv_bindings
                .into_iter()
                .map(|k| k.name.to_string())
                .collect();
            target_collector.push((
                CallbackOp::Put,
                NamedRows::new(headers.clone(), new_tuples),
                NamedRows::new(headers, old_tuples),
            ))
        }
        Ok(())
    }

    /// Maintain every index attached to `relation_store` for one row
    /// transition: `old_kv` deleted (if given), `new_kv` inserted (if
    /// given). Plain indices are handled here; manifest kinds are the
    /// operator tier's typed seam.
    fn update_indices(
        &mut self,
        relation_store: &RelationHandle,
        new_kv: Option<&[DataValue]>,
        old_kv: Option<&[DataValue]>,
        valid: ValidityTs,
        stamp: ValidityTs,
    ) -> Result<()> {
        for index in &relation_store.indices {
            match &index.kind {
                IndexKind::Plain { mapper } => {
                    let idx_handle =
                        self.get_relation(&index.relation_name(&relation_store.name))?;
                    if let Some(old) = old_kv {
                        self.plain_index_write(
                            relation_store,
                            &idx_handle,
                            mapper,
                            old,
                            ClaimPolarity::Retract,
                            valid,
                            stamp,
                        )?;
                    }
                    if let Some(new) = new_kv {
                        self.plain_index_write(
                            relation_store,
                            &idx_handle,
                            mapper,
                            new,
                            ClaimPolarity::Assert,
                            valid,
                            stamp,
                        )?;
                    }
                }
                IndexKind::Hnsw(..) | IndexKind::Fts(..) | IndexKind::Lsh { .. } => {
                    let ctx = self.manifest_index_ctx(relation_store, index)?;
                    self.apply_manifest_index(relation_store, &ctx, new_kv, old_kv)?;
                }
            }
        }
        Ok(())
    }
}

impl<T: WriteTx> SessionTx<T> {
    /// One plain-index mirror row: the base row projected through the
    /// mapper, written bitemporally at the base write's own coordinate
    /// (valid AND system, both — a `@`-carrying base write's index mirror
    /// must share its exact coordinate, not just its system stamp) with
    /// the base write's polarity — so as-of reads through the index
    /// answer exactly like as-of reads of the base.
    #[allow(clippy::too_many_arguments)]
    fn plain_index_write(
        &mut self,
        base: &RelationHandle,
        idx_handle: &RelationHandle,
        mapper: &[usize],
        row: &[DataValue],
        polarity: ClaimPolarity,
        valid: ValidityTs,
        stamp: ValidityTs,
    ) -> Result<()> {
        let span = SourceSpan::default();
        let idx_tup: Tuple = project_mapper(mapper, row, base)?;
        let key = idx_handle.encode_bitemporal_key_for_store(&idx_tup, valid, stamp, span)?;
        let val = idx_handle.encode_bitemporal_val_for_store(&idx_tup, polarity, span)?;
        // The index relation is a mutated relation in its own right: its
        // segment watermark must bump with this commit, or a served index
        // segment silently outlives the write (hostile-review finding,
        // demonstrated stale reads on `*t:by_v{..}` after a base `:put`).
        self.touched_relations.insert(idx_handle.id);
        self.put_routed(idx_handle.is_temp, &key, &val)
    }
}

/// Project a full row through a plain index's column mapper. A mapper
/// position beyond the row is a stale catalog row: a typed error, never a
/// panic (law 5; the original indexed unchecked).
fn project_mapper(
    mapper: &[usize],
    kv: &[DataValue],
    relation_store: &RelationHandle,
) -> Result<Tuple> {
    #[derive(Debug, Error, Diagnostic)]
    #[error("index mapper position {0} is out of range for relation '{1}'")]
    #[diagnostic(code(tx::stale_index_mapper))]
    struct StaleIndexMapper(usize, String);

    mapper
        .iter()
        .map(|i| {
            kv.get(*i)
                .cloned()
                .ok_or_else(|| StaleIndexMapper(*i, relation_store.name.to_string()).into())
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────
// Row extraction: result tuples → typed column values
// ─────────────────────────────────────────────────────────────────────────

enum DataExtractor {
    DefaultExtractor(Expr, NullableColType),
    IndexExtractor(usize, NullableColType),
}

impl DataExtractor {
    fn extract_data(&self, tuple: &Tuple, cur_vld: ValidityTs) -> Result<DataValue> {
        Ok(match self {
            DataExtractor::DefaultExtractor(expr, typ) => typ
                .coerce(expr.clone().eval_to_const()?, cur_vld)
                .wrap_err_with(|| format!("when processing tuple {tuple:?}"))?,
            DataExtractor::IndexExtractor(i, typ) => {
                // Law 5: a result row shorter than the header is a typed
                // error, not an index panic.
                let v = tuple.get(*i).ok_or_else(|| {
                    miette::miette!("result row {tuple:?} is shorter than the query head")
                })?;
                typ.coerce(v.clone(), cur_vld)
                    .wrap_err_with(|| format!("when processing tuple {tuple:?}"))?
            }
        })
    }
}

fn make_extractors(
    stored: &[ColumnDef],
    input: &[ColumnDef],
    bindings: &[Symbol],
    tuple_headers: &[Symbol],
) -> Result<Vec<DataExtractor>> {
    stored
        .iter()
        .map(|s| make_extractor(s, input, bindings, tuple_headers))
        .try_collect()
}

/// For `:update`: `None` for a stored dependent column the input does not
/// mention (its old value is carried over).
fn make_update_extractors(
    stored: &[ColumnDef],
    input: &[ColumnDef],
    bindings: &[Symbol],
    tuple_headers: &[Symbol],
) -> Result<Vec<Option<DataExtractor>>> {
    let input_keys: BTreeSet<_> = input.iter().map(|b| &b.name).collect();
    let mut extractors = Vec::with_capacity(stored.len());
    for col in stored.iter() {
        if input_keys.contains(&col.name) {
            extractors.push(Some(make_extractor(col, input, bindings, tuple_headers)?));
        } else {
            extractors.push(None);
        }
    }
    Ok(extractors)
}

fn make_extractor(
    stored: &ColumnDef,
    input: &[ColumnDef],
    bindings: &[Symbol],
    tuple_headers: &[Symbol],
) -> Result<DataExtractor> {
    for (inp_col, inp_binding) in input.iter().zip(bindings.iter()) {
        if inp_col.name == stored.name {
            for (idx, tuple_head) in tuple_headers.iter().enumerate() {
                if tuple_head == inp_binding {
                    return Ok(DataExtractor::IndexExtractor(idx, stored.typing.clone()));
                }
            }
        }
    }
    if let Some(expr) = &stored.default_gen {
        Ok(DataExtractor::DefaultExtractor(
            expr.clone(),
            stored.typing.clone(),
        ))
    } else {
        #[derive(Debug, Error, Diagnostic)]
        #[error("cannot make extractor for column {0}")]
        #[diagnostic(code(eval::unable_to_make_extractor))]
        struct UnableToMakeExtractor(String);
        Err(UnableToMakeExtractor(stored.name.to_string()).into())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Trigger support: the injected `_new` / `_old` constant rules
// ─────────────────────────────────────────────────────────────────────────

/// Inject a constant rule carrying the mutation's rows into a trigger's
/// program, as the `Constant` fixed rule (the same shape the parser builds
/// for `<-` bodies). `init_options` runs here, so the injected options are
/// in the proven form `Constant::run` requires.
pub(crate) fn make_const_rule(
    program: &mut InputProgram,
    rule_name: &str,
    bindings: Vec<Symbol>,
    data: &[Tuple],
) -> Result<()> {
    let rule_symbol = Symbol::new(rule_name, SourceSpan::default());
    let mut options = BTreeMap::new();
    options.insert(
        SmartString::from("data"),
        Expr::Const {
            val: DataValue::List(data.iter().map(|t| DataValue::List(t.clone())).collect()),
            span: SourceSpan::default(),
        },
    );
    let fixed_impl = Arc::new(Constant);
    fixed_impl.init_options(&mut options, SourceSpan::default())?;
    let bindings_arity = bindings.len();
    program.insert_rule(
        rule_symbol,
        InputInlineRulesOrFixed::Fixed {
            fixed: FixedRuleApply {
                fixed_handle: FixedRuleHandle::new("Constant", SourceSpan::default()),
                rule_args: vec![],
                options: Arc::new(options),
                head: bindings,
                arity: bindings_arity,
                span: SourceSpan::default(),
                fixed_impl,
            },
        },
    );
    Ok(())
}

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
        manifest: crate::engines::hnsw::HnswIndexManifest,
        filter: Option<Vec<crate::data::expr::Bytecode>>,
    },
    Fts {
        idx: RelationHandle,
        extractor: Vec<crate::data::expr::Bytecode>,
        analyzer: Arc<crate::engines::text::tokenizer::TextAnalyzer>,
    },
    Lsh {
        idx: RelationHandle,
        inv: RelationHandle,
        manifest: crate::engines::lsh::MinHashLshIndexManifest,
        extractor: Vec<crate::data::expr::Bytecode>,
        analyzer: Arc<crate::engines::text::tokenizer::TextAnalyzer>,
        perms: Arc<crate::engines::lsh::HashPermutations>,
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

    /// Parse + resolve + compile a row expression (extractor or filter)
    /// against the base relation's columns.
    fn compile_row_expr(
        base: &RelationHandle,
        src: &str,
    ) -> Result<Vec<crate::data::expr::Bytecode>> {
        let mut expr = crate::parse::parse_expressions(src, &BTreeMap::new())?;
        expr.fill_binding_indices(&Self::base_column_frame(base))?;
        expr.compile()
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
            IndexKind::Plain { .. } => {
                bail!(IndexLifecycleError(format!(
                    "index '{}' is a plain index; it has no manifest context",
                    index.name
                )))
            }
            IndexKind::Hnsw(manifest) => {
                let filter = manifest
                    .index_filter
                    .as_deref()
                    .map(|src| Self::compile_row_expr(base, src))
                    .transpose()?;
                IndexCtx::Hnsw {
                    idx,
                    manifest: manifest.clone(),
                    filter,
                }
            }
            IndexKind::Fts(manifest) => IndexCtx::Fts {
                idx,
                extractor: Self::compile_row_expr(base, &manifest.extractor)?,
                analyzer: Arc::new(manifest.tokenizer.build(&manifest.filters)?),
            },
            IndexKind::Lsh { manifest, inverse } => IndexCtx::Lsh {
                idx,
                inv: self.get_relation(&format!("{}:{}", base.name, inverse))?,
                extractor: Self::compile_row_expr(base, &manifest.extractor)?,
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
        let mut stack = vec![];
        match ctx {
            IndexCtx::Hnsw {
                idx,
                manifest,
                filter,
            } => {
                if let Some(old) = old_kv {
                    crate::engines::hnsw::hnsw_remove(&mut self.store, base, idx, old)?;
                }
                if let Some(new) = new_kv {
                    crate::engines::hnsw::hnsw_put(
                        &mut self.store,
                        manifest,
                        base,
                        idx,
                        filter.as_deref(),
                        &mut stack,
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
                    crate::engines::fts::fts_del(
                        &mut self.store,
                        old,
                        extractor,
                        &mut stack,
                        analyzer,
                        base,
                        idx,
                    )?;
                }
                if let Some(new) = new_kv {
                    crate::engines::fts::fts_put(
                        &mut self.store,
                        new,
                        extractor,
                        &mut stack,
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
                    crate::engines::lsh::lsh_del(&mut self.store, old, None, idx, inv)?;
                }
                if let Some(new) = new_kv {
                    crate::engines::lsh::lsh_put(
                        &mut self.store,
                        new,
                        extractor,
                        &mut stack,
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
        if base.is_temp {
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
        // A plain index mirrors its base's facts bitemporally; every
        // manifest index keyspace is the algorithm's own current-only
        // state.
        let kind = match &index_ref.kind {
            IndexKind::Plain { .. } => KeyspaceKind::Facts,
            _ => KeyspaceKind::AlgorithmState,
        };
        for (name, metadata) in index_metas {
            self.create_relation(
                crate::data::program::InputRelationHandle {
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
        self.write_relation_row(&base)?;

        // Backfill: index every existing base row, in bounded batches — the
        // scan borrows the store the puts need mutably, so each round
        // materializes at most BACKFILL_BATCH rows and resumes from the
        // strict successor of the last key (memcmp order: key ++ 0x00).
        const BACKFILL_BATCH: usize = 4096;
        let plain = matches!(&index_ref.kind, IndexKind::Plain { .. });
        let ctx = if plain {
            None
        } else {
            Some(self.manifest_index_ctx(&base, &index_ref)?)
        };
        let stamp = self.system_stamp_routed(base.is_temp);
        let upper = (base.id.0 + 1).to_be_bytes();
        let keys_len = base.metadata.keys.len();
        let as_of = crate::data::value::AsOf::current(crate::data::value::MAX_VALIDITY_TS);
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
            // Resume past ALL versions of the last fact: `Bot` encodes
            // above every slot byte, so this bound clears its group.
            let mut succ = last[0..keys_len].to_vec();
            succ.push(DataValue::Bot);
            lower = base.encode_partial_key_for_store(&succ).as_ref().to_vec();
            for row in &batch {
                match &ctx {
                    Some(ctx) => self.apply_manifest_index(&base, ctx, Some(row), None)?,
                    None => {
                        let IndexKind::Plain { mapper } = &index_ref.kind else {
                            unreachable!("ctx is None only for plain indexes")
                        };
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
                            row,
                            ClaimPolarity::Assert,
                            stamp,
                            stamp,
                        )?;
                    }
                }
            }
        }
        Ok(crate::runtime::db::status_ok())
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
        let all_cols: Vec<&crate::data::relation::ColumnDef> = base
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
        let metadata = crate::data::relation::StoredRelationMetadata {
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
        if let Some(src) = &cfg.index_filter {
            // Prove the filter now; the ctx re-compiles it per session.
            Self::compile_row_expr(&base, src)?;
        }
        let manifest = crate::engines::hnsw::HnswIndexManifest {
            base_relation: cfg.base_relation.clone(),
            index_name: cfg.index_name.clone(),
            vec_dim: cfg.vec_dim,
            dtype: cfg.dtype,
            vec_fields,
            distance: cfg.distance,
            ef_construction: cfg.ef_construction,
            m_neighbours: cfg.m_neighbours,
            // The standard HNSW derivations (the original's constants):
            // layer-0 keeps twice the neighbours; the level multiplier is
            // 1/ln(m) so expected layer occupancy decays geometrically.
            m_max: cfg.m_neighbours,
            m_max0: cfg.m_neighbours * 2,
            level_multiplier: 1.0 / (cfg.m_neighbours as f64).ln(),
            index_filter: cfg.index_filter.clone(),
            extend_candidates: cfg.extend_candidates,
            keep_pruned_connections: cfg.keep_pruned_connections,
        };
        let idx_meta = crate::engines::hnsw::hnsw_index_metadata(&base.metadata);
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
        Self::compile_row_expr(&base, &cfg.extractor)?;
        let manifest = crate::engines::text::FtsIndexManifest {
            base_relation: cfg.base_relation.clone(),
            index_name: cfg.index_name.clone(),
            extractor: cfg.extractor.clone(),
            tokenizer: cfg.tokenizer.clone(),
            filters: cfg.filters.clone(),
        };
        let idx_meta = crate::engines::fts::fts_index_metadata(&base.metadata);
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
        use crate::engines::lsh::{DEFAULT_PERM_SEED, HashPermutations, LshParams, Weights};
        let base = self.get_relation(&cfg.base_relation)?;
        cfg.tokenizer.build(&cfg.filters)?;
        Self::compile_row_expr(&base, &cfg.extractor)?;
        let params = LshParams::find_optimal_params(
            cfg.target_threshold.0,
            cfg.n_perm,
            &Weights(cfg.false_positive_weight.0, cfg.false_negative_weight.0),
        );
        // The signature holds exactly b*r hashes (the engine's band-chunk
        // contract); the requested n_perm is the optimizer's search budget,
        // not the drawn count.
        let n_drawn = params.b * params.r;
        let perms = HashPermutations::new(n_drawn, DEFAULT_PERM_SEED);
        let inverse: SmartString<LazyCompact> = format!("{}:inv", cfg.index_name).into();
        let manifest = crate::engines::lsh::MinHashLshIndexManifest {
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
            perms: perms.to_bytes(),
        };
        let idx_meta = crate::engines::lsh::lsh_index_metadata(&base.metadata);
        let inv_meta = crate::engines::lsh::lsh_inv_index_metadata(&base.metadata);
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
        self.write_relation_row(&base)?;
        Ok(crate::runtime::db::status_ok())
    }
}

#[cfg(test)]
mod bulk_write_tests {
    use std::collections::BTreeMap;

    use crate::data::value::DataValue;
    use crate::runtime::db::Db;
    use crate::storage::sim::SimStorage;
    use crate::storage::{ReadTx, Storage};

    fn no_params() -> BTreeMap<String, DataValue> {
        BTreeMap::new()
    }

    /// A deterministic seeded workload exercising every branch the bulk-write
    /// path's per-row key encode (`encode_bitemporal_key_for_store`) and its
    /// SSI current-row probe (`current_row`) take: fresh inserts (probe
    /// finds nothing), re-puts of existing keys (probe finds a row,
    /// `has_indices`/`need_to_collect` both false so only the probe and the
    /// write run), and removals (retraction through the same key encoder).
    fn run_seeded_workload(db: &Db<SimStorage>) {
        db.run_script("?[k, v] <- [] :create w {k => v}", no_params())
            .expect("create");
        let mut fresh = String::from("?[k, v] <- [");
        for i in 0..500i64 {
            fresh.push_str(&format!("[{i},{}],", i * 3));
        }
        fresh.push_str("] :put w {k => v}");
        db.run_script(&fresh, no_params()).expect("bulk insert");

        // Re-put 200 of those keys with a different value: exercises the
        // probe's FOUND branch (`current_row` returns `Some`) through the
        // same encoder.
        let mut updates = String::from("?[k, v] <- [");
        for i in 0..200i64 {
            updates.push_str(&format!("[{i},{}],", i * 7));
        }
        updates.push_str("] :put w {k => v}");
        db.run_script(&updates, no_params()).expect("re-put");

        // Retract 100 keys: exercises `remove_from_relation`'s use of the
        // same key encoder for a Retract row.
        let mut removals = String::from("?[k] <- [");
        for i in 400..500i64 {
            removals.push_str(&format!("[{i}],"));
        }
        removals.push_str("] :rm w {k}");
        db.run_script(&removals, no_params()).expect("bulk remove");
    }

    /// The bulk-write allocation fix (`encode_key_with_suffix` replacing
    /// the materialize-then-encode `Vec<DataValue>` in both
    /// `encode_bitemporal_key_for_store` and `current_row`) must not move a
    /// single byte of what actually lands in the store: a seeded workload's
    /// full raw scan, sorted, must be identical to what it was before the
    /// fix. `tuple.rs`'s `key_with_suffix_encoding_is_byte_identical_to_materialized`
    /// proves the encoder itself is byte-identical in isolation; this test
    /// proves it end to end, through the real mutation pipeline (extract,
    /// probe, put/remove, commit).
    #[test]
    fn bulk_write_path_store_bytes_are_unchanged_by_the_allocation_fix() {
        let db = Db::new(SimStorage::new(0xB01C_0001)).expect("db");
        run_seeded_workload(&db);

        let tx = db.storage.read_tx().expect("read tx");
        let scan: Vec<(Vec<u8>, Vec<u8>)> =
            tx.total_scan().collect::<Result<_, _>>().expect("scan");
        assert_eq!(
            scan.len(),
            802,
            "bitemporal writes are pure appends (retraction is revision, not \
             erasure): 500 initial versions + 200 re-put versions + 100 \
             retraction versions = 800 fact rows, plus 2 system rows (the id \
             counter and the relation's own catalog row)"
        );

        // Pinned against a run of this exact workload captured against the
        // pre-fix code (materialize-then-encode `Vec<DataValue>` in both
        // call sites), via `git stash` of just `relation.rs` — see the PR
        // description for the before/after diff-free comparison. Any future
        // change to the bulk-write key/value encoding must keep this equal
        // or explain, in a FormatVersion bump, why it no longer can.
        let mut hasher_input = Vec::new();
        for (k, v) in &scan {
            hasher_input.extend_from_slice(&(k.len() as u64).to_le_bytes());
            hasher_input.extend_from_slice(k);
            hasher_input.extend_from_slice(&(v.len() as u64).to_le_bytes());
            hasher_input.extend_from_slice(v);
        }
        use sha2::Digest;
        let digest = sha2::Sha256::digest(&hasher_input);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex, "befcab34181e7818f461e4a439791e0fbcd5ef615ecaac03de3c97f3a491316a",
            "store bytes for the seeded bulk workload changed"
        );
    }
}
