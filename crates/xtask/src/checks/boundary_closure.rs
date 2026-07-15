/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Boundary-closure ratchet: the two condemned shapes story #299 deleted
//! from `kyzo-core`'s trigger and extractor boundaries may never return.
//!
//! Story #299's demolition (commit `2c83795`) cut two raw-but-probably-
//! valid forms whole: triggers stored as raw KyzoScript source strings
//! (re-parsed at fire time), and FTS/LSH extractor expressions captured via
//! `Display`/`to_string` and textually spliced back together for
//! re-parsing. T2/T3 rebuilt both boundaries as typed substances — parsed
//! `Trigger`s with provenance, and a typed `Expr` folded through
//! `combine_extractor` — so a value that would fail its own parse can no
//! longer be stored. This check is the mechanical half of that closure: a
//! grep that fails the moment either condemned shape is reintroduced,
//! exactly as `allocation_admission` closes its own boundary with no
//! allowlist to grant. Test scope is exempt — the deliberate corruption
//! fixture in `runtime/relation.rs` (`ShadowHandle`) reconstructs the
//! condemned trigger shape on purpose, to prove decode refuses it.

use syn::visit::{self, Visit};

use crate::fsutil::{SourceFile, span_line};
use crate::synutil::mod_is_test_scope;

/// Struct-field names that once held raw trigger source and must never
/// again be typed as a string collection.
const CONDEMNED_TRIGGER_FIELDS: &[&str] = &["put_triggers", "rm_triggers", "replace_triggers"];

/// Local/field names that once carried the parsed extractor `Expr` back out
/// as text, for the `.to_string()`-capture and struct-literal checks.
const CONDEMNED_EXTRACTOR_NAMES: &[&str] = &["extractor", "extract_filter"];

/// One condemned shape found back in non-test `kyzo-core`/`kyzo-bin` code.
pub struct Violation {
    pub file: String,
    pub line: usize,
    pub shape: &'static str,
    pub detail: String,
}

/// True if `ty` is `Vec<String>`, `Vec<SmartString<..>>`, or an equivalent
/// collection-of-strings shape — the condemned raw-source trigger
/// representation.
fn is_string_collection_type(ty: &syn::Type) -> bool {
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

/// True if `expr` is a bare `.to_string()` method call — the condemned
/// `Display`-capture of a parsed extractor expression.
fn is_to_string_call(expr: &syn::Expr) -> bool {
    matches!(expr, syn::Expr::MethodCall(m) if m.method == "to_string")
}

/// True if a `format!` macro's literal format string looks like the
/// condemned `if({filter}, {extractor})` textual splice: it mentions
/// `if(` and captures at least two interpolated values.
fn format_macro_looks_like_if_splice(mac: &syn::ExprMacro) -> bool {
    let Ok(args) = mac.mac.parse_body_with(
        syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated,
    ) else {
        return false;
    };
    let Some(syn::Expr::Lit(syn::ExprLit {
        lit: syn::Lit::Str(s),
        ..
    })) = args.first()
    else {
        return false;
    };
    let value = s.value();
    value.contains("if(") && value.matches('{').count() >= 2
}

struct Scanner {
    hits: Vec<(usize, &'static str, String)>,
}

impl<'ast> Visit<'ast> for Scanner {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        // Test scaffolding is out of scope: the deliberate corruption
        // fixture reconstructing the condemned trigger shape lives here.
        if mod_is_test_scope(&node.ident, &node.attrs) {
            return;
        }
        visit::visit_item_mod(self, node);
    }

    fn visit_field(&mut self, node: &'ast syn::Field) {
        if let Some(ident) = &node.ident {
            let name = ident.to_string();
            if CONDEMNED_TRIGGER_FIELDS.contains(&name.as_str())
                && is_string_collection_type(&node.ty)
            {
                self.hits.push((
                    span_line(&ident.span()),
                    "stored-source-trigger-field",
                    format!(
                        "field `{name}` is typed as a string collection — triggers must be \
                         stored as parsed typed substances, never raw source re-parsed at \
                         fire time"
                    ),
                ));
            }
        }
        visit::visit_field(self, node);
    }

    fn visit_expr(&mut self, node: &'ast syn::Expr) {
        match node {
            syn::Expr::Macro(mac) => {
                if mac
                    .mac
                    .path
                    .segments
                    .last()
                    .is_some_and(|seg| seg.ident == "format")
                    && format_macro_looks_like_if_splice(mac)
                {
                    self.hits.push((
                        span_line(&mac.mac.path.segments.last().unwrap().ident.span()),
                        "extractor-display-splice",
                        "format!(...) textually splices an `if(filter, extractor)` extractor \
                         expression back together — fold typed sub-expressions instead \
                         (see `combine_extractor`)"
                            .to_string(),
                    ));
                }
            }
            syn::Expr::Assign(a) => {
                if let syn::Expr::Path(p) = a.left.as_ref()
                    && let Some(seg) = p.path.segments.last()
                    && CONDEMNED_EXTRACTOR_NAMES.contains(&seg.ident.to_string().as_str())
                    && is_to_string_call(&a.right)
                {
                    self.hits.push((
                        span_line(&seg.ident.span()),
                        "extractor-to-string-capture",
                        format!(
                            "`{}` is assigned a `.to_string()` capture of a parsed extractor \
                             expression — store the typed `Expr`, never its Display text",
                            seg.ident
                        ),
                    ));
                }
            }
            syn::Expr::Struct(s) => {
                for field in &s.fields {
                    if let syn::Member::Named(ident) = &field.member
                        && CONDEMNED_EXTRACTOR_NAMES.contains(&ident.to_string().as_str())
                        && is_to_string_call(&field.expr)
                    {
                        self.hits.push((
                            span_line(&ident.span()),
                            "extractor-to-string-capture",
                            format!(
                                "field `{ident}` is initialized from a `.to_string()` capture \
                                 of a parsed extractor expression — store the typed `Expr`, \
                                 never its Display text"
                            ),
                        ));
                    }
                }
            }
            _ => {}
        }
        visit::visit_expr(self, node);
    }
}

/// Scan every first-party source file for the two condemned boundary
/// shapes story #299 deleted: stored-source trigger fields, and
/// `Display`-splice/`.to_string()`-capture extractor round-trips.
pub fn check(files: &[SourceFile]) -> Vec<Violation> {
    let mut violations = vec![];
    for f in files {
        let mut s = Scanner { hits: vec![] };
        s.visit_file(&f.ast);
        for (line, shape, detail) in s.hits {
            violations.push(Violation {
                file: f.rel_path.clone(),
                line,
                shape,
                detail,
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
    fn flags_stored_source_trigger_fields() {
        let f = src("struct RelationHandle {\n\
                 put_triggers: Vec<String>,\n\
                 rm_triggers: Vec<smartstring::SmartString<smartstring::LazyCompact>>,\n\
                 replace_triggers: Vec<String>,\n\
             }");
        let violations = check(&[f]);
        assert_eq!(
            violations.len(),
            3,
            "all three condemned trigger fields are flagged"
        );
        assert!(
            violations
                .iter()
                .all(|v| v.shape == "stored-source-trigger-field")
        );
    }

    #[test]
    fn typed_trigger_field_passes() {
        let f = src("struct RelationHandle {\n\
                 put_triggers: Vec<Trigger>,\n\
                 rm_triggers: Vec<Trigger>,\n\
                 replace_triggers: Vec<Trigger>,\n\
             }");
        assert!(
            check(&[f]).is_empty(),
            "triggers typed as the parsed `Trigger` substance are lawful"
        );
    }

    #[test]
    fn flags_extractor_display_splice() {
        let f = src(
            "fn combine(extract_filter: String, extractor: String) -> String {\n\
                 format!(\"if({extract_filter}, {extractor})\")\n\
             }",
        );
        let violations = check(&[f]);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].shape, "extractor-display-splice");
    }

    #[test]
    fn flags_extractor_to_string_capture_in_assignment_and_struct_literal() {
        let f = src("fn a(ex: Expr) {\n\
                 let mut extractor;\n\
                 extractor = ex.to_string();\n\
             }\n\
             fn b(ex: Expr) -> FtsIndexConfig {\n\
                 FtsIndexConfig { extractor: ex.to_string(), extract_filter: ex.to_string() }\n\
             }");
        let violations = check(&[f]);
        assert_eq!(
            violations.len(),
            3,
            "the bare assignment and both struct-literal captures are flagged"
        );
        assert!(
            violations
                .iter()
                .all(|v| v.shape == "extractor-to-string-capture")
        );
    }

    #[test]
    fn typed_expr_extractor_construction_passes() {
        let f = src(
            "fn combine(extractor: Option<Expr>, extract_filter: Option<Expr>) -> Expr {\n\
                 let extractor = extractor.unwrap();\n\
                 FtsIndexConfig { extractor, extract_filter: extract_filter.unwrap() }.extractor\n\
             }",
        );
        assert!(
            check(&[f]).is_empty(),
            "folding typed sub-expressions (never Display/to_string) is lawful"
        );
    }

    #[test]
    fn unrelated_format_and_to_string_calls_pass() {
        let f = src("fn a(x: i32, y: i32) -> String { format!(\"{x}:{y}\") }\n\
             fn b(n: i32) -> String { n.to_string() }");
        assert!(
            check(&[f]).is_empty(),
            "an ordinary format! or to_string() unrelated to the extractor/trigger shapes \
             is not condemned"
        );
    }

    #[test]
    fn test_scope_is_exempt() {
        let f = src("#[cfg(test)]\n\
             mod tests {\n\
                 struct ShadowHandle {\n\
                     put_triggers: Vec<String>,\n\
                     rm_triggers: Vec<String>,\n\
                     replace_triggers: Vec<String>,\n\
                 }\n\
             }");
        assert!(
            check(&[f]).is_empty(),
            "the deliberate corruption fixture reconstructing the condemned trigger shape \
             is test scaffolding, not production surface"
        );
    }
}
