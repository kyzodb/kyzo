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
use serde::{Deserialize, Deserializer};

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
        // `memcmp.rs`'s law 1 (decode(encode(v)) == v), checked cheaply right
        // here where the original tuple is already in hand — every real key
        // write passes through this one function, so this is the single
        // seam to catch a codec bug (new tag, new variant, a byte-order slip)
        // before it ever reaches disk, without re-deriving the generative
        // property tests' whole corpus. Value-level comparison, not a raw
        // byte comparison: some values (e.g. `Json`) can legitimately
        // re-serialize to different bytes that still decode back equal.
        #[cfg(debug_assertions)]
        {
            let decoded = decode_tuple_bare(&ret[EncodedKey::RELATION_PREFIX_LEN..])
                .expect("bytes this function just encoded must decode");
            debug_assert_eq!(
                decoded.as_slice(),
                self.as_ref(),
                "memcmp round-trip violated: decode(encode(tuple)) != tuple"
            );
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

/// A tuple's columns as one memcomparable byte string, with NO relation
/// prefix — the bare in-memory form for structures that compare, sort, and
/// dedup tuples without ever writing them to storage (story #77's
/// representation-tax cut: the fixpoint's own admission bookkeeping is the
/// first consumer, `query/temp_store.rs`). Byte-identical to
/// [`encode_as_key`](TupleT::encode_as_key)'s per-value loop minus the
/// prefix, so it inherits both of `memcmp.rs`'s laws unchanged: round-trip
/// identity and order embedding — comparing two bare encodings byte-for-byte
/// gives exactly the comparison `DataValue`'s own slice `Ord` would give,
/// proven generatively below rather than merely asserted, since the
/// per-value encoder's self-delimiting property (each value ends where the
/// next begins, never ambiguously) is exactly what a concatenation bug would
/// violate.
///
/// Story #77 chunk 2 (`query/temp_store.rs`'s `RegularTempStore`,
/// `query/levels.rs`'s `NormalLevel`) is the first consumer: a regular
/// rule's admission bookkeeping keys on this instead of `Tuple`. The meet
/// path (`MeetAggrStore`/`MeetLevel`) stays `DataValue`-shaped — its fold
/// needs mutable typed values, so bytes buy nothing there — deferred
/// further, not bundled here.
pub(crate) fn encode_tuple_bare(tuple: &[DataValue]) -> Vec<u8> {
    let mut ret = Vec::with_capacity(14 * tuple.len());
    for val in tuple {
        ret.encode_datavalue(val);
    }
    ret
}

/// The inverse of [`encode_tuple_bare`]: self-delimiting, so every column is
/// recovered without a stored width or count — corrupt bytes are a typed
/// error, never a panic, matching [`decode_tuple_from_key`]'s discipline.
pub(crate) fn decode_tuple_bare(bytes: &[u8]) -> Result<Tuple> {
    let mut out = Vec::with_capacity(bytes.len() / 8 + 1);
    let mut remaining = bytes;
    while !remaining.is_empty() {
        let (val, next) = DataValue::decode_from_key(remaining)?;
        out.push(val);
        remaining = next;
    }
    Ok(out)
}

/// The byte length of the first `n` self-delimiting values in a bare
/// encoding (`None` if fewer than `n` are present) — a byte-backed prefix
/// probe's boundary finder: `bytes[..bare_prefix_len(bytes, n)?]` is the
/// bare encoding of `bytes`'s own first `n` columns, comparable directly
/// against another `n`-column bare encoding by the same order-embedding
/// law `encode_tuple_bare` proves for whole tuples (a prefix of a tuple is
/// a tuple).
pub(crate) fn bare_prefix_len(bytes: &[u8], n: usize) -> Option<usize> {
    let mut remaining = bytes;
    for _ in 0..n {
        let (_, next) = DataValue::decode_from_key(remaining).ok()?;
        remaining = next;
    }
    Some(bytes.len() - remaining.len())
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
#[derive(Copy, Clone, Eq, PartialEq, Debug, serde_derive::Serialize, PartialOrd, Ord)]
pub(crate) struct RelationId(pub(crate) u64);

/// Deserialize is hand-written, NOT derived: a derived impl sees the wire's
/// raw `u64` and builds a `RelationId` by direct field assignment (the
/// field is only `pub(crate)`, but serde's derive reaches past that),
/// bypassing the 48-bit bound entirely. `RelationId` is a field of
/// `RelationHandle` — the on-disk catalog row (`runtime/relation.rs`),
/// decoded straight from stored bytes via `rmp_serde::from_slice`
/// (`RelationHandle::decode`) — so a corrupt catalog row could otherwise
/// synthesize an out-of-range id with no error at all. Same seam as
/// `Interval`'s hand-written `Deserialize` in `data/value.rs`: the raw
/// field passes through serde's machinery as a plain `u64`, then
/// [`RelationId::checked`] sees it as untrusted input, not a preexisting
/// invariant, and refuses it with a typed error rather than the
/// programmer-error `assert!` in [`RelationId::new`].
impl<'de> Deserialize<'de> for RelationId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = u64::deserialize(deserializer)?;
        RelationId::checked(raw).map_err(serde::de::Error::custom)
    }
}

/// Relation ids occupy a 48-bit space (the upstream invariant: ids stay
/// within 6 bytes even though the encoded prefix is 8), leaving headroom in
/// the key prefix.
pub(crate) const MAX_RELATION_ID: u64 = 1 << 48;

impl RelationId {
    /// The bound check, shared by every path that must refuse rather than
    /// panic: the wire `Deserialize` above and [`Self::raw_decode`] below.
    /// One function is the single source of truth for the bound so the two
    /// typed-refusal seams cannot drift apart.
    fn checked(u: u64) -> Result<Self> {
        if u > MAX_RELATION_ID {
            bail!("corrupt relation id: {u} exceeds the 48-bit bound");
        }
        Ok(Self(u))
    }

    /// Internal-mint constructor: panics on overflow. Its only production
    /// caller is `encode_tuple_key`'s benches-and-tooling façade, given a
    /// literal or an already-validated id this process already holds —
    /// never bytes read back off disk or off the wire, which route through
    /// [`Self::raw_decode`] or the `Deserialize` impl instead and refuse
    /// typed. An overflow reaching here is a programmer error (a caller
    /// invented a relation id out of thin air), not corrupt data, so the
    /// panic stays: hostile bytes cannot reach this path after the
    /// `Deserialize` fix above.
    pub(crate) fn new(u: u64) -> Self {
        Self::checked(u).unwrap_or_else(|e| panic!("StoredRelId overflow: {e}"))
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
        let arr: [u8; 8] = bytes
            .try_into()
            .map_err(|_| miette!("corrupt key: relation-id prefix"))?;
        let u = u64::from_be_bytes(arr);
        // The overflow assert in `new()` guards the internal-mint path; on
        // the read path an out-of-range id is data corruption and must be
        // an error, never a panic — routed through the same `checked` the
        // `Deserialize` impl uses.
        Self::checked(u).map_err(|_| miette!("corrupt key: relation id out of range"))
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

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
                timestamp: ValidityTs::from_raw(9),
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
                timestamp: ValidityTs::from_raw(9),
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

    /// Valid ids round-trip byte-identically through the hand-written
    /// `Deserialize` — the existing on-disk catalog rows every store
    /// already holds must keep decoding exactly as before this fix.
    #[test]
    fn relation_id_round_trips_through_rmp_serde() {
        for raw in [0u64, 1, 42, MAX_RELATION_ID - 1, MAX_RELATION_ID] {
            let id = RelationId(raw);
            let bytes = rmp_serde::to_vec(&id).unwrap();
            let back: RelationId = rmp_serde::from_slice(&bytes).unwrap();
            assert_eq!(back, id, "round-trip mismatch for {raw}");
        }
    }

    /// The bound is inclusive at `MAX_RELATION_ID` and refuses its
    /// successor — both through the smart constructor directly.
    #[test]
    fn relation_id_boundary_is_inclusive() {
        assert!(RelationId::checked(MAX_RELATION_ID).is_ok());
        assert!(RelationId::checked(MAX_RELATION_ID + 1).is_err());
        assert_eq!(
            RelationId::new(MAX_RELATION_ID),
            RelationId(MAX_RELATION_ID)
        );
    }

    /// The bug this fix closes: hostile wire bytes carrying an
    /// out-of-range id must refuse with a typed error, never panic and
    /// never construct. A newtype struct's msgpack encoding is transparent
    /// (identical bytes to its bare inner value), so a plain `u64` over
    /// the bound stands in for the corrupt wire form directly — no need to
    /// reach for `RelationId`'s own (now bound-checked) `Serialize`.
    #[test]
    fn relation_id_deserialize_refuses_out_of_range_bytes() {
        let hostile_raw = MAX_RELATION_ID + 1;
        let bytes = rmp_serde::to_vec(&hostile_raw).unwrap();
        let err = rmp_serde::from_slice::<RelationId>(&bytes).unwrap_err();
        assert!(
            err.to_string().contains("48-bit bound"),
            "unexpected error: {err}"
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // encode_tuple_bare/decode_tuple_bare (story #77): the two memcmp laws,
    // transferred to the whole-tuple bare form.
    // ─────────────────────────────────────────────────────────────────────

    /// Round-trip identity and order embedding for [`encode_tuple_bare`],
    /// over variable-length tuples of every value kind — including the
    /// prefix case (`[1]` vs `[1, 2]`), where a concatenation bug would most
    /// plausibly hide: the self-delimiting per-value encoding must make a
    /// strict tuple prefix's bytes a strict byte-prefix of the longer
    /// tuple's bytes, matching `Vec<DataValue>`'s own derived `Ord`.
    #[test]
    fn bare_tuple_round_trips_and_preserves_order_generatively() {
        let kinds: Vec<DataValue> = vec![
            DataValue::Null,
            DataValue::Bool(true),
            DataValue::Bool(false),
            DataValue::from(-42i64),
            DataValue::from(0i64),
            DataValue::from(42i64),
            DataValue::Num(Num::Float(2.5)),
            DataValue::Num(Num::Float(-0.5)),
            DataValue::from(""),
            DataValue::from("bare_tuple"),
            DataValue::Bytes(vec![]),
            DataValue::Bytes(vec![0, 255, 7]),
            DataValue::List(vec![DataValue::from(1), DataValue::from("x")]),
            DataValue::List(vec![]),
            DataValue::Validity(Validity {
                timestamp: ValidityTs::from_raw(9),
                is_assert: std::cmp::Reverse(true),
            }),
            DataValue::Bot,
        ];
        let mut state = 0xB47E_u64;
        let mut next = move |m: usize| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as usize % m
        };
        for trial in 0..1000 {
            let a: Vec<DataValue> = (0..next(6))
                .map(|_| kinds[next(kinds.len())].clone())
                .collect();
            let b: Vec<DataValue> = (0..next(6))
                .map(|_| kinds[next(kinds.len())].clone())
                .collect();

            // Law 1: round-trip identity.
            let encoded_a = encode_tuple_bare(&a);
            let decoded_a = decode_tuple_bare(&encoded_a).expect("valid encoding decodes");
            assert_eq!(decoded_a, a, "trial {trial}: round-trip mismatch for {a:?}");

            // Law 2: order embedding — bytewise order equals the semantic
            // slice order `Vec<DataValue>`'s derived `Ord` already gives.
            let encoded_b = encode_tuple_bare(&b);
            assert_eq!(
                a.cmp(&b),
                encoded_a.cmp(&encoded_b),
                "trial {trial}: order disagreement between {a:?} and {b:?}"
            );
        }
    }

    /// The prefix case in isolation, pinned by example rather than left to
    /// the generative walk's odds: a tuple that is a strict prefix of
    /// another (same leading columns, the longer one has more) must encode
    /// so the shorter one's bytes are a strict byte-prefix of the longer
    /// one's — never a tie, never reversed.
    #[test]
    fn bare_tuple_prefix_relationship_is_preserved() {
        let short = vec![DataValue::from(1i64)];
        let long = vec![DataValue::from(1i64), DataValue::from(2i64)];
        assert_eq!(short.cmp(&long), Ordering::Less);
        let (enc_short, enc_long) = (encode_tuple_bare(&short), encode_tuple_bare(&long));
        assert_eq!(enc_short.cmp(&enc_long), Ordering::Less);
        assert!(
            enc_long.starts_with(&enc_short),
            "a tuple prefix's bytes must be a strict byte-prefix of the extension's"
        );
    }

    /// The empty tuple encodes to the empty byte string and decodes back
    /// to itself — the base case every recursive proof above assumes.
    #[test]
    fn bare_empty_tuple_round_trips() {
        let empty: Tuple = vec![];
        let encoded = encode_tuple_bare(&empty);
        assert!(encoded.is_empty());
        assert_eq!(decode_tuple_bare(&encoded).unwrap(), empty);
    }

    /// Corrupt bytes refuse typed, never panic — the same discipline
    /// `decode_from_key` already proves per-value; this pins it survives
    /// composition into a whole-tuple walk.
    #[test]
    fn bare_decode_never_panics_on_arbitrary_bytes() {
        let mut state = 0xDEAD_BEEF_u64;
        let mut next_byte = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        };
        for len in 0..40 {
            let bytes: Vec<u8> = (0..len).map(|_| next_byte()).collect();
            let _ = decode_tuple_bare(&bytes);
        }
    }

    /// [`bare_prefix_len`]'s load-bearing law: the byte range it returns for
    /// a row's first `n` columns is byte-identical to bare-encoding just
    /// those `n` columns on their own — the exact property `NormalLevel`'s
    /// prefix probe (`query/levels.rs`) depends on to compare a stored row's
    /// leading columns without decoding them.
    #[test]
    fn bare_prefix_len_matches_a_standalone_prefix_encoding() {
        let row = vec![
            DataValue::from(1i64),
            DataValue::from("middle"),
            DataValue::List(vec![DataValue::from(2i64)]),
            DataValue::Bot,
        ];
        let encoded = encode_tuple_bare(&row);
        for n in 0..=row.len() {
            let boundary = bare_prefix_len(&encoded, n).unwrap();
            let standalone = encode_tuple_bare(&row[..n]);
            assert_eq!(
                &encoded[..boundary],
                standalone.as_slice(),
                "n={n}: prefix boundary bytes diverged from a standalone encoding"
            );
        }
        // Asking for more columns than the row has is a clean `None`, not a
        // truncated/garbage boundary.
        assert!(bare_prefix_len(&encoded, row.len() + 1).is_none());
    }
}
