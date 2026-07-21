/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! metric.rs — stdlib kernel (move_plan).

use miette::{Result, bail};

use kyzo_model::data_value_any;
use kyzo_model::value::DataValue;

use crate::exec::stdlib::errors::{DomainError, no_nan, no_nan_vec};

pub(crate) fn op_cos_dist(args: &[DataValue]) -> Result<DataValue> {
    let a = &args[0];
    let b = &args[1];
    match (a, b) {
        (DataValue::Vector(a), DataValue::Vector(b)) => {
            if a.len() != b.len() {
                bail!("'cos_dist' requires two vectors of the same length");
            }
            let (sa, sb) = (a.to_f64s(), b.to_f64s());
            let a_norm: f64 = sa.iter().map(|x| x * x).sum();
            let b_norm: f64 = sb.iter().map(|x| x * x).sum();
            if a_norm == 0.0 || b_norm == 0.0 {
                bail!(DomainError {
                    op: "cos_dist".into()
                });
            }
            let dot: f64 = sa.iter().zip(sb.iter()).map(|(x, y)| *x * *y).sum();
            no_nan("cos_dist", 1. - dot / (a_norm * b_norm).sqrt())
        }
        _ => bail!("'cos_dist' requires two vectors"),
    }
}

fn vec_pair_dist(
    op: &'static str,
    args: &[DataValue],
    reduce: impl FnOnce(&[f64], &[f64]) -> f64,
) -> Result<DataValue> {
    match (&args[0], &args[1]) {
        (DataValue::Vector(a), DataValue::Vector(b)) => {
            if a.len() != b.len() {
                bail!("'{op}' requires two vectors of the same length");
            }
            let (sa, sb) = (a.to_f64s(), b.to_f64s());
            Ok(DataValue::from(reduce(&sa, &sb)))
        }
        _ => bail!("'{op}' requires two vectors"),
    }
}

pub(crate) fn op_ip_dist(args: &[DataValue]) -> Result<DataValue> {
    vec_pair_dist("ip_dist", args, |sa, sb| {
        1. - sa.iter().zip(sb.iter()).map(|(x, y)| *x * *y).sum::<f64>()
    })
}

pub(crate) fn op_l2_dist(args: &[DataValue]) -> Result<DataValue> {
    vec_pair_dist("l2_dist", args, |sa, sb| {
        sa.iter()
            .zip(sb.iter())
            .map(|(x, y)| (*x - *y) * (*x - *y))
            .sum()
    })
}

pub(crate) fn op_l2_normalize(args: &[DataValue]) -> Result<DataValue> {
    let a = &args[0];
    match a {
        DataValue::Vector(a) => {
            let s = a.to_f64s();
            let norm = s.iter().map(|x| x * x).sum::<f64>().sqrt();
            if norm == 0.0 {
                bail!(DomainError {
                    op: "l2_normalize".into()
                });
            }
            no_nan_vec("l2_normalize", s.iter().map(|x| x / norm).collect())
        }
        data_value_any!() => bail!("'l2_normalize' requires a vector"),
    }
}
