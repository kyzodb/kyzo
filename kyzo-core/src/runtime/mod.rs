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
#[allow(dead_code)]
pub(crate) mod constraint;
#[allow(dead_code)]
pub(crate) mod db;
#[cfg(test)]
mod db_battery;
pub(crate) mod json;
// The mutation tier and catalog carry surface that is lib-dead until its
// operator lands (`::index drop` variants, schema-compat checks used by
// unlanded ops) and items whose only callers are tests; a mod-level
// `allow` covers that remainder honestly.
#[allow(dead_code)]
pub(crate) mod mutate;
#[allow(dead_code)]
pub(crate) mod relation;
pub(crate) mod verify;
