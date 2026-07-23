/*
 * Copyright 2023, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0). See `server/mod.rs`'s module doc for the auth-model changes
 * (no per-request mutability, `x-kyzo-auth`, the simplified token-table
 * check) — this file is the mechanism, that doc is the account of what
 * changed and why. The token-table relation name (an operator-supplied
 * `--token-table` value, not per-request attacker input) is spliced into
 * composed KyzoScript the same way `bulk.rs` splices caller-supplied
 * relation names, so it gets the same validation — see that module's doc
 * for why this hardens rather than closes an escalation.
 */

//! The auth gate every route (except `/`) passes through: bind address
//! `127.0.0.1` skips it, otherwise a request needs the `x-kyzo-auth`
//! header, an `?auth=` query parameter, or (if a token table is
//! configured) a `Bearer` token matching a row in it.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use futures::future::BoxFuture;
use kyzo::{DataValue, Engine, FjallStorage};
use tower_http::auth::AsyncAuthorizeRequest;

#[derive(Clone)]
pub(super) struct MyAuth {
    skip_auth: bool,
    auth_guard: String,
    token_table: Option<Arc<(String, Engine<FjallStorage>)>>,
}

impl MyAuth {
    pub(super) fn new(
        skip_auth: bool,
        auth_guard: String,
        token_table: Option<Arc<(String, Engine<FjallStorage>)>>,
    ) -> Self {
        Self {
            skip_auth,
            auth_guard,
            token_table,
        }
    }

    /// Does a matching `{token: <token>}` row exist in the configured
    /// token-table relation? `false` on any error (a broken auth check
    /// refuses, it never fails open) — including an invalid relation name,
    /// which is an operator misconfiguration, not a request to satisfy.
    fn token_table_authorizes(table: &(String, Engine<FjallStorage>), token: &str) -> bool {
        let (name, db) = table;
        if crate::bulk::validate_identifier(name).is_err() {
            eprintln!("token-table auth check failed: '{name}' is not a valid relation name");
            return false;
        }
        match db.run_script(
            &format!("?[token] := *{name}{{token: $token}}"),
            BTreeMap::from([("token".to_string(), DataValue::from(token))]),
        ) {
            Ok(rows) => !rows.rows().is_empty(),
            Err(err) => {
                eprintln!("token-table auth check failed: {err}");
                false
            }
        }
    }
}

impl AsyncAuthorizeRequest<Body> for MyAuth {
    type RequestBody = Body;
    type ResponseBody = Body;
    type Future = BoxFuture<'static, Result<Request<Body>, Response<Self::ResponseBody>>>;

    fn authorize(&mut self, request: Request<Body>) -> Self::Future {
        let skip_auth = self.skip_auth;
        let auth_guard = self.auth_guard.clone();
        let token_table = self.token_table.clone();
        Box::pin(async move {
            let authorized = if skip_auth {
                true
            } else {
                match request.headers().get("x-kyzo-auth") {
                    Some(v) => match v.to_str() {
                        Ok(s) => s == auth_guard,
                        // Non-UTF8 header cannot equal the guard — refuse closed.
                        Err(_) => {
                            let non_utf8_header_refused = false;
                            non_utf8_header_refused
                        },
                    },
                    None => {
                        let via_query = request
                            .uri()
                            .query()
                            .into_iter()
                            .flat_map(|q| q.split('&'))
                            .filter_map(|pair| pair.split_once('='))
                            .any(|(k, v)| k == "auth" && v == auth_guard);
                        if via_query {
                            true
                        } else {
                            match (&token_table, request.headers().get("Authorization")) {
                                (Some(tt), Some(auth_header)) => match auth_header.to_str() {
                                    Ok(s) => match s.strip_prefix("Bearer ") {
                                        Some(token) => {
                                            MyAuth::token_table_authorizes(tt, token)
                                        }
                                        // Present Authorization that is not Bearer — refuse.
                                        None => {
                                            let non_bearer_refused = false;
                                            non_bearer_refused
                                        },
                                    },
                                    // Non-UTF8 Authorization — refuse closed.
                                    Err(_) => {
                                        let non_utf8_authorization_refused = false;
                                        non_utf8_authorization_refused
                                    },
                                },
                                (None, Some(_)) | (Some(_), None) | (None, None) => false,
                            }
                        }
                    }
                }
            };
            if authorized {
                Ok(request)
            } else {
                // Builder with only a status cannot fail; construct the 401
                // without unwrap so a poisoned builder cannot panic the gate.
                let mut response = Response::new(Body::empty());
                *response.status_mut() = StatusCode::UNAUTHORIZED;
                Err(response)
            }
        })
    }
}
