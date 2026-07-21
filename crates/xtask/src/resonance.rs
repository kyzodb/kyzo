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
//!
//! Default output is quiet (GATE-OUTPUT): one GREEN line, or fail-only RED
//! lines plus the summary. `--verbose` restores per-check headers and PASS
//! chatter.

use std::fmt;

use clap::ValueEnum;

use crate::{allowlist, checks, fsutil};

/// The five story #81 resonance ontology checks. Illegal check names are
/// unconstructable at the CLI (`--only` is a [`ValueEnum`], not a free string).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum ResonanceCheck {
    DeriveBypass,
    PanicLint,
    CopyDetector,
    DeadCodeRatchet,
    AgreementRegistry,
}

impl ResonanceCheck {
    fn as_meter_name(self) -> &'static str {
        match self {
            ResonanceCheck::DeriveBypass => "derive_bypass",
            ResonanceCheck::PanicLint => "panic_lint",
            ResonanceCheck::CopyDetector => "copy_detector",
            ResonanceCheck::DeadCodeRatchet => "dead_code_ratchet",
            ResonanceCheck::AgreementRegistry => "agreement_registry",
        }
    }
}

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
    /// failing check's violation lines were already printed.
    ViolationsFound { failing_checks: Vec<&'static str> },
}

impl fmt::Display for ResonanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResonanceError::RepoRoot(e) => write!(f, "could not locate workspace root: {e:#}"),
            ResonanceError::SourceScan(e) => write!(f, "could not scan engine sources: {e:#}"),
            ResonanceError::NoSourceFiles => write!(
                f,
                "no source files found under crates/kyzo-core/src, crates/kyzo-bin/src, or crates/kyzo-model/src"
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

/// Per-check output buffer: quiet mode holds violation lines until fail;
/// verbose mode streams the historical chatty headers/PASS/FAIL shape.
struct CheckOut {
    verbose: bool,
    lines: Vec<String>,
}

impl CheckOut {
    fn new(verbose: bool) -> Self {
        Self {
            verbose,
            lines: Vec::new(),
        }
    }

    fn header(&self, msg: &str) {
        if self.verbose {
            println!("{msg}");
        }
    }

    /// Record one violation. Verbose prints immediately with a `FAIL` prefix;
    /// quiet buffers the `file:line — reason` line for emit-on-fail.
    fn violation(&mut self, line: impl Into<String>) {
        let line = line.into();
        if self.verbose {
            println!("FAIL {line}");
        }
        self.lines.push(line);
    }

    /// Verbose-only chatter (PASS summaries, counts). Never emitted in quiet.
    fn note(&self, msg: impl AsRef<str>) {
        if self.verbose {
            println!("{}", msg.as_ref());
        }
    }

    /// Verbose-only stderr note (baseline ratchet messages).
    fn note_err(&self, msg: impl AsRef<str>) {
        if self.verbose {
            eprintln!("{}", msg.as_ref());
        }
    }

    /// Emit buffered violation lines when the check failed (quiet path).
    /// Returns whether the check passed (`lines` empty).
    fn finish_ok(&self) -> bool {
        let ok = self.lines.is_empty();
        if !ok && !self.verbose {
            for line in &self.lines {
                println!("{line}");
            }
        }
        ok
    }
}

/// Run the resonance gate. `only`, if given, runs a single story #81 check.
/// When `only` is `None`, all five plus the later ratchets
/// (`allocation_admission`, `boundary_closure`, `unchecked_arith`) run.
/// Default output is quiet; `verbose` restores per-check chatter.
pub fn run(only: Option<ResonanceCheck>, verbose: bool) -> Result<(), ResonanceError> {
    let root = fsutil::repo_root().map_err(ResonanceError::RepoRoot)?;
    let files = fsutil::walk_engine_sources(&root).map_err(ResonanceError::SourceScan)?;
    if files.is_empty() {
        return Err(ResonanceError::NoSourceFiles);
    }
    let allow = allowlist::load(&root).map_err(ResonanceError::AllowlistLoad)?;

    let mut failing_checks: Vec<&'static str> = Vec::new();
    let mut checks_run: usize = 0;
    let want = |check: ResonanceCheck| only.map(|o| o == check).unwrap_or(true);
    let run_later_ratchets = only.is_none();

    if want(ResonanceCheck::DeriveBypass) {
        checks_run += 1;
        if !run_derive_bypass(&files, &allow, verbose) {
            failing_checks.push(ResonanceCheck::DeriveBypass.as_meter_name());
        }
    }
    if want(ResonanceCheck::PanicLint) {
        checks_run += 1;
        match run_panic_lint(&files, &allow, &root, verbose) {
            Ok(true) => {}
            Ok(false) => failing_checks.push(ResonanceCheck::PanicLint.as_meter_name()),
            Err(source) => {
                return Err(ResonanceError::CheckConfig {
                    check: ResonanceCheck::PanicLint.as_meter_name(),
                    source,
                });
            }
        }
    }
    if want(ResonanceCheck::CopyDetector) {
        checks_run += 1;
        if !run_copy_detector(&files, &allow, verbose) {
            failing_checks.push(ResonanceCheck::CopyDetector.as_meter_name());
        }
    }
    if want(ResonanceCheck::DeadCodeRatchet) {
        checks_run += 1;
        if !run_dead_code_ratchet(&files, &allow, verbose) {
            failing_checks.push(ResonanceCheck::DeadCodeRatchet.as_meter_name());
        }
    }
    if want(ResonanceCheck::AgreementRegistry) {
        checks_run += 1;
        match run_agreement_registry(&files, &root, verbose) {
            Ok(true) => {}
            Ok(false) => failing_checks.push(ResonanceCheck::AgreementRegistry.as_meter_name()),
            Err(source) => {
                return Err(ResonanceError::CheckConfig {
                    check: ResonanceCheck::AgreementRegistry.as_meter_name(),
                    source,
                });
            }
        }
    }
    if run_later_ratchets {
        checks_run += 1;
        if !run_allocation_admission(&files, verbose) {
            failing_checks.push("allocation_admission");
        }
        checks_run += 1;
        if !run_boundary_closure(&files, verbose) {
            failing_checks.push("boundary_closure");
        }
        checks_run += 1;
        if !run_peer_dial_ban(&files, verbose) {
            failing_checks.push("peer_dial_ban");
        }
        checks_run += 1;
        if !run_serializer_authority(&files, verbose) {
            failing_checks.push("serializer_authority");
        }
        checks_run += 1;
        if !run_determinism_ban(&files, verbose) {
            failing_checks.push("determinism_ban");
        }
        checks_run += 1;
        match run_unchecked_arith(&files, &root, verbose) {
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
        if verbose {
            println!(
                "resonance gate: ALL CHECKS PASSED ({} source files scanned)",
                files.len()
            );
        } else {
            println!(
                "resonance gate clears ({checks_run} checks, {} files)",
                files.len()
            );
        }
        Ok(())
    } else {
        if verbose {
            eprintln!("FAIL: resonance gate found violations (see above).");
        }
        Err(ResonanceError::ViolationsFound { failing_checks })
    }
}

fn run_derive_bypass(
    files: &[fsutil::SourceFile],
    allow: &allowlist::Allowlist,
    verbose: bool,
) -> bool {
    let mut out = CheckOut::new(verbose);
    out.header("== check 1: derive-bypass detector ==");
    let (violations, stale) = checks::derive_bypass::check(files, allow);
    for v in &violations {
        out.violation(format!(
            "{}:{} — `{}` derives {:?} but also has a fallible `{}` (line {}) — the derive bypasses the constructor's invariant",
            v.file, v.def_line, v.type_name, v.derives, v.ctor_name, v.ctor_line
        ));
    }
    for s in &stale {
        out.violation(format!("allowlist:0 — stale allowlist: {s}"));
    }
    let ok = out.finish_ok();
    out.note(format!(
        "check 1: {} ({} violation(s), {} stale waiver(s))",
        if ok { "PASS" } else { "FAIL" },
        violations.len(),
        stale.len()
    ));
    ok
}

fn run_panic_lint(
    files: &[fsutil::SourceFile],
    allow: &allowlist::Allowlist,
    root: &std::path::Path,
    verbose: bool,
) -> Result<bool, anyhow::Error> {
    let mut out = CheckOut::new(verbose);
    out.header("== check 2: panic-on-hostile-bytes lint ==");
    let surfaces = checks::panic_lint::load_config(root)?;
    let (occurrences, missing) = checks::panic_lint::check(files, &surfaces, allow);
    for m in &missing {
        out.violation(format!("config:0 — {m}"));
    }
    for o in &occurrences {
        out.violation(format!(
            "{}:{} — in `{}`: {}(...) reachable from a declared decode surface",
            o.file, o.line, o.function, o.kind
        ));
    }
    let ok = out.finish_ok();
    out.note(format!(
        "check 2: {} ({} occurrence(s), {} config problem(s))",
        if ok { "PASS" } else { "FAIL" },
        occurrences.len(),
        missing.len()
    ));
    Ok(ok)
}

fn run_copy_detector(
    files: &[fsutil::SourceFile],
    allow: &allowlist::Allowlist,
    verbose: bool,
) -> bool {
    let mut out = CheckOut::new(verbose);
    out.header("== check 3: copy-detector ==");
    let (violations, _pairs, units, stale) = checks::copy_detector::check(files, allow);
    for v in &violations {
        out.violation(format!(
            "{}:{} — near-identical bodies (similarity {:.2}): `{}`  <->  {}:{} `{}`",
            v.file_a, v.line_a, v.similarity, v.label_a, v.file_b, v.line_b, v.label_b
        ));
    }
    for s in &stale {
        out.violation(format!("allowlist:0 — stale allowlist: {s}"));
    }
    let ok = out.finish_ok();
    out.note(format!(
        "check 3: {} ({} unwaived near-duplicate pair(s), {} stale waiver member(s), {} comparison units >= {} tokens)",
        if ok { "PASS" } else { "FAIL" },
        violations.len(),
        stale.len(),
        units.len(),
        checks::copy_detector::MIN_TOKENS
    ));
    ok
}

fn run_dead_code_ratchet(
    files: &[fsutil::SourceFile],
    allow: &allowlist::Allowlist,
    verbose: bool,
) -> bool {
    let mut out = CheckOut::new(verbose);
    out.header("== check 4: dead-concept ratchet ==");
    let (violations, stale) = checks::dead_code_ratchet::check(files, allow);
    for v in &violations {
        out.violation(format!(
            "{}:{} — uncited `{}`",
            v.file, v.line, v.attr_text
        ));
    }
    for s in &stale {
        out.violation(format!("allowlist:0 — stale allowlist: {s}"));
    }
    let ok = out.finish_ok();
    out.note(format!(
        "check 4: {} ({} uncited, {} stale waiver(s))",
        if ok { "PASS" } else { "FAIL" },
        violations.len(),
        stale.len()
    ));
    ok
}

fn run_allocation_admission(files: &[fsutil::SourceFile], verbose: bool) -> bool {
    let mut out = CheckOut::new(verbose);
    out.header("== check: allocation-admission ratchet ==");
    let violations = checks::allocation_admission::check(files);
    for v in &violations {
        out.violation(format!(
            "{}:{} — `{}` caps its size inline with `.min(...)` — route the \
             caller-declared size through `crate::capacity::admit(declared, available)` instead",
            v.file, v.line, v.call
        ));
    }
    let ok = out.finish_ok();
    out.note(format!(
        "check: allocation-admission {} ({} inline-cap site(s))",
        if ok { "PASS" } else { "FAIL" },
        violations.len()
    ));
    ok
}

fn run_boundary_closure(files: &[fsutil::SourceFile], verbose: bool) -> bool {
    let mut out = CheckOut::new(verbose);
    out.header("== check: boundary-closure ratchet ==");
    let violations = checks::boundary_closure::check(files);
    for v in &violations {
        out.violation(format!(
            "{}:{} — [{}]: {}",
            v.file, v.line, v.shape, v.detail
        ));
    }
    let ok = out.finish_ok();
    out.note(format!(
        "check: boundary-closure {} ({} condemned-shape site(s))",
        if ok { "PASS" } else { "FAIL" },
        violations.len()
    ));
    ok
}

fn run_peer_dial_ban(files: &[fsutil::SourceFile], verbose: bool) -> bool {
    let mut out = CheckOut::new(verbose);
    out.header("== check: peer-dial ban (seats 18/92 — NATS is the only nervous system) ==");
    let violations = checks::peer_dial_ban::check(files);
    for v in &violations {
        out.violation(format!(
            "{}:{} — `{}` — a raw peer/transport socket in the engine is a second \
             nervous system; fabric-down is `Refuse(FabricUnavailable)`, never a dial",
            v.file, v.line, v.symbol
        ));
    }
    let ok = out.finish_ok();
    out.note(format!(
        "check: peer-dial ban {} ({} raw-socket site(s))",
        if ok { "PASS" } else { "FAIL" },
        violations.len()
    ));
    ok
}

fn run_determinism_ban(files: &[fsutil::SourceFile], verbose: bool) -> bool {
    let mut out = CheckOut::new(verbose);
    out.header("== check: determinism ban (seats 25/45/83/84 — sealed surface is clock/rng-free) ==");
    let violations = checks::determinism_ban::check(files);
    for v in &violations {
        out.violation(format!(
            "{}:{} — `{}` — wall-clock/unseeded randomness on the sealed/commit surface; \
             commit time is CommitOrdinal, the entropy arm lives in session/admit.rs",
            v.file, v.line, v.symbol
        ));
    }
    let ok = out.finish_ok();
    out.note(format!(
        "check: determinism ban {} ({} nondeterminism site(s) on the sealed surface)",
        if ok { "PASS" } else { "FAIL" },
        violations.len()
    ));
    ok
}

fn run_serializer_authority(files: &[fsutil::SourceFile], verbose: bool) -> bool {
    let mut out = CheckOut::new(verbose);
    out.header("== check: sealed-serializer authority ratchet (seat 59 — one CanonicalTranscript) ==");
    let sites = checks::serializer_authority::check(files);
    let n = sites.len();
    let baseline = checks::serializer_authority::BASELINE;
    if n > baseline {
        for s in &sites {
            out.violation(format!(
                "{}:{} — hand-rolled-layout site (sealed-serializer authority)",
                s.file, s.line
            ));
        }
        out.violation(format!(
            "serializer_authority:0 — {n} hand-rolled-layout site(s) exceeds baseline {baseline}"
        ));
        let ok = out.finish_ok();
        out.note_err(format!(
            "FAIL sealed-serializer authority: {n} hand-rolled-layout site(s) exceeds baseline \
             {baseline} — a new byte-literal hasher on the sealed surface. Route sealed artifacts \
             through the ONE CanonicalTranscript encoder; if this is a genuine internal digest, \
             raise serializer_authority::BASELINE in a reviewed commit."
        ));
        out.note(format!(
            "check: sealed-serializer authority FAIL ({n} sites, baseline {baseline})"
        ));
        return ok;
    }
    if n < baseline {
        out.violation(format!(
            "serializer_authority:0 — baseline stale: {n} < {baseline} — tighten BASELINE to {n}"
        ));
        let ok = out.finish_ok();
        out.note_err(format!(
            "RATCHET IMPROVED sealed-serializer authority: {n} < baseline {baseline} — tighten \
             serializer_authority::BASELINE to {n}"
        ));
        out.note(format!(
            "check: sealed-serializer authority FAIL (baseline stale: {n} < {baseline})"
        ));
        return ok;
    }
    // At baseline: sites are expected internal digests, not violations.
    out.note(format!(
        "check: sealed-serializer authority PASS ({n} internal-digest site(s); baseline {baseline})"
    ));
    true
}

fn run_unchecked_arith(
    files: &[fsutil::SourceFile],
    root: &std::path::Path,
    verbose: bool,
) -> Result<bool, anyhow::Error> {
    let mut out = CheckOut::new(verbose);
    out.header("== check: unchecked-arith named-invariant ratchet ==");
    let baseline = checks::unchecked_arith::load_baseline(root).map_err(|e| anyhow::anyhow!(e))?;
    let examples = checks::unchecked_arith::walk_examples(root)?;
    let mut violations = checks::unchecked_arith::check(files);
    violations.extend(checks::unchecked_arith::check(&examples));
    violations.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
    let n = violations.len();
    if n > baseline {
        for v in &violations {
            out.violation(format!(
                "{}:{} — `{}` lacks an adjacent `// INVARIANT(Name): …` proof \
                 (unchecked arithmetic requires a named invariant at the same rung as unsafe)",
                v.file, v.line, v.method
            ));
        }
        out.violation(format!(
            "unchecked_arith:0 — {n} uncommented site(s) exceeds baseline {baseline}"
        ));
        let ok = out.finish_ok();
        out.note_err(format!(
            "FAIL unchecked-arith: {n} uncommented site(s) exceeds baseline {baseline} \
             (crates/xtask/unchecked-arith-baseline.json)"
        ));
        out.note(format!(
            "check: unchecked-arith FAIL ({n} uncommented, baseline {baseline})"
        ));
        return Ok(ok);
    }
    if n < baseline {
        out.violation(format!(
            "unchecked_arith:0 — baseline stale: {n} < {baseline} — tighten baseline to {n}"
        ));
        let ok = out.finish_ok();
        out.note_err(format!(
            "RATCHET IMPROVED unchecked-arith: {n} < baseline {baseline} — tighten \
             crates/xtask/unchecked-arith-baseline.json to {n}"
        ));
        out.note(format!(
            "check: unchecked-arith FAIL (baseline stale: {n} < {baseline})"
        ));
        return Ok(ok);
    }
    // At baseline: uncommented sites are ratcheted debt, not new violations.
    if verbose {
        for v in &violations {
            println!(
                "FAIL {}:{}: `{}` lacks an adjacent `// INVARIANT(Name): …` proof \
                 (unchecked arithmetic requires a named invariant at the same rung as unsafe)",
                v.file, v.line, v.method
            );
        }
    }
    out.note(format!(
        "check: unchecked-arith PASS ({n} uncommented; baseline {baseline})"
    ));
    Ok(true)
}

fn run_agreement_registry(
    files: &[fsutil::SourceFile],
    root: &std::path::Path,
    verbose: bool,
) -> Result<bool, anyhow::Error> {
    let mut out = CheckOut::new(verbose);
    out.header("== check 5: agreement-law registry ==");
    let registry = checks::agreement_registry::load(root)?;
    let violations = checks::agreement_registry::check(files, &registry, root);
    for v in &violations {
        out.violation(format!(
            "{}:0 — registry entry \"{}\" ({}): {}",
            v.file, v.name, v.test_fn, v.reason
        ));
    }
    let ok = out.finish_ok();
    out.note(format!(
        "check 5: {} ({} law(s) registered, {} missing)",
        if ok { "PASS" } else { "FAIL" },
        registry.len(),
        violations.len()
    ));
    Ok(ok)
}
