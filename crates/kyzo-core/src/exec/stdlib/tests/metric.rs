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
use crate::exec::stdlib::metric::*;
use kyzo_model::value::{DataValue, Vector};


#[test]
fn test_vector_distance_domain_errors() -> Result<()>  {
    let zero = DataValue::Vector(Vector::try_new(vec![0.0, 0.0]).ok_or_else(|| miette!("vector"))?);
    let unit = DataValue::Vector(Vector::try_new(vec![1.0, 1.0]).ok_or_else(|| miette!("vector"))?);

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
        op_cos_dist(&[unit.clone(), unit.clone()])?,
        DataValue::from(0.0)
    );
    assert!(op_l2_normalize(&[unit]).is_ok());

    // The F32 lane is guarded identically.
    let zero32 = DataValue::Vector(Vector::try_new(vec![0.0f64, 0.0]).ok_or_else(|| miette!("vector"))?);
    let unit32 = DataValue::Vector(Vector::try_new(vec![1.0f64, 1.0]).ok_or_else(|| miette!("vector"))?);
    assert!(op_cos_dist(&[zero32.clone(), unit32.clone()]).is_err());
    assert!(op_l2_normalize(&[zero32]).is_err());
    assert!(op_cos_dist(&[unit32.clone(), unit32]).is_ok());
    Ok(())
}
