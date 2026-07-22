/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Shared tree-walking: every check operates over the same notion of "the
//! engine source tree" so a bite-proof run against a throwaway copy sees
//! exactly the files a real CI run would.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// One parsed source file: its path (relative to `root`), the raw text (for
/// line lookups), and its `syn` AST.
pub struct SourceFile {
    /// Repo-root-relative path, e.g. `crates/kyzo-core/src/data/tuple.rs`. Stable
    /// across a bite-proof's throwaway rsync copy, so allowlist entries
    /// (which cite this form) still resolve there.
    pub rel_path: String,
    pub text: String,
    pub ast: syn::File,
}

/// Every `.rs` file under every first-party workspace crate that is not the
/// gate's own tooling. Widened from the original three (`kyzo-core`,
/// `kyzo-bin`, `kyzo-model`) after an audit found this list undisclosed and
/// three real crates — `kyzo-trials` (the DST/crash-testing harness that is
/// supposed to *prove* crash safety), `kyzo-oracle` (the independent
/// `::verify` judge), and `kyzo-crashfs` (the fault-injection layer) —
/// completely invisible to every resonance check. `xtask` itself is
/// deliberately excluded: its own test fixtures (`DETONATIONS` tables etc.)
/// contain banned-shape substrings as intentional string-literal samples,
/// which the line-based matchers cannot distinguish from a live occurrence —
/// scanning it would self-trigger on its own proof data, not find a real
/// defect. That gap is named here, not silently dropped.
pub fn walk_engine_sources(root: &Path) -> Result<Vec<SourceFile>> {
    let mut out = Vec::new();
    for crate_dir in [
        "crates/kyzo-core/src",
        "crates/kyzo-bin/src",
        "crates/kyzo-model/src",
        "crates/kyzo-trials/src",
        "crates/kyzo-oracle/src",
        "crates/kyzo-crashfs/src",
        "crates/kyzo-lsp/src",
        "crates/kyzo-arrow-interop/src",
    ] {
        let abs = root.join(crate_dir);
        if !abs.exists() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&abs)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let ast = syn::parse_file(&text)
                .with_context(|| format!("parsing {} as Rust", path.display()))?;
            let rel_path = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push(SourceFile {
                rel_path,
                text,
                ast,
            });
        }
    }
    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(out)
}

/// Load one repo-root-relative `.rs` file for checks that cite seats outside
/// the engine walk (`kyzo-oracle`, `kyzo-trials`, …).
pub fn load_source_file(root: &Path, rel: &str) -> Result<SourceFile> {
    let rel = rel.trim_start_matches("./");
    let path = root.join(rel);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let ast =
        syn::parse_file(&text).with_context(|| format!("parsing {} as Rust", path.display()))?;
    Ok(SourceFile {
        rel_path: rel.replace('\\', "/"),
        text,
        ast,
    })
}

/// Byte offset -> 1-based line number, for reporting `file:line` against a
/// `proc_macro2::Span` (which `syn`, run outside a proc-macro, only gives us
/// as a line/column pair already — kept here as the one place that maps a
/// span to the line-number convention the rest of the tool reports in).
pub fn span_line(span: &proc_macro2::Span) -> usize {
    span.start().line
}

pub fn repo_root() -> Result<PathBuf> {
    // Overridable so bite-proofs can point at a throwaway copy.
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
