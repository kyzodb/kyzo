/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The registry: checks.toml parsed into typed [`Check`]s, or refused.
//!
//! A check is DATA — (lie-shape, engine, policy, bite-proof) — never a file
//! with its own loop. Scope lives in waivers.toml as a `[[scope_waiver]]`
//! (narrow = sworn + visible), so this file carries no scope field at all:
//! there is nothing here to quietly shrink. Load-time refusals, each one a
//! lie-shape made unconstructible:
//! - a check naming a matcher the shape engine doesn't export (typo'd
//!   detection = detection that silently never runs);
//! - a check without a registered bite-proof (a detector that was never
//!   proven to bite is theater);
//! - a waiver naming an unregistered check (confessing to nobody);
//! - a waiver naming a HardBan check (no testimony can exist for those);
//! - duplicate check names (second authority).

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::engines::shape;
use crate::policy::Policy;
use crate::waiver::WaiverFile;

/// Which engine executes a registered check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Engine {
    /// Site scan over each file's AST/text (BANNED.md shapes, symbol bans,
    /// signature bans).
    Shape,
    /// Computed property across files (near-duplicate bodies, derive
    /// bypass, registry drift).
    Graph,
    /// Execute-and-diff (build-script sandbox and friends) — invoked, not
    /// simulated.
    Behavior,
    /// Detector integrity: coverage proof, stale waivers, scope report.
    /// Always on; registered so it is visible, not so it is optional.
    Meta,
}

/// One registered check, straight from checks.toml.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    /// Identity; also the `check` key waivers bind to.
    pub name: String,
    pub engine: Engine,
    /// For Shape checks: the matcher this name resolves to in
    /// `engines::shape::MATCHERS`. Graph/Behavior/Meta checks are wired by
    /// name in their engines. (`Option` deserializes absent-as-None with no
    /// default attribute needed.)
    pub matcher: Option<String>,
    pub policy: Policy,
    /// One sentence: the lie this check exists to catch. Printed in
    /// reports so a red is always self-explanatory.
    pub law: String,
    /// The bite-proof test function name in tests/bite_proofs.rs proving
    /// this check detonates on its historical bug shape. Load cross-checks
    /// the name against the fns actually defined in the bite-proof source,
    /// so a stale or invented name refuses to register.
    pub bite_proof: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChecksFile {
    check: Vec<Check>,
}

pub struct Registry {
    pub checks: Vec<Check>,
}

impl Registry {
    /// `bite_src` is the full text of tests/bite_proofs.rs; every check's
    /// `bite_proof` must name a fn defined there or the registry refuses.
    pub fn load(checks_path: &Path, waivers: &WaiverFile, bite_src: &str) -> Result<Registry> {
        let text = std::fs::read_to_string(checks_path)
            .with_context(|| format!("reading {}", checks_path.display()))?;
        let parsed: ChecksFile =
            toml::from_str(&text).with_context(|| format!("parsing {}", checks_path.display()))?;
        let checks = parsed.check;
        let bite_fns = defined_fns(bite_src)?;

        let mut names: BTreeSet<&str> = BTreeSet::new();
        for c in &checks {
            if !names.insert(&c.name) {
                bail!("duplicate check `{}` — one name, one authority", c.name);
            }
            if c.law.trim().len() < 20 {
                bail!("check `{}` — `law` under 20 chars does not name a lie", c.name);
            }
            if c.bite_proof.trim().is_empty() {
                bail!(
                    "check `{}` has no bite_proof — a detector never proven to bite is theater",
                    c.name
                );
            }
            if !bite_fns.contains(c.bite_proof.trim()) {
                bail!(
                    "check `{}` names bite_proof `{}` but no #[test] fn of that name exists in the bite-proof source — a name that never runs under the suite proves nothing",
                    c.name,
                    c.bite_proof
                );
            }
            match c.engine {
                Engine::Shape => {
                    let m = c.matcher.as_deref().and_then(shape::matcher_by_name);
                    if m.is_none() {
                        bail!(
                            "check `{}` names shape matcher {:?} which the shape engine does not export",
                            c.name,
                            c.matcher
                        );
                    }
                }
                Engine::Graph | Engine::Behavior | Engine::Meta => {
                    if c.matcher.is_some() {
                        bail!(
                            "check `{}` sets `matcher` but is not a shape check — dead config is a lie about coverage",
                            c.name
                        );
                    }
                }
            }
        }

        for w in &waivers.waiver {
            match checks.iter().find(|c| c.name == w.check) {
                None => bail!(
                    "waiver {}:{} names unregistered check `{}` — confessing to nobody",
                    w.file,
                    w.line,
                    w.check
                ),
                Some(c) if c.policy == Policy::HardBan => bail!(
                    "waiver {}:{} names hard-ban check `{}` — no testimony can exist for a hard ban",
                    w.file,
                    w.line,
                    w.check
                ),
                Some(_) => {}
            }
        }
        for s in &waivers.scope_waiver {
            if !checks.iter().any(|c| c.name == s.check) {
                bail!(
                    "scope_waiver names unregistered check `{}` — narrowing nothing",
                    s.check
                );
            }
        }

        Ok(Registry { checks })
    }
}

/// Every `#[test]` fn name defined in the bite-proof source, by parsing it.
/// Two frauds refused here: a name inside a comment or string literal (a
/// `contains` scan would accept it), and a non-test helper fn cited as a
/// proof (it would never execute under the suite). What this cross-check
/// enforces is existence-as-a-test; DETONATION is enforced by the suite
/// itself running in the same gate (`cargo test -p bs-detector`) — a proof
/// that stops asserting fails there, red either way.
fn defined_fns(src: &str) -> Result<BTreeSet<String>> {
    use syn::visit::Visit;
    let ast = syn::parse_file(src)
        .map_err(|e| anyhow::anyhow!("bite-proof source does not parse: {e}"))?;
    struct V {
        fns: BTreeSet<String>,
    }
    impl<'ast> Visit<'ast> for V {
        fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
            let is_test = node.attrs.iter().any(|a| {
                a.path().is_ident("test")
                    || a.path().segments.last().is_some_and(|s| s.ident == "test")
            });
            if is_test {
                self.fns.insert(node.sig.ident.to_string());
            }
            syn::visit::visit_item_fn(self, node);
        }
    }
    let mut v = V {
        fns: BTreeSet::new(),
    };
    v.visit_file(&ast);
    Ok(v.fns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn load(checks: &str, waivers: &str) -> Result<Registry> {
        let mut cf = tempfile::NamedTempFile::new().expect("checks tempfile");
        cf.write_all(checks.as_bytes()).expect("write checks");
        let mut wf = tempfile::NamedTempFile::new().expect("waivers tempfile");
        wf.write_all(waivers.as_bytes()).expect("write waivers");
        let waivers = WaiverFile::load(wf.path())?;
        Registry::load(cf.path(), &waivers, BITE_SRC)
    }

    const GOOD: &str = r#"
[[check]]
name = "unwrap"
engine = "shape"
matcher = "unwrap"
policy = "sworn-waiver"
law = "a panic where a typed refusal is owed"
bite_proof = "unwrap_detonates_on_bare_unwrap"
"#;

    const BITE_SRC: &str = "#[test]\nfn unwrap_detonates_on_bare_unwrap() {}\n";

    const EMPTY_WAIVERS: &str = "waiver = []\nscope_waiver = []\n";

    #[test]
    fn a_lawful_check_registers() {
        let r = load(GOOD, EMPTY_WAIVERS).expect("registers");
        assert_eq!(r.checks.len(), 1);
    }

    #[test]
    fn refuses_unknown_matcher_and_missing_bite_proof() {
        let bad_matcher = GOOD.replace("matcher = \"unwrap\"", "matcher = \"unwarp\"");
        assert!(load(&bad_matcher, EMPTY_WAIVERS).is_err(), "typo'd matcher must refuse");
        let no_bite = GOOD.replace("bite_proof = \"unwrap_detonates_on_bare_unwrap\"", "bite_proof = \"\"");
        assert!(load(&no_bite, EMPTY_WAIVERS).is_err(), "biteless check must refuse");
    }

    #[test]
    fn refuses_bite_proof_naming_a_fn_that_does_not_exist() {
        let ghost = GOOD.replace("unwrap_detonates_on_bare_unwrap", "ghost_bite");
        assert!(
            load(&ghost, EMPTY_WAIVERS).is_err(),
            "a bite_proof naming no defined fn must refuse — unproven detectors do not register"
        );
    }

    #[test]
    fn bite_proof_naming_a_non_test_helper_does_not_count() {
        let mut cf = tempfile::NamedTempFile::new().expect("checks tempfile");
        cf.write_all(GOOD.as_bytes()).expect("write checks");
        let waivers = WaiverFile {
            waiver: vec![],
            scope_waiver: vec![],
        };
        let helper_only = "fn unwrap_detonates_on_bare_unwrap() {}\n#[test]\nfn other() {}\n";
        assert!(
            Registry::load(cf.path(), &waivers, helper_only).is_err(),
            "a helper fn never runs under the suite — citing it proves nothing"
        );
    }

    #[test]
    fn bite_proof_name_inside_a_comment_does_not_count() {
        let mut cf = tempfile::NamedTempFile::new().expect("checks tempfile");
        cf.write_all(GOOD.as_bytes()).expect("write checks");
        let waivers = WaiverFile {
            waiver: vec![],
            scope_waiver: vec![],
        };
        let smuggled = "// unwrap_detonates_on_bare_unwrap\n#[test]\nfn other() {}\n";
        assert!(
            Registry::load(cf.path(), &waivers, smuggled).is_err(),
            "the cross-check parses fns; a name in a comment is not a proof"
        );
    }

    #[test]
    fn refuses_waivers_against_unknown_or_hardban_checks() {
        let w_unknown = r#"
scope_waiver = []

[[waiver]]
check = "ghost"
file = "a.rs"
line = 1
construct = "x"
why_not_sabotage = "long enough testimony for the fixture"
"#;
        assert!(load(GOOD, w_unknown).is_err(), "unknown check must refuse");

        let hard = GOOD.replace("sworn-waiver", "hard-ban");
        let w_hard = r#"
scope_waiver = []

[[waiver]]
check = "unwrap"
file = "a.rs"
line = 1
construct = "x"
why_not_sabotage = "long enough testimony for the fixture"
"#;
        assert!(load(&hard, w_hard).is_err(), "hard-ban waiver must refuse");
    }
}
