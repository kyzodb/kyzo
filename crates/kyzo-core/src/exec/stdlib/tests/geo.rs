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
use crate::exec::stdlib::geo::*;
use kyzo_model::value::DataValue;
use std::f64::consts::PI;

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-5
}

#[test]
fn test_haversine() -> Result<()>  {
    let d = op_haversine_deg_input(&[
        DataValue::from(0),
        DataValue::from(0),
        DataValue::from(0),
        DataValue::from(180),
    ])?
    .get_float()
    ?;
    assert!(close(d, PI));

    let d = op_haversine_deg_input(&[
        DataValue::from(90),
        DataValue::from(0),
        DataValue::from(0),
        DataValue::from(123),
    ])?
    .get_float()
    ?;
    assert!(close(d, PI / 2.));

    let d = op_haversine(&[
        DataValue::from(0),
        DataValue::from(0),
        DataValue::from(0),
        DataValue::from(PI),
    ])?
    .get_float()
    ?;
    assert!(close(d, PI));
    Ok(())
}

#[test]
fn test_deg_rad() -> Result<()>  {
    assert_eq!(
        op_deg_to_rad(&[DataValue::from(180)])?,
        DataValue::from(PI)
    );
    assert_eq!(
        op_rad_to_deg(&[DataValue::from(PI)])?,
        DataValue::from(180.0)
    );
    Ok(())
}
