/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0, `cozo-bin/src/repl.rs`'s `%`-command arms). This file is based
 * on code contributed by https://github.com/rhn to the CozoDB original;
 * that authorship is preserved here. Split into one function per command
 * (the original had all nine inline in a single `match` arm of
 * `process_line`). Behavior changes from the original, beyond what
 * `repl/mod.rs`'s doc already covers (no `%eval`, dump/restore instead of
 * an sqlite backup file):
 *
 * - **`%import` now handles `https://` too.** Earlier in this port it
 *   refused `https://` outright (no pure-Rust TLS stack was wired yet);
 *   `client.rs` now provides one, so both schemes work.
 * - **An unrecognized `%foo` falls through to a named error**, not "hand
 *   the whole line, `%` included, to the script parser and let it produce
 *   a parse error about the `%`." Same outcome (the command is refused),
 *   clearer diagnostic.
 */

//! The `%`-prefixed REPL commands: session parameters (`set`/`unset`/
//! `clear`/`params`), whole-store dump/restore (`backup`/`restore`),
//! running a script file (`run`), deferring the next result to a file
//! (`save`), and importing relation data (`import`). [`dispatch`] is pure
//! routing; each command is its own function.

use std::collections::BTreeMap;
use std::fs;

use kyzo::{DataValue, Engine, FjallStorage, NamedRows};
use miette::{IntoDiagnostic, Result, bail, miette};
use serde_json::{Value, json};

/// Route one `%`-command (`op` is the word right after `%`, `payload` is
/// the rest of the line). `Ok(Some(rows))` means the caller
/// (`repl::process_line`) should render `rows`; `Ok(None)` means this
/// command already produced its own output, or none.
pub(super) fn dispatch(
    op: &str,
    payload: &str,
    db: &Engine<FjallStorage>,
    storage: &FjallStorage,
    params: &mut BTreeMap<String, DataValue>,
    save_next: &mut Option<String>,
) -> Result<Option<NamedRows>> {
    match op {
        "set" => set(payload, params).map(|()| None),
        "unset" => unset(payload, params).map(|()| None),
        "clear" => {
            params.clear();
            Ok(None)
        }
        "params" => print_params(params).map(|()| None),
        "backup" => backup(payload, storage).map(|()| None),
        "restore" => restore(payload, storage).map(|()| None),
        "save" => {
            save(payload, save_next);
            Ok(None)
        }
        "run" => run(payload, db, params).map(Some),
        "import" => import(payload, db).map(|()| None),
        other => bail!("unrecognized command '%{other}'"),
    }
}

fn set(payload: &str, params: &mut BTreeMap<String, DataValue>) -> Result<()> {
    let (key, v_str) = payload
        .trim()
        .split_once(|c: char| c.is_whitespace())
        .ok_or_else(|| miette!("Bad set syntax. Should be '%set <KEY> <VALUE>'."))?;
    let val: Value = serde_json::from_str(v_str).into_diagnostic()?;
    params.insert(key.to_string(), DataValue::from(&val));
    Ok(())
}

fn unset(payload: &str, params: &mut BTreeMap<String, DataValue>) -> Result<()> {
    let key = payload.trim();
    if params.remove(key).is_none() {
        bail!("Key not found: '{}'", key)
    }
    Ok(())
}

fn print_params(params: &BTreeMap<String, DataValue>) -> Result<()> {
    let as_json: serde_json::Map<_, _> = params
        .iter()
        .map(|(k, v)| Value::try_from(v).map(|j| (k.clone(), j)))
        .collect::<Result<_, _>>()
        .into_diagnostic()?;
    let display = serde_json::to_string_pretty(&json!(as_json)).into_diagnostic()?;
    println!("{display}");
    Ok(())
}

fn backup(payload: &str, storage: &FjallStorage) -> Result<()> {
    let path = payload.trim();
    if path.is_empty() {
        bail!("Backup requires a path");
    }
    kyzo::dump_storage(storage, path)?;
    println!("Backup written successfully to {path}");
    Ok(())
}

fn restore(payload: &str, storage: &FjallStorage) -> Result<()> {
    let path = payload.trim();
    if path.is_empty() {
        bail!("Restore requires a path");
    }
    kyzo::restore_storage(storage, path)?;
    println!("Backup successfully loaded from {path}");
    Ok(())
}

fn save(payload: &str, save_next: &mut Option<String>) {
    let next_path = payload.trim();
    if next_path.is_empty() {
        println!("Next result will NOT be saved to file");
    } else {
        println!("Next result will be saved to file: {next_path}");
        *save_next = Some(next_path.to_string());
    }
}

fn run(
    payload: &str,
    db: &Engine<FjallStorage>,
    params: &BTreeMap<String, DataValue>,
) -> Result<NamedRows> {
    let path = payload.trim();
    if path.is_empty() {
        bail!("Run requires path to a script");
    }
    let content = fs::read_to_string(path).into_diagnostic()?;
    db.run_script(&content, params.clone())
}

fn import(payload: &str, db: &Engine<FjallStorage>) -> Result<()> {
    let url = payload.trim();
    let content = if url.starts_with("http://") || url.starts_with("https://") {
        crate::repl::fetch::get(url)?
    } else {
        let file_path = url.strip_prefix("file://").unwrap_or(url);
        fs::read_to_string(file_path).into_diagnostic()?
    };
    let json_data: Value = serde_json::from_str(&content).into_diagnostic()?;
    let json_object = json_data
        .as_object()
        .ok_or_else(|| miette!("A JSON object is required"))?;
    let mapping = json_object
        .iter()
        .map(|(k, v)| -> Result<(String, NamedRows)> {
            Ok((k.to_string(), NamedRows::from_json(v)?))
        })
        .collect::<Result<_>>()?;
    crate::bulk::import_relations(db, mapping)?;
    println!("Imported data from {url}");
    Ok(())
}
