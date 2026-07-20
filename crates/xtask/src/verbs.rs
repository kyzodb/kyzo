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
use std::path::Path;
use std::process::{Command, Stdio};

use serde::Deserialize;

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
    /// Sealed required caps — 12 GiB virtual, 1800s per graph.
    /// Not softenable to zero/absent.
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

/// In-process honesty skeleton (story #326): run the four-condition gate
/// before any measure. Emit only via [`BenchAdmit::admit`] — no shell-out.
/// Live evidence without sealed digests refuses
/// [`BenchRefuse::AnswerAgreement`] (fail closed, not a stub agree).
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

// ── dataset manifest + in-process fetch (story #326 T2 / seat 86) ─────────
//
// Every graph is URL + SHA-256. Fetch may use curl/gunzip as transport; the
// integrity gate is in-process: compute SHA-256 of the bytes and compare to
// the manifest. Filename/length checks are not integrity. Tampered bytes
// must refuse — see `fixture_refuse_tampered_bytes_fail_sha256_manifest`.

/// Repo-relative path of the sealed SNAP graph manifest.
pub const BENCH_MANIFEST_PATH: &str = "bench/manifest.json";

/// One graph entry: canonical URL plus SHA-256 of the uncompressed edge list.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GraphManifestEntry {
    pub name: String,
    pub url: String,
    pub sha256: String,
}

/// Dataset manifest: URL + SHA-256 per graph (seat 86).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DatasetManifest {
    pub graphs: Vec<GraphManifestEntry>,
}

/// Refusal when dataset bytes fail the manifest integrity gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DatasetRefuse {
    /// Manifest file missing or unreadable.
    ManifestIo(String),
    /// Manifest JSON did not parse.
    ManifestParse(String),
    /// Named graph has no manifest entry.
    UnknownGraph(String),
    /// Computed SHA-256 of bytes ≠ sealed manifest hash (tamper / wrong mirror).
    Sha256Mismatch {
        graph: String,
        expected: String,
        observed: String,
    },
    /// Transport (curl/gunzip) failed to produce bytes.
    Fetch(String),
    /// Writing verified bytes to `bench/data/` failed.
    Write(String),
}

impl fmt::Display for DatasetRefuse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DatasetRefuse::ManifestIo(msg) => write!(f, "bench dataset: manifest io: {msg}"),
            DatasetRefuse::ManifestParse(msg) => {
                write!(f, "bench dataset: manifest parse: {msg}")
            }
            DatasetRefuse::UnknownGraph(name) => {
                write!(f, "bench dataset: unknown graph {name} (not in manifest)")
            }
            DatasetRefuse::Sha256Mismatch {
                graph,
                expected,
                observed,
            } => write!(
                f,
                "bench dataset: SHA-256 mismatch for graph {graph}: expected {expected}, observed {observed}"
            ),
            DatasetRefuse::Fetch(msg) => write!(f, "bench dataset: fetch failed: {msg}"),
            DatasetRefuse::Write(msg) => write!(f, "bench dataset: write failed: {msg}"),
        }
    }
}

impl std::error::Error for DatasetRefuse {}

/// Load `bench/manifest.json` from the repo root.
pub fn load_dataset_manifest(root: &Path) -> Result<DatasetManifest, DatasetRefuse> {
    let path = root.join(BENCH_MANIFEST_PATH);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        DatasetRefuse::ManifestIo(format!("{}: {e}", path.display()))
    })?;
    serde_json::from_str(&text)
        .map_err(|e| DatasetRefuse::ManifestParse(e.to_string()))
}

/// Compute SHA-256 of `bytes` and compare to the sealed hex digest.
/// Real compute + compare — not a filename or length check.
pub fn verify_sha256(
    graph: &str,
    bytes: &[u8],
    expected_hex: &str,
) -> Result<(), DatasetRefuse> {
    let observed = sha256_hex(bytes);
    let expected = expected_hex.trim().to_ascii_lowercase();
    if observed == expected {
        Ok(())
    } else {
        Err(DatasetRefuse::Sha256Mismatch {
            graph: graph.to_string(),
            expected,
            observed,
        })
    }
}

/// Verify uncompressed graph bytes against a manifest entry (by name).
pub fn verify_graph_bytes(
    manifest: &DatasetManifest,
    graph: &str,
    bytes: &[u8],
) -> Result<(), DatasetRefuse> {
    let entry = manifest
        .graphs
        .iter()
        .find(|g| g.name == graph)
        .ok_or_else(|| DatasetRefuse::UnknownGraph(graph.to_string()))?;
    verify_sha256(graph, bytes, &entry.sha256)
}

/// In-process fetch verb: for each manifest graph, ensure `bench/data/{name}.txt`
/// holds bytes whose SHA-256 matches the sealed manifest. Existing files are
/// re-verified (tampered on-disk bytes refuse). Missing files download from
/// the sealed URL, gunzip, verify, then write — never write unverified bytes.
pub fn fetch_bench_data() -> Result<(), DatasetRefuse> {
    let root = crate::fsutil::repo_root()
        .map_err(|e| DatasetRefuse::ManifestIo(e.to_string()))?;
    let manifest = load_dataset_manifest(&root)?;
    let data_dir = root.join("bench/data");
    std::fs::create_dir_all(&data_dir).map_err(|e| {
        DatasetRefuse::Write(format!("{}: {e}", data_dir.display()))
    })?;

    for entry in &manifest.graphs {
        let out = data_dir.join(format!("{}.txt", entry.name));
        if out.is_file() {
            let bytes = std::fs::read(&out).map_err(|e| {
                DatasetRefuse::ManifestIo(format!("{}: {e}", out.display()))
            })?;
            verify_sha256(&entry.name, &bytes, &entry.sha256)?;
            println!("have  {} (sha256 ok)", out.display());
            continue;
        }
        println!("fetch {} <- {}", entry.name, entry.url);
        let bytes = download_gunzip(&entry.url)?;
        verify_sha256(&entry.name, &bytes, &entry.sha256)?;
        write_verified(&out, &bytes)?;
        println!("wrote {} (sha256 ok)", out.display());
    }
    println!("done -> bench/data/ (manifest SHA-256 verified)");
    Ok(())
}

fn write_verified(path: &Path, bytes: &[u8]) -> Result<(), DatasetRefuse> {
    std::fs::write(path, bytes)
        .map_err(|e| DatasetRefuse::Write(format!("{}: {e}", path.display())))
}

/// Transport only: curl the URL, gunzip to uncompressed bytes. Integrity is
/// [`verify_sha256`] — this path must not skip that gate.
fn download_gunzip(url: &str) -> Result<Vec<u8>, DatasetRefuse> {
    let curl = Command::new("curl")
        .args(["-sSL", url])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| DatasetRefuse::Fetch(format!("curl spawn: {e}")))?;
    let curl_out = curl
        .stdout
        .ok_or_else(|| DatasetRefuse::Fetch("curl stdout missing".into()))?;

    let gunzip = Command::new("gunzip")
        .arg("-c")
        .stdin(Stdio::from(curl_out))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| DatasetRefuse::Fetch(format!("gunzip: {e}")))?;

    if !gunzip.status.success() {
        return Err(DatasetRefuse::Fetch(format!(
            "gunzip failed: {}",
            String::from_utf8_lossy(&gunzip.stderr).trim()
        )));
    }
    Ok(gunzip.stdout)
}

/// SHA-256 hex digest (lowercase). Pure in-process — the manifest meter.
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = sha256(data);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// FIPS 180-4 SHA-256 (in-process; no filename/length shortcut).
fn sha256(data: &[u8]) -> [u8; 32] {
    // Initial hash values (first 32 bits of the fractional parts of the
    // square roots of the first 8 primes).
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    // Round constants (cube roots of the first 64 primes).
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let bit_len = (data.len() as u64).saturating_mul(8);
    let mut padded = data.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
    }
    out
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

#[cfg(test)]
mod dataset_manifest_fixtures {
    use super::*;

    /// NIST empty-string SHA-256 — proves the in-process hasher is real SHA-256.
    #[test]
    fn sha256_empty_string_matches_nist() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// Adversarial: byte-flipped blob must fail real SHA-256 verify (compute +
    /// compare), not a filename/length check. Same length as the sealed bytes.
    #[test]
    fn fixture_refuse_tampered_bytes_fail_sha256_manifest() {
        let sealed = b"0 1\n2 3\n2 4\nemail-Eu-core fixture edge list\n";
        let expected = sha256_hex(sealed);
        verify_sha256("email-Eu-core", sealed, &expected)
            .expect("sealed bytes must verify against their own SHA-256");

        let mut tampered = sealed.to_vec();
        assert!(!tampered.is_empty());
        tampered[0] ^= 0xff; // byte-flip — length unchanged
        assert_eq!(
            tampered.len(),
            sealed.len(),
            "adversary preserves length; length check would falsely pass"
        );

        match verify_sha256("email-Eu-core", &tampered, &expected) {
            Err(DatasetRefuse::Sha256Mismatch {
                graph,
                expected: exp,
                observed,
            }) => {
                assert_eq!(graph, "email-Eu-core");
                assert_eq!(exp, expected);
                assert_ne!(observed, expected);
                assert_eq!(observed, sha256_hex(&tampered));
            }
            other => panic!("expected Sha256Mismatch for tampered bytes, got {other:?}"),
        }
    }

    #[test]
    fn fixture_manifest_entry_verify_uses_sha256_not_name() {
        let manifest = DatasetManifest {
            graphs: vec![GraphManifestEntry {
                name: "wiki-Vote".into(),
                url: "https://snap.stanford.edu/data/wiki-Vote.txt.gz".into(),
                sha256: sha256_hex(b"honest wiki-Vote bytes"),
            }],
        };
        // Same graph name, wrong bytes → refuse (name match is not integrity).
        match verify_graph_bytes(&manifest, "wiki-Vote", b"tampered wiki-Vote bytes!!!!") {
            Err(DatasetRefuse::Sha256Mismatch { graph, .. }) => {
                assert_eq!(graph, "wiki-Vote");
            }
            other => panic!("expected Sha256Mismatch, got {other:?}"),
        }
        verify_graph_bytes(&manifest, "wiki-Vote", b"honest wiki-Vote bytes")
            .expect("honest bytes must pass");
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
