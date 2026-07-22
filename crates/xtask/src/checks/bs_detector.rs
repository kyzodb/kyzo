/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The BS detector. Every shape in [`BANNED`] is illegal in the program.
//!
//! There is **no baseline and no grandfathered count**. The count is not
//! frozen; it is driven to zero-or-confessed. The ONLY way an occurrence is
//! legal is an entry in the one register — `resonance-allow.toml`'s
//! `[[bs_detector]]` array — naming its exact `pattern`, `file`, and `line`,
//! and answering in writing, in `why_not_sabotage`, **why this specific site
//! is not sabotage**. Every occurrence that is not in the register is a
//! violation; the build is red until each one is either removed or confessed.
//!
//! There is no inline tag and no blanket test exemption — a blanket exemption
//! is the loophole. A test that deliberately exercises a banned shape to prove
//! a guard fires is legal only by its own register entry with its own written
//! confession. The gate cannot judge whether the confession is true, only that
//! it was signed at a named line every diff and reviewer sees. A silent
//! omission is impossible; a written lie in `why_not_sabotage` is on the record.

use crate::allowlist::Allowlist;
use crate::fsutil::SourceFile;

/// One forbidden source shape: a meter name, a one-line reason it is banned,
/// and a raw-line predicate that recognizes it.
pub struct BsPattern {
    pub name: &'static str,
    pub why: &'static str,
    pub matches: fn(&str) -> bool,
}

/// One unregistered occurrence of a banned shape — a violation.
pub struct Violation {
    pub file: String,
    pub line: usize,
    pub pattern: &'static str,
    pub why: &'static str,
}

/// Strip an end-of-line `//` comment so a shape merely named in a comment is
/// not counted as a live occurrence.
fn code_only(line: &str) -> &str {
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

fn m_allow_dead(l: &str) -> bool {
    code_only(l).contains("allow(dead_code)")
}
fn m_allow_unused(l: &str) -> bool {
    code_only(l).contains("allow(unused")
}
fn m_allow_clippy(l: &str) -> bool {
    code_only(l).contains("allow(clippy::")
}
fn m_allow_missing_docs(l: &str) -> bool {
    code_only(l).contains("allow(missing_docs")
}
fn m_allow_private(l: &str) -> bool {
    let c = code_only(l);
    c.contains("allow(private_interfaces") || c.contains("allow(private_bounds")
}
fn m_let_underscore(l: &str) -> bool {
    let c = code_only(l).trim_start();
    c.starts_with("let _ =") || c.starts_with("let _:")
}
fn m_unwrap_or_default(l: &str) -> bool {
    code_only(l).contains(".unwrap_or_default()")
}
fn m_unwrap_or_else(l: &str) -> bool {
    code_only(l).contains(".unwrap_or_else(")
}
fn m_unwrap_or(l: &str) -> bool {
    let c = code_only(l);
    c.contains(".unwrap_or(") && !c.contains(".unwrap_or_else(")
}
fn m_ok_drop(l: &str) -> bool {
    code_only(l).contains(".ok()")
}
fn m_catchall_arm(l: &str) -> bool {
    let c = code_only(l).trim_start();
    c.starts_with("_ =>") || c.starts_with("_ if ")
}
fn m_wrapping(l: &str) -> bool {
    code_only(l).contains(".wrapping_")
}
fn m_saturating(l: &str) -> bool {
    code_only(l).contains(".saturating_")
}
fn m_default_derive(l: &str) -> bool {
    let c = code_only(l);
    c.contains("#[derive(") && c.contains("Default")
}
fn m_default_impl(l: &str) -> bool {
    code_only(l).contains("impl Default for")
}
fn m_construction_door(l: &str) -> bool {
    let c = code_only(l);
    c.contains("fn from_raw(")
        || c.contains("fn from_bytes(")
        || c.contains("fn from_len_unchecked(")
        || c.contains("fn from_unchecked(")
        || c.contains("fn new_unchecked(")
}
fn m_unwrap(l: &str) -> bool {
    let c = code_only(l);
    c.contains(".unwrap()")
}
fn m_expect(l: &str) -> bool {
    code_only(l).contains(".expect(")
}
fn m_panic(l: &str) -> bool {
    code_only(l).contains("panic!(")
}
fn m_unreachable(l: &str) -> bool {
    code_only(l).contains("unreachable!(")
}
fn m_debug_assert(l: &str) -> bool {
    code_only(l).contains("debug_assert")
}
fn m_as_cast(l: &str) -> bool {
    let c = code_only(l);
    [" as u8", " as u16", " as u32", " as u64", " as usize", " as i8", " as i16",
     " as i32", " as i64", " as isize", " as f32", " as f64"]
        .iter()
        .any(|p| c.contains(p))
}
fn m_todo(l: &str) -> bool {
    let c = code_only(l);
    c.contains("todo!(") || c.contains("unimplemented!(")
}
fn m_ignore(l: &str) -> bool {
    code_only(l).trim_start().starts_with("#[ignore")
}
fn m_should_panic(l: &str) -> bool {
    code_only(l).trim_start().starts_with("#[should_panic")
}
fn m_process_exit(l: &str) -> bool {
    let c = code_only(l);
    c.contains("process::exit(") || c.contains("process::abort(")
}

/// The BS table. Every entry is illegal on sight — legality comes only from the
/// register, never from a baseline.
pub const BANNED: &[BsPattern] = &[
    BsPattern { name: "allow_dead_code", why: "silenced unused-code alarm (corpse/dupe/unwired hiding)", matches: m_allow_dead },
    BsPattern { name: "allow_unused", why: "silenced unused warning", matches: m_allow_unused },
    BsPattern { name: "allow_clippy", why: "silenced lint", matches: m_allow_clippy },
    BsPattern { name: "allow_missing_docs", why: "undocumented public surface", matches: m_allow_missing_docs },
    BsPattern { name: "allow_private", why: "leaked private type", matches: m_allow_private },
    BsPattern { name: "let_underscore", why: "`let _ =` discards a value/Result", matches: m_let_underscore },
    BsPattern { name: "unwrap_or_default", why: "error/None → silent default", matches: m_unwrap_or_default },
    BsPattern { name: "unwrap_or_else", why: "error → silent fallback closure", matches: m_unwrap_or_else },
    BsPattern { name: "unwrap_or", why: "error/None → silent fallback value", matches: m_unwrap_or },
    BsPattern { name: "ok_drop", why: "`.ok()` drops the error", matches: m_ok_drop },
    BsPattern { name: "catchall_arm", why: "`_ =>` swallows unhandled variants", matches: m_catchall_arm },
    BsPattern { name: "wrapping_arith", why: "silent overflow wrap", matches: m_wrapping },
    BsPattern { name: "saturating_arith", why: "silent overflow clamp", matches: m_saturating },
    BsPattern { name: "default_derive", why: "derive(Default) — sentinel/zero on a domain type", matches: m_default_derive },
    BsPattern { name: "default_impl", why: "impl Default — sentinel/zero on a domain type", matches: m_default_impl },
    BsPattern { name: "construction_door", why: "from_raw/from_bytes/*_unchecked — second construction door", matches: m_construction_door },
    BsPattern { name: "unwrap", why: "panic on None/Err", matches: m_unwrap },
    BsPattern { name: "expect", why: "panic on None/Err with a message", matches: m_expect },
    BsPattern { name: "panic_bang", why: "explicit panic", matches: m_panic },
    BsPattern { name: "unreachable_bang", why: "`unreachable!()` — a can't-happen that can", matches: m_unreachable },
    BsPattern { name: "debug_assert", why: "compiled out in release — absent in prod", matches: m_debug_assert },
    BsPattern { name: "as_cast", why: "lossy/truncating numeric cast (use TryFrom)", matches: m_as_cast },
    BsPattern { name: "todo_bang", why: "todo!()/unimplemented!() — a hole", matches: m_todo },
    BsPattern { name: "ignore_test", why: "#[ignore] — a silently skipped test", matches: m_ignore },
    BsPattern { name: "should_panic", why: "#[should_panic] — asserts a panic, not a typed refusal", matches: m_should_panic },
    BsPattern { name: "process_exit", why: "process::exit/abort — a hard exit", matches: m_process_exit },
];

/// True if `(pattern, file, line)` is confessed in the register.
fn is_registered(allow: &Allowlist, pattern: &str, rel_path: &str, line: usize) -> bool {
    allow.bs_detector.iter().any(|e| {
        e.pattern == pattern
            && e.line == line
            && rel_path.ends_with(e.file.trim_start_matches("./"))
    })
}

/// Every banned occurrence that is not confessed in the register is a
/// violation. Returns `(violations, stale_register_entries)`.
pub fn check(files: &[SourceFile], allow: &Allowlist) -> (Vec<Violation>, Vec<String>) {
    let mut violations = Vec::new();
    for f in files {
        for (i, raw) in f.text.lines().enumerate() {
            let line_no = i + 1;
            for pat in BANNED {
                if (pat.matches)(raw) && !is_registered(allow, pat.name, &f.rel_path, line_no) {
                    violations.push(Violation {
                        file: f.rel_path.clone(),
                        line: line_no,
                        pattern: pat.name,
                        why: pat.why,
                    });
                }
            }
        }
    }

    // A register entry whose site no longer matches its pattern is stale — the
    // confession outlived its subject and must be removed, so a future banned
    // shape can never inherit a dead waiver.
    let mut stale = Vec::new();
    for e in &allow.bs_detector {
        let still = files.iter().any(|f| {
            f.rel_path.ends_with(e.file.trim_start_matches("./"))
                && f.text.lines().nth(e.line.saturating_sub(1)).is_some_and(|l| {
                    BANNED.iter().any(|p| p.name == e.pattern && (p.matches)(l))
                })
        });
        if !still {
            stale.push(format!(
                "bs_detector register entry {}:{} ({}) no longer matches — remove it",
                e.file, e.line, e.pattern
            ));
        }
    }

    (violations, stale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowlist::{Allowlist, BsDetectorEntry};

    /// One representative detonation line per BANNED pattern. Coverage is
    /// pinned both ways below: a pattern with no line here, or a line its
    /// pattern fails to bite, is a test failure — a banned shape without a
    /// demonstrated detonation is theater and may not register.
    const DETONATIONS: &[(&str, &str)] = &[
        ("allow_dead_code", "#[allow(dead_code)]"),
        ("allow_unused", "#[allow(unused_variables)]"),
        ("allow_clippy", "#[allow(clippy::too_many_arguments)]"),
        ("allow_missing_docs", "#[allow(missing_docs)]"),
        ("allow_private", "#[allow(private_interfaces)]"),
        ("let_underscore", "    let _ = tx.send(v);"),
        ("unwrap_or_default", "let n = s.parse().unwrap_or_default();"),
        ("unwrap_or_else", "let n = m.get(k).unwrap_or_else(|| fallback());"),
        ("unwrap_or", "let n = m.get(k).unwrap_or(0);"),
        ("ok_drop", "writer.flush().ok();"),
        ("catchall_arm", "        _ => {}"),
        ("wrapping_arith", "let n = a.wrapping_add(b);"),
        ("saturating_arith", "let n = a.saturating_sub(b);"),
        ("default_derive", "#[derive(Debug, Clone, Default)]"),
        ("default_impl", "impl Default for Config {"),
        ("construction_door", "pub fn from_raw(bits: u64) -> Self {"),
        ("unwrap", "let v = maybe.unwrap();"),
        ("expect", "let v = maybe.expect(\"present\");"),
        ("panic_bang", "panic!(\"invariant broken\");"),
        ("unreachable_bang", "unreachable!(),"),
        ("debug_assert", "debug_assert!(idx < len);"),
        ("as_cast", "let short = long as u32;"),
        ("todo_bang", "todo!()"),
        ("ignore_test", "#[ignore]"),
        ("should_panic", "#[should_panic]"),
        ("process_exit", "std::process::exit(2);"),
    ];

    fn sf(rel: &str, lines: &[&str]) -> SourceFile {
        SourceFile {
            rel_path: rel.to_string(),
            text: lines.join("\n"),
            ast: syn::parse_str::<syn::File>("").expect("empty source parses"),
        }
    }

    fn confession(pattern: &str, file: &str, line: usize) -> BsDetectorEntry {
        BsDetectorEntry {
            pattern: pattern.to_string(),
            file: file.to_string(),
            line,
            why_not_sabotage: "bite-proof fixture confession".to_string(),
        }
    }

    /// Every BANNED pattern bites its detonation line, and both tables cover
    /// each other exactly — no pattern without a proof, no proof without a
    /// pattern.
    #[test]
    fn every_banned_pattern_detonates_on_its_sample() {
        for pat in BANNED {
            let (_, line) = DETONATIONS
                .iter()
                .find(|(n, _)| *n == pat.name)
                .unwrap_or_else(|| panic!("pattern `{}` has no detonation sample", pat.name));
            assert!(
                (pat.matches)(line),
                "pattern `{}` failed to bite its own detonation line {line:?}",
                pat.name
            );
        }
        for (name, _) in DETONATIONS {
            assert!(
                BANNED.iter().any(|p| p.name == *name),
                "detonation sample `{name}` names no BANNED pattern"
            );
        }
    }

    /// End-to-end: an unregistered occurrence of every pattern is a violation
    /// at its exact line, against an empty register.
    #[test]
    fn unregistered_occurrences_are_violations_at_exact_lines() {
        let lines: Vec<&str> = DETONATIONS.iter().map(|(_, l)| *l).collect();
        let file = sf("crates/kyzo-core/src/fixture.rs", &lines);
        let (violations, stale) = check(&[file], &Allowlist::default());
        for (i, (name, _)) in DETONATIONS.iter().enumerate() {
            assert!(
                violations
                    .iter()
                    .any(|v| v.pattern == *name && v.line == i + 1),
                "pattern `{name}` did not detonate at line {}",
                i + 1
            );
        }
        assert!(stale.is_empty(), "empty register cannot be stale");
    }

    /// A register confession suppresses exactly its own site — the second,
    /// unconfessed occurrence of the same shape still detonates.
    #[test]
    fn confession_covers_one_site_only() {
        let rel = "crates/kyzo-core/src/fixture.rs";
        let file = sf(rel, &["let a = x.unwrap();", "let b = y.unwrap();"]);
        let allow = Allowlist {
            bs_detector: vec![confession("unwrap", rel, 1)],
            ..Allowlist::default()
        };
        let (violations, stale) = check(&[file], &allow);
        assert_eq!(violations.len(), 1, "exactly the unconfessed site remains");
        assert_eq!((violations[0].pattern, violations[0].line), ("unwrap", 2));
        assert!(stale.is_empty(), "the confession still matches its site");
    }

    /// A shape merely named in a `//` comment is not a live occurrence.
    #[test]
    fn comment_mentions_do_not_count() {
        let file = sf(
            "crates/kyzo-core/src/fixture.rs",
            &["// forbidden: .unwrap(), panic!(), x as u8", "let ok = 1;"],
        );
        let (violations, _) = check(&[file], &Allowlist::default());
        assert!(
            violations.is_empty(),
            "comment-only mentions detonated: {:?}",
            violations
                .iter()
                .map(|v| (v.pattern, v.line))
                .collect::<Vec<_>>()
        );
    }

    /// A confession whose site no longer matches its pattern is stale and is
    /// reported — a dead waiver can never silently cover a future shape.
    #[test]
    fn dead_confession_is_stale() {
        let rel = "crates/kyzo-core/src/fixture.rs";
        let file = sf(rel, &["let clean = 1;"]);
        let allow = Allowlist {
            bs_detector: vec![confession("unwrap", rel, 1)],
            ..Allowlist::default()
        };
        let (violations, stale) = check(&[file], &allow);
        assert!(violations.is_empty());
        assert_eq!(stale.len(), 1, "the dead confession must be reported");
        assert!(
            stale[0].contains(rel) && stale[0].contains("unwrap"),
            "stale report names the entry: {}",
            stale[0]
        );
    }
}
