/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! StdlibRefuse::NanAnswer at BoundOp::apply — sole NaN success refuse.
use miette::{Result, miette};
use crate::exec::stdlib::errors::StdlibRefuse;
use crate::exec::stdlib::resolve_op;
use kyzo_model::value::DataValue;

#[test]
fn to_float_nan_string_refused_as_nan_answer() -> Result<()>  {
    let op = resolve_op("to_float").ok_or_else(|| miette!("to_float"))?;
    let err = op
        .apply(&[DataValue::from("NAN")])
        .expect_err("NaN must not be a successful answer");
    let refuse = err.downcast_ref::<StdlibRefuse>().ok_or_else(|| miette!("StdlibRefuse"))?;
    assert!(matches!(refuse, StdlibRefuse::NanAnswer { .. }));
    Ok(())
}

#[test]
fn to_float_finite_ok() -> Result<()>  {
    let op = resolve_op("to_float").ok_or_else(|| miette!("to_float"))?;
    let v = op.apply(&[DataValue::from("1.5")])?;
    assert_eq!(v.get_float(), Some(1.5));
    Ok(())
}
