/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Named-invariant protocol for unchecked arithmetic (story #306 T5).
//!
//! Unchecked arithmetic at a hot bounded site carries a named-invariant proof
//! at the same rung as `unsafe`'s `SAFETY:` comment (frontier source
//! `unchecked-arithmetic-at-a-hot-bounded-site-carries-a-named-i`). The proof
//! is adjacent, greppable, and enforced by this ratchet at baseline zero.
//!
//! # Comment schema
//!
//! Every governed call site must have a preceding line-comment of the form:
//!
//! ```text
//! // INVARIANT(<Name>): <proof>
//! ```
//!
//! - `<Name>` is a Rust identifier naming the invariant (grep/xtask key).
//! - `<proof>` is non-empty prose stating why wrap/overflow is lawful here.
//! - The comment must appear within [`PROOF_LOOKBACK`] source lines above the
//!   call (same window covers a short method chain under one proof).
//!
//! Grep: `INVARIANT\([A-Za-z_][A-Za-z0-9_]*\):`
//!
//! # Governed calls
//!
//! Method or path-segment calls whose name starts with one of:
//! `wrapping_`, `overflowing_`, `unchecked_`.
//!
//! `saturating_*` and `checked_*` are out of scope (defined clamp / typed
//! refusal). Raw `+`/`*` on primitives are a separate story surface.

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use syn::visit::{self, Visit};

use crate::fsutil::{SourceFile, span_line};

/// How many source lines above a call may hold its `INVARIANT(name):` proof.
pub const PROOF_LOOKBACK: usize = 8;

/// Committed ratchet floor: uncommented governed sites must be exactly this.
pub const BASELINE_UNCOMMENTED: usize = 0;

static PROOF_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*//\s*INVARIANT\([A-Za-z_][A-Za-z0-9_]*\):\s*\S").unwrap()
});

const GOVERNED_PREFIXES: &[&str] = &["wrapping_", "overflowing_", "unchecked_"];

/// One governed call lacking an adjacent named-invariant proof.
pub struct Violation {
    pub file: String,
    pub line: usize,
    pub method: String,
}

fn is_governed_name(name: &str) -> bool {
    GOVERNED_PREFIXES.iter().any(|p| name.starts_with(p))
}

fn line_has_proof(line: &str) -> bool {
    PROOF_LINE.is_match(line)
}

/// True when a proof comment sits within [`PROOF_LOOKBACK`] lines above `line`
/// (1-based), inclusive of looking at the call line itself for a trailing form.
fn has_adjacent_proof(lines: &[&str], line: usize) -> bool {
    if line == 0 {
        return false;
    }
    let idx = line - 1;
    let start = idx.saturating_sub(PROOF_LOOKBACK);
    for i in start..=idx {
        if line_has_proof(lines[i]) {
            return true;
        }
    }
    false
}

struct Scanner<'a> {
    lines: &'a [&'a str],
    hits: Vec<(usize, String)>,
}

impl<'ast> Visit<'ast> for Scanner<'_> {
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        let name = node.method.to_string();
        if is_governed_name(&name) {
            let line = span_line(&node.method.span());
            if !has_adjacent_proof(self.lines, line) {
                self.hits.push((line, name));
            }
        }
        visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = node.func.as_ref()
            && let Some(seg) = p.path.segments.last()
        {
            let name = seg.ident.to_string();
            if is_governed_name(&name) {
                let line = span_line(&seg.ident.span());
                if !has_adjacent_proof(self.lines, line) {
                    self.hits.push((line, name));
                }
            }
        }
        visit::visit_expr_call(self, node);
    }
}

/// Scan parsed engine sources for governed calls missing a named-invariant proof.
pub fn check(files: &[SourceFile]) -> Vec<Violation> {
    let mut violations = Vec::new();
    for f in files {
        let lines: Vec<&str> = f.text.lines().collect();
        let mut s = Scanner {
            lines: &lines,
            hits: Vec::new(),
        };
        s.visit_file(&f.ast);
        for (line, method) in s.hits {
            violations.push(Violation {
                file: f.rel_path.clone(),
                line,
                method,
            });
        }
    }
    violations.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
    violations
}

/// Example binaries under `crates/kyzo-core/examples` also carry hot-path
/// PRNG wrapping and must obey the same protocol (allowlist covers them).
pub fn walk_examples(root: &Path) -> anyhow::Result<Vec<SourceFile>> {
    let dir = root.join("crates/kyzo-core/examples");
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let ast = syn::parse_file(&text)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
        let rel_path = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        out.push(SourceFile {
            rel_path,
            text,
            ast,
        });
    }
    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(out)
}

/// Load the committed baseline. Protocol floor is zero — a raised floor is a
/// protocol defect, not a waiver. Missing file ⇒ treat as zero.
pub fn load_baseline(root: &Path) -> Result<usize, String> {
    let path = root.join("crates/xtask/unchecked-arith-baseline.json");
    if !path.is_file() {
        return Ok(BASELINE_UNCOMMENTED);
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("parsing {}: {e}", path.display()))?;
    let n = v
        .get("uncommented")
        .and_then(|x| x.as_u64())
        .ok_or_else(|| format!("{}: missing numeric `uncommented` key", path.display()))?
        as usize;
    if n != BASELINE_UNCOMMENTED {
        return Err(format!(
            "{}: uncommented baseline must be {BASELINE_UNCOMMENTED} (got {n}); \
             raising the floor waives the named-invariant protocol",
            path.display()
        ));
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(content: &str) -> SourceFile {
        SourceFile {
            rel_path: "test.rs".to_string(),
            text: content.to_string(),
            ast: syn::parse_file(content).expect("parse fixture"),
        }
    }

    #[test]
    fn flags_bare_wrapping_call() {
        let f = src("fn f(x: u64) -> u64 { x.wrapping_add(1) }\n");
        let v = check(&[f]);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].method, "wrapping_add");
    }

    #[test]
    fn accepts_named_invariant_proof() {
        let f = src(
            "fn f(x: u64) -> u64 {\n\
             // INVARIANT(token_pos): position is a modular counter; wrap is intentional.\n\
             x.wrapping_add(1)\n\
             }\n",
        );
        assert!(check(&[f]).is_empty());
    }

    #[test]
    fn proof_covers_short_method_chain() {
        let f = src(
            "fn f(x: u64) -> u64 {\n\
             // INVARIANT(splitmix64): modular mix per the splitmix64 contract.\n\
             x.wrapping_add(1)\n\
                 .wrapping_mul(3)\n\
             }\n",
        );
        assert!(check(&[f]).is_empty());
    }

    #[test]
    fn empty_proof_body_does_not_count() {
        let f = src(
            "fn f(x: u64) -> u64 {\n\
             // INVARIANT(empty):\n\
             x.wrapping_add(1)\n\
             }\n",
        );
        assert_eq!(check(&[f]).len(), 1);
    }

    #[test]
    fn path_form_is_governed() {
        let f = src("fn f(a: u64, b: u64) -> u64 { u64::wrapping_add(a, b) }\n");
        assert_eq!(check(&[f]).len(), 1);
    }

    #[test]
    fn checked_and_saturating_are_out_of_scope() {
        let f = src(
            "fn f(x: u64) -> u64 {\n\
             let _ = x.checked_add(1);\n\
             x.saturating_add(1)\n\
             }\n",
        );
        assert!(check(&[f]).is_empty());
    }
}
