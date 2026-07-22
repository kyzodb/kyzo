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
    /// The BS-detector register: the one and only way a banned shape is legal.
    #[serde(default)]
    pub bs_detector: Vec<BsDetectorEntry>,
    /// The T17 naked-array-signature register: the one and only way a
    /// crypto/auth door's naked `[u8; 32]`/`[u8; 12]`/`[u8; 64]` is legal
    /// (a documented RustCrypto-primitive-edge boundary function).
    #[serde(default)]
    pub naked_array_sig: Vec<NakedArraySigEntry>,
}

/// One confessed occurrence of a banned shape. There is no baseline — this
/// register is the complete list of every occurrence permitted to exist, and
/// each entry must answer, in writing, why that exact site is not sabotage.
#[derive(Debug, Deserialize)]
pub struct BsDetectorEntry {
    /// The `BANNED` pattern name (e.g. `unwrap`, `catchall_arm`).
    pub pattern: String,
    /// Repo-root-relative file the occurrence is in.
    pub file: String,
    /// The exact source line of the occurrence.
    pub line: usize,
    /// The audit confession — the `WHY THIS ISN'T SABOTAGE:` line. Mandatory,
    /// and rejected at load if it is not a real written justification.
    pub why_not_sabotage: String,
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

/// One confessed naked-array crypto/auth door — the T17 guardian
/// (`checks::naked_array_sig`). Keyed by function name + exact line, same
/// specificity law as [`PanicLintEntry`]: a waiver keyed by function name
/// alone would blanket-cover any future naked array added next to an
/// already-waived, provably-safe RustCrypto edge call.
#[derive(Debug, Deserialize)]
pub struct NakedArraySigEntry {
    pub file: String,
    pub function: String,
    pub line: usize,
    pub citation: String,
}

fn is_cited(s: &str) -> bool {
    // A citation is an issue reference or a real sentence — reject empty/
    // whitespace-only/placeholder text so the "every entry cites" rule is
    // enforced on the allowlist itself, not just on the source tree.
    s.trim().len() >= 8
}

/// Deferral / scope-park prose is Spec theater: it names a second HOW
/// ("consolidate later") while the gate stays green. Banned in citations.
fn is_deferral_theater(s: &str) -> bool {
    let t = s.to_ascii_lowercase();
    const BANNED: &[&str] = &[
        "outside this task",
        "outside this story",
        "outside this task's scope",
        "outside this story's file scope",
        "future story",
        "deferred only with a story",
        "design choice outside",
        "a design choice for a future",
        "not this story's file scope",
    ];
    BANNED.iter().any(|p| t.contains(p))
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
        if is_deferral_theater(&e.citation) {
            bail!(
                "resonance-allow.toml: copy_detector entry {:?} citation is deferral theater \
                 (outside-task / future-story / deferred-consolidate) — consolidate the twin \
                 or cite a sealed independence law, never park the HOW",
                e.members
            );
        }
    }
    for e in &list.bs_detector {
        if !is_cited(&e.why_not_sabotage) {
            bail!(
                "resonance-allow.toml: bs_detector entry {} {}:{} has no written \
                 `why_not_sabotage` confession — every banned shape that survives must \
                 answer why this exact site is not sabotage",
                e.pattern,
                e.file,
                e.line
            );
        }
    }
    for e in &list.naked_array_sig {
        if !is_cited(&e.citation) {
            bail!(
                "resonance-allow.toml: naked_array_sig entry `{}` {}:{} has no real citation",
                e.function,
                e.file,
                e.line
            );
        }
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact deferral-theater prose caught live in this tree's
    /// copy_detector allowlist (sparse put/del, neg.rs closures) — citations
    /// that park the HOW instead of doing it or naming a sealed law. Every
    /// one of these must be rejected, not just theoretical phrasing.
    #[test]
    fn deferral_theater_citations_are_rejected() {
        let live_examples = [
            "Consolidating would need a per-operation callback, a design choice outside this task's scope.",
            "a visitor trait — deferred to a future story",
            "not this story's file scope to refactor",
        ];
        for c in live_examples {
            assert!(
                is_deferral_theater(c),
                "must reject deferral theater: {c:?}"
            );
        }
    }

    /// A genuine sealed-law independence citation — the honest alternative
    /// to deferral theater — must not be rejected merely for citing scope or
    /// architecture; only the specific park-the-HOW phrasings are banned.
    #[test]
    fn honest_independence_citations_pass() {
        let honest = [
            "Per-engine index metadata twins (LSH vs sparse): each index owns its \
             StoredRelationMetadata shape under the shared projection architecture \
             (.claude/rules/engines.md). Collapsing would erase per-engine schema seats.",
            "SEAT-59: closed discriminant tables for CrossingKind, SealedArtifactKind, \
             and Tag. Each enum owns its wire tags; independence is the per-enum tag set.",
        ];
        for c in honest {
            assert!(
                !is_deferral_theater(c),
                "honest independence citation wrongly flagged as theater: {c:?}"
            );
        }
    }

    /// `is_cited` rejects empty, whitespace-only, and too-short placeholder
    /// text — the citation-exists check that theater-rejection layers on.
    #[test]
    fn uncited_placeholders_are_rejected() {
        assert!(!is_cited(""));
        assert!(!is_cited("   "));
        assert!(!is_cited("n/a"));
        assert!(is_cited("SEAT-59: closed discriminant tables."));
    }
}
