/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The Bullshit Detector CLI — the one door, and the ONLY writer of the
//! gate log. Verdict contract (frozen; the Stop hook, the watcher, and CI
//! all parse it): line 1 of the log is `RESONANCE: PASS` or
//! `RESONANCE: FAIL <check, check, …>`; the counts artifact is one line,
//! `name:N …  = TOTAL unconfessed`. All detection lives in the library;
//! this binary only parses arguments, runs, prints, writes, and exits.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

#[derive(Parser)]
#[command(about = "the bullshit detector: every check, one door, zero baseline")]
struct Cli {
    /// Repo root (defaults to the working directory).
    #[arg(long, default_value = ".")]
    root: PathBuf,
    /// Run only this registered check.
    #[arg(long)]
    only: Option<String>,
    /// Gate log path (line 1 is the verdict header).
    #[arg(long, default_value = "crates/xtask/resonance.log")]
    log: PathBuf,
    /// Counts artifact path (the [BS] banner's source).
    #[arg(long, default_value = "crates/xtask/bs-counts.txt")]
    counts: PathBuf,
    /// Print findings without writing the log/counts artifacts.
    #[arg(long)]
    dry_run: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = cli
        .root
        .canonicalize()
        .with_context(|| format!("resolving root {}", cli.root.display()))?;

    let v = bs_detector::run::run(&root, cli.only.as_deref())?;

    println!("{}", v.header);
    print!("{}", v.report);
    println!("[BS] {}", v.counts_line);

    if !cli.dry_run {
        std::fs::write(root.join(&cli.log), format!("{}\n{}", v.header, v.report))
            .with_context(|| "writing gate log")?;
        std::fs::write(root.join(&cli.counts), format!("{}\n", v.counts_line))
            .with_context(|| "writing counts artifact")?;
    }

    if v.red {
        std::process::exit(1); // INVARIANT(GateVerdict): the gate's red exit IS the verdict for hooks/CI — a named, audited circuit breaker, not an error swallow.
    }
    Ok(())
}
