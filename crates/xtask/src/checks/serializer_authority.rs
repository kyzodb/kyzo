/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Sealed-serializer authority ratchet (decisions.md seat 59): there is **one**
//! `CanonicalTranscript` constructor, and a second serialization path is
//! *Unconstructible*. This is the mechanical lock on the seat-59 consolidation —
//! the exact catastrophe it prevents from recurring: a sealed artifact
//! (CheckpointSeal, MergeProof, ForkGrant/RecoveryGrant, WAL header,
//! leave-is-free pack, STH) whose canonical bytes are hand-built field-by-field
//! instead of routed through the one transcript.
//!
//! **The tell.** After consolidation every sealed digest is
//! `h.update(transcript.as_bytes())` — the field layout lives *only* inside the
//! `store/transcript.rs` encoders, so the hasher sees no byte-string literal.
//! Every hand-rolled byte layout — a second serializer — begins instead with a
//! domain-tag literal fed straight to the hasher: `h.update(b"kyzo.<kind>.v1")`.
//! That `.update(<byte-string-literal>)` is the fingerprint this ratchet counts.
//!
//! **Why a baseline, not a ban.** A byte-literal hasher update is *also* the
//! shape of a legitimate internal key-derivation / identity digest (a DEK KDF,
//! the chained-state-root bind, a nonce, an OperationKey) — those are not sealed
//! artifacts and correctly hash raw fields. So the law is a ratchet, mirroring
//! `unchecked_arith`: the count of hand-rolled-layout sites on the store surface
//! (outside the one `transcript.rs` constructor, outside test scaffolding) may
//! never *rise*. A rise means a new hand layout appeared — which is either a
//! sealed-artifact serializer that must instead route through the transcript, or
//! a genuinely new internal digest that must be justified by consciously raising
//! [`BASELINE`] in a reviewed commit. Either way a human must look. A drop means
//! the surface got purer — tighten [`BASELINE`] to lock the win in.

use syn::visit::{self, Visit};

use crate::fsutil::{SourceFile, span_line};
use crate::synutil::mod_is_test_scope;

/// Committed count of hand-rolled byte-layout sites on the store surface
/// (byte-string-literal hasher updates outside `transcript.rs` / test scope).
/// Measured on the seat-59-consolidated tree. Raising it is a reviewed act;
/// lowering the real count means this must be tightened.
pub const BASELINE: usize = 24;

/// One `h.update(b"...")` site — a domain-tagged byte layout fed to a hasher.
pub struct Site {
    pub file: String,
    pub line: usize,
}

/// True for the sealed-artifact surface this ratchet governs: `store/` inside
/// `kyzo-core`, excluding the one canonical constructor (`transcript.rs`).
fn is_sealed_surface(rel_path: &str) -> bool {
    rel_path.starts_with("crates/kyzo-core/src/store/") && !rel_path.ends_with("/transcript.rs")
}

struct Scanner {
    hits: Vec<usize>,
}

impl<'ast> Visit<'ast> for Scanner {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        // Test scaffolding may hand-build bytes to forge a corrupt artifact.
        if mod_is_test_scope(&node.ident, &node.attrs) {
            return;
        }
        visit::visit_item_mod(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if node.method == "update"
            && let Some(first) = node.args.first()
            && expr_is_byte_str_literal(first)
        {
            self.hits.push(span_line(&node.method.span()));
        }
        visit::visit_expr_method_call(self, node);
    }
}

/// A `b"..."` byte-string literal — the domain-tag shape a hand layout opens with.
fn expr_is_byte_str_literal(expr: &syn::Expr) -> bool {
    matches!(
        expr,
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::ByteStr(_),
            ..
        })
    )
}

/// Every hand-rolled-layout site on the store surface (outside the one
/// transcript constructor and outside test scope).
pub fn check(files: &[SourceFile]) -> Vec<Site> {
    let mut sites = vec![];
    for f in files {
        if !is_sealed_surface(&f.rel_path) {
            continue;
        }
        let mut s = Scanner { hits: vec![] };
        s.visit_file(&f.ast);
        for line in s.hits {
            sites.push(Site {
                file: f.rel_path.clone(),
                line,
            });
        }
    }
    sites
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
    fn flags_a_hand_rolled_sealed_layout() {
        // A re-introduced hand serializer: domain tag + raw fields to a hasher.
        let f = parse(
            "crates/kyzo-core/src/store/seal.rs",
            "fn digest(p: &P) { let mut h = Sha256::new(); \
             h.update(b\"kyzo.checkpoint_seal.v1\"); h.update(p.store_id()); }",
        );
        let hits = check(std::slice::from_ref(&f));
        assert_eq!(
            hits.len(),
            1,
            "the b\"...\" domain-tag update must be counted"
        );
    }

    #[test]
    fn ignores_the_correct_transcript_digest() {
        // The consolidated shape: hash the canonical transcript, no byte literal.
        let f = parse(
            "crates/kyzo-core/src/store/seal.rs",
            "fn digest(t: &T) { let mut h = Sha256::new(); h.update(t.as_bytes()); }",
        );
        assert!(
            check(std::slice::from_ref(&f)).is_empty(),
            "hashing transcript.as_bytes() is the one lawful path — never counted"
        );
    }

    #[test]
    fn ignores_the_one_transcript_constructor() {
        // transcript.rs itself legitimately hashes byte literals building the
        // canonical encoding — it IS the one constructor.
        let f = parse(
            "crates/kyzo-core/src/store/transcript.rs",
            "fn enc() { let mut h = Sha256::new(); h.update(b\"kyzo.transcript.v1\"); }",
        );
        assert!(check(std::slice::from_ref(&f)).is_empty());
    }

    #[test]
    fn ignores_non_store_and_test_scope() {
        let outside = parse(
            "crates/kyzo-core/src/session/admit.rs",
            "fn f() { let mut h = Sha256::new(); h.update(b\"x\"); }",
        );
        assert!(
            check(std::slice::from_ref(&outside)).is_empty(),
            "off the store surface"
        );
        let test_scope = parse(
            "crates/kyzo-core/src/store/seal.rs",
            "#[cfg(test)] mod tests { fn f() { let mut h = Sha256::new(); h.update(b\"x\"); } }",
        );
        assert!(
            check(std::slice::from_ref(&test_scope)).is_empty(),
            "test scaffolding"
        );
    }
}
