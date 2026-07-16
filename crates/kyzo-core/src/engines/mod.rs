/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Index-operator engines (HNSW, MinHash-LSH, FTS, and kin) and their shared
//! corruption boundary.
//!
//! [`IndexRowCorrupt`] is the typed error every engine raises when stored
//! index bytes (or a base row an index points at) do not decode as the
//! format says they must. It extends the kernel's corruption doctrine —
//! corruption is an error, never a panic — into every index read path.
//!
//! Projection freshness and staged construction are the shared
//! [`projection`] machine (story #305): [`projection::ProjectionBuilder`]
//! seals into generation-carrying [`projection::Sealed`]; staleness is
//! [`projection::Stale`], not an `Option` from a get-shaped call. Kind
//! engines re-land as `K` parameterizations of that machine (T3).

// `fts`/`hnsw`/`lsh`'s `db.rs` surface has landed (`::fts|hnsw|lsh
// create/drop` in `runtime/mutate.rs` dispatch to the real creation/
// backfill tier, tested end to end in `runtime/db.rs`), so neither carries
// `#[allow(dead_code)]` any more. `gazetteer`/`sparse`/`spatial` below have
// no `db.rs` surface yet and are still lib-dead by construction; their
// in-file tests keep them live under test, so a plain `allow` covers them.
pub(crate) mod fts;
/// Generic build→seal→query machine for every projection kind (story #305).
pub(crate) mod projection;
// gazetteer has no `db.rs` surface yet (see the block comment above);
// its in-file tests keep it live under test, so a plain `allow` covers it.
#[allow(dead_code)]
pub(crate) mod gazetteer;
#[cfg(test)]
mod gazetteer_hostile;
pub(crate) mod hnsw;
pub(crate) mod lsh;
/// Columnar current-state segments: rebuildable typed-column mirrors of a
/// relation's plain scan. Runtime watermark equality freshness was
/// demolished (story #305). `#[allow(dead_code)]` covers surfaces whose
/// callers are severed until freshness is re-seated.
#[allow(dead_code)]
pub(crate) mod segments;
// sparse has no `db.rs` surface yet (see the block comment above); its
// in-file tests keep it live under test, so a plain `allow` covers it.
#[allow(dead_code)]
pub(crate) mod sparse;
#[cfg(test)]
mod sparse_hostile;
// spatial has no `db.rs` surface yet (see the block comment above); its
// in-file tests keep it live under test, so a plain `allow` covers it.
#[allow(dead_code)]
pub(crate) mod spatial;
/// Text analysis (tokenizers, filters, the tantivy-derived pipeline) —
/// the engines' shared linguistic plumbing. Carries surface its future
/// consumers (config-driven tokenizer caches) haven't landed for; the
/// in-file tests keep it live under test.
#[allow(dead_code)]
pub(crate) mod text;

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

/// Wrap a scanned index-row stream so a codec refusal surfaces as this
/// index's OWN typed [`IndexRowCorrupt`], never a bare
/// [`DecodeError`](crate::data::value::DecodeError). Storage/IO errors are
/// NOT corruption and pass through unchanged (distinguished by diagnostic
/// code `value::decode`). Every engine consumes an index scan through this
/// boundary, so a raw codec error cannot leak out of an engine as its
/// contract.
pub(crate) fn index_rows<'a>(
    index_name: &'a str,
    scan: impl Iterator<Item = miette::Result<crate::data::value::Tuple>> + 'a,
) -> impl Iterator<Item = miette::Result<crate::data::value::Tuple>> + 'a {
    scan.map(move |r| {
        r.map_err(|e| {
            if e.code().is_some_and(|c| c.to_string() == "value::decode") {
                IndexRowCorrupt::from_decode(index_name, e).into()
            } else {
                e
            }
        })
    })
}

impl IndexRowCorrupt {
    pub(crate) fn new(index: &str, row: &[DataValue], reason: impl Into<String>) -> Self {
        IndexRowCorrupt {
            index: index.to_string(),
            row: format!("{row:?}"),
            reason: reason.into(),
        }
    }

    /// The codec refused a scanned index row's stored bytes before they
    /// could become a tuple: wrap that raw decode failure as this index's
    /// own typed corruption error, so a `DecodeError` never leaks out of
    /// an engine as the engine's contract.
    pub(crate) fn from_decode(index: &str, err: impl std::fmt::Display) -> Self {
        IndexRowCorrupt {
            index: index.to_string(),
            row: "<undecodable bytes>".to_string(),
            reason: format!("stored row bytes did not decode: {err}"),
        }
    }
}
