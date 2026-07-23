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
use std::fmt::Debug;

use kyzo::{Catalog, DataValue, Engine, FjallStorage, NamedRows, new_fjall_storage};

/// No query parameters — the common case.
pub fn no_params() -> BTreeMap<String, DataValue> {
    BTreeMap::new()
}

/// Loud test door: Option/Result must be inhabited or the fixture is broken.
#[cfg(test)]
fn must<T, E: Debug>(r: Result<T, E>, door: &'static str) -> T {
    match r {
        Ok(v) => v,
        Err(e) => {
            assert!(false, "{door}: {e:?}");
            loop {}
        }
    }
}

#[cfg(test)]
fn must_some<T>(o: Option<T>, door: &'static str) -> T {
    match o {
        Some(v) => v,
        None => {
            assert!(false, "{door}");
            loop {}
        }
    }
}

/// A fresh, real fjall-backed store. Leaks its tempdir on purpose (an
/// `#[test]` process is short-lived and every test needs its own store
/// torn down only at exit, not mid-run) — the same choice
/// `examples/language_tour.rs` makes.
#[cfg(test)]
pub fn fresh_db() -> Engine<FjallStorage> {
    let dir = must(tempfile::tempdir(), "tempdir");
    let storage = must(new_fjall_storage(dir.path()), "fjall storage");
    std::mem::forget(dir);
    must(Engine::compose(storage, Catalog::new()), "engine")
}

/// Every row's column `col` as an `i64` — refuses (loud) if any row's cell
/// isn't an int, which is exactly what we want from a test that already knows
/// its own schema.
#[cfg(test)]
pub fn ints(rows: &NamedRows, col: usize) -> Vec<i64> {
    rows.rows()
        .iter()
        .map(|r| must_some(r[col].get_int(), "row col not an int"))
        .collect()
}

#[cfg(test)]
pub fn floats(rows: &NamedRows, col: usize) -> Vec<f64> {
    rows.rows()
        .iter()
        .map(|r| must_some(r[col].get_float(), "row col not a float"))
        .collect()
}

#[cfg(test)]
pub fn strs(rows: &NamedRows, col: usize) -> Vec<String> {
    rows.rows()
        .iter()
        .map(|r| must_some(r[col].get_str(), "row col not a string").to_string())
        .collect()
}

#[cfg(test)]
pub fn bools(rows: &NamedRows, col: usize) -> Vec<bool> {
    rows.rows()
        .iter()
        .map(|r| must_some(r[col].get_bool(), "row col not a bool"))
        .collect()
}
