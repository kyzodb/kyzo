/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Re-homed domain tables from data/tests/functions.rs.
use miette::{Result, miette};
use crate::exec::stdlib::convert::*;
use kyzo_model::data_value_any;
use kyzo_model::value::{DataValue, Vector};
use serde_json::json;


#[test]
fn test_encode_decode() -> Result<()>  {
    assert_eq!(
        op_decode_base64(&[op_encode_base64(&[DataValue::Bytes([1, 2, 3].into())])?])?,
        DataValue::Bytes([1, 2, 3].into())
    );
    Ok(())
}

#[test]
fn test_to_string() -> Result<()>  {
    assert_eq!(
        op_to_string(&[DataValue::from(false)])?,
        DataValue::Str("false".into())
    );
    Ok(())
}

#[test]
fn test_to_unity() -> Result<()>  {
    assert_eq!(op_to_unity(&[DataValue::Null])?, DataValue::from(0));
    assert_eq!(
        op_to_unity(&[DataValue::from(false)])?,
        DataValue::from(0)
    );
    assert_eq!(
        op_to_unity(&[DataValue::from(true)])?,
        DataValue::from(1)
    );
    assert_eq!(
        op_to_unity(&[DataValue::from(10)])?,
        DataValue::from(1)
    );
    assert_eq!(
        op_to_unity(&[DataValue::from(1.0)])?,
        DataValue::from(1)
    );
    assert_eq!(
        op_to_unity(&[DataValue::from(f64::NAN)])?,
        DataValue::from(1)
    );
    assert_eq!(
        op_to_unity(&[DataValue::Str("0".into())])?,
        DataValue::from(1)
    );
    assert_eq!(
        op_to_unity(&[DataValue::Str("".into())])?,
        DataValue::from(0)
    );
    assert_eq!(
        op_to_unity(&[DataValue::List(vec![])])?,
        DataValue::from(0)
    );
    assert_eq!(
        op_to_unity(&[DataValue::List(vec![DataValue::Null])])?,
        DataValue::from(1)
    );
    Ok(())
}

#[test]
fn test_to_float() -> Result<()>  {
    assert_eq!(
        op_to_float(&[DataValue::Null])?,
        DataValue::from(0.0)
    );
    assert_eq!(
        op_to_float(&[DataValue::from(false)])?,
        DataValue::from(0.0)
    );
    assert_eq!(
        op_to_float(&[DataValue::from(true)])?,
        DataValue::from(1.0)
    );
    assert_eq!(
        op_to_float(&[DataValue::from(1)])?,
        DataValue::from(1.0)
    );
    assert_eq!(
        op_to_float(&[DataValue::from(1.0)])?,
        DataValue::from(1.0)
    );
    assert!(
        op_to_float(&[DataValue::Str("NAN".into())])?
            .get_float().ok_or_else(|| miette!("float"))?
            .is_nan()
    );
    assert!(
        op_to_float(&[DataValue::Str("INF".into())])?
            .get_float().ok_or_else(|| miette!("float"))?
            .is_infinite()
    );
    assert!(
        op_to_float(&[DataValue::Str("NEG_INF".into())])?
            .get_float().ok_or_else(|| miette!("float"))?
            .is_infinite()
    );
    assert_eq!(
        op_to_float(&[DataValue::Str("3".into())])?
            .get_float().ok_or_else(|| miette!("float"))?,
        3.
    );
    Ok(())
}

#[test]
fn test_to_bool() -> Result<()>  {
    assert_eq!(
        op_to_bool(&[DataValue::Null])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from(true)])?,
        DataValue::from(true)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from(false)])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from(0)])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from(0.0)])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from(1)])?,
        DataValue::from(true)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from("")])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::from("a")])?,
        DataValue::from(true)
    );
    assert_eq!(
        op_to_bool(&[DataValue::List(vec![])])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_to_bool(&[DataValue::List(vec![DataValue::from(0)])])?,
        DataValue::from(true)
    );
    Ok(())
}

// The upstream `test_range` ran `int_range` through a `DbInstance`; the op
// is exercised directly here.
#[test]
fn test_vec_rejects_trailing_bytes() -> Result<()>  {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    // 5 bytes: one whole f32 plus one trailing byte.
    let b64 = STANDARD.encode([0u8, 0, 128, 63, 7]);
    assert!(op_vec(&[DataValue::Str(b64)]).is_err());
    // 4 bytes decode cleanly to one f32 (1.0, little-endian).
    let ok = STANDARD.encode([0u8, 0, 128, 63]);
    match op_vec(&[DataValue::Str(ok)])? {
        DataValue::Vector(v) => assert_eq!(v.len(), 1),
        other @ (data_value_any!()) => {
            return Err(miette!("expected vector, got {other:?}"));
        }
    }
    // The F64 path is equally strict: 9 bytes is one f64 plus trailing.
    let bad64 = STANDARD.encode([0u8; 9]);
    assert!(op_vec(&[DataValue::Str(bad64), DataValue::Str("F64".into())]).is_err());
    let ok64 = STANDARD.encode([0u8; 8]);
    match op_vec(&[DataValue::Str(ok64), DataValue::Str("F64".into())])? {
        DataValue::Vector(v) => assert_eq!(v.len(), 1),
        other @ (data_value_any!()) => {
            return Err(miette!("expected vector, got {other:?}"));
        }
    }
    Ok(())
}

// `vec` on non-array JSON is an error: the upstream original unwrapped
// `as_array()` and aborted on e.g. `vec(json('{}'))`.
#[test]
fn test_vec_rejects_non_array_json() -> Result<()>  {
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
        )))])?,
        DataValue::Vector(Vector::try_new(vec![1.0f64, 2.0]).ok_or_else(|| miette!("vector"))?)
    );
    Ok(())
}

// A negative JSON array index is an error: the upstream original cast
// `i64 as usize`, turning `-1` into a huge index (an OOM-scale
// `resize_with` on the write path).
