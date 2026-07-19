//! geo.rs — stdlib kernel (move_plan).
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::ops::{Div, Rem};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use itertools::Itertools;
use jiff::tz::{Offset, TimeZone};
use miette::{Diagnostic, IntoDiagnostic, Result, bail, ensure, miette};
use rand::prelude::*;
use serde_json::{Value, json};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;
use uuid::v1::Timestamp;

use kyzo_model::data_value_any;
use kyzo_model::value::{
    Bound, DataValue, Interval, Json, Num, NumRepr, NumericOrd, RegexFlags, RegexSource, Validity,
    ValidityTs, Vector,
};
use kyzo_model::{json_from_serde, serde_from_json};
use serde_json::Value as JsonValue;

use kyzo_model::schema::VecElementType;
use crate::exec::stdlib::errors::{
    DivisionByZero, DomainError, IntegerOverflow, StdlibRefuse, TimestampFormatRefused,
    VecOpEmptyArgs, no_nan, no_nan_vec, result_has_nan, vec_value,
};

pub(crate) fn op_deg_to_rad(args: &[DataValue]) -> Result<DataValue> {
    let x = args[0]
        .get_float()
        .ok_or_else(|| miette!("'deg_to_rad' requires numbers"))?;
    Ok(DataValue::from(x * std::f64::consts::PI / 180.))
}


pub(crate) fn op_haversine(args: &[DataValue]) -> Result<DataValue> {
    let miette = || miette!("'haversine' requires numbers");
    let lat1 = args[0].get_float().ok_or_else(miette)?;
    let lon1 = args[1].get_float().ok_or_else(miette)?;
    let lat2 = args[2].get_float().ok_or_else(miette)?;
    let lon2 = args[3].get_float().ok_or_else(miette)?;
    let ret = 2.
        * f64::asin(f64::sqrt(
            f64::sin((lat1 - lat2) / 2.).powi(2)
                + f64::cos(lat1) * f64::cos(lat2) * f64::sin((lon1 - lon2) / 2.).powi(2),
        ));
    Ok(DataValue::from(ret))
}


pub(crate) fn op_haversine_deg_input(args: &[DataValue]) -> Result<DataValue> {
    let miette = || miette!("'haversine_deg_input' requires numbers");
    let lat1 = args[0].get_float().ok_or_else(miette)? * std::f64::consts::PI / 180.;
    let lon1 = args[1].get_float().ok_or_else(miette)? * std::f64::consts::PI / 180.;
    let lat2 = args[2].get_float().ok_or_else(miette)? * std::f64::consts::PI / 180.;
    let lon2 = args[3].get_float().ok_or_else(miette)? * std::f64::consts::PI / 180.;
    let ret = 2.
        * f64::asin(f64::sqrt(
            f64::sin((lat1 - lat2) / 2.).powi(2)
                + f64::cos(lat1) * f64::cos(lat2) * f64::sin((lon1 - lon2) / 2.).powi(2),
        ));
    Ok(DataValue::from(ret))
}


pub(crate) fn op_rad_to_deg(args: &[DataValue]) -> Result<DataValue> {
    let x = args[0]
        .get_float()
        .ok_or_else(|| miette!("'rad_to_deg' requires numbers"))?;
    Ok(DataValue::from(x * 180. / std::f64::consts::PI))
}

