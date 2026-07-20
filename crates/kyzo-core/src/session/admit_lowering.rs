/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Type-entailed deterministic lowering of [`KyzoRecord`] (#268 T2/T3).
//!
//! Kind fixes which members of the closed six-dimension set are produced.
//! There is no per-write projection menu. Each call recomputes from the
//! record's typed fields — never a memoized cache on the record.
//! Every lowered row carries the source [`RecordId`].
//!
//! Crossing / federation receive (#270 T1/T3): a foreign record may lower only
//! after [`crate::store::replica::validate_crossing_before_lower`] mints
//! [`CrossingValidated`]. [`lower_after_crossing`] seals rows under the
//! token's `origin_schema_cut` ([`OriginSealedLowering`]) — local Catalog
//! cuts cannot reshape it; kind mismatch refuses in release builds.

use crate::data::statement::{OntokKind, StatementContext, StatementSource};
use crate::project::dimension::{LoweredRow, RecordLowering, StatementDimension};
use crate::store::replica::{
    AdmissionCertificate, CrossingCapabilitySet, CrossingContext, CrossingEnvelope,
    CrossingEvidence, CrossingEvidenceDemand, CrossingKind, CrossingRefuse, CrossingStatus,
    CrossingValidated, GraphBoundKey, GraphBoundary, NamespacedRecordIdentity, PromotionMeaning,
    ReplicaKey, TenantId, view_under_schema_cut,
};
use kyzo_model::value::canonical::encode_owned;
use kyzo_model::value::DataValue;
use sha2::{Digest, Sha256};

use super::{KyzoRecord, SemanticSurface};

/// Named semantic-surface tag for projection encoding — not a bare i64.
///
/// Symmetric [`SurfaceTag::encode`] / [`SurfaceTag::decode`] round-trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SurfaceTag {
    /// [`SemanticSurface::None`].
    None,
    /// [`SemanticSurface::Embedding`].
    Embedding,
    /// [`SemanticSurface::FullText`].
    FullText,
    /// [`SemanticSurface::Lexical`].
    Lexical,
}

impl SurfaceTag {
    /// Map a live surface to its projection tag.
    pub(crate) fn from_surface(surface: SemanticSurface) -> Self {
        match surface {
            SemanticSurface::None => Self::None,
            SemanticSurface::Embedding => Self::Embedding,
            SemanticSurface::FullText => Self::FullText,
            SemanticSurface::Lexical => Self::Lexical,
        }
    }

    /// Encode to the wire / projection integer.
    pub(crate) fn encode(self) -> i64 {
        match self {
            Self::None => 0,
            Self::Embedding => 1,
            Self::FullText => 2,
            Self::Lexical => 3,
        }
    }

    /// Decode a projection integer; unknown tags refuse.
    pub(crate) fn decode(tag: i64) -> Option<Self> {
        match tag {
            0 => Some(Self::None),
            1 => Some(Self::Embedding),
            2 => Some(Self::FullText),
            3 => Some(Self::Lexical),
            _ => None,
        }
    }

    /// Recover the semantic surface from this tag.
    pub(crate) fn to_surface(self) -> SemanticSurface {
        match self {
            Self::None => SemanticSurface::None,
            Self::Embedding => SemanticSurface::Embedding,
            Self::FullText => SemanticSurface::FullText,
            Self::Lexical => SemanticSurface::Lexical,
        }
    }
}

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
/// Every row carries [`RecordId`] so projections resolve home (#268 T3).
///
/// **Local / same-store path only.** Crossing receive must use
/// [`lower_after_crossing`] after [`validate_crossing_before_lower`].
pub(crate) fn lower_record(record: &KyzoRecord) -> RecordLowering {
    let source = record.record_id();
    let mut rows = Vec::with_capacity(4);
    for &dimension in dimensions_entailed(record.kind()) {
        let payload = encode_dimension_row(record, dimension);
        rows.push(LoweredRow::new(dimension, source, payload));
    }
    RecordLowering::from_ordered_rows(rows)
}

/// Map ONTOK kind to the crossing wire tag (#270 T1).
pub(crate) fn crossing_kind_from_ontok(kind: OntokKind) -> CrossingKind {
    match kind {
        OntokKind::Entity => CrossingKind::Entity,
        OntokKind::Event => CrossingKind::Event,
        OntokKind::State => CrossingKind::State,
        OntokKind::Role => CrossingKind::Role,
        OntokKind::Relation => CrossingKind::Relation,
        OntokKind::Claim => CrossingKind::Claim,
        OntokKind::Evidence => CrossingKind::Evidence,
        OntokKind::Context => CrossingKind::Context,
        OntokKind::Concept => CrossingKind::Concept,
        OntokKind::Rule => CrossingKind::Rule,
        OntokKind::Derivation => CrossingKind::Derivation,
        OntokKind::Invalidation => CrossingKind::Invalidation,
    }
}

/// Build a crossing envelope from an admitted record + its certificate.
///
/// Evidence demand follows semantic surface: interpreted surfaces declare
/// evidence required; [`SemanticSurface::None`] does not.
pub(crate) fn crossing_envelope_from_record(
    record: &KyzoRecord,
    certificate: &AdmissionCertificate,
    status: CrossingStatus,
    shared_capabilities: CrossingCapabilitySet,
) -> CrossingEnvelope {
    let evidence_demand = match record.surface() {
        SemanticSurface::None => CrossingEvidenceDemand::NotRequired,
        SemanticSurface::Embedding | SemanticSurface::FullText | SemanticSurface::Lexical => {
            CrossingEvidenceDemand::DeclaredRequired
        }
    };
    let evidence = match record.evidence() {
        Some(coords) => CrossingEvidence::Present(*coords.hash().as_digest()),
        None => CrossingEvidence::Absent,
    };
    let context = match record.context() {
        StatementContext::Unscoped => CrossingContext::Unscoped,
        StatementContext::Scoped(id) => CrossingContext::Scoped(*id.as_digest()),
    };
    CrossingEnvelope::new(
        crossing_kind_from_ontok(record.kind()),
        *certificate.protocol_version(),
        *certificate.schema_cut(),
        certificate.authorizing_key_id(),
        context,
        evidence_demand,
        evidence,
        status,
        shared_capabilities,
    )
}

/// Lowering sealed under the origin schema cut (#270 T3 / seat 69).
///
/// Equality and [`OriginSealedLowering::replay_digest`] include the cut —
/// carrying-but-ignoring `origin_schema_cut` is deleted. A different local
/// Catalog cut cannot reshape this via [`OriginSealedLowering::under_local_cut`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginSealedLowering {
    origin_schema_cut: [u8; 32],
    kind: CrossingKind,
    custody_key: ReplicaKey,
    rows: RecordLowering,
}

impl OriginSealedLowering {
    /// Seal rows under the validated origin schema cut — private mint path.
    fn seal(
        origin_schema_cut: [u8; 32],
        kind: CrossingKind,
        custody_key: ReplicaKey,
        rows: RecordLowering,
    ) -> Self {
        Self {
            origin_schema_cut,
            kind,
            custody_key,
            rows,
        }
    }

    /// Origin schema cut that constrains this lowering.
    pub fn origin_schema_cut(&self) -> &[u8; 32] {
        &self.origin_schema_cut
    }

    /// Validated ONTOK kind sealed with the cut.
    pub fn kind(&self) -> CrossingKind {
        self.kind
    }

    /// Custody key from crossing validation — confine via [`Self::confine_to_graph`].
    pub fn custody_key(&self) -> ReplicaKey {
        self.custody_key
    }

    /// Projection rows — only meaningful as-of [`Self::origin_schema_cut`].
    pub fn lowering(&self) -> &RecordLowering {
        &self.rows
    }

    /// Seat 69: view under a local Catalog cut — equal cut only; else refuse.
    pub fn under_local_cut(
        &self,
        local_schema_cut: &[u8; 32],
    ) -> Result<&RecordLowering, CrossingRefuse> {
        view_under_schema_cut(&self.origin_schema_cut, local_schema_cut)?;
        Ok(&self.rows)
    }

    /// Confine the sealed custody key to a graph boundary (#270 T3).
    pub fn confine_to_graph(&self, graph: GraphBoundary) -> GraphBoundKey {
        GraphBoundKey::bind(graph, &self.custody_key)
    }

    /// Replay meter: origin cut + kind + custody key + concatenated row bytes.
    pub fn replay_digest(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(b"kyzo.origin_sealed_lowering.v1");
        h.update(self.origin_schema_cut);
        h.update([self.kind.as_wire()]);
        h.update(self.custody_key.as_bytes());
        h.update(self.rows.concatenated_bytes());
        h.finalize().into()
    }
}

/// Lower a crossing record only after full contract validation (#270 T1/T3).
///
/// Requires [`CrossingValidated`] from [`validate_crossing_before_lower`].
/// Consumes `origin_schema_cut` to seal [`OriginSealedLowering`] — kind
/// mismatch refuses in release (not `debug_assert`); local Catalog
/// reinterpretation is Unconstructible via [`OriginSealedLowering::under_local_cut`].
pub(crate) fn lower_after_crossing(
    record: &KyzoRecord,
    validated: &CrossingValidated,
) -> Result<OriginSealedLowering, CrossingRefuse> {
    let record_kind = crossing_kind_from_ontok(record.kind());
    if record_kind != validated.kind() {
        return Err(CrossingRefuse::KindMismatch);
    }
    // Consume origin_schema_cut: seal lowering under it (not a local Catalog cut).
    // under_local_cut / replay_digest constrain meaning to this cut in release.
    let origin_schema_cut = *validated.origin_schema_cut();
    let custody_key = validated.custody().key();
    let rows = lower_record(record);
    Ok(OriginSealedLowering::seal(
        origin_schema_cut,
        validated.kind(),
        custody_key,
        rows,
    ))
}

/// Digest of validity-time for [`PromotionMeaning`] (#270 T3).
pub(crate) fn promotion_valid_time_digest(record: &KyzoRecord) -> [u8; 32] {
    let encoded = encode_owned(&DataValue::Interval(record.validity_time().as_interval()));
    let mut h = Sha256::new();
    h.update(b"kyzo.promotion.valid_time.v1");
    h.update(encoded.as_bytes());
    h.finalize().into()
}

/// Digest of provenance / source for [`PromotionMeaning`] (#270 T3).
pub(crate) fn promotion_provenance_digest(record: &KyzoRecord) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"kyzo.promotion.provenance.v1");
    match record.source() {
        StatementSource::Unbound => h.update(b"unbound"),
        StatementSource::Artifact(id) => h.update(id.as_digest()),
    }
    h.finalize().into()
}

/// Bind promotion meaning seats from an admitted record + certificate (#270 T3).
pub(crate) fn promotion_meaning_from_record(
    record: &KyzoRecord,
    certificate: &AdmissionCertificate,
    tenant: TenantId,
) -> PromotionMeaning {
    let identity = NamespacedRecordIdentity::from_certificate(
        *record.record_id().as_bytes(),
        certificate,
        tenant,
    );
    PromotionMeaning::bind(
        identity,
        promotion_valid_time_digest(record),
        promotion_provenance_digest(record),
        *certificate.schema_cut(),
    )
}

fn encode_dimension_row(record: &KyzoRecord, dimension: StatementDimension) -> Vec<u8> {
    let tuple = dimension_tuple(record, dimension);
    encode_owned(&tuple).as_bytes().to_vec()
}

fn dimension_tuple(record: &KyzoRecord, dimension: StatementDimension) -> DataValue {
    // RecordId anchors every projection row so rebuilds / retrieval resolve home.
    let record_id = DataValue::Bytes(record.record_id().as_bytes().to_vec());
    let subject = record.subject().as_value().clone();
    match dimension {
        StatementDimension::Identity => DataValue::List(vec![record_id, subject]),
        StatementDimension::Relationship => DataValue::List(vec![
            record_id,
            subject,
            DataValue::Str(record.predicate().as_str().to_owned()),
            record.value().as_value().clone(),
        ]),
        StatementDimension::Similarity => DataValue::List(vec![
            record_id,
            subject,
            record.value().as_value().clone(),
            DataValue::from(SurfaceTag::from_surface(record.surface()).encode()),
        ]),
        StatementDimension::QuantityAndLocation => DataValue::List(vec![
            record_id,
            subject,
            record.value().as_value().clone(),
        ]),
        StatementDimension::Time => DataValue::List(vec![
            record_id,
            subject,
            DataValue::Interval(record.validity_time().as_interval()),
        ]),
        StatementDimension::Source => {
            let source = match record.source() {
                StatementSource::Unbound => DataValue::Null,
                StatementSource::Artifact(id) => DataValue::Bytes(id.as_digest().to_vec()),
            };
            let context = match record.context() {
                StatementContext::Unscoped => DataValue::Null,
                StatementContext::Scoped(id) => DataValue::Bytes(id.as_digest().to_vec()),
            };
            DataValue::List(vec![record_id, subject, source, context])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::digest::RecordContentDigest;
    use crate::data::statement::{
        construct, ContextId, SourceArtifactId, StatementBody, StatementContext, StatementSource,
        StatementSubject, StatementValue, ValidityTime,
    };
    use crate::session::admit::{
        admit_record, AdmitRecordParts, IngestShape, LiveCertificateInputs, Placement, RecordCore,
        SemanticSurface,
    };
    use crate::session::generation::{CatalogGeneration, RelationGeneration};
    use crate::store::authority::WriteAuthority;
    use crate::store::merkle::RootChain;
    use crate::store::open::StoreId;
    use crate::store::replica::ScopeManifestDigest;
    use crate::store::sweep::CommitOrdinal;
    use kyzo_model::value::DataValue;

    fn live_cert(store: StoreId) -> LiveCertificateInputs {
        let authority = WriteAuthority::mint(store, [0xA1; 32]);
        let chain = RootChain::empty();
        LiveCertificateInputs::from_live(
            CatalogGeneration::from_relation(RelationGeneration::witness(1)),
            &chain,
            &authority,
            CommitOrdinal::ZERO,
            ScopeManifestDigest::from_digest([0x51; 32]),
        )
    }

    fn admit_claim_record() -> KyzoRecord {
        let store = StoreId::from_digest([0x26; 32]);
        let digest = RecordContentDigest::from_digest([0xA1; 32]);
        let (kind, statement) = construct::claim(
            StatementSubject::new(DataValue::from("widget")),
            crate::data::statement::StatementPredicate::new("part_of").expect("predicate"),
            StatementValue::new(DataValue::from("assembly")),
            ValidityTime::instant(1_700_000_000_000_000),
            StatementContext::Scoped(ContextId::from_digest([0xC0; 32])),
            StatementSource::unbound(),
        );
        admit_record(AdmitRecordParts::new(
            RecordCore::new(
                store,
                digest,
                SemanticSurface::None,
                None,
                kind,
                statement,
            ),
            Placement::Unrestricted,
            None,
            IngestShape::Record,
            live_cert(store),
        ))
        .expect("admit")
        .0
    }

    fn admit_kind(kind_body: (OntokKind, StatementBody)) -> KyzoRecord {
        let store = StoreId::from_digest([0x27; 32]);
        let digest = RecordContentDigest::from_digest([0xB2; 32]);
        let (kind, statement) = kind_body;
        admit_record(AdmitRecordParts::new(
            RecordCore::new(
                store,
                digest,
                SemanticSurface::None,
                None,
                kind,
                statement,
            ),
            Placement::Unrestricted,
            None,
            IngestShape::Record,
            live_cert(store),
        ))
        .expect("admit")
        .0
    }

    #[test]
    fn surface_tag_encode_decode_round_trip() {
        for surface in [
            SemanticSurface::None,
            SemanticSurface::Embedding,
            SemanticSurface::FullText,
            SemanticSurface::Lexical,
        ] {
            let tag = SurfaceTag::from_surface(surface);
            let encoded = tag.encode();
            let decoded = SurfaceTag::decode(encoded).expect("known tag");
            assert_eq!(decoded, tag);
            assert_eq!(decoded.to_surface(), surface);
        }
        assert_eq!(SurfaceTag::decode(99), None);
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
                StatementSource::unbound(),
            )
            .expect("entity"),
        );
        let relation = admit_kind(construct::relation(
            StatementSubject::new(DataValue::from("e1")),
            crate::data::statement::StatementPredicate::new("owns").expect("pred"),
            StatementValue::new(DataValue::from("e2")),
            ValidityTime::instant(1),
            StatementContext::Unscoped,
            StatementSource::unbound(),
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

    /// Every projection row resolves to the source RecordId (#268 T3).
    #[test]
    fn every_lowered_row_resolves_to_source_record_id() {
        let record = admit_claim_record();
        let id = record.record_id();
        let lowering = lower_record(&record);
        assert_eq!(lowering.source_record_id(), Some(id));
        for row in lowering.rows() {
            assert_eq!(
                row.source_record_id(),
                id,
                "projection row for {:?} must resolve to source RecordId",
                row.dimension()
            );
        }
    }

    /// Sugar relation put mints through admit_record — same RecordId door.
    #[test]
    fn sugar_relation_row_mints_through_admit_record() {
        use kyzo_model::value::ValidityTs;
        let store = StoreId::from_digest([0x28; 32]);
        let live = live_cert(store);
        let (record, cert) = super::super::admit_sugar_relation_row(
            store,
            &live,
            "parts",
            &[DataValue::from(1), DataValue::from("widget")],
            1,
            ValidityTs::from_raw(100),
        )
        .expect("sugar admit");
        assert_eq!(record.kind(), OntokKind::Relation);
        assert_eq!(record.store_id(), store);
        assert_eq!(cert.record_digest(), record.digest().as_digest());
        let permit = record.durable_write_permit();
        assert_eq!(permit.record_id(), record.record_id());
        let lowering = record.lower();
        assert_eq!(lowering.source_record_id(), Some(record.record_id()));
    }

    /// construct::{event,state,role,concept,rule,derivation,context_record}
    /// wire through the admit_construct door.
    #[test]
    fn construct_kinds_wire_through_admit_construct() {
        let store = StoreId::from_digest([0x29; 32]);
        let live = live_cert(store);
        let subject = StatementSubject::new(DataValue::from("s"));
        let pred = crate::data::statement::StatementPredicate::new("p").expect("pred");
        let value = StatementValue::new(DataValue::from("v"));
        let vt = ValidityTime::instant(1);
        let ctx = StatementContext::Unscoped;
        let src = StatementSource::unbound();
        let digest = RecordContentDigest::from_digest([0xD1; 32]);

        let kinds = [
            super::super::admit_construct::event(
                store, digest, subject.clone(), pred.clone(), value.clone(), vt, ctx.clone(),
                src.clone(), SemanticSurface::None, None, &live,
            )
            .expect("event")
            .0
            .kind(),
            super::super::admit_construct::state(
                store, digest, subject.clone(), pred.clone(), value.clone(), vt, ctx.clone(),
                src.clone(), SemanticSurface::None, None, &live,
            )
            .expect("state")
            .0
            .kind(),
            super::super::admit_construct::role(
                store, digest, subject.clone(), pred.clone(), value.clone(), vt, ctx.clone(),
                src.clone(), SemanticSurface::None, None, &live,
            )
            .expect("role")
            .0
            .kind(),
            super::super::admit_construct::concept(
                store, digest, subject.clone(), pred.clone(), value.clone(), vt, ctx.clone(),
                src.clone(), SemanticSurface::None, None, &live,
            )
            .expect("concept")
            .0
            .kind(),
            super::super::admit_construct::rule(
                store, digest, subject.clone(), pred.clone(), value.clone(), vt, ctx.clone(),
                src.clone(), SemanticSurface::None, None, &live,
            )
            .expect("rule")
            .0
            .kind(),
            super::super::admit_construct::derivation(
                store, digest, subject.clone(), pred.clone(), value.clone(), vt, ctx.clone(),
                src.clone(), SemanticSurface::None, None, &live,
            )
            .expect("derivation")
            .0
            .kind(),
            super::super::admit_construct::context_record(
                store, digest, subject.clone(), value.clone(), vt, ctx.clone(), src.clone(),
                SemanticSurface::None, None, &live,
            )
            .expect("context")
            .0
            .kind(),
        ];
        assert_eq!(
            kinds,
            [
                OntokKind::Event,
                OntokKind::State,
                OntokKind::Role,
                OntokKind::Concept,
                OntokKind::Rule,
                OntokKind::Derivation,
                OntokKind::Context,
            ]
        );
    }

    /// RecordId is a derived view of the one stored digest.
    #[test]
    fn record_id_is_derived_view_of_digest() {
        let record = admit_claim_record();
        assert_eq!(record.record_id().as_bytes(), record.digest().as_digest());
    }

    /// Crossing lower seals under origin_schema_cut; rows match local lower
    /// under that cut (#270 T1/T3). Kind mismatch and local-cut reinterpret
    /// refuse in release builds.
    #[test]
    fn lower_after_crossing_matches_local_lower() {
        use crate::store::epoch::FenceEpoch;
        use crate::store::replica::{
            AdmissionCertificateParts, AuthorizingKey, AuthorizingKeyTable, CrossingCapabilitySet,
            CrossingEvidenceDemand, CrossingStatus, GraphBoundary, KeyBoundaryRefuse,
            OriginContinuity, PostStateRoot, ScopeManifestDigest, ScopeManifestStatus,
            ScopeManifestTable, TenantId, mint_admission_certificate, prove_promotion_replay,
            sign_admission_parts, validate_crossing_before_lower,
        };
        use crate::store::sweep::CommitOrdinal;

        let record = admit_claim_record();
        let key = AuthorizingKey::mint_with_verifying_id([0x27; 32]);
        let scope = ScopeManifestDigest::from_digest([0x51; 32]);
        let mut parts = AdmissionCertificateParts {
            protocol_version: *b"kyzo.v01",
            origin_store: record.store_id(),
            origin_epoch: FenceEpoch::genesis(record.store_id()),
            origin_commit: CommitOrdinal::ZERO,
            schema_cut: [0x51; 32],
            record_digest: *record.digest().as_digest(),
            predecessor_history_digest: [0x52; 32],
            post_state_root: PostStateRoot::from_digest([0x53; 32]),
            authorizing_key_id: key.id(),
            scope_manifest_digest: scope,
            operation_key: None,
            signature: [0u8; 64],
        };
        parts.signature = sign_admission_parts(&parts, &key).expect("sign");
        let cert = mint_admission_certificate(parts).expect("mint");

        let mut keys = AuthorizingKeyTable::new();
        keys.insert(key);
        let mut scopes = ScopeManifestTable::new();
        scopes.set(scope, ScopeManifestStatus::Verified);

        // Claim with None surface → evidence not required.
        let envelope = super::crossing_envelope_from_record(
            &record,
            &cert,
            CrossingStatus::Active,
            CrossingCapabilitySet::new(),
        );
        assert_eq!(
            envelope.evidence_demand(),
            CrossingEvidenceDemand::NotRequired
        );

        let validated = validate_crossing_before_lower(
            &cert,
            &envelope,
            record.store_id(),
            CommitOrdinal::ZERO,
            &keys,
            &scopes,
            Some(&OriginContinuity::mint()),
            &CrossingCapabilitySet::new(),
        )
        .expect("crossing contract");

        let sealed = super::lower_after_crossing(&record, &validated).expect("lower");
        // Origin cut is consumed: sealed into the result and constrains views.
        assert_eq!(sealed.origin_schema_cut(), cert.schema_cut());
        assert_eq!(
            sealed.under_local_cut(cert.schema_cut()).expect("same cut"),
            &lower_record(&record)
        );
        assert_eq!(
            sealed.under_local_cut(&[0x99; 32]),
            Err(crate::store::replica::CrossingRefuse::LocalReinterpretationUnconstructible)
        );
        // Replay digest includes the cut — different cut would diverge.
        let again = super::lower_after_crossing(&record, &validated).expect("re-lower");
        assert_eq!(sealed.replay_digest(), again.replay_digest());
        assert_eq!(sealed, again);

        // Promotion: identity/time/provenance/schema replay-equal across "host move".
        let tenant = TenantId::from_digest([0x7E; 32]);
        let before = record.promotion_meaning(&cert, tenant);
        let after = record.promotion_meaning(&cert, tenant);
        assert_eq!(prove_promotion_replay(&before, &after), Ok(()));
        assert_eq!(before.schema_cut(), cert.schema_cut());

        // Key confinement: custody key authorizes only in its graph.
        let home = GraphBoundary::from_tenant(tenant);
        let foreign = GraphBoundary::from_tenant(TenantId::from_digest([0x7F; 32]));
        let bound = sealed.confine_to_graph(home);
        assert_eq!(bound.authorize(home), Ok(()));
        assert_eq!(
            bound.authorize(foreign),
            Err(KeyBoundaryRefuse::KeyCrossesGraphBoundary)
        );
    }

    /// NASTY (guardian, seat 69): a `CrossingValidated` minted for record A must
    /// NOT authorize lowering an unrelated same-kind record B. The token binds
    /// kind only — confused deputy. RED until `CrossingValidated` binds the
    /// validated record identity and `lower_after_crossing` gates on it.
    #[test]
    fn crossing_validation_for_a_must_not_authorize_lowering_b() {
        use crate::store::epoch::FenceEpoch;
        use crate::store::replica::{
            AdmissionCertificateParts, AuthorizingKey, AuthorizingKeyTable, CrossingCapabilitySet,
            CrossingStatus, OriginContinuity, PostStateRoot, ScopeManifestDigest,
            ScopeManifestStatus, ScopeManifestTable, mint_admission_certificate,
            sign_admission_parts, validate_crossing_before_lower,
        };
        use crate::store::sweep::CommitOrdinal;

        // Validate record A, exactly as the legitimate path does.
        let record = admit_claim_record();
        let key = AuthorizingKey::mint_with_verifying_id([0x27; 32]);
        let scope = ScopeManifestDigest::from_digest([0x51; 32]);
        let mut parts = AdmissionCertificateParts {
            protocol_version: *b"kyzo.v01",
            origin_store: record.store_id(),
            origin_epoch: FenceEpoch::genesis(record.store_id()),
            origin_commit: CommitOrdinal::ZERO,
            schema_cut: [0x51; 32],
            record_digest: *record.digest().as_digest(),
            predecessor_history_digest: [0x52; 32],
            post_state_root: PostStateRoot::from_digest([0x53; 32]),
            authorizing_key_id: key.id(),
            scope_manifest_digest: scope,
            operation_key: None,
            signature: [0u8; 64],
        };
        parts.signature = sign_admission_parts(&parts, &key).expect("sign");
        let cert = mint_admission_certificate(parts).expect("mint");
        let mut keys = AuthorizingKeyTable::new();
        keys.insert(key);
        let mut scopes = ScopeManifestTable::new();
        scopes.set(scope, ScopeManifestStatus::Verified);
        let envelope = super::crossing_envelope_from_record(
            &record,
            &cert,
            CrossingStatus::Active,
            CrossingCapabilitySet::new(),
        );
        let validated = validate_crossing_before_lower(
            &cert,
            &envelope,
            record.store_id(),
            CommitOrdinal::ZERO,
            &keys,
            &scopes,
            Some(&OriginContinuity::mint()),
            &CrossingCapabilitySet::new(),
        )
        .expect("validate A");

        // B: a DIFFERENT Claim record that never passed crossing validation.
        let b = admit_kind(construct::claim(
            StatementSubject::new(DataValue::from("intruder")),
            crate::data::statement::StatementPredicate::new("part_of").expect("pred"),
            StatementValue::new(DataValue::from("forged_assembly")),
            ValidityTime::instant(1_700_000_000_000_001),
            StatementContext::Scoped(ContextId::from_digest([0xC1; 32])),
            StatementSource::unbound(),
        ));
        assert_ne!(
            record.record_id(),
            b.record_id(),
            "A and B must be distinct records"
        );
        assert_eq!(
            record.kind(),
            b.kind(),
            "same kind — only an identity binding can refuse B"
        );

        assert!(
            super::lower_after_crossing(&b, &validated).is_err(),
            "CONFUSED DEPUTY: CrossingValidated minted for record A authorized lowering unrelated record B — token binds kind, not identity (seat 69)"
        );
    }

    /// Kind mismatch refuses in release — not debug_assert-only (#270 T3).
    #[test]
    fn lower_after_crossing_kind_mismatch_refuses() {
        use crate::store::epoch::FenceEpoch;
        use crate::store::replica::{
            AdmissionCertificateParts, AuthorizingKey, AuthorizingKeyTable, CrossingCapabilitySet,
            CrossingContext, CrossingEnvelope, CrossingEvidence, CrossingEvidenceDemand,
            CrossingKind, CrossingRefuse, CrossingStatus, OriginContinuity, PostStateRoot,
            ScopeManifestDigest, ScopeManifestStatus, ScopeManifestTable,
            mint_admission_certificate, sign_admission_parts, validate_crossing_before_lower,
        };
        use crate::store::sweep::CommitOrdinal;

        let record = admit_claim_record();
        let key = AuthorizingKey::mint_with_verifying_id([0x28; 32]);
        let scope = ScopeManifestDigest::from_digest([0x61; 32]);
        let mut parts = AdmissionCertificateParts {
            protocol_version: *b"kyzo.v01",
            origin_store: record.store_id(),
            origin_epoch: FenceEpoch::genesis(record.store_id()),
            origin_commit: CommitOrdinal::ZERO,
            schema_cut: [0x51; 32],
            record_digest: *record.digest().as_digest(),
            predecessor_history_digest: [0x52; 32],
            post_state_root: PostStateRoot::from_digest([0x53; 32]),
            authorizing_key_id: key.id(),
            scope_manifest_digest: scope,
            operation_key: None,
            signature: [0u8; 64],
        };
        parts.signature = sign_admission_parts(&parts, &key).expect("sign");
        let cert = mint_admission_certificate(parts).expect("mint");
        let mut keys = AuthorizingKeyTable::new();
        keys.insert(key);
        let mut scopes = ScopeManifestTable::new();
        scopes.set(scope, ScopeManifestStatus::Verified);

        // Envelope claims Entity while record is Claim — validate would pass
        // envelope-vs-cert; lower_after_crossing must still refuse KindMismatch.
        let mismatched = CrossingEnvelope::new(
            CrossingKind::Entity,
            *cert.protocol_version(),
            *cert.schema_cut(),
            cert.authorizing_key_id(),
            CrossingContext::Unscoped,
            CrossingEvidenceDemand::NotRequired,
            CrossingEvidence::Absent,
            CrossingStatus::Active,
            CrossingCapabilitySet::new(),
        );
        let validated = validate_crossing_before_lower(
            &cert,
            &mismatched,
            record.store_id(),
            CommitOrdinal::ZERO,
            &keys,
            &scopes,
            Some(&OriginContinuity::mint()),
            &CrossingCapabilitySet::new(),
        )
        .expect("envelope kind is a known ONTOK tag");
        assert_eq!(
            super::lower_after_crossing(&record, &validated),
            Err(CrossingRefuse::KindMismatch)
        );
    }
}
