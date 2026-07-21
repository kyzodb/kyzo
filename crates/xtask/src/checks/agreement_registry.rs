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
//! its listed `file`, **and the file must be reachable by the gate's
//! compile** — transitively included from a crate root (`src/lib.rs`,
//! `src/main.rs`, `src/bin/*.rs`, `tests/*.rs`) through `mod` /
//! `#[path]` edges. Text presence alone proved nothing: the
//! `rehomed_from_core/` lesson was four registry-cited law files (6000+
//! lines of oracle differentials) that no target compiled, kept "green"
//! by a string grep. A law that quietly stopped being tested — renamed,
//! deleted, moved, **or orphaned from the module tree** — is exactly the
//! failure mode this check exists to catch; absence and unreachability
//! are both red.
//!
//! Named limits of the reachability walk (approximations refuse toward
//! red, never toward silent green): `#[cfg(feature = …)]`-gated mod edges
//! are treated as unreachable (the gate tests default features; a
//! default-on feature gating a law file would need this check extended);
//! `include!` is not followed; non-feature `cfg` edges (`test`, platform)
//! are treated as reachable.
//!
//! Cited files may sit outside the engine walk (oracle / trials seats); those
//! are loaded from disk by path so a re-home does not require expanding every
//! resonance scan into the judge crates.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

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

/// Repo-root-relative paths of every `.rs` file transitively included from a
/// compiled crate target. Membership is the mechanical meaning of "the gate
/// can run this test".
pub struct ReachableSet {
    set: BTreeSet<PathBuf>,
}

impl ReachableSet {
    pub fn contains(&self, root: &Path, repo_rel: &str) -> bool {
        self.set
            .contains(&normalize_path(&root.join(repo_rel.trim_start_matches("./"))))
    }
}

/// Collapse `.` / `..` components so `#[path = "../../kyzo-trials/src/dst.rs"]`
/// wiring (the sweep.rs → dst.rs seam) lands on the same key as a direct walk.
fn normalize_path(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Build the reachable set by walking every workspace crate's compiled roots.
pub fn reachable_files(root: &Path) -> anyhow::Result<ReachableSet> {
    let mut set = BTreeSet::new();
    let crates_dir = root.join("crates");
    for entry in std::fs::read_dir(&crates_dir)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", crates_dir.display()))?
    {
        let dir = entry?.path();
        if !dir.is_dir() {
            continue;
        }
        collect_crate_roots(&dir)
            .into_iter()
            .try_for_each(|r| walk_file(&r, &mut set))?;
    }
    Ok(ReachableSet { set })
}

fn collect_crate_roots(crate_dir: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for cand in ["src/lib.rs", "src/main.rs"] {
        let p = crate_dir.join(cand);
        if p.is_file() {
            roots.push(p);
        }
    }
    for sub in ["tests", "src/bin"] {
        let d = crate_dir.join(sub);
        if let Ok(entries) = std::fs::read_dir(&d) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "rs") {
                    roots.push(p);
                }
            }
        }
    }
    roots
}

/// True when a `mod` item's attributes gate it behind a cargo feature —
/// unreachable in the gate's default-feature test build.
fn feature_gated(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg")
            && matches!(&a.meta, syn::Meta::List(l) if l.tokens.to_string().contains("feature"))
    })
}

/// `#[path = "…"]` override on a `mod` item, if present.
fn path_override(attrs: &[syn::Attribute]) -> Option<String> {
    attrs.iter().find_map(|a| {
        if !a.path().is_ident("path") {
            return None;
        }
        match &a.meta {
            syn::Meta::NameValue(nv) => match &nv.value {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }) => Some(s.value()),
                _ => None,
            },
            _ => None,
        }
    })
}

fn walk_file(file: &Path, set: &mut BTreeSet<PathBuf>) -> anyhow::Result<()> {
    let norm = normalize_path(file);
    if !set.insert(norm) {
        return Ok(());
    }
    let Ok(text) = std::fs::read_to_string(file) else {
        return Ok(()); // dangling mod decl; the real build breaks loudly
    };
    let Ok(ast) = syn::parse_file(&text) else {
        return Ok(()); // unparsable file; the real build breaks loudly
    };
    // Cargo's child-resolution base: lib/main/mod.rs resolve children beside
    // themselves; a non-mod-rs file `foo.rs` resolves children under `foo/`.
    let parent = file.parent().unwrap_or(Path::new(""));
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let is_dir_root = matches!(
        file.file_name().and_then(|s| s.to_str()),
        Some("lib.rs" | "main.rs" | "mod.rs")
    ) || parent.ends_with("tests")
        || parent.ends_with("bin");
    let base = if is_dir_root {
        parent.to_path_buf()
    } else {
        parent.join(stem)
    };
    walk_items(&ast.items, parent, &base, set)
}

fn walk_items(
    items: &[syn::Item],
    file_dir: &Path,
    base: &Path,
    set: &mut BTreeSet<PathBuf>,
) -> anyhow::Result<()> {
    for item in items {
        let syn::Item::Mod(m) = item else { continue };
        if feature_gated(&m.attrs) {
            continue; // unreachable in the gate's default-feature build
        }
        match (&m.content, path_override(&m.attrs)) {
            // Inline mod: children resolve one directory deeper.
            (Some((_, nested)), _) => {
                let deeper = base.join(m.ident.to_string());
                walk_items(nested, file_dir, &deeper, set)?;
            }
            // `#[path = "…"] mod x;` — relative to the declaring file's dir.
            (None, Some(p)) => walk_file(&file_dir.join(p), set)?,
            // Plain `mod x;` — `<base>/x.rs` or `<base>/x/mod.rs`.
            (None, None) => {
                let name = m.ident.to_string();
                let flat = base.join(format!("{name}.rs"));
                let nested = base.join(&name).join("mod.rs");
                if flat.is_file() {
                    walk_file(&flat, set)?;
                } else if nested.is_file() {
                    walk_file(&nested, set)?;
                }
            }
        }
    }
    Ok(())
}

pub fn check(files: &[SourceFile], registry: &[LawEntry], root: &Path) -> Vec<Violation> {
    let mut violations = Vec::new();
    let reachable = match reachable_files(root) {
        Ok(r) => r,
        Err(e) => {
            violations.push(Violation {
                name: "reachability walk".to_string(),
                file: "crates/".to_string(),
                test_fn: String::new(),
                reason: format!("mod-graph walk failed (refusing green without proof): {e}"),
            });
            return violations;
        }
    };
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
            continue;
        }
        if !reachable.contains(root, &law.file) {
            violations.push(Violation {
                name: law.name.clone(),
                file: law.file.clone(),
                test_fn: law.test_fn.clone(),
                reason: format!(
                    "{} is not reachable from any compiled target (mod-graph walk from \
                     lib/main/bin/tests roots; feature-gated edges excluded) — the gate \
                     never runs `fn {}`; text presence is not proof",
                    law.file, law.test_fn
                ),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic repo exercising every edge shape the walker claims
    /// to handle, then assert each reachability verdict individually. Every
    /// negative case is a real lie-shape: an orphan law file, a
    /// feature-gated oracle, a bench-only wiring.
    fn build_fixture(root: &Path) {
        let src = root.join("crates/fake/src");
        std::fs::create_dir_all(src.join("deep")).unwrap();
        std::fs::create_dir_all(root.join("crates/fake/tests")).unwrap();
        std::fs::create_dir_all(root.join("crates/other/src")).unwrap();
        let w = |p: &Path, t: &str| std::fs::write(p, t).unwrap();
        w(
            &src.join("lib.rs"),
            "mod wired;\n#[cfg(feature = \"bench-internals\")]\nmod gated;\nmod deep;\n",
        );
        w(&src.join("wired.rs"), "#[cfg(test)]\nmod wired_tests;\n");
        // Child of a non-mod-rs file resolves under `wired/`.
        std::fs::create_dir_all(src.join("wired")).unwrap();
        w(&src.join("wired/wired_tests.rs"), "fn law_wired() {}\n");
        w(&src.join("gated.rs"), "fn law_gated() {}\n");
        w(
            &src.join("deep/mod.rs"),
            "mod inline_outer { #[path = \"../../../other/src/cross.rs\"] mod cross; }\n",
        );
        w(
            &root.join("crates/other/src/cross.rs"),
            "fn law_cross() {}\n",
        );
        w(
            &root.join("crates/fake/tests/integration.rs"),
            "mod helper;\n",
        );
        w(
            &root.join("crates/fake/tests/helper.rs"),
            "fn law_helper() {}\n",
        );
        // The rehomed_from_core shape: present on disk, wired by NOTHING.
        std::fs::create_dir_all(root.join("crates/fake/orphaned")).unwrap();
        w(
            &root.join("crates/fake/orphaned/dead_tests.rs"),
            "fn law_dead() {}\n",
        );
    }

    fn fixture_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "agreement_reach_fixture_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        build_fixture(&root);
        root
    }

    #[test]
    fn reachability_walk_accepts_every_wiring_shape_and_refuses_every_lie_shape() {
        let root = fixture_root();
        let reach = reachable_files(&root).expect("walk succeeds");
        // Wired shapes must be reachable — a false negative here would let
        // the check red-flag genuinely live laws.
        assert!(
            reach.contains(&root, "crates/fake/src/wired/wired_tests.rs"),
            "cfg(test) mod under a non-mod-rs parent must be reachable"
        );
        assert!(
            reach.contains(&root, "crates/other/src/cross.rs"),
            "#[path] with ../ traversal inside an inline mod must be reachable \
             (the sweep.rs -> dst.rs wiring shape)"
        );
        assert!(
            reach.contains(&root, "crates/fake/tests/helper.rs"),
            "a mod of a tests/ integration root must be reachable"
        );
        // Lie shapes must be unreachable — each is a way a 'tested' law
        // never actually runs.
        assert!(
            !reach.contains(&root, "crates/fake/orphaned/dead_tests.rs"),
            "a file wired by nothing (rehomed_from_core shape) must be unreachable"
        );
        assert!(
            !reach.contains(&root, "crates/fake/src/gated.rs"),
            "a feature-gated mod (bench-internals shape) must be unreachable"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn check_goes_red_on_a_present_but_unreachable_law_file() {
        let root = fixture_root();
        let registry = vec![
            LawEntry {
                name: "live law".into(),
                file: "crates/fake/src/wired/wired_tests.rs".into(),
                test_fn: "law_wired".into(),
            },
            LawEntry {
                name: "dead law".into(),
                file: "crates/fake/orphaned/dead_tests.rs".into(),
                test_fn: "law_dead".into(),
            },
        ];
        let violations = check(&[], &registry, &root);
        assert_eq!(
            violations.len(),
            1,
            "exactly the dead entry must violate; got: {:?}",
            violations
                .iter()
                .map(|v| (&v.name, &v.reason))
                .collect::<Vec<_>>()
        );
        assert_eq!(violations[0].name, "dead law");
        assert!(
            violations[0].reason.contains("not reachable"),
            "reason must name unreachability, got: {}",
            violations[0].reason
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
