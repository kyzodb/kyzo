//! numeric.rs — stdlib kernel (move_plan).
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

pub(crate) fn op_abs(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Num(n) => match n.repr() {
            NumRepr::Int(i) => match i.checked_abs() {
                Some(v) => DataValue::Num(Num::int(v)),
                None => bail!(IntegerOverflow { op: "abs" }),
            },
            NumRepr::Float(f) => DataValue::Num(Num::float(f.abs())),
        },
        DataValue::Vector(v) => {
            DataValue::Vector(vec_value(v.to_f64s().iter().map(|x| x.abs()).collect())?)
        }
        data_value_any!() => bail!("'abs' requires numbers"),
    })
}

pub(crate) fn op_acos(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(n) => n.to_f64(),
        DataValue::Vector(v) => {
            if v.to_f64s().iter().any(|x| !(-1.0..=1.0).contains(x)) {
                bail!(DomainError { op: "acos".into() });
            }
            return no_nan_vec("acos", v.to_f64s().iter().map(|x| x.acos()).collect());
        }
        data_value_any!() => bail!("'acos' requires numbers"),
    };
    // `acos` is defined only on [-1, 1]; outside it the raw `f64` method
    // returns `NaN`.
    if !(-1.0..=1.0).contains(&a) {
        bail!(DomainError { op: "acos".into() });
    }
    no_nan("acos", a.acos())
}

pub(crate) fn op_acosh(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(n) => n.to_f64(),
        DataValue::Vector(v) => {
            if v.to_f64s().iter().any(|x| *x < 1.0) {
                bail!(DomainError { op: "acosh".into() });
            }
            return no_nan_vec("acosh", v.to_f64s().iter().map(|x| x.acosh()).collect());
        }
        data_value_any!() => bail!("'acosh' requires numbers"),
    };
    // `acosh` is defined only on [1, +inf); below 1 the raw `f64` method
    // returns `NaN`.
    if a < 1.0 {
        bail!(DomainError { op: "acosh".into() });
    }
    no_nan("acosh", a.acosh())
}

pub(crate) fn op_add(args: &[DataValue]) -> Result<DataValue> {
    let mut i_accum = 0i64;
    let mut f_accum = 0.0f64;
    for arg in args {
        match arg {
            DataValue::Num(n) => match n.repr() {
                NumRepr::Int(i) => {
                    i_accum = i_accum
                        .checked_add(i)
                        .ok_or(IntegerOverflow { op: "add" })?;
                }
                NumRepr::Float(f) => f_accum += f,
            },
            DataValue::Vector(_) => return add_vecs(args),
            data_value_any!() => bail!("addition requires numbers"),
        }
    }
    if f_accum == 0.0f64 {
        Ok(DataValue::Num(Num::int(i_accum)))
    } else {
        Ok(DataValue::Num(Num::float(i_accum as f64 + f_accum)))
    }
}

pub(crate) fn op_asin(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(n) => n.to_f64(),
        DataValue::Vector(v) => {
            if v.to_f64s().iter().any(|x| !(-1.0..=1.0).contains(x)) {
                bail!(DomainError { op: "asin".into() });
            }
            return no_nan_vec("asin", v.to_f64s().iter().map(|x| x.asin()).collect());
        }
        data_value_any!() => bail!("'asin' requires numbers"),
    };
    // `asin` is defined only on [-1, 1]; outside it the raw `f64` method
    // returns `NaN`.
    if !(-1.0..=1.0).contains(&a) {
        bail!(DomainError { op: "asin".into() });
    }
    no_nan("asin", a.asin())
}

pub(crate) fn op_asinh(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(n) => n.to_f64(),
        DataValue::Vector(v) => {
            return Ok(DataValue::Vector(vec_value(
                v.to_f64s().iter().map(|x| x.asinh()).collect(),
            )?));
        }
        data_value_any!() => bail!("'asinh' requires numbers"),
    };
    Ok(DataValue::Num(Num::float(a.asinh())))
}

pub(crate) fn op_atan(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(n) => n.to_f64(),
        DataValue::Vector(v) => {
            return Ok(DataValue::Vector(vec_value(
                v.to_f64s().iter().map(|x| x.atan()).collect(),
            )?));
        }
        data_value_any!() => bail!("'atan' requires numbers"),
    };
    Ok(DataValue::Num(Num::float(a.atan())))
}

pub(crate) fn op_atan2(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(n) => n.to_f64(),
        data_value_any!() => bail!("'atan2' requires numbers"),
    };
    let b = match &args[1] {
        DataValue::Num(n) => n.to_f64(),
        data_value_any!() => bail!("'atan2' requires numbers"),
    };

    Ok(DataValue::Num(Num::float(a.atan2(b))))
}

pub(crate) fn op_atanh(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(n) => n.to_f64(),
        DataValue::Vector(v) => {
            if v.to_f64s().iter().any(|x| x.abs() >= 1.0) {
                bail!(DomainError { op: "atanh".into() });
            }
            return no_nan_vec("atanh", v.to_f64s().iter().map(|x| x.atanh()).collect());
        }
        data_value_any!() => bail!("'atanh' requires numbers"),
    };
    // `atanh` is defined only on the open interval (-1, 1): outside it the
    // raw `f64` method returns `NaN`, and at either endpoint it diverges to
    // an infinity — just as much a silent poison value.
    if a.abs() >= 1.0 {
        bail!(DomainError { op: "atanh".into() });
    }
    no_nan("atanh", a.atanh())
}

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

pub(crate) fn op_bit_not(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::Bytes(arg) => {
            let mut ret = arg.clone();
            for l in ret.iter_mut() {
                *l = !*l;
            }
            Ok(DataValue::Bytes(ret))
        }
        data_value_any!() => bail!("'bit_not' requires bytes"),
    }
}

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

pub(crate) fn op_ceil(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Num(n) => match n.repr() {
            NumRepr::Int(i) => DataValue::Num(Num::int(i)),
            NumRepr::Float(f) => DataValue::Num(Num::float(f.ceil())),
        },
        data_value_any!() => bail!("'ceil' requires numbers"),
    })
}

pub(crate) fn op_cos(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(n) => n.to_f64(),
        DataValue::Vector(v) => {
            return Ok(DataValue::Vector(vec_value(
                v.to_f64s().iter().map(|x| x.cos()).collect(),
            )?));
        }
        data_value_any!() => bail!("'cos' requires numbers"),
    };
    Ok(DataValue::Num(Num::float(a.cos())))
}

pub(crate) fn op_cosh(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(n) => n.to_f64(),
        DataValue::Vector(v) => {
            return Ok(DataValue::Vector(vec_value(
                v.to_f64s().iter().map(|x| x.cosh()).collect(),
            )?));
        }
        data_value_any!() => bail!("'cosh' requires numbers"),
    };
    Ok(DataValue::Num(Num::float(a.cosh())))
}

pub(crate) fn op_div(args: &[DataValue]) -> Result<DataValue> {
    Ok(match (&args[0], &args[1]) {
        (DataValue::Num(na), DataValue::Num(nb)) => {
            // Division's own law: a zero divisor refuses (int or float
            // alike — no silent infinity), and the result is ALWAYS a
            // float, including int/int.
            let (a, b) = (na.to_f64(), nb.to_f64());
            if b == 0.0 {
                bail!(DivisionByZero { op: "div" })
            }
            DataValue::Num(Num::float(a / b))
        }
        (DataValue::Vector(a), DataValue::Vector(b)) => {
            if a.len() != b.len() {
                bail!("can only divide vectors of the same length");
            }
            DataValue::Vector(vec_value(
                a.to_f64s()
                    .iter()
                    .zip(b.to_f64s().iter())
                    .map(|(x, y)| x / y)
                    .collect(),
            )?)
        }
        (DataValue::Vector(a), b) => {
            let b = b
                .get_float()
                .ok_or_else(|| miette!("can only divide vectors by numbers"))?;
            DataValue::Vector(vec_value(a.to_f64s().iter().map(|x| *x / b).collect())?)
        }
        (a, DataValue::Vector(b)) => {
            let a = a
                .get_float()
                .ok_or_else(|| miette!("can only divide numbers by vectors"))?;
            DataValue::Vector(vec_value(b.to_f64s().iter().map(|x| a / x).collect())?)
        }
        _ => bail!("division requires numbers"),
    })
}

pub(crate) fn op_exp(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(n) => n.to_f64(),
        DataValue::Vector(v) => {
            return Ok(DataValue::Vector(vec_value(
                v.to_f64s().iter().map(|x| x.exp()).collect(),
            )?));
        }
        data_value_any!() => bail!("'exp' requires numbers"),
    };
    Ok(DataValue::Num(Num::float(a.exp())))
}

pub(crate) fn op_exp2(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(n) => n.to_f64(),
        DataValue::Vector(v) => {
            return Ok(DataValue::Vector(vec_value(
                v.to_f64s().iter().map(|x| x.exp2()).collect(),
            )?));
        }
        data_value_any!() => bail!("'exp2' requires numbers"),
    };
    Ok(DataValue::Num(Num::float(a.exp2())))
}

pub(crate) fn op_floor(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Num(n) => match n.repr() {
            NumRepr::Int(i) => DataValue::Num(Num::int(i)),
            NumRepr::Float(f) => DataValue::Num(Num::float(f.floor())),
        },
        data_value_any!() => bail!("'floor' requires numbers"),
    })
}

pub(crate) fn op_ln(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(n) => n.to_f64(),
        DataValue::Vector(v) => {
            if v.to_f64s().iter().any(|x| *x <= 0.0) {
                bail!(DomainError { op: "ln".into() });
            }
            return no_nan_vec("ln", v.to_f64s().iter().map(|x| x.ln()).collect());
        }
        data_value_any!() => bail!("'ln' requires numbers"),
    };
    // `ln` is defined only for positive reals: zero diverges to `-inf` and a
    // negative argument has no real logarithm (`NaN`) — both are silent
    // poison values, refused up front rather than returned.
    if a <= 0.0 {
        bail!(DomainError { op: "ln".into() });
    }
    no_nan("ln", a.ln())
}

pub(crate) fn op_log10(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(n) => n.to_f64(),
        DataValue::Vector(v) => {
            if v.to_f64s().iter().any(|x| *x <= 0.0) {
                bail!(DomainError { op: "log10".into() });
            }
            return no_nan_vec("log10", v.to_f64s().iter().map(|x| x.log10()).collect());
        }
        data_value_any!() => bail!("'log10' requires numbers"),
    };
    // Same domain as `ln`: only positive reals have a base-10 logarithm.
    if a <= 0.0 {
        bail!(DomainError { op: "log10".into() });
    }
    no_nan("log10", a.log10())
}

pub(crate) fn op_log2(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(n) => n.to_f64(),
        DataValue::Vector(v) => {
            if v.to_f64s().iter().any(|x| *x <= 0.0) {
                bail!(DomainError { op: "log2".into() });
            }
            return no_nan_vec("log2", v.to_f64s().iter().map(|x| x.log2()).collect());
        }
        data_value_any!() => bail!("'log2' requires numbers"),
    };
    // Same domain as `ln`: only positive reals have a base-2 logarithm.
    if a <= 0.0 {
        bail!(DomainError { op: "log2".into() });
    }
    no_nan("log2", a.log2())
}

pub(crate) fn op_max(args: &[DataValue]) -> Result<DataValue> {
    let res = args
        .iter()
        .try_fold(None, |accum, nxt| match (accum, nxt) {
            (None, d @ DataValue::Num(_)) => Ok(Some(d.clone())),
            (Some(DataValue::Num(a)), DataValue::Num(b)) => {
                // Ties keep `a` — deterministic and accumulation-friendly.
                let chosen = if NumericOrd::of(a) < NumericOrd::of(*b) {
                    *b
                } else {
                    a
                };
                Ok(Some(DataValue::Num(chosen)))
            }
            _ => bail!("'max can only be applied to numbers'"),
        })?;
    match res {
        None => Ok(DataValue::Num(Num::float(f64::NEG_INFINITY))),
        Some(v) => Ok(v),
    }
}

pub(crate) fn op_min(args: &[DataValue]) -> Result<DataValue> {
    let res = args
        .iter()
        .try_fold(None, |accum, nxt| match (accum, nxt) {
            (None, d @ DataValue::Num(_)) => Ok(Some(d.clone())),
            (Some(DataValue::Num(a)), DataValue::Num(b)) => {
                // Ties keep `a`.
                let chosen = if NumericOrd::of(a) > NumericOrd::of(*b) {
                    *b
                } else {
                    a
                };
                Ok(Some(DataValue::Num(chosen)))
            }
            _ => bail!("'min' can only be applied to numbers"),
        })?;
    match res {
        None => Ok(DataValue::Num(Num::float(f64::INFINITY))),
        Some(v) => Ok(v),
    }
}

pub(crate) fn op_minus(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Num(n) => match n.repr() {
            NumRepr::Int(i) => match i.checked_neg() {
                Some(v) => DataValue::Num(Num::int(v)),
                None => bail!(IntegerOverflow { op: "minus" }),
            },
            NumRepr::Float(f) => DataValue::Num(Num::float(-f)),
        },
        DataValue::Vector(v) => {
            DataValue::Vector(vec_value(v.to_f64s().iter().map(|x| -*x).collect())?)
        }
        data_value_any!() => bail!("minus can only be applied to numbers"),
    })
}

pub(crate) fn op_mod(args: &[DataValue]) -> Result<DataValue> {
    Ok(match (&args[0], &args[1]) {
        (DataValue::Num(na), DataValue::Num(nb)) => match (na.repr(), nb.repr()) {
            (NumRepr::Int(a), NumRepr::Int(b)) => {
                if b == 0 {
                    bail!(DivisionByZero { op: "mod" })
                }
                // `i64::MIN % -1` is the one other input pair `Rem` can't
                // service: the mathematical quotient (`i64::MIN / -1`)
                // doesn't fit in i64, so the divide-then-subtract this
                // performs internally overflows too, distinct from the
                // zero-divisor case just above.
                match a.checked_rem(b) {
                    Some(v) => DataValue::Num(Num::int(v)),
                    None => bail!(IntegerOverflow { op: "mod" }),
                }
            }
            // Mixed and float pairs: Rust remainder semantics (result
            // takes the dividend's sign), zero divisor refused.
            _ => {
                let (a, b) = (na.to_f64(), nb.to_f64());
                if b == 0.0 {
                    bail!(DivisionByZero { op: "mod" })
                }
                DataValue::Num(Num::float(a.rem(b)))
            }
        },
        _ => bail!("'mod' requires numbers"),
    })
}

pub(crate) fn op_mul(args: &[DataValue]) -> Result<DataValue> {
    let mut i_accum = 1i64;
    let mut f_accum = 1.0f64;
    for arg in args {
        match arg {
            DataValue::Num(n) => match n.repr() {
                NumRepr::Int(i) => {
                    i_accum = i_accum
                        .checked_mul(i)
                        .ok_or(IntegerOverflow { op: "mul" })?;
                }
                NumRepr::Float(f) => f_accum *= f,
            },
            DataValue::Vector(_) => return mul_vecs(args),
            data_value_any!() => bail!("multiplication requires numbers"),
        }
    }
    if f_accum == 1.0f64 {
        Ok(DataValue::Num(Num::int(i_accum)))
    } else {
        Ok(DataValue::Num(Num::float(i_accum as f64 * f_accum)))
    }
}

pub(crate) fn op_negate(args: &[DataValue]) -> Result<DataValue> {
    if let DataValue::Bool(b) = &args[0] {
        Ok(DataValue::from(!*b))
    } else {
        bail!("'negate' requires booleans");
    }
}

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
                data_value_any!() => bail!("'pack_bits' requires list of booleans"),
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

pub(crate) fn op_pow(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(n) => n.to_f64(),
        DataValue::Vector(v) => {
            let b = args[1]
                .get_float()
                .ok_or_else(|| miette!("'pow' requires numbers"))?;
            if v.to_f64s().iter().any(|x| pow_out_of_domain(*x, b)) {
                bail!(DomainError { op: "pow".into() });
            }
            return no_nan_vec("pow", v.to_f64s().iter().map(|x| x.powf(b)).collect());
        }
        data_value_any!() => bail!("'pow' requires numbers"),
    };
    let b = match &args[1] {
        DataValue::Num(n) => n.to_f64(),
        data_value_any!() => bail!("'pow' requires numbers"),
    };
    if pow_out_of_domain(a, b) {
        bail!(DomainError { op: "pow".into() });
    }
    no_nan("pow", a.powf(b))
}

pub(crate) fn op_round(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Num(n) => match n.repr() {
            NumRepr::Int(i) => DataValue::Num(Num::int(i)),
            NumRepr::Float(f) => DataValue::Num(Num::float(f.round())),
        },
        data_value_any!() => bail!("'round' requires numbers"),
    })
}

pub(crate) fn op_signum(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Num(n) => match n.repr() {
            NumRepr::Int(i) => DataValue::Num(Num::int(i.signum())),
            NumRepr::Float(f) => {
                if f.signum() < 0. {
                    DataValue::from(-1)
                } else if f == 0. {
                    DataValue::from(0)
                } else if f > 0. {
                    DataValue::from(1)
                } else {
                    DataValue::from(f64::NAN)
                }
            }
        },
        data_value_any!() => bail!("'signum' requires numbers"),
    })
}

pub(crate) fn op_sin(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(n) => n.to_f64(),
        DataValue::Vector(v) => {
            return Ok(DataValue::Vector(vec_value(
                v.to_f64s().iter().map(|x| x.sin()).collect(),
            )?));
        }
        data_value_any!() => bail!("'sin' requires numbers"),
    };
    Ok(DataValue::Num(Num::float(a.sin())))
}

pub(crate) fn op_sinh(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(n) => n.to_f64(),
        DataValue::Vector(v) => {
            return Ok(DataValue::Vector(vec_value(
                v.to_f64s().iter().map(|x| x.sinh()).collect(),
            )?));
        }
        data_value_any!() => bail!("'sinh' requires numbers"),
    };
    Ok(DataValue::Num(Num::float(a.sinh())))
}

pub(crate) fn op_sqrt(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(n) => n.to_f64(),
        DataValue::Vector(v) => {
            if v.to_f64s().iter().any(|x| *x < 0.0) {
                bail!(DomainError { op: "sqrt".into() });
            }
            return no_nan_vec("sqrt", v.to_f64s().iter().map(|x| x.sqrt()).collect());
        }
        data_value_any!() => bail!("'sqrt' requires numbers"),
    };
    // `sqrt` is defined only for non-negative reals; a negative argument has
    // no real square root and the raw `f64` method returns `NaN`.
    if a < 0.0 {
        bail!(DomainError { op: "sqrt".into() });
    }
    no_nan("sqrt", a.sqrt())
}

pub(crate) fn op_sub(args: &[DataValue]) -> Result<DataValue> {
    Ok(match (&args[0], &args[1]) {
        (DataValue::Num(na), DataValue::Num(nb)) => match (na.repr(), nb.repr()) {
            (NumRepr::Int(a), NumRepr::Int(b)) => match a.checked_sub(b) {
                Some(v) => DataValue::Num(Num::int(v)),
                None => bail!(IntegerOverflow { op: "sub" }),
            },
            (NumRepr::Float(a), NumRepr::Float(b)) => DataValue::Num(Num::float(a - b)),
            (NumRepr::Int(a), NumRepr::Float(b)) => DataValue::Num(Num::float((a as f64) - b)),
            (NumRepr::Float(a), NumRepr::Int(b)) => DataValue::Num(Num::float(a - (b as f64))),
        },
        (DataValue::Vector(a), DataValue::Vector(b)) => {
            if a.len() != b.len() {
                bail!("can only subtract vectors of the same length");
            }
            DataValue::Vector(vec_value(
                a.to_f64s()
                    .iter()
                    .zip(b.to_f64s().iter())
                    .map(|(x, y)| x - y)
                    .collect(),
            )?)
        }
        (DataValue::Vector(a), b) => {
            let b = b
                .get_float()
                .ok_or_else(|| miette!("can only subtract numbers from vectors"))?;
            DataValue::Vector(vec_value(a.to_f64s().iter().map(|x| *x - b).collect())?)
        }
        (a, DataValue::Vector(b)) => {
            let a = a
                .get_float()
                .ok_or_else(|| miette!("can only subtract vectors from numbers"))?;
            DataValue::Vector(vec_value(b.to_f64s().iter().map(|x| a - x).collect())?)
        }
        _ => bail!("subtraction requires numbers"),
    })
}

pub(crate) fn op_tan(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(n) => n.to_f64(),
        DataValue::Vector(v) => {
            return Ok(DataValue::Vector(vec_value(
                v.to_f64s().iter().map(|x| x.tan()).collect(),
            )?));
        }
        data_value_any!() => bail!("'tan' requires numbers"),
    };
    Ok(DataValue::Num(Num::float(a.tan())))
}

pub(crate) fn op_tanh(args: &[DataValue]) -> Result<DataValue> {
    let a = match &args[0] {
        DataValue::Num(n) => n.to_f64(),
        DataValue::Vector(v) => {
            return Ok(DataValue::Vector(vec_value(
                v.to_f64s().iter().map(|x| x.tanh()).collect(),
            )?));
        }
        data_value_any!() => bail!("'tanh' requires numbers"),
    };
    Ok(DataValue::Num(Num::float(a.tanh())))
}

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

fn add_vecs(args: &[DataValue]) -> Result<DataValue> {
    if args.len() == 1 {
        return Ok(args[0].clone());
    }
    // Non-empty: only called from `op_add`/`op_mul` after a `Vec` argument
    // was seen (so len >= 1), and the len == 1 case returned above.
    let Some((last, first)) = args.split_last() else {
        bail!(VecOpEmptyArgs { op: "add" });
    };
    let first = add_vecs(first)?;
    match (first, last) {
        (DataValue::Vector(a), DataValue::Vector(b)) => {
            if a.len() != b.len() {
                bail!("can only add vectors of the same length");
            }
            Ok(DataValue::Vector(vec_value(
                a.to_f64s()
                    .iter()
                    .zip(b.to_f64s().iter())
                    .map(|(x, y)| x + y)
                    .collect(),
            )?))
        }
        (DataValue::Vector(a), b) => {
            let f = b
                .get_float()
                .ok_or_else(|| miette!("can only add numbers to vectors"))?;
            Ok(DataValue::Vector(vec_value(
                a.to_f64s().iter().map(|x| x + f).collect(),
            )?))
        }
        (a, DataValue::Vector(b)) => {
            let f = a
                .get_float()
                .ok_or_else(|| miette!("can only add numbers to vectors"))?;
            Ok(DataValue::Vector(vec_value(
                b.to_f64s().iter().map(|x| x + f).collect(),
            )?))
        }
        _ => bail!("addition requires numbers"),
    }
}

fn mul_vecs(args: &[DataValue]) -> Result<DataValue> {
    if args.len() == 1 {
        return Ok(args[0].clone());
    }
    // Non-empty: see `add_vecs`.
    let Some((last, first)) = args.split_last() else {
        bail!(VecOpEmptyArgs { op: "mul" });
    };
    // The CozoDB original recursed into `add_vecs` here, so multiplying
    // three or more vector arguments *added* the prefix before multiplying
    // by the last: `v1 * v2 * v3` computed `(v1 + v2) * v3`. Fixed to
    // recurse into multiplication; flagged as a deliberate deviation.
    let first = mul_vecs(first)?;
    match (first, last) {
        (DataValue::Vector(a), DataValue::Vector(b)) => {
            if a.len() != b.len() {
                bail!("can only multiply vectors of the same length");
            }
            Ok(DataValue::Vector(vec_value(
                a.to_f64s()
                    .iter()
                    .zip(b.to_f64s().iter())
                    .map(|(x, y)| *x * *y)
                    .collect(),
            )?))
        }
        (DataValue::Vector(a), b) => {
            let f = b
                .get_float()
                .ok_or_else(|| miette!("can only multiply vectors by numbers"))?;
            Ok(DataValue::Vector(vec_value(
                a.to_f64s().iter().map(|x| x * f).collect(),
            )?))
        }
        (a, DataValue::Vector(b)) => {
            let f = a
                .get_float()
                .ok_or_else(|| miette!("can only multiply vectors by numbers"))?;
            Ok(DataValue::Vector(vec_value(
                b.to_f64s().iter().map(|x| x * f).collect(),
            )?))
        }
        _ => bail!("multiplication requires numbers"),
    }
}

/// `a.powf(b)` is partial in two ways: a negative base raised to a
/// fractional exponent has no real result (`NaN` — e.g. `(-1)^0.5`), and a
/// zero base raised to a negative exponent diverges to an infinity (e.g.
/// `0^-1`, the same shape as a division by zero expressed through `pow`).
fn pow_out_of_domain(a: f64, b: f64) -> bool {
    (a < 0.0 && b.fract() != 0.0) || (a == 0.0 && b < 0.0)
}
