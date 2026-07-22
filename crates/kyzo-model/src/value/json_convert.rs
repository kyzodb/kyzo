/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Json ↔ DataValue conversion vocabulary for schema coerce + collection kernels.
//!
//! # Kind-preserving tags
//!
//! JSON has no native Bytes / Uuid / Vector / Regex. Plain Cozo-style
//! renders (byte arrays, UUID byte arrays, float arrays, pattern-only
//! strings) collapse distinct values — opposite-flag regexes serialize
//! identically, and `json` → `json_to_scalar` cannot recover kind.
//!
//! Non-native kinds therefore wire as a closed tagged object under the
//! reserved discriminant [`KYZO_KIND`]. `json2val` reconstructs those
//! tags; unknown / malformed tags stay `DataValue::Json` (total, never
//! panics). Ordinary JSON objects without the discriminant are unchanged.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value as JsonValue;
use serde_json::{Map, Value, json};

use crate::envelope::{json_from_serde, serde_from_json};
use crate::value::kind::interval::{Hi, Lo};
use crate::value::kind::regex::{RegexFlags, RegexSource};
use crate::value::{DataValue, Interval, NumRepr, Vector};

/// Reserved discriminant for kind-preserving JSON tags. Objects carrying
/// this key are interpreted by [`json2val`]; do not use it in ordinary
/// JSON payloads that must round-trip as [`DataValue::Json`].
pub const KYZO_KIND: &str = "$kyzo";

fn tagged(kind: &'static str, value: JsonValue) -> JsonValue {
    json!({ KYZO_KIND: kind, "v": value })
}

fn tagged_regex(pattern: &str, flags: u8) -> JsonValue {
    json!({ KYZO_KIND: "regex", "v": pattern, "f": flags })
}

pub fn to_json(d: &DataValue) -> JsonValue {
    match d {
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
        DataValue::Bytes(b) => tagged("bytes", JsonValue::String(STANDARD.encode(b))),
        DataValue::Uuid(u) => tagged("uuid", json!(u.as_uuid().to_string())),
        DataValue::Regex(r) => tagged_regex(r.pattern(), r.flags().bits()),
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
        DataValue::Vector(v) => {
            let mut arr = Vec::with_capacity(v.len());
            for el in v.to_f64s() {
                arr.push(json!(el));
            }
            tagged("vec", arr.into())
        }
        DataValue::Json(j) => serde_from_json(j),
        DataValue::Validity(vld) => {
            json!([vld.ts_micros(), vld.is_assert()])
        }
        DataValue::Interval(iv) => interval_to_json(iv),
        DataValue::Geometry(g) => {
            json!([g.lat().get(), g.lon().get()])
        }
    }
}

fn lift_tagged(obj: &Map<String, Value>) -> Option<DataValue> {
    let kind = obj.get(KYZO_KIND)?.as_str()?;
    match kind {
        "bytes" => {
            let s = obj.get("v")?.as_str()?;
            let bytes = match STANDARD.decode(s) {
                Ok(b) => b,
                Err(_b64) => return None,
            };
            Some(DataValue::Bytes(bytes))
        }
        "uuid" => {
            let s = obj.get("v")?.as_str()?;
            let u = match uuid::Uuid::try_parse(s) {
                Ok(u) => u,
                Err(_parse) => return None,
            };
            Some(DataValue::uuid(u))
        }
        "vec" => {
            let arr = obj.get("v")?.as_array()?;
            let mut floats = Vec::with_capacity(arr.len());
            for el in arr {
                floats.push(el.as_f64()?);
            }
            Some(DataValue::Vector(Vector::try_new(floats)?))
        }
        "regex" => {
            let pattern = obj.get("v")?.as_str()?.to_string();
            let flags_u = obj.get("f")?.as_u64()?;
            let flags_u8 = match u8::try_from(flags_u) {
                Ok(v) => v,
                Err(_wide) => return None,
            };
            let flags = RegexFlags::from_bits(flags_u8)?;
            let src = match RegexSource::validated(flags, pattern) {
                Ok(s) => s,
                Err(_bad) => return None,
            };
            Some(DataValue::Regex(src))
        }
        _other => None,
    }
}

pub fn json2val(res: Value) -> DataValue {
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
        Value::String(s) => DataValue::Str(s),
        Value::Array(arr) => DataValue::Json(json_from_serde(&json!(arr))),
        Value::Object(obj) => {
            if let Some(v) = lift_tagged(&obj) {
                v
            } else {
                DataValue::Json(json_from_serde(&Value::Object(obj)))
            }
        }
    }
}

/// The interval render shared with the bridge: `null` for empty,
/// `[lo|null, hi|null]` otherwise.
pub fn interval_to_json(iv: &Interval) -> JsonValue {
    match iv.ends() {
        None => JsonValue::Null,
        Some((lo, hi)) => {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{RegexFlags, RegexSource, UuidWrapper, Vector};

    fn rt(v: DataValue) -> DataValue {
        json2val(to_json(&v))
    }

    #[test]
    fn regex_flags_participate_in_json_identity() {
        let ci = DataValue::Regex(
            RegexSource::validated(RegexFlags::CASE_INSENSITIVE, "foo".into()).unwrap(),
        );
        let cs = DataValue::Regex(RegexSource::validated(RegexFlags::NONE, "foo".into()).unwrap());
        assert_ne!(
            to_json(&ci),
            to_json(&cs),
            "opposite-semantics regexes must not serialize identically"
        );
        assert_eq!(rt(ci.clone()), ci);
        assert_eq!(rt(cs.clone()), cs);
        // Flagged source must not collapse to the pattern-only string.
        assert!(matches!(rt(ci), DataValue::Regex(_)));
    }

    #[test]
    fn bytes_uuid_vector_round_trip_preserve_kind() {
        let bytes = DataValue::Bytes(vec![0, 1, 255, 0x80]);
        assert_eq!(rt(bytes.clone()), bytes);
        assert!(matches!(rt(bytes), DataValue::Bytes(_)));

        let u = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let uuid = DataValue::Uuid(UuidWrapper::new(u));
        assert_eq!(rt(uuid.clone()), uuid);
        assert!(matches!(rt(uuid), DataValue::Uuid(_)));

        let vec = DataValue::Vector(Vector::try_new(vec![1.0, -2.5, 0.0]).unwrap());
        assert_eq!(rt(vec.clone()), vec);
        assert!(matches!(rt(vec), DataValue::Vector(_)));
    }

    #[test]
    fn ordinary_json_object_without_discriminant_stays_json() {
        let v = json!({"a": 1, "b": [true, null]});
        let back = json2val(v.clone());
        assert!(matches!(back, DataValue::Json(_)));
        assert_eq!(to_json(&back), v);
    }

    #[test]
    fn malformed_kyzo_tag_falls_through_to_json() {
        let bad = json!({ KYZO_KIND: "bytes", "v": "@@@not-base64@@@" });
        assert!(matches!(json2val(bad), DataValue::Json(_)));
        let unknown = json!({ KYZO_KIND: "nope", "v": 1 });
        assert!(matches!(json2val(unknown), DataValue::Json(_)));
    }
}
