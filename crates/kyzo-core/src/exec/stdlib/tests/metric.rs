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
fn test_vector_distance_domain_errors() {
    let zero = DataValue::Vector(Vector::try_new(vec![0.0, 0.0]).unwrap());
    let unit = DataValue::Vector(Vector::try_new(vec![1.0, 1.0]).unwrap());

    for res in [
        op_cos_dist(&[zero.clone(), unit.clone()]),
        op_cos_dist(&[unit.clone(), zero.clone()]),
        op_l2_normalize(std::slice::from_ref(&zero)),
    ] {
        let err = res.expect_err("a zero vector has no defined direction — must be a typed Err");
        assert!(
            format!("{err:?}").contains("domain_error"),
            "expected the domain-error diagnostic code"
        );
    }

    // Non-degenerate vectors still compute: cos_dist of a vector with itself
    // is 0, and normalization succeeds.
    assert_eq!(
        op_cos_dist(&[unit.clone(), unit.clone()]).unwrap(),
        DataValue::from(0.0)
    );
    assert!(op_l2_normalize(&[unit]).is_ok());

    // The F32 lane is guarded identically.
    let zero32 = DataValue::Vector(Vector::try_new(vec![0.0f64, 0.0]).unwrap());
    let unit32 = DataValue::Vector(Vector::try_new(vec![1.0f64, 1.0]).unwrap());
    assert!(op_cos_dist(&[zero32.clone(), unit32.clone()]).is_err());
    assert!(op_l2_normalize(&[zero32]).is_err());
    assert!(op_cos_dist(&[unit32.clone(), unit32]).is_ok());
}

