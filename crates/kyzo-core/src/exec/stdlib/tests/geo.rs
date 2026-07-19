//! Re-homed domain tables from data/tests/functions.rs.
use std::f64::consts::{E, PI};
use serde_json::json;
use kyzo_model::data_value_any;
use kyzo_model::schema::{ColType, NullableColType};
use kyzo_model::value::{DataValue, ValidityTs, Vector};
use crate::exec::stdlib::collection::*;
use crate::exec::stdlib::compare::*;
use crate::exec::stdlib::convert::*;
use crate::exec::stdlib::geo::*;
use crate::exec::stdlib::interval::*;
use crate::exec::stdlib::metric::*;
use crate::exec::stdlib::nondet::*;
use crate::exec::stdlib::numeric::*;
use crate::exec::stdlib::temporal_format::*;
use crate::exec::stdlib::text::*;

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-5
}

#[test]
fn test_haversine() {
    let d = op_haversine_deg_input(&[
        DataValue::from(0),
        DataValue::from(0),
        DataValue::from(0),
        DataValue::from(180),
    ])
    .unwrap()
    .get_float()
    .unwrap();
    assert!(close(d, PI));

    let d = op_haversine_deg_input(&[
        DataValue::from(90),
        DataValue::from(0),
        DataValue::from(0),
        DataValue::from(123),
    ])
    .unwrap()
    .get_float()
    .unwrap();
    assert!(close(d, PI / 2.));

    let d = op_haversine(&[
        DataValue::from(0),
        DataValue::from(0),
        DataValue::from(0),
        DataValue::from(PI),
    ])
    .unwrap()
    .get_float()
    .unwrap();
    assert!(close(d, PI));
}

#[test]
fn test_deg_rad() {
    assert_eq!(
        op_deg_to_rad(&[DataValue::from(180)]).unwrap(),
        DataValue::from(PI)
    );
    assert_eq!(
        op_rad_to_deg(&[DataValue::from(PI)]).unwrap(),
        DataValue::from(180.0)
    );
}

