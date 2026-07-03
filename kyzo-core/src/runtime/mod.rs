/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The runtime tier: the engine's execution substances. Today that is the
//! catalog ([`relation`]) and the semi-naive delta stores ([`temp_store`]);
//! the `db.rs` entrypoint (`run_query`, sessions, cooperative cancellation)
//! and the index operators land with the rest of the tier.

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
// The index-operator engines. `index` is shared plumbing (`hnsw` and
// `spatial_index` both construct IndexRowCorrupt), so it is live in the lib
// build; the engines' db.rs dispatch (::hnsw etc.) is a typed refusal until
// the operator ops land, and their in-file tests keep them live under test.
#[allow(dead_code)]
pub(crate) mod fts_index;
#[allow(dead_code)]
pub(crate) mod gazetteer;
#[cfg(test)]
mod gazetteer_hostile;
#[allow(dead_code)]
pub(crate) mod hnsw;
#[allow(dead_code)]
pub(crate) mod index;
#[allow(dead_code)]
pub(crate) mod minhash_lsh;
#[allow(dead_code)]
pub(crate) mod relation;
#[cfg(test)]
mod sparse_hostile;
#[allow(dead_code)]
pub(crate) mod sparse_index;
#[allow(dead_code)]
pub(crate) mod spatial_index;
#[allow(dead_code)]
pub(crate) mod temp_store;
