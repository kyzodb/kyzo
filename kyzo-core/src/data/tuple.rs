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
//! bytes, and (for versioned facts) a fixed-width validity tail. Encoding is
//! infallible; decoding parses *claimed* bytes and is fallible everywhere.

use miette::{Result, bail, miette};
use std::cmp::Reverse;

use crate::data::memcmp::{ENCODED_VLD_LEN, MemCmpEncoder};
use crate::data::value::{DataValue, TERMINAL_VALIDITY, Validity, ValidityTs};

/// A fact's body: an ordered sequence of values.
pub type Tuple = Vec<DataValue>;

/// A fact's written form: the relation prefix followed by the memcomparable
/// tuple encoding, with any validity as the fixed-width tail.
///
/// Only encoders construct this — possession is proof of well-formed
/// provenance. Bytes read back from storage are *claimed* keys until
/// decoding proves them; they stay `&[u8]` in signatures on purpose.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EncodedKey(Vec<u8>);

impl EncodedKey {
    /// Every key begins with this many bytes of relation id.
    pub const RELATION_PREFIX_LEN: usize = 8;
    /// An encoded validity, when present, is exactly this many trailing bytes.
    pub const VALIDITY_TAIL_LEN: usize = ENCODED_VLD_LEN;

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

/// Check if the tuple key passed in should be a valid return for a validity query.
///
/// Returns two elements: the first contains `Some(tuple)` if the key belongs
/// in the result set and `None` otherwise; the second gives the next seek
/// bound (inclusive lower). The bound is raw bytes, not an [`EncodedKey`]:
/// its prefix comes from stored bytes that were never fully decoded.
///
/// The validity occupies a fixed-width tail at the end of the key, so seek
/// keys are computed by splicing bytes: the (potentially long) tuple prefix
/// is never decoded or re-encoded on the skip paths — only an emitted hit
/// pays for a full decode.
pub fn check_key_for_validity(
    key: &[u8],
    valid_at: ValidityTs,
    size_hint: Option<usize>,
) -> Result<(Option<Tuple>, Vec<u8>)> {
    if key.len() < EncodedKey::RELATION_PREFIX_LEN + EncodedKey::VALIDITY_TAIL_LEN {
        bail!("time-travel scan over a key too short to carry a validity");
    }
    let vld_off = key.len() - EncodedKey::VALIDITY_TAIL_LEN;
    let (vld_val, rest) = DataValue::decode_from_key(&key[vld_off..])?;
    let DataValue::Validity(vld) = vld_val else {
        bail!("time-travel scan over a key without a trailing validity");
    };
    if !rest.is_empty() {
        bail!("time-travel scan over a key with trailing bytes after its validity");
    }

    // The spliced result is a seek BOUND, not an EncodedKey: only the
    // validity tail was proven above — the prefix is raw stored bytes, and
    // blessing them into EncodedKey would launder unproven data into a type
    // whose possession means provenance. Bounds live in the claimed-bytes
    // domain (the seek loop's byte-successor bounds are not keys either).
    let splice = |v: Validity| -> Vec<u8> {
        let mut nxt = Vec::with_capacity(key.len());
        nxt.extend_from_slice(&key[..vld_off]);
        nxt.encode_datavalue(&DataValue::Validity(v));
        nxt
    };

    if vld.timestamp < valid_at {
        // Version is newer than the query time: seek to the newest version
        // at or before `valid_at` for this same tuple.
        Ok((
            None,
            splice(Validity {
                timestamp: valid_at,
                is_assert: Reverse(true),
            }),
        ))
    } else if !vld.is_assert.0 {
        // Retraction: this tuple does not exist at `valid_at`; skip past all
        // of its remaining (older) versions.
        Ok((None, splice(TERMINAL_VALIDITY)))
    } else {
        // Hit: emit it, then skip past this tuple's older versions.
        let decoded = decode_tuple_from_key(key, size_hint.unwrap_or(DEFAULT_SIZE_HINT))?;
        Ok((Some(decoded), splice(TERMINAL_VALIDITY)))
    }
}

/// Stored values carry this many bytes of header before the rmp payload.
const VALUE_HEADER_LEN: usize = 8;

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
/// MEASURED (vectorization camp 2): a streaming `DeserializeSeed` that
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
const MAX_RELATION_ID: u64 = 1 << 48;

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
