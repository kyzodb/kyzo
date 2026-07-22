/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Column typing and coerce — the contract facts must pass to rest in a relation.

use std::fmt::{Display, Formatter};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use miette::{Diagnostic, Result, bail, ensure};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data_value_any;
use crate::envelope::json_from_serde;
use crate::program::expr::Expr;
use crate::value::json_convert::to_json;
use crate::value::{DataValue, Validity, ValidityTs, Vector};

/// Schema vocabulary: a vector column's declared element width. Stored
/// vector VALUES are always f64 canonical (format v1); `F32` columns
/// constrain declared width and let engines pack narrower internally —
/// every f32 is exactly representable as f64, so identity round-trips.
#[derive(
    Debug, Copy, Clone, PartialEq, Eq, Hash, serde_derive::Serialize, serde_derive::Deserialize,
)]
pub enum VecElementType {
    F32,
    F64,
}

/// Whether a column admits Null — a closed sum, not an open bool poke.
#[derive(Debug, Clone, Copy, Eq, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
pub enum ColNullability {
    Required,
    Optional,
}


/// Soft `f64 → f32 → f64` (IEEE round-ties-to-even) — no numeric `as` cast.
fn soft_f64_via_f32(f: f64) -> f64 {
    f64::from(f32::from_bits(f64_bits_to_f32_bits(f.to_bits())))
}

fn f64_bits_to_f32_bits(a: u64) -> u32 {
    let sign = match u32::try_from(a >> 63) {
        Ok(s) => s << 31,
        Err(_bit) => 0,
    };
    let exp = match i32::try_from((a >> 52) & 0x7FF) {
        Ok(e) => e,
        Err(_exp_field) => 0,
    };
    let frac = a & 0x000F_FFFF_FFFF_FFFF;
    if exp == 0x7FF {
        let nan_bit = if frac != 0 { 0x0040_0000u32 } else { 0 };
        return sign | 0x7F80_0000 | nan_bit;
    }
    let mut exp32 = exp - 1023 + 127;
    if exp == 0 {
        return sign;
    }
    if exp32 >= 0xFF {
        return sign | 0x7F80_0000;
    }
    if exp32 <= 0 {
        // Flush/subnormal: keep sign only (matches convert.rs softfloat door).
        return sign;
    }
    let mut m32 = match u32::try_from(frac >> 29) {
        Ok(v) => v,
        Err(_) => 0,
    };
    let round_bit = match u32::try_from((frac >> 28) & 1) {
        Ok(v) => v,
        Err(_) => 0,
    };
    let sticky = if (frac & ((1u64 << 28) - 1)) != 0 { 1u32 } else { 0 };
    if round_bit == 1 && (sticky == 1 || m32 & 1 == 1) {
        // INVARIANT(F64ToF32RoundTieEven): mantissa+1 on round-up; wrap to 0
        // with m32==0x0080_0000 is handled next (bump exp / clamp inf).
        m32 = (std::num::Wrapping(m32) + std::num::Wrapping(1)).0;
        if m32 == 0x0080_0000 {
            m32 = 0;
            exp32 += 1;
            if exp32 >= 0xFF {
                return sign | 0x7F80_0000;
            }
        }
    }
    let exp_bits = match u32::try_from(exp32) {
        Ok(v) => v << 23,
        Err(_) => 0x7F80_0000,
    };
    sign | exp_bits | (m32 & 0x007F_FFFF)
}


impl ColNullability {
    pub fn from_bool(nullable: bool) -> ColNullability {
        if nullable {
            ColNullability::Optional
        } else {
            ColNullability::Required
        }
    }

    pub fn is_nullable(self) -> bool {
        matches!(self, ColNullability::Optional)
    }
}

/// Column typing: private fields; nullability only via [`ColNullability`].
#[derive(Debug, Clone, Eq, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
pub struct NullableColType {
    coltype: ColType,
    /// Wire key stays `nullable` (bool); in-memory authority is [`ColNullability`].
    #[serde(rename = "nullable", with = "col_nullability_as_bool")]
    nullability: ColNullability,
}

mod col_nullability_as_bool {
    use super::ColNullability;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &ColNullability, serializer: S) -> Result<S::Ok, S::Error> {
        v.is_nullable().serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<ColNullability, D::Error> {
        bool::deserialize(deserializer).map(ColNullability::from_bool)
    }
}

impl NullableColType {
    /// Typed door: nullability is a sum, not an unbound bool pair.
    pub fn new(coltype: ColType, nullability: ColNullability) -> NullableColType {
        NullableColType {
            coltype,
            nullability,
        }
    }

    pub fn required(coltype: ColType) -> NullableColType {
        Self::new(coltype, ColNullability::Required)
    }

    pub fn optional(coltype: ColType) -> NullableColType {
        Self::new(coltype, ColNullability::Optional)
    }

    pub fn coltype(&self) -> &ColType {
        &self.coltype
    }

    pub fn nullability(&self) -> ColNullability {
        self.nullability
    }

    pub fn is_nullable(&self) -> bool {
        self.nullability.is_nullable()
    }
}

impl Display for NullableColType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match &self.coltype {
            ColType::Any => f.write_str("Any")?,
            ColType::Bool => f.write_str("Bool")?,
            ColType::Int => f.write_str("Int")?,
            ColType::Float => f.write_str("Float")?,
            ColType::String => f.write_str("String")?,
            ColType::Bytes => f.write_str("Bytes")?,
            ColType::Uuid => f.write_str("Uuid")?,
            ColType::Validity => f.write_str("Validity")?,
            ColType::List { eltype, len } => {
                f.write_str("[")?;
                write!(f, "{eltype}")?;
                if let Some(l) = len {
                    write!(f, ";{l}")?;
                }
                f.write_str("]")?;
            }
            ColType::Tuple(t) => {
                f.write_str("(")?;
                let l = t.len();
                for (i, el) in t.iter().enumerate() {
                    write!(f, "{el}")?;
                    if i != l - 1 {
                        f.write_str(",")?
                    }
                }
                f.write_str(")")?;
            }
            ColType::Vec { eltype, len } => {
                f.write_str("<")?;
                match eltype {
                    VecElementType::F32 => f.write_str("F32")?,
                    VecElementType::F64 => f.write_str("F64")?,
                }
                write!(f, ";{len}")?;
                f.write_str(">")?;
            }
            ColType::Json => {
                f.write_str("Json")?;
            }
        }
        if self.is_nullable() {
            f.write_str("?")?;
        }
        Ok(())
    }
}

/// Proven list / vector length on a column schema — the only length
/// type [`ColType`] may carry.
#[derive(
    Debug,
    Clone,
    Copy,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    serde_derive::Deserialize,
    serde_derive::Serialize,
)]
pub struct ColLen(usize);

impl ColLen {
    pub fn new(n: usize) -> ColLen {
        ColLen(n)
    }

    pub fn get(self) -> usize {
        self.0
    }
}

impl From<usize> for ColLen {
    fn from(n: usize) -> ColLen {
        ColLen(n)
    }
}

impl From<ColLen> for usize {
    fn from(n: ColLen) -> usize {
        n.0
    }
}

impl Display for ColLen {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.0, f)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
pub enum ColType {
    Any,
    Bool,
    Int,
    Float,
    String,
    Bytes,
    Uuid,
    List {
        eltype: Box<NullableColType>,
        len: Option<ColLen>,
    },
    Vec {
        eltype: VecElementType,
        len: ColLen,
    },
    Tuple(Vec<NullableColType>),
    Validity,
    Json,
}

#[derive(Debug, Clone, Eq, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
pub struct ColumnDef {
    pub name: SmartString<LazyCompact>,
    pub typing: NullableColType,
    pub default_gen: Option<Expr>,
}

impl NullableColType {
    /// Parse an untyped value into this column's typed shape, or fail.
    ///
    /// This is the contract applied at the data boundary: the returned
    /// value is proven to satisfy the column typing, so nothing downstream
    /// re-checks it. The mutation / admit tier is the live consumer.
    pub fn coerce(&self, data: DataValue, cur_vld: ValidityTs) -> Result<DataValue> {
        if matches!(data, DataValue::Null) {
            return if self.is_nullable() {
                Ok(data)
            } else {
                #[derive(Debug, Error, Diagnostic)]
                #[error("encountered null value for non-null type {0}")]
                #[diagnostic(code(eval::coercion_null))]
                struct InvalidNullValue(NullableColType);

                Err(InvalidNullValue(self.clone()).into())
            };
        }

        #[derive(Debug, Error, Diagnostic)]
        #[error("data coercion failed: expected type {0}, got value {1:?}")]
        #[diagnostic(code(eval::coercion_failed))]
        struct DataCoercionFailed(NullableColType, DataValue);

        #[derive(Debug, Error, Diagnostic)]
        #[error("bad list length: expected datatype {0}, got length {1}")]
        #[diagnostic(code(eval::coercion_bad_list_len))]
        struct BadListLength(NullableColType, usize);

        let make_err = || DataCoercionFailed(self.clone(), data.clone());

        Ok(match &self.coltype {
            ColType::Any => match data {
                DataValue::Set(s) => DataValue::List(s.into_iter().collect::<Vec<_>>()),
                d @ (data_value_any!()) => d,
            },
            ColType::Bool => DataValue::from(data.get_bool().ok_or_else(make_err)?),
            ColType::Int => DataValue::from(data.get_int().ok_or_else(make_err)?),
            ColType::Float => DataValue::from(data.get_float().ok_or_else(make_err)?),
            ColType::String => {
                if matches!(data, DataValue::Str(_)) {
                    data
                } else {
                    bail!(make_err())
                }
            }
            ColType::Bytes => match data {
                d @ DataValue::Bytes(_) => d,
                DataValue::Str(s) => {
                    #[derive(Debug, Error, Diagnostic)]
                    #[error("cannot decode string as base64-encoded bytes: {0}")]
                    #[diagnostic(code(eval::coercion_bad_base_64))]
                    struct BadBase64EncodedString(String);
                    let b = STANDARD
                        .decode(s.as_bytes())
                        .map_err(|e| BadBase64EncodedString(e.to_string()))?;
                    DataValue::Bytes(b)
                }
                data_value_any!() => bail!(make_err()),
            },
            ColType::Uuid => match data {
                u @ DataValue::Uuid(_) => u,
                // A UUID string literal coerces by parsing — the embedder
                // boundary (and KyzoScript) hand UUIDs as strings.
                DataValue::Str(ref s) => {
                    DataValue::uuid(uuid::Uuid::try_parse(s).map_err(|_| make_err())?)
                }
                data_value_any!() => bail!(make_err()),
            },
            ColType::List { eltype, len } => {
                if let DataValue::List(l) = data {
                    if let Some(expected) = len {
                        ensure!(
                            expected.get() == l.len(),
                            BadListLength(self.clone(), l.len())
                        )
                    }
                    DataValue::List(
                        l.into_iter()
                            .map(|el| eltype.coerce(el, cur_vld))
                            .collect::<Result<Vec<_>, _>>()?,
                    )
                } else {
                    bail!(make_err())
                }
            }
            ColType::Vec { eltype, len } => match &data {
                DataValue::List(l) => {
                    if l.len() != len.get() {
                        bail!(BadListLength(self.clone(), l.len()))
                    }
                    let collected: Vec<f64> = l
                        .iter()
                        .map(|el| {
                            el.get_float()
                                .map(|f| match eltype {
                                    VecElementType::F32 => soft_f64_via_f32(f),
                                    VecElementType::F64 => f,
                                })
                                .ok_or_else(make_err)
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    DataValue::Vector(Vector::try_new(collected).ok_or_else(|| make_err())?)
                }
                DataValue::Vector(arr) => {
                    if len.get() != arr.len() {
                        bail!(make_err())
                    }
                    // The declared element type is a precision constraint on
                    // the f64-canonical components, not a storage variant.
                    if matches!(eltype, VecElementType::F32)
                        && arr
                            .to_f64s()
                            .iter()
                            .any(|&f| soft_f64_via_f32(f) != f && !f.is_nan())
                    {
                        bail!(make_err())
                    }
                    data
                }
                DataValue::Str(s) => {
                    // Base64-encoded raw floats. Little-endian is the
                    // defined byte convention for vector payloads (the
                    // CozoDB original reinterpreted the buffer through an
                    // unsafe, unaligned, native-endian pointer cast). The
                    // decoded byte length must be exactly `len` elements: a
                    // mismatch — including trailing bytes short of a full
                    // element — is an error, never a silent truncation.
                    let bytes = STANDARD.decode(s).map_err(|_| make_err())?;
                    let collected: Vec<f64> = match eltype {
                        VecElementType::F32 => {
                            // Division form: the multiplication form wraps in
                            // release on a pathological declared length.
                            if bytes.len() / size_of::<f32>() != len.get()
                                || !bytes.len().is_multiple_of(size_of::<f32>())
                            {
                                bail!(make_err())
                            }
                            bytes
                                .chunks_exact(4)
                                // In bounds: `chunks_exact(4)` yields 4-byte chunks.
                                .map(|c| {
                                    let mut arr = [0u8; 4];
                                    arr.copy_from_slice(c);
                                    f64::from(f32::from_le_bytes(arr))
                                })
                                .collect()
                        }
                        VecElementType::F64 => {
                            if bytes.len() / size_of::<f64>() != len.get()
                                || !bytes.len().is_multiple_of(size_of::<f64>())
                            {
                                bail!(make_err())
                            }
                            bytes
                                .chunks_exact(8)
                                // In bounds: `chunks_exact(8)` yields 8-byte chunks.
                                .map(|c| {
                                    let mut arr = [0u8; 8];
                                    arr.copy_from_slice(c);
                                    f64::from_le_bytes(arr)
                                })
                                .collect()
                        }
                    };
                    DataValue::Vector(Vector::try_new(collected).ok_or_else(|| make_err())?)
                }
                data_value_any!() => bail!(make_err()),
            },
            ColType::Tuple(typ) => {
                if let DataValue::List(l) = data {
                    ensure!(typ.len() == l.len(), BadListLength(self.clone(), l.len()));
                    DataValue::List(
                        l.into_iter()
                            .zip(typ.iter())
                            .map(|(el, t)| t.coerce(el, cur_vld))
                            .collect::<Result<Vec<_>, _>>()?,
                    )
                } else {
                    bail!(make_err())
                }
            }
            ColType::Validity => {
                #[derive(Debug, Error, Diagnostic)]
                #[error("{0} cannot be coerced into validity")]
                #[diagnostic(code(eval::invalid_validity))]
                struct InvalidValidity(DataValue);

                match data {
                    vld @ DataValue::Validity(_) => vld,
                    DataValue::Str(s) => match &s as &str {
                        "ASSERT" => {
                            let Some(v) = Validity::new(cur_vld, true) else {
                                bail!(InvalidValidity(DataValue::Str("ASSERT".into())));
                            };
                            DataValue::Validity(v.into())
                        }
                        "RETRACT" => {
                            let Some(v) = Validity::new(cur_vld, false) else {
                                bail!(InvalidValidity(DataValue::Str("RETRACT".into())));
                            };
                            DataValue::Validity(v.into())
                        }
                        s => {
                            let (is_assert, ts_str) = match s.strip_prefix('~') {
                                None => (true, s),
                                Some(remaining) => (false, remaining),
                            };
                            let ts: jiff::Timestamp = ts_str
                                .parse()
                                .map_err(|_| InvalidValidity(DataValue::Str(s.into())))?;
                            // Signed microseconds floored toward negative
                            // infinity (shared with `str2vld`, so validity
                            // coercion and parse agree on the microsecond that
                            // contains a sub-microsecond instant): a pre-1970
                            // date is a negative count, not a panic (the CozoDB
                            // original unwrapped `duration_since(UNIX_EPOCH)` on
                            // a `SystemTime` and aborted on any pre-epoch input).
                            let microseconds = crate::timestamp_to_micros(ts);

                            if microseconds == i64::MAX || microseconds == i64::MIN {
                                bail!(InvalidValidity(DataValue::Str(s.into())))
                            }

                            let Some(v) =
                                Validity::new(ValidityTs::of_micros(microseconds), is_assert)
                            else {
                                bail!(InvalidValidity(DataValue::Str(s.into())));
                            };
                            DataValue::Validity(v.into())
                        }
                    },
                    DataValue::List(l) => {
                        if l.len() == 2 {
                            let o_ts = l[0].get_int();
                            let o_is_assert = l[1].get_bool();
                            if let (Some(ts), Some(is_assert)) = (o_ts, o_is_assert) {
                                if ts == i64::MAX || ts == i64::MIN {
                                    bail!(InvalidValidity(DataValue::List(l)))
                                }
                                let Some(v) = Validity::new(ValidityTs::of_micros(ts), is_assert)
                                else {
                                    bail!(InvalidValidity(DataValue::List(l)));
                                };
                                return Ok(DataValue::Validity(v.into()));
                            }
                        }
                        bail!(InvalidValidity(DataValue::List(l)))
                    }
                    v @ (data_value_any!()) => bail!(InvalidValidity(v)),
                }
            }
            ColType::Json => {
                // One door: kind-preserving tags (Bytes/Uuid/Vector/Regex)
                // live in `to_json` — never a second hand-rolled render.
                DataValue::Json(json_from_serde(&to_json(&data)))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{RegexFlags, RegexSource, ValidityTs};
    use miette::{IntoDiagnostic, Result};

    #[test]
    fn json_coerce_preserves_regex_flags_via_to_json() -> Result<()> {
        let col = NullableColType::required(ColType::Json);
        let cur = ValidityTs::of_micros(0);
        let ci = DataValue::Regex(
            RegexSource::validated(RegexFlags::CASE_INSENSITIVE, "foo".into())?,
        );
        let cs = DataValue::Regex(
            RegexSource::validated(RegexFlags::NONE, "foo".into())?,
        );
        let j_ci = col.coerce(ci, cur)?;
        let j_cs = col.coerce(cs, cur)?;
        assert_ne!(j_ci, j_cs, "Json coerce must not drop regex flags");
        Ok(())
    }

    #[test]
    fn required_null_refuses_optional_null_admits() -> Result<()> {
        let cur = ValidityTs::of_micros(0);
        assert!(
            NullableColType::required(ColType::Int)
                .coerce(DataValue::Null, cur)
                .is_err()
        );
        assert_eq!(
            NullableColType::optional(ColType::Int)
                .coerce(DataValue::Null, cur)
                ?,
            DataValue::Null
        );
        Ok(())
    }

    #[test]
    fn bytes_from_base64_string_and_uuid_from_str() -> Result<()> {
        let cur = ValidityTs::of_micros(0);
        let b = NullableColType::required(ColType::Bytes)
            .coerce(DataValue::from("AQID"), cur)
            ?;
        assert_eq!(b, DataValue::Bytes(vec![1, 2, 3]));
        let u = NullableColType::required(ColType::Uuid)
            .coerce(DataValue::from("550e8400-e29b-41d4-a716-446655440000"), cur)
            ?;
        assert!(matches!(u, DataValue::Uuid(_)));
        Ok(())
    }
}
