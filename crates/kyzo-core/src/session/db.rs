/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The session tier: everything between a caller and the query/storage
//! organs. The [`db`] entrypoint (`run_query`, sessions, cooperative
//! cancellation, commit retry), the mutation tier ([`mutate`]: puts,
//! retractions, index creation and backfill), the catalog ([`relation`]),
//! transaction-scoped [`constraint`]s, and change [`callback`]s.

// Production host door: `runtime/db.rs` and `runtime/mutate.rs` dispatch
// commit notifications through `CallbackCollector` / `EventCallbackRegistry`
// (P112).
pub(crate) mod callback;

/// The engine's ONE wall-clock read: the system-time stamp for writes.
/// Lives in the runtime tier by law — the value plane has no ambient
/// clock, and determinism campaigns replay stamps rather than minting
/// them.
pub(crate) fn current_validity() -> miette::Result<kyzo_model::value::ValidityTs> {
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| miette::miette!("system clock before the epoch: {e}"))?
        .as_micros();
    let micros: i64 = micros
        .try_into()
        .map_err(|_| miette::miette!("system clock beyond i64 microseconds"))?;
    Ok(kyzo_model::value::ValidityTs::from_raw(micros))
}

// Production host door: `runtime/db.rs` enforces constraints at commit
// (`enforce_constraints`, `sys_create_constraint`, …) (P112).
pub(crate) mod constraint;

// Production host door: `Db`, `run_script`, `compile_and_eval` are live and
// re-exported at the crate root (P112).
pub(crate) mod db;
#[cfg(test)]
mod db_battery;
/// Catalog / relation / index freshness authorities (story #337).
pub(crate) mod generation;
pub(crate) mod json;
// Production host door: `runtime/db.rs` and `runtime/mutate.rs` dispatch
// puts, retractions, and index create/drop (P112).
pub(crate) mod mutate;
// Production host door: the catalog is live on every query and mutation
// path via `get_relation` / schema checks (P112).
pub(crate) mod relation;
pub(crate) mod verify;
