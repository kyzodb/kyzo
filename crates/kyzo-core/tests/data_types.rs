/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Story #88: every `DataValue` kind round-trips through a real stored
//! relation and a real query — Int, Float, String, Bool, Bytes, List,
//! Uuid, Json, Validity, Interval, and Vector — via the public API only.
//! Where a value's own Rust accessor is crate-internal (`Interval`'s
//! `start`/`end`, `DataValue::get_uuid`), the check goes through
//! KyzoScript's own public functions (`interval_start`/`interval_end`) or
//! full `DataValue` equality built from the crate's public re-exports
//! (`UuidWrapper`, `JsonData`, `Validity`) — exactly the boundary an
//! external embedder sits at.

mod common;
use common::*;

use kyzo::{DataValue, UuidWrapper, ValiditySlot, ValidityTs};

#[test]
fn every_data_type_round_trips_through_a_stored_relation() {
    let db = fresh_db();
    db.run_script(
        "?[id, i, f, s, b, by, l, u, j, val, iv] <- [[1, 42, 3.5, 'hello', true, \
         'aGk=', [1, 2, 3], '550e8400-e29b-41d4-a716-446655440000', \
         parse_json('{\"a\": 1}'), validity(100, true), make_interval(10, 20)]] \
         :create allt {id => i: Int, f: Float, s: String, b: Bool, by: Bytes, \
         l: [Int], u: Uuid, j: Json, val: Validity, iv}",
        no_params(),
    )
    .expect("create allt");

    let out = db
        .run_script(
            "?[i, f, s, b, by, l, u, j, val, istart, iend] := \
             *allt{id, i, f, s, b, by, l, u, j, val, iv}, \
             istart = interval_start(iv), iend = interval_end(iv)",
            no_params(),
        )
        .expect("scan allt");
    assert_eq!(out.rows().len(), 1);
    let row = &out.rows()[0];

    assert_eq!(row[0].get_int(), Some(42), "Int");
    assert_eq!(row[1].get_float(), Some(3.5), "Float");
    assert_eq!(row[2].get_str(), Some("hello"), "String");
    assert_eq!(row[3].get_bool(), Some(true), "Bool");
    assert_eq!(row[4].get_bytes(), Some(b"hi".as_slice()), "Bytes");

    let list: Vec<i64> = row[5]
        .get_slice()
        .expect("List")
        .iter()
        .map(|v| v.get_int().unwrap())
        .collect();
    assert_eq!(list, vec![1, 2, 3], "List");

    let expected_uuid = DataValue::Uuid(UuidWrapper::new(
        uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap(),
    ));
    assert_eq!(row[6], expected_uuid, "Uuid");

    let expected_json = DataValue::from(serde_json::json!({"a": 1}));
    assert_eq!(row[7], expected_json, "Json");

    let expected_validity = DataValue::Validity(ValiditySlot::from_stored(ValidityTs::from_raw(100), true));
    assert_eq!(row[8], expected_validity, "Validity");

    assert_eq!(row[9].get_int(), Some(10), "Interval start");
    assert_eq!(row[10].get_int(), Some(20), "Interval end");
}

/// A retracted `Validity` (`validity(100, false)`, i.e. `is_assert =
/// false`) round-trips distinctly from an asserted one at the same
/// timestamp — the polarity is part of the value, not a side channel.
#[test]
fn validity_polarity_is_part_of_the_value() {
    let db = fresh_db();
    db.run_script(
        "?[id, val] <- [[1, validity(100, true)], [2, validity(100, false)]] \
         :create v {id => val: Validity}",
        no_params(),
    )
    .expect("create v");

    let out = db
        .run_script("?[id, val] := *v{id, val}", no_params())
        .expect("scan v");
    let asserted = DataValue::Validity(ValiditySlot::from_stored(ValidityTs::from_raw(100), true));
    let retracted = DataValue::Validity(ValiditySlot::from_stored(ValidityTs::from_raw(100), false));
    for r in out.rows() {
        let id = r[0].get_int().unwrap();
        if id == 1 {
            assert_eq!(r[1], asserted, "id 1 keeps its asserted polarity");
        } else {
            assert_eq!(r[1], retracted, "id 2 keeps its retracted polarity");
        }
    }
}

/// A `<F32; N>` vector round-trips exactly — checked through KyzoScript's
/// own equality operator (`Vector`'s Rust-side constructors aren't part
/// of the public surface, so this is the same check an external embedder
/// would write).
#[test]
fn vector_round_trips_through_a_stored_relation() {
    let db = fresh_db();
    db.run_script(
        "?[id, v] <- [[1, vec([1.0, 2.0, 3.0])]] :create vt {id => v: <F32; 3>}",
        no_params(),
    )
    .expect("create vt");

    let out = db
        .run_script(
            "?[same] := *vt{id: 1, v}, same = (v == vec([1.0, 2.0, 3.0]))",
            no_params(),
        )
        .expect("vector equality check");
    assert_eq!(
        out.rows()[0][0].get_bool(),
        Some(true),
        "vector round-trips byte-exact"
    );

    let out2 = db
        .run_script(
            "?[same] := *vt{id: 1, v}, same = (v == vec([9.0, 9.0, 9.0]))",
            no_params(),
        )
        .expect("vector inequality check");
    assert_eq!(
        out2.rows()[0][0].get_bool(),
        Some(false),
        "a different vector must not compare equal"
    );
}

/// Coercion-class regression guard (#119): an INTEGRAL float coerces into an
/// `Int` column, matching the pre-value-plane behavior. The rewrite had
/// dropped this (get_int became a pure representation read), silently
/// breaking `:create ... n: Int` fed a `3.0` literal — invisible to the old
/// corpus, which only inserted 3.5 into Float and 42 into Int.
#[test]
fn integral_float_coerces_into_an_int_column() {
    let db = fresh_db();
    db.run_script(
        "?[id, n] <- [[1, 3.0], [2, -7.0], [3, 42]] :create ic {id => n: Int}",
        no_params(),
    )
    .expect("integral floats coerce into an Int column");
    let out = db
        .run_script("?[id, n] := *ic{id, n} :order id", no_params())
        .expect("scan");
    let got: Vec<(i64, i64)> = out
        .rows()
        .iter()
        .map(|r| (r[0].get_int().unwrap(), r[1].get_int().unwrap()))
        .collect();
    assert_eq!(
        got,
        vec![(1, 3), (2, -7), (3, 42)],
        "integral floats stored as ints"
    );
    // A NON-integral float into an Int column is still refused.
    let err = db
        .run_script("?[id, n] <- [[9, 3.5]] :put ic {id => n: Int}", no_params())
        .expect_err("a non-integral float must not silently truncate into an Int column");
    assert!(
        format!("{err:?}").contains("coercion"),
        "non-integral float must be refused"
    );
}
