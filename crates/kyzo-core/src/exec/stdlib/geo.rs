/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! geo.rs — stdlib kernel (move_plan).

use miette::{Result, miette};

use kyzo_model::value::DataValue;

pub(crate) fn op_deg_to_rad(args: &[DataValue]) -> Result<DataValue> {
    let x = args[0]
        .get_float()
        .ok_or_else(|| miette!("'deg_to_rad' requires numbers"))?;
    Ok(DataValue::from(x * std::f64::consts::PI / 180.))
}

/// Great-circle central angle in radians — ONE seat for haversine variants.
fn haversine_radians(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    2. * f64::asin(f64::sqrt(
        f64::sin((lat1 - lat2) / 2.).powi(2)
            + f64::cos(lat1) * f64::cos(lat2) * f64::sin((lon1 - lon2) / 2.).powi(2),
    ))
}

pub(crate) fn op_haversine(args: &[DataValue]) -> Result<DataValue> {
    let refuse = || miette!("'haversine' requires numbers");
    Ok(DataValue::from(haversine_radians(
        args[0].get_float().ok_or_else(refuse)?,
        args[1].get_float().ok_or_else(refuse)?,
        args[2].get_float().ok_or_else(refuse)?,
        args[3].get_float().ok_or_else(refuse)?,
    )))
}

pub(crate) fn op_haversine_deg_input(args: &[DataValue]) -> Result<DataValue> {
    let refuse = || miette!("'haversine_deg_input' requires numbers");
    let to_rad = |deg: f64| deg * std::f64::consts::PI / 180.;
    Ok(DataValue::from(haversine_radians(
        to_rad(args[0].get_float().ok_or_else(refuse)?),
        to_rad(args[1].get_float().ok_or_else(refuse)?),
        to_rad(args[2].get_float().ok_or_else(refuse)?),
        to_rad(args[3].get_float().ok_or_else(refuse)?),
    )))
}

pub(crate) fn op_rad_to_deg(args: &[DataValue]) -> Result<DataValue> {
    let x = args[0]
        .get_float()
        .ok_or_else(|| miette!("'rad_to_deg' requires numbers"))?;
    Ok(DataValue::from(x * 180. / std::f64::consts::PI))
}
