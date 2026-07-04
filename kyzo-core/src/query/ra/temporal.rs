/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Interval derivation (`@spans`) and net diff (`@delta`/`@delta_sys`) as
//! real RA operators over a bitemporal stored relation — story #62 chunk
//! 3. Semantics are judged by `query/laws.rs`'s `derive_intervals`/`diff`;
//! this module is production code and does not depend on that test-only
//! oracle (it is an independently-written twin of the same algebra,
//! proven equal to it by the differential tests in
//! `query/temporal_trials.rs`).
//!
//! ## The scan-direction mismatch, and the buffering decision
//!
//! The storage kernel's governing-version resolution
//! (`data::bitemporal::check_key_for_bitemporal`) is a SKIP scan: it seeks
//! straight to the newest version at or before an `AsOf` coordinate and
//! then splices past every OLDER version of the same fact — it never
//! enumerates a fact's full history. [`SpansRA`] needs the opposite: every
//! stored point-event for a fact, because a maximal-run sweep must see
//! every breakpoint (every distinct valid instant) before it can decide
//! where one run ends and the next begins.
//!
//! No storage-tier primitive yields "every version of one key" today — the
//! contract requires only ordered range scans plus the as-of skip scan
//! (see `storage/mod.rs`'s doc and `data/bitemporal.rs`). Adding one is out
//! of reach for this chunk: `runtime/relation.rs` (the natural home for
//! such a method) carries another builder's uncommitted fix awaiting its
//! own verdict, so this module builds the raw multi-version scan directly
//! against the public storage contract (`ReadTx::range_scan` over the
//! relation's own keyspace bounds, computed here rather than by calling
//! the frozen file's private `keyspace_lower`/`keyspace_upper` — a small,
//! named duplication, not a design choice) and decodes each row itself
//! (`decode_raw_version`, mirroring `check_key_for_bitemporal`'s tail-slot
//! split without its skip-and-splice bound computation, since this scan
//! never skips).
//!
//! **The buffering decision**: [`SpansRA`] streams the ascending raw scan
//! and buffers only ONE fact key's full version set at a time (grouped by
//! the byte-equal key prefix that ascending order already keeps
//! contiguous — `Validity`'s `Reverse` encoding makes each such run
//! newest-first, though the sweep below re-sorts ascending itself and
//! does not rely on that). Memory cost is O(one fact's write history),
//! never O(relation) — the sweep, resolution, and Interval construction
//! for that one key complete before the scan advances to the next key's
//! first row. This is the "buffering strategy per fact key" the brief
//! calls a real design decision: the alternative (materialize the WHOLE
//! relation's raw event log before sweeping any key) was rejected as an
//! avoidable O(relation) working set for no compensating benefit.
//!
//! [`DeltaRA`] needs no such buffering: axis-parameterized diff is defined
//! as the set difference of two already-RESOLVED snapshots
//! (`laws::diff`'s own shape), and each snapshot is exactly what the
//! existing `RelationHandle::skip_scan_all` as-of scan already produces —
//! reused here unchanged. This chunk's diff is deliberately naive (two
//! full snapshots, materialized, differenced) per the ruling; the
//! O(changes) transposed posting-index acceleration is chunk 4, and the
//! SEAM for it is exactly this operator's constructor: when that index
//! lands, `DeltaRA::iter_batched` gains an index-probe fast path ahead of
//! the full-snapshot fallback below, with identical output — nothing
//! about `DeltaRA`'s bindings, `from`/`to` coordinates, or its place in
//! the `RelAlgebra` tree needs to change.

use std::collections::BTreeSet;

use miette::{Result, bail, miette};

use crate::data::bitemporal::{
    ClaimPolarity, claim_polarity_of_value, extend_tuple_from_bitemporal_v,
};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::tuple::{EncodedKey, Tuple, TupleT, decode_tuple_from_key};
use crate::data::value::{AsOf, DataValue, Interval};
use crate::query::batch_ops::{Batch, BatchIter};
use crate::runtime::relation::RelationHandle;
use crate::storage::ReadTx;

/// Interval derivation: `*rel{k, v} @spans iv[, sys]` — one output row per
/// (fact, maximal equal-payload run) along the VALID axis at the fixed
/// system snapshot `sys` (default the record's current belief,
/// `i64::MAX`), `iv` bound to the produced [`Interval`]. Only the valid
/// axis is exposed today (issue #62's ratified surface: system time keeps
/// its own simpler versioning algebra) — see the module doc for why the
/// operator still buffers a full per-fact version set regardless of axis.
#[derive(Debug)]
pub(crate) struct SpansRA {
    /// The base relation's key and payload bindings, plus one trailing
    /// binding for the produced interval — never folded into the base
    /// columns, so relation-arity checks against the base columns are
    /// unaffected (the same shape `SearchAtom::own_bindings` uses for a
    /// search's engine-appended columns).
    pub(crate) bindings: Vec<Symbol>,
    pub(crate) storage: RelationHandle,
    /// The fixed system snapshot the sweep resolves against.
    pub(crate) sys: i64,
    pub(crate) span: SourceSpan,
}

/// Axis-parameterized net diff: `*rel{k} @delta(a, b) sgn` (valid axis,
/// fixed at the current system snapshot) or `@delta_sys(a, b) sgn`
/// (system axis, fixed at the current valid instant). `from`/`to` are
/// already the full bitemporal coordinates (the constructor resolved the
/// axis once); `sgn` binds `+1`/`-1` (new/gone), one trailing binding
/// beyond the base row exactly like [`SpansRA`]'s interval column.
#[derive(Debug)]
pub(crate) struct DeltaRA {
    pub(crate) bindings: Vec<Symbol>,
    pub(crate) storage: RelationHandle,
    pub(crate) from: AsOf,
    pub(crate) to: AsOf,
    pub(crate) span: SourceSpan,
}

/// `+1`: the fact holds at `to` but not at `from`.
const SIGN_PLUS: i64 = 1;
/// `-1`: the fact held at `from` but not at `to`.
const SIGN_MINUS: i64 = -1;

/// One stored point-event, decoded from a raw bitemporal row without
/// resolving it against any coordinate: the valid instant, the system
/// version, the claim polarity, and (for [`ClaimPolarity::Assert`] only)
/// the row's non-key payload columns.
struct RawVersion {
    valid: i64,
    sys: i64,
    polarity: ClaimPolarity,
    payload: Tuple,
}

/// The relation's whole keyspace, as raw byte bounds. Duplicates
/// `RelationHandle::keyspace_lower`/`keyspace_upper` (`runtime/relation.rs`,
/// private methods in a file this story does not touch — see the module
/// doc) rather than exposing them; the computation itself
/// (`Tuple::default()` encoded under the relation's id, and the next
/// relation id's raw prefix as the exclusive upper bound) is a few lines
/// of the same public encoding API every caller of this module already
/// uses.
fn relation_keyspace_bounds(storage: &RelationHandle) -> (Vec<u8>, Vec<u8>) {
    let lower = Tuple::default().encode_as_key(storage.id).into_vec();
    let upper = (storage.id.0 + 1).to_be_bytes().to_vec();
    (lower, upper)
}

/// Split a raw bitemporal row into its key-prefix bytes (for grouping),
/// decoded key columns, and [`RawVersion`]. `key_len` is the relation's
/// key arity — used both as a decode-size hint and to prove the decoded
/// key matches the relation's own shape (a mismatch is corruption, refused
/// rather than trusted).
fn decode_raw_version(
    key: &[u8],
    val: &[u8],
    key_len: usize,
) -> Result<(Vec<u8>, Tuple, RawVersion)> {
    if key.len() < EncodedKey::RELATION_PREFIX_LEN + EncodedKey::BITEMPORAL_TAIL_LEN {
        bail!("temporal scan over a key too short to carry its two time slots");
    }
    let prefix_len = key.len() - EncodedKey::BITEMPORAL_TAIL_LEN;
    let mut full = decode_tuple_from_key(key, key_len + 2)?;
    let sys_dv = full
        .pop()
        .ok_or_else(|| miette!("corrupt temporal key: missing its system-time slot"))?;
    let valid_dv = full
        .pop()
        .ok_or_else(|| miette!("corrupt temporal key: missing its valid-time slot"))?;
    let DataValue::Validity(sys_slot) = sys_dv else {
        bail!("corrupt temporal key: system-time slot is not a Validity encoding");
    };
    let DataValue::Validity(valid_slot) = valid_dv else {
        bail!("corrupt temporal key: valid-time slot is not a Validity encoding");
    };
    if !valid_slot.is_assert.0 || !sys_slot.is_assert.0 {
        bail!(
            "corrupt temporal key: a retract flag in a stored time slot \
             (polarity lives in the value; stored slot flags are pinned)"
        );
    }
    if full.len() != key_len {
        bail!(
            "corrupt temporal key: decoded {} key columns, relation declares {key_len}",
            full.len()
        );
    }
    let polarity = claim_polarity_of_value(val)?;
    let mut row = full.clone();
    extend_tuple_from_bitemporal_v(&mut row, val)?;
    let payload = row.split_off(key_len);
    Ok((
        key[..prefix_len].to_vec(),
        full,
        RawVersion {
            valid: valid_slot.timestamp.0.0,
            sys: sys_slot.timestamp.0.0,
            polarity,
            payload,
        },
    ))
}

/// The governing tuple for one fact's already-collected version set, at
/// `(at_valid, at_sys)`: among instants at or before `at_valid`, newest
/// first, the newest version at or before `at_sys` governs that instant;
/// `Assert` holds (`key ++ payload`), `Retract` settles absent (no
/// fall-through), `Erase` is transparent — resolution falls through to
/// the fact's next older instant. The production twin of
/// `laws::resolve_events` (independently written; see the module doc for
/// why production code does not call into the oracle).
fn resolve_at(
    group: &[RawVersion],
    key: &[DataValue],
    at_valid: i64,
    at_sys: i64,
) -> Option<Tuple> {
    let mut instants: Vec<i64> = group
        .iter()
        .map(|e| e.valid)
        .filter(|v| *v <= at_valid)
        .collect();
    instants.sort_unstable();
    instants.dedup();
    for instant in instants.into_iter().rev() {
        let governing = group
            .iter()
            .filter(|e| e.valid == instant && e.sys <= at_sys)
            .max_by_key(|e| e.sys);
        match governing.map(|e| e.polarity) {
            Some(ClaimPolarity::Assert) => {
                let e = governing.expect("just matched Some");
                let mut tuple = key.to_vec();
                tuple.extend(e.payload.iter().cloned());
                return Some(tuple);
            }
            Some(ClaimPolarity::Retract) => return None,
            Some(ClaimPolarity::Erase) | None => {}
        }
    }
    None
}

/// The maximal-run sweep over one fact's fully-collected version set,
/// along the valid axis at the fixed system snapshot `fixed_sys`: every
/// stored valid instant is a candidate breakpoint; the loop closes a run
/// only when the next breakpoint resolves to a DIFFERENT tuple, so
/// coalescing is definitional (un-coalesced output is unrepresentable),
/// exactly mirroring `laws::derive_intervals`. Returns one `key ++
/// payload ++ Interval` row per maximal run.
fn derive_group(group: &[RawVersion], key: &[DataValue], fixed_sys: i64) -> Result<Vec<Tuple>> {
    let mut breaks: Vec<i64> = group.iter().map(|e| e.valid).collect();
    breaks.sort_unstable();
    breaks.dedup();

    let mut out = Vec::new();
    let mut i = 0;
    while i < breaks.len() {
        let start = breaks[i];
        let Some(tuple) = resolve_at(group, key, start, fixed_sys) else {
            i += 1;
            continue;
        };
        let mut j = i;
        while j + 1 < breaks.len()
            && resolve_at(group, key, breaks[j + 1], fixed_sys).as_ref() == Some(&tuple)
        {
            j += 1;
        }
        let end = if j + 1 < breaks.len() {
            breaks[j + 1]
        } else {
            i64::MAX
        };
        // `start` is a real stored valid instant (never the reserved
        // terminal tick — the write path refuses it) and `end` is either
        // a strictly later stored instant or the open-end sentinel, so
        // `start < end` always; a corrupt stored row that defeated the
        // write-side reservation would surface here as a typed error,
        // never a panic (law 5).
        let iv = Interval::new(start, end).map_err(|e| {
            miette!("temporal derivation produced an invalid interval [{start}, {end}): {e}")
        })?;
        let mut row = tuple;
        row.push(DataValue::Interval(iv));
        out.push(row);
        i = j + 1;
    }
    Ok(out)
}

impl SpansRA {
    pub(crate) fn iter_batched<'a>(&'a self, tx: &'a impl ReadTx) -> Result<BatchIter<'a>> {
        let (lower, upper) = relation_keyspace_bounds(&self.storage);
        Ok(Box::new(SpansScanBatches {
            raw: tx.range_scan(&lower, &upper),
            pending_key: None,
            key_len: self.storage.metadata.keys.len(),
            sys_fixed: self.sys,
            done: false,
        }))
    }
}

/// Batch-native derivation scan: pulls the relation's raw ascending
/// key/value stream, groups consecutive rows sharing the same byte-equal
/// key prefix (ascending order keeps one fact's versions contiguous),
/// sweeps each group once it is fully collected, and packs the produced
/// rows into batches of up to [`BATCH_ROWS`] — the same discipline every
/// other batched operator in this crate follows (no row-at-a-time
/// fallback, per `query/ra/mod.rs`'s module doc).
struct SpansScanBatches<'a> {
    raw: Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>,
    /// A row already pulled from `raw` that belongs to the NEXT group
    /// (its key prefix differed from the group being collected) — carried
    /// over so each row is decoded exactly once.
    pending_key: Option<(Vec<u8>, Vec<u8>)>,
    key_len: usize,
    sys_fixed: i64,
    done: bool,
}

impl<'a> SpansScanBatches<'a> {
    /// Collect every raw row sharing one fact's key-prefix bytes, starting
    /// from `first` (already pulled from `raw`), leaving the first row of
    /// the NEXT group (if any) in `self.pending_key`.
    fn collect_group(
        &mut self,
        first: (Vec<u8>, Vec<u8>),
    ) -> Result<(Vec<DataValue>, Vec<RawVersion>)> {
        let (prefix, key, first_ver) = decode_raw_version(&first.0, &first.1, self.key_len)?;
        let mut group = vec![first_ver];
        loop {
            let Some(next) = self.raw.next() else { break };
            let (k, v) = next?;
            if k.len() < EncodedKey::BITEMPORAL_TAIL_LEN
                || k[..k.len() - EncodedKey::BITEMPORAL_TAIL_LEN] != prefix[..]
            {
                self.pending_key = Some((k, v));
                break;
            }
            let (_, _, ver) = decode_raw_version(&k, &v, self.key_len)?;
            group.push(ver);
        }
        Ok((key, group))
    }
}

impl<'a> Iterator for SpansScanBatches<'a> {
    type Item = Result<Batch>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let mut out = Batch::new();
        loop {
            if out.is_full() {
                return Some(Ok(out));
            }
            let first = match self.pending_key.take() {
                Some(kv) => kv,
                None => match self.raw.next() {
                    Some(Ok(kv)) => kv,
                    Some(Err(e)) => {
                        self.done = true;
                        return Some(Err(e));
                    }
                    None => {
                        self.done = true;
                        break;
                    }
                },
            };
            let (key, group) = match self.collect_group(first) {
                Ok(kg) => kg,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            };
            let rows = match derive_group(&group, &key, self.sys_fixed) {
                Ok(rows) => rows,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            };
            for row in rows {
                out.push(row);
            }
        }
        if out.is_empty() { None } else { Some(Ok(out)) }
    }
}

impl DeltaRA {
    /// Naive by design (per the ruling — the O(changes) posting-index
    /// acceleration is chunk 4): resolve both endpoints as full snapshots
    /// through the existing as-of scan, and set-difference them. `from`
    /// governs `-1` rows (present there, gone at `to`); `to` governs `+1`
    /// rows (absent there, present at `to`) — a payload change is
    /// therefore both a `-1` (old payload) and a `+1` (new payload) row
    /// at the same key, never a "modified" kind, matching
    /// `laws::SignedFact`.
    pub(crate) fn iter_batched<'a>(&'a self, tx: &'a impl ReadTx) -> Result<BatchIter<'a>> {
        let mut from_set: BTreeSet<Tuple> = BTreeSet::new();
        for t in self.storage.skip_scan_all(tx, self.from) {
            from_set.insert(t?);
        }
        let mut to_set: BTreeSet<Tuple> = BTreeSet::new();
        for t in self.storage.skip_scan_all(tx, self.to) {
            to_set.insert(t?);
        }
        let mut rows: Vec<Tuple> = Vec::new();
        for t in from_set.difference(&to_set) {
            let mut row = t.clone();
            row.push(DataValue::from(SIGN_MINUS));
            rows.push(row);
        }
        for t in to_set.difference(&from_set) {
            let mut row = t.clone();
            row.push(DataValue::from(SIGN_PLUS));
            rows.push(row);
        }
        // Canonical, deterministic output order (the determinism law):
        // every row is sorted by its full content, sign column included.
        rows.sort();
        Ok(Box::new(RowChunks {
            rows: rows.into_iter(),
        }))
    }
}

/// Packs an already-materialized, already-ordered row sequence into
/// batches of up to [`BATCH_ROWS`] — [`DeltaRA`]'s naive snapshot-diff is
/// fully resolved before any batch is built, so this is plain chunking,
/// not a scan.
struct RowChunks {
    rows: std::vec::IntoIter<Tuple>,
}

impl Iterator for RowChunks {
    type Item = Result<Batch>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut out = Batch::new();
        for row in self.rows.by_ref() {
            out.push(row);
            if out.is_full() {
                return Some(Ok(out));
            }
        }
        if out.is_empty() { None } else { Some(Ok(out)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::relation::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
    use crate::data::span::SourceSpan;
    use crate::data::symb::Symbol;
    use crate::data::value::{MAX_VALIDITY_TS, ValidityTs};
    use crate::runtime::relation::{KeyspaceKind, create_relation};
    use crate::storage::fjall::new_fjall_storage;
    use crate::storage::{Storage, WriteTx};
    use itertools::Itertools;
    use std::cmp::Reverse;

    fn sp() -> SourceSpan {
        SourceSpan(0, 0)
    }
    fn sym(name: &str) -> Symbol {
        Symbol::new(name, sp())
    }
    fn v(i: i64) -> DataValue {
        DataValue::from(i)
    }
    fn vts(t: i64) -> ValidityTs {
        ValidityTs(Reverse(t))
    }
    fn col(name: &str) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            typing: NullableColType {
                coltype: ColType::Any,
                nullable: false,
            },
            default_gen: None,
        }
    }

    /// A one-key, one-value relation, `key_arity` key columns.
    fn make_relation(
        db: &crate::storage::fjall::FjallStorage,
        name: &str,
        key_arity: usize,
    ) -> RelationHandle {
        let keys: Vec<ColumnDef> = (0..key_arity).map(|i| col(&format!("k{i}"))).collect();
        let key_bindings = keys.iter().map(|c| sym(&c.name)).collect();
        let input = crate::data::program::InputRelationHandle {
            name: sym(name),
            metadata: StoredRelationMetadata {
                keys,
                non_keys: vec![col("val")],
            },
            key_bindings,
            dep_bindings: vec![sym("val")],
            span: sp(),
        };
        let mut tx = db.write_tx().expect("write tx");
        let handle = create_relation(&mut tx, input, KeyspaceKind::Facts).expect("create relation");
        tx.commit().expect("commit");
        handle
    }

    /// Assert `key ++ [val]` at `valid`, its own transaction (so it gets a
    /// genuinely distinct, monotonically increasing system stamp — every
    /// commit mints one).
    fn assert_at(
        db: &crate::storage::fjall::FjallStorage,
        handle: &RelationHandle,
        key: i64,
        valid: i64,
        val: i64,
    ) {
        let mut tx = db.write_tx().expect("write tx");
        handle
            .put_fact(&mut tx, &[v(key), v(val)], vts(valid), sp())
            .expect("put fact");
        tx.commit().expect("commit");
    }

    fn retract_at(
        db: &crate::storage::fjall::FjallStorage,
        handle: &RelationHandle,
        key: i64,
        valid: i64,
    ) {
        let mut tx = db.write_tx().expect("write tx");
        handle
            .retract_fact(&mut tx, &[v(key)], vts(valid), sp())
            .expect("retract fact");
        tx.commit().expect("commit");
    }

    /// Write a raw ERASE row directly (no production write-path exposes
    /// `ClaimPolarity::Erase` today — see the module doc's law list; this
    /// is test-only plumbing through the same `pub(crate)` encoders
    /// `put_fact`/`retract_fact` themselves use).
    fn erase_at(
        db: &crate::storage::fjall::FjallStorage,
        handle: &RelationHandle,
        key: i64,
        valid: i64,
    ) {
        let mut tx = db.write_tx().expect("write tx");
        let sys = tx.system_stamp();
        let encoded_key = handle
            .encode_bitemporal_key_for_store(&[v(key)], vts(valid), sys, sp())
            .expect("encode key");
        let val = handle
            .encode_bitemporal_val_for_store(&[v(key)], ClaimPolarity::Erase, sp())
            .expect("encode erase value");
        tx.put(encoded_key.as_bytes(), &val).expect("put erase row");
        tx.commit().expect("commit");
    }

    fn spans_rows(
        db: &crate::storage::fjall::FjallStorage,
        handle: &RelationHandle,
        sys: i64,
    ) -> Vec<(i64, i64, i64, i64)> {
        let ra = SpansRA {
            bindings: vec![sym("k"), sym("val"), sym("iv")],
            storage: handle.clone(),
            sys,
            span: sp(),
        };
        let rtx = db.read_tx().expect("read tx");
        let mut out = vec![];
        for batch in ra.iter_batched(&rtx).expect("iter") {
            for row in batch.expect("batch").into_rows() {
                let DataValue::Num(crate::data::value::Num::Int(k)) = row[0] else {
                    panic!("key not an int")
                };
                let DataValue::Num(crate::data::value::Num::Int(val)) = row[1] else {
                    panic!("val not an int")
                };
                let DataValue::Interval(iv) = &row[2] else {
                    panic!("third column not an interval: {row:?}")
                };
                out.push((k, val, iv.start(), iv.end()));
            }
        }
        out.sort();
        out
    }

    #[test]
    fn single_assert_is_one_open_interval() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "spans_single", 1);
        assert_at(&db, &h, 1, 10, 100);
        let rows = spans_rows(&db, &h, i64::MAX);
        assert_eq!(rows, vec![(1, 100, 10, i64::MAX)]);
    }

    #[test]
    fn retract_clips_the_interval_exclusive() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "spans_retract", 1);
        assert_at(&db, &h, 1, 10, 100);
        retract_at(&db, &h, 1, 20);
        let rows = spans_rows(&db, &h, i64::MAX);
        assert_eq!(rows, vec![(1, 100, 10, 20)]);
    }

    #[test]
    fn payload_change_splits_into_two_intervals() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "spans_split", 1);
        assert_at(&db, &h, 1, 10, 100);
        assert_at(&db, &h, 1, 20, 200);
        let rows = spans_rows(&db, &h, i64::MAX);
        assert_eq!(rows, vec![(1, 100, 10, 20), (1, 200, 20, i64::MAX)]);
    }

    #[test]
    fn double_assert_same_payload_is_idempotent_one_interval() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "spans_idempotent", 1);
        assert_at(&db, &h, 1, 10, 100);
        assert_at(&db, &h, 1, 20, 100);
        let rows = spans_rows(&db, &h, i64::MAX);
        assert_eq!(rows, vec![(1, 100, 10, i64::MAX)]);
    }

    #[test]
    fn assert_after_retract_opens_a_new_interval() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "spans_reopen", 1);
        assert_at(&db, &h, 1, 10, 100);
        retract_at(&db, &h, 1, 20);
        assert_at(&db, &h, 1, 30, 100);
        let rows = spans_rows(&db, &h, i64::MAX);
        assert_eq!(rows, vec![(1, 100, 10, 20), (1, 100, 30, i64::MAX)]);
    }

    #[test]
    fn dangling_retract_holds_nowhere() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "spans_dangling_retract", 1);
        retract_at(&db, &h, 1, 10);
        let rows = spans_rows(&db, &h, i64::MAX);
        assert!(rows.is_empty());
    }

    /// Erase transparency: a system-time correction that un-records the
    /// instant-10 assertion falls through to the older instant-0 one.
    #[test]
    fn erase_is_transparent_falls_through_to_older_instant() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "spans_erase", 1);
        assert_at(&db, &h, 1, 0, 100);
        assert_at(&db, &h, 1, 10, 200);
        erase_at(&db, &h, 1, 10);
        let rows = spans_rows(&db, &h, i64::MAX);
        // Instant 10 is erased, so the payload is 100 throughout (the
        // instant-0 assert governs everywhere it would have fallen
        // through to) — one interval, not two.
        assert_eq!(rows, vec![(1, 100, 0, i64::MAX)]);
    }

    #[test]
    fn no_zero_width_intervals_at_any_fixed_sys() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "spans_no_zero_width", 1);
        assert_at(&db, &h, 1, 10, 100);
        assert_at(&db, &h, 1, 10, 200); // same instant, later sys: corrects it
        for (start, end, _, _) in spans_rows(&db, &h, i64::MAX)
            .into_iter()
            .map(|(k, val, s, e)| (s, e, k, val))
        {
            assert!(start < end, "zero-width interval [{start}, {end})");
        }
        // At the OLDER system snapshot the first (pre-correction) write
        // governs — still no zero-width run.
        let db2 = new_fjall_storage(tempfile_dir()).expect("storage");
        let h2 = make_relation(&db2, "spans_no_zero_width2", 1);
        assert_at(&db2, &h2, 1, 10, 100);
        let rows = spans_rows(&db2, &h2, i64::MAX);
        assert_eq!(rows, vec![(1, 100, 10, i64::MAX)]);
    }

    #[test]
    fn multiple_facts_derive_independently() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "spans_multi", 1);
        assert_at(&db, &h, 1, 10, 100);
        assert_at(&db, &h, 2, 5, 900);
        retract_at(&db, &h, 2, 15);
        let rows = spans_rows(&db, &h, i64::MAX);
        assert_eq!(
            rows.into_iter().sorted().collect_vec(),
            vec![(1, 100, 10, i64::MAX), (2, 900, 5, 15)]
        );
    }

    fn delta_rows(
        db: &crate::storage::fjall::FjallStorage,
        handle: &RelationHandle,
        from: AsOf,
        to: AsOf,
    ) -> Vec<(i64, i64, i64)> {
        let ra = DeltaRA {
            bindings: vec![sym("k"), sym("val"), sym("sgn")],
            storage: handle.clone(),
            from,
            to,
            span: sp(),
        };
        let rtx = db.read_tx().expect("read tx");
        let mut out = vec![];
        for batch in ra.iter_batched(&rtx).expect("iter") {
            for row in batch.expect("batch").into_rows() {
                let DataValue::Num(crate::data::value::Num::Int(k)) = row[0] else {
                    panic!("key not an int")
                };
                let DataValue::Num(crate::data::value::Num::Int(val)) = row[1] else {
                    panic!("val not an int")
                };
                let DataValue::Num(crate::data::value::Num::Int(sgn)) = row[2] else {
                    panic!("sign not an int")
                };
                out.push((k, val, sgn));
            }
        }
        out
    }

    #[test]
    fn diff_valid_axis_sees_a_new_assertion_as_plus() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "delta_new", 1);
        assert_at(&db, &h, 1, 10, 100);
        let rows = delta_rows(&db, &h, AsOf::current(vts(5)), AsOf::current(vts(20)));
        assert_eq!(rows, vec![(1, 100, SIGN_PLUS)]);
    }

    #[test]
    fn diff_payload_change_is_a_minus_plus_pair_never_modified() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "delta_change", 1);
        assert_at(&db, &h, 1, 10, 100);
        assert_at(&db, &h, 1, 20, 200);
        let rows = delta_rows(&db, &h, AsOf::current(vts(15)), AsOf::current(vts(25)));
        assert_eq!(rows.len(), 2);
        assert!(rows.contains(&(1, 100, SIGN_MINUS)));
        assert!(rows.contains(&(1, 200, SIGN_PLUS)));
    }

    #[test]
    fn diff_identical_snapshots_is_empty() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "delta_empty", 1);
        assert_at(&db, &h, 1, 10, 100);
        let rows = delta_rows(&db, &h, AsOf::current(vts(20)), AsOf::current(vts(20)));
        assert!(rows.is_empty());
    }

    #[test]
    fn diff_sys_axis_sees_a_correction() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "delta_sys", 1);
        assert_at(&db, &h, 1, 10, 100); // sys stamp 1 (first commit)
        assert_at(&db, &h, 1, 10, 200); // sys stamp 2 (correction, same instant)
        // Fixed valid = MAX (current belief about "now"); sys axis varies.
        let before = AsOf::at(vts(0), MAX_VALIDITY_TS);
        let after = AsOf::current(MAX_VALIDITY_TS);
        let rows = delta_rows(&db, &h, before, after);
        assert_eq!(rows, vec![(1, 200, SIGN_PLUS)]);
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kyzo-temporal-ra-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }
}
