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

// The tier's consumers (runtime/db.rs, query/eval.rs) land later. In the
// lib build the modules are dead (expect); in test builds the in-file tests
// keep them live but not every item, so a plain `allow` covers the
// remainder — the same pattern as the `parse` module in lib.rs.
#[allow(dead_code)]
pub(crate) mod callback;

/// The engine's ONE wall-clock read: the system-time stamp for writes.
/// Lives in the runtime tier by law — the value plane has no ambient
/// clock, and determinism campaigns replay stamps rather than minting
/// them.
pub(crate) fn current_validity() -> miette::Result<crate::data::value::ValidityTs> {
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| miette::miette!("system clock before the epoch: {e}"))?
        .as_micros();
    let micros: i64 = micros
        .try_into()
        .map_err(|_| miette::miette!("system clock beyond i64 microseconds"))?;
    Ok(crate::data::value::ValidityTs::from_raw(micros))
}
// constraint has no production caller yet, unlike db/relation below,
// which have landed; kept live only by its own in-file tests.
#[allow(dead_code)]
pub(crate) mod constraint;
// db's core production entrypoint (Db, run_script, compile_and_eval) is
// fully live and re-exported at the crate root; `#[allow(dead_code)]`
// stays for residual routed accessors (get_routed/exists_routed/
// del_routed) no production path reaches yet, kept live by this
// module's own tests.
#[allow(dead_code)]
pub(crate) mod db;
#[cfg(test)]
mod db_battery;
/// Catalog / relation / index freshness authorities (story #337).
pub(crate) mod generation;
pub(crate) mod json;
// The mutation tier and catalog carry surface that is lib-dead until its
// operator lands (`::index drop` variants, schema-compat checks used by
// unlanded ops) and items whose only callers are tests; a mod-level
// `allow` covers that remainder honestly.
#[allow(dead_code)]
pub(crate) mod mutate;
// relation's catalog is fully live in production; `#[allow(dead_code)]`
// stays for residual accessors (raw_binding_map, has_index, put_fact,
// retract_fact, encode_val_for_store, prove_compatible_input, exists,
// skip_scan_bounded_prefix, relation_exists) no production path reaches
// yet, kept live by this module's own tests.
#[allow(dead_code)]
pub(crate) mod relation;
pub(crate) mod verify;
