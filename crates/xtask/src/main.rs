/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! xtask: the gate program (story #322). Every invocable action —
//! developer, agent, hook, or CI — is one typed verb of this one program,
//! run through the container (`docker compose run --rm kyzo-dev cargo
//! xtask <verb>`). There is no other door: no justfile, no scripts run
//! directly, no floating toolchain. `cargo xtask gate` is the seal.
//!
//! Story #81's resonance gate (five deterministic ontology checks) is the
//! `resonance` verb; it joins `gate`'s sequence on day one.

mod allowlist;
mod checks;
mod fsutil;
mod gate;
mod proc;
mod resonance;
mod synutil;
mod verbs;

use std::fmt;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// The one closed sum of every verb's own closed refusal type. `main`'s
/// dispatch match arm is the only place a per-verb error is converted into
/// this — never a trait object, never an erased first-party failure
/// (CLAUDE.md law 33): each variant still names exactly which verb's own
/// typed error it carries.
enum XtaskError {
    RepoRoot(anyhow::Error),
    Gate(gate::GateError),
    Process(proc::ProcessFailure),
    Resonance(resonance::ResonanceError),
    UnsafeCheck(checks::unsafe_check::UnsafeCheckError),
    PureRust(checks::pure_rust::PureRustError),
    BuildScriptSandbox(checks::build_script_sandbox::BuildScriptSandboxError),
    Authority(checks::authority_graph::AuthorityError),
}

impl fmt::Display for XtaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            XtaskError::RepoRoot(e) => write!(f, "could not locate workspace root: {e:#}"),
            XtaskError::Gate(e) => write!(f, "{e}"),
            XtaskError::Process(e) => write!(f, "{e}"),
            XtaskError::Resonance(e) => write!(f, "{e}"),
            XtaskError::UnsafeCheck(e) => write!(f, "{e}"),
            XtaskError::PureRust(e) => write!(f, "{e}"),
            XtaskError::BuildScriptSandbox(e) => write!(f, "{e}"),
            XtaskError::Authority(e) => write!(f, "{e}"),
        }
    }
}

#[derive(Parser)]
#[command(
    name = "xtask",
    about = "KyzoDB's gate program: one door, every caller, one verdict."
)]
struct Cli {
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Subcommand)]
enum Verb {
    /// The one-command seal: everything that must be true to close a story.
    Gate,
    /// The environment fingerprint: cgroup memory limit, test thread count,
    /// core count, toolchain version.
    EnvReport,
    /// `cargo check --workspace --all-targets`.
    Check,
    /// `cargo fmt --check` over every first-party package.
    Fmt,
    /// `cargo clippy --no-deps -- -D warnings` over every first-party package.
    Clippy,
    /// The unsafe law: forbid present, zero allows, no lying docs.
    Unsafe,
    /// No C/C++-toolchain crate in the engine dependency tree.
    PureRust,
    /// Every build-script target, net-isolated and snapshot-diffed for
    /// writes outside its own OUT_DIR.
    BuildScriptSandbox,
    /// The Type Authority Graph: self-test, ratchet, artifact freshness.
    Authority {
        /// Regenerate authority/authority-map.json and
        /// authority-report.md instead of running the gate check (the
        /// ported tool's report mode).
        #[arg(long)]
        write: bool,
        /// Tighten crates/xtask/authority-baseline.json to the current tree's
        /// finding counts instead of running the gate check.
        #[arg(long)]
        update_baseline: bool,
    },
    /// The five resonance-gate ontology checks (story #81).
    Resonance {
        /// Run a single named check instead of all five.
        #[arg(long)]
        only: Option<String>,
    },
    /// The full first-party test suite.
    Test,
    /// The bench-internals/fuzz-internals feature configuration's tests.
    TestFeatures,
    /// The whole test suite under the `release-checked` profile (overflow-checks live).
    TestReleaseChecked,
    /// The bench-internals/fuzz-internals feature tests under the `release-checked` profile.
    TestFeaturesReleaseChecked,
    /// Run the freshly-built binary in the container.
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// The transitive-closure benchmark over the SNAP graphs.
    Bench {
        #[arg(trailing_var_arg = true)]
        graphs: Vec<String>,
    },
    /// MPL header preservation over every tracked .rs file.
    MplHeaders,
    /// `cargo deny check bans licenses advisories`.
    Deny,
    /// Supply-chain vetting (`cargo vet check`).
    SupplyChain,
    /// The memcmp on-disk-format tripwire.
    MemcmpInvariant,
    /// Every fuzz target, 60s smoke each (requires a nightly toolchain).
    FuzzSmoke,
    /// The cross-run/thread/architecture determinism campaign.
    DeterminismCampaign {
        /// Where to write this run's digest.
        out: String,
        /// An earlier run's digest to compare against (e.g. a downloaded
        /// cross-architecture artifact).
        compare: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // Validates we are being run from within the repo; no verb below
    // consumes the path itself (each shells out relative to the container's
    // working directory).
    if let Err(e) = fsutil::repo_root() {
        eprintln!("FAIL: {}", XtaskError::RepoRoot(e));
        return ExitCode::FAILURE;
    }

    let result: Result<(), XtaskError> = match cli.verb {
        Verb::Gate => gate::run().map_err(XtaskError::Gate),
        Verb::EnvReport => verbs::env_report().map_err(XtaskError::Process),
        Verb::Check => verbs::check().map_err(XtaskError::Process),
        Verb::Fmt => verbs::fmt().map_err(XtaskError::Process),
        Verb::Clippy => verbs::clippy().map_err(XtaskError::Process),
        Verb::Unsafe => verbs::unsafe_check().map_err(XtaskError::UnsafeCheck),
        Verb::PureRust => verbs::pure_rust().map_err(XtaskError::PureRust),
        Verb::BuildScriptSandbox => {
            verbs::build_script_sandbox().map_err(XtaskError::BuildScriptSandbox)
        }
        Verb::Authority {
            write,
            update_baseline,
        } => match (write, update_baseline) {
            (true, _) => verbs::authority_write().map_err(XtaskError::Authority),
            (false, true) => verbs::authority_update_baseline().map_err(XtaskError::Authority),
            (false, false) => verbs::authority().map_err(XtaskError::Authority),
        },
        Verb::Resonance { only } => resonance::run(only.as_deref()).map_err(XtaskError::Resonance),
        Verb::Test => verbs::test().map_err(XtaskError::Process),
        Verb::TestFeatures => verbs::test_features().map_err(XtaskError::Process),
        Verb::TestReleaseChecked => verbs::test_release_checked().map_err(XtaskError::Process),
        Verb::TestFeaturesReleaseChecked => {
            verbs::test_features_release_checked().map_err(XtaskError::Process)
        }
        Verb::Run { args } => verbs::run_bin(&args).map_err(XtaskError::Process),
        Verb::Bench { graphs } => verbs::bench(&graphs).map_err(XtaskError::Process),
        Verb::MplHeaders => verbs::mpl_headers().map_err(XtaskError::Process),
        Verb::Deny => verbs::deny().map_err(XtaskError::Process),
        Verb::SupplyChain => verbs::supply_chain().map_err(XtaskError::Process),
        Verb::MemcmpInvariant => verbs::memcmp_invariant().map_err(XtaskError::Process),
        Verb::FuzzSmoke => verbs::fuzz_smoke().map_err(XtaskError::Process),
        Verb::DeterminismCampaign { out, compare } => {
            verbs::determinism_campaign(&out, compare.as_deref()).map_err(XtaskError::Process)
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("FAIL: {e}");
            ExitCode::FAILURE
        }
    }
}
