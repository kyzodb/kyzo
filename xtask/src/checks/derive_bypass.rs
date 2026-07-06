/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Check 1 (the hardest, per the story): a type with a fallible constructor
//! — `fn new(...)` (or `fn try_new*`) returning `Result`/`Option` — must
//! never *also* derive `Deserialize`, `From`, or `Default`. Each of those
//! three derives builds the type by direct field assignment, which is
//! exactly how `Interval` (issue #62, fork-base) and `RelationId` (its
//! second site) both bypassed their own invariant: the smart constructor
//! that refuses `end <= start` / an out-of-48-bit-range id was never on the
//! decode path at all.
//!
//! The taxonomy is declared, not inferred: "invariant-carrying type" here
//! means, precisely, *any* type with a `new`/`try_new*` returning
//! `Result`/`Option` — the constructor's own fallibility is the declaration
//! that the type has a real invariant. A type with an infallible `new` is
//! not in scope (nothing to bypass); one is added to `resonance-allow.toml`
//! with a citation if its fallible constructor is legitimately absent from
//! the wire path (nothing decodes it) rather than fixed by hand-writing
//! `Deserialize` the way `Interval`/`RelationId` were.

use std::collections::BTreeMap;

use syn::visit::{self, Visit};

use crate::allowlist::Allowlist;
use crate::fsutil::{SourceFile, span_line};
use crate::synutil::mod_is_test_scope;

const BYPASS_DERIVES: &[&str] = &["Deserialize", "From", "Default"];

/// One derive-bearing type: does it derive a bypass-relevant trait, and at
/// what span (for reporting).
#[derive(Default, Clone)]
struct TypeFacts {
    derives_bypass: Vec<String>,
    def_line: usize,
}

/// One fallible constructor found on an inherent impl of some type name.
struct FallibleCtor {
    type_name: String,
    fn_name: String,
    line: usize,
}

struct Collector {
    types: BTreeMap<String, TypeFacts>,
    ctors: Vec<FallibleCtor>,
}

fn derive_hits(attrs: &[syn::Attribute]) -> Vec<String> {
    let mut hits = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("derive") {
            continue;
        }
        let Ok(paths) = attr.parse_args_with(
            syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
        ) else {
            continue;
        };
        for p in paths {
            if let Some(last) = p.segments.last() {
                let ident = last.ident.to_string();
                if BYPASS_DERIVES.contains(&ident.as_str()) {
                    hits.push(ident);
                }
            }
        }
    }
    hits
}

fn is_fallible_ctor_name(name: &str) -> bool {
    name == "new" || name.starts_with("try_new")
}

fn returns_result_or_option(sig: &syn::Signature) -> bool {
    let syn::ReturnType::Type(_, ty) = &sig.output else {
        return false;
    };
    let syn::Type::Path(tp) = ty.as_ref() else {
        return false;
    };
    tp.path
        .segments
        .last()
        .map(|s| s.ident == "Result" || s.ident == "Option")
        .unwrap_or(false)
}

impl<'ast> Visit<'ast> for Collector {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if mod_is_test_scope(&node.ident, &node.attrs) {
            return; // shadow/hostile-fixture types in test scopes are not production types
        }
        visit::visit_item_mod(self, node);
    }

    fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
        let hits = derive_hits(&node.attrs);
        if !hits.is_empty() {
            self.types.insert(
                node.ident.to_string(),
                TypeFacts {
                    derives_bypass: hits,
                    def_line: span_line(&node.ident.span()),
                },
            );
        }
        visit::visit_item_struct(self, node);
    }

    fn visit_item_enum(&mut self, node: &'ast syn::ItemEnum) {
        let hits = derive_hits(&node.attrs);
        if !hits.is_empty() {
            self.types.insert(
                node.ident.to_string(),
                TypeFacts {
                    derives_bypass: hits,
                    def_line: span_line(&node.ident.span()),
                },
            );
        }
        visit::visit_item_enum(self, node);
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        if node.trait_.is_some() {
            visit::visit_item_impl(self, node);
            return; // trait impls (incl. a hand-written Deserialize) are not the bypass shape
        }
        let syn::Type::Path(tp) = node.self_ty.as_ref() else {
            visit::visit_item_impl(self, node);
            return;
        };
        let Some(type_name) = tp.path.segments.last().map(|s| s.ident.to_string()) else {
            visit::visit_item_impl(self, node);
            return;
        };
        for item in &node.items {
            if let syn::ImplItem::Fn(f) = item
                && is_fallible_ctor_name(&f.sig.ident.to_string())
                && returns_result_or_option(&f.sig)
            {
                self.ctors.push(FallibleCtor {
                    type_name: type_name.clone(),
                    fn_name: f.sig.ident.to_string(),
                    line: span_line(&f.sig.ident.span()),
                });
            }
        }
        visit::visit_item_impl(self, node);
    }
}

pub struct Violation {
    pub file: String,
    pub type_name: String,
    pub def_line: usize,
    pub ctor_name: String,
    pub ctor_line: usize,
    pub derives: Vec<String>,
}

pub fn check(files: &[SourceFile], allow: &Allowlist) -> (Vec<Violation>, Vec<String>) {
    let mut violations = Vec::new();
    let mut stale = Vec::new();

    for f in files {
        let mut collector = Collector {
            types: BTreeMap::new(),
            ctors: Vec::new(),
        };
        collector.visit_file(&f.ast);

        for ctor in &collector.ctors {
            let Some(facts) = collector.types.get(&ctor.type_name) else {
                continue;
            };
            let allowed = allow.derive_bypass.iter().any(|e| {
                e.type_name == ctor.type_name
                    && f.rel_path.ends_with(e.file.trim_start_matches("./"))
            });
            if allowed {
                continue;
            }
            violations.push(Violation {
                file: f.rel_path.clone(),
                type_name: ctor.type_name.clone(),
                def_line: facts.def_line,
                ctor_name: ctor.fn_name.clone(),
                ctor_line: ctor.line,
                derives: facts.derives_bypass.clone(),
            });
        }
    }

    // Stale-waiver check: every allowlist entry must still name a type that
    // actually exists with a derive hit somewhere in the tree, or the
    // waiver has drifted from what it claims to excuse.
    for e in &allow.derive_bypass {
        let still_present = files.iter().any(|f| {
            if !f.rel_path.ends_with(e.file.trim_start_matches("./")) {
                return false;
            }
            let mut collector = Collector {
                types: BTreeMap::new(),
                ctors: Vec::new(),
            };
            collector.visit_file(&f.ast);
            collector.types.contains_key(&e.type_name)
        });
        if !still_present {
            stale.push(format!(
                "derive_bypass allowlist entry for `{}` in {} no longer matches any derive in the tree — remove it",
                e.type_name, e.file
            ));
        }
    }

    (violations, stale)
}
