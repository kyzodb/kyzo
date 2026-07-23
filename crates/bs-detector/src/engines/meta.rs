/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Engine 4 — Meta: the detector policing itself. Always-on:
//! - coverage: a run must prove it saw every file in the boundary.
//! - unparsed: a file syn can't read is invisible to every syn check, so
//!   it is reported, never skipped.
//! - stale waivers: a confession whose site no longer matches is itself a
//!   violation (BANNED #22).
//! - forbid roots: every crate root carries `#![forbid(unsafe_code)]`.

use crate::boundary::Boundary;
use crate::engines::Hit;
use crate::waiver::WaiverFile;

pub fn coverage(b: &Boundary) -> Vec<Hit> {
    b.coverage_proof()
        .missing
        .into_iter()
        .map(|f| Hit {
            file: f,
            line: 0,
            construct: "coverage_hole".to_string(),
        })
        .collect()
}

pub fn unparsed(b: &Boundary) -> Vec<Hit> {
    b.unparsed
        .iter()
        .map(|u| Hit {
            file: u.rel_path.clone(),
            line: 0,
            construct: format!("unparseable: {}", u.error),
        })
        .collect()
}

/// Waivers whose site no longer matches any raw hit of their check. The
/// caller supplies every check's raw (pre-waiver) hits.
pub fn stale_waivers(waivers: &WaiverFile, raw_hits: &[(String, Vec<Hit>)]) -> Vec<Hit> {
    let mut stale = vec![];
    for w in &waivers.waiver {
        let live = raw_hits.iter().any(|(check, hits)| {
            *check == w.check
                && hits
                    .iter()
                    .any(|h| h.file == w.file && h.line == w.line && h.construct == w.construct)
        });
        if !live {
            stale.push(Hit {
                file: w.file.clone(),
                line: w.line,
                construct: format!("stale_waiver:{}:{}", w.check, w.construct),
            });
        }
    }
    stale
}

/// Every crate root under `crates/` must forbid unsafe at the root.
pub fn forbid_roots(b: &Boundary) -> Vec<Hit> {
    let mut hits = vec![];
    for f in &b.files {
        let is_root = f.rel_path.ends_with("/src/lib.rs") || f.rel_path.ends_with("/src/main.rs");
        if is_root && !f.text.contains("#![forbid(unsafe_code)]") {
            hits.push(Hit {
                file: f.rel_path.clone(),
                line: 1,
                construct: "root_without_forbid_unsafe".to_string(),
            });
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boundary::SourceFile;

    #[test]
    fn stale_waiver_is_reported_when_its_site_dies() {
        let toml = r#"
scope_waiver = []

[[waiver]]
check = "expect"
file = "crates/x/src/a.rs"
line = 3
construct = "expect"
why_not_sabotage = "a genuinely long testimony for the fixture"
"#;
        let mut tf = tempfile::NamedTempFile::new().expect("tempfile");
        std::io::Write::write_all(&mut tf, toml.as_bytes()).expect("write");
        let wf = WaiverFile::load(tf.path()).expect("loads");

        let live = vec![(
            "expect".to_string(),
            vec![Hit {
                file: "crates/x/src/a.rs".to_string(),
                line: 3,
                construct: "expect".to_string(),
            }],
        )];
        assert!(stale_waivers(&wf, &live).is_empty(), "matching site = not stale");
        let drifted: Vec<(String, Vec<Hit>)> = vec![("expect".to_string(), vec![])];
        assert_eq!(stale_waivers(&wf, &drifted).len(), 1, "dead site = reported");
    }

    #[test]
    fn root_without_forbid_detonates() {
        let b = Boundary {
            files: vec![SourceFile {
                rel_path: "crates/x/src/lib.rs".to_string(),
                text: "pub fn f() {}\n".to_string(),
                ast: syn::parse_file("pub fn f() {}\n").expect("parses"),
            }],
            unparsed: vec![],
            existing: vec!["crates/x/src/lib.rs".to_string()],
        };
        assert_eq!(forbid_roots(&b).len(), 1);
    }
}
