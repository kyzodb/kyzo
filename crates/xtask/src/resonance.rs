/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Story #81's resonance gate (the five deterministic ontology checks),
//! wired into the gate program as an in-process verb (story #322): it runs
//! inside the same binary as every other verb, in dependency order, and
//! joins the seal on day one instead of living only in a separate CI job.
//! `RESONANCE_ROOT` still overrides the scanned root, which is what the
//! bite-proof harness uses against a throwaway rsync copy.

use std::fmt;

use crate::{allowlist, checks, fsutil};

/// Every way the resonance verb can refuse. Closed and phase-specific: a
/// caller (the gate summary, a human reading the exit) always knows which
/// phase failed, even though the underlying scan/parse/config error itself
/// is `fsutil`/`allowlist`'s own (pre-existing, not part of this story's
/// scope) `anyhow::Error`.
#[derive(Debug)]
pub enum ResonanceError {
    /// The workspace root could not be located.
    RepoRoot(anyhow::Error),
    /// The engine source tree could not be walked/parsed.
    SourceScan(anyhow::Error),
    /// The source tree was found but contained zero `.rs` files.
    NoSourceFiles,
    /// `resonance-allow.toml` could not be loaded.
    AllowlistLoad(anyhow::Error),
    /// A check's own config file (decode-surfaces.toml, agreements.toml)
    /// failed to load.
    CheckConfig {
        check: &'static str,
        source: anyhow::Error,
    },
    /// One or more checks found unwaived violations or stale waivers; each
    /// has already printed its own findings to stdout as it ran.
    ViolationsFound { failing_checks: Vec<&'static str> },
}

impl fmt::Display for ResonanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResonanceError::RepoRoot(e) => write!(f, "could not locate workspace root: {e:#}"),
            ResonanceError::SourceScan(e) => write!(f, "could not scan engine sources: {e:#}"),
            ResonanceError::NoSourceFiles => write!(
                f,
                "no source files found under crates/kyzo-core/src or crates/kyzo-bin/src"
            ),
            ResonanceError::AllowlistLoad(e) => write!(f, "could not load allowlist: {e:#}"),
            ResonanceError::CheckConfig { check, source } => {
                write!(f, "check {check} config error: {source:#}")
            }
            ResonanceError::ViolationsFound { failing_checks } => write!(
                f,
                "resonance gate found violations in: {}",
                failing_checks.join(", ")
            ),
        }
    }
}

impl std::error::Error for ResonanceError {}

/// Run the resonance gate. `only`, if given, runs a single named check
/// (`derive_bypass`, `panic_lint`, `copy_detector`, `dead_code_ratchet`,
/// `agreement_registry`, `allocation_admission`, `boundary_closure`,
/// `unchecked_arith`) — what the bite-proof harness uses.
pub fn run(only: Option<&str>) -> Result<(), ResonanceError> {
    let root = fsutil::repo_root().map_err(ResonanceError::RepoRoot)?;
    let files = fsutil::walk_engine_sources(&root).map_err(ResonanceError::SourceScan)?;
    if files.is_empty() {
        return Err(ResonanceError::NoSourceFiles);
    }
    let allow = allowlist::load(&root).map_err(ResonanceError::AllowlistLoad)?;

    let mut failing_checks: Vec<&'static str> = Vec::new();
    let want = |name: &str| only.map(|o| o == name).unwrap_or(true);

    if want("derive_bypass") && !run_derive_bypass(&files, &allow) {
        failing_checks.push("derive_bypass");
    }
    if want("panic_lint") {
        match run_panic_lint(&files, &allow, &root) {
            Ok(true) => {}
            Ok(false) => failing_checks.push("panic_lint"),
            Err(source) => {
                return Err(ResonanceError::CheckConfig {
                    check: "panic_lint",
                    source,
                });
            }
        }
    }
    if want("copy_detector") && !run_copy_detector(&files, &allow) {
        failing_checks.push("copy_detector");
    }
    if want("dead_code_ratchet") && !run_dead_code_ratchet(&files, &allow) {
        failing_checks.push("dead_code_ratchet");
    }
    if want("agreement_registry") {
        match run_agreement_registry(&files, &root) {
            Ok(true) => {}
            Ok(false) => failing_checks.push("agreement_registry"),
            Err(source) => {
                return Err(ResonanceError::CheckConfig {
                    check: "agreement_registry",
                    source,
                });
            }
        }
    }
    if want("allocation_admission") && !run_allocation_admission(&files) {
        failing_checks.push("allocation_admission");
    }
    if want("boundary_closure") && !run_boundary_closure(&files) {
        failing_checks.push("boundary_closure");
    }
    if want("unchecked_arith") {
        match run_unchecked_arith(&files, &root) {
            Ok(true) => {}
            Ok(false) => failing_checks.push("unchecked_arith"),
            Err(source) => {
                return Err(ResonanceError::CheckConfig {
                    check: "unchecked_arith",
                    source,
                });
            }
        }
    }

    if failing_checks.is_empty() {
        println!(
            "resonance gate: ALL CHECKS PASSED ({} source files scanned)",
            files.len()
        );
        Ok(())
    } else {
        eprintln!("FAIL: resonance gate found violations (see above).");
        Err(ResonanceError::ViolationsFound { failing_checks })
    }
}

fn run_derive_bypass(files: &[fsutil::SourceFile], allow: &allowlist::Allowlist) -> bool {
    println!("== check 1: derive-bypass detector ==");
    let (violations, stale) = checks::derive_bypass::check(files, allow);
    for v in &violations {
        println!(
            "FAIL {}:{}: `{}` derives {:?} but also has a fallible `{}` (line {}) — the derive bypasses the constructor's invariant",
            v.file, v.def_line, v.type_name, v.derives, v.ctor_name, v.ctor_line
        );
    }
    for s in &stale {
        println!("FAIL (stale allowlist) {s}");
    }
    let ok = violations.is_empty() && stale.is_empty();
    println!(
        "check 1: {} ({} violation(s), {} stale waiver(s))",
        if ok { "PASS" } else { "FAIL" },
        violations.len(),
        stale.len()
    );
    ok
}

fn run_panic_lint(
    files: &[fsutil::SourceFile],
    allow: &allowlist::Allowlist,
    root: &std::path::Path,
) -> Result<bool, anyhow::Error> {
    println!("== check 2: panic-on-hostile-bytes lint ==");
    let surfaces = checks::panic_lint::load_config(root)?;
    let (occurrences, missing) = checks::panic_lint::check(files, &surfaces, allow);
    for m in &missing {
        println!("FAIL {m}");
    }
    for o in &occurrences {
        println!(
            "FAIL {}:{} in `{}`: {}(...) reachable from a declared decode surface",
            o.file, o.line, o.function, o.kind
        );
    }
    let ok = occurrences.is_empty() && missing.is_empty();
    println!(
        "check 2: {} ({} occurrence(s), {} config problem(s))",
        if ok { "PASS" } else { "FAIL" },
        occurrences.len(),
        missing.len()
    );
    Ok(ok)
}

fn run_copy_detector(files: &[fsutil::SourceFile], allow: &allowlist::Allowlist) -> bool {
    println!("== check 3: copy-detector ==");
    let (violations, _pairs, units) = checks::copy_detector::check(files, allow);
    for v in &violations {
        println!(
            "FAIL near-identical bodies (similarity {:.2}): {}:{} `{}`  <->  {}:{} `{}`",
            v.similarity, v.file_a, v.line_a, v.label_a, v.file_b, v.line_b, v.label_b
        );
    }
    let ok = violations.is_empty();
    println!(
        "check 3: {} ({} unwaived near-duplicate pair(s), {} comparison units >= {} tokens)",
        if ok { "PASS" } else { "FAIL" },
        violations.len(),
        units.len(),
        checks::copy_detector::MIN_TOKENS
    );
    ok
}

fn run_dead_code_ratchet(files: &[fsutil::SourceFile], allow: &allowlist::Allowlist) -> bool {
    println!("== check 4: dead-concept ratchet ==");
    let (violations, stale) = checks::dead_code_ratchet::check(files, allow);
    for v in &violations {
        println!("FAIL {}:{}: uncited `{}`", v.file, v.line, v.attr_text);
    }
    for s in &stale {
        println!("FAIL (stale allowlist) {s}");
    }
    let ok = violations.is_empty() && stale.is_empty();
    println!(
        "check 4: {} ({} uncited, {} stale waiver(s))",
        if ok { "PASS" } else { "FAIL" },
        violations.len(),
        stale.len()
    );
    ok
}

fn run_allocation_admission(files: &[fsutil::SourceFile]) -> bool {
    println!("== check: allocation-admission ratchet ==");
    let violations = checks::allocation_admission::check(files);
    for v in &violations {
        println!(
            "FAIL {}:{}: `{}` caps its size inline with `.min(...)` — route the \
             caller-declared size through `crate::capacity::admit(declared, available)` instead",
            v.file, v.line, v.call
        );
    }
    let ok = violations.is_empty();
    println!(
        "check: allocation-admission {} ({} inline-cap site(s))",
        if ok { "PASS" } else { "FAIL" },
        violations.len()
    );
    ok
}

fn run_boundary_closure(files: &[fsutil::SourceFile]) -> bool {
    println!("== check: boundary-closure ratchet ==");
    let violations = checks::boundary_closure::check(files);
    for v in &violations {
        println!("FAIL {}:{} [{}]: {}", v.file, v.line, v.shape, v.detail);
    }
    let ok = violations.is_empty();
    println!(
        "check: boundary-closure {} ({} condemned-shape site(s))",
        if ok { "PASS" } else { "FAIL" },
        violations.len()
    );
    ok
}

fn run_unchecked_arith(
    files: &[fsutil::SourceFile],
    root: &std::path::Path,
) -> Result<bool, anyhow::Error> {
    println!("== check: unchecked-arith named-invariant ratchet ==");
    let baseline = checks::unchecked_arith::load_baseline(root)
        .map_err(|e| anyhow::anyhow!(e))?;
    let examples = checks::unchecked_arith::walk_examples(root)?;
    let mut violations = checks::unchecked_arith::check(files);
    violations.extend(checks::unchecked_arith::check(&examples));
    violations.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
    for v in &violations {
        println!(
            "FAIL {}:{}: `{}` lacks an adjacent `// INVARIANT(Name): …` proof \
             (unchecked arithmetic requires a named invariant at the same rung as unsafe)",
            v.file, v.line, v.method
        );
    }
    let n = violations.len();
    if n > baseline {
        eprintln!(
            "FAIL unchecked-arith: {n} uncommented site(s) exceeds baseline {baseline} \
             (crates/xtask/unchecked-arith-baseline.json)"
        );
        println!(
            "check: unchecked-arith FAIL ({n} uncommented, baseline {baseline})"
        );
        return Ok(false);
    }
    if n < baseline {
        eprintln!(
            "RATCHET IMPROVED unchecked-arith: {n} < baseline {baseline} — tighten \
             crates/xtask/unchecked-arith-baseline.json to {n}"
        );
        println!(
            "check: unchecked-arith FAIL (baseline stale: {n} < {baseline})"
        );
        return Ok(false);
    }
    println!(
        "check: unchecked-arith PASS ({n} uncommented; baseline {baseline})"
    );
    Ok(true)
}

fn run_agreement_registry(
    files: &[fsutil::SourceFile],
    root: &std::path::Path,
) -> Result<bool, anyhow::Error> {
    println!("== check 5: agreement-law registry ==");
    let registry = checks::agreement_registry::load(root)?;
    let violations = checks::agreement_registry::check(files, &registry);
    for v in &violations {
        println!(
            "FAIL registry entry \"{}\" ({}::{}): {}",
            v.name, v.file, v.test_fn, v.reason
        );
    }
    let ok = violations.is_empty();
    println!(
        "check 5: {} ({} law(s) registered, {} missing)",
        if ok { "PASS" } else { "FAIL" },
        registry.len(),
        violations.len()
    );
    Ok(ok)
}
