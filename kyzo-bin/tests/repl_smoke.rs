/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! RUNTIME proof for the `kyzo` binary. `cargo check --workspace` proves the
//! CLI COMPILES; it does not prove the binary RUNS. This spawns the real
//! built binary (`CARGO_BIN_EXE_kyzo`), drives a minimal valid script through
//! the REPL/CLI path over piped stdin, and asserts both a successful exit and
//! the expected rendered output — the end-to-end path an actual user takes,
//! which no in-process test exercises. It runs in the ordinary workspace gate.

use std::io::Write;
use std::process::{Command, Stdio};

/// Spawn `kyzo repl --engine mem`, feed it one script line, and read back the
/// rendered table. The ephemeral `mem` engine keeps the test hermetic; a
/// temp cwd keeps the REPL's `.kyzo_repl_history` out of the source tree.
#[test]
fn repl_runs_a_script_end_to_end() {
    let cwd = tempfile::tempdir().expect("temp cwd");

    let mut child = Command::new(env!("CARGO_BIN_EXE_kyzo"))
        .args(["repl", "--engine", "mem"])
        .current_dir(cwd.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the kyzo binary spawns");

    // One valid script; dropping stdin signals EOF, so the REPL loop ends and
    // the process exits cleanly rather than blocking on the next prompt.
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(b"?[x] <- [[1], [2], [3]]\n")
        .expect("pipe the script to the REPL");

    let out = child.wait_with_output().expect("the kyzo process exits");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "kyzo exited non-zero ({:?})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        out.status
    );
    // The REPL actually started (banner), and the query actually evaluated and
    // rendered its three rows through the real CLI output path.
    assert!(
        stdout.contains("Welcome to the KyzoDB REPL"),
        "REPL banner missing — the binary did not reach the REPL loop:\n{stdout}"
    );
    for want in ["1", "2", "3"] {
        assert!(
            stdout.contains(want),
            "rendered output is missing row `{want}`:\n{stdout}"
        );
    }
}

/// A malformed script must NOT crash the binary: the REPL reports the error
/// (on stderr) and still exits cleanly at EOF. Proves the CLI's error path is
/// wired to the engine's typed refusals, not to a panic.
#[test]
fn repl_survives_a_bad_script() {
    let cwd = tempfile::tempdir().expect("temp cwd");

    let mut child = Command::new(env!("CARGO_BIN_EXE_kyzo"))
        .args(["repl", "--engine", "mem"])
        .current_dir(cwd.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the kyzo binary spawns");

    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(b"this is not a valid kyzoscript program\n")
        .expect("pipe the bad script to the REPL");

    let out = child.wait_with_output().expect("the kyzo process exits");
    assert!(
        out.status.success(),
        "a bad script must be reported, not crash the process ({:?})\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}
