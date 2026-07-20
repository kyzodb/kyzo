/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Typed statement body for the one private [`crate::session::admit::KyzoRecord`].
//!
//! A KyzoRecord is a typed statement: subject / predicate / value, qualified by
//! validity-time and context, bound to a source (story #268 T1; product brief).
//! ONTOK variants are constructions over this kernel — never a second record
//! type and never a nullable mega-struct fork.

use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use kyzo_model::value::{Bound, DataValue, Interval};

/// ONTOK conceptual kind — type authority on the statement kernel.
///
/// Variants are constructions over the one [`StatementBody`], not distinct
/// record types (`EntityRecord` / `ClaimRecord` / … are condemned forks).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OntokKind {
    /// Anchors identity.
    Entity,
    /// Something that happened.
    Event,
    /// Truth over a scope or interval.
    State,
    /// Contextual participation.
    Role,
    /// Typed connection.
    Relation,
    /// Assertion whose standing can change.
    Claim,
    /// Source support (span / message / tool output).
    Evidence,
    /// Scopes truth (tenant, project, conversation, …).
    Context,
    /// Reusable classification.
    Concept,
    /// Behavior / derivation / governance rule.
    Rule,
    /// Derived knowledge linked to inputs.
    Derivation,
    /// Supersession / invalidation without overwrite.
    Invalidation,
}

/// Subject of a typed statement — what the assertion is about.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StatementSubject(DataValue);

impl StatementSubject {
    /// Mint a subject from an already-typed value.
    pub fn new(value: DataValue) -> Self {
        Self(value)
    }

    /// Borrow the subject value.
    pub fn as_value(&self) -> &DataValue {
        &self.0
    }
}

/// Predicate of a typed statement — ontology slot, not a free-string taxonomy.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StatementPredicate(SmartString<LazyCompact>);

impl StatementPredicate {
    /// Mint a non-empty predicate name.
    pub fn new(name: impl AsRef<str>) -> Result<Self, StatementRefuse> {
        let name = name.as_ref();
        if name.is_empty() {
            return Err(StatementRefuse::EmptyPredicate);
        }
        Ok(Self(SmartString::from(name)))
    }

    /// Borrow the predicate name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Value (object) of a typed statement.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StatementValue(DataValue);

impl StatementValue {
    /// Mint a statement value from an already-typed [`DataValue`].
    pub fn new(value: DataValue) -> Self {
        Self(value)
    }

    /// Borrow the value.
    pub fn as_value(&self) -> &DataValue {
        &self.0
    }
}

/// Validity-time scope — when the assertion holds in the represented world.
///
/// Uses the engine's [`Interval`] (unbounded ends as variants, not sentinels).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ValidityTime(Interval);

impl ValidityTime {
    /// Wrap a proven interval.
    pub fn new(interval: Interval) -> Self {
        Self(interval)
    }

    /// A single closed instant.
    pub fn instant(ts_micros: i64) -> Self {
        Self(Interval::new(
            Bound::Closed(ts_micros),
            Bound::Closed(ts_micros),
        ))
    }

    /// Closed start, open end (still holds).
    pub fn from_onward(from_micros: i64) -> Self {
        Self(Interval::new(Bound::Closed(from_micros), Bound::Unbounded))
    }

    /// Borrow the interval.
    pub fn as_interval(self) -> Interval {
        self.0
    }
}

/// Durable context identity that scopes an assertion.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContextId([u8; 32]);

impl ContextId {
    /// Wrap an already-proven context digest.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Borrow the digest bytes.
    pub fn as_digest(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Durable context that scopes the statement (seat 11).
///
/// Not query-time retrieval context — that is a projection concern.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StatementContext {
    /// Holds without a restricting durable scope.
    Unscoped,
    /// Scoped to a durable context identity.
    Scoped(ContextId),
}

/// Source artifact the statement is bound to (seats 10/11).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceArtifactId([u8; 32]);

impl SourceArtifactId {
    /// Wrap an already-proven source-artifact digest.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Borrow the digest bytes.
    pub fn as_digest(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Source binding on the statement body (seats 10/11).
///
/// [`StatementSource::Unbound`] is the only legal binding for
/// [`crate::session::admit::SemanticSurface::None`]. Interpreted surfaces
/// carry [`StatementSource::Artifact`] (from evidence), never a forged
/// relation-name hash.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StatementSource {
    /// None-surface: no source artifact — field is unbound by type.
    Unbound,
    /// Interpreted knowledge: artifact identity from evidence coordinates.
    Artifact(SourceArtifactId),
}

impl StatementSource {
    /// None-surface source gate — no artifact binding.
    pub fn unbound() -> Self {
        Self::Unbound
    }

    /// Bind to a source artifact (interpreted surfaces only).
    pub fn new(artifact: SourceArtifactId) -> Self {
        Self::Artifact(artifact)
    }

    /// Borrow the source artifact id when bound.
    pub fn artifact_id(&self) -> Option<&SourceArtifactId> {
        match self {
            Self::Unbound => None,
            Self::Artifact(id) => Some(id),
        }
    }
}

/// The six typed statement-body fields as one value object.
///
/// Embedded into the one private KyzoRecord — not a second record type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StatementBody {
    subject: StatementSubject,
    predicate: StatementPredicate,
    value: StatementValue,
    validity_time: ValidityTime,
    context: StatementContext,
    source: StatementSource,
}

impl StatementBody {
    /// Assemble the six typed fields.
    pub fn new(
        subject: StatementSubject,
        predicate: StatementPredicate,
        value: StatementValue,
        validity_time: ValidityTime,
        context: StatementContext,
        source: StatementSource,
    ) -> Self {
        Self {
            subject,
            predicate,
            value,
            validity_time,
            context,
            source,
        }
    }

    /// Subject.
    pub fn subject(&self) -> &StatementSubject {
        &self.subject
    }

    /// Predicate.
    pub fn predicate(&self) -> &StatementPredicate {
        &self.predicate
    }

    /// Value.
    pub fn value(&self) -> &StatementValue {
        &self.value
    }

    /// Validity-time scope.
    pub fn validity_time(&self) -> ValidityTime {
        self.validity_time
    }

    /// Context scope.
    pub fn context(&self) -> &StatementContext {
        &self.context
    }

    /// Source binding.
    pub fn source(&self) -> &StatementSource {
        &self.source
    }

    /// Consume into the six typed fields.
    pub fn into_fields(
        self,
    ) -> (
        StatementSubject,
        StatementPredicate,
        StatementValue,
        ValidityTime,
        StatementContext,
        StatementSource,
    ) {
        (
            self.subject,
            self.predicate,
            self.value,
            self.validity_time,
            self.context,
            self.source,
        )
    }
}

/// Refusal at statement-body construction.
#[derive(Debug, Clone, PartialEq, Eq, Error, miette::Diagnostic)]
pub enum StatementRefuse {
    /// Predicate name was empty.
    #[error("EmptyPredicate: statement predicate must be non-empty")]
    #[diagnostic(code(data::statement::empty_predicate))]
    EmptyPredicate,
}

/// ONTOK constructions over the statement kernel.
///
/// Each function returns `(OntokKind, StatementBody)` for the one private
/// KyzoRecord. There is no `EntityRecord` / `ClaimRecord` second type.
pub mod construct {
    use super::*;

    fn body(
        subject: StatementSubject,
        predicate: StatementPredicate,
        value: StatementValue,
        validity_time: ValidityTime,
        context: StatementContext,
        source: StatementSource,
    ) -> StatementBody {
        StatementBody::new(subject, predicate, value, validity_time, context, source)
    }

    /// Entity: subject exists and is identifiable.
    pub fn entity(
        subject: StatementSubject,
        validity_time: ValidityTime,
        context: StatementContext,
        source: StatementSource,
    ) -> Result<(OntokKind, StatementBody), StatementRefuse> {
        let predicate = StatementPredicate::new("ontok:entity")?;
        let value = StatementValue::new(DataValue::Bool(true));
        Ok((
            OntokKind::Entity,
            body(subject, predicate, value, validity_time, context, source),
        ))
    }

    /// Claim: subject / domain-predicate / value assertion.
    pub fn claim(
        subject: StatementSubject,
        predicate: StatementPredicate,
        value: StatementValue,
        validity_time: ValidityTime,
        context: StatementContext,
        source: StatementSource,
    ) -> (OntokKind, StatementBody) {
        (
            OntokKind::Claim,
            body(subject, predicate, value, validity_time, context, source),
        )
    }

    /// Evidence: source-support statement (coordinates live on KyzoRecord.evidence).
    pub fn evidence(
        subject: StatementSubject,
        value: StatementValue,
        validity_time: ValidityTime,
        context: StatementContext,
        source: StatementSource,
    ) -> Result<(OntokKind, StatementBody), StatementRefuse> {
        let predicate = StatementPredicate::new("ontok:evidence")?;
        Ok((
            OntokKind::Evidence,
            body(subject, predicate, value, validity_time, context, source),
        ))
    }

    /// Relation: typed connection (predicate names the edge kind).
    pub fn relation(
        subject: StatementSubject,
        predicate: StatementPredicate,
        value: StatementValue,
        validity_time: ValidityTime,
        context: StatementContext,
        source: StatementSource,
    ) -> (OntokKind, StatementBody) {
        (
            OntokKind::Relation,
            body(subject, predicate, value, validity_time, context, source),
        )
    }

    /// State: scoped truth over validity-time.
    pub fn state(
        subject: StatementSubject,
        predicate: StatementPredicate,
        value: StatementValue,
        validity_time: ValidityTime,
        context: StatementContext,
        source: StatementSource,
    ) -> (OntokKind, StatementBody) {
        (
            OntokKind::State,
            body(subject, predicate, value, validity_time, context, source),
        )
    }

    /// Event: something that happened at a validity instant/interval.
    pub fn event(
        subject: StatementSubject,
        predicate: StatementPredicate,
        value: StatementValue,
        validity_time: ValidityTime,
        context: StatementContext,
        source: StatementSource,
    ) -> (OntokKind, StatementBody) {
        (
            OntokKind::Event,
            body(subject, predicate, value, validity_time, context, source),
        )
    }

    /// Role: contextual participation.
    pub fn role(
        subject: StatementSubject,
        predicate: StatementPredicate,
        value: StatementValue,
        validity_time: ValidityTime,
        context: StatementContext,
        source: StatementSource,
    ) -> (OntokKind, StatementBody) {
        (
            OntokKind::Role,
            body(subject, predicate, value, validity_time, context, source),
        )
    }

    /// Context: durable scope declaration.
    pub fn context_record(
        subject: StatementSubject,
        value: StatementValue,
        validity_time: ValidityTime,
        context: StatementContext,
        source: StatementSource,
    ) -> Result<(OntokKind, StatementBody), StatementRefuse> {
        let predicate = StatementPredicate::new("ontok:context")?;
        Ok((
            OntokKind::Context,
            body(subject, predicate, value, validity_time, context, source),
        ))
    }

    /// Concept: reusable classification.
    pub fn concept(
        subject: StatementSubject,
        predicate: StatementPredicate,
        value: StatementValue,
        validity_time: ValidityTime,
        context: StatementContext,
        source: StatementSource,
    ) -> (OntokKind, StatementBody) {
        (
            OntokKind::Concept,
            body(subject, predicate, value, validity_time, context, source),
        )
    }

    /// Rule: behavior / derivation / governance.
    pub fn rule(
        subject: StatementSubject,
        predicate: StatementPredicate,
        value: StatementValue,
        validity_time: ValidityTime,
        context: StatementContext,
        source: StatementSource,
    ) -> (OntokKind, StatementBody) {
        (
            OntokKind::Rule,
            body(subject, predicate, value, validity_time, context, source),
        )
    }

    /// Derivation: derived knowledge linked to inputs (inputs via provenance later).
    pub fn derivation(
        subject: StatementSubject,
        predicate: StatementPredicate,
        value: StatementValue,
        validity_time: ValidityTime,
        context: StatementContext,
        source: StatementSource,
    ) -> (OntokKind, StatementBody) {
        (
            OntokKind::Derivation,
            body(subject, predicate, value, validity_time, context, source),
        )
    }

    /// Invalidation: supersession without overwrite (seat 34 — T4 owns replay law).
    pub fn invalidation(
        subject: StatementSubject,
        value: StatementValue,
        validity_time: ValidityTime,
        context: StatementContext,
        source: StatementSource,
    ) -> Result<(OntokKind, StatementBody), StatementRefuse> {
        let predicate = StatementPredicate::new("ontok:invalidation")?;
        Ok((
            OntokKind::Invalidation,
            body(subject, predicate, value, validity_time, context, source),
        ))
    }
}
