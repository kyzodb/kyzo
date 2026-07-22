/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (`cozo-core/src/lib.rs`'s `run_script_fold_err`, MPL-2.0).
 *
 * The wire format itself — `DataValue`<->JSON, `NamedRows`<->JSON,
 * error-report<->JSON — lives in `data::json` (it needs no live session, so
 * it belongs with the value kernel, not the runtime tier; see that module's
 * doc for the full account, including the `DataValue::Bot` panic fix).
 * This file adds exactly the one piece that DOES need a live session: the
 * entry point that runs a script and renders the result through that wire
 * format. It composes `data::json`'s functions; it does not reimplement
 * any JSON shaping.
 *
 * - [`Db::run_script_json`] takes `params` as a `&JsonValue` (must be a JSON
 *   object) rather than a pre-built `BTreeMap<String, DataValue>`, so a
 *   binding whose only natural representation of "the caller's parameters"
 *   is JSON (JS, a C string, an HTTP body) never has to hand-roll the
 *   `DataValue` conversion `data::json` already does. A non-object
 *   `params` is reported through the same envelope as any other query
 *   error, not a separate `Result` a caller could forget to check.
 * - Timing (`"took"`) is `#[cfg(not(target_arch = "wasm32"))]`:
 *   `std::time::Instant` panics on bare `wasm32-unknown-unknown` (the same
 *   platform gap `data/value.rs`'s `current_validity` already calls out),
 *   so on that target the field is simply absent rather than a compile
 *   error or a stubbed zero that would silently misreport.
 */

//! [`Db::run_script_json`]: the single "JSON params in, JSON envelope out"
//! entry point every binding shares, built on `data::json`'s wire format.

use serde_json::{Value as JsonValue, json};
use std::collections::BTreeMap;

use crate::data::json::format_error_as_json;
use crate::session::db::Engine;
use crate::store::Storage;
use kyzo_model::value::DataValue;

impl<S: Storage> Engine<S> {
    /// Run a script with JSON-encoded parameters, returning a JSON envelope
    /// that is always `Ok` at the Rust level — the ONE "JSON in, JSON out"
    /// entry point every binding (HTTP, WASM, C ABI) should call instead of
    /// reimplementing this shaping:
    ///
    /// - success: `{"ok": true, "took": <seconds, omitted on wasm32>,
    ///   "headers": [...], "rows": [...], "next": ...}`
    /// - failure (bad params, parse error, budget refusal, conflict, ...):
    ///   [`format_error_as_json`]'s envelope, always `"ok": false`.
    ///
    /// `params` must be a JSON object (`{"name": <value>, ...}`); anything
    /// else is reported through the same failure envelope, not a bare
    /// `Result` a caller could forget to check.
    pub fn run_script_json(&self, payload: &str, params: &JsonValue) -> JsonValue {
        #[cfg(not(target_arch = "wasm32"))]
        let start = std::time::Instant::now();

        let params: BTreeMap<String, DataValue> = match params {
            JsonValue::Object(map) => map
                .iter()
                .map(|(k, v)| (k.clone(), DataValue::from(v)))
                .collect(),
            JsonValue::Null => BTreeMap::new(),
            JsonValue::Bool(_)
            | JsonValue::Number(_)
            | JsonValue::String(_)
            | JsonValue::Array(_) => {
                return format_error_as_json(
                    miette::miette!("query parameters must be a JSON object"),
                    Some(payload),
                );
            }
        };

        match self.run_script(payload, params) {
            Ok(rows) => match rows.into_json() {
                Ok(mut envelope) => {
                    // `NamedRows::into_json` is defined to emit an object envelope;
                    // refuse typed if that law ever breaks — never `.expect`.
                    let Some(map) = envelope.as_object_mut() else {
                        return format_error_as_json(
                            miette::miette!("NamedRows JSON envelope was not an object"),
                            Some(payload),
                        );
                    };
                    map.insert("ok".to_string(), json!(true));
                    #[cfg(not(target_arch = "wasm32"))]
                    map.insert("took".to_string(), json!(start.elapsed().as_secs_f64()));
                    envelope
                }
                Err(err) => format_error_as_json(miette::miette!("{err}"), Some(payload)),
            },
            Err(err) => format_error_as_json(err, Some(payload)),
        }
    }
}

#[cfg(test)]
mod tests {
    use miette::{Result, miette};
    use super::*;
    use crate::session::catalog::Catalog;
    use crate::session::db::Engine;
    use crate::store::fjall::new_fjall_storage;

    fn open_engine<S: crate::store::Storage>(store: S) -> Result<Engine<S>> {
        Ok(Engine::compose(store, Catalog::new())?)
    }

    #[test]
    fn run_script_json_success_envelope_has_ok_headers_rows() -> Result<()>  {
        let dir = tempfile::tempdir()?;
        let db = open_engine(new_fjall_storage(dir.path())?)?;
        let out = db.run_script_json("?[x] <- [[1],[2]]", &json!({}));
        assert_eq!(out["ok"], json!(true));
        assert_eq!(out["headers"], json!(["x"]));
        assert_eq!(out["rows"], json!([[1], [2]]));
        Ok(())
    }

    #[test]
    fn run_script_json_binds_params_from_json() -> Result<()>  {
        let dir = tempfile::tempdir()?;
        let db = open_engine(new_fjall_storage(dir.path())?)?;
        let out = db.run_script_json("?[x] <- [[$v]]", &json!({"v": 99}));
        assert_eq!(out["ok"], json!(true));
        assert_eq!(out["rows"], json!([[99]]));
        Ok(())
    }

    #[test]
    fn run_script_json_reports_parse_error_without_panicking() -> Result<()>  {
        let dir = tempfile::tempdir()?;
        let db = open_engine(new_fjall_storage(dir.path())?)?;
        let out = db.run_script_json("not a valid script", &json!({}));
        assert_eq!(out["ok"], json!(false));
        assert!(out.get("message").is_some());
        assert!(out.get("display").is_some());
        Ok(())
    }

    #[test]
    fn run_script_json_refuses_non_object_params() -> Result<()>  {
        let dir = tempfile::tempdir()?;
        let db = open_engine(new_fjall_storage(dir.path())?)?;
        let out = db.run_script_json("?[x] <- [[1]]", &json!([1, 2, 3]));
        assert_eq!(out["ok"], json!(false));
        Ok(())
    }

    /// Story #80's product-surface claim, proven at THIS seam rather than
    /// asserted: `run_script_json` is the one entry point every binding
    /// (kyzo-bin's `POST /text-query`, its REPL, WASM, the C ABI) already
    /// calls for arbitrary script text — it does not special-case any
    /// `SysOp`. So `::verify { ... }` needs no new kyzo-bin code at all to
    /// reach HTTP or the CLI; it rides this seam like any other script the
    /// moment `SysOp::Verify` exists in `kyzo-core`.
    #[test]
    fn run_script_json_carries_the_verify_directive_for_free() -> Result<()>  {
        let dir = tempfile::tempdir()?;
        let db = open_engine(new_fjall_storage(dir.path())?)?;
        db.run_script(
            "?[a, b] <- [[1, 2], [2, 3]] :create edge {a, b}",
            BTreeMap::new(),
        )?;
        let out = db.run_script_json(
            "::verify { path[x, y] := *edge[x, y]
             path[x, z] := path[x, y], *edge[y, z]
             ?[x, y] := path[x, y] }",
            &json!({}),
        );
        assert_eq!(out["ok"], json!(true));
        assert_eq!(out["headers"], json!(["status", "summary", "detail"]));
        assert_eq!(out["rows"][0][0], json!("match"));
        Ok(())
    }
}
