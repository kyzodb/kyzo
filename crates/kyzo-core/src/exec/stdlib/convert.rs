/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! convert.rs — stdlib kernel (move_plan).
use std::str::FromStr;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use miette::{Result, bail, miette};

use kyzo_model::data_value_any;
use kyzo_model::value::{DataValue, Json, Num, NumericOrd, Validity, ValidityTs};

use crate::exec::stdlib::errors::vec_value;
use crate::exec::stdlib::text::val2str;
use kyzo_model::schema::VecElementType;

/// Truncate a float toward zero into an `i64`, refusing non-finite and
/// values outside the exact i64 range (typed — never saturating `as`).
pub(crate) fn f64_trunc_to_i64(f: f64) -> Result<i64> {
    if !f.is_finite() {
        bail!("'to_int' refuses non-finite float");
    }
    let truncated = f.trunc();
    Num::float(truncated)
        .to_int_coerced()
        .ok_or_else(|| miette!("'to_int' cannot represent float {f} in i64"))
}

/// Round through IEEE754 binary32 precision, then widen back to f64.
/// Soft conversion — no `as f32` (bs_detector).
pub(crate) fn via_f32_precision(f: f64) -> f64 {
    f64::from(f64_to_f32(f))
}

/// Soft `f64` → `f32` (IEEE round-ties-to-even) — no numeric `as` cast.
pub(crate) fn f64_to_f32(f: f64) -> f32 {
    f32::from_bits(f64_bits_to_f32_bits(f.to_bits()))
}

/// usize → f64 without a numeric `as` cast.
pub(crate) fn usize_to_f64(n: usize) -> f64 {
    match u32::try_from(n) {
        Ok(v) => f64::from(v),
        Err(_gt_u32) => match i64::try_from(n) {
            Ok(i) => Num::int(i).to_f64(),
            // Pointer width ≤ 64: usize → u64 is total via LE assemble.
            Err(_gt_i64) => u64_to_f64(crate::rules::convert::u64_from_usize_total(n)),
        },
    }
}

/// u64 → f64 without a numeric `as` cast.
pub(crate) fn u64_to_f64(n: u64) -> f64 {
    match i64::try_from(n) {
        Ok(i) => Num::int(i).to_f64(),
        Err(_above_i64_max) => {
            let lo = crate::rules::convert::u32_low(n);
            let hi = crate::rules::convert::u32_hi(n);
            f64::from(hi) * 4_294_967_296.0 + f64::from(lo)
        }
    }
}

/// Approximate i128 as f64 — model seat (copy_detector).
#[inline]
pub(crate) fn i128_approx_f64(n: i128) -> f64 {
    kyzo_model::value::convert::i128_approx_f64(n)
}

fn f64_bits_to_f32_bits(a: u64) -> u32 {
    // Softfloat-style binary64→binary32, round-ties-to-even.
    // Sign is bit 63; IEEE exp field is bits[62:52] ∈ 0..=0x7FF — LE doors.
    let sign = crate::rules::convert::u32_low(a >> 63) << 31;
    let exp = crate::rules::convert::i32_from_u11((a >> 52) & 0x7FF);
    let frac = a & 0x000F_FFFF_FFFF_FFFF;

    if exp == 0x7FF {
        // Inf / NaN
        let nan_bit = if frac != 0 { 0x0040_0000u32 } else { 0 };
        return sign | 0x7F80_0000 | nan_bit;
    }
    // Unbias, rebias for f32
    let mut exp32 = exp - 1023 + 127;
    if exp == 0 {
        // ±0 or subnormal → ±0 in f32 (flush)
        return sign;
    }
    if exp32 >= 0xFF {
        // Overflow → infinity
        return sign | 0x7F80_0000;
    }
    if exp32 <= 0 {
        // Underflow / f32 subnormal
        if exp32 < -24 {
            return sign; // flush to zero
        }
        // Align so the implicit 1 falls into the subnormal window
        let shift = match u32::try_from(1 - exp32) {
            Ok(s) => s,
            Err(_) => {
                return sign;
            }
        };
        let mut mant = frac | 0x0010_0000_0000_0000; // explicit leading 1
        // Keep round bit
        let round_bit = (mant >> (shift + 28)) & 1;
        let sticky = if mant & ((1u64 << (shift + 28)) - 1) != 0 {
            1u64
        } else {
            0
        };
        mant >>= shift + 29;
        // mant already shifted into ≤23-bit window; LE low door.
        let mut m32 = crate::rules::convert::u32_low(mant);
        // ties-to-even
        if round_bit == 1 && (sticky == 1 || m32 & 1 == 1) {
            // INVARIANT(F64ToF32RoundTieEven): mantissa+1 on round-up; wrap to 0
            // is the IEEE overflow into the exponent bump path (subnormal lane
            // returns immediately — no exponent field here).
            m32 = (std::num::Wrapping(m32) + std::num::Wrapping(1)).0;
        }
        return sign | m32;
    }
    // Normal number
    let mut mant = frac;
    // Round: bits below the 23 kept for f32
    let round_bit = (mant >> 28) & 1;
    let sticky = if mant & 0x0FFF_FFFF != 0 { 1u64 } else { 0 };
    mant >>= 29;
    let mut m32 = crate::rules::convert::u32_low(mant);
    if round_bit == 1 && (sticky == 1 || m32 & 1 == 1) {
        // INVARIANT(F64ToF32RoundTieEven): mantissa+1 on round-up; wrap to 0
        // with m32==0x0080_0000 is handled next line (bump exp / clamp inf).
        m32 = (std::num::Wrapping(m32) + std::num::Wrapping(1)).0;
        if m32 == 0x0080_0000 {
            // mantissa overflow → bump exponent
            m32 = 0;
            exp32 += 1;
            if exp32 >= 0xFF {
                return sign | 0x7F80_0000;
            }
        }
    }
    let e_bits = match u32::try_from(exp32) {
        Ok(e) => e << 23,
        Err(_) => 0x7F80_0000,
    };
    sign | e_bits | (m32 & 0x007F_FFFF)
}

pub(crate) fn op_decode_base64(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::Str(s) => {
            let b = STANDARD
                .decode(s.as_bytes())
                .map_err(|_| miette!("Data is not properly encoded"))?;
            Ok(DataValue::Bytes(b))
        }
        data_value_any!() => bail!("'decode_base64' requires strings"),
    }
}

pub(crate) fn op_encode_base64(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        DataValue::Bytes(b) => {
            let s = STANDARD.encode(b);
            Ok(DataValue::from(s))
        }
        data_value_any!() => bail!("'encode_base64' requires bytes"),
    }
}

pub(crate) fn op_to_bool(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(match &args[0] {
        DataValue::Null => false,
        DataValue::Bool(b) => *b,
        DataValue::Num(n) => NumericOrd::of(*n) != NumericOrd::of(Num::int(0)),
        DataValue::Str(s) => !s.is_empty(),
        DataValue::Bytes(b) => !b.is_empty(),
        DataValue::Uuid(u) => !u.as_uuid().is_nil(),
        DataValue::Regex(r) => !r.pattern().is_empty(),
        DataValue::List(l) => !l.is_empty(),
        DataValue::Set(s) => !s.is_empty(),
        DataValue::Vector(_) => true,
        DataValue::Validity(vld) => vld.is_assert(),
        DataValue::Interval(_) => true,
        DataValue::Geometry(_) => true,
        DataValue::Json(json) => match json {
            Json::Null => false,
            Json::Bool(b) => *b,
            Json::Num(n) => NumericOrd::of(n.num()) != NumericOrd::of(Num::int(0)),
            Json::Str(s) => !s.is_empty(),
            Json::Arr(a) => !a.is_empty(),
            Json::Obj(o) => !o.entries().is_empty(),
        },
    }))
}

pub(crate) fn op_to_float(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Num(n) => n.to_f64().into(),
        DataValue::Null => DataValue::from(0.0),
        DataValue::Bool(b) => DataValue::from(if *b { 1.0 } else { 0.0 }),
        DataValue::Str(t) => match t.as_str() {
            "PI" => std::f64::consts::PI.into(),
            "E" => std::f64::consts::E.into(),
            "NAN" => f64::NAN.into(),
            "INF" => f64::INFINITY.into(),
            "NEG_INF" => f64::NEG_INFINITY.into(),
            s => f64::from_str(s)
                .map_err(|_| miette!("The string cannot be interpreted as float"))?
                .into(),
        },
        v @ (data_value_any!()) => bail!("'to_float' does not recognize {:?}", v),
    })
}

pub(crate) fn op_to_int(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Num(n) => match n.as_int() {
            None => DataValue::Num(Num::int(f64_trunc_to_i64(n.to_f64())?)),
            Some(i) => DataValue::Num(Num::int(i)),
        },
        DataValue::Null => DataValue::from(0),
        DataValue::Bool(b) => DataValue::from(if *b { 1 } else { 0 }),
        DataValue::Str(t) => {
            let s = t.as_str();
            i64::from_str(s)
                .map_err(|_| miette!("The string cannot be interpreted as int"))?
                .into()
        }
        DataValue::Validity(vld) => DataValue::Num(Num::int(vld.ts_micros())),
        v @ (data_value_any!()) => bail!("'to_int' does not recognize {:?}", v),
    })
}

pub(crate) fn op_to_string(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::Str(val2str(&args[0])))
}

pub(crate) fn op_to_unity(args: &[DataValue]) -> Result<DataValue> {
    Ok(DataValue::from(match &args[0] {
        DataValue::Null => 0,
        DataValue::Bool(b) => i64::from(*b),
        DataValue::Num(n) => i64::from(NumericOrd::of(*n) != NumericOrd::of(Num::int(0))),
        DataValue::Str(s) => i64::from(!s.is_empty()),
        DataValue::Bytes(b) => i64::from(!b.is_empty()),
        DataValue::Uuid(u) => i64::from(!u.as_uuid().is_nil()),
        DataValue::Regex(r) => i64::from(!r.pattern().is_empty()),
        DataValue::List(l) => i64::from(!l.is_empty()),
        DataValue::Set(s) => i64::from(!s.is_empty()),
        DataValue::Vector(_) => 1,
        DataValue::Validity(vld) => i64::from(vld.is_assert()),
        DataValue::Interval(_) => 1,
        DataValue::Geometry(_) => 1,
        DataValue::Json(json) => match json {
            Json::Null => 0,
            Json::Bool(b) => i64::from(*b),
            Json::Num(n) => i64::from(NumericOrd::of(n.num()) != NumericOrd::of(Num::int(0))),
            Json::Str(s) => i64::from(!s.is_empty()),
            Json::Arr(a) => i64::from(!a.is_empty()),
            Json::Obj(o) => i64::from(!o.entries().is_empty()),
        },
    }))
}

pub(crate) fn op_to_uuid(args: &[DataValue]) -> Result<DataValue> {
    match &args[0] {
        d @ DataValue::Uuid(_u) => Ok(d.clone()),
        DataValue::Str(s) => {
            let id = uuid::Uuid::try_parse(s.as_str()).map_err(|_| miette!("invalid UUID"))?;
            Ok(DataValue::uuid(id))
        }
        data_value_any!() => bail!("'to_uuid' requires a string"),
    }
}

pub(crate) fn op_uuid_timestamp(args: &[DataValue]) -> Result<DataValue> {
    Ok(match &args[0] {
        DataValue::Uuid(u) => match u.as_uuid().get_timestamp() {
            None => {
            // Absent cell — SQL NULL is the published render.
            DataValue::Null
        },
            Some(t) => {
                let (s, subs) = t.to_unix();
                let s_secs = match u32::try_from(s) {
                    Ok(v) => f64::from(v),
                    Err(_secs_fit_u32_for_uuid_ts) => {
                        bail!("'uuid_timestamp' seconds exceed u32 range")
                    }
                };
                let s = s_secs + (f64::from(subs) / 10_000_000.);
                s.into()
            }
        },
        data_value_any!() => bail!("not an UUID"),
    })
}

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
    // User-facing validity construction: mint the coordinate through
    // `for_assertion`, not `from_raw`. The reserved terminal is a typed
    // refusal at this door (P005).
    let coord = ValidityTs::for_assertion(ts)
        .ok_or_else(|| miette!("'validity' refuses the reserved terminal tick (i64::MAX)"))?;
    let vld = Validity::new(coord, is_assert).ok_or_else(|| {
        miette!("'validity' refuses assert of the reserved terminal tick (i64::MAX)")
    })?;
    Ok(DataValue::Validity(vld.into()))
}

pub(crate) fn op_vec(args: &[DataValue]) -> Result<DataValue> {
    let t = vec_element_type(args.get(1), "vec")?;
    // Vector VALUES are always f64 canonical; an 'F32' request rounds
    // each component through f32 precision (the observable meaning the
    // element type ever had) before widening back.
    let quantize = |f: f64| match t {
        VecElementType::F32 => via_f32_precision(f),
        VecElementType::F64 => f,
    };

    match &args[0] {
        DataValue::Json(j) => {
            // Non-array JSON is a typed error; the CozoDB original unwrapped
            // `as_array()` here and aborted on e.g. `vec(json('{}'))`.
            let Json::Arr(arr) = j else {
                bail!("'vec' requires a JSON array");
            };
            let mut components = Vec::with_capacity(arr.len());
            for el in arr {
                let Json::Num(n) = el else {
                    bail!("'vec' requires a list of numbers");
                };
                components.push(quantize(n.num().to_f64()));
            }
            Ok(DataValue::Vector(vec_value(components)?))
        }
        DataValue::List(l) => {
            let mut components = Vec::with_capacity(l.len());
            for el in l {
                let f = el
                    .get_float()
                    .ok_or_else(|| miette!("'vec' requires a list of numbers"))?;
                components.push(quantize(f));
            }
            Ok(DataValue::Vector(vec_value(components)?))
        }
        DataValue::Vector(v) => Ok(DataValue::Vector(vec_value(
            v.to_f64s().iter().map(|&f| quantize(f)).collect(),
        )?)),
        DataValue::Str(s) => {
            // Base64-encoded raw floats, decoded little-endian by definition.
            // Trailing bytes short of a full element are an error (the
            // CozoDB original silently ignored them) — the same
            // exact-length law the schema coercion path applies.
            let bytes = STANDARD
                .decode(s)
                .map_err(|_| miette!("Data is not base64 encoded"))?;
            let el_size = match t {
                VecElementType::F32 => 4,
                VecElementType::F64 => 8,
            };
            if !bytes.len().is_multiple_of(el_size) {
                bail!(
                    "vector byte payload of length {} is not a whole number of {}-byte elements",
                    bytes.len(),
                    el_size
                );
            }
            let components: Vec<f64> = match t {
                VecElementType::F32 => bytes
                    .chunks_exact(4)
                    // In bounds: `chunks_exact(4)` yields 4-byte chunks.
                    .map(|c| f64::from(f32::from_le_bytes([c[0], c[1], c[2], c[3]])))
                    .collect(),
                VecElementType::F64 => bytes
                    .chunks_exact(8)
                    // In bounds: `chunks_exact(8)` yields 8-byte chunks.
                    .map(|c| f64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
                    .collect(),
            };
            Ok(DataValue::Vector(vec_value(components)?))
        }
        data_value_any!() => bail!("'vec' requires a list or a vector"),
    }
}

/// The optional trailing element-type argument shared by `vec` and
/// `rand_vec`.
pub(crate) fn vec_element_type(arg: Option<&DataValue>, op: &str) -> Result<VecElementType> {
    match arg {
        Some(DataValue::Str(s)) => match s.as_str() {
            "F32" | "Float" => Ok(VecElementType::F32),
            "F64" | "Double" => Ok(VecElementType::F64),
            unrecognized => bail!("'{op}' does not recognize type {unrecognized}"),
        },
        None => Ok(VecElementType::F32),
        Some(data_value_any!()) => bail!("'{op}' requires a string as second argument"),
    }
}
