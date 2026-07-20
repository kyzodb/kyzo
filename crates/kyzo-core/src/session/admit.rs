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
 * peeled from runtime/mutate.rs into session/admit.rs (story #350 T2).
 *
 * Carried obligation: phase-c-parsed-substances — record at this seat.
 */

//! The mutation pipeline: how a query's result set changes a stored
//! relation.
//!
//! `execute_relation` receives the evaluated rows and the `:put`/`:rm`/…
//! operation, coerces each row through the relation's declared column
//! types, writes through the session, maintains plain/temporal indices,
//! and collects old/new rows for triggers and callbacks.
//!
//! ## Spec extend (§8/§10/§11/§12/§13/§61/§77/§90)
//!
//! Record admission monopoly under the Spec: private [`KyzoRecord`]
//! constructors, [`SemanticSurface`], evidence-stack admission law,
//! placement check, [`AdmissionCertificate`] mint call, KV-ingest refuse.
//! Bytes decode to ordered currency / ObjectRef material, never KyzoRecord
//! — store/decode modules have no mint path (compile-fail).

use std::collections::BTreeSet;

use itertools::Itertools;
use miette::{Diagnostic, Result, WrapErr, bail};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::json::NamedRows;
use crate::data::statement::{
    OntokKind, StatementBody, StatementContext, StatementPredicate, StatementSource,
    StatementSubject, StatementValue, ValidityTime,
};
use crate::rules::contract::{FixedRule, FixedRuleHandle};
use crate::rules::io::constant::Constant;
use crate::session::access::{AccessLevel, InsufficientAccessLevel};
use crate::session::catalog::{IndexKind, KeyspaceKind, RelationHandle, Residency};
use crate::session::db::{Engine, SessionTx};
use crate::session::observe::{CallbackCollector, CallbackOp};
use crate::store::time::ClaimPolarity;
use crate::store::{Storage, WriteTx};
use kyzo_model::SourceSpan;
use kyzo_model::program::expr::Expr;
use kyzo_model::program::rule::FixedRuleOptions;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::program::{
    FixedRuleApply, InputInlineRulesOrFixed, InputProgram, InputRelationHandle, RelationOp, Trivia,
    WriteValidity,
};
use kyzo_model::schema::{ColumnDef, NullableColType, StoredRelationMetadata};
use kyzo_model::value::Tuple;
use kyzo_model::value::{DataValue, ValidityTs};

use crate::store::keys::Secret;
use crate::store::open::StoreId;
use crate::store::replica::{
    AdmissionCertificate, AdmissionCertificateParts, ReplicaRefuse, mint_admission_certificate,
};

// ─────────────────────────────────────────────────────────────────────────
// Spec admission monopoly (§8/§10–§13/§61/§77/§90) — seats beside the
// carried mutation pipeline below. Store modules cannot mint KyzoRecord.
// ─────────────────────────────────────────────────────────────────────────

/// Semantic surface for indexed interpretation (§12).
///
/// [`SemanticSurface::None`] refuses index build (`NoSemanticSurface`) —
/// forcing surfaces on every record corrupts embedding meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SemanticSurface {
    /// No semantic surface — index build against this refuses.
    None,
    /// Embedding / vector surface.
    Embedding,
    /// Full-text surface.
    FullText,
    /// Lexical / token surface.
    Lexical,
}

/// Evidence coordinates required for interpreted knowledge (§10).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EvidenceCoordinates {
    /// Span start offset.
    pub start: u64,
    /// Span end offset.
    pub end: u64,
    /// Content hash of the evidence span.
    pub hash: [u8; 32],
    /// Provenance digest.
    pub provenance: [u8; 32],
}

/// Placement constraint for geography / residency (§77).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlacementConstraint {
    /// Allowed region ids (empty = unrestricted).
    pub allowed_regions: Vec<[u8; 16]>,
}

/// Privately constructed Record — admission monopoly (§8/§93).
///
/// No public constructor. Store/decode modules cannot mint this type.
///
/// Statement body (subject/predicate/value + validity-time/context/source)
/// lives as typed fields on this one record. ONTOK variants are constructions
/// over that kernel ([`crate::data::statement::construct`]), not a second
/// record type (#268 T1; seats 8/10/11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KyzoRecord {
    /// Store that admitted this record.
    store_id: StoreId,
    /// Record content digest.
    digest: [u8; 32],
    /// Optional semantic surface.
    surface: SemanticSurface,
    /// Evidence coordinates (required for interpreted knowledge).
    evidence: Option<EvidenceCoordinates>,
    /// ONTOK kind / type authority.
    kind: OntokKind,
    /// Statement subject.
    subject: StatementSubject,
    /// Statement predicate.
    predicate: StatementPredicate,
    /// Statement value.
    value: StatementValue,
    /// Validity-time scope.
    validity_time: ValidityTime,
    /// Durable context scope.
    context: StatementContext,
    /// Source artifact binding.
    source: StatementSource,
}

impl KyzoRecord {
    /// Digest.
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    /// Semantic surface.
    pub fn surface(&self) -> SemanticSurface {
        self.surface
    }

    /// Evidence coordinates, when present.
    pub fn evidence(&self) -> Option<&EvidenceCoordinates> {
        self.evidence.as_ref()
    }

    /// Owning Store.
    pub fn store_id(&self) -> StoreId {
        self.store_id
    }

    /// ONTOK kind.
    pub fn kind(&self) -> OntokKind {
        self.kind
    }

    /// Statement subject.
    pub fn subject(&self) -> &StatementSubject {
        &self.subject
    }

    /// Statement predicate.
    pub fn predicate(&self) -> &StatementPredicate {
        &self.predicate
    }

    /// Statement value.
    pub fn value(&self) -> &StatementValue {
        &self.value
    }

    /// Validity-time scope.
    pub fn validity_time(&self) -> ValidityTime {
        self.validity_time
    }

    /// Durable context scope.
    pub fn context(&self) -> &StatementContext {
        &self.context
    }

    /// Source artifact binding.
    pub fn source(&self) -> &StatementSource {
        &self.source
    }

    /// Type-entailed deterministic lowering to the closed six-dimension set.
    ///
    /// Recomputes from typed fields every call — never memoized (#268 T2).
    pub fn lower(&self) -> crate::project::dimension::RecordLowering {
        lowering::lower_record(self)
    }
}

/// Type-entailed deterministic lowering (#268 T2).
#[path = "admit_lowering.rs"]
pub(crate) mod lowering;

/// Inputs for the admission door (private mint path).
#[derive(Debug, Clone)]
pub struct AdmitRecordParts {
    /// Target Store.
    pub store_id: StoreId,
    /// Record content digest.
    pub digest: [u8; 32],
    /// Semantic surface.
    pub surface: SemanticSurface,
    /// Evidence coordinates for interpreted knowledge.
    pub evidence: Option<EvidenceCoordinates>,
    /// ONTOK kind / type authority.
    pub kind: OntokKind,
    /// Full typed statement body (subject/predicate/value + time/context/source).
    pub statement: StatementBody,
    /// Placement constraint (refuse-before-write).
    pub placement: PlacementConstraint,
    /// Region this write would land in.
    pub write_region: [u8; 16],
    /// Whether any indexed key column carries Secret-class material.
    pub secret_in_indexed_key: Option<Secret>,
    /// True when the ingest path is raw KV-as-truth (§17/§90).
    pub kv_as_truth: bool,
    /// True when the row is chunk-shaped rather than typed evidence (§11).
    pub chunk_shaped: bool,
    /// Certificate parts to mint at successful admission.
    pub certificate: AdmissionCertificateParts,
}

/// Session-door admission refuses (not StoreRefuse / EngineRefuse).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum AdmitRefuse {
    /// Interpreted knowledge without evidence coordinates (§10).
    #[error("MissingEvidenceCoordinates: interpreted knowledge requires evidence")]
    #[diagnostic(code(session::admit::missing_evidence_coordinates))]
    MissingEvidenceCoordinates,
    /// Chunk-shaped row as evidence (§11).
    #[error("ChunkIsNotEvidence: durable evidence is a typed span, not a chunk row")]
    #[diagnostic(code(session::admit::chunk_is_not_evidence))]
    ChunkIsNotEvidence,
    /// Index build against SemanticSurface::None (§12).
    #[error("NoSemanticSurface: index build against SemanticSurface::None")]
    #[diagnostic(code(session::admit::no_semantic_surface))]
    NoSemanticSurface,
    /// Secret material in an indexed key (§61).
    #[error("SecretInIndexedKey: Secret-class material illegal in indexed keys")]
    #[diagnostic(code(session::admit::secret_in_indexed_key))]
    SecretInIndexedKey {
        /// Which Secret class was found.
        kind: Secret,
    },
    /// Write landing in a forbidden geography (§77).
    #[error("PlacementForbidden: write region not in allowed set")]
    #[diagnostic(code(session::admit::placement_forbidden))]
    PlacementForbidden,
    /// KV-as-truth ingest (§17/§90).
    #[error("KvIsNotTruth: decryptability does not mint Engine meaning")]
    #[diagnostic(code(session::admit::kv_is_not_truth))]
    KvIsNotTruth,
    /// Certificate mint / replica refuse bubbled from the store seat.
    #[error(transparent)]
    #[diagnostic(transparent)]
    Replica(#[from] ReplicaRefuse),
}

/// Admit a Record through the monopoly door: placement, evidence, surface,
/// secret-key, KV-ingest checks, then mint [`AdmissionCertificate`] + [`KyzoRecord`].
pub(crate) fn admit_record(
    parts: AdmitRecordParts,
) -> Result<(KyzoRecord, AdmissionCertificate), AdmitRefuse> {
    if parts.kv_as_truth {
        return Err(AdmitRefuse::KvIsNotTruth);
    }
    if parts.chunk_shaped {
        return Err(AdmitRefuse::ChunkIsNotEvidence);
    }
    if let Some(kind) = parts.secret_in_indexed_key {
        return Err(AdmitRefuse::SecretInIndexedKey { kind });
    }
    if !parts.placement.allowed_regions.is_empty()
        && !parts
            .placement
            .allowed_regions
            .iter()
            .any(|r| r == &parts.write_region)
    {
        return Err(AdmitRefuse::PlacementForbidden);
    }
    // Interpreted knowledge (non-None surface) requires evidence coordinates.
    if parts.surface != SemanticSurface::None && parts.evidence.is_none() {
        return Err(AdmitRefuse::MissingEvidenceCoordinates);
    }
    let certificate = mint_admission_certificate(parts.certificate)?;
    let (subject, predicate, value, validity_time, context, source) =
        parts.statement.into_fields();
    let record = KyzoRecord {
        store_id: parts.store_id,
        digest: parts.digest,
        surface: parts.surface,
        evidence: parts.evidence,
        kind: parts.kind,
        subject,
        predicate,
        value,
        validity_time,
        context,
        source,
    };
    Ok((record, certificate))
}

/// Index-build law: refuse against [`SemanticSurface::None`] (§12).
pub(crate) fn admit_index_surface(surface: SemanticSurface) -> Result<(), AdmitRefuse> {
    match surface {
        SemanticSurface::None => Err(AdmitRefuse::NoSemanticSurface),
        SemanticSurface::Embedding | SemanticSurface::FullText | SemanticSurface::Lexical => Ok(()),
    }
}

/// AuthorityRecovered check at the next admission-visible write boundary (§2/§36).
pub(crate) fn refuse_if_authority_recovered(
    observed_recovery: bool,
) -> Result<(), crate::store::failure::StoreRefuse> {
    if observed_recovery {
        Err(crate::store::failure::StoreRefuse::AuthorityRecovered)
    } else {
        Ok(())
    }
}

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
        db: &Engine<S>,
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
                            err.with_source_code(trigger.program().to_string())
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
            self.write_catalog_row(&relation_store)?;
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
        db: &Engine<S>,
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
            let valid = crate::exec::expr::resolve_write_validity(
                write_vld,
                tuple.as_slice(),
                stamp,
                cur_vld,
            )?;
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
        db: &Engine<S>,
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
            let valid = crate::exec::expr::resolve_write_validity(
                write_vld,
                tuple.as_slice(),
                stamp,
                cur_vld,
            )?;
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
        db: &Engine<S>,
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
            let valid = crate::exec::expr::resolve_write_validity(
                write_vld,
                tuple.as_slice(),
                stamp,
                cur_vld,
            )?;
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
                            err.with_source_code(format!("{} ", trigger.program()))
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
                    NamedRows::try_new(
                        k_bindings.into_iter().map(|k| k.name.to_string()).collect(),
                        new_tuples,
                    )?,
                    NamedRows::try_new(
                        kv_bindings
                            .into_iter()
                            .map(|k| k.name.to_string())
                            .collect(),
                        old_tuples,
                    )?,
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
                kyzo_model::value::MAX_VALIDITY_TS,
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
                    kyzo_model::value::MAX_VALIDITY_TS,
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
        db: &Engine<S>,
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
                        err.with_source_code(format!("{} ", trigger.program()))
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
                NamedRows::try_new(headers.clone(), new_tuples)?,
                NamedRows::try_new(headers, old_tuples)?,
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
    pub(crate) fn plain_index_write(
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
    pub(crate) fn temporal_index_write(
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
    out.push(kyzo_model::value::StoredValiditySlot::new(valid).as_datavalue());
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
    let mut options = FixedRuleOptions::empty();
    options.insert(
        Symbol::new("data", SourceSpan::default()),
        Expr::Const {
            val: DataValue::List(data.iter().map(|t| DataValue::List(t.to_vec())).collect()),
            span: SourceSpan::default(),
        },
    )?;
    let options = Constant.init_options(options, SourceSpan::default())?;
    let bindings_arity = bindings.len();
    program.insert_rule(
        rule_symbol,
        InputInlineRulesOrFixed::Fixed {
            fixed: FixedRuleApply {
                fixed_handle: FixedRuleHandle::new("Constant", SourceSpan::default()),
                rule_args: vec![],
                options,
                head: bindings,
                arity: bindings_arity,
                span: SourceSpan::default(),
                trivia: Trivia::default(),
            },
        },
    );
    Ok(())
}

#[cfg(test)]
mod bulk_write_tests {
    use std::collections::BTreeMap;

    use fjall::Slice;

    use crate::session::catalog::Catalog;
    use crate::session::db::Engine;
    use crate::store::sim::SimStorage;
    use crate::store::{ReadTx, Storage};
    use kyzo_model::value::DataValue;
    use kyzo_model::value::Tuple;

    fn no_params() -> BTreeMap<String, DataValue> {
        BTreeMap::new()
    }

    fn open_engine(store: SimStorage) -> Engine<SimStorage> {
        Engine::compose(store, Catalog::new()).expect("compose engine")
    }

    /// A deterministic seeded workload exercising every branch the bulk-write
    /// path's per-row key encode (`encode_bitemporal_key_for_store`) and its
    /// SSI current-row probe (`current_row`) take: fresh inserts (probe
    /// finds nothing), re-puts of existing keys (probe finds a row,
    /// `has_indices`/`need_to_collect` both false so only the probe and the
    /// write run), and removals (retraction through the same key encoder).
    fn run_seeded_workload(db: &Engine<SimStorage>) {
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
        let db = open_engine(SimStorage::new(0xB01C_0001));
        run_seeded_workload(&db);

        let tx = db.store.read_tx().expect("read tx");
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
            .rows()
            .to_vec();
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
    /// (`WriteValidity::PerRow`, resolved once per row via
    /// `resolve_write_validity` inside `put_into_relation`'s loop), so the
    /// reserved terminal tick
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
        let db = open_engine(SimStorage::new(0xB01C_0002));
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
            out.rows().len(),
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
        let db = open_engine(SimStorage::new(0xB01C_0003));
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
            out.rows(),
            want.as_slice(),
            "the refused insert must not overwrite the existing row"
        );
    }

    /// Story #88 coverage gap: `:update`'s missing-key refusal
    /// (`update_in_relation`'s `None => bail!(... "key to update does not
    /// exist")`) was reached by no test — every existing `:update` script
    /// updates a key it just wrote. Updating an absent key must refuse.
    #[test]
    fn update_of_a_missing_key_refuses() {
        let db = open_engine(SimStorage::new(0xB01C_0004));
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
        let db = open_engine(SimStorage::new(0xB01C_0005));
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
            out.rows(),
            want.as_slice(),
            "a is updated to 99; b (omitted from the :update) carries forward as 20"
        );
    }
}

#[cfg(test)]
mod trigger_cache_battery {
    use std::collections::BTreeMap;

    use crate::data::json::NamedRows;
    use crate::session::catalog::Catalog;
    use crate::session::db::Engine;
    use crate::store::sim::SimStorage;
    use kyzo_model::value::DataValue;

    fn no_params() -> BTreeMap<String, DataValue> {
        BTreeMap::new()
    }

    fn open_engine(store: SimStorage) -> Engine<SimStorage> {
        Engine::compose(store, Catalog::new()).expect("compose engine")
    }

    fn int_rows(nr: &NamedRows) -> Vec<Vec<i64>> {
        let mut out: Vec<Vec<i64>> = nr
            .rows()
            .iter()
            .map(|r| r.iter().map(|v| v.get_int().expect("int")).collect())
            .collect();
        out.sort();
        out
    }

    /// Two `on put` triggers on one relation fire in ONE session, and each runs
    /// its own program (the trigger-parse cache must key by source). Also proves
    /// the trigger pipeline works at all — nothing else in the tree tests it.
    #[test]
    fn rs3_two_put_triggers_fire_distinctly_in_one_session() {
        let db = open_engine(SimStorage::new(41));
        db.run_script("?[a, b] <- [[0, 0]] :create src {a => b}", no_params())
            .expect("create src");
        db.run_script("?[a, b] <- [[0, 0]] :create mirror {a => b}", no_params())
            .expect("create mirror");
        db.run_script("?[a, b] <- [[0, 0]] :create mirror2 {a => b}", no_params())
            .expect("create mirror2");
        db.run_script(
            "::set_triggers src \
             on put { ?[a, b] := _new[a, b] :put mirror {a, b} } \
             on put { ?[a, b] := _new[a, b] :put mirror2 {a, b} }",
            no_params(),
        )
        .expect("set triggers");

        db.run_script("?[a, b] <- [[1, 10], [2, 20]] :put src {a, b}", no_params())
            .expect("put fires triggers");

        let mirror = db
            .run_script("?[a, b] := *mirror[a, b]", no_params())
            .expect("mirror scan");
        assert_eq!(
            int_rows(&mirror),
            vec![vec![0, 0], vec![1, 10], vec![2, 20]],
            "first on-put trigger must mirror the new rows"
        );
        let mirror2 = db
            .run_script("?[a, b] := *mirror2[a, b]", no_params())
            .expect("mirror2 scan");
        assert_eq!(
            int_rows(&mirror2),
            vec![vec![0, 0], vec![1, 10], vec![2, 20]],
            "second on-put trigger must run ITS program, not a cache-collided one"
        );
    }

    /// `:replace` atomically clears the old rows and inserts the new set,
    /// inside one transaction — the kernel `del_range` and the puts commit
    /// together.
    #[test]
    fn replace_is_atomic_clear_and_insert() {
        let db = open_engine(SimStorage::new(3));
        db.run_script(
            "?[a, b] <- [[1, 2], [2, 3], [3, 4]] :create edge {a, b}",
            no_params(),
        )
        .expect("create");
        db.run_script("?[a, b] <- [[9, 9]] :replace edge {a, b}", no_params())
            .expect("replace");
        let out = db
            .run_script("?[a, b] := *edge[a, b]", no_params())
            .expect("scan");
        // The old three rows are gone; only the replacement survives.
        assert_eq!(int_rows(&out), vec![vec![9, 9]]);
    }
}
