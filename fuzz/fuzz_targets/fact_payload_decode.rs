/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

#![no_main]

//! Fuzzes the msgpack / codec value islands that `memcmp_codec.rs` doesn't
//! reach — canonical multi-value decode (`decode_values_all`), a bare
//! `DataValue` off the wire (`rmp_serde::from_slice`), and `RelationId`'s
//! hand-written `Deserialize` — the places a corrupt or hostile byte stream
//! reaches validating constructors without going through the memcmp key
//! codec's own re-validating path.
//!
//! Rewired after sealed-door demolition: these laws speak public
//! `kyzo` / `kyzo-model` seats only. The old `kyzo::fuzz_api` façade is gone
//! and must not be restored.
//!
//! Invariants:
//! 1. never panic on arbitrary bytes (`Ok`/`Err` both acceptable);
//! 2. on `Ok(DataValue::Interval(iv))` from ANY decode path, a finite
//!    interval satisfies `start <= end` (closed normal form);
//! 3. on `Ok(id)` from `RelationId` deserialize, `id.raw() < RelationId::CAP`.

use kyzo::{DataValue, RelationId};
use kyzo_model::value::decode_values_all;
use libfuzzer_sys::fuzz_target;

/// Bypass-detecting law: a successfully decoded finite Interval must
/// satisfy closed-form `start <= end`. Non-intervals and unbounded /
/// empty ends pass.
fn finite_interval_is_ordered(v: &DataValue) -> bool {
    match v {
        DataValue::Interval(iv) => match (iv.start(), iv.end()) {
            (Some(a), Some(b)) => a <= b,
            _ => true,
        },
        _ => true,
    }
}

/// Check the bypass-detecting law on every `DataValue`, recursing into
/// `List`/`Set` so an interval nested inside a composite is caught too.
fn check_value(v: &DataValue) {
    assert!(
        finite_interval_is_ordered(v),
        "smart-constructor bypass: decoded Interval violates start <= end"
    );
    match v {
        DataValue::List(items) => items.iter().for_each(check_value),
        DataValue::Set(items) => items.iter().for_each(check_value),
        _ => {}
    }
}

fuzz_target!(|data: &[u8]| {
    // (a) Canonical multi-value island (same seat the old façade wrapped).
    if let Ok(vals) = decode_values_all(data) {
        vals.iter().for_each(check_value);
    }

    // (b) A bare `DataValue` straight off the wire — DataValue's validating
    // `Deserialize` (canonical bytes).
    if let Ok(v) = rmp_serde::from_slice::<DataValue>(data) {
        check_value(&v);
    }

    // (c) RelationId's hand-written Deserialize (catalog wire form: raw u64).
    if let Ok(id) = rmp_serde::from_slice::<RelationId>(data) {
        assert!(
            id.raw() < RelationId::CAP,
            "smart-constructor bypass: decoded RelationId({}) exceeds the 48-bit bound",
            id.raw()
        );
    }
});
