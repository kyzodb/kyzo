/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0). This file is based on code contributed by
 * https://github.com/rhn to the CozoDB original; that authorship is
 * preserved above and in the port's git history. The former flat
 * `repl.rs` is now this directory: line-editing (`editor`), `%`-command
 * dispatch (`commands`), and result rendering (`output`) are separate
 * concerns; this file owns only `ReplArgs` and the read-eval-render loop.
 * Behavior changes from the CozoDB original:
 *
 * - **Engine selection.** `--engine`/`--path`/`--config` (a free-form JSON
 *   blob almost nothing read) become `--engine`/`--path` plus the two typed
 *   `StorageArgs` knobs (`engine::StorageArgs`) — see `engine.rs`'s module
 *   doc for why there is one engine, not five.
 * - **No `%eval`.** Upstream's `evaluate_expressions` evaluated a bare
 *   expression (not a full query) against the parser's expression grammar.
 *   That entry point lives in kyzo-core's `parse` tier, which is
 *   `pub(crate)` (`lib.rs`: "the tiers between ... stay `pub(crate)` —
 *   internal organs, not API"); there is no public expression-only
 *   evaluator to call. Dropped rather than faked with a wrapper query,
 *   which would silently change its semantics (a query has an entry head
 *   and runs the fixpoint machinery; a bare expression does not).
 * - **`%backup`/`%restore` now mean a whole-store byte dump**
 *   (`kyzo::dump_storage`/`restore_storage`), not "write an sqlite backup
 *   file": kyzo-core has no sqlite dependency (the pure-Rust invariant), so
 *   the CozoDB original's sqlite-backed backup format has no target format
 *   to write into. The dump/restore pair is the direct analogue: the whole
 *   store, one file, one format version stamped in.
 * - **Ctrl-C no longer claims to kill a running query.** Upstream's
 *   handler ran `::kill $id` for every row of `::running`. In this port
 *   `::running` always returns zero rows and `::kill` refuses with
 *   `IndexOpNotLanded` (`kyzo-core/src/runtime/db.rs`): the live-query
 *   registry is runtime-tier work that has not landed. Rather than port a
 *   loop that iterates nothing and calls a script guaranteed to fail, the
 *   handler says plainly what it can't do yet.
 * - `%import`'s HTTP(S) handling moved to `commands.rs`; see that file and
 *   `client.rs` for the pure-Rust TLS story.
 */

mod commands;
mod editor;
mod output;

use std::collections::BTreeMap;

use clap::Args;
use kyzo::{DataValue, Db, FjallStorage};
use miette::{IntoDiagnostic, Result};
use rustyline::history::DefaultHistory;

use crate::engine::{self, StorageArgs};
use editor::Indented;

#[derive(Args, Debug)]
pub(crate) struct ReplArgs {
    /// Storage engine: `fjall` (persistent, at `--path`) or `mem`
    /// (ephemeral, discarded on exit). See `engine.rs`.
    #[clap(short, long, default_value = "mem")]
    engine: String,

    /// Path to the directory to store the database (only used by `fjall`).
    #[clap(short, long, default_value = "kyzo.db")]
    path: String,

    #[clap(flatten)]
    storage: StorageArgs,
}

pub(crate) fn repl_main(args: ReplArgs) -> Result<()> {
    let handle = engine::open(&args.engine, &args.path, args.storage)?;
    let db = handle.db;

    ctrlc::set_handler(|| {
        eprintln!(
            "Ctrl-C: no in-flight query can be cancelled yet (kyzo-core's `::kill` \
             is not landed — the live-query registry is unwritten runtime-tier work). \
             The current query keeps running to its own budget/deadline."
        );
    })
    .expect("Error setting Ctrl-C handler");

    println!("Welcome to the KyzoDB REPL.");
    println!("Type a space followed by newline to enter multiline mode.");

    let mut exit = false;
    let mut rl = rustyline::Editor::<Indented, DefaultHistory>::new().into_diagnostic()?;
    let mut params = BTreeMap::new();
    let mut save_next: Option<String> = None;
    rl.set_helper(Some(Indented));

    let history_file = ".kyzo_repl_history";
    if rl.load_history(history_file).is_ok() {
        println!("Loaded history from {history_file}");
    }

    loop {
        let readline = rl.readline("=> ");
        match readline {
            Ok(line) => {
                if let Err(err) =
                    process_line(&line, &db, &handle.storage, &mut params, &mut save_next)
                {
                    eprintln!("{err:?}");
                }
                if let Err(err) = rl.add_history_entry(line) {
                    eprintln!("{err:?}");
                }
                exit = false;
            }
            Err(rustyline::error::ReadlineError::Interrupted) => {
                if exit {
                    break;
                } else {
                    println!("Again to exit");
                    exit = true;
                }
            }
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(e) => eprintln!("{e:?}"),
        }
    }

    if rl.save_history(history_file).is_ok() {
        eprintln!("Query history saved in {history_file}");
    }
    Ok(())
}

/// One input line: a `%`-command (routed through `commands::dispatch`) or a
/// script handed straight to the engine. Either way, whatever rows come
/// back are rendered through `output::render`.
fn process_line(
    line: &str,
    db: &Db<FjallStorage>,
    storage: &FjallStorage,
    params: &mut BTreeMap<String, DataValue>,
    save_next: &mut Option<String>,
) -> Result<()> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(());
    }

    if let Some(remaining) = line.strip_prefix('%') {
        let remaining = remaining.trim();
        let (op, payload) = remaining
            .split_once(|c: char| c.is_whitespace())
            .unwrap_or((remaining, ""));
        if let Some(rows) = commands::dispatch(op, payload, db, storage, params, save_next)? {
            output::render(rows, save_next)?;
        }
    } else {
        let out = db.run_script(line, params.clone())?;
        output::render(out, save_next)?;
    }
    Ok(())
}
