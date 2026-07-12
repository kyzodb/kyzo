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

//! Fuzzes the memcomparable key codec (`crates/kyzo-core/src/data/value/`)
//! through its public façade — `fuzz_encode_tuple_key` / `decode_tuple_from_key`
//! (the encoder needed a small `fuzz_api` façade after the value-plane split
//! moved `encode_key_with_suffix` off the crate root; see `fuzz_api.rs`'s
//! module doc) — which exercise the exact codec under test.
//!
//! Two laws, mirroring the doc comment on `data/value/canonical.rs` itself:
//! 1. round-trip: `decode(encode(tuple)) == tuple`
//! 2. order embedding: `encode(a) < encode(b)` (bytewise) iff `a < b`
//!    (semantically, via `Tuple`'s derived `Ord`)
//!
//! Every `DataValue` kind is covered: the enum and all its field types
//! (`Num`, `UuidWrapper`, `RegexSource`, `Vector`, `JsonData`, `Validity`,
//! `ValidityTs`, `Interval`, `Bound`) are `pub` and re-exported at the
//! crate root, so arbitrary values are built directly with no visibility
//! widening either.

use std::cmp::Reverse;
use std::collections::BTreeSet;

use arbitrary::{Arbitrary, Unstructured};
use kyzo::fuzz_api::{fuzz_encode_tuple_key, fuzz_interval, fuzz_regex};
use kyzo::{
    DataValue, Tuple, UuidWrapper, Validity, ValidityTs, Vector, decode_tuple_from_key,
};
use libfuzzer_sys::fuzz_target;

/// Bound on `List`/`Set` nesting so the generator itself always terminates
/// (and stays well clear of the codec's own `MAX_DEPTH` refusal, which is
/// corrupt-input handling, not something a well-formed generated value
/// should ever hit).
const MAX_DEPTH: usize = 5;

/// A small pool of always-valid regex patterns: `Regex` is "used internally
/// only" (never round-tripped through untrusted bytes), so the codec never
/// has to parse a regex string — it only ever re-parses one *this process*
/// already validated. Feeding `RegexSource::validated` a guaranteed-valid
/// pattern keeps the generator itself panic-free.
const REGEX_PATTERNS: &[&str] = &[
    "a+",
    "[a-z0-9]+",
    ".*",
    "^abc$",
    "(foo|bar)+",
    r"\d{2,4}",
    "",
];

fn gen_json(u: &mut Unstructured, depth: usize) -> arbitrary::Result<serde_json::Value> {
    let max_kind: u32 = if depth < 3 { 5 } else { 3 };
    Ok(match u.int_in_range(0..=max_kind)? {
        0 => serde_json::Value::Null,
        1 => serde_json::Value::Bool(bool::arbitrary(u)?),
        2 => serde_json::Value::from(u32::arbitrary(u)? as u64),
        3 => serde_json::Value::String(String::arbitrary(u)?),
        4 => {
            let n = u.int_in_range(0..=3u32)?;
            let mut v = Vec::with_capacity(n as usize);
            for _ in 0..n {
                v.push(gen_json(u, depth + 1)?);
            }
            serde_json::Value::Array(v)
        }
        _ => {
            let n = u.int_in_range(0..=3u32)?;
            let mut m = serde_json::Map::new();
            for i in 0..n {
                let k = String::arbitrary(u)?;
                m.insert(format!("{i}_{k}"), gen_json(u, depth + 1)?);
            }
            serde_json::Value::Object(m)
        }
    })
}

/// Build an arbitrary [`DataValue`] covering every kind the enum has. Every
/// constructor used here is public (or reachable through a public `From`
/// impl) at the `kyzo` crate root — see the module doc.
fn gen_value(u: &mut Unstructured, depth: usize) -> arbitrary::Result<DataValue> {
    // List/Set (recursive) only offered while under the depth bound.
    let max_kind: u32 = if depth < MAX_DEPTH { 13 } else { 11 };
    Ok(match u.int_in_range(0..=max_kind)? {
        0 => DataValue::Null,
        1 => DataValue::Bool(bool::arbitrary(u)?),
        2 => DataValue::from(i64::arbitrary(u)?),
        3 => DataValue::from(f64::from_bits(u64::arbitrary(u)?)),
        4 => DataValue::Str(String::arbitrary(u)?),
        5 => DataValue::Bytes(Vec::<u8>::arbitrary(u)?),
        6 => DataValue::Uuid(UuidWrapper(uuid::Uuid::from_u128(u128::arbitrary(u)?))),
        7 => {
            let pat =
                REGEX_PATTERNS[u.int_in_range(0..=(REGEX_PATTERNS.len() - 1) as u32)? as usize];
            fuzz_regex(pat.to_string()).expect("fixed pool is always valid regex")
        }
        8 => {
            let len = u.int_in_range(0..=8u32)? as usize;
            let mut v = Vec::with_capacity(len);
            for _ in 0..len {
                v.push(f64::from_bits(u64::arbitrary(u)?));
            }
            DataValue::Vector(Vector::new(v))
        }
        9 => {
            // Re-parsed through the same `to_string` the codec itself uses
            // to serialize JSON: the round-trip law tests the memcmp codec,
            // not `serde_json`'s own text-serialization stability (e.g.
            // `-0` vs `0`, or numeric-repr normalization) — a JSON value
            // that hasn't already reached that fixed point isn't a fair
            // input for a law about `encode`/`decode` alone.
            let j = gen_json(u, 0)?;
            let normalized: serde_json::Value =
                serde_json::from_str(&j.to_string()).unwrap_or(serde_json::Value::Null);
            DataValue::from(normalized)
        }
        10 => DataValue::Validity(Validity {
            timestamp: ValidityTs::from_raw(i64::arbitrary(u)?),
            is_assert: Reverse(bool::arbitrary(u)?),
        }),
        11 => {
            // `fuzz_interval` canonicalizes (empty denotations collapse to
            // the empty interval), so every input quadruple is a lawful
            // value — no separate validation needed here.
            fuzz_interval(
                u8::arbitrary(u)?,
                i64::arbitrary(u)?,
                u8::arbitrary(u)?,
                i64::arbitrary(u)?,
            )
        }
        12 => {
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
    let mut u = Unstructured::new(data);
    let Ok(tuple_a) = gen_tuple(&mut u) else {
        return;
    };
    let Ok(tuple_b) = gen_tuple(&mut u) else {
        return;
    };

    // Same relation id for both: the tag prefix is identical, so a
    // bytewise comparison of the two full encoded keys reduces exactly to
    // a bytewise comparison of the tuple encodings.
    let Some(key_a) = fuzz_encode_tuple_key(0, &tuple_a) else {
        return;
    };
    let Some(key_b) = fuzz_encode_tuple_key(0, &tuple_b) else {
        return;
    };

    // Law 1: round-trip identity.
    let decoded_a = decode_tuple_from_key(key_a.as_bytes(), tuple_a.len()).unwrap_or_else(|e| {
        panic!("encoder-produced bytes must decode: {e:?}\ntuple: {tuple_a:?}")
    });
    assert_eq!(
        decoded_a, tuple_a,
        "round-trip law violated: decode(encode(t)) != t\nt = {tuple_a:?}"
    );
    let decoded_b = decode_tuple_from_key(key_b.as_bytes(), tuple_b.len()).unwrap_or_else(|e| {
        panic!("encoder-produced bytes must decode: {e:?}\ntuple: {tuple_b:?}")
    });
    assert_eq!(
        decoded_b, tuple_b,
        "round-trip law violated: decode(encode(t)) != t\nt = {tuple_b:?}"
    );

    // Law 2: order embedding — bytewise key order equals semantic tuple
    // order (`Tuple = Vec<DataValue>`'s derived, lexicographic `Ord`).
    let byte_order = key_a.as_bytes().cmp(key_b.as_bytes());
    let semantic_order = tuple_a.cmp(&tuple_b);
    assert_eq!(
        byte_order, semantic_order,
        "order-embedding law violated: bytewise {byte_order:?} != semantic {semantic_order:?}\na = {tuple_a:?}\nb = {tuple_b:?}"
    );
});
