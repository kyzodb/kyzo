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

use std::collections::BTreeMap;
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
/// The bs-detector conduct gate: one door, zero baseline. The binary
/// itself writes crates/xtask/resonance.log and bs-counts.txt; a red exit
/// here is the verdict.
pub fn bs_detector() -> Result<(), ProcessFailure> {
    let mut cmd = Command::new("cargo");
    cmd.args(["run", "--release", "-p", "bs-detector", "--", "--root", "."]);
    run_step("bs-detector", cmd)
}

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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpponentPin(String);

impl OpponentPin {
    /// The only lawful pin identity. Not softenable.
    pub const SEALED: &'static str = "kyzo.bench.tc.snap.corpus.v1";

    /// The sealed pin the verb arms.
    pub fn sealed() -> Self {
        Self(Self::SEALED.to_string())
    }

    /// Evidence-construction only — [`BenchAdmit::admit`] refuses any value
    /// other than [`Self::sealed`].
    #[allow(dead_code)] // honesty-chain fixture door (tests construct non-sealed pins)
    pub fn for_fixture(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl fmt::Display for OpponentPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// SNAP corpus graph identity — never a bare `String` at honesty sites.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(transparent)]
pub struct GraphName(String);

impl GraphName {
    pub fn new(s: impl AsRef<str>) -> Self {
        Self(s.as_ref().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GraphName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<&str> for GraphName {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for GraphName {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl PartialEq<&str> for GraphName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// Real SHA-256 digest bytes (FIPS 180-4). Never a placeholder identity string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sha256([u8; 32]);

impl Sha256 {
    pub fn admit(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// FIPS 180-4 SHA-256 (in-process; no filename/length shortcut).
    pub fn hash(data: &[u8]) -> Self {
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

        // INVARIANT(Sha256): FIPS 180-4 length field is bit-count mod 2⁶⁴.
        let bit_len = (data.len() as u64).wrapping_mul(8);
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
                w[i] = w[i - 16] // INVARIANT(Sha256): message schedule Σ wraps mod 2³² (FIPS 180-4).
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
                // INVARIANT(Sha256): compression Σ wraps mod 2³² (FIPS 180-4).
                let temp1 = hh
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    // INVARIANT(Sha256): compression Σ wraps mod 2³² (FIPS 180-4).
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                // INVARIANT(Sha256): compression Σ wraps mod 2³² (FIPS 180-4).
                let temp2 = s0.wrapping_add(maj);

                hh = g;
                g = f;
                f = e;
                // INVARIANT(Sha256): compression Σ wraps mod 2³² (FIPS 180-4).
                e = d.wrapping_add(temp1);
                d = c;
                c = b;
                b = a;
                // INVARIANT(Sha256): compression Σ wraps mod 2³² (FIPS 180-4).
                a = temp1.wrapping_add(temp2);
            }

            // INVARIANT(Sha256): chaining variables accumulate mod 2³².
            h[0] = h[0].wrapping_add(a);
            h[1] = h[1].wrapping_add(b);
            h[2] = h[2].wrapping_add(c);
            // INVARIANT(Sha256): chaining variables accumulate mod 2³².
            h[3] = h[3].wrapping_add(d);
            h[4] = h[4].wrapping_add(e);
            h[5] = h[5].wrapping_add(f);
            // INVARIANT(Sha256): chaining variables accumulate mod 2³².
            h[6] = h[6].wrapping_add(g);
            h[7] = h[7].wrapping_add(hh);
        }

        let mut out = [0u8; 32];
        for (i, word) in h.iter().enumerate() {
            out[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
        }
        Self(out)
    }

    pub fn from_hex(hex: &str) -> Option<Self> {
        let hex = hex.trim().as_bytes();
        if hex.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, slot) in out.iter_mut().enumerate() {
            let hi = from_hex_nibble(hex[i * 2])?;
            let lo = from_hex_nibble(hex[i * 2 + 1])?;
            *slot = (hi << 4) | lo;
        }
        Some(Self(out))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in &self.0 {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }
}

impl fmt::Display for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

fn from_hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// One graph's sealed or observed answer digest.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphDigest {
    pub graph: GraphName,
    pub digest: Sha256,
}

impl GraphDigest {
    pub fn new(graph: GraphName, digest: Sha256) -> Self {
        Self { graph, digest }
    }
}

impl FromIterator<GraphDigest> for BTreeMap<GraphName, Sha256> {
    fn from_iter<I: IntoIterator<Item = GraphDigest>>(iter: I) -> Self {
        iter.into_iter()
            .map(|GraphDigest { graph, digest }| (graph, digest))
            .collect()
    }
}

/// Corpus graph names the opponent pin covers (names only — not digests).
pub fn corpus_graph_names() -> [GraphName; 3] {
    [
        GraphName::new("email-Eu-core"),
        GraphName::new("p2p-Gnutella08"),
        GraphName::new("wiki-Vote"),
    ]
}

/// Production sealed answer digests. Empty until the pinned corpus is run and
/// real SHA-256 answers are sealed. **No placeholder identity strings** —
/// an empty map makes answer-agreement refuse.
pub fn production_sealed_digests() -> BTreeMap<GraphName, Sha256> {
    BTreeMap::new()
}

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
    #[allow(dead_code)] // honesty-chain fixture door (incomplete-axis refuse tests)
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
    /// Must equal [`OpponentPin::sealed`] exactly.
    pub opponent_pin: OpponentPin,
    /// Both memory and time caps, or refuse [`BenchRefuse::Caps`].
    pub caps: Option<BenchCaps>,
    pub commit: CommitState,
    /// Observed digests keyed by graph — compared to the sealed digest set.
    pub observed_digests: BTreeMap<GraphName, Sha256>,
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
    /// Observed digests failed answer-agreement against sealed expected digests,
    /// or no sealed digests exist yet (`expected` / `observed` may be absent).
    AnswerAgreement {
        graph: GraphName,
        expected: Option<Sha256>,
        observed: Option<Sha256>,
    },
    /// In-process TC measurement infrastructure is not built (no runner to
    /// produce real observed digests). Loud refuse — never an empty stub body.
    MeasurementUnbuilt,
}

impl fmt::Display for BenchRefuse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BenchRefuse::OpponentPin => write!(
                f,
                "bench: opponent pin missing or softenable — required pin is {}",
                OpponentPin::SEALED
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
            } => {
                let exp = expected
                    .as_ref()
                    .map(Sha256::to_hex)
                    .unwrap_or_else(|| "<absent — no sealed digest>".to_string());
                let obs = observed
                    .as_ref()
                    .map(Sha256::to_hex)
                    .unwrap_or_else(|| "<absent>".to_string());
                write!(
                    f,
                    "bench: answer-agreement failed for graph {graph}: expected {exp}, observed {obs}"
                )
            }
            BenchRefuse::MeasurementUnbuilt => write!(
                f,
                "bench: measurement infrastructure unbuilt — cannot compute real observed digests \
                 (no in-process TC runner); refusing rather than emitting a stub"
            ),
        }
    }
}

impl std::error::Error for BenchRefuse {}

/// Proof that all four honesty conditions held. Private field: the only
/// constructors are [`BenchAdmit::admit`] / [`BenchAdmit::admit_with_sealed`].
#[derive(Debug, PartialEq, Eq)]
pub struct BenchAdmit {
    _proof: (),
}

impl BenchAdmit {
    /// Gate against [`production_sealed_digests`]. Empty sealed set →
    /// [`BenchRefuse::AnswerAgreement`] (corpus unsealed — no placeholders).
    pub fn admit(evidence: &BenchEvidence) -> Result<Self, BenchRefuse> {
        Self::admit_with_sealed(evidence, &production_sealed_digests())
    }

    /// Gate every refuse condition against an explicit sealed digest map.
    /// Fixtures inject **real** SHA-256 values; production passes the empty
    /// sealed map until the corpus seals answers.
    pub fn admit_with_sealed(
        evidence: &BenchEvidence,
        sealed: &BTreeMap<GraphName, Sha256>,
    ) -> Result<Self, BenchRefuse> {
        if evidence.opponent_pin != OpponentPin::sealed() {
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
        if sealed.is_empty() {
            let graph = corpus_graph_names()
                .into_iter()
                .next()
                .expect("corpus names non-empty");
            return Err(BenchRefuse::AnswerAgreement {
                graph: graph.clone(),
                expected: None,
                observed: evidence.observed_digests.get(&graph).copied(),
            });
        }
        for (graph, expected) in sealed {
            match evidence.observed_digests.get(graph) {
                Some(obs) if obs == expected => {}
                Some(obs) => {
                    return Err(BenchRefuse::AnswerAgreement {
                        graph: graph.clone(),
                        expected: Some(*expected),
                        observed: Some(*obs),
                    });
                }
                None => {
                    return Err(BenchRefuse::AnswerAgreement {
                        graph: graph.clone(),
                        expected: Some(*expected),
                        observed: None,
                    });
                }
            }
        }
        Ok(Self { _proof: () })
    }
}

/// Run the measurement, compute real observed digests, then gate through
/// [`BenchAdmit::admit`]. If measurement infra is unbuilt →
/// [`BenchRefuse::MeasurementUnbuilt`] — never an empty body / `BTreeMap::new()` stub.
pub fn emit_bench(
    opponent_pin: OpponentPin,
    caps: BenchCaps,
    commit: CommitState,
    graphs: &[GraphName],
) -> Result<BenchAdmit, BenchRefuse> {
    let observed_digests = measure_corpus(graphs)?;
    let evidence = BenchEvidence {
        opponent_pin,
        caps: Some(caps),
        commit,
        observed_digests,
    };
    BenchAdmit::admit(&evidence)
}

/// In-process TC measurement over the pinned corpus. Infrastructure is not
/// wired yet (`examples/bench_tc` deleted with the condemned shell runner) —
/// refuse loudly; never return empty digests.
fn measure_corpus(_graphs: &[GraphName]) -> Result<BTreeMap<GraphName, Sha256>, BenchRefuse> {
    Err(BenchRefuse::MeasurementUnbuilt)
}

/// In-process honesty path: measure → admit. No shell-out, no stub emit.
pub fn bench(graphs: &[String]) -> Result<(), BenchRefuse> {
    let graphs: Vec<GraphName> = if graphs.is_empty() {
        corpus_graph_names().to_vec()
    } else {
        graphs.iter().map(|g| GraphName::new(g)).collect()
    };
    let _admit = emit_bench(
        OpponentPin::sealed(),
        BenchCaps::required(),
        probe_commit_state(),
        &graphs,
    )?;
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
    pub name: GraphName,
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
    #[allow(dead_code)]
    // armed by verify_graph_bytes when the graph is absent from the manifest
    UnknownGraph(GraphName),
    /// Computed SHA-256 of bytes ≠ sealed manifest hash (tamper / wrong mirror).
    Sha256Mismatch {
        graph: GraphName,
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
    let text = std::fs::read_to_string(&path)
        .map_err(|e| DatasetRefuse::ManifestIo(format!("{}: {e}", path.display())))?;
    serde_json::from_str(&text).map_err(|e| DatasetRefuse::ManifestParse(e.to_string()))
}

/// Compute SHA-256 of `bytes` and compare to the sealed hex digest.
/// Real compute + compare — not a filename or length check.
pub fn verify_sha256(
    graph: &GraphName,
    bytes: &[u8],
    expected_hex: &str,
) -> Result<(), DatasetRefuse> {
    let observed = Sha256::hash(bytes).to_hex();
    let expected = expected_hex.trim().to_ascii_lowercase();
    if observed == expected {
        Ok(())
    } else {
        Err(DatasetRefuse::Sha256Mismatch {
            graph: graph.clone(),
            expected,
            observed,
        })
    }
}

/// Verify uncompressed graph bytes against a manifest entry (by name).
#[allow(dead_code)] // honesty-chain fixture door (manifest SHA-256 integrity tests)
pub fn verify_graph_bytes(
    manifest: &DatasetManifest,
    graph: &GraphName,
    bytes: &[u8],
) -> Result<(), DatasetRefuse> {
    let entry = manifest
        .graphs
        .iter()
        .find(|g| &g.name == graph)
        .ok_or_else(|| DatasetRefuse::UnknownGraph(graph.clone()))?;
    verify_sha256(graph, bytes, &entry.sha256)
}

/// In-process fetch verb: for each manifest graph, ensure `bench/data/{name}.txt`
/// holds bytes whose SHA-256 matches the sealed manifest. Existing files are
/// re-verified (tampered on-disk bytes refuse). Missing files download from
/// the sealed URL, gunzip, verify, then write — never write unverified bytes.
pub fn fetch_bench_data() -> Result<(), DatasetRefuse> {
    let root = crate::fsutil::repo_root().map_err(|e| DatasetRefuse::ManifestIo(e.to_string()))?;
    let manifest = load_dataset_manifest(&root)?;
    let data_dir = root.join("bench/data");
    std::fs::create_dir_all(&data_dir)
        .map_err(|e| DatasetRefuse::Write(format!("{}: {e}", data_dir.display())))?;

    for entry in &manifest.graphs {
        let out = data_dir.join(format!("{}.txt", entry.name));
        if out.is_file() {
            let bytes = std::fs::read(&out)
                .map_err(|e| DatasetRefuse::ManifestIo(format!("{}: {e}", out.display())))?;
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
#[allow(dead_code)] // honesty-chain fixture door (NIST / tamper SHA-256 tests)
pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::hash(data).to_hex()
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

    /// Fixture sealed digests are **real** SHA-256 of known corpus labels —
    /// never placeholder identity strings like `kyzo.tc.digest.*`.
    fn fixture_sealed_digests() -> BTreeMap<GraphName, Sha256> {
        corpus_graph_names()
            .into_iter()
            .map(|name| {
                GraphDigest::new(
                    name.clone(),
                    Sha256::hash(format!("fixture-answer:{}", name.as_str()).as_bytes()),
                )
            })
            .collect()
    }

    fn lawful_evidence(sealed: &BTreeMap<GraphName, Sha256>) -> BenchEvidence {
        BenchEvidence {
            opponent_pin: OpponentPin::sealed(),
            caps: Some(BenchCaps::required()),
            commit: CommitState::CleanTagged {
                tag: "bench-seal-v1".to_string(),
                sha: "abc123".to_string(),
            },
            observed_digests: sealed.clone(),
        }
    }

    #[test]
    fn production_sealed_digests_are_absent_not_placeholders() {
        assert!(
            production_sealed_digests().is_empty(),
            "no placeholder sealed-digest constant may exist until the corpus seals real SHA-256"
        );
    }

    #[test]
    fn fixture_refuse_opponent_pin_missing() {
        let sealed = fixture_sealed_digests();
        let mut e = lawful_evidence(&sealed);
        e.opponent_pin = OpponentPin::for_fixture("");
        assert_eq!(
            BenchAdmit::admit_with_sealed(&e, &sealed),
            Err(BenchRefuse::OpponentPin)
        );
    }

    #[test]
    fn fixture_refuse_opponent_pin_strawman_not_softenable() {
        let sealed = fixture_sealed_digests();
        let mut e = lawful_evidence(&sealed);
        e.opponent_pin = OpponentPin::for_fixture("kyzo.bench.strawman.easy.v0");
        assert_eq!(
            BenchAdmit::admit_with_sealed(&e, &sealed),
            Err(BenchRefuse::OpponentPin)
        );
    }

    #[test]
    fn fixture_refuse_caps_missing() {
        let sealed = fixture_sealed_digests();
        let mut e = lawful_evidence(&sealed);
        e.caps = None;
        assert_eq!(
            BenchAdmit::admit_with_sealed(&e, &sealed),
            Err(BenchRefuse::Caps)
        );
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
        let sealed = fixture_sealed_digests();
        let mut e = lawful_evidence(&sealed);
        e.commit = CommitState::Dirty;
        assert_eq!(
            BenchAdmit::admit_with_sealed(&e, &sealed),
            Err(BenchRefuse::Untagged)
        );
    }

    #[test]
    fn fixture_refuse_untagged_no_tag() {
        let sealed = fixture_sealed_digests();
        let mut e = lawful_evidence(&sealed);
        e.commit = CommitState::Untagged {
            sha: "abc123".to_string(),
        };
        assert_eq!(
            BenchAdmit::admit_with_sealed(&e, &sealed),
            Err(BenchRefuse::Untagged)
        );
    }

    #[test]
    fn fixture_refuse_answer_agreement_mismatch() {
        let sealed = fixture_sealed_digests();
        let mut e = lawful_evidence(&sealed);
        let graph = GraphName::new("email-Eu-core");
        let forged = Sha256::hash(b"forged-digest-bytes");
        e.observed_digests.insert(graph.clone(), forged);
        match BenchAdmit::admit_with_sealed(&e, &sealed) {
            Err(BenchRefuse::AnswerAgreement {
                graph: g,
                expected,
                observed,
            }) => {
                assert_eq!(g, graph);
                assert_eq!(expected, sealed.get(&graph).copied());
                assert_eq!(observed, Some(forged));
            }
            other => panic!("expected AnswerAgreement, got {other:?}"),
        }
    }

    #[test]
    fn fixture_refuse_answer_agreement_missing_observed() {
        let sealed = fixture_sealed_digests();
        let mut e = lawful_evidence(&sealed);
        e.observed_digests.clear();
        assert!(matches!(
            BenchAdmit::admit_with_sealed(&e, &sealed),
            Err(BenchRefuse::AnswerAgreement { observed: None, .. })
        ));
    }

    #[test]
    fn fixture_refuse_answer_agreement_when_production_sealed_absent() {
        let sealed = fixture_sealed_digests();
        let e = lawful_evidence(&sealed);
        assert!(matches!(
            BenchAdmit::admit(&e),
            Err(BenchRefuse::AnswerAgreement { expected: None, .. })
        ));
    }

    #[test]
    fn admit_all_four_conditions_yields_emit_token() {
        let sealed = fixture_sealed_digests();
        let _admit = BenchAdmit::admit_with_sealed(&lawful_evidence(&sealed), &sealed)
            .expect("lawful evidence must admit");
    }

    #[test]
    fn emit_bench_refuses_measurement_unbuilt_not_empty_stub() {
        let err = emit_bench(
            OpponentPin::sealed(),
            BenchCaps::required(),
            CommitState::CleanTagged {
                tag: "bench-seal-v1".into(),
                sha: "abc123".into(),
            },
            &corpus_graph_names(),
        );
        assert_eq!(err, Err(BenchRefuse::MeasurementUnbuilt));
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
        let graph = GraphName::new("email-Eu-core");
        let sealed = b"0 1\n2 3\n2 4\nemail-Eu-core fixture edge list\n";
        let expected = sha256_hex(sealed);
        verify_sha256(&graph, sealed, &expected)
            .expect("sealed bytes must verify against their own SHA-256");

        let mut tampered = sealed.to_vec();
        assert!(!tampered.is_empty());
        tampered[0] ^= 0xff; // byte-flip — length unchanged
        assert_eq!(
            tampered.len(),
            sealed.len(),
            "adversary preserves length; length check would falsely pass"
        );

        match verify_sha256(&graph, &tampered, &expected) {
            Err(DatasetRefuse::Sha256Mismatch {
                graph: g,
                expected: exp,
                observed,
            }) => {
                assert_eq!(g, graph);
                assert_eq!(exp, expected);
                assert_ne!(observed, expected);
                assert_eq!(observed, sha256_hex(&tampered));
            }
            other => panic!("expected Sha256Mismatch for tampered bytes, got {other:?}"),
        }
    }

    #[test]
    fn fixture_manifest_entry_verify_uses_sha256_not_name() {
        let graph = GraphName::new("wiki-Vote");
        let manifest = DatasetManifest {
            graphs: vec![GraphManifestEntry {
                name: graph.clone(),
                url: "https://snap.stanford.edu/data/wiki-Vote.txt.gz".into(),
                sha256: sha256_hex(b"honest wiki-Vote bytes"),
            }],
        };
        // Same graph name, wrong bytes → refuse (name match is not integrity).
        match verify_graph_bytes(&manifest, &graph, b"tampered wiki-Vote bytes!!!!") {
            Err(DatasetRefuse::Sha256Mismatch { graph: g, .. }) => {
                assert_eq!(g, graph);
            }
            other => panic!("expected Sha256Mismatch, got {other:?}"),
        }
        verify_graph_bytes(&manifest, &graph, b"honest wiki-Vote bytes")
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
/// entry). Successor of condemned `storage::tests::law` after the
/// storage→kyzo-model peel: `--lib` scopes to `kyzo-model`'s unit tests,
/// filter `format::tests::law` (law1 round-trip / law2 order embedding /
/// law3 corrupt-input — see `crates/kyzo-model/src/format/tests.rs`).
pub fn memcmp_invariant() -> Result<(), ProcessFailure> {
    let mut cmd = Command::new("cargo");
    cmd.args([
        "test",
        "-p",
        "kyzo-model",
        "--release",
        "--lib",
        "format::tests::law",
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
