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
fn test_rand() {
    let n = op_rand_float(&[]).unwrap().get_float().unwrap();
    assert!(n >= 0.);
    assert!(n <= 1.);
    assert_eq!(
        op_rand_bernoulli(&[DataValue::from(0)]).unwrap(),
        DataValue::from(false)
    );
    assert_eq!(
        op_rand_bernoulli(&[DataValue::from(1)]).unwrap(),
        DataValue::from(true)
    );
    assert!(op_rand_bernoulli(&[DataValue::from(2)]).is_err());
    let n = op_rand_int(&[DataValue::from(100), DataValue::from(200)])
        .unwrap()
        .get_int()
        .unwrap();
    assert!(n >= 100);
    assert!(n <= 200);
    // An empty range is an error, not a panic.
    assert!(op_rand_int(&[DataValue::from(200), DataValue::from(100)]).is_err());
    assert_eq!(
        op_rand_choose(&[DataValue::List(vec![])]).unwrap(),
        DataValue::Null
    );
    assert_eq!(
        op_rand_choose(&[DataValue::List(vec![DataValue::from(123)])]).unwrap(),
        DataValue::from(123)
    );
}

#[test]
fn test_now() {
    let now = op_now(&[]).unwrap();
    assert!(matches!(now, DataValue::Num(_)));
    let s = op_format_timestamp(&[now]).unwrap();
    let _dt = op_parse_timestamp(&[s]).unwrap();
}

// A pre-epoch datetime is a negative count, not a panic: the upstream
// original unwrapped `duration_since(UNIX_EPOCH)` and aborted the process
// on any user-supplied datetime before 1970.
