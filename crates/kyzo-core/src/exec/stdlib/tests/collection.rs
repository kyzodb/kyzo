/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Re-homed domain tables from data/tests/functions.rs.
use crate::exec::stdlib::collection::*;
use crate::exec::stdlib::temporal_format::*;
use kyzo_model::data_value_any;
use kyzo_model::schema::{ColType, NullableColType};
use kyzo_model::str2vld;
use kyzo_model::value::{DataValue, ValidityTs};
use serde_json::json;

#[allow(dead_code)] // mid-wiring / test-only surface
fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-5
}

#[test]
fn test_list() {
    assert_eq!(op_list(&[]).unwrap(), DataValue::List(vec![]));
    assert_eq!(
        op_list(&[DataValue::from(1)]).unwrap(),
        DataValue::List(vec![DataValue::from(1)])
    );
    assert_eq!(
        op_list(&[DataValue::from(1), DataValue::List(vec![])]).unwrap(),
        DataValue::List(vec![DataValue::from(1), DataValue::List(vec![])])
    );
}

#[test]
fn test_concat() {
    assert_eq!(
        op_concat(&[DataValue::Str("abc".into()), DataValue::Str("def".into())]).unwrap(),
        DataValue::Str("abcdef".into())
    );

    assert_eq!(
        op_concat(&[
            DataValue::List(vec![DataValue::from(true), DataValue::from(false)]),
            DataValue::List(vec![DataValue::from(true)])
        ])
        .unwrap(),
        DataValue::List(vec![
            DataValue::from(true),
            DataValue::from(false),
            DataValue::from(true),
        ])
    );
}

#[test]
fn test_prepend_append() {
    assert_eq!(
        op_prepend(&[
            DataValue::List(vec![DataValue::from(1), DataValue::from(2)]),
            DataValue::Null,
        ])
        .unwrap(),
        DataValue::List(vec![
            DataValue::Null,
            DataValue::from(1),
            DataValue::from(2),
        ]),
    );
    assert_eq!(
        op_append(&[
            DataValue::List(vec![DataValue::from(1), DataValue::from(2)]),
            DataValue::Null,
        ])
        .unwrap(),
        DataValue::List(vec![
            DataValue::from(1),
            DataValue::from(2),
            DataValue::Null,
        ]),
    );
}

#[test]
fn test_length() {
    assert_eq!(
        op_length(&[DataValue::Str("abc".into())]).unwrap(),
        DataValue::from(3)
    );
    assert_eq!(
        op_length(&[DataValue::List(vec![])]).unwrap(),
        DataValue::from(0)
    );
    assert_eq!(
        op_length(&[DataValue::Bytes([].into())]).unwrap(),
        DataValue::from(0)
    );
}

#[test]
fn test_sort_reverse() {
    assert_eq!(
        op_sorted(&[DataValue::List(vec![
            DataValue::from(2.0),
            DataValue::from(1),
            DataValue::from(2),
            DataValue::Null,
        ])])
        .unwrap(),
        DataValue::List(vec![
            DataValue::Null,
            DataValue::from(1),
            DataValue::from(2),
            DataValue::from(2.0),
        ])
    );
    assert_eq!(
        op_reverse(&[DataValue::List(vec![
            DataValue::from(2.0),
            DataValue::from(1),
            DataValue::from(2),
            DataValue::Null,
        ])])
        .unwrap(),
        DataValue::List(vec![
            DataValue::Null,
            DataValue::from(2),
            DataValue::from(1),
            DataValue::from(2.0),
        ])
    )
}

#[test]
fn test_first_last() {
    assert_eq!(
        op_first(&[DataValue::List(vec![])]).unwrap(),
        DataValue::Null,
    );
    assert_eq!(
        op_last(&[DataValue::List(vec![])]).unwrap(),
        DataValue::Null,
    );
    assert_eq!(
        op_first(&[DataValue::List(vec![
            DataValue::from(1),
            DataValue::from(2),
        ])])
        .unwrap(),
        DataValue::from(1),
    );
    assert_eq!(
        op_last(&[DataValue::List(vec![
            DataValue::from(1),
            DataValue::from(2),
        ])])
        .unwrap(),
        DataValue::from(2),
    );
}

#[test]
fn test_chunks() {
    assert_eq!(
        op_chunks(&[
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
                DataValue::from(4),
                DataValue::from(5),
            ]),
            DataValue::from(2),
        ])
        .unwrap(),
        DataValue::List(vec![
            DataValue::List(vec![DataValue::from(1), DataValue::from(2)]),
            DataValue::List(vec![DataValue::from(3), DataValue::from(4)]),
            DataValue::List(vec![DataValue::from(5)]),
        ])
    );
    assert_eq!(
        op_chunks_exact(&[
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
                DataValue::from(4),
                DataValue::from(5),
            ]),
            DataValue::from(2),
        ])
        .unwrap(),
        DataValue::List(vec![
            DataValue::List(vec![DataValue::from(1), DataValue::from(2)]),
            DataValue::List(vec![DataValue::from(3), DataValue::from(4)]),
        ])
    );
    assert_eq!(
        op_windows(&[
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
                DataValue::from(4),
                DataValue::from(5),
            ]),
            DataValue::from(3),
        ])
        .unwrap(),
        DataValue::List(vec![
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
            ]),
            DataValue::List(vec![
                DataValue::from(2),
                DataValue::from(3),
                DataValue::from(4),
            ]),
            DataValue::List(vec![
                DataValue::from(3),
                DataValue::from(4),
                DataValue::from(5),
            ]),
        ])
    )
}

#[test]
fn test_get() {
    assert!(op_get(&[DataValue::List(vec![]), DataValue::from(0)]).is_err());
    assert_eq!(
        op_get(&[
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
            ]),
            DataValue::from(1)
        ])
        .unwrap(),
        DataValue::from(2)
    );
    assert_eq!(
        op_maybe_get(&[DataValue::List(vec![]), DataValue::from(0)]).unwrap(),
        DataValue::Null
    );
    assert_eq!(
        op_maybe_get(&[
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
            ]),
            DataValue::from(1)
        ])
        .unwrap(),
        DataValue::from(2)
    );
}

#[test]
fn test_slice() {
    assert!(
        op_slice(&[
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
            ]),
            DataValue::from(1),
            DataValue::from(4)
        ])
        .is_err()
    );

    assert!(
        op_slice(&[
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
            ]),
            DataValue::from(1),
            DataValue::from(3)
        ])
        .is_ok()
    );

    assert_eq!(
        op_slice(&[
            DataValue::List(vec![
                DataValue::from(1),
                DataValue::from(2),
                DataValue::from(3),
            ]),
            DataValue::from(1),
            DataValue::from(-1)
        ])
        .unwrap(),
        DataValue::List(vec![DataValue::from(2)])
    );
}

#[test]
fn test_set_ops() {
    assert_eq!(
        op_union(&[
            DataValue::List([1, 2, 3].into_iter().map(DataValue::from).collect()),
            DataValue::List([2, 3, 4].into_iter().map(DataValue::from).collect()),
            DataValue::List([3, 4, 5].into_iter().map(DataValue::from).collect())
        ])
        .unwrap(),
        DataValue::List([1, 2, 3, 4, 5].into_iter().map(DataValue::from).collect())
    );
    assert_eq!(
        op_intersection(&[
            DataValue::List(
                [1, 2, 3, 4, 5, 6]
                    .into_iter()
                    .map(DataValue::from)
                    .collect(),
            ),
            DataValue::List([2, 3, 4].into_iter().map(DataValue::from).collect()),
            DataValue::List([3, 4, 5].into_iter().map(DataValue::from).collect())
        ])
        .unwrap(),
        DataValue::List([3, 4].into_iter().map(DataValue::from).collect())
    );
    assert_eq!(
        op_difference(&[
            DataValue::List(
                [1, 2, 3, 4, 5, 6]
                    .into_iter()
                    .map(DataValue::from)
                    .collect(),
            ),
            DataValue::List([2, 3, 4].into_iter().map(DataValue::from).collect()),
            DataValue::List([3, 4, 5].into_iter().map(DataValue::from).collect())
        ])
        .unwrap(),
        DataValue::List([1, 6].into_iter().map(DataValue::from).collect())
    );
}

#[test]
fn test_range() {
    assert_eq!(
        op_int_range(&[DataValue::from(1), DataValue::from(5)]).unwrap(),
        DataValue::List([1, 2, 3, 4].into_iter().map(DataValue::from).collect())
    );
    assert_eq!(
        op_int_range(&[DataValue::from(5)]).unwrap(),
        DataValue::List([0, 1, 2, 3, 4].into_iter().map(DataValue::from).collect())
    );
    assert_eq!(
        op_int_range(&[DataValue::from(15), DataValue::from(3), DataValue::from(-2)]).unwrap(),
        DataValue::List(
            [15, 13, 11, 9, 7, 5]
                .into_iter()
                .map(DataValue::from)
                .collect()
        )
    );
}

// Base64 vector payloads must be a whole number of elements: trailing
// bytes are an error, the same exact-length law as schema coercion (the
// upstream original silently ignored them).
#[test]
fn test_json_path_negative_index_errors() {
    let arr = DataValue::Json(crate::data::json::json_from_serde(&json!([1, 2, 3])));
    let neg_path = DataValue::List(vec![DataValue::from(-1)]);
    assert!(op_set_json_path(&[arr.clone(), neg_path.clone(), DataValue::from(9)]).is_err());
    assert!(op_remove_json_path(&[arr.clone(), neg_path.clone()]).is_err());
    assert!(op_get(&[arr.clone(), neg_path]).is_err());
    // Positive control: a valid index works.
    assert_eq!(
        op_get(&[arr, DataValue::List(vec![DataValue::from(1)])]).unwrap(),
        DataValue::from(2)
    );
}

// ===========================================================================
// Date/time behavior-pinning fixtures for the chrono -> jiff migration.
//
// Every expected value below was first captured from the chrono 0.4 /
// chrono-tz 0.10 baseline and then re-verified byte-for-byte against jiff 0.2.
// The `*_agreed` tests are inputs where chrono and jiff produce identical
// results: they pin the preserved public behavior. The `datetime_deltas_vs_chrono`
// test pins the exact inputs where jiff and chrono diverge; each case asserts
// jiff's current output and records chrono's former output in a comment. See
// jiff-migration-report.md for the taxonomy.
// ===========================================================================

// `op_format_timestamp` -> RFC3339 string; helper returns Ok(String)/Err(msg).
fn fmt(args: &[DataValue]) -> Result<String, String> {
    op_format_timestamp(args)
        .map(|v| v.get_str().unwrap().to_string())
        .map_err(|e| e.to_string())
}
fn fmt_n(n: f64) -> Result<String, String> {
    fmt(&[DataValue::from(n)])
}
// `op_parse_timestamp` -> float seconds.
fn parse_secs(s: &str) -> Result<f64, String> {
    op_parse_timestamp(&[DataValue::from(s)])
        .map(|v| v.get_float().unwrap())
        .map_err(|e| e.to_string())
}
// `str2vld` -> signed microseconds.
fn vld_micros(s: &str) -> Result<i64, String> {
    str2vld(s).map(|v| v.raw()).map_err(|e| e.to_string())
}
// Validity coercion path (`data/relation.rs`) -> (micros, is_assert).
fn coerce_vld(s: &str) -> Result<(i64, bool), String> {
    let typing = NullableColType::required(ColType::Validity);
    typing
        .coerce(DataValue::Str(s.into()), ValidityTs::from_raw(999))
        .map(|v| match v {
            DataValue::Validity(vld) => (vld.timestamp().raw(), vld.is_assert()),
            other @ (data_value_any!()) => panic!("expected Validity, got {other:?}"),
        })
        .map_err(|e| e.to_string())
}

#[test]
fn format_timestamp_numeric_agreed() {
    // Whole seconds: no fractional digits (chrono SecondsFormat::AutoSi).
    assert_eq!(fmt_n(0.0).unwrap(), "1970-01-01T00:00:00+00:00");
    assert_eq!(fmt_n(1.0).unwrap(), "1970-01-01T00:00:01+00:00");
    assert_eq!(fmt_n(-1.0).unwrap(), "1969-12-31T23:59:59+00:00");
    // UTC renders as `+00:00`, never `Z` (matches chrono's DateTime::to_rfc3339).
    assert_eq!(fmt_n(1600000000.0).unwrap(), "2020-09-13T12:26:40+00:00");
    // Millisecond subsecond -> exactly 3 digits.
    assert_eq!(fmt_n(1.5).unwrap(), "1970-01-01T00:00:01.500+00:00");
    assert_eq!(fmt_n(-1.5).unwrap(), "1969-12-31T23:59:58.500+00:00");
    assert_eq!(
        fmt_n(1600000000.5).unwrap(),
        "2020-09-13T12:26:40.500+00:00"
    );
    assert_eq!(fmt_n(0.123).unwrap(), "1970-01-01T00:00:00.123+00:00");
    assert_eq!(fmt_n(0.001).unwrap(), "1970-01-01T00:00:00.001+00:00");
    // Sub-millisecond input is truncated to whole seconds by the `* 1000` path.
    assert_eq!(fmt_n(0.0005).unwrap(), "1970-01-01T00:00:00+00:00");
    // Fractional beyond milliseconds is dropped (the op keeps only millis).
    assert_eq!(
        fmt_n(1234567890.123456).unwrap(),
        "2009-02-13T23:31:30.123+00:00"
    );
    // Pre-epoch and small positive years (0000..=9999) print 4-digit years.
    assert_eq!(fmt_n(-2208988800.0).unwrap(), "1900-01-01T00:00:00+00:00");
    assert_eq!(fmt_n(-30610224000.0).unwrap(), "1000-01-01T00:00:00+00:00");
    assert_eq!(fmt_n(-46394772000.0).unwrap(), "0499-10-22T11:20:00+00:00");
    assert_eq!(fmt_n(-62135596800.0).unwrap(), "0001-01-01T00:00:00+00:00");
    assert_eq!(fmt_n(-62162035200.0).unwrap(), "0000-03-01T00:00:00+00:00");
}

#[test]
fn format_timestamp_validity_input_agreed() {
    use kyzo_model::value::Validity;
    let f = |micros: i64| {
        let vld = DataValue::Validity(
            Validity::new(ValidityTs::from_raw(micros), true)
                .expect("non-reserved")
                .into(),
        );
        fmt(&[vld]).unwrap()
    };
    // Validity stores microseconds; the op divides by 1000 to milliseconds.
    assert_eq!(f(0), "1970-01-01T00:00:00+00:00");
    assert_eq!(f(1_500_000), "1970-01-01T00:00:01.500+00:00");
    assert_eq!(f(-1_500_000), "1969-12-31T23:59:58.500+00:00");
    assert_eq!(f(1_600_000_000_000_000), "2020-09-13T12:26:40+00:00");
}

#[test]
fn format_timestamp_timezone_agreed() {
    let z = |n: f64, tz: &str| fmt(&[DataValue::from(n), DataValue::from(tz)]);
    // Named zones apply the offset in effect AT that instant (incl. DST).
    assert_eq!(z(0.0, "UTC").unwrap(), "1970-01-01T00:00:00+00:00");
    assert_eq!(
        z(0.0, "Asia/Shanghai").unwrap(),
        "1970-01-01T08:00:00+08:00"
    );
    assert_eq!(
        z(0.0, "America/New_York").unwrap(),
        "1969-12-31T19:00:00-05:00"
    );
    assert_eq!(
        z(0.0, "Europe/London").unwrap(),
        "1970-01-01T01:00:00+01:00"
    );
    assert_eq!(z(0.0, "Asia/Kolkata").unwrap(), "1970-01-01T05:30:00+05:30");
    // September 2020: New York is on EDT (-04:00), not EST.
    assert_eq!(
        z(1600000000.0, "America/New_York").unwrap(),
        "2020-09-13T08:26:40-04:00"
    );
    assert_eq!(
        z(1600000000.5, "Asia/Kolkata").unwrap(),
        "2020-09-13T17:56:40.500+05:30"
    );
    // Unknown / empty zone is rejected with the same message chrono produced.
    assert_eq!(
        z(0.0, "bogus/zone").unwrap_err(),
        "bad timezone specification: bogus/zone"
    );
    assert_eq!(z(0.0, "").unwrap_err(), "bad timezone specification: ");
}

#[test]
fn parse_timestamp_agreed() {
    assert_eq!(parse_secs("1970-01-01T00:00:00Z").unwrap(), 0.0);
    assert_eq!(parse_secs("2020-01-01T00:00:00Z").unwrap(), 1577836800.0);
    assert_eq!(parse_secs("2020-01-01T00:00:00.5Z").unwrap(), 1577836800.5);
    assert_eq!(
        parse_secs("2020-01-01T00:00:00+08:00").unwrap(),
        1577808000.0
    );
    assert_eq!(
        parse_secs("2020-01-01T00:00:00-05:00").unwrap(),
        1577854800.0
    );
    assert_eq!(
        parse_secs("2020-01-01T00:00:00-00:00").unwrap(),
        1577836800.0
    );
    // Pre-epoch parses to a negative count, not a panic (pinned regression).
    assert_eq!(parse_secs("1969-07-20T20:17:00Z").unwrap(), -14182980.0);
    assert_eq!(parse_secs("1969-12-31T23:59:59.5Z").unwrap(), -0.5);
    assert_eq!(parse_secs("1900-01-01T00:00:00Z").unwrap(), -2208988800.0);
    assert_eq!(parse_secs("0000-01-01T00:00:00Z").unwrap(), -62167219200.0);
    // RFC3339 leniencies both libraries share: space separator, lowercase t/z.
    assert_eq!(parse_secs("2020-01-01 00:00:00Z").unwrap(), 1577836800.0);
    assert_eq!(parse_secs("2020-01-01t00:00:00z").unwrap(), 1577836800.0);
    // Rejections shared with chrono.
    for bad in [
        "garbage",
        "",
        "2020-01-01",
        "2020-01-01T00:00:00",
        "2020-13-01T00:00:00Z",
        "2020-02-30T00:00:00Z",
        "2020-01-01T00:00:00.Z",
    ] {
        assert!(parse_secs(bad).is_err(), "expected {bad:?} to be rejected");
    }
}

/// `op_parse_timestamp` returns lossy f64 seconds. For sub-microsecond pre-epoch
/// instants the two-term sum can land on a non-obvious double; jiff reproduces
/// chrono's exact bit pattern because both floor the whole second and add a
/// non-negative subsecond fraction. Pins that bit-identity.
#[test]
fn parse_timestamp_sub_microsecond_matches_chrono() {
    assert_eq!(
        parse_secs("1969-12-31T23:59:59.123456789Z").unwrap(),
        -0.876543211
    );
    assert_eq!(
        parse_secs("1970-01-01T00:00:00.123456789Z").unwrap(),
        0.123456789
    );
    assert_eq!(
        parse_secs("1970-01-01T00:00:00.0000005Z").unwrap(),
        0.0000005
    );
    // The ULP-noisy pair chrono produced (floored second -1 + 0.9999995):
    assert_eq!(
        parse_secs("1969-12-31T23:59:59.9999995Z").unwrap(),
        -0.0000004999999999588667
    );
    assert_eq!(
        parse_secs("1969-12-31T23:59:59.999999Z").unwrap(),
        -0.0000010000000000287557
    );
}

#[test]
fn str2vld_agreed() {
    assert_eq!(vld_micros("1970-01-01T00:00:00Z").unwrap(), 0);
    assert_eq!(
        vld_micros("2020-01-01T00:00:00Z").unwrap(),
        1577836800000000
    );
    assert_eq!(
        vld_micros("2020-01-01T00:00:00.123456Z").unwrap(),
        1577836800123456
    );
    assert_eq!(
        vld_micros("2020-01-01T00:00:00+08:00").unwrap(),
        1577808000000000
    );
    // Pre-epoch validity is a negative microsecond count.
    assert_eq!(vld_micros("1969-07-20T20:17:00Z").unwrap(), -14182980000000);
    assert_eq!(vld_micros("1969-12-31T23:59:59.5Z").unwrap(), -500000);
    assert_eq!(
        vld_micros("1900-01-01T00:00:00Z").unwrap(),
        -2208988800000000
    );
    assert_eq!(
        vld_micros("0000-01-01T00:00:00Z").unwrap(),
        -62167219200000000
    );
    for bad in ["garbage", "", "2020-01-01", "2020-13-01T00:00:00Z"] {
        assert!(vld_micros(bad).is_err(), "expected {bad:?} to be rejected");
    }
}

/// Microsecond timestamps floor toward negative infinity (the microsecond that
/// *contains* the instant), UNIFORMLY on both sides of the epoch, on both the
/// `str2vld` and validity-coercion paths. chrono floored; jiff's `as_microsecond`
/// truncates toward zero, so this pins the shared `timestamp_to_micros` helper
/// that restores the floor. Sub-microsecond finer than a µs is dropped downward.
#[test]
fn validity_micros_floor_agreed_pre_and_post_epoch() {
    // (input, floored micros) — chrono baseline; jiff matches after the fix.
    let cases: &[(&str, i64)] = &[
        // Post-epoch: floor == truncate, unaffected by the fix.
        ("1970-01-01T00:00:00.123456789Z", 123456),
        ("1970-01-01T00:00:00.0000005Z", 0),
        ("2020-01-01T00:00:00.123456Z", 1577836800123456),
        // Pre-epoch sub-microsecond: floor differs from truncate by 1 µs.
        ("1969-12-31T23:59:59.123456789Z", -876544),
        ("1969-12-31T23:59:59.9999995Z", -1),
        ("1969-12-31T23:59:59.999999Z", -1),
        ("1969-12-31T23:59:59.000001Z", -999999),
    ];
    for &(s, micros) in cases {
        assert_eq!(vld_micros(s).unwrap(), micros, "str2vld({s:?})");
        assert_eq!(coerce_vld(s).unwrap(), (micros, true), "coerce({s:?})");
    }
}

#[test]
fn validity_coerce_agreed() {
    assert_eq!(coerce_vld("ASSERT").unwrap(), (999, true));
    assert_eq!(coerce_vld("RETRACT").unwrap(), (999, false));
    assert_eq!(
        coerce_vld("2020-01-01T00:00:00Z").unwrap(),
        (1577836800000000, true)
    );
    assert_eq!(
        coerce_vld("~2020-01-01T00:00:00Z").unwrap(),
        (1577836800000000, false)
    );
    assert_eq!(
        coerce_vld("2020-01-01T00:00:00.123456Z").unwrap(),
        (1577836800123456, true)
    );
    assert_eq!(
        coerce_vld("2020-01-01T00:00:00+08:00").unwrap(),
        (1577808000000000, true)
    );
    // Pre-epoch, asserted and retracted.
    assert_eq!(
        coerce_vld("1969-07-20T20:17:00Z").unwrap(),
        (-14182980000000, true)
    );
    assert_eq!(
        coerce_vld("~1969-07-20T20:17:00Z").unwrap(),
        (-14182980000000, false)
    );
    for bad in ["garbage", "~garbage", ""] {
        assert!(coerce_vld(bad).is_err(), "expected {bad:?} to be rejected");
    }
}

/// Pins every input where jiff diverges from the chrono baseline. Each assert
/// locks jiff's CURRENT behavior; the trailing comment is chrono's old result.
/// If a future jiff upgrade shifts any of these, this test fails loudly rather
/// than letting a semantics change land silently.
#[test]
fn datetime_deltas_vs_chrono() {
    // -- Delta 1: leap-second ":60". chrono folds :60 UP to the next second;
    //    jiff clamps it DOWN to :59. A ":60" timestamp parses 1 second earlier.
    assert_eq!(parse_secs("2016-12-31T23:59:60Z").unwrap(), 1483228799.0); // chrono: 1483228800.0
    assert_eq!(parse_secs("2020-01-01T00:00:60Z").unwrap(), 1577836859.0); // chrono: 1577836860.0
    assert_eq!(
        vld_micros("2016-12-31T23:59:60Z").unwrap(),
        1483228799000000
    ); // chrono: 1483228800000000

    // -- Delta 2: representable range. jiff's Timestamp spans
    //    -009999-01-02T01:59:59Z ..= 9999-12-30T22:00:00.999999999Z (it reserves
    //    ~26h at each end so any instant maps into any tz without overflow).
    //    chrono reached year +-262143. Inputs at/above the RFC3339 max now error.
    assert!(fmt_n(253402300799.0).is_err()); // chrono OK: "9999-12-31T23:59:59+00:00"
    assert!(fmt_n(253402300800.0).is_err()); // chrono OK: "+10000-01-01T00:00:00+00:00" (non-RFC3339)
    assert!(fmt_n(1.5e12).is_err()); // chrono OK: "+49503-02-10T02:40:00+00:00" (non-RFC3339)
    assert!(fmt_n(-4.0e11).is_err()); // chrono OK: "-10706-07-03T08:53:20+00:00"
    assert!(parse_secs("9999-12-31T23:59:59Z").is_err()); // chrono OK: 253402300799.0

    // -- Delta 3: expanded-year FORMAT for in-range NEGATIVE years. jiff emits a
    //    signed 6-digit year; chrono emitted a signed minimal-width year. Only
    //    affects years < 0 (positive years 0000..=9999 are byte-identical).
    assert_eq!(
        fmt_n(-250000000000.0).unwrap(),
        "-005953-10-25T11:33:20+00:00"
    ); // chrono: "-5953-10-25T11:33:20+00:00"

    // -- Delta 4: jiff ACCEPTS parse forms chrono rejected (Temporal/RFC9557).
    //    Each was `bad datetime` under chrono; jiff yields the instant below.
    assert_eq!(parse_secs("+002020-01-01T00:00:00Z").unwrap(), 1577836800.0); // expanded-year sign
    assert_eq!(
        parse_secs("2020-01-01T00:00:00+0000").unwrap(),
        1577836800.0
    ); // offset without colon
    assert_eq!(
        parse_secs("2020-01-01T00:00:00Z[UTC]").unwrap(),
        1577836800.0
    ); // RFC9557 zone annotation
    assert_eq!(parse_secs("2020-01-01T00:00:00,5Z").unwrap(), 1577836800.5); // comma decimal separator
    assert_eq!(
        parse_secs("2020-01-01T00:00:00+05:30:30").unwrap(),
        1577816970.0
    ); // offset with seconds
    assert_eq!(parse_secs("2020-01-01T00:00Z").unwrap(), 1577836800.0); // seconds field omitted
    assert_eq!(
        parse_secs("2020-01-01T00:00:00+24:00").unwrap(),
        1577750400.0
    ); // out-of-range (>23:59) offset
    // The widening reaches the validity paths too.
    assert_eq!(
        vld_micros("2020-01-01T00:00:00,5Z").unwrap(),
        1577836800500000
    ); // chrono: ERR
    assert_eq!(
        coerce_vld("2020-01-01T00:00:00+05:30:30").unwrap(),
        (1577816970000000, true)
    ); // chrono: ERR

    // -- Delta 5: jiff REJECTS > 9 fractional digits; chrono truncated silently.
    assert!(parse_secs("2020-01-01T00:00:00.1234567890123Z").is_err()); // chrono OK: 1577836800.1234567

    // NOTE: the microsecond floor difference (chrono floors, jiff's
    // `as_microsecond` truncates) is NOT listed here: it is fixed in
    // `timestamp_to_micros` and pinned as agreed behavior in
    // `validity_micros_floor_agreed_pre_and_post_epoch`.
}
