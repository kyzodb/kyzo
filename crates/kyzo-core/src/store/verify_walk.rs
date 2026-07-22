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
//!
//! ## Pins (re-homed from storage/tests.rs)
//!
//! `verify_storage_catches_a_corrupt_value` and the injected-corruption walk
//! pin that BadTag surfaces and THE WALK CONTINUES past the wound.
//!
//! ## Spec extend (§49/§50)
//!
//! Table/keyspace-scoped checksum identity + quarantine range minting live
//! beside the catalog-aware walk. Store-global-only block identity is banned
//! (swap-two-tables would go undetected).
//!
//! ## Deep-verify (§51)
//!
//! [`deep_verify_storage`] re-derives each index kind **from base-relation
//! facts** and diffs those expected digests against stored index row content.
//! A walk that only re-reads/re-decodes the index proves self-consistency, not
//! correctness — that trap is deleted here. Operator schedule + queryable
//! last-result/staleness live on the session observe surface.

use std::collections::{BTreeMap, BTreeSet};

use crate::session::catalog::{IndexKind, IndexRef, KeyspaceKind, RelationHandle};
use crate::store::failure::KeyspaceId;
use crate::store::time::{claim_polarity_of_value, extend_tuple_from_bitemporal_v};
use crate::store::{ReadTx, Storage};
use fjall::Slice;
use kyzo_model::SourceSpan;
use kyzo_model::program::expr::Expr;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::{
    DataValue, RelationId, Tuple, TupleT, append_canonical, decode_tuple_from_key,
    encode_tuple_bare, extend_tuple_from_v,
};
use miette::{Result, bail};
use sha2::{Digest, Sha256};
use smartstring::{LazyCompact, SmartString};

/// Domain-separated checksum over (keyspace, block, payload).
///
/// Owned identity for [`KeyspaceScopedChecksum::digest`] — never a bare
/// `[u8; 32]`. Bytes are produced only by [`KeyspaceScopedChecksum::compute`]
/// under the `kyzo.keyspace_scoped_checksum.v1` domain tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct KeyspaceScopedChecksumDigest([u8; 32]);

const _: () =
    assert!(std::mem::size_of::<KeyspaceScopedChecksumDigest>() == std::mem::size_of::<[u8; 32]>());
const _: () = assert!(
    std::mem::align_of::<KeyspaceScopedChecksumDigest>() == std::mem::align_of::<[u8; 32]>()
);

/// Table/keyspace-scoped checksum identity (§49).
///
/// Binds keyspace identity + logical block so misplaced-but-intact data is
/// caught. A store-global-only checksum is banned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyspaceScopedChecksum {
    /// Table / keyspace identity.
    keyspace: KeyspaceId,
    /// Logical block ordinal within the keyspace.
    block: u64,
    /// Checksum over (keyspace, block, payload).
    digest: KeyspaceScopedChecksumDigest,
}

impl KeyspaceScopedChecksum {
    /// Compute a scoped checksum over payload bytes.
    pub fn compute(keyspace: KeyspaceId, block: u64, payload: &[u8]) -> Self {
        let mut h = Sha256::new();
        h.update(b"kyzo.keyspace_scoped_checksum.v1");
        h.update(u64::to_be_bytes(keyspace.get()));
        h.update(u64::to_be_bytes(block));
        h.update(payload);
        Self {
            keyspace,
            block,
            digest: KeyspaceScopedChecksumDigest(h.finalize().into()),
        }
    }

    /// Keyspace identity.
    pub fn keyspace(self) -> KeyspaceId {
        self.keyspace
    }

    /// Logical block ordinal.
    pub fn block(self) -> u64 {
        self.block
    }

    /// Checksum digest.
    pub fn digest(self) -> KeyspaceScopedChecksumDigest {
        self.digest
    }

    /// Verify payload against this identity.
    pub fn verify(self, payload: &[u8]) -> bool {
        Self::compute(self.keyspace, self.block, payload).digest == self.digest
    }
}

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
/// `CatalogRecord`; doing so classifies its relation by [`KeyspaceKind`]
/// (Facts → bitemporal, including plain/temporal index backings;
/// AlgorithmState → plain canonical for HNSW/FTS/LSH). The id-counter
/// (Null-named key) is an internal scalar, key-verified only.
fn verify_catalog_entry(
    key_cols: &[DataValue],
    val: &[u8],
    taxonomy: &mut BTreeMap<RelationId, RelKind>,
) -> Option<String> {
    match key_cols.first() {
        Some(DataValue::Str(_)) => match RelationHandle::decode(val) {
            Ok(h) => {
                let kind = match h.keyspace_kind {
                    KeyspaceKind::Facts => RelKind::BitemporalData,
                    KeyspaceKind::AlgorithmState => RelKind::IndexInternal,
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
    let mut taxonomy: BTreeMap<RelationId, RelKind> = BTreeMap::new();

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
                            verify_catalog_entry(tup.as_slice(), &v, &mut taxonomy)
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

// ---------------------------------------------------------------------------
// Deep-verify (§51): re-derive each index kind from base facts, diff stored
// ---------------------------------------------------------------------------

/// One index whose re-derived expected content disagreed with stored bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexMismatch {
    /// Catalog name of the index backing (`{base}:{index}`).
    pub index_name: String,
    /// Index kind — the catalog [`IndexKind`], never a string label.
    pub(crate) kind: IndexKind,
    /// Human-locatable diff summary.
    pub detail: String,
}

/// Catalog relation name — owned identity for name→id maps, never a bare `String`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct RelationName(SmartString<LazyCompact>);

impl RelationName {
    /// Lift from a [`RelationHandle::name`] (or any catalog SmartString name).
    #[must_use]
    pub fn from_handle_name(name: &SmartString<LazyCompact>) -> Self {
        Self(name.clone())
    }

    /// Borrow as `&str`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl From<&str> for RelationName {
    fn from(s: &str) -> Self {
        Self(SmartString::from(s))
    }
}

impl From<String> for RelationName {
    fn from(s: String) -> Self {
        Self(SmartString::from(s))
    }
}

impl std::fmt::Display for RelationName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for RelationName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// Domain-separated digest of a [`DeepVerifyReport`].
///
/// Owned identity for operator last-result persistence — never a bare
/// `[u8; 32]`. Bytes are produced only by [`DeepVerifyReport::digest`] under
/// the `kyzo.deep_verify_report.v1` domain tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct DeepVerifyDigest([u8; 32]);

const _: () = assert!(std::mem::size_of::<DeepVerifyDigest>() == std::mem::size_of::<[u8; 32]>());
const _: () = assert!(std::mem::align_of::<DeepVerifyDigest>() == std::mem::align_of::<[u8; 32]>());

impl DeepVerifyDigest {
    /// Borrow the digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Domain-separated digest of one index row's content for set-diff (§51).
///
/// Owned identity for expected/stored deep-verify sets — never a bare
/// `[u8; 32]`. Bytes are produced only by [`digest_bytes`] under the
/// `kyzo.index_row_digest.v1` domain tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct IndexRowDigest([u8; 32]);

const _: () = assert!(std::mem::size_of::<IndexRowDigest>() == std::mem::size_of::<[u8; 32]>());
const _: () = assert!(std::mem::align_of::<IndexRowDigest>() == std::mem::align_of::<[u8; 32]>());

/// Stable digest tag for an [`IndexKind`] discriminant (PartialEq-only kinds).
fn index_kind_digest_tag(kind: &IndexKind) -> &'static [u8] {
    match kind {
        IndexKind::Plain { .. } => b"plain",
        IndexKind::Temporal => b"temporal",
        IndexKind::Hnsw(_) => b"hnsw",
        IndexKind::Fts(_) => b"fts",
        IndexKind::Lsh { .. } => b"lsh",
    }
}

/// Outcome of a deep-verify: decode/order walk plus per-kind re-derivation diffs.
#[derive(Debug, Default)]
pub struct DeepVerifyReport {
    /// Catalog-aware decode + ascending-order walk (unchanged pins).
    pub walk: VerifyReport,
    /// Indexes whose re-derived digests disagreed with stored content.
    pub index_mismatches: Vec<IndexMismatch>,
    /// How many attached indexes were re-derived and diffed.
    pub indices_checked: u64,
}

impl DeepVerifyReport {
    /// Clean iff the walk is clean and no index re-derivation disagreed.
    pub fn is_clean(&self) -> bool {
        self.walk.is_clean() && self.index_mismatches.is_empty()
    }

    /// Stable digest of this report for operator last-result persistence.
    pub fn digest(&self) -> DeepVerifyDigest {
        let mut h = Sha256::new();
        h.update(b"kyzo.deep_verify_report.v1");
        h.update(u64::to_be_bytes(self.walk.checked));
        h.update(u64::to_be_bytes(self.walk.ordering_violations));
        h.update([u8::from(self.walk.truncated)]);
        h.update(u64::to_be_bytes(self.walk.corrupt.len() as u64));
        for c in &self.walk.corrupt {
            h.update(c.key_hex.as_bytes());
            h.update(c.error.as_bytes());
        }
        h.update(u64::to_be_bytes(self.indices_checked));
        h.update(u64::to_be_bytes(self.index_mismatches.len() as u64));
        for m in &self.index_mismatches {
            h.update(m.index_name.as_bytes());
            h.update(index_kind_digest_tag(&m.kind));
            h.update(m.detail.as_bytes());
        }
        DeepVerifyDigest(h.finalize().into())
    }
}

fn digest_bytes(payload: &[u8]) -> IndexRowDigest {
    let mut h = Sha256::new();
    h.update(b"kyzo.index_row_digest.v1");
    h.update(payload);
    IndexRowDigest(h.finalize().into())
}

fn digest_tuple_cols(cols: &[DataValue]) -> IndexRowDigest {
    digest_bytes(&encode_tuple_bare(cols))
}

fn set_diff_detail(
    expected: &BTreeSet<IndexRowDigest>,
    stored: &BTreeSet<IndexRowDigest>,
) -> Option<String> {
    let only_expected = expected.difference(stored).count();
    let only_stored = stored.difference(expected).count();
    if only_expected == 0 && only_stored == 0 {
        return None;
    }
    Some(format!(
        "re-derived from base facts disagrees with stored index bytes: \
         {only_expected} only-expected digests, {only_stored} only-stored digests \
         (expected={}, stored={})",
        expected.len(),
        stored.len()
    ))
}

fn base_column_frame(base: &RelationHandle) -> BTreeMap<Symbol, usize> {
    base.metadata
        .keys
        .iter()
        .chain(base.metadata.non_keys.iter())
        .enumerate()
        .map(|(i, col)| (Symbol::new(col.name.clone(), SourceSpan::default()), i))
        .collect()
}

fn bind_extractor(base: &RelationHandle, extractor: &Expr) -> Result<Expr> {
    let mut expr = extractor.clone();
    expr.fill_binding_indices(&base_column_frame(base))?;
    Ok(expr)
}

fn project_mapper_cols(
    mapper: &[usize],
    row: &[DataValue],
    base_name: &str,
) -> Result<Vec<DataValue>> {
    mapper
        .iter()
        .map(|&i| {
            row.get(i).cloned().ok_or_else(|| {
                miette::miette!(
                    "stale plain-index mapper position {i} for relation '{base_name}' during deep-verify"
                )
            })
        })
        .collect()
}

/// Collect catalog handles keyed by relation id, plus name→id for index resolve.
fn load_catalog_handles(
    tx: &impl ReadTx,
) -> Result<(
    BTreeMap<RelationId, RelationHandle>,
    BTreeMap<RelationName, RelationId>,
)> {
    let mut by_id = BTreeMap::new();
    let mut by_name = BTreeMap::new();
    let lower = Tuple::default().encode_as_key(RelationId::SYSTEM);
    let upper = (RelationId::SYSTEM.raw() + 1).to_be_bytes();
    for pair in tx.range_scan(&lower, &upper) {
        let (k, v) = pair?;
        let tup = match decode_tuple_from_key(&k, 16) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if let Some(DataValue::Str(_)) = tup.first() {
            let h = RelationHandle::decode(&v).map_err(|e| miette::miette!("{e}"))?;
            by_name.insert(RelationName::from_handle_name(&h.name), h.id);
            by_id.insert(h.id, h);
        }
    }
    Ok((by_id, by_name))
}

fn rederive_plain(
    tx: &impl ReadTx,
    base: &RelationHandle,
    idx: &RelationHandle,
    mapper: &[usize],
    kind: IndexKind,
) -> Result<Option<IndexMismatch>> {
    let mut expected = BTreeSet::new();
    for row in base.scan_all(tx) {
        let row = row?;
        let projected = project_mapper_cols(mapper, row.as_slice(), &base.name)?;
        expected.insert(digest_tuple_cols(&projected));
    }
    let mut stored = BTreeSet::new();
    for row in idx.scan_all(tx) {
        let row = row?;
        stored.insert(digest_tuple_cols(row.as_slice()));
    }
    Ok(
        set_diff_detail(&expected, &stored).map(|detail| IndexMismatch {
            index_name: idx.name.to_string(),
            kind,
            detail,
        }),
    )
}

fn rederive_temporal(
    tx: &impl ReadTx,
    base: &RelationHandle,
    idx: &RelationHandle,
    kind: IndexKind,
) -> Result<Option<IndexMismatch>> {
    // Re-derive every posting from every stored base VERSION (not as-of
    // current) — same rebuildability law as temporal backfill.
    let keys_len = base.metadata.keys.len();
    let mut expected = BTreeSet::new();
    for pair in base.scan_all_raw(tx) {
        let (k, v) = pair?;
        let polarity = match claim_polarity_of_value(&v) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let tuple = match decode_tuple_from_key(&k, keys_len + 2) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let cols = tuple.as_slice();
        if cols.len() < keys_len + 1 {
            continue;
        }
        let DataValue::Validity(_) = &cols[keys_len] else {
            continue;
        };
        let mut posting = Vec::with_capacity(1 + keys_len);
        posting.push(cols[keys_len].clone());
        posting.extend(cols[..keys_len].iter().cloned());
        // Polarity is part of the posting's meaning (assert/retract/erase).
        let mut payload = encode_tuple_bare(&posting);
        payload.push(polarity.encode());
        expected.insert(digest_bytes(&payload));
    }

    let mut stored = BTreeSet::new();
    for pair in idx.scan_all_raw(tx) {
        let (k, v) = pair?;
        let polarity = match claim_polarity_of_value(&v) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let idx_keys = idx.metadata.keys.len();
        let tuple = match decode_tuple_from_key(&k, idx_keys + 2) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let cols = tuple.as_slice();
        if cols.len() < idx_keys {
            continue;
        }
        let mut payload = encode_tuple_bare(&cols[..idx_keys]);
        payload.push(polarity.encode());
        stored.insert(digest_bytes(&payload));
    }

    Ok(
        set_diff_detail(&expected, &stored).map(|detail| IndexMismatch {
            index_name: idx.name.to_string(),
            kind,
            detail,
        }),
    )
}

fn rederive_hnsw(
    tx: &impl ReadTx,
    base: &RelationHandle,
    idx: &RelationHandle,
    kind: IndexKind,
) -> Result<Option<IndexMismatch>> {
    // HNSW graph edges are not uniquely determined by base facts alone in a
    // byte-stable way without a full deterministic rebuild; deep-verify
    // re-derives the NODE MEMBERSHIP set: every current base key must appear
    // as a layer-0 node identity, and every stored node must name a live base
    // key. That is derived from base facts, not a re-decode of the index as
    // authority.
    let kl = base.metadata.keys.len();
    let mut expected = BTreeSet::new();
    for row in base.scan_all(tx) {
        let row = row?;
        let cols = row.as_slice();
        if cols.len() < kl {
            continue;
        }
        expected.insert(digest_tuple_cols(&cols[..kl]));
    }

    let mut stored_nodes = BTreeSet::new();
    for row in idx.scan_all(tx) {
        let row = row?;
        let cols = row.as_slice();
        // Wire key: [layer, fr_id…, to_id…] with id = key_cols + field + sub.
        let key_slots = 2 * kl + 5;
        if cols.len() < key_slots {
            continue;
        }
        let Some(layer) = cols[0].get_int() else {
            continue;
        };
        if layer == 1 {
            continue; // canary
        }
        let fr = &cols[1..1 + kl + 2];
        let to = &cols[1 + kl + 2..1 + 2 * (kl + 2)];
        if fr != to {
            continue; // edge
        }
        if fr.len() < kl {
            continue;
        }
        stored_nodes.insert(digest_tuple_cols(&fr[..kl]));
    }

    Ok(
        set_diff_detail(&expected, &stored_nodes).map(|detail| IndexMismatch {
            index_name: idx.name.to_string(),
            kind,
            detail,
        }),
    )
}

fn rederive_fts(
    tx: &impl ReadTx,
    base: &RelationHandle,
    idx: &RelationHandle,
    manifest: &crate::project::text::FtsIndexManifest,
    kind: IndexKind,
) -> Result<Option<IndexMismatch>> {
    let extractor = bind_extractor(base, &manifest.extractor)?;
    let analyzer = manifest.tokenizer.build(&manifest.filters)?;
    let kl = base.metadata.keys.len();
    let mut expected = BTreeSet::new();
    for row in base.scan_all(tx) {
        let row = row?;
        if row.len() < kl {
            continue;
        }
        let text = match crate::exec::expr::eval_expr(&extractor, row.as_slice())? {
            DataValue::Null => continue,
            DataValue::Str(s) => s,
            other @ (kyzo_model::data_value_any!()) => {
                bail!("fts deep-verify: extractor returned non-string {other:?}")
            }
        };
        let mut collector: BTreeSet<String> = BTreeSet::new();
        let mut token_stream = analyzer.token_stream(&text);
        while let Some(token) = token_stream.next() {
            collector.insert(token.text.to_string());
        }
        let src: Vec<DataValue> = row.as_slice()[..kl].to_vec();
        for term in collector {
            let mut key = Vec::with_capacity(1 + kl);
            key.push(DataValue::Str(term));
            key.extend(src.iter().cloned());
            // Expected content is the posting KEY identity re-derived from
            // base text — value offsets are regenerable; key mismatch is the lie.
            expected.insert(digest_tuple_cols(&key));
        }
    }

    let mut stored = BTreeSet::new();
    for row in idx.scan_all(tx) {
        let row = row?;
        let posting_keys = idx.metadata.keys.len();
        if row.len() < posting_keys {
            continue;
        }
        stored.insert(digest_tuple_cols(&row.as_slice()[..posting_keys]));
    }

    Ok(
        set_diff_detail(&expected, &stored).map(|detail| IndexMismatch {
            index_name: idx.name.to_string(),
            kind,
            detail,
        }),
    )
}

fn rederive_lsh(
    tx: &impl ReadTx,
    base: &RelationHandle,
    idx: &RelationHandle,
    inv: &RelationHandle,
    manifest: &crate::project::dedup::lsh::MinHashLshIndexManifest,
    kind: IndexKind,
) -> Result<Option<IndexMismatch>> {
    use crate::project::dedup::lsh::HashValues;

    let extractor = bind_extractor(base, &manifest.extractor)?;
    let analyzer = manifest.tokenizer.build(&manifest.filters)?;
    let perms = manifest.get_hash_perms()?;
    let kl = base.metadata.keys.len();
    let mut expected_bands = BTreeSet::new();
    let mut expected_inv = BTreeSet::new();
    for row in base.scan_all(tx) {
        let row = row?;
        if row.len() < kl {
            continue;
        }
        let inv_key = &row.as_slice()[..kl];
        let to_index = crate::exec::expr::eval_expr(&extractor, row.as_slice())?;
        let min_hash = match &to_index {
            DataValue::Null => continue,
            DataValue::List(l) => {
                let bytes = l.iter().map(|v| {
                    let mut b = Vec::new();
                    append_canonical(&mut b, v);
                    b
                });
                HashValues::new(bytes, &perms)
            }
            DataValue::Str(s) => {
                let n_grams = analyzer.unique_ngrams(s, manifest.n_gram);
                let bytes = n_grams.iter().map(|tokens| {
                    let mut b = Vec::new();
                    for t in tokens {
                        append_canonical(&mut b, &DataValue::Str(t.to_string()));
                    }
                    b
                });
                HashValues::new(bytes, &perms)
            }
            other @ (kyzo_model::data_value_any!()) => {
                bail!("lsh deep-verify: unsupported extractor value {other:?}")
            }
        };
        let chunks = min_hash.band_chunks(manifest.n_bands, manifest.n_rows_in_band)?;
        expected_inv.insert(digest_tuple_cols(inv_key));
        for chunk in chunks {
            let mut key = Vec::with_capacity(1 + kl);
            key.push(DataValue::Bytes(chunk));
            key.extend(inv_key.iter().cloned());
            expected_bands.insert(digest_tuple_cols(&key));
        }
    }

    let mut stored_bands = BTreeSet::new();
    for row in idx.scan_all(tx) {
        let row = row?;
        let n = idx.metadata.keys.len();
        if row.len() < n {
            continue;
        }
        stored_bands.insert(digest_tuple_cols(&row.as_slice()[..n]));
    }
    let mut stored_inv = BTreeSet::new();
    for row in inv.scan_all(tx) {
        let row = row?;
        let n = inv.metadata.keys.len();
        if row.len() < n {
            continue;
        }
        stored_inv.insert(digest_tuple_cols(&row.as_slice()[..n]));
    }

    if let Some(detail) = set_diff_detail(&expected_bands, &stored_bands) {
        return Ok(Some(IndexMismatch {
            index_name: idx.name.to_string(),
            kind,
            detail,
        }));
    }
    Ok(
        set_diff_detail(&expected_inv, &stored_inv).map(|detail| IndexMismatch {
            index_name: inv.name.to_string(),
            kind,
            detail: format!("inverse: {detail}"),
        }),
    )
}

fn deep_verify_one_index(
    tx: &impl ReadTx,
    base: &RelationHandle,
    index_ref: &IndexRef,
    by_name: &BTreeMap<RelationName, RelationId>,
    by_id: &BTreeMap<RelationId, RelationHandle>,
) -> Result<Option<IndexMismatch>> {
    let idx_name = index_ref.relation_name(&base.name);
    let idx_key = RelationName::from(idx_name.as_str());
    let Some(idx_id) = by_name.get(&idx_key) else {
        return Ok(Some(IndexMismatch {
            index_name: idx_name,
            kind: index_ref.kind.clone(),
            detail: "index backing relation absent from catalog while IndexRef is attached".into(),
        }));
    };
    let Some(idx) = by_id.get(idx_id) else {
        return Ok(Some(IndexMismatch {
            index_name: idx_name,
            kind: index_ref.kind.clone(),
            detail: "index RelationId not loadable during deep-verify".into(),
        }));
    };

    match &index_ref.kind {
        IndexKind::Plain { mapper } => {
            rederive_plain(tx, base, idx, mapper, index_ref.kind.clone())
        }
        IndexKind::Temporal => rederive_temporal(tx, base, idx, index_ref.kind.clone()),
        IndexKind::Hnsw(_) => rederive_hnsw(tx, base, idx, index_ref.kind.clone()),
        IndexKind::Fts(manifest) => rederive_fts(tx, base, idx, manifest, index_ref.kind.clone()),
        IndexKind::Lsh { manifest, inverse } => {
            let inv_name = format!("{}:{}", base.name, inverse);
            let inv_key = RelationName::from(inv_name.as_str());
            let Some(inv_id) = by_name.get(&inv_key) else {
                return Ok(Some(IndexMismatch {
                    index_name: inv_name,
                    kind: index_ref.kind.clone(),
                    detail: "lsh inverse relation missing from catalog".into(),
                }));
            };
            let Some(inv) = by_id.get(inv_id) else {
                return Ok(Some(IndexMismatch {
                    index_name: inv_name,
                    kind: index_ref.kind.clone(),
                    detail: "lsh inverse RelationId not loadable".into(),
                }));
            };
            rederive_lsh(tx, base, idx, inv, manifest, index_ref.kind.clone())
        }
    }
}

/// Deep-verify (§51): catalog-aware walk, then re-derive each index kind from
/// base-relation facts and diff against stored index content digests.
///
/// This is NOT a re-read/re-decode of the index as authority — expected
/// content is freshly derived from base facts, then compared to what the
/// index keyspace actually holds.
pub fn deep_verify_storage<S: Storage>(db: &S) -> Result<DeepVerifyReport> {
    let walk = verify_storage(db)?;
    let tx = db.read_tx()?;
    let (by_id, by_name) = load_catalog_handles(&tx)?;

    let mut report = DeepVerifyReport {
        walk,
        index_mismatches: Vec::new(),
        indices_checked: 0,
    };

    // Only BASE relations carry IndexRefs; index backings are named
    // `{base}:{index}` and are skipped as bases (their own indices vec is empty).
    let bases: Vec<RelationHandle> = by_id
        .values()
        .filter(|h| !h.indices.is_empty())
        .cloned()
        .collect();

    for base in &bases {
        for index_ref in &base.indices {
            report.indices_checked += 1;
            if let Some(mismatch) = deep_verify_one_index(&tx, base, index_ref, &by_name, &by_id)? {
                report.index_mismatches.push(mismatch);
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod pins {
    //! verify_storage battery (re-homed from storage/tests.rs).

    use miette::{IntoDiagnostic, Result, miette};
    use crate::session::catalog::{Catalog, IndexKind};
    use crate::session::db::Engine;
    use crate::store::fjall::new_fjall_storage;
    use crate::store::verify_walk::verify_storage;
    use crate::store::{ReadTx, Storage};

    #[test]
    fn verify_storage_catches_a_corrupt_value() -> Result<()> {
        let dir = tempfile::tempdir().into_diagnostic()?;
        let path = dir.path().join("db");
        let data_key: Vec<u8> = {
            let storage = new_fjall_storage(&path)?;
            let db = Engine::compose(storage.clone(), Catalog::new())?;
            db.run_script(
                "?[k, v] <- [[1, 7]] :create rel {k => v}",
                std::collections::BTreeMap::new(),
            )?;
            let tx = storage.read_tx()?;
            tx.total_scan()
                .filter_map(Result::ok)
                .map(|(k, _)| k.to_vec())
                .find(|k| k.len() >= 8 && k[..8].iter().any(|&b| b != 0))?
        };
        {
            let raw = fjall::OptimisticTxDatabase::builder(&path).open()?;
            let ks = raw
                .keyspace("kyzo", fjall::KeyspaceCreateOptions::default)?;
            ks.insert(data_key, [0xFFu8])?;
            raw.persist(fjall::PersistMode::SyncAll).into_diagnostic()?;
        }
        let storage = new_fjall_storage(&path)?;
        let report = verify_storage(&storage)?;
        assert!(!report.is_clean());
        assert!(
            !report.corrupt.is_empty(),
            "corrupt polarity value must be caught: {report:?}"
        );
    
        Ok(())
    }

    #[test]
    fn verify_storage_reports_injected_corruption() -> Result<()> {
        let dir = tempfile::tempdir().into_diagnostic()?;
        let path = dir.path().join("db");
        let clean_checked;
        {
            let storage = new_fjall_storage(&path)?;
            let db = Engine::compose(storage.clone(), Catalog::new())?;
            db.run_script(
                "?[k, v] <- [[1, 7], [2, 14], [3, 21], [4, 28], [5, 35]] :create rel {k => v}",
                std::collections::BTreeMap::new(),
            )?;
            let report = verify_storage(&storage)?;
            assert!(report.is_clean(), "healthy store: {report:?}");
            clean_checked = report.checked;
        }
        {
            let raw = fjall::OptimisticTxDatabase::builder(&path).open()?;
            let ks = raw
                .keyspace("kyzo", fjall::KeyspaceCreateOptions::default)?;
            ks.insert([0u8, 0, 0, 0, 0, 0, 0, 7, 0xEE, 0xEE], b"?")?;
            raw.persist(fjall::PersistMode::SyncAll).into_diagnostic()?;
        }
        let storage = new_fjall_storage(&path)?;
        let report = verify_storage(&storage)?;
        assert!(!report.is_clean());
        assert_eq!(
            report.checked,
            clean_checked + 1,
            "walk continues: {report:?}"
        );
        assert!(
            report.corrupt[0].error.contains("BadTag"),
            "names BadTag: {}",
            report.corrupt[0].error
        );
    
        Ok(())
    }

    /// §51 trap: a wrong index that still decodes and order-checks clean must
    /// fail deep-verify, because expected content is re-derived from base facts.
    #[test]
    fn deep_verify_catches_wrong_index_that_still_decodes() -> Result<()> {
        use crate::store::verify_walk::deep_verify_storage;
        use kyzo_model::value::{DataValue, RelationId, Tuple, TupleT};

        let dir = tempfile::tempdir().into_diagnostic()?;
        let path = dir.path().join("db");
        let phantom_kv: (Vec<u8>, Vec<u8>) = {
            let storage = new_fjall_storage(&path)?;
            let db = Engine::compose(storage.clone(), Catalog::new())?;
            db.run_script(
                "?[k, v] <- [[1, 7], [2, 14]] :create t {k => v}",
                std::collections::BTreeMap::new(),
            )?;
            db.run_script(
                "::index create t:by_v {v}",
                std::collections::BTreeMap::new(),
            )?;

            let clean = deep_verify_storage(&storage)?;
            assert!(clean.is_clean(), "healthy indexed store: {clean:?}");
            assert!(
                clean.indices_checked >= 1,
                "must re-derive at least the plain index"
            );

            // Locate the plain index relation id from the catalog.
            let tx = storage.read_tx()?;
            let mut idx_id = None;
            let lower = Tuple::default().encode_as_key(RelationId::SYSTEM);
            let upper = (RelationId::SYSTEM.raw() + 1).to_be_bytes();
            for pair in tx.range_scan(&lower, &upper) {
                let (k, v) = pair?;
                let Ok(tup) = kyzo_model::value::decode_tuple_from_key(&k, 16) else {
                    continue;
                };
                if matches!(tup.first(), Some(DataValue::Str(_))) {
                    let h = crate::session::catalog::RelationHandle::decode(&v)?;
                    if h.name.as_str() == "t:by_v" {
                        idx_id = Some(h.id);
                        break;
                    }
                }
            }
            let idx_id = idx_id?;

            // Phantom index row: projects v=99 for a key that was never a base
            // fact. Valid polarity + canonical bytes → decode/order walk passes.
            let phantom_key_cols = [
                DataValue::from(99i64),
                DataValue::from(99i64), // mapper {v} then riding base key
            ];
            // Use a fresh stamp far above existing so order stays ascending.
            let stamp = kyzo_model::value::ValidityTs::from_raw(i64::MAX / 4);
            let handle = {
                // Minimal handle shape for encode doors — fetch real metadata.
                let tx = storage.read_tx()?;
                let lower = Tuple::default().encode_as_key(RelationId::SYSTEM);
                let upper = (RelationId::SYSTEM.raw() + 1).to_be_bytes();
                let mut found = None;
                for pair in tx.range_scan(&lower, &upper) {
                    let (_, v) = pair?;
                    if let Ok(h) = crate::session::catalog::RelationHandle::decode(&v)
                        && h.name.as_str() == "t:by_v"
                    {
                        found = Some(h);
                        break;
                    }
                }
                found?
            };
            let key = handle
                .encode_bitemporal_key_for_store(
                    &phantom_key_cols,
                    stamp,
                    stamp,
                    kyzo_model::SourceSpan::default(),
                )?;
            let val = handle
                .encode_bitemporal_val_for_store(
                    &phantom_key_cols,
                    crate::store::time::ClaimPolarity::Assert,
                    kyzo_model::SourceSpan::default(),
                )?;
            drop(idx_id);
            (key.as_ref().to_vec(), val)
        };

        {
            let raw = fjall::OptimisticTxDatabase::builder(&path).open()?;
            let ks = raw
                .keyspace("kyzo", fjall::KeyspaceCreateOptions::default)?;
            ks.insert(&phantom_kv.0, &phantom_kv.1)?;
            raw.persist(fjall::PersistMode::SyncAll).into_diagnostic()?;
        }

        let storage = new_fjall_storage(&path)?;
        let walk = verify_storage(&storage)?;
        assert!(
            walk.is_clean(),
            "decode+order walk must still pass on a wrong-but-well-formed index: {walk:?}"
        );
        let deep = deep_verify_storage(&storage)?;
        assert!(
            !deep.is_clean(),
            "deep-verify must catch phantom index row re-derived from base facts: {deep:?}"
        );
        assert!(
            deep.index_mismatches
                .iter()
                .any(|m| matches!(m.kind, IndexKind::Plain { .. })),
            "expected a plain-index mismatch: {:?}",
            deep.index_mismatches
        );
    
        Ok(())
    }
}
