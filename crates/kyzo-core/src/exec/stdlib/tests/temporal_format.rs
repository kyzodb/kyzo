//! Re-homed domain tables from data/tests/functions.rs.
use std::f64::consts::{E, PI};
use serde_json::json;
use kyzo_model::data_value_any;
use kyzo_model::schema::{ColType, NullableColType};
use kyzo_model::str2vld;
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
fn test_pre_epoch_timestamps() {
    let secs = op_parse_timestamp(&[DataValue::from("1969-07-20T20:17:00Z")])
        .unwrap()
        .get_float()
        .unwrap();
    assert!(secs < 0.);

    let vld = str2vld("1969-07-20T20:17:00Z").unwrap();
    assert!(vld.raw() < 0);

    // The schema boundary obeys the same law: coercing a pre-epoch validity
    // string yields negative microseconds.
    let typing = NullableColType::required(ColType::Validity);
    let coerced = typing
        .coerce(
            DataValue::Str("1969-07-20T20:17:00Z".into()),
            ValidityTs::from_raw(0),
        )
        .unwrap();
    match coerced {
        DataValue::Validity(vld) => assert!(vld.timestamp().raw() < 0),
        v @ (data_value_any!()) => panic!("expected a validity, got {v:?}"),
    }
}

