/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Integrity verification: walk the whole store and verify every invariant
//! that can be checked offline. A suspect store gets a *report*, not a chain
//! of mystery query failures — corruption is diagnosed, never discovered.
//!
//! This is CATALOG-AWARE. The store holds three distinct value formats, and a
//! verifier that decoded them all one way would either false-flag a healthy
//! store or, worse, silently pass corruption — a partial verifier reported as
//! "storage verification" is worse than none. So it reconstructs the relation
//! taxonomy from the catalog (which sorts first, under `RelationId::SYSTEM`)
//! and decodes each stored VALUE in its true codec:
//!
//! - **Catalog** (`RelationId::SYSTEM`, id 0): each relation handle's value is
//!   a msgpack `CatalogRecord`, verified by decoding it — that decode also
//!   builds the taxonomy (every relation's id + name, and which relations are
//!   index backings via the `{base}:{index}` naming convention).
//! - **Base-relation rows**: value is `[polarity byte][canonical non-key
//!   columns]` (bitemporal), verified through the polarity-aware decoder.
//! - **Index-backing rows** (HNSW/FTS/LSH/spatial/sparse/temporal): value is a
//!   plain canonical tuple (rebuildable current state, no bitemporal polarity),
//!   verified through the canonical decoder.
//!
//! Per pair: key decodes, VALUE decodes in its format, and keys arrive in
//! strictly ascending order (a store-level ordering violation means the KV
//! engine itself is unwell). A data entry for a relation absent from the
//! catalog is a dangling-data corruption, reported too.

use std::collections::{BTreeMap, BTreeSet};

use fjall::Slice;
use miette::Result;

use crate::data::bitemporal::{claim_polarity_of_value, extend_tuple_from_bitemporal_v};
use crate::data::value::{DataValue, RelationId, decode_tuple_from_key, extend_tuple_from_v};
use crate::runtime::relation::RelationHandle;
use crate::storage::{ReadTx, Storage};

/// Cap on recorded corrupt entries: the report proves and locates corruption
/// without itself growing unboundedly on a badly damaged store.
const MAX_RECORDED: usize = 100;

/// One corrupt pair: where and why.
#[derive(Debug)]
pub struct CorruptEntry {
    /// The raw key, hex-encoded, truncated to 64 bytes.
    pub key_hex: String,
    /// What failed to decode.
    pub error: String,
}

/// The result of a full-store verification walk.
#[derive(Debug, Default)]
pub struct VerifyReport {
    /// Total key-value pairs examined.
    pub checked: u64,
    /// Pairs whose key or value failed to decode (capped; see `truncated`).
    pub corrupt: Vec<CorruptEntry>,
    /// Count of adjacent key pairs violating ascending order.
    pub ordering_violations: u64,
    /// True if more corrupt entries existed than were recorded.
    pub truncated: bool,
}

impl VerifyReport {
    /// A store passes verification iff nothing was found.
    pub fn is_clean(&self) -> bool {
        self.corrupt.is_empty() && self.ordering_violations == 0 && !self.truncated
    }
}

fn hex_prefix(bytes: &[u8]) -> String {
    let take = bytes.len().min(64);
    let mut s = String::with_capacity(take * 2 + 1);
    for b in &bytes[..take] {
        s.push_str(&format!("{b:02x}"));
    }
    if bytes.len() > 64 {
        s.push('…');
    }
    s
}

/// The storage kind of an entry, resolved from the catalog: the TYPE that
/// decides which value codec verifies a row. Every stored value is one of
/// these three formats; the verifier dispatches by `match` on this kind, never
/// by a string test or set membership at the decode site.
#[derive(Clone, Copy)]
enum RelKind {
    /// `RelationId::SYSTEM`: a msgpack `CatalogRecord` (or the id-counter).
    Catalog,
    /// A base relation's rows: `[polarity byte][canonical non-key columns]`.
    BitemporalData,
    /// An index backing's rows: a plain canonical tuple (rebuildable current
    /// state, no bitemporal polarity).
    IndexInternal,
}

/// Verify one catalog (`SYSTEM`) entry and fold it into the typed taxonomy
/// (`RelationId` → `RelKind`), which is the ONLY thing the per-row value
/// dispatch consults. A relation-handle value (Str-named key) must decode as a
/// `CatalogRecord`; doing so classifies its relation and records its index
/// backings. The id-counter (Null-named key) is an internal scalar,
/// key-verified only.
///
/// `index_backings` is a string set ONLY because the catalog links a base to
/// its index by NAME (`{base}:{index}`) — reading that link is a catalog-name
/// boundary, resolved here ONCE. It never reaches the dispatch site: a base
/// always sorts before its backing, so by the time a backing handle is
/// decoded its name is already present, and the classification result is
/// immediately a typed `RelKind` in `taxonomy`. Verification behaviour is
/// controlled by that typed map, not by string membership.
fn verify_catalog_entry(
    key_cols: &[DataValue],
    val: &[u8],
    taxonomy: &mut BTreeMap<RelationId, RelKind>,
    index_backings: &mut BTreeSet<String>,
) -> Option<String> {
    match key_cols.first() {
        Some(DataValue::Str(_)) => match RelationHandle::decode(val) {
            Ok(h) => {
                for ix in &h.indices {
                    index_backings.insert(ix.relation_name(&h.name));
                }
                let kind = if index_backings.contains(h.name.as_str()) {
                    RelKind::IndexInternal
                } else {
                    RelKind::BitemporalData
                };
                taxonomy.insert(h.id, kind);
                None
            }
            Err(e) => Some(format!("catalog record: {e}")),
        },
        Some(DataValue::Null) => None,
        other => Some(format!("unrecognized system key column: {other:?}")),
    }
}

/// Walk every pair in the store and verify key decodability, VALUE
/// decodability in each entry's true codec (catalog-aware), and ordering.
///
/// Read-only, snapshot-consistent, and total: corrupt pairs are recorded and
/// the walk continues — one bad page must not hide the rest of the damage.
pub fn verify_storage<S: Storage>(db: &S) -> Result<VerifyReport> {
    let tx = db.read_tx()?;
    let mut report = VerifyReport::default();
    let mut prev_key: Option<Slice> = None;

    // The typed taxonomy, reconstructed from the catalog as the ordered scan
    // crosses `RelationId::SYSTEM` (id 0, which sorts before every data
    // relation): `RelationId` → its typed `RelKind`. Complete before the first
    // non-system entry, and the SOLE input to per-row value dispatch.
    // `index_backings` is the build-time catalog-name cross-reference (see
    // `verify_catalog_entry`), never consulted at dispatch.
    let mut taxonomy: BTreeMap<RelationId, RelKind> = BTreeMap::new();
    let mut index_backings: BTreeSet<String> = BTreeSet::new();

    for pair in tx.total_scan() {
        let (k, v) = pair?;
        report.checked += 1;

        if let Some(prev) = &prev_key
            && k.as_slice() <= prev.as_slice()
        {
            report.ordering_violations += 1;
        }

        // The KEY is uniform across every entry kind (relation prefix +
        // canonical column bytes + fixed-width bitemporal tail), so a
        // non-decoding key is unambiguous structural corruption. Resolve the
        // entry to a typed `RelKind`, then verify its value in that kind's
        // codec — the dispatch is a `match` on the type, never a string test.
        let error: Option<String> = match decode_tuple_from_key(&k, 16) {
            Err(e) => Some(e.to_string()),
            Ok(mut tup) => match RelationId::raw_decode(&k) {
                Err(e) => Some(e.to_string()),
                Ok(rel) => {
                    let kind = if rel == RelationId::SYSTEM {
                        Some(RelKind::Catalog)
                    } else {
                        taxonomy.get(&rel).copied()
                    };
                    match kind {
                        None => Some(format!(
                            "dangling data: no catalog entry for relation id {}",
                            rel.raw()
                        )),
                        Some(RelKind::Catalog) => {
                            verify_catalog_entry(&tup, &v, &mut taxonomy, &mut index_backings)
                        }
                        Some(RelKind::BitemporalData) => claim_polarity_of_value(&v)
                            .and_then(|_| extend_tuple_from_bitemporal_v(&mut tup, &v))
                            .err()
                            .map(|e| e.to_string()),
                        Some(RelKind::IndexInternal) => extend_tuple_from_v(&mut tup, &v)
                            .err()
                            .map(|e| e.to_string()),
                    }
                }
            },
        };

        if let Some(error) = error {
            if report.corrupt.len() < MAX_RECORDED {
                report.corrupt.push(CorruptEntry {
                    key_hex: hex_prefix(&k),
                    error,
                });
            } else {
                report.truncated = true;
            }
        }
        prev_key = Some(k);
    }
    Ok(report)
}
