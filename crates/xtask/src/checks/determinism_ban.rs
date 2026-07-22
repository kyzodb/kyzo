/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Determinism ban on the sealed/commit surface (decisions.md seats 25 / 45 /
//! 83 / 84): committed state, sealed artifacts, and compaction pace are pure
//! functions of Store facts. "Timer-tuning is Unconstructible" (25); "pace is a
//! pure function of commit debt — no wall-clock input" (45); "unseeded RNG on
//! sealed paths and racy wall-clock schedules that affect sealed artifact bytes
//! are unrepresentable" (84).
//!
//! **Scope: every file the walker sees — all of `crates/`, no directory
//! carve-outs.** Standing operator law: a check never pre-decides where its
//! law "really" applies; this check was previously scoped to `store/`, then
//! `store/`+`session/`, and each widening surfaced real hits the narrower
//! scope had silently hidden. The engine's own clock is `CommitOrdinal` (a
//! monotone counter), never wall time; its one approved randomness (the
//! `IncarnationId` entropy arm, seat 62) is minted at the admission seam in
//! `session/admit.rs` and passed in — it never appears on the store surface.
//!
//! **No allowlist:** any hit is reported, full stop — this check grants no
//! exceptions of its own. A wall-clock read that must exist (the one
//! `ValidityTs` mint door, a query-budget deadline, a campaign watchdog) is
//! an operator-audited ruling, never a scope carve-out written here. Seeded
//! RNG (`ChaCha20Rng::from_seed`, content-addressed draws) is deterministic
//! and is not among the banned sources; `#[cfg(test)] mod` scopes are
//! skipped by the shared scanner.

use crate::checks::banned_path::scan_banned_idents;
use crate::fsutil::SourceFile;

/// Non-deterministic sources forbidden on the sealed/commit surface: wall-clock
/// reads and unseeded / OS randomness. Matched on any path segment.
const BANNED_NONDET: &[&str] = &[
    "Instant",    // std::time::Instant — wall-clock stopwatch
    "SystemTime", // std::time::SystemTime — wall-clock
    "thread_rng", // unseeded thread-local RNG
    "OsRng",      // OS CSPRNG draw (the entropy arm lives in session/, not here)
    "getrandom",  // raw OS entropy
];

/// One non-determinism source found on the sealed surface.
pub struct Violation {
    pub file: String,
    pub line: usize,
    pub symbol: String,
}

/// Scan every walked file for a wall-clock read or unseeded draw — full
/// `crates/` scope, no directory predicate (standing operator law).
pub fn check(files: &[SourceFile]) -> Vec<Violation> {
    let mut violations = vec![];
    for f in files {
        for hit in scan_banned_idents(f, BANNED_NONDET) {
            violations.push(Violation {
                file: f.rel_path.clone(),
                line: hit.line,
                symbol: hit.ident,
            });
        }
    }
    violations
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(rel: &str, src: &str) -> SourceFile {
        SourceFile {
            rel_path: rel.to_string(),
            text: src.to_string(),
            ast: syn::parse_file(src).expect("fixture parses"),
        }
    }

    #[test]
    fn flags_wall_clock_on_the_commit_surface() {
        let f = parse(
            "crates/kyzo-core/src/store/sweep.rs",
            "fn seal() { let _t = std::time::SystemTime::now(); }",
        );
        let v = check(std::slice::from_ref(&f));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].symbol, "SystemTime");
    }

    #[test]
    fn flags_unseeded_rng_on_the_commit_surface() {
        let f = parse(
            "crates/kyzo-core/src/store/nonce.rs",
            "fn draw() { let mut r = rand::thread_rng(); let _ = &mut r; }",
        );
        let v = check(std::slice::from_ref(&f));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].symbol, "thread_rng");
    }

    #[test]
    fn flags_wall_clock_on_the_widened_session_admission_surface() {
        let f = parse(
            "crates/kyzo-core/src/session/mod.rs",
            "fn current_validity() { let _t = std::time::SystemTime::now(); }",
        );
        let v = check(std::slice::from_ref(&f));
        assert_eq!(v.len(), 1, "session/ must now be in scope for this ban");
        assert_eq!(v[0].symbol, "SystemTime");
    }

    #[test]
    fn flags_wall_clock_anywhere_in_the_tree_full_scope() {
        // The old scope silently exempted exec/, project/, and every crate
        // outside kyzo-core — the exact narrowing that hid live hits.
        for rel in [
            "crates/kyzo-core/src/exec/op/mod.rs",
            "crates/kyzo-core/src/project/vector/hnsw.rs",
            "crates/kyzo-oracle/src/eval.rs",
            "crates/xtask/src/gate.rs",
        ] {
            let f = parse(rel, "fn run() { let _t0 = std::time::Instant::now(); }");
            let v = check(std::slice::from_ref(&f));
            assert_eq!(v.len(), 1, "{rel} must be in scope — no directory is exempt");
        }
    }

    #[test]
    fn ignores_seeded_rng_and_test_scope() {
        let seeded = parse(
            "crates/kyzo-core/src/store/sweep.rs",
            "fn draw(seed: [u8;32]) { let _r = ChaCha20Rng::from_seed(seed); }",
        );
        assert!(
            check(std::slice::from_ref(&seeded)).is_empty(),
            "seeded RNG is deterministic — not a banned source"
        );
        let test_scope = parse(
            "crates/kyzo-core/src/store/sweep.rs",
            "#[cfg(test)] mod t { fn f() { let _ = std::time::Instant::now(); } }",
        );
        assert!(check(std::slice::from_ref(&test_scope)).is_empty());
    }
}
