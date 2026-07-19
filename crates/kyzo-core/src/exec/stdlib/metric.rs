//! metric.rs — stdlib kernel (move_plan).
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

pub(crate) fn op_ip_dist(args: &[DataValue]) -> Result<DataValue> {
    let a = &args[0];
    let b = &args[1];
    match (a, b) {
        (DataValue::Vector(a), DataValue::Vector(b)) => {
            if a.len() != b.len() {
                bail!("'ip_dist' requires two vectors of the same length");
            }
            let (sa, sb) = (a.to_f64s(), b.to_f64s());
            let dot: f64 = sa.iter().zip(sb.iter()).map(|(x, y)| *x * *y).sum();
            Ok(DataValue::from(1. - dot))
        }
        _ => bail!("'ip_dist' requires two vectors"),
    }
}

pub(crate) fn op_l2_dist(args: &[DataValue]) -> Result<DataValue> {
    let a = &args[0];
    let b = &args[1];
    match (a, b) {
        (DataValue::Vector(a), DataValue::Vector(b)) => {
            if a.len() != b.len() {
                bail!("'l2_dist' requires two vectors of the same length");
            }
            let (sa, sb) = (a.to_f64s(), b.to_f64s());
            let d: f64 = sa
                .iter()
                .zip(sb.iter())
                .map(|(x, y)| (*x - *y) * (*x - *y))
                .sum();
            Ok(DataValue::from(d))
        }
        _ => bail!("'l2_dist' requires two vectors"),
    }
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
