/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! One gate run: walk the boundary, run every registered check, filter
//! sworn waivers, report. The [`Verdict`] returned here is the ONLY source
//! of the gate log and counts artifact; hooks/CI parse those artifacts and
//! never re-derive them.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::boundary::{Boundary, SourceFile};
use crate::engines::{Hit, graph, meta, shape};
use crate::policy::Policy;
use crate::registry::{Engine, Registry};
use crate::waiver::WaiverFile;

pub struct Verdict {
    /// `RESONANCE: PASS` or `RESONANCE: FAIL <check, …>` — frozen contract.
    pub header: String,
    /// One `FAIL file:line — construct — law` line per unconfessed hit,
    /// plus `SCOPE`/`stale` lines.
    pub report: String,
    /// `name:N … = TOTAL unconfessed` — frozen contract for the banner.
    pub counts_line: String,
    pub red: bool,
}

pub fn run(root: &Path, only: Option<&str>) -> Result<Verdict> {
    let waivers = WaiverFile::load(&root.join("crates/bs-detector/waivers.toml"))?;
    let bite_src = std::fs::read_to_string(root.join("crates/bs-detector/tests/bite_proofs.rs"))
        .with_context(|| "reading bite_proofs.rs — a gate without bite proofs may not run")?;
    let reg = Registry::load(&root.join("crates/bs-detector/checks.toml"), &waivers, &bite_src)?;
    let b = Boundary::walk(root)?;

    let mut report = String::new();
    let mut failed: Vec<String> = vec![];
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut raw_by_check: Vec<(String, Vec<Hit>)> = vec![];

    for check in &reg.checks {
        if let Some(o) = only {
            if o != check.name {
                continue;
            }
        }
        let scope = waivers.scope_for(&check.name);
        let in_scope = |file: &str| -> bool {
            match scope {
                None => true,
                Some(s) => s.scope.iter().any(|p| file.starts_with(p.as_str())),
            }
        };
        // Borrows, never clones: one parse per file per run, shared by
        // every check regardless of scope.
        let scoped: Vec<&SourceFile> = b
            .files
            .iter()
            .filter(|f| in_scope(&f.rel_path))
            .collect();

        let raw: Vec<Hit> = match check.engine {
            Engine::Shape => {
                let name = match &check.matcher {
                    Some(m) => m.as_str(),
                    None => check.name.as_str(),
                };
                match shape::matcher_by_name(name) {
                    Some(m) => scoped.iter().flat_map(|f| shape::run_matcher(m, f)).collect(),
                    None => bail!("check `{}`: matcher vanished post-load", check.name),
                }
            }
            Engine::Graph => match check.name.as_str() {
                "derive_bypass" => graph::derive_bypass(&scoped),
                "copy_detector" => graph::copy_detector(&scoped),
                "agreement_registry" => {
                    let reg_path = root.join("crates/xtask/agreements.toml");
                    let text = std::fs::read_to_string(&reg_path)
                        .with_context(|| "reading agreements.toml — a missing registry is drift, not a pass")?;
                    graph::agreement_registry(&scoped, &text)
                }
                other => bail!(
                    "graph check `{other}` registered but not wired — a registered check that cannot run is a coverage lie"
                ),
            },
            Engine::Behavior => bail!(
                "behavior check `{}` registered but v1 has no behavior engine",
                check.name
            ),
            Engine::Meta => match check.name.as_str() {
                "coverage" => meta::coverage(&b),
                "unparsed" => meta::unparsed(&b),
                "forbid_roots" => meta::forbid_roots(&b),
                other => bail!("meta check `{other}` registered but not wired"),
            },
        };

        raw_by_check.push((check.name.clone(), raw.clone()));

        let unconfessed: Vec<&Hit> = match check.policy {
            Policy::HardBan => raw.iter().collect(),
            Policy::SwornWaiver => {
                // One waiver confesses exactly ONE hit. Two violations of
                // the same construct sharing a line coordinate cannot ride
                // one confession — the duplicate-site refusal in
                // WaiverFile::load means the second occurrence has no
                // representable waiver and stays red until FIXED.
                let sworn = waivers.for_check(&check.name);
                let mut consumed = vec![false; sworn.len()];
                raw.iter()
                    .filter(|h| {
                        let found = sworn.iter().enumerate().find(|(i, w)| {
                            !consumed[*i]
                                && w.file == h.file
                                && w.line == h.line
                                && w.construct == h.construct
                        });
                        match found {
                            Some((i, _)) => {
                                consumed[i] = true;
                                false
                            }
                            None => true,
                        }
                    })
                    .collect()
            }
        };

        if !unconfessed.is_empty() {
            failed.push(check.name.clone());
        }
        counts.insert(check.name.clone(), unconfessed.len());
        for h in &unconfessed {
            report.push_str(&format!(
                "FAIL {}:{} — {} — {}\n",
                h.file, h.line, h.construct, check.law
            ));
        }
    }

    if only.is_none() {
        let stale = meta::stale_waivers(&waivers, &raw_by_check);
        if !stale.is_empty() {
            failed.push("stale_waivers".to_string());
            counts.insert("stale_waivers".to_string(), stale.len());
            for h in &stale {
                report.push_str(&format!("FAIL {}:{} — {}\n", h.file, h.line, h.construct));
            }
        }
    }
    for s in &waivers.scope_waiver {
        report.push_str(&format!(
            "SCOPE {} limited to {:?} — sworn: {}\n",
            s.check, s.scope, s.why_full_scope_breaks
        ));
    }

    let total: usize = counts.values().sum();
    let red = !failed.is_empty();
    let header = if red {
        format!("RESONANCE: FAIL {}", failed.join(", "))
    } else {
        "RESONANCE: PASS".to_string()
    };
    let counts_line = {
        let mut parts: Vec<(&String, &usize)> = counts.iter().filter(|(_, n)| **n > 0).collect();
        parts.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let body: Vec<String> = parts.iter().map(|(k, v)| format!("{k}:{v}")).collect();
        format!("{} = {total} unconfessed", body.join(" "))
    };

    Ok(Verdict {
        header,
        report,
        counts_line,
        red,
    })
}
