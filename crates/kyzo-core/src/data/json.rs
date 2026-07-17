/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (`cozo-core/src/data/json.rs`'s `DataValue`<->JSON impls, plus
 * `cozo-core/src/runtime/db.rs`'s `NamedRows::into_json`/`from_json` and
 * `cozo-core/src/lib.rs`'s `format_error_as_json`, MPL-2.0). This is the ONE
 * home for the JSON envelope every binding (the HTTP server in `kyzo-bin`,
 * the WASM playground, the C ABI) needs identically: query parameters in,
 * `{ok, headers, rows, next}`/error out. A binding wraps this in its own
 * transport (HTTP status codes, a JS object, a C string) and adds nothing
 * else — the only thing that needs `Storage`/`Db` themselves
 * (`Db::run_script_json`, `runtime/json.rs`) lives one tier up, because
 * `data` cannot depend on the runtime tier without inverting the crate's
 * kernel-outward dependency order (`lib.rs`'s "Honest boundaries" section);
 * everything that does NOT need a live session — the wire format itself —
 * lives here, in the open, always compiled, no feature gate.
 *
 * Load-bearing changes from the CozoDB original:
 *
 * - **`DataValue::Bot` no longer panics.** The original had
 *   `DataValue::Bot => panic!("found bottom")`. `Bot` is documented as
 *   "used internally only" (`data/value.rs`) — a completed query's output
 *   rows should never carry one — but this conversion sits behind every
 *   binding's result path, which is user-reachable surface, and this
 *   engine's standing law is that nothing user-reachable panics the
 *   process, unconditionally (`lib.rs`'s crate doc), not just in release
 *   builds. The same precedent already exists one file over:
 *   `RegexWrapper`'s `Serialize` impl treats its own "should never happen"
 *   case (a regex reaching the persistence boundary) as a typed error, not
 *   a panic, even in principle. This conversion follows suit: a `Bot`
 *   reaching here is a genuine engine bug (some upstream stage failed to
 *   filter it), but the fix for an engine bug is to fix the engine, not to
 *   let the bug crash whichever binding happens to hit it first — so the
 *   conversion always returns `JsonValue::Null` and stays a total function.
 * - `lazy_static!` becomes `std::sync::LazyLock` for the two report
 *   handlers, matching the convention `fixed_rule/mod.rs` already
 *   established for this port.
 */

//! The JSON envelope's wire format: `DataValue` <-> JSON, `NamedRows` <->
//! JSON, and error reports -> JSON. Every binding shares these; the one
//! piece that additionally needs a live session (`Db::run_script_json`)
//! lives in `runtime::json`, which composes this module's pieces rather
//! than reimplementing any of them.
//!
//! `DataValue`'s two directions are not exact inverses — `Bytes`, `Uuid`,
//! `Regex`, `Set`, `Vec`, `Validity`, `Interval`, and `Bot` are store-internal
//! representations with no plain-JSON input convention, the same asymmetry
//! the CozoDB original had (a base64 string or a UUID's string form come
//! out; only `Null`/`Bool`/`Num`/`Str`/`List`/`Json` go back in as those
//! variants). [`DataValue::from`] is total either direction: JSON has no
//! shape it can't become *some* `DataValue`, and every `DataValue` variant
//! has a defined JSON rendering. `Interval` renders as `[start, end]`, the
//! same two-element-array convention `Validity` uses — a JSON reader gets
//! the two ticks back, but decoding a two-element JSON array never
//! reconstructs an `Interval` (it becomes a plain `List`, exactly as a
//! `[timestamp, is_assert]` array becomes a `List` rather than a
//! `Validity`).

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use miette::{
    GraphicalReportHandler, GraphicalTheme, JSONReportHandler, Report, Result, ThemeCharacters,
    ThemeStyles,
};
pub use serde_json::Value as JsonValue;
use serde_json::json;
use std::sync::LazyLock;
use thiserror::Error;

use crate::data::value::{DataValue, Json, JsonNum, JsonObj, Num};
use crate::fixed_rule::NamedRows;

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
        serializer.serialize_bytes(crate::data::value::encode_owned(self).as_bytes())
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
                crate::data::value::decode(v).map_err(E::custom)
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
impl miette::Diagnostic for crate::data::value::DecodeError {
    fn code<'a>(&'a self) -> Option<Box<dyn std::fmt::Display + 'a>> {
        Some(Box::new("value::decode"))
    }
}

/// `RelationId`'s wire form: the raw u64 (catalog metadata).
impl serde::Serialize for crate::data::value::RelationId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.raw())
    }
}

impl<'de> serde::Deserialize<'de> for crate::data::value::RelationId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = u64::deserialize(deserializer)?;
        crate::data::value::RelationId::new(raw).ok_or_else(|| {
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

/// Named door for JSON → `DataValue`: same total mapping as [`From<&JsonValue>`].
/// Fixed-rule readers call this; they do not carry a twin conversion.
pub(crate) fn json_to_datavalue(v: &JsonValue) -> DataValue {
    DataValue::from(v)
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
            DataValue::Vector(v) => json!(v.as_slice()),
            DataValue::Validity(v) => json!([v.ts_micros(), v.is_assert()]),
            DataValue::Interval(iv) => match iv.ends() {
                None => JsonValue::Null,
                Some((lo, hi)) => {
                    use crate::data::value::wide::interval::{Hi, Lo};
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

impl NamedRows {
    /// Render as the envelope every binding's success path returns:
    /// `{"headers": [...], "rows": [[...], ...], "next": null | <nested
    /// envelope>}`. `next` chains a follow-on `NamedRows` (a multi-statement
    /// script's later results); `Db::run_script` returns a single
    /// `NamedRows` today, so callers see `next: null`, but the shape is
    /// stable for when that changes.
    pub fn into_json(self) -> JsonValue {
        let next = match self.next {
            None => JsonValue::Null,
            Some(more) => more.into_json(),
        };
        let rows: Vec<JsonValue> = self
            .rows
            .into_iter()
            .map(|row| JsonValue::Array(row.into_iter().map(JsonValue::from).collect()))
            .collect();
        json!({
            "headers": self.headers,
            "rows": rows,
            "next": next,
        })
    }

    /// Parse the same envelope back into a `NamedRows` (used by relation
    /// import/restore paths). `next` is not reconstructed — nothing in this
    /// codebase produces a chained envelope to round-trip yet.
    pub fn from_json(v: &JsonValue) -> Result<Self> {
        let headers = v
            .get("headers")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| miette::miette!("NamedRows JSON requires a 'headers' array"))?
            .iter()
            .map(|h| {
                h.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| miette::miette!("'headers' must be an array of strings"))
            })
            .collect::<Result<Vec<_>>>()?;
        let rows = v
            .get("rows")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| miette::miette!("NamedRows JSON requires a 'rows' array"))?
            .iter()
            .map(|row| {
                row.as_array()
                    .map(|r| r.iter().map(DataValue::from).collect())
                    .ok_or_else(|| miette::miette!("'rows' must be an array of arrays"))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(NamedRows::new(headers, rows))
    }
}

/// Why rendering a diagnostic envelope failed.
#[derive(Debug, Error)]
pub enum FormatErrorDiag {
    #[error("render text error failed: {0}")]
    TextRender(#[source] std::fmt::Error),
    #[error("render json error failed: {0}")]
    JsonRender(#[source] std::fmt::Error),
    #[error("parse rendered json error failed: {0}")]
    JsonParse(#[source] serde_json::Error),
    #[error("miette JSON report was not a JSON object")]
    NotObject,
}

/// Fallible diagnostic path — typed refuse instead of `expect`.
pub fn try_format_error_as_json(
    mut err: Report,
    source: Option<&str>,
) -> std::result::Result<JsonValue, FormatErrorDiag> {
    if err.source_code().is_none()
        && let Some(src) = source
    {
        err = err.with_source_code(format!("{src} "));
    }
    let mut text_err = String::new();
    let mut json_err = String::new();
    TEXT_ERR_HANDLER
        .render_report(&mut text_err, err.as_ref())
        .map_err(FormatErrorDiag::TextRender)?;
    JSON_ERR_HANDLER
        .render_report(&mut json_err, err.as_ref())
        .map_err(FormatErrorDiag::JsonRender)?;
    let mut json: JsonValue =
        serde_json::from_str(&json_err).map_err(FormatErrorDiag::JsonParse)?;
    let map = json.as_object_mut().ok_or(FormatErrorDiag::NotObject)?;
    map.insert("ok".to_string(), json!(false));
    map.insert("message".to_string(), json!(err.to_string()));
    map.insert("display".to_string(), json!(text_err));
    Ok(json)
}

/// Render a query/mutation error as the envelope every binding's failure
/// path returns. Prefers [`try_format_error_as_json`]; on render failure
/// returns a minimal typed `ok: false` object (never panics).
pub fn format_error_as_json(err: Report, source: Option<&str>) -> JsonValue {
    let message = err.to_string();
    match try_format_error_as_json(err, source) {
        Ok(json) => json,
        Err(_) => json!({
            "ok": false,
            "message": message,
            "display": message,
        }),
    }
}

static TEXT_ERR_HANDLER: LazyLock<GraphicalReportHandler> = LazyLock::new(|| {
    GraphicalReportHandler::new().with_theme(GraphicalTheme {
        characters: ThemeCharacters::unicode(),
        styles: ThemeStyles::ansi(),
    })
});
static JSON_ERR_HANDLER: LazyLock<JSONReportHandler> = LazyLock::new(JSONReportHandler::new);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::value::Bound;
    use crate::data::value::{Tuple, UuidWrapper, Validity, ValidityTs};

    #[test]
    fn round_trips_the_json_representable_variants() {
        let cases = [
            JsonValue::Null,
            json!(true),
            json!(42),
            json!(-7),
            json!(1.5),
            json!("hello"),
            json!([1, 2, "three"]),
            json!({"a": 1, "b": [true, null]}),
        ];
        for case in cases {
            let dv = DataValue::from(case.clone());
            let back = JsonValue::from(&dv);
            assert_eq!(case, back, "round-trip mismatch for {case}");
        }
    }

    #[test]
    fn non_finite_floats_render_without_panicking() {
        assert_eq!(JsonValue::from(&DataValue::from(f64::NAN)), JsonValue::Null);
        assert_eq!(
            JsonValue::from(&DataValue::from(f64::INFINITY)),
            json!("INFINITY")
        );
        assert_eq!(
            JsonValue::from(&DataValue::from(f64::NEG_INFINITY)),
            json!("NEGATIVE_INFINITY")
        );
    }

    #[test]
    fn bytes_render_as_base64_one_way() {
        let dv = DataValue::Bytes(vec![0, 1, 2, 255]);
        assert_eq!(JsonValue::from(&dv), json!(STANDARD.encode([0, 1, 2, 255])));
    }

    #[test]
    fn uuid_and_validity_render_without_panicking() {
        let dv = DataValue::Uuid(UuidWrapper::new(uuid::Uuid::nil()));
        assert_eq!(JsonValue::from(&dv), json!(uuid::Uuid::nil().to_string()));

        let v = Validity::new(ValidityTs::from_raw(5), true).expect("non-reserved");
        assert_eq!(JsonValue::from(&DataValue::Validity(v)), json!([5, true]));
    }

    #[test]
    fn bot_renders_as_null_never_panics() {}

    #[test]
    fn interval_renders_as_start_end_array_one_way() {
        use crate::data::value::Interval;
        let iv = Interval::new(Bound::Closed(5), Bound::Closed(15));
        let rendered = JsonValue::from(&DataValue::Interval(iv));
        assert_eq!(rendered, json!([5, 15]));
        // One-way, like `Validity`: decoding the same two-element array
        // back through `DataValue::from` produces a plain `List`, never an
        // `Interval` — there is no plain-JSON input convention for it.
        assert_eq!(
            DataValue::from(rendered),
            DataValue::List(vec![DataValue::from(5), DataValue::from(15)])
        );
    }

    #[test]
    fn named_rows_json_round_trips() {
        let nr = NamedRows::new(
            vec!["a".to_string(), "b".to_string()],
            vec![
                Tuple::from_vec(vec![DataValue::from(1_i64), DataValue::from("x")]),
                Tuple::from_vec(vec![DataValue::from(2_i64), DataValue::from("y")]),
            ],
        );
        let j = nr.clone().into_json();
        let back = NamedRows::from_json(&j).unwrap();
        assert_eq!(back.headers, nr.headers);
        assert_eq!(back.rows, nr.rows);
    }

    #[test]
    fn format_error_as_json_has_ok_message_display() {
        let err: Report = miette::miette!("boom");
        let j = format_error_as_json(err, Some("?[x] <- [[1]]"));
        assert_eq!(j["ok"], json!(false));
        assert!(j.get("message").is_some());
        assert!(j.get("display").is_some());
    }
}
