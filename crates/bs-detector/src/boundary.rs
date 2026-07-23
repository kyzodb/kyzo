/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The Boundary: what a check is entitled to see, and the proof it saw it.
//!
//! Standing operator law: every check's boundary is ALL of `crates/`.
//! No constructor here accepts a narrower path — a narrower scope exists
//! only as a `ScopeWaiver` in waivers.toml, printed on every run (BANNED
//! #20). [`CoverageProof`] records files-existing vs files-visited; any
//! gap is a red verdict.

use std::path::Path;

use anyhow::{Context, Result};
use proc_macro2::LineColumn;
use walkdir::WalkDir;

/// One parsed source file inside the boundary. The AST is parsed once here
/// and shared by every engine — a second parse authority would let two
/// checks disagree about what the file says.
pub struct SourceFile {
    /// Repo-root-relative path, e.g. `crates/kyzo-core/src/data/tuple.rs`.
    pub rel_path: String,
    pub text: String,
    pub ast: syn::File,
}

/// A file inside the boundary that refused to parse. Not silently skipped:
/// an unparseable file is invisible to every syn-based check, which is
/// exactly how a hidden violation would ride in — so the run reports it.
pub struct UnparsedFile {
    pub rel_path: String,
    pub error: String,
}

/// The whole boundary, walked once: every `.rs` file under `crates/`
/// (sources, integration tests, benches, examples — everything that is
/// code), excluding only build artifacts (`target/`).
pub struct Boundary {
    pub files: Vec<SourceFile>,
    pub unparsed: Vec<UnparsedFile>,
    /// Every `.rs` path that exists in the boundary right now, whether or
    /// not it parsed — the denominator of the coverage proof.
    pub existing: Vec<String>,
}

/// The proof a run saw the whole boundary: files that exist vs files the
/// walk delivered to the engines. `gap()` non-empty is a run-level red.
pub struct CoverageProof {
    pub existing: usize,
    pub visited: usize,
    pub missing: Vec<String>,
}

impl Boundary {
    /// Walk all of `crates/` under `root`. The only exclusion is `target/`
    /// build output — never a crate, never a directory of meaning.
    pub fn walk(root: &Path) -> Result<Boundary> {
        let crates_dir = root.join("crates");
        let mut files = Vec::new();
        let mut unparsed = Vec::new();
        let mut existing = Vec::new();

        for entry in WalkDir::new(&crates_dir)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|e| e.file_name() != "target")
        {
            let entry = entry.with_context(|| "walking crates/")?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().map(|x| x == "rs") != Some(true) {
                continue;
            }
            let rel = rel_path(root, path)?;
            existing.push(rel.clone());
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {rel}"))?;
            match syn::parse_file(&text) {
                Ok(ast) => files.push(SourceFile {
                    rel_path: rel,
                    text,
                    ast,
                }),
                Err(e) => unparsed.push(UnparsedFile {
                    rel_path: rel,
                    error: e.to_string(),
                }),
            }
        }
        Ok(Boundary {
            files,
            unparsed,
            existing,
        })
    }

    /// The coverage proof for this walk: every existing file is either a
    /// parsed [`SourceFile`] or a reported [`UnparsedFile`]; anything else
    /// is a hole the run must refuse over.
    pub fn coverage_proof(&self) -> CoverageProof {
        let mut visited: Vec<&str> = self
            .files
            .iter()
            .map(|f| f.rel_path.as_str())
            .chain(self.unparsed.iter().map(|u| u.rel_path.as_str()))
            .collect();
        visited.sort_unstable();
        let missing: Vec<String> = self
            .existing
            .iter()
            .filter(|e| visited.binary_search(&e.as_str()).is_err())
            .cloned()
            .collect();
        CoverageProof {
            existing: self.existing.len(),
            visited: visited.len(),
            missing,
        }
    }
}

fn rel_path(root: &Path, path: &Path) -> Result<String> {
    let rel = path
        .strip_prefix(root)
        .with_context(|| format!("{} is outside the repo root", path.display()))?;
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

/// The one line-number authority: every finding's line comes from the
/// span's start, matching what an editor shows.
pub fn span_line(span: &proc_macro2::Span) -> usize {
    let LineColumn { line, .. } = span.start();
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_covers_every_rs_file_and_proof_balances() {
        let dir = tempfile::tempdir().expect("tempdir for fixture tree");
        let root = dir.path();
        std::fs::create_dir_all(root.join("crates/a/src")).expect("mkdir a");
        std::fs::create_dir_all(root.join("crates/b/tests")).expect("mkdir b");
        std::fs::create_dir_all(root.join("crates/a/target/debug")).expect("mkdir target");
        std::fs::write(root.join("crates/a/src/lib.rs"), "pub fn f() {}\n").expect("write");
        std::fs::write(root.join("crates/b/tests/t.rs"), "#[test] fn t() {}\n").expect("write");
        std::fs::write(root.join("crates/a/src/broken.rs"), "fn {{{\n").expect("write");
        // target/ output must be invisible; everything else must be seen.
        std::fs::write(root.join("crates/a/target/debug/gen.rs"), "fn x() {}\n").expect("write");

        let b = Boundary::walk(root).expect("walk");
        assert_eq!(b.files.len(), 2, "lib.rs and tests/t.rs parse");
        assert_eq!(b.unparsed.len(), 1, "broken.rs is REPORTED, not skipped");
        assert_eq!(b.existing.len(), 3, "target/ is excluded from existence");
        let proof = b.coverage_proof();
        assert_eq!(proof.existing, 3);
        assert_eq!(proof.visited, 3);
        assert!(proof.missing.is_empty(), "no silent hole");
    }
}
