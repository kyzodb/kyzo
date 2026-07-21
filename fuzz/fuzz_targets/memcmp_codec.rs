/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

#![no_main]
// `DataValue` is used as a `BTreeSet` element in `gen_value`'s `Set` arm,
// exactly as `crates/kyzo-core/src/lib.rs` notes for its own crate-wide allow:
// clippy's interior-mutability check is a false positive here (the
// `Regex`/cache internals it flags are never mutated through a shared
// reference), and that crate-level allow does not reach across the crate
// boundary into this separate fuzz-target binary, so it is repeated here.
#![allow(clippy::mutable_key_type)]

//! Fuzzes the memcomparable key codec through the public façade
//! (`TupleKey::from_values` + relation-prefix assembly /
//! `decode_tuple_from_key`).
//!
//! Two laws:
//! 1. round-trip: `decode(encode(tuple)) == tuple`
//! 2. order embedding: `encode(a) < encode(b)` (bytewise) iff `a < b`
//!
//! Plus a near-exhaustive structured campaign over a small enumerable
//! corpus (tag-boundary scalars, cross-type representatives, validity
//! corners) — the zone where DST random edge-bouncing is the wrong tool
//! and consistent-wrong codec changes must still fail.

use std::collections::BTreeSet;

use arbitrary::{Arbitrary, Unstructured};
use kyzo::{
    DataValue, Num, RelationId, Tuple, TupleKey, UuidWrapper, Validity, ValidityTs, Vector,
    decode_tuple_from_key,
};
use libfuzzer_sys::fuzz_target;

/// Bound on `List`/`Set` nesting so the generator itself always terminates.
const MAX_DEPTH: usize = 5;

/// Pinned format-v1 tag bytes for the kinds this target can construct
/// through the public `kyzo` surface (independent oracle).
const TAG_NULL: u8 = 0x05;
const TAG_BOOL: u8 = 0x08;
const TAG_NUM: u8 = 0x10;
const TAG_STR: u8 = 0x18;
const TAG_BYTES: u8 = 0x20;
const TAG_UUID: u8 = 0x28;
const TAG_VECTOR: u8 = 0x40;
const TAG_LIST: u8 = 0x48;
const TAG_SET: u8 = 0x50;
const TAG_VALIDITY: u8 = 0x58;

/// Encode a bare tuple under relation id 0 — same shape
/// `decode_tuple_from_key` expects (8-byte relation prefix + values).
fn encode_tuple_key(tuple: &Tuple) -> Vec<u8> {
    let rel = RelationId::SYSTEM;
    let bare = TupleKey::from_values(tuple.as_slice());
    let mut out = Vec::with_capacity(8 + bare.len());
    out.extend_from_slice(&rel.raw_encode());
    out.extend_from_slice(bare.as_bytes());
    out
}

fn assert_laws(tuple_a: &Tuple, tuple_b: &Tuple) {
    let key_a = encode_tuple_key(tuple_a);
    let key_b = encode_tuple_key(tuple_b);

    let decoded_a = decode_tuple_from_key(key_a.as_slice(), tuple_a.len()).unwrap_or_else(|e| {
        panic!("encoder-produced bytes must decode: {e:?}\ntuple: {tuple_a:?}")
    });
    assert_eq!(
        decoded_a, *tuple_a,
        "round-trip law violated: decode(encode(t)) != t\nt = {tuple_a:?}"
    );
    let decoded_b = decode_tuple_from_key(key_b.as_slice(), tuple_b.len()).unwrap_or_else(|e| {
        panic!("encoder-produced bytes must decode: {e:?}\ntuple: {tuple_b:?}")
    });
    assert_eq!(
        decoded_b, *tuple_b,
        "round-trip law violated: decode(encode(t)) != t\nt = {tuple_b:?}"
    );

    let byte_order = key_a.as_slice().cmp(key_b.as_slice());
    let semantic_order = tuple_a.cmp(tuple_b);
    assert_eq!(
        byte_order, semantic_order,
        "order-embedding law violated: bytewise {byte_order:?} != semantic {semantic_order:?}\na = {tuple_a:?}\nb = {tuple_b:?}"
    );
}

/// Enumerable corpus: tag prefixes, cross-type representatives, small
/// scalar domains, validity corners. Pairwise checks are tractable.
fn enumerable_corpus() -> Vec<DataValue> {
    let mut out = vec![
        DataValue::Null,
        DataValue::Bool(false),
        DataValue::Bool(true),
        DataValue::Num(Num::int(i64::MIN)),
        DataValue::Num(Num::int(-1)),
        DataValue::Num(Num::int(0)),
        DataValue::Num(Num::int(1)),
        DataValue::Num(Num::int(i64::MAX)),
        DataValue::Num(Num::float(-1.0)),
        DataValue::Num(Num::float(0.0)),
        DataValue::Num(Num::float(1.0)),
        DataValue::Str(String::new()),
        DataValue::Str("a".into()),
        DataValue::Str("a\u{0}".into()),
        DataValue::Str("ab".into()),
        DataValue::Bytes(vec![]),
        DataValue::Bytes(vec![0x00]),
        DataValue::Bytes(vec![0x00, 0xFF]),
        DataValue::Bytes(vec![0xFF]),
        DataValue::Uuid(UuidWrapper::new(uuid::Uuid::nil())),
        DataValue::Uuid(UuidWrapper::new(uuid::Uuid::from_bytes([0xFF; 16]))),
        DataValue::Vector(Vector::try_new(vec![]).expect("empty")),
        DataValue::Vector(Vector::try_new(vec![0.0]).expect("dim1")),
        DataValue::List(vec![]),
        DataValue::List(vec![DataValue::Null]),
        DataValue::List(vec![DataValue::Bool(false)]),
        DataValue::Set(BTreeSet::new()),
        DataValue::Set([DataValue::from(1i64)].into_iter().collect()),
    ];
    // Validity: lawful asserts + retract at extremes (not i64::MAX assert).
    for &(ts, is_assert) in &[
        (i64::MIN, true),
        (i64::MIN, false),
        (0i64, true),
        (0i64, false),
        (i64::MAX - 1, true),
        (i64::MAX - 1, false),
        (i64::MAX, false), // retract at reserved tick is representable
    ] {
        if let Some(v) = ValidityTs::for_assertion(ts)
            .and_then(|t| Validity::new(t, is_assert))
            .or_else(|| {
                // for_assertion refuses i64::MAX; retract-at-MAX still mints.
                if ts == i64::MAX && !is_assert {
                    Validity::new(ValidityTs::from_raw(ts), false)
                } else {
                    None
                }
            })
        {
            out.push(DataValue::Validity(v.into()));
        }
    }
    out
}

fn first_tag_byte(v: &DataValue) -> u8 {
    match v {
        DataValue::Null => TAG_NULL,
        DataValue::Bool(_) => TAG_BOOL,
        DataValue::Num(_) => TAG_NUM,
        DataValue::Str(_) => TAG_STR,
        DataValue::Bytes(_) => TAG_BYTES,
        DataValue::Uuid(_) => TAG_UUID,
        DataValue::Vector(_) => TAG_VECTOR,
        DataValue::List(_) => TAG_LIST,
        DataValue::Set(_) => TAG_SET,
        DataValue::Validity(_) => TAG_VALIDITY,
        // Kinds not constructed above — still total for the match.
        DataValue::Regex(_) => 0x30,
        DataValue::Json(_) => 0x38,
        DataValue::Interval(_) => 0x60,
        DataValue::Geometry(_) => 0x68,
    }
}

/// Near-exhaustive campaign: every pair in the enumerable corpus, plus
/// pinned tag-prefix checks on each singleton encoding.
fn near_exhaustive_campaign() {
    let corpus = enumerable_corpus();
    // Tag prefix law against the pinned format-v1 table.
    for v in &corpus {
        let t = Tuple::from_vec(vec![v.clone()]);
        let key = encode_tuple_key(&t);
        // Skip 8-byte relation prefix; first value tag is next.
        assert!(
            key.len() > 8,
            "key too short for value tag: {v:?}"
        );
        assert_eq!(
            key[8],
            first_tag_byte(v),
            "tag prefix left format v1 for {v:?}: got {:#04x}",
            key[8]
        );
    }
    // Pairwise round-trip + order embedding (including cross-type).
    for a in &corpus {
        for b in &corpus {
            let ta = Tuple::from_vec(vec![a.clone()]);
            let tb = Tuple::from_vec(vec![b.clone()]);
            assert_laws(&ta, &tb);
            if first_tag_byte(a) != first_tag_byte(b) {
                let ka = encode_tuple_key(&ta);
                let kb = encode_tuple_key(&tb);
                assert_eq!(
                    ka.as_slice().cmp(kb.as_slice()),
                    first_tag_byte(a).cmp(&first_tag_byte(b)),
                    "cross-type byte order left pinned tags:\n  a={a:?}\n  b={b:?}"
                );
            }
        }
    }
}

fn gen_value(u: &mut Unstructured, depth: usize) -> arbitrary::Result<DataValue> {
    // Constructible through the public `kyzo` surface only.
    let max_kind: u32 = if depth < MAX_DEPTH { 9 } else { 7 };
    Ok(match u.int_in_range(0..=max_kind)? {
        0 => DataValue::Null,
        1 => DataValue::Bool(bool::arbitrary(u)?),
        2 => DataValue::from(i64::arbitrary(u)?),
        3 => DataValue::from(f64::from_bits(u64::arbitrary(u)?)),
        4 => DataValue::Str(String::arbitrary(u)?),
        5 => DataValue::Bytes(Vec::<u8>::arbitrary(u)?),
        6 => DataValue::Uuid(UuidWrapper::new(uuid::Uuid::from_u128(u128::arbitrary(u)?))),
        7 => {
            let len = u.int_in_range(0..=8u32)? as usize;
            let mut v = Vec::with_capacity(len);
            for _ in 0..len {
                v.push(f64::from_bits(u64::arbitrary(u)?));
            }
            DataValue::Vector(Vector::try_new(v).expect("fuzz vector len fits u32"))
        }
        8 => {
            let mut out = DataValue::Null;
            for _ in 0..8 {
                let ts = i64::arbitrary(u)?;
                let is_assert = bool::arbitrary(u)?;
                if let Some(v) = ValidityTs::for_assertion(ts)
                    .and_then(|t| Validity::new(t, is_assert))
                {
                    out = DataValue::Validity(v.into());
                    break;
                }
            }
            out
        }
        9 => {
            let n = u.int_in_range(0..=4u32)?;
            let mut items = Vec::with_capacity(n as usize);
            for _ in 0..n {
                items.push(gen_value(u, depth + 1)?);
            }
            DataValue::List(items)
        }
        _ => {
            let n = u.int_in_range(0..=4u32)?;
            let mut items = BTreeSet::new();
            for _ in 0..n {
                items.insert(gen_value(u, depth + 1)?);
            }
            DataValue::Set(items)
        }
    })
}

fn gen_tuple(u: &mut Unstructured) -> arbitrary::Result<Tuple> {
    let n = u.int_in_range(0..=4u32)?;
    let mut t = Tuple::with_capacity(n as usize);
    for _ in 0..n {
        t.push(gen_value(u, 0)?);
    }
    Ok(t)
}

fuzz_target!(|data: &[u8]| {
    // Always: near-exhaustive enumerable campaign (tag / cross-type /
    // small domains). Independent of the fuzzer bytes so a consistent-
    // wrong retag fails on every input, not only lucky draws.
    near_exhaustive_campaign();

    let mut u = Unstructured::new(data);
    let Ok(tuple_a) = gen_tuple(&mut u) else {
        return;
    };
    let Ok(tuple_b) = gen_tuple(&mut u) else {
        return;
    };
    assert_laws(&tuple_a, &tuple_b);
});
