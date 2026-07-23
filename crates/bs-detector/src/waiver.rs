/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Sworn testimony: the ONLY way a detector hit survives without a fix.
//!
//! A waiver binds to exactly one site (check, file, line, construct) and
//! carries a `why_not_sabotage` the operator audits — earned by argument,
//! never by incumbency. The moment its site drifts, the waiver does not
//! quietly keep counting as confessed: it becomes a reported violation of
//! its own (BANNED #22). Blanket confessions are not representable: there
//! is no pattern-wide or file-wide waiver constructor.
//!
//! File format: `crates/bs-detector/waivers.toml`, one `[[waiver]]` table
//! per confession, plus `[[scope_waiver]]` for the single lawful way a
//! check runs narrower than all of `crates/` (BANNED #20 made data).

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// One sworn, site-bound confession.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Waiver {
    /// The registered check this confesses a hit of (must exist in
    /// checks.toml — a waiver naming an unknown check is a load error).
    pub check: String,
    /// Repo-root-relative file of the confessed site.
    pub file: String,
    /// 1-indexed line of the confessed site.
    pub line: usize,
    /// The construct at the site, exactly as the check reports it (pattern
    /// name for shape checks, function/member name for graph checks).
    pub construct: String,
    /// The testimony. Validated substantive at load; audited by the
    /// operator; a lie here is the highest-severity fraud in this repo.
    pub why_not_sabotage: String,
}

/// The single lawful narrow scope: a check whose boundary is less than all
/// of `crates/` must carry one of these, stating why full scope BREAKS the
/// check (not why it's inconvenient). The meta engine prints every one on
/// every run — narrow scope is always visible, never ambient.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeWaiver {
    pub check: String,
    /// Path prefixes (repo-root-relative) the check is limited to.
    pub scope: Vec<String>,
    /// Why the full boundary would break us — sworn, audited.
    pub why_full_scope_breaks: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaiverFile {
    /// Both ledgers are REQUIRED keys: an empty ledger is written
    /// `waiver = []`, stated on purpose — zero confessions is a claim the
    /// file makes, never a shape it defaults into.
    pub waiver: Vec<Waiver>,
    pub scope_waiver: Vec<ScopeWaiver>,
}

impl WaiverFile {
    /// Load and validate. Refuses: empty/uncited testimony, duplicate
    /// site entries (one confession per site — a duplicate means someone
    /// is padding), and unknown fields (a typo'd field name silently
    /// weakening a waiver is itself a lie-shape).
    pub fn load(path: &Path) -> Result<WaiverFile> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let parsed: WaiverFile =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

        let mut seen: BTreeSet<(String, String, usize, String)> = BTreeSet::new();
        for w in &parsed.waiver {
            if w.why_not_sabotage.trim().len() < 20 {
                bail!(
                    "waiver {}:{} ({}) — testimony under 20 chars is not testimony",
                    w.file,
                    w.line,
                    w.construct
                );
            }
            let key = (
                w.check.clone(),
                w.file.clone(),
                w.line,
                w.construct.clone(),
            );
            if !seen.insert(key) {
                bail!(
                    "duplicate waiver for {}:{} ({}) — one confession per site",
                    w.file,
                    w.line,
                    w.construct
                );
            }
        }
        for s in &parsed.scope_waiver {
            if s.why_full_scope_breaks.trim().len() < 20 {
                bail!(
                    "scope_waiver for `{}` — 'why full scope breaks' under 20 chars is not an argument",
                    s.check
                );
            }
            if s.scope.is_empty() {
                bail!(
                    "scope_waiver for `{}` with an empty scope list — that's a delete, not a waiver",
                    s.check
                );
            }
        }
        Ok(parsed)
    }

    /// The waivers sworn for one check.
    pub fn for_check<'a>(&'a self, check: &str) -> Vec<&'a Waiver> {
        self.waiver.iter().filter(|w| w.check == check).collect()
    }

    /// The scope waiver for one check, if any.
    pub fn scope_for<'a>(&'a self, check: &str) -> Option<&'a ScopeWaiver> {
        self.scope_waiver.iter().find(|s| s.check == check)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn load_str(s: &str) -> Result<WaiverFile> {
        let mut f = tempfile::NamedTempFile::new().expect("tempfile");
        f.write_all(s.as_bytes()).expect("write fixture");
        WaiverFile::load(f.path())
    }

    #[test]
    fn a_real_waiver_loads_and_binds_to_its_check() {
        let wf = load_str(
            r#"
scope_waiver = []

[[waiver]]
check = "expect"
file = "crates/kyzo-lsp/src/translate.rs"
line = 59
construct = "expect"
why_not_sabotage = "LSP Position.character is u32 by wire protocol; a >4B-unit line is outside the protocol's representable range."
"#,
        )
        .expect("loads");
        assert_eq!(wf.for_check("expect").len(), 1);
        assert!(wf.for_check("unwrap").is_empty());
    }

    #[test]
    fn refuses_thin_testimony() {
        let e = load_str(
            r#"
scope_waiver = []

[[waiver]]
check = "expect"
file = "a.rs"
line = 1
construct = "expect"
why_not_sabotage = "it's fine"
"#,
        );
        assert!(e.is_err(), "sub-20-char testimony must refuse to load");
    }

    #[test]
    fn refuses_duplicate_site() {
        let e = load_str(
            r#"
scope_waiver = []

[[waiver]]
check = "expect"
file = "a.rs"
line = 1
construct = "expect"
why_not_sabotage = "a genuinely long enough testimony string here"

[[waiver]]
check = "expect"
file = "a.rs"
line = 1
construct = "expect"
why_not_sabotage = "a second confession for the same site is padding"
"#,
        );
        assert!(e.is_err(), "duplicate site must refuse to load");
    }

    #[test]
    fn refuses_unknown_fields_and_empty_scope() {
        assert!(
            load_str("scope_waiver = []\n[[waiver]]\ncheck='x'\nfile='a'\nline=1\nconstruct='c'\nwhy_not_sabotage='long enough testimony right here'\nbaseline=5\n").is_err(),
            "unknown field (e.g. a smuggled 'baseline') must refuse"
        );
        assert!(
            load_str("waiver = []\n[[scope_waiver]]\ncheck='x'\nscope=[]\nwhy_full_scope_breaks='long enough argument text here'\n").is_err(),
            "empty scope list must refuse"
        );
        assert!(
            load_str("waiver = []\n").is_err(),
            "an undeclared ledger key must refuse — zero is stated, never defaulted"
        );
    }
}
