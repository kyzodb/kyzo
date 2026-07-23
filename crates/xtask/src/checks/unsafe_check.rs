/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The unsafe law, ported from `scripts/check-unsafe.sh` (story #322): both
//! engine crates (kyzo-core, kyzo-bin — the language bindings are exempt,
//! unsafe FFI is what a binding is) must forbid unsafe with zero exceptions.
//!
//! Three checks per crate, same as the condemned script:
//!   1. the crate root declares `#![forbid(unsafe_code)]`;
//!   2. no `allow(unsafe_code)` attribute appears anywhere in its src tree;
//!   3. the docs do not claim an unsafe exception that does not exist.

use std::fmt;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use walkdir::WalkDir;

/// (crate root file, crate src dir), same pair the script checked, in the
/// same order.
const ENGINE_CRATES: &[(&str, &str)] = &[
    ("crates/kyzo-bin/src/main.rs", "crates/kyzo-bin/src"),
    ("crates/kyzo-core/src/lib.rs", "crates/kyzo-core/src"),
];

/// A real attribute line only — not prose comments that mention the
/// attribute. `(?m)` makes `^` match at each line start, matching grep's
/// per-line semantics without pre-splitting the file ourselves.
fn static_regex(pattern: &'static str) -> Regex {
    match Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => std::panic::resume_unwind(Box::new(format!("static regex `{pattern}`: {e}"))),
    }
}

static ALLOW_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| static_regex(r"(?m)^[[:space:]]*#!?\[allow\(unsafe_code\)\]"));

/// Docs that claim an unsafe exception that does not exist — a lying guard
/// is worse than no guard.
static LIAR_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    static_regex(
        r"(?i)germanstr[^a-z]*unsafe|unsafe[- ]exception|reviewed exception|Miri-audited exception",
    )
});

#[derive(Debug)]
pub enum UnsafeCheckError {
    RepoRoot(anyhow::Error),
    Io(anyhow::Error),
    MissingForbid { root: String },
    AllowFound { offenders: Vec<String> },
    LyingDoc { offenders: Vec<String> },
}

impl fmt::Display for UnsafeCheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnsafeCheckError::RepoRoot(e) => write!(f, "could not locate workspace root: {e:#}"),
            UnsafeCheckError::Io(e) => write!(f, "unsafe gate: {e:#}"),
            UnsafeCheckError::MissingForbid { root } => write!(
                f,
                "unsafe gate: {root} does not declare #![forbid(unsafe_code)]. First-party engine code forbids unsafe with zero exceptions; removing forbid is a reviewed, in-story decision, not an edit."
            ),
            UnsafeCheckError::AllowFound { offenders } => write!(
                f,
                "unsafe gate: allow(unsafe_code) found in the forbid-governed surface: {}. There is no unsafe exception. A new one must be introduced deliberately in its own story with a full safety case.",
                offenders.join(" ")
            ),
            UnsafeCheckError::LyingDoc { offenders } => write!(
                f,
                "unsafe gate: claims an unsafe exception that does not exist: {}. The value plane is pure safe Rust. Delete the phantom exception language so the docs match the enforced rule.",
                offenders.join(" ")
            ),
        }
    }
}

impl std::error::Error for UnsafeCheckError {}

pub fn check() -> Result<String, UnsafeCheckError> {
    let repo_root = crate::fsutil::repo_root().map_err(UnsafeCheckError::RepoRoot)?;
    check_at(&repo_root)
}

fn check_at(repo_root: &Path) -> Result<String, UnsafeCheckError> {
    if !repo_root.join("Cargo.toml").is_file() {
        return Ok("unsafe gate: no Cargo workspace yet — armed but idle".to_string());
    }

    let mut checked = Vec::new();
    for (root_file, src_dir) in ENGINE_CRATES {
        let root_path = repo_root.join(root_file);
        if !root_path.is_file() {
            continue;
        }
        let root_text = std::fs::read_to_string(&root_path)
            .map_err(|e| UnsafeCheckError::Io(anyhow::anyhow!("reading {root_file}: {e}")))?;
        if !root_text.contains("#![forbid(unsafe_code)]") {
            return Err(UnsafeCheckError::MissingForbid {
                root: (*root_file).to_string(),
            });
        }

        let src_path = repo_root.join(src_dir);
        let mut allow_offenders = Vec::new();
        let mut liar_offenders = Vec::new();
        for entry in WalkDir::new(&src_path).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(path).map_err(|e| {
                UnsafeCheckError::Io(anyhow::anyhow!("reading {}: {e}", path.display()))
            })?;
            let rel = match path.strip_prefix(repo_root) {
                Ok(rel) => rel,
                Err(_) => path,
            }
            .to_string_lossy()
            .replace('\\', "/");
            if ALLOW_PATTERN.is_match(&text) {
                allow_offenders.push(rel.clone());
            }
            if LIAR_PATTERN.is_match(&text) {
                liar_offenders.push(rel);
            }
        }
        if !allow_offenders.is_empty() {
            allow_offenders.sort();
            return Err(UnsafeCheckError::AllowFound {
                offenders: allow_offenders,
            });
        }
        if !liar_offenders.is_empty() {
            liar_offenders.sort();
            return Err(UnsafeCheckError::LyingDoc {
                offenders: liar_offenders,
            });
        }
        checked.push((*root_file).to_string());
    }

    if checked.is_empty() {
        return Ok(
            "unsafe gate: workspace exists but no engine crate roots yet — armed but idle"
                .to_string(),
        );
    }

    Ok(format!(
        "unsafe gate: clean — both engine crates forbid unsafe with zero exceptions:{}",
        checked.iter().map(|c| format!(" {c}")).collect::<String>()
    ))
}
