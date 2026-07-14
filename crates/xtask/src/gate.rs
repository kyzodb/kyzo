/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The `gate` verb: the one-command seal. Runs env-report, check, fmt,
//! clippy, unsafe, pure-rust, authority, test, and test-features, plus the
//! resonance suite, in that dependency order — cheapest/most-arguable
//! checks first, the full test suite last — stopping at the first failure
//! (story #322's Engineering Choice, task 1).

use std::fmt;

use crate::checks::authority_graph::AuthorityError;
use crate::checks::build_script_sandbox::BuildScriptSandboxError;
use crate::checks::pure_rust::PureRustError;
use crate::checks::unsafe_check::UnsafeCheckError;
use crate::proc::ProcessFailure;
use crate::resonance::{self, ResonanceError};
use crate::verbs;

/// Every way `gate` can refuse, one variant per step it runs — a closed sum
/// of the underlying verb's own closed refusal type. A caller (a human
/// reading stderr, the CI gate-summary job) always knows exactly which step
/// failed and why, never an erased or generic failure.
#[derive(Debug)]
pub enum GateError {
    EnvReport(ProcessFailure),
    Check(ProcessFailure),
    Fmt(ProcessFailure),
    Clippy(ProcessFailure),
    Unsafe(UnsafeCheckError),
    PureRust(PureRustError),
    BuildScriptSandbox(BuildScriptSandboxError),
    Authority(AuthorityError),
    Resonance(ResonanceError),
    Test(ProcessFailure),
    TestFeatures(ProcessFailure),
}

impl fmt::Display for GateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GateError::EnvReport(e) => write!(f, "env-report step failed: {e}"),
            GateError::Check(e) => write!(f, "check step failed: {e}"),
            GateError::Fmt(e) => write!(f, "fmt step failed: {e}"),
            GateError::Clippy(e) => write!(f, "clippy step failed: {e}"),
            GateError::Unsafe(e) => write!(f, "unsafe step failed: {e}"),
            GateError::PureRust(e) => write!(f, "pure-rust step failed: {e}"),
            GateError::BuildScriptSandbox(e) => {
                write!(f, "build-script-sandbox step failed: {e}")
            }
            GateError::Authority(e) => write!(f, "authority step failed: {e}"),
            GateError::Resonance(e) => write!(f, "resonance step failed: {e}"),
            GateError::Test(e) => write!(f, "test step failed: {e}"),
            GateError::TestFeatures(e) => write!(f, "test-features step failed: {e}"),
        }
    }
}

impl std::error::Error for GateError {}

pub fn run() -> Result<(), GateError> {
    verbs::env_report().map_err(GateError::EnvReport)?;
    verbs::check().map_err(GateError::Check)?;
    verbs::fmt().map_err(GateError::Fmt)?;
    verbs::clippy().map_err(GateError::Clippy)?;
    verbs::unsafe_check().map_err(GateError::Unsafe)?;
    verbs::pure_rust().map_err(GateError::PureRust)?;
    verbs::build_script_sandbox().map_err(GateError::BuildScriptSandbox)?;
    verbs::authority().map_err(GateError::Authority)?;
    resonance::run(None).map_err(GateError::Resonance)?;
    verbs::test().map_err(GateError::Test)?;
    verbs::test_features().map_err(GateError::TestFeatures)?;

    println!("=== GATE PASSED ===");
    Ok(())
}
