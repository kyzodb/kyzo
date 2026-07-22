/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Shared scaffolding for the end-to-end KyzoScript surface tests (story
//! #88): every test file in this directory is an EXTERNAL integration
//! crate, forced through the same public API a real embedder uses
//! (`kyzo::Engine`, `Engine::run_script`, `Engine::register_standing`,
//! `new_fjall_storage`) — no `pub(crate)` internals reachable from here,
//! by construction of where this file lives. A fresh store per test, real
//! `fjall` storage (not an in-memory stand-in), torn down only at process
//! exit — mirroring `examples/language_tour.rs`'s own fixture.


use std::collections::BTreeMap;

use kyzo::{Catalog, DataValue, Engine, FjallStorage, NamedRows, new_fjall_storage};

/// No query parameters — the common case.
pub fn no_params() -> BTreeMap<String, DataValue> {
    BTreeMap::new()
}

/// A fresh, real fjall-backed store. Leaks its tempdir on purpose (an
/// `#[test]` process is short-lived and every test needs its own store
/// torn down only at exit, not mid-run) — the same choice
/// `examples/language_tour.rs` makes.
pub fn fresh_db() -> Engine<FjallStorage> {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = new_fjall_storage(dir.path()).expect("fjall storage");
    std::mem::forget(dir);
    Engine::compose(storage, Catalog::new()).expect("engine")
}

/// Every row's column `col` as an `i64` — panics if any row's cell isn't
/// an int, which is exactly what we want from a test that already knows
/// its own schema.
pub fn ints(rows: &NamedRows, col: usize) -> Vec<i64> {
    rows.rows()
        .iter()
        .map(|r| {
            r[col]
                .get_int()
                .unwrap_or_else(|| {
                    assert!(false, "row {r:?} col {col} not an int");
                    0
                })
        })
        .collect()
}

pub fn floats(rows: &NamedRows, col: usize) -> Vec<f64> {
    rows.rows()
        .iter()
        .map(|r| {
            r[col]
                .get_float()
                .unwrap_or_else(|| {
                    assert!(false, "row {r:?} col {col} not a float");
                    0.0
                })
        })
        .collect()
}

pub fn strs(rows: &NamedRows, col: usize) -> Vec<String> {
    rows.rows()
        .iter()
        .map(|r| {
            r[col]
                .get_str()
                .unwrap_or_else(|| {
                    assert!(false, "row {r:?} col {col} not a string");
                    ""
                })
                .to_string()
        })
        .collect()
}

pub fn bools(rows: &NamedRows, col: usize) -> Vec<bool> {
    rows.rows()
        .iter()
        .map(|r| {
            r[col]
                .get_bool()
                .unwrap_or_else(|| {
                    assert!(false, "row {r:?} col {col} not a bool");
                    false
                })
        })
        .collect()
}
