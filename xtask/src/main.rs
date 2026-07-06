/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! xtask: the resonance gate (story #81). `cargo run -p xtask -- resonance`
//! runs all five deterministic ontology checks over the workspace rooted at
//! `RESONANCE_ROOT` (default: the real checkout xtask itself lives in) and
//! exits non-zero if any check finds an unwaived violation, a stale
//! allowlist/registry entry, or a config error. Each check can also run
//! alone (`-- resonance --only <name>`) — what the bite-proof harness uses
//! against a throwaway rsync copy.

mod allowlist;
mod checks;
mod fsutil;
mod synutil;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("resonance") => run_resonance(&args[1..]),
        _ => {
            eprintln!("usage: cargo run -p xtask -- resonance [--only <check>]");
            eprintln!(
                "checks: derive_bypass, panic_lint, copy_detector, dead_code_ratchet, agreement_registry"
            );
            ExitCode::FAILURE
        }
    }
}

fn run_resonance(args: &[String]) -> ExitCode {
    let only = args
        .iter()
        .position(|a| a == "--only")
        .and_then(|i| args.get(i + 1))
        .cloned();

    let root = match fsutil::repo_root() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("FAIL resonance gate: {e:#}");
            return ExitCode::FAILURE;
        }
    };

    let files = match fsutil::walk_engine_sources(&root) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("FAIL resonance gate: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    if files.is_empty() {
        eprintln!(
            "FAIL resonance gate: no source files found under kyzo-core/src or kyzo-bin/src at {}",
            root.display()
        );
        return ExitCode::FAILURE;
    }

    let allow = match allowlist::load(&root) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("FAIL resonance gate: {e:#}");
            return ExitCode::FAILURE;
        }
    };

    let mut overall_ok = true;
    let want = |name: &str| only.as_deref().map(|o| o == name).unwrap_or(true);

    if want("derive_bypass") {
        overall_ok &= run_derive_bypass(&files, &allow);
    }
    if want("panic_lint") {
        overall_ok &= run_panic_lint(&files, &allow, &root);
    }
    if want("copy_detector") {
        overall_ok &= run_copy_detector(&files, &allow);
    }
    if want("dead_code_ratchet") {
        overall_ok &= run_dead_code_ratchet(&files, &allow);
    }
    if want("agreement_registry") {
        overall_ok &= run_agreement_registry(&files, &root);
    }

    if overall_ok {
        println!(
            "resonance gate: ALL CHECKS PASSED ({} source files scanned)",
            files.len()
        );
        ExitCode::SUCCESS
    } else {
        eprintln!("FAIL: resonance gate found violations (see above).");
        ExitCode::FAILURE
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
) -> bool {
    println!("== check 2: panic-on-hostile-bytes lint ==");
    let surfaces = match checks::panic_lint::load_config(root) {
        Ok(s) => s,
        Err(e) => {
            println!("FAIL check 2 config: {e:#}");
            return false;
        }
    };
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
    ok
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

fn run_agreement_registry(files: &[fsutil::SourceFile], root: &std::path::Path) -> bool {
    println!("== check 5: agreement-law registry ==");
    let registry = match checks::agreement_registry::load(root) {
        Ok(r) => r,
        Err(e) => {
            println!("FAIL check 5 config: {e:#}");
            return false;
        }
    };
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
    ok
}
