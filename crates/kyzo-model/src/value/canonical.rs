/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The one order-preserving byte form: the interning key and the on-disk key. memcmp/fact-payload become thin views of this.
//!
//! ## Canonical format v1
//!
//! `encoding := tag byte (see [`super::tag`]) + payload`, where every
//! payload is self-terminating, so encodings concatenate into sequences
//! without length prefixes and lexicographic byte order equals semantic
//! order — the guardrail invariant, total over all kinds:
//!
//! - `Null`: empty payload.
//! - `Bool`: one byte, `0x00`/`0x01`.
//! - `Num`: the numeric key of [`super::number`] (its own spec).
//! - `Str`/`Bytes`: content with `0x00 → 0x00 0xFF` escaping, terminated
//!   by `0x00 0x00`. Escaped content sorts above the terminator, so a
//!   prefix string sorts before its extensions; embedded NULs order
//!   correctly; decode is unambiguous.
//! - `Uuid`: 16 raw bytes, fixed width.
//! - `List`: the elements' encodings concatenated, terminated by `0x01`.
//!   Every tag byte is ≥ 0x05, so the terminator sorts below any
//!   continuation: `[a] < [a, …]` exactly as the semantic law requires.
//! - `Set`: identity is the *set*, not the writing order — elements are
//!   encoded, sorted bytewise, deduplicated, concatenated, terminated by
//!   `0x01`. `{2,1}` and `{1,2,1}` are one value by construction.
//!
//! The wide faces (identity laws in [`super::wide`]):
//!
//! - `Regex`: one flags byte, then the pattern text escaped/terminated —
//!   textual identity under the pinned dialect.
//! - `Json`: the canonical JSON value bytes (its own marker grammar:
//!   null `0x05` < false `0x06` < true `0x07` < number `0x10` < string
//!   `0x18` < array `0x48` < object `0x4C`, objects as sorted unique
//!   key/value pairs), followed by a trailing FNV-1a 64 hash of exactly
//!   those bytes — after the self-terminating value, so it can never
//!   influence order; verified on decode.
//! - `Vector`: u32 big-endian dimension count, then each component as
//!   Num's float key (components normalized through Num's law). Identity
//!   is content-addressed ([`Vector::content_id`](crate::value::kind::vector::Vector::content_id)
//!   — FNV-1a of dimension + exact float bits), not row-positional;
//!   exact float remains the stored authority (T4 total order unchanged).
//! - `Validity`: the 8-byte DESCENDING timestamp key (the imported
//!   time-axis law: latest first), then the polarity byte (assert `0x00`
//!   before retract `0x01`).
//! - `Interval`: `0x01` for the one empty value, or `0x02` + lower end +
//!   upper end, each end `0x01` (unbounded) or `0x02` + the 8-byte
//!   ascending timestamp key. Closed normal form is enforced on decode.
//! - `Geometry`: eight big-endian bytes of the Hilbert curve key of the
//!   point's fixed-point `(lat, lon)` cells — memcmp of the payload
//!   equals Hilbert order; no float floor in the encoding path.
//!
//! ## `CanonicalBytes` is a witness, not a costume
//!
//! Its field is private and there is no `From`, no `from_bytes`, no
//! unchecked constructor: the only mint is [`encode`], so holding a
//! `CanonicalBytes` *is* proof the bytes are lawful. Field privacy is the
//! deliberate authority boundary here — mint and type share this one
//! file, which is exactly the enforcement a proof token buys when they
//! don't (`StampedCode`'s case). If the mint ever moves out of this
//! file, the token pattern becomes mandatory. Reading is free
//! (`as_bytes`), and its derived `Ord` is exactly the storage total
//! order. [`decode`] is total over arbitrary bytes: a typed error, never
//! a panic, never trust.

use super::kind::interval::{Hi, Interval, Lo};
use super::kind::json::{Json, JsonNum, JsonObj, fnv1a64};
use super::kind::regex::{RegexFlags, RegexSource};
use super::kind::validity::{Validity, ValiditySlot, ValidityTs};
use super::number::{Num, NumDecodeError};
use super::prefix::prefix4;
use super::tag::{STRUCT_SEQ_END, STRUCT_STRING, Tag};
use super::{DataValue, Geometry, UuidWrapper, Vector};

/// A lawful canonical encoding: mintable only by [`encode`]. Derived
/// `Ord`/`Eq` are the storage total order over values, byte for byte.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct CanonicalBytes(Vec<u8>);

const _: () = assert!(std::mem::size_of::<CanonicalBytes>() == std::mem::size_of::<Vec<u8>>());
const _: () = assert!(std::mem::align_of::<CanonicalBytes>() == std::mem::align_of::<Vec<u8>>());

impl CanonicalBytes {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The shared 4-byte prefix of this encoding (the cell/arena prefix
    /// doctrine applied to the canonical form).
    pub fn prefix4(&self) -> [u8; 4] {
        prefix4(&self.0)
    }
}

/// The logical value the canonical codec speaks, borrowed. The 16-byte
/// cell (task 3) realizes this physically; the codec is the authority on
/// bytes. Kinds whose identity laws are not yet ruled are absent here —
/// their tags are reserved, their payloads arrive with their laws.
#[derive(Clone, Copy, Debug)]
pub enum Datum<'a> {
    Null,
    Bool(bool),
    Num(Num),
    Str(&'a str),
    Bytes(&'a [u8]),
    Uuid([u8; 16]),
    List(&'a [Datum<'a>]),
    /// Writing order and duplicates are irrelevant: the encoder
    /// canonicalizes to the sorted, deduplicated element sequence.
    Set(&'a [Datum<'a>]),
    /// Regex STORAGE identity (see `wide::regex`): writer construction
    /// validates; decode preserves stored source without re-proof;
    /// executability is `CompiledRegexV1`'s claim, never this variant's.
    Regex(&'a RegexSource),
    Json(&'a Json),
    /// Proven vector (dimension already admitted); components pass through
    /// Num's float law at encode. Identity is content-addressed from those
    /// exact floats (`Vector::content_id`); bare `&[f64]` is not a mint.
    Vector(&'a Vector),
    Validity(Validity),
    Interval(Interval),
    Geometry(Geometry),
}

/// Encode a value into its canonical form: the one mint of
/// [`CanonicalBytes`].
pub fn encode(d: Datum<'_>) -> CanonicalBytes {
    let mut out = Vec::new();
    encode_into(&mut out, d);
    CanonicalBytes(out)
}

/// Encode an owned logical value: the production second arm of the one
/// codec authority (same grammar, same mints). Deliberately does NOT
/// trust `DataValue`'s `Ord` for set ordering — sets are sorted by their
/// ENCODED bytes here, so the codec stays the independent authority the
/// `Ord` mirror is law-locked against (no circularity).
pub fn encode_owned(v: &DataValue) -> CanonicalBytes {
    let mut out = Vec::new();
    encode_owned_into(&mut out, v);
    CanonicalBytes(out)
}

/// The length of the FIRST canonical encoding in `bytes`, without
/// materializing the value: a no-allocation grammar walk (the hot-path
/// arm of the one decoder — binary searches over encoded rows skip, not
/// decode). Differentially locked to [`decode_one`]'s consumed length.
pub fn skip_one(bytes: &[u8]) -> Result<usize, DecodeError> {
    skip_at(bytes, 0)
}

fn skip_at(bytes: &[u8], depth: usize) -> Result<usize, DecodeError> {
    if depth > MAX_DEPTH {
        return Err(DecodeError::TooDeep);
    }
    let Some(&tag_byte) = bytes.first() else {
        return Err(DecodeError::Truncated);
    };
    let Some(tag) = Tag::from_byte(tag_byte) else {
        return Err(DecodeError::BadTag(tag_byte));
    };
    let body = &bytes[1..];
    let need = |n: usize| -> Result<usize, DecodeError> {
        if body.len() < n {
            Err(DecodeError::Truncated)
        } else {
            Ok(1 + n)
        }
    };
    match tag {
        Tag::Null => Ok(1),
        Tag::Bool => need(1),
        Tag::Num => {
            let (_, used) = Num::decode_key(body).map_err(DecodeError::Num)?;
            Ok(1 + used)
        }
        Tag::Str | Tag::Bytes => Ok(1 + skip_terminated(body)?),
        Tag::Uuid => need(16),
        Tag::Regex => {
            let Some(&flag_byte) = body.first() else {
                return Err(DecodeError::Truncated);
            };
            if RegexFlags::from_bits(flag_byte).is_none() {
                return Err(DecodeError::BadRegexFlags);
            }
            Ok(2 + skip_terminated(&body[1..])?)
        }
        Tag::Json => {
            let jused = skip_json(body, 0)?;
            if body.len() < jused + 8 {
                return Err(DecodeError::Truncated);
            }
            Ok(1 + jused + 8)
        }
        Tag::Vector => {
            let Some(dim_bytes) = body.get(..4) else {
                return Err(DecodeError::Truncated);
            };
            let mut dim_arr = [0u8; 4];
            dim_arr.copy_from_slice(dim_bytes);
            let dim = match usize::try_from(u32::from_be_bytes(dim_arr)) { Ok(n) => n, Err(_) => 0 };
            let mut at = 4;
            for _ in 0..dim {
                let (_, used) = Num::decode_key(&body[at..]).map_err(DecodeError::Num)?;
                at += used;
            }
            Ok(1 + at)
        }
        Tag::List | Tag::Set => {
            let mut at = 0;
            loop {
                match body.get(at) {
                    None => return Err(DecodeError::Truncated),
                    Some(&STRUCT_SEQ_END) => return Ok(1 + at + 1),
                    Some(_) => at += skip_at(&body[at..], depth + 1)?,
                }
            }
        }
        Tag::Validity => need(9),
        Tag::Interval => match body.first() {
            None => Err(DecodeError::Truncated),
            Some(0x01) => Ok(2),
            Some(0x02) => {
                let mut at = 1;
                for _ in 0..2 {
                    match body.get(at) {
                        Some(0x01) => at += 1,
                        Some(0x02) => {
                            if body.len() < at + 9 {
                                return Err(DecodeError::Truncated);
                            }
                            at += 9;
                        }
                        _other => return Err(DecodeError::IntervalNotCanonical),
                    }
                }
                Ok(1 + at)
            }
            Some(_) => Err(DecodeError::IntervalNotCanonical),
        },
        Tag::Geometry => need(8),
    }
}

/// Skip a 0x00-escaped, terminator-closed byte string; returns the bytes
/// consumed including the terminator.
fn skip_terminated(body: &[u8]) -> Result<usize, DecodeError> {
    let mut at = 0;
    loop {
        match body.get(at) {
            None => return Err(DecodeError::Truncated),
            Some(0x00) => match body.get(at + 1) {
                // Terminator [0x00, 0x00]; escaped zero [0x00, 0xFF].
                Some(0x00) => return Ok(at + 2),
                Some(0xFF) => at += 2,
                _other => return Err(DecodeError::BadEscape),
            },
            Some(_) => at += 1,
        }
    }
}

/// Skip one JSON payload value (grammar walk; the trailing hash is the
/// enclosing skip's business, verification is decode's).
fn skip_json(body: &[u8], depth: usize) -> Result<usize, DecodeError> {
    if depth > MAX_DEPTH {
        return Err(DecodeError::TooDeep);
    }
    match body.first() {
        None => Err(DecodeError::Truncated),
        Some(&JNULL) | Some(&JFALSE) | Some(&JTRUE) => Ok(1),
        Some(&JNUM) => {
            let (_, used) = Num::decode_key(&body[1..]).map_err(DecodeError::Num)?;
            Ok(1 + used)
        }
        Some(&JSTR) => Ok(1 + skip_terminated(&body[1..])?),
        Some(&JARR) => {
            let mut at = 1;
            loop {
                match body.get(at) {
                    None => return Err(DecodeError::Truncated),
                    Some(&STRUCT_SEQ_END) => return Ok(at + 1),
                    Some(_) => at += skip_json(&body[at..], depth + 1)?,
                }
            }
        }
        Some(&JOBJ) => {
            let mut at = 1;
            loop {
                match body.get(at) {
                    None => return Err(DecodeError::Truncated),
                    Some(&STRUCT_SEQ_END) => return Ok(at + 1),
                    Some(_) => {
                        at += skip_terminated(&body[at..])?;
                        at += skip_json(&body[at..], depth + 1)?;
                    }
                }
            }
        }
        Some(&b) => Err(DecodeError::BadJsonMarker(b)),
    }
}

/// Append a value's canonical encoding to a raw byte buffer: the key
/// assembler's zero-claim door (the output is deliberately NOT a
/// `CanonicalBytes` witness — key splicing works in the claimed-bytes
/// domain of the storage tier).
pub fn append_canonical(out: &mut Vec<u8>, v: &DataValue) {
    encode_owned_into(out, v);
}

fn encode_owned_into(out: &mut Vec<u8>, v: &DataValue) {
    match v {
        DataValue::Null => out.push(Tag::Null.byte()),
        DataValue::Bool(b) => {
            out.push(Tag::Bool.byte());
            out.push(u8::from(*b));
        }
        DataValue::Num(n) => {
            out.push(Tag::Num.byte());
            n.encode_key(out);
        }
        DataValue::Str(s) => {
            out.push(Tag::Str.byte());
            encode_terminated(out, s.as_bytes());
        }
        DataValue::Bytes(b) => {
            out.push(Tag::Bytes.byte());
            encode_terminated(out, b);
        }
        DataValue::Uuid(u) => {
            out.push(Tag::Uuid.byte());
            out.extend_from_slice(u.as_uuid().as_bytes());
        }
        DataValue::Regex(r) => {
            out.push(Tag::Regex.byte());
            out.push(r.flags().bits());
            encode_terminated(out, r.pattern().as_bytes());
        }
        DataValue::Json(j) => {
            out.push(Tag::Json.byte());
            let start = out.len();
            encode_json(out, j);
            let h = fnv1a64(&out[start..]);
            out.extend_from_slice(&h.to_be_bytes());
        }
        DataValue::Vector(vec) => {
            out.push(Tag::Vector.byte());
            out.extend_from_slice(&vec.dimension().get().to_be_bytes());
            for c in vec.components() {
                Num::float(c.get()).encode_key(out);
            }
        }
        DataValue::List(items) => {
            out.push(Tag::List.byte());
            for item in items {
                encode_owned_into(out, item);
            }
            out.push(STRUCT_SEQ_END);
        }
        DataValue::Set(items) => {
            out.push(Tag::Set.byte());
            let mut encoded: Vec<Vec<u8>> = items
                .iter()
                .map(|item| {
                    let mut e = Vec::new();
                    encode_owned_into(&mut e, item);
                    e
                })
                .collect();
            encoded.sort();
            encoded.dedup();
            for e in encoded {
                out.extend_from_slice(&e);
            }
            out.push(STRUCT_SEQ_END);
        }
        DataValue::Validity(v) => {
            out.push(Tag::Validity.byte());
            out.extend_from_slice(&desc_ts_key(v.ts_micros()));
            out.push(if v.is_assert() { 0x00 } else { 0x01 });
        }
        DataValue::Interval(iv) => {
            out.push(Tag::Interval.byte());
            encode_interval_body(out, iv);
        }
        DataValue::Geometry(g) => {
            out.push(Tag::Geometry.byte());
            out.extend_from_slice(&g.curve_key_bytes());
        }
    }
}

/// The interval body grammar, shared by both encoder arms.
fn encode_interval_body(out: &mut Vec<u8>, iv: &Interval) {
    match iv.ends() {
        None => out.push(0x01),
        Some((lo, hi)) => {
            out.push(0x02);
            match lo {
                Lo::NegUnbounded => out.push(0x01),
                Lo::At(t) => {
                    out.push(0x02);
                    out.extend_from_slice(&asc_ts_key(t));
                }
            }
            match hi {
                Hi::PosUnbounded => out.push(0x01),
                Hi::At(t) => {
                    out.push(0x02);
                    out.extend_from_slice(&asc_ts_key(t));
                }
            }
        }
    }
}

fn encode_into(out: &mut Vec<u8>, d: Datum<'_>) {
    match d {
        Datum::Null => out.push(Tag::Null.byte()),
        Datum::Bool(b) => {
            out.push(Tag::Bool.byte());
            out.push(u8::from(b));
        }
        Datum::Num(n) => {
            out.push(Tag::Num.byte());
            n.encode_key(out);
        }
        Datum::Str(s) => {
            out.push(Tag::Str.byte());
            encode_terminated(out, s.as_bytes());
        }
        Datum::Bytes(b) => {
            out.push(Tag::Bytes.byte());
            encode_terminated(out, b);
        }
        Datum::Uuid(u) => {
            out.push(Tag::Uuid.byte());
            out.extend_from_slice(&u);
        }
        Datum::List(items) => {
            out.push(Tag::List.byte());
            for &item in items {
                encode_into(out, item);
            }
            out.push(STRUCT_SEQ_END);
        }
        Datum::Set(items) => {
            out.push(Tag::Set.byte());
            let mut encoded: Vec<Vec<u8>> = items
                .iter()
                .map(|&item| {
                    let mut e = Vec::new();
                    encode_into(&mut e, item);
                    e
                })
                .collect();
            encoded.sort();
            encoded.dedup();
            for e in encoded {
                out.extend_from_slice(&e);
            }
            out.push(STRUCT_SEQ_END);
        }
        Datum::Regex(lit) => {
            out.push(Tag::Regex.byte());
            out.push(lit.flags().bits());
            encode_terminated(out, lit.pattern().as_bytes());
        }
        Datum::Json(j) => {
            out.push(Tag::Json.byte());
            let start = out.len();
            encode_json(out, j);
            let h = fnv1a64(&out[start..]);
            out.extend_from_slice(&h.to_be_bytes());
        }
        Datum::Vector(vec) => {
            out.push(Tag::Vector.byte());
            out.extend_from_slice(&vec.dimension().get().to_be_bytes());
            for c in vec.components() {
                Num::float(c.get()).encode_key(out);
            }
        }
        Datum::Validity(v) => {
            out.push(Tag::Validity.byte());
            out.extend_from_slice(&desc_ts_key(v.ts_micros()));
            out.push(if v.is_assert() { 0x00 } else { 0x01 });
        }
        Datum::Interval(iv) => {
            out.push(Tag::Interval.byte());
            encode_interval_body(out, &iv);
        }
        Datum::Geometry(g) => {
            out.push(Tag::Geometry.byte());
            out.extend_from_slice(&g.curve_key_bytes());
        }
    }
}

/// JSON value grammar markers (inside the Json payload).
const JNULL: u8 = 0x05;
const JFALSE: u8 = 0x06;
const JTRUE: u8 = 0x07;
const JNUM: u8 = 0x10;
const JSTR: u8 = 0x18;
const JARR: u8 = 0x48;
const JOBJ: u8 = 0x4C;

fn encode_json(out: &mut Vec<u8>, j: &Json) {
    match j {
        Json::Null => out.push(JNULL),
        Json::Bool(false) => out.push(JFALSE),
        Json::Bool(true) => out.push(JTRUE),
        Json::Num(n) => {
            out.push(JNUM);
            n.num().encode_key(out);
        }
        Json::Str(s) => {
            out.push(JSTR);
            encode_terminated(out, s.as_bytes());
        }
        Json::Arr(items) => {
            out.push(JARR);
            for item in items {
                encode_json(out, item);
            }
            out.push(STRUCT_SEQ_END);
        }
        Json::Obj(obj) => {
            out.push(JOBJ);
            for (k, v) in obj.entries() {
                // Each entry MUST begin with a byte strictly greater than
                // the `STRUCT_SEQ_END` (0x01) terminator, or a present entry
                // could sort BELOW a shorter object's terminator and break
                // the byte-order == `DataValue::Ord` law — a NUL-leading key
                // encodes to a leading 0x00 < 0x01. Tagging the key `JSTR`
                // (a key IS a string; 0x18 > 0x01) makes the entry marker
                // outrank the terminator, exactly as array elements do.
                out.push(JSTR);
                encode_terminated(out, k.as_bytes());
                encode_json(out, v);
            }
            out.push(STRUCT_SEQ_END);
        }
    }
}

/// Ascending order-preserving i64 key (sign bit flipped, big-endian).
fn asc_ts_key(ts: i64) -> [u8; 8] {
    (ts ^ i64::MIN).cast_unsigned().to_be_bytes()
}

/// Descending order-preserving i64 key: the imported validity law (latest
/// instant sorts first).
fn desc_ts_key(ts: i64) -> [u8; 8] {
    let mut k = asc_ts_key(ts);
    for b in &mut k {
        *b = !*b;
    }
    k
}

fn ts_from_asc(k: [u8; 8]) -> i64 {
    u64::from_be_bytes(k).cast_signed() ^ i64::MIN
}

/// `0x00 → 0x00 0xFF` escaping with a `0x00 0x00` terminator: prefix-safe
/// and order-preserving (escaped content sorts above the terminator).
fn encode_terminated(out: &mut Vec<u8>, content: &[u8]) {
    for &b in content {
        if b == STRUCT_STRING {
            out.push(STRUCT_STRING);
            out.push(0xFF);
        } else {
            out.push(b);
        }
    }
    out.push(STRUCT_STRING);
    out.push(0x00);
}

/// Typed decode failures: total input handling, never a panic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecodeError {
    Truncated,
    BadTag(u8),
    BadBool,
    BadEscape,
    BadUtf8,
    Num(NumDecodeError),
    /// Set payload not in canonical (sorted, deduplicated) form.
    SetNotCanonical,
    TrailingBytes,
    TooDeep,
    BadRegexFlags,
    BadPolarity,
    /// A vector component that is not a float-representation Num key.
    VectorComponentNotFloat,
    /// Vector dimension count exceeds the wire `u32` limit.
    VectorDimensionOverflow,
    BadJsonMarker(u8),
    /// Object keys not strictly ascending and unique.
    JsonNotCanonical,
    /// The trailing hash does not match the canonical value bytes.
    JsonHashMismatch,
    /// JSON has no NaN or infinity.
    JsonNumberNotFinite,
    /// A relation id at or beyond the allocation ceiling
    /// (`RelationId::CAP`): not a lawful id, refused at decode so the
    /// allocator's exhaustion door cannot be bypassed by stored bytes.
    RelationIdOverCap,
    /// An interval form that the closed-normal-form law forbids.
    IntervalNotCanonical,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "canonical decode refused: {self:?}")
    }
}

impl std::error::Error for DecodeError {}

/// Nesting bound: decode of hostile input must refuse, not blow the
/// stack.
const MAX_DEPTH: usize = 128;

/// Decode a full canonical encoding. Total: arbitrary bytes yield a value
/// or a typed error; trailing bytes are an error; nesting is bounded.
pub fn decode(bytes: &[u8]) -> Result<DataValue, DecodeError> {
    let (value, used) = decode_at(bytes, 0)?;
    if used != bytes.len() {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(value)
}

/// Decode ONE canonical value from the front of `bytes`, returning it
/// and the bytes consumed (plane-internal: `StorageKey` splitting).
pub(super) fn decode_one(bytes: &[u8]) -> Result<(DataValue, usize), DecodeError> {
    decode_at(bytes, 0)
}

fn decode_at(bytes: &[u8], depth: usize) -> Result<(DataValue, usize), DecodeError> {
    if depth > MAX_DEPTH {
        return Err(DecodeError::TooDeep);
    }
    let &tag_byte = bytes.first().ok_or(DecodeError::Truncated)?;
    let tag = Tag::from_byte(tag_byte).ok_or(DecodeError::BadTag(tag_byte))?;
    let body = &bytes[1..];
    match tag {
        Tag::Null => Ok((DataValue::Null, 1)),
        Tag::Bool => match body.first() {
            Some(0) => Ok((DataValue::Bool(false), 2)),
            Some(1) => Ok((DataValue::Bool(true), 2)),
            Some(_) => Err(DecodeError::BadBool),
            None => Err(DecodeError::Truncated),
        },
        Tag::Num => {
            let (n, used) = Num::decode_key(body).map_err(DecodeError::Num)?;
            Ok((DataValue::Num(n), 1 + used))
        }
        Tag::Str => {
            let (content, used) = decode_terminated(body)?;
            let s = String::from_utf8(content).map_err(|_| DecodeError::BadUtf8)?;
            Ok((DataValue::Str(s), 1 + used))
        }
        Tag::Bytes => {
            let (content, used) = decode_terminated(body)?;
            Ok((DataValue::Bytes(content), 1 + used))
        }
        Tag::Uuid => {
            if body.len() < 16 {
                return Err(DecodeError::Truncated);
            }
            let mut u = [0u8; 16];
            u.copy_from_slice(&body[..16]);
            Ok((
                DataValue::Uuid(UuidWrapper::new(uuid::Uuid::from_bytes(u))),
                17,
            ))
        }
        Tag::List => {
            let (items, used) = decode_sequence(body, depth)?;
            Ok((DataValue::List(items), 1 + used))
        }
        Tag::Set => {
            // Single-pass: each item is decoded exactly once, and the
            // canonical (sorted, deduplicated) form is checked against the
            // SAME decode's consumed length — never a second full decode of
            // the item. A prior two-pass shape (decode_sequence, then
            // re-decode every item to learn its length for the ordering
            // check) made a nested Set's cost double per level of nesting:
            // O(2^depth) on adversarial input, the hang the fuzzer found.
            let mut items = Vec::new();
            let mut prev: Option<&[u8]> = None;
            let mut used = 0usize;
            loop {
                match body.get(used) {
                    None => return Err(DecodeError::Truncated),
                    Some(&STRUCT_SEQ_END) => {
                        return Ok((DataValue::Set(items.into_iter().collect()), 1 + used + 1));
                    }
                    Some(_) => {
                        let (item, ilen) = decode_at(&body[used..], depth + 1)?;
                        let this = &body[used..used + ilen];
                        if let Some(p) = prev
                            && p >= this
                        {
                            return Err(DecodeError::SetNotCanonical);
                        }
                        prev = Some(this);
                        items.push(item);
                        used += ilen;
                    }
                }
            }
        }
        Tag::Regex => {
            let &flag_byte = body.first().ok_or(DecodeError::Truncated)?;
            let flags = RegexFlags::from_bits(flag_byte).ok_or(DecodeError::BadRegexFlags)?;
            let (content, used) = decode_terminated(&body[1..])?;
            let pattern = String::from_utf8(content).map_err(|_| DecodeError::BadUtf8)?;
            // Stored patterns were validated at write; decode stays total
            // over stored history (see wide::regex validity law).
            Ok((
                DataValue::Regex(RegexSource::from_stored(flags, pattern)),
                2 + used,
            ))
        }
        Tag::Json => {
            let (j, jused) = decode_json(body, depth)?;
            let hash_bytes = body.get(jused..jused + 8).ok_or(DecodeError::Truncated)?;
            let expect = fnv1a64(&body[..jused]);
            if hash_bytes != expect.to_be_bytes() {
                return Err(DecodeError::JsonHashMismatch);
            }
            Ok((DataValue::Json(j), 1 + jused + 8))
        }
        Tag::Vector => {
            let count_bytes = body.get(..4).ok_or(DecodeError::Truncated)?;
            let mut count_arr = [0u8; 4];
            count_arr.copy_from_slice(count_bytes);
            let count = match usize::try_from(u32::from_be_bytes(count_arr)) { Ok(n) => n, Err(_) => 0 };
            // Hostile length prefix: each float Num key is ≥1 byte, so a
            // claimed count larger than the remaining body cannot be
            // lawful. Refuse before `Vec::with_capacity` — otherwise a
            // 5-byte input can ask for a multi-gigabyte allocation
            // (fuzz-smoke fact_payload_decode OOM on tag+u32).
            let rest = body.len().saturating_sub(4);
            if count > rest {
                return Err(DecodeError::Truncated);
            }
            let mut floats = Vec::with_capacity(count);
            let mut used = 4usize;
            for _ in 0..count {
                let (n, nused) = Num::decode_key(&body[used..]).map_err(DecodeError::Num)?;
                let Some(f) = n.as_float() else {
                    return Err(DecodeError::VectorComponentNotFloat);
                };
                floats.push(f);
                used += nused;
            }
            let vector = Vector::try_new(floats).ok_or(DecodeError::VectorDimensionOverflow)?;
            Ok((DataValue::Vector(vector), 1 + used))
        }
        Tag::Validity => {
            let ts_bytes = body.get(..8).ok_or(DecodeError::Truncated)?;
            let mut asc = [0u8; 8];
            asc.copy_from_slice(ts_bytes);
            for b in &mut asc {
                *b = !*b;
            }
            let ts = ts_from_asc(asc);
            let is_assert = match body.get(8) {
                Some(0x00) => true,
                Some(0x01) => false,
                Some(_) => return Err(DecodeError::BadPolarity),
                None => return Err(DecodeError::Truncated),
            };
            Ok((
                DataValue::Validity(ValiditySlot::from_stored(
                    ValidityTs::from_raw(ts),
                    is_assert,
                )),
                10,
            ))
        }
        Tag::Interval => match body.first() {
            Some(0x01) => Ok((DataValue::Interval(Interval::EMPTY), 2)),
            Some(0x02) => {
                let mut used = 1usize;
                let (lo, lo_used) = decode_interval_end(&body[used..], true)?;
                used += lo_used;
                let (hi, hi_used) = decode_interval_end(&body[used..], false)?;
                used += hi_used;
                let (lo, hi) = match (lo, hi) {
                    (End::Unbounded, End::At(h)) => (Lo::NegUnbounded, Hi::At(h)),
                    (End::Unbounded, End::Unbounded) => (Lo::NegUnbounded, Hi::PosUnbounded),
                    (End::At(l), End::Unbounded) => (Lo::At(l), Hi::PosUnbounded),
                    (End::At(l), End::At(h)) => (Lo::At(l), Hi::At(h)),
                };
                let iv = Interval::range(lo, hi);
                // The closed-normal-form law: a written Range must BE a
                // lawful range; a denotes-empty pair is not canonical.
                if iv.is_empty() {
                    return Err(DecodeError::IntervalNotCanonical);
                }
                Ok((DataValue::Interval(iv), 1 + used))
            }
            Some(_) => Err(DecodeError::IntervalNotCanonical),
            None => Err(DecodeError::Truncated),
        },
        Tag::Geometry => {
            if body.len() < 8 {
                return Err(DecodeError::Truncated);
            }
            let mut key = [0u8; 8];
            key.copy_from_slice(&body[..8]);
            let code = u64::from_be_bytes(key);
            Ok((DataValue::Geometry(Geometry::from_curve_key(code)), 9))
        }
    }
}

enum End {
    Unbounded,
    At(i64),
}

fn decode_interval_end(body: &[u8], _is_lo: bool) -> Result<(End, usize), DecodeError> {
    match body.first() {
        Some(0x01) => Ok((End::Unbounded, 1)),
        Some(0x02) => {
            let ts_bytes = body.get(1..9).ok_or(DecodeError::Truncated)?;
            let mut arr = [0u8; 8];
            arr.copy_from_slice(ts_bytes);
            Ok((End::At(ts_from_asc(arr)), 9))
        }
        Some(_) => Err(DecodeError::IntervalNotCanonical),
        None => Err(DecodeError::Truncated),
    }
}

fn decode_json(body: &[u8], depth: usize) -> Result<(Json, usize), DecodeError> {
    if depth > MAX_DEPTH {
        return Err(DecodeError::TooDeep);
    }
    let &marker = body.first().ok_or(DecodeError::Truncated)?;
    let rest = &body[1..];
    match marker {
        JNULL => Ok((Json::Null, 1)),
        JFALSE => Ok((Json::Bool(false), 1)),
        JTRUE => Ok((Json::Bool(true), 1)),
        JNUM => {
            let (n, used) = Num::decode_key(rest).map_err(DecodeError::Num)?;
            let jn = JsonNum::new(n).map_err(|_| DecodeError::JsonNumberNotFinite)?;
            Ok((Json::Num(jn), 1 + used))
        }
        JSTR => {
            let (content, used) = decode_terminated(rest)?;
            let s = String::from_utf8(content).map_err(|_| DecodeError::BadUtf8)?;
            Ok((Json::Str(s), 1 + used))
        }
        JARR => {
            let mut items = Vec::new();
            let mut used = 0usize;
            loop {
                match rest.get(used) {
                    None => return Err(DecodeError::Truncated),
                    Some(&STRUCT_SEQ_END) => return Ok((Json::Arr(items), 1 + used + 1)),
                    Some(_) => {
                        let (item, ilen) = decode_json(&rest[used..], depth + 1)?;
                        items.push(item);
                        used += ilen;
                    }
                }
            }
        }
        JOBJ => {
            let mut entries: Vec<(String, Json)> = Vec::new();
            let mut prev_key: Option<Vec<u8>> = None;
            let mut used = 0usize;
            loop {
                match rest.get(used) {
                    None => return Err(DecodeError::Truncated),
                    Some(&STRUCT_SEQ_END) => {
                        let obj =
                            JsonObj::new(entries).map_err(|_| DecodeError::JsonNotCanonical)?;
                        return Ok((Json::Obj(obj), 1 + used + 1));
                    }
                    // Each entry is a `JSTR`-tagged key (the marker that
                    // keeps a present entry above the terminator) then its
                    // value.
                    Some(&JSTR) => {
                        let (key_bytes, klen) = decode_terminated(&rest[used + 1..])?;
                        if let Some(p) = &prev_key
                            && p.as_slice() >= key_bytes.as_slice()
                        {
                            return Err(DecodeError::JsonNotCanonical);
                        }
                        let key = String::from_utf8(key_bytes.clone())
                            .map_err(|_| DecodeError::BadUtf8)?;
                        prev_key = Some(key_bytes);
                        used += 1 + klen; // JSTR marker + terminated key
                        let (val, vlen) = decode_json(&rest[used..], depth + 1)?;
                        used += vlen;
                        entries.push((key, val));
                    }
                    Some(&other) => return Err(DecodeError::BadJsonMarker(other)),
                }
            }
        }
        other => Err(DecodeError::BadJsonMarker(other)),
    }
}

fn decode_sequence(body: &[u8], depth: usize) -> Result<(Vec<DataValue>, usize), DecodeError> {
    let mut items = Vec::new();
    let mut used = 0usize;
    loop {
        match body.get(used) {
            None => return Err(DecodeError::Truncated),
            Some(&STRUCT_SEQ_END) => return Ok((items, used + 1)),
            Some(_) => {
                let (item, ilen) = decode_at(&body[used..], depth + 1)?;
                items.push(item);
                used += ilen;
            }
        }
    }
}

pub(super) fn decode_terminated(body: &[u8]) -> Result<(Vec<u8>, usize), DecodeError> {
    let mut content = Vec::new();
    let mut i = 0usize;
    loop {
        match body.get(i) {
            None => return Err(DecodeError::Truncated),
            Some(&STRUCT_STRING) => match body.get(i + 1) {
                Some(0x00) => return Ok((content, i + 2)),
                Some(0xFF) => {
                    content.push(0x00);
                    i += 2;
                }
                Some(_) => return Err(DecodeError::BadEscape),
                None => return Err(DecodeError::Truncated),
            },
            Some(&b) => {
                content.push(b);
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::super::kind::interval::Bound;
    use super::*;
    use std::cmp::Ordering;

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
            match u64::try_from(n) {
                Ok(n_u) => match usize::try_from(self.next() % n_u) {
                    Ok(v) => v,
                    Err(_) => 0,
                },
                Err(_) => 0,
            }
        }
    }

    // ------------------------------------------------------------------
    // Independent semantic comparator over owned datums: tag order, then
    // per-kind law. Shares nothing with the encoder.
    // ------------------------------------------------------------------

    fn tag_of(d: &DataValue) -> Tag {
        match d {
            DataValue::Null => Tag::Null,
            DataValue::Bool(_) => Tag::Bool,
            DataValue::Num(_) => Tag::Num,
            DataValue::Str(_) => Tag::Str,
            DataValue::Bytes(_) => Tag::Bytes,
            DataValue::Uuid(_) => Tag::Uuid,
            DataValue::List(_) => Tag::List,
            DataValue::Set(_) => Tag::Set,
            DataValue::Regex(_) => Tag::Regex,
            DataValue::Json(_) => Tag::Json,
            DataValue::Vector(_) => Tag::Vector,
            DataValue::Validity(_) => Tag::Validity,
            DataValue::Interval(_) => Tag::Interval,
            DataValue::Geometry(_) => Tag::Geometry,
        }
    }

    fn semantic_cmp(a: &DataValue, b: &DataValue) -> Ordering {
        let t = tag_of(a).cmp(&tag_of(b));
        if t != Ordering::Equal {
            return t;
        }
        match (a, b) {
            (DataValue::Null, DataValue::Null) => Ordering::Equal,
            (DataValue::Bool(x), DataValue::Bool(y)) => x.cmp(y),
            (DataValue::Num(x), DataValue::Num(y)) => x.cmp(y),
            (DataValue::Str(x), DataValue::Str(y)) => cmp_terminated(x.as_bytes(), y.as_bytes()),
            (DataValue::Bytes(x), DataValue::Bytes(y)) => cmp_terminated(x, y),
            (DataValue::Uuid(x), DataValue::Uuid(y)) => x.cmp(y),
            (DataValue::List(x), DataValue::List(y)) => {
                for (i, j) in x.iter().zip(y.iter()) {
                    let c = semantic_cmp(i, j);
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                x.len().cmp(&y.len())
            }
            (DataValue::Set(x), DataValue::Set(y)) => {
                for (i, j) in x.iter().zip(y.iter()) {
                    let c = semantic_cmp(i, j);
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                x.len().cmp(&y.len())
            }
            (DataValue::Regex(a), DataValue::Regex(b)) => a
                .flags()
                .bits()
                .cmp(&b.flags().bits())
                .then(a.pattern().as_bytes().cmp(b.pattern().as_bytes())),
            (DataValue::Json(x), DataValue::Json(y)) => semantic_json_cmp(x, y),
            (DataValue::Vector(x), DataValue::Vector(y)) => {
                x.dimension().cmp(&y.dimension()).then_with(|| {
                    for (i, j) in x.components().zip(y.components()) {
                        let c = Num::float(i.get()).cmp(&Num::float(j.get()));
                        if c != Ordering::Equal {
                            return c;
                        }
                    }
                    Ordering::Equal
                })
            }
            (DataValue::Validity(x), DataValue::Validity(y)) => x.cmp_as_of_order(*y),
            (DataValue::Interval(x), DataValue::Interval(y)) => semantic_interval_cmp(x, y),
            (DataValue::Geometry(x), DataValue::Geometry(y)) => x.cmp(y),
            // T4 totalization: tags already compared equal above; every
            // DataValue kind is covered by the arms. Unreachable by the
            // sum type — kept as the independent mirror's totality seal.
            _other => unreachable!("tags equal"),
        }
    }

    /// Independent mirror of the JSON grammar's deterministic order.
    fn semantic_json_cmp(a: &Json, b: &Json) -> Ordering {
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
            (Json::Null, Json::Null)
            | (Json::Bool(false), Json::Bool(false))
            | (Json::Bool(true), Json::Bool(true)) => Ordering::Equal,
            (Json::Num(x), Json::Num(y)) => x.num().cmp(&y.num()),
            (Json::Str(x), Json::Str(y)) => x.as_bytes().cmp(y.as_bytes()),
            (Json::Arr(x), Json::Arr(y)) => {
                for (i, j) in x.iter().zip(y.iter()) {
                    let c = semantic_json_cmp(i, j);
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
                        .then_with(|| semantic_json_cmp(va, vb));
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                x.entries().len().cmp(&y.entries().len())
            }
            // T4 totalization: ranks already compared equal; every Json
            // variant is covered. Unreachable by the sum type — kept as
            // the independent mirror's totality seal.
            _other => unreachable!("ranks equal"),
        }
    }

    /// Independent mirror of the interval grammar's deterministic order.
    fn semantic_interval_cmp(a: &Interval, b: &Interval) -> Ordering {
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

    /// Semantic string/bytes order: plain lexicographic — a prefix sorts
    /// first. (Same as `[u8]::cmp`; named for clarity at call sites.)
    fn cmp_terminated(a: &[u8], b: &[u8]) -> Ordering {
        a.cmp(b)
    }

    // ------------------------------------------------------------------
    // Laws.
    // ------------------------------------------------------------------

    const U1: [u8; 16] = [0x11; 16];

    fn edge_datums() -> Vec<DataValue> {
        let nums = [
            Num::int(0),
            Num::float(0.0),
            Num::int(1),
            Num::float(1.0),
            Num::int(-1),
            Num::float(-1.5),
            Num::int(i64::MAX),
            Num::float(f64::INFINITY),
            Num::float(f64::NAN),
        ];
        let mut out = vec![
            DataValue::Null,
            DataValue::Bool(false),
            DataValue::Bool(true),
            DataValue::Str(String::new()),
            DataValue::Str("a".into()),
            DataValue::Str("ab".into()),
            DataValue::Str("a\u{0}b".into()),
            DataValue::Str("a\u{0}".into()),
            DataValue::Bytes(vec![]),
            DataValue::Bytes(vec![0x00]),
            DataValue::Bytes(vec![0x00, 0x00]),
            DataValue::Bytes(vec![0x00, 0xFF]),
            DataValue::Bytes(vec![0xFF]),
            DataValue::Uuid(UuidWrapper::new(uuid::Uuid::from_bytes(U1))),
            DataValue::List(vec![]),
        ];
        out.extend(nums.iter().map(|&n| DataValue::Num(n)));
        let a = DataValue::Str("a".into());
        let b = DataValue::Str("b".into());
        out.push(DataValue::List(vec![a.clone()]));
        out.push(DataValue::List(vec![a.clone(), b.clone()]));
        out.push(DataValue::List(vec![a.clone(), DataValue::List(vec![])]));
        out.push(DataValue::List(vec![DataValue::List(vec![a.clone()])]));
        out.push(DataValue::Set(BTreeSet::new()));
        out.push(DataValue::Set([a.clone(), b.clone()].into_iter().collect()));
        out.push(DataValue::Regex(
            RegexSource::validated(RegexFlags::NONE, "a+".into()).expect("valid"),
        ));
        out.push(DataValue::Regex(
            RegexSource::validated(RegexFlags::CASE_INSENSITIVE, "a+".into()).expect("valid"),
        ));
        out.push(DataValue::Json(Json::Null));
        out.push(DataValue::Json(Json::Obj(
            JsonObj::new(vec![
                (
                    "b".into(),
                    Json::Num(JsonNum::new(Num::int(2)).expect("finite")),
                ),
                ("a".into(), Json::Str("x\u{0}y".into())),
            ])
            .expect("lawful"),
        )));
        out.push(DataValue::Json(Json::Arr(vec![
            Json::Bool(true),
            Json::Null,
        ])));
        out.push(DataValue::Json(Json::Obj(
            JsonObj::new(vec![
                ("\u{0}k".into(), Json::Null),
                ("k".into(), Json::Bool(false)),
            ])
            .expect("lawful"),
        )));
        out.push(DataValue::Vector(Vector::try_new(vec![]).unwrap()));
        out.push(DataValue::Vector(Vector::try_new(vec![0.0]).unwrap()));
        out.push(DataValue::Vector(
            Vector::try_new(vec![-1.5, f64::NAN]).unwrap(),
        ));
        out.push(DataValue::Validity(
            Validity::new(ValidityTs::from_raw(0), true)
                .expect("non-reserved")
                .into(),
        ));
        out.push(DataValue::Validity(
            Validity::new(ValidityTs::from_raw(0), false)
                .expect("retract admits every tick")
                .into(),
        ));
        out.push(DataValue::Validity(ValiditySlot::from_stored(
            ValidityTs::from_raw(i64::MAX),
            true,
        )));
        out.push(DataValue::Interval(Interval::EMPTY));
        out.push(DataValue::Interval(Interval::new(
            Bound::Closed(0),
            Bound::Unbounded,
        )));
        out.push(DataValue::Interval(Interval::new(
            Bound::Closed(-5),
            Bound::Open(9),
        )));
        out.push(DataValue::Interval(Interval::new(
            Bound::Unbounded,
            Bound::Unbounded,
        )));
        out.push(DataValue::Geometry(Geometry::from_cells(0, 0)));
        out.push(DataValue::Geometry(Geometry::from_cells(1, 0)));
        out.push(DataValue::Geometry(Geometry::from_cells(0, 1)));
        out.push(DataValue::Geometry(Geometry::from_cells(
            u32::MAX,
            u32::MAX,
        )));
        out
    }

    /// THE one-law triple on the edge corpus: canonical byte order ==
    /// `DataValue::Ord` == independent semantic order. Every pair.
    #[test]
    fn law_order_embedding_edge_corpus() {
        let corpus = edge_datums();
        let encoded: Vec<CanonicalBytes> = corpus.iter().map(encode_owned).collect();
        for i in 0..corpus.len() {
            for j in 0..corpus.len() {
                let byte = encoded[i].as_bytes().cmp(encoded[j].as_bytes());
                let structural = corpus[i].cmp(&corpus[j]);
                let semantic = semantic_cmp(&corpus[i], &corpus[j]);
                assert_eq!(
                    byte, structural,
                    "byte order != Ord: {:?} vs {:?}",
                    corpus[i], corpus[j]
                );
                assert_eq!(
                    structural, semantic,
                    "Ord != semantic: {:?} vs {:?}",
                    corpus[i], corpus[j]
                );
            }
        }
    }

    /// skip_one is decode_one's length, everywhere (including nested
    /// containers), and refuses garbage decode refuses.
    #[test]
    fn law_skip_matches_decode_consumed_length() {
        let mut rng = Rng(0x5C1B);
        for _ in 0..400 {
            let d = random_datum(&mut rng, 0);
            let enc = encode_owned(&d);
            let (_, used) = decode_one(enc.as_bytes()).expect("lawful");
            match skip_one(enc.as_bytes()) {
                Ok(s) => assert_eq!(s, used, "skip diverged: {d:?}"),
                Err(e) => panic!(
                    "skip refused lawful {d:?}: {e:?} bytes={:02x?}",
                    enc.as_bytes()
                ),
            }
        }
        for d in edge_datums() {
            let enc = encode_owned(&d);
            let (_, used) = decode_one(enc.as_bytes()).expect("lawful");
            assert_eq!(
                skip_one(enc.as_bytes()).expect("lawful"),
                used,
                "skip diverged: {d:?}"
            );
        }
        assert!(skip_one(&[]).is_err());
        assert!(skip_one(&[0xEE]).is_err());
        assert!(skip_one(&[Tag::Str.byte(), 0x61]).is_err());
    }

    /// Round-trip totality over the edge corpus.
    #[test]
    fn law_round_trip_edge_corpus() {
        for d in edge_datums() {
            let enc = encode_owned(&d);
            let back = decode(enc.as_bytes()).expect("decode own encoding");
            assert_eq!(back, d, "round-trip changed {d:?}");
        }
    }

    /// The mandated grammar prefix cases, pinned explicitly.
    #[test]
    fn law_sequence_grammar_prefix_cases() {
        let a = Datum::Str("a");
        let b = Datum::Str("b");
        let empty = Datum::List(&[]);
        let l_a = [a];
        let l_ab = [a, b];
        let inner = [a, empty];
        let cases = [
            (encode(Datum::List(&[])), encode(Datum::List(&l_a))), // [] < [a]
            (encode(Datum::List(&l_a)), encode(Datum::List(&l_ab))), // [a] < [a,b]
            (encode(Datum::List(&l_a)), encode(Datum::List(&inner))), // [a] < [a,[]]
        ];
        for (lo, hi) in &cases {
            assert!(lo < hi, "prefix law broken: {lo:?} !< {hi:?}");
        }
        // Sets: writing order and duplicates are not identity.
        let s21 = [Datum::Num(Num::int(2)), Datum::Num(Num::int(1))];
        let s121 = [
            Datum::Num(Num::int(1)),
            Datum::Num(Num::int(2)),
            Datum::Num(Num::int(1)),
        ];
        assert_eq!(encode(Datum::Set(&s21)), encode(Datum::Set(&s121)));
        // Strings with embedded NULs order correctly around their
        // prefixes and neighbors.
        assert!(encode(Datum::Str("a")) < encode(Datum::Str("a\u{0}")));
        assert!(encode(Datum::Str("a\u{0}")) < encode(Datum::Str("a\u{1}")));
        assert!(encode(Datum::Str("a\u{0}")) < encode(Datum::Str("aa")));
        // Interval storage order, pinned so nobody "fixes" it later:
        // Empty first; an unbounded end marker (0x01) sorts before any
        // finite end (0x02 + key) — deterministic storage order, not
        // semantic interval order.
        let empty = encode(Datum::Interval(Interval::EMPTY));
        let unb_lo = encode(Datum::Interval(Interval::new(
            Bound::Unbounded,
            Bound::Closed(0),
        )));
        let fin_lo = encode(Datum::Interval(Interval::new(
            Bound::Closed(i64::MIN),
            Bound::Closed(0),
        )));
        let unb_hi = encode(Datum::Interval(Interval::new(
            Bound::Closed(0),
            Bound::Unbounded,
        )));
        let fin_hi = encode(Datum::Interval(Interval::new(
            Bound::Closed(0),
            Bound::Closed(i64::MAX),
        )));
        assert!(empty < unb_lo);
        assert!(
            unb_lo < fin_lo,
            "unbounded lower end sorts before any finite one"
        );
        assert!(
            unb_hi < fin_hi,
            "unbounded upper end sorts before any finite one"
        );
        // Vector storage order is dimension-first: fewer dimensions sort
        // before more, regardless of component values.
        assert!(
            encode(Datum::Vector(&Vector::try_new(vec![9e300]).unwrap()))
                < encode(Datum::Vector(&Vector::try_new(vec![0.0, 0.0]).unwrap()))
        );
    }

    /// Randomized one-law triple + round-trip: generated scalars and
    /// shallow sequences, adversarial alphabets (NUL-heavy strings),
    /// every kind including Interval.
    #[test]
    fn law_order_embedding_randomized() {
        let mut rng = Rng(0xC0FFEE);
        let mut corpus: Vec<DataValue> = Vec::new();
        for _ in 0..300 {
            corpus.push(random_datum(&mut rng, 0));
        }
        let encoded: Vec<CanonicalBytes> = corpus.iter().map(encode_owned).collect();
        for i in 0..corpus.len() {
            for j in 0..corpus.len() {
                let byte = encoded[i].as_bytes().cmp(encoded[j].as_bytes());
                let structural = corpus[i].cmp(&corpus[j]);
                let semantic = semantic_cmp(&corpus[i], &corpus[j]);
                assert_eq!(
                    byte, structural,
                    "random byte != Ord: {:?} vs {:?}",
                    corpus[i], corpus[j]
                );
                assert_eq!(
                    structural, semantic,
                    "random Ord != semantic: {:?} vs {:?}",
                    corpus[i], corpus[j]
                );
            }
        }
        for (d, enc) in corpus.iter().zip(encoded.iter()) {
            assert_eq!(&decode(enc.as_bytes()).expect("round-trip"), d);
        }
    }

    /// Totality of the storage order over every constructible kind:
    /// no NaN hole, no cross-kind ambiguity, no panic on compare,
    /// `PartialOrd` always `Some`. The tie-break authority #199 stands on.
    #[test]
    fn law_datavalue_order_is_total_no_holes() {
        // One representative per kind — tag-byte order IS cross-kind order.
        let kinds: [DataValue; 14] = [
            DataValue::Null,
            DataValue::Bool(false),
            DataValue::Num(Num::float(f64::NAN)), // NaN is a Float, greatest Num
            DataValue::Str(String::new()),
            DataValue::Bytes(vec![]),
            DataValue::Uuid(UuidWrapper::new(uuid::Uuid::from_bytes(U1))),
            DataValue::Regex(RegexSource::validated(RegexFlags::NONE, "a".into()).expect("valid")),
            DataValue::Json(Json::Null),
            DataValue::Vector(Vector::try_new(vec![f64::NAN]).unwrap()),
            DataValue::List(vec![]),
            DataValue::Set(BTreeSet::new()),
            DataValue::Validity(
                Validity::new(ValidityTs::from_raw(0), true)
                    .expect("non-reserved")
                    .into(),
            ),
            DataValue::Interval(Interval::EMPTY),
            DataValue::Geometry(Geometry::from_cells(0, 0)),
        ];
        // Cross-kind: every pair is comparable; PartialOrd never None;
        // cmp never panics; byte == Ord == semantic.
        for a in &kinds {
            for b in &kinds {
                let partial = a.partial_cmp(b);
                assert!(partial.is_some(), "PartialOrd hole: {:?} vs {:?}", a, b);
                let structural = a.cmp(b);
                assert_eq!(partial, Some(structural));
                let byte = encode_owned(a).as_bytes().cmp(encode_owned(b).as_bytes());
                let semantic = semantic_cmp(a, b);
                assert_eq!(byte, structural, "cross-kind byte != Ord: {a:?} vs {b:?}");
                assert_eq!(
                    structural, semantic,
                    "cross-kind Ord != semantic: {a:?} vs {b:?}"
                );
            }
        }
        // NaN hole closed inside Num: every NaN bit pattern is one Num,
        // equal to itself, greater than +∞, PartialOrd always Some.
        let nan_bits = [
            f64::NAN,
            f64::from_bits(0x7FF8_0000_0000_0000),
            f64::from_bits(0xFFF8_0000_0000_0001),
            f64::from_bits(0x7FF0_0000_0000_0001),
        ];
        let nans: Vec<DataValue> = nan_bits
            .iter()
            .map(|&f| DataValue::Num(Num::float(f)))
            .collect();
        let pos_inf = DataValue::Num(Num::float(f64::INFINITY));
        for n in &nans {
            assert_eq!(n.partial_cmp(n), Some(Ordering::Equal));
            assert_eq!(n.cmp(n), Ordering::Equal);
            assert_eq!(n.cmp(&pos_inf), Ordering::Greater);
            assert_eq!(
                encode_owned(n)
                    .as_bytes()
                    .cmp(encode_owned(&pos_inf).as_bytes()),
                Ordering::Greater
            );
            for m in &nans {
                assert_eq!(n.cmp(m), Ordering::Equal);
                assert_eq!(encode_owned(n), encode_owned(m));
            }
        }
        // Randomized stress: compare must not panic for any generated pair.
        let mut rng = Rng(0x70_7A_11);
        let corpus: Vec<DataValue> = (0..200).map(|_| random_datum(&mut rng, 0)).collect();
        for a in &corpus {
            for b in &corpus {
                match a.cmp(b) {
        ord => core::mem::drop(ord),
    }
                assert!(a.partial_cmp(b).is_some());
            }
        }
    }

    fn random_datum(rng: &mut Rng, depth: usize) -> DataValue {
        // Depth 0: all 14 kinds. Nested: scalars + List/Set only.
        let roll = rng.below(if depth == 0 { 14 } else { 7 });
        match roll {
            0 => DataValue::Null,
            1 => DataValue::Bool(rng.next().is_multiple_of(2)),
            2 => DataValue::Num(if rng.next().is_multiple_of(2) {
                Num::int(rng.next().cast_signed())
            } else {
                Num::float(f64::from_bits(rng.next()))
            }),
            3 => {
                let len = rng.below(6);
                // NUL-heavy alphabet: exactly the escape/terminator stress.
                let s: String = (0..len)
                    .map(|_| ['\u{0}', 'a', 'b', '\u{1}'][rng.below(4)])
                    .collect();
                DataValue::Str(s)
            }
            4 => {
                let len = rng.below(6);
                DataValue::Bytes((0..len).map(|_| [0x00, 0x61, 0xFF][rng.below(3)]).collect())
            }
            5 => {
                let mut u = [0u8; 16];
                u[0] = match u8::try_from(rng.next() & 0xFF) { Ok(b) => b, Err(_) => 0 };
                DataValue::Uuid(UuidWrapper::new(uuid::Uuid::from_bytes(u)))
            }
            6 => {
                let len = rng.below(4);
                DataValue::List((0..len).map(|_| random_datum(rng, depth + 1)).collect())
            }
            7 => {
                let len = rng.below(4);
                DataValue::Set((0..len).map(|_| random_datum(rng, depth + 1)).collect())
            }
            8 => {
                let flags = RegexFlags::from_bits(match u8::try_from(rng.next() % 0x40) {
                    Ok(b) => b,
                    Err(_) => 0,
                })
                .expect("masked");
                let pattern = ["", "a", "a\\+", "^x$"][rng.below(4)].to_string();
                DataValue::Regex(
                    RegexSource::validated(flags, pattern).expect("corpus patterns are valid"),
                )
            }
            9 => {
                let len = rng.below(3);
                DataValue::Vector(
                    Vector::try_new((0..len).map(|_| f64::from_bits(rng.next())).collect())
                        .unwrap(),
                )
            }
            10 => {
                let ts = ValidityTs::from_raw(rng.next().cast_signed());
                let is_assert = rng.next().is_multiple_of(2);
                DataValue::Validity(ValiditySlot::from_stored(ts, is_assert))
            }
            11 => DataValue::Interval(if rng.next().is_multiple_of(4) {
                Interval::EMPTY
            } else {
                let lo = rng.next().cast_signed() % 1000;
                let span = match i64::try_from(rng.next() % 50) { Ok(v) => v, Err(_) => 0 };
                Interval::new(Bound::Closed(lo), Bound::Closed(lo.saturating_add(span)))
            }),
            12 => DataValue::Geometry(Geometry::from_cells(
                match u32::try_from(rng.next() & 0xFFFF_FFFF) { Ok(v) => v, Err(_) => 0 },
                match u32::try_from(rng.next() & 0xFFFF_FFFF) { Ok(v) => v, Err(_) => 0 },
            )),
            _other => DataValue::Json(random_json(rng, 0)),
        }
    }

    fn random_json(rng: &mut Rng, depth: usize) -> Json {
        let roll = rng.below(if depth < 2 { 7 } else { 5 });
        match roll {
            0 => Json::Null,
            1 => Json::Bool(rng.next().is_multiple_of(2)),
            2 => {
                let n = if rng.next().is_multiple_of(2) {
                    Num::int(rng.next().cast_signed())
                } else {
                    // JSON numbers must be finite; map non-finite draws.
                    let f = f64::from_bits(rng.next());
                    Num::float(if f.is_finite() { f } else { 0.25 })
                };
                Json::Num(JsonNum::new(n).expect("finite by construction"))
            }
            3 => Json::Str(["", "k", "\u{0}v"][rng.below(3)].to_string()),
            4 => Json::Str("s".into()),
            5 => Json::Arr(
                (0..rng.below(3))
                    .map(|_| random_json(rng, depth + 1))
                    .collect(),
            ),
            _other => {
                let keys = ["a", "b", "cc"];
                let n = rng.below(3);
                let entries: Vec<(String, Json)> = (0..n)
                    .map(|i| (keys[i].to_string(), random_json(rng, depth + 1)))
                    .collect();
                Json::Obj(JsonObj::new(entries).expect("distinct keys"))
            }
        }
    }

    /// Format v1 golden vectors for composite encodings: permanent bytes
    /// (seat 59). A failure means the on-disk canonical form moved.
    #[test]
    fn format_v1_golden_vectors() {
        let hex = |cb: &CanonicalBytes| -> String {
            cb.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
        };
        assert_eq!(hex(&encode(Datum::Null)), "05");
        assert_eq!(hex(&encode(Datum::Bool(false))), "0800");
        assert_eq!(hex(&encode(Datum::Bool(true))), "0801");
        assert_eq!(hex(&encode(Datum::Num(Num::int(0)))), "100200");
        assert_eq!(hex(&encode(Datum::Str(""))), "180000");
        assert_eq!(hex(&encode(Datum::Str("a"))), "18610000");
        assert_eq!(hex(&encode(Datum::Str("a\u{0}b"))), "186100ff620000");
        assert_eq!(hex(&encode(Datum::Bytes(&[0x00]))), "2000ff0000");
        assert_eq!(hex(&encode(Datum::List(&[]))), "4801");
        let one = [Datum::Num(Num::int(1))];
        assert_eq!(
            hex(&encode(Datum::List(&one))),
            "48100304398000000000000000000001"
        );
        assert_eq!(hex(&encode(Datum::Set(&[]))), "5001");
        let re_a = RegexSource::validated(RegexFlags::NONE, "a".into()).expect("valid");
        assert_eq!(hex(&encode(Datum::Regex(&re_a))), "3000610000");
        assert_eq!(
            hex(&encode(Datum::Vector(&Vector::try_new(vec![1.0]).unwrap()))),
            "400000000103043980000000000000000001"
        );
        assert_eq!(
            hex(&encode(Datum::Validity(
                Validity::new(ValidityTs::from_raw(0), true).expect("non-reserved")
            ))),
            "587fffffffffffffff00"
        );
        assert_eq!(hex(&encode(Datum::Interval(Interval::EMPTY))), "6001");
        assert_eq!(
            hex(&encode(Datum::Interval(Interval::new(
                Bound::Closed(0),
                Bound::Unbounded
            )))),
            "600202800000000000000001"
        );
        // Hilbert @ 32-bit depth: origin 2×2 is {0,1,2,3} contiguous
        // (orientation reflected vs order-1 by odd empty high levels).
        assert_eq!(
            hex(&encode(Datum::Geometry(Geometry::from_cells(0, 0)))),
            "680000000000000000"
        );
        assert_eq!(
            hex(&encode(Datum::Geometry(Geometry::from_cells(1, 0)))),
            "680000000000000001"
        );
        assert_eq!(
            hex(&encode(Datum::Geometry(Geometry::from_cells(0, 1)))),
            "680000000000000003"
        );
        // Json: the value bytes are pinned exactly; the trailing hash is
        // verified against an INDEPENDENT in-test FNV implementation.
        let obj = Json::Obj(JsonObj::new(vec![("a".into(), Json::Null)]).expect("lawful"));
        let enc = encode(Datum::Json(&obj));
        let bytes = enc.as_bytes();
        let value_span = &bytes[1..bytes.len() - 8];
        let value_hex: String = value_span.iter().map(|b| format!("{b:02x}")).collect();
        // JOBJ(4c) JSTR(18) key "a"(61) str-term(00 00) JNULL(05) SEQ_END(01):
        // each entry is JSTR-tagged so its marker (0x18) outranks the 0x01
        // terminator, keeping byte order == DataValue::Ord for objects.
        assert_eq!(value_hex, "4c186100000501");
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in value_span {
            // INVARIANT(fnv1a): FNV-1a prime mix is defined as wrapping mul on u64.
            h = (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
        }
        assert_eq!(&bytes[bytes.len() - 8..], h.to_be_bytes());
    }

    /// REGRESSION (adversarial storage review): byte order == DataValue::Ord
    /// for JSON objects even when a key begins with U+0000. Before the
    /// JSTR-tagged entry marker, `{"\0": null}` encoded to a leading 0x00
    /// that sorted BELOW the empty object's 0x01 terminator — the two order
    /// authorities disagreed, silently mis-ordering stored JSON keys.
    #[test]
    fn json_object_byte_order_matches_structural_order_with_nul_key() {
        use crate::value::DataValue;
        let empty = DataValue::Json(crate::value::Json::Obj(
            JsonObj::new(vec![]).expect("lawful"),
        ));
        let nul = DataValue::Json(crate::value::Json::Obj(
            JsonObj::new(vec![("\u{0}".into(), Json::Null)]).expect("lawful"),
        ));
        // Structural order: fewer entries is less.
        assert_eq!(empty.cmp(&nul), std::cmp::Ordering::Less);
        // Byte order must AGREE (the guardrail law).
        let eb = encode_owned(&empty);
        let nb = encode_owned(&nul);
        assert_eq!(
            eb.as_bytes().cmp(nb.as_bytes()),
            std::cmp::Ordering::Less,
            "encode_owned byte order must match DataValue::Ord for objects with a NUL-leading key"
        );
        // And both still round-trip.
        assert_eq!(decode(eb.as_bytes()).unwrap(), empty);
        assert_eq!(decode(nb.as_bytes()).unwrap(), nul);
        // The LAW, over several NUL-key-bearing pairs: byte order AGREES
        // with structural order, whatever the direction. (JsonObj sorts
        // keys, so a "\0" key leads.)
        let mk = |kvs: Vec<(String, Json)>| {
            DataValue::Json(crate::value::Json::Obj(JsonObj::new(kvs).expect("lawful")))
        };
        let objs = [
            mk(vec![]),
            mk(vec![("\u{0}".into(), Json::Null)]),
            mk(vec![("a".into(), Json::Null)]),
            mk(vec![("\u{0}".into(), Json::Null), ("a".into(), Json::Null)]),
            mk(vec![("a".into(), Json::Bool(true))]),
        ];
        for a in &objs {
            for b in &objs {
                let structural = a.cmp(b);
                let byte = encode_owned(a).as_bytes().cmp(encode_owned(b).as_bytes());
                assert_eq!(
                    structural, byte,
                    "byte order must equal DataValue::Ord for {a:?} vs {b:?}"
                );
            }
        }
    }

    /// Decode totality: random bytes and truncations never panic; every
    /// valid encoding's strict prefixes are errors.
    #[test]
    fn decode_is_total() {
        let mut rng = Rng(0xDEAD);
        for _ in 0..20_000 {
            let len = rng.below(24);
            let bytes: Vec<u8> = (0..len)
                .map(|_| match u8::try_from(rng.next() & 0xFF) { Ok(b) => b, Err(_) => 0 })
                .collect();
            match decode(&bytes) {
        // must not panic
        Ok(v) => core::mem::drop(v),
        Err(e) => core::mem::drop(e),
    }
        }
        for d in edge_datums() {
            let enc = encode_owned(&d);
            for cut in 0..enc.len() {
                assert!(
                    decode(&enc.as_bytes()[..cut]).is_err(),
                    "truncation accepted for {d:?} at {cut}"
                );
            }
        }
        // Non-canonical set payloads are refused, not repaired.
        let unsorted: Vec<u8> = {
            let two = encode(Datum::Num(Num::int(2)));
            let one = encode(Datum::Num(Num::int(1)));
            let mut v = vec![Tag::Set.byte()];
            v.extend_from_slice(two.as_bytes());
            v.extend_from_slice(one.as_bytes());
            v.push(STRUCT_SEQ_END);
            v
        };
        assert_eq!(decode(&unsorted), Err(DecodeError::SetNotCanonical));
        // Hostile nesting refuses instead of overflowing.
        let mut deep = vec![Tag::List.byte(); 4000];
        deep.extend_from_slice(&[STRUCT_SEQ_END; 4000]);
        assert_eq!(decode(&deep), Err(DecodeError::TooDeep));
        // Non-canonical JSON: a corrupted hash and an unsorted object are
        // both refused, never repaired.
        let good = {
            let obj = Json::Obj(JsonObj::new(vec![("a".into(), Json::Null)]).expect("lawful"));
            encode(Datum::Json(&obj))
        };
        let mut bad_hash = good.as_bytes().to_vec();
        let last = bad_hash.len() - 1;
        bad_hash[last] ^= 0xFF;
        assert_eq!(decode(&bad_hash), Err(DecodeError::JsonHashMismatch));
        let mut unsorted_obj = vec![Tag::Json.byte(), 0x4C];
        // Each entry is JSTR(0x18)-tagged key, terminated, then value.
        unsorted_obj.extend_from_slice(&[0x18, 0x62, 0x00, 0x00, 0x05]); // "b": null
        unsorted_obj.extend_from_slice(&[0x18, 0x61, 0x00, 0x00, 0x05]); // "a": null
        unsorted_obj.push(STRUCT_SEQ_END);
        let span = unsorted_obj[1..].to_vec();
        let h = fnv1a64(&span);
        unsorted_obj.extend_from_slice(&h.to_be_bytes());
        assert_eq!(decode(&unsorted_obj), Err(DecodeError::JsonNotCanonical));
        // A denotes-empty interval written as a range is refused.
        let mut bad_iv = vec![Tag::Interval.byte(), 0x02, 0x02];
        bad_iv.extend_from_slice(&asc_ts_key(9));
        bad_iv.push(0x02);
        bad_iv.extend_from_slice(&asc_ts_key(1));
        assert_eq!(decode(&bad_iv), Err(DecodeError::IntervalNotCanonical));
    }

    /// Malformed wide-kind payloads refuse with their typed error, kind
    /// by kind.
    #[test]
    fn malformed_wide_payloads_refuse_by_kind() {
        // Regex: unknown flag bits; invalid UTF-8 pattern.
        assert_eq!(
            decode(&[Tag::Regex.byte(), 0x40, 0x00, 0x00]),
            Err(DecodeError::BadRegexFlags)
        );
        assert_eq!(
            decode(&[Tag::Regex.byte(), 0x00, 0xFF, 0x00, 0x00]),
            Err(DecodeError::BadUtf8)
        );
        // Vector: a non-float component (an Int Num key) refuses.
        let mut int_component = vec![Tag::Vector.byte()];
        int_component.extend_from_slice(&1u32.to_be_bytes());
        Num::int(5).encode_key(&mut int_component);
        assert_eq!(
            decode(&int_component),
            Err(DecodeError::VectorComponentNotFloat)
        );
        // Vector: count larger than the payload truncates.
        let mut short_vec = vec![Tag::Vector.byte()];
        short_vec.extend_from_slice(&3u32.to_be_bytes());
        Num::float(1.0).encode_key(&mut short_vec);
        assert!(decode(&short_vec).is_err());
        // REGRESSION (CI memcmp release OOM / fuzz fact_payload_decode):
        // a 5-byte Vector claiming u32::MAX components must refuse before
        // `Vec::with_capacity` — otherwise law3 byte-flip / fuzz allocate
        // tens of GB and abort the tripwire under --release.
        let hostile = {
            let mut v = vec![Tag::Vector.byte()];
            v.extend_from_slice(&u32::MAX.to_be_bytes());
            v
        };
        assert_eq!(decode(&hostile), Err(DecodeError::Truncated));
        // Validity: bad polarity byte.
        let mut bad_pol = vec![Tag::Validity.byte()];
        bad_pol.extend_from_slice(&[0x7F; 8]);
        bad_pol.push(0x02);
        assert_eq!(decode(&bad_pol), Err(DecodeError::BadPolarity));
        // Interval: bad form/end markers.
        assert_eq!(
            decode(&[Tag::Interval.byte(), 0x03]),
            Err(DecodeError::IntervalNotCanonical)
        );
        assert_eq!(
            decode(&[Tag::Interval.byte(), 0x02, 0x07]),
            Err(DecodeError::IntervalNotCanonical)
        );
        // Json: non-finite number in bytes refuses.
        let mut nan_json = vec![Tag::Json.byte()];
        let start = nan_json.len();
        nan_json.push(0x10); // JNUM
        Num::float(f64::NAN).encode_key(&mut nan_json);
        let h = fnv1a64(&nan_json[start..]);
        nan_json.extend_from_slice(&h.to_be_bytes());
        assert_eq!(decode(&nan_json), Err(DecodeError::JsonNumberNotFinite));
        // Json: duplicate keys (equal, escape-bearing) refuse.
        let mut dup = vec![Tag::Json.byte()];
        let start = dup.len();
        dup.push(0x4C); // JOBJ
        for _ in 0..2 {
            dup.push(0x18); // JSTR: the entry marker
            dup.extend_from_slice(&[0x00, 0xFF, 0x00, 0x00]); // key "\0"
            dup.push(0x05); // null
        }
        dup.push(STRUCT_SEQ_END);
        let h = fnv1a64(&dup[start..]);
        dup.extend_from_slice(&h.to_be_bytes());
        assert_eq!(decode(&dup), Err(DecodeError::JsonNotCanonical));
    }

    /// REGRESSION (fuzz timeout, `fuzz_targets/fact_payload_decode.rs`):
    /// libFuzzer hung ~1775s on one input. `Tag::Set`'s canonicality check
    /// used to re-decode every item a SECOND time — once via
    /// `decode_sequence` to build the value, once more (over the exact
    /// same bytes) to learn the item's encoded length for the
    /// sorted-order check — so a Set nested inside a Set inside a Set...
    /// doubled decode cost at every level of nesting: O(2^depth), not
    /// O(depth). At `NEST_DEPTH` below, the old two-pass shape would not
    /// finish before the heat death of the universe; this test completing
    /// at all is the proof the fix holds — no wall-clock bound needed.
    #[test]
    fn nested_set_decode_is_linear_not_exponential() {
        // Safely below MAX_DEPTH (128) so the well-formed control decodes
        // rather than hitting `TooDeep`; 2^NEST_DEPTH is astronomically
        // larger than anything the old two-pass decode could finish.
        const NEST_DEPTH: usize = 100;

        // Build Set(Set(Set(...Num(0)...))), NEST_DEPTH levels deep. Each
        // level holds exactly one element, so the encoding is lawfully
        // canonical (a single-element set can never violate sorted order).
        let mut value = DataValue::Num(Num::int(0));
        for _ in 0..NEST_DEPTH {
            let mut s = BTreeSet::new();
            s.insert(value);
            value = DataValue::Set(s);
        }
        let good = encode_owned(&value);

        // Well-formed control: decodes to the exact value.
        let decoded = decode(good.as_bytes()).expect("lawful nested set decodes");
        assert_eq!(decoded, value, "round-trip changed the nested value");

        // Hostile bytes: the SAME encoding with the outermost terminator
        // dropped. Discovering the truncation requires descending through
        // every nested level first (each inner level decodes clean) before
        // the outermost loop finds no closing byte left — exactly the
        // shape that made the old two-pass decode catastrophic — then
        // refusing, typed.
        let mut hostile = good.as_bytes().to_vec();
        hostile.pop();
        assert_eq!(decode(&hostile), Err(DecodeError::Truncated));
    }

    /// Deliberate construction of refusal variants that the random totality
    /// battery can miss (exact match, not merely `is_err`).
    #[test]
    fn decode_refusal_variants_deliberately_constructed() {
        // TrailingBytes: a complete Null plus one extra byte.
        assert_eq!(
            decode(&[Tag::Null.byte(), 0x00]),
            Err(DecodeError::TrailingBytes)
        );
        assert_eq!(decode(&[0xFF]), Err(DecodeError::BadTag(0xFF)));
        assert_eq!(decode(&[Tag::Bool.byte(), 0x02]), Err(DecodeError::BadBool));
        // Str body: `0x00 0x01` is not a lawful escape (only 0x00/0xFF after NUL).
        assert_eq!(
            decode(&[Tag::Str.byte(), 0x00, 0x01]),
            Err(DecodeError::BadEscape)
        );
        // Num payload that is Truncated wraps as DecodeError::Num.
        assert_eq!(
            decode(&[Tag::Num.byte()]),
            Err(DecodeError::Num(NumDecodeError::Truncated))
        );
    }
}
