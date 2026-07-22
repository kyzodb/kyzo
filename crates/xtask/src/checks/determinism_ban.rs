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
//! **Scope: `crates/kyzo-core/src/store/` and `crates/kyzo-core/src/session/`**
//! — the durability / commit / sealed surface, PLUS the admission surface
//! where `IncarnationId` minting and other admission-time determinism
//! decisions actually live (`session/admit.rs`), where determinism is *law*,
//! not telemetry. Widened from `store/` alone after an audit found the
//! admission seam undisclosed and unscanned. The engine's own clock is
//! `CommitOrdinal` (a monotone counter), never wall time; its one approved
//! randomness (the `IncarnationId` entropy arm, seat 62) is minted at the
//! admission seam in `session/admit.rs` and passed in — it never appears on the
//! store surface. Perf timing in `exec/` / `project/` is deliberately **out of
//! scope**: seat 83 excludes wall-clock worker races from the byte-identical
//! set, so an `Instant::now()` stopwatch there is observation, not authority.
//!
//! **No allowlist:** on this surface there is no lawful wall-clock read and
//! no lawful unseeded draw — any hit is a real determinism breach, not an
//! exception to grant. Seeded RNG (`ChaCha20Rng::from_seed`, content-addressed
//! draws) is deterministic and is not among the banned sources. Widening to
//! `session/` surfaces two real, pre-existing hits this check has never
//! scanned before — `session/mod.rs::current_validity`'s `SystemTime::now()`
//! (documented as "the engine's ONE wall-clock read", feeding `ValidityTs`
//! into committed bitemporal keys) and `session/json.rs::run_script_json`'s
//! `Instant::now()` (a response-envelope `"took"` timing field, never
//! committed). Both are reported by the check rather than silently carved
//! out here: whether either is a legitimate, separately-ruled exception
//! (the way `exec/`/`project/` is) is an architecture decision this check
//! does not make for itself.

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

/// True for the sealed/commit surface this ban governs: the store proper
/// plus the admission seam (`session/`) where `IncarnationId` minting and
/// other admission-time determinism decisions live.
fn is_sealed_surface(rel_path: &str) -> bool {
    rel_path.starts_with("crates/kyzo-core/src/store/")
        || rel_path.starts_with("crates/kyzo-core/src/session/")
}

/// Scan the sealed/commit surface for a wall-clock read or unseeded draw.
pub fn check(files: &[SourceFile]) -> Vec<Violation> {
    let mut violations = vec![];
    for f in files {
        if !is_sealed_surface(&f.rel_path) {
            continue;
        }
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
    fn ignores_perf_timing_off_the_sealed_surface() {
        // Seat 83: a stopwatch in exec/ is telemetry, not sealed authority.
        let f = parse(
            "crates/kyzo-core/src/exec/op/mod.rs",
            "fn run() { let _t0 = std::time::Instant::now(); }",
        );
        assert!(check(std::slice::from_ref(&f)).is_empty());
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
