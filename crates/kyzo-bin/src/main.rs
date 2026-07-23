/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0). Dropped the commented-out pre-axum `server_main` (dead code in
 * the original, `rouille`-based, superseded before this port ever started)
 * and the unused `extern crate core` (an `edition = "2018"`-ism the 2024
 * edition doesn't need). `exit(-1)` becomes `exit(1)` (the conventional
 * failure code — `-1` truncates to 255 on Unix, which reads as "unspecified
 * crash", not "the REPL reported an error"); the error is now printed via
 * `{e:?}` so miette's fancy `Debug` rendering (source span, help text) is
 * what a user sees, not the original's bare `Display` message.
 */

// Zero `unsafe` is a compiler guarantee here too, matching kyzo-core
// (`lib.rs`'s `#![forbid(unsafe_code)]`); `cargo xtask unsafe` checks
// for this attribute at both crate roots.
#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use env_logger::Env;

use crate::repl::{ReplArgs, repl_main};
use crate::server::{ServerArgs, server_main};

mod bulk;
mod engine;
mod repl;
mod server;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
struct AppArgs {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the HTTP API server.
    Server(ServerArgs),
    /// Run the interactive REPL.
    Repl(ReplArgs),
}

// Returning ExitCode instead of calling process::exit keeps every
// destructor on the unwind-free path alive — the runtime and storage
// handles drop before the process code is surrendered.
fn main() -> ExitCode {
    match AppArgs::parse().command {
        Commands::Server(args) => {
            env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    eprintln!("failed to build tokio runtime: {err}");
                    return ExitCode::FAILURE;
                }
            };
            if let Err(err) = runtime.block_on(server_main(args)) {
                eprintln!("{err:?}");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Commands::Repl(args) => {
            if let Err(e) = repl_main(args) {
                eprintln!("{e:?}");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
    }
}
