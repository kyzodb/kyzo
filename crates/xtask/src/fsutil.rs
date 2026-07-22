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
use syn::spanned::Spanned;

use crate::synutil::mod_is_test_scope;

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

/// Every `.rs` file under every first-party workspace crate, including the
/// gate's own tooling (`xtask` itself). Widened from the original three
/// (`kyzo-core`, `kyzo-bin`, `kyzo-model`) after an audit found this list
/// undisclosed and three real crates — `kyzo-trials` (the DST/crash-testing
/// harness that is supposed to *prove* crash safety), `kyzo-oracle` (the
/// independent `::verify` judge), and `kyzo-crashfs` (the fault-injection
/// layer) — completely invisible to every resonance check. A second audit
/// found `xtask` itself invisible too: its production checks (this file
/// included) were never scanned by their own law. `xtask`'s `#[cfg(test)]`
/// scopes hold `DETONATIONS`-style fixture tables whose string-literal
/// samples intentionally *quote* a banned shape as text data (never execute
/// it) — a line-based matcher cannot tell that from a live occurrence, so
/// those scopes are blanked out of both `text` and `ast` for `xtask` files
/// only (see [`strip_xtask_test_scope`]), the same `mod_is_test_scope`
/// mechanism `banned_path::scan_banned_idents` already uses to skip test
/// scaffolding — never a blanket crate-level skip, and never applied to any
/// other crate (whose test scopes stay in-law per bs_detector's own
/// no-blanket-test-exemption rule).
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
        "crates/xtask/src",
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
            let (text, ast) = if crate_dir == "crates/xtask/src" {
                strip_xtask_test_scope(&text, &ast)
                    .with_context(|| format!("stripping test scope from {}", path.display()))?
            } else {
                (text, ast)
            };
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

/// Blank every top-level-or-nested `#[cfg(test)]`/`mod tests` scope's lines
/// out of `text` (line count preserved — every surviving line's number is
/// unchanged) and reparse `ast` from the blanked text. `xtask`'s own
/// fixture/test-data modules (banned-shape string samples, ordinary
/// `.expect("fixture parses")` test idiom) are not production surface and
/// not a live occurrence of anything they quote — this is the one crate
/// where `walk_engine_sources` applies the exclusion, per its own doc.
fn strip_xtask_test_scope(text: &str, ast: &syn::File) -> Result<(String, syn::File)> {
    let mut ranges = Vec::new();
    collect_test_mod_line_ranges(&ast.items, &mut ranges);
    if ranges.is_empty() {
        return Ok((text.to_string(), syn::parse_file(text)?));
    }
    let mut lines: Vec<&str> = text.lines().collect();
    for (start, end) in ranges {
        let lo = start.saturating_sub(1);
        let hi = end.min(lines.len());
        for line in lines.iter_mut().take(hi).skip(lo) {
            *line = "";
        }
    }
    let blanked = lines.join("\n");
    let ast = syn::parse_file(&blanked).context("reparsing after blanking xtask test scope")?;
    Ok((blanked, ast))
}

/// Every `(start_line, end_line)` (1-based, inclusive) covered by a
/// `mod_is_test_scope` module, found at any nesting depth. A matched module
/// is not recursed into further — its whole span is already excluded.
fn collect_test_mod_line_ranges(items: &[syn::Item], out: &mut Vec<(usize, usize)>) {
    for item in items {
        let syn::Item::Mod(m) = item else { continue };
        if mod_is_test_scope(&m.ident, &m.attrs) {
            let span = m.span();
            out.push((span.start().line, span.end().line));
            continue;
        }
        if let Some((_, inner)) = &m.content {
            collect_test_mod_line_ranges(inner, out);
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_xtask_test_scope_blanks_cfg_test_mod_only() {
        let src = "fn prod() -> u32 { 1 }\n\
                    #[cfg(test)]\n\
                    mod tests {\n\
                    fn f() { let v = maybe.unwrap(); }\n\
                    }\n\
                    fn after() -> u32 { 2 }\n";
        let ast = syn::parse_file(src).expect("fixture parses");
        let (blanked, reparsed) = strip_xtask_test_scope(src, &ast).expect("strip succeeds");
        assert!(
            !blanked.contains(".unwrap()"),
            "the test-scope fixture body must be blanked: {blanked:?}"
        );
        assert!(blanked.contains("fn prod()"));
        assert!(blanked.contains("fn after()"));
        assert_eq!(
            blanked.lines().count(),
            src.lines().count(),
            "line count must be preserved so other line numbers stay accurate"
        );
        assert_eq!(
            reparsed.items.len(),
            2,
            "the blanked mod produces no items; only prod/after survive"
        );
    }

    #[test]
    fn strip_xtask_test_scope_ignores_bare_tests_ident_without_cfg_and_prod_untouched() {
        let src = "fn only_prod() -> u32 { 3 }\n";
        let ast = syn::parse_file(src).expect("fixture parses");
        let (blanked, _) = strip_xtask_test_scope(src, &ast).expect("strip succeeds");
        assert_eq!(
            blanked, src,
            "a file with no test-scope module is unchanged"
        );
    }
}
