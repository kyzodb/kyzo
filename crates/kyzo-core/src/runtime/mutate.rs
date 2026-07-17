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
 * - **Triggers are parsed typed substances, not source** (`Trigger` in
 *   `runtime/relation.rs`): the catalog stores each trigger's provenance
 *   source, but the substance is the already-parsed `InputProgram`, lifted
 *   ONCE at the store boundary (`set_relation_triggers`) and rebuilt at
 *   catalog decode from the same fixed context — never re-parsed at fire
 *   time. Firing clones `trigger.program()` directly. A trigger's program
 *   is `cur_vld`-free by construction (it parses under a fixed sentinel at
 *   both store and decode, since decode has no session context), so the
 *   durable substance reproduces exactly and a source that would fail its
 *   own parse can never be stored.
 * - **Index maintenance is a typed seam.** Every index kind is maintained
 *   here, resolved BY REFERENCE through the catalog (the landed `IndexRef`
 *   model — no embedded handle copies): plain projection indices and
 *   temporal posting indices directly, manifest kinds (HNSW/FTS/LSH)
 *   through `apply_manifest_index`'s per-engine put/del hooks.
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

use fjall::Slice;
use itertools::Itertools;
use miette::{Diagnostic, Result, WrapErr, bail};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::bitemporal::ClaimPolarity;
use crate::data::expr::Expr;
use crate::data::program::{
    FixedRuleApply, InputInlineRulesOrFixed, InputProgram, InputRelationHandle, RelationOp, Trivia,
    WriteValidity,
};
use crate::data::relation::{ColumnDef, NullableColType, StoredRelationMetadata};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::value::{DataValue, ValidityTs};
use crate::data::value::{Tuple, TupleT};
use crate::fixed_rule::utilities::Constant;
use crate::fixed_rule::{FixedRule, FixedRuleHandle, NamedRows};
use crate::runtime::callback::{CallbackCollector, CallbackOp};
use crate::runtime::db::{Db, SessionTx};
use crate::runtime::relation::{
    AccessLevel, IndexKind, IndexRef, InsufficientAccessLevel, KeyspaceKind, RelationHandle,
    Residency,
};
use crate::storage::{Storage, WriteTx};
use crate::data::value::data_value_any;

#[derive(Debug, Error, Diagnostic)]
#[error("Assertion failure for {key:?} of {relation}: {notice}")]
#[diagnostic(code(transact::assertion_failure))]
pub(crate) struct TransactAssertionFailure {
    relation: String,
    key: Tuple,
    notice: String,
}

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
                // A `:replace` preserves the relation's put/rm triggers
                // across the swap (they are carried onto the fresh handle
                // below); the replace triggers fire now, once, against the
                // pre-swap handle.
                if old_handle.has_triggers() {
                    replaced_old_triggers = Some((
                        old_handle.put_triggers.clone(),
                        old_handle.rm_triggers.clone(),
                    ));
                }
                for trigger in &old_handle.replace_triggers {
                    // The trigger substance is already parsed — fire the
                    // stored program directly, never a re-parse of source.
                    let program = trigger.program().clone();
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
                            err.with_source_code(trigger.source().to_string())
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
            // into a generation bump BEFORE the commit (runtime/db.rs).
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
            || (matches!(relation_store.residency(), Residency::Stored)
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
        let stamp = self.system_stamp_routed(relation_store.residency());
        for tuple in res_iter {
            // The valid coordinate: an unspecified `@` defaults to the
            // transaction's own system stamp — snapshot-monotone, so a
            // retrying writer can never land its update at an instant an
            // already-committed writer has shadowed (wall-clock script
            // time is NOT monotone across retries; the stamp is). A
            // `@`-carrying mutation instead asserts the row at the
            // instant its own clause names, per row if the clause names
            // one of this row's own columns.
            let valid = write_vld.resolve(tuple.as_slice(), stamp, cur_vld)?;
            let extracted: Tuple = key_extractors
                .iter()
                .map(|ex| ex.extract_data(&tuple, cur_vld))
                .try_collect()?;

            let key = relation_store.encode_bitemporal_key_for_store(
                extracted.as_slice(),
                valid,
                stamp,
                span,
            )?;

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
            let current =
                self.current_row_routed(relation_store, extracted.as_slice(), valid, span)?;

            if is_insert && current.is_some() {
                bail!(TransactAssertionFailure {
                    relation: relation_store.name.to_string(),
                    key: extracted,
                    notice: "key exists in database".to_string()
                });
            }

            let val = relation_store.encode_bitemporal_val_for_store(
                extracted.as_slice(),
                ClaimPolarity::Assert,
                span,
            )?;

            if need_to_collect || has_indices {
                match current {
                    Some(tup) => {
                        if has_indices && extracted != tup {
                            self.update_indices(
                                relation_store,
                                Some(extracted.as_slice()),
                                Some(tup.as_slice()),
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
                                Some(extracted.as_slice()),
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

            self.put_routed(relation_store.residency(), &key, &val)?;
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
            || (matches!(relation_store.residency(), Residency::Stored)
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

        let stamp = self.system_stamp_routed(relation_store.residency());
        for tuple in res_iter {
            let valid = write_vld.resolve(tuple.as_slice(), stamp, cur_vld)?;
            let mut new_kv: Tuple = key_extractors
                .iter()
                .map(|ex| ex.extract_data(&tuple, cur_vld))
                .try_collect()?;

            let key = relation_store.encode_bitemporal_key_for_store(
                new_kv.as_slice(),
                valid,
                stamp,
                span,
            )?;
            // The row being updated must already exist AT THIS WRITE'S
            // OWN `valid`: a bitemporal point read of the fact, resolved
            // at that instant, yielding its logical row — the value an
            // unspecified (non-key) column carries forward is whatever
            // held at THAT instant, never a later write's belief.
            let old_kv: Tuple =
                match self.current_row_routed(relation_store, new_kv.as_slice(), valid, span)? {
                    None => {
                        bail!(TransactAssertionFailure {
                            relation: relation_store.name.to_string(),
                            key: new_kv,
                            notice: "key to update does not exist".to_string()
                        })
                    }
                    Some(row) => row,
                };
            let original_val: Tuple =
                Tuple::from_vec(old_kv.as_slice()[relation_store.metadata.keys.len()..].to_vec());
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
                new_kv.as_slice(),
                ClaimPolarity::Assert,
                span,
            )?;

            if need_to_collect || has_indices {
                if has_indices {
                    self.update_indices(
                        relation_store,
                        Some(new_kv.as_slice()),
                        Some(old_kv.as_slice()),
                        valid,
                        stamp,
                    )?;
                }
                if need_to_collect {
                    old_tuples.push(old_kv);
                    new_tuples.push(new_kv.clone());
                }
            }

            self.put_routed(relation_store.residency(), &key, &new_val)?;
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
            || (matches!(relation_store.residency(), Residency::Stored)
                && (is_callback_target || !relation_store.rm_triggers.is_empty()));
        let has_indices = !relation_store.has_no_index();
        let mut new_tuples: Vec<Tuple> = vec![];
        let mut old_tuples: Vec<Tuple> = vec![];

        let stamp = self.system_stamp_routed(relation_store.residency());
        for tuple in res_iter {
            let valid = write_vld.resolve(tuple.as_slice(), stamp, cur_vld)?;
            let extracted: Tuple = key_extractors
                .iter()
                .map(|ex| ex.extract_data(&tuple, cur_vld))
                .try_collect()?;
            let key = relation_store.encode_bitemporal_key_for_store(
                extracted.as_slice(),
                valid,
                stamp,
                span,
            )?;
            // Resolved AT THIS RETRACTION'S OWN `valid`: what it retracts
            // is whatever governed the instant it targets.
            let current =
                self.current_row_routed(relation_store, extracted.as_slice(), valid, span)?;
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
                        self.update_indices(
                            relation_store,
                            None,
                            Some(tup.as_slice()),
                            valid,
                            stamp,
                        )?;
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
                extracted.as_slice(),
                ClaimPolarity::Retract,
                span,
            )?;
            self.put_routed(relation_store.residency(), &key, &val)?;
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
                    // The trigger substance is already parsed — clone the
                    // stored program and inject the mutation's rows. No
                    // fire-time re-parse of source exists any more.
                    let mut program = trigger.program().clone();

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
                            err.with_source_code(format!("{} ", trigger.source()))
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
                extracted.as_slice(),
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
                    extracted.as_slice(),
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
                // The trigger substance is already parsed — clone the
                // stored program and inject the mutation's rows. No
                // fire-time re-parse of source exists any more.
                let mut program = trigger.program().clone();

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
                        err.with_source_code(format!("{} ", trigger.source()))
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
    /// given). Plain and Temporal indices — both scan-shaped, both
    /// maintained through the same mirror-row seam below — are handled
    /// here; manifest kinds are the operator tier's typed seam.
    ///
    /// `pub(crate)`, not merely `fn`: story #62 chunk 4's read-side
    /// differential (`query/ra/temporal.rs`'s test module) drives this
    /// exact primitive directly, the same way this file's own
    /// `temporal_index_tests` already does — one write-side seam, called
    /// from wherever a test needs a base relation and its posting index
    /// to advance in lockstep, not a second hand-rolled maintenance path.
    pub(crate) fn update_indices(
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
                IndexKind::Temporal => {
                    let idx_handle =
                        self.get_relation(&index.relation_name(&relation_store.name))?;
                    // Postings mirror the base's EVENT, never a Plain-style
                    // transition. `Plain` fires both `old` (Retract) and
                    // `new` (Assert) because its mirror row is payload-
                    // mapped: the two can carry different data and land at
                    // DIFFERENT mirror keys (the mapper can include
                    // non-key columns). A posting's key is base-key-only
                    // (`temporal_posting_tuple` never looks past
                    // `row[..keys_len]`), and every call site here resolves
                    // `old_kv` at THIS WRITE'S OWN `valid` — so whenever
                    // both are `Some` (a `:put` overwrite or `:update` on
                    // an existing key), `old` and `new` compose to the
                    // IDENTICAL posting key at the IDENTICAL coordinate.
                    // Firing both would silently let the Assert clobber
                    // the Retract inside this same transaction — a wasted,
                    // SSI-tracked write, not two events (hostile-review
                    // finding, story #62). The base itself writes exactly
                    // ONE row per mutation: Assert for put/update (the
                    // prior payload just becomes an older SYS version of
                    // the same instant, never a second event), Retract for
                    // remove — so the posting mirrors exactly that one
                    // event, unconditionally on `new_kv`'s presence. This
                    // single-fire shape is a write-AMPLIFICATION invariant
                    // (content-equivalent to the old dual-fire shape under
                    // the caller invariants above, so no byte-content test
                    // can guard it): the guard is the write-count law test
                    // `temporal_index_write_count_law_holds_for_every_mutation_kind`.
                    match new_kv {
                        Some(new) => {
                            self.temporal_index_write(
                                relation_store,
                                &idx_handle,
                                new,
                                ClaimPolarity::Assert,
                                valid,
                                stamp,
                            )?;
                        }
                        None => {
                            if let Some(old) = old_kv {
                                self.temporal_index_write(
                                    relation_store,
                                    &idx_handle,
                                    old,
                                    ClaimPolarity::Retract,
                                    valid,
                                    stamp,
                                )?;
                            }
                        }
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
    /// The maintenance seam shared by every scan-shaped index kind
    /// (`Plain`, `Temporal`): write one already-composed index row
    /// bitemporally at the base write's own coordinate (valid AND system,
    /// both — a `@`-carrying base write's index mirror must share its
    /// exact coordinate, not just its system stamp) with the base write's
    /// polarity, so as-of reads through the index answer exactly like
    /// as-of reads of the base. Only the ROW composition differs between
    /// index kinds (a mapper projection for `Plain`, the
    /// leading-Validity posting shape for `Temporal`) — never the write
    /// path itself.
    fn index_write_row(
        &mut self,
        idx_handle: &RelationHandle,
        idx_tup: &[DataValue],
        polarity: ClaimPolarity,
        valid: ValidityTs,
        stamp: ValidityTs,
    ) -> Result<()> {
        let span = SourceSpan::default();
        let key = idx_handle.encode_bitemporal_key_for_store(idx_tup, valid, stamp, span)?;
        let val = idx_handle.encode_bitemporal_val_for_store(idx_tup, polarity, span)?;
        // The index relation is a mutated relation in its own right: its
        // segment generation must bump with this commit, or a served index
        // segment silently outlives the write (hostile-review finding,
        // demonstrated stale reads on `*t:by_v{..}` after a base `:put`).
        self.touched_relations.insert(idx_handle.id);
        self.put_routed(idx_handle.residency(), &key, &val)
    }

    /// One plain-index mirror row: the base row projected through the
    /// mapper.
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
        let idx_tup: Tuple = project_mapper(mapper, row, base)?;
        self.index_write_row(idx_handle, idx_tup.as_slice(), polarity, valid, stamp)
    }

    /// One posting row: the write's own valid instant as a leading data
    /// column, followed by the base relation's key columns — see
    /// [`IndexKind::Temporal`]'s doc comment for the key layout and why a
    /// `Plain` mapper cannot express this composition.
    fn temporal_index_write(
        &mut self,
        base: &RelationHandle,
        idx_handle: &RelationHandle,
        row: &[DataValue],
        polarity: ClaimPolarity,
        valid: ValidityTs,
        stamp: ValidityTs,
    ) -> Result<()> {
        let idx_tup = temporal_posting_tuple(base, row, valid)?;
        self.index_write_row(idx_handle, idx_tup.as_slice(), polarity, valid, stamp)
    }
}

/// A row shorter than the base relation's own key arity reaching temporal
/// index composition. Nothing today can produce one — `update_indices`'s
/// `old_kv`/`new_kv` are always full logical rows, and backfill slices
/// exactly `keys_len` columns off the base's own stored keys — but this
/// stays a typed refusal rather than an indexing panic (Law 5), the same
/// posture as `project_mapper`'s `StaleIndexMapper`.
#[derive(Debug, Error, Diagnostic)]
#[error("temporal index row for '{0}' is shorter than the base relation's key arity")]
#[diagnostic(code(tx::short_temporal_index_row))]
struct ShortTemporalIndexRow(String);

/// The temporal posting index's key composer: `[Validity(valid) as a
/// leading data column][base key columns…]`. The leading column is the
/// write's OWN coordinate — never a position in `row` — which is exactly
/// what a `Plain` mapper (a permutation of positions already in the row)
/// cannot express.
fn temporal_posting_tuple(
    base: &RelationHandle,
    row: &[DataValue],
    valid: ValidityTs,
) -> Result<Tuple> {
    let keys_len = base.metadata.keys.len();
    if row.len() < keys_len {
        bail!(ShortTemporalIndexRow(base.name.to_string()));
    }
    let mut out = Tuple::with_capacity(1 + keys_len);
    out.push(crate::data::value::StoredValiditySlot::new(valid).as_datavalue());
    out.extend(row[..keys_len].iter().cloned());
    Ok(out)
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
            val: DataValue::List(data.iter().map(|t| DataValue::List(t.to_vec())).collect()),
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
                trivia: Trivia::default(),
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
        filter: Option<Expr>,
    },
    Fts {
        idx: RelationHandle,
        extractor: Expr,
        analyzer: Arc<crate::engines::text::tokenizer::TextAnalyzer>,
    },
    Lsh {
        idx: RelationHandle,
        inv: RelationHandle,
        manifest: crate::engines::lsh::MinHashLshIndexManifest,
        extractor: Expr,
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
    fn compile_row_expr(base: &RelationHandle, src: &str) -> Result<Expr> {
        let mut expr = crate::parse::parse_expressions(src, &BTreeMap::new())?;
        expr.fill_binding_indices(&Self::base_column_frame(base))?;
        Ok(expr)
    }

    /// Bind an already-parsed row extractor ([`crate::parse::sys::FtsIndexConfig::extractor`]
    /// / the manifest's stored typed substance) to the base column frame.
    /// The extractor is never re-parsed from source at build time — it arrives typed.
    fn compile_row_extractor(
        base: &RelationHandle,
        extractor: &crate::data::expr::Expr,
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
                    crate::engines::hnsw::hnsw_remove(&mut self.store, base, idx, old)?;
                }
                if let Some(new) = new_kv {
                    crate::engines::hnsw::hnsw_put(
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
                    crate::engines::fts::fts_del(
                        &mut self.store,
                        old,
                        extractor,
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
            IndexKind::Hnsw(_) | IndexKind::Fts(_) | IndexKind::Lsh { .. } => KeyspaceKind::AlgorithmState,
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
                    let tuple = crate::data::value::decode_tuple_from_key(k, keys_len + 2)?;
                    let polarity = crate::data::bitemporal::claim_polarity_of_value(v)?;
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
            return Ok(crate::runtime::db::status_ok());
        }

        // Backfill: index every existing base row, in bounded batches — the
        // scan borrows the store the puts need mutably, so each round
        // materializes at most BACKFILL_BATCH rows and resumes from the
        // strict successor of the last key (memcmp order: key ++ 0x00).
        let plain = matches!(&index_ref.kind, IndexKind::Plain { .. });
        let ctx = if plain {
            None
        } else {
            Some(self.manifest_index_ctx(&base, &index_ref)?)
        };
        let stamp = self.system_stamp_routed(base.residency());
        let upper = (base.id.raw() + 1).to_be_bytes();
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
                match &ctx {
                    Some(ctx) => {
                        self.apply_manifest_index(&base, ctx, Some(row.as_slice()), None)?
                    }
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
                            row.as_slice(),
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

    /// `::temporal index create` — issue #62's transposed event-posting
    /// index: opt-in per relation, no column choice (unlike `::index
    /// create`, a posting's whole identity is the base's own key, always
    /// — see [`IndexKind::Temporal`]). The stored posting rows are the
    /// write's own valid instant as a leading column, followed by the
    /// base relation's key columns.
    pub(crate) fn create_temporal_index(&mut self, rel: &str, idx_name: &str) -> Result<NamedRows> {
        let base = self.get_relation(rel)?;
        let mut keys = Vec::with_capacity(1 + base.metadata.keys.len());
        keys.push(crate::data::relation::ColumnDef {
            name: SmartString::from(crate::runtime::relation::TEMPORAL_POSTING_LEADING_COLUMN),
            typing: crate::data::relation::NullableColType {
                coltype: crate::data::relation::ColType::Validity,
                nullable: false,
            },
            default_gen: None,
        });
        keys.extend(base.metadata.keys.iter().cloned());
        let metadata = crate::data::relation::StoredRelationMetadata {
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
        let manifest = crate::engines::hnsw::HnswIndexManifest::admit(
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
        Self::compile_row_extractor(&base, &cfg.extractor)?;
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
            perms: crate::engines::lsh::LshPermutationBytes(perms.to_bytes()),
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

    use fjall::Slice;

    use crate::data::value::DataValue;
    use crate::data::value::Tuple;
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
        let scan: Vec<(Slice, Slice)> = tx.total_scan().collect::<Result<_, _>>().expect("scan");
        assert_eq!(
            scan.len(),
            802,
            "bitemporal writes are pure appends (retraction is revision, not \
             erasure): 500 initial versions + 200 re-put versions + 100 \
             retraction versions = 800 fact rows, plus 2 system rows (the id \
             counter and the relation's own catalog row)"
        );

        // MEANING ANCHOR. Before pinning the raw bytes, prove they carry
        // the correct v5 content by DECODING the store back through the
        // public query path and checking the workload's current state:
        // keys 0..200 hold `i*7` (re-put), keys 200..400 hold `i*3`
        // (initial), keys 400..500 are retracted (absent). If the
        // key/value encoding were wrong, the bytes could still hash to a
        // stable-but-meaningless value; this makes the pin a witness over
        // format-CORRECT bytes, not an implementation snapshot.
        let live = db
            .run_script("?[k, v] := *w{k, v}", no_params())
            .expect("scan back")
            .rows;
        assert_eq!(live.len(), 400, "200 re-put + 200 untouched, 100 retracted");
        let mut by_key: std::collections::BTreeMap<i64, i64> = std::collections::BTreeMap::new();
        for row in &live {
            by_key.insert(row[0].get_int().unwrap(), row[1].get_int().unwrap());
        }
        assert_eq!(by_key.get(&0), Some(&0)); // re-put i*7 = 0
        assert_eq!(by_key.get(&1), Some(&7)); // re-put 1*7
        assert_eq!(by_key.get(&199), Some(&(199 * 7)));
        assert_eq!(by_key.get(&200), Some(&(200 * 3))); // untouched i*3
        assert_eq!(by_key.get(&399), Some(&(399 * 3)));
        assert_eq!(by_key.get(&450), None); // retracted

        // The whole-store byte fingerprint: a drift witness over the v5
        // canonical key+value format (independently pinned by the value
        // round-trip/order laws and `number::format_v1_golden_vectors`).
        // A change to the bulk-write key/value encoding must keep this equal
        // or land a FormatVersion bump explaining why it cannot.
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
        // Regenerated for #299 T5: the 800 fact rows and the id-counter row
        // are byte-identical; only the single catalog row moved, because the
        // `RelationHandle` wire format lost its redundant `is_temp` field
        // (residency is now derived from the name). The meaning anchor above
        // proves the fact key/value encoding is unchanged.
        assert_eq!(
            hex, "6babc59f2f44b9f8a2b21e08295f7c35da2cd25c41fba44252584ccda6f20b3c",
            "store bytes for the seeded bulk workload changed"
        );
    }

    /// A per-row `@` clause's coordinate comes out of the row's own data
    /// (`WriteValidity::PerRow`, resolved once per row inside
    /// `put_into_relation`'s loop), so the reserved terminal tick
    /// (`i64::MAX`, issue #62's ruling) can only be caught here, at
    /// runtime, when the offending row is actually reached — parse time
    /// only proved `@ ts` names one of the mutation's own output columns,
    /// nothing about the values that column will hold. This seeds one
    /// well-formed row ahead of the offending one to prove the whole
    /// mutation refuses, not just the bad row: `put_into_relation` writes
    /// straight into the (uncommitted) write transaction as it iterates,
    /// so "no partial write" is a property of `run_script` never
    /// committing that transaction on error, not of the loop stopping
    /// early.
    #[test]
    fn per_row_write_validity_at_terminal_instant_refuses_whole_mutation() {
        let db = Db::new(SimStorage::new(0xB01C_0002)).expect("db");
        db.run_script("?[k, v] <- [] :create w3 {k => v}", no_params())
            .expect("create");

        let err = db
            .run_script(
                &format!(
                    "?[k, v, ts] <- [[1, 'a', 100], [2, 'b', {}]] :put w3 {{k => v}} @ ts",
                    i64::MAX
                ),
                no_params(),
            )
            .expect_err("row 2's coordinate is the reserved terminal tick");
        assert!(err.to_string().contains("reserved"), "got: {err}");

        let out = db
            .run_script("?[k, v] := *w3{k, v}", no_params())
            .expect("read back");
        assert_eq!(
            out.rows.len(),
            0,
            "the refused mutation must not commit row 1 either — the write \
             transaction that reached the reserved instant on row 2 was never \
             committed"
        );
    }

    /// Story #88 coverage gap: `:insert`'s duplicate-key refusal
    /// (`put_into_relation`'s `is_insert && current.is_some()` bail,
    /// `TransactAssertionFailure` "key exists in database") was reached by
    /// no test anywhere in the tree — `:put` always passes `is_insert =
    /// false`, so the whole assertion-on-existing-key branch ran zero times
    /// in every suite run. A fresh `:insert` succeeds; a second `:insert` of
    /// the SAME key must refuse, and (like every refused mutation) commit
    /// nothing — the first row's value stays what the successful insert
    /// wrote, not the value the refused one tried to place.
    #[test]
    fn insert_of_an_existing_key_refuses_and_commits_nothing() {
        let db = Db::new(SimStorage::new(0xB01C_0003)).expect("db");
        db.run_script("?[k, v] <- [] :create wi {k => v}", no_params())
            .expect("create");
        db.run_script("?[k, v] <- [[1, 10]] :insert wi {k => v}", no_params())
            .expect("first insert of a fresh key succeeds");

        let err = db
            .run_script("?[k, v] <- [[1, 999]] :insert wi {k => v}", no_params())
            .expect_err("re-inserting an existing key must refuse");
        assert!(
            err.to_string().contains("key exists in database"),
            "expected the duplicate-key assertion failure, got: {err}"
        );

        let out = db
            .run_script("?[k, v] := *wi{k, v}", no_params())
            .expect("read back");
        let want: Vec<Tuple> = vec![Tuple::from_vec(vec![
            DataValue::from(1),
            DataValue::from(10),
        ])];
        assert_eq!(
            out.rows, want,
            "the refused insert must not overwrite the existing row"
        );
    }

    /// Story #88 coverage gap: `:update`'s missing-key refusal
    /// (`update_in_relation`'s `None => bail!(... "key to update does not
    /// exist")`) was reached by no test — every existing `:update` script
    /// updates a key it just wrote. Updating an absent key must refuse.
    #[test]
    fn update_of_a_missing_key_refuses() {
        let db = Db::new(SimStorage::new(0xB01C_0004)).expect("db");
        db.run_script("?[k, v] <- [] :create wu {k => v}", no_params())
            .expect("create");
        db.run_script("?[k, v] <- [[1, 10]] :put wu {k => v}", no_params())
            .expect("seed one key");

        let err = db
            .run_script("?[k, v] <- [[2, 20]] :update wu {k => v}", no_params())
            .expect_err("updating a key that does not exist must refuse");
        assert!(
            err.to_string().contains("key to update does not exist"),
            "expected the missing-key update refusal, got: {err}"
        );
    }

    /// Story #88 coverage gap: `:update`'s value-CARRY-FORWARD branch
    /// (`make_update_extractors` returning `None` for a stored non-key
    /// column the `:update` clause omits, and `update_in_relation` pushing
    /// the row's ORIGINAL value for it) was reached by no test — every
    /// existing `:update` names every non-key column, so the `Some` arm
    /// always won and the carry-forward path never ran. Here a two-value
    /// relation is updated naming only ONE of its two non-key columns; the
    /// omitted one must retain its prior stored value, untouched.
    #[test]
    fn update_carries_forward_an_omitted_non_key_column() {
        let db = Db::new(SimStorage::new(0xB01C_0005)).expect("db");
        db.run_script("?[k, a, b] <- [] :create wc {k => a, b}", no_params())
            .expect("create");
        db.run_script(
            "?[k, a, b] <- [[1, 10, 20]] :put wc {k => a, b}",
            no_params(),
        )
        .expect("seed one full row");

        // Update naming only `a` (omitting `b`): b must carry forward as 20.
        db.run_script("?[k, a] <- [[1, 99]] :update wc {k => a}", no_params())
            .expect("partial update succeeds");

        let out = db
            .run_script("?[k, a, b] := *wc{k, a, b}", no_params())
            .expect("read back");
        let want: Vec<Tuple> = vec![Tuple::from_vec(vec![
            DataValue::from(1),
            DataValue::from(99),
            DataValue::from(20),
        ])];
        assert_eq!(
            out.rows, want,
            "a is updated to 99; b (omitted from the :update) carries forward as 20"
        );
    }
}

/// Issue #62's transposed event-posting index — the write side only (the
/// read-side RA operator that serves window/stab queries over these
/// postings is a separate chunk, see `IndexKind::Temporal`'s doc comment).
///
/// These tests drive `SessionTx` directly rather than through
/// `Db::run_script`: `::temporal index create` has no parsed KyzoScript
/// surface yet (the grammar and `SysOp` dispatch live in `parse/sys.rs`
/// and `runtime/db.rs`, both outside this chunk's file scope — see the
/// landing report), and `ClaimPolarity::Erase` (a system-time correction)
/// has no scripted write surface at all today, in or out of this scope.
/// Every function called here (`create_temporal_index`, `update_indices`,
/// `temporal_index_write`) is the exact same code the eventual parsed
/// surface and correction mechanism would call.
#[cfg(test)]
mod temporal_index_tests {
    use std::cmp::Reverse;

    use super::*;
    use crate::data::relation::ColType;
    use crate::data::value::{StoredValiditySlot, ValidityTs};
    use crate::runtime::db::ScriptOptions;
    use crate::storage::ReadTx;
    use crate::storage::sim::SimStorage;

    fn vts(t: i64) -> ValidityTs {
        ValidityTs::from_raw(t)
    }

    fn col(name: &str) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            typing: NullableColType {
                coltype: ColType::Int,
                nullable: false,
            },
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

    fn open_session(db: &Db<SimStorage>) -> SessionTx<<SimStorage as Storage>::WriteTx> {
        SessionTx::new_write(db.storage.write_tx().unwrap(), ScriptOptions::default())
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
            .encode_bitemporal_key_for_store(&row, vts(valid), vts(sys), span)
            .unwrap();
        let val = base
            .encode_bitemporal_val_for_store(&row, polarity, span)
            .unwrap();
        stx.put_routed(Residency::Stored, &key, &val).unwrap();
        match polarity {
            ClaimPolarity::Assert => {
                let old = reasserts_existing.then_some(row.as_slice());
                stx.update_indices(base, Some(&row), old, vts(valid), vts(sys))
                    .unwrap();
            }
            ClaimPolarity::Retract => {
                stx.update_indices(base, None, Some(&row), vts(valid), vts(sys))
                    .unwrap();
            }
            ClaimPolarity::Erase => {
                stx.temporal_index_write(
                    base,
                    idx_handle,
                    &row,
                    ClaimPolarity::Erase,
                    vts(valid),
                    vts(sys),
                )
                .unwrap();
            }
        }
    }

    /// One decoded posting row, in the form every assertion below compares
    /// against: `(leading valid ts, base key, tail valid ts, tail sys ts,
    /// polarity)`.
    type DecodedPosting = (i64, i64, i64, i64, ClaimPolarity);

    fn scan_postings(tx: &impl ReadTx, idx_handle: &RelationHandle) -> Vec<DecodedPosting> {
        let lower: Vec<u8> = Tuple::default()
            .encode_as_key(idx_handle.id)
            .as_ref()
            .to_vec();
        let upper = (idx_handle.id.raw() + 1).to_be_bytes().to_vec();
        tx.range_scan(&lower, &upper)
            .map(|r| {
                let (k, v) = r.expect("posting row decodes cleanly");
                let tup = crate::data::value::decode_tuple_from_key(&k, 4)
                    .expect("posting key decodes cleanly");
                let leading = match &tup.as_slice()[0] {
                    DataValue::Validity(vv) => vv.ts_micros(),
                    other @ (data_value_any!()) => panic!("expected the leading Validity column, got {other:?}"),
                };
                let key_col = tup[1].get_int().expect("int base key column");
                let tail_valid = match &tup.as_slice()[2] {
                    DataValue::Validity(vv) => vv.ts_micros(),
                    other @ (data_value_any!()) => panic!("expected the tail valid slot, got {other:?}"),
                };
                let tail_sys = match &tup.as_slice()[3] {
                    DataValue::Validity(vv) => vv.ts_micros(),
                    other @ (data_value_any!()) => panic!("expected the tail sys slot, got {other:?}"),
                };
                let polarity = crate::data::bitemporal::claim_polarity_of_value(&v)
                    .expect("posting value decodes cleanly");
                (leading, key_col, tail_valid, tail_sys, polarity)
            })
            .collect()
    }

    /// One decoded BASE row, in the SAME `DecodedPosting` shape as
    /// [`scan_postings`] (a base row's own valid instant fills both the
    /// "leading" and "tail valid" fields), so the two scans compare
    /// directly for the bijection tests below. Every base relation in
    /// this module has exactly one Int key column.
    fn scan_base_rows(tx: &impl ReadTx, base: &RelationHandle) -> Vec<DecodedPosting> {
        let lower: Vec<u8> = Tuple::default().encode_as_key(base.id).as_ref().to_vec();
        let upper = (base.id.raw() + 1).to_be_bytes().to_vec();
        tx.range_scan(&lower, &upper)
            .map(|r| {
                let (k, v) = r.expect("base row decodes cleanly");
                let tup = crate::data::value::decode_tuple_from_key(&k, 3)
                    .expect("base key decodes cleanly");
                let key_col = tup[0].get_int().expect("int base key column");
                let valid = match &tup.as_slice()[1] {
                    DataValue::Validity(vv) => vv.ts_micros(),
                    other @ (data_value_any!()) => panic!("expected the valid slot, got {other:?}"),
                };
                let sys = match &tup.as_slice()[2] {
                    DataValue::Validity(vv) => vv.ts_micros(),
                    other @ (data_value_any!()) => panic!("expected the sys slot, got {other:?}"),
                };
                let polarity = crate::data::bitemporal::claim_polarity_of_value(&v)
                    .expect("base value decodes cleanly");
                (valid, key_col, valid, sys, polarity)
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
    fn temporal_index_posting_rows_match_the_scripted_history_exactly() {
        let db = Db::new(SimStorage::new(0x7E57_0001)).expect("db");
        let mut stx = open_session(&db);
        stx.create_relation(base_input("e"), KeyspaceKind::Facts)
            .unwrap();
        stx.create_temporal_index("e", "t").unwrap();
        let base = stx.get_relation("e").unwrap();
        let idx_handle = stx.get_relation("e:t").unwrap();

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
        stx.store.commit().unwrap();

        let tx = db.storage.read_tx().unwrap();
        let mut got = scan_postings(&tx, &idx_handle);
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
            .expect("at least one posting at or after the hand-encoded key")
            .unwrap();
        assert_eq!(
            got_key,
            expected_key.as_ref(),
            "the hand-encoded posting key (leading Validity(10), then k=1, \
             then the tail) must be the literal first key on disk"
        );
    }

    /// The rebuildability law: an index attached BEFORE any base writes
    /// (maintained incrementally, one posting per write) and the SAME
    /// index attached AFTER those writes (backfilled by a full rescan of
    /// the base's stored history) must produce byte-identical posting
    /// keyspaces. Both universes create exactly the same two relations in
    /// the same order (base, then index), so their relation ids align and
    /// a literal raw-byte comparison — no id-prefix stripping — is valid.
    #[test]
    fn temporal_index_backfill_equals_incremental() {
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
        let db_a = Db::new(SimStorage::new(0xB0071)).expect("db a");
        let mut stx_a = open_session(&db_a);
        stx_a
            .create_relation(base_input("b"), KeyspaceKind::Facts)
            .unwrap();
        stx_a.create_temporal_index("b", "t").unwrap();
        let base_a = stx_a.get_relation("b").unwrap();
        let idx_a = stx_a.get_relation("b:t").unwrap();
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
        stx_a.store.commit().unwrap();

        // Universe B: index attached AFTER the same writes (backfill).
        // No index exists yet, so `write_base_event`'s `update_indices`
        // call is a no-op over an empty index list — write the base rows
        // directly instead, to keep the helper's contract ("an index is
        // attached") honest.
        let db_b = Db::new(SimStorage::new(0xB0072)).expect("db b");
        let mut stx_b = open_session(&db_b);
        stx_b
            .create_relation(base_input("b"), KeyspaceKind::Facts)
            .unwrap();
        let base_b = stx_b.get_relation("b").unwrap();
        for &(k, valid, sys, polarity, _) in &events {
            let span = SourceSpan::default();
            let row = vec![DataValue::from(k)];
            let key = base_b
                .encode_bitemporal_key_for_store(&row, vts(valid), vts(sys), span)
                .unwrap();
            let val = base_b
                .encode_bitemporal_val_for_store(&row, polarity, span)
                .unwrap();
            stx_b.put_routed(Residency::Stored, &key, &val).unwrap();
        }
        stx_b.create_temporal_index("b", "t").unwrap();
        let idx_b = stx_b.get_relation("b:t").unwrap();
        stx_b.store.commit().unwrap();

        assert_eq!(
            base_a.id, base_b.id,
            "both universes must create the same relations in the same \
             order for the raw-byte comparison below to be valid"
        );
        assert_eq!(idx_a.id, idx_b.id);

        let tx_a = db_a.storage.read_tx().unwrap();
        let tx_b = db_b.storage.read_tx().unwrap();
        let lower: Vec<u8> = Tuple::default().encode_as_key(idx_a.id).as_ref().to_vec();
        let upper = (idx_a.id.raw() + 1).to_be_bytes().to_vec();
        let raw_a: Vec<(Slice, Slice)> = tx_a
            .range_scan(&lower, &upper)
            .collect::<Result<_>>()
            .unwrap();
        let raw_b: Vec<(Slice, Slice)> = tx_b
            .range_scan(&lower, &upper)
            .collect::<Result<_>>()
            .unwrap();
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
    }

    /// Polarity/coordinate mirroring, generalized over a scripted history:
    /// every base row implies exactly one posting at the SAME (valid,
    /// sys, polarity) — not merely for the byte-verified fixture above.
    #[test]
    fn every_base_row_mirrors_to_exactly_one_posting_at_its_own_coordinate() {
        let db = Db::new(SimStorage::new(0x7E57_0002)).expect("db");
        let mut stx = open_session(&db);
        stx.create_relation(base_input("m"), KeyspaceKind::Facts)
            .unwrap();
        stx.create_temporal_index("m", "t").unwrap();
        let base = stx.get_relation("m").unwrap();
        let idx_handle = stx.get_relation("m:t").unwrap();

        let events = [
            (1i64, 5i64, 1i64, ClaimPolarity::Assert),
            (2, 6, 2, ClaimPolarity::Assert),
            (1, 15, 3, ClaimPolarity::Retract),
            (2, 6, 4, ClaimPolarity::Erase),
        ];
        for &(k, valid, sys, polarity) in &events {
            write_base_event(&mut stx, &base, &idx_handle, k, valid, sys, polarity, false);
        }
        stx.store.commit().unwrap();

        let tx = db.storage.read_tx().unwrap();
        let mut base_rows = scan_base_rows(&tx, &base);
        let mut postings = scan_postings(&tx, &idx_handle);
        base_rows.sort_by_key(decoded_posting_sort_key);
        postings.sort_by_key(decoded_posting_sort_key);
        assert_eq!(
            base_rows, postings,
            "the posting keyspace, read as (valid, key, valid, sys, \
             polarity), must be a bijection with the base's own rows"
        );
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
    fn temporal_index_production_pipeline_mirrors_one_posting_per_base_event() {
        let db = Db::new(SimStorage::new(0x7E57_0003)).expect("db");
        db.run_script("?[k, v] <- [] :create po {k => v}", BTreeMap::new())
            .expect("create");
        {
            // `::temporal index create` has no parsed surface yet (see
            // the landing report); attach it directly.
            let mut stx = open_session(&db);
            stx.create_temporal_index("po", "t").unwrap();
            stx.store.commit().unwrap();
        }

        db.run_script("?[k, v] <- [[1, 100]] :put po {k, v}", BTreeMap::new())
            .expect("fresh insert");
        // The overwrite: `current_row_routed` resolves at THIS write's own
        // valid to the prior row (`old_kv = Some`), and this write
        // supplies a new payload (`new_kv = Some`) — both `Some`, the
        // exact branch under review.
        db.run_script("?[k, v] <- [[1, 200]] :put po {k, v}", BTreeMap::new())
            .expect("overwrite");
        db.run_script("?[k, v] <- [[1, 300]] :update po {k, v}", BTreeMap::new())
            .expect("update");
        db.run_script("?[k] <- [[1]] :rm po {k}", BTreeMap::new())
            .expect("remove");

        let rtx = SessionTx::new_read(db.storage.read_tx().unwrap(), ScriptOptions::default());
        let base = rtx.get_relation("po").unwrap();
        let idx_handle = rtx.get_relation("po:t").unwrap();
        let mut base_rows = scan_base_rows(&rtx.store, &base);
        let mut postings = scan_postings(&rtx.store, &idx_handle);
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
    fn temporal_index_write_count_law_holds_for_every_mutation_kind() {
        let db = Db::new(SimStorage::new(0x7E57_0004)).expect("db");
        db.run_script("?[k, v] <- [] :create po {k => v}", BTreeMap::new())
            .expect("create");
        {
            let mut stx = open_session(&db);
            stx.create_temporal_index("po", "t").unwrap();
            stx.store.commit().unwrap();
        }

        let puts_before_all = db.storage.put_call_count();
        let dels_before_all = db.storage.del_call_count();

        let check_delta = |label: &str, puts_before: u64, dels_before: u64| {
            let puts_after = db.storage.put_call_count();
            let dels_after = db.storage.del_call_count();
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
            .expect("fresh insert");
        let (p1, d1) = check_delta("put fresh", puts_before_all, dels_before_all);

        // :put overwrite — `old_kv` AND `new_kv` both `Some`: the exact
        // branch story #62's hostile review found unguarded.
        db.run_script("?[k, v] <- [[1, 200]] :put po {k, v}", BTreeMap::new())
            .expect("overwrite");
        let (p2, d2) = check_delta("put overwrite", p1, d1);

        // :update — same both-`Some` shape as the overwrite.
        db.run_script("?[k, v] <- [[1, 300]] :update po {k, v}", BTreeMap::new())
            .expect("update");
        let (p3, d3) = check_delta("update", p2, d2);

        // :rm — `new_kv` is `None`, `old_kv` is `Some`: single-fire and
        // the dual-fire mutant agree here too (only overwrite/update
        // diverge), so this is the control showing the law holds
        // everywhere, not just where the mutant happens to differ.
        db.run_script("?[k] <- [[1]] :rm po {k}", BTreeMap::new())
            .expect("remove");
        check_delta("rm", p3, d3);
    }
}
