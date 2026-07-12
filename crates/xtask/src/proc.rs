/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The one subprocess primitive every gate verb that shells out to `cargo`
//! or a check script uses, and the one closed refusal type all of them
//! share on failure (story #322: closed refusal enums, no catch-alls, no
//! erased failures). The verb name identifies which step failed — a
//! `GateError` names its variant per verb and carries this as the payload,
//! so no separate near-identical error type is needed per verb.

use std::fmt;
use std::process::Command;

/// Every way running a gate verb's underlying subprocess can refuse.
#[derive(Debug)]
pub enum ProcessFailure {
    /// The subprocess could not even be spawned (verb name, OS error).
    Spawn(&'static str, std::io::Error),
    /// The subprocess ran and exited with this nonzero code.
    ExitCode(&'static str, i32),
    /// The subprocess was killed by a signal before it could exit.
    Signal(&'static str),
}

impl fmt::Display for ProcessFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProcessFailure::Spawn(verb, e) => write!(f, "{verb}: failed to spawn subprocess: {e}"),
            ProcessFailure::ExitCode(verb, code) => {
                write!(f, "{verb}: exited with status {code}")
            }
            ProcessFailure::Signal(verb) => write!(f, "{verb}: terminated by signal"),
        }
    }
}

impl std::error::Error for ProcessFailure {}

/// Run `cmd` to completion with inherited stdio, tagging any failure with
/// `verb` (the gate step name reported in a `GateError`).
pub fn run_step(verb: &'static str, mut cmd: Command) -> Result<(), ProcessFailure> {
    let status = cmd.status().map_err(|e| ProcessFailure::Spawn(verb, e))?;
    match status.code() {
        Some(0) => Ok(()),
        Some(code) => Err(ProcessFailure::ExitCode(verb, code)),
        None => Err(ProcessFailure::Signal(verb)),
    }
}
