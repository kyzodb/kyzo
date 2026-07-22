/*
 * Copyright 2023, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0). The former flat `server.rs` is now this directory: one module
 * per HTTP concern (`auth`, `query`, `bulk`, `rules`, `feeds`, `console`);
 * this file owns only `ServerArgs`, the shared `DbState`, and wiring the
 * router together. Behavior changes from the CozoDB original — sealed
 * doctrine (cite rather than re-litigate):
 *
 * - **Engine selection** goes through `engine::open` (one backend, `fjall`;
 *   see that module's doc) instead of `DbInstance::new(engine, path, config)`
 *   dispatching over five backends.
 * - **No per-request mutability override.** Upstream's auth layer resolved
 *   to a `ScriptMutability` (`Mutable`/`Immutable`) inserted as a request
 *   extension, and `text_query` could additionally downgrade an
 *   authenticated mutable request to read-only via `{"immutable": true}` in
 *   the payload; `DbInstance::run_script` enforced the mismatch internally.
 *   kyzo-core's `Engine::run_script` takes no mutability parameter at all — a
 *   script's mutability is a property the engine reads off the parsed
 *   program itself (`needs_write_lock`), not a caller-supplied claim to
 *   check against — and there is no public hook to ask "would this script
 *   write, without running it." Authorization is therefore all-or-nothing
 *   here: a request either passes the token/localhost gate or gets 401.
 *   Reopening this would mean adding a new public capability to
 *   kyzo-core's runtime tier (script-mutation introspection pre-execution),
 *   which is a runtime-tier design decision past this story's scope
 *   (porting cozo-bin), not a difficulty deferral.
 * - **No `/transact` multi-statement transactions.** Upstream's
 *   `DbInstance::multi_transaction`/`run_multi_transaction` held a live
 *   transaction across several HTTP requests. kyzo-core's transaction type
 *   (`SessionTx`) has only `pub(crate)` constructors (`session/db.rs`) and
 *   `Engine` exposes no equivalent of `multi_transaction` — there is nothing to
 *   call. Dropped, not stubbed; the fix is new kyzo-core runtime-tier API,
 *   not a bin-crate workaround.
 * - **No `/import-from-backup`.** Upstream partially restored named
 *   relations from a backup file into a live, non-empty store.
 *   `kyzo::restore_storage`'s contract is whole-store-only into an EMPTY
 *   target (`store/backup.rs`: "recovery is always discard-and-re-run");
 *   there is no selective-relation restore to call. `bulk::backup`
 *   (whole-store dump) and `bulk::import_relations` (row-level) remain and
 *   together cover the same ground through different, real entry points.
 * - **`query::text_query` is one line of engine logic** —
 *   `kyzo::Engine::run_script_json` — because the JSON envelope (params in,
 *   `{ok, headers, rows, took}`/error out) now lives in kyzo-core itself
 *   (`session::json`), shared with every future binding. This file only
 *   maps the envelope's `ok` field to an HTTP status code
 *   ([`wrap_json`]).
 * - **The `/rules` SSE bridge speaks `std::sync::mpsc`, not `crossbeam`.**
 *   `SimpleFixedRule::rule_with_channel` in this port already made that
 *   swap (`rules/` port note); `rules.rs` follows it, which
 *   drops the `crossbeam` dependency entirely.
 * - **`x-cozo-auth` becomes `x-kyzo-auth`**; the token-table auth check
 *   (`auth.rs`) now tests only that a matching token row exists
 *   (`?[token] := *{name}{token: $token}`) — the row's old `mutable` column
 *   controlled the dropped read/write distinction, so requiring it would
 *   ask every deployment's token table to carry a column this port cannot
 *   honor.
 * - **`CompressionLayer` is back, gzip+brotli only.** `compression-zstd`
 *   depends on `zstd-safe` -> `zstd-sys`, a C dependency, so it is excluded
 *   by naming exactly the codecs wanted (`compression-gzip`,
 *   `compression-br`) rather than the umbrella feature — `flate2`'s
 *   default backend is `miniz_oxide` (pure Rust; its C zlib backends are
 *   separate, non-default features never enabled here) and the `brotli`
 *   crate is a from-scratch Rust port, not a binding to Google's C
 *   library. Verified the same way as the TLS stack: `cargo tree -e
 *   normal,build` shows no `cc`/`*-sys` crate with these two enabled.
 * - `rand::thread_rng()` / `Alphanumeric` sampling (0.8 API) becomes
 *   `rand::rng()` / `Alphanumeric.sample_string` (0.9's `SampleString`
 *   trait) for the auth-token generator.
 * - `lazy_static!`/lifetime-annotated `Db<'s, S>` are gone: kyzo-core's `Engine`
 *   is not lifetime-parameterized (its sessions own their transactions
 *   rather than borrowing one — `session/db.rs`'s module doc), so
 *   `DbState`/`MyAuth` hold plain owned clones.
 * - **No workspace-wide `DefaultBodyLimit::disable()`.** The original
 *   disabled axum's default 2 MiB body cap for the WHOLE router so `/import`
 *   could accept bulk data — but that disables it for every other route
 *   too, including `/text-query`: one oversized request to any endpoint
 *   would buffer unbounded via the `Json` extractor with no limit at all
 *   (a one-connection memory-exhaustion DoS). Only `/import` now raises its
 *   limit, via a route-specific `DefaultBodyLimit::max` layer sized by
 *   `--max-import-body-mb`; every other route keeps axum's own default.
 * - **Startup bind/port parse** refuses with a typed miette error (address
 *   parse, auth-token write, listen, serve) — never process-entry unwrap.
 */

//! The HTTP API server: route table, shared state, and startup. Each route
//! group is its own module — this file only assembles them.

mod auth;
mod bulk;
mod console;
mod feeds;
mod query;
mod rules;

use std::collections::BTreeMap;
use std::net::{Ipv6Addr, SocketAddr};
use std::str::FromStr;
use std::sync::atomic::AtomicU32;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderName, Method, StatusCode, header};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use clap::Args;
use log::{error, info, warn};
use rand::distr::{Alphanumeric, SampleString};
use serde_json::{Value as JsonValue, json};
use tokio::net::TcpListener;
use tower_http::auth::AsyncRequireAuthorizationLayer;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};

use kyzo::{Engine, FjallStorage, NamedRows};

use crate::engine::{self, StorageArgs};
use auth::MyAuth;

#[derive(Args, Debug)]
pub(crate) struct ServerArgs {
    /// Storage engine: `fjall` (persistent, at `--path`) or `mem` (ephemeral).
    #[clap(short, long, default_value = "mem")]
    engine: String,

    /// Path to the directory to store the database (only used by `fjall`).
    #[clap(short, long, default_value = "kyzo.db")]
    path: String,

    #[clap(flatten)]
    storage: StorageArgs,

    /// Restore from the given whole-store dump before starting the server.
    /// The target engine/path must be empty (`kyzo::restore_storage`'s
    /// contract).
    #[clap(long)]
    restore: Option<String>,

    /// Address to bind the service to.
    #[clap(short, long, default_value = "127.0.0.1")]
    bind: String,

    /// Port to use.
    #[clap(short = 'P', long, default_value_t = 9070)]
    port: u16,

    /// When set, the content of the named relation is used as a token
    /// table: a row `{token: <str>}` in it authorizes that token.
    #[clap(long)]
    token_table: Option<String>,

    /// Maximum request body size for `PUT /import` (the bulk relation-data
    /// endpoint), in mebibytes. Every other route keeps axum's own default
    /// limit of 2 MiB — disabling the limit workspace-wide would let a
    /// single oversized request to ANY endpoint (e.g. `/text-query`)
    /// exhaust memory before the engine ever sees it.
    #[clap(long, default_value_t = 512)]
    max_import_body_mb: u64,
}

/// Shared server state, one clone per request. `db`/`storage` are the same
/// pair `engine::DbHandle` hands back the REPL: `db` for scripts, `storage`
/// for the whole-store dump/restore ops that need the backend directly.
#[derive(Clone)]
struct DbState {
    db: Engine<FjallStorage>,
    storage: FjallStorage,
    rule_senders: Arc<Mutex<BTreeMap<u32, SyncSender<miette::Result<NamedRows>>>>>,
    rule_counter: Arc<AtomicU32>,
}

pub(crate) async fn server_main(args: ServerArgs) -> miette::Result<()> {
    let handle = match engine::open(&args.engine, &args.path, args.storage) {
        Ok(h) => h,
        Err(err) => {
            error!("{err}");
            return Err(miette::miette!("cannot open database: {err}"));
        }
    };
    if let Some(p) = &args.restore
        && let Err(err) = kyzo::restore_storage(&handle.storage, p)
    {
        error!("{err}");
        return Err(miette::miette!(
            "restore from backup failed, terminate: {err}"
        ));
    }

    let skip_auth = args.bind == "127.0.0.1";

    let conf_path = if skip_auth {
        String::new()
    } else {
        format!("{}.{}.kyzo_auth", args.path, args.engine)
    };
    let auth_guard = if skip_auth {
        String::new()
    } else {
        match tokio::fs::read_to_string(&conf_path).await {
            Ok(s) => s.trim().to_string(),
            Err(_) => {
                let s = Alphanumeric.sample_string(&mut rand::rng(), 64);
                tokio::fs::write(&conf_path, &s).await.map_err(|err| {
                    miette::miette!("failed to write auth token file {conf_path}: {err}")
                })?;
                s
            }
        }
    };

    let auth_obj = MyAuth::new(
        skip_auth,
        auth_guard,
        args.token_table.map(|t| Arc::new((t, handle.db.clone()))),
    );

    let state = DbState {
        db: handle.db,
        storage: handle.storage,
        rule_senders: Default::default(),
        rule_counter: Default::default(),
    };
    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_origin(Any)
        .allow_headers([header::CONTENT_TYPE, HeaderName::from_static("x-kyzo-auth")]);

    // Only `/import` gets a raised body limit, and only there: every other
    // route keeps axum's own 2 MiB default (`DefaultBodyLimit`'s doc), so a
    // request to `/text-query` or any other endpoint still gets a 413
    // instead of being buffered without bound.
    let mb = usize::try_from(args.max_import_body_mb).map_err(|_| {
        miette::miette!(
            "max_import_body_mb {} does not fit usize on this host",
            args.max_import_body_mb
        )
    })?;
    let max_import_bytes = mb.checked_mul(1024 * 1024).ok_or_else(|| {
        miette::miette!(
            "max_import_body_mb {} overflows the import byte budget",
            args.max_import_body_mb
        )
    })?;

    let app = Router::new()
        .route("/text-query", post(query::text_query))
        .route("/export/{relations}", get(bulk::export_relations))
        .route(
            "/import",
            put(bulk::import_relations).layer(DefaultBodyLimit::max(max_import_bytes)),
        )
        .route("/backup", post(bulk::backup))
        .route("/changes/{relation}", get(feeds::observe_changes))
        .route("/standing", get(feeds::observe_standing))
        .route("/rules/{name}", get(rules::register_rule))
        .route(
            "/rule-result/{id}",
            post(rules::post_rule_result).delete(rules::post_rule_err),
        )
        .with_state(state)
        .layer(AsyncRequireAuthorizationLayer::new(auth_obj))
        .fallback(console::not_found)
        .route("/", get(console::root))
        .layer(cors)
        .layer(CompressionLayer::new());

    let addr = if Ipv6Addr::from_str(&args.bind).is_ok() {
        SocketAddr::from_str(&format!("[{}]:{}", args.bind, args.port)).map_err(|err| {
            miette::miette!(
                "invalid bind address '[{}]:{}': {err}",
                args.bind,
                args.port
            )
        })?
    } else {
        SocketAddr::from_str(&format!("{}:{}", args.bind, args.port)).map_err(|err| {
            miette::miette!("invalid bind address '{}:{}': {err}", args.bind, args.port)
        })?
    };

    if args.bind != "127.0.0.1" {
        warn!("{}", include_str!("../security.txt"));
        info!("The auth token is in the file: {conf_path}");
    }

    info!(
        "Starting KyzoDB ({}-backed) API at http://{}",
        args.engine, addr
    );

    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|err| miette::miette!("failed to bind TCP listener on {addr}: {err}"))?;
    axum::serve(listener, app.into_make_service())
        .await
        .map_err(|err| miette::miette!("HTTP server failed: {err}"))?;
    Ok(())
}

/// Map a JSON envelope carrying an `"ok"` boolean (kyzo-core's
/// `run_script_json`, or a handler's own `json!({"ok": ..})`) to an HTTP
/// status: 200 when `ok`, 400 otherwise.
fn wrap_json(envelope: JsonValue) -> (StatusCode, Json<JsonValue>) {
    let code = if envelope.get("ok") == Some(&JsonValue::Bool(true)) {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    (code, envelope.into())
}

/// A `tokio::task::JoinError` (the blocking task itself panicked) is the
/// only error shape a handler using `spawn_blocking` reports this way —
/// everything the engine itself can refuse comes back as `Ok(Err(_))` and
/// is rendered through the ordinary error envelope instead.
fn internal_error<E>(err: E) -> (StatusCode, Json<JsonValue>)
where
    E: std::error::Error,
{
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"ok": false, "message": err.to_string()}).into(),
    )
}
