/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Seat 34 — correction as supersession without overwrite.
//!
//! A correction is a **new** admitted [`KyzoRecord`] that supersedes a prior
//! by [`RecordId`]. The prior stays committed. History appends; there is no
//! rewrite / update-in-place / overwrite door on committed fact bytes.
//! Dense [`CommitOrdinal`] advances on the successor at attach.

use kyzo_model::SourceSpan;
use kyzo_model::value::canonical::encode_owned;
use kyzo_model::value::{DataValue, ValidityTs};
use sha2::{Digest, Sha256};

use crate::data::digest::RecordContentDigest;
use crate::data::statement::{
    StatementContext, StatementPredicate, StatementSource, StatementSubject, StatementValue,
    ValidityTime,
};
use crate::session::catalog::RelationHandle;
use crate::session::generation::{CatalogGeneration, RelationGeneration};
use crate::store::replica::AdmissionCertificate;
use crate::store::sweep::CommitOrdinal;
use crate::store::time::ClaimPolarity;
use crate::store::{StoreId, WriteTx};

use super::{
    AdmitRefuse, AdmittedDurableWrite, IngestShape, KyzoRecord, LiveAdmissionSeats,
    LiveCertificateInputs, Placement, RecordCore, RecordId, SemanticSurface, admit_record,
};

/// Auditable link: successor admitted record supersedes prior by identity.
///
/// Minted only when a correction attaches through [`seal_supersession`] —
/// never by rewriting prior committed bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Supersession {
    prior: RecordId,
    successor: RecordId,
    /// Dense CommitOrdinal of the successor at attach (seat 34).
    commit_ordinal: CommitOrdinal,
}

impl Supersession {
    /// Prior committed record identity — still addressable after correction.
    pub fn prior(self) -> RecordId {
        self.prior
    }

    /// Successor record identity (the correction).
    pub fn successor(self) -> RecordId {
        self.successor
    }

    /// Dense CommitOrdinal sealed with the successor.
    pub fn commit_ordinal(self) -> CommitOrdinal {
        self.commit_ordinal
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
    let keys_len = keys_len.min(corrected_row.len());
    let subject = StatementSubject::new(DataValue::List(corrected_row[..keys_len].to_vec()));
    let predicate =
        StatementPredicate::new(relation_name).map_err(|_| AdmitRefuse::SugarStatementRefuse)?;
    let value = StatementValue::new(DataValue::List(corrected_row[keys_len..].to_vec()));
    let (kind, statement) = crate::data::statement::construct::relation(
        subject,
        predicate,
        value,
        ValidityTime::instant(valid.raw()),
        StatementContext::Unscoped,
        StatementSource::unbound(),
    );
    let digest = digest_correction(prior, relation_name, corrected_row);
    let core = RecordCore::new(
        store_id,
        digest,
        SemanticSurface::None,
        None,
        kind,
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
    if record.record_id() == prior {
        return Err(AdmitRefuse::SugarStatementRefuse);
    }
    seats.attach_verified(record, certificate)?;
    let commit_ordinal = seats.origin_commit();
    let link = Supersession {
        prior,
        successor: record.record_id(),
        commit_ordinal,
    };
    seats.retain_supersession(link);
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
    ));
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
    let key = relation.encode_bitemporal_key_for_store(
        corrected_row,
        valid,
        tx.system_stamp(),
        span,
    )?;
    let val = relation.encode_bitemporal_val_for_store(
        corrected_row,
        ClaimPolarity::Assert,
        span,
    )?;
    // Append only: a new (valid, sys) key. Prior committed keys stay put.
    tx.put(&key, &val)?;
    Ok(link)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::admit::{admit_sugar_relation_row, LiveAdmissionSeats};
    use crate::session::catalog::{get_relation, Catalog};
    use crate::session::db::Engine;
    use crate::store::sim::SimStorage;
    use crate::store::{ReadTx, Storage};
    use kyzo_model::value::{AsOf, Tuple};
    use std::collections::BTreeMap;

    fn no_params() -> BTreeMap<String, DataValue> {
        BTreeMap::new()
    }

    fn open_engine(store: SimStorage) -> Engine<SimStorage> {
        Engine::compose(store, Catalog::new()).expect("compose engine")
    }

    /// Admit an original sugar row through the live seats (same door as put).
    fn admit_original(
        seats: &LiveAdmissionSeats,
        relation: &str,
        row: &[DataValue],
        keys_len: usize,
        valid: ValidityTs,
    ) -> (KyzoRecord, AdmissionCertificate) {
        let live = seats.certificate_inputs(CatalogGeneration::from_relation(
            RelationGeneration::witness(0),
        ));
        let (record, cert) = admit_sugar_relation_row(
            seats.store_id(),
            &live,
            relation,
            row,
            keys_len,
            valid,
        )
        .expect("admit original");
        seats.attach_verified(&record, cert.clone()).expect("attach");
        (record, cert)
    }

    #[test]
    fn correction_supersedes_by_record_id_with_dense_commit_ordinal() {
        let seats = LiveAdmissionSeats::mint_genesis();
        let original_row = [DataValue::from(1i64), DataValue::from(100i64)];
        let (original, _) = admit_original(
            &seats,
            "quote",
            &original_row,
            1,
            ValidityTs::from_raw(100),
        );
        let prior = original.record_id();
        let prior_commit = seats.origin_commit();

        let live = seats.certificate_inputs(CatalogGeneration::from_relation(
            RelationGeneration::witness(0),
        ));
        let corrected = [DataValue::from(1i64), DataValue::from(150i64)];
        let (successor, cert) = admit_correction(
            seats.store_id(),
            &live,
            prior,
            "quote",
            &corrected,
            1,
            ValidityTs::from_raw(200),
        )
        .expect("admit correction");
        let (_permit, link) =
            seal_supersession(&seats, prior, &successor, cert).expect("seal supersession");

        assert_eq!(link.prior(), prior);
        assert_eq!(link.successor(), successor.record_id());
        assert_ne!(link.prior(), link.successor());
        assert_eq!(
            link.commit_ordinal(),
            prior_commit.successor().expect("dense successor"),
            "successor carries the dense CommitOrdinal after prior"
        );
        assert_eq!(seats.origin_commit(), link.commit_ordinal());
        assert_eq!(
            seats.retained_supersessions(),
            vec![link],
            "supersession is retained on the live admission spine"
        );
    }

    #[test]
    fn as_of_pre_correction_replays_original_exactly() {
        let db = open_engine(SimStorage::new(0x2680_0004));
        db.run_script(
            "?[id, price] <- [[1, 100]] :create quote {id => price} @ 100",
            no_params(),
        )
        .expect("create original @100");

        let original_row = [DataValue::from(1i64), DataValue::from(100i64)];
        let seats = LiveAdmissionSeats::mint_genesis();
        // Admit the create's logical prior through the correction door's
        // identity plane, then append the correction on the real store.
        let (prior_record, _) = admit_original(
            &seats,
            "quote",
            &original_row,
            1,
            ValidityTs::from_raw(100),
        );
        let prior = prior_record.record_id();

        let mut tx = db.store.write_tx().expect("correction tx");
        let handle = get_relation(&tx, "quote").expect("quote");
        let corrected = [DataValue::from(1i64), DataValue::from(150i64)];
        let link = append_corrected_fact(
            &seats,
            &handle,
            &mut tx,
            prior,
            &corrected,
            ValidityTs::from_raw(200),
            SourceSpan::default(),
        )
        .expect("append correction");
        tx.commit().expect("commit correction");

        assert_ne!(link.prior(), link.successor());

        // As-of valid 150: after original @100, before correction @200.
        let at_150 = db
            .run_script("?[price] := *quote{id, price @ 150}", no_params())
            .expect("as-of 150");
        assert_eq!(
            at_150.rows(),
            &[Tuple::from_vec(vec![DataValue::from(100i64)])],
            "as-of pre-correction must replay the ORIGINAL value exactly"
        );

        // As-of after correction sees the superseding value.
        let at_250 = db
            .run_script("?[price] := *quote{id, price @ 250}", no_params())
            .expect("as-of 250");
        assert_eq!(
            at_250.rows(),
            &[Tuple::from_vec(vec![DataValue::from(150i64)])],
            "as-of post-correction sees the successor"
        );
    }

    #[test]
    fn prior_committed_bytes_survive_correction_append() {
        let db = open_engine(SimStorage::new(0x2680_0005));
        db.run_script(
            "?[id, price] <- [[1, 100]] :create quote {id => price} @ 100",
            no_params(),
        )
        .expect("create");

        let rtx = db.store.read_tx().expect("read");
        let before: Vec<(Vec<u8>, Vec<u8>)> = rtx
            .total_scan()
            .map(|kv| {
                let (k, v) = kv.expect("kv");
                (k.to_vec(), v.to_vec())
            })
            .collect();
        drop(rtx);
        let before_len = before.len();

        let seats = LiveAdmissionSeats::mint_genesis();
        let original_row = [DataValue::from(1i64), DataValue::from(100i64)];
        let (prior_record, _) = admit_original(
            &seats,
            "quote",
            &original_row,
            1,
            ValidityTs::from_raw(100),
        );

        let mut tx = db.store.write_tx().expect("tx");
        let handle = get_relation(&tx, "quote").expect("handle");
        append_corrected_fact(
            &seats,
            &handle,
            &mut tx,
            prior_record.record_id(),
            &[DataValue::from(1i64), DataValue::from(150i64)],
            ValidityTs::from_raw(200),
            SourceSpan::default(),
        )
        .expect("append");
        tx.commit().expect("commit");

        let rtx = db.store.read_tx().expect("read after");
        let after: Vec<(Vec<u8>, Vec<u8>)> = rtx
            .total_scan()
            .map(|kv| {
                let (k, v) = kv.expect("kv");
                (k.to_vec(), v.to_vec())
            })
            .collect();
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
    }

    #[test]
    fn no_rewrite_api_on_committed_facts() {
        // Seat 34 grep-proof: correction appends; no rewrite/overwrite door.
        // Scan production surfaces only — strip this file's cfg(test) module so
        // the forbidden-needle table cannot match itself.
        let supersession_prod = include_str!("admit_supersession.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production supersession surface");
        let sources = [
            include_str!("admit.rs"),
            supersession_prod,
            include_str!("../store/time.rs"),
            include_str!("../store/tx.rs"),
        ];
        // Split so this test body never contains a contiguous forbidden ident.
        let forbidden: [String; 6] = [
            ["fn rewr", "ite_committed"].concat(),
            ["fn overwr", "ite_fact"].concat(),
            ["fn overwr", "ite_committed"].concat(),
            ["fn muta", "te_committed"].concat(),
            ["fn update_in", "_place"].concat(),
            ["fn rewr", "ite_fact"].concat(),
        ];
        for src in sources {
            for needle in &forbidden {
                assert!(
                    !src.contains(needle.as_str()),
                    "forbidden rewrite API `{needle}` must not exist on committed facts"
                );
            }
        }
    }

    #[test]
    fn append_correction_uses_as_of_skip_scan_not_rewrite() {
        // Point-read as-of on the live store path (not script sugar alone).
        let db = open_engine(SimStorage::new(0x2680_0006));
        db.run_script(
            "?[id, price] <- [[1, 100]] :create quote {id => price} @ 100",
            no_params(),
        )
        .expect("create");

        let seats = LiveAdmissionSeats::mint_genesis();
        let original = [DataValue::from(1i64), DataValue::from(100i64)];
        let (prior_record, _) =
            admit_original(&seats, "quote", &original, 1, ValidityTs::from_raw(100));

        let mut tx = db.store.write_tx().expect("tx");
        let handle = get_relation(&tx, "quote").expect("handle");
        append_corrected_fact(
            &seats,
            &handle,
            &mut tx,
            prior_record.record_id(),
            &[DataValue::from(1i64), DataValue::from(175i64)],
            ValidityTs::from_raw(300),
            SourceSpan::default(),
        )
        .expect("append @300");
        tx.commit().expect("commit");

        let rtx = db.store.read_tx().expect("read");
        let handle = get_relation(&rtx, "quote").expect("handle");
        let pre = handle
            .current_row(
                &rtx,
                &[DataValue::from(1i64)],
                AsOf::at(ValidityTs::from_raw(i64::MAX), ValidityTs::from_raw(150)),
                SourceSpan::default(),
            )
            .expect("as-of 150");
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
                AsOf::at(ValidityTs::from_raw(i64::MAX), ValidityTs::from_raw(350)),
                SourceSpan::default(),
            )
            .expect("as-of 350");
        assert_eq!(
            post,
            Some(Tuple::from_vec(vec![
                DataValue::from(1i64),
                DataValue::from(175i64)
            ])),
            "store as-of after correction sees successor"
        );
    }
}
