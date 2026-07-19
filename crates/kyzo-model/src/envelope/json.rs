/*
 * Copyright 2022, The Cozo Project Authors. / Copyright 2026, The KyzoDB Authors.
 * MPL-2.0. DataValue <-> JSON wire conversions (no NamedRows — that stays in kyzo-core).
 */

//! JSON wire conversions for [`DataValue`]: serde bridge, plane Json, and
//! total From mappings. NamedRows envelopes and diagnostic envelopes live
//! in `kyzo` (`data::json`) because they need session/fixed-rule types.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::json;
pub use serde_json::Value as JsonValue;

use crate::value::kind::interval::{Hi, Lo};
use crate::value::{DataValue, DecodeError, Json, JsonNum, JsonObj, Num, RelationId, encode_owned, decode};

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
                None => Num::float(n.as_f64().unwrap_or(0.0)),
            };
            Json::Num(JsonNum::new(num).expect("serde numbers are finite"))
        }
        JsonValue::String(s) => Json::Str(s.clone()),
        JsonValue::Array(a) => Json::Arr(a.iter().map(json_from_serde).collect()),
        JsonValue::Object(o) => Json::Obj(
            JsonObj::new(
                o.iter()
                    .map(|(k, x)| (k.clone(), json_from_serde(x)))
                    .collect(),
            )
            .expect("serde maps have unique keys"),
        ),
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
            _ => unreachable!("Num is int or float"),
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


impl From<&DataValue> for JsonValue {
    fn from(v: &DataValue) -> Self {
        match v {
            DataValue::Null => JsonValue::Null,
            DataValue::Bool(b) => JsonValue::Bool(*b),
            DataValue::Num(n) => match (n.as_int(), n.as_float()) {
                (Some(i), _) => JsonValue::Number(i.into()),
                (_, Some(f)) => {
                    if f.is_finite() {
                        json!(f)
                    } else if f.is_nan() {
                        JsonValue::Null
                    } else if f.is_sign_negative() {
                        json!("NEGATIVE_INFINITY")
                    } else {
                        json!("INFINITY")
                    }
                }
                _ => unreachable!("Num is int or float"),
            },
            DataValue::Str(s) => JsonValue::String(s.to_string()),
            DataValue::Bytes(bytes) => JsonValue::String(STANDARD.encode(bytes)),
            DataValue::List(l) => JsonValue::Array(l.iter().map(JsonValue::from).collect()),
            DataValue::Set(s) => JsonValue::Array(s.iter().map(JsonValue::from).collect()),
            DataValue::Regex(r) => json!(r.pattern()),
            DataValue::Uuid(u) => json!(u.as_uuid().to_string()),
            DataValue::Vector(v) => json!(v.to_f64s()),
            DataValue::Validity(v) => json!([v.ts_micros(), v.is_assert()]),
            DataValue::Interval(iv) => match iv.ends() {
                None => JsonValue::Null,
                Some((lo, hi)) => {
                    use crate::value::kind::interval::{Hi, Lo};
                    let l = match lo {
                        Lo::NegUnbounded => JsonValue::Null,
                        Lo::At(t) => JsonValue::Number(t.into()),
                    };
                    let h = match hi {
                        Hi::PosUnbounded => JsonValue::Null,
                        Hi::At(t) => JsonValue::Number(t.into()),
                    };
                    JsonValue::Array(vec![l, h])
                }
            },
            DataValue::Json(j) => serde_from_json(j),
        }
    }
}

impl From<DataValue> for JsonValue {
    fn from(v: DataValue) -> Self {
        JsonValue::from(&v)
    }
}


/// Named door for JSON → `DataValue`: same total mapping as [`From<&JsonValue>`].
pub fn json_to_datavalue(v: &JsonValue) -> DataValue {
    DataValue::from(v)
}
