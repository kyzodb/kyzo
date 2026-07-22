/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The value plane: a value is a 16-byte tagged cell, either fully inline
//! or a dense `Code` into a shared, order-preserving interning arena —
//! and [`DataValue`] is its logical owned face, the engine's value
//! currency.
//!
//! ## `DataValue`: one owned logical value
//!
//! The 14 canonical kinds as an owned tree. Its trait surfaces are the
//! identity laws made ergonomic, each with a single authority:
//!
//! - `Eq`/`Hash` ARE the identity laws (Num's representation-faithful
//!   identity with one NaN and no `-0.0`; canonicalized sets; sorted
//!   unique JSON objects; normalized vector components).
//! - `Ord` is the STORAGE total order — the same order the canonical
//!   bytes embed — implemented as a fast structural mirror and
//!   **law-locked** to the codec by differential property tests (byte
//!   order of `encode_owned(v)` must equal `v.cmp(w)` on generated
//!   corpora, both directions, mutation-gated). One order, two proven
//!   implementations, machine-checked equal — never two truths.
//! - Expression-level comparability remains a separate, refusable
//!   authority in the query layer; `Ord` here is deterministic storage
//!   order, exactly what sorted containers and canonical output need.
//!
//! There is deliberately no bottom/sentinel variant: scan bounds are the
//! codec's bound vocabulary (the empty byte string sorts below every
//! encoding), never members of the value domain.
//!
//! ## Production host doors (P112)
//!
//! - [`canonical::encode_owned`] / [`decode`] — storage and expression currency.
//! - [`SearchHits::admit_decoded`] — engine→query search-hit admission, wired
//!   through [`crate::engines::admit_relation_search_hits`].
//! - [`string::MintedStr`] — compile-time absence proofs in [`proofs`]
//!   (every build); RA columnar lane lands in story #120.
//! - [`exec`] is `#[cfg(test)]` only (zero production references until #120).

use std::cmp::Ordering;
use std::collections::BTreeSet;

pub mod admission;
pub mod arena;
pub mod arity;
mod bytes_qty;
pub mod canonical;
pub mod cell;
pub mod code;
pub mod column;
#[cfg(test)]
pub mod exec;
pub mod json_convert;
pub mod kind;
pub mod number;
pub mod prefix;
pub mod proofs;
pub mod row;
pub mod search_hits;
pub mod string;
pub mod tag;
pub mod validity_coerce;

pub use admission::{Admission, Denial};
pub use arity::Arity;
pub use canonical::{DecodeError, append_canonical, decode, encode_owned};
pub use number::{Num, NumRepr, NumericOrd};
pub use row::{
    RelationId, StorageKey, TupleKey, TupleT, encode_key_with_suffix, scan_key_lower,
    scan_key_lower_projected, scan_key_upper, scan_key_upper_projected,
};
pub use search_hits::SearchHits;
pub use tag::Tag;
// Kind faces seated at model/value/kind (wide/ cut). Re-export until data/
// dissolves — no second copy under data/value.
pub use crate::value::kind::interval::{Bound, Interval};
pub use crate::value::kind::json::{Json, JsonNum, JsonObj, NonFiniteJsonNumber};
pub use crate::value::kind::regex::{CompiledRegexV1, RegexFlags, RegexSource};
pub use crate::value::kind::validity::{
    AsOf, MAX_VALIDITY_TS, StoredValiditySlot, TERMINAL_VALIDITY, Validity, ValiditySeekBound,
    ValiditySlot, ValidityTs,
};
pub use crate::value::kind::{CellCoord, Geometry, Vector, VectorComponent, VectorDimension};

/// The engine-facing UUID value (16 bytes; identity and order are the
/// bytes, per the uuid face's law). Field is private — construction and
/// reads go through the accessors so the wrapper stays the only door.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct UuidWrapper(uuid::Uuid);

const _: () = assert!(std::mem::size_of::<UuidWrapper>() == std::mem::size_of::<uuid::Uuid>());
const _: () = assert!(std::mem::align_of::<UuidWrapper>() == std::mem::align_of::<uuid::Uuid>());

impl UuidWrapper {
    pub fn new(uuid: uuid::Uuid) -> UuidWrapper {
        UuidWrapper(uuid)
    }

    pub fn get(self) -> uuid::Uuid {
        self.0
    }

    pub fn as_uuid(&self) -> &uuid::Uuid {
        &self.0
    }
}

/// The logical owned value: the engine's currency across parsing,
/// expression evaluation, and materialization. See the module docs for
/// its trait-law contract.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum DataValue {
    Null,
    Bool(bool),
    Num(Num),
    Str(String),
    Bytes(Vec<u8>),
    Uuid(UuidWrapper),
    Regex(RegexSource),
    Json(Json),
    Vector(Vector),
    List(Vec<DataValue>),
    /// Canonical set form by construction: `BTreeSet` under the storage
    /// order.
    Set(BTreeSet<DataValue>),
    Validity(ValiditySlot),
    Interval(Interval),
    Geometry(Geometry),
}

/// Exhaustive [`DataValue`] or-pattern for refuse / default arms under
/// `clippy::wildcard_enum_match_arm`. Prefer specific arms above; a newly
/// added variant omitted here makes those matches non-exhaustive.
#[macro_export]
macro_rules! data_value_any {
    () => {
        $crate::value::DataValue::Null
            | $crate::value::DataValue::Bool(_)
            | $crate::value::DataValue::Num(_)
            | $crate::value::DataValue::Str(_)
            | $crate::value::DataValue::Bytes(_)
            | $crate::value::DataValue::Uuid(_)
            | $crate::value::DataValue::Regex(_)
            | $crate::value::DataValue::Json(_)
            | $crate::value::DataValue::Vector(_)
            | $crate::value::DataValue::List(_)
            | $crate::value::DataValue::Set(_)
            | $crate::value::DataValue::Validity(_)
            | $crate::value::DataValue::Interval(_)
            | $crate::value::DataValue::Geometry(_)
    };
}

impl DataValue {
    /// The value's kind: the cross-type order authority.
    pub fn tag(&self) -> Tag {
        match self {
            DataValue::Null => Tag::Null,
            DataValue::Bool(_) => Tag::Bool,
            DataValue::Num(_) => Tag::Num,
            DataValue::Str(_) => Tag::Str,
            DataValue::Bytes(_) => Tag::Bytes,
            DataValue::Uuid(_) => Tag::Uuid,
            DataValue::Regex(_) => Tag::Regex,
            DataValue::Json(_) => Tag::Json,
            DataValue::Vector(_) => Tag::Vector,
            DataValue::List(_) => Tag::List,
            DataValue::Set(_) => Tag::Set,
            DataValue::Validity(_) => Tag::Validity,
            DataValue::Interval(_) => Tag::Interval,
            DataValue::Geometry(_) => Tag::Geometry,
        }
    }

    pub fn uuid(u: uuid::Uuid) -> DataValue {
        DataValue::Uuid(UuidWrapper::new(u))
    }

    /// The integer, for `Num` values holding the int representation.
    pub fn get_int(&self) -> Option<i64> {
        // Coercing numeric read: an integral in-range float yields its
        // int value (a `3.0` written into an `Int` column is `3`). Use
        // `Num::as_int` for the pure "is an Int representation" test.
        let DataValue::Num(n) = self else {
            return None;
        };
        n.to_int_coerced()
    }

    /// The numeric value as f64 (ints promote losslessly where they can;
    /// this is a NUMERIC read, not an identity claim).
    pub fn get_float(&self) -> Option<f64> {
        let DataValue::Num(n) = self else {
            return None;
        };
        Some(n.to_f64())
    }

    pub fn get_str(&self) -> Option<&str> {
        let DataValue::Str(s) = self else {
            return None;
        };
        Some(s)
    }

    pub fn get_bytes(&self) -> Option<&[u8]> {
        let DataValue::Bytes(b) = self else {
            return None;
        };
        Some(b)
    }

    pub fn get_bool(&self) -> Option<bool> {
        let DataValue::Bool(b) = self else {
            return None;
        };
        Some(*b)
    }

    pub fn get_slice(&self) -> Option<&[DataValue]> {
        let DataValue::List(l) = self else {
            return None;
        };
        Some(l)
    }

    pub fn get_interval(&self) -> Option<Interval> {
        let DataValue::Interval(iv) = self else {
            return None;
        };
        Some(*iv)
    }

    pub fn get_validity(&self) -> Option<Validity> {
        let DataValue::Validity(v) = self else {
            return None;
        };
        v.as_validity()
    }

    pub fn get_json(&self) -> Option<&Json> {
        let DataValue::Json(j) = self else {
            return None;
        };
        Some(j)
    }

    pub fn get_vector(&self) -> Option<&Vector> {
        let DataValue::Vector(v) = self else {
            return None;
        };
        Some(v)
    }

    pub fn get_geometry(&self) -> Option<Geometry> {
        let DataValue::Geometry(g) = self else {
            return None;
        };
        Some(*g)
    }
}

impl From<i64> for DataValue {
    fn from(v: i64) -> DataValue {
        DataValue::Num(Num::int(v))
    }
}

impl From<f64> for DataValue {
    fn from(v: f64) -> DataValue {
        DataValue::Num(Num::float(v))
    }
}

impl From<bool> for DataValue {
    fn from(v: bool) -> DataValue {
        DataValue::Bool(v)
    }
}

impl From<&str> for DataValue {
    fn from(v: &str) -> DataValue {
        DataValue::Str(v.to_string())
    }
}

impl From<String> for DataValue {
    fn from(v: String) -> DataValue {
        DataValue::Str(v)
    }
}

impl From<Num> for DataValue {
    fn from(v: Num) -> DataValue {
        DataValue::Num(v)
    }
}

impl From<Vec<DataValue>> for DataValue {
    fn from(v: Vec<DataValue>) -> DataValue {
        DataValue::List(v)
    }
}

impl PartialOrd for DataValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DataValue {
    /// The storage total order: a structural mirror of canonical byte
    /// order, law-locked to the codec by differential tests. Total over
    /// every constructible pair — no NaN hole, no cross-kind refuse, no
    /// panic (the T4 / #199 tie-break authority).
    fn cmp(&self, other: &Self) -> Ordering {
        let t = self.tag().cmp(&other.tag());
        if t != Ordering::Equal {
            return t;
        }
        match (self, other) {
            (DataValue::Null, DataValue::Null) => Ordering::Equal,
            (DataValue::Bool(a), DataValue::Bool(b)) => a.cmp(b),
            (DataValue::Num(a), DataValue::Num(b)) => a.cmp(b),
            (DataValue::Str(a), DataValue::Str(b)) => a.as_bytes().cmp(b.as_bytes()),
            (DataValue::Bytes(a), DataValue::Bytes(b)) => a.cmp(b),
            (DataValue::Uuid(a), DataValue::Uuid(b)) => a.cmp(b),
            (DataValue::Regex(a), DataValue::Regex(b)) => a
                .flags()
                .bits()
                .cmp(&b.flags().bits())
                .then_with(|| a.pattern().as_bytes().cmp(b.pattern().as_bytes())),
            (DataValue::Json(a), DataValue::Json(b)) => json_storage_cmp(a, b),
            (DataValue::Vector(a), DataValue::Vector(b)) => {
                a.dimension().cmp(&b.dimension()).then_with(|| {
                    for (x, y) in a.components().zip(b.components()) {
                        let c = Num::float(x.get()).cmp(&Num::float(y.get()));
                        if c != Ordering::Equal {
                            return c;
                        }
                    }
                    Ordering::Equal
                })
            }
            (DataValue::List(a), DataValue::List(b)) => {
                for (x, y) in a.iter().zip(b.iter()) {
                    let c = x.cmp(y);
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                a.len().cmp(&b.len())
            }
            (DataValue::Set(a), DataValue::Set(b)) => {
                for (x, y) in a.iter().zip(b.iter()) {
                    let c = x.cmp(y);
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                a.len().cmp(&b.len())
            }
            (DataValue::Validity(a), DataValue::Validity(b)) => a.cmp(b),
            (DataValue::Interval(a), DataValue::Interval(b)) => interval_storage_cmp(a, b),
            (DataValue::Geometry(a), DataValue::Geometry(b)) => a.cmp(b),
            // Tags already agreed; every same-kind arm is covered above.
            // Equal is the total-order refuse of panic — compare never panics
            // for any constructible DataValue pair (the T4 law).
            _other => Ordering::Equal,
        }
    }
}

/// The JSON storage order: the mirror of the JSON payload grammar.
fn json_storage_cmp(a: &Json, b: &Json) -> Ordering {
    fn rank(j: &Json) -> u8 {
        match j {
            Json::Null => 0x05,
            Json::Bool(false) => 0x06,
            Json::Bool(true) => 0x07,
            Json::Num(_) => 0x10,
            Json::Str(_) => 0x18,
            Json::Arr(_) => 0x48,
            Json::Obj(_) => 0x4C,
        }
    }
    let r = rank(a).cmp(&rank(b));
    if r != Ordering::Equal {
        return r;
    }
    match (a, b) {
        (Json::Num(x), Json::Num(y)) => x.num().cmp(&y.num()),
        (Json::Str(x), Json::Str(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Json::Arr(x), Json::Arr(y)) => {
            for (i, j) in x.iter().zip(y.iter()) {
                let c = json_storage_cmp(i, j);
                if c != Ordering::Equal {
                    return c;
                }
            }
            x.len().cmp(&y.len())
        }
        (Json::Obj(x), Json::Obj(y)) => {
            for ((ka, va), (kb, vb)) in x.entries().iter().zip(y.entries().iter()) {
                let c = ka
                    .as_bytes()
                    .cmp(kb.as_bytes())
                    .then_with(|| json_storage_cmp(va, vb));
                if c != Ordering::Equal {
                    return c;
                }
            }
            x.entries().len().cmp(&y.entries().len())
        }
        _other => Ordering::Equal,
    }
}

/// The interval storage order: the mirror of the interval grammar.
fn interval_storage_cmp(a: &Interval, b: &Interval) -> Ordering {
    use crate::value::kind::interval::{Hi, Lo};
    fn key(iv: &Interval) -> (u8, u8, i64, u8, i64) {
        match iv.ends() {
            None => (0x01, 0, 0, 0, 0),
            Some((lo, hi)) => {
                let (lm, lt) = match lo {
                    Lo::NegUnbounded => (0x01, 0),
                    Lo::At(t) => (0x02, t),
                };
                let (hm, ht) = match hi {
                    Hi::PosUnbounded => (0x01, 0),
                    Hi::At(t) => (0x02, t),
                };
                (0x02, lm, lt, hm, ht)
            }
        }
    }
    let (af, alm, alt, ahm, aht) = key(a);
    let (bf, blm, blt, bhm, bht) = key(b);
    af.cmp(&bf)
        .then(alm.cmp(&blm))
        .then(alt.cmp(&blt))
        .then(ahm.cmp(&bhm))
        .then(aht.cmp(&bht))
}

impl std::fmt::Display for DataValue {
    /// Deterministic display: the stable textual face (KyzoScript-ish
    /// literals). Part of query semantics; the oracle differentials at
    /// the gates judge any change.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DataValue::Null => write!(f, "null"),
            DataValue::Bool(b) => write!(f, "{b}"),
            DataValue::Num(n) => match n.repr() {
                NumRepr::Int(i) => write!(f, "{i}"),
                NumRepr::Float(x) => write!(f, "{x:?}"),
            },
            DataValue::Str(s) => write!(f, "{s:?}"),
            DataValue::Bytes(b) => {
                write!(f, "x'")?;
                for byte in b {
                    write!(f, "{byte:02x}")?;
                }
                write!(f, "'")
            }
            DataValue::Uuid(u) => write!(f, "uuid(\"{}\")", u.get()),
            DataValue::Regex(r) => write!(
                f,
                "regex({:?}, flags={:#04x})",
                r.pattern(),
                r.flags().bits()
            ),
            DataValue::Json(_) => write!(f, "json(…)"),
            DataValue::Vector(v) => {
                write!(f, "vec[")?;
                for (i, c) in v.components().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{:?}", c.get())?;
                }
                write!(f, "]")
            }
            DataValue::List(l) => {
                write!(f, "[")?;
                for (i, v) in l.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "]")
            }
            DataValue::Set(s) => {
                write!(f, "{{")?;
                for (i, v) in s.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "}}")
            }
            DataValue::Validity(v) => {
                write!(f, "validity({}, {})", v.ts_micros(), v.is_assert())
            }
            DataValue::Interval(_) => write!(f, "interval(…)"),
            DataValue::Geometry(g) => {
                write!(f, "geometry({}, {})", g.lat().get(), g.lon().get())
            }
        }
    }
}

/// The largest Unicode scalar value: the query layer's string
/// prefix-scan upper-bound character (canonical string order is raw byte
/// order, so `prefix + LARGEST_UTF_CHAR` bounds every extension of
/// `prefix` by one more character).
pub const LARGEST_UTF_CHAR: char = '\u{10FFFF}';

impl DataValue {
    /// Decode one canonical value from the front of a stored key,
    /// returning the remainder. Total: typed refusal, never trust.
    pub fn decode_from_key(bytes: &[u8]) -> Result<(DataValue, &[u8]), DecodeError> {
        let (v, used) = canonical::decode_one(bytes)?;
        Ok((v, &bytes[used..]))
    }
}

/// A logical row: an ordered sequence of [`DataValue`]s. Sealed row
/// identity — a bare `Vec<DataValue>` never coerces to a `Tuple` (no
/// `Deref`, no `From<Vec<DataValue>>`); every read or write goes through
/// an explicit named door below.
///
/// @authority Tuple
/// @layer value
/// @owns row identity as an ordered `DataValue` sequence, distinct from any bare `Vec<DataValue>`
/// @constructs Tuple::new | Tuple::with_capacity | Tuple::from_vec
/// @forbids a bare `Vec<DataValue>` standing in for row authority (no Deref/DerefMut/From)
/// @status established #300
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Tuple(Vec<DataValue>);

impl Tuple {
    /// An empty tuple.
    pub fn new() -> Tuple {
        Tuple(Vec::new())
    }

    /// An empty tuple with reserved capacity.
    pub fn with_capacity(cap: usize) -> Tuple {
        Tuple(Vec::with_capacity(cap))
    }

    /// The one door from a bare vector into row authority: explicit,
    /// never implicit.
    pub fn from_vec(values: Vec<DataValue>) -> Tuple {
        Tuple(values)
    }

    /// The bare vector back out, consuming the row.
    pub fn into_vec(self) -> Vec<DataValue> {
        self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_slice(&self) -> &[DataValue] {
        &self.0
    }

    pub fn push(&mut self, value: DataValue) {
        self.0.push(value);
    }

    pub fn extend(&mut self, values: impl IntoIterator<Item = DataValue>) {
        self.0.extend(values);
    }

    pub fn iter(&self) -> std::slice::Iter<'_, DataValue> {
        self.0.iter()
    }

    pub fn get(&self, i: usize) -> Option<&DataValue> {
        self.0.get(i)
    }

    pub fn get_mut(&mut self, i: usize) -> Option<&mut DataValue> {
        self.0.get_mut(i)
    }

    /// The first cell, if any — explicit door parallel to [`slice::first`].
    pub fn first(&self) -> Option<&DataValue> {
        self.0.first()
    }

    /// The last cell, if any — explicit door parallel to [`slice::last`].
    pub fn last(&self) -> Option<&DataValue> {
        self.0.last()
    }

    /// Clone the row's values out to a bare vector (borrowing; the row
    /// stays intact). Contrast [`Tuple::into_vec`], which consumes.
    pub fn to_vec(&self) -> Vec<DataValue> {
        self.0.clone()
    }

    pub fn pop(&mut self) -> Option<DataValue> {
        self.0.pop()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn truncate(&mut self, len: usize) {
        self.0.truncate(len);
    }

    pub fn reserve_exact(&mut self, additional: usize) {
        self.0.reserve_exact(additional);
    }

    pub fn drain<R: std::ops::RangeBounds<usize>>(
        &mut self,
        range: R,
    ) -> std::vec::Drain<'_, DataValue> {
        self.0.drain(range)
    }
}

impl std::ops::Index<usize> for Tuple {
    type Output = DataValue;

    fn index(&self, i: usize) -> &DataValue {
        &self.0[i]
    }
}

impl std::ops::IndexMut<usize> for Tuple {
    fn index_mut(&mut self, i: usize) -> &mut DataValue {
        &mut self.0[i]
    }
}

impl AsRef<[DataValue]> for Tuple {
    fn as_ref(&self) -> &[DataValue] {
        &self.0
    }
}

impl FromIterator<DataValue> for Tuple {
    fn from_iter<I: IntoIterator<Item = DataValue>>(iter: I) -> Tuple {
        Tuple(Vec::from_iter(iter))
    }
}

impl IntoIterator for Tuple {
    type Item = DataValue;
    type IntoIter = std::vec::IntoIter<DataValue>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a Tuple {
    type Item = &'a DataValue;
    type IntoIter = std::slice::Iter<'a, DataValue>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Decode every canonical value in `bytes` until exhaustion (stored keys
/// whose arity the caller does not know). Total.
pub fn decode_values_all(bytes: &[u8]) -> Result<Vec<DataValue>, DecodeError> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        let (v, used) = canonical::decode_one(&bytes[at..])?;
        out.push(v);
        at += used;
    }
    Ok(out)
}

/// Decode every value of a stored relation key (skipping the relation-id
/// prefix): key columns plus, for bitemporal keys, the two validity
/// slots. Total.
pub fn decode_tuple_from_key(key: &[u8], size_hint: usize) -> Result<Tuple, DecodeError> {
    if key.len() < StorageKey::RELATION_PREFIX_LEN {
        return Err(DecodeError::Truncated);
    }
    let mut out = Tuple::with_capacity(size_hint);
    let mut at = StorageKey::RELATION_PREFIX_LEN;
    while at < key.len() {
        let (v, used) = canonical::decode_one(&key[at..])?;
        out.push(v);
        at += used;
    }
    Ok(out)
}

/// A tuple's BARE encoding: canonical concatenation with no relation
/// prefix — the in-memory fixpoint stores' row form (byte order there IS
/// value order, by the Ord mirror law).
pub fn encode_tuple_bare(vals: &[DataValue]) -> Vec<u8> {
    let mut out = Vec::new();
    for v in vals {
        canonical::append_canonical(&mut out, v);
    }
    out
}

/// Decode a bare row back to values. Total.
pub fn decode_tuple_bare(bytes: &[u8]) -> Result<Tuple, DecodeError> {
    decode_values_all(bytes).map(Tuple::from_vec)
}

/// The byte length of the first `n_cols` encodings of a bare row (no
/// materialization — the binary-search comparator's walk). `None` if the
/// row has fewer columns.
pub fn bare_prefix_len(bytes: &[u8], n_cols: usize) -> Option<usize> {
    let mut at = 0usize;
    for _ in 0..n_cols {
        if at >= bytes.len() {
            return None;
        }
        at += match canonical::skip_one(&bytes[at..]) {
            Ok(n) => n,
            Err(_skip) => return None,
        };
    }
    Some(at)
}

/// A bare LOWER scan bound from column-wise [`ScanBound`]s.
pub fn bare_bounds_lower(bounds: &[ScanBound]) -> Vec<u8> {
    let mut out = Vec::new();
    for b in bounds {
        match b {
            ScanBound::Value(v) => canonical::append_canonical(&mut out, v),
            ScanBound::Least => break,
            ScanBound::Greatest => {
                out.push(0xFF);
                break;
            }
        }
    }
    out
}

/// A bare UPPER scan bound: like [`bare_bounds_lower`] (a `Greatest`
/// closes with the 0xFF byte); pair with an inclusive byte compare.
pub fn bare_bounds_upper(bounds: &[ScanBound]) -> Vec<u8> {
    bare_bounds_lower(bounds)
}

/// Append a stored VALUE payload's rows onto `tuple` (plain canonical
/// concatenation — the non-temporal keyspaces' value form).
pub fn extend_tuple_from_v(tuple: &mut Tuple, val: &[u8]) -> Result<(), DecodeError> {
    tuple.extend(decode_values_all(val)?);
    Ok(())
}

/// Decode a stored relation key's columns into a scratch tuple (the
/// batch reader's zero-fresh-allocation path).
pub fn decode_key_into(key: &[u8], out: &mut Tuple) -> Result<(), DecodeError> {
    if key.len() < StorageKey::RELATION_PREFIX_LEN {
        return Err(DecodeError::Truncated);
    }
    let mut at = StorageKey::RELATION_PREFIX_LEN;
    while at < key.len() {
        let (v, used) = canonical::decode_one(&key[at..])?;
        out.push(v);
        at += used;
    }
    Ok(())
}

/// Decode a stored key/value pair into one logical row: the key's
/// columns (relation prefix skipped), then the value payload's columns.
pub fn decode_tuple_from_kv(
    key: &[u8],
    val: &[u8],
    size_hint: Option<usize>,
) -> Result<Tuple, DecodeError> {
    let mut out = decode_tuple_from_key(
        key,
        match size_hint {
            Some(v) => v,
            None => 0,
        },
    )?;
    extend_tuple_from_v(&mut out, val)?;
    Ok(out)
}

/// A per-column scan bound: below every value, a value, or past every
/// value of every kind. THE bound vocabulary — bounds are the codec's
/// domain, never members of the value domain (there is no bottom/top
/// value). Its `Ord` extends the storage order by the two sentinels,
/// and its written form extends the canonical encoding the same way:
/// `Least` writes nothing (the empty prefix sorts below every encoding)
/// and `Greatest` writes the single byte `0xFF`, which no canonical
/// encoding begins.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ScanBound {
    Least,
    Value(DataValue),
    Greatest,
}

impl ScanBound {
    /// Append the bound's written form to a key buffer.
    #[cfg(test)]
    pub fn append_to_key(&self, out: &mut Vec<u8>) {
        match self {
            ScanBound::Least => {}
            ScanBound::Value(v) => canonical::append_canonical(out, v),
            ScanBound::Greatest => out.push(0xFF),
        }
    }
}

impl PartialOrd for ScanBound {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScanBound {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (ScanBound::Least, ScanBound::Least) => Ordering::Equal,
            (ScanBound::Least, _) => Ordering::Less,
            (_, ScanBound::Least) => Ordering::Greater,
            (ScanBound::Greatest, ScanBound::Greatest) => Ordering::Equal,
            (ScanBound::Greatest, _) => Ordering::Greater,
            (_, ScanBound::Greatest) => Ordering::Less,
            (ScanBound::Value(a), ScanBound::Value(b)) => a.cmp(b),
        }
    }
}

/// Decode a stored tuple: exactly `arity` canonical encodings, nothing
/// trailing. Total — the storage tier's typed read door.
#[cfg(test)]
pub fn decode_tuple(bytes: &[u8], arity: usize) -> Result<Vec<DataValue>, DecodeError> {
    let mut out = Vec::with_capacity(arity);
    let mut at = 0usize;
    for _ in 0..arity {
        let (v, used) = canonical::decode_one(&bytes[at..])?;
        out.push(v);
        at += used;
    }
    if at != bytes.len() {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(out)
}

#[cfg(test)]
mod facade_tests {
    use miette::{IntoDiagnostic, Result, miette};

    use super::canonical::CanonicalBytes;
    use super::*;

    /// Deterministic PRNG (xorshift64*): seeded, reproducible, no clock.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            // INVARIANT(xorshift_finalizer): xorshift* final mul is defined wrapping on u64.
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: usize) -> usize {
            let n_u = match u64::try_from(n) {
                Ok(v) => v,
                Err(_) => return 0,
            };
            match usize::try_from(self.next() % n_u) {
                Ok(v) => v,
                Err(_) => 0,
            }
        }
    }

    fn random_value(rng: &mut Rng, depth: usize) -> Result<DataValue> {
        let roll = rng.below(if depth == 0 { 14 } else { 7 });
        Ok(match roll {
            0 => DataValue::Null,
            1 => DataValue::Bool(rng.next().is_multiple_of(2)),
            2 => DataValue::Num(if rng.next().is_multiple_of(2) {
                Num::int(rng.next().cast_signed())
            } else {
                Num::float(f64::from_bits(rng.next()))
            }),
            3 => DataValue::Str(["", "a", "ab", "a\u{0}b"][rng.below(4)].to_string()),
            4 => DataValue::Bytes(vec![0x00, 0xFF][..rng.below(3).min(2)].to_vec()),
            5 => {
                let mut items = Vec::new();
                for _ in 0..rng.below(3) {
                    items.push(random_value(rng, depth + 1)?);
                }
                DataValue::List(items)
            }
            6 => {
                let mut items = Vec::new();
                for _ in 0..rng.below(3) {
                    items.push(random_value(rng, depth + 1)?);
                }
                DataValue::Set(items.into_iter().collect())
            }
            7 => DataValue::uuid(uuid::Uuid::from_bytes({
                let mut b = [0u8; 16];
                b[0] = match u8::try_from(rng.next() & 0xFF) {
                    Ok(b) => b,
                    Err(_) => 0,
                };
                b
            })),
            8 => DataValue::Regex(
                RegexSource::validated(RegexFlags::NONE, ["a", "b+", ""][rng.below(3)].into())
                    .into_diagnostic()?,
            ),
            9 => DataValue::Vector(
                Vector::try_new(
                    (0..rng.below(3))
                        .map(|_| f64::from_bits(rng.next()))
                        .collect(),
                )
                .ok_or_else(|| miette!("try_new"))?,
            ),
            10 => {
                let ts = ValidityTs::of_micros(rng.next().cast_signed());
                let is_assert = rng.next().is_multiple_of(2);
                DataValue::Validity(ValiditySlot::from_stored(ts, is_assert))
            }
            11 => DataValue::Interval(if rng.next().is_multiple_of(4) {
                Interval::EMPTY
            } else {
                Interval::new(
                    Bound::Closed(rng.next().cast_signed() % 1000),
                    Bound::Unbounded,
                )
            }),
            12 => DataValue::Geometry(Geometry::from_cells(
                match u32::try_from(rng.next() & 0xFFFF_FFFF) {
                    Ok(v) => v,
                    Err(_) => 0,
                },
                match u32::try_from(rng.next() & 0xFFFF_FFFF) {
                    Ok(v) => v,
                    Err(_) => 0,
                },
            )),
            _other => DataValue::Json(if rng.next().is_multiple_of(2) {
                Json::Null
            } else {
                Json::Str("k".into())
            }),
        })
    }

    /// THE Ord law: the structural mirror equals canonical byte order,
    /// every generated pair, both directions.
    #[test]
    fn law_storage_ord_equals_canonical_byte_order() -> Result<()> {
        let mut rng = Rng(0xFACADE);
        let mut corpus: Vec<DataValue> = Vec::with_capacity(250);
        for _ in 0..250 {
            corpus.push(random_value(&mut rng, 0)?);
        }
        let encoded: Vec<CanonicalBytes> = corpus.iter().map(encode_owned).collect();
        for i in 0..corpus.len() {
            for j in 0..corpus.len() {
                assert_eq!(
                    corpus[i].cmp(&corpus[j]),
                    encoded[i].as_bytes().cmp(encoded[j].as_bytes()),
                    "Ord mirror diverged from canonical order: {:?} vs {:?}",
                    corpus[i],
                    corpus[j]
                );
            }
        }
        Ok(())
    }

    /// Identity laws surface through Eq: -0.0 vectors, NaN components,
    /// set writing order.
    #[test]
    fn identity_laws_flow_through_the_facade() -> Result<()> {
        assert_eq!(
            DataValue::Vector(Vector::try_new(vec![0.0]).ok_or_else(|| miette!("try_new"))?),
            DataValue::Vector(Vector::try_new(vec![-0.0]).ok_or_else(|| miette!("try_new"))?)
        );
        assert_eq!(
            DataValue::Vector(Vector::try_new(vec![f64::NAN]).ok_or_else(|| miette!("try_new"))?),
            DataValue::Vector(
                Vector::try_new(vec![f64::from_bits(0xFFF8_0000_0000_0001)]).ok_or_else(|| miette!("try_new"))?
            )
        );
        let a: DataValue = DataValue::Set(
            [DataValue::from(2i64), DataValue::from(1i64)]
                .into_iter()
                .collect(),
        );
        let b: DataValue = DataValue::Set(
            [
                DataValue::from(1i64),
                DataValue::from(2i64),
                DataValue::from(1i64),
            ]
            .into_iter()
            .collect(),
        );
        assert_eq!(a, b);
        assert_eq!(encode_owned(&a), encode_owned(&b));
        // Num identity is representation-faithful through From.
        assert_ne!(DataValue::from(1i64), DataValue::from(1.0f64));
        assert!(DataValue::from(1i64) < DataValue::from(1.0f64));
        Ok(())
    }

    /// Round-trip through the codec: DataValue is the decode result.
    #[test]
    fn facade_round_trips_through_the_codec() -> Result<()> {
        let mut rng = Rng(0x5EED);
        for _ in 0..300 {
            let v = random_value(&mut rng, 0)?;
            let enc = encode_owned(&v);
            let back = decode(enc.as_bytes()).into_diagnostic()?;
            assert_eq!(back, v, "round-trip changed {v:?}");
        }
        Ok(())
    }

    /// ScanBound's Ord mirrors its written form's byte order — the two
    /// sentinels really do bracket every canonical encoding.
    #[test]
    fn law_scan_bound_order_equals_written_byte_order() -> Result<()> {
        let mut rng = Rng(0xB0BB1E5);
        let mut bounds: Vec<ScanBound> = Vec::with_capacity(80);
        for _ in 0..80 {
            bounds.push(ScanBound::Value(random_value(&mut rng, 0)?));
        }
        bounds.push(ScanBound::Least);
        bounds.push(ScanBound::Greatest);
        let keys: Vec<Vec<u8>> = bounds
            .iter()
            .map(|b| {
                let mut k = Vec::new();
                b.append_to_key(&mut k);
                k
            })
            .collect();
        for i in 0..bounds.len() {
            for j in 0..bounds.len() {
                assert_eq!(
                    bounds[i].cmp(&bounds[j]),
                    keys[i].cmp(&keys[j]),
                    "bound order diverged from key order: {:?} vs {:?}",
                    bounds[i],
                    bounds[j]
                );
            }
        }
        Ok(())
    }

    /// decode_tuple: exact arity, nothing trailing, total.
    #[test]
    fn decode_tuple_is_exact_and_total() -> Result<()> {
        let vals = [DataValue::from(7i64), DataValue::from("x"), DataValue::Null];
        let key = TupleKey::from_values(&vals);
        let back = decode_tuple(key.as_bytes(), 3).into_diagnostic()?;
        assert_eq!(back.as_slice(), &vals);
        assert!(decode_tuple(key.as_bytes(), 2).is_err());
        assert!(decode_tuple(key.as_bytes(), 4).is_err());
        assert!(decode_tuple(&[0xEE], 1).is_err());
        Ok(())
    }
}
