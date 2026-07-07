/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): `VecElementType` is imported from the value model instead of
 * redefined here; the base64 vector decode is safe and little-endian by
 * definition (the original used an unsafe, unaligned, native-endian pointer
 * cast) and rejects length mismatches instead of truncating; pre-epoch
 * validity timestamps parse to signed microseconds instead of panicking;
 * and the list-to-vector coercion builds arrays directly from collected
 * values.
 */

//! The schema vocabulary: what a stored relation promises about its rows.
//!
//! A column's typing ([`NullableColType`]) is a **contract facts must pass
//! to rest in a relation**: nothing enters storage without satisfying it.
//! [`NullableColType::coerce`] is that contract applied at the data
//! boundary — fallible parsing, not validation. It consumes an untyped
//! [`DataValue`] and returns the value in its typed, storable shape (or an
//! error); downstream code never re-checks what coercion already proved.
//!
//! [`StoredRelationMetadata`] is a stored relation's whole schema — its key
//! columns and its dependent columns, each a named, typed [`ColumnDef`].

use std::cmp::Reverse;
use std::fmt::{Display, Formatter};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use itertools::Itertools;
use miette::{Diagnostic, Result, bail, ensure};
use serde_json::json;
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::expr::Expr;
use crate::data::functions::to_json;
use crate::data::value::{DataValue, Num, NumRepr, RegexSource, Validity, ValidityTs, Vector};

use crate::data::json::{json_from_serde, serde_from_json};

/// Schema vocabulary: a vector column's declared element width. Stored
/// vector VALUES are always f64 canonical (format v1); `F32` columns
/// constrain declared width and let engines pack narrower internally —
/// every f32 is exactly representable as f64, so identity round-trips.
#[derive(Debug, Copy, Clone, PartialEq, Eq, serde_derive::Serialize, serde_derive::Deserialize)]
pub(crate) enum VecElementType {
    F32,
    F64,
}

#[derive(Debug, Clone, Eq, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
pub struct NullableColType {
    pub coltype: ColType,
    pub nullable: bool,
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
        if self.nullable {
            f.write_str("?")?;
        }
        Ok(())
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
        len: Option<usize>,
    },
    Vec {
        eltype: VecElementType,
        len: usize,
    },
    Tuple(Vec<NullableColType>),
    Validity,
    Json,
}

#[derive(Debug, Clone, Eq, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
pub(crate) struct ColumnDef {
    pub(crate) name: SmartString<LazyCompact>,
    pub(crate) typing: NullableColType,
    pub(crate) default_gen: Option<Expr>,
}

#[derive(Debug, Clone, Eq, PartialEq, serde_derive::Deserialize, serde_derive::Serialize)]
pub(crate) struct StoredRelationMetadata {
    pub(crate) keys: Vec<ColumnDef>,
    pub(crate) non_keys: Vec<ColumnDef>,
}

impl StoredRelationMetadata {
    pub(crate) fn satisfied_by_required_col(&self, col: &ColumnDef) -> Result<()> {
        for target in self.keys.iter().chain(self.non_keys.iter()) {
            if target.name == col.name {
                return Ok(());
            }
        }
        if col.default_gen.is_none() {
            #[derive(Debug, Error, Diagnostic)]
            #[error("required column {0} not provided by input")]
            #[diagnostic(code(eval::required_col_not_provided))]
            struct ColumnNotProvided(String);

            bail!(ColumnNotProvided(col.name.to_string()))
        }
        Ok(())
    }
    pub(crate) fn compatible_with_col(&self, col: &ColumnDef) -> Result<()> {
        for target in self.keys.iter().chain(self.non_keys.iter()) {
            if target.name == col.name {
                #[derive(Debug, Error, Diagnostic)]
                #[error("requested column {0} has typing {1}, but the requested typing is {2}")]
                #[diagnostic(code(eval::col_type_mismatch))]
                struct IncompatibleTyping(String, NullableColType, NullableColType);
                if (!col.typing.nullable || col.typing.coltype != ColType::Any)
                    && target.typing != col.typing
                {
                    bail!(IncompatibleTyping(
                        col.name.to_string(),
                        target.typing.clone(),
                        col.typing.clone()
                    ))
                }

                return Ok(());
            }
        }

        #[derive(Debug, Error, Diagnostic)]
        #[error("required column {0} not found")]
        #[diagnostic(code(eval::required_col_not_found))]
        struct ColumnNotFound(String);

        bail!(ColumnNotFound(col.name.to_string()))
    }
}

impl NullableColType {
    /// Parse an untyped value into this column's typed shape, or fail.
    ///
    /// This is the contract applied at the data boundary: the returned
    /// value is proven to satisfy the column typing, so nothing downstream
    /// re-checks it.
    // The mutation tier (runtime/db.rs) is coerce's lib consumer and lands
    // later; the law tests keep it live under cfg(test). `allow`, not
    // `expect`: an item-level dead_code expectation marks the item a live
    // root, so it can never be fulfilled.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn coerce(&self, data: DataValue, cur_vld: ValidityTs) -> Result<DataValue> {
        if matches!(data, DataValue::Null) {
            return if self.nullable {
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
                DataValue::Set(s) => DataValue::List(s.into_iter().collect_vec()),
                d => d,
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
                _ => bail!(make_err()),
            },
            ColType::Uuid => match data {
                u @ DataValue::Uuid(_) => u,
                _ => bail!(make_err()),
            },
            ColType::List { eltype, len } => {
                if let DataValue::List(l) = data {
                    if let Some(expected) = len {
                        ensure!(*expected == l.len(), BadListLength(self.clone(), l.len()))
                    }
                    DataValue::List(
                        l.into_iter()
                            .map(|el| eltype.coerce(el, cur_vld))
                            .try_collect()?,
                    )
                } else {
                    bail!(make_err())
                }
            }
            ColType::Vec { eltype, len } => match &data {
                DataValue::List(l) => {
                    if l.len() != *len {
                        bail!(BadListLength(self.clone(), l.len()))
                    }
                    let collected: Vec<f64> = l
                        .iter()
                        .map(|el| {
                            el.get_float()
                                .map(|f| match eltype {
                                    VecElementType::F32 => f as f32 as f64,
                                    VecElementType::F64 => f,
                                })
                                .ok_or_else(make_err)
                        })
                        .try_collect()?;
                    DataValue::Vector(Vector::new(collected))
                }
                DataValue::Vector(arr) => {
                    if *len != arr.len() {
                        bail!(make_err())
                    }
                    // The declared element type is a precision constraint on
                    // the f64-canonical components, not a storage variant.
                    if matches!(eltype, VecElementType::F32)
                        && arr
                            .as_slice()
                            .iter()
                            .any(|&f| (f as f32 as f64) != f && !f.is_nan())
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
                            if bytes.len() / size_of::<f32>() != *len
                                || !bytes.len().is_multiple_of(size_of::<f32>())
                            {
                                bail!(make_err())
                            }
                            bytes
                                .chunks_exact(4)
                                // In bounds: `chunks_exact(4)` yields 4-byte chunks.
                                .map(|c| {
                                    f32::from_le_bytes(c.try_into().expect("chunk of 4")) as f64
                                })
                                .collect()
                        }
                        VecElementType::F64 => {
                            if bytes.len() / size_of::<f64>() != *len
                                || !bytes.len().is_multiple_of(size_of::<f64>())
                            {
                                bail!(make_err())
                            }
                            bytes
                                .chunks_exact(8)
                                // In bounds: `chunks_exact(8)` yields 8-byte chunks.
                                .map(|c| f64::from_le_bytes(c.try_into().expect("chunk of 8")))
                                .collect()
                        }
                    };
                    DataValue::Vector(Vector::new(collected))
                }
                _ => bail!(make_err()),
            },
            ColType::Tuple(typ) => {
                if let DataValue::List(l) = data {
                    ensure!(typ.len() == l.len(), BadListLength(self.clone(), l.len()));
                    DataValue::List(
                        l.into_iter()
                            .zip(typ.iter())
                            .map(|(el, t)| t.coerce(el, cur_vld))
                            .try_collect()?,
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
                        "ASSERT" => DataValue::Validity(Validity {
                            timestamp: cur_vld,
                            is_assert: Reverse(true),
                        }),
                        "RETRACT" => DataValue::Validity(Validity {
                            timestamp: cur_vld,
                            is_assert: Reverse(false),
                        }),
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
                            let microseconds = crate::data::functions::timestamp_to_micros(ts);

                            if microseconds == i64::MAX || microseconds == i64::MIN {
                                bail!(InvalidValidity(DataValue::Str(s.into())))
                            }

                            DataValue::Validity(Validity {
                                timestamp: ValidityTs::from_raw(microseconds),
                                is_assert: Reverse(is_assert),
                            })
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
                                return Ok(DataValue::Validity(Validity {
                                    timestamp: ValidityTs::from_raw(ts),
                                    is_assert: Reverse(is_assert),
                                }));
                            }
                        }
                        bail!(InvalidValidity(DataValue::List(l)))
                    }
                    v => bail!(InvalidValidity(v)),
                }
            }
            ColType::Json => {
                let serde_val = match data {
                    DataValue::Null => {
                        json!(null)
                    }
                    DataValue::Bool(b) => {
                        json!(b)
                    }
                    DataValue::Num(n) => match n.repr() {
                        NumRepr::Int(i) => {
                            json!(i)
                        }
                        NumRepr::Float(f) => {
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
                        json!(r.pattern())
                    }
                    DataValue::List(l) => {
                        let mut arr = Vec::with_capacity(l.len());
                        for el in l {
                            arr.push(to_json(&self.coerce(el, cur_vld)?));
                        }
                        arr.into()
                    }
                    DataValue::Set(l) => {
                        let mut arr = Vec::with_capacity(l.len());
                        for el in l {
                            arr.push(to_json(&self.coerce(el, cur_vld)?));
                        }
                        arr.into()
                    }
                    DataValue::Vector(v) => {
                        let mut arr = Vec::with_capacity(v.len());
                        for el in v.as_slice() {
                            arr.push(json!(el));
                        }
                        arr.into()
                    }
                    DataValue::Json(j) => serde_from_json(&j),
                    DataValue::Validity(vld) => {
                        json!([vld.timestamp.raw(), vld.is_assert.0])
                    }
                    DataValue::Interval(iv) => {
                        json!([iv.start(), iv.end()])
                    }
                };
                DataValue::Json(json_from_serde(&serde_val))
            }
        })
    }
}
