/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! numeric.rs — stdlib kernel (move_plan).
use std::ops::{Div, Rem};

use itertools::Itertools;
use miette::{Result, bail, ensure, miette};

use kyzo_model::data_value_any;
use kyzo_model::value::{DataValue, Num, NumRepr, NumericOrd};

use crate::exec::stdlib::errors::{
    DivisionByZero, DomainError, IntegerOverflow, VecOpEmptyArgs, no_nan, no_nan_vec, vec_value,
};

/// Unary f64/vector scaffold: Num→f64→`f`, Vector→elementwise `f`.
fn unary_f64_op(op: &'static str, args: &[DataValue], f: impl Fn(f64) -> f64) -> Result<DataValue> {
    match &args[0] {
        DataValue::Num(n) => Ok(DataValue::Num(Num::float(f(n.to_f64())))),
        DataValue::Vector(v) => Ok(DataValue::Vector(vec_value(
            v.to_f64s().iter().map(|x| f(*x)).collect(),
        )?)),
        data_value_any!() => bail!("'{op}' requires numbers"),
    }
}

/// Unary f64/vector scaffold with a domain gate; out-of-domain → DomainError,
/// in-domain NaN → refused via [`no_nan`] / [`no_nan_vec`].
fn unary_f64_domain(
    op: &'static str,
    args: &[DataValue],
    in_domain: impl Fn(f64) -> bool,
    f: impl Fn(f64) -> f64,
) -> Result<DataValue> {
    match &args[0] {
        DataValue::Num(n) => {
            let a = n.to_f64();
            if !in_domain(a) {
                bail!(DomainError { op: op.into() });
            }
            no_nan(op, f(a))
        }
        DataValue::Vector(v) => {
            if v.to_f64s().iter().any(|x| !in_domain(*x)) {
                bail!(DomainError { op: op.into() });
            }
            no_nan_vec(op, v.to_f64s().iter().map(|x| f(*x)).collect())
        }
        data_value_any!() => bail!("'{op}' requires numbers"),
    }
}

/// Bytes pairwise bit-op scaffold (same-length check + zip-mutate).
fn bit_bytes_binop(
    op: &'static str,
    args: &[DataValue],
    combine: impl Fn(u8, u8) -> u8,
) -> Result<DataValue> {
    match (&args[0], &args[1]) {
        (DataValue::Bytes(left), DataValue::Bytes(right)) => {
            ensure!(
                left.len() == right.len(),
                "operands of '{op}' must have the same lengths"
            );
            let mut ret = left.clone();
            for (l, r) in ret.iter_mut().zip(right.iter()) {
                *l = combine(*l, *r);
            }
            Ok(DataValue::Bytes(ret))
        }
        _ => bail!("'{op}' requires bytes"),
    }
}

/// Unary Num/Vector scaffold for checked-int + float + elementwise vector ops.
fn unary_num_vec(
    op: &'static str,
    args: &[DataValue],
    on_int: impl FnOnce(i64) -> Result<i64>,
    on_float: impl FnOnce(f64) -> f64,
    on_vec: impl Fn(f64) -> f64,
) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Num(n) => match n.repr() {
            NumRepr::Int(i) => DataValue::Num(Num::int(on_int(i)?)),
            NumRepr::Float(f) => DataValue::Num(Num::float(on_float(f))),
        },
        DataValue::Vector(v) => {
            DataValue::Vector(vec_value(v.to_f64s().iter().map(|x| on_vec(*x)).collect())?)
        }
        data_value_any!() => bail!("'{op}' requires numbers"),
    })
}

pub(crate) fn op_abs(args: &[DataValue]) -> Result<DataValue> {
    unary_num_vec(
        "abs",
        args,
        |i| {
            i.checked_abs()
                .ok_or_else(|| IntegerOverflow { op: "abs" }.into())
        },
        f64::abs,
        f64::abs,
    )
}

pub(crate) fn op_acos(args: &[DataValue]) -> Result<DataValue> {
    // Defined only on [-1, 1]; outside, raw f64 returns NaN.
    unary_f64_domain("acos", args, |a| (-1.0..=1.0).contains(&a), f64::acos)
}

pub(crate) fn op_acosh(args: &[DataValue]) -> Result<DataValue> {
    // Defined only on [1, +inf).
    unary_f64_domain("acosh", args, |a| a >= 1.0, f64::acosh)
}

fn fold_num_args(
    op: &'static str,
    args: &[DataValue],
    i_init: i64,
    f_init: f64,
    on_int: impl Fn(i64, i64) -> Result<i64>,
    on_float: impl Fn(f64, f64) -> f64,
    combine_final: impl Fn(i64, f64) -> DataValue,
    vecs: impl FnOnce(&[DataValue]) -> Result<DataValue>,
    require_msg: &'static str,
) -> Result<DataValue> {
    let mut i_accum = i_init;
    let mut f_accum = f_init;
    for arg in args {
        match arg {
            DataValue::Num(n) => match n.repr() {
                NumRepr::Int(i) => i_accum = on_int(i_accum, i)?,
                NumRepr::Float(f) => f_accum = on_float(f_accum, f),
            },
            DataValue::Vector(_) => return vecs(args),
            data_value_any!() => bail!("{require_msg}"),
        }
    }
    let _ = op;
    Ok(combine_final(i_accum, f_accum))
}

pub(crate) fn op_add(args: &[DataValue]) -> Result<DataValue> {
    fold_num_args(
        "add",
        args,
        0,
        0.0,
        |a, b| a.checked_add(b).ok_or(IntegerOverflow { op: "add" }.into()),
        |a, b| a + b,
        |i, f| {
            if f == 0.0 {
                DataValue::Num(Num::int(i))
            } else {
                DataValue::Num(Num::float(i as f64 + f))
            }
        },
        add_vecs,
        "addition requires numbers",
    )
}

pub(crate) fn op_asin(args: &[DataValue]) -> Result<DataValue> {
    unary_f64_domain("asin", args, |a| (-1.0..=1.0).contains(&a), f64::asin)
}

pub(crate) fn op_asinh(args: &[DataValue]) -> Result<DataValue> {
    unary_f64_op("asinh", args, f64::asinh)
}

pub(crate) fn op_atan(args: &[DataValue]) -> Result<DataValue> {
    unary_f64_op("atan", args, f64::atan)
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
    // Open interval (-1, 1): endpoints diverge; outside → NaN.
    unary_f64_domain("atanh", args, |a| a.abs() < 1.0, f64::atanh)
}

pub(crate) fn op_bit_and(args: &[DataValue]) -> Result<DataValue> {
    bit_bytes_binop("bit_and", args, |l, r| l & r)
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
    bit_bytes_binop("bit_or", args, |l, r| l | r)
}

pub(crate) fn op_bit_xor(args: &[DataValue]) -> Result<DataValue> {
    bit_bytes_binop("bit_xor", args, |l, r| l ^ r)
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
    unary_f64_op("cos", args, f64::cos)
}

pub(crate) fn op_cosh(args: &[DataValue]) -> Result<DataValue> {
    unary_f64_op("cosh", args, f64::cosh)
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
    unary_f64_op("exp", args, f64::exp)
}

pub(crate) fn op_exp2(args: &[DataValue]) -> Result<DataValue> {
    unary_f64_op("exp2", args, f64::exp2)
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
    // Positive reals only: zero → -inf, negative → NaN.
    unary_f64_domain("ln", args, |a| a > 0.0, f64::ln)
}

pub(crate) fn op_log10(args: &[DataValue]) -> Result<DataValue> {
    unary_f64_domain("log10", args, |a| a > 0.0, f64::log10)
}

pub(crate) fn op_log2(args: &[DataValue]) -> Result<DataValue> {
    unary_f64_domain("log2", args, |a| a > 0.0, f64::log2)
}

fn fold_minmax(
    op: &'static str,
    args: &[DataValue],
    prefer_b: impl Fn(Num, Num) -> bool,
    empty: f64,
) -> Result<DataValue> {
    let res = args
        .iter()
        .try_fold(None, |accum, nxt| match (accum, nxt) {
            (None, d @ DataValue::Num(_)) => Ok(Some(d.clone())),
            (Some(DataValue::Num(a)), DataValue::Num(b)) => {
                // Ties keep `a` — deterministic and accumulation-friendly.
                let chosen = if prefer_b(a, *b) { *b } else { a };
                Ok(Some(DataValue::Num(chosen)))
            }
            _ => bail!("'{op}' can only be applied to numbers"),
        })?;
    match res {
        None => Ok(DataValue::Num(Num::float(empty))),
        Some(v) => Ok(v),
    }
}

pub(crate) fn op_max(args: &[DataValue]) -> Result<DataValue> {
    fold_minmax(
        "max",
        args,
        |a, b| NumericOrd::of(a) < NumericOrd::of(b),
        f64::NEG_INFINITY,
    )
}

pub(crate) fn op_min(args: &[DataValue]) -> Result<DataValue> {
    fold_minmax(
        "min",
        args,
        |a, b| NumericOrd::of(a) > NumericOrd::of(b),
        f64::INFINITY,
    )
}

pub(crate) fn op_minus(args: &[DataValue]) -> Result<DataValue> {
    unary_num_vec(
        "minus",
        args,
        |i| {
            i.checked_neg()
                .ok_or_else(|| IntegerOverflow { op: "minus" }.into())
        },
        |f| -f,
        |x| -x,
    )
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
    fold_num_args(
        "mul",
        args,
        1,
        1.0,
        |a, b| a.checked_mul(b).ok_or(IntegerOverflow { op: "mul" }.into()),
        |a, b| a * b,
        |i, f| {
            if f == 1.0 {
                DataValue::Num(Num::int(i))
            } else {
                DataValue::Num(Num::float(i as f64 * f))
            }
        },
        mul_vecs,
        "multiplication requires numbers",
    )
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
    unary_f64_op("sin", args, f64::sin)
}

pub(crate) fn op_sinh(args: &[DataValue]) -> Result<DataValue> {
    unary_f64_op("sinh", args, f64::sinh)
}

pub(crate) fn op_sqrt(args: &[DataValue]) -> Result<DataValue> {
    unary_f64_domain("sqrt", args, |a| a >= 0.0, f64::sqrt)
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
    unary_f64_op("tan", args, f64::tan)
}

pub(crate) fn op_tanh(args: &[DataValue]) -> Result<DataValue> {
    unary_f64_op("tanh", args, f64::tanh)
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

/// Fold vector±scalar args under a pairwise f64 combine (add / mul).
fn fold_vecs(
    op: &'static str,
    args: &[DataValue],
    same_len_msg: &'static str,
    scalar_msg: &'static str,
    scalar_requires: &'static str,
    combine: impl Fn(f64, f64) -> f64 + Copy,
) -> Result<DataValue> {
    if args.len() == 1 {
        return Ok(args[0].clone());
    }
    let Some((last, first)) = args.split_last() else {
        bail!(VecOpEmptyArgs { op });
    };
    let first = fold_vecs(
        op,
        first,
        same_len_msg,
        scalar_msg,
        scalar_requires,
        combine,
    )?;
    match (first, last) {
        (DataValue::Vector(a), DataValue::Vector(b)) => {
            if a.len() != b.len() {
                bail!("{same_len_msg}");
            }
            Ok(DataValue::Vector(vec_value(
                a.to_f64s()
                    .iter()
                    .zip(b.to_f64s().iter())
                    .map(|(x, y)| combine(*x, *y))
                    .collect(),
            )?))
        }
        (DataValue::Vector(a), b) => {
            let f = b.get_float().ok_or_else(|| miette!("{scalar_msg}"))?;
            Ok(DataValue::Vector(vec_value(
                a.to_f64s().iter().map(|x| combine(*x, f)).collect(),
            )?))
        }
        (a, DataValue::Vector(b)) => {
            let f = a.get_float().ok_or_else(|| miette!("{scalar_msg}"))?;
            Ok(DataValue::Vector(vec_value(
                b.to_f64s().iter().map(|x| combine(f, *x)).collect(),
            )?))
        }
        _ => bail!("{scalar_requires}"),
    }
}

fn add_vecs(args: &[DataValue]) -> Result<DataValue> {
    fold_vecs(
        "add",
        args,
        "can only add vectors of the same length",
        "can only add numbers to vectors",
        "addition requires numbers",
        |x, y| x + y,
    )
}

fn mul_vecs(args: &[DataValue]) -> Result<DataValue> {
    // CozoDB originally recursed into `add_vecs` here (`v1*v2*v3` → `(v1+v2)*v3`).
    // Fixed to recurse into multiplication; deliberate deviation.
    fold_vecs(
        "mul",
        args,
        "can only multiply vectors of the same length",
        "can only multiply vectors by numbers",
        "multiplication requires numbers",
        |x, y| x * y,
    )
}

/// `a.powf(b)` is partial in two ways: a negative base raised to a
/// fractional exponent has no real result (`NaN` — e.g. `(-1)^0.5`), and a
/// zero base raised to a negative exponent diverges to an infinity (e.g.
/// `0^-1`, the same shape as a division by zero expressed through `pow`).
fn pow_out_of_domain(a: f64, b: f64) -> bool {
    (a < 0.0 && b.fract() != 0.0) || (a == 0.0 && b < 0.0)
}
