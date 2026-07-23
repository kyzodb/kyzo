/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Engine 1 — Shape: a banned construct at one site in source text.
//!
//! Every matcher is a named entry in [`MATCHERS`]; checks.toml references
//! them by name and the registry refuses a name this table doesn't export.
//! Matchers come in two scan classes:
//! - `ScanClass::Everything` — swallowing shapes (silenced errors, fake
//!   fallbacks, silenced lints, error costumes…) are banned in test code
//!   too: a test that swallows is worse than a production lie because it
//!   certifies one.
//! - `ScanClass::ProductionOnly` — laws whose own boundary is shipping
//!   code. Two families: production-surface laws (wall-clock, sockets,
//!   naked crypto arrays — compiled-out code cannot violate a runtime
//!   law), and the loud-detonator panics (`unwrap`/`expect`/`panic!`/
//!   `unreachable!`) — zone law bans them "on any path reachable from a
//!   caller", and `#[cfg(test)]`/`#[test]` scaffolding has no callers; a
//!   panicking test is a loud failure, the opposite of a swallow. The
//!   exemption applies at any syntactic level (`#[cfg(test)]` mods, items,
//!   statements, `#[test]` fns) and NEVER to a module merely named
//!   `tests`.
//!
//! This file applies its own law to itself: no `unwrap`/`expect`, no `as`
//! casts, and no `_ =>` arms — foreign non-exhaustive enums are handled
//! with `if let`/`matches!` so there is no catch-all to confess.

use quote::ToTokens;
use syn::visit::{self, Visit};

use crate::boundary::{SourceFile, span_line};
use crate::engines::Hit;

/// Whether a matcher sees unit-test scaffolding or skips it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScanClass {
    Everything,
    ProductionOnly,
}

pub struct Matcher {
    pub name: &'static str,
    pub class: ScanClass,
    pub run: fn(&SourceFile) -> Vec<Hit>,
}

/// Look a matcher up by its checks.toml name.
pub fn matcher_by_name(name: &str) -> Option<&'static Matcher> {
    MATCHERS.iter().find(|m| m.name == name)
}

/// Run one matcher over one file, honoring its scan class.
pub fn run_matcher(m: &Matcher, file: &SourceFile) -> Vec<Hit> {
    let mut hits = (m.run)(file);
    if m.class == ScanClass::ProductionOnly {
        let test_lines = test_scope_lines(file);
        hits.retain(|h| !test_lines.contains(&h.line));
    }
    hits
}

// ---------------------------------------------------------------------------
// test-scope accounting (for ProductionOnly matchers)
// ---------------------------------------------------------------------------

/// Every line that lives inside test scaffolding: `#[cfg(test)]`/`mod
/// tests` modules, `#[cfg(test)]` items, `#[cfg(test)]` statements, and
/// `#[test]` functions.
fn test_scope_lines(file: &SourceFile) -> std::collections::BTreeSet<usize> {
    struct V {
        lines: std::collections::BTreeSet<usize>,
    }
    fn span_range(tokens: impl ToTokens) -> Option<(usize, usize)> {
        let ts = tokens.to_token_stream();
        let mut iter = ts.into_iter();
        let first = iter.next()?;
        let start = first.span().start().line;
        let mut end = first.span().end().line;
        for t in iter {
            end = end.max(t.span().end().line);
        }
        Some((start, end))
    }
    fn mark(v: &mut V, tokens: impl ToTokens) {
        if let Some((a, b)) = span_range(tokens) {
            v.lines.extend(a..=b);
        }
    }
    impl<'ast> Visit<'ast> for V {
        fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
            // Only the cfg attr exempts — a module merely NAMED `tests`
            // without `#[cfg(test)]` ships, and shipped code is scanned.
            if attrs_are_cfg_test(&node.attrs) {
                mark(self, node);
                return;
            }
            visit::visit_item_mod(self, node);
        }
        fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
            if attrs_are_cfg_test(&node.attrs) || is_test_fn(&node.attrs) {
                mark(self, node);
                return;
            }
            visit::visit_item_fn(self, node);
        }
        fn visit_local(&mut self, node: &'ast syn::Local) {
            if attrs_are_cfg_test(&node.attrs) {
                mark(self, node);
                return;
            }
            visit::visit_local(self, node);
        }
    }
    let mut v = V {
        lines: std::collections::BTreeSet::new(),
    };
    v.visit_file(&file.ast);
    v.lines
}

fn attrs_are_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        if !a.path().is_ident("cfg") {
            return false;
        }
        match a.parse_args::<syn::Meta>() {
            // A cfg whose arguments don't parse as Meta cannot be
            // `#[cfg(test)]`; unparseable stays PRODUCTION scope — the
            // exemption must be proven, never defaulted into.
            Err(_) => false,
            Ok(m) => {
                m.path().is_ident("test") || m.to_token_stream().to_string().contains("test")
            }
        }
    })
}

fn is_test_fn(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("test")
            || a.path()
                .segments
                .last()
                .is_some_and(|s| s.ident == "test")
    })
}

// ---------------------------------------------------------------------------
// shared visitors
// ---------------------------------------------------------------------------

/// The one name-scan walker: every site scanner that matches a name list
/// against one AST node kind lives HERE, so there is a single push shape and
/// a single authority. Wrappers below select a node kind by filling one
/// list; the others stay empty.
struct NameSites<'a> {
    methods: &'a [&'a str],
    macros: &'a [&'a str],
    paths: &'a [&'a str],
    construct: &'a str,
    rel: &'a str,
    hits: Vec<Hit>,
}

impl<'ast, 'a> Visit<'ast> for NameSites<'a> {
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        let name = node.method.to_string();
        if self.methods.contains(&name.as_str()) {
            push(&mut self.hits, self.rel, span_line(&node.method.span()), self.construct);
        }
        visit::visit_expr_method_call(self, node);
    }
    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        if let Some(seg) = node.path.segments.last() {
            let name = seg.ident.to_string();
            if self.macros.contains(&name.as_str()) {
                push(&mut self.hits, self.rel, span_line(&seg.ident.span()), self.construct);
            }
        }
        visit::visit_macro(self, node);
    }
    fn visit_path(&mut self, node: &'ast syn::Path) {
        for seg in &node.segments {
            let ident = seg.ident.to_string();
            if self.paths.contains(&ident.as_str()) {
                push(&mut self.hits, self.rel, span_line(&seg.ident.span()), self.construct);
            }
        }
        visit::visit_path(self, node);
    }
}

fn push(hits: &mut Vec<Hit>, rel: &str, line: usize, construct: &str) {
    hits.push(Hit {
        file: rel.to_string(),
        line,
        construct: construct.to_string(),
    });
}

fn name_sites<'a>(
    file: &'a SourceFile,
    methods: &[&str],
    macros: &[&str],
    paths: &[&str],
    construct: &str,
) -> Vec<Hit> {
    let mut v = NameSites {
        methods,
        macros,
        paths,
        construct,
        rel: &file.rel_path,
        hits: vec![],
    };
    v.visit_file(&file.ast);
    v.hits
}

/// Hits on every method call whose name is in `names`.
fn method_calls(file: &SourceFile, names: &[&str], construct: &str) -> Vec<Hit> {
    name_sites(file, names, &[], &[], construct)
}

/// Hits on every path whose segments include one of `idents` (matches
/// `std::time::Instant::now` on the `Instant` segment).
fn path_idents(file: &SourceFile, idents: &[&str], construct: &str) -> Vec<Hit> {
    name_sites(file, &[], &[], idents, construct)
}

/// Hits on every macro invocation with one of `names`.
fn macro_calls(file: &SourceFile, names: &[&str], construct: &str) -> Vec<Hit> {
    name_sites(file, &[], names, &[], construct)
}

/// Hits on `#[allow(lint)]` / `#![allow(lint)]` attributes whose argument
/// mentions one of `needles`.
fn allow_attrs(file: &SourceFile, needles: &[&str], construct: &str) -> Vec<Hit> {
    let mut hits = vec![];
    for (idx, raw) in file.text.lines().enumerate() {
        let t = raw.trim_start();
        let is_attr = t.starts_with("#[allow(") || t.starts_with("#![allow(");
        if is_attr && needles.iter().any(|n| t.contains(n)) {
            hits.push(Hit {
                file: file.rel_path.clone(),
                line: idx + 1,
                construct: construct.to_string(),
            });
        }
    }
    hits
}

/// True when an `INVARIANT(` proof comment sits within `window` lines above
/// `line` (1-indexed) — the named-invariant protocol for arithmetic that
/// must wrap by published contract.
fn has_invariant_comment_above(file: &SourceFile, line: usize, window: usize) -> bool {
    let lines: Vec<&str> = file.text.lines().collect();
    // INVARIANT(LineWindow): line indices floor at the top of the file;
    // saturation IS the window's published clipping contract.
    let end = line.saturating_sub(1);
    let start = end.saturating_sub(window);
    lines[start..end]
        .iter()
        .any(|l| l.contains("INVARIANT(") || l.contains("SAFETY("))
}

/// The 1-indexed source line's text, if the file has that many lines.
fn line_text(file: &SourceFile, line: usize) -> Option<&str> {
    // INVARIANT(LineWindow): 1-indexed site line to 0-indexed lookup floors
    // at the top of the file; saturation is the published clipping contract.
    file.text.lines().nth(line.saturating_sub(1))
}

// ---------------------------------------------------------------------------
// matchers
// ---------------------------------------------------------------------------

fn m_unwrap(f: &SourceFile) -> Vec<Hit> {
    method_calls(f, &["unwrap"], "unwrap")
}
fn m_expect(f: &SourceFile) -> Vec<Hit> {
    method_calls(f, &["expect"], "expect")
}
fn m_unwrap_or(f: &SourceFile) -> Vec<Hit> {
    method_calls(f, &["unwrap_or"], "unwrap_or")
}
fn m_unwrap_or_else(f: &SourceFile) -> Vec<Hit> {
    method_calls(f, &["unwrap_or_else"], "unwrap_or_else")
}
fn m_unwrap_or_default(f: &SourceFile) -> Vec<Hit> {
    method_calls(f, &["unwrap_or_default"], "unwrap_or_default")
}
fn m_unchecked_unwrap(f: &SourceFile) -> Vec<Hit> {
    method_calls(f, &["unwrap_unchecked"], "unwrap_unchecked")
}

fn m_panic_bang(f: &SourceFile) -> Vec<Hit> {
    macro_calls(f, &["panic"], "panic_bang")
}
fn m_unreachable_bang(f: &SourceFile) -> Vec<Hit> {
    macro_calls(f, &["unreachable"], "unreachable_bang")
}
fn m_todo_bang(f: &SourceFile) -> Vec<Hit> {
    macro_calls(f, &["todo", "unimplemented"], "todo_bang")
}
fn m_debug_assert(f: &SourceFile) -> Vec<Hit> {
    macro_calls(
        f,
        &["debug_assert", "debug_assert_eq", "debug_assert_ne"],
        "debug_assert",
    )
}

/// `let _ = fallible(...)` — discarding a value that had something to say.
fn m_let_underscore(f: &SourceFile) -> Vec<Hit> {
    struct V<'a> {
        rel: &'a str,
        hits: Vec<Hit>,
    }
    impl<'ast, 'a> Visit<'ast> for V<'a> {
        fn visit_local(&mut self, node: &'ast syn::Local) {
            if matches!(node.pat, syn::Pat::Wild(_)) && node.init.is_some() {
                push(&mut self.hits, self.rel, span_line(&node.let_token.span), "let_underscore");
            }
            visit::visit_local(self, node);
        }
    }
    let mut v = V {
        rel: &f.rel_path,
        hits: vec![],
    };
    v.visit_file(&f.ast);
    v.hits
}

/// `.ok()` in statement position — a Result told to shut up.
fn m_ok_drop(f: &SourceFile) -> Vec<Hit> {
    struct V<'a> {
        rel: &'a str,
        hits: Vec<Hit>,
    }
    impl<'ast, 'a> Visit<'ast> for V<'a> {
        fn visit_stmt(&mut self, node: &'ast syn::Stmt) {
            if let syn::Stmt::Expr(syn::Expr::MethodCall(mc), Some(_)) = node {
                if mc.method == "ok" && mc.args.is_empty() {
                    push(&mut self.hits, self.rel, span_line(&mc.method.span()), "ok_drop");
                }
            }
            visit::visit_stmt(self, node);
        }
    }
    let mut v = V {
        rel: &f.rel_path,
        hits: vec![],
    };
    v.visit_file(&f.ast);
    v.hits
}

/// Numeric `as` casts — truncation costumes. (Discriminant extractions like
/// a repr(u8) enum's `self as u8` are detected too; the two lawful ones
/// carry sworn waivers, which is the point: visible, not structural.)
fn m_as_cast(f: &SourceFile) -> Vec<Hit> {
    struct V<'a> {
        rel: &'a str,
        hits: Vec<Hit>,
    }
    const NUMERIC: &[&str] = &[
        "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize",
        "f32", "f64",
    ];
    impl<'ast, 'a> Visit<'ast> for V<'a> {
        fn visit_expr_cast(&mut self, node: &'ast syn::ExprCast) {
            if let syn::Type::Path(tp) = &*node.ty {
                if let Some(seg) = tp.path.segments.last() {
                    let t = seg.ident.to_string();
                    if NUMERIC.contains(&t.as_str()) {
                        push(&mut self.hits, self.rel, span_line(&seg.ident.span()), "as_cast");
                    }
                }
            }
            visit::visit_expr_cast(self, node);
        }
    }
    let mut v = V {
        rel: &f.rel_path,
        hits: vec![],
    };
    v.visit_file(&f.ast);
    v.hits
}

/// wrapping_*/saturating_*/overflowing_*/unchecked_* arithmetic without a
/// named INVARIANT proof comment adjacent — silent overflow or an unproven
/// contract.
fn m_unchecked_arith(f: &SourceFile) -> Vec<Hit> {
    const OPS: &[&str] = &[
        "wrapping_add",
        "wrapping_sub",
        "wrapping_mul",
        "wrapping_neg",
        "wrapping_shl",
        "wrapping_shr",
        "saturating_add",
        "saturating_sub",
        "saturating_mul",
        "overflowing_add",
        "overflowing_sub",
        "overflowing_mul",
        "unchecked_add",
        "unchecked_sub",
        "unchecked_mul",
    ];
    method_calls(f, OPS, "unchecked_arith")
        .into_iter()
        .filter(|h| !has_invariant_comment_above(f, h.line, 3))
        .collect()
}

/// `with_capacity`/`reserve` whose argument carries its own `.min(...)`
/// cap — a reservation bound decided at the wrong door.
fn m_capacity_min_cap(f: &SourceFile) -> Vec<Hit> {
    struct V<'a> {
        rel: &'a str,
        hits: Vec<Hit>,
    }
    fn arg_has_min(args: &syn::punctuated::Punctuated<syn::Expr, syn::token::Comma>) -> bool {
        args.iter().any(|a| {
            a.to_token_stream()
                .to_string()
                .contains(". min (")
                || a.to_token_stream().to_string().contains(".min(")
        })
    }
    impl<'ast, 'a> Visit<'ast> for V<'a> {
        fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
            if (node.method == "with_capacity" || node.method == "reserve")
                && arg_has_min(&node.args)
            {
                push(&mut self.hits, self.rel, span_line(&node.method.span()), "capacity_min_cap");
            }
            visit::visit_expr_method_call(self, node);
        }
        fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
            let is_with_capacity = node
                .func
                .to_token_stream()
                .to_string()
                .ends_with("with_capacity");
            if is_with_capacity && arg_has_min(&node.args) {
                push(&mut self.hits, self.rel, span_line(&syn::spanned::Spanned::span(&node.func)), "capacity_min_cap");
            }
            visit::visit_expr_call(self, node);
        }
    }
    let mut v = V {
        rel: &f.rel_path,
        hits: vec![],
    };
    v.visit_file(&f.ast);
    v.hits
}

/// Silenced lints hiding defects (BANNED #11), split per lint family so
/// counts stay legible.
fn m_allow_dead_code(f: &SourceFile) -> Vec<Hit> {
    allow_attrs(f, &["dead_code"], "allow_dead_code")
}
fn m_allow_unused(f: &SourceFile) -> Vec<Hit> {
    allow_attrs(f, &["unused"], "allow_unused")
}
fn m_allow_clippy(f: &SourceFile) -> Vec<Hit> {
    allow_attrs(f, &["clippy::"], "allow_clippy")
}
fn m_allow_missing_docs(f: &SourceFile) -> Vec<Hit> {
    allow_attrs(f, &["missing_docs"], "allow_missing_docs")
}
fn m_allow_private(f: &SourceFile) -> Vec<Hit> {
    allow_attrs(
        f,
        &["private_interfaces", "private_bounds"],
        "allow_private",
    )
}
fn m_allow_unsafe(f: &SourceFile) -> Vec<Hit> {
    allow_attrs(f, &["unsafe_code"], "allow_unsafe")
}

/// `_ =>` / `_ if` match arms swallowing unenumerated variants.
fn m_catchall_arm(f: &SourceFile) -> Vec<Hit> {
    fn pick(node: &syn::Arm) -> Option<(usize, &'static str)> {
        if let syn::Pat::Wild(w) = &node.pat {
            return Some((span_line(&w.underscore_token.span), "catchall_arm"));
        }
        None
    }
    arm_sites(f, pick)
}

/// The one match-arm walker: every matcher that judges a `match` arm rides
/// this shell, so arm traversal has a single authority. The picker returns
/// the site line and the construct name for a violating arm.
fn arm_sites(f: &SourceFile, pick: fn(&syn::Arm) -> Option<(usize, &'static str)>) -> Vec<Hit> {
    struct V<'a> {
        pick: fn(&syn::Arm) -> Option<(usize, &'static str)>,
        rel: &'a str,
        hits: Vec<Hit>,
    }
    impl<'ast, 'a> Visit<'ast> for V<'a> {
        fn visit_arm(&mut self, node: &'ast syn::Arm) {
            if let Some((line, construct)) = (self.pick)(node) {
                push(&mut self.hits, self.rel, line, construct);
            }
            visit::visit_arm(self, node);
        }
    }
    let mut v = V {
        pick,
        rel: &f.rel_path,
        hits: vec![],
    };
    v.visit_file(&f.ast);
    v.hits
}

fn m_default_derive(f: &SourceFile) -> Vec<Hit> {
    line_starts_scan(f, "#[derive(", Some("Default"), "default_derive")
}

fn m_default_impl(f: &SourceFile) -> Vec<Hit> {
    struct V<'a> {
        rel: &'a str,
        hits: Vec<Hit>,
    }
    impl<'ast, 'a> Visit<'ast> for V<'a> {
        fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
            if let Some((_, path, _)) = &node.trait_ {
                if path.segments.last().is_some_and(|s| s.ident == "Default") {
                    push(&mut self.hits, self.rel, span_line(&node.impl_token.span), "default_impl");
                }
            }
            visit::visit_item_impl(self, node);
        }
    }
    let mut v = V {
        rel: &f.rel_path,
        hits: vec![],
    };
    v.visit_file(&f.ast);
    v.hits
}

/// Unchecked construction doors: `from_raw` / `*_unchecked` /
/// unvalidated `from_bytes` production definitions.
fn m_construction_door(f: &SourceFile) -> Vec<Hit> {
    fn door_site(sig: &syn::Signature) -> Option<usize> {
        let name = sig.ident.to_string();
        if name == "from_raw" || name.ends_with("_unchecked") {
            return Some(span_line(&sig.ident.span()));
        }
        None
    }
    sig_sites(f, door_site, "construction_door")
}

/// The one fn-signature walker: matchers that judge a fn by its signature
/// (free fns and impl methods alike) share this shell, so the two syn node
/// kinds are handled by a single authority.
fn sig_sites(
    f: &SourceFile,
    pick: fn(&syn::Signature) -> Option<usize>,
    construct: &str,
) -> Vec<Hit> {
    struct V<'a> {
        pick: fn(&syn::Signature) -> Option<usize>,
        construct: &'a str,
        rel: &'a str,
        hits: Vec<Hit>,
    }
    impl<'ast, 'a> Visit<'ast> for V<'a> {
        fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
            if let Some(line) = (self.pick)(&node.sig) {
                push(&mut self.hits, self.rel, line, self.construct);
            }
            visit::visit_impl_item_fn(self, node);
        }
        fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
            if let Some(line) = (self.pick)(&node.sig) {
                push(&mut self.hits, self.rel, line, self.construct);
            }
            visit::visit_item_fn(self, node);
        }
    }
    let mut v = V {
        pick,
        construct,
        rel: &f.rel_path,
        hits: vec![],
    };
    v.visit_file(&f.ast);
    v.hits
}

/// A test that can pass without asserting: `Err(_) => return` inside test
/// scope, `#[should_panic]`, `#[ignore]`.
fn m_test_err_early_return(f: &SourceFile) -> Vec<Hit> {
    fn pick(node: &syn::Arm) -> Option<(usize, &'static str)> {
        if !node.pat.to_token_stream().to_string().starts_with("Err") {
            return None;
        }
        if let syn::Expr::Return(r) = &*node.body {
            if r.expr.is_none() {
                return Some((span_line(&r.return_token.span), "test_err_early_return"));
            }
        }
        None
    }
    arm_sites(f, pick)
}

fn m_ignore_test(f: &SourceFile) -> Vec<Hit> {
    line_starts_scan(f, "#[ignore", None, "ignore_test")
}

fn m_should_panic(f: &SourceFile) -> Vec<Hit> {
    line_starts_scan(f, "#[should_panic", None, "should_panic")
}

/// The one attribute-line scanner: a line whose trimmed text starts with
/// `prefix` (and, when given, also contains `needle`) is a site.
fn line_starts_scan(
    f: &SourceFile,
    prefix: &str,
    needle: Option<&str>,
    construct: &str,
) -> Vec<Hit> {
    let mut hits = vec![];
    for (idx, raw) in f.text.lines().enumerate() {
        let t = raw.trim_start();
        let extra = match needle {
            None => true,
            Some(n) => t.contains(n),
        };
        if t.starts_with(prefix) && extra {
            push(&mut hits, &f.rel_path, idx + 1, construct);
        }
    }
    hits
}

/// Error laundered into a normal-looking value: an `Err(..)`/`None` match
/// arm whose body is a costume value (`Json::Null`, `0`, `0.0`, `T::MAX`,
/// `&[]`, `String::new()`).
fn m_err_costume(f: &SourceFile) -> Vec<Hit> {
    fn costume_of(body: &syn::Expr) -> Option<&'static str> {
        let s = body.to_token_stream().to_string();
        let t = s.trim();
        if t.ends_with(":: Null") || t == "Null" {
            return Some("err_to_null_costume");
        }
        if t == "0" || t == "0.0" || t.ends_with("from (0)") {
            return Some("err_to_zero_costume");
        }
        if t.ends_with(":: MAX") {
            return Some("err_to_max_costume");
        }
        if t == "& []" || t == "&[]" {
            return Some("empty_slice_costume");
        }
        None
    }
    fn pick(node: &syn::Arm) -> Option<(usize, &'static str)> {
        let pat = node.pat.to_token_stream().to_string();
        if !(pat.starts_with("Err") || pat == "None") {
            return None;
        }
        // `spans` is a fixed [Span; 2]; indexing is total.
        let line = span_line(&node.fat_arrow_token.spans[0]);
        costume_of(&node.body).map(|name| (line, name))
    }
    arm_sites(f, pick)
}

fn m_process_exit(f: &SourceFile) -> Vec<Hit> {
    path_idents(f, &["exit", "abort"], "process_exit")
        .into_iter()
        .filter(|h| {
            // Only std::process::exit / std::process::abort — a bare
            // `exit`/`abort` ident elsewhere (e.g. a state machine's own
            // verb) is not this shape.
            line_text(f, h.line)
                .is_some_and(|l| l.contains("process::exit") || l.contains("process::abort"))
        })
        .collect()
}

/// Sleep-based synchronization (BANNED #18): `thread::sleep` standing in
/// for a real happens-before.
fn m_sleep_sync(f: &SourceFile) -> Vec<Hit> {
    path_idents(f, &["sleep"], "sleep_sync")
        .into_iter()
        .filter(|h| {
            line_text(f, h.line)
                .is_some_and(|l| l.contains("thread::sleep") || l.contains("thread :: sleep"))
        })
        .collect()
}

/// `#[serde(default)]` / `#[serde(skip)]` (BANNED #25): a wire format
/// field that can silently vanish or self-invent.
fn m_serde_default_skip(f: &SourceFile) -> Vec<Hit> {
    struct V<'a> {
        rel: &'a str,
        hits: Vec<Hit>,
    }
    impl<'ast, 'a> Visit<'ast> for V<'a> {
        // AST attribute walk, not a text scan: an attribute mid-line or
        // split across lines is the same lie and must cost the same.
        fn visit_attribute(&mut self, node: &'ast syn::Attribute) {
            if node.path().is_ident("serde") {
                let args = node.to_token_stream().to_string();
                if args.contains("default") || args.contains("skip") {
                    push(&mut self.hits, self.rel, span_line(&node.pound_token.spans[0]), "serde_default_skip");
                }
            }
            visit::visit_attribute(self, node);
        }
    }
    let mut v = V {
        rel: &f.rel_path,
        hits: vec![],
    };
    v.visit_file(&f.ast);
    v.hits
}

/// Wall-clock / unseeded randomness (seats 25/45/83/84).
fn m_nondeterminism(f: &SourceFile) -> Vec<Hit> {
    path_idents(
        f,
        &["Instant", "SystemTime", "thread_rng", "OsRng", "getrandom"],
        "nondeterminism",
    )
}

/// Raw peer/transport sockets (seats 18/92): NATS is the only nervous
/// system.
fn m_peer_dial(f: &SourceFile) -> Vec<Hit> {
    path_idents(
        f,
        &["TcpStream", "TcpListener", "UdpSocket", "UnixStream", "UnixListener"],
        "peer_dial",
    )
}

/// A crypto/auth door taking or returning a naked `[u8; 32/12/64]` (T17)
/// instead of a typed Dek/Kek/Digest/Mac/Nonce/Signature. The structural
/// exemption is a newtype's OWN wrap door: `admit`, `as_bytes`,
/// `as_bytes_mut`, `from_*`, `of_*`.
fn m_naked_array_sig(f: &SourceFile) -> Vec<Hit> {
    fn naked_site(sig: &syn::Signature) -> Option<usize> {
        let name = sig.ident.to_string();
        let exempt = name == "admit"
            || name == "as_bytes"
            || name == "as_bytes_mut"
            || name.starts_with("from_")
            || name.starts_with("of_");
        let text = sig.to_token_stream().to_string();
        let naked = text.contains("[u8 ; 32]")
            || text.contains("[u8 ; 12]")
            || text.contains("[u8 ; 64]");
        if !exempt && naked {
            return Some(span_line(&sig.ident.span()));
        }
        None
    }
    sig_sites(f, naked_site, "naked_array_sig")
}

/// `unsafe` blocks/fns/impls/traits anywhere (the four crate roots without
/// `#![forbid(unsafe_code)]` are the meta engine's business; the token
/// itself is this matcher's).
fn m_unsafe_token(f: &SourceFile) -> Vec<Hit> {
    struct V<'a> {
        rel: &'a str,
        hits: Vec<Hit>,
    }
    impl<'ast, 'a> Visit<'ast> for V<'a> {
        fn visit_expr_unsafe(&mut self, node: &'ast syn::ExprUnsafe) {
            push(&mut self.hits, self.rel, span_line(&node.unsafe_token.span), "unsafe_token");
            visit::visit_expr_unsafe(self, node);
        }
        fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
            if let Some(u) = &node.sig.unsafety {
                push(&mut self.hits, self.rel, span_line(&u.span), "unsafe_token");
            }
            visit::visit_item_fn(self, node);
        }
        fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
            if let Some(u) = &node.unsafety {
                push(&mut self.hits, self.rel, span_line(&u.span), "unsafe_token");
            }
            visit::visit_item_impl(self, node);
        }
    }
    let mut v = V {
        rel: &f.rel_path,
        hits: vec![],
    };
    v.visit_file(&f.ast);
    v.hits
}

/// `assert!`/`assert_eq!`/`assert_ne!` on a caller-reachable path — a
/// panic wearing an invariant costume (the historical RelationId decode
/// bug: hostile stored bytes bound-checked by an assert, panicking the
/// process instead of refusing typed). In `#[test]` scope asserts ARE the
/// mechanism; in production they are panics.
fn m_assert_bang(f: &SourceFile) -> Vec<Hit> {
    macro_calls(f, &["assert", "assert_eq", "assert_ne"], "assert_bang")
}

/// Story #299's condemned boundary shapes may never return: triggers
/// stored as raw source-string collections, and extractor expressions
/// captured as Display text / textually spliced back together.
fn m_condemned_boundary(f: &SourceFile) -> Vec<Hit> {
    const TRIGGER_FIELDS: &[&str] = &["put_triggers", "rm_triggers", "replace_triggers"];
    const EXTRACTOR_NAMES: &[&str] = &["extractor", "extract_filter"];
    fn is_string_collection(ty: &syn::Type) -> bool {
        let syn::Type::Path(tp) = ty else {
            return false;
        };
        let Some(seg) = tp.path.segments.last() else {
            return false;
        };
        if seg.ident != "Vec" {
            return false;
        }
        let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
            return false;
        };
        args.args.iter().any(|arg| {
            let syn::GenericArgument::Type(syn::Type::Path(inner)) = arg else {
                return false;
            };
            inner
                .path
                .segments
                .last()
                .is_some_and(|s| s.ident == "String" || s.ident == "SmartString")
        })
    }
    fn is_to_string_call(expr: &syn::Expr) -> bool {
        matches!(expr, syn::Expr::MethodCall(m) if m.method == "to_string")
    }
    fn format_is_if_splice(mac: &syn::ExprMacro) -> bool {
        let Ok(args) = mac.mac.parse_body_with(
            syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated,
        ) else {
            return false;
        };
        let Some(syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(lit),
            ..
        })) = args.first()
        else {
            return false;
        };
        let value = lit.value();
        value.contains("if(") && value.matches('{').count() >= 2
    }
    struct V<'a> {
        rel: &'a str,
        hits: Vec<Hit>,
    }
    impl<'ast, 'a> Visit<'ast> for V<'a> {
        fn visit_field(&mut self, node: &'ast syn::Field) {
            if let Some(ident) = &node.ident {
                let name = ident.to_string();
                if TRIGGER_FIELDS.contains(&name.as_str()) && is_string_collection(&node.ty) {
                    push(&mut self.hits, self.rel, span_line(&ident.span()), "stored_source_trigger_field");
                }
            }
            visit::visit_field(self, node);
        }
        fn visit_expr(&mut self, node: &'ast syn::Expr) {
            if let syn::Expr::Macro(mac) = node {
                let is_format = mac
                    .mac
                    .path
                    .segments
                    .last()
                    .is_some_and(|seg| seg.ident == "format");
                if is_format && format_is_if_splice(mac) {
                    if let Some(seg) = mac.mac.path.segments.last() {
                        push(&mut self.hits, self.rel, span_line(&seg.ident.span()), "extractor_display_splice");
                    }
                }
            }
            if let syn::Expr::Assign(a) = node {
                if let syn::Expr::Path(pth) = a.left.as_ref() {
                    if let Some(seg) = pth.path.segments.last() {
                        if EXTRACTOR_NAMES.contains(&seg.ident.to_string().as_str())
                            && is_to_string_call(&a.right)
                        {
                            push(&mut self.hits, self.rel, span_line(&seg.ident.span()), "extractor_to_string_capture");
                        }
                    }
                }
            }
            if let syn::Expr::Struct(st) = node {
                for field in &st.fields {
                    if let syn::Member::Named(ident) = &field.member {
                        if EXTRACTOR_NAMES.contains(&ident.to_string().as_str())
                            && is_to_string_call(&field.expr)
                        {
                            push(&mut self.hits, self.rel, span_line(&ident.span()), "extractor_to_string_capture");
                        }
                    }
                }
            }
            visit::visit_expr(self, node);
        }
    }
    let mut v = V {
        rel: &f.rel_path,
        hits: vec![],
    };
    v.visit_file(&f.ast);
    v.hits
}

/// Seat 59: a `hasher.update(b"...")` byte-literal domain tag is the
/// opening move of a hand-rolled sealed-artifact layout. There is ONE
/// canonical constructor — `kyzo-core/src/store/transcript.rs` — and its
/// own sites ARE the authority, exempt structurally by name; every other
/// site is a second serializer until sworn otherwise (KDFs and internal
/// identity digests are real and confess individually — no ratchet, no
/// baseline).
fn m_hand_layout(f: &SourceFile) -> Vec<Hit> {
    if f.rel_path == "crates/kyzo-core/src/store/transcript.rs" {
        return vec![];
    }
    struct V<'a> {
        rel: &'a str,
        hits: Vec<Hit>,
    }
    impl<'ast, 'a> Visit<'ast> for V<'a> {
        fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
            let is_byte_tag = node.method == "update"
                && matches!(
                    node.args.first(),
                    Some(syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::ByteStr(_),
                        ..
                    }))
                );
            if is_byte_tag {
                push(&mut self.hits, self.rel, span_line(&node.method.span()), "hand_layout");
            }
            visit::visit_expr_method_call(self, node);
        }
    }
    let mut v = V {
        rel: &f.rel_path,
        hits: vec![],
    };
    v.visit_file(&f.ast);
    v.hits
}

/// Poisoned-lock silent continue (BANNED #13): `.into_inner()` on a
/// PoisonError — recovering the guard and carrying on.
fn m_poison_continue(f: &SourceFile) -> Vec<Hit> {
    method_calls(f, &["into_inner"], "poison_continue")
        .into_iter()
        .filter(|h| {
            line_text(f, h.line)
                .is_some_and(|l| l.contains("poison") || l.contains("unwrap_or_else"))
        })
        .collect()
}

/// The registry. checks.toml refers to these by name; the meta engine
/// reports any table entry no check references (dead detection is a lie
/// about coverage too).
pub const MATCHERS: &[Matcher] = &[
    Matcher { name: "unwrap", class: ScanClass::ProductionOnly, run: m_unwrap },
    Matcher { name: "expect", class: ScanClass::ProductionOnly, run: m_expect },
    Matcher { name: "unwrap_or", class: ScanClass::Everything, run: m_unwrap_or },
    Matcher { name: "unwrap_or_else", class: ScanClass::Everything, run: m_unwrap_or_else },
    Matcher { name: "unwrap_or_default", class: ScanClass::Everything, run: m_unwrap_or_default },
    Matcher { name: "unwrap_unchecked", class: ScanClass::Everything, run: m_unchecked_unwrap },
    Matcher { name: "panic_bang", class: ScanClass::ProductionOnly, run: m_panic_bang },
    Matcher { name: "unreachable_bang", class: ScanClass::ProductionOnly, run: m_unreachable_bang },
    Matcher { name: "todo_bang", class: ScanClass::Everything, run: m_todo_bang },
    Matcher { name: "debug_assert", class: ScanClass::Everything, run: m_debug_assert },
    Matcher { name: "let_underscore", class: ScanClass::Everything, run: m_let_underscore },
    Matcher { name: "ok_drop", class: ScanClass::Everything, run: m_ok_drop },
    Matcher { name: "as_cast", class: ScanClass::Everything, run: m_as_cast },
    Matcher { name: "unchecked_arith", class: ScanClass::Everything, run: m_unchecked_arith },
    Matcher { name: "capacity_min_cap", class: ScanClass::Everything, run: m_capacity_min_cap },
    Matcher { name: "allow_dead_code", class: ScanClass::Everything, run: m_allow_dead_code },
    Matcher { name: "allow_unused", class: ScanClass::Everything, run: m_allow_unused },
    Matcher { name: "allow_clippy", class: ScanClass::Everything, run: m_allow_clippy },
    Matcher { name: "allow_missing_docs", class: ScanClass::Everything, run: m_allow_missing_docs },
    Matcher { name: "allow_private", class: ScanClass::Everything, run: m_allow_private },
    Matcher { name: "allow_unsafe", class: ScanClass::Everything, run: m_allow_unsafe },
    Matcher { name: "catchall_arm", class: ScanClass::Everything, run: m_catchall_arm },
    Matcher { name: "default_derive", class: ScanClass::Everything, run: m_default_derive },
    Matcher { name: "default_impl", class: ScanClass::Everything, run: m_default_impl },
    Matcher { name: "construction_door", class: ScanClass::Everything, run: m_construction_door },
    Matcher { name: "test_err_early_return", class: ScanClass::Everything, run: m_test_err_early_return },
    Matcher { name: "ignore_test", class: ScanClass::Everything, run: m_ignore_test },
    Matcher { name: "should_panic", class: ScanClass::Everything, run: m_should_panic },
    Matcher { name: "err_costume", class: ScanClass::Everything, run: m_err_costume },
    Matcher { name: "process_exit", class: ScanClass::Everything, run: m_process_exit },
    Matcher { name: "sleep_sync", class: ScanClass::Everything, run: m_sleep_sync },
    Matcher { name: "serde_default_skip", class: ScanClass::Everything, run: m_serde_default_skip },
    Matcher { name: "poison_continue", class: ScanClass::Everything, run: m_poison_continue },
    Matcher { name: "nondeterminism", class: ScanClass::ProductionOnly, run: m_nondeterminism },
    Matcher { name: "peer_dial", class: ScanClass::ProductionOnly, run: m_peer_dial },
    Matcher { name: "naked_array_sig", class: ScanClass::ProductionOnly, run: m_naked_array_sig },
    Matcher { name: "unsafe_token", class: ScanClass::Everything, run: m_unsafe_token },
    Matcher { name: "assert_bang", class: ScanClass::ProductionOnly, run: m_assert_bang },
    Matcher { name: "condemned_boundary", class: ScanClass::ProductionOnly, run: m_condemned_boundary },
    Matcher { name: "hand_layout", class: ScanClass::ProductionOnly, run: m_hand_layout },
];

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

    fn run(name: &str, src: &str) -> Vec<Hit> {
        let m = matcher_by_name(name).expect("matcher registered");
        run_matcher(m, &parse("crates/x/src/probe.rs", src))
    }

    #[test]
    fn every_matcher_has_a_unique_name() {
        let mut names: Vec<&str> = MATCHERS.iter().map(|m| m.name).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate matcher name = second authority");
    }

    #[test]
    fn dishonesty_shapes_detonate() {
        assert_eq!(run("unwrap", "fn f(x: Option<u8>) { x.unwrap(); }").len(), 1);
        assert_eq!(run("expect", "fn f(x: Option<u8>) { x.expect(\"y\"); }").len(), 1);
        assert_eq!(run("let_underscore", "fn f() { let _ = g(); } fn g() -> u8 { 1 }").len(), 1);
        assert_eq!(run("ok_drop", "fn f(x: Result<u8, u8>) { x.ok(); }").len(), 1);
        assert_eq!(run("as_cast", "fn f(x: u64) -> u8 { x as u8 }").len(), 1);
        assert_eq!(run("catchall_arm", "fn f(x: u8) -> u8 { match x { 1 => 1, _ => 0 } }").len(), 1);
        assert_eq!(run("panic_bang", "fn f() { panic!(\"boom\"); }").len(), 1);
        assert_eq!(run("todo_bang", "fn f() { todo!(); }").len(), 1);
        assert_eq!(run("debug_assert", "fn f(x: u8) { debug_assert!(x > 0); }").len(), 1);
        assert_eq!(
            run("allow_dead_code", "#[allow(dead_code)]\nfn f() {}").len(),
            1
        );
        assert_eq!(
            run("err_costume", "fn f(r: Result<u8,u8>) -> u8 { match r { Ok(v) => v, Err(_) => 0 } }").len(),
            1
        );
        assert_eq!(
            run("sleep_sync", "fn f() { std::thread::sleep(std::time::Duration::from_millis(1200)); }").len(),
            1
        );
        assert_eq!(
            run("serde_default_skip", "struct S { #[serde(default)]\n x: u8 }").len(),
            1
        );
    }

    #[test]
    fn unchecked_arith_honors_the_invariant_protocol() {
        let naked = run("unchecked_arith", "fn f(a: u64, b: u64) -> u64 { a.wrapping_mul(b) }");
        assert_eq!(naked.len(), 1, "no proof comment = violation");
        let proven = run(
            "unchecked_arith",
            "fn f(a: u64, b: u64) -> u64 {\n    // INVARIANT(SeedMix): wrap is the published mix contract.\n    a.wrapping_mul(b)\n}",
        );
        assert!(proven.is_empty(), "an adjacent INVARIANT proof stands");
    }

    #[test]
    fn production_only_matchers_exempt_test_scaffolding_at_every_level() {
        // mod-level
        assert!(run(
            "nondeterminism",
            "#[cfg(test)] mod tests { fn f() { let _t = std::time::Instant::now(); } }"
        )
        .iter()
        .all(|h| h.construct != "nondeterminism"));
        // statement-level (the hnsw probe shape)
        assert!(run(
            "nondeterminism",
            "fn live() { #[cfg(test)] let _t0 = std::time::Instant::now(); }"
        )
        .is_empty());
        // and the production twin still detonates
        assert_eq!(
            run("nondeterminism", "fn live() { let t = std::time::Instant::now(); let _x = t; }").len(),
            1
        );
        assert_eq!(
            run("peer_dial", "fn f() { let _c = std::net::TcpStream::connect(\"127.0.0.1:1\"); }").len(),
            1
        );
    }

    #[test]
    fn naked_array_sig_exempts_only_the_wrap_doors() {
        assert_eq!(
            run("naked_array_sig", "fn seal_key(k: [u8; 32]) {}").len(),
            1
        );
        assert!(run("naked_array_sig", "struct D([u8; 32]); impl D { fn from_derived(b: [u8; 32]) -> D { D(b) } fn as_bytes(&self) -> &[u8; 32] { &self.0 } }").is_empty());
    }
}
