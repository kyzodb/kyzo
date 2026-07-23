/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Re-homed domain tables from data/tests/functions.rs.
use crate::exec::stdlib::nondet::*;
use crate::exec::stdlib::temporal_format::*;
use kyzo_model::value::DataValue;
use miette::{Result, miette};

#[test]
fn test_rand() -> Result<()> {
    let n = op_rand_float(&[])?
        .get_float()
        .ok_or_else(|| miette!("get_float"))?;
    assert!(n >= 0.);
    assert!(n <= 1.);
    assert_eq!(
        op_rand_bernoulli(&[DataValue::from(0)])?,
        DataValue::from(false)
    );
    assert_eq!(
        op_rand_bernoulli(&[DataValue::from(1)])?,
        DataValue::from(true)
    );
    assert!(op_rand_bernoulli(&[DataValue::from(2)]).is_err());
    let n = op_rand_int(&[DataValue::from(100), DataValue::from(200)])?
        .get_int()
        .ok_or_else(|| miette!("int"))?;
    assert!(n >= 100);
    assert!(n <= 200);
    // An empty range is an error, not a panic.
    assert!(op_rand_int(&[DataValue::from(200), DataValue::from(100)]).is_err());
    assert_eq!(op_rand_choose(&[DataValue::List(vec![])])?, DataValue::Null);
    assert_eq!(
        op_rand_choose(&[DataValue::List(vec![DataValue::from(123)])])?,
        DataValue::from(123)
    );
    Ok(())
}

#[test]
fn test_now() -> Result<()> {
    let now = op_now(&[])?;
    assert!(matches!(now, DataValue::Num(_)));
    let s = op_format_timestamp(&[now])?;
    let dt = op_parse_timestamp(&[s])?;
    assert!(matches!(dt, DataValue::Validity(_) | DataValue::Num(_)));
    Ok(())
}

// A pre-epoch datetime is a negative count, not a panic: the upstream
// original unwrapped `duration_since(UNIX_EPOCH)` and aborted the process
// on any user-supplied datetime before 1970.
