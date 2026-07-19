//! Json ↔ DataValue conversion vocabulary for schema coerce + collection kernels.
use serde_json::{Value, json};
use serde_json::Value as JsonValue;

use crate::envelope::{json_from_serde, serde_from_json};
use crate::value::kind::interval::{Hi, Lo};
use crate::value::{DataValue, Interval, NumRepr};

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
        DataValue::Bytes(b) => {
            json!(b)
        }
        DataValue::Uuid(u) => {
            json!(u.as_uuid().as_bytes())
        }
        DataValue::Regex(r) => {
            json!(r.pattern())
        }
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
            arr.into()
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
        Value::Object(obj) => DataValue::Json(json_from_serde(&json!(obj))),
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
