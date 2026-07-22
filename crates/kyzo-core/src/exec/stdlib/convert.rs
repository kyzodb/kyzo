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
            None => {
                let f = n.to_f64();
                DataValue::Num(Num::int(f as i64))
            }
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
        DataValue::Bool(b) => *b as i64,
        DataValue::Num(n) => (NumericOrd::of(*n) != NumericOrd::of(Num::int(0))) as i64,
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
            Json::Bool(b) => *b as i64,
            Json::Num(n) => (NumericOrd::of(n.num()) != NumericOrd::of(Num::int(0))) as i64,
            Json::Str(s) => !s.is_empty() as i64,
            Json::Arr(a) => !a.is_empty() as i64,
            Json::Obj(o) => !o.entries().is_empty() as i64,
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
            None => DataValue::Null,
            Some(t) => {
                let (s, subs) = t.to_unix();
                let s = (s as f64) + (subs as f64 / 10_000_000.);
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
        VecElementType::F32 => f as f32 as f64,
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
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f64)
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
