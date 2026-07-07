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
//!   Num's float key (components normalized through Num's law).
//! - `Validity`: the 8-byte DESCENDING timestamp key (the imported
//!   time-axis law: latest first), then the polarity byte (assert `0x00`
//!   before retract `0x01`).
//! - `Interval`: `0x01` for the one empty value, or `0x02` + lower end +
//!   upper end, each end `0x01` (unbounded) or `0x02` + the 8-byte
//!   ascending timestamp key. Closed normal form is enforced on decode.
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

use super::number::{Num, NumDecodeError};
use super::prefix::prefix4;
use super::tag::{STRUCT_SEQ_END, STRUCT_STRING, Tag};
use super::wide::interval::{Hi, Interval, Lo};
use super::wide::json::{Json, JsonNum, JsonObj, fnv1a64};
use super::wide::regex::{RegexFlags, RegexSource};
use super::wide::validity::Validity;

/// A lawful canonical encoding: mintable only by [`encode`]. Derived
/// `Ord`/`Eq` are the storage total order over values, byte for byte.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CanonicalBytes(Vec<u8>);

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
    /// Textual identity under KyzoRegexV1 (see `wide::regex`); the
    /// source is writer-door validated; executability is CompiledRegexV1's claim.
    Regex(&'a RegexSource),
    Json(&'a Json),
    /// Components pass through Num's float law at encode.
    Vector(&'a [f64]),
    Validity(Validity),
    Interval(Interval),
}

/// Owned mirror of [`Datum`], what [`decode`] returns.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum OwnedDatum {
    Null,
    Bool(bool),
    Num(Num),
    Str(String),
    Bytes(Vec<u8>),
    Uuid([u8; 16]),
    List(Vec<OwnedDatum>),
    /// Always sorted and deduplicated (the canonical set form).
    Set(Vec<OwnedDatum>),
    Regex(RegexSource),
    Json(Json),
    /// Components held as [`Num`] (always the float representation),
    /// so identity is exact under Num's law (`Eq` incl. canonical NaN).
    Vector(Vec<Num>),
    Validity(Validity),
    Interval(Interval),
}

/// Encode a value into its canonical form: the one mint of
/// [`CanonicalBytes`].
pub fn encode(d: Datum<'_>) -> CanonicalBytes {
    let mut out = Vec::new();
    encode_into(&mut out, d);
    CanonicalBytes(out)
}

fn encode_into(out: &mut Vec<u8>, d: Datum<'_>) {
    match d {
        Datum::Null => out.push(Tag::Null.byte()),
        Datum::Bool(b) => {
            out.push(Tag::Bool.byte());
            out.push(b as u8);
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
        Datum::Vector(components) => {
            out.push(Tag::Vector.byte());
            assert!(
                components.len() <= u32::MAX as usize,
                "vector dimension exceeds u32"
            );
            out.extend_from_slice(&(components.len() as u32).to_be_bytes());
            for &c in components {
                Num::float(c).encode_key(out);
            }
        }
        Datum::Validity(v) => {
            out.push(Tag::Validity.byte());
            out.extend_from_slice(&desc_ts_key(v.ts_micros()));
            out.push(if v.is_assert() { 0x00 } else { 0x01 });
        }
        Datum::Interval(iv) => {
            out.push(Tag::Interval.byte());
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
                encode_terminated(out, k.as_bytes());
                encode_json(out, v);
            }
            out.push(STRUCT_SEQ_END);
        }
    }
}

/// Ascending order-preserving i64 key (sign bit flipped, big-endian).
fn asc_ts_key(ts: i64) -> [u8; 8] {
    ((ts ^ i64::MIN) as u64).to_be_bytes()
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
    (u64::from_be_bytes(k) as i64) ^ i64::MIN
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
    BadJsonMarker(u8),
    /// Object keys not strictly ascending and unique.
    JsonNotCanonical,
    /// The trailing hash does not match the canonical value bytes.
    JsonHashMismatch,
    /// JSON has no NaN or infinity.
    JsonNumberNotFinite,
    /// An interval form that the closed-normal-form law forbids.
    IntervalNotCanonical,
}

/// Nesting bound: decode of hostile input must refuse, not blow the
/// stack.
const MAX_DEPTH: usize = 128;

/// Decode a full canonical encoding. Total: arbitrary bytes yield a value
/// or a typed error; trailing bytes are an error; nesting is bounded.
pub fn decode(bytes: &[u8]) -> Result<OwnedDatum, DecodeError> {
    let (value, used) = decode_at(bytes, 0)?;
    if used != bytes.len() {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(value)
}

fn decode_at(bytes: &[u8], depth: usize) -> Result<(OwnedDatum, usize), DecodeError> {
    if depth > MAX_DEPTH {
        return Err(DecodeError::TooDeep);
    }
    let &tag_byte = bytes.first().ok_or(DecodeError::Truncated)?;
    let tag = Tag::from_byte(tag_byte).ok_or(DecodeError::BadTag(tag_byte))?;
    let body = &bytes[1..];
    match tag {
        Tag::Null => Ok((OwnedDatum::Null, 1)),
        Tag::Bool => match body.first() {
            Some(0) => Ok((OwnedDatum::Bool(false), 2)),
            Some(1) => Ok((OwnedDatum::Bool(true), 2)),
            Some(_) => Err(DecodeError::BadBool),
            None => Err(DecodeError::Truncated),
        },
        Tag::Num => {
            let (n, used) = Num::decode_key(body).map_err(DecodeError::Num)?;
            Ok((OwnedDatum::Num(n), 1 + used))
        }
        Tag::Str => {
            let (content, used) = decode_terminated(body)?;
            let s = String::from_utf8(content).map_err(|_| DecodeError::BadUtf8)?;
            Ok((OwnedDatum::Str(s), 1 + used))
        }
        Tag::Bytes => {
            let (content, used) = decode_terminated(body)?;
            Ok((OwnedDatum::Bytes(content), 1 + used))
        }
        Tag::Uuid => {
            if body.len() < 16 {
                return Err(DecodeError::Truncated);
            }
            let mut u = [0u8; 16];
            u.copy_from_slice(&body[..16]);
            Ok((OwnedDatum::Uuid(u), 17))
        }
        Tag::List => {
            let (items, used) = decode_sequence(body, depth)?;
            Ok((OwnedDatum::List(items), 1 + used))
        }
        Tag::Set => {
            let (items, used) = decode_sequence(body, depth)?;
            // The canonical set form is sorted and deduplicated; anything
            // else is not a lawful encoding.
            let mut prev: Option<&[u8]> = None;
            let mut cursor = body;
            for _ in 0..items.len() {
                let (_, elen) = decode_at(cursor, depth + 1)?;
                let this = &cursor[..elen];
                if let Some(p) = prev
                    && p >= this
                {
                    return Err(DecodeError::SetNotCanonical);
                }
                prev = Some(this);
                cursor = &cursor[elen..];
            }
            Ok((OwnedDatum::Set(items), 1 + used))
        }
        Tag::Regex => {
            let &flag_byte = body.first().ok_or(DecodeError::Truncated)?;
            let flags = RegexFlags::from_bits(flag_byte).ok_or(DecodeError::BadRegexFlags)?;
            let (content, used) = decode_terminated(&body[1..])?;
            let pattern = String::from_utf8(content).map_err(|_| DecodeError::BadUtf8)?;
            // Stored patterns were validated at write; decode stays total
            // over stored history (see wide::regex validity law).
            Ok((
                OwnedDatum::Regex(RegexSource::from_stored(flags, pattern)),
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
            Ok((OwnedDatum::Json(j), 1 + jused + 8))
        }
        Tag::Vector => {
            let count_bytes = body.get(..4).ok_or(DecodeError::Truncated)?;
            let count = u32::from_be_bytes(count_bytes.try_into().expect("4 bytes")) as usize;
            let mut components = Vec::new();
            let mut used = 4usize;
            for _ in 0..count {
                let (n, nused) = Num::decode_key(&body[used..]).map_err(DecodeError::Num)?;
                if n.as_float().is_none() {
                    return Err(DecodeError::VectorComponentNotFloat);
                }
                components.push(n);
                used += nused;
            }
            Ok((OwnedDatum::Vector(components), 1 + used))
        }
        Tag::Validity => {
            let ts_bytes = body.get(..8).ok_or(DecodeError::Truncated)?;
            let mut asc: [u8; 8] = ts_bytes.try_into().expect("8 bytes");
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
            Ok((OwnedDatum::Validity(Validity::new(ts, is_assert)), 10))
        }
        Tag::Interval => match body.first() {
            Some(0x01) => Ok((OwnedDatum::Interval(Interval::EMPTY), 2)),
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
                Ok((OwnedDatum::Interval(iv), 1 + used))
            }
            Some(_) => Err(DecodeError::IntervalNotCanonical),
            None => Err(DecodeError::Truncated),
        },
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
            Ok((
                End::At(ts_from_asc(ts_bytes.try_into().expect("8 bytes"))),
                9,
            ))
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
                    Some(_) => {
                        let (key_bytes, klen) = decode_terminated(&rest[used..])?;
                        if let Some(p) = &prev_key
                            && p.as_slice() >= key_bytes.as_slice()
                        {
                            return Err(DecodeError::JsonNotCanonical);
                        }
                        let key = String::from_utf8(key_bytes.clone())
                            .map_err(|_| DecodeError::BadUtf8)?;
                        prev_key = Some(key_bytes);
                        used += klen;
                        let (val, vlen) = decode_json(&rest[used..], depth + 1)?;
                        used += vlen;
                        entries.push((key, val));
                    }
                }
            }
        }
        other => Err(DecodeError::BadJsonMarker(other)),
    }
}

fn decode_sequence(body: &[u8], depth: usize) -> Result<(Vec<OwnedDatum>, usize), DecodeError> {
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
    use super::super::wide::interval::Bound;
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
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    // ------------------------------------------------------------------
    // Independent semantic comparator over owned datums: tag order, then
    // per-kind law. Shares nothing with the encoder.
    // ------------------------------------------------------------------

    fn tag_of(d: &OwnedDatum) -> Tag {
        match d {
            OwnedDatum::Null => Tag::Null,
            OwnedDatum::Bool(_) => Tag::Bool,
            OwnedDatum::Num(_) => Tag::Num,
            OwnedDatum::Str(_) => Tag::Str,
            OwnedDatum::Bytes(_) => Tag::Bytes,
            OwnedDatum::Uuid(_) => Tag::Uuid,
            OwnedDatum::List(_) => Tag::List,
            OwnedDatum::Set(_) => Tag::Set,
            OwnedDatum::Regex(_) => Tag::Regex,
            OwnedDatum::Json(_) => Tag::Json,
            OwnedDatum::Vector(_) => Tag::Vector,
            OwnedDatum::Validity(_) => Tag::Validity,
            OwnedDatum::Interval(_) => Tag::Interval,
        }
    }

    fn semantic_cmp(a: &OwnedDatum, b: &OwnedDatum) -> Ordering {
        let t = tag_of(a).cmp(&tag_of(b));
        if t != Ordering::Equal {
            return t;
        }
        match (a, b) {
            (OwnedDatum::Null, OwnedDatum::Null) => Ordering::Equal,
            (OwnedDatum::Bool(x), OwnedDatum::Bool(y)) => x.cmp(y),
            (OwnedDatum::Num(x), OwnedDatum::Num(y)) => x.cmp(y),
            (OwnedDatum::Str(x), OwnedDatum::Str(y)) => cmp_terminated(x.as_bytes(), y.as_bytes()),
            (OwnedDatum::Bytes(x), OwnedDatum::Bytes(y)) => cmp_terminated(x, y),
            (OwnedDatum::Uuid(x), OwnedDatum::Uuid(y)) => x.cmp(y),
            (OwnedDatum::List(x), OwnedDatum::List(y))
            | (OwnedDatum::Set(x), OwnedDatum::Set(y)) => {
                for (i, j) in x.iter().zip(y.iter()) {
                    let c = semantic_cmp(i, j);
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                x.len().cmp(&y.len())
            }
            (OwnedDatum::Regex(a), OwnedDatum::Regex(b)) => a
                .flags()
                .bits()
                .cmp(&b.flags().bits())
                .then(a.pattern().as_bytes().cmp(b.pattern().as_bytes())),
            (OwnedDatum::Json(x), OwnedDatum::Json(y)) => semantic_json_cmp(x, y),
            (OwnedDatum::Vector(x), OwnedDatum::Vector(y)) => {
                x.len().cmp(&y.len()).then_with(|| {
                    for (i, j) in x.iter().zip(y.iter()) {
                        let c = i.cmp(j);
                        if c != Ordering::Equal {
                            return c;
                        }
                    }
                    Ordering::Equal
                })
            }
            (OwnedDatum::Validity(x), OwnedDatum::Validity(y)) => x.cmp_as_of_order(*y),
            (OwnedDatum::Interval(x), OwnedDatum::Interval(y)) => semantic_interval_cmp(x, y),
            _ => unreachable!("tags equal"),
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
            _ => unreachable!("ranks equal"),
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

    fn edge_datums() -> Vec<OwnedDatum> {
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
            OwnedDatum::Null,
            OwnedDatum::Bool(false),
            OwnedDatum::Bool(true),
            OwnedDatum::Str(String::new()),
            OwnedDatum::Str("a".into()),
            OwnedDatum::Str("ab".into()),
            OwnedDatum::Str("a\u{0}b".into()),
            OwnedDatum::Str("a\u{0}".into()),
            OwnedDatum::Bytes(vec![]),
            OwnedDatum::Bytes(vec![0x00]),
            OwnedDatum::Bytes(vec![0x00, 0x00]),
            OwnedDatum::Bytes(vec![0x00, 0xFF]),
            OwnedDatum::Bytes(vec![0xFF]),
            OwnedDatum::Uuid(U1),
            OwnedDatum::List(vec![]),
        ];
        out.extend(nums.iter().map(|&n| OwnedDatum::Num(n)));
        let a = OwnedDatum::Str("a".into());
        let b = OwnedDatum::Str("b".into());
        out.push(OwnedDatum::List(vec![a.clone()]));
        out.push(OwnedDatum::List(vec![a.clone(), b.clone()]));
        out.push(OwnedDatum::List(vec![a.clone(), OwnedDatum::List(vec![])]));
        out.push(OwnedDatum::List(vec![OwnedDatum::List(vec![a.clone()])]));
        out.push(OwnedDatum::Set(vec![]));
        out.push(OwnedDatum::Set(vec![a.clone(), b.clone()]));
        out.push(OwnedDatum::Regex(
            RegexSource::validated(RegexFlags::NONE, "a+".into()).expect("valid"),
        ));
        out.push(OwnedDatum::Regex(
            RegexSource::validated(RegexFlags::CASE_INSENSITIVE, "a+".into()).expect("valid"),
        ));
        out.push(OwnedDatum::Json(Json::Null));
        out.push(OwnedDatum::Json(Json::Obj(
            JsonObj::new(vec![
                (
                    "b".into(),
                    Json::Num(JsonNum::new(Num::int(2)).expect("finite")),
                ),
                ("a".into(), Json::Str("x\u{0}y".into())),
            ])
            .expect("lawful"),
        )));
        out.push(OwnedDatum::Json(Json::Arr(vec![
            Json::Bool(true),
            Json::Null,
        ])));
        out.push(OwnedDatum::Json(Json::Obj(
            JsonObj::new(vec![
                ("\u{0}k".into(), Json::Null),
                ("k".into(), Json::Bool(false)),
            ])
            .expect("lawful"),
        )));
        out.push(OwnedDatum::Vector(vec![]));
        out.push(OwnedDatum::Vector(vec![Num::float(0.0)]));
        out.push(OwnedDatum::Vector(vec![
            Num::float(-1.5),
            Num::float(f64::NAN),
        ]));
        out.push(OwnedDatum::Validity(Validity::new(0, true)));
        out.push(OwnedDatum::Validity(Validity::new(0, false)));
        out.push(OwnedDatum::Validity(Validity::new(i64::MAX, true)));
        out.push(OwnedDatum::Interval(Interval::EMPTY));
        out.push(OwnedDatum::Interval(Interval::new(
            Bound::Closed(0),
            Bound::Unbounded,
        )));
        out.push(OwnedDatum::Interval(Interval::new(
            Bound::Closed(-5),
            Bound::Open(9),
        )));
        out.push(OwnedDatum::Interval(Interval::new(
            Bound::Unbounded,
            Bound::Unbounded,
        )));
        out
    }

    /// Owned-datum encoder mirroring the production one exactly, so the
    /// arbitrarily nested test corpus re-encodes without borrow gymnastics.
    fn encode_owned(d: &OwnedDatum) -> CanonicalBytes {
        let mut out = Vec::new();
        enc_owned_into(&mut out, d);
        CanonicalBytes(out)
    }

    fn enc_owned_into(out: &mut Vec<u8>, d: &OwnedDatum) {
        match d {
            OwnedDatum::Null => out.push(Tag::Null.byte()),
            OwnedDatum::Bool(b) => {
                out.push(Tag::Bool.byte());
                out.push(*b as u8);
            }
            OwnedDatum::Num(n) => {
                out.push(Tag::Num.byte());
                n.encode_key(out);
            }
            OwnedDatum::Str(s) => {
                out.push(Tag::Str.byte());
                encode_terminated(out, s.as_bytes());
            }
            OwnedDatum::Bytes(b) => {
                out.push(Tag::Bytes.byte());
                encode_terminated(out, b);
            }
            OwnedDatum::Uuid(u) => {
                out.push(Tag::Uuid.byte());
                out.extend_from_slice(u);
            }
            OwnedDatum::List(items) => {
                out.push(Tag::List.byte());
                for item in items {
                    enc_owned_into(out, item);
                }
                out.push(STRUCT_SEQ_END);
            }
            OwnedDatum::Set(items) => {
                out.push(Tag::Set.byte());
                let mut encoded: Vec<Vec<u8>> = items
                    .iter()
                    .map(|item| {
                        let mut e = Vec::new();
                        enc_owned_into(&mut e, item);
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
            OwnedDatum::Regex(lit) => encode_into(out, Datum::Regex(lit)),
            OwnedDatum::Json(j) => encode_into(out, Datum::Json(j)),
            OwnedDatum::Vector(components) => {
                out.push(Tag::Vector.byte());
                assert!(
                    components.len() <= u32::MAX as usize,
                    "vector dimension exceeds u32"
                );
                out.extend_from_slice(&(components.len() as u32).to_be_bytes());
                for n in components {
                    n.encode_key(out);
                }
            }
            OwnedDatum::Validity(v) => encode_into(out, Datum::Validity(*v)),
            OwnedDatum::Interval(iv) => encode_into(out, Datum::Interval(*iv)),
        }
    }

    /// Order embedding: canonical byte order == semantic order, every
    /// pair of the edge corpus.
    #[test]
    fn law_order_embedding_edge_corpus() {
        let corpus = edge_datums();
        let encoded: Vec<CanonicalBytes> = corpus.iter().map(encode_owned).collect();
        for i in 0..corpus.len() {
            for j in 0..corpus.len() {
                assert_eq!(
                    encoded[i].as_bytes().cmp(encoded[j].as_bytes()),
                    semantic_cmp(&corpus[i], &corpus[j]),
                    "order embedding broken: {:?} vs {:?}",
                    corpus[i],
                    corpus[j]
                );
            }
        }
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
        assert!(encode(Datum::Vector(&[9e300])) < encode(Datum::Vector(&[0.0, 0.0])));
    }

    /// Randomized order embedding + round-trip: generated scalars and
    /// shallow sequences, adversarial alphabets (NUL-heavy strings).
    #[test]
    fn law_order_embedding_randomized() {
        let mut rng = Rng(0xC0FFEE);
        let mut corpus: Vec<OwnedDatum> = Vec::new();
        for _ in 0..300 {
            corpus.push(random_datum(&mut rng, 0));
        }
        let encoded: Vec<CanonicalBytes> = corpus.iter().map(encode_owned).collect();
        for i in 0..corpus.len() {
            for j in 0..corpus.len() {
                assert_eq!(
                    encoded[i].as_bytes().cmp(encoded[j].as_bytes()),
                    semantic_cmp(&corpus[i], &corpus[j]),
                    "random order embedding broken: {:?} vs {:?}",
                    corpus[i],
                    corpus[j]
                );
            }
        }
        for (d, enc) in corpus.iter().zip(encoded.iter()) {
            assert_eq!(&decode(enc.as_bytes()).expect("round-trip"), d);
        }
    }

    fn random_datum(rng: &mut Rng, depth: usize) -> OwnedDatum {
        let roll = rng.below(if depth == 0 { 12 } else { 6 });
        match roll {
            0 => OwnedDatum::Null,
            1 => OwnedDatum::Bool(rng.next().is_multiple_of(2)),
            2 => OwnedDatum::Num(if rng.next().is_multiple_of(2) {
                Num::int(rng.next() as i64)
            } else {
                Num::float(f64::from_bits(rng.next()))
            }),
            3 => {
                let len = rng.below(6);
                // NUL-heavy alphabet: exactly the escape/terminator stress.
                let s: String = (0..len)
                    .map(|_| ['\u{0}', 'a', 'b', '\u{1}'][rng.below(4)])
                    .collect();
                OwnedDatum::Str(s)
            }
            4 => {
                let len = rng.below(6);
                OwnedDatum::Bytes((0..len).map(|_| [0x00, 0x61, 0xFF][rng.below(3)]).collect())
            }
            5 => {
                let mut u = [0u8; 16];
                u[0] = rng.next() as u8;
                OwnedDatum::Uuid(u)
            }
            6 => {
                let len = rng.below(4);
                OwnedDatum::List((0..len).map(|_| random_datum(rng, depth + 1)).collect())
            }
            7 => {
                let len = rng.below(4);
                let mut v: Vec<OwnedDatum> =
                    (0..len).map(|_| random_datum(rng, depth + 1)).collect();
                v.sort_by(semantic_cmp);
                v.dedup_by(|a, b| semantic_cmp(a, b) == Ordering::Equal);
                OwnedDatum::Set(v)
            }
            8 => {
                let flags = RegexFlags::from_bits((rng.next() % 0x40) as u8).expect("masked");
                let pattern = ["", "a", "a\\+", "^x$"][rng.below(4)].to_string();
                OwnedDatum::Regex(
                    RegexSource::validated(flags, pattern).expect("corpus patterns are valid"),
                )
            }
            9 => {
                let len = rng.below(3);
                OwnedDatum::Vector(
                    (0..len)
                        .map(|_| Num::float(f64::from_bits(rng.next())))
                        .collect(),
                )
            }
            10 => OwnedDatum::Validity(Validity::new(
                rng.next() as i64,
                rng.next().is_multiple_of(2),
            )),
            _ => OwnedDatum::Json(random_json(rng, 0)),
        }
    }

    fn random_json(rng: &mut Rng, depth: usize) -> Json {
        let roll = rng.below(if depth < 2 { 7 } else { 5 });
        match roll {
            0 => Json::Null,
            1 => Json::Bool(rng.next().is_multiple_of(2)),
            2 => {
                let n = if rng.next().is_multiple_of(2) {
                    Num::int(rng.next() as i64)
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
            _ => {
                let keys = ["a", "b", "cc"];
                let n = rng.below(3);
                let entries: Vec<(String, Json)> = (0..n)
                    .map(|i| (keys[i].to_string(), random_json(rng, depth + 1)))
                    .collect();
                Json::Obj(JsonObj::new(entries).expect("distinct keys"))
            }
        }
    }

    /// Format v1 golden vectors for composite encodings: permanent bytes.
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
            hex(&encode(Datum::Vector(&[1.0]))),
            "400000000103043980000000000000000001"
        );
        assert_eq!(
            hex(&encode(Datum::Validity(Validity::new(0, true)))),
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
        // Json: the value bytes are pinned exactly; the trailing hash is
        // verified against an INDEPENDENT in-test FNV implementation.
        let obj = Json::Obj(JsonObj::new(vec![("a".into(), Json::Null)]).expect("lawful"));
        let enc = encode(Datum::Json(&obj));
        let bytes = enc.as_bytes();
        let value_span = &bytes[1..bytes.len() - 8];
        let value_hex: String = value_span.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(value_hex, "4c6100000501");
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in value_span {
            h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
        assert_eq!(&bytes[bytes.len() - 8..], h.to_be_bytes());
    }

    /// Decode totality: random bytes and truncations never panic; every
    /// valid encoding's strict prefixes are errors.
    #[test]
    fn decode_is_total() {
        let mut rng = Rng(0xDEAD);
        for _ in 0..20_000 {
            let len = rng.below(24);
            let bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            let _ = decode(&bytes); // must not panic
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
        unsorted_obj.extend_from_slice(&[0x62, 0x00, 0x00, 0x05]); // "b": null
        unsorted_obj.extend_from_slice(&[0x61, 0x00, 0x00, 0x05]); // "a": null
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
            dup.extend_from_slice(&[0x00, 0xFF, 0x00, 0x00]); // key "\0"
            dup.push(0x05); // null
        }
        dup.push(STRUCT_SEQ_END);
        let h = fnv1a64(&dup[start..]);
        dup.extend_from_slice(&h.to_be_bytes());
        assert_eq!(decode(&dup), Err(DecodeError::JsonNotCanonical));
    }
}
