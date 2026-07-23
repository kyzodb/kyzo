/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Workspace-root discovery — the one fsutil surface still owned by
//! xtask's remaining verbs. Tree scanning moved wholesale to the
//! bs-detector crate's Boundary.

use std::path::PathBuf;

use anyhow::{Context, Result};

pub fn repo_root() -> Result<PathBuf> {
    // Overridable so tooling can point at a throwaway copy.
    if let Ok(r) = std::env::var("RESONANCE_ROOT") {
        return Ok(PathBuf::from(r));
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .context("CARGO_MANIFEST_DIR not set (run via `cargo run -p xtask`)")?;
    // Walk up from xtask's own manifest dir to the real workspace root
    // (marked by a root `Cargo.toml` containing a `[workspace]` table),
    // rather than assuming a fixed nesting depth: the crates/ move put
    // xtask two levels below the root instead of one, and a hardcoded
    // `.parent()` silently pointed at `crates/` instead — "no source files
    // found" on CI. Walking up to the marker survives the next move too.
    let mut dir = PathBuf::from(manifest_dir);
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.is_file() {
            let text = std::fs::read_to_string(&candidate)
                .with_context(|| format!("reading {}", candidate.display()))?;
            if text.contains("[workspace]") {
                return Ok(dir);
            }
        }
        if !dir.pop() {
            return Err(anyhow::anyhow!(
                "no workspace root (Cargo.toml with [workspace]) found above xtask's manifest dir"
            ));
        }
    }
}
