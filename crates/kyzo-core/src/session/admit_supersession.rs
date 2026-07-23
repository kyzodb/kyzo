/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Seat 34 — correction as supersession without overwrite.
//! Seat / #270 T4 — semantic deletion as supersession-shaped records
//! ([`SemanticDeletionKind`]): Invalidation / Tombstone / RetentionRedaction —
//! never message-delete. Prior committed bytes stay; history appends.
//!
//! A correction or semantic deletion is a **new** admitted [`KyzoRecord`] that
//! supersedes a prior by [`RecordId`]. The prior stays committed. There is no
//! rewrite / update-in-place / overwrite / message-delete door on committed
//! fact bytes. Dense [`CommitOrdinal`] advances on the successor at attach.

use kyzo_model::SourceSpan;
use kyzo_model::value::canonical::encode_owned;
use kyzo_model::value::{DataValue, ValidityTs};
use sha2::{Digest, Sha256};

use crate::data::digest::RecordContentDigest;
use crate::data::statement::{
    StatementContext, StatementSource, StatementSubject, StatementValue, ValidityTime,
};
use crate::session::catalog::RelationHandle;
use crate::session::generation::{CatalogGeneration, RelationGeneration};
use crate::store::replica::{AdmissionCertificate, CrossingStatus};
use crate::store::sweep::CommitOrdinal;
use crate::store::time::ClaimPolarity;
use crate::store::{StoreId, WriteTx};

use super::{
    AdmitRefuse, AdmittedDurableWrite, IngestShape, KyzoRecord, LiveAdmissionSeats,
    LiveCertificateInputs, Placement, RecordCore, RecordId, SemanticSurface, admit_record,
};

/// Motive of a supersession link — correction or semantic deletion (#270 T4).
///
/// Semantic deletion kinds map to [`CrossingStatus`] variants that refuse
/// live lowering. Correction has no deletion status (as-of still replays).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SupersessionKind {
    /// Seat 34 correction — successor replaces prior meaning without overwrite.
    Correction,
    /// Semantic invalidation of prior meaning.
    Invalidation,
    /// Tombstone supersession — prior not lowerable as live meaning.
    Tombstone,
    /// Retention redaction — prior redacted under retention law.
    RetentionRedaction,
}

impl SupersessionKind {
    /// Crossing status this motive seals onto the prior's federation surface.
    ///
    /// [`None`] for correction (prior remains historically readable via as-of).
    /// Semantic deletions map onto the closed [`CrossingStatus`] deletion set.
    pub fn crossing_status(self) -> Option<CrossingStatus> {
        match self {
            Self::Correction => None,
            Self::Invalidation => Some(CrossingStatus::Invalidated),
            Self::Tombstone => Some(CrossingStatus::Tombstoned),
            Self::RetentionRedaction => Some(CrossingStatus::RetentionRedacted),
        }
    }

    /// True when this motive is a semantic deletion (never message-delete).
    pub fn is_semantic_deletion(self) -> bool {
        !matches!(self, Self::Correction)
    }
}

/// Closed semantic-deletion kinds — Invalidation / Tombstone / RetentionRedaction.
///
/// Each is a supersession-shaped record admitted through this seat — never a
/// message-delete / erase-bytes door (#270 T4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SemanticDeletionKind {
    /// Invalidate prior meaning without overwrite.
    Invalidation,
    /// Tombstone the prior — not lowerable as live meaning.
    Tombstone,
    /// Retention redaction of the prior.
    RetentionRedaction,
}

impl SemanticDeletionKind {
    /// Lift into the supersession motive closed sum.
    pub fn as_supersession_kind(self) -> SupersessionKind {
        match self {
            Self::Invalidation => SupersessionKind::Invalidation,
            Self::Tombstone => SupersessionKind::Tombstone,
            Self::RetentionRedaction => SupersessionKind::RetentionRedaction,
        }
    }

    /// Crossing status sealed by this deletion kind.
    pub fn crossing_status(self) -> CrossingStatus {
        match self {
            Self::Invalidation => CrossingStatus::Invalidated,
            Self::Tombstone => CrossingStatus::Tombstoned,
            Self::RetentionRedaction => CrossingStatus::RetentionRedacted,
        }
    }
}

/// Auditable link: successor admitted record supersedes prior by identity.
///
/// Minted only when a correction or semantic deletion attaches through
/// [`seal_supersession`] / [`seal_semantic_deletion`] — never by rewriting
/// prior committed bytes or message-deleting them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Supersession {
    prior: RecordId,
    successor: RecordId,
    /// Dense CommitOrdinal of the successor at attach (seat 34).
    commit_ordinal: CommitOrdinal,
    /// Correction vs semantic-deletion motive (#270 T4).
    kind: SupersessionKind,
}

impl Supersession {
    /// Prior committed record identity — still addressable after correction.
    pub fn prior(self) -> RecordId {
        self.prior
    }

    /// Successor record identity (the correction or deletion record).
    pub fn successor(self) -> RecordId {
        self.successor
    }

    /// Dense CommitOrdinal sealed with the successor.
    pub fn commit_ordinal(self) -> CommitOrdinal {
        self.commit_ordinal
    }

    /// Motive — correction or semantic deletion kind.
    pub fn kind(self) -> SupersessionKind {
        self.kind
    }
}

/// Content digest for a correction — binds prior [`RecordId`] so the
/// successor identity cannot collide with the prior.
fn digest_correction(prior: RecordId, relation: &str, row: &[DataValue]) -> RecordContentDigest {
    let mut h = Sha256::new();
    h.update(b"kyzo.sugar.correction.v1");
    h.update(prior.as_bytes());
    h.update(relation.as_bytes());
    for v in row {
        h.update(encode_owned(v).as_bytes());
    }
    RecordContentDigest::from_digest(h.finalize().into())
}

/// Content digest for a semantic deletion — binds kind + prior so deletion
/// records cannot collide with corrections or each other.
fn digest_semantic_deletion(
    kind: SemanticDeletionKind,
    prior: RecordId,
    subject: &DataValue,
) -> RecordContentDigest {
    let mut h = Sha256::new();
    h.update(b"kyzo.semantic.deletion.v1");
    h.update(match kind {
        SemanticDeletionKind::Invalidation => b"invalidation".as_slice(),
        SemanticDeletionKind::Tombstone => b"tombstone".as_slice(),
        SemanticDeletionKind::RetentionRedaction => b"retention_redaction".as_slice(),
    });
    h.update(prior.as_bytes());
    h.update(encode_owned(subject).as_bytes());
    RecordContentDigest::from_digest(h.finalize().into())
}

/// Admit a correction: a **new** record that names `prior` as the identity
/// it supersedes. Does not touch store bytes — callers append via
/// [`append_corrected_fact`] (or an equivalent put of a **new** bitemporal
/// key). There is no path that rewrites the prior's committed key.
pub(crate) fn admit_correction(
    store_id: StoreId,
    live: &LiveCertificateInputs,
    prior: RecordId,
    relation_name: &str,
    corrected_row: &[DataValue],
    keys_len: usize,
    valid: ValidityTs,
) -> Result<(KyzoRecord, AdmissionCertificate), AdmitRefuse> {
    // One-door: same [`super::admit_relation_row_through_record`] seam as
    // sugar assert. Independence is the correction digest (binds `prior`)
    // plus the successor≠prior refuse — not a parallel admission path.
    let (record, cert) = super::admit_relation_row_through_record(
        store_id,
        live,
        relation_name,
        corrected_row,
        keys_len,
        valid,
        digest_correction(prior, relation_name, corrected_row),
    )?;
    if record.record_id() == prior {
        return Err(AdmitRefuse::SugarStatementRefuse);
    }
    Ok((record, cert))
}

/// Admit a semantic deletion record — Invalidation / Tombstone /
/// RetentionRedaction — as supersession without message-delete (#270 T4).
///
/// Uses the ONTOK Invalidation construction (the statement-kernel deletion
/// kind) with a digest that binds [`SemanticDeletionKind`] + prior. Does not
/// touch prior committed bytes.
pub(crate) fn admit_semantic_deletion(
    store_id: StoreId,
    live: &LiveCertificateInputs,
    kind: SemanticDeletionKind,
    prior: RecordId,
    subject: DataValue,
    valid: ValidityTs,
) -> Result<(KyzoRecord, AdmissionCertificate), AdmitRefuse> {
    let subject = StatementSubject::new(subject);
    let value = StatementValue::new(DataValue::Null);
    let (ontok_kind, statement) = crate::data::statement::construct::invalidation(
        subject.clone(),
        value,
        ValidityTime::instant(valid.raw()),
        StatementContext::Unscoped,
        StatementSource::unbound(),
    )
    .map_err(|_| AdmitRefuse::SugarStatementRefuse)?;
    let digest = digest_semantic_deletion(kind, prior, subject.as_value());
    let core = RecordCore::new(
        store_id,
        digest,
        SemanticSurface::None,
        None,
        ontok_kind,
        statement,
    );
    let (record, cert) = admit_record(super::AdmitRecordParts::new(
        core,
        Placement::Unrestricted,
        None,
        IngestShape::Record,
        live.clone(),
    ))?;
    if record.record_id() == prior {
        return Err(AdmitRefuse::SugarStatementRefuse);
    }
    Ok((record, cert))
}

/// Attach a correction certificate and seal the supersession link.
///
/// Advances dense [`CommitOrdinal`] on the admission spine for the
/// successor. Prior identity is retained on the link — never erased.
pub(crate) fn seal_supersession(
    seats: &LiveAdmissionSeats,
    prior: RecordId,
    record: &KyzoRecord,
    certificate: AdmissionCertificate,
) -> Result<(AdmittedDurableWrite, Supersession), AdmitRefuse> {
    seal_supersession_kind(
        seats,
        prior,
        record,
        certificate,
        SupersessionKind::Correction,
    )
}

/// Attach a semantic-deletion certificate and seal the supersession link.
///
/// Same append-only law as correction: prior stays committed; the deletion
/// is a new record. Crossing status is the kind's [`CrossingStatus`].
pub(crate) fn seal_semantic_deletion(
    seats: &LiveAdmissionSeats,
    kind: SemanticDeletionKind,
    prior: RecordId,
    record: &KyzoRecord,
    certificate: AdmissionCertificate,
) -> Result<(AdmittedDurableWrite, Supersession), AdmitRefuse> {
    seal_supersession_kind(
        seats,
        prior,
        record,
        certificate,
        kind.as_supersession_kind(),
    )
}

fn seal_supersession_kind(
    seats: &LiveAdmissionSeats,
    prior: RecordId,
    record: &KyzoRecord,
    certificate: AdmissionCertificate,
    kind: SupersessionKind,
) -> Result<(AdmittedDurableWrite, Supersession), AdmitRefuse> {
    if record.record_id() == prior {
        return Err(AdmitRefuse::SugarStatementRefuse);
    }
    seats.attach_verified(record, certificate)?;
    let commit_ordinal = seats.origin_commit()?;
    let link = Supersession {
        prior,
        successor: record.record_id(),
        commit_ordinal,
        kind,
    };
    seats.retain_supersession(link)?;
    Ok((record.durable_write_permit(), link))
}

/// Live-path correction: admit successor → seal supersession → **append**
/// a new bitemporal fact key. The prior key is not deleted, rewritten, or
/// overwritten — seat 34.
///
/// `prior` must be the [`RecordId`] of the already-committed fact this
/// correction supersedes.
pub(crate) fn append_corrected_fact(
    seats: &LiveAdmissionSeats,
    relation: &RelationHandle,
    tx: &mut impl WriteTx,
    prior: RecordId,
    corrected_row: &[DataValue],
    valid: ValidityTs,
    span: SourceSpan,
) -> Result<Supersession, miette::Report> {
    let live = seats.certificate_inputs(CatalogGeneration::from_relation(
        RelationGeneration::witness(relation.id.raw()),
    ))?;
    let (record, cert) = admit_correction(
        seats.store_id(),
        &live,
        prior,
        relation.name.as_str(),
        corrected_row,
        relation.metadata.keys.len(),
        valid,
    )?;
    let (_permit, link) = seal_supersession(seats, prior, &record, cert)?;
    let key =
        relation.encode_bitemporal_key_for_store(corrected_row, valid, tx.system_stamp(), span)?;
    let val =
        relation.encode_bitemporal_val_for_store(corrected_row, ClaimPolarity::Assert, span)?;
    // Append only: a new (valid, sys) key. Prior committed keys stay put.
    tx.put(&key, &val)?;
    Ok(link)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::admit::{LiveAdmissionSeats, admit_sugar_relation_row};
    use crate::session::catalog::{Catalog, get_relation};
    use crate::session::db::Engine;
    use crate::store::sim::SimStorage;
    use crate::store::{ReadTx, Storage};
    use kyzo_model::value::{AsOf, Tuple};
    use miette::{Result, miette};
    use std::collections::BTreeMap;

    fn no_params() -> BTreeMap<String, DataValue> {
        BTreeMap::new()
    }

    fn open_engine(store: SimStorage) -> Result<Engine<SimStorage>> {
        Ok(Engine::compose(store, Catalog::new())?)
    }

    /// Admit an original sugar row through the live seats (same door as put).
    fn admit_original(
        seats: &LiveAdmissionSeats,
        relation: &str,
        row: &[DataValue],
        keys_len: usize,
        valid: ValidityTs,
    ) -> Result<(KyzoRecord, AdmissionCertificate)> {
        let live = seats.certificate_inputs(CatalogGeneration::from_relation(
            RelationGeneration::witness(0),
        ))?;
        let (record, cert) =
            admit_sugar_relation_row(seats.store_id(), &live, relation, row, keys_len, valid)
                .map_err(|e| miette!("admit original: {e}"))?;
        seats
            .attach_verified(&record, cert.clone())
            .map_err(|e| miette!("attach: {e}"))?;
        Ok((record, cert))
    }

    #[test]
    fn correction_supersedes_by_record_id_with_dense_commit_ordinal() -> Result<()> {
        let seats = LiveAdmissionSeats::mint_genesis();
        let original_row = [DataValue::from(1i64), DataValue::from(100i64)];
        let (original, _) = admit_original(
            &seats,
            "quote",
            &original_row,
            1,
            ValidityTs::of_micros(100),
        )?;
        let prior = original.record_id();
        let prior_commit = seats.origin_commit()?;

        let live = seats.certificate_inputs(CatalogGeneration::from_relation(
            RelationGeneration::witness(0),
        ))?;
        let corrected = [DataValue::from(1i64), DataValue::from(150i64)];
        let (successor, cert) = admit_correction(
            seats.store_id(),
            &live,
            prior,
            "quote",
            &corrected,
            1,
            ValidityTs::of_micros(200),
        )
        .map_err(|e| miette!("admit correction: {e}"))?;
        let (_permit, link) = seal_supersession(&seats, prior, &successor, cert)
            .map_err(|e| miette!("seal supersession: {e}"))?;

        assert_eq!(link.prior(), prior);
        assert_eq!(link.successor(), successor.record_id());
        assert_ne!(link.prior(), link.successor());
        assert_eq!(
            link.commit_ordinal(),
            prior_commit
                .successor()
                .map_err(|e| miette!("dense successor: {e}"))?,
            "successor carries the dense CommitOrdinal after prior"
        );
        assert_eq!(seats.origin_commit()?, link.commit_ordinal());
        assert_eq!(
            seats.retained_supersessions()?,
            vec![link],
            "supersession is retained on the live admission spine"
        );
        assert_eq!(link.kind(), SupersessionKind::Correction);
        assert!(!link.kind().is_semantic_deletion());
        Ok(())
    }

    #[test]
    fn as_of_pre_correction_replays_original_exactly() -> Result<()> {
        let db = open_engine(SimStorage::new(0x2680_0004))?;
        db.run_script(
            "?[id, price] <- [[1, 100]] :create quote {id => price} @ 100",
            no_params(),
        )
        .map_err(|e| miette!("create original @100: {e}"))?;

        let original_row = [DataValue::from(1i64), DataValue::from(100i64)];
        let seats = LiveAdmissionSeats::mint_genesis();
        // Admit the create's logical prior through the correction door's
        // identity plane, then append the correction on the real store.
        let (prior_record, _) = admit_original(
            &seats,
            "quote",
            &original_row,
            1,
            ValidityTs::of_micros(100),
        )?;
        let prior = prior_record.record_id();

        let mut tx = db
            .store
            .write_tx()
            .map_err(|e| miette!("correction tx: {e}"))?;
        let handle = get_relation(&tx, "quote").map_err(|e| miette!("quote: {e}"))?;
        let corrected = [DataValue::from(1i64), DataValue::from(150i64)];
        let link = append_corrected_fact(
            &seats,
            &handle,
            &mut tx,
            prior,
            &corrected,
            ValidityTs::of_micros(200),
            SourceSpan::empty(),
        )
        .map_err(|e| miette!("append correction: {e}"))?;
        tx.commit().map_err(|e| miette!("commit correction: {e}"))?;

        assert_ne!(link.prior(), link.successor());

        // As-of valid 150: after original @100, before correction @200.
        let at_150 = db
            .run_script("?[price] := *quote{id, price @ 150}", no_params())
            .map_err(|e| miette!("as-of 150: {e}"))?;
        assert_eq!(
            at_150.rows(),
            &[Tuple::from_vec(vec![DataValue::from(100i64)])],
            "as-of pre-correction must replay the ORIGINAL value exactly"
        );

        // As-of after correction sees the superseding value.
        let at_250 = db
            .run_script("?[price] := *quote{id, price @ 250}", no_params())
            .map_err(|e| miette!("as-of 250: {e}"))?;
        assert_eq!(
            at_250.rows(),
            &[Tuple::from_vec(vec![DataValue::from(150i64)])],
            "as-of post-correction sees the successor"
        );
        Ok(())
    }

    /// GUARDIAN HUNT (#3 as-of/supersession): the CANONICAL bitemporal
    /// correction -- same valid-time, corrected value. The existing test uses
    /// distinct valid-times (a schedule of values, not a correction). Here the
    /// original and the correction share valid-time 100; the correction carries
    /// a later dense system-time (commit ordinal). As-of latest knowledge must
    /// return the CORRECTED value: the correction supersedes the stale original
    /// at the same valid-time. If as-of replays the stale value (or returns both
    /// rows for a single-valued key), history is being read wrong.
    #[test]
    fn as_of_same_valid_time_correction_supersedes_stale_value() -> Result<()> {
        let db = open_engine(SimStorage::new(0x2680_0006))?;
        db.run_script(
            "?[id, price] <- [[1, 100]] :create quote {id => price} @ 100",
            no_params(),
        )
        .map_err(|e| miette!("create original @100: {e}"))?;

        let original_row = [DataValue::from(1i64), DataValue::from(100i64)];
        let seats = LiveAdmissionSeats::mint_genesis();
        let (prior_record, _) = admit_original(
            &seats,
            "quote",
            &original_row,
            1,
            ValidityTs::of_micros(100),
        )?;
        let prior = prior_record.record_id();

        let mut tx = db
            .store
            .write_tx()
            .map_err(|e| miette!("correction tx: {e}"))?;
        let handle = get_relation(&tx, "quote").map_err(|e| miette!("quote: {e}"))?;
        let corrected = [DataValue::from(1i64), DataValue::from(150i64)];
        // SAME valid-time (100) as the original: the canonical correction.
        append_corrected_fact(
            &seats,
            &handle,
            &mut tx,
            prior,
            &corrected,
            ValidityTs::of_micros(100),
            SourceSpan::empty(),
        )
        .map_err(|e| miette!("append same-valid correction: {e}"))?;
        tx.commit().map_err(|e| miette!("commit correction: {e}"))?;

        // As-of valid 150 (latest knowledge): the correction at valid 100 must
        // win over the original at valid 100 (later system-time supersedes).
        let at_150 = db
            .run_script("?[price] := *quote{id, price @ 150}", no_params())
            .map_err(|e| miette!("as-of 150: {e}"))?;
        assert_eq!(
            at_150.rows(),
            &[Tuple::from_vec(vec![DataValue::from(150i64)])],
            "same-valid-time correction must supersede the stale value in as-of"
        );
        Ok(())
    }

    #[test]
    fn prior_committed_bytes_survive_correction_append() -> Result<()> {
        let db = open_engine(SimStorage::new(0x2680_0005))?;
        db.run_script(
            "?[id, price] <- [[1, 100]] :create quote {id => price} @ 100",
            no_params(),
        )
        .map_err(|e| miette!("create: {e}"))?;

        let rtx = db.store.read_tx().map_err(|e| miette!("read: {e}"))?;
        let before: Vec<(Vec<u8>, Vec<u8>)> = rtx
            .total_scan()
            .map(|kv| -> Result<_> {
                let (k, v) = kv.map_err(|e| miette!("kv: {e}"))?;
                Ok((k.to_vec(), v.to_vec()))
            })
            .collect::<Result<Vec<_>>>()?;
        drop(rtx);
        let before_len = before.len();

        let seats = LiveAdmissionSeats::mint_genesis();
        let original_row = [DataValue::from(1i64), DataValue::from(100i64)];
        let (prior_record, _) = admit_original(
            &seats,
            "quote",
            &original_row,
            1,
            ValidityTs::of_micros(100),
        )?;

        let mut tx = db.store.write_tx().map_err(|e| miette!("tx: {e}"))?;
        let handle = get_relation(&tx, "quote").map_err(|e| miette!("handle: {e}"))?;
        append_corrected_fact(
            &seats,
            &handle,
            &mut tx,
            prior_record.record_id(),
            &[DataValue::from(1i64), DataValue::from(150i64)],
            ValidityTs::of_micros(200),
            SourceSpan::empty(),
        )
        .map_err(|e| miette!("append: {e}"))?;
        tx.commit().map_err(|e| miette!("commit: {e}"))?;

        let rtx = db.store.read_tx().map_err(|e| miette!("read after: {e}"))?;
        let after: Vec<(Vec<u8>, Vec<u8>)> = rtx
            .total_scan()
            .map(|kv| -> Result<_> {
                let (k, v) = kv.map_err(|e| miette!("kv: {e}"))?;
                Ok((k.to_vec(), v.to_vec()))
            })
            .collect::<Result<Vec<_>>>()?;
        assert!(
            after.len() > before_len,
            "correction appends; store must grow (before={before_len}, after={})",
            after.len()
        );
        for (k, v) in &before {
            let found = after.iter().find(|(ak, _)| ak == k);
            assert!(
                found.is_some(),
                "prior committed key must still exist after correction"
            );
            assert_eq!(
                found.map(|(_, av)| av.as_slice()),
                Some(v.as_slice()),
                "prior committed value bytes must be unchanged (no overwrite)"
            );
        }
        Ok(())
    }

    #[test]
    fn no_rewrite_api_on_committed_facts() -> Result<()> {
        // Seat 34 grep-proof: correction appends; no rewrite/overwrite door.
        // Scan production surfaces only — strip this file's cfg(test) module so
        // the forbidden-needle table cannot match itself.
        let supersession_prod = include_str!("admit_supersession.rs")
            .split("#[cfg(test)]")
            .next()
            .ok_or_else(|| miette!("production supersession surface"))?;
        let sources = [
            include_str!("admit.rs"),
            supersession_prod,
            include_str!("../store/time.rs"),
            include_str!("../store/tx.rs"),
        ];
        // Split so this test body never contains a contiguous forbidden ident.
        let forbidden: [String; 9] = [
            ["fn rewr", "ite_committed"].concat(),
            ["fn overwr", "ite_fact"].concat(),
            ["fn overwr", "ite_committed"].concat(),
            ["fn muta", "te_committed"].concat(),
            ["fn update_in", "_place"].concat(),
            ["fn rewr", "ite_fact"].concat(),
            ["fn message_de", "lete"].concat(),
            ["fn delete_mes", "sage"].concat(),
            ["fn erase_comm", "itted_bytes"].concat(),
        ];
        for src in sources {
            for needle in &forbidden {
                assert!(
                    !src.contains(needle.as_str()),
                    "forbidden rewrite/message-delete API `{needle}` must not exist on committed facts"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn semantic_deletion_kinds_map_to_crossing_status() -> Result<()> {
        assert_eq!(
            SemanticDeletionKind::Invalidation.crossing_status(),
            CrossingStatus::Invalidated
        );
        assert_eq!(
            SemanticDeletionKind::Tombstone.crossing_status(),
            CrossingStatus::Tombstoned
        );
        assert_eq!(
            SemanticDeletionKind::RetentionRedaction.crossing_status(),
            CrossingStatus::RetentionRedacted
        );
        for kind in [
            SemanticDeletionKind::Invalidation,
            SemanticDeletionKind::Tombstone,
            SemanticDeletionKind::RetentionRedaction,
        ] {
            assert!(kind.as_supersession_kind().is_semantic_deletion());
            assert!(kind.as_supersession_kind().crossing_status().is_some());
        }
        assert!(SupersessionKind::Correction.crossing_status().is_none());
        Ok(())
    }

    #[test]
    fn semantic_deletion_supersedes_without_message_delete() -> Result<()> {
        let seats = LiveAdmissionSeats::mint_genesis();
        let original_row = [DataValue::from(1i64), DataValue::from(100i64)];
        let (original, _) = admit_original(
            &seats,
            "quote",
            &original_row,
            1,
            ValidityTs::of_micros(100),
        )?;
        let prior = original.record_id();
        let prior_commit = seats.origin_commit()?;

        let kinds = [
            SemanticDeletionKind::Invalidation,
            SemanticDeletionKind::Tombstone,
            SemanticDeletionKind::RetentionRedaction,
        ];
        let mut retained = Vec::new();
        for (i, kind) in kinds.into_iter().enumerate() {
            let live = seats.certificate_inputs(CatalogGeneration::from_relation(
                RelationGeneration::witness(0),
            ))?;
            let i_i = crate::rules::convert::i64_from_u64_nonneg_fitting(crate::rules::convert::u64_from_usize_total(i));
            let subject = DataValue::List(vec![DataValue::from(1i64), DataValue::from(i_i)]);
            let (successor, cert) = admit_semantic_deletion(
                seats.store_id(),
                &live,
                kind,
                prior,
                subject,
                ValidityTs::of_micros(
                    200 + crate::rules::convert::i64_from_u64_nonneg_fitting(crate::rules::convert::u64_from_usize_total(i)),
                ),
            )
            .map_err(|e| miette!("admit semantic deletion: {e}"))?;
            let (_permit, link) = seal_semantic_deletion(&seats, kind, prior, &successor, cert)
                .map_err(|e| miette!("seal semantic deletion: {e}"))?;
            assert_eq!(link.prior(), prior);
            assert_eq!(link.successor(), successor.record_id());
            assert_ne!(link.prior(), link.successor());
            assert_eq!(link.kind(), kind.as_supersession_kind());
            assert!(link.kind().is_semantic_deletion());
            assert_eq!(link.kind().crossing_status(), Some(kind.crossing_status()));
            retained.push(link);
        }
        assert!(
            seats.origin_commit()? > prior_commit,
            "each semantic deletion advances dense CommitOrdinal"
        );
        let all = seats.retained_supersessions()?;
        assert_eq!(all.len(), 3, "three semantic deletions retained");
        for link in &retained {
            assert!(all.contains(link));
        }
        Ok(())
    }

    #[test]
    fn prior_committed_bytes_survive_semantic_deletion_append() -> Result<()> {
        let db = open_engine(SimStorage::new(0x2700_0004))?;
        db.run_script(
            "?[id, price] <- [[1, 100]] :create quote {id => price} @ 100",
            no_params(),
        )
        .map_err(|e| miette!("create: {e}"))?;

        let rtx = db.store.read_tx().map_err(|e| miette!("read: {e}"))?;
        let before: Vec<(Vec<u8>, Vec<u8>)> = rtx
            .total_scan()
            .map(|kv| -> Result<_> {
                let (k, v) = kv.map_err(|e| miette!("kv: {e}"))?;
                Ok((k.to_vec(), v.to_vec()))
            })
            .collect::<Result<Vec<_>>>()?;
        drop(rtx);
        let before_len = before.len();

        let seats = LiveAdmissionSeats::mint_genesis();
        let original_row = [DataValue::from(1i64), DataValue::from(100i64)];
        let (prior_record, _) = admit_original(
            &seats,
            "quote",
            &original_row,
            1,
            ValidityTs::of_micros(100),
        )?;
        let live = seats.certificate_inputs(CatalogGeneration::from_relation(
            RelationGeneration::witness(0),
        ))?;
        let (deletion, cert) = admit_semantic_deletion(
            seats.store_id(),
            &live,
            SemanticDeletionKind::Tombstone,
            prior_record.record_id(),
            DataValue::List(vec![DataValue::from(1i64)]),
            ValidityTs::of_micros(200),
        )
        .map_err(|e| miette!("admit tombstone: {e}"))?;
        seal_semantic_deletion(
            &seats,
            SemanticDeletionKind::Tombstone,
            prior_record.record_id(),
            &deletion,
            cert,
        )
        .map_err(|e| miette!("seal tombstone: {e}"))?;

        // Store bytes unchanged by admission alone — semantic deletion never
        // message-deletes. Append path (retract) would add a Retract row; the
        // prior Assert key must still exist either way.
        let rtx = db.store.read_tx().map_err(|e| miette!("read after: {e}"))?;
        let after: Vec<(Vec<u8>, Vec<u8>)> = rtx
            .total_scan()
            .map(|kv| -> Result<_> {
                let (k, v) = kv.map_err(|e| miette!("kv: {e}"))?;
                Ok((k.to_vec(), v.to_vec()))
            })
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(
            after.len(),
            before_len,
            "admission-only semantic deletion must not erase store keys"
        );
        for (k, v) in &before {
            let found = after.iter().find(|(ak, _)| ak == k);
            assert!(
                found.is_some(),
                "prior committed key must still exist after semantic deletion admit"
            );
            assert_eq!(
                found.map(|(_, av)| av.as_slice()),
                Some(v.as_slice()),
                "prior committed value bytes must be unchanged (no message-delete)"
            );
        }
        Ok(())
    }

    #[test]
    fn append_correction_uses_as_of_skip_scan_not_rewrite() -> Result<()> {
        // Point-read as-of on the live store path (not script sugar alone).
        let db = open_engine(SimStorage::new(0x2680_0006))?;
        db.run_script(
            "?[id, price] <- [[1, 100]] :create quote {id => price} @ 100",
            no_params(),
        )
        .map_err(|e| miette!("create: {e}"))?;

        let seats = LiveAdmissionSeats::mint_genesis();
        let original = [DataValue::from(1i64), DataValue::from(100i64)];
        let (prior_record, _) =
            admit_original(&seats, "quote", &original, 1, ValidityTs::of_micros(100))?;

        let mut tx = db.store.write_tx().map_err(|e| miette!("tx: {e}"))?;
        let handle = get_relation(&tx, "quote").map_err(|e| miette!("handle: {e}"))?;
        append_corrected_fact(
            &seats,
            &handle,
            &mut tx,
            prior_record.record_id(),
            &[DataValue::from(1i64), DataValue::from(175i64)],
            ValidityTs::of_micros(300),
            SourceSpan::empty(),
        )
        .map_err(|e| miette!("append @300: {e}"))?;
        tx.commit().map_err(|e| miette!("commit: {e}"))?;

        let rtx = db.store.read_tx().map_err(|e| miette!("read: {e}"))?;
        let handle = get_relation(&rtx, "quote").map_err(|e| miette!("handle: {e}"))?;
        let pre = handle
            .current_row(
                &rtx,
                &[DataValue::from(1i64)],
                AsOf::at(ValidityTs::of_micros(i64::MAX), ValidityTs::of_micros(150)),
                SourceSpan::empty(),
            )
            .map_err(|e| miette!("as-of 150: {e}"))?;
        assert_eq!(
            pre,
            Some(Tuple::from_vec(vec![
                DataValue::from(1i64),
                DataValue::from(100i64)
            ])),
            "store as-of before correction replays original"
        );
        let post = handle
            .current_row(
                &rtx,
                &[DataValue::from(1i64)],
                AsOf::at(ValidityTs::of_micros(i64::MAX), ValidityTs::of_micros(350)),
                SourceSpan::empty(),
            )
            .map_err(|e| miette!("as-of 350: {e}"))?;
        assert_eq!(
            post,
            Some(Tuple::from_vec(vec![
                DataValue::from(1i64),
                DataValue::from(175i64)
            ])),
            "store as-of after correction sees successor"
        );
        Ok(())
    }
}
