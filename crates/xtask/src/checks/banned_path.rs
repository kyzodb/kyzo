/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Shared primitive for symbol-ban ratchets: scan a source file for any path
//! whose segments name a banned identifier, skipping `#[cfg(test)]` scopes.
//!
//! One scanner, many bans. Both the peer-dial ban and the determinism ban are
//! "this identifier may not appear in this crate scope" laws — factoring the
//! path walk here keeps them a single authority instead of two near-identical
//! copies (which is the very second-authority shape `copy_detector` deletes).
//! Callers own only their banned set and their file-scope predicate.

use syn::visit::{self, Visit};

use crate::fsutil::{span_line, SourceFile};
use crate::synutil::mod_is_test_scope;

/// One banned-identifier occurrence: the line and the exact segment matched.
pub struct Hit {
    pub line: usize,
    pub ident: String,
}

struct Scanner<'a> {
    banned: &'a [&'a str],
    hits: Vec<Hit>,
}

impl<'ast, 'a> Visit<'ast> for Scanner<'a> {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if mod_is_test_scope(&node.ident, &node.attrs) {
            return;
        }
        visit::visit_item_mod(self, node);
    }

    fn visit_path(&mut self, node: &'ast syn::Path) {
        // Scan EVERY segment: in `std::net::TcpStream::connect` or
        // `std::time::Instant::now` the banned name is an interior segment,
        // and the final segment (`connect` / `now`) is innocent.
        for seg in &node.segments {
            let ident = seg.ident.to_string();
            if self.banned.contains(&ident.as_str()) {
                self.hits.push(Hit {
                    line: span_line(&seg.ident.span()),
                    ident,
                });
            }
        }
        visit::visit_path(self, node);
    }
}

/// Every banned-identifier occurrence in `file`, outside test scaffolding.
pub fn scan_banned_idents(file: &SourceFile, banned: &[&str]) -> Vec<Hit> {
    let mut s = Scanner {
        banned,
        hits: vec![],
    };
    s.visit_file(&file.ast);
    s.hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> SourceFile {
        SourceFile {
            rel_path: "probe.rs".to_string(),
            text: src.to_string(),
            ast: syn::parse_file(src).expect("parses"),
        }
    }

    #[test]
    fn matches_interior_segment_not_just_last() {
        let f = parse("fn f() { let _ = std::time::Instant::now(); }");
        let hits = scan_banned_idents(&f, &["Instant"]);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].ident, "Instant");
    }

    #[test]
    fn skips_test_scope() {
        let f = parse("#[cfg(test)] mod tests { fn f() { let _ = Instant::now(); } }");
        assert!(scan_banned_idents(&f, &["Instant"]).is_empty());
    }

    #[test]
    fn clean_passes() {
        let f = parse("fn f() -> u64 { 7 }");
        assert!(scan_banned_idents(&f, &["Instant", "TcpStream"]).is_empty());
    }
}
