/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Engine 2 — Graph: lies that are relational, invisible at any one site.
//! - `derive_bypass`: a type with a fallible constructor (`new`/`try_new*`
//!   returning `Result`/`Option`) must not also derive `Deserialize`,
//!   `From`, or `Default` — each builds by field assignment, a second door
//!   around the admission proof.
//! - `copy_detector`: near-identical fn/method bodies across files by
//!   normalized token-shingle similarity — second authority by copy.
//! - `agreement_registry`: every test named in `crates/xtask/agreements.toml`
//!   must still exist as a `fn` in the tree.

use std::collections::{BTreeMap, BTreeSet};

use quote::ToTokens;
use syn::visit::{self, Visit};

use crate::boundary::{SourceFile, span_line};
use crate::engines::Hit;

// ---------------------------------------------------------------------------
// derive_bypass
// ---------------------------------------------------------------------------

pub fn derive_bypass(files: &[&SourceFile]) -> Vec<Hit> {
    struct TypeFacts {
        file: String,
        line: usize,
        fallible_ctor: bool,
        bad_derives: Vec<String>,
    }
    let mut types: BTreeMap<String, TypeFacts> = BTreeMap::new();

    for f in files {
        struct V<'a> {
            rel: &'a str,
            types: &'a mut BTreeMap<String, TypeFacts>,
        }
        impl<'ast, 'a> Visit<'ast> for V<'a> {
            fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
                let bad: Vec<String> = derive_names(&node.attrs)
                    .into_iter()
                    .filter(|d| d == "Deserialize" || d == "Default" || d == "From")
                    .collect();
                self.types
                    .entry(node.ident.to_string())
                    .or_insert(TypeFacts {
                        file: self.rel.to_string(),
                        line: span_line(&node.ident.span()),
                        fallible_ctor: false,
                        bad_derives: vec![],
                    })
                    .bad_derives
                    .extend(bad);
                visit::visit_item_struct(self, node);
            }
            fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
                if let syn::Type::Path(tp) = &*node.self_ty {
                    if let Some(seg) = tp.path.segments.last() {
                        let ty = seg.ident.to_string();
                        for item in &node.items {
                            if let syn::ImplItem::Fn(m) = item {
                                let name = m.sig.ident.to_string();
                                let is_ctor = name == "new" || name.starts_with("try_new");
                                let ret = m.sig.output.to_token_stream().to_string();
                                if is_ctor && (ret.contains("Result <") || ret.contains("Option <"))
                                {
                                    self.types
                                        .entry(ty.clone())
                                        .or_insert(TypeFacts {
                                            file: self.rel.to_string(),
                                            line: span_line(&m.sig.ident.span()),
                                            fallible_ctor: false,
                                            bad_derives: vec![],
                                        })
                                        .fallible_ctor = true;
                                }
                            }
                        }
                    }
                }
                visit::visit_item_impl(self, node);
            }
        }
        let mut v = V {
            rel: &f.rel_path,
            types: &mut types,
        };
        v.visit_file(&f.ast);
    }

    types
        .into_iter()
        .filter(|(_, t)| t.fallible_ctor && !t.bad_derives.is_empty())
        .map(|(name, t)| Hit {
            file: t.file,
            line: t.line,
            construct: format!("{name}+derive({})", t.bad_derives.join("+")),
        })
        .collect()
}

fn derive_names(attrs: &[syn::Attribute]) -> Vec<String> {
    let mut out = vec![];
    for a in attrs {
        if a.path().is_ident("derive") {
            let text = a.to_token_stream().to_string();
            for cand in ["Deserialize", "Default", "From"] {
                if text.contains(cand) {
                    out.push(cand.to_string());
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// copy_detector
// ---------------------------------------------------------------------------

const SHINGLE: usize = 8;
const MIN_TOKENS: usize = 60;
/// Jaccard similarity floor, in percent. Integer arithmetic end to end:
/// `inter * 100 >= union * SIMILARITY_PERCENT` — no float, no cast.
const SIMILARITY_PERCENT: usize = 60;

struct Unit {
    label: String,
    file: String,
    line: usize,
    /// Line of the body's closing brace — used to refuse parent~nested-fn
    /// "pairs": a fn textually contained in another is the same text
    /// counted twice, one authority, not a copy.
    line_end: usize,
    shingles: BTreeSet<u64>,
}

pub fn copy_detector(files: &[&SourceFile]) -> Vec<Hit> {
    let mut units: Vec<Unit> = vec![];
    for f in files {
        struct V<'a> {
            rel: &'a str,
            units: &'a mut Vec<Unit>,
        }
        fn push_unit(units: &mut Vec<Unit>, rel: &str, name: &str, line: usize, body: &syn::Block) {
            let toks = normalize_tokens(body);
            if toks.len() < MIN_TOKENS {
                return;
            }
            let mut shingles = BTreeSet::new();
            for w in toks.windows(SHINGLE) {
                shingles.insert(fnv(w));
            }
            units.push(Unit {
                label: format!("{rel}::{name}"),
                file: rel.to_string(),
                line,
                line_end: body.brace_token.span.close().end().line,
                shingles,
            });
        }
        impl<'ast, 'a> Visit<'ast> for V<'a> {
            fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
                push_unit(
                    self.units,
                    self.rel,
                    &node.sig.ident.to_string(),
                    span_line(&node.sig.ident.span()),
                    &node.block,
                );
                visit::visit_item_fn(self, node);
            }
            fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
                push_unit(
                    self.units,
                    self.rel,
                    &node.sig.ident.to_string(),
                    span_line(&node.sig.ident.span()),
                    &node.block,
                );
                visit::visit_impl_item_fn(self, node);
            }
        }
        let mut v = V {
            rel: &f.rel_path,
            units: &mut units,
        };
        v.visit_file(&f.ast);
    }

    // Exact Jaccard via an inverted shingle index: a pair's co-occurrence
    // count across postings IS its intersection size (shingle sets are
    // deduped), so union = |A| + |B| - inter with no second pass. The
    // size-ratio skip is a proven bound, not a heuristic: inter ≤ min and
    // union ≥ max, so score ≤ min/max — below threshold means the pair
    // can never qualify.
    let mut postings: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (i, u) in units.iter().enumerate() {
        for s in &u.shingles {
            postings.entry(*s).or_default().push(i);
        }
    }
    let mut inter: BTreeMap<(usize, usize), usize> = BTreeMap::new();
    for list in postings.values() {
        for (ai, a) in list.iter().enumerate() {
            for b in &list[ai + 1..] {
                let (sa, sb) = (units[*a].shingles.len(), units[*b].shingles.len());
                let (mn, mx) = if sa < sb { (sa, sb) } else { (sb, sa) };
                if mn * 100 < mx * SIMILARITY_PERCENT {
                    continue;
                }
                *inter.entry((*a, *b)).or_insert(0) += 1;
            }
        }
    }
    let mut hits = vec![];
    for ((a, b), n) in &inter {
        let (a, b) = (&units[*a], &units[*b]);
        let union = a.shingles.len() + b.shingles.len() - n;
        if union == 0 || n * 100 < union * SIMILARITY_PERCENT {
            continue;
        }
        let contained = a.file == b.file
            && ((a.line <= b.line && b.line_end <= a.line_end)
                || (b.line <= a.line && a.line_end <= b.line_end));
        if contained {
            continue;
        }
        // The construct is the bare pair — stable across edits, so a
        // sworn waiver binds to the twins, not to a drifting score.
        hits.push(Hit {
            file: a.file.clone(),
            line: a.line,
            construct: format!("{} ~ {}", a.label, b.label),
        });
    }
    hits.sort_by(|x, y| {
        (x.file.as_str(), x.line, x.construct.as_str())
            .cmp(&(y.file.as_str(), y.line, y.construct.as_str()))
    });
    hits
}

fn normalize_tokens(block: &syn::Block) -> Vec<String> {
    block
        .to_token_stream()
        .into_iter()
        .flat_map(flatten)
        .collect()
}

fn flatten(t: proc_macro2::TokenTree) -> Vec<String> {
    if let proc_macro2::TokenTree::Group(g) = t {
        let mut v = vec![format!("open{:?}", g.delimiter())];
        v.extend(g.stream().into_iter().flat_map(flatten));
        v.push("close".to_string());
        return v;
    }
    if let proc_macro2::TokenTree::Ident(i) = &t {
        // Identifiers normalize to a class token so renames can't hide a
        // copy; keywords keep their spelling (they carry structure).
        let s = i.to_string();
        let kw = [
            "if", "else", "match", "for", "while", "loop", "let", "fn", "return", "mut", "ref",
            "move", "break", "continue",
        ];
        if kw.contains(&s.as_str()) {
            return vec![s];
        }
        return vec!["id".to_string()];
    }
    vec![t.to_string()]
}

fn fnv(words: &[String]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for w in words {
        for b in w.as_bytes() {
            h ^= u64::from(*b);
            // INVARIANT(FnvMix): FNV-1a's published contract wraps.
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h ^= 0x1f;
        // INVARIANT(FnvMix): word-boundary fold, same wrapping contract.
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

// ---------------------------------------------------------------------------
// agreement_registry
// ---------------------------------------------------------------------------

pub fn agreement_registry(files: &[&SourceFile], registry_text: &str) -> Vec<Hit> {
    let mut declared: Vec<(String, usize)> = vec![];
    for (idx, line) in registry_text.lines().enumerate() {
        if let Some(rest) = line.trim_start().strip_prefix("test_fn") {
            if let Some(name) = rest.split('"').nth(1) {
                declared.push((name.to_string(), idx + 1));
            }
        }
    }
    let mut defined: BTreeSet<String> = BTreeSet::new();
    for f in files {
        struct V<'a> {
            defined: &'a mut BTreeSet<String>,
        }
        impl<'ast, 'a> Visit<'ast> for V<'a> {
            fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
                self.defined.insert(node.sig.ident.to_string());
                visit::visit_item_fn(self, node);
            }
        }
        let mut v = V {
            defined: &mut defined,
        };
        v.visit_file(&f.ast);
    }
    declared
        .into_iter()
        .filter(|(name, _)| !defined.contains(name))
        .map(|(name, line)| Hit {
            file: "crates/xtask/agreements.toml".to_string(),
            line,
            construct: format!("declared-but-missing:{name}"),
        })
        .collect()
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
    fn derive_bypass_detonates_on_the_historical_shape() {
        let f = parse(
            "crates/x/src/a.rs",
            "#[derive(Default)] struct Interval(u8);\n impl Interval { fn new(x: u8) -> Result<Interval, ()> { if x > 0 { Ok(Interval(x)) } else { Err(()) } } }",
        );
        let hits = derive_bypass(&[&f]);
        assert_eq!(hits.len(), 1, "fallible ctor + Default derive = second door");
    }

    #[test]
    fn copy_detector_detonates_on_a_renamed_copy() {
        let body = "{ let mut acc = 0; for i in 0..100 { if i % 2 == 0 { acc += i * 3 + 7; } else { acc -= i / 2 + 11; } while acc > 500 { acc /= 2; } match acc { 0 => acc = 1, 1 => acc = 2, other => acc = other - 1, } } acc }";
        let a = parse("crates/x/src/a.rs", &format!("fn alpha() -> i64 {body}"));
        let b = parse("crates/y/src/b.rs", &format!("fn beta() -> i64 {body}"));
        let hits = copy_detector(&[&a, &b]);
        assert_eq!(hits.len(), 1, "a renamed byte-copy must be caught");
        assert_eq!(
            hits[0].construct, "crates/x/src/a.rs::alpha ~ crates/y/src/b.rs::beta",
            "construct is the stable pair a waiver binds to"
        );
    }

    #[test]
    fn agreement_registry_detonates_on_drift() {
        let f = parse("crates/x/src/a.rs", "fn law_alive() {}");
        let reg = "[[agreement]]\ntest_fn = \"law_alive\"\n[[agreement]]\ntest_fn = \"law_deleted\"\n";
        let hits = agreement_registry(&[&f], reg);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].construct.contains("law_deleted"));
    }
}
