/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original (MPL-2.0):
 * decoding made fallible; the key layout and its types live here now.
 */

//! Tuples and their written form.
//!
//! A [`Tuple`] is a fact's body — an ordered sequence of values. An
//! [`EncodedKey`] is its written form: relation prefix, memcomparable tuple
//! bytes, and (for facts) the two fixed-width bitemporal time slots.
//! Encoding is infallible; decoding parses *claimed* bytes and is fallible
//! everywhere.

use miette::{Result, bail, miette};

use crate::data::memcmp::{ENCODED_VLD_LEN, MemCmpEncoder};
use crate::data::value::DataValue;

/// A fact's body: an ordered sequence of values.
pub type Tuple = Vec<DataValue>;

/// A fact's written form: the relation prefix followed by the memcomparable
/// tuple encoding, with the two bitemporal time slots as the fixed-width
/// tail.
///
/// Only encoders construct this — possession is proof of well-formed
/// provenance. Bytes read back from storage are *claimed* keys until
/// decoding proves them; they stay `&[u8]` in signatures on purpose.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EncodedKey(Vec<u8>);

impl EncodedKey {
    /// Every key begins with this many bytes of relation id.
    pub const RELATION_PREFIX_LEN: usize = 8;
    /// One encoded time slot is exactly this many trailing bytes.
    pub const VALIDITY_TAIL_LEN: usize = ENCODED_VLD_LEN;
    /// A bitemporal key ends with two fixed-width validity slots, in this
    /// order: the VALID instant (outer — when in the world the row speaks
    /// of), then the SYSTEM version (inner — when the record came to say
    /// it). Both flags are PINNED to assert: what the row says — assert,
    /// retract, or erase — is its [`ClaimPolarity`], carried in the VALUE,
    /// so one valid instant has exactly one system lineage and a
    /// contradictory pair of lineages at the same instant cannot be
    /// written at all. Valid-outer means a fact's versions group by valid
    /// instant, newest first, with each instant's system versions adjacent
    /// inside its group; that adjacency is what lets the two-axis skip
    /// scan resolve "what did the record say at S about V?" with spliced
    /// seeks and no reconstruction.
    pub const BITEMPORAL_TAIL_LEN: usize = 2 * ENCODED_VLD_LEN;

    /// The raw bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Surrender the raw bytes.
    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

impl std::ops::Deref for EncodedKey {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl AsRef<[u8]> for EncodedKey {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

pub(crate) trait TupleT {
    fn encode_as_key(&self, prefix: RelationId) -> EncodedKey;
}

impl<T> TupleT for T
where
    T: AsRef<[DataValue]>,
{
    fn encode_as_key(&self, prefix: RelationId) -> EncodedKey {
        let len = self.as_ref().len();
        // Relation prefix + a rough 14 bytes per value.
        let mut ret = Vec::with_capacity(EncodedKey::RELATION_PREFIX_LEN + 14 * len);
        ret.extend(prefix.raw_encode());
        for val in self.as_ref().iter() {
            ret.encode_datavalue(val);
        }
        EncodedKey(ret)
    }
}

/// Encode a PROJECTION of a row as a storage key: the key bytes of
/// `cols.map(|i| &row[i])` under the relation id, built without ever
/// materializing the projected tuple — the zero-clone probe path's
/// encoder. Identical bytes to `encode_as_key` on the materialized
/// projection (pinned by test).
pub(crate) fn encode_projected_key(
    relation: RelationId,
    row: &[DataValue],
    cols: &[usize],
) -> EncodedKey {
    let mut ret = Vec::with_capacity(EncodedKey::RELATION_PREFIX_LEN + 14 * cols.len());
    ret.extend(relation.raw_encode());
    for &c in cols {
        ret.encode_datavalue(&row[c]);
    }
    EncodedKey(ret)
}

/// [`encode_projected_key`] with literal values appended after the
/// projection — bound values of a bounded prefix scan, or the `Bot`
/// sentinel closing a plain prefix scan. Zero materialization either way.
pub(crate) fn encode_projected_key_with_suffix(
    relation: RelationId,
    row: &[DataValue],
    cols: &[usize],
    suffix: &[DataValue],
) -> EncodedKey {
    let mut ret =
        Vec::with_capacity(EncodedKey::RELATION_PREFIX_LEN + 14 * (cols.len() + suffix.len()));
    ret.extend(relation.raw_encode());
    for &c in cols {
        ret.encode_datavalue(&row[c]);
    }
    for v in suffix {
        ret.encode_datavalue(v);
    }
    EncodedKey(ret)
}

/// Encode a key from a tuple PREFIX plus literal values appended after it,
/// without ever materializing the concatenated tuple — the same
/// zero-clone shape as [`encode_projected_key_with_suffix`], specialized to
/// a contiguous prefix (no column-index indirection, so no `cols: &[usize]`
/// allocation on the caller's side either). Byte-identical to
/// `[tuple, suffix].concat().encode_as_key(relation)` (pinned by test).
///
/// This is the bulk-write path's per-row key encoder (every fact write's
/// bitemporal key, and the current-row probe's bounds): it used to go
/// through an intermediate `Vec<DataValue>` (tuple columns copied, then the
/// bitemporal slots or the `Bot` sentinel pushed) before a second pass
/// re-walked it into bytes. One pass, one allocation.
pub(crate) fn encode_key_with_suffix(
    relation: RelationId,
    tuple: &[DataValue],
    suffix: &[DataValue],
) -> EncodedKey {
    let mut ret =
        Vec::with_capacity(EncodedKey::RELATION_PREFIX_LEN + 14 * (tuple.len() + suffix.len()));
    ret.extend(relation.raw_encode());
    for val in tuple {
        ret.encode_datavalue(val);
    }
    for val in suffix {
        ret.encode_datavalue(val);
    }
    EncodedKey(ret)
}

/// Encode a tuple as a storage key under the given relation id. The public
/// face of the key format for benchmarks and tooling; the engine uses the
/// internal typed path.
pub fn encode_tuple_key(relation_id: u64, tuple: &[DataValue]) -> EncodedKey {
    tuple.encode_as_key(RelationId::new(relation_id))
}

/// Parse a claimed key into its tuple. Fallible: the bytes may be corrupt.
pub fn decode_tuple_from_key(key: &[u8], size_hint: usize) -> Result<Tuple> {
    let mut ret = Vec::with_capacity(size_hint);
    decode_key_into(key, &mut ret)?;
    Ok(ret)
}

/// As [`decode_tuple_from_key`], but appending into a caller-owned buffer —
/// the batched scan's allocation-free row decode.
pub fn decode_key_into(key: &[u8], out: &mut Vec<DataValue>) -> Result<()> {
    let Some(mut remaining) = key.get(EncodedKey::RELATION_PREFIX_LEN..) else {
        bail!("corrupt tuple key: shorter than the relation-id prefix");
    };
    while !remaining.is_empty() {
        let (val, next) = DataValue::decode_from_key(remaining)?;
        out.push(val);
        remaining = next;
    }
    Ok(())
}

const DEFAULT_SIZE_HINT: usize = 16;

/// Check whether a claimed key belongs in a bitemporal as-of result at
/// the [`AsOf`] coordinate, and compute the next seek bound.
///
/// The key's last [`EncodedKey::BITEMPORAL_TAIL_LEN`] bytes are its two
/// time slots (valid outer, system inner — see the constant's doc), both
/// carried as [`Validity`] encodings with the flag PINNED to assert: a
/// retract flag in a stored slot is corruption, refused here. The row's
/// meaning at its instant is `polarity`, which the caller reads from the
/// row's value — scans hold the value alongside the key, so the peek
/// costs nothing.
///
/// The question the scan answers: **as the record stood at system time
/// `as_of.sys`, what held at valid time `as_of.valid`?** Resolution, per fact:
///
/// 1. Find the newest valid instant at or before `as_of.valid`.
/// 2. The instant's state at `as_of.sys` is its newest system version
///    at or before it — a single total order, because an instant has one
///    lineage. An instant whose versions are all later-recorded, or whose
///    governing version is [`ClaimPolarity::Erase`], contributes nothing:
///    resolution falls to the fact's next older instant.
/// 3. The first contributing instant decides: `Assert` emits the fact,
///    `Retract` settles it absent. Either way the fact's older instants
///    are skipped entirely.
///
/// The returned bound is raw claimed
/// Stored values carry this many bytes of header before the rmp payload.
pub(crate) const VALUE_HEADER_LEN: usize = 8;

/// Decode a tuple from a key-value pair. Used by [`ReadTx`](crate::ReadTx)
/// implementations and scans.
#[inline]
pub fn decode_tuple_from_kv(key: &[u8], val: &[u8], size_hint: Option<usize>) -> Result<Tuple> {
    let mut tup = decode_tuple_from_key(key, size_hint.unwrap_or(DEFAULT_SIZE_HINT))?;
    extend_tuple_from_v(&mut tup, val)?;
    Ok(tup)
}

/// Extend a key-decoded tuple with the non-key columns stored in the value.
///
/// MEASURED: a streaming `DeserializeSeed` that
/// appended into the existing buffer — saving this function's intermediate
/// `Vec` — made decode-heavy workloads 3x SLOWER (join3 41ms -> 150ms);
/// `rmp_serde::from_slice`'s monolithic path is faster than element-at-a-time
/// seeded deserialization despite the extra allocation. Keep the Vec.
pub fn extend_tuple_from_v(key: &mut Tuple, val: &[u8]) -> Result<()> {
    if val.is_empty() {
        return Ok(());
    }
    let Some(payload) = val.get(VALUE_HEADER_LEN..) else {
        bail!("corrupt tuple value: shorter than its header");
    };
    let vals: Vec<DataValue> =
        rmp_serde::from_slice(payload).map_err(|e| miette!("corrupt tuple value: {e}"))?;
    key.extend(vals);
    Ok(())
}

/// The stored-relation id: the first 8 bytes of every key.
#[derive(
    Copy,
    Clone,
    Eq,
    PartialEq,
    Debug,
    serde_derive::Serialize,
    serde_derive::Deserialize,
    PartialOrd,
    Ord,
)]
pub(crate) struct RelationId(pub(crate) u64);

/// Relation ids occupy a 48-bit space (the upstream invariant: ids stay
/// within 6 bytes even though the encoded prefix is 8), leaving headroom in
/// the key prefix.
pub(crate) const MAX_RELATION_ID: u64 = 1 << 48;

impl RelationId {
    pub(crate) fn new(u: u64) -> Self {
        assert!(u <= MAX_RELATION_ID, "StoredRelId overflow: {u}");
        Self(u)
    }
    #[allow(dead_code)] // used by the engine layers growing around the kernel
    pub(crate) fn next(&self) -> Self {
        Self::new(self.0 + 1)
    }
    #[allow(dead_code)] // used by the engine layers growing around the kernel
    pub(crate) const SYSTEM: Self = Self(0);
    pub(crate) fn raw_encode(&self) -> [u8; 8] {
        self.0.to_be_bytes()
    }
    #[allow(dead_code)] // used by the engine layers growing around the kernel
    pub(crate) fn raw_decode(src: &[u8]) -> Result<Self> {
        let Some(bytes) = src.get(0..8) else {
            bail!("corrupt key: shorter than the relation-id prefix");
        };
        let u = u64::from_be_bytes(bytes.try_into().expect("length checked"));
        // The overflow assert in `new()` guards the WRITE path; on the read
        // path an out-of-range id is data corruption and must be an error,
        // never a panic.
        if u > MAX_RELATION_ID {
            bail!("corrupt key: relation id out of range");
        }
        Ok(Self(u))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::value::{DataValue, Num, Validity, ValidityTs};

    /// The projected encoders' load-bearing law: byte-identical keys to
    /// materialize-then-`encode_as_key`, across every value kind, any
    /// column selection (unordered, repeated), and any literal suffix.
    #[test]
    fn projected_key_encoding_is_byte_identical_to_materialized() {
        let kinds: Vec<DataValue> = vec![
            DataValue::Null,
            DataValue::Bool(true),
            DataValue::from(-42i64),
            DataValue::Num(Num::Float(2.5)),
            DataValue::from("projected"),
            DataValue::Bytes(vec![0, 255, 7]),
            DataValue::List(vec![DataValue::from(1), DataValue::from("x")]),
            DataValue::Validity(Validity {
                timestamp: ValidityTs(std::cmp::Reverse(9)),
                is_assert: std::cmp::Reverse(true),
            }),
            DataValue::Bot,
        ];
        // A deterministic seeded walk over rows/cols/suffixes.
        let mut state = 0x9E37_79B9_u64;
        let mut next = move |m: usize| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as usize % m
        };
        for trial in 0..500 {
            let rel = RelationId(1 + (trial % 7) as u64);
            let row: Vec<DataValue> = (0..1 + next(6))
                .map(|_| kinds[next(kinds.len())].clone())
                .collect();
            let cols: Vec<usize> = (0..next(row.len() + 1)).map(|_| next(row.len())).collect();
            let suffix: Vec<DataValue> = (0..next(3))
                .map(|_| kinds[next(kinds.len())].clone())
                .collect();

            let materialized: Vec<DataValue> = cols
                .iter()
                .map(|&c| row[c].clone())
                .chain(suffix.iter().cloned())
                .collect();
            let expected = materialized.encode_as_key(rel);
            let got = if suffix.is_empty() {
                encode_projected_key(rel, &row, &cols)
            } else {
                encode_projected_key_with_suffix(rel, &row, &cols, &suffix)
            };
            assert_eq!(
                got.0, expected.0,
                "trial {trial}: rel {rel:?} row {row:?} cols {cols:?} suffix {suffix:?}"
            );
        }
    }

    /// [`encode_key_with_suffix`]'s load-bearing law: byte-identical to
    /// `[tuple, suffix].concat().encode_as_key(rel)` — the bulk-write
    /// path's per-row key encoder must not shift a single byte of the
    /// sealed on-disk key format, only avoid materializing the concatenated
    /// tuple.
    #[test]
    fn key_with_suffix_encoding_is_byte_identical_to_materialized() {
        let kinds: Vec<DataValue> = vec![
            DataValue::Null,
            DataValue::Bool(true),
            DataValue::from(-42i64),
            DataValue::Num(Num::Float(2.5)),
            DataValue::from("with_suffix"),
            DataValue::Bytes(vec![0, 255, 7]),
            DataValue::List(vec![DataValue::from(1), DataValue::from("x")]),
            DataValue::Validity(Validity {
                timestamp: ValidityTs(std::cmp::Reverse(9)),
                is_assert: std::cmp::Reverse(true),
            }),
            DataValue::Bot,
        ];
        let mut state = 0xC0FF_EE01_u64;
        let mut next = move |m: usize| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as usize % m
        };
        for trial in 0..500 {
            let rel = RelationId(1 + (trial % 7) as u64);
            let tuple: Vec<DataValue> = (0..next(6))
                .map(|_| kinds[next(kinds.len())].clone())
                .collect();
            let suffix: Vec<DataValue> = (0..next(3))
                .map(|_| kinds[next(kinds.len())].clone())
                .collect();

            let materialized: Vec<DataValue> = tuple
                .iter()
                .cloned()
                .chain(suffix.iter().cloned())
                .collect();
            let expected = materialized.encode_as_key(rel);
            let got = encode_key_with_suffix(rel, &tuple, &suffix);
            assert_eq!(
                got.0, expected.0,
                "trial {trial}: rel {rel:?} tuple {tuple:?} suffix {suffix:?}"
            );
        }
    }
}
