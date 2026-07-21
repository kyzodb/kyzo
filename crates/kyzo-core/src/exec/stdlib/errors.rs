/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Typed stdlib refuse + NaN checkpoint helpers. Sole seat for these shapes.
use std::borrow::Cow;

use miette::{Diagnostic, Result, bail, miette};
use thiserror::Error;

use kyzo_model::data_value_any;
use kyzo_model::value::{DataValue, Num, NumRepr, Vector};

/// Named stdlib refuse variants (move_plan sealed names).
#[derive(Debug, Error, Diagnostic)]
pub enum StdlibRefuse {
    /// Successful NaN answer refused at BoundOp::apply (cut_destiny).
    #[error("op '{op}' produced a NaN result")]
    #[diagnostic(code(eval::nan_answer))]
    #[diagnostic(help("NaN is never a successful builtin answer; infinity is legal."))]
    NanAnswer { op: Cow<'static, str> },

    /// Language compare across value kinds (cut_destiny — door separation per §14).
    #[error("comparison can only be done between the same datatypes, got {left:?} and {right:?}")]
    #[diagnostic(code(eval::cross_type_compare))]
    CrossTypeCompare { left: String, right: String },
}

/// A 64-bit integer scalar op overflowed.
#[derive(Debug, Error, Diagnostic)]
#[error("integer overflow evaluating '{op}'")]
#[diagnostic(code(eval::integer_overflow))]
#[diagnostic(help("The operands are exact 64-bit integers whose result does not fit in i64."))]
pub(crate) struct IntegerOverflow {
    pub(crate) op: &'static str,
}

/// Zero divisor offered to div/mod.
#[derive(Debug, Error, Diagnostic)]
#[error("'{op}' requires a non-zero divisor")]
#[diagnostic(code(eval::division_by_zero))]
#[diagnostic(help(
    "Division and modulo both refuse a zero divisor rather than returning infinity or NaN."
))]
pub(crate) struct DivisionByZero {
    pub(crate) op: &'static str,
}

/// Argument outside a partial math op's domain.
#[derive(Debug, Error, Diagnostic)]
#[error("'{op}' is undefined for the given argument")]
#[diagnostic(code(eval::domain_error))]
#[diagnostic(help(
    "This op is partial: some inputs have no real, finite result. Check the      argument lies within the function's mathematical domain before calling it."
))]
pub(crate) struct DomainError {
    pub(crate) op: Cow<'static, str>,
}

/// Vector op invoked with an empty argument slice after the single-argument early return.
#[derive(Debug, Error, Diagnostic)]
#[error("'{op}' vector lane requires a non-empty argument slice")]
#[diagnostic(code(eval::vec_op_empty_args))]
pub(crate) struct VecOpEmptyArgs {
    pub(crate) op: &'static str,
}

/// RFC3339 formatting into a String buffer refused.
#[derive(Debug, Error, Diagnostic)]
#[error("timestamp formatting into a string buffer failed")]
#[diagnostic(code(eval::timestamp_format_refused))]
pub(crate) struct TimestampFormatRefused;

pub(crate) fn no_nan(op: &'static str, x: f64) -> Result<DataValue> {
    if x.is_nan() {
        bail!(DomainError { op: op.into() });
    }
    Ok(DataValue::Num(Num::float(x)))
}

pub(crate) fn vec_value(components: Vec<f64>) -> Result<Vector> {
    Vector::try_new(components).ok_or_else(|| miette!("vector dimension exceeds u32"))
}

pub(crate) fn no_nan_vec(op: &'static str, v: Vec<f64>) -> Result<DataValue> {
    if v.iter().any(|x| x.is_nan()) {
        bail!(DomainError { op: op.into() });
    }
    Ok(DataValue::Vector(vec_value(v)?))
}

pub(crate) fn result_has_nan(v: &DataValue) -> bool {
    match v {
        DataValue::Num(n) => matches!(n.repr(), NumRepr::Float(x) if x.is_nan()),
        DataValue::Vector(v) => v.to_f64s().iter().any(|x| x.is_nan()),
        data_value_any!() => false,
    }
}
