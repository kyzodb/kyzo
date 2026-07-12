/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The resonance gate's one allowlist file: `resonance-allow.toml` at the
//! repo root. Every check consults it for the entries that mechanically
//! match its own violation shape; every entry must carry a citation (an
//! issue number or a plain-English reason) — an uncited entry is rejected
//! at load time, same as an uncited source-level violation would be.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize, Default)]
pub struct Allowlist {
    #[serde(default)]
    pub derive_bypass: Vec<DeriveBypassEntry>,
    #[serde(default)]
    pub panic_lint: Vec<PanicLintEntry>,
    #[serde(default)]
    pub copy_detector: Vec<CopyGroupEntry>,
    #[serde(default)]
    pub dead_code_ratchet: Vec<DeadCodeEntry>,
}

#[derive(Debug, Deserialize)]
pub struct DeriveBypassEntry {
    /// The type name as it appears in source (e.g. `Interval`).
    pub type_name: String,
    /// Repo-root-relative file the type is defined in.
    pub file: String,
    pub citation: String,
}

#[derive(Debug, Deserialize)]
pub struct PanicLintEntry {
    pub file: String,
    /// Function name as `Type::method` for impl methods, or a bare fn name.
    pub function: String,
    /// The exact source line of the waived occurrence. Required: a waiver
    /// keyed by function name alone would blanket-cover every future
    /// panic-shaped construct added anywhere else in that same function —
    /// exactly the shape that would hide a NEW `assert!` sitting next to
    /// an already-waived, provably-safe `.expect(...)`.
    pub line: usize,
    pub citation: String,
}

#[derive(Debug, Deserialize)]
pub struct CopyGroupEntry {
    /// Each member is `file::item` (item = fn name, or `Type::method`, or
    /// `enclosing_fn::closure` for a closure body).
    pub members: Vec<String>,
    pub citation: String,
}

#[derive(Debug, Deserialize)]
pub struct DeadCodeEntry {
    pub file: String,
    pub line: usize,
    pub citation: String,
}

fn is_cited(s: &str) -> bool {
    // A citation is an issue reference or a real sentence — reject empty/
    // whitespace-only/placeholder text so the "every entry cites" rule is
    // enforced on the allowlist itself, not just on the source tree.
    s.trim().len() >= 8
}

pub fn load(root: &Path) -> Result<Allowlist> {
    let path = root.join("resonance-allow.toml");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let list: Allowlist =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

    for e in &list.derive_bypass {
        if !is_cited(&e.citation) {
            bail!(
                "resonance-allow.toml: derive_bypass entry for `{}` has no real citation",
                e.type_name
            );
        }
    }
    for e in &list.panic_lint {
        if !is_cited(&e.citation) {
            bail!(
                "resonance-allow.toml: panic_lint entry for `{}::{}` has no real citation",
                e.file,
                e.function
            );
        }
    }
    for e in &list.copy_detector {
        if !is_cited(&e.citation) {
            bail!(
                "resonance-allow.toml: copy_detector entry {:?} has no real citation",
                e.members
            );
        }
    }
    for e in &list.dead_code_ratchet {
        if !is_cited(&e.citation) {
            bail!(
                "resonance-allow.toml: dead_code_ratchet entry {}:{} has no real citation",
                e.file,
                e.line
            );
        }
    }
    Ok(list)
}
