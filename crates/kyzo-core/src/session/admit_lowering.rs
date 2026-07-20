/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Type-entailed deterministic lowering of [`KyzoRecord`] (#268 T2).
//!
//! Kind fixes which members of the closed six-dimension set are produced.
//! There is no per-write projection menu. Each call recomputes from the
//! record's typed fields — never a memoized cache on the record.

use crate::data::statement::{OntokKind, StatementContext};
use crate::project::dimension::{LoweredRow, RecordLowering, StatementDimension};
use kyzo_model::value::canonical::encode_owned;
use kyzo_model::value::DataValue;

use super::{KyzoRecord, SemanticSurface};

/// Dimensions entailed by an ONTOK kind — pure function of type, not write config.
///
/// Returned slice is sorted by [`StatementDimension`] Ord. Every kind produces
/// Identity / Time / Source (statement kernel). Relationship, Similarity, and
/// QuantityAndLocation are selected by kind responsibility.
pub(crate) fn dimensions_entailed(kind: OntokKind) -> &'static [StatementDimension] {
    use StatementDimension::{
        Identity, QuantityAndLocation, Relationship, Similarity, Source, Time,
    };
    match kind {
        OntokKind::Entity | OntokKind::Context => &[Identity, Time, Source],
        OntokKind::Event | OntokKind::State => &[Identity, QuantityAndLocation, Time, Source],
        OntokKind::Role
        | OntokKind::Relation
        | OntokKind::Rule
        | OntokKind::Derivation
        | OntokKind::Invalidation => &[Identity, Relationship, Time, Source],
        OntokKind::Claim | OntokKind::Evidence | OntokKind::Concept => {
            &[Identity, Similarity, Time, Source]
        }
    }
}

/// Lower a record into the closed six-dimension projection set.
///
/// Recomputes from typed fields on every call. Same record → same rows/bytes.
pub(crate) fn lower_record(record: &KyzoRecord) -> RecordLowering {
    let mut rows = Vec::with_capacity(4);
    for &dimension in dimensions_entailed(record.kind()) {
        let payload = encode_dimension_row(record, dimension);
        rows.push(LoweredRow::new(dimension, payload));
    }
    RecordLowering::from_ordered_rows(rows)
}

fn encode_dimension_row(record: &KyzoRecord, dimension: StatementDimension) -> Vec<u8> {
    let tuple = dimension_tuple(record, dimension);
    encode_owned(&tuple).as_bytes().to_vec()
}

fn dimension_tuple(record: &KyzoRecord, dimension: StatementDimension) -> DataValue {
    // Digest anchors every projection row so rebuilds resolve home (T3 seed).
    let digest = DataValue::Bytes(record.digest().to_vec());
    let subject = record.subject().as_value().clone();
    match dimension {
        StatementDimension::Identity => DataValue::List(vec![digest, subject]),
        StatementDimension::Relationship => DataValue::List(vec![
            digest,
            subject,
            DataValue::Str(record.predicate().as_str().to_owned()),
            record.value().as_value().clone(),
        ]),
        StatementDimension::Similarity => DataValue::List(vec![
            digest,
            subject,
            record.value().as_value().clone(),
            DataValue::from(surface_tag(record.surface())),
        ]),
        StatementDimension::QuantityAndLocation => DataValue::List(vec![
            digest,
            subject,
            record.value().as_value().clone(),
        ]),
        StatementDimension::Time => DataValue::List(vec![
            digest,
            subject,
            DataValue::Interval(record.validity_time().as_interval()),
        ]),
        StatementDimension::Source => {
            let source = DataValue::Bytes(record.source().artifact_id().as_digest().to_vec());
            let context = match record.context() {
                StatementContext::Unscoped => DataValue::Null,
                StatementContext::Scoped(id) => DataValue::Bytes(id.as_digest().to_vec()),
            };
            DataValue::List(vec![digest, subject, source, context])
        }
    }
}

fn surface_tag(surface: SemanticSurface) -> i64 {
    match surface {
        SemanticSurface::None => 0,
        SemanticSurface::Embedding => 1,
        SemanticSurface::FullText => 2,
        SemanticSurface::Lexical => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::statement::{
        construct, ContextId, SourceArtifactId, StatementBody, StatementContext, StatementSource,
        StatementSubject, StatementValue, ValidityTime,
    };
    use crate::store::open::StoreId;
    use crate::store::replica::AdmissionCertificateParts;
    use crate::store::sweep::CommitOrdinal;
    use crate::store::FenceEpoch;
    use kyzo_model::value::DataValue;

    use super::super::{
        admit_record, AdmitRecordParts, PlacementConstraint, SemanticSurface,
    };

    fn admit_claim_record() -> KyzoRecord {
        let store = StoreId::from_digest([0x26; 32]);
        let digest = [0xA1; 32];
        let (kind, statement) = construct::claim(
            StatementSubject::new(DataValue::from("widget")),
            crate::data::statement::StatementPredicate::new("part_of").expect("predicate"),
            StatementValue::new(DataValue::from("assembly")),
            ValidityTime::instant(1_700_000_000_000_000),
            StatementContext::Scoped(ContextId::from_digest([0xC0; 32])),
            StatementSource::new(SourceArtifactId::from_digest([0x50; 32])),
        );
        admit_record(AdmitRecordParts {
            store_id: store,
            digest,
            surface: SemanticSurface::None,
            evidence: None,
            kind,
            statement,
            placement: PlacementConstraint {
                allowed_regions: vec![],
            },
            write_region: [0; 16],
            secret_in_indexed_key: None,
            kv_as_truth: false,
            chunk_shaped: false,
            certificate: AdmissionCertificateParts {
                protocol_version: *b"kyzo.v01",
                origin_store: store,
                origin_epoch: FenceEpoch::genesis(store),
                origin_commit: CommitOrdinal::ZERO,
                schema_cut: [0x11; 32],
                record_digest: digest,
                predecessor_history_digest: [0x22; 32],
                post_state_root: [0x33; 32],
                authorizing_key_id: [0x44; 32],
                scope_manifest_digest: [0x55; 32],
                operation_key: None,
                signature: [0x66; 64],
            },
        })
        .expect("admit")
        .0
    }

    fn admit_kind(kind_body: (OntokKind, StatementBody)) -> KyzoRecord {
        let store = StoreId::from_digest([0x27; 32]);
        let digest = [0xB2; 32];
        let (kind, statement) = kind_body;
        admit_record(AdmitRecordParts {
            store_id: store,
            digest,
            surface: SemanticSurface::None,
            evidence: None,
            kind,
            statement,
            placement: PlacementConstraint {
                allowed_regions: vec![],
            },
            write_region: [0; 16],
            secret_in_indexed_key: None,
            kv_as_truth: false,
            chunk_shaped: false,
            certificate: AdmissionCertificateParts {
                protocol_version: *b"kyzo.v01",
                origin_store: store,
                origin_epoch: FenceEpoch::genesis(store),
                origin_commit: CommitOrdinal::ZERO,
                schema_cut: [0x11; 32],
                record_digest: digest,
                predecessor_history_digest: [0x22; 32],
                post_state_root: [0x33; 32],
                authorizing_key_id: [0x44; 32],
                scope_manifest_digest: [0x55; 32],
                operation_key: None,
                signature: [0x66; 64],
            },
        })
        .expect("admit")
        .0
    }

    /// Real determinism: lower twice into independent allocations; assert
    /// byte/row identity. Not a memoized re-call (no cache field on the record).
    #[test]
    fn repeated_lowering_equality() {
        let record = admit_claim_record();
        let first = lower_record(&record);
        let second = lower_record(&record);

        assert_eq!(
            first.rows().len(),
            second.rows().len(),
            "same record must produce the same number of projection rows"
        );
        assert_eq!(
            first, second,
            "RecordLowering must be PartialEq-identical across independent lowers"
        );
        let first_bytes = first.concatenated_bytes();
        let second_bytes = second.concatenated_bytes();
        assert_eq!(
            first_bytes, second_bytes,
            "concatenated row bytes must be byte-identical across independent lowers"
        );
        // Distinct row buffers inside each RecordLowering — not one memoized share.
        assert!(
            !first.rows().is_empty(),
            "claim lowering must produce at least one dimension row"
        );
        assert_ne!(
            first.rows()[0].as_bytes().as_ptr(),
            second.rows()[0].as_bytes().as_ptr(),
            "each lower_record call must allocate its own row bytes — not a memoized share"
        );
        for (a, b) in first.rows().iter().zip(second.rows().iter()) {
            assert_eq!(a.dimension(), b.dimension());
            assert_eq!(
                a.as_bytes(),
                b.as_bytes(),
                "dimension {:?} row bytes must match",
                a.dimension()
            );
        }
    }

    /// Kind decides the dimension set — not a per-write menu.
    #[test]
    fn kind_type_entails_dimension_set() {
        let entity = admit_kind(
            construct::entity(
                StatementSubject::new(DataValue::from("e1")),
                ValidityTime::instant(1),
                StatementContext::Unscoped,
                StatementSource::new(SourceArtifactId::from_digest([1; 32])),
            )
            .expect("entity"),
        );
        let relation = admit_kind(construct::relation(
            StatementSubject::new(DataValue::from("e1")),
            crate::data::statement::StatementPredicate::new("owns").expect("pred"),
            StatementValue::new(DataValue::from("e2")),
            ValidityTime::instant(1),
            StatementContext::Unscoped,
            StatementSource::new(SourceArtifactId::from_digest([1; 32])),
        ));
        let claim = admit_claim_record();

        let entity_dims: Vec<_> = lower_record(&entity).dimensions().collect();
        let relation_dims: Vec<_> = lower_record(&relation).dimensions().collect();
        let claim_dims: Vec<_> = lower_record(&claim).dimensions().collect();

        assert_eq!(
            entity_dims,
            vec![
                StatementDimension::Identity,
                StatementDimension::Time,
                StatementDimension::Source,
            ]
        );
        assert_eq!(
            relation_dims,
            vec![
                StatementDimension::Identity,
                StatementDimension::Relationship,
                StatementDimension::Time,
                StatementDimension::Source,
            ]
        );
        assert_eq!(
            claim_dims,
            vec![
                StatementDimension::Identity,
                StatementDimension::Similarity,
                StatementDimension::Time,
                StatementDimension::Source,
            ]
        );
        assert_ne!(
            entity_dims, relation_dims,
            "different kinds must type-entail different projection sets"
        );

        // Closed universe: every dimension appears for some kind.
        let mut seen = std::collections::BTreeSet::new();
        for kind in [
            OntokKind::Entity,
            OntokKind::Event,
            OntokKind::State,
            OntokKind::Role,
            OntokKind::Relation,
            OntokKind::Claim,
            OntokKind::Evidence,
            OntokKind::Context,
            OntokKind::Concept,
            OntokKind::Rule,
            OntokKind::Derivation,
            OntokKind::Invalidation,
        ] {
            for &d in dimensions_entailed(kind) {
                seen.insert(d);
            }
        }
        assert_eq!(
            seen.into_iter().collect::<Vec<_>>(),
            StatementDimension::ALL.to_vec(),
            "kind table must inhabit the full closed six-dimension set"
        );
    }

    /// KyzoRecord::lower is the door; same as free lower_record.
    #[test]
    fn record_lower_door_matches_free_function() {
        let record = admit_claim_record();
        assert_eq!(record.lower(), lower_record(&record));
    }
}
