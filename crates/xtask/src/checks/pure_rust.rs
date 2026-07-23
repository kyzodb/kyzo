/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The pure-Rust gate, ported from `scripts/check-pure-rust.sh` (story
//! #322): kyzo-core and kyzo-bin must carry no C/C++-toolchain crate in
//! their dependency tree (normal + build edges, every shipped target). The
//! six language bindings are intrinsically FFI and are not checked here.

use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::process::Command;
use std::sync::LazyLock;

use regex::Regex;

use crate::cargo_meta::CargoMetadata;

/// Crates whose presence means a C/C++ compiler or a banned base backend
/// got in. The C-carrying crypto/compression stacks are named explicitly as
/// defense in depth (ported verbatim from the script's own comment): each
/// also pulls `cc`, but a named hit reads as "wrong TLS/codec stack" instead
/// of a bare toolchain violation.
static BANNED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:cc|cmake|cxx|cxx-build|bindgen|pkg-config|sqlite3-src|libsqlite3-sys|rusqlite|librocksdb-sys|rocksdb|cozorocks|ring|aws-lc-rs|aws-lc-sys|aws-lc-fips-sys|openssl|openssl-sys|openssl-src|native-tls|zstd|zstd-sys|zstd-safe|libz-sys|libz-ng-sys|lzma-sys|bzip2-sys) ",
    )
    .unwrap()
});
/// Any `*-sys` crate is, by convention, a binding to a native library —
/// caught as a class so a new C binding can't slip in under an unlisted name.
static BANNED_SUFFIX_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-sys v").unwrap());
/// The two `-sys`-by-name crates that are pure Rust: syscall/ABI metadata
/// only, no C source, no cc/bindgen.
static PURE_SYS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:linux-raw-sys|windows-sys) ").unwrap());

#[derive(Debug)]
pub enum PureRustError {
    RepoRoot(anyhow::Error),
    Spawn(std::io::Error),
    TreeCommandFailed { package: String, output: String },
    NoPackageFound,
    BannedCratesFound(Vec<String>),
}

impl fmt::Display for PureRustError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PureRustError::RepoRoot(e) => write!(f, "could not locate workspace root: {e:#}"),
            PureRustError::Spawn(e) => write!(f, "pure-Rust gate: failed to spawn cargo: {e}"),
            PureRustError::TreeCommandFailed { package, output } => write!(
                f,
                "pure-Rust gate: 'cargo tree -p {package}' errored (an unreadable tree is not a clean tree):\n{output}"
            ),
            PureRustError::NoPackageFound => {
                write!(
                    f,
                    "pure-Rust gate: no engine package found in the workspace"
                )
            }
            PureRustError::BannedCratesFound(hits) => write!(
                f,
                "pure-Rust gate: C/C++-toolchain crates found in the engine dependency tree:\n{}",
                hits.join("\n")
            ),
        }
    }
}

impl std::error::Error for PureRustError {}

pub fn check() -> Result<String, PureRustError> {
    let repo_root = crate::fsutil::repo_root().map_err(PureRustError::RepoRoot)?;
    check_at(&repo_root)
}

fn run_cargo(repo_root: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("cargo")
        .args(args)
        .current_dir(repo_root)
        .output()
}

/// Story #322: replaces the hard-coded `ENGINE_PACKAGES` list entirely — no
/// crate name is named anywhere in this discovery. A workspace member is a
/// purity-gated root when `cargo metadata` shows it is:
///
/// 1. a real, current workspace member (not a dependency merely pulled in);
/// 2. not marked `publish = false` — the root `Cargo.toml`'s own convention
///    for internal dev tooling that never ships (`xtask`,
///    `kyzo-crashfs`, `kyzo-arrow-interop` all carry that marker precisely
///    because none of them is part of the shipped engine tree); and
/// 3. not vendored third-party source patched into the workspace under
///    `vendor/` (already scanned as an ordinary dependency wherever a
///    shipped root actually pulls it in, e.g. `fjall` beneath `kyzo`).
///
/// Because this reads real, current package data instead of a maintained
/// name list, a new shipped product crate is a covered root the moment it
/// lands in the workspace, with no xtask edit required to keep it covered
/// — closing the "a new crate silently escapes" defect (Condemned).
fn discover_roots(metadata: &CargoMetadata) -> Vec<String> {
    let workspace_members: HashSet<&str> = metadata
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect();
    let workspace_root = Path::new(&metadata.workspace_root);

    let mut roots: Vec<String> = metadata
        .packages
        .iter()
        .filter(|p| workspace_members.contains(p.id.as_str()))
        .filter(|p| !matches!(&p.publish, Some(registries) if registries.is_empty()))
        .filter(|p| {
            Path::new(&p.manifest_path)
                .strip_prefix(workspace_root)
                .map(|rel| !rel.starts_with("vendor"))
                .unwrap_or(true)
        })
        .map(|p| p.name.clone())
        .collect();
    roots.sort_unstable();
    roots.dedup();
    roots
}

fn check_at(repo_root: &Path) -> Result<String, PureRustError> {
    if !repo_root.join("Cargo.toml").is_file() {
        return Ok("pure-Rust gate: no Cargo workspace yet — armed but idle".to_string());
    }

    // Warm the registry/dep cache first: a cold cache pollutes stderr with
    // "Updating index / Downloading ..." noise that would false-match the
    // banned-crate scan below. Both fetch variants may fail; that is fine,
    // same as the script's `|| true`.
    let _fetch_warm =
        run_cargo(repo_root, &["fetch", "--locked"]).or_else(|_| run_cargo(repo_root, &["fetch"]));

    // Story #322: the set of package trees scanned is derived from `cargo
    // metadata`'s real workspace-member list, not a hand-maintained package
    // list — see `discover_roots`.
    let meta_output = run_cargo(repo_root, &["metadata", "--format-version=1", "--no-deps"])
        .map_err(PureRustError::Spawn)?;
    let meta_combined = format!(
        "{}{}",
        String::from_utf8_lossy(&meta_output.stdout),
        String::from_utf8_lossy(&meta_output.stderr)
    );
    if !meta_output.status.success() {
        return Err(PureRustError::TreeCommandFailed {
            package: "workspace metadata".to_string(),
            output: meta_combined,
        });
    }

    let metadata: CargoMetadata = serde_json::from_slice(&meta_output.stdout).map_err(|e| {
        PureRustError::TreeCommandFailed {
            package: "workspace metadata".to_string(),
            output: format!("could not parse `cargo metadata` JSON: {e}"),
        }
    })?;

    let roots = discover_roots(&metadata);

    // Each discovered root gets its dependency tree scanned via `cargo
    // tree`, exactly as the ported script did: `cargo tree` resolves a
    // package's own precise, per-consumer feature set, which `cargo
    // metadata`'s resolve graph cannot represent under whole-workspace
    // feature unification (verified: `cargo build -p kyzo-bin` never
    // compiles `ring`/`js-sys`/`cc`, yet metadata's unfiltered resolve
    // graph claims all three are reachable from `kyzo-bin` — a real crate
    // pulled in only by *other* workspace members' feature requests, not by
    // `kyzo-bin` itself). `cargo metadata` supplies the package SET to
    // scan; `cargo tree` stays the source of the scanned lines so the scan
    // itself does not regress to false positives.
    let mut trees = String::new();
    let mut checked = Vec::new();
    for pkg in &roots {
        let output = run_cargo(
            repo_root,
            &[
                "tree",
                "-p",
                pkg,
                "-e",
                "normal,build",
                "--target=all",
                "--prefix",
                "none",
            ],
        )
        .map_err(PureRustError::Spawn)?;
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if !output.status.success() {
            return Err(PureRustError::TreeCommandFailed {
                package: pkg.clone(),
                output: combined,
            });
        }
        trees.push_str(&combined);
        trees.push('\n');
        checked.push(pkg.clone());
    }

    if checked.is_empty() {
        return Err(PureRustError::NoPackageFound);
    }

    let mut lines: Vec<&str> = trees.lines().collect();
    lines.sort_unstable();
    lines.dedup();
    let hits: Vec<String> = lines
        .into_iter()
        .filter(|l| BANNED_RE.is_match(l) || BANNED_SUFFIX_RE.is_match(l))
        .filter(|l| !PURE_SYS_RE.is_match(l))
        .map(|s| s.to_string())
        .collect();

    if !hits.is_empty() {
        return Err(PureRustError::BannedCratesFound(hits));
    }

    Ok(format!(
        "pure-Rust gate: clean (checked:{} — no C/C++-toolchain crate in the dependency tree)",
        checked.iter().map(|c| format!(" {c}")).collect::<String>()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path as StdPath;

    fn write(root: &StdPath, rel: &str, content: &str) {
        let fp = root.join(rel);
        if let Some(parent) = fp.parent() {
            fs::create_dir_all(parent).expect("create_dir_all");
        }
        fs::write(&fp, content).expect("write fixture file");
    }

    /// Story #322: crate coverage must come from the real workspace, not a
    /// maintained list, at BOTH levels the Condemned defect names — a new
    /// dependency of an already-known root, and a whole new root/product
    /// crate xtask has never named anywhere. Plants:
    ///
    /// - `kyzo` (an existing root) gaining a brand-new normal dependency
    ///   named to match `BANNED_RE`, plus a dev-only banned dependency that
    ///   must stay excluded (`-e normal,build` parity);
    /// - `kyzo-newproduct`: a whole new workspace member, never named
    ///   anywhere in xtask, not marked `publish = false` — i.e. a shipped
    ///   product exactly like `kyzo`/`kyzo-bin` — carrying its own banned
    ///   dependency. `discover_roots` must find and scan it with zero xtask
    ///   code naming it, proving a new PRODUCT crate cannot silently escape;
    /// - `internal-tool`: a new workspace member marked `publish = false`
    ///   (the same marker `xtask`/`kyzo-crashfs`/`kyzo-arrow-interop` carry
    ///   in the real workspace) carrying its own banned dependency, which
    ///   must stay OUT of the hit list — dev tooling must not become a root
    ///   just because it exists in the workspace.
    #[test]
    fn plants_new_crate_and_proves_metadata_derived_coverage() {
        let tmp = tempfile::Builder::new()
            .prefix("pure-rust-plant-")
            .tempdir()
            .expect("tempdir");
        let root = tmp.path();

        // The four leaf dependency crates below live in a SEPARATE tempdir,
        // entirely outside the workspace root's own directory tree.
        // A Cargo package nested under a `[workspace]` root auto-joins that
        // workspace the moment anything path-depends on it, even if it is
        // never listed in `members` — so keeping them outside is what makes
        // them ordinary path dependencies rather than extra roots in their
        // own right (which would confound the dev-dependency/publish-false
        // exclusion assertions below).
        let deps = tempfile::Builder::new()
            .prefix("pure-rust-plant-deps-")
            .tempdir()
            .expect("tempdir");
        let dep_path = |name: &str| deps.path().join(name).display().to_string();

        write(
            root,
            "Cargo.toml",
            "[workspace]\n\
             resolver = \"2\"\n\
             members = [\"kyzo\", \"kyzo-bin\", \"kyzo-newproduct\", \"internal-tool\"]\n",
        );

        write(
            root,
            "kyzo/Cargo.toml",
            &format!(
                "[package]\n\
                 name = \"kyzo\"\n\
                 version = \"0.0.0\"\n\
                 edition = \"2021\"\n\
                 \n\
                 [dependencies]\n\
                 cc = {{ path = {:?} }}\n\
                 \n\
                 [dev-dependencies]\n\
                 bindgen = {{ path = {:?} }}\n",
                dep_path("planted-normal-dep"),
                dep_path("planted-dev-dep"),
            ),
        );
        write(root, "kyzo/src/lib.rs", "");

        write(
            root,
            "kyzo-bin/Cargo.toml",
            "[package]\n\
             name = \"kyzo-bin\"\n\
             version = \"0.0.0\"\n\
             edition = \"2021\"\n",
        );
        write(root, "kyzo-bin/src/main.rs", "fn main() {}\n");

        // Planted crate #1: a brand-new normal dependency of the EXISTING
        // root `kyzo`, deliberately named to match `BANNED_RE`.
        write(
            deps.path(),
            "planted-normal-dep/Cargo.toml",
            "[package]\n\
             name = \"cc\"\n\
             version = \"0.0.0\"\n\
             edition = \"2021\"\n",
        );
        write(deps.path(), "planted-normal-dep/src/lib.rs", "");

        // Planted crate #2: a dev-only dependency, also banned by name. The
        // ported script scanned only `-e normal,build`; a dev-only banned
        // crate must stay OUT of the hit list.
        write(
            deps.path(),
            "planted-dev-dep/Cargo.toml",
            "[package]\n\
             name = \"bindgen\"\n\
             version = \"0.0.0\"\n\
             edition = \"2021\"\n",
        );
        write(deps.path(), "planted-dev-dep/src/lib.rs", "");

        // A whole new PRODUCT crate: not `kyzo`, not `kyzo-bin`, never named
        // in xtask, not `publish = false` — a shipped root by the same
        // convention `kyzo`/`kyzo-bin` already use.
        write(
            root,
            "kyzo-newproduct/Cargo.toml",
            &format!(
                "[package]\n\
                 name = \"kyzo-newproduct\"\n\
                 version = \"0.0.0\"\n\
                 edition = \"2021\"\n\
                 \n\
                 [dependencies]\n\
                 openssl = {{ path = {:?} }}\n",
                dep_path("planted-product-dep"),
            ),
        );
        write(root, "kyzo-newproduct/src/lib.rs", "");
        write(
            deps.path(),
            "planted-product-dep/Cargo.toml",
            "[package]\n\
             name = \"openssl\"\n\
             version = \"0.0.0\"\n\
             edition = \"2021\"\n",
        );
        write(deps.path(), "planted-product-dep/src/lib.rs", "");

        // A new internal-tooling crate marked `publish = false`, exactly
        // like `xtask`/`kyzo-crashfs`/`kyzo-arrow-interop` in the real
        // workspace. It must never become a scanned root.
        write(
            root,
            "internal-tool/Cargo.toml",
            &format!(
                "[package]\n\
                 name = \"internal-tool\"\n\
                 version = \"0.0.0\"\n\
                 edition = \"2021\"\n\
                 publish = false\n\
                 \n\
                 [dependencies]\n\
                 aws-lc-sys = {{ path = {:?} }}\n",
                dep_path("planted-tool-dep"),
            ),
        );
        write(root, "internal-tool/src/lib.rs", "");
        write(
            deps.path(),
            "planted-tool-dep/Cargo.toml",
            "[package]\n\
             name = \"aws-lc-sys\"\n\
             version = \"0.0.0\"\n\
             edition = \"2021\"\n",
        );
        write(deps.path(), "planted-tool-dep/src/lib.rs", "");

        match check_at(root) {
            Err(PureRustError::BannedCratesFound(hits)) => {
                assert!(
                    hits.iter().any(|h| h.starts_with("cc v")),
                    "planted normal-dependency crate `cc` (existing root `kyzo`) was not covered: {hits:?}"
                );
                assert!(
                    hits.iter().any(|h| h.starts_with("openssl v")),
                    "planted new-product root `kyzo-newproduct`'s dependency `openssl` was not \
                     discovered — root discovery is not actually metadata-derived: {hits:?}"
                );
                assert!(
                    !hits.iter().any(|h| h.starts_with("bindgen v")),
                    "dev-only planted crate `bindgen` leaked into the normal+build scan: {hits:?}"
                );
                assert!(
                    !hits.iter().any(|h| h.starts_with("aws-lc-sys v")),
                    "publish=false internal-tooling crate `internal-tool` was wrongly scanned \
                     as a root: {hits:?}"
                );
            }
            other => panic!(
                "expected the planted `cc`/`openssl` dependencies to trip BannedCratesFound, got {other:?}"
            ),
        }
    }
}
