/*
 * Copyright 2023, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): `register_callback` takes one argument here (no per-op
 * filter), matching `runtime/callback.rs`'s signature. The original's
 * `Event::json_data(item).unwrap()` panicked the whole per-connection async
 * task if encoding ever failed; that panic is user-reachable (any change to
 * a watched relation drives it), so a failure now logs and ends the stream
 * — the client sees the SSE connection close, not the server crash a task.
 */

//! `GET /changes/{relation}`: an SSE stream of put/retract events on a
//! relation, bridged from `kyzo::Engine::register_callback`'s
//! `std::sync::mpsc::Receiver`.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive};
use axum::response::{IntoResponse, Response, Sse};
use futures::stream::Stream;
use log::{error, info};
use serde_json::{Value as JsonValue, json};
use tokio::task::spawn_blocking;

use kyzo::{
    CallbackId, DataValue, Engine, FjallStorage, NamedRows, NonFiniteJsonNumber, SignedFact,
    StandingQuery, Storage, Tuple,
};

use super::DbState;

pub(super) async fn observe_changes(
    State(st): State<DbState>,
    Path(relation): Path<String>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (id, recv) = st.db.register_callback(&relation);
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    struct Guard {
        id: CallbackId,
        db: Engine<FjallStorage>,
        relation: String,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            info!("dropping changes SSE {}: {}", self.relation, self.id.get());
            self.db.unregister_callback(self.id);
        }
    }

    spawn_blocking(move || {
        for data in recv {
            if sender.blocking_send(data).is_err() {
                break;
            }
        }
    });
    let stream = async_stream::stream! {
        info!("starting changes SSE {}: {}", relation, id.get());
        let _guard = Guard {id, db: st.db, relation};
        while let Some(event) = receiver.recv().await {
            let (Ok(new_rows), Ok(old_rows)) = (
                NamedRows::into_json(event.new_rows),
                NamedRows::into_json(event.old_rows),
            ) else {
                error!("changes SSE: non-finite Num cannot encode as JSON; ending stream");
                break;
            };
            let item = json!({
                "op": event.op.to_string(),
                "new_rows": new_rows,
                "old_rows": old_rows,
            });
            match Event::default().json_data(item) {
                Ok(event) => yield Ok(event),
                Err(err) => {
                    error!("changes SSE: failed to encode event, ending stream: {err}");
                    break;
                }
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// `GET /standing?query=...&params=...`: an SSE stream of a standing
// query's OWN answer — the initial snapshot, then each subsequent
// signed delta as commits land — bridged from
// `kyzo::Engine::register_standing`'s `StandingQuery`. The per-query analog
// of `GET /changes/{relation}` one tier up: that streams one relation's
// raw put/retract events, this streams a QUERY's own maintained answer.
//
// `apply_pending` is pull-based (see `query::standing`'s module doc), so
// this handler polls on a short fixed interval rather than inventing a
// second drive model at the HTTP tier.

#[derive(serde_derive::Deserialize)]
pub(super) struct StandingQueryParams {
    query: String,
    /// A JSON object, URL-encoded like `query` — the same "JSON in"
    /// convention `run_script_json` uses, adapted to a query-string
    /// parameter since GET has no body.
    #[serde(default)]
    params: String,
}

/// [`StandingQueryParams::params`], parsed and converted through the
/// SAME `JsonValue -> DataValue` conversion `run_script_json` uses (see
/// `runtime/json.rs`) — never a second hand-rolled one.
fn parse_params(raw: &str) -> Result<BTreeMap<String, DataValue>, String> {
    if raw.is_empty() {
        return Ok(BTreeMap::new());
    }
    match serde_json::from_str::<JsonValue>(raw) {
        Ok(JsonValue::Object(map)) => Ok(map
            .iter()
            .map(|(k, v)| (k.clone(), DataValue::from(v)))
            .collect()),
        Ok(JsonValue::Null) => Ok(BTreeMap::new()),
        Ok(_) => Err("params must be a JSON object".to_string()),
        Err(err) => Err(format!("params is not valid JSON: {err}")),
    }
}

fn tuple_json(row: &Tuple) -> Result<JsonValue, NonFiniteJsonNumber> {
    row.iter()
        .map(JsonValue::try_from)
        .collect::<Result<Vec<_>, _>>()
        .map(JsonValue::Array)
}

fn signed_fact_json(fact: &SignedFact) -> Result<JsonValue, NonFiniteJsonNumber> {
    match fact {
        SignedFact::Plus(row) => Ok(json!({"op": "assert", "row": tuple_json(row)?})),
        SignedFact::Minus(row) => Ok(json!({"op": "retract", "row": tuple_json(row)?})),
    }
}

fn error_response(status: StatusCode, message: String) -> Response {
    (status, Json(json!({"ok": false, "message": message}))).into_response()
}

/// Guards a live [`StandingQuery`] so a dropped SSE connection (client
/// disconnect, or the stream ending its own loop) unregisters its
/// callback subscriptions PROMPTLY rather than leaking them until the
/// registry's own lazy disconnect-pruning next happens to run — the
/// same discipline `changes.rs`'s own `Guard` applies to a raw
/// `register_callback` subscription, one level up. `Option`, not a bare
/// `StandingQuery`: `teardown` takes owned `self` (it consumes
/// `subscriptions` to unregister each one), which a `Drop::drop(&mut
/// self)` can only reach through a `take`.
struct TeardownGuard<S: Storage>(Option<StandingQuery<S>>);

impl<S: Storage> Drop for TeardownGuard<S> {
    fn drop(&mut self) {
        if let Some(sq) = self.0.take() {
            sq.teardown();
        }
    }
}

pub(super) async fn observe_standing(
    State(st): State<DbState>,
    Query(q): Query<StandingQueryParams>,
) -> Response {
    let params = match parse_params(&q.params) {
        Ok(p) => p,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };

    let query_text = q.query.clone();
    let registered = spawn_blocking(move || st.db.register_standing(&query_text, params)).await;
    let sq = match registered {
        Ok(Ok(sq)) => sq,
        // A refused query (recursion, a fixed rule, a construct the
        // translator has no representation for, …) is exactly the typed
        // error `register_standing` already carries — rendered as a
        // clean 400, never a panic or a hung connection.
        Ok(Err(err)) => return error_response(StatusCode::BAD_REQUEST, err.to_string()),
        Err(join_err) => {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, join_err.to_string());
        }
    };

    let initial = match sq
        .current_answer()
        .iter()
        .map(tuple_json)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(rows) => rows,
        Err(err) => return error_response(StatusCode::BAD_REQUEST, err.to_string()),
    };
    let log_query = q.query.clone();

    let stream = async_stream::stream! {
        info!("starting standing SSE: {log_query}");
        let mut guard = TeardownGuard(Some(sq));

        match Event::default().json_data(json!({"type": "init", "rows": initial})) {
            Ok(event) => yield Ok::<_, Infallible>(event),
            Err(err) => {
                error!("standing SSE: failed to encode init event, ending stream: {err}");
                return;
            }
        }

        let mut ticker = tokio::time::interval(Duration::from_millis(150));
        loop {
            ticker.tick().await;
            let Some(mut sq) = guard.0.take() else {
                break;
            };
            let (result, sq) = match spawn_blocking(move || {
                let result = sq.apply_pending_answer();
                (result, sq)
            })
            .await
            {
                Ok(pair) => pair,
                Err(join_err) => {
                    error!("standing SSE: blocking task panicked, ending stream: {join_err}");
                    break;
                }
            };
            guard.0 = Some(sq);

            // `SignedFact` wraps `DataValue`, which clippy flags as a
            // "mutable key type" through false-positive interior-
            // mutability detection in ITS OWN field types (a regex
            // engine's internal cache pool, several layers down) — the
            // exact false positive kyzo-core's lib.rs already documents
            // and allows crate-wide; keys here are never mutated via a
            // shared reference either.
            #[allow(clippy::mutable_key_type)]
            let delta = match result {
                Ok(d) => d,
                Err(err) => {
                    error!("standing SSE: apply_pending failed, ending stream: {err}");
                    break;
                }
            };
            if delta.is_empty() {
                continue;
            }
            let changes = match delta.iter().map(signed_fact_json).collect::<Result<Vec<_>, _>>() {
                Ok(c) => c,
                Err(err) => {
                    error!(
                        "standing SSE: non-finite Num cannot encode as JSON, ending stream: {err}"
                    );
                    break;
                }
            };
            match Event::default().json_data(json!({"type": "delta", "changes": changes})) {
                Ok(event) => yield Ok(event),
                Err(err) => {
                    error!("standing SSE: failed to encode delta event, ending stream: {err}");
                    break;
                }
            }
        }
        info!("ending standing SSE: {log_query}");
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}
