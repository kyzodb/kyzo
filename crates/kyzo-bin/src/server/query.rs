/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `POST /text-query`: run a script. All the JSON shaping — params in,
//! `{ok, headers, rows, took}`/error envelope out — is
//! `kyzo::Engine::run_script_json`; this handler only runs it off the async
//! runtime's blocking pool and maps the envelope to an HTTP status.

use std::fmt;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use serde::de::{MapAccess, Visitor};
use serde_json::Value as JsonValue;
use tokio::task::spawn_blocking;

use super::{DbState, internal_error, wrap_json};

/// POST body: `script` required; `params` absent means JSON null (same as
/// the former serde Default for [`JsonValue`]), never a silent object invent.
pub(super) struct QueryPayload {
    script: String,
    params: JsonValue,
}

impl<'de> Deserialize<'de> for QueryPayload {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = QueryPayload;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("QueryPayload object with script and optional params")
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<QueryPayload, A::Error> {
                let (script, params) =
                    super::payload_wire::take_required_string_and_params(map, "script")?;
                Ok(QueryPayload {
                    script,
                    // Absent params is the published wire null — convert door, not Err/None costume.
                    params: match params {
                        Some(p) => p,
                        None => JsonValue::Null,
                    },
                })
            }
        }
        deserializer.deserialize_map(V)
    }
}

pub(super) async fn text_query(
    State(st): State<DbState>,
    Json(payload): Json<QueryPayload>,
) -> (StatusCode, Json<JsonValue>) {
    let result =
        spawn_blocking(move || st.db.run_script_json(&payload.script, &payload.params)).await;
    match result {
        Ok(envelope) => wrap_json(envelope),
        Err(err) => internal_error(err),
    }
}
