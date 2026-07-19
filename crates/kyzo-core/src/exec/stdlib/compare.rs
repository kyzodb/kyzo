//! compare.rs — stdlib kernel (move_plan).
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

pub(crate) fn op_assert(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::Bool(true) => Ok(DataValue::from(true)),
        data_value_any!() => bail!("assertion failed: {:?}", args),
    }
}


pub(crate) fn op_eq(args: &[DataValue]) -> Result<DataValue> {
    // Expression equality: NUMERIC for numbers (1 == 1.0, and — unlike
    // the inherited lossy `i as f64` — EXACT beyond 2^53), identity for
    // every other kind. Numeric order lives on [`NumericOrd`], not [`Num`].
    Ok(DataValue::from(match (&args[0], &args[1]) {
        (DataValue::Num(a), DataValue::Num(b)) => NumericOrd::of(*a) == NumericOrd::of(*b),
        (a, b) => a == b,
    }))
}


pub(crate) fn op_ge(args: &[DataValue]) -> Result<DataValue> {
    ensure_same_value_type(&args[0], &args[1])?;
    Ok(DataValue::from(match (&args[0], &args[1]) {
        (DataValue::Num(a), DataValue::Num(b)) => {
            NumericOrd::of(*a).cmp(&NumericOrd::of(*b)) != std::cmp::Ordering::Less
        }
        (a, b) => a.cmp(b) != std::cmp::Ordering::Less,
    }))
}


pub(crate) fn op_gt(args: &[DataValue]) -> Result<DataValue> {
    ensure_same_value_type(&args[0], &args[1])?;
    Ok(DataValue::from(match (&args[0], &args[1]) {
        (DataValue::Num(a), DataValue::Num(b)) => {
            NumericOrd::of(*a).cmp(&NumericOrd::of(*b)) == std::cmp::Ordering::Greater
        }
        (a, b) => a.cmp(b) == std::cmp::Ordering::Greater,
    }))
}


pub(crate) fn op_is_bytes(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(matches!(args[0], DataValue::Bytes(_))))
}


pub(crate) fn op_is_finite(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(match &args[0] {
        DataValue::Num(n) => match n.repr() {
            NumRepr::Int(_) => true,
            NumRepr::Float(f) => f.is_finite(),
        },
        data_value_any!() => false,
    }))
}


pub(crate) fn op_is_float(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(
        matches!(&args[0], DataValue::Num(n) if matches!(n.repr(), NumRepr::Float(_))),
    ))
}


pub(crate) fn op_is_in(args: &[DataValue]) -> Result<DataValue> {
    let left = &args[0];
    let right = args[1]
        .get_slice()
        .ok_or_else(|| miette!("right hand side of 'is_in' must be a list"))?;
    Ok(DataValue::from(right.contains(left)))
}


pub(crate) fn op_is_infinite(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(
        matches!(&args[0], DataValue::Num(n) if matches!(n.repr(), NumRepr::Float(f) if f.is_infinite())),
    ))
}


pub(crate) fn op_is_int(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(
        matches!(&args[0], DataValue::Num(n) if matches!(n.repr(), NumRepr::Int(_))),
    ))
}


pub(crate) fn op_is_json(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(matches!(args[0], DataValue::Json(_))))
}


pub(crate) fn op_is_list(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(matches!(
        args[0],
        DataValue::List(_) | DataValue::Set(_)
    )))
}


pub(crate) fn op_is_nan(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(
        matches!(&args[0], DataValue::Num(n) if matches!(n.repr(), NumRepr::Float(f) if f.is_nan())),
    ))
}


pub(crate) fn op_is_null(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(matches!(args[0], DataValue::Null)))
}


pub(crate) fn op_is_num(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(matches!(args[0], DataValue::Num(_))))
}


pub(crate) fn op_is_string(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(matches!(args[0], DataValue::Str(_))))
}


pub(crate) fn op_is_uuid(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(matches!(args[0], DataValue::Uuid(_))))
}


pub(crate) fn op_is_vec(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(matches!(args[0], DataValue::Vector(_))))
}


pub(crate) fn op_le(args: &[DataValue]) -> Result<DataValue> {
    ensure_same_value_type(&args[0], &args[1])?;
    Ok(DataValue::from(match (&args[0], &args[1]) {
        (DataValue::Num(a), DataValue::Num(b)) => {
            NumericOrd::of(*a).cmp(&NumericOrd::of(*b)) != std::cmp::Ordering::Greater
        }
        (a, b) => a.cmp(b) != std::cmp::Ordering::Greater,
    }))
}


pub(crate) fn op_lt(args: &[DataValue]) -> Result<DataValue> {
    ensure_same_value_type(&args[0], &args[1])?;
    Ok(DataValue::from(match (&args[0], &args[1]) {
        (DataValue::Num(a), DataValue::Num(b)) => {
            NumericOrd::of(*a).cmp(&NumericOrd::of(*b)) == std::cmp::Ordering::Less
        }
        (a, b) => a.cmp(b) == std::cmp::Ordering::Less,
    }))
}


pub(crate) fn op_neq(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(match (&args[0], &args[1]) {
        (DataValue::Num(a), DataValue::Num(b)) => NumericOrd::of(*a) != NumericOrd::of(*b),
        (a, b) => a != b,
    }))
}



fn ensure_same_value_type(a: &DataValue, b: &DataValue) -> Result<()> {
    use DataValue::*;
    if !matches!(
        (a, b),
        (Null, Null)
            | (Bool(_), Bool(_))
            | (Num(_), Num(_))
            | (Str(_), Str(_))
            | (Bytes(_), Bytes(_))
            | (Regex(_), Regex(_))
            | (List(_), List(_))
            | (Set(_), Set(_))
    ) {
        bail!(StdlibRefuse::CrossTypeCompare {
            left: format!("{a:?}"),
            right: format!("{b:?}"),
        })
    }
    Ok(())
}

