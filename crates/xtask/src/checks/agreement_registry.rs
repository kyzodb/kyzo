/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Check 5: the world-model agreement-law registry (`crates/xtask/agreements.toml`)
//! enumerates every cross-file agreement-law test by name. This check does
//! not re-derive the taxonomy — it verifies the registry hasn't drifted
//! from the tree: every listed `test_fn` must still exist, as a `fn`, in
//! its listed `file`. A law that quietly stopped being tested (renamed,
//! deleted, moved to a different module without updating the registry) is
//! exactly the failure mode this check exists to catch — absence is red.
//!
//! Cited files may sit outside the engine walk (oracle / trials seats); those
//! are loaded from disk by path so a re-home does not require expanding every
//! resonance scan into the judge crates.

use std::path::Path;

use serde::Deserialize;

use crate::fsutil::{self, SourceFile};

#[derive(Debug, Deserialize)]
struct Registry {
    law: Vec<LawEntry>,
}

#[derive(Debug, Deserialize)]
pub struct LawEntry {
    pub name: String,
    pub file: String,
    pub test_fn: String,
}

pub fn load(root: &std::path::Path) -> anyhow::Result<Vec<LawEntry>> {
    let path = root.join("crates/xtask/agreements.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    let reg: Registry =
        toml::from_str(&text).map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
    Ok(reg.law)
}

/// True if `text` contains a `fn <name>` declaration for exactly `name` —
/// word-bounded, so `differential_transitive_closure` does not spuriously
/// match `differential_transitive_closure_self_join`.
fn contains_fn(text: &str, name: &str) -> bool {
    let needle = format!("fn {name}");
    let mut start = 0;
    while let Some(pos) = text[start..].find(&needle) {
        let abs = start + pos;
        let after = abs + needle.len();
        let boundary_ok = text[after..]
            .chars()
            .next()
            .map(|c| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(true);
        if boundary_ok {
            return true;
        }
        start = abs + needle.len();
    }
    false
}

pub struct Violation {
    pub name: String,
    pub file: String,
    pub test_fn: String,
    pub reason: String,
}

pub fn check(files: &[SourceFile], registry: &[LawEntry], root: &Path) -> Vec<Violation> {
    let mut violations = Vec::new();
    for law in registry {
        let Some(text) = resolve_text(root, files, &law.file) else {
            violations.push(Violation {
                name: law.name.clone(),
                file: law.file.clone(),
                test_fn: law.test_fn.clone(),
                reason: "file no longer in the tree".to_string(),
            });
            continue;
        };
        if !contains_fn(&text, &law.test_fn) {
            violations.push(Violation {
                name: law.name.clone(),
                file: law.file.clone(),
                test_fn: law.test_fn.clone(),
                reason: format!("`fn {}` not found in {}", law.test_fn, law.file),
            });
        }
    }
    violations
}

fn resolve_text(root: &Path, files: &[SourceFile], law_file: &str) -> Option<String> {
    let needle = law_file.trim_start_matches("./");
    if let Some(f) = files.iter().find(|f| f.rel_path.ends_with(needle)) {
        return Some(f.text.clone());
    }
    fsutil::load_source_file(root, needle)
        .ok()
        .map(|f| f.text)
}
