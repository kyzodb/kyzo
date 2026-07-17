/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

#![no_main]

//! Fuzzes the msgpack value islands that `memcmp_codec.rs` doesn't reach —
//! the fact-value payload (`data/fact_payload.rs::decode_fact_payload`), a
//! bare `DataValue` off the wire (`rmp_serde::from_slice`), and the on-disk
//! catalog row (`runtime/relation.rs::RelationHandle::decode`) — the three
//! places a corrupt or hostile byte stream reaches `DataValue`'s derived
//! `Deserialize` directly, rather than through the memcmp key codec's own
//! re-validating decode path.
//!
//! This is the fuzz target story #62 chunk 2's hostile review named:
//! `Interval`'s and `RelationId`'s derived `Deserialize` impls used to
//! bypass their smart constructors on this exact path (a corrupt payload
//! field, or a corrupt catalog row, could synthesize a backwards interval
//! or an out-of-bound relation id by direct field assignment — the latter
//! `assert!`-panicked the process in `RelationId::new`). Both are now
//! hand-written `Deserialize` impls that re-validate (both in
//! `data/json.rs`). `decode_fact_payload` and `RelationHandle::decode` are
//! `pub(crate)`, so `crates/kyzo-core/src/fuzz_api.rs` adds three narrow
//! `fuzz-internals`-gated façades — same posture as the existing
//! `fuzz_parse_script` target — to reach them without widening the crate's
//! normal public surface.
//!
//! Invariants, mirroring the module doc on `memcmp_codec.rs`:
//! 1. never panic on arbitrary bytes (`Ok`/`Err` both acceptable);
//! 2. on `Ok(DataValue::Interval(iv))` from ANY of the three decode paths,
//!    `iv.start() < iv.end()` — the bypass-detecting law. A derive
//!    regression here builds an interval straight from wire bytes with no
//!    constructor call, so only checking the invariant on successful decode
//!    (not merely absence of a panic) catches it;
//! 3. on `Ok(id)` from the catalog-row decode, `id <= MAX_RELATION_ID` —
//!    the analogous law for `RelationId`.

use kyzo::DataValue;
use kyzo::fuzz_api::{
    MAX_RELATION_ID, finite_interval_is_ordered, fuzz_decode_fact_payload, fuzz_decode_relation_handle_id,
};
use libfuzzer_sys::fuzz_target;

/// Check the bypass-detecting law on every `DataValue`, recursing into
/// `List`/`Set` so an interval nested inside a composite is caught too.
fn check_value(v: &DataValue) {
    assert!(
        finite_interval_is_ordered(v),
        "smart-constructor bypass: decoded Interval violates start < end"
    );
    match v {
        DataValue::List(items) => items.iter().for_each(check_value),
        DataValue::Set(items) => items.iter().for_each(check_value),
        _ => {}
    }
}

fuzz_target!(|data: &[u8]| {
    // (a) The v3 fact-payload island: count + offset table + tagged fields,
    // `FIELD_OTHER` bottoming out in exactly the same derived `Deserialize`
    // as (b).
    if let Ok(tuple) = fuzz_decode_fact_payload(data) {
        tuple.iter().for_each(check_value);
    }

    // (b) A bare `DataValue` straight off the wire — the same derived
    // `Deserialize` (a)'s `FIELD_OTHER` fields and the runtime's other
    // msgpack islands (e.g. sketch state) all route through.
    if let Ok(v) = rmp_serde::from_slice::<DataValue>(data) {
        check_value(&v);
    }

    // (c) The on-disk catalog row: `RelationHandle::decode`, exercising
    // `RelationId`'s hand-written `Deserialize` the same way.
    if let Ok(id) = fuzz_decode_relation_handle_id(data) {
        assert!(
            id <= MAX_RELATION_ID,
            "smart-constructor bypass: decoded RelationId({id}) exceeds the 48-bit bound"
        );
    }
});
