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
//! The memcmp-codec fuzz target needs no façade: `encode_tuple_key` /
//! `decode_tuple_from_key` ([`crate::data::tuple`], re-exported at the
//! crate root) are already public and exercise the exact codec under test.
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

use crate::current_validity;
use crate::data::fact_payload::decode_fact_payload;
use crate::data::value::DataValue;
use crate::data::value::Tuple;
use crate::fixed_rule::DEFAULT_FIXED_RULES;
use crate::parse::parse_script;
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

/// Decode arbitrary bytes as a v3 fact payload (`decode_fact_payload`),
/// handing back the decoded row. `Tuple`/`DataValue` are already public, so
/// nothing is hidden here — the façade exists only because
/// `decode_fact_payload` itself is `pub(crate)`.
pub fn fuzz_decode_fact_payload(data: &[u8]) -> Result<Tuple> {
    let mut row: Tuple = Tuple::new();
    decode_fact_payload(data, &mut row)?;
    Ok(row)
}

/// Decode arbitrary bytes as a catalog row (`RelationHandle::decode`),
/// handing back only the decoded relation id's raw `u64`. `RelationHandle`
/// stays crate-internal; the fuzz law only needs the id to check it never
/// escapes the 48-bit bound smart constructor.
pub fn fuzz_decode_relation_handle_id(data: &[u8]) -> Result<u64> {
    let handle = RelationHandle::decode(data)?;
    Ok(handle.id.0)
}

/// The relation-id space bound (`data/tuple.rs::MAX_RELATION_ID`),
/// re-exported so the fuzz target can check
/// [`fuzz_decode_relation_handle_id`]'s result without duplicating the
/// constant.
pub const MAX_RELATION_ID: u64 = crate::data::value::MAX_RELATION_ID;

/// Extract an `Interval`'s bounds as raw `i64`s. `Interval` (`data/
/// value.rs`) is public, but its `start()`/`end()` accessors are
/// `pub(crate)` — this is the seam the fuzz law needs to check the
/// bypass-detecting invariant (`start < end`) on every successfully-decoded
/// `DataValue::Interval`, without widening the accessors themselves.
pub fn interval_bounds(v: &DataValue) -> Option<(i64, i64)> {
    match v {
        DataValue::Interval(iv) => Some((iv.start(), iv.end())),
        _ => None,
    }
}
