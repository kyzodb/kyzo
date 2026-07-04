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
//! `Regex`, `Set`, `Vec`, `Validity`, and `Bot` are store-internal
//! representations with no plain-JSON input convention, the same asymmetry
//! the CozoDB original had (a base64 string or a UUID's string form come
//! out; only `Null`/`Bool`/`Num`/`Str`/`List`/`Json` go back in as those
//! variants). [`DataValue::from`] is total either direction: JSON has no
//! shape it can't become *some* `DataValue`, and every `DataValue` variant
//! has a defined JSON rendering.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use miette::{
    GraphicalReportHandler, GraphicalTheme, JSONReportHandler, Report, Result, ThemeCharacters,
    ThemeStyles,
};
use serde_json::{Value as JsonValue, json};
use std::sync::LazyLock;

use crate::data::value::{DataValue, JsonData, Num, Vector};
use crate::fixed_rule::NamedRows;

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
            JsonValue::Object(d) => DataValue::Json(JsonData(JsonValue::Object(d))),
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
            JsonValue::Object(d) => DataValue::Json(JsonData(JsonValue::Object(d.clone()))),
        }
    }
}

impl From<&DataValue> for JsonValue {
    fn from(v: &DataValue) -> Self {
        match v {
            DataValue::Null => JsonValue::Null,
            DataValue::Bool(b) => JsonValue::Bool(*b),
            DataValue::Num(Num::Int(i)) => JsonValue::Number((*i).into()),
            DataValue::Num(Num::Float(f)) => {
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
            DataValue::Str(s) => JsonValue::String(s.to_string()),
            DataValue::Bytes(bytes) => JsonValue::String(STANDARD.encode(bytes)),
            DataValue::List(l) => JsonValue::Array(l.iter().map(JsonValue::from).collect()),
            DataValue::Set(s) => JsonValue::Array(s.iter().map(JsonValue::from).collect()),
            DataValue::Regex(r) => json!(r.0.as_str()),
            DataValue::Uuid(u) => json!(u.0.to_string()),
            DataValue::Vec(v) => match v {
                Vector::F32(a) => json!(a.iter().copied().collect::<Vec<f32>>()),
                Vector::F64(a) => json!(a.iter().copied().collect::<Vec<f64>>()),
            },
            DataValue::Validity(v) => json!([v.timestamp.0.0, v.is_assert.0]),
            DataValue::Json(j) => j.0.clone(),
            // Unreachable in a correct query result (see the port note
            // above); `Null` keeps this a total function rather than a
            // panic if it ever is reached.
            DataValue::Bot => JsonValue::Null,
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

/// Render a query/mutation error as the envelope every binding's failure
/// path returns: miette's own JSON report (`code`, `labels`, `severity`,
/// ...) plus `ok: false`, a flat `message`, and a `display` field carrying
/// the same fancy rendering a terminal would show (a labeled source span,
/// if the error carries one and `source` supplies the text it points into).
pub fn format_error_as_json(mut err: Report, source: Option<&str>) -> JsonValue {
    if err.source_code().is_none()
        && let Some(src) = source
    {
        err = err.with_source_code(format!("{src} "));
    }
    let mut text_err = String::new();
    let mut json_err = String::new();
    TEXT_ERR_HANDLER
        .render_report(&mut text_err, err.as_ref())
        .expect("render text error failed");
    JSON_ERR_HANDLER
        .render_report(&mut json_err, err.as_ref())
        .expect("render json error failed");
    let mut json: JsonValue =
        serde_json::from_str(&json_err).expect("parse rendered json error failed");
    let map = json
        .as_object_mut()
        .expect("miette's JSONReportHandler always renders a JSON object");
    map.insert("ok".to_string(), json!(false));
    map.insert("message".to_string(), json!(err.to_string()));
    map.insert("display".to_string(), json!(text_err));
    json
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
    use crate::data::value::{UuidWrapper, Validity, ValidityTs};
    use std::cmp::Reverse;

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
        let dv = DataValue::Uuid(UuidWrapper(uuid::Uuid::nil()));
        assert_eq!(JsonValue::from(&dv), json!(uuid::Uuid::nil().to_string()));

        let v = Validity {
            timestamp: ValidityTs(Reverse(5)),
            is_assert: Reverse(true),
        };
        assert_eq!(JsonValue::from(&DataValue::Validity(v)), json!([5, true]));
    }

    #[test]
    fn bot_renders_as_null_never_panics() {
        assert_eq!(JsonValue::from(&DataValue::Bot), JsonValue::Null);
    }

    #[test]
    fn named_rows_json_round_trips() {
        let nr = NamedRows::new(
            vec!["a".to_string(), "b".to_string()],
            vec![
                vec![DataValue::from(1_i64), DataValue::from("x")],
                vec![DataValue::from(2_i64), DataValue::from("y")],
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
