/*
 * Copyright 2022, The Cozo Project Authors. / Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! JSON wire conversions for [`DataValue`]: serde bridge, plane Json, and
//! ordered lifts. NamedRows envelopes and diagnostic envelopes live
//! in `kyzo` (`data::json`) because they need session/fixed-rule types.
//!
//! # Float-edge round-trip law
//!
//! Standard JSON cannot carry NaN/±Inf as a number without a nonstandard
//! dialect. Silently remapping `DataValue::Num` → `Null` / `Str` on encode
//! (or inventing `Num` from those on decode) changes value kind — Spec fraud.
//! Finite `Num` encodes as JSON number and round-trips as `Num`. Non-finite
//! `Num` encode refuses with [`NonFiniteJsonNumber`]. Decode never lifts
//! `Null` or `Str` into `Num`.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
pub use serde_json::Value as JsonValue;
use serde_json::json;

use crate::value::kind::interval::{Hi, Lo};
use crate::value::{DataValue, Json, JsonNum, JsonObj, NonFiniteJsonNumber, Num};

/// The serde bridge value: engine-side JSON carried as `serde_json`
/// until it crosses into the value plane's identity-lawful [`Json`].
/// Lives HERE, not in the plane — the plane never depends on serde.
#[derive(Clone, PartialEq, Debug)]
pub struct JsonData(JsonValue);

impl JsonData {
    /// The only mint: wrap a serde JSON value for the host boundary.
    pub fn new(v: JsonValue) -> JsonData {
        JsonData(v)
    }

    pub fn value(&self) -> &JsonValue {
        &self.0
    }
}

/// `DataValue`'s wire form for engine metadata (stored programs,
/// catalogs): the CANONICAL BYTES, nothing else — serde here is a thin
/// skin over the one codec authority, so there is no second
/// serialization truth to drift. Deserialize runs the full validating
/// decode: corrupted metadata refuses, never half-loads.
impl serde::Serialize for DataValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(crate::value::encode_owned(self).as_bytes())
    }
}

impl<'de> serde::Deserialize<'de> for DataValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<DataValue, D::Error> {
        struct CanonicalVisitor;
        impl<'de> serde::de::Visitor<'de> for CanonicalVisitor {
            type Value = DataValue;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("canonical value bytes")
            }

            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<DataValue, E> {
                crate::value::decode(v).map_err(E::custom)
            }

            fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<DataValue, E> {
                self.visit_bytes(&v)
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<DataValue, A::Error> {
                let mut buf = Vec::new();
                while let Some(b) = seq.next_element::<u8>()? {
                    buf.push(b);
                }
                self.visit_bytes(&buf)
            }
        }
        deserializer.deserialize_bytes(CanonicalVisitor)
    }
}

/// Typed refusals compose with the engine's diagnostics. Stable code
/// `value::decode` lets product boundaries recognize a codec refusal
/// without `downcast_ref` (story #302 T6).
impl miette::Diagnostic for crate::value::DecodeError {
    fn code<'a>(&'a self) -> Option<Box<dyn std::fmt::Display + 'a>> {
        Some(Box::new("value::decode"))
    }
}

/// `RelationId`'s wire form: the raw u64 (catalog metadata).
impl serde::Serialize for crate::value::RelationId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.raw())
    }
}

impl<'de> serde::Deserialize<'de> for crate::value::RelationId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = u64::deserialize(deserializer)?;
        crate::value::RelationId::new(raw).ok_or_else(|| {
            serde::de::Error::custom("relation id at or beyond the allocation ceiling")
        })
    }
}

/// serde → plane: total. serde_json numbers are finite by construction
/// (standard parsing admits no NaN/inf), and serde maps carry unique
/// keys, so the plane's refusals cannot fire on this path.
pub fn json_from_serde(v: &JsonValue) -> Json {
    match v {
        JsonValue::Null => Json::Null,
        JsonValue::Bool(b) => Json::Bool(*b),
        JsonValue::Number(n) => {
            let num = match n.as_i64() {
                Some(i) => Num::int(i),
                None => Num::float(match n.as_f64() {
                Some(f) => f,
                None => 0.0,
            }),
            };
            match JsonNum::new(num) {
                Ok(n) => Json::Num(n),
                Err(_non_finite) => std::process::abort(),
            }
        }
        JsonValue::String(s) => Json::Str(s.clone()),
        JsonValue::Array(a) => Json::Arr(a.iter().map(json_from_serde).collect()),
        JsonValue::Object(o) => {
            let entries: Vec<_> = o
                .iter()
                .map(|(k, x)| (k.clone(), json_from_serde(x)))
                .collect();
            match JsonObj::new(entries) {
                Ok(obj) => Json::Obj(obj),
                Err(_dup) => std::process::abort(),
            }
        }
    }
}

/// plane → serde: total (plane numbers are finite by the JsonNum law).
pub fn serde_from_json(j: &Json) -> JsonValue {
    match j {
        Json::Null => JsonValue::Null,
        Json::Bool(b) => JsonValue::Bool(*b),
        Json::Num(n) => match (n.num().as_int(), n.num().as_float()) {
            (Some(i), _) => JsonValue::Number(i.into()),
            (_, Some(f)) => json!(f),
            _other => unreachable!("Num is int or float"),
        },
        Json::Str(s) => JsonValue::String(s.clone()),
        Json::Arr(a) => JsonValue::Array(a.iter().map(serde_from_json).collect()),
        Json::Obj(o) => JsonValue::Object(
            o.entries()
                .iter()
                .map(|(k, x)| (k.clone(), serde_from_json(x)))
                .collect(),
        ),
    }
}

impl From<JsonValue> for DataValue {
    fn from(v: JsonValue) -> Self {
        match v {
            JsonValue::Null => DataValue::Null,
            JsonValue::Bool(b) => DataValue::Bool(b),
            JsonValue::Number(n) => match n.as_i64() {
                Some(i) => DataValue::from(i),
                None => match n.as_f64() {
                    // serde_json::Number is finite by construction; as_f64
                    // never yields NaN/Inf, so this arm cannot invent a
                    // non-finite Num from JSON wire.
                    Some(f) => DataValue::from(f),
                    None => DataValue::from(n.to_string()),
                },
            },
            JsonValue::String(s) => DataValue::from(s),
            JsonValue::Array(arr) => {
                DataValue::List(arr.into_iter().map(DataValue::from).collect())
            }
            JsonValue::Object(d) => DataValue::Json(json_from_serde(&JsonValue::Object(d))),
        }
    }
}

impl From<&JsonValue> for DataValue {
    fn from(v: &JsonValue) -> Self {
        match v {
            JsonValue::Null => DataValue::Null,
            JsonValue::Bool(b) => DataValue::Bool(*b),
            JsonValue::Number(n) => match n.as_i64() {
                Some(i) => DataValue::from(i),
                None => match n.as_f64() {
                    Some(f) => DataValue::from(f),
                    None => DataValue::from(n.to_string()),
                },
            },
            JsonValue::String(s) => DataValue::from(s.as_str()),
            JsonValue::Array(arr) => DataValue::List(arr.iter().map(DataValue::from).collect()),
            JsonValue::Object(d) => DataValue::Json(json_from_serde(&JsonValue::Object(d.clone()))),
        }
    }
}

fn json_number_from_finite_f64(f: f64) -> Result<JsonValue, NonFiniteJsonNumber> {
    serde_json::Number::from_f64(f)
        .map(JsonValue::Number)
        .ok_or(NonFiniteJsonNumber)
}

/// Named door for `DataValue` → JSON wire. Same law as [`TryFrom<&DataValue>`].
pub fn datavalue_to_json(v: &DataValue) -> Result<JsonValue, NonFiniteJsonNumber> {
    JsonValue::try_from(v)
}

impl TryFrom<&DataValue> for JsonValue {
    type Error = NonFiniteJsonNumber;

    fn try_from(v: &DataValue) -> Result<Self, Self::Error> {
        match v {
            DataValue::Null => Ok(JsonValue::Null),
            DataValue::Bool(b) => Ok(JsonValue::Bool(*b)),
            DataValue::Num(n) => match (n.as_int(), n.as_float()) {
                (Some(i), _) => Ok(JsonValue::Number(i.into())),
                (_, Some(f)) => json_number_from_finite_f64(f),
                _other => unreachable!("Num is int or float"),
            },
            DataValue::Str(s) => Ok(JsonValue::String(s.to_string())),
            DataValue::Bytes(bytes) => Ok(JsonValue::String(STANDARD.encode(bytes))),
            DataValue::List(l) => Ok(JsonValue::Array(
                l.iter()
                    .map(JsonValue::try_from)
                    .collect::<Result<Vec<_>, _>>()?,
            )),
            DataValue::Set(s) => Ok(JsonValue::Array(
                s.iter()
                    .map(JsonValue::try_from)
                    .collect::<Result<Vec<_>, _>>()?,
            )),
            DataValue::Regex(r) => Ok(json!(r.pattern())),
            DataValue::Uuid(u) => Ok(json!(u.as_uuid().to_string())),
            DataValue::Vector(v) => Ok(json!(v.to_f64s())),
            DataValue::Validity(v) => Ok(json!([v.ts_micros(), v.is_assert()])),
            DataValue::Interval(iv) => match iv.ends() {
                None => Ok(JsonValue::Null),
                Some((lo, hi)) => {
                    let l = match lo {
                        Lo::NegUnbounded => JsonValue::Null,
                        Lo::At(t) => JsonValue::Number(t.into()),
                    };
                    let h = match hi {
                        Hi::PosUnbounded => JsonValue::Null,
                        Hi::At(t) => JsonValue::Number(t.into()),
                    };
                    Ok(JsonValue::Array(vec![l, h]))
                }
            },
            DataValue::Geometry(g) => Ok(json!([g.lat().get(), g.lon().get()])),
            DataValue::Json(j) => Ok(serde_from_json(j)),
        }
    }
}

impl TryFrom<DataValue> for JsonValue {
    type Error = NonFiniteJsonNumber;

    fn try_from(v: DataValue) -> Result<Self, Self::Error> {
        JsonValue::try_from(&v)
    }
}

/// Named door for JSON → `DataValue`: same total mapping as [`From<&JsonValue>`].
/// Never invents `Num` from `Null` or `Str`.
pub fn json_to_datavalue(v: &JsonValue) -> DataValue {
    DataValue::from(v)
}

#[cfg(test)]
mod tests {
    use miette::{IntoDiagnostic, Result, miette};

    use super::*;
    use crate::value::Num;

    #[test]
    fn nan_num_encode_refuses() {
        let v = DataValue::Num(Num::float(f64::NAN));
        assert_eq!(datavalue_to_json(&v), Err(NonFiniteJsonNumber));
    }

    #[test]
    fn pos_inf_num_encode_refuses() {
        let v = DataValue::Num(Num::float(f64::INFINITY));
        assert_eq!(datavalue_to_json(&v), Err(NonFiniteJsonNumber));
    }

    #[test]
    fn neg_inf_num_encode_refuses() {
        let v = DataValue::Num(Num::float(f64::NEG_INFINITY));
        assert_eq!(datavalue_to_json(&v), Err(NonFiniteJsonNumber));
    }

    #[test]
    fn finite_num_round_trip_preserves_num_kind() -> Result<()> {
        for f in [0.0_f64, -0.0, 1.5, -2.25, 1e308, f64::MIN_POSITIVE] {
            let v = DataValue::Num(Num::float(f));
            let wire = datavalue_to_json(&v).into_diagnostic()?;
            assert!(
                matches!(wire, JsonValue::Number(_)),
                "finite Num must wire as JSON number, got {wire:?}"
            );
            let back = json_to_datavalue(&wire);
            assert!(
                matches!(back, DataValue::Num(_)),
                "round-trip must preserve Num kind, got {back:?}"
            );
            assert_eq!(back, v);
        }
        let i = DataValue::Num(Num::int(42));
        let wire = datavalue_to_json(&i).into_diagnostic()?;
        let back = json_to_datavalue(&wire);
        assert_eq!(back, i);
        assert!(matches!(back, DataValue::Num(_)));
        Ok(())
    }

    #[test]
    fn null_decode_stays_null_not_num() {
        let back = json_to_datavalue(&JsonValue::Null);
        assert_eq!(back, DataValue::Null);
        assert!(!matches!(back, DataValue::Num(_)));
    }

    #[test]
    fn infinity_string_decode_stays_str_not_num() -> Result<()> {
        // Former Cozo remap tokens must not invent Num on the way back.
        for s in ["INFINITY", "NEGATIVE_INFINITY", "NaN", "nan", "Inf"] {
            let back = json_to_datavalue(&JsonValue::String(s.to_string()));
            assert!(
                matches!(back, DataValue::Str(_)),
                "{s:?} must decode as Str, got {back:?}"
            );
        }
        Ok(())
    }
}
