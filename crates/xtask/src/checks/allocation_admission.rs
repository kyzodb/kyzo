/*
 * Copyright 2025 the Kyzo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Allocation-admission ratchet: no `with_capacity`/`reserve` argument may
//! carry its own `.min(...)` cap.
//!
//! A caller- or user-declared reservation size is bounded in exactly ONE
//! place — `kyzo_core`'s `capacity::admit` — so "cap the reservation at the
//! available count" is a single law, not a per-site patch re-derived (and
//! forgettable) at each allocation. `admit` performs the `.min` internally;
//! every other reservation argument must be a proven-finite size (a `.len()`,
//! a literal, an `admit(...)` result), never a bare declared size capped
//! inline. This check has no allowlist: an inline `.min` cap can ALWAYS be
//! expressed as `admit(declared, available)`, so there is no exception to
//! grant.

use syn::visit::{self, Visit};

use crate::fsutil::{SourceFile, span_line};
use crate::synutil::mod_is_test_scope;

/// Reservation method calls (`x.reserve(n)` …) whose size argument this check
/// governs. `with_capacity` is an associated function, handled separately.
const RESERVE_METHODS: &[&str] = &[
    "reserve",
    "reserve_exact",
    "try_reserve",
    "try_reserve_exact",
];

/// One inline-capped reservation: a `with_capacity`/`reserve` call whose size
/// argument carries a `.min(...)` instead of routing through `capacity::admit`.
pub struct Violation {
    pub file: String,
    pub line: usize,
    pub call: String,
}

/// Detects a `.min(...)` method call anywhere within a reservation-size
/// argument subtree — the per-site cap the seam replaces.
struct HasMinCap(bool);
impl<'ast> Visit<'ast> for HasMinCap {
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if node.method == "min" {
            self.0 = true;
        }
        visit::visit_expr_method_call(self, node);
    }
}

fn arg_has_min_cap(arg: &syn::Expr) -> bool {
    let mut v = HasMinCap(false);
    v.visit_expr(arg);
    v.0
}

struct Scanner {
    hits: Vec<(usize, String)>,
}
impl<'ast> Visit<'ast> for Scanner {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        // Test scaffolding is out of scope: a fixture may cap inline.
        if mod_is_test_scope(&node.ident, &node.attrs) {
            return;
        }
        visit::visit_item_mod(self, node);
    }

    fn visit_expr(&mut self, node: &'ast syn::Expr) {
        match node {
            // `T::with_capacity(arg)` — an associated-function call.
            syn::Expr::Call(c) => {
                if let syn::Expr::Path(p) = c.func.as_ref()
                    && let Some(seg) = p.path.segments.last()
                    && seg.ident == "with_capacity"
                    && let Some(arg) = c.args.first()
                    && arg_has_min_cap(arg)
                {
                    self.hits
                        .push((span_line(&seg.ident.span()), "with_capacity".to_string()));
                }
            }
            // `x.reserve(arg)` / `x.reserve_exact(arg)` — method calls.
            syn::Expr::MethodCall(m) => {
                let name = m.method.to_string();
                if RESERVE_METHODS.contains(&name.as_str())
                    && let Some(arg) = m.args.first()
                    && arg_has_min_cap(arg)
                {
                    self.hits.push((span_line(&m.method.span()), name));
                }
            }
            _ => {}
        }
        visit::visit_expr(self, node);
    }
}

/// Scan every first-party source file for inline-capped reservations.
pub fn check(files: &[SourceFile]) -> Vec<Violation> {
    let mut violations = vec![];
    for f in files {
        let mut s = Scanner { hits: vec![] };
        s.visit_file(&f.ast);
        for (line, call) in s.hits {
            violations.push(Violation {
                file: f.rel_path.clone(),
                line,
                call,
            });
        }
    }
    violations
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
    fn flags_inline_min_cap_in_with_capacity_and_reserve() {
        let f = src(
            "fn a(k: usize, v: Vec<u8>) { let _ = Vec::<u8>::with_capacity(k.min(v.len())); }\n\
             fn b(k: usize) { let mut w: Vec<u8> = Vec::new(); w.reserve(k.min(4)); }",
        );
        assert_eq!(
            check(&[f]).len(),
            2,
            "both the with_capacity and the reserve inline caps are flagged"
        );
    }

    #[test]
    fn admit_len_and_literal_sizes_pass() {
        let f = src(
            "fn a(k: usize, v: Vec<u8>) { let _ = Vec::<u8>::with_capacity(crate::capacity::admit(k, v.len())); }\n\
             fn b(v: Vec<u8>) { let _ = Vec::<u8>::with_capacity(v.len()); }\n\
             fn c() { let _ = Vec::<u8>::with_capacity(16); }",
        );
        assert!(
            check(&[f]).is_empty(),
            "admit(), .len(), and literal reservation sizes are all lawful"
        );
    }

    #[test]
    fn test_scope_is_exempt() {
        let f = src(
            "#[cfg(test)]\nmod tests { fn t(k: usize, v: Vec<u8>) { let _ = Vec::<u8>::with_capacity(k.min(v.len())); } }",
        );
        assert!(
            check(&[f]).is_empty(),
            "test scaffolding may cap a fixture inline"
        );
    }
}
