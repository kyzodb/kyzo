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

use std::fmt;
use std::path::Path;
use std::process::Command;
use std::sync::LazyLock;

use regex::Regex;

const ENGINE_PACKAGES: &[&str] = &["kyzo", "kyzo-bin"];

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

fn check_at(repo_root: &Path) -> Result<String, PureRustError> {
    if !repo_root.join("Cargo.toml").is_file() {
        return Ok("pure-Rust gate: no Cargo workspace yet — armed but idle".to_string());
    }

    // Warm the registry/dep cache first: a cold cache pollutes stderr with
    // "Updating index / Downloading ..." noise that would false-match the
    // banned-crate scan below. Both fetch variants may fail; that is fine,
    // same as the script's `|| true`.
    let _ =
        run_cargo(repo_root, &["fetch", "--locked"]).or_else(|_| run_cargo(repo_root, &["fetch"]));

    let mut trees = String::new();
    let mut checked = Vec::new();
    for pkg in ENGINE_PACKAGES {
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
        if output.status.success() {
            trees.push_str(&combined);
            trees.push('\n');
            checked.push((*pkg).to_string());
        } else if combined.contains("not found in workspace")
            || combined.contains("did not match any packages")
        {
            println!(
                "note: package '{pkg}' not in the workspace yet — the gate covers it when it lands"
            );
        } else {
            return Err(PureRustError::TreeCommandFailed {
                package: (*pkg).to_string(),
                output: combined,
            });
        }
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
