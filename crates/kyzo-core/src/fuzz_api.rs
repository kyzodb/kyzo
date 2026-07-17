/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! An opaque façade over the crate-internal parse tier, for the KyzoScript
//! fuzz target (`fuzz/fuzz_targets/kyzoscript_parser.rs`). Same posture as
//! [`crate::bench_api`]: gated behind its own feature so it never touches
//! the normal public surface, and hands the fuzzer only what it needs —
//! "does this text parse without panicking or hanging" — never the AST
//! type itself, which stays crate-internal.
//!
//! `decode_tuple_from_key` is public at the crate root and needs no
//! façade, but its encode-side counterpart moved during the value-plane
//! split: the current `encode_key_with_suffix` (`data/value/row.rs`)
//! is not re-exported (the key-layout module stays crate-internal), so
//! `fuzz_encode_tuple_key` below is the same "already public, façade
//! exists only because the constructor itself isn't" posture as
//! `fuzz_decode_fact_payload`.
//!
//! `fuzz_decode_fact_payload` and `fuzz_decode_relation_handle_id` (story
//! #62 chunk 2 hostile-review follow-up, `fuzz/fuzz_targets/
//! fact_payload_decode.rs`) reach the two remaining msgpack value islands
//! the memcmp-codec target doesn't cover: the v3 fact payload
//! (`data/fact_payload.rs`) and the on-disk catalog row (`runtime/
//! relation.rs::RelationHandle`). `decode_fact_payload` returns a public
//! `Tuple`/`DataValue`, so it crosses the boundary unchanged; `RelationHandle`
//! itself stays crate-internal — the wrapper hands back only its relation
//! id's raw `u64`, same "discard the internal type" posture as
//! `fuzz_parse_script` above.

use std::collections::BTreeMap;

use miette::Result;

use crate::data::value::DataValue;
use crate::data::value::StorageKey;
use crate::data::value::RelationId;
use crate::data::value::Tuple;
use crate::data::value::decode_values_all;
use crate::data::value::encode_key_with_suffix;
use crate::fixed_rule::DEFAULT_FIXED_RULES;
use crate::parse::parse_script;
use crate::runtime::current_validity;
use crate::runtime::relation::RelationHandle;

/// Parse a KyzoScript source string with empty params and the real default
/// fixed-rule registry, discarding the parsed AST. The fuzz target's only
/// invariant is "never panics, never hangs" on arbitrary bytes; `Ok`/`Err`
/// are both acceptable outcomes.
pub fn fuzz_parse_script(src: &str) -> Result<()> {
    let cur_vld = current_validity()?;
    parse_script(src, &BTreeMap::new(), &DEFAULT_FIXED_RULES, cur_vld)?;
    Ok(())
}

/// Encode a tuple as a relation-prefixed memcmp key
/// (`encode_key_with_suffix` with no suffix columns), for the
/// memcmp-codec fuzz target's round-trip law against
/// [`crate::decode_tuple_from_key`]. `StorageKey`/`RelationId`/`Tuple`
/// are already public; the façade exists only because
/// `encode_key_with_suffix` itself is crate-internal.
pub fn fuzz_encode_tuple_key(rel: u64, tuple: &Tuple) -> Option<StorageKey> {
    let rel = RelationId::new(rel)?;
    Some(encode_key_with_suffix(rel, tuple.as_slice(), &[]))
}

/// Decode arbitrary bytes as a v3 fact payload (`decode_fact_payload`),
/// handing back the decoded row. `Tuple`/`DataValue` are already public, so
/// nothing is hidden here — the façade exists only because
/// `decode_fact_payload` itself is `pub(crate)`.
pub fn fuzz_decode_fact_payload(data: &[u8]) -> Result<Tuple> {
    let mut row: Tuple = Tuple::new();
    row.extend(decode_values_all(data)?);
    Ok(row)
}

/// Decode arbitrary bytes as a catalog row (`RelationHandle::decode`),
/// handing back only the decoded relation id's raw `u64`. `RelationHandle`
/// stays crate-internal; the fuzz law only needs the id to check it never
/// escapes the 48-bit bound smart constructor.
pub fn fuzz_decode_relation_handle_id(data: &[u8]) -> Result<u64> {
    let handle = RelationHandle::decode(data)?;
    Ok(handle.id.raw())
}

/// The relation-id space bound (`data/value/row.rs`'s `RelationId::CAP`),
/// re-exported so the fuzz target can check
/// [`fuzz_decode_relation_handle_id`]'s result without duplicating the
/// constant.
pub const MAX_RELATION_ID: u64 = crate::data::value::RelationId::CAP;

/// Build a `DataValue::Interval` from a canonicalizing `(start, end)`
/// pair, each bound spelled as a `(kind, value)` primitive pair —
/// `kind % 3`: 0 = unbounded, 1 = closed at `value`, 2 = open at `value`
/// — so the memcmp fuzz target can cover every Interval shape without
/// naming the crate-internal `Bound`/`Interval` construction types.
/// `Interval::new` itself canonicalizes (empty denotations collapse to
/// `Interval::EMPTY`), so every input pair yields a lawful value.
///
/// Interval stays opaque here: no façade projects bounds as
/// `Option<(i64, i64)>`. Bypass-detecting fuzz laws use `Interval`'s own
/// typed accessors (`start`/`end`) on a `DataValue::Interval` match arm.

/// Bypass-detecting law for fuzz targets: a successfully decoded finite
/// Interval must satisfy `start < end`. Returns `true` for non-intervals
/// and for intervals with an unbounded end. Does **not** project bounds
/// as `Option<(i64, i64)>` — Interval stays opaque.
pub fn finite_interval_is_ordered(v: &DataValue) -> bool {
    match v {
        DataValue::Interval(iv) => match (iv.start(), iv.end()) {
            (Some(a), Some(b)) => a < b,
            _ => true,
        },
        _ => true,
    }
}

pub fn fuzz_interval(start_kind: u8, start_val: i64, end_kind: u8, end_val: i64) -> DataValue {
    use crate::data::value::Bound;
    fn bound(kind: u8, val: i64) -> Bound {
        match kind % 3 {
            0 => Bound::Unbounded,
            1 => Bound::Closed(val),
            _ => Bound::Open(val),
        }
    }
    DataValue::Interval(crate::data::value::Interval::new(
        bound(start_kind, start_val),
        bound(end_kind, end_val),
    ))
}

/// Build a `DataValue::Regex` from one of the memcmp fuzz target's fixed
/// always-valid patterns, with no non-default flags — so the target can
/// cover the Regex kind without naming the crate-internal `RegexFlags`
/// construction type (`RegexSource` itself is already public).
pub fn fuzz_regex(pattern: String) -> Option<DataValue> {
    let source =
        crate::data::value::RegexSource::validated(crate::data::value::RegexFlags::NONE, pattern)
            .ok()?;
    Some(DataValue::Regex(source))
}

/// Lawful `DataValue::Validity` mint for memcmp Ord↔bytes fuzzing.
/// Uses [`ValidityTs::for_assertion`] + [`Validity::new`] only — assert+`i64::MAX`
/// and other unrepresentable Validity states are refused (`None`), never forged
/// via struct literals or `from_raw` past the purity seals.
pub fn fuzz_validity(ts_micros: i64, is_assert: bool) -> Option<DataValue> {
    use crate::data::value::{Validity, ValidityTs};
    let ts = ValidityTs::for_assertion(ts_micros)?;
    let v = Validity::new(ts, is_assert)?;
    Some(DataValue::Validity(v.into()))
}

