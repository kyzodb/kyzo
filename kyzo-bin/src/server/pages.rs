/*
 * Copyright 2023, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0).
 */

//! The two routes that aren't API calls: the static console page at `/`,
//! and the JSON 404 every unmatched route falls back to.

use axum::Json;
use axum::http::{StatusCode, Uri};
use axum::response::Html;
use serde_json::{Value as JsonValue, json};

pub(super) async fn root() -> Html<&'static str> {
    Html(include_str!("../index.html"))
}

pub(super) async fn not_found(uri: Uri) -> (StatusCode, Json<JsonValue>) {
    (
        StatusCode::NOT_FOUND,
        json!({"ok": false, "message": format!("No route {uri}")}).into(),
    )
}
