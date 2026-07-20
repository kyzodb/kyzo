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
//! fingerprint, and the fmt/clippy package lists below are the justfile's
//! own hand-maintained `-p` lists, ported unchanged).

use std::fmt;
use std::process::Command;

use crate::proc::{ProcessFailure, run_step};

/// kyzo-core (package name `kyzo`) plus the other first-party packages.
/// The old bench-internals/fuzz-internals dual clippy pass died with the
/// sealed doors. Any OTHER first-party package is covered by `first_party_packages`
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

/// Every build-script (`custom-build`) target in the full dependency graph
/// runs net-isolated and is snapshot-diffed for stray writes (story #294:
/// `crates/xtask/src/checks/build_script_sandbox.rs`).
pub fn build_script_sandbox()
-> Result<(), crate::checks::build_script_sandbox::BuildScriptSandboxError> {
    let msg = crate::checks::build_script_sandbox::check()?;
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

/// Former bench-internals/fuzz-internals lib tests. Sealed doors are gone;
/// this step is a no-op so the gate does not keep a ghost feature alive.
pub fn test_features() -> Result<(), ProcessFailure> {
    println!("test-features: skipped — sealed doors deleted (bench_api/fuzz_api)");
    Ok(())
}

/// The whole first-party test suite repeated under the `release-checked`
/// profile (overflow-checks + debug-assertions live, release-speed
/// optimization) — the gate's mechanical proof that no raw stored-byte
/// arithmetic slipped past the T2 quantity types. The blanket
/// `overflow-checks` toggle in `[profile.release]` is the fuzzing safety
/// net only; this step is the correctness mechanism.
pub fn test_release_checked() -> Result<(), ProcessFailure> {
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
        "--profile",
        "release-checked",
    ]);
    run_step("test-release-checked", cmd)
}

/// Former bench-internals/fuzz-internals lib tests under release-checked.
/// Sealed doors are gone; no-op (see `test_features`).
pub fn test_features_release_checked() -> Result<(), ProcessFailure> {
    println!("test-features-release-checked: skipped — sealed doors deleted (bench_api/fuzz_api)");
    Ok(())
}

/// Run the freshly-built binary IN the container: `cargo xtask run -- <args>`.
pub fn run_bin(args: &[String]) -> Result<(), ProcessFailure> {
    let mut cmd = Command::new("cargo");
    cmd.args(["run", "-p", "kyzo-bin", "--release", "--"]);
    cmd.args(args);
    run_step("run", cmd)
}

// ── bench verb honesty chain (story #326 / seats 87, 86) ──────────────────
//
// Emitting a naked number is unrepresentable. Four conditions must hold as
// typed proofs before any result may emit; each refusal is a named variant
// of [`BenchRefuse`], fired by fixtures below. Softening the opponent to a
// strawman, omitting caps, emitting from a dirty/untagged tree, or stubbing
// answer-agreement to always agree are all illegal — the happy path is
// unconstructable without [`BenchAdmit`].

/// Sealed opponent-pin identity for the TC SNAP corpus. Not CLI-softenable:
/// any other pin (including a convenient strawman) is [`BenchRefuse::OpponentPin`].
pub const BENCH_OPPONENT_PIN: &str = "kyzo.bench.tc.snap.corpus.v1";

/// Sealed expected answer digests (graph → digest identity). Answer-agreement
/// compares observed digests against these exactly — never a stub that always
/// agrees. Real SHA-256 campaign digests supersede these identities in later
/// tasks; the comparison law is already fail-closed equality.
pub const SEALED_EXPECTED_DIGESTS: &[(&str, &str)] = &[
    ("email-Eu-core", "kyzo.tc.digest.email-Eu-core.v1"),
    ("p2p-Gnutella08", "kyzo.tc.digest.p2p-Gnutella08.v1"),
    ("wiki-Vote", "kyzo.tc.digest.wiki-Vote.v1"),
];

/// Memory and wall-time caps the bench must run under. Both axes required;
/// missing either is [`BenchRefuse::Caps`] (no silent "unlimited" default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BenchCaps {
    memory_kib: u64,
    time_secs: u64,
}

impl BenchCaps {
    /// Sealed required caps — same envelope `scripts/run-bench.sh` used
    /// (12 GiB virtual, 1800s per graph). Not softenable to zero/absent.
    pub const REQUIRED_MEMORY_KIB: u64 = 12 * 1024 * 1024;
    pub const REQUIRED_TIME_SECS: u64 = 1800;

    /// The only lawful caps object the verb arms.
    pub fn required() -> Self {
        Self {
            memory_kib: Self::REQUIRED_MEMORY_KIB,
            time_secs: Self::REQUIRED_TIME_SECS,
        }
    }

    /// Construct caps only when both axes are present and positive.
    pub fn try_from_parts(
        memory_kib: Option<u64>,
        time_secs: Option<u64>,
    ) -> Result<Self, BenchRefuse> {
        match (memory_kib, time_secs) {
            (Some(m), Some(t)) if m > 0 && t > 0 => Ok(Self {
                memory_kib: m,
                time_secs: t,
            }),
            _ => Err(BenchRefuse::Caps),
        }
    }

    pub fn memory_kib(&self) -> u64 {
        self.memory_kib
    }

    pub fn time_secs(&self) -> u64 {
        self.time_secs
    }
}

/// Working-tree + tag state the tagged-clean commit condition requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitState {
    /// Clean tree at a named Spec/bench tag.
    CleanTagged { tag: String, sha: String },
    /// Dirty working tree — tagged-clean missing.
    Dirty,
    /// Clean but no tag on HEAD — tagged-clean missing.
    Untagged { sha: String },
}

/// Evidence the honesty chain must prove before emit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchEvidence {
    /// Must equal [`BENCH_OPPONENT_PIN`] exactly.
    pub opponent_pin: String,
    /// Both memory and time caps, or refuse [`BenchRefuse::Caps`].
    pub caps: Option<BenchCaps>,
    pub commit: CommitState,
    /// Observed digests keyed by graph name — compared to [`SEALED_EXPECTED_DIGESTS`].
    pub observed_digests: Vec<(String, String)>,
}

/// Every way the bench verb refuses an unprovable number (seat 87).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BenchRefuse {
    /// Opponent pin missing or softenable to a strawman (not the sealed pin).
    OpponentPin,
    /// Memory and/or time caps missing.
    Caps,
    /// Tagged-clean commit missing (dirty working tree or untagged HEAD).
    Untagged,
    /// Observed digests failed answer-agreement against sealed expected digests.
    AnswerAgreement {
        graph: String,
        expected: String,
        observed: String,
    },
}

impl fmt::Display for BenchRefuse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BenchRefuse::OpponentPin => write!(
                f,
                "bench: opponent pin missing or softenable — required pin is {BENCH_OPPONENT_PIN}"
            ),
            BenchRefuse::Caps => write!(
                f,
                "bench: memory/time caps missing — both axes required (no unlimited default)"
            ),
            BenchRefuse::Untagged => write!(
                f,
                "bench: tagged-clean commit missing (dirty working tree or untagged HEAD)"
            ),
            BenchRefuse::AnswerAgreement {
                graph,
                expected,
                observed,
            } => write!(
                f,
                "bench: answer-agreement failed for graph {graph}: expected {expected}, observed {observed}"
            ),
        }
    }
}

impl std::error::Error for BenchRefuse {}

/// Proof that all four honesty conditions held. Private field: the only
/// constructor is [`BenchAdmit::admit`]. Emit takes this by value — a happy
/// path that skips a condition is unconstructable.
#[derive(Debug, PartialEq, Eq)]
pub struct BenchAdmit {
    _proof: (),
}

impl BenchAdmit {
    /// Gate every refuse condition. Returns [`BenchAdmit`] only when opponent
    /// pin, caps, tagged-clean commit, and answer-agreement all hold.
    pub fn admit(evidence: &BenchEvidence) -> Result<Self, BenchRefuse> {
        if evidence.opponent_pin != BENCH_OPPONENT_PIN {
            return Err(BenchRefuse::OpponentPin);
        }
        let Some(caps) = evidence.caps else {
            return Err(BenchRefuse::Caps);
        };
        if caps.memory_kib() == 0 || caps.time_secs() == 0 {
            return Err(BenchRefuse::Caps);
        }
        match &evidence.commit {
            CommitState::CleanTagged { .. } => {}
            CommitState::Dirty | CommitState::Untagged { .. } => {
                return Err(BenchRefuse::Untagged);
            }
        }
        for &(graph, expected) in SEALED_EXPECTED_DIGESTS {
            let observed = evidence
                .observed_digests
                .iter()
                .find(|(g, _)| g == graph)
                .map(|(_, d)| d.as_str());
            match observed {
                Some(d) if d == expected => {}
                Some(d) => {
                    return Err(BenchRefuse::AnswerAgreement {
                        graph: graph.to_string(),
                        expected: expected.to_string(),
                        observed: d.to_string(),
                    });
                }
                None => {
                    return Err(BenchRefuse::AnswerAgreement {
                        graph: graph.to_string(),
                        expected: expected.to_string(),
                        observed: String::new(),
                    });
                }
            }
        }
        Ok(Self { _proof: () })
    }
}

/// Emit is only reachable with a [`BenchAdmit`] token — numbers without the
/// four proofs are unrepresentable at the type level.
pub fn emit_bench(_admit: BenchAdmit) {
    // T1: admit is the seal; measurement + number lines land in later tasks.
}

/// In-process honesty skeleton (story #326 T1): run the four-condition gate
/// before any measure. Does not shell to `scripts/run-bench.sh` — that path
/// is condemned and deleted in T3. Live evidence without sealed digests
/// refuses [`BenchRefuse::AnswerAgreement`] (fail closed, not a stub agree).
pub fn bench(_graphs: &[String]) -> Result<(), BenchRefuse> {
    let evidence = BenchEvidence {
        opponent_pin: BENCH_OPPONENT_PIN.to_string(),
        caps: Some(BenchCaps::required()),
        commit: probe_commit_state(),
        // T1: no in-process measure yet — empty observed digests refuse
        // answer-agreement against sealed expected digests (not a stub agree).
        observed_digests: Vec::new(),
    };
    let admit = BenchAdmit::admit(&evidence)?;
    emit_bench(admit);
    Ok(())
}

/// Best-effort live commit probe for the verb path. Failure to spawn git or
/// a dirty/untagged tree is [`CommitState::Dirty`] / [`CommitState::Untagged`]
/// — never silently treated as clean-tagged.
fn probe_commit_state() -> CommitState {
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(true);
    if dirty {
        return CommitState::Dirty;
    }
    let sha = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_default();
    let tag = Command::new("git")
        .args(["describe", "--exact-match", "--tags", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let t = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if t.is_empty() { None } else { Some(t) }
            } else {
                None
            }
        });
    match tag {
        Some(tag) => CommitState::CleanTagged { tag, sha },
        None => CommitState::Untagged { sha },
    }
}

#[cfg(test)]
mod bench_refuse_fixtures {
    use super::*;

    fn sealed_digests() -> Vec<(String, String)> {
        SEALED_EXPECTED_DIGESTS
            .iter()
            .map(|(g, d)| ((*g).to_string(), (*d).to_string()))
            .collect()
    }

    fn lawful_evidence() -> BenchEvidence {
        BenchEvidence {
            opponent_pin: BENCH_OPPONENT_PIN.to_string(),
            caps: Some(BenchCaps::required()),
            commit: CommitState::CleanTagged {
                tag: "bench-seal-v1".to_string(),
                sha: "abc123".to_string(),
            },
            observed_digests: sealed_digests(),
        }
    }

    #[test]
    fn fixture_refuse_opponent_pin_missing() {
        let mut e = lawful_evidence();
        e.opponent_pin.clear();
        assert_eq!(BenchAdmit::admit(&e), Err(BenchRefuse::OpponentPin));
    }

    #[test]
    fn fixture_refuse_opponent_pin_strawman_not_softenable() {
        let mut e = lawful_evidence();
        e.opponent_pin = "kyzo.bench.strawman.easy.v0".to_string();
        assert_eq!(BenchAdmit::admit(&e), Err(BenchRefuse::OpponentPin));
    }

    #[test]
    fn fixture_refuse_caps_missing() {
        let mut e = lawful_evidence();
        e.caps = None;
        assert_eq!(BenchAdmit::admit(&e), Err(BenchRefuse::Caps));
    }

    #[test]
    fn fixture_refuse_caps_incomplete_axes() {
        assert_eq!(
            BenchCaps::try_from_parts(Some(1), None),
            Err(BenchRefuse::Caps)
        );
        assert_eq!(
            BenchCaps::try_from_parts(None, Some(1)),
            Err(BenchRefuse::Caps)
        );
    }

    #[test]
    fn fixture_refuse_untagged_dirty() {
        let mut e = lawful_evidence();
        e.commit = CommitState::Dirty;
        assert_eq!(BenchAdmit::admit(&e), Err(BenchRefuse::Untagged));
    }

    #[test]
    fn fixture_refuse_untagged_no_tag() {
        let mut e = lawful_evidence();
        e.commit = CommitState::Untagged {
            sha: "abc123".to_string(),
        };
        assert_eq!(BenchAdmit::admit(&e), Err(BenchRefuse::Untagged));
    }

    #[test]
    fn fixture_refuse_answer_agreement_mismatch() {
        let mut e = lawful_evidence();
        e.observed_digests = vec![
            ("email-Eu-core".to_string(), "forged-digest".to_string()),
            (
                "p2p-Gnutella08".to_string(),
                "kyzo.tc.digest.p2p-Gnutella08.v1".to_string(),
            ),
            ("wiki-Vote".to_string(), "kyzo.tc.digest.wiki-Vote.v1".to_string()),
        ];
        match BenchAdmit::admit(&e) {
            Err(BenchRefuse::AnswerAgreement {
                graph,
                expected,
                observed,
            }) => {
                assert_eq!(graph, "email-Eu-core");
                assert_eq!(expected, "kyzo.tc.digest.email-Eu-core.v1");
                assert_eq!(observed, "forged-digest");
            }
            other => panic!("expected AnswerAgreement, got {other:?}"),
        }
    }

    #[test]
    fn fixture_refuse_answer_agreement_missing_observed() {
        let mut e = lawful_evidence();
        e.observed_digests.clear();
        assert!(matches!(
            BenchAdmit::admit(&e),
            Err(BenchRefuse::AnswerAgreement { .. })
        ));
    }

    #[test]
    fn admit_all_four_conditions_yields_emit_token() {
        let admit = BenchAdmit::admit(&lawful_evidence()).expect("lawful evidence must admit");
        emit_bench(admit);
    }
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
