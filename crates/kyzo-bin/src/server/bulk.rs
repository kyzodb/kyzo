/*
 * Copyright 2023, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0). See `server/mod.rs`'s module doc for why `/import-from-backup`
 * has no equivalent here.
 */

//! Bulk data movement: `GET /export/{relations}`, `PUT /import`, and
//! `POST /backup` (a whole-store dump via `kyzo::dump_storage`). The
//! row-level composition (`::columns` + a scan/mutation query) lives in
//! `crate::bulk`; these handlers are the HTTP plumbing around it.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use itertools::Itertools;
use serde_json::{Value as JsonValue, json};
use tokio::task::spawn_blocking;

use kyzo::NamedRows;

use super::{DbState, internal_error};
use crate::bulk;

pub(super) async fn export_relations(
    State(st): State<DbState>,
    Path(relations_param): Path<String>,
) -> (StatusCode, Json<JsonValue>) {
    let names = relations_param
        .split(',')
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect_vec();
    let result = spawn_blocking(move || bulk::export_relations(&st.db, names.into_iter())).await;
    match result {
        Ok(Ok(exported)) => match exported
            .into_iter()
            .map(|(name, rows)| rows.into_json().map(|j| (name, j)))
            .collect::<Result<serde_json::Map<_, _>, _>>()
        {
            Ok(data) => (StatusCode::OK, json!({"ok": true, "data": data}).into()),
            Err(err) => (
                StatusCode::BAD_REQUEST,
                json!({"ok": false, "message": err.to_string()}).into(),
            ),
        },
        Ok(Err(err)) => (
            StatusCode::BAD_REQUEST,
            json!({"ok": false, "message": err.to_string()}).into(),
        ),
        Err(err) => internal_error(err),
    }
}

pub(super) async fn import_relations(
    State(st): State<DbState>,
    Json(payload): Json<JsonValue>,
) -> (StatusCode, Json<JsonValue>) {
    let Some(payload) = payload.as_object() else {
        return (
            StatusCode::BAD_REQUEST,
            json!({"ok": false, "message": "payload must be a JSON object"}).into(),
        );
    };
    let mut mapping = std::collections::BTreeMap::new();
    for (k, v) in payload {
        match NamedRows::from_json(v) {
            Ok(nr) => {
                mapping.insert(k.to_string(), nr);
            }
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    json!({"ok": false, "message": err.to_string()}).into(),
                );
            }
        }
    }

    let result = spawn_blocking(move || bulk::import_relations(&st.db, mapping)).await;
    match result {
        Ok(Ok(())) => (StatusCode::OK, json!({"ok": true}).into()),
        Ok(Err(err)) => (
            StatusCode::BAD_REQUEST,
            json!({"ok": false, "message": err.to_string()}).into(),
        ),
        Err(err) => internal_error(err),
    }
}

#[derive(serde_derive::Deserialize)]
pub(super) struct BackupPayload {
    path: String,
}

pub(super) async fn backup(
    State(st): State<DbState>,
    Json(payload): Json<BackupPayload>,
) -> (StatusCode, Json<JsonValue>) {
    let result = spawn_blocking(move || kyzo::dump_storage(&st.storage, payload.path)).await;
    match result {
        Ok(Ok(())) => (StatusCode::OK, json!({"ok": true}).into()),
        Ok(Err(err)) => (
            StatusCode::BAD_REQUEST,
            json!({"ok": false, "message": err.to_string()}).into(),
        ),
        Err(err) => internal_error(err),
    }
}
