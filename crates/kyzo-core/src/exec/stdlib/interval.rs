//! interval.rs — stdlib kernel (move_plan).
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

use crate::exec::stdlib::errors::{
    DivisionByZero, DomainError, IntegerOverflow, StdlibRefuse, TimestampFormatRefused,
    VecOpEmptyArgs, no_nan, no_nan_vec, result_has_nan, vec_value,
};
use kyzo_model::schema::VecElementType;

pub(crate) fn op_interval_before(args: &[DataValue]) -> Result<DataValue> {
    let (a, b) = two_intervals("interval_before", args)?;
    Ok(DataValue::from(a.before(b)))
}

pub(crate) fn op_interval_during(args: &[DataValue]) -> Result<DataValue> {
    let (a, b) = two_intervals("interval_during", args)?;
    Ok(DataValue::from(a.during(b)))
}

pub(crate) fn op_interval_end(args: &[DataValue]) -> Result<DataValue> {
    let iv = args[0]
        .get_interval()
        .ok_or_else(|| miette!("'interval_end' expects an interval, got {:?}", args[0]))?;
    Ok(match iv.end() {
        Some(t) => DataValue::from(t),
        None => DataValue::Null,
    })
}

pub(crate) fn op_interval_finishes(args: &[DataValue]) -> Result<DataValue> {
    let (a, b) = two_intervals("interval_finishes", args)?;
    Ok(DataValue::from(a.finishes(b)))
}

pub(crate) fn op_interval_has_end(args: &[DataValue]) -> Result<DataValue> {
    let iv = args[0]
        .get_interval()
        .ok_or_else(|| miette!("'interval_has_end' expects an interval, got {:?}", args[0]))?;
    Ok(DataValue::from(iv.has_end()))
}

pub(crate) fn op_interval_has_start(args: &[DataValue]) -> Result<DataValue> {
    let iv = args[0].get_interval().ok_or_else(|| {
        miette!(
            "'interval_has_start' expects an interval, got {:?}",
            args[0]
        )
    })?;
    Ok(DataValue::from(iv.has_start()))
}

pub(crate) fn op_interval_intersects(args: &[DataValue]) -> Result<DataValue> {
    let (a, b) = two_intervals("interval_intersects", args)?;
    Ok(DataValue::from(a.intersects(b)))
}

pub(crate) fn op_interval_is_end_unbounded(args: &[DataValue]) -> Result<DataValue> {
    let iv = args[0].get_interval().ok_or_else(|| {
        miette!(
            "'interval_is_end_unbounded' expects an interval, got {:?}",
            args[0]
        )
    })?;
    Ok(DataValue::from(iv.is_end_unbounded()))
}

pub(crate) fn op_interval_is_start_unbounded(args: &[DataValue]) -> Result<DataValue> {
    let iv = args[0].get_interval().ok_or_else(|| {
        miette!(
            "'interval_is_start_unbounded' expects an interval, got {:?}",
            args[0]
        )
    })?;
    Ok(DataValue::from(iv.is_start_unbounded()))
}

pub(crate) fn op_interval_meets(args: &[DataValue]) -> Result<DataValue> {
    let (a, b) = two_intervals("interval_meets", args)?;
    Ok(DataValue::from(a.meets(b)))
}

pub(crate) fn op_interval_overlaps(args: &[DataValue]) -> Result<DataValue> {
    let (a, b) = two_intervals("interval_overlaps", args)?;
    Ok(DataValue::from(a.overlaps(b)))
}

pub(crate) fn op_interval_start(args: &[DataValue]) -> Result<DataValue> {
    let iv = args[0]
        .get_interval()
        .ok_or_else(|| miette!("'interval_start' expects an interval, got {:?}", args[0]))?;
    Ok(match iv.start() {
        Some(t) => DataValue::from(t),
        None => DataValue::Null,
    })
}

pub(crate) fn op_interval_starts(args: &[DataValue]) -> Result<DataValue> {
    let (a, b) = two_intervals("interval_starts", args)?;
    Ok(DataValue::from(a.starts(b)))
}

pub(crate) fn op_make_interval(args: &[DataValue]) -> Result<DataValue> {
    let start = args[0].get_int().ok_or_else(|| {
        miette!(
            "'make_interval' expects an integer start, got {:?}",
            args[0]
        )
    })?;
    let end = args[1]
        .get_int()
        .ok_or_else(|| miette!("'make_interval' expects an integer end, got {:?}", args[1]))?;
    // start > end collapses to the EMPTY interval — a lawful value of
    // the kind, not an error.
    Ok(DataValue::Interval(Interval::new(
        Bound::Closed(start),
        Bound::Closed(end),
    )))
}

/// Extracts both arguments as `Interval`s for a two-interval predicate op, or
/// a typed error naming which argument was wrong — never a panic.
fn two_intervals(op: &str, args: &[DataValue]) -> Result<(Interval, Interval)> {
    let a = args[0].get_interval().ok_or_else(|| {
        miette!(
            "'{op}' expects an interval as its first argument, got {:?}",
            args[0]
        )
    })?;
    let b = args[1].get_interval().ok_or_else(|| {
        miette!(
            "'{op}' expects an interval as its second argument, got {:?}",
            args[1]
        )
    })?;
    Ok((a, b))
}
