/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Retrieval is a projection that never owns truth (#268 T3; absorbed #269).
//!
//! A retrieval span resolves to a source [`RecordId`] or the read refuses
//! with a typed [`RetrievalRefuse`]. Orphan spans (no source RecordId) cannot
//! be served as truth.

use miette::Diagnostic;
use thiserror::Error;

use crate::session::record_id::RecordId;

/// A retrieval span over evidence / projection material.
///
/// Construction that already names a source uses [`RetrievalSpan::from_source`].
/// An orphan span ([`RetrievalSpan::orphan`]) exists only so resolve can
/// typed-refuse — it must never be served as truth.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RetrievalSpan {
    start: u64,
    end: u64,
    source: Option<RecordId>,
}

impl RetrievalSpan {
    /// Span that already resolves to an admitted source RecordId.
    pub fn from_source(source: RecordId, start: u64, end: u64) -> Self {
        Self {
            start,
            end,
            source: Some(source),
        }
    }

    /// Orphan span with no source RecordId — resolve must refuse.
    pub fn orphan(start: u64, end: u64) -> Self {
        Self {
            start,
            end,
            source: None,
        }
    }

    /// Span start offset.
    pub fn start(&self) -> u64 {
        self.start
    }

    /// Span end offset.
    pub fn end(&self) -> u64 {
        self.end
    }

    /// Resolve to the source [`RecordId`], or typed-refuse if unresolved.
    pub fn resolve_source(&self) -> Result<RecordId, RetrievalRefuse> {
        self.source.ok_or(RetrievalRefuse::UnresolvedRecordId)
    }
}

/// Typed refuse when retrieval cannot name a source RecordId.
#[derive(Debug, Clone, PartialEq, Eq, Error, Diagnostic)]
pub enum RetrievalRefuse {
    /// Span has no source RecordId — serving it would make retrieval a second truth.
    #[error("UnresolvedRecordId: retrieval span does not resolve to a source RecordId")]
    #[diagnostic(code(project::retrieval::unresolved_record_id))]
    UnresolvedRecordId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::digest::RecordContentDigest;
    use crate::data::statement::{
        construct, ContextId, StatementContext, StatementSource, StatementSubject, StatementValue,
        ValidityTime,
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

    fn admit_one_record_id() -> RecordId {
        let store = StoreId::from_digest([0x52; 32]);
        let digest = RecordContentDigest::from_digest([0xE1; 32]);
        let (kind, statement) = construct::claim(
            StatementSubject::new(DataValue::from("span-subject")),
            crate::data::statement::StatementPredicate::new("about").expect("predicate"),
            StatementValue::new(DataValue::from("payload")),
            ValidityTime::instant(1),
            StatementContext::Scoped(ContextId::from_digest([0xC1; 32])),
            StatementSource::unbound(),
        );
        let authority = WriteAuthority::mint(store, [0xB2; 32]);
        let chain = RootChain::empty();
        let live = LiveCertificateInputs::from_live(
            CatalogGeneration::from_relation(RelationGeneration::witness(3)),
            &chain,
            &authority,
            CommitOrdinal::ZERO,
            ScopeManifestDigest::from_digest([0x61; 32]),
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
            live,
        ))
        .expect("admit")
        .0
        .record_id()
    }

    #[test]
    fn orphan_retrieval_span_typed_refuses() {
        let span = RetrievalSpan::orphan(0, 10);
        assert_eq!(
            span.resolve_source(),
            Err(RetrievalRefuse::UnresolvedRecordId)
        );
    }

    #[test]
    fn sourced_retrieval_span_resolves() {
        let id = admit_one_record_id();
        let span = RetrievalSpan::from_source(id, 1, 2);
        assert_eq!(span.resolve_source(), Ok(id));
    }
}
