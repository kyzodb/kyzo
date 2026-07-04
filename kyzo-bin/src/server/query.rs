/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `POST /text-query`: run a script. All the JSON shaping — params in,
//! `{ok, headers, rows, took}`/error envelope out — is
//! `kyzo::Db::run_script_json`; this handler only runs it off the async
//! runtime's blocking pool and maps the envelope to an HTTP status.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value as JsonValue;
use tokio::task::spawn_blocking;

use super::{DbState, internal_error, wrap_json};

#[derive(serde_derive::Deserialize)]
pub(super) struct QueryPayload {
    script: String,
    #[serde(default)]
    params: JsonValue,
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
