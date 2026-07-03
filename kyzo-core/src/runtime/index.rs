/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Shared vocabulary of the index-operator engines (HNSW, MinHash-LSH, FTS).
//!
//! One concept lives here rather than in any single engine: [`IndexRowCorrupt`],
//! the typed error every engine raises when stored index bytes (or a base row
//! an index points at) do not decode as the format says they must. It extends
//! the kernel's corruption doctrine — corruption is an error, never a panic —
//! into every index read path. The three engines all decode stored rows; they
//! all name this one error, so it is defined once, here.

use miette::Diagnostic;
use thiserror::Error;

use crate::data::value::DataValue;

/// A stored index row (or a base row an index points at) failed to decode as
/// what the index format says it must be. Corruption is an error, never a
/// panic. Carries the row's key context so the failure is locatable.
#[derive(Debug, Error, Diagnostic)]
#[error("index '{index}' corrupt: {reason}; row {row}")]
#[diagnostic(code(index::corrupt))]
#[diagnostic(help(
    "the stored bytes do not decode as a valid index row; the index can be \
     dropped and re-created from its base relation"
))]
pub(crate) struct IndexRowCorrupt {
    pub(crate) index: String,
    pub(crate) row: String,
    pub(crate) reason: String,
}

impl IndexRowCorrupt {
    pub(crate) fn new(index: &str, row: &[DataValue], reason: impl Into<String>) -> Self {
        IndexRowCorrupt {
            index: index.to_string(),
            row: format!("{row:?}"),
            reason: reason.into(),
        }
    }
}
