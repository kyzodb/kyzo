/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Closed six-dimension projection set for KyzoRecord lowering (#268 T2/T3).
//!
//! A statement inherently has these dimensions — they are not a per-write
//! menu. Kind type-entails which subset is produced; this module owns the
//! closed universe and the lowered-row currency. Derivation lives at the
//! admission seat ([`crate::session::admit::lowering`]).
//!
//! Every lowered row carries its source [`RecordId`] — projections are
//! rebuildable accelerators, never a second truth (#268 T3).

use std::fmt;

use crate::session::record_id::RecordId;

/// The closed set of dimensions a typed statement can project into.
///
/// ```text
/// identity              which thing this is about
/// relationship          how it connects to other things
/// similarity            what it is like in meaning
/// quantity-and-location magnitudes and place
/// time                  when it holds
/// source                who asserted it, on what basis
/// ```
///
/// Ord order is the canonical lowering order (stable, schema-driven).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum StatementDimension {
    /// Relational rows / keys — subject identity.
    Identity = 1,
    /// Graph edges — typed connections.
    Relationship = 2,
    /// Vector / text meaning surfaces.
    Similarity = 3,
    /// Scalar magnitudes and spatial place.
    QuantityAndLocation = 4,
    /// Validity-time axis.
    Time = 5,
    /// Provenance / source-authority links.
    Source = 6,
}

impl StatementDimension {
    /// The closed universe — every dimension a statement may ever project.
    pub const ALL: [StatementDimension; 6] = [
        StatementDimension::Identity,
        StatementDimension::Relationship,
        StatementDimension::Similarity,
        StatementDimension::QuantityAndLocation,
        StatementDimension::Time,
        StatementDimension::Source,
    ];

    /// Stable wire tag (matches [`StatementDimension`] discriminant).
    pub fn tag(self) -> u8 {
        self as u8
    }
}

impl fmt::Display for StatementDimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            StatementDimension::Identity => "identity",
            StatementDimension::Relationship => "relationship",
            StatementDimension::Similarity => "similarity",
            StatementDimension::QuantityAndLocation => "quantity-and-location",
            StatementDimension::Time => "time",
            StatementDimension::Source => "source",
        })
    }
}

/// One dimension's lowered projection row — canonical bytes, not a second truth.
///
/// `source` is the admitted [`RecordId`] this row derives from. A projection
/// row without a source RecordId is unrepresentable (#268 T3).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LoweredRow {
    dimension: StatementDimension,
    /// Source record this projection row resolves to.
    source: RecordId,
    /// Canonical encoding of the dimension's row tuple (value-plane law).
    bytes: Vec<u8>,
}

impl LoweredRow {
    /// Assemble a lowered row from an already-encoded payload + source RecordId.
    pub(crate) fn new(dimension: StatementDimension, source: RecordId, bytes: Vec<u8>) -> Self {
        Self {
            dimension,
            source,
            bytes,
        }
    }

    /// Which closed dimension this row serves.
    pub fn dimension(&self) -> StatementDimension {
        self.dimension
    }

    /// Source [`RecordId`] this projection row resolves to.
    pub fn source_record_id(&self) -> RecordId {
        self.source
    }

    /// Canonical row bytes for equality / rebuild proofs.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Complete type-entailed lowering of one KyzoRecord.
///
/// Rows are ordered by [`StatementDimension`] Ord. Equality is byte/row
/// identity of the derived set — recomputed each call, never memoized.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RecordLowering {
    rows: Vec<LoweredRow>,
}

impl RecordLowering {
    /// Assemble from rows already ordered by dimension.
    pub(crate) fn from_ordered_rows(rows: Vec<LoweredRow>) -> Self {
        debug_assert!(
            rows.windows(2).all(|w| w[0].dimension() < w[1].dimension()),
            "lowering rows must be strictly ordered by StatementDimension"
        );
        Self { rows }
    }

    /// Borrow the ordered lowered rows.
    pub fn rows(&self) -> &[LoweredRow] {
        &self.rows
    }

    /// Dimensions present in this lowering (type-entailed subset).
    pub fn dimensions(&self) -> impl Iterator<Item = StatementDimension> + '_ {
        self.rows.iter().map(LoweredRow::dimension)
    }

    /// Source [`RecordId`] shared by every row in this lowering.
    ///
    /// Empty lowering has no source — callers must not treat absence as a
    /// free-floating projection (#268 T3).
    pub fn source_record_id(&self) -> Option<RecordId> {
        self.rows.first().map(LoweredRow::source_record_id)
    }

    /// Concatenated row bytes (dimension tag + payload per row) for
    /// byte-identical equality proofs across independent lowerings.
    pub fn concatenated_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for row in &self.rows {
            out.push(row.dimension.tag());
            out.extend_from_slice(row.source.as_bytes());
            let len = row.bytes.len() as u64;
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(&row.bytes);
        }
        out
    }
}
