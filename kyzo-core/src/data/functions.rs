/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): every op declares its determinism, user-reachable panics
 * (non-array JSON in `vec`, negative/out-of-range JSON paths, pre-epoch
 * clocks and datetimes) became typed errors, and the unsafe pointer-cast
 * vector decode became a safe, endian-defined one.
 */

//! The standard library of KyzoScript: every built-in operation, one
//! function each.
//!
//! An op is a **total function over values**: applied to any argument slice
//! satisfying its declared arity, it returns a `DataValue` or an error —
//! never panics; errors are values. Corrupt or hostile input is an error by
//! law, so nothing an op body does may abort the process.
//!
//! Each op is declared once through [`define_op!`], which welds together the
//! constant's name, the implementing function (derived from that same name),
//! the arity, and the determinism claim — the facts cannot drift apart.
//! Argument-count safety is the caller's proof: the parser and the serde
//! boundary (see `data/expr.rs`) guarantee the declared arity before an op
//! body runs, which is why bodies index `args[0]`, `args[1]` … up to their
//! minimum arity without checking.
//!
//! Determinism is data, not folklore: the clock and randomness ops carry
//! `deterministic = false` and are therefore never constant-folded — they
//! evaluate per row at runtime.

use std::cmp::Reverse;
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
use smartstring::SmartString;
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;
use uuid::v1::Timestamp;

use crate::data::expr::Op;
use crate::data::value::{
    DataValue, Interval, JsonData, JsonValue, Num, RegexWrapper, UuidWrapper, Validity, ValidityTs,
    VecElementType, Vector,
};

/// Declares one built-in op: the `Op` const, its name (stringified from the
/// const's own identifier), its arity contract, its determinism claim, and
/// its implementation (the same identifier, lowercased by `casey`). One
/// invocation, five facts, zero drift.
macro_rules! define_op {
    ($name:ident, $min_arity:expr, $vararg:expr, $deterministic:expr) => {
        pub(crate) const $name: Op = Op {
            name: stringify!($name),
            min_arity: $min_arity,
            vararg: $vararg,
            deterministic: $deterministic,
            inner: ::casey::lower!($name),
        };
    };
}

/// The host clock as a duration since the Unix epoch.
///
/// Policy (documented choice): a clock reading before 1970 is an **error**,
/// not saturation — a time-travel database whose host clock is decades wrong
/// should refuse loudly rather than silently write validity at the epoch.
/// The CozoDB original unwrapped and aborted the process.
fn unix_now() -> Result<Duration> {
    #[derive(Debug, Error, Diagnostic)]
    #[error("The system clock reads earlier than the Unix epoch")]
    #[diagnostic(code(eval::clock_before_epoch))]
    #[diagnostic(help("Fix the host clock; timestamps are seconds since 1970-01-01T00:00:00Z"))]
    struct ClockBeforeEpochError;

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ClockBeforeEpochError.into())
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
            | (Bot, Bot)
    ) {
        bail!(
            "comparison can only be done between the same datatypes, got {:?} and {:?}",
            a,
            b
        )
    }
    Ok(())
}

define_op!(OP_LIST, 0, true, true);
pub(crate) fn op_list(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::List(args.to_vec()))
}

define_op!(OP_JSON, 1, false, true);
pub(crate) fn op_json(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::Json(JsonData(to_json(&args[0]))))
}

define_op!(OP_SET_JSON_PATH, 3, false, true);
pub(crate) fn op_set_json_path(args: &[DataValue]) -> Result<DataValue> {
    let mut result = to_json(&args[0]);
    let path = args[1]
        .get_slice()
        .ok_or_else(|| miette!("json path must be a list"))?;
    let pointer = get_json_path(&mut result, path)?;
    let new_val = to_json(&args[2]);
    *pointer = new_val;
    Ok(DataValue::Json(JsonData(result)))
}

/// A path step into a JSON array, proven non-negative and machine-sized.
/// The original cast `i64 as usize`, so a hostile `-1` became a huge index —
/// harmless on reads, but an OOM-scale `resize_with` on writes.
fn json_array_index(key: &DataValue) -> Result<usize> {
    let i = key
        .get_int()
        .ok_or_else(|| miette!("json path must be a string or a number"))?;
    usize::try_from(i).map_err(|_| miette!("json array index must be non-negative, got {i}"))
}

fn get_json_path_immutable<'a>(
    mut pointer: &'a JsonValue,
    path: &[DataValue],
) -> Result<&'a JsonValue> {
    for key in path {
        match pointer {
            JsonValue::Object(obj) => {
                let key = val2str(key);
                let entry = obj
                    .get(&key)
                    .ok_or_else(|| miette!("json path does not exist"))?;
                pointer = entry;
            }
            JsonValue::Array(arr) => {
                let key = json_array_index(key)?;
                let val = arr
                    .get(key)
                    .ok_or_else(|| miette!("json path does not exist"))?;
                pointer = val;
            }
            _ => {
                bail!("json path does not exist")
            }
        }
    }
    Ok(pointer)
}

fn get_json_path<'a>(
    mut pointer: &'a mut JsonValue,
    path: &[DataValue],
) -> Result<&'a mut JsonValue> {
    for key in path {
        match pointer {
            JsonValue::Object(obj) => {
                let key = val2str(key);
                let entry = obj.entry(key).or_insert(json!({}));
                pointer = entry;
            }
            JsonValue::Array(arr) => {
                let key = json_array_index(key)?;
                if arr.len() <= key {
                    arr.resize_with(key + 1, || JsonValue::Null);
                }
                // In bounds: just resized to at least `key + 1`.
                pointer = &mut arr[key];
            }
            _ => {
                bail!("json path does not exist")
            }
        }
    }
    Ok(pointer)
}

define_op!(OP_REMOVE_JSON_PATH, 2, false, true);
pub(crate) fn op_remove_json_path(args: &[DataValue]) -> Result<DataValue> {
    let mut result = to_json(&args[0]);
    let path = args[1]
        .get_slice()
        .ok_or_else(|| miette!("json path must be a list"))?;
    let (last, path) = path
        .split_last()
        .ok_or_else(|| miette!("json path must not be empty"))?;
    let pointer = get_json_path(&mut result, path)?;
    match pointer {
        JsonValue::Object(obj) => {
            let key = val2str(last);
            obj.remove(&key);
        }
        JsonValue::Array(arr) => {
            let key = json_array_index(last)?;
            // `Vec::remove` panics out of range; a missing path is an error
            // like everywhere else in the path walkers (the original panicked).
            ensure!(key < arr.len(), "json path does not exist");
            arr.remove(key);
        }
        _ => {
            bail!("json path does not exist")
        }
    }
    Ok(DataValue::Json(JsonData(result)))
}

define_op!(OP_JSON_OBJECT, 0, true, true);
pub(crate) fn op_json_object(args: &[DataValue]) -> Result<DataValue> {
    ensure!(
        args.len().is_multiple_of(2),
        "json_object requires an even number of arguments"
    );
    let mut obj = serde_json::Map::with_capacity(args.len() / 2);
    for pair in args.chunks_exact(2) {
        let key = val2str(&pair[0]);
        let value = to_json(&pair[1]);
        obj.insert(key.to_string(), value);
    }
    Ok(DataValue::Json(JsonData(Value::Object(obj))))
}

pub(crate) fn to_json(d: &DataValue) -> JsonValue {
    match d {
        DataValue::Null => {
            json!(null)
        }
        DataValue::Bool(b) => {
            json!(b)
        }
        DataValue::Num(n) => match n {
            Num::Int(i) => {
                json!(i)
            }
            Num::Float(f) => {
                json!(f)
            }
        },
        DataValue::Str(s) => {
            json!(s)
        }
        DataValue::Bytes(b) => {
            json!(b)
        }
        DataValue::Uuid(u) => {
            json!(u.0.as_bytes())
        }
        DataValue::Regex(r) => {
            json!(r.0.as_str())
        }
        DataValue::List(l) => {
            let mut arr = Vec::with_capacity(l.len());
            for el in l {
                arr.push(to_json(el));
            }
            arr.into()
        }
        DataValue::Set(l) => {
            let mut arr = Vec::with_capacity(l.len());
            for el in l {
                arr.push(to_json(el));
            }
            arr.into()
        }
        DataValue::Vec(v) => {
            let mut arr = Vec::with_capacity(v.len());
            match v {
                Vector::F32(a) => {
                    for el in a {
                        arr.push(json!(el));
                    }
                }
                Vector::F64(a) => {
                    for el in a {
                        arr.push(json!(el));
                    }
                }
            }
            arr.into()
        }
        DataValue::Json(j) => j.0.clone(),
        DataValue::Validity(vld) => {
            json!([vld.timestamp.0, vld.is_assert.0])
        }
        DataValue::Interval(iv) => {
            json!([iv.start(), iv.end()])
        }
        DataValue::Bot => {
            json!(null)
        }
    }
}

define_op!(OP_PARSE_JSON, 1, false, true);
pub(crate) fn op_parse_json(args: &[DataValue]) -> Result<DataValue> {
    match args[0].get_str() {
        Some(s) => {
            let value = serde_json::from_str(s).into_diagnostic()?;
            Ok(DataValue::Json(JsonData(value)))
        }
        None => bail!("parse_json requires a string argument"),
    }
}

define_op!(OP_DUMP_JSON, 1, false, true);
pub(crate) fn op_dump_json(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::Json(j) => Ok(DataValue::Str(j.0.to_string().into())),
        _ => bail!("dump_json requires a json argument"),
    }
}

define_op!(OP_EQ, 2, false, true);
pub(crate) fn op_eq(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(match (&args[0], &args[1]) {
        (DataValue::Num(Num::Float(f)), DataValue::Num(Num::Int(i)))
        | (DataValue::Num(Num::Int(i)), DataValue::Num(Num::Float(f))) => *i as f64 == *f,
        (a, b) => a == b,
    }))
}

define_op!(OP_IS_UUID, 1, false, true);
pub(crate) fn op_is_uuid(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(matches!(args[0], DataValue::Uuid(_))))
}

define_op!(OP_IS_JSON, 1, false, true);
pub(crate) fn op_is_json(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(matches!(args[0], DataValue::Json(_))))
}

define_op!(OP_JSON_TO_SCALAR, 1, false, true);
pub(crate) fn op_json_to_scalar(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Json(JsonData(j)) => json2val(j.clone()),
        d => d.clone(),
    })
}

define_op!(OP_IS_IN, 2, false, true);
pub(crate) fn op_is_in(args: &[DataValue]) -> Result<DataValue> {
    let left = &args[0];
    let right = args[1]
        .get_slice()
        .ok_or_else(|| miette!("right hand side of 'is_in' must be a list"))?;
    Ok(DataValue::from(right.contains(left)))
}

define_op!(OP_NEQ, 2, false, true);
pub(crate) fn op_neq(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(match (&args[0], &args[1]) {
        (DataValue::Num(Num::Float(f)), DataValue::Num(Num::Int(i)))
        | (DataValue::Num(Num::Int(i)), DataValue::Num(Num::Float(f))) => *i as f64 != *f,
        (a, b) => a != b,
    }))
}

define_op!(OP_GT, 2, false, true);
pub(crate) fn op_gt(args: &[DataValue]) -> Result<DataValue> {
    ensure_same_value_type(&args[0], &args[1])?;
    Ok(DataValue::from(match (&args[0], &args[1]) {
        (DataValue::Num(Num::Float(l)), DataValue::Num(Num::Int(r))) => *l > *r as f64,
        (DataValue::Num(Num::Int(l)), DataValue::Num(Num::Float(r))) => *l as f64 > *r,
        (a, b) => a > b,
    }))
}

define_op!(OP_GE, 2, false, true);
pub(crate) fn op_ge(args: &[DataValue]) -> Result<DataValue> {
    ensure_same_value_type(&args[0], &args[1])?;
    Ok(DataValue::from(match (&args[0], &args[1]) {
        (DataValue::Num(Num::Float(l)), DataValue::Num(Num::Int(r))) => *l >= *r as f64,
        (DataValue::Num(Num::Int(l)), DataValue::Num(Num::Float(r))) => *l as f64 >= *r,
        (a, b) => a >= b,
    }))
}

define_op!(OP_LT, 2, false, true);
pub(crate) fn op_lt(args: &[DataValue]) -> Result<DataValue> {
    ensure_same_value_type(&args[0], &args[1])?;
    Ok(DataValue::from(match (&args[0], &args[1]) {
        (DataValue::Num(Num::Float(l)), DataValue::Num(Num::Int(r))) => *l < (*r as f64),
        (DataValue::Num(Num::Int(l)), DataValue::Num(Num::Float(r))) => (*l as f64) < *r,
        (a, b) => a < b,
    }))
}

define_op!(OP_LE, 2, false, true);
pub(crate) fn op_le(args: &[DataValue]) -> Result<DataValue> {
    ensure_same_value_type(&args[0], &args[1])?;
    Ok(DataValue::from(match (&args[0], &args[1]) {
        (DataValue::Num(Num::Float(l)), DataValue::Num(Num::Int(r))) => *l <= (*r as f64),
        (DataValue::Num(Num::Int(l)), DataValue::Num(Num::Float(r))) => (*l as f64) <= *r,
        (a, b) => a <= b,
    }))
}

define_op!(OP_ADD, 0, true, true);
/// A 64-bit integer scalar op overflowed. Arithmetic errors are typed
/// errors by law: never a silent panic (debug builds) and never silent
/// wraparound (release builds serving a wrong answer). Float paths are
/// untouched — they saturate to infinity legitimately.
#[derive(Debug, Error, Diagnostic)]
#[error("integer overflow evaluating '{op}'")]
#[diagnostic(code(eval::integer_overflow))]
#[diagnostic(help("The operands are exact 64-bit integers whose result does not fit in i64."))]
pub(crate) struct IntegerOverflow {
    pub(crate) op: &'static str,
}

/// A zero divisor was offered to `div` or `mod`, integer or float alike. A
/// silent `Infinity`/`NaN` is a poison value that buries the caller's logic
/// bug; this engine refuses instead, the same way `mod` always has and `div`
/// now does too — one typed shape for both ops, parameterized only by name.
#[derive(Debug, Error, Diagnostic)]
#[error("'{op}' requires a non-zero divisor")]
#[diagnostic(code(eval::division_by_zero))]
#[diagnostic(help(
    "Division and modulo both refuse a zero divisor rather than returning infinity or NaN."
))]
pub(crate) struct DivisionByZero {
    pub(crate) op: &'static str,
}

pub(crate) fn op_add(args: &[DataValue]) -> Result<DataValue> {
    let mut i_accum = 0i64;
    let mut f_accum = 0.0f64;
    for arg in args {
        match arg {
            DataValue::Num(Num::Int(i)) => {
                i_accum = i_accum
                    .checked_add(*i)
                    .ok_or(IntegerOverflow { op: "add" })?;
            }
            DataValue::Num(Num::Float(f)) => f_accum += f,
            DataValue::Vec(_) => return add_vecs(args),
            _ => bail!("addition requires numbers"),
        }
    }
    if f_accum == 0.0f64 {
        Ok(DataValue::Num(Num::Int(i_accum)))
    } else {
        Ok(DataValue::Num(Num::Float(i_accum as f64 + f_accum)))
    }
}

fn add_vecs(args: &[DataValue]) -> Result<DataValue> {
    if args.len() == 1 {
        return Ok(args[0].clone());
    }
    // Non-empty: only called from `op_add`/`op_mul` after a `Vec` argument
    // was seen (so len >= 1), and the len == 1 case returned above.
    let (last, first) = args.split_last().expect("args non-empty");
    let first = add_vecs(first)?;
    match (first, last) {
        (DataValue::Vec(a), DataValue::Vec(b)) => {
            if a.len() != b.len() {
                bail!("can only add vectors of the same length");
            }
            match (a, b) {
                (Vector::F32(a), Vector::F32(b)) => Ok(DataValue::Vec(Vector::F32(a + b))),
                (Vector::F64(a), Vector::F64(b)) => Ok(DataValue::Vec(Vector::F64(a + b))),
                (Vector::F32(a), Vector::F64(b)) => {
                    let a = a.mapv(|x| x as f64);
                    Ok(DataValue::Vec(Vector::F64(a + b)))
                }
                (Vector::F64(a), Vector::F32(b)) => {
                    let b = b.mapv(|x| x as f64);
                    Ok(DataValue::Vec(Vector::F64(a + b)))
                }
            }
        }
        (DataValue::Vec(a), b) => {
            let f = b
                .get_float()
                .ok_or_else(|| miette!("can only add numbers to vectors"))?;
            match a {
                Vector::F32(mut v) => {
                    v += f as f32;
                    Ok(DataValue::Vec(Vector::F32(v)))
                }
                Vector::F64(mut v) => {
                    v += f;
                    Ok(DataValue::Vec(Vector::F64(v)))
                }
            }
        }
        (a, DataValue::Vec(b)) => {
            let f = a
                .get_float()
                .ok_or_else(|| miette!("can only add numbers to vectors"))?;
            match b {
                Vector::F32(v) => Ok(DataValue::Vec(Vector::F32(v + f as f32))),
                Vector::F64(v) => Ok(DataValue::Vec(Vector::F64(v + f))),
            }
        }
        _ => bail!("addition requires numbers"),
    }
}

define_op!(OP_MAX, 1, true, true);
pub(crate) fn op_max(args: &[DataValue]) -> Result<DataValue> {
    let res = args
        .iter()
        .try_fold(None, |accum, nxt| match (accum, nxt) {
            (None, d @ DataValue::Num(_)) => Ok(Some(d.clone())),
            (Some(DataValue::Num(a)), DataValue::Num(b)) => Ok(Some(DataValue::Num(a.max(*b)))),
            _ => bail!("'max can only be applied to numbers'"),
        })?;
    match res {
        None => Ok(DataValue::Num(Num::Float(f64::NEG_INFINITY))),
        Some(v) => Ok(v),
    }
}

define_op!(OP_MIN, 1, true, true);
pub(crate) fn op_min(args: &[DataValue]) -> Result<DataValue> {
    let res = args
        .iter()
        .try_fold(None, |accum, nxt| match (accum, nxt) {
            (None, d @ DataValue::Num(_)) => Ok(Some(d.clone())),
            (Some(DataValue::Num(a)), DataValue::Num(b)) => Ok(Some(DataValue::Num(a.min(*b)))),
            _ => bail!("'min' can only be applied to numbers"),
        })?;
    match res {
        None => Ok(DataValue::Num(Num::Float(f64::INFINITY))),
        Some(v) => Ok(v),
    }
}

define_op!(OP_SUB, 2, false, true);
pub(crate) fn op_sub(args: &[DataValue]) -> Result<DataValue> {
    Ok(match (&args[0], &args[1]) {
        (DataValue::Num(Num::Int(a)), DataValue::Num(Num::Int(b))) => match a.checked_sub(*b) {
            Some(v) => DataValue::Num(Num::Int(v)),
            None => bail!(IntegerOverflow { op: "sub" }),
        },
        (DataValue::Num(Num::Float(a)), DataValue::Num(Num::Float(b))) => {
            DataValue::Num(Num::Float(*a - *b))
        }
        (DataValue::Num(Num::Int(a)), DataValue::Num(Num::Float(b))) => {
            DataValue::Num(Num::Float((*a as f64) - b))
        }
        (DataValue::Num(Num::Float(a)), DataValue::Num(Num::Int(b))) => {
            DataValue::Num(Num::Float(a - (*b as f64)))
        }
        (DataValue::Vec(a), DataValue::Vec(b)) => match (a, b) {
            (Vector::F32(a), Vector::F32(b)) => DataValue::Vec(Vector::F32(a - b)),
            (Vector::F64(a), Vector::F64(b)) => DataValue::Vec(Vector::F64(a - b)),
            (Vector::F32(a), Vector::F64(b)) => {
                let a = a.mapv(|x| x as f64);
                DataValue::Vec(Vector::F64(a - b))
            }
            (Vector::F64(a), Vector::F32(b)) => {
                let b = b.mapv(|x| x as f64);
                DataValue::Vec(Vector::F64(a - b))
            }
        },
        (DataValue::Vec(a), b) => {
            let b = b
                .get_float()
                .ok_or_else(|| miette!("can only subtract numbers from vectors"))?;
            match a.clone() {
                Vector::F32(mut v) => {
                    v -= b as f32;
                    DataValue::Vec(Vector::F32(v))
                }
                Vector::F64(mut v) => {
                    v -= b;
                    DataValue::Vec(Vector::F64(v))
                }
            }
        }
        (a, DataValue::Vec(b)) => {
            let a = a
                .get_float()
                .ok_or_else(|| miette!("can only subtract vectors from numbers"))?;
            match b.clone() {
                Vector::F32(mut v) => {
                    v -= a as f32;
                    DataValue::Vec(Vector::F32(-v))
                }
                Vector::F64(mut v) => {
                    v -= a;
                    DataValue::Vec(Vector::F64(-v))
                }
            }
        }
        _ => bail!("subtraction requires numbers"),
    })
}

define_op!(OP_MUL, 0, true, true);
pub(crate) fn op_mul(args: &[DataValue]) -> Result<DataValue> {
    let mut i_accum = 1i64;
    let mut f_accum = 1.0f64;
    for arg in args {
        match arg {
            DataValue::Num(Num::Int(i)) => {
                i_accum = i_accum
                    .checked_mul(*i)
                    .ok_or(IntegerOverflow { op: "mul" })?;
            }
            DataValue::Num(Num::Float(f)) => f_accum *= f,
            DataValue::Vec(_) => return mul_vecs(args),
            _ => bail!("multiplication requires numbers"),
        }
    }
    if f_accum == 1.0f64 {
        Ok(DataValue::Num(Num::Int(i_accum)))
    } else {
        Ok(DataValue::Num(Num::Float(i_accum as f64 * f_accum)))
    }
}

fn mul_vecs(args: &[DataValue]) -> Result<DataValue> {
    if args.len() == 1 {
        return Ok(args[0].clone());
    }
    // Non-empty: see `add_vecs`.
    let (last, first) = args.split_last().expect("args non-empty");
    // The CozoDB original recursed into `add_vecs` here, so multiplying
    // three or more vector arguments *added* the prefix before multiplying
    // by the last: `v1 * v2 * v3` computed `(v1 + v2) * v3`. Fixed to
    // recurse into multiplication; flagged as a deliberate deviation.
    let first = mul_vecs(first)?;
    match (first, last) {
        (DataValue::Vec(a), DataValue::Vec(b)) => {
            if a.len() != b.len() {
                bail!("can only multiply vectors of the same length");
            }
            match (a, b) {
                (Vector::F32(a), Vector::F32(b)) => Ok(DataValue::Vec(Vector::F32(a * b))),
                (Vector::F64(a), Vector::F64(b)) => Ok(DataValue::Vec(Vector::F64(a * b))),
                (Vector::F32(a), Vector::F64(b)) => {
                    let a = a.mapv(|x| x as f64);
                    Ok(DataValue::Vec(Vector::F64(a * b)))
                }
                (Vector::F64(a), Vector::F32(b)) => {
                    let b = b.mapv(|x| x as f64);
                    Ok(DataValue::Vec(Vector::F64(a * b)))
                }
            }
        }
        (DataValue::Vec(a), b) => {
            let f = b
                .get_float()
                .ok_or_else(|| miette!("can only multiply vectors by numbers"))?;
            match a {
                Vector::F32(mut v) => {
                    v *= f as f32;
                    Ok(DataValue::Vec(Vector::F32(v)))
                }
                Vector::F64(mut v) => {
                    v *= f;
                    Ok(DataValue::Vec(Vector::F64(v)))
                }
            }
        }
        (a, DataValue::Vec(b)) => {
            let f = a
                .get_float()
                .ok_or_else(|| miette!("can only multiply vectors by numbers"))?;
            match b {
                Vector::F32(v) => Ok(DataValue::Vec(Vector::F32(v * f as f32))),
                Vector::F64(v) => Ok(DataValue::Vec(Vector::F64(v * f))),
            }
        }
        _ => bail!("multiplication requires numbers"),
    }
}

define_op!(OP_DIV, 2, false, true);
pub(crate) fn op_div(args: &[DataValue]) -> Result<DataValue> {
    Ok(match (&args[0], &args[1]) {
        (DataValue::Num(Num::Int(a)), DataValue::Num(Num::Int(b))) => {
            if *b == 0 {
                bail!(DivisionByZero { op: "div" })
            }
            DataValue::Num(Num::Float((*a as f64) / (*b as f64)))
        }
        (DataValue::Num(Num::Float(a)), DataValue::Num(Num::Float(b))) => {
            if *b == 0.0 {
                bail!(DivisionByZero { op: "div" })
            }
            DataValue::Num(Num::Float(*a / *b))
        }
        (DataValue::Num(Num::Int(a)), DataValue::Num(Num::Float(b))) => {
            if *b == 0.0 {
                bail!(DivisionByZero { op: "div" })
            }
            DataValue::Num(Num::Float((*a as f64) / b))
        }
        (DataValue::Num(Num::Float(a)), DataValue::Num(Num::Int(b))) => {
            if *b == 0 {
                bail!(DivisionByZero { op: "div" })
            }
            DataValue::Num(Num::Float(a / (*b as f64)))
        }
        (DataValue::Vec(a), DataValue::Vec(b)) => match (a, b) {
            (Vector::F32(a), Vector::F32(b)) => DataValue::Vec(Vector::F32(a / b)),
            (Vector::F64(a), Vector::F64(b)) => DataValue::Vec(Vector::F64(a / b)),
            (Vector::F32(a), Vector::F64(b)) => {
                let a = a.mapv(|x| x as f64);
                DataValue::Vec(Vector::F64(a / b))
            }
            (Vector::F64(a), Vector::F32(b)) => {
                let b = b.mapv(|x| x as f64);
                DataValue::Vec(Vector::F64(a / b))
            }
        },
        (DataValue::Vec(a), b) => {
            let b = b
                .get_float()
                .ok_or_else(|| miette!("can only divide vectors by numbers"))?;
            match a.clone() {
                Vector::F32(mut v) => {
                    v /= b as f32;
                    DataValue::Vec(Vector::F32(v))
                }
                Vector::F64(mut v) => {
                    v /= b;
                    DataValue::Vec(Vector::F64(v))
                }
            }
        }
        (a, DataValue::Vec(b)) => {
            let a = a
                .get_float()
                .ok_or_else(|| miette!("can only divide numbers by vectors"))?;
            match b {
                Vector::F32(v) => DataValue::Vec(Vector::F32(a as f32 / v)),
                Vector::F64(v) => DataValue::Vec(Vector::F64(a / v)),
            }
        }
        _ => bail!("division requires numbers"),
    })
}

define_op!(OP_MINUS, 1, false, true);
pub(crate) fn op_minus(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Num(Num::Int(i)) => match i.checked_neg() {
            Some(v) => DataValue::Num(Num::Int(v)),
            None => bail!(IntegerOverflow { op: "minus" }),
        },
        DataValue::Num(Num::Float(f)) => DataValue::Num(Num::Float(-(*f))),
        DataValue::Vec(Vector::F64(v)) => DataValue::Vec(Vector::F64(0. - v)),
        DataValue::Vec(Vector::F32(v)) => DataValue::Vec(Vector::F32(0. - v)),
        _ => bail!("minus can only be applied to numbers"),
    })
}

define_op!(OP_ABS, 1, false, true);
pub(crate) fn op_abs(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Num(Num::Int(i)) => match i.checked_abs() {
            Some(v) => DataValue::Num(Num::Int(v)),
            None => bail!(IntegerOverflow { op: "abs" }),
        },
        DataValue::Num(Num::Float(f)) => DataValue::Num(Num::Float(f.abs())),
        DataValue::Vec(Vector::F64(v)) => DataValue::Vec(Vector::F64(v.mapv(|x| x.abs()))),
        DataValue::Vec(Vector::F32(v)) => DataValue::Vec(Vector::F32(v.mapv(|x| x.abs()))),
        _ => bail!("'abs' requires numbers"),
    })
}

define_op!(OP_SIGNUM, 1, false, true);
pub(crate) fn op_signum(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Num(Num::Int(i)) => DataValue::Num(Num::Int(i.signum())),
        DataValue::Num(Num::Float(f)) => {
            if f.signum() < 0. {
                DataValue::from(-1)
            } else if *f == 0. {
                DataValue::from(0)
            } else if *f > 0. {
                DataValue::from(1)
            } else {
                DataValue::from(f64::NAN)
            }
        }
        _ => bail!("'signum' requires numbers"),
    })
}

define_op!(OP_FLOOR, 1, false, true);
pub(crate) fn op_floor(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Num(Num::Int(i)) => DataValue::Num(Num::Int(*i)),
        DataValue::Num(Num::Float(f)) => DataValue::Num(Num::Float(f.floor())),
        _ => bail!("'floor' requires numbers"),
    })
}

define_op!(OP_CEIL, 1, false, true);
pub(crate) fn op_ceil(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Num(Num::Int(i)) => DataValue::Num(Num::Int(*i)),
        DataValue::Num(Num::Float(f)) => DataValue::Num(Num::Float(f.ceil())),
        _ => bail!("'ceil' requires numbers"),
    })
}

define_op!(OP_ROUND, 1, false, true);
pub(crate) fn op_round(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Num(Num::Int(i)) => DataValue::Num(Num::Int(*i)),
        DataValue::Num(Num::Float(f)) => DataValue::Num(Num::Float(f.round())),
        _ => bail!("'round' requires numbers"),
    })
}

define_op!(OP_EXP, 1, false, true);
pub(crate) fn op_exp(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(Num::Int(i)) => *i as f64,
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Vec(Vector::F32(v)) => {
            return Ok(DataValue::Vec(Vector::F32(v.mapv(|x| x.exp()))));
        }
        DataValue::Vec(Vector::F64(v)) => {
            return Ok(DataValue::Vec(Vector::F64(v.mapv(|x| x.exp()))));
        }
        _ => bail!("'exp' requires numbers"),
    };
    Ok(DataValue::Num(Num::Float(a.exp())))
}

define_op!(OP_EXP2, 1, false, true);
pub(crate) fn op_exp2(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(Num::Int(i)) => *i as f64,
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Vec(Vector::F32(v)) => {
            return Ok(DataValue::Vec(Vector::F32(v.mapv(|x| x.exp2()))));
        }
        DataValue::Vec(Vector::F64(v)) => {
            return Ok(DataValue::Vec(Vector::F64(v.mapv(|x| x.exp2()))));
        }
        _ => bail!("'exp2' requires numbers"),
    };
    Ok(DataValue::Num(Num::Float(a.exp2())))
}

define_op!(OP_LN, 1, false, true);
pub(crate) fn op_ln(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(Num::Int(i)) => *i as f64,
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Vec(Vector::F32(v)) => {
            return Ok(DataValue::Vec(Vector::F32(v.mapv(|x| x.ln()))));
        }
        DataValue::Vec(Vector::F64(v)) => {
            return Ok(DataValue::Vec(Vector::F64(v.mapv(|x| x.ln()))));
        }
        _ => bail!("'ln' requires numbers"),
    };
    Ok(DataValue::Num(Num::Float(a.ln())))
}

define_op!(OP_LOG2, 1, false, true);
pub(crate) fn op_log2(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(Num::Int(i)) => *i as f64,
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Vec(Vector::F32(v)) => {
            return Ok(DataValue::Vec(Vector::F32(v.mapv(|x| x.log2()))));
        }
        DataValue::Vec(Vector::F64(v)) => {
            return Ok(DataValue::Vec(Vector::F64(v.mapv(|x| x.log2()))));
        }
        _ => bail!("'log2' requires numbers"),
    };
    Ok(DataValue::Num(Num::Float(a.log2())))
}

define_op!(OP_LOG10, 1, false, true);
pub(crate) fn op_log10(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(Num::Int(i)) => *i as f64,
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Vec(Vector::F32(v)) => {
            return Ok(DataValue::Vec(Vector::F32(v.mapv(|x| x.log10()))));
        }
        DataValue::Vec(Vector::F64(v)) => {
            return Ok(DataValue::Vec(Vector::F64(v.mapv(|x| x.log10()))));
        }
        _ => bail!("'log10' requires numbers"),
    };
    Ok(DataValue::Num(Num::Float(a.log10())))
}

define_op!(OP_SIN, 1, false, true);
pub(crate) fn op_sin(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(Num::Int(i)) => *i as f64,
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Vec(Vector::F32(v)) => {
            return Ok(DataValue::Vec(Vector::F32(v.mapv(|x| x.sin()))));
        }
        DataValue::Vec(Vector::F64(v)) => {
            return Ok(DataValue::Vec(Vector::F64(v.mapv(|x| x.sin()))));
        }
        _ => bail!("'sin' requires numbers"),
    };
    Ok(DataValue::Num(Num::Float(a.sin())))
}

define_op!(OP_COS, 1, false, true);
pub(crate) fn op_cos(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(Num::Int(i)) => *i as f64,
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Vec(Vector::F32(v)) => {
            return Ok(DataValue::Vec(Vector::F32(v.mapv(|x| x.cos()))));
        }
        DataValue::Vec(Vector::F64(v)) => {
            return Ok(DataValue::Vec(Vector::F64(v.mapv(|x| x.cos()))));
        }
        _ => bail!("'cos' requires numbers"),
    };
    Ok(DataValue::Num(Num::Float(a.cos())))
}

define_op!(OP_TAN, 1, false, true);
pub(crate) fn op_tan(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(Num::Int(i)) => *i as f64,
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Vec(Vector::F32(v)) => {
            return Ok(DataValue::Vec(Vector::F32(v.mapv(|x| x.tan()))));
        }
        DataValue::Vec(Vector::F64(v)) => {
            return Ok(DataValue::Vec(Vector::F64(v.mapv(|x| x.tan()))));
        }
        _ => bail!("'tan' requires numbers"),
    };
    Ok(DataValue::Num(Num::Float(a.tan())))
}

define_op!(OP_ASIN, 1, false, true);
pub(crate) fn op_asin(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(Num::Int(i)) => *i as f64,
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Vec(Vector::F32(v)) => {
            return Ok(DataValue::Vec(Vector::F32(v.mapv(|x| x.asin()))));
        }
        DataValue::Vec(Vector::F64(v)) => {
            return Ok(DataValue::Vec(Vector::F64(v.mapv(|x| x.asin()))));
        }
        _ => bail!("'asin' requires numbers"),
    };
    Ok(DataValue::Num(Num::Float(a.asin())))
}

define_op!(OP_ACOS, 1, false, true);
pub(crate) fn op_acos(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(Num::Int(i)) => *i as f64,
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Vec(Vector::F32(v)) => {
            return Ok(DataValue::Vec(Vector::F32(v.mapv(|x| x.acos()))));
        }
        DataValue::Vec(Vector::F64(v)) => {
            return Ok(DataValue::Vec(Vector::F64(v.mapv(|x| x.acos()))));
        }
        _ => bail!("'acos' requires numbers"),
    };
    Ok(DataValue::Num(Num::Float(a.acos())))
}

define_op!(OP_ATAN, 1, false, true);
pub(crate) fn op_atan(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(Num::Int(i)) => *i as f64,
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Vec(Vector::F32(v)) => {
            return Ok(DataValue::Vec(Vector::F32(v.mapv(|x| x.atan()))));
        }
        DataValue::Vec(Vector::F64(v)) => {
            return Ok(DataValue::Vec(Vector::F64(v.mapv(|x| x.atan()))));
        }
        _ => bail!("'atan' requires numbers"),
    };
    Ok(DataValue::Num(Num::Float(a.atan())))
}

define_op!(OP_ATAN2, 2, false, true);
pub(crate) fn op_atan2(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(Num::Int(i)) => *i as f64,
        DataValue::Num(Num::Float(f)) => *f,
        _ => bail!("'atan2' requires numbers"),
    };
    let b = match &args[1] {
        DataValue::Num(Num::Int(i)) => *i as f64,
        DataValue::Num(Num::Float(f)) => *f,
        _ => bail!("'atan2' requires numbers"),
    };

    Ok(DataValue::Num(Num::Float(a.atan2(b))))
}

define_op!(OP_SINH, 1, false, true);
pub(crate) fn op_sinh(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(Num::Int(i)) => *i as f64,
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Vec(Vector::F32(v)) => {
            return Ok(DataValue::Vec(Vector::F32(v.mapv(|x| x.sinh()))));
        }
        DataValue::Vec(Vector::F64(v)) => {
            return Ok(DataValue::Vec(Vector::F64(v.mapv(|x| x.sinh()))));
        }
        _ => bail!("'sinh' requires numbers"),
    };
    Ok(DataValue::Num(Num::Float(a.sinh())))
}

define_op!(OP_COSH, 1, false, true);
pub(crate) fn op_cosh(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(Num::Int(i)) => *i as f64,
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Vec(Vector::F32(v)) => {
            return Ok(DataValue::Vec(Vector::F32(v.mapv(|x| x.cosh()))));
        }
        DataValue::Vec(Vector::F64(v)) => {
            return Ok(DataValue::Vec(Vector::F64(v.mapv(|x| x.cosh()))));
        }
        _ => bail!("'cosh' requires numbers"),
    };
    Ok(DataValue::Num(Num::Float(a.cosh())))
}

define_op!(OP_TANH, 1, false, true);
pub(crate) fn op_tanh(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(Num::Int(i)) => *i as f64,
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Vec(Vector::F32(v)) => {
            return Ok(DataValue::Vec(Vector::F32(v.mapv(|x| x.tanh()))));
        }
        DataValue::Vec(Vector::F64(v)) => {
            return Ok(DataValue::Vec(Vector::F64(v.mapv(|x| x.tanh()))));
        }
        _ => bail!("'tanh' requires numbers"),
    };
    Ok(DataValue::Num(Num::Float(a.tanh())))
}

define_op!(OP_ASINH, 1, false, true);
pub(crate) fn op_asinh(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(Num::Int(i)) => *i as f64,
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Vec(Vector::F32(v)) => {
            return Ok(DataValue::Vec(Vector::F32(v.mapv(|x| x.asinh()))));
        }
        DataValue::Vec(Vector::F64(v)) => {
            return Ok(DataValue::Vec(Vector::F64(v.mapv(|x| x.asinh()))));
        }
        _ => bail!("'asinh' requires numbers"),
    };
    Ok(DataValue::Num(Num::Float(a.asinh())))
}

define_op!(OP_ACOSH, 1, false, true);
pub(crate) fn op_acosh(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(Num::Int(i)) => *i as f64,
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Vec(Vector::F32(v)) => {
            return Ok(DataValue::Vec(Vector::F32(v.mapv(|x| x.acosh()))));
        }
        DataValue::Vec(Vector::F64(v)) => {
            return Ok(DataValue::Vec(Vector::F64(v.mapv(|x| x.acosh()))));
        }
        _ => bail!("'acosh' requires numbers"),
    };
    Ok(DataValue::Num(Num::Float(a.acosh())))
}

define_op!(OP_ATANH, 1, false, true);
pub(crate) fn op_atanh(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(Num::Int(i)) => *i as f64,
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Vec(Vector::F32(v)) => {
            return Ok(DataValue::Vec(Vector::F32(v.mapv(|x| x.atanh()))));
        }
        DataValue::Vec(Vector::F64(v)) => {
            return Ok(DataValue::Vec(Vector::F64(v.mapv(|x| x.atanh()))));
        }
        _ => bail!("'atanh' requires numbers"),
    };
    Ok(DataValue::Num(Num::Float(a.atanh())))
}

define_op!(OP_SQRT, 1, false, true);
pub(crate) fn op_sqrt(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(Num::Int(i)) => *i as f64,
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Vec(Vector::F32(v)) => {
            return Ok(DataValue::Vec(Vector::F32(v.mapv(|x| x.sqrt()))));
        }
        DataValue::Vec(Vector::F64(v)) => {
            return Ok(DataValue::Vec(Vector::F64(v.mapv(|x| x.sqrt()))));
        }
        _ => bail!("'sqrt' requires numbers"),
    };
    Ok(DataValue::Num(Num::Float(a.sqrt())))
}

define_op!(OP_POW, 2, false, true);
pub(crate) fn op_pow(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(Num::Int(i)) => *i as f64,
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Vec(Vector::F32(v)) => {
            let b = args[1]
                .get_float()
                .ok_or_else(|| miette!("'pow' requires numbers"))?;
            return Ok(DataValue::Vec(Vector::F32(v.mapv(|x| x.powf(b as f32)))));
        }
        DataValue::Vec(Vector::F64(v)) => {
            let b = args[1]
                .get_float()
                .ok_or_else(|| miette!("'pow' requires numbers"))?;
            return Ok(DataValue::Vec(Vector::F64(v.mapv(|x| x.powf(b)))));
        }
        _ => bail!("'pow' requires numbers"),
    };
    let b = match &args[1] {
        DataValue::Num(Num::Int(i)) => *i as f64,
        DataValue::Num(Num::Float(f)) => *f,
        _ => bail!("'pow' requires numbers"),
    };
    Ok(DataValue::Num(Num::Float(a.powf(b))))
}

define_op!(OP_MOD, 2, false, true);
pub(crate) fn op_mod(args: &[DataValue]) -> Result<DataValue> {
    Ok(match (&args[0], &args[1]) {
        (DataValue::Num(Num::Int(a)), DataValue::Num(Num::Int(b))) => {
            if *b == 0 {
                bail!(DivisionByZero { op: "mod" })
            }
            // `i64::MIN % -1` is the one other input pair `Rem` can't
            // service: the mathematical quotient (`i64::MIN / -1`)
            // doesn't fit in i64, so the divide-then-subtract this
            // performs internally overflows too, distinct from the
            // zero-divisor case just above.
            match a.checked_rem(*b) {
                Some(v) => DataValue::Num(Num::Int(v)),
                None => bail!(IntegerOverflow { op: "mod" }),
            }
        }
        (DataValue::Num(Num::Float(a)), DataValue::Num(Num::Float(b))) => {
            if *b == 0.0 {
                bail!(DivisionByZero { op: "mod" })
            }
            DataValue::Num(Num::Float(a.rem(*b)))
        }
        (DataValue::Num(Num::Int(a)), DataValue::Num(Num::Float(b))) => {
            if *b == 0.0 {
                bail!(DivisionByZero { op: "mod" })
            }
            DataValue::Num(Num::Float((*a as f64).rem(b)))
        }
        (DataValue::Num(Num::Float(a)), DataValue::Num(Num::Int(b))) => {
            if *b == 0 {
                bail!(DivisionByZero { op: "mod" })
            }
            DataValue::Num(Num::Float(a.rem(*b as f64)))
        }
        _ => bail!("'mod' requires numbers"),
    })
}

define_op!(OP_NEGATE, 1, false, true);
pub(crate) fn op_negate(args: &[DataValue]) -> Result<DataValue> {
    if let DataValue::Bool(b) = &args[0] {
        Ok(DataValue::from(!*b))
    } else {
        bail!("'negate' requires booleans");
    }
}

define_op!(OP_BIT_AND, 2, false, true);
pub(crate) fn op_bit_and(args: &[DataValue]) -> Result<DataValue> {
    match (&args[0], &args[1]) {
        (DataValue::Bytes(left), DataValue::Bytes(right)) => {
            ensure!(
                left.len() == right.len(),
                "operands of 'bit_and' must have the same lengths"
            );
            let mut ret = left.clone();
            for (l, r) in ret.iter_mut().zip(right.iter()) {
                *l &= *r;
            }
            Ok(DataValue::Bytes(ret))
        }
        _ => bail!("'bit_and' requires bytes"),
    }
}

define_op!(OP_BIT_OR, 2, false, true);
pub(crate) fn op_bit_or(args: &[DataValue]) -> Result<DataValue> {
    match (&args[0], &args[1]) {
        (DataValue::Bytes(left), DataValue::Bytes(right)) => {
            ensure!(
                left.len() == right.len(),
                "operands of 'bit_or' must have the same lengths",
            );
            let mut ret = left.clone();
            for (l, r) in ret.iter_mut().zip(right.iter()) {
                *l |= *r;
            }
            Ok(DataValue::Bytes(ret))
        }
        _ => bail!("'bit_or' requires bytes"),
    }
}

define_op!(OP_BIT_NOT, 1, false, true);
pub(crate) fn op_bit_not(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::Bytes(arg) => {
            let mut ret = arg.clone();
            for l in ret.iter_mut() {
                *l = !*l;
            }
            Ok(DataValue::Bytes(ret))
        }
        _ => bail!("'bit_not' requires bytes"),
    }
}

define_op!(OP_BIT_XOR, 2, false, true);
pub(crate) fn op_bit_xor(args: &[DataValue]) -> Result<DataValue> {
    match (&args[0], &args[1]) {
        (DataValue::Bytes(left), DataValue::Bytes(right)) => {
            ensure!(
                left.len() == right.len(),
                "operands of 'bit_xor' must have the same lengths"
            );
            let mut ret = left.clone();
            for (l, r) in ret.iter_mut().zip(right.iter()) {
                *l ^= *r;
            }
            Ok(DataValue::Bytes(ret))
        }
        _ => bail!("'bit_xor' requires bytes"),
    }
}

define_op!(OP_UNPACK_BITS, 1, false, true);
pub(crate) fn op_unpack_bits(args: &[DataValue]) -> Result<DataValue> {
    if let DataValue::Bytes(bs) = &args[0] {
        let mut ret = vec![false; bs.len() * 8];
        for (chunk, byte) in bs.iter().enumerate() {
            ret[chunk * 8] = (*byte & 0b10000000) != 0;
            ret[chunk * 8 + 1] = (*byte & 0b01000000) != 0;
            ret[chunk * 8 + 2] = (*byte & 0b00100000) != 0;
            ret[chunk * 8 + 3] = (*byte & 0b00010000) != 0;
            ret[chunk * 8 + 4] = (*byte & 0b00001000) != 0;
            ret[chunk * 8 + 5] = (*byte & 0b00000100) != 0;
            ret[chunk * 8 + 6] = (*byte & 0b00000010) != 0;
            ret[chunk * 8 + 7] = (*byte & 0b00000001) != 0;
        }
        Ok(DataValue::List(
            ret.into_iter().map(DataValue::Bool).collect_vec(),
        ))
    } else {
        bail!("'unpack_bits' requires bytes")
    }
}

define_op!(OP_PACK_BITS, 1, false, true);
pub(crate) fn op_pack_bits(args: &[DataValue]) -> Result<DataValue> {
    if let DataValue::List(v) = &args[0] {
        let l = (v.len() as f64 / 8.).ceil() as usize;
        let mut res = vec![0u8; l];
        for (i, b) in v.iter().enumerate() {
            match b {
                DataValue::Bool(b) => {
                    if *b {
                        let chunk = i.div(&8);
                        let idx = i % 8;
                        // In bounds: chunk = i/8 < ceil(v.len()/8) = res.len().
                        let target = &mut res[chunk];
                        match idx {
                            0 => *target |= 0b10000000,
                            1 => *target |= 0b01000000,
                            2 => *target |= 0b00100000,
                            3 => *target |= 0b00010000,
                            4 => *target |= 0b00001000,
                            5 => *target |= 0b00000100,
                            6 => *target |= 0b00000010,
                            // idx = i % 8 is exhaustive over 0..=7.
                            _ => *target |= 0b00000001,
                        }
                    }
                }
                _ => bail!("'pack_bits' requires list of booleans"),
            }
        }
        Ok(DataValue::Bytes(res))
    } else if let DataValue::Set(v) = &args[0] {
        let l = v.iter().cloned().collect_vec();
        op_pack_bits(&[DataValue::List(l)])
    } else {
        bail!("'pack_bits' requires list of booleans")
    }
}

define_op!(OP_CONCAT, 1, true, true);
pub(crate) fn op_concat(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::Str(_) => {
            let mut ret: String = Default::default();
            for arg in args {
                if let DataValue::Str(s) = arg {
                    ret += s;
                } else {
                    bail!("'concat' requires strings, or lists");
                }
            }
            Ok(DataValue::from(ret))
        }
        DataValue::List(_) | DataValue::Set(_) => {
            let mut ret = vec![];
            for arg in args {
                if let DataValue::List(l) = arg {
                    ret.extend_from_slice(l);
                } else if let DataValue::Set(s) = arg {
                    ret.extend(s.iter().cloned());
                } else {
                    bail!("'concat' requires strings, or lists");
                }
            }
            Ok(DataValue::List(ret))
        }
        DataValue::Json(_) => {
            let mut ret = json!(null);
            for arg in args {
                if let DataValue::Json(j) = arg {
                    ret = deep_merge_json(ret, j.0.clone());
                } else {
                    bail!("'concat' requires strings, lists, or JSON objects");
                }
            }
            Ok(DataValue::Json(JsonData(ret)))
        }
        _ => bail!("'concat' requires strings, lists, or JSON objects"),
    }
}

fn deep_merge_json(value1: JsonValue, value2: JsonValue) -> JsonValue {
    match (value1, value2) {
        (JsonValue::Object(mut obj1), JsonValue::Object(obj2)) => {
            for (key, value2) in obj2 {
                let value1 = obj1.remove(&key);
                obj1.insert(key, deep_merge_json(value1.unwrap_or(Value::Null), value2));
            }
            JsonValue::Object(obj1)
        }
        (JsonValue::Array(mut arr1), JsonValue::Array(arr2)) => {
            arr1.extend(arr2);
            JsonValue::Array(arr1)
        }
        (_, value2) => value2,
    }
}

define_op!(OP_STR_INCLUDES, 2, false, true);
pub(crate) fn op_str_includes(args: &[DataValue]) -> Result<DataValue> {
    match (&args[0], &args[1]) {
        (DataValue::Str(l), DataValue::Str(r)) => Ok(DataValue::from(l.find(r as &str).is_some())),
        _ => bail!("'str_includes' requires strings"),
    }
}

define_op!(OP_LOWERCASE, 1, false, true);
pub(crate) fn op_lowercase(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::Str(s) => Ok(DataValue::from(s.to_lowercase())),
        _ => bail!("'lowercase' requires strings"),
    }
}

define_op!(OP_UPPERCASE, 1, false, true);
pub(crate) fn op_uppercase(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::Str(s) => Ok(DataValue::from(s.to_uppercase())),
        _ => bail!("'uppercase' requires strings"),
    }
}

define_op!(OP_TRIM, 1, false, true);
pub(crate) fn op_trim(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::Str(s) => Ok(DataValue::from(s.trim())),
        _ => bail!("'trim' requires strings"),
    }
}

define_op!(OP_TRIM_START, 1, false, true);
pub(crate) fn op_trim_start(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::Str(s) => Ok(DataValue::from(s.trim_start())),
        v => bail!("'trim_start' requires strings, got {}", v),
    }
}

define_op!(OP_TRIM_END, 1, false, true);
pub(crate) fn op_trim_end(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::Str(s) => Ok(DataValue::from(s.trim_end())),
        _ => bail!("'trim_end' requires strings"),
    }
}

define_op!(OP_STARTS_WITH, 2, false, true);
pub(crate) fn op_starts_with(args: &[DataValue]) -> Result<DataValue> {
    match (&args[0], &args[1]) {
        (DataValue::Str(l), DataValue::Str(r)) => Ok(DataValue::from(l.starts_with(r as &str))),
        (DataValue::Bytes(l), DataValue::Bytes(r)) => {
            Ok(DataValue::from(l.starts_with(r as &[u8])))
        }
        _ => bail!("'starts_with' requires strings or bytes"),
    }
}

define_op!(OP_ENDS_WITH, 2, false, true);
pub(crate) fn op_ends_with(args: &[DataValue]) -> Result<DataValue> {
    match (&args[0], &args[1]) {
        (DataValue::Str(l), DataValue::Str(r)) => Ok(DataValue::from(l.ends_with(r as &str))),
        (DataValue::Bytes(l), DataValue::Bytes(r)) => Ok(DataValue::from(l.ends_with(r as &[u8]))),
        _ => bail!("'ends_with' requires strings or bytes"),
    }
}

// ⚠ OP_REGEX is not user-callable (it has no `get_op` entry): the parser
// injects it around the pattern argument of every `OP_REGEX_*` application
// via `Op::post_process_args` — a hidden AST rewrite that hoists regex
// compilation to compile time. Constant patterns are compiled once by
// constant folding; invalid ones are rejected before the query runs.
define_op!(OP_REGEX, 1, false, true);
pub(crate) fn op_regex(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        r @ DataValue::Regex(_) => r.clone(),
        DataValue::Str(s) => {
            DataValue::Regex(RegexWrapper(regex::Regex::new(s).map_err(|err| {
                miette!("The string cannot be interpreted as regex: {}", err)
            })?))
        }
        _ => bail!("'regex' requires strings"),
    })
}

define_op!(OP_REGEX_MATCHES, 2, false, true);
pub(crate) fn op_regex_matches(args: &[DataValue]) -> Result<DataValue> {
    match (&args[0], &args[1]) {
        (DataValue::Str(s), DataValue::Regex(r)) => Ok(DataValue::from(r.0.is_match(s))),
        _ => bail!("'regex_matches' requires strings"),
    }
}

define_op!(OP_REGEX_REPLACE, 3, false, true);
pub(crate) fn op_regex_replace(args: &[DataValue]) -> Result<DataValue> {
    match (&args[0], &args[1], &args[2]) {
        (DataValue::Str(s), DataValue::Regex(r), DataValue::Str(rp)) => {
            Ok(DataValue::Str(r.0.replace(s, rp as &str).into()))
        }
        _ => bail!("'regex_replace' requires strings"),
    }
}

define_op!(OP_REGEX_REPLACE_ALL, 3, false, true);
pub(crate) fn op_regex_replace_all(args: &[DataValue]) -> Result<DataValue> {
    match (&args[0], &args[1], &args[2]) {
        (DataValue::Str(s), DataValue::Regex(r), DataValue::Str(rp)) => {
            Ok(DataValue::Str(r.0.replace_all(s, rp as &str).into()))
        }
        _ => bail!("'regex_replace' requires strings"),
    }
}

define_op!(OP_REGEX_EXTRACT, 2, false, true);
pub(crate) fn op_regex_extract(args: &[DataValue]) -> Result<DataValue> {
    match (&args[0], &args[1]) {
        (DataValue::Str(s), DataValue::Regex(r)) => {
            let found =
                r.0.find_iter(s)
                    .map(|v| DataValue::from(v.as_str()))
                    .collect_vec();
            Ok(DataValue::List(found))
        }
        _ => bail!("'regex_extract' requires strings"),
    }
}

define_op!(OP_REGEX_EXTRACT_FIRST, 2, false, true);
pub(crate) fn op_regex_extract_first(args: &[DataValue]) -> Result<DataValue> {
    match (&args[0], &args[1]) {
        (DataValue::Str(s), DataValue::Regex(r)) => {
            let found = r.0.find(s).map(|v| DataValue::from(v.as_str()));
            Ok(found.unwrap_or(DataValue::Null))
        }
        _ => bail!("'regex_extract_first' requires strings"),
    }
}

define_op!(OP_T2S, 1, false, true);
fn op_t2s(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Str(s) => DataValue::Str(fast2s::convert(s).into()),
        d => d.clone(),
    })
}

define_op!(OP_IS_NULL, 1, false, true);
pub(crate) fn op_is_null(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(matches!(args[0], DataValue::Null)))
}

define_op!(OP_IS_INT, 1, false, true);
pub(crate) fn op_is_int(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(matches!(
        args[0],
        DataValue::Num(Num::Int(_))
    )))
}

define_op!(OP_IS_FLOAT, 1, false, true);
pub(crate) fn op_is_float(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(matches!(
        args[0],
        DataValue::Num(Num::Float(_))
    )))
}

define_op!(OP_IS_NUM, 1, false, true);
pub(crate) fn op_is_num(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(matches!(
        args[0],
        DataValue::Num(Num::Int(_)) | DataValue::Num(Num::Float(_))
    )))
}

define_op!(OP_IS_FINITE, 1, false, true);
pub(crate) fn op_is_finite(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(match &args[0] {
        DataValue::Num(Num::Int(_)) => true,
        DataValue::Num(Num::Float(f)) => f.is_finite(),
        _ => false,
    }))
}

define_op!(OP_IS_INFINITE, 1, false, true);
pub(crate) fn op_is_infinite(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(match &args[0] {
        DataValue::Num(Num::Float(f)) => f.is_infinite(),
        _ => false,
    }))
}

define_op!(OP_IS_NAN, 1, false, true);
pub(crate) fn op_is_nan(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(match &args[0] {
        DataValue::Num(Num::Float(f)) => f.is_nan(),
        _ => false,
    }))
}

define_op!(OP_IS_STRING, 1, false, true);
pub(crate) fn op_is_string(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(matches!(args[0], DataValue::Str(_))))
}

define_op!(OP_IS_LIST, 1, false, true);
pub(crate) fn op_is_list(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(matches!(
        args[0],
        DataValue::List(_) | DataValue::Set(_)
    )))
}

define_op!(OP_IS_VEC, 1, false, true);
pub(crate) fn op_is_vec(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(matches!(args[0], DataValue::Vec(_))))
}

define_op!(OP_APPEND, 2, false, true);
pub(crate) fn op_append(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::List(l) => {
            let mut l = l.clone();
            l.push(args[1].clone());
            Ok(DataValue::List(l))
        }
        DataValue::Set(l) => {
            let mut l = l.iter().cloned().collect_vec();
            l.push(args[1].clone());
            Ok(DataValue::List(l))
        }
        _ => bail!("'append' requires first argument to be a list"),
    }
}

define_op!(OP_PREPEND, 2, false, true);
pub(crate) fn op_prepend(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::List(pl) => {
            let mut l = vec![args[1].clone()];
            l.extend_from_slice(pl);
            Ok(DataValue::List(l))
        }
        DataValue::Set(pl) => {
            let mut l = vec![args[1].clone()];
            l.extend(pl.iter().cloned());
            Ok(DataValue::List(l))
        }
        _ => bail!("'prepend' requires first argument to be a list"),
    }
}

define_op!(OP_IS_BYTES, 1, false, true);
pub(crate) fn op_is_bytes(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(matches!(args[0], DataValue::Bytes(_))))
}

define_op!(OP_LENGTH, 1, false, true);
pub(crate) fn op_length(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(match &args[0] {
        DataValue::Set(s) => s.len() as i64,
        DataValue::List(l) => l.len() as i64,
        DataValue::Str(s) => s.chars().count() as i64,
        DataValue::Bytes(b) => b.len() as i64,
        DataValue::Vec(v) => v.len() as i64,
        _ => bail!("'length' requires lists"),
    }))
}

define_op!(OP_UNICODE_NORMALIZE, 2, false, true);
pub(crate) fn op_unicode_normalize(args: &[DataValue]) -> Result<DataValue> {
    match (&args[0], &args[1]) {
        (DataValue::Str(s), DataValue::Str(n)) => Ok(DataValue::Str(match n as &str {
            "nfc" => s.nfc().collect(),
            "nfd" => s.nfd().collect(),
            "nfkc" => s.nfkc().collect(),
            "nfkd" => s.nfkd().collect(),
            u => bail!("unknown normalization {} for 'unicode_normalize'", u),
        })),
        _ => bail!("'unicode_normalize' requires strings"),
    }
}

define_op!(OP_SORTED, 1, false, true);
pub(crate) fn op_sorted(args: &[DataValue]) -> Result<DataValue> {
    let mut arg = args[0]
        .get_slice()
        .ok_or_else(|| miette!("'sort' requires lists"))?
        .to_vec();
    arg.sort();
    Ok(DataValue::List(arg))
}

define_op!(OP_REVERSE, 1, false, true);
pub(crate) fn op_reverse(args: &[DataValue]) -> Result<DataValue> {
    let mut arg = args[0]
        .get_slice()
        .ok_or_else(|| miette!("'reverse' requires lists"))?
        .to_vec();
    arg.reverse();
    Ok(DataValue::List(arg))
}

define_op!(OP_HAVERSINE, 4, false, true);
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

define_op!(OP_HAVERSINE_DEG_INPUT, 4, false, true);
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

define_op!(OP_DEG_TO_RAD, 1, false, true);
pub(crate) fn op_deg_to_rad(args: &[DataValue]) -> Result<DataValue> {
    let x = args[0]
        .get_float()
        .ok_or_else(|| miette!("'deg_to_rad' requires numbers"))?;
    Ok(DataValue::from(x * std::f64::consts::PI / 180.))
}

define_op!(OP_RAD_TO_DEG, 1, false, true);
pub(crate) fn op_rad_to_deg(args: &[DataValue]) -> Result<DataValue> {
    let x = args[0]
        .get_float()
        .ok_or_else(|| miette!("'rad_to_deg' requires numbers"))?;
    Ok(DataValue::from(x * 180. / std::f64::consts::PI))
}

define_op!(OP_FIRST, 1, false, true);
pub(crate) fn op_first(args: &[DataValue]) -> Result<DataValue> {
    Ok(args[0]
        .get_slice()
        .ok_or_else(|| miette!("'first' requires lists"))?
        .first()
        .cloned()
        .unwrap_or(DataValue::Null))
}

define_op!(OP_LAST, 1, false, true);
pub(crate) fn op_last(args: &[DataValue]) -> Result<DataValue> {
    Ok(args[0]
        .get_slice()
        .ok_or_else(|| miette!("'last' requires lists"))?
        .last()
        .cloned()
        .unwrap_or(DataValue::Null))
}

define_op!(OP_CHUNKS, 2, false, true);
pub(crate) fn op_chunks(args: &[DataValue]) -> Result<DataValue> {
    let arg = args[0]
        .get_slice()
        .ok_or_else(|| miette!("first argument of 'chunks' must be a list"))?;
    let n = args[1]
        .get_int()
        .ok_or_else(|| miette!("second argument of 'chunks' must be an integer"))?;
    ensure!(n > 0, "second argument to 'chunks' must be positive");
    let res = arg
        .chunks(n as usize)
        .map(|el| DataValue::List(el.to_vec()))
        .collect_vec();
    Ok(DataValue::List(res))
}

define_op!(OP_CHUNKS_EXACT, 2, false, true);
pub(crate) fn op_chunks_exact(args: &[DataValue]) -> Result<DataValue> {
    let arg = args[0]
        .get_slice()
        .ok_or_else(|| miette!("first argument of 'chunks_exact' must be a list"))?;
    let n = args[1]
        .get_int()
        .ok_or_else(|| miette!("second argument of 'chunks_exact' must be an integer"))?;
    ensure!(n > 0, "second argument to 'chunks_exact' must be positive");
    let res = arg
        .chunks_exact(n as usize)
        .map(|el| DataValue::List(el.to_vec()))
        .collect_vec();
    Ok(DataValue::List(res))
}

define_op!(OP_WINDOWS, 2, false, true);
pub(crate) fn op_windows(args: &[DataValue]) -> Result<DataValue> {
    let arg = args[0]
        .get_slice()
        .ok_or_else(|| miette!("first argument of 'windows' must be a list"))?;
    let n = args[1]
        .get_int()
        .ok_or_else(|| miette!("second argument of 'windows' must be an integer"))?;
    ensure!(n > 0, "second argument to 'windows' must be positive");
    let res = arg
        .windows(n as usize)
        .map(|el| DataValue::List(el.to_vec()))
        .collect_vec();
    Ok(DataValue::List(res))
}

fn get_index(mut i: i64, total: usize, is_upper: bool) -> Result<usize> {
    if i < 0 {
        i += total as i64;
    }
    Ok(if i >= 0 {
        let i = i as usize;
        if i > total || (!is_upper && i == total) {
            bail!("index {} out of bound", i)
        } else {
            i
        }
    } else {
        bail!("index {} out of bound", i)
    })
}

define_op!(OP_GET, 2, true, true);
pub(crate) fn op_get(args: &[DataValue]) -> Result<DataValue> {
    match get_impl(args) {
        Ok(res) => Ok(res),
        Err(err) => {
            if let Some(default) = args.get(2) {
                Ok(default.clone())
            } else {
                Err(err)
            }
        }
    }
}

fn get_impl(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::List(l) => {
            let n = args[1]
                .get_int()
                .ok_or_else(|| miette!("second argument to 'get' mut be an integer"))?;
            let idx = get_index(n, l.len(), false)?;
            Ok(l[idx].clone())
        }
        DataValue::Json(json) => {
            let res = match &args[1] {
                DataValue::Str(s) => json
                    .get(s as &str)
                    .ok_or_else(|| miette!("key '{}' not found in json", s))?
                    .clone(),
                DataValue::Num(i) => {
                    let i = i
                        .get_int()
                        .ok_or_else(|| miette!("index '{}' not found in json", i))?;
                    json.get(i as usize)
                        .ok_or_else(|| miette!("index '{}' not found in json", i))?
                        .clone()
                }
                DataValue::List(l) => get_json_path_immutable(json, l)?.clone(),
                _ => bail!("second argument to 'get' mut be a string or integer"),
            };
            let res = json2val(res);
            Ok(res)
        }
        _ => bail!("first argument to 'get' mut be a list or json"),
    }
}

fn json2val(res: Value) -> DataValue {
    match res {
        Value::Null => DataValue::Null,
        Value::Bool(b) => DataValue::Bool(b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                DataValue::from(i)
            } else if let Some(f) = n.as_f64() {
                DataValue::from(f)
            } else {
                DataValue::Null
            }
        }
        Value::String(s) => DataValue::Str(SmartString::from(s)),
        Value::Array(arr) => DataValue::Json(JsonData(json!(arr))),
        Value::Object(obj) => DataValue::Json(JsonData(json!(obj))),
    }
}

define_op!(OP_MAYBE_GET, 2, false, true);
pub(crate) fn op_maybe_get(args: &[DataValue]) -> Result<DataValue> {
    match get_impl(args) {
        Ok(res) => Ok(res),
        Err(_) => Ok(DataValue::Null),
    }
}

define_op!(OP_SLICE, 3, false, true);
pub(crate) fn op_slice(args: &[DataValue]) -> Result<DataValue> {
    let l = args[0]
        .get_slice()
        .ok_or_else(|| miette!("first argument to 'slice' mut be a list"))?;
    let m = args[1]
        .get_int()
        .ok_or_else(|| miette!("second argument to 'slice' mut be an integer"))?;
    let n = args[2]
        .get_int()
        .ok_or_else(|| miette!("third argument to 'slice' mut be an integer"))?;
    let m = get_index(m, l.len(), false)?;
    let n = get_index(n, l.len(), true)?;
    Ok(DataValue::List(l[m..n].to_vec()))
}

define_op!(OP_CHARS, 1, false, true);
pub(crate) fn op_chars(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::List(
        args[0]
            .get_str()
            .ok_or_else(|| miette!("'chars' requires strings"))?
            .chars()
            .map(|c| {
                let mut s = SmartString::new();
                s.push(c);
                DataValue::Str(s)
            })
            .collect_vec(),
    ))
}

define_op!(OP_SLICE_STRING, 3, false, true);
pub(crate) fn op_slice_string(args: &[DataValue]) -> Result<DataValue> {
    let s = args[0]
        .get_str()
        .ok_or_else(|| miette!("first argument to 'slice_string' mut be a string"))?;
    let m = args[1]
        .get_int()
        .ok_or_else(|| miette!("second argument to 'slice_string' mut be an integer"))?;
    ensure!(
        m >= 0,
        "second argument to 'slice_string' mut be a positive integer"
    );
    let n = args[2]
        .get_int()
        .ok_or_else(|| miette!("third argument to 'slice_string' mut be an integer"))?;
    ensure!(
        n >= m,
        "third argument to 'slice_string' mut be a positive integer greater than the second argument"
    );
    Ok(DataValue::Str(
        s.chars().skip(m as usize).take((n - m) as usize).collect(),
    ))
}

define_op!(OP_FROM_SUBSTRINGS, 1, false, true);
pub(crate) fn op_from_substrings(args: &[DataValue]) -> Result<DataValue> {
    let mut ret = String::new();
    match &args[0] {
        DataValue::List(ss) => {
            for arg in ss {
                if let DataValue::Str(s) = arg {
                    ret.push_str(s);
                } else {
                    bail!("'from_substring' requires a list of strings")
                }
            }
        }
        DataValue::Set(ss) => {
            for arg in ss {
                if let DataValue::Str(s) = arg {
                    ret.push_str(s);
                } else {
                    bail!("'from_substring' requires a list of strings")
                }
            }
        }
        _ => bail!("'from_substring' requires a list of strings"),
    }
    Ok(DataValue::from(ret))
}

define_op!(OP_ENCODE_BASE64, 1, false, true);
pub(crate) fn op_encode_base64(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::Bytes(b) => {
            let s = STANDARD.encode(b);
            Ok(DataValue::from(s))
        }
        _ => bail!("'encode_base64' requires bytes"),
    }
}

define_op!(OP_DECODE_BASE64, 1, false, true);
pub(crate) fn op_decode_base64(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::Str(s) => {
            let b = STANDARD
                .decode(s)
                .map_err(|_| miette!("Data is not properly encoded"))?;
            Ok(DataValue::Bytes(b))
        }
        _ => bail!("'decode_base64' requires strings"),
    }
}

define_op!(OP_TO_BOOL, 1, false, true);
pub(crate) fn op_to_bool(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(match &args[0] {
        DataValue::Null => false,
        DataValue::Bool(b) => *b,
        DataValue::Num(n) => n.get_int() != Some(0),
        DataValue::Str(s) => !s.is_empty(),
        DataValue::Bytes(b) => !b.is_empty(),
        DataValue::Uuid(u) => !u.0.is_nil(),
        DataValue::Regex(r) => !r.0.as_str().is_empty(),
        DataValue::List(l) => !l.is_empty(),
        DataValue::Set(s) => !s.is_empty(),
        DataValue::Vec(_) => true,
        DataValue::Validity(vld) => vld.is_assert.0,
        DataValue::Interval(_) => true,
        DataValue::Bot => false,
        DataValue::Json(json) => match &json.0 {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_i64() != Some(0),
            Value::String(s) => !s.is_empty(),
            Value::Array(a) => !a.is_empty(),
            Value::Object(o) => !o.is_empty(),
        },
    }))
}

define_op!(OP_TO_UNITY, 1, false, true);
pub(crate) fn op_to_unity(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(match &args[0] {
        DataValue::Null => 0,
        DataValue::Bool(b) => *b as i64,
        DataValue::Num(n) => (n.get_float() != 0.) as i64,
        DataValue::Str(s) => i64::from(!s.is_empty()),
        DataValue::Bytes(b) => i64::from(!b.is_empty()),
        DataValue::Uuid(u) => i64::from(!u.0.is_nil()),
        DataValue::Regex(r) => i64::from(!r.0.as_str().is_empty()),
        DataValue::List(l) => i64::from(!l.is_empty()),
        DataValue::Set(s) => i64::from(!s.is_empty()),
        DataValue::Vec(_) => 1,
        DataValue::Validity(vld) => i64::from(vld.is_assert.0),
        DataValue::Interval(_) => 1,
        DataValue::Bot => 0,
        DataValue::Json(json) => match &json.0 {
            Value::Null => 0,
            Value::Bool(b) => *b as i64,
            Value::Number(n) => (n.as_i64() != Some(0)) as i64,
            Value::String(s) => !s.is_empty() as i64,
            Value::Array(a) => !a.is_empty() as i64,
            Value::Object(o) => !o.is_empty() as i64,
        },
    }))
}

define_op!(OP_TO_INT, 1, false, true);
pub(crate) fn op_to_int(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Num(n) => match n.get_int() {
            None => {
                let f = n.get_float();
                DataValue::Num(Num::Int(f as i64))
            }
            Some(i) => DataValue::Num(Num::Int(i)),
        },
        DataValue::Null => DataValue::from(0),
        DataValue::Bool(b) => DataValue::from(if *b { 1 } else { 0 }),
        DataValue::Str(t) => {
            let s = t as &str;
            i64::from_str(s)
                .map_err(|_| miette!("The string cannot be interpreted as int"))?
                .into()
        }
        DataValue::Validity(vld) => DataValue::Num(Num::Int(vld.timestamp.0.0)),
        v => bail!("'to_int' does not recognize {:?}", v),
    })
}

define_op!(OP_TO_FLOAT, 1, false, true);
pub(crate) fn op_to_float(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Num(n) => n.get_float().into(),
        DataValue::Null => DataValue::from(0.0),
        DataValue::Bool(b) => DataValue::from(if *b { 1.0 } else { 0.0 }),
        DataValue::Str(t) => match t as &str {
            "PI" => std::f64::consts::PI.into(),
            "E" => std::f64::consts::E.into(),
            "NAN" => f64::NAN.into(),
            "INF" => f64::INFINITY.into(),
            "NEG_INF" => f64::NEG_INFINITY.into(),
            s => f64::from_str(s)
                .map_err(|_| miette!("The string cannot be interpreted as float"))?
                .into(),
        },
        v => bail!("'to_float' does not recognize {:?}", v),
    })
}

define_op!(OP_TO_STRING, 1, false, true);
pub(crate) fn op_to_string(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::Str(val2str(&args[0]).into()))
}

fn val2str(arg: &DataValue) -> String {
    match arg {
        DataValue::Str(s) => s.to_string(),
        DataValue::Json(JsonData(JsonValue::String(s))) => s.clone(),
        v => {
            let jv = to_json(v);
            jv.to_string()
        }
    }
}

/// The optional trailing element-type argument shared by `vec` and
/// `rand_vec`.
fn vec_element_type(arg: Option<&DataValue>, op: &str) -> Result<VecElementType> {
    match arg {
        Some(DataValue::Str(s)) => match s as &str {
            "F32" | "Float" => Ok(VecElementType::F32),
            "F64" | "Double" => Ok(VecElementType::F64),
            _ => bail!("'{op}' does not recognize type {s}"),
        },
        None => Ok(VecElementType::F32),
        _ => bail!("'{op}' requires a string as second argument"),
    }
}

define_op!(OP_VEC, 1, true, true);
pub(crate) fn op_vec(args: &[DataValue]) -> Result<DataValue> {
    let t = vec_element_type(args.get(1), "vec")?;

    match &args[0] {
        DataValue::Json(j) => {
            // Non-array JSON is a typed error; the CozoDB original unwrapped
            // `as_array()` here and aborted on e.g. `vec(json('{}'))`.
            let arr =
                j.0.as_array()
                    .ok_or_else(|| miette!("'vec' requires a JSON array, got {}", j.0))?;
            match t {
                VecElementType::F32 => {
                    let mut res_arr = ndarray::Array1::zeros(arr.len());
                    for (mut row, el) in res_arr.axis_iter_mut(ndarray::Axis(0)).zip(arr.iter()) {
                        let f = el
                            .as_f64()
                            .ok_or_else(|| miette!("'vec' requires a list of numbers"))?;
                        row.fill(f as f32);
                    }
                    Ok(DataValue::Vec(Vector::F32(res_arr)))
                }
                VecElementType::F64 => {
                    let mut res_arr = ndarray::Array1::zeros(arr.len());
                    for (mut row, el) in res_arr.axis_iter_mut(ndarray::Axis(0)).zip(arr.iter()) {
                        let f = el
                            .as_f64()
                            .ok_or_else(|| miette!("'vec' requires a list of numbers"))?;
                        row.fill(f);
                    }
                    Ok(DataValue::Vec(Vector::F64(res_arr)))
                }
            }
        }
        DataValue::List(l) => match t {
            VecElementType::F32 => {
                let mut res_arr = ndarray::Array1::zeros(l.len());
                for (mut row, el) in res_arr.axis_iter_mut(ndarray::Axis(0)).zip(l.iter()) {
                    let f = el
                        .get_float()
                        .ok_or_else(|| miette!("'vec' requires a list of numbers"))?;
                    row.fill(f as f32);
                }
                Ok(DataValue::Vec(Vector::F32(res_arr)))
            }
            VecElementType::F64 => {
                let mut res_arr = ndarray::Array1::zeros(l.len());
                for (mut row, el) in res_arr.axis_iter_mut(ndarray::Axis(0)).zip(l.iter()) {
                    let f = el
                        .get_float()
                        .ok_or_else(|| miette!("'vec' requires a list of numbers"))?;
                    row.fill(f);
                }
                Ok(DataValue::Vec(Vector::F64(res_arr)))
            }
        },
        DataValue::Vec(v) => match (t, v) {
            (VecElementType::F32, Vector::F32(v)) => Ok(DataValue::Vec(Vector::F32(v.clone()))),
            (VecElementType::F64, Vector::F64(v)) => Ok(DataValue::Vec(Vector::F64(v.clone()))),
            (VecElementType::F32, Vector::F64(v)) => {
                Ok(DataValue::Vec(Vector::F32(v.mapv(|x| x as f32))))
            }
            (VecElementType::F64, Vector::F32(v)) => {
                Ok(DataValue::Vec(Vector::F64(v.mapv(|x| x as f64))))
            }
        },
        DataValue::Str(s) => {
            // Base64-encoded raw floats, decoded little-endian by definition
            // — the same convention as the stored `Vector` representation in
            // `data/value.rs`. The CozoDB original reinterpreted the bytes
            // in place through an unsafe pointer cast: native-endian (the
            // stored value would differ across platforms) and undefined
            // behaviour on an unaligned buffer. Trailing bytes short of a
            // full element are an error (the CozoDB original silently
            // ignored them) — the same exact-length law the schema
            // coercion path applies.
            let bytes = STANDARD
                .decode(s)
                .map_err(|_| miette!("Data is not base64 encoded"))?;
            let el_size = match t {
                VecElementType::F32 => 4,
                VecElementType::F64 => 8,
            };
            if bytes.len() % el_size != 0 {
                bail!(
                    "vector byte payload of length {} is not a whole number of {}-byte elements",
                    bytes.len(),
                    el_size
                );
            }
            match t {
                VecElementType::F32 => {
                    let v: Vec<f32> = bytes
                        .chunks_exact(4)
                        // In bounds: `chunks_exact(4)` yields 4-byte chunks.
                        .map(|c| f32::from_le_bytes(c.try_into().expect("chunk of 4")))
                        .collect();
                    Ok(DataValue::Vec(Vector::F32(ndarray::Array1::from(v))))
                }
                VecElementType::F64 => {
                    let v: Vec<f64> = bytes
                        .chunks_exact(8)
                        // In bounds: `chunks_exact(8)` yields 8-byte chunks.
                        .map(|c| f64::from_le_bytes(c.try_into().expect("chunk of 8")))
                        .collect();
                    Ok(DataValue::Vec(Vector::F64(ndarray::Array1::from(v))))
                }
            }
        }
        _ => bail!("'vec' requires a list or a vector"),
    }
}

// Nondeterministic: fresh randomness per evaluation; never constant-folded.
define_op!(OP_RAND_VEC, 1, true, false);
pub(crate) fn op_rand_vec(args: &[DataValue]) -> Result<DataValue> {
    let len = args[0]
        .get_int()
        .ok_or_else(|| miette!("'rand_vec' requires an integer"))? as usize;
    let t = vec_element_type(args.get(1), "rand_vec")?;

    let mut rng = rand::rng();
    match t {
        VecElementType::F32 => {
            let mut res_arr = ndarray::Array1::zeros(len);
            for mut row in res_arr.axis_iter_mut(ndarray::Axis(0)) {
                row.fill(rng.random::<f64>() as f32);
            }
            Ok(DataValue::Vec(Vector::F32(res_arr)))
        }
        VecElementType::F64 => {
            let mut res_arr = ndarray::Array1::zeros(len);
            for mut row in res_arr.axis_iter_mut(ndarray::Axis(0)) {
                row.fill(rng.random::<f64>());
            }
            Ok(DataValue::Vec(Vector::F64(res_arr)))
        }
    }
}

define_op!(OP_L2_NORMALIZE, 1, false, true);
pub(crate) fn op_l2_normalize(args: &[DataValue]) -> Result<DataValue> {
    let a = &args[0];
    match a {
        DataValue::Vec(Vector::F32(a)) => {
            let norm = a.dot(a).sqrt();
            Ok(DataValue::Vec(Vector::F32(a / norm)))
        }
        DataValue::Vec(Vector::F64(a)) => {
            let norm = a.dot(a).sqrt();
            Ok(DataValue::Vec(Vector::F64(a / norm)))
        }
        _ => bail!("'l2_normalize' requires a vector"),
    }
}

define_op!(OP_L2_DIST, 2, false, true);
pub(crate) fn op_l2_dist(args: &[DataValue]) -> Result<DataValue> {
    let a = &args[0];
    let b = &args[1];
    match (a, b) {
        (DataValue::Vec(Vector::F32(a)), DataValue::Vec(Vector::F32(b))) => {
            if a.len() != b.len() {
                bail!("'l2_dist' requires two vectors of the same length");
            }
            let diff = a - b;
            Ok(DataValue::from(diff.dot(&diff) as f64))
        }
        (DataValue::Vec(Vector::F64(a)), DataValue::Vec(Vector::F64(b))) => {
            if a.len() != b.len() {
                bail!("'l2_dist' requires two vectors of the same length");
            }
            let diff = a - b;
            Ok(DataValue::from(diff.dot(&diff)))
        }
        _ => bail!("'l2_dist' requires two vectors of the same type"),
    }
}

define_op!(OP_IP_DIST, 2, false, true);
pub(crate) fn op_ip_dist(args: &[DataValue]) -> Result<DataValue> {
    let a = &args[0];
    let b = &args[1];
    match (a, b) {
        (DataValue::Vec(Vector::F32(a)), DataValue::Vec(Vector::F32(b))) => {
            if a.len() != b.len() {
                bail!("'ip_dist' requires two vectors of the same length");
            }
            let dot = a.dot(b);
            Ok(DataValue::from(1. - dot as f64))
        }
        (DataValue::Vec(Vector::F64(a)), DataValue::Vec(Vector::F64(b))) => {
            if a.len() != b.len() {
                bail!("'ip_dist' requires two vectors of the same length");
            }
            let dot = a.dot(b);
            Ok(DataValue::from(1. - dot))
        }
        _ => bail!("'ip_dist' requires two vectors of the same type"),
    }
}

define_op!(OP_COS_DIST, 2, false, true);
pub(crate) fn op_cos_dist(args: &[DataValue]) -> Result<DataValue> {
    let a = &args[0];
    let b = &args[1];
    match (a, b) {
        (DataValue::Vec(Vector::F32(a)), DataValue::Vec(Vector::F32(b))) => {
            if a.len() != b.len() {
                bail!("'cos_dist' requires two vectors of the same length");
            }
            let a_norm = a.dot(a) as f64;
            let b_norm = b.dot(b) as f64;
            let dot = a.dot(b) as f64;
            Ok(DataValue::from(1. - dot / (a_norm * b_norm).sqrt()))
        }
        (DataValue::Vec(Vector::F64(a)), DataValue::Vec(Vector::F64(b))) => {
            if a.len() != b.len() {
                bail!("'cos_dist' requires two vectors of the same length");
            }
            let a_norm = a.dot(a);
            let b_norm = b.dot(b);
            let dot = a.dot(b);
            Ok(DataValue::from(1. - dot / (a_norm * b_norm).sqrt()))
        }
        _ => bail!("'cos_dist' requires two vectors of the same type"),
    }
}

define_op!(OP_INT_RANGE, 1, true, true);
pub(crate) fn op_int_range(args: &[DataValue]) -> Result<DataValue> {
    let [start, end] = match args.len() {
        1 => {
            let end = args[0]
                .get_int()
                .ok_or_else(|| miette!("'int_range' requires integer argument for end"))?;
            [0, end]
        }
        2 => {
            let start = args[0]
                .get_int()
                .ok_or_else(|| miette!("'int_range' requires integer argument for start"))?;
            let end = args[1]
                .get_int()
                .ok_or_else(|| miette!("'int_range' requires integer argument for end"))?;
            [start, end]
        }
        3 => {
            let start = args[0]
                .get_int()
                .ok_or_else(|| miette!("'int_range' requires integer argument for start"))?;
            let end = args[1]
                .get_int()
                .ok_or_else(|| miette!("'int_range' requires integer argument for end"))?;
            let step = args[2]
                .get_int()
                .ok_or_else(|| miette!("'int_range' requires integer argument for step"))?;
            let mut current = start;
            let mut result = vec![];
            if step > 0 {
                while current < end {
                    result.push(DataValue::from(current));
                    // Checked: a step landing past i64::MAX would otherwise
                    // wrap (or abort in debug builds) near the type's edge.
                    current = match current.checked_add(step) {
                        Some(nxt) => nxt,
                        None => break,
                    };
                }
            } else {
                while current > end {
                    result.push(DataValue::from(current));
                    current = match current.checked_add(step) {
                        Some(nxt) => nxt,
                        None => break,
                    };
                }
            }
            return Ok(DataValue::List(result));
        }
        _ => bail!("'int_range' requires 1 to 3 argument"),
    };
    Ok(DataValue::List((start..end).map(DataValue::from).collect()))
}

// Nondeterministic: fresh randomness per evaluation; never constant-folded.
// (The CozoDB original folded an all-constant call at compile time, silently
// freezing it into one per-query number.)
define_op!(OP_RAND_FLOAT, 0, false, false);
pub(crate) fn op_rand_float(_args: &[DataValue]) -> Result<DataValue> {
    Ok(rand::rng().random::<f64>().into())
}

// Nondeterministic: fresh randomness per evaluation; never constant-folded.
define_op!(OP_RAND_BERNOULLI, 1, false, false);
pub(crate) fn op_rand_bernoulli(args: &[DataValue]) -> Result<DataValue> {
    let prob = match &args[0] {
        DataValue::Num(n) => {
            let f = n.get_float();
            ensure!(
                (0. ..=1.).contains(&f),
                "'rand_bernoulli' requires number between 0. and 1."
            );
            f
        }
        _ => bail!("'rand_bernoulli' requires number between 0. and 1."),
    };
    Ok(DataValue::from(rand::rng().random_bool(prob)))
}

// Nondeterministic: fresh randomness per evaluation; never constant-folded.
define_op!(OP_RAND_INT, 2, false, false);
pub(crate) fn op_rand_int(args: &[DataValue]) -> Result<DataValue> {
    let lower = &args[0]
        .get_int()
        .ok_or_else(|| miette!("'rand_int' requires integers"))?;
    let upper = &args[1]
        .get_int()
        .ok_or_else(|| miette!("'rand_int' requires integers"))?;
    // Checked here because rand 0.9's `random_range` panics on an empty
    // range, and an op body must never panic on user input.
    ensure!(
        lower <= upper,
        "'rand_int' requires a lower bound not greater than the upper bound"
    );
    Ok(rand::rng().random_range(*lower..=*upper).into())
}

// Nondeterministic: fresh randomness per evaluation; never constant-folded.
define_op!(OP_RAND_CHOOSE, 1, false, false);
pub(crate) fn op_rand_choose(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::List(l) => Ok(l
            .choose(&mut rand::rng())
            .cloned()
            .unwrap_or(DataValue::Null)),
        DataValue::Set(l) => Ok(l
            .iter()
            .collect_vec()
            .choose(&mut rand::rng())
            .cloned()
            .cloned()
            .unwrap_or(DataValue::Null)),
        _ => bail!("'rand_choice' requires lists"),
    }
}

define_op!(OP_ASSERT, 1, true, true);
pub(crate) fn op_assert(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::Bool(true) => Ok(DataValue::from(true)),
        _ => bail!("assertion failed: {:?}", args),
    }
}

define_op!(OP_UNION, 1, true, true);
pub(crate) fn op_union(args: &[DataValue]) -> Result<DataValue> {
    let mut ret = BTreeSet::new();
    for arg in args {
        match arg {
            DataValue::List(l) => {
                for el in l {
                    ret.insert(el.clone());
                }
            }
            DataValue::Set(s) => {
                for el in s {
                    ret.insert(el.clone());
                }
            }
            _ => bail!("'union' requires lists"),
        }
    }
    Ok(DataValue::List(ret.into_iter().collect()))
}

define_op!(OP_DIFFERENCE, 2, true, true);
pub(crate) fn op_difference(args: &[DataValue]) -> Result<DataValue> {
    let mut start: BTreeSet<_> = match &args[0] {
        DataValue::List(l) => l.iter().cloned().collect(),
        DataValue::Set(s) => s.iter().cloned().collect(),
        _ => bail!("'difference' requires lists"),
    };
    for arg in &args[1..] {
        match arg {
            DataValue::List(l) => {
                for el in l {
                    start.remove(el);
                }
            }
            DataValue::Set(s) => {
                for el in s {
                    start.remove(el);
                }
            }
            _ => bail!("'difference' requires lists"),
        }
    }
    Ok(DataValue::List(start.into_iter().collect()))
}

define_op!(OP_INTERSECTION, 1, true, true);
pub(crate) fn op_intersection(args: &[DataValue]) -> Result<DataValue> {
    let mut start: BTreeSet<_> = match &args[0] {
        DataValue::List(l) => l.iter().cloned().collect(),
        DataValue::Set(s) => s.iter().cloned().collect(),
        _ => bail!("'intersection' requires lists"),
    };
    for arg in &args[1..] {
        match arg {
            DataValue::List(l) => {
                let other: BTreeSet<_> = l.iter().cloned().collect();
                start = start.intersection(&other).cloned().collect();
            }
            DataValue::Set(s) => start = start.intersection(s).cloned().collect(),
            _ => bail!("'intersection' requires lists"),
        }
    }
    Ok(DataValue::List(start.into_iter().collect()))
}

define_op!(OP_TO_UUID, 1, false, true);
pub(crate) fn op_to_uuid(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        d @ DataValue::Uuid(_u) => Ok(d.clone()),
        DataValue::Str(s) => {
            let id = uuid::Uuid::try_parse(s).map_err(|_| miette!("invalid UUID"))?;
            Ok(DataValue::uuid(id))
        }
        _ => bail!("'to_uuid' requires a string"),
    }
}

// Nondeterministic: reads the clock per evaluation; never constant-folded.
// The CozoDB original folded an all-constant `now()` at compile time, so it
// behaved as a per-query constant by accident. It now evaluates per row; if
// per-query-constant semantics is ever wanted, that is an engine decision to
// make deliberately, not a side effect of folding.
define_op!(OP_NOW, 0, false, false);
pub(crate) fn op_now(_args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(unix_now()?.as_secs_f64()))
}

// `current_validity`, `MAX_VALIDITY_TS` and `TERMINAL_VALIDITY` live in
// `data/value.rs`: they are value-model vocabulary, not ops.

// chrono's `to_rfc3339()` used `SecondsFormat::AutoSi`: no fractional digits
// for a whole second, otherwise the shortest lossless run of 3, 6, or 9. jiff's
// printer defaults to minimal trailing-zero trimming (`.5`, not `.500`), so we
// pick the precision explicitly to reproduce the exact chrono output strings.
fn autosi_precision(subsec_nanos: i32) -> Option<u8> {
    let n = subsec_nanos.unsigned_abs();
    if n == 0 {
        None
    } else if n.is_multiple_of(1_000_000) {
        Some(3)
    } else if n.is_multiple_of(1_000) {
        Some(6)
    } else {
        Some(9)
    }
}

// Reproduces chrono's `DateTime::to_rfc3339()` for a jiff `Timestamp` viewed at
// a fixed offset: an RFC3339 string with a numeric offset (`+00:00`, never `Z`)
// and AutoSi subsecond digits. jiff's zoned printer appends an IANA/offset zone
// annotation (`...+08:00[+08:00]`) that chrono never emits, so we truncate at
// the `[`.
fn format_rfc3339(ts: jiff::Timestamp, off: Offset) -> String {
    let prec = autosi_precision(ts.subsec_nanosecond());
    let zoned = ts.to_zoned(TimeZone::fixed(off));
    let mut buf = String::new();
    jiff::fmt::temporal::DateTimePrinter::new()
        .precision(prec)
        .print_zoned(&zoned, &mut buf)
        .expect("formatting a timestamp into a String is infallible");
    if let Some(i) = buf.rfind('[') {
        buf.truncate(i);
    }
    buf
}

define_op!(OP_FORMAT_TIMESTAMP, 1, true, true);
pub(crate) fn op_format_timestamp(args: &[DataValue]) -> Result<DataValue> {
    let millis = match &args[0] {
        DataValue::Validity(vld) => vld.timestamp.0.0 / 1000,
        v => {
            let f = v
                .get_float()
                .ok_or_else(|| miette!("'format_timestamp' expects a number"))?;
            (f * 1000.) as i64
        }
    };
    let ts =
        jiff::Timestamp::from_millisecond(millis).map_err(|_| miette!("bad time: {}", &args[0]))?;
    let off = match args.get(1) {
        Some(tz_v) => {
            let tz_s = tz_v.get_str().ok_or_else(|| {
                miette!("'format_timestamp' timezone specification requires a string")
            })?;
            let tz =
                TimeZone::get(tz_s).map_err(|_| miette!("bad timezone specification: {}", tz_s))?;
            tz.to_offset(ts)
        }
        None => Offset::UTC,
    };
    Ok(DataValue::Str(SmartString::from(format_rfc3339(ts, off))))
}

// Microseconds since the Unix epoch for a parsed timestamp, FLOORED toward
// negative infinity: the microsecond that *contains* the instant, applied
// uniformly on both sides of 1970. jiff's `as_microsecond` truncates toward
// zero, which would place a pre-epoch sub-microsecond instant one microsecond
// too late; the chrono predecessor floored, and validity timestamps feed the
// time-travel key, so we preserve the floor. `as_nanosecond` is an `i128` in
// range [-2.6e23, 2.6e23] here; the floored microsecond count fits an i64.
pub(crate) fn timestamp_to_micros(ts: jiff::Timestamp) -> i64 {
    (ts.as_nanosecond().div_euclid(1000)) as i64
}

define_op!(OP_PARSE_TIMESTAMP, 1, false, true);
/// Parses an RFC 3339 / ISO 8601 timestamp string to seconds since the Unix
/// epoch (a float; pre-1970 inputs are negative). A `:60` leap second is
/// clamped to `:59` of the same minute — jiff does not fold it into the
/// following second, which the former chrono implementation did, so an input
/// like `2016-12-31T23:59:60Z` parses one second earlier than it used to.
pub(crate) fn op_parse_timestamp(args: &[DataValue]) -> Result<DataValue> {
    let s = args[0]
        .get_str()
        .ok_or_else(|| miette!("'parse_timestamp' expects a string"))?;
    let ts: jiff::Timestamp = s.parse().map_err(|_| miette!("bad datetime: {}", s))?;
    // Pre-epoch datetimes yield negative seconds. The CozoDB original went
    // through `SystemTime` and unwrapped `duration_since(UNIX_EPOCH)`, aborting
    // the process on any user-supplied datetime before 1970.
    //
    // Decomposed as chrono did — a FLOORED whole second plus a non-negative
    // subsecond nanosecond count — so the lossy f64 result is bit-identical to
    // the former `chrono::DateTime::timestamp() as f64 + subsec_nanos / 1e9`
    // across the epoch boundary. jiff's own `as_second()` truncates toward zero
    // and pairs it with a signed subsecond, which rounds pre-epoch instants to a
    // slightly different f64.
    let nanos = ts.as_nanosecond();
    let secs =
        nanos.div_euclid(1_000_000_000) as f64 + (nanos.rem_euclid(1_000_000_000) as f64) / 1e9;
    Ok(DataValue::from(secs))
}

/// Parses an RFC 3339 / ISO 8601 timestamp string to a validity timestamp in
/// microseconds since the Unix epoch, floored toward negative infinity (see
/// [`timestamp_to_micros`]). A `:60` leap second is clamped to `:59` of the
/// same minute (jiff does not fold it into the following second, unlike the
/// former chrono implementation).
pub(crate) fn str2vld(s: &str) -> Result<ValidityTs> {
    let ts: jiff::Timestamp = s.parse().map_err(|_| miette!("bad datetime: {}", s))?;
    // Same law as `op_parse_timestamp`: a pre-epoch validity is a negative
    // microsecond count, not a panic.
    Ok(ValidityTs(Reverse(timestamp_to_micros(ts))))
}

#[derive(Debug, Error, Diagnostic)]
#[error("bad specification of validity")]
#[diagnostic(code(parser::bad_validity_spec))]
pub(crate) struct BadValiditySpecification(#[label] pub(crate) crate::data::span::SourceSpan);

/// Interpret an already-evaluated [`DataValue`] as a validity coordinate — an
/// integer microsecond count, the sentinels `"NOW"`/`"END"`, or an RFC 3339
/// string. Shared by both `@` clauses in the grammar: the read side's
/// (`parse::query::expr2vld_spec`, evaluated once at parse time) and the
/// write side's per-row form (`runtime::mutate`, evaluated once per output
/// row) — one coercion law for what a validity expression may mean, however
/// many times it ends up evaluated.
pub(crate) fn data_value_to_vld_spec(
    val: DataValue,
    span: crate::data::span::SourceSpan,
    cur_vld: ValidityTs,
) -> Result<ValidityTs> {
    match val {
        DataValue::Num(n) => {
            let microseconds = n.get_int().ok_or(BadValiditySpecification(span))?;
            Ok(ValidityTs(Reverse(microseconds)))
        }
        DataValue::Str(s) => match &s as &str {
            "NOW" => Ok(cur_vld),
            "END" => Ok(crate::data::value::MAX_VALIDITY_TS),
            s => Ok(str2vld(s).map_err(|_| BadValiditySpecification(span))?),
        },
        _ => {
            bail!(BadValiditySpecification(span))
        }
    }
}

// Nondeterministic: fresh randomness and a clock read per evaluation; never
// constant-folded.
define_op!(OP_RAND_UUID_V1, 0, false, false);
pub(crate) fn op_rand_uuid_v1(_args: &[DataValue]) -> Result<DataValue> {
    let mut rng = rand::rng();
    let uuid_ctx = uuid::ContextV1::new(rng.random());
    let ts = {
        let since_epoch = unix_now()?;
        Timestamp::from_unix(uuid_ctx, since_epoch.as_secs(), since_epoch.subsec_nanos())
    };
    let mut rand_vals = [0u8; 6];
    rng.fill(&mut rand_vals);
    let id = uuid::Uuid::new_v1(ts, &rand_vals);
    Ok(DataValue::uuid(id))
}

// Nondeterministic: fresh randomness per evaluation; never constant-folded.
// (Folding was how the original turned `rand_uuid_v4()` into the same UUID
// for every row of a query.)
define_op!(OP_RAND_UUID_V4, 0, false, false);
pub(crate) fn op_rand_uuid_v4(_args: &[DataValue]) -> Result<DataValue> {
    let id = uuid::Uuid::new_v4();
    Ok(DataValue::uuid(id))
}

define_op!(OP_UUID_TIMESTAMP, 1, false, true);
pub(crate) fn op_uuid_timestamp(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Uuid(UuidWrapper(id)) => match id.get_timestamp() {
            None => DataValue::Null,
            Some(t) => {
                let (s, subs) = t.to_unix();
                let s = (s as f64) + (subs as f64 / 10_000_000.);
                s.into()
            }
        },
        _ => bail!("not an UUID"),
    })
}

define_op!(OP_VALIDITY, 1, true, true);
pub(crate) fn op_validity(args: &[DataValue]) -> Result<DataValue> {
    let ts = args[0]
        .get_int()
        .ok_or_else(|| miette!("'validity' expects an integer"))?;
    let is_assert = if args.len() == 1 {
        true
    } else {
        args[1]
            .get_bool()
            .ok_or_else(|| miette!("'validity' expects a boolean as second argument"))?
    };
    Ok(DataValue::Validity(Validity {
        timestamp: ValidityTs(Reverse(ts)),
        is_assert: Reverse(is_assert),
    }))
}

/// Extracts both arguments as `Interval`s for a two-interval predicate op, or
/// a typed error naming which argument was wrong — never a panic.
fn two_intervals<'a>(op: &str, args: &'a [DataValue]) -> Result<(&'a Interval, &'a Interval)> {
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

define_op!(OP_MAKE_INTERVAL, 2, false, true);
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
    Ok(DataValue::Interval(Interval::new(start, end)?))
}

define_op!(OP_INTERVAL_START, 1, false, true);
pub(crate) fn op_interval_start(args: &[DataValue]) -> Result<DataValue> {
    let iv = args[0]
        .get_interval()
        .ok_or_else(|| miette!("'interval_start' expects an interval, got {:?}", args[0]))?;
    Ok(DataValue::from(iv.start()))
}

define_op!(OP_INTERVAL_END, 1, false, true);
pub(crate) fn op_interval_end(args: &[DataValue]) -> Result<DataValue> {
    let iv = args[0]
        .get_interval()
        .ok_or_else(|| miette!("'interval_end' expects an interval, got {:?}", args[0]))?;
    Ok(DataValue::from(iv.end()))
}

// Allen's relations: six primitives + `intersects`, the workhorse. Equality
// is already covered by the generic `eq`/`neq` ops (`Interval` derives
// `PartialEq`/`Eq`), so it gets no dedicated op here. The five inverse
// relations (after, met_by, overlapped_by, started_by, contains,
// finished_by) likewise get no separate op: every primitive below is
// asymmetric in its two arguments, so the inverse is the same op with the
// call-site argument order swapped (`interval_before(b, a)` reads as "a is
// after b"). One op per relation, not twelve.

define_op!(OP_INTERVAL_BEFORE, 2, false, true);
pub(crate) fn op_interval_before(args: &[DataValue]) -> Result<DataValue> {
    let (a, b) = two_intervals("interval_before", args)?;
    Ok(DataValue::from(a.before(b)))
}

define_op!(OP_INTERVAL_MEETS, 2, false, true);
pub(crate) fn op_interval_meets(args: &[DataValue]) -> Result<DataValue> {
    let (a, b) = two_intervals("interval_meets", args)?;
    Ok(DataValue::from(a.meets(b)))
}

define_op!(OP_INTERVAL_OVERLAPS, 2, false, true);
pub(crate) fn op_interval_overlaps(args: &[DataValue]) -> Result<DataValue> {
    let (a, b) = two_intervals("interval_overlaps", args)?;
    Ok(DataValue::from(a.overlaps(b)))
}

define_op!(OP_INTERVAL_STARTS, 2, false, true);
pub(crate) fn op_interval_starts(args: &[DataValue]) -> Result<DataValue> {
    let (a, b) = two_intervals("interval_starts", args)?;
    Ok(DataValue::from(a.starts(b)))
}

define_op!(OP_INTERVAL_DURING, 2, false, true);
pub(crate) fn op_interval_during(args: &[DataValue]) -> Result<DataValue> {
    let (a, b) = two_intervals("interval_during", args)?;
    Ok(DataValue::from(a.during(b)))
}

define_op!(OP_INTERVAL_FINISHES, 2, false, true);
pub(crate) fn op_interval_finishes(args: &[DataValue]) -> Result<DataValue> {
    let (a, b) = two_intervals("interval_finishes", args)?;
    Ok(DataValue::from(a.finishes(b)))
}

define_op!(OP_INTERVAL_INTERSECTS, 2, false, true);
pub(crate) fn op_interval_intersects(args: &[DataValue]) -> Result<DataValue> {
    let (a, b) = two_intervals("interval_intersects", args)?;
    Ok(DataValue::from(a.intersects(b)))
}
