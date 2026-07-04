//! Check 4: every `#[allow(dead_code)]` / `#[expect(dead_code)]` (bare or
//! inside a `cfg_attr`) must carry a citation — an issue number or a plain
//! reason — on its own line or in the contiguous comment block immediately
//! above it. A dead-code allow with no reason anywhere near it is a
//! concept nobody is tracking: the ratchet is that a NEW uncited one fails
//! the build, forcing the citation (or the removal) at the moment it's
//! added, rather than accumulating silently.
//!
//! This check works over raw source text, not the `syn` AST: `syn`'s
//! tokenizer (like rustc's) discards plain `//` line comments entirely
//! during lexing (only `///`/`//!` doc comments survive, as `#[doc = ...]`
//! attributes) — and this codebase's existing citations are almost all
//! plain `//` comments. Line-based text scanning is therefore the only way
//! to see them at all.

use crate::allowlist::Allowlist;
use crate::fsutil::SourceFile;

pub struct Violation {
    pub file: String,
    pub line: usize,
    pub attr_text: String,
}

fn is_attr_line(trimmed: &str) -> bool {
    trimmed.starts_with("#[")
}

fn is_comment_line(trimmed: &str) -> bool {
    trimmed.starts_with("//")
}

fn has_dead_code(trimmed: &str) -> bool {
    is_attr_line(trimmed) && trimmed.contains("dead_code")
}

/// Same-line trailing comment: text after the attribute's balanced `]`
/// close, on the same physical line, in the `// reason` style used
/// throughout the tree (e.g. `tuple.rs`'s `#[allow(dead_code)] // used by
/// the engine layers growing around the kernel`).
fn trailing_comment(line: &str) -> bool {
    // A `]` followed eventually by `//` on the same line, with something
    // after the `//` besides whitespace.
    if let Some(close) = line.rfind(']')
        && let Some(slash) = line[close..].find("//")
    {
        let after = &line[close + slash + 2..];
        return !after.trim().is_empty();
    }
    false
}

fn is_cited(lines: &[&str], attr_idx: usize) -> bool {
    if trailing_comment(lines[attr_idx]) {
        return true;
    }
    // Walk upward through any stacked attribute lines decorating the same
    // item (e.g. the paired `cfg_attr(not(test), expect(dead_code))` /
    // `cfg_attr(test, allow(dead_code))` on `query/mod.rs`'s `semiring`),
    // then require the line immediately above that run to be a comment.
    let mut idx = attr_idx;
    while idx > 0 {
        let prev = lines[idx - 1].trim();
        if is_attr_line(prev) {
            idx -= 1;
            continue;
        }
        return is_comment_line(prev);
    }
    false
}

pub fn check(files: &[SourceFile], allow: &Allowlist) -> (Vec<Violation>, Vec<String>) {
    let mut violations = Vec::new();
    let mut stale = Vec::new();

    for f in files {
        let lines: Vec<&str> = f.text.lines().collect();
        for (i, raw) in lines.iter().enumerate() {
            let trimmed = raw.trim();
            if !has_dead_code(trimmed) {
                continue;
            }
            let line_no = i + 1;
            if is_cited(&lines, i) {
                continue;
            }
            let waived = allow.dead_code_ratchet.iter().any(|e| {
                f.rel_path.ends_with(e.file.trim_start_matches("./")) && e.line == line_no
            });
            if waived {
                continue;
            }
            violations.push(Violation {
                file: f.rel_path.clone(),
                line: line_no,
                attr_text: trimmed.to_string(),
            });
        }
    }

    for e in &allow.dead_code_ratchet {
        let still_uncited = files.iter().any(|f| {
            if !f.rel_path.ends_with(e.file.trim_start_matches("./")) {
                return false;
            }
            let lines: Vec<&str> = f.text.lines().collect();
            let Some(line) = lines.get(e.line - 1) else {
                return false;
            };
            has_dead_code(line.trim()) && !is_cited(&lines, e.line - 1)
        });
        if !still_uncited {
            stale.push(format!(
                "dead_code_ratchet waiver for {}:{} no longer applies (now cited, or the line changed) — remove it",
                e.file, e.line
            ));
        }
    }

    (violations, stale)
}
