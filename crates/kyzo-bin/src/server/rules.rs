/*
 * Copyright 2023, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0). Speaks `std::sync::mpsc`, not `crossbeam`: `fixed_rule/mod.rs`'s
 * `SimpleFixedRule::rule_with_channel` already made that swap in this port
 * (see its port note), and this bridge follows it — see `server/mod.rs`'s
 * module doc for the whole account. Same fix as `changes.rs`: the
 * original's `Event::json_data(item).unwrap()` panicked the per-connection
 * async task on an encoding failure; both sites here now log and end the
 * stream instead.
 */

//! The downstream-computed fixed-rule bridge: `GET /rules/{name}` registers
//! a rule and opens an SSE stream of its invocation requests; a downstream
//! worker answers each one via `POST`/`DELETE /rule-result/{id}`.

use std::convert::Infallible;
use std::sync::atomic::Ordering;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Sse;
use axum::response::sse::{Event, KeepAlive};
use futures::stream::Stream;
use log::{error, info};
use miette::miette;
use serde_json::{Value as JsonValue, json};

use kyzo::{Engine, FjallStorage, NamedRows, SimpleFixedRule};

use super::DbState;

#[derive(serde_derive::Deserialize)]
pub(super) struct RuleRegisterOptions {
    arity: usize,
}

pub(super) async fn post_rule_result(
    State(st): State<DbState>,
    Path(id): Path<u32>,
    Json(res): Json<JsonValue>,
) -> (StatusCode, Json<JsonValue>) {
    let res = match NamedRows::from_json(&res) {
        Ok(res) => res,
        Err(err) => {
            if let Some(ch) = st.rule_senders.lock().unwrap().remove(&id) {
                let _ = ch.send(Err(miette!("downstream posted malformed result")));
            }
            return (
                StatusCode::BAD_REQUEST,
                json!({"ok": false, "message": err.to_string()}).into(),
            );
        }
    };
    if let Some(ch) = st.rule_senders.lock().unwrap().remove(&id) {
        match ch.send(Ok(res)) {
            Ok(_) => (StatusCode::OK, json!({"ok": true}).into()),
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"ok": false, "message": err.to_string()}).into(),
            ),
        }
    } else {
        (StatusCode::NOT_FOUND, json!({"ok": false}).into())
    }
}

pub(super) async fn post_rule_err(
    State(st): State<DbState>,
    Path(id): Path<u32>,
) -> (StatusCode, Json<JsonValue>) {
    if let Some(ch) = st.rule_senders.lock().unwrap().remove(&id) {
        match ch.send(Err(miette!("downstream cancelled computation"))) {
            Ok(_) => (StatusCode::OK, json!({"ok": true}).into()),
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"ok": false, "message": err.to_string()}).into(),
            ),
        }
    } else {
        (StatusCode::NOT_FOUND, json!({"ok": false}).into())
    }
}

pub(super) async fn register_rule(
    State(st): State<DbState>,
    Path(name): Path<String>,
    Query(rule_opts): Query<RuleRegisterOptions>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (rule, task_receiver) = SimpleFixedRule::rule_with_channel(rule_opts.arity);
    let (down_sender, mut down_receiver) = tokio::sync::mpsc::channel(1);
    let mut errored = None;

    if let Err(err) = st.db.register_fixed_rule(name.clone(), rule) {
        errored = Some(err);
    } else {
        let rule_senders = st.rule_senders.clone();
        let rule_counter = st.rule_counter.clone();

        rayon::spawn(move || {
            for (inputs, options, sender) in task_receiver {
                let id = rule_counter.fetch_add(1, Ordering::AcqRel);
                let inputs: JsonValue = inputs.into_iter().map(NamedRows::into_json).collect();
                let options: JsonValue = options
                    .into_iter()
                    .map(|(k, v)| (k, JsonValue::from(&v)))
                    .collect();
                if down_sender.blocking_send((id, inputs, options)).is_err() {
                    let _ = sender.send(Err(miette!("cannot send request to downstream")));
                } else {
                    rule_senders.lock().unwrap().insert(id, sender);
                }
            }
        });
    }

    struct Guard {
        name: String,
        db: Engine<FjallStorage>,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            info!("dropping rules SSE {}", self.name);
            self.db.unregister_fixed_rule(&self.name);
        }
    }

    let stream = async_stream::stream! {
        if let Some(err) = errored {
            let item = json!({"type": "register-error", "error": err.to_string()});
            match Event::default().json_data(item) {
                Ok(event) => yield Ok(event),
                Err(err) => error!("rules SSE: failed to encode register-error event: {err}"),
            }
        } else {
            info!("starting rule SSE {}", name);
            let _guard = Guard {db: st.db, name};
            while let Some((id, inputs, options)) = down_receiver.recv().await {
                let item = json!({"type": "request", "id": id, "inputs": inputs, "options": options});
                match Event::default().json_data(item) {
                    Ok(event) => yield Ok(event),
                    Err(err) => {
                        error!("rules SSE: failed to encode event, ending stream: {err}");
                        break;
                    }
                }
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}
