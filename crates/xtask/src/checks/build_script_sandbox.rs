/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The build-script sandbox gate (story #294): every `custom-build` target in
//! the full dependency graph — first-party or vendored, but today entirely
//! registry crates — must run its build script with no network access and
//! write only inside its own `OUT_DIR`. The whole workspace is clean-rebuilt
//! once net-isolated (`unshare --net`) and once, only if the sandboxed build
//! fails, plainly (the control) to tell "this script needs the network"
//! apart from "this script is just broken" (amended ruling, story #294 T4,
//! Amendment 2: a per-package `-p <pkg>` unit hit an unrelated cargo
//! feature-resolver panic on at least one real registry crate (`alloca`)
//! when built in isolation from the rest of the graph; one whole-workspace
//! pass sidesteps that entirely, and needs `--all-targets` to reach
//! dev-dependency-only build scripts like `criterion`'s pull of `alloca`).
//!
//! The check owns its target directory outright (Amendment 3): every cargo
//! invocation here runs with `CARGO_TARGET_DIR` pointed at a directory this
//! check creates empty and deletes itself — production uses
//! `<ambient target dir>/build-script-sandbox`, each test plant its own
//! tempdir. An empty owned directory is what forces every build script to
//! actually run, so no `cargo clean` exists here at all: a workspace
//! `cargo clean` against the container's shared `/target` volume both
//! destroys the warm cache every other gate step relies on and fails
//! outright on the volume mountpoint ("Device or resource busy").

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use walkdir::WalkDir;

use crate::cargo_meta::CargoMetadata;

/// Where a violating write was traced to: a single package by name, or — if
/// the evidence does not pin it down to exactly one — the full set of
/// candidates it could be. Never collapsed into a formatted string: the
/// caller (the `Display` impl, a test's `match`) still branches on which
/// case it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attribution {
    Attributed(String),
    Ambiguous(Vec<String>),
}

impl fmt::Display for Attribution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Attribution::Attributed(name) => write!(f, "{name}"),
            Attribution::Ambiguous(candidates) => {
                write!(f, "ambiguous among: {}", candidates.join(", "))
            }
        }
    }
}

#[derive(Debug)]
pub enum BuildScriptSandboxError {
    RepoRoot(anyhow::Error),
    /// `cargo metadata` itself could not be run (spawn failure or nonzero exit).
    Metadata(String),
    /// `cargo metadata` ran but its JSON could not be parsed.
    MetadataParse {
        reason: String,
    },
    /// Resetting the check-owned target directory failed (Amendment 3:
    /// this replaced `cargo clean` outright).
    Clean {
        output: String,
    },
    /// A before/after filesystem snapshot could not be taken.
    Snapshot {
        reason: String,
    },
    /// The sandboxed build failed, and the control build (whole workspace,
    /// re-cleaned, plain network) succeeded — the only difference between
    /// the two runs was network access, so some build script needs it.
    NetworkAccessAttempted {
        sandboxed_output: String,
    },
    /// The sandboxed build failed AND the control build reproduced the
    /// failure — an ordinary build defect, not a network dependency.
    ControlBuildFailed {
        output: String,
    },
    /// The sandboxed build (or its control) succeeded, but some build
    /// script wrote somewhere it never should have: the workspace source
    /// tree, or the target directory outside its own `build/<pkg>-*/` area.
    WriteOutsideOutDir {
        package: Attribution,
        paths: Vec<String>,
    },
    /// Namespace setup itself failed (`unshare --net` unavailable) — this
    /// FAILS the gate; it is never treated as "nothing to check."
    SandboxUnavailable {
        reason: String,
    },
}

impl fmt::Display for BuildScriptSandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildScriptSandboxError::RepoRoot(e) => {
                write!(f, "could not locate workspace root: {e:#}")
            }
            BuildScriptSandboxError::Metadata(output) => {
                write!(
                    f,
                    "build-script sandbox gate: `cargo metadata` failed:\n{output}"
                )
            }
            BuildScriptSandboxError::MetadataParse { reason } => write!(
                f,
                "build-script sandbox gate: could not parse `cargo metadata` output: {reason}"
            ),
            BuildScriptSandboxError::Clean { output } => write!(
                f,
                "build-script sandbox gate: resetting the check-owned target directory failed:\n{output}"
            ),
            BuildScriptSandboxError::Snapshot { reason } => write!(
                f,
                "build-script sandbox gate: filesystem snapshot failed: {reason}"
            ),
            BuildScriptSandboxError::NetworkAccessAttempted { sandboxed_output } => write!(
                f,
                "build-script sandbox gate: a build script needs network access — the \
                 whole-workspace build failed net-isolated (`unshare --net`) and succeeded \
                 identically with network restored. Sandboxed run output:\n{sandboxed_output}"
            ),
            BuildScriptSandboxError::ControlBuildFailed { output } => write!(
                f,
                "build-script sandbox gate: the workspace failed to build even with network \
                 restored (control build) — an ordinary build defect, not network isolation:\n{output}"
            ),
            BuildScriptSandboxError::WriteOutsideOutDir { package, paths } => write!(
                f,
                "build-script sandbox gate: {package}'s build script wrote outside its own \
                 OUT_DIR:\n{}",
                paths.join("\n")
            ),
            BuildScriptSandboxError::SandboxUnavailable { reason } => write!(
                f,
                "build-script sandbox gate: sandbox setup unavailable — `unshare --net` could not \
                 be used ({reason}). This fails the gate; it is never silently skipped."
            ),
        }
    }
}

impl std::error::Error for BuildScriptSandboxError {}

pub fn check() -> Result<String, BuildScriptSandboxError> {
    let repo_root = crate::fsutil::repo_root().map_err(BuildScriptSandboxError::RepoRoot)?;
    check_at(&repo_root)
}

/// Shape of `cargo metadata --format-version=1` lives in [`crate::cargo_meta`]
/// — shared with the pure-Rust gate so the two never drift. Full-graph
/// (no `--no-deps`): a build-script target can belong to any dependency, not
/// only a workspace member.

fn combined_output(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Every build this check runs gets `CARGO_TARGET_DIR` pointed at the
/// check-owned directory — set only on the one `Command`, never on the
/// process itself, so concurrently running tests never contend over a
/// shared, process-wide environment variable.
fn scoped_env(cmd: &mut Command, owned_target: &Path) {
    cmd.env("CARGO_TARGET_DIR", owned_target);
}

fn run_metadata(repo_root: &Path) -> Result<CargoMetadata, BuildScriptSandboxError> {
    let mut cmd = Command::new("cargo");
    cmd.args(["metadata", "--format-version=1"])
        .current_dir(repo_root);
    let output = cmd
        .output()
        .map_err(|e| BuildScriptSandboxError::Metadata(format!("failed to spawn: {e}")))?;
    if !output.status.success() {
        return Err(BuildScriptSandboxError::Metadata(combined_output(&output)));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| BuildScriptSandboxError::MetadataParse {
        reason: format!("{e}"),
    })
}

/// One `custom-build`-carrying package: its bare name (used for the success
/// message and to seed the "ordinary artifact"/attribution name set — cargo's
/// build/lib directory naming never embeds the version) and its exact
/// `name@version` spec (kept for display only now that builds run
/// workspace-wide rather than per-package).
struct BuildScriptTarget {
    name: String,
    spec: String,
}

/// Every package with at least one `custom-build` (build.rs) target,
/// anywhere in the full resolved dependency graph — workspace member or
/// not. No name is hard-coded: a build script that appears tomorrow, first
/// party or vendored, is covered the moment `cargo metadata` reports it.
fn discover_build_script_packages(metadata: &CargoMetadata) -> Vec<BuildScriptTarget> {
    let mut targets: Vec<BuildScriptTarget> = metadata
        .packages
        .iter()
        .filter(|p| {
            p.targets
                .iter()
                .any(|t| t.kind.iter().any(|k| k == "custom-build"))
        })
        .map(|p| BuildScriptTarget {
            name: p.name.clone(),
            spec: format!("{}@{}", p.name, p.version),
        })
        .collect();
    targets.sort_unstable_by(|a, b| a.spec.cmp(&b.spec));
    targets.dedup_by(|a, b| a.spec == b.spec);
    targets
}

/// Workspace member package names (by cross-referencing `workspace_members`
/// ids against `packages`), same lookup `pure_rust.rs`'s `discover_roots`
/// uses to tell a real member from a merely-resolved dependency.
fn discover_workspace_member_names(metadata: &CargoMetadata) -> Vec<String> {
    let member_ids: std::collections::HashSet<&str> = metadata
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect();
    metadata
        .packages
        .iter()
        .filter(|p| member_ids.contains(p.id.as_str()))
        .map(|p| p.name.clone())
        .collect()
}

/// Every package's own source directory (the parent of its manifest), for
/// attribution (b): the longest-matching directory prefix a violating
/// workspace-source-tree path falls under.
fn package_dirs(metadata: &CargoMetadata) -> Vec<(String, PathBuf)> {
    metadata
        .packages
        .iter()
        .map(|p| {
            let dir = match Path::new(&p.manifest_path).parent() {
                Some(parent) => parent.to_path_buf(),
                None => PathBuf::from(&p.manifest_path),
            };
            (p.name.clone(), dir)
        })
        .collect()
}

/// A failed namespace setup FAILS the gate (never a silent skip): probed
/// once, up front, rather than per package, so a genuine network-dependent
/// build failure downstream is never mistaken for a broken sandbox.
fn verify_sandbox_available() -> Result<(), BuildScriptSandboxError> {
    match Command::new("unshare")
        .args(["--net", "--", "true"])
        .output()
    {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(BuildScriptSandboxError::SandboxUnavailable {
            reason: format!(
                "`unshare --net -- true` exited with status {:?}: {}",
                output.status.code(),
                combined_output(&output)
            ),
        }),
        Err(e) => Err(BuildScriptSandboxError::SandboxUnavailable {
            reason: format!("failed to spawn `unshare`: {e}"),
        }),
    }
}

/// Resets the check-owned target directory to empty (Amendment 3: replaces
/// `cargo clean` outright — an empty owned directory forces every build
/// script to run, without touching the container's shared target volume,
/// whose mountpoint `cargo clean` cannot even remove).
fn reset_owned_target(owned_target: &Path) -> Result<(), BuildScriptSandboxError> {
    if let Err(e) = std::fs::remove_dir_all(owned_target) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(BuildScriptSandboxError::Clean {
                output: format!(
                    "failed to remove check-owned target dir {}: {e}",
                    owned_target.display()
                ),
            });
        }
    }
    std::fs::create_dir_all(owned_target).map_err(|e| BuildScriptSandboxError::Clean {
        output: format!(
            "failed to create check-owned target dir {}: {e}",
            owned_target.display()
        ),
    })
}

/// `Ok` carries the combined output of a successful run; `Err` carries the
/// combined output of a failed one — both sides are needed for the error
/// variants' evidence, so neither is discarded the way `proc::run_step`
/// (exit-code only) would.
///
/// `--all-targets` at workspace scope (Amendment 2) is required, not
/// optional: it is what reaches a build script that only a dev-dependency
/// edge pulls in (e.g. a benchmark harness pulling `alloca`) — unlike the
/// earlier, abandoned per-package `-p <pkg> --all-targets` shape, every
/// dev-dependency here belongs to an actual workspace member and is already
/// resolved in this workspace's own `Cargo.lock`, so it carries none of the
/// "registry crate's own unresolved dev-deps" failure mode that sank the
/// per-package unit.
fn run_sandboxed_build(repo_root: &Path, owned_target: &Path) -> Result<String, String> {
    let mut cmd = Command::new("unshare");
    cmd.args([
        "--net",
        "--",
        "cargo",
        "build",
        "--workspace",
        "--all-targets",
    ])
    .current_dir(repo_root);
    scoped_env(&mut cmd, owned_target);
    let output = cmd.output();
    match output {
        Ok(o) if o.status.success() => Ok(combined_output(&o)),
        Ok(o) => Err(combined_output(&o)),
        Err(e) => Err(format!("failed to spawn sandboxed build: {e}")),
    }
}

fn run_plain_build(repo_root: &Path, owned_target: &Path) -> Result<String, String> {
    let mut cmd = Command::new("cargo");
    cmd.args(["build", "--workspace", "--all-targets"])
        .current_dir(repo_root);
    scoped_env(&mut cmd, owned_target);
    let output = cmd.output();
    match output {
        Ok(o) if o.status.success() => Ok(combined_output(&o)),
        Ok(o) => Err(combined_output(&o)),
        Err(e) => Err(format!("failed to spawn control build: {e}")),
    }
}

/// File mtime as nanoseconds since Unix epoch — a comparable snapshot key,
/// not a wall-clock read site. Absent/unrepresentable mtimes stay `None`.
fn mtime_nanos(meta: &std::fs::Metadata) -> Option<u64> {
    let modified = meta.modified().ok()?;
    let since_epoch = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    u64::try_from(since_epoch.as_nanos()).ok()
}

/// path (relative to `root`) -> (size, mtime_nanos), for a cheap existence/change
/// diff. `.git` is skipped (repository-internal churn, not source); nothing
/// else is — the workspace source tree must never move at all.
///
/// A vanished entry (removed between listing and stat-ing) is skipped rather
/// than failing the whole snapshot: on a shared filesystem this can happen
/// for reasons entirely outside this one check run.
fn snapshot_dir(
    root: &Path,
    skip_dirnames: &[&str],
) -> Result<HashMap<PathBuf, (u64, Option<u64>)>, std::io::Error> {
    let mut out = HashMap::new();
    let walker = WalkDir::new(root).into_iter().filter_entry(|e| {
        if e.depth() == 0 {
            return true;
        }
        let name = e.file_name().to_string_lossy();
        !skip_dirnames.iter().any(|s| *s == name)
    });
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            // The walk itself hit a path that vanished mid-traversal.
            Err(e)
                if e.io_error()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(e) => return Err(std::io::Error::other(e)),
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e)
                if e.io_error()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(e) => return Err(std::io::Error::other(e)),
        };
        let rel = match entry.path().strip_prefix(root) {
            Ok(rel) => rel.to_path_buf(),
            Err(_) => entry.path().to_path_buf(),
        };
        out.insert(rel, (meta.len(), mtime_nanos(&meta)));
    }
    Ok(out)
}

/// Every path present in only one snapshot, or present in both with a
/// different (size, mtime) — an add, a removal, or a rewrite.
fn diff_paths(
    before: &HashMap<PathBuf, (u64, Option<u64>)>,
    after: &HashMap<PathBuf, (u64, Option<u64>)>,
) -> Vec<PathBuf> {
    let mut changed = Vec::new();
    for (path, after_v) in after {
        match before.get(path) {
            Some(before_v) if before_v == after_v => {}
            Some(_) | None => changed.push(path.clone()),
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            changed.push(path.clone());
        }
    }
    changed.sort();
    changed.dedup();
    changed
}

/// Cargo's own bookkeeping for a whole-workspace clean+rebuild: `deps/`,
/// `.fingerprint/`, `incremental/` (package-agnostic — every compiled unit
/// lands there, custom-build or not), each known package's own
/// `build/<name>-<hash>/` working directory (which holds a build script's
/// `out/` alongside cargo's own stamp/output/stderr files for that same
/// script), and a known package's top-level "uplifted" build product
/// directly under the profile dir (e.g. `debug/libname.rlib`/`.d`, or
/// `debug/name` for a bin target — named after the crate's Rust identifier,
/// dashes replaced with underscores, never the hash-suffixed form; only a
/// primary build target — a workspace member under `--workspace` — ever
/// gets one). `known_names` is every custom-build package name union every
/// workspace member name: the discovered set Amendment 2 broadens this
/// check to, now that a single combined build recreates ALL of it at once,
/// not just one package's own slice.
fn is_ordinary_artifact(rel_path: &Path, known_names: &[String]) -> bool {
    // Cargo's own package-agnostic bookkeeping, written fresh into any empty
    // target directory by cargo itself, never by a build script: the
    // `.cargo-*` lock family (`.cargo-lock`, `.cargo-build-lock`,
    // `.cargo-artifact-lock`, ...), the cache marker, and the rustc probe
    // cache.
    if let Some(file_name) = rel_path.file_name().and_then(|n| n.to_str()) {
        if file_name.starts_with(".cargo-")
            || matches!(file_name, "CACHEDIR.TAG" | ".rustc_info.json")
        {
            return true;
        }
    }

    let components: Vec<_> = rel_path.components().collect();

    // A direct child of the profile dir (2 components: `<profile>/<file>`)
    // whose filename contains a known crate's identifier is that crate's
    // top-level uplifted artifact.
    if components.len() == 2 {
        if let Some(file_name) = rel_path.file_name().and_then(|n| n.to_str()) {
            if known_names
                .iter()
                .any(|n| file_name.contains(n.replace('-', "_").as_str()))
            {
                return true;
            }
        }
    }

    let mut iter = components.iter().peekable();
    while let Some(component) = iter.next() {
        let name = component.as_os_str().to_string_lossy();
        match name.as_ref() {
            // `examples/` joins the package-agnostic set: `--all-targets`
            // uplifts every example binary there, hash-suffixed, for any
            // workspace member that declares one.
            "deps" | ".fingerprint" | "incremental" | "examples" => return true,
            "build" => {
                if let Some(next) = iter.peek() {
                    let next_name = next.as_os_str().to_string_lossy();
                    if known_names
                        .iter()
                        .any(|n| next_name.starts_with(format!("{n}-").as_str()))
                    {
                        return true;
                    }
                }
            }
            _other_target_segment => {}
        }
    }
    false
}

/// Traces one violating path back to the package whose build script (most
/// likely) wrote it: (a) parse `<name>` out of a `build/<name>-<hash>/**`
/// segment under the target directory, matched against the known-name set;
/// (b) failing that, the longest-matching package source-directory prefix
/// (a workspace-source-tree write can only belong to a package whose
/// manifest lives inside the workspace root at all); (c) failing both, the
/// full known-name set as the candidate pool — attribution stays honestly
/// ambiguous rather than guessing, but the violation still hard-fails
/// either way.
fn attribute_violation(
    abs_path: &Path,
    target_dir: &Path,
    known_names: &[String],
    package_dirs: &[(String, PathBuf)],
) -> Attribution {
    if let Ok(rel_to_target) = abs_path.strip_prefix(target_dir) {
        let comps: Vec<_> = rel_to_target.components().collect();
        for (i, c) in comps.iter().enumerate() {
            if c.as_os_str() == "build" {
                if let Some(next) = comps.get(i + 1) {
                    let next_name = next.as_os_str().to_string_lossy();
                    let candidates: Vec<String> = known_names
                        .iter()
                        .filter(|n| next_name.starts_with(format!("{n}-").as_str()))
                        .cloned()
                        .collect();
                    match candidates.len() {
                        1 => {
                            let mut it = candidates.into_iter();
                            match it.next() {
                                Some(name) => return Attribution::Attributed(name),
                                None => break,
                            }
                        }
                        0 => break,
                        2.. => return Attribution::Ambiguous(candidates),
                    }
                }
            }
        }
    }

    let mut best: Option<(&str, usize)> = None;
    for (name, dir) in package_dirs {
        if dir.as_os_str().is_empty() {
            continue;
        }
        if abs_path.starts_with(dir) {
            let len = dir.as_os_str().len();
            let better = match best {
                Some((_, best_len)) => len > best_len,
                None => true,
            };
            if better {
                best = Some((name.as_str(), len));
            }
        }
    }
    if let Some((name, _)) = best {
        return Attribution::Attributed(name.to_string());
    }

    Attribution::Ambiguous(known_names.to_vec())
}

/// Reduces every violating path's own attribution to the one value the
/// error's `package` field carries: a single name if every path agreed, the
/// union of every candidate seen otherwise. Still hard-fails either way —
/// this only decides how the failure names its culprit, never whether it
/// fires.
fn reduce_attributions(attributions: Vec<Attribution>) -> Attribution {
    let mut names: Vec<String> = Vec::new();
    for a in attributions {
        match a {
            Attribution::Attributed(n) => names.push(n),
            Attribution::Ambiguous(candidates) => names.extend(candidates),
        }
    }
    names.sort();
    names.dedup();
    match names.len() {
        1 => match names.into_iter().next() {
            Some(name) => Attribution::Attributed(name),
            None => Attribution::Ambiguous(Vec::new()),
        },
        0 | 2.. => Attribution::Ambiguous(names),
    }
}

/// The one whole-workspace reset → sandboxed build → snapshot diff →
/// (on sandboxed failure only) control build sequence (Amendment 2:
/// replaces the earlier per-package `check_package` loop; Amendment 3:
/// the reset empties the check-owned target directory instead of running
/// any `cargo clean`).
fn check_workspace(
    repo_root: &Path,
    workspace_root: &Path,
    owned_target: &Path,
    known_names: &[String],
    package_dirs: &[(String, PathBuf)],
) -> Result<(), BuildScriptSandboxError> {
    reset_owned_target(owned_target)?;

    let skip = ["target", ".git"];
    let src_before =
        snapshot_dir(workspace_root, &skip).map_err(|e| BuildScriptSandboxError::Snapshot {
            reason: format!("workspace source tree (pre-build): {e}"),
        })?;
    let target_before =
        snapshot_dir(owned_target, &[]).map_err(|e| BuildScriptSandboxError::Snapshot {
            reason: format!("check-owned target directory (pre-build): {e}"),
        })?;

    let sandboxed = run_sandboxed_build(repo_root, owned_target);

    let result = match sandboxed {
        Ok(_sandboxed_output) => {
            let src_after = snapshot_dir(workspace_root, &skip).map_err(|e| {
                BuildScriptSandboxError::Snapshot {
                    reason: format!("workspace source tree (post-build): {e}"),
                }
            })?;
            let target_after =
                snapshot_dir(owned_target, &[]).map_err(|e| BuildScriptSandboxError::Snapshot {
                    reason: format!("check-owned target directory (post-build): {e}"),
                })?;

            let src_violations: Vec<PathBuf> = diff_paths(&src_before, &src_after)
                .into_iter()
                .map(|rel| workspace_root.join(rel))
                .collect();
            let target_violations: Vec<PathBuf> = diff_paths(&target_before, &target_after)
                .into_iter()
                .filter(|p| !is_ordinary_artifact(p, known_names))
                .map(|rel| owned_target.join(rel))
                .collect();

            let mut violations: Vec<PathBuf> = src_violations
                .into_iter()
                .chain(target_violations)
                .collect();
            violations.sort();
            violations.dedup();

            if !violations.is_empty() {
                let attributions: Vec<Attribution> = violations
                    .iter()
                    .map(|p| attribute_violation(p, owned_target, known_names, package_dirs))
                    .collect();
                let package = reduce_attributions(attributions);
                return Err(BuildScriptSandboxError::WriteOutsideOutDir {
                    package,
                    paths: violations
                        .into_iter()
                        .map(|p| p.display().to_string())
                        .collect(),
                });
            }
            Ok(())
        }
        Err(sandboxed_output) => {
            // The sandboxed build failed. That is expected of a build script
            // that needs the network — but it is also what an ordinary build
            // defect looks like. Reset and rebuild once, plainly, to tell
            // the two apart: only the network differs between the two runs.
            reset_owned_target(owned_target)?;
            match run_plain_build(repo_root, owned_target) {
                Ok(_) => Err(BuildScriptSandboxError::NetworkAccessAttempted { sandboxed_output }),
                Err(control_output) => Err(BuildScriptSandboxError::ControlBuildFailed {
                    output: control_output,
                }),
            }
        }
    };

    // The owned directory is this check's scratch, not a cache anything else
    // reads: remove it on success so a full extra workspace build never
    // lingers on the shared volume. Failure keeps it for forensics.
    if result.is_ok() {
        match std::fs::remove_dir_all(owned_target) {
            Ok(()) => {}
            Err(scratch_remove_refuse) => {
                // Best-effort cleanup of check-owned scratch; forensics keep
                // the dir on the primary Result::Err path above.
                drop(scratch_remove_refuse);
            }
        }
    }
    result
}

/// `check()`/`check_at()` (production) derive the check-owned directory as
/// `<ambient target dir>/build-script-sandbox` — on the shared volume for
/// speed, but created, diffed, and deleted only by this check. Tests call
/// `check_at_scoped` with `Some(<a dir named "target" inside their own
/// tempdir>)` ("target" so the source-tree snapshot's skip list ignores it),
/// so a plant's reset+build cycle is fully isolated from every other
/// concurrently-running plant and from the real workspace — never by
/// serializing the test runner, which the container owns.
fn check_at(repo_root: &Path) -> Result<String, BuildScriptSandboxError> {
    check_at_scoped(repo_root, None)
}

fn check_at_scoped(
    repo_root: &Path,
    owned_target_override: Option<&Path>,
) -> Result<String, BuildScriptSandboxError> {
    if !repo_root.join("Cargo.toml").is_file() {
        return Ok(
            "build-script sandbox gate: no Cargo workspace yet — armed but idle".to_string(),
        );
    }

    verify_sandbox_available()?;

    let metadata = run_metadata(repo_root)?;
    let owned_target = match owned_target_override {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(&metadata.target_directory).join("build-script-sandbox"),
    };
    let workspace_root = PathBuf::from(&metadata.workspace_root);
    let targets = discover_build_script_packages(&metadata);

    if targets.is_empty() {
        return Ok(
            "build-script sandbox gate: no custom-build targets in the full dependency graph — armed but idle"
                .to_string(),
        );
    }

    let member_names = discover_workspace_member_names(&metadata);
    let mut known_names: Vec<String> = targets.iter().map(|t| t.name.clone()).collect();
    known_names.extend(member_names);
    known_names.sort_unstable();
    known_names.dedup();

    let dirs = package_dirs(&metadata);

    check_workspace(
        repo_root,
        &workspace_root,
        &owned_target,
        &known_names,
        &dirs,
    )?;

    Ok(format!(
        "build-script sandbox gate: clean (checked, net-isolated, whole-workspace build:{} — wrote only inside their own build/<pkg>-*/out/, touched no network)",
        targets
            .iter()
            .map(|t| format!(" {}", t.spec))
            .collect::<String>()
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

    /// A single-crate workspace whose `build.rs` is swapped in by the
    /// caller, mirroring `pure_rust.rs`'s plant-test pattern. Each plant gets
    /// its own `crate_name`: the two tests below run concurrently under the
    /// default test harness and share one process-wide `CARGO_TARGET_DIR`
    /// (this container sets it once, globally), and cargo's top-level
    /// "uplifted" artifact for a lib target is named after the crate's own
    /// identifier with no hash suffix — an identically-named plant in each
    /// test would collide on that one shared path.
    fn plant_workspace(crate_name: &str, build_rs: &str) -> tempfile::TempDir {
        let tmp = tempfile::Builder::new()
            .prefix("build-script-sandbox-plant-")
            .tempdir()
            .expect("tempdir");
        let root = tmp.path();

        write(
            root,
            "Cargo.toml",
            &format!(
                "[workspace]\n\
                 resolver = \"2\"\n\
                 members = [{crate_name:?}]\n"
            ),
        );
        write(
            root,
            &format!("{crate_name}/Cargo.toml"),
            &format!(
                "[package]\n\
                 name = {crate_name:?}\n\
                 version = \"0.0.0\"\n\
                 edition = \"2021\"\n\
                 build = \"build.rs\"\n"
            ),
        );
        write(root, &format!("{crate_name}/src/lib.rs"), "");
        write(root, &format!("{crate_name}/build.rs"), build_rs);
        tmp
    }

    /// Plant #1 (ruling task 6a): a build script that needs the network. It
    /// fails net-isolated (no route out of a fresh `unshare --net` namespace)
    /// and succeeds identically once network is restored — exactly the
    /// `NetworkAccessAttempted` signature, not a `ControlBuildFailed`.
    #[test]
    fn plants_network_dependent_build_script_and_trips_network_access_attempted() {
        let crate_name = "xtask-plant-network";
        let tmp = plant_workspace(
            crate_name,
            "fn main() {\n\
             \x20   use std::net::TcpStream;\n\
             \x20   use std::time::Duration;\n\
             \x20   let addr = \"1.1.1.1:80\".parse().unwrap();\n\
             \x20   if TcpStream::connect_timeout(&addr, Duration::from_secs(5)).is_err() {\n\
             \x20       panic!(\"build script requires network access\");\n\
             \x20   }\n\
             }\n",
        );

        assert!(
            matches!(
                check_at_scoped(tmp.path(), Some(&tmp.path().join("target"))),
                Err(BuildScriptSandboxError::NetworkAccessAttempted { .. })
            ),
            "expected the planted network-dependent build script to trip NetworkAccessAttempted"
        );
    }

    /// Plant #2 (ruling task 6b): a build script that writes outside
    /// `OUT_DIR`, straight into the workspace source tree — must trip
    /// `WriteOutsideOutDir`, never pass silently.
    #[test]
    fn plants_out_of_bounds_write_and_trips_write_outside_out_dir() {
        let crate_name = "xtask-plant-writeescape";
        let tmp = plant_workspace(
            crate_name,
            "fn main() {\n\
             \x20   // CARGO_MANIFEST_DIR is the crate's own directory, a direct child of\n\
             \x20   // the workspace root regardless of where CARGO_TARGET_DIR physically\n\
             \x20   // lives — climbing exactly one level lands squarely in the workspace\n\
             \x20   // source tree, unambiguously outside OUT_DIR.\n\
             \x20   let manifest_dir = std::env::var(\"CARGO_MANIFEST_DIR\").unwrap();\n\
             \x20   let escape = std::path::Path::new(&manifest_dir).join(\"../ESCAPED.txt\");\n\
             \x20   std::fs::write(escape, \"should never land here\").expect(\"escape write\");\n\
             }\n",
        );

        match check_at_scoped(tmp.path(), Some(&tmp.path().join("target"))) {
            Err(BuildScriptSandboxError::WriteOutsideOutDir { package, paths }) => {
                assert_eq!(package, Attribution::Attributed(crate_name.to_string()));
                assert!(
                    paths.iter().any(|p| p.contains("ESCAPED.txt")),
                    "expected the escaped write to be named in the violation paths: {paths:?}"
                );
            }
            other => panic!(
                "expected the planted out-of-OUT_DIR write to trip WriteOutsideOutDir, got {other:?}"
            ),
        }
    }

    /// Plant #3 (Amendment 2, ruling task 7): a two-crate workspace, one
    /// well-behaved build script and one violator, over the now-broadened
    /// `is_ordinary_artifact`/attribution logic. Proves two things at once:
    /// the clean crate's own ordinary compiled output (top-level uplift,
    /// `build/<name>-*/`, `deps/`, `.fingerprint/`) is never mistaken for a
    /// violation now that the check runs one combined whole-workspace build
    /// instead of per package, and the violator's write attributes to its
    /// own name specifically — not `Ambiguous` — even though it shares the
    /// build with an innocent sibling.
    #[test]
    fn plants_two_crate_workspace_and_attributes_the_violator_alone() {
        let clean_name = "xtask-plant-clean";
        let violator_name = "xtask-plant-violator";

        let tmp = tempfile::Builder::new()
            .prefix("build-script-sandbox-plant-")
            .tempdir()
            .expect("tempdir");
        let root = tmp.path();

        write(
            root,
            "Cargo.toml",
            &format!(
                "[workspace]\n\
                 resolver = \"2\"\n\
                 members = [{clean_name:?}, {violator_name:?}]\n"
            ),
        );

        write(
            root,
            &format!("{clean_name}/Cargo.toml"),
            &format!(
                "[package]\n\
                 name = {clean_name:?}\n\
                 version = \"0.0.0\"\n\
                 edition = \"2021\"\n\
                 build = \"build.rs\"\n"
            ),
        );
        write(root, &format!("{clean_name}/src/lib.rs"), "");
        // A well-behaved build script: writes only inside its own OUT_DIR.
        write(
            root,
            &format!("{clean_name}/build.rs"),
            "fn main() {\n\
             \x20   let out_dir = std::env::var(\"OUT_DIR\").unwrap();\n\
             \x20   let inside = std::path::Path::new(&out_dir).join(\"fine.txt\");\n\
             \x20   std::fs::write(inside, \"this is exactly where it belongs\").expect(\"ok write\");\n\
             }\n",
        );

        write(
            root,
            &format!("{violator_name}/Cargo.toml"),
            &format!(
                "[package]\n\
                 name = {violator_name:?}\n\
                 version = \"0.0.0\"\n\
                 edition = \"2021\"\n\
                 build = \"build.rs\"\n"
            ),
        );
        write(root, &format!("{violator_name}/src/lib.rs"), "");
        // Writes into its OWN source directory: still squarely a violation
        // (the source tree must never move at all), and — unlike a
        // workspace-root write, which no path evidence can pin to one of two
        // sibling crates — attributable to the violator alone via the
        // longest-matching manifest-directory prefix (ruling attribution
        // rule b).
        write(
            root,
            &format!("{violator_name}/build.rs"),
            "fn main() {\n\
             \x20   let manifest_dir = std::env::var(\"CARGO_MANIFEST_DIR\").unwrap();\n\
             \x20   let escape = std::path::Path::new(&manifest_dir).join(\"VIOLATOR_ESCAPED.txt\");\n\
             \x20   std::fs::write(escape, \"should never land here\").expect(\"escape write\");\n\
             }\n",
        );

        match check_at_scoped(root, Some(&root.join("target"))) {
            Err(BuildScriptSandboxError::WriteOutsideOutDir { package, paths }) => {
                assert_eq!(
                    package,
                    Attribution::Attributed(violator_name.to_string()),
                    "expected the violation to attribute to {violator_name} alone, not Ambiguous \
                     or the clean sibling; violation paths: {paths:?}"
                );
                assert!(
                    paths.iter().any(|p| p.contains("VIOLATOR_ESCAPED.txt")),
                    "expected the violator's escape write in the violation paths: {paths:?}"
                );
                assert!(
                    !paths.iter().any(|p| p.contains(clean_name)),
                    "the clean crate's own ordinary artifacts must never be reported as \
                     violations: {paths:?}"
                );
            }
            other => panic!(
                "expected the two-crate plant to trip WriteOutsideOutDir attributed to the \
                 violator alone, got {other:?}"
            ),
        }
    }
}
