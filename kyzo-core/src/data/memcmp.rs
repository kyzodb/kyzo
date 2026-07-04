/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original (MPL-2.0):
 * type tags renumbered to agree with the semantic `DataValue` ordering, and all
 * decoding made fallible (corrupt data is an error, never a panic or UB).
 */

//! The memcomparable key encoding: `DataValue`s to bytes such that **bytewise
//! lexicographic order equals semantic value order**. This is the on-disk key
//! format and the load-bearing invariant of the whole system — it is what
//! lets one ordered key-value store serve relational, graph, vector, and
//! text access paths uniformly.
//!
//! Two laws, enforced by the property tests in `storage/tests.rs`:
//! 1. decode(encode(v)) == v            (round-trip identity)
//! 2. encode(a) < encode(b) ⇔ a < b     (order embedding)
//!
//! The type tags below are assigned in exactly the declaration order of the
//! `DataValue` enum, so cross-type byte order and cross-type semantic order
//! agree by construction. Never reorder or reuse a tag once data exists.

use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::io::Write;
use std::str::FromStr;

use byteorder::{BigEndian, ByteOrder, WriteBytesExt};
use miette::{Result, bail, miette};
use regex::Regex;

use crate::data::value::{
    DataValue, JsonData, Num, RegexWrapper, UuidWrapper, Validity, ValidityTs, Vector,
};

// Tags in `DataValue` declaration order: the single source of cross-type order.
const INIT_TAG: u8 = 0x00; // collection terminator, sorts before everything
const NULL_TAG: u8 = 0x01;
const FALSE_TAG: u8 = 0x02;
const TRUE_TAG: u8 = 0x03;
const NUM_TAG: u8 = 0x04;
const STR_TAG: u8 = 0x05;
const BYTES_TAG: u8 = 0x06;
const UUID_TAG: u8 = 0x07;
const REGEX_TAG: u8 = 0x08;
const LIST_TAG: u8 = 0x09;
const SET_TAG: u8 = 0x0A;
const VEC_TAG: u8 = 0x0B;
const JSON_TAG: u8 = 0x0C;
const VLD_TAG: u8 = 0x0D;
const BOT_TAG: u8 = 0xFF;

/// An encoded `Validity` is always exactly this many bytes (tag + flipped
/// timestamp + assert flag); `check_key_for_bitemporal` splices seek keys
/// on this fixed width, two slots per fact key.
pub(crate) const ENCODED_VLD_LEN: usize = 10;

const VEC_F32: u8 = 0x01;
const VEC_F64: u8 = 0x02;

// Ints and floats share one sortable space: every number is encoded first by
// its f64 order-image; ints outside the exactly-representable f64 range carry
// their exact value in a second field. The subtag orders equal-valued
// int-before-float.
const IS_FLOAT: u8 = 0b0001_0000;
const IS_APPROX_INT: u8 = 0b0000_0100;
const IS_EXACT_INT: u8 = 0b0000_0000;
const EXACT_INT_BOUND: i64 = 0x20_0000_0000_0000;

pub(crate) trait MemCmpEncoder: Write {
    fn encode_datavalue(&mut self, v: &DataValue) {
        // Writes to the underlying sink are infallible in practice: the
        // encoder is only ever used with `Vec<u8>`.
        match v {
            DataValue::Null => self.write_u8(NULL_TAG).unwrap(),
            DataValue::Bool(false) => self.write_u8(FALSE_TAG).unwrap(),
            DataValue::Bool(true) => self.write_u8(TRUE_TAG).unwrap(),
            DataValue::Num(n) => {
                self.write_u8(NUM_TAG).unwrap();
                self.encode_num(*n);
            }
            DataValue::Str(s) => {
                self.write_u8(STR_TAG).unwrap();
                self.encode_bytes(s.as_bytes());
            }
            DataValue::Bytes(b) => {
                self.write_u8(BYTES_TAG).unwrap();
                self.encode_bytes(b)
            }
            DataValue::Uuid(u) => {
                self.write_u8(UUID_TAG).unwrap();
                // Field order (hi, mid, low) makes v1 UUIDs time-ordered; it
                // matches UuidWrapper's Ord implementation.
                let (s_l, s_m, s_h, s_rest) = u.0.as_fields();
                self.write_u16::<BigEndian>(s_h).unwrap();
                self.write_u16::<BigEndian>(s_m).unwrap();
                self.write_u32::<BigEndian>(s_l).unwrap();
                self.write_all(s_rest.as_ref()).unwrap();
            }
            DataValue::Regex(rx) => {
                self.write_u8(REGEX_TAG).unwrap();
                self.encode_bytes(rx.0.as_str().as_bytes())
            }
            DataValue::List(l) => {
                self.write_u8(LIST_TAG).unwrap();
                for el in l {
                    self.encode_datavalue(el);
                }
                self.write_u8(INIT_TAG).unwrap()
            }
            DataValue::Set(s) => {
                self.write_u8(SET_TAG).unwrap();
                for el in s {
                    self.encode_datavalue(el);
                }
                self.write_u8(INIT_TAG).unwrap()
            }
            DataValue::Vec(arr) => {
                // Elements are order-encoded (not raw IEEE bits): raw bits of
                // negative floats sort backwards bytewise. Length sorts before
                // content, matching Vector's Ord.
                self.write_u8(VEC_TAG).unwrap();
                match arr {
                    Vector::F32(a) => {
                        self.write_u8(VEC_F32).unwrap();
                        self.write_u64::<BigEndian>(a.len() as u64).unwrap();
                        for el in a {
                            self.write_u32::<BigEndian>(order_encode_f32(canonical_nan_f32(*el)))
                                .unwrap();
                        }
                    }
                    Vector::F64(a) => {
                        self.write_u8(VEC_F64).unwrap();
                        self.write_u64::<BigEndian>(a.len() as u64).unwrap();
                        for el in a {
                            self.write_u64::<BigEndian>(order_encode_f64(canonical_nan_f64(*el)))
                                .unwrap();
                        }
                    }
                }
            }
            DataValue::Json(j) => {
                self.write_u8(JSON_TAG).unwrap();
                let s = j.0.to_string();
                self.encode_bytes(s.as_bytes());
            }
            DataValue::Validity(vld) => {
                // Timestamp and assert flag are bit-flipped so that newer
                // versions sort FIRST among keys sharing a tuple prefix:
                // that is what makes the as-of seek land on the newest
                // eligible version.
                let ts = vld.timestamp.0.0;
                let ts_flipped = !order_encode_i64(ts);
                self.write_u8(VLD_TAG).unwrap();
                self.write_u64::<BigEndian>(ts_flipped).unwrap();
                self.write_u8(!vld.is_assert.0 as u8).unwrap();
            }
            DataValue::Bot => self.write_u8(BOT_TAG).unwrap(),
        }
    }

    fn encode_num(&mut self, v: Num) {
        let f = v.get_float();
        let u = order_encode_f64(f);
        self.write_u64::<BigEndian>(u).unwrap();
        match v {
            Num::Int(i) => {
                if i > -EXACT_INT_BOUND && i < EXACT_INT_BOUND {
                    self.write_u8(IS_EXACT_INT).unwrap();
                } else {
                    self.write_u8(IS_APPROX_INT).unwrap();
                    self.write_u64::<BigEndian>(order_encode_i64(i)).unwrap();
                }
            }
            Num::Float(_) => {
                self.write_u8(IS_FLOAT).unwrap();
            }
        }
    }

    /// Group encoding: bytes are written in groups of 8 followed by a marker
    /// byte encoding how much of the group is padding. This keeps arbitrary
    /// byte strings memcomparable (a prefix always sorts first) while
    /// remaining self-delimiting.
    fn encode_bytes(&mut self, key: &[u8]) {
        let len = key.len();
        let mut index = 0;
        while index <= len {
            let remain = len - index;
            let mut pad: usize = 0;
            if remain > ENC_GROUP_SIZE {
                self.write_all(&key[index..index + ENC_GROUP_SIZE]).unwrap();
            } else {
                pad = ENC_GROUP_SIZE - remain;
                self.write_all(&key[index..]).unwrap();
                self.write_all(&ENC_ASC_PADDING[..pad]).unwrap();
            }
            self.write_all(&[ENC_MARKER - (pad as u8)]).unwrap();
            index += ENC_GROUP_SIZE;
        }
    }
}

// Only `Vec<u8>` may be an encoder sink: writes to a Vec are infallible,
// which is what makes the encoder's `unwrap()`s a fact of the type system
// rather than a promise. A fallible sink (a File) must not get this trait.
impl MemCmpEncoder for Vec<u8> {}

pub(crate) fn decode_bytes(data: &[u8]) -> Result<(Vec<u8>, &[u8])> {
    let chunk_len = ENC_GROUP_SIZE + 1;
    let mut key = Vec::with_capacity(data.len() / chunk_len * ENC_GROUP_SIZE);
    let mut offset = 0;
    loop {
        let next_offset = offset + chunk_len;
        let Some(chunk) = data.get(offset..next_offset) else {
            bail!("corrupt memcmp key: truncated byte-string group");
        };
        offset = next_offset;

        let (&marker, bytes) = chunk.split_last().expect("chunk is non-empty");
        let pad_size = (ENC_MARKER - marker) as usize;

        if pad_size == 0 {
            key.extend_from_slice(bytes);
            continue;
        }
        if pad_size > ENC_GROUP_SIZE {
            bail!("corrupt memcmp key: invalid group marker {marker:#x}");
        }

        let (bytes, padding) = bytes.split_at(ENC_GROUP_SIZE - pad_size);
        key.extend_from_slice(bytes);

        if padding.iter().any(|x| *x != 0) {
            bail!("corrupt memcmp key: non-zero padding");
        }

        return Ok((key, &data[offset..]));
    }
}

const SIGN_MARK: u64 = 0x8000_0000_0000_0000;
const SIGN_MARK_32: u32 = 0x8000_0000;

fn order_encode_i64(v: i64) -> u64 {
    v as u64 ^ SIGN_MARK
}

fn order_decode_i64(u: u64) -> i64 {
    (u ^ SIGN_MARK) as i64
}

/// Order-encode a float: positive values get the sign bit set, negative
/// values are bit-flipped, so unsigned byte order equals `f64::total_cmp`
/// order — which is exactly the order `Num::cmp` uses for floats (including
/// -NaN below -∞ and +NaN above +∞). No NaN normalization here: scalars
/// follow total_cmp.
fn order_encode_f64(v: f64) -> u64 {
    let u = v.to_bits();
    if v.is_sign_positive() {
        u | SIGN_MARK
    } else {
        !u
    }
}

fn order_decode_f64(u: u64) -> f64 {
    let u = if u & SIGN_MARK > 0 {
        u & (!SIGN_MARK)
    } else {
        !u
    };
    f64::from_bits(u)
}

fn order_encode_f32(v: f32) -> u32 {
    let u = v.to_bits();
    if v.is_sign_positive() {
        u | SIGN_MARK_32
    } else {
        !u
    }
}

/// Vector elements order under `OrderedFloat` semantics (all NaNs equal and
/// greater than everything), unlike scalar `Num` floats which use
/// `total_cmp`. Canonicalizing every NaN to the positive quiet NaN makes the
/// byte order match, at the cost of NaN sign/payload not round-tripping —
/// which `OrderedFloat` equality cannot observe.
fn canonical_nan_f32(v: f32) -> f32 {
    if v.is_nan() { f32::NAN } else { v }
}

fn canonical_nan_f64(v: f64) -> f64 {
    if v.is_nan() { f64::NAN } else { v }
}

fn order_decode_f32(u: u32) -> f32 {
    let u = if u & SIGN_MARK_32 > 0 {
        u & (!SIGN_MARK_32)
    } else {
        !u
    };
    f32::from_bits(u)
}

const ENC_GROUP_SIZE: usize = 8;
const ENC_MARKER: u8 = b'\xff';
const ENC_ASC_PADDING: [u8; ENC_GROUP_SIZE] = [0; ENC_GROUP_SIZE];

fn take<'a>(data: &'a [u8], n: usize, what: &str) -> Result<(&'a [u8], &'a [u8])> {
    if data.len() < n {
        bail!("corrupt memcmp key: truncated {what}");
    }
    Ok(data.split_at(n))
}

impl Num {
    pub(crate) fn decode_from_key(bs: &[u8]) -> Result<(Self, &[u8])> {
        let (float_part, remaining) = take(bs, 8, "number")?;
        let f = order_decode_f64(BigEndian::read_u64(float_part));
        let (tag, remaining) = take(remaining, 1, "number subtag")?;
        match tag[0] {
            IS_FLOAT => Ok((Num::Float(f), remaining)),
            IS_EXACT_INT => Ok((Num::Int(f as i64), remaining)),
            IS_APPROX_INT => {
                let (int_part, remaining) = take(remaining, 8, "big integer")?;
                Ok((
                    Num::Int(order_decode_i64(BigEndian::read_u64(int_part))),
                    remaining,
                ))
            }
            t => bail!("corrupt memcmp key: unknown number subtag {t:#x}"),
        }
    }
}

/// Corrupt input can nest LIST/SET tags arbitrarily deep; recursion past this
/// bound is a corruption error, not a stack overflow. Encoder-produced keys
/// are nowhere near this deep.
const MAX_DECODE_DEPTH: usize = 128;

impl DataValue {
    pub(crate) fn decode_from_key(bs: &[u8]) -> Result<(Self, &[u8])> {
        Self::decode_from_key_at_depth(bs, 0)
    }

    fn decode_from_key_at_depth(bs: &[u8], depth: usize) -> Result<(Self, &[u8])> {
        if depth > MAX_DECODE_DEPTH {
            bail!("corrupt memcmp key: nesting deeper than {MAX_DECODE_DEPTH}");
        }
        let (tag, remaining) = take(bs, 1, "type tag")?;
        Ok(match tag[0] {
            NULL_TAG => (DataValue::Null, remaining),
            FALSE_TAG => (DataValue::Bool(false), remaining),
            TRUE_TAG => (DataValue::Bool(true), remaining),
            NUM_TAG => {
                let (n, remaining) = Num::decode_from_key(remaining)?;
                (DataValue::Num(n), remaining)
            }
            STR_TAG => {
                let (bytes, remaining) = decode_bytes(remaining)?;
                let s = String::from_utf8(bytes)
                    .map_err(|_| miette!("corrupt memcmp key: string is not UTF-8"))?;
                (DataValue::Str(s.into()), remaining)
            }
            BYTES_TAG => {
                let (bytes, remaining) = decode_bytes(remaining)?;
                (DataValue::Bytes(bytes), remaining)
            }
            UUID_TAG => {
                let (uuid_data, remaining) = take(remaining, 16, "uuid")?;
                let s_h = BigEndian::read_u16(&uuid_data[0..2]);
                let s_m = BigEndian::read_u16(&uuid_data[2..4]);
                let s_l = BigEndian::read_u32(&uuid_data[4..8]);
                let mut s_rest = [0u8; 8];
                s_rest.copy_from_slice(&uuid_data[8..]);
                let uuid = uuid::Uuid::from_fields(s_l, s_m, s_h, &s_rest);
                (DataValue::Uuid(UuidWrapper(uuid)), remaining)
            }
            REGEX_TAG => {
                let (bytes, remaining) = decode_bytes(remaining)?;
                let s = String::from_utf8(bytes)
                    .map_err(|_| miette!("corrupt memcmp key: regex is not UTF-8"))?;
                let rx = Regex::from_str(&s)
                    .map_err(|e| miette!("corrupt memcmp key: invalid regex: {e}"))?;
                (DataValue::Regex(RegexWrapper(rx)), remaining)
            }
            LIST_TAG => {
                let mut collected = vec![];
                let mut remaining = remaining;
                loop {
                    match remaining.first() {
                        None => bail!("corrupt memcmp key: unterminated list"),
                        Some(&INIT_TAG) => break,
                        Some(_) => {
                            let (val, next_chunk) =
                                DataValue::decode_from_key_at_depth(remaining, depth + 1)?;
                            remaining = next_chunk;
                            collected.push(val);
                        }
                    }
                }
                (DataValue::List(collected), &remaining[1..])
            }
            SET_TAG => {
                let mut collected = BTreeSet::default();
                let mut remaining = remaining;
                loop {
                    match remaining.first() {
                        None => bail!("corrupt memcmp key: unterminated set"),
                        Some(&INIT_TAG) => break,
                        Some(_) => {
                            let (val, next_chunk) =
                                DataValue::decode_from_key_at_depth(remaining, depth + 1)?;
                            remaining = next_chunk;
                            collected.insert(val);
                        }
                    }
                }
                (DataValue::Set(collected), &remaining[1..])
            }
            VEC_TAG => {
                let (t_tag, remaining) = take(remaining, 1, "vector element type")?;
                let (len_bytes, rest) = take(remaining, 8, "vector length")?;
                let len = BigEndian::read_u64(len_bytes) as usize;
                match t_tag[0] {
                    VEC_F32 => {
                        let byte_len = len
                            .checked_mul(4)
                            .ok_or_else(|| miette!("corrupt memcmp key: vector length overflow"))?;
                        let (data, rest) = take(rest, byte_len, "f32 vector body")?;
                        let v: Vec<f32> = data
                            .chunks_exact(4)
                            .map(|c| order_decode_f32(BigEndian::read_u32(c)))
                            .collect();
                        (DataValue::Vec(Vector::F32(ndarray::Array1::from(v))), rest)
                    }
                    VEC_F64 => {
                        let byte_len = len
                            .checked_mul(8)
                            .ok_or_else(|| miette!("corrupt memcmp key: vector length overflow"))?;
                        let (data, rest) = take(rest, byte_len, "f64 vector body")?;
                        let v: Vec<f64> = data
                            .chunks_exact(8)
                            .map(|c| order_decode_f64(BigEndian::read_u64(c)))
                            .collect();
                        (DataValue::Vec(Vector::F64(ndarray::Array1::from(v))), rest)
                    }
                    t => bail!("corrupt memcmp key: unknown vector element type {t:#x}"),
                }
            }
            JSON_TAG => {
                let (bytes, remaining) = decode_bytes(remaining)?;
                let json = serde_json::from_slice(&bytes)
                    .map_err(|e| miette!("corrupt memcmp key: invalid JSON: {e}"))?;
                (DataValue::Json(JsonData(json)), remaining)
            }
            VLD_TAG => {
                let (ts_flipped_bytes, rest) = take(remaining, 8, "validity timestamp")?;
                let ts = order_decode_i64(!BigEndian::read_u64(ts_flipped_bytes));
                let (is_assert_byte, rest) = take(rest, 1, "validity flag")?;
                let is_assert = is_assert_byte[0] == 0;
                (
                    DataValue::Validity(Validity {
                        timestamp: ValidityTs(Reverse(ts)),
                        is_assert: Reverse(is_assert),
                    }),
                    rest,
                )
            }
            BOT_TAG => (DataValue::Bot, remaining),
            t => bail!("corrupt memcmp key: unknown type tag {t:#x}"),
        })
    }
}
