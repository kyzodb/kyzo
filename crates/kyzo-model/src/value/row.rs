/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `Row`: an interned tuple — a slice of `Code`s — with a `Value`-cell view only at the API boundary. `StorageKey` is the written form.
//!
//! ## The code-lifetime law (the two-form Row)
//!
//! **Codes never persist across a seal. The durable form is canonical
//! bytes. Codes are within-epoch execution currency.**
//!
//! The two forms are two types with one conversion authority each way:
//!
//! - [`Rows`] is the execution form: row-major packed raw codes under ONE
//!   container domain (it *is* a [`CodeColumn`] with an arity, inheriting
//!   the write door, the admission theorem, and the gather law wholesale).
//!   It has **no serialization surface** — you cannot write codes down.
//! - [`StorageKey`] is the written form: the tuple's canonical encodings
//!   concatenated. Self-terminating element encodings make concatenation
//!   order-preserving (lexicographic tuple order = elementwise semantic
//!   order) and unambiguous to split. It has **no code accessors** — you
//!   cannot smuggle execution currency out of stored bytes.
//! - The doors: [`AdmittedRows::encode_row`] (execution → bytes, through
//!   an admitted observer) and [`Rows::push_encoded`] (bytes → execution,
//!   validated element-by-element, re-interned into the current epoch).
//!
//! Consequence: a seal invalidates nothing durable. Standing state at
//! rest is bytes and needs no gather; only live in-memory containers
//! cross epochs, explicitly, through their gather doors.
//!
//! ## The fixpoint choreography
//!
//! A semi-naive iteration alternates a **read phase** (frame open, joins
//! and dedup on admitted raw codes — identity is exact within the domain)
//! with a **mint phase** (frame dropped, newly derived values interned,
//! stamps pushed through the write door). `intern` takes `&mut Arena`, so
//! the borrow checker enforces the alternation; the epoch is unchanged
//! throughout, so held containers stay admissible with no remap mid-run.
//! The choreography is pinned as a law test below.

use super::admission::Denial;
use super::arena::{Arena, BulkObserver, EpochRemap};
use super::arity::Arity;
use super::canonical::{DecodeError, decode_one};
use super::code::StampedCode;
use super::column::{AdmittedCodes, CodeColumn, Domain};
use super::{DataValue, ScanBound};
use crate::data_value_any;

/// A stream of tuples, each fallibly produced (a storage read can fail mid-stream).
pub type TupleIter<'a> = Box<dyn Iterator<Item = miette::Result<super::Tuple>> + 'a>;

/// The execution form of a relation fragment: `arity`-wide tuples as
/// row-major packed codes under one container domain.
pub struct Rows {
    arity: Arity,
    codes: CodeColumn,
}

impl Rows {
    /// An empty tuple container in the observer's domain.
    ///
    /// Zero width is unrepresentable: [`Arity`] is [`NonZeroUsize`](std::num::NonZeroUsize)-backed.
    pub fn new_in<O: BulkObserver>(arity: Arity, o: &O) -> Rows {
        Rows {
            arity,
            codes: CodeColumn::new_in(o),
        }
    }

    pub fn arity(&self) -> Arity {
        self.arity
    }

    pub fn len(&self) -> usize {
        self.codes.len() / self.arity.get()
    }

    pub fn is_empty(&self) -> bool {
        self.codes.is_empty()
    }

    pub fn domain(&self) -> Domain {
        self.codes.domain()
    }

    /// The write door: one tuple of stamped codes, verified element by
    /// element into the domain. Typed refusal on foreign/stale stamps or
    /// arity mismatch — never a process abort.
    pub fn push_row(&mut self, stamps: &[StampedCode]) -> Result<(), Denial> {
        let expected = self.arity.get();
        let got = stamps.len();
        if got != expected {
            return Err(Denial::ArityMismatch { expected, got });
        }
        for &sc in stamps {
            self.codes.push(sc)?;
        }
        Ok(())
    }

    /// The bytes→execution door: refuse a stale/foreign container with a
    /// TYPED error (storage ingestion is a refusal surface, not a panic
    /// surface), validate the written key element by element (total),
    /// and only then re-intern into the current epoch.
    pub fn push_encoded(&mut self, key: &TupleKey, arena: &mut Arena) -> Result<(), PushError> {
        if self.domain().arena_id() != arena.id() {
            return Err(PushError::ForeignArena);
        }
        if self.domain().epoch() != arena.epoch() {
            return Err(PushError::StaleDomain {
                container: self.domain().epoch(),
                arena: arena.epoch(),
            });
        }
        let bytes = key.as_bytes();
        // Validate and split FIRST — nothing is interned unless the whole
        // key is lawful (no partial tuples on refusal).
        let splits = split_key(bytes, self.arity.get()).map_err(PushError::Decode)?;
        for (lo, hi) in splits {
            let sc = arena.intern(&bytes[lo..hi]).map_err(PushError::Denial)?;
            self.codes.push(sc).map_err(PushError::Denial)?;
        }
        Ok(())
    }

    /// The admission: one container-domain check for the whole relation
    /// fragment. Arena/epoch mismatch is a typed refusal.
    ///
    /// **Coexisting-arena boundary:** delegates to [`CodeColumn::admit`] —
    /// mint-checked [`super::admission::Admission`], not a nest brand (rows
    /// outlive observer nests; see [`super::code`] measurement).
    pub fn admit<'a, O: BulkObserver>(&'a self, o: &'a O) -> Result<AdmittedRows<'a, O>, Denial> {
        Ok(AdmittedRows {
            arity: self.arity,
            codes: self.codes.admit(o)?,
        })
    }

    /// The gather door (see the gather law): consuming, the only mint of
    /// a new-epoch tuple container. Typed refusal on a foreign/wrong-epoch
    /// remap.
    pub fn gather(self, remap: &EpochRemap) -> Result<Rows, Denial> {
        Ok(Rows {
            arity: self.arity,
            codes: self.codes.gather(remap)?,
        })
    }
}

/// Admitted tuples: raw-code reads under the proven domain.
pub struct AdmittedRows<'a, O: BulkObserver> {
    arity: Arity,
    codes: AdmittedCodes<'a, O>,
}

impl<'a, O: BulkObserver> AdmittedRows<'a, O> {
    pub fn len(&self) -> usize {
        self.codes.len() / self.arity.get()
    }

    pub fn is_empty(&self) -> bool {
        self.codes.is_empty()
    }

    pub fn arity(&self) -> Arity {
        self.arity
    }

    /// The flat raw codes of every tuple — identity currency for bulk
    /// dedup within this domain; never an ordering surface.
    pub fn raw(&self) -> &'a [u32] {
        self.codes.raw()
    }

    /// The raw codes of row `i` — tuple identity within this domain
    /// (equality/hash/dedup currency; never an ordering surface).
    pub fn row(&self, i: usize) -> &'a [u32] {
        let w = self.arity.get();
        &self.codes.raw()[i * w..(i + 1) * w]
    }

    /// Canonical bytes of cell `(row, col)`.
    pub fn resolve_cell(&self, row: usize, col: usize) -> Result<&'a [u8], Denial> {
        self.codes.resolve(row * self.arity.get() + col)
    }

    /// Semantic tuple order: elementwise value order (which is exactly
    /// what the written form's byte order embeds).
    pub fn cmp_rows(&self, i: usize, j: usize) -> Result<std::cmp::Ordering, Denial> {
        let w = self.arity.get();
        for k in 0..w {
            let c = self.codes.cmp_at(i * w + k, j * w + k)?;
            if c != std::cmp::Ordering::Equal {
                return Ok(c);
            }
        }
        Ok(std::cmp::Ordering::Equal)
    }

    /// The execution→bytes door: the written form of row `i`. Minted only
    /// here — an `TupleKey` in hand is proof its bytes are concatenated
    /// canonical encodings.
    pub fn encode_row(&self, i: usize) -> Result<TupleKey, Denial> {
        let mut out = Vec::new();
        for k in 0..self.arity.get() {
            out.extend_from_slice(self.resolve_cell(i, k)?);
        }
        Ok(TupleKey(out))
    }
}

/// Typed refusals of the bytes→execution door.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PushError {
    /// The key bytes are not `arity` lawful canonical encodings.
    Decode(DecodeError),
    /// The container belongs to a different arena than the interning one.
    ForeignArena,
    /// The container's epoch is not the arena's: gather first.
    StaleDomain {
        container: super::arena::Epoch,
        arena: super::arena::Epoch,
    },
    /// Intern or domain absorb refused (capacity / stamp / extent).
    Denial(Denial),
}

/// Split a key into exactly `arity` lawful canonical encodings, refusing
/// truncation, malformation, and trailing bytes.
fn split_key(bytes: &[u8], arity: usize) -> Result<Vec<(usize, usize)>, DecodeError> {
    let mut splits = Vec::with_capacity(arity);
    let mut at = 0usize;
    for _ in 0..arity {
        let (_, used) = decode_one(&bytes[at..])?;
        splits.push((at, at + used));
        at += used;
    }
    if at != bytes.len() {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(splits)
}

/// Column-wise bound arrays close a scan key the moment they hit a
/// sentinel: `Value` columns keep contributing bytes, `Least` ends the
/// key with nothing (every extension sorts at-or-after), `Greatest` ends
/// it with the `0xFF` byte no canonical encoding begins (every extension
/// sorts before). An UPPER key that runs out of bounds without a
/// sentinel gets the `0xFF` tail — the scan includes every extension of
/// its value prefix.
fn append_bounds(out: &mut Vec<u8>, bounds: &[ScanBound], upper: bool) {
    for b in bounds {
        match b {
            ScanBound::Value(v) => super::canonical::append_canonical(out, v),
            ScanBound::Least => return,
            ScanBound::Greatest => {
                out.push(0xFF);
                return;
            }
        }
    }
    if upper {
        out.push(0xFF);
    }
}

/// Bare written tuple: concatenated canonical encodings with NO relation
/// prefix. Proof that bytes are arity-split lawful encodings — never a
/// storage keyspace key.
///
/// @authority TupleKey
/// @layer value
/// @owns bare-tuple memcmp identity (no relation prefix); bytewise order equals DataValue structural order (byte-order law)
/// @constructs TupleKey::from_values | TupleKey::from_stored
/// @forbids forging bytes bypassing canonical encode | treating a TupleKey as a relation-prefixed StorageKey
/// @converts TupleKey -> StorageKey (prefix with RelationId at the storage boundary)
/// @gate round-trip + byte-order law (storage format gate)
/// @status established #303
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct TupleKey(Vec<u8>);

const _: () = assert!(std::mem::size_of::<TupleKey>() == std::mem::size_of::<Vec<u8>>());
const _: () = assert!(std::mem::align_of::<TupleKey>() == std::mem::align_of::<Vec<u8>>());

/// Relation-prefixed storage key (keyspace layout v1): 8-byte relation id
/// then key columns (then optional bitemporal tails).
///
/// @authority StorageKey
/// @layer value
/// @owns canonical storage identity in the memcmp keyspace; bytewise order equals DataValue structural order (byte-order law)
/// @constructs TupleT::encode_as_key | encode_key_with_suffix
/// @forbids leaking codes out of a StorageKey | forging bytes bypassing canonical encode | confusing storage identity with record/entity identity | treating a StorageKey as a bare TupleKey
/// @converts StorageKey -> Domain (admitted into an arena via push_encoded)
/// @gate round-trip + byte-order law (storage format gate)
/// @status established #303
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct StorageKey(pub(crate) Vec<u8>);

const _: () = assert!(std::mem::size_of::<StorageKey>() == std::mem::size_of::<Vec<u8>>());
const _: () = assert!(std::mem::align_of::<StorageKey>() == std::mem::align_of::<Vec<u8>>());

/// A stored relation's identity: the 8-byte big-endian keyspace prefix
/// every key of the relation opens with (storage key layout v1).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct RelationId(u64);

const _: () = assert!(std::mem::size_of::<RelationId>() == std::mem::size_of::<u64>());
const _: () = assert!(std::mem::align_of::<RelationId>() == std::mem::align_of::<u64>());

impl RelationId {
    /// The system catalog keyspace.
    pub const SYSTEM: RelationId = RelationId(0);

    /// The one checked constructor: `None` at or beyond [`RelationId::CAP`].
    /// Every other mint (decode, allocation) routes through the same
    /// refusal, so an over-cap id is unrepresentable.
    pub const fn new(raw: u64) -> Option<RelationId> {
        if raw >= RelationId::CAP {
            None
        } else {
            Some(RelationId(raw))
        }
    }

    /// The raw id (read-only; construction goes through [`RelationId::new`]
    /// or [`RelationId::raw_decode`]).
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// The exclusive allocation ceiling: every assignable id stays below
    /// `1 << 48`, so a key's 8-byte relation prefix always begins with two
    /// `0x00` bytes — far below the `0xFF` the scan-bound vocabulary
    /// reserves as its `Greatest` tail, and the bound every storage
    /// consumer (merkle roots, keyspace probes) already assumes.
    pub const CAP: u64 = 1_u64 << 48;

    pub fn raw_encode(self) -> [u8; 8] {
        self.0.to_be_bytes()
    }

    /// Decode 8 big-endian bytes as a relation id, REFUSING anything at
    /// or beyond [`RelationId::CAP`] — the exhaustion door: stored bytes
    /// cannot smuggle an unassignable id back into the allocator.
    pub fn raw_decode(bytes: &[u8]) -> Result<RelationId, DecodeError> {
        let Some(head) = bytes.get(..8) else {
            return Err(DecodeError::Truncated);
        };
        let mut arr = [0u8; 8];
        arr.copy_from_slice(head);
        let id = u64::from_be_bytes(arr);
        if id >= RelationId::CAP {
            return Err(DecodeError::RelationIdOverCap);
        }
        Ok(RelationId(id))
    }

    /// The next id, `None` on exhaustion or at/beyond [`RelationId::CAP`]
    /// — the same ceiling as [`RelationId::new`] / [`RelationId::raw_decode`].
    pub fn next(self) -> Option<RelationId> {
        self.0.checked_add(1).and_then(RelationId::new)
    }
}

/// Displays as the bare numeric id — the form diagnostics and error
/// messages carry; the typed identity never has to degrade to a raw `u64`
/// just to be printed.
impl std::fmt::Display for RelationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Key encoding for anything that dereferences to a value slice: the
/// relation prefix, then each value's canonical bytes.
pub trait TupleT {
    fn encode_as_key(&self, rel: RelationId) -> StorageKey;
}

impl<S: AsRef<[DataValue]> + ?Sized> TupleT for S {
    fn encode_as_key(&self, rel: RelationId) -> StorageKey {
        encode_key_with_suffix(rel, self.as_ref(), &[])
    }
}

/// The write path's key mint: prefix, key columns, then a value suffix
/// (e.g. the two bitemporal slots), in one pass.
pub fn encode_key_with_suffix(
    rel: RelationId,
    cols: &[DataValue],
    suffix: &[DataValue],
) -> StorageKey {
    let mut out = Vec::with_capacity(8 + 16 * (cols.len() + suffix.len()));
    out.extend_from_slice(&rel.raw_encode());
    for v in cols {
        super::canonical::append_canonical(&mut out, v);
    }
    for v in suffix {
        super::canonical::append_canonical(&mut out, v);
    }
    StorageKey(out)
}

impl std::ops::Deref for TupleKey {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl AsRef<[u8]> for TupleKey {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl std::ops::Deref for StorageKey {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl AsRef<[u8]> for StorageKey {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl TupleKey {
    /// The lawful multi-value mint: encode each value through the codec
    /// authority and concatenate — no relation prefix.
    pub fn from_values<'v>(values: impl IntoIterator<Item = &'v super::DataValue>) -> TupleKey {
        let mut out = Vec::new();
        for v in values {
            out.extend_from_slice(super::canonical::encode_owned(v).as_bytes());
        }
        TupleKey(out)
    }

    /// Claim stored bare-tuple bytes by proving they split into exactly
    /// `arity` lawful canonical encodings with nothing trailing.
    pub fn from_stored(bytes: Vec<u8>, arity: usize) -> Result<TupleKey, DecodeError> {
        split_key(&bytes, arity)?;
        Ok(TupleKey(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl StorageKey {
    /// Storage key layout v1: keys open with the relation id as 8
    /// big-endian bytes (the keyspace prefix), then the key columns'
    /// canonical encodings, then — for bitemporal relations — the two
    /// fixed-width validity slots.
    pub const RELATION_PREFIX_LEN: usize = 8;
    /// One canonical validity slot: tag byte + 9-byte payload.
    pub const VALIDITY_TAIL_LEN: usize = 10;
    /// Both time slots of a bitemporal key.
    pub const BITEMPORAL_TAIL_LEN: usize = 2 * Self::VALIDITY_TAIL_LEN;

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The LOWER scan key for `prefix` columns then column-wise `bounds`.
pub fn scan_key_lower(rel: RelationId, prefix: &[DataValue], bounds: &[ScanBound]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 16 * (prefix.len() + bounds.len()));
    out.extend_from_slice(&rel.raw_encode());
    for v in prefix {
        super::canonical::append_canonical(&mut out, v);
    }
    append_bounds(&mut out, bounds, false);
    out
}

/// The UPPER scan key (inclusive of every extension; see
/// [`scan_key_lower`] for the sentinel law).
pub fn scan_key_upper(rel: RelationId, prefix: &[DataValue], bounds: &[ScanBound]) -> Vec<u8> {
    let mut out = Vec::with_capacity(9 + 16 * (prefix.len() + bounds.len()));
    out.extend_from_slice(&rel.raw_encode());
    for v in prefix {
        super::canonical::append_canonical(&mut out, v);
    }
    append_bounds(&mut out, bounds, true);
    out
}

/// [`scan_key_lower`] with the prefix read through a projection of
/// `row` — the zero-materialization probe path.
pub fn scan_key_lower_projected(
    rel: RelationId,
    row: &[DataValue],
    cols: &[usize],
    bounds: &[ScanBound],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 16 * (cols.len() + bounds.len()));
    out.extend_from_slice(&rel.raw_encode());
    for &c in cols {
        super::canonical::append_canonical(&mut out, &row[c]);
    }
    append_bounds(&mut out, bounds, false);
    out
}

/// [`scan_key_upper`] through a projection of `row`.
pub fn scan_key_upper_projected(
    rel: RelationId,
    row: &[DataValue],
    cols: &[usize],
    bounds: &[ScanBound],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(9 + 16 * (cols.len() + bounds.len()));
    out.extend_from_slice(&rel.raw_encode());
    for &c in cols {
        super::canonical::append_canonical(&mut out, &row[c]);
    }
    append_bounds(&mut out, bounds, true);
    out
}

#[cfg(test)]
mod tests {
    use super::super::admission::Denial;
    use super::super::canonical::{Datum, encode};
    use super::super::code::StampedCode;
    use super::super::number::Num;
    use super::*;

    fn stamp_of(arena: &mut Arena, d: Datum<'_>) -> StampedCode {
        arena.intern(encode(d).as_bytes()).expect("intern")
    }

    // ------------------------------------------------------------------
    // The two-form law: written bytes are the durable identity; codes
    // move under seals while the bytes do not.
    // ------------------------------------------------------------------

    #[test]
    fn written_form_is_durable_across_seals_while_codes_move() {
        let mut arena = Arena::new();
        let mut rows = Rows::new_in(Arity::try_new(2).expect("test arity 2"), &arena.frame());
        for i in 0..30i64 {
            let a = stamp_of(&mut arena, Datum::Num(Num::int(i * 7 % 13)));
            let b = stamp_of(
                &mut arena,
                Datum::Str(if i % 2 == 0 { "even" } else { "odd" }),
            );
            rows.push_row(&[a, b]).expect("lawful push");
        }
        let keys_before: Vec<TupleKey> = {
            let f = arena.frame();
            let adm = rows.admit(&f).expect("lawful admit");
            (0..adm.len())
                .map(|i| adm.encode_row(i).expect("lawful"))
                .collect()
        };
        let raw_before: Vec<Vec<u32>> = {
            let f = arena.frame();
            let adm = rows.admit(&f).expect("lawful admit");
            (0..adm.len()).map(|i| adm.row(i).to_vec()).collect()
        };
        // Seal + gather: the execution currency moves...
        let remap = arena.seal().expect("lawful seal");
        let rows = rows.gather(&remap).expect("lawful gather");
        // ...and moves visibly (something re-ranked: 13 distinct nums +
        // 2 strings all started as tail codes).
        let f = arena.frame();
        let adm = rows.admit(&f).expect("lawful admit");
        let raw_after: Vec<Vec<u32>> = (0..adm.len()).map(|i| adm.row(i).to_vec()).collect();
        assert_ne!(
            raw_before, raw_after,
            "seal moved no codes — test is vacuous"
        );
        // ...while the written form is byte-identical, row for row.
        for (i, k) in keys_before.iter().enumerate() {
            assert_eq!(
                &adm.encode_row(i).expect("lawful"),
                k,
                "the durable form moved with the seal"
            );
        }
    }

    /// The written form's byte order embeds elementwise tuple order.
    #[test]
    fn encoded_key_order_is_tuple_semantic_order() {
        let mut arena = Arena::new();
        let mut rows = Rows::new_in(Arity::try_new(2).expect("test arity 2"), &arena.frame());
        let tuples: [(i64, &str); 5] = [(3, "b"), (1, "zzz"), (3, "a"), (-5, "x"), (1, "a")];
        for (n, s) in tuples {
            let a = stamp_of(&mut arena, Datum::Num(Num::int(n)));
            let b = stamp_of(&mut arena, Datum::Str(s));
            rows.push_row(&[a, b]).expect("lawful push");
        }
        let f = arena.frame();
        let adm = rows.admit(&f).expect("lawful admit");
        for i in 0..adm.len() {
            for j in 0..adm.len() {
                assert_eq!(
                    adm.encode_row(i)
                        .expect("lawful")
                        .cmp(&adm.encode_row(j).expect("lawful")),
                    adm.cmp_rows(i, j).expect("lawful"),
                    "key byte order diverged from tuple order at ({i},{j})"
                );
            }
        }
    }

    /// bytes → execution → bytes round-trips exactly; malformed keys
    /// refuse without partial pushes.
    #[test]
    fn push_encoded_round_trips_and_refuses_totally() {
        let mut arena = Arena::new();
        let mut rows = Rows::new_in(Arity::try_new(2).expect("test arity 2"), &arena.frame());
        let a = stamp_of(&mut arena, Datum::Num(Num::int(42)));
        let b = stamp_of(&mut arena, Datum::Str("hello"));
        rows.push_row(&[a, b]).expect("lawful push");
        let key = {
            let f = arena.frame();
            rows.admit(&f)
                .expect("lawful admit")
                .encode_row(0)
                .expect("lawful")
        };
        // Re-enter through the bytes door.
        let mut rows2 = Rows::new_in(Arity::try_new(2).expect("test arity 2"), &arena.frame());
        rows2.push_encoded(&key, &mut arena).expect("lawful key");
        {
            let f = arena.frame();
            let adm2 = rows2.admit(&f).expect("lawful admit");
            assert_eq!(
                adm2.encode_row(0).expect("lawful"),
                key,
                "bytes door changed the tuple"
            );
            // Same epoch + arena dedup ⟹ same codes: tuple identity holds.
            let adm = rows.admit(&f).expect("lawful admit");
            assert_eq!(adm.row(0), adm2.row(0));
        }
        // Truncated key: typed refusal, nothing pushed.
        let cut = TupleKey(key.as_bytes()[..key.len() - 3].to_vec());
        let before = rows2.len();
        assert!(rows2.push_encoded(&cut, &mut arena).is_err());
        assert_eq!(rows2.len(), before, "refusal left a partial tuple");
        // Trailing garbage: refused.
        let mut fat = key.as_bytes().to_vec();
        fat.push(0x05);
        assert!(rows2.push_encoded(&TupleKey(fat), &mut arena).is_err());
        assert_eq!(rows2.len(), before);
    }

    // ------------------------------------------------------------------
    // The fixpoint choreography, pinned: read phase / mint phase
    // alternation, identity-dedup on raw codes, stability across rounds,
    // and the commit boundary (seal + gather) at the end.
    // ------------------------------------------------------------------

    #[test]
    fn fixpoint_choreography_law() {
        let mut arena = Arena::new();
        let epoch0 = arena.epoch();
        // Seed relation: reach(x) for x in {0}; rule: reach(x+3) up to 12.
        let mut total = Rows::new_in(Arity::ONE, &arena.frame());
        let seed = stamp_of(&mut arena, Datum::Num(Num::int(0)));
        total.push_row(&[seed]).expect("lawful push");
        let mut frontier: Vec<Vec<u8>> = vec![encode(Datum::Num(Num::int(0))).as_bytes().to_vec()];
        let mut rounds = 0;
        while !frontier.is_empty() {
            rounds += 1;
            // MINT PHASE: derive new values from the frontier as bytes,
            // intern them (frame necessarily closed: intern is &mut).
            let mut fresh: Vec<StampedCode> = Vec::new();
            for bytes in frontier.drain(..) {
                let (datum, _) = decode_one(&bytes).expect("lawful");
                let n = match datum {
                    super::super::DataValue::Num(n) => n.as_int().expect("int domain"),
                    other @ (data_value_any!()) => panic!("wrong kind: {other:?}"),
                };
                if n + 3 <= 12 {
                    fresh.push(stamp_of(&mut arena, Datum::Num(Num::int(n + 3))));
                }
            }
            // READ PHASE: dedup the derived tuples against the total by
            // raw-code identity under one admitted domain, then extend.
            let novel: Vec<StampedCode> = {
                let f = arena.frame();
                let adm = total.admit(&f).expect("lawful admit");
                let existing: std::collections::BTreeSet<u32> = adm.raw().iter().copied().collect();
                fresh
                    .into_iter()
                    .filter(|sc| !existing.contains(&sc.code().raw()))
                    .collect()
            };
            for sc in &novel {
                total.push_row(&[*sc]).expect("lawful push");
                let f = arena.frame();
                let adm = total.admit(&f).expect("lawful admit");
                frontier.push(adm.resolve_cell(adm.len() - 1, 0).expect("lawful").to_vec());
            }
            assert_eq!(arena.epoch(), epoch0, "no seal mid-fixpoint");
            assert!(rounds < 32, "fixpoint diverged");
        }
        // Fixpoint reached: {0,3,6,9,12}.
        assert_eq!(total.len(), 5);
        let keys_at_fixpoint: Vec<TupleKey> = {
            let f = arena.frame();
            let adm = total.admit(&f).expect("lawful admit");
            (0..adm.len())
                .map(|i| adm.encode_row(i).expect("lawful"))
                .collect()
        };
        // COMMIT BOUNDARY: seal once, gather the held container, and the
        // durable form is untouched.
        let remap = arena.seal().expect("lawful seal");
        let total = total.gather(&remap).expect("lawful gather");
        let f = arena.frame();
        let adm = total.admit(&f).expect("lawful admit");
        for (i, k) in keys_at_fixpoint.iter().enumerate() {
            assert_eq!(&adm.encode_row(i).expect("lawful"), k);
        }
    }

    /// The storage door: stored bytes become a key only by proving the
    /// split; garbage and wrong-arity bytes refuse typed.
    #[test]
    fn from_stored_is_a_validating_door() {
        let mut arena = Arena::new();
        let mut rows = Rows::new_in(Arity::try_new(2).expect("test arity 2"), &arena.frame());
        let a = stamp_of(&mut arena, Datum::Num(Num::int(1)));
        let b = stamp_of(&mut arena, Datum::Str("s"));
        rows.push_row(&[a, b]).expect("lawful push");
        let key = {
            let f = arena.frame();
            rows.admit(&f)
                .expect("lawful admit")
                .encode_row(0)
                .expect("lawful")
        };
        // Lawful bytes round-trip through the storage door.
        let reclaimed = TupleKey::from_stored(key.as_bytes().to_vec(), 2).expect("lawful");
        assert_eq!(reclaimed, key);
        // Wrong arity, garbage, truncation: typed refusals.
        assert!(TupleKey::from_stored(key.as_bytes().to_vec(), 3).is_err());
        assert!(TupleKey::from_stored(vec![0xEE, 0x00], 1).is_err());
        assert!(TupleKey::from_stored(key.as_bytes()[..key.len() - 1].to_vec(), 2).is_err());
    }

    /// Storage ingestion refuses stale/foreign containers with typed
    /// errors, never panics.
    #[test]
    fn push_encoded_refuses_stale_and_foreign_domains_typed() {
        let mut arena = Arena::new();
        let mut rows = Rows::new_in(Arity::ONE, &arena.frame());
        let a = stamp_of(&mut arena, Datum::Num(Num::int(9)));
        rows.push_row(&[a]).expect("lawful push");
        let key = {
            let f = arena.frame();
            rows.admit(&f)
                .expect("lawful admit")
                .encode_row(0)
                .expect("lawful")
        };
        // Stale: the container predates the seal.
        arena.seal().expect("lawful seal");
        assert!(matches!(
            rows.push_encoded(&key, &mut arena),
            Err(PushError::StaleDomain { .. })
        ));
        // Foreign: a container from another arena entirely.
        let other = Arena::new();
        let mut foreign_rows = Rows::new_in(Arity::ONE, &other.frame());
        assert!(matches!(
            foreign_rows.push_encoded(&key, &mut arena),
            Err(PushError::ForeignArena)
        ));
        let _ = other.epoch();
    }

    /// The exhaustion door: ids at/beyond the cap refuse at decode, so
    /// the allocator's ceiling cannot be bypassed by stored counter bytes.
    #[test]
    fn relation_id_cap_is_enforced_at_decode() {
        assert_eq!(
            RelationId::raw_decode(&7u64.to_be_bytes()),
            Ok(RelationId(7))
        );
        assert!(RelationId::raw_decode(&RelationId::CAP.to_be_bytes()).is_err());
        assert!(RelationId::raw_decode(&u64::MAX.to_be_bytes()).is_err());
        assert!(RelationId::raw_decode(&[0u8; 4]).is_err());
        // Every assignable prefix stays below the 0xFF bound byte.
        assert!(
            RelationId::new(RelationId::CAP - 1)
                .expect("last assignable")
                .raw_encode()[0]
                < 0xFF
        );
        // The constructor door itself refuses the cap.
        assert!(RelationId::new(RelationId::CAP).is_none());
        assert!(RelationId::new(u64::MAX).is_none());
        // Allocator step cannot skip the ceiling either.
        assert!(
            RelationId::new(RelationId::CAP - 1)
                .expect("last assignable")
                .next()
                .is_none()
        );
    }

    /// The scan-key sentinel law: lower <= every key of matching rows
    /// <= upper, for value bounds and both sentinels.
    #[test]
    fn scan_keys_bracket_matching_rows() {
        use super::super::ScanBound;
        let rel = RelationId::new(7).expect("below cap");
        let rows: Vec<Vec<DataValue>> = vec![
            vec![DataValue::from(0i64), DataValue::from("a")],
            vec![DataValue::from(0i64), DataValue::from("zz")],
            vec![DataValue::from(1i64), DataValue::from("a")],
        ];
        let keys: Vec<Vec<u8>> = rows
            .iter()
            .map(|r| r.encode_as_key(rel).as_bytes().to_vec())
            .collect();
        // Bounds [Value(0)]..[Value(0)]: exactly the first-column-0 rows.
        let lo = scan_key_lower(rel, &[], &[ScanBound::Value(DataValue::from(0i64))]);
        let hi = scan_key_upper(rel, &[], &[ScanBound::Value(DataValue::from(0i64))]);
        assert!(lo.as_slice() <= keys[0].as_slice() && keys[1].as_slice() <= hi.as_slice());
        assert!(keys[2].as_slice() > hi.as_slice());
        // Full range: Least..Greatest brackets everything in the relation.
        let lo = scan_key_lower(rel, &[], &[ScanBound::Least]);
        let hi = scan_key_upper(rel, &[], &[ScanBound::Greatest]);
        for k in &keys {
            assert!(lo.as_slice() <= k.as_slice() && k.as_slice() <= hi.as_slice());
        }
        // Next relation's keys fall outside.
        let foreign = rows[0].encode_as_key(RelationId::new(8).expect("below cap"));
        assert!(foreign.as_bytes() > hi.as_slice());
        // Projected == materialized.
        let row = vec![DataValue::from("x"), DataValue::from(0i64)];
        assert_eq!(
            scan_key_lower_projected(rel, &row, &[1], &[]),
            scan_key_lower(rel, &row[1..2], &[])
        );
    }

    /// The fixed slot widths the storage layout constants promise are
    /// exactly what the codec produces.
    #[test]
    fn validity_slot_width_is_pinned() {
        use super::super::kind::validity::{Validity, ValiditySlot, ValidityTs};
        let enc = super::super::canonical::encode_owned(&super::super::DataValue::Validity(
            ValiditySlot::Value(
                Validity::new(ValidityTs::from_raw(123), true).expect("non-reserved"),
            ),
        ));
        assert_eq!(enc.len(), StorageKey::VALIDITY_TAIL_LEN);
        let enc2 = super::super::canonical::encode_owned(&super::super::DataValue::Validity(
            ValiditySlot::Value(
                Validity::new(ValidityTs::from_raw(i64::MIN), false)
                    .expect("retract admits every tick"),
            ),
        ));
        assert_eq!(enc2.len(), StorageKey::VALIDITY_TAIL_LEN);
    }

    #[test]
    fn arity_is_enforced_at_the_write_door() {
        let mut arena = Arena::new();
        let sc = stamp_of(&mut arena, Datum::Null);
        let mut rows = Rows::new_in(Arity::try_new(2).expect("test arity 2"), &arena.frame());
        assert!(
            matches!(
                rows.push_row(&[sc]),
                Err(Denial::ArityMismatch {
                    expected: 2,
                    got: 1
                })
            ),
            "wrong-width push must refuse typed — never abort"
        );
    }
}
