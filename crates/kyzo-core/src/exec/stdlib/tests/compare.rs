/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Re-homed domain tables from data/tests/functions.rs.
use crate::exec::stdlib::compare::*;
use crate::exec::stdlib::convert::*;
use crate::exec::stdlib::nondet::*;
use kyzo_model::value::DataValue;
use miette::{Result, miette};

#[test]
fn test_eq_neq() -> Result<()> {
    assert_eq!(
        op_eq(&[DataValue::from(1), DataValue::from(1.0)])?,
        DataValue::from(true)
    );
    assert_eq!(
        op_eq(&[DataValue::from(123), DataValue::from(123)])?,
        DataValue::from(true)
    );
    assert_eq!(
        op_neq(&[DataValue::from(1), DataValue::from(1.0)])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_neq(&[DataValue::from(123), DataValue::from(123.0)])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_eq(&[DataValue::from(123), DataValue::from(123.1)])?,
        DataValue::from(false)
    );
    Ok(())
}

#[test]
fn test_is_in() -> Result<()> {
    assert_eq!(
        op_is_in(&[
            DataValue::from(1),
            DataValue::List(vec![DataValue::from(1), DataValue::from(2)])
        ])?,
        DataValue::from(true)
    );
    assert_eq!(
        op_is_in(&[
            DataValue::from(3),
            DataValue::List(vec![DataValue::from(1), DataValue::from(2)])
        ])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_is_in(&[DataValue::from(3), DataValue::List(vec![])])?,
        DataValue::from(false)
    );
    Ok(())
}

#[test]
fn test_comparators() -> Result<()> {
    assert_eq!(
        op_ge(&[DataValue::from(2), DataValue::from(1)])?,
        DataValue::from(true)
    );
    assert_eq!(
        op_ge(&[DataValue::from(2.), DataValue::from(1)])?,
        DataValue::from(true)
    );
    assert_eq!(
        op_ge(&[DataValue::from(2), DataValue::from(1.)])?,
        DataValue::from(true)
    );

    assert_eq!(
        op_ge(&[DataValue::from(1), DataValue::from(1)])?,
        DataValue::from(true)
    );
    assert_eq!(
        op_ge(&[DataValue::from(1), DataValue::from(1.0)])?,
        DataValue::from(true)
    );
    assert_eq!(
        op_ge(&[DataValue::from(1), DataValue::from(2)])?,
        DataValue::from(false)
    );
    assert!(op_ge(&[DataValue::Null, DataValue::from(true)]).is_err());
    assert_eq!(
        op_gt(&[DataValue::from(2), DataValue::from(1)])?,
        DataValue::from(true)
    );
    assert_eq!(
        op_gt(&[DataValue::from(2.), DataValue::from(1)])?,
        DataValue::from(true)
    );
    assert_eq!(
        op_gt(&[DataValue::from(2), DataValue::from(1.)])?,
        DataValue::from(true)
    );
    assert_eq!(
        op_gt(&[DataValue::from(1), DataValue::from(1)])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_gt(&[DataValue::from(1), DataValue::from(1.0)])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_gt(&[DataValue::from(1), DataValue::from(2)])?,
        DataValue::from(false)
    );
    assert!(op_gt(&[DataValue::Null, DataValue::from(true)]).is_err());
    assert_eq!(
        op_le(&[DataValue::from(2), DataValue::from(1)])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_le(&[DataValue::from(2.), DataValue::from(1)])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_le(&[DataValue::from(2), DataValue::from(1.)])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_le(&[DataValue::from(1), DataValue::from(1)])?,
        DataValue::from(true)
    );
    assert_eq!(
        op_le(&[DataValue::from(1), DataValue::from(1.0)])?,
        DataValue::from(true)
    );
    assert_eq!(
        op_le(&[DataValue::from(1), DataValue::from(2)])?,
        DataValue::from(true)
    );
    assert!(op_le(&[DataValue::Null, DataValue::from(true)]).is_err());
    assert_eq!(
        op_lt(&[DataValue::from(2), DataValue::from(1)])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_lt(&[DataValue::from(2.), DataValue::from(1)])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_lt(&[DataValue::from(2), DataValue::from(1.)])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_lt(&[DataValue::from(1), DataValue::from(1)])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_lt(&[DataValue::from(1), DataValue::from(1.0)])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_lt(&[DataValue::from(1), DataValue::from(2)])?,
        DataValue::from(true)
    );
    assert!(op_lt(&[DataValue::Null, DataValue::from(true)]).is_err());
    Ok(())
}

#[test]
fn test_predicates() -> Result<()> {
    assert_eq!(op_is_null(&[DataValue::Null])?, DataValue::from(true));
    assert_eq!(op_is_int(&[DataValue::from(1)])?, DataValue::from(true));
    assert_eq!(op_is_int(&[DataValue::from(1.0)])?, DataValue::from(false));
    assert_eq!(op_is_float(&[DataValue::from(1)])?, DataValue::from(false));
    assert_eq!(op_is_float(&[DataValue::from(1.0)])?, DataValue::from(true));
    assert_eq!(op_is_num(&[DataValue::from(1)])?, DataValue::from(true));
    assert_eq!(op_is_num(&[DataValue::from(1.0)])?, DataValue::from(true));
    assert_eq!(op_is_num(&[DataValue::Null])?, DataValue::from(false));
    assert_eq!(
        op_is_bytes(&[DataValue::Bytes([0b1].into())])?,
        DataValue::from(true)
    );
    assert_eq!(op_is_bytes(&[DataValue::Null])?, DataValue::from(false));
    assert_eq!(
        op_is_list(&[DataValue::List(vec![])])?,
        DataValue::from(true)
    );
    assert_eq!(op_is_list(&[DataValue::Null])?, DataValue::from(false));
    assert_eq!(
        op_is_string(&[DataValue::Str("".into())])?,
        DataValue::from(true)
    );
    assert_eq!(op_is_string(&[DataValue::Null])?, DataValue::from(false));
    assert_eq!(
        op_is_finite(&[DataValue::from(1.0)])?,
        DataValue::from(true)
    );
    assert_eq!(
        op_is_finite(&[DataValue::from(f64::INFINITY)])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_is_finite(&[DataValue::from(f64::NAN)])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_is_infinite(&[DataValue::from(1.0)])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_is_infinite(&[DataValue::from(f64::INFINITY)])?,
        DataValue::from(true)
    );
    assert_eq!(
        op_is_infinite(&[DataValue::from(f64::NEG_INFINITY)])?,
        DataValue::from(true)
    );
    assert_eq!(
        op_is_infinite(&[DataValue::from(f64::NAN)])?,
        DataValue::from(false)
    );
    assert_eq!(op_is_nan(&[DataValue::from(1.0)])?, DataValue::from(false));
    assert_eq!(
        op_is_nan(&[DataValue::from(f64::INFINITY)])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_is_nan(&[DataValue::from(f64::NEG_INFINITY)])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_is_nan(&[DataValue::from(f64::NAN)])?,
        DataValue::from(true)
    );
    Ok(())
}

#[test]
fn test_uuid() -> Result<()> {
    let v1 = op_rand_uuid_v1(&[])?;
    let v4 = op_rand_uuid_v4(&[])?;
    assert!(
        op_is_uuid(&[v4])?
            .get_bool()
            .ok_or_else(|| miette!("get_bool"))?
    );
    assert!(op_uuid_timestamp(&[v1])?.get_float().is_some());
    assert!(op_to_uuid(&[DataValue::from("")]).is_err());
    assert!(op_to_uuid(&[DataValue::from("f3b4958c-52a1-11e7-802a-010203040506")]).is_ok());
    Ok(())
}
