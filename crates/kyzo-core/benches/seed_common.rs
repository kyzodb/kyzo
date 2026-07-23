/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Shared seeded-engine door for criterion benches (copy_detector — one seat).

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::path::Path;

use kyzo::{Catalog, DataValue, Engine, new_fjall_storage};

pub fn open_door<T, E: Debug>(r: Result<T, E>, door: &'static str) -> T {
    match r {
        Ok(v) => v,
        Err(e) => std::panic::resume_unwind(Box::new(format!("{door}: {e:?}"))),
    }
}

pub fn no_params() -> BTreeMap<String, DataValue> {
    BTreeMap::new()
}

/// Seed a fresh fjall engine with `n` rows via `:create {rel} {k => v}`.
/// `row` emits one `[k, v],` fragment per index.
pub fn seeded_db(
    n: u64,
    dir: &Path,
    rel: &str,
    mut row: impl FnMut(u64) -> String,
) -> Engine<kyzo::FjallStorage> {
    let storage = open_door(new_fjall_storage(dir), "storage");
    let db = open_door(Engine::compose(storage, Catalog::new()), "engine");
    let mut script = String::from("?[k, v] <- [");
    for i in 0..n {
        script.push_str(&row(i));
    }
    script.push_str(&format!("] :create {rel} {{k => v}}"));
    open_door(db.run_script(&script, no_params()), "seed");
    db
}
