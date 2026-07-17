/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Trybuild harness: Open spent by commit/abort, and typed CommitFailure
//! refuses erased-carrier `downcast_ref` (story #302 T5).

#[test]
fn write_tx_use_after_commit_refused() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/write_tx_use_after_commit.rs");
}

#[test]
fn write_tx_use_after_abort_refused() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/write_tx_use_after_abort.rs");
}

#[test]
fn commit_failure_downcast_ref_refused() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/commit_failure_downcast_ref_refused.rs");
}
