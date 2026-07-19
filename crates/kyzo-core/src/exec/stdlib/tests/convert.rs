//! Re-homed domain tables from data/tests/functions.rs.
use crate::exec::stdlib::convert::*;
use kyzo_model::data_value_any;
use kyzo_model::value::{DataValue, Vector};
use serde_json::json;

#[allow(dead_code)] // mid-wiring / test-only surface
fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-5
}

#[test]
fn test_encode_decode() {
    assert_eq!(
        op_decode_base64(&[op_encode_base64(&[DataValue::Bytes([1, 2, 3].into())]).unwrap()])
            .unwrap(),
        DataValue::Bytes([1, 2, 3].into())
    )
}

#[test]
fn test_to_string() {
    assert_eq!(
        op_to_string(&[DataValue::from(false)]).unwrap(),
        DataValue::Str("false".into())
    );
}

#[test]
fn test_to_unity() {
    assert_eq!(op_to_unity(&[DataValue::Null]).unwrap(), DataValue::from(0));
    assert_eq!(
        op_to_unity(&[DataValue::from(false)]).unwrap(),
        DataValue::from(0)
    );
    assert_eq!(
        op_to_unity(&[DataValue::from(true)]).unwrap(),
        DataValue::from(1)
    );
    assert_eq!(
        op_to_unity(&[DataValue::from(10)]).unwrap(),
        DataValue::from(1)
    );
    assert_eq!(
        op_to_unity(&[DataValue::from(1.0)]).unwrap(),
        DataValue::from(1)
    );
    assert_eq!(
        op_to_unity(&[DataValue::from(f64::NAN)]).unwrap(),
        DataValue::from(1)
    );
    assert_eq!(
        op_to_unity(&[DataValue::Str("0".into())]).unwrap(),
        DataValue::from(1)
    );
    assert_eq!(
        op_to_unity(&[DataValue::Str("".into())]).unwrap(),
        DataValue::from(0)
    );
    assert_eq!(
        op_to_unity(&[DataValue::List(vec![])]).unwrap(),
        DataValue::from(0)
    );
    assert_eq!(
        op_to_unity(&[DataValue::List(vec![DataValue::Null])]).unwrap(),
        DataValue::from(1)
    );
}

#[test]
fn test_to_float() {
    assert_eq!(
        op_to_float(&[DataValue::Null]).unwrap(),
        DataValue::from(0.0)
    );
    assert_eq!(
        op_to_float(&[DataValue::from(false)]).unwrap(),
        DataValue::from(0.0)
    );
    assert_eq!(
        op_to_float(&[DataValue::from(true)]).unwrap(),
        DataValue::from(1.0)
    );
    assert_eq!(
        op_to_float(&[DataValue::from(1)]).unwrap(),
        DataValue::from(1.0)
    );
    assert_eq!(
        op_to_float(&[DataValue::from(1.0)]).unwrap(),
        DataValue::from(1.0)
    );
    assert!(
        op_to_float(&[DataValue::Str("NAN".into())])
            .unwrap()
            .get_float()
            .unwrap()
            .is_nan()
    );
    assert!(
        op_to_float(&[DataValue::Str("INF".into())])
            .unwrap()
            .get_float()
            .unwrap()
            .is_infinite()
    );
    assert!(
        op_to_float(&[DataValue::Str("NEG_INF".into())])
            .unwrap()
            .get_float()
            .unwrap()
            .is_infinite()
    );
    assert_eq!(
        op_to_float(&[DataValue::Str("3".into())])
            .unwrap()
            .get_float()
            .unwrap(),
        3.
    );
}

#[test]
fn test_to_bool() {
    assert_eq!(
        op_to_bool(&[DataValue::Null]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from(true)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from(false)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from(0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from(0.0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from(1)]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from("")]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from("a")]).unwrap(),
        DataValue::from(true)
    );
    assert_eq!(
        op_to_bool(&[DataValue::List(vec![])]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::List(vec![DataValue::from(0)])]).unwrap(),
        DataValue::from(true)
    );
}

// The upstream `test_range` ran `int_range` through a `DbInstance`; the op
// is exercised directly here.
#[test]
fn test_vec_rejects_trailing_bytes() {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    // 5 bytes: one whole f32 plus one trailing byte.
    let b64 = STANDARD.encode([0u8, 0, 128, 63, 7]);
    assert!(op_vec(&[DataValue::Str(b64)]).is_err());
    // 4 bytes decode cleanly to one f32 (1.0, little-endian).
    let ok = STANDARD.encode([0u8, 0, 128, 63]);
    match op_vec(&[DataValue::Str(ok)]).unwrap() {
        DataValue::Vector(v) => assert_eq!(v.len(), 1),
        other @ (data_value_any!()) => panic!("expected vector, got {other:?}"),
    }
    // The F64 path is equally strict: 9 bytes is one f64 plus trailing.
    let bad64 = STANDARD.encode([0u8; 9]);
    assert!(op_vec(&[DataValue::Str(bad64), DataValue::Str("F64".into())]).is_err());
    let ok64 = STANDARD.encode([0u8; 8]);
    match op_vec(&[DataValue::Str(ok64), DataValue::Str("F64".into())]).unwrap() {
        DataValue::Vector(v) => assert_eq!(v.len(), 1),
        other @ (data_value_any!()) => panic!("expected vector, got {other:?}"),
    }
}

// `vec` on non-array JSON is an error: the upstream original unwrapped
// `as_array()` and aborted on e.g. `vec(json('{}'))`.
#[test]
fn test_vec_rejects_non_array_json() {
    assert!(
        op_vec(&[DataValue::Json(crate::data::json::json_from_serde(
            &json!({"a": 1})
        ))])
        .is_err()
    );
    assert!(
        op_vec(&[DataValue::Json(crate::data::json::json_from_serde(&json!(
            1
        )))])
        .is_err()
    );
    assert!(
        op_vec(&[DataValue::Json(crate::data::json::json_from_serde(&json!(
            "x"
        )))])
        .is_err()
    );
    // Positive control: a JSON array of numbers converts.
    assert_eq!(
        op_vec(&[DataValue::Json(crate::data::json::json_from_serde(&json!(
            [1.0, 2.0]
        )))])
        .unwrap(),
        DataValue::Vector(Vector::try_new(vec![1.0f64, 2.0]).unwrap())
    );
}

// A negative JSON array index is an error: the upstream original cast
// `i64 as usize`, turning `-1` into a huge index (an OOM-scale
// `resize_with` on the write path).
