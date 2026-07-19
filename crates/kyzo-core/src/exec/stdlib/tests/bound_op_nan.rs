//! StdlibRefuse::NanAnswer at BoundOp::apply — sole NaN success refuse.
use kyzo_model::value::DataValue;
use crate::exec::stdlib::{resolve_op, StdlibRefuse};

#[test]
fn to_float_nan_string_refused_as_nan_answer() {
    let op = resolve_op("to_float").expect("to_float");
    let err = op
        .apply(&[DataValue::from("NAN")])
        .expect_err("NaN must not be a successful answer");
    let refuse = err.downcast_ref::<StdlibRefuse>().expect("StdlibRefuse");
    assert!(matches!(refuse, StdlibRefuse::NanAnswer { .. }));
}

#[test]
fn to_float_finite_ok() {
    let op = resolve_op("to_float").expect("to_float");
    let v = op.apply(&[DataValue::from("1.5")]).unwrap();
    assert_eq!(v.get_float(), Some(1.5));
}
