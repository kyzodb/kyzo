/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The leaf verbs: each one shells out to exactly the command the condemned
//! justfile recipe of the same name ran, preserving every piece of
//! justfile knowledge the story requires not be lost (the env-report
//! fingerprint, the release-profile overflow-checks rationale lives in
//! Cargo.toml's own `[profile.release-checked]` doc, and the fmt/clippy
//! package lists below are the justfile's own hand-maintained `-p` lists,
//! ported unchanged).

use std::process::Command;

use crate::proc::{ProcessFailure, run_step};

/// The one package every other first-party crate's clippy config differs
/// from: kyzo-core (package name `kyzo`) alone carries the
/// bench-internals/fuzz-internals feature configuration and is clippy'd
/// under both, same as the condemned justfile's `clippy` recipe did by
/// hand. Any OTHER first-party package is covered by `first_party_packages`
/// alone — this name does not gate whether a new crate is covered, only
/// whether it gets a second, feature-gated clippy pass.
const CORE_PACKAGE: &str = "kyzo";

/// Every other first-party package the condemned justfile's fmt/clippy
/// recipes named by hand. Ported unchanged (behavior-preserving, story
/// #322's Engineering Choice); a metadata-derived replacement for this
/// hand-maintained list is a separate, later task.
const OTHER_PACKAGES: &[&str] = &["kyzo-bin", "kyzo-crashfs", "kyzo-lsp", "kyzo-arrow-interop"];

/// `container memory.max` / `RUST_TEST_THREADS` / `nproc` / `rustc
/// --version` — the boring, unarguable environment fingerprint every gate
/// run opens with.
pub fn env_report() -> Result<(), ProcessFailure> {
    let mem = std::fs::read_to_string("/sys/fs/cgroup/memory.max")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "native (no cgroup limit)".to_string());
    println!("container memory.max: {mem}");

    let threads =
        std::env::var("RUST_TEST_THREADS").unwrap_or_else(|_| "unset (cargo default)".to_string());
    println!("RUST_TEST_THREADS:    {threads}");

    let nproc = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    println!("nproc:                {nproc}");

    let mut cmd = Command::new("rustc");
    cmd.arg("--version");
    let output = cmd
        .output()
        .map_err(|e| ProcessFailure::Spawn("env-report", e))?;
    if !output.status.success() {
        return Err(match output.status.code() {
            Some(code) => ProcessFailure::ExitCode("env-report", code),
            None => ProcessFailure::Signal("env-report"),
        });
    }
    println!(
        "toolchain:            {}",
        String::from_utf8_lossy(&output.stdout).trim()
    );
    Ok(())
}

/// `cargo check --workspace --all-targets`.
pub fn check() -> Result<(), ProcessFailure> {
    let mut cmd = Command::new("cargo");
    cmd.args(["check", "--workspace", "--all-targets"]);
    run_step("check", cmd)
}

/// `cargo fmt --check` over every first-party package.
pub fn fmt() -> Result<(), ProcessFailure> {
    let mut cmd = Command::new("cargo");
    cmd.args(["fmt", "--check", "-p", CORE_PACKAGE]);
    for p in OTHER_PACKAGES {
        cmd.args(["-p", p]);
    }
    run_step("fmt", cmd)
}

fn clippy_cmd(packages: &[&str], features: &[&str]) -> Command {
    let mut cmd = Command::new("cargo");
    cmd.arg("clippy");
    for p in packages {
        cmd.args(["-p", p]);
    }
    cmd.args(["--release", "--all-targets", "--no-deps"]);
    if !features.is_empty() {
        cmd.args(["--features", &features.join(",")]);
    }
    cmd.args(["--", "-D", "warnings"]);
    cmd
}

/// Own-code `-D warnings`, `--no-deps` (the vendored fjall/lsm-tree path
/// deps are story #118's clippy state, not this gate's). `CORE_PACKAGE`
/// runs both feature configurations; `OTHER_PACKAGES` runs its default
/// config once, together.
pub fn clippy() -> Result<(), ProcessFailure> {
    run_step("clippy", clippy_cmd(&[CORE_PACKAGE], &[]))?;
    run_step(
        "clippy",
        clippy_cmd(&[CORE_PACKAGE], &["bench-internals", "fuzz-internals"]),
    )?;
    run_step("clippy", clippy_cmd(OTHER_PACKAGES, &[]))
}

/// The unsafe law (story #322: ported from `scripts/check-unsafe.sh`,
/// behavior-preserving): forbid present, zero allows, no doc claims a
/// nonexistent exception.
pub fn unsafe_check() -> Result<(), crate::checks::unsafe_check::UnsafeCheckError> {
    let msg = crate::checks::unsafe_check::check()?;
    println!("{msg}");
    Ok(())
}

/// No C/C++-toolchain crate in the engine dependency tree (story #322:
/// ported from `scripts/check-pure-rust.sh`, behavior-preserving).
pub fn pure_rust() -> Result<(), crate::checks::pure_rust::PureRustError> {
    let msg = crate::checks::pure_rust::check()?;
    println!("{msg}");
    Ok(())
}

/// The Type Authority Graph (#139): self-test, then the ratchet + committed
/// artifact freshness check against the tree — the same combination the
/// condemned justfile's `authority` recipe ran (story #322: ported from
/// `scripts/authority-graph.py`, behavior-preserving).
pub fn authority() -> Result<(), crate::checks::authority_graph::AuthorityError> {
    let self_test_msg = crate::checks::authority_graph::self_test()?;
    println!("{self_test_msg}");
    let root = crate::fsutil::repo_root()
        .map_err(crate::checks::authority_graph::AuthorityError::RepoScan)?;
    let msg = crate::checks::authority_graph::run_gate_check(&root)?;
    println!("{msg}");
    Ok(())
}

/// Regenerate the committed `authority/` artifacts (report mode).
pub fn authority_write() -> Result<(), crate::checks::authority_graph::AuthorityError> {
    let root = crate::fsutil::repo_root()
        .map_err(crate::checks::authority_graph::AuthorityError::RepoScan)?;
    let msg = crate::checks::authority_graph::write_report(&root)?;
    println!("{msg}");
    Ok(())
}

/// Tighten the ratchet floor at `crates/xtask/authority-baseline.json` to the
/// current tree's finding counts.
pub fn authority_update_baseline() -> Result<(), crate::checks::authority_graph::AuthorityError> {
    let root = crate::fsutil::repo_root()
        .map_err(crate::checks::authority_graph::AuthorityError::RepoScan)?;
    let msg = crate::checks::authority_graph::update_baseline(&root)?;
    println!("{msg}");
    Ok(())
}

/// Default config, lib + integration, across every first-party package.
pub fn test() -> Result<(), ProcessFailure> {
    let mut cmd = Command::new("cargo");
    cmd.args([
        "test",
        "--workspace",
        "--exclude",
        "fjall",
        "--exclude",
        "lsm-tree",
        "--exclude",
        "xtask",
        "--release",
    ]);
    run_step("test", cmd)
}

/// The bench-internals/fuzz-internals feature configuration's own lib tests.
pub fn test_features() -> Result<(), ProcessFailure> {
    let mut cmd = Command::new("cargo");
    cmd.args([
        "test",
        "-p",
        CORE_PACKAGE,
        "--release",
        "--features",
        "bench-internals,fuzz-internals",
        "--lib",
    ]);
    run_step("test-features", cmd)
}

/// Run the freshly-built binary IN the container: `cargo xtask run -- <args>`.
pub fn run_bin(args: &[String]) -> Result<(), ProcessFailure> {
    let mut cmd = Command::new("cargo");
    cmd.args(["run", "-p", "kyzo-bin", "--release", "--"]);
    cmd.args(args);
    run_step("run", cmd)
}

/// The transitive-closure benchmark over the SNAP graphs (kyzo-bench only).
pub fn bench(graphs: &[String]) -> Result<(), ProcessFailure> {
    let mut cmd = Command::new("bash");
    cmd.arg("scripts/run-bench.sh");
    cmd.args(graphs);
    run_step("bench", cmd)
}

/// `bash scripts/check-mpl-headers.sh` — the MPL half of the license
/// lineage law (falsifier-map's `inputs-license-lineage` entry).
pub fn mpl_headers() -> Result<(), ProcessFailure> {
    let mut cmd = Command::new("bash");
    cmd.arg("scripts/check-mpl-headers.sh");
    run_step("mpl-headers", cmd)
}

/// `cargo deny check bans licenses advisories` — CI installs cargo-deny;
/// this verb assumes it is already on PATH, same as `authority` assumes
/// python3 and `unsafe`/`pure-rust` assume bash are already present.
pub fn deny() -> Result<(), ProcessFailure> {
    let mut cmd = Command::new("cargo");
    cmd.args(["deny", "check", "bans", "licenses", "advisories"]);
    run_step("deny", cmd)
}

/// `bash scripts/check-supply-chain.sh` — cargo vet.
pub fn supply_chain() -> Result<(), ProcessFailure> {
    let mut cmd = Command::new("bash");
    cmd.arg("scripts/check-supply-chain.sh");
    run_step("supply-chain", cmd)
}

/// The memcmp on-disk-format tripwire: run the law tests, and treat a
/// zero-match filter as its own failure (a probe referencing a dead path
/// must go red, never silently green — CLAUDE.md's activation-probe-freshness
/// law, and the falsifier map's `meaning-memcomparable-order-contract`
/// entry). `--lib` scopes the run to kyzo-core's own unit test binary, the
/// only one the filter can ever match.
pub fn memcmp_invariant() -> Result<(), ProcessFailure> {
    let mut cmd = Command::new("cargo");
    cmd.args([
        "test",
        "-p",
        CORE_PACKAGE,
        "--release",
        "--lib",
        "storage::tests::law",
    ]);
    let output = cmd
        .output()
        .map_err(|e| ProcessFailure::Spawn("memcmp-invariant", e))?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        return Err(match output.status.code() {
            Some(code) => ProcessFailure::ExitCode("memcmp-invariant", code),
            None => ProcessFailure::Signal("memcmp-invariant"),
        });
    }
    if String::from_utf8_lossy(&output.stdout).contains("running 0 tests") {
        eprintln!(
            "FAIL memcmp tripwire: the law-test filter matched zero tests — fix the filter or the tests, do not delete the job."
        );
        return Err(ProcessFailure::ExitCode("memcmp-invariant", 1));
    }
    Ok(())
}

/// Every target in fuzz/, 60s smoke each. Requires a nightly toolchain and
/// cargo-fuzz on PATH (CI installs both; nightly is a deliberate, separate
/// axis from THE toolchain authority — fuzzing's sanitizer instrumentation
/// needs it, same as it always has).
pub fn fuzz_smoke() -> Result<(), ProcessFailure> {
    let list = Command::new("cargo")
        .args(["fuzz", "list"])
        .output()
        .map_err(|e| ProcessFailure::Spawn("fuzz-smoke", e))?;
    if !list.status.success() {
        return Err(match list.status.code() {
            Some(code) => ProcessFailure::ExitCode("fuzz-smoke", code),
            None => ProcessFailure::Signal("fuzz-smoke"),
        });
    }
    let targets: Vec<String> = String::from_utf8_lossy(&list.stdout)
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if targets.is_empty() {
        eprintln!(
            "FAIL: fuzz/ exists but lists zero targets — a dead fuzz gate is a failure, not a pass."
        );
        return Err(ProcessFailure::ExitCode("fuzz-smoke", 1));
    }
    for t in &targets {
        println!("=== fuzz target: {t} (60s smoke)");
        let mut cmd = Command::new("cargo");
        cmd.args([
            "+nightly",
            "fuzz",
            "run",
            t,
            "--target",
            "x86_64-unknown-linux-gnu",
            "--",
            "-max_total_time=60",
        ]);
        run_step("fuzz-smoke", cmd)?;
    }
    Ok(())
}

/// `bash scripts/determinism-campaign.sh <out> [compare]` — story #30's
/// cross-run/thread/architecture determinism campaign.
pub fn determinism_campaign(out: &str, compare: Option<&str>) -> Result<(), ProcessFailure> {
    let mut cmd = Command::new("bash");
    cmd.arg("scripts/determinism-campaign.sh");
    cmd.arg(out);
    if let Some(c) = compare {
        cmd.arg(c);
    }
    run_step("determinism-campaign", cmd)
}
