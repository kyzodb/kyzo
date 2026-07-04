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
//! relation, bridged from `kyzo::Db::register_callback`'s
//! `std::sync::mpsc::Receiver`.

use std::convert::Infallible;

use axum::extract::{Path, State};
use axum::response::Sse;
use axum::response::sse::{Event, KeepAlive};
use futures::stream::Stream;
use log::{error, info};
use serde_json::json;
use tokio::task::spawn_blocking;

use kyzo::{Db, FjallStorage, NamedRows};

use super::DbState;

pub(super) async fn observe_changes(
    State(st): State<DbState>,
    Path(relation): Path<String>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (id, recv) = st.db.register_callback(&relation);
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    struct Guard {
        id: u32,
        db: Db<FjallStorage>,
        relation: String,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            info!("dropping changes SSE {}: {}", self.relation, self.id);
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
        info!("starting changes SSE {}: {}", relation, id);
        let _guard = Guard {id, db: st.db, relation};
        while let Some((op, new, old)) = receiver.recv().await {
            let item = json!({
                "op": op.to_string(),
                "new_rows": NamedRows::into_json(new),
                "old_rows": NamedRows::into_json(old),
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
