/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! T17 guardian (#376 T17): named crypto/auth doors take typed `Dek` / `Kek`
//! / `Digest` / `Mac` / `Nonce` / `Signature`, never a naked `[u8; 32]` /
//! `[u8; 12]` / `[u8; 64]` in their signature. A raw fixed array in a
//! business-logic door's signature is the exact leak the newtype role-split
//! (seat 67, `store/crypto.rs`'s `AeadKeyBytes` deletion) exists to prevent:
//! a DEK and a KEK are both `[u8; 32]` at the byte level, so a naked-array
//! signature is where a wrong-kind key becomes silently type-representable
//! again.
//!
//! Before this landed, the only proof was `store/crypto.rs`'s own in-crate
//! `#[cfg(test)] mod tests` test (`t17_no_naked_fixed_arrays_in_auth_fn_signatures`),
//! which greps a hand-picked list of door names in one file. This check
//! generalizes the same law over the whole admission/crypto/auth surface —
//! `crates/kyzo-core/src/store/` and `crates/kyzo-core/src/session/` — by
//! walking the directory (`fsutil::walk_engine_sources`), so a new file
//! carrying a crypto/auth door is covered automatically, not just the one
//! file a hand-picked list happened to name.
//!
//! **Scope: every fn/method signature under `store/` and `session/`**,
//! `#[cfg(test)]` scopes excluded (`synutil::mod_is_test_scope`, the same
//! mechanism `banned_path::scan_banned_idents` already uses).
//!
//! **The structural exemption (no citation needed):** a newtype's OWN
//! constructor/accessor for the bytes it wraps — `admit`, `as_bytes` (and
//! `as_bytes_mut`), or a `from_*`/`of_*`-named constructor (`from_derived`,
//! `from_persisted`, `from_hex`, `of_u64`, …) — is exactly the "wrap
//! already-proven bytes" door every proven newtype needs at its own
//! definition (rust-values-success's newtype-scalar shape); banning it would
//! make the newtype itself unconstructible. `from_raw`/`from_bytes`/
//! `*_unchecked` are deliberately NOT in this exemption: `bs_detector`'s
//! `construction_door` pattern already bans those names outright, so they
//! cannot legitimately exist here to need one.
//!
//! **The cited exception (register-only):** the literal RustCrypto FFI edge
//! — the handful of bodies that call directly into `aes_gcm_siv`/
//! `chacha20poly1305` (whose own APIs take `&[u8; 32]`/`&[u8; 12]`) and the
//! ONE shared committing-AEAD body each side delegates to. Each such
//! function is cited individually in `resonance-allow.toml`'s
//! `[[naked_array_sig]]` table — following the same "no blanket exemption,
//! every survivor confessed" law `bs_detector` already enforces — naming the
//! file, function, and line, and answering why that exact site is the edge,
//! not a leak.

use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::allowlist::Allowlist;
use crate::fsutil::{SourceFile, span_line};
use crate::synutil::mod_is_test_scope;

/// Fixed-array widths this door law governs (nonce / digest·mac / signature).
const BANNED_WIDTHS: &[usize] = &[32, 12, 64];

/// One naked-array occurrence in a crypto/auth door's signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub file: String,
    pub line: usize,
    pub function: String,
    pub width: usize,
}

/// True for the admission/crypto/auth surface this door law governs.
fn is_admission_crypto_surface(rel_path: &str) -> bool {
    rel_path.starts_with("crates/kyzo-core/src/store/")
        || rel_path.starts_with("crates/kyzo-core/src/session/")
}

/// A newtype's own "wrap already-proven bytes" constructor/accessor — the
/// one shape every proven newtype needs at its own definition. `from_raw` /
/// `from_bytes` / `*_unchecked` are excluded on purpose: `bs_detector`'s
/// `construction_door` pattern already bans those names in production, so a
/// real occurrence here would already be a different, older violation.
fn fn_name_is_newtype_door(name: &str) -> bool {
    name == "admit"
        || name == "as_bytes"
        || name == "as_bytes_mut"
        || ((name.starts_with("from_") || name.starts_with("of_"))
            && !matches!(name, "from_raw" | "from_bytes"))
}

/// Every `[u8; N]` (N in [`BANNED_WIDTHS`]) reachable anywhere inside a
/// type — including nested in `&_`, `Result<_, _>`, `Option<_>`, tuples —
/// so a role door cannot launder a naked array through one layer of generic
/// wrapping.
struct ArrayWidths(Vec<usize>);

impl<'ast> Visit<'ast> for ArrayWidths {
    fn visit_type_array(&mut self, node: &'ast syn::TypeArray) {
        if let syn::Type::Path(p) = &*node.elem {
            if p.path.is_ident("u8") {
                if let syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Int(n),
                    ..
                }) = &node.len
                {
                    if let Ok(w) = n.base10_parse::<usize>() {
                        if BANNED_WIDTHS.contains(&w) {
                            self.0.push(w);
                        }
                    }
                }
            }
        }
        visit::visit_type_array(self, node);
    }
}

/// Widths found in `sig`'s parameter types and return type only — a
/// signature-only scan never descends into the function body, so a local
/// `let buf: [u8; 32]` inside an ordinary function never trips this door law.
fn signature_array_widths(sig: &syn::Signature) -> Vec<usize> {
    let mut v = ArrayWidths(Vec::new());
    for input in &sig.inputs {
        if let syn::FnArg::Typed(pat_type) = input {
            v.visit_type(&pat_type.ty);
        }
    }
    if let syn::ReturnType::Type(_, ty) = &sig.output {
        v.visit_type(ty);
    }
    v.0
}

struct Scanner<'a> {
    file: &'a str,
    hits: Vec<Violation>,
}

impl<'a> Scanner<'a> {
    fn check_sig(&mut self, sig: &syn::Signature, span: proc_macro2::Span) {
        let name = sig.ident.to_string();
        if fn_name_is_newtype_door(&name) {
            return;
        }
        for width in signature_array_widths(sig) {
            self.hits.push(Violation {
                file: self.file.to_string(),
                line: span_line(&span),
                function: name.clone(),
                width,
            });
        }
    }
}

impl<'ast, 'a> Visit<'ast> for Scanner<'a> {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if mod_is_test_scope(&node.ident, &node.attrs) {
            return;
        }
        visit::visit_item_mod(self, node);
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.check_sig(&node.sig, node.span());
        visit::visit_item_fn(self, node);
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        self.check_sig(&node.sig, node.span());
        visit::visit_impl_item_fn(self, node);
    }
}

/// True if `(file, function, line)` is confessed in the register.
fn is_registered(allow: &Allowlist, file: &str, function: &str, line: usize) -> bool {
    allow.naked_array_sig.iter().any(|e| {
        e.function == function && e.line == line && file.ends_with(e.file.trim_start_matches("./"))
    })
}

/// Every naked-array crypto/auth door on the admission/crypto/auth surface
/// that is not confessed in the register. Returns `(violations,
/// stale_register_entries)`.
pub fn check(files: &[SourceFile], allow: &Allowlist) -> (Vec<Violation>, Vec<String>) {
    let mut raw_hits: Vec<Violation> = Vec::new();
    for f in files {
        if !is_admission_crypto_surface(&f.rel_path) {
            continue;
        }
        let mut scanner = Scanner {
            file: &f.rel_path,
            hits: Vec::new(),
        };
        scanner.visit_file(&f.ast);
        raw_hits.extend(scanner.hits);
    }

    let violations: Vec<Violation> = raw_hits
        .iter()
        .filter(|v| !is_registered(allow, &v.file, &v.function, v.line))
        .cloned()
        .collect();

    let mut stale = Vec::new();
    for e in &allow.naked_array_sig {
        let still = raw_hits.iter().any(|v| {
            v.file.ends_with(e.file.trim_start_matches("./"))
                && v.function == e.function
                && v.line == e.line
        });
        if !still {
            stale.push(format!(
                "naked_array_sig register entry {}:{} (`{}`) no longer matches — remove it",
                e.file, e.line, e.function
            ));
        }
    }

    (violations, stale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowlist::{Allowlist, NakedArraySigEntry};

    fn sf(rel: &str, src: &str) -> SourceFile {
        SourceFile {
            rel_path: rel.to_string(),
            text: src.to_string(),
            ast: syn::parse_file(src).expect("fixture parses"),
        }
    }

    #[test]
    fn naked_array_role_door_is_caught() {
        let src = "fn seal_arm(key: &[u8; 32], nonce: &[u8; 12]) -> Vec<u8> { Vec::new() }";
        let f = sf("crates/kyzo-core/src/store/crypto.rs", src);
        let (violations, _) = check(&[f], &Allowlist::default());
        assert_eq!(
            violations.len(),
            2,
            "both the 32- and 12-byte params must be caught"
        );
        assert!(
            violations
                .iter()
                .any(|v| v.width == 32 && v.function == "seal_arm")
        );
        assert!(
            violations
                .iter()
                .any(|v| v.width == 12 && v.function == "seal_arm")
        );
    }

    #[test]
    fn naked_array_in_return_type_is_caught() {
        let src = "fn leak_kek(cap: &KekUnwrapCap) -> [u8; 32] { [0u8; 32] }";
        let f = sf("crates/kyzo-core/src/store/crypto.rs", src);
        let (violations, _) = check(&[f], &Allowlist::default());
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].width, 32);
    }

    #[test]
    fn newtype_only_signature_passes() {
        let src = "fn seal_arm(key: &Dek, nonce: &Nonce) -> Result<Vec<u8>, CryptoRefuse> { unimplemented!() }";
        let f = sf("crates/kyzo-core/src/store/crypto.rs", src);
        let (violations, _) = check(&[f], &Allowlist::default());
        assert!(
            violations.is_empty(),
            "a newtype-only signature must pass: {violations:?}"
        );
    }

    #[test]
    fn newtype_own_admit_and_as_bytes_doors_are_structurally_exempt() {
        let src = "impl Nonce {\n\
                    pub fn admit(bytes: [u8; 12]) -> Self { Self(bytes) }\n\
                    pub fn as_bytes(&self) -> &[u8; 12] { &self.0 }\n\
                    }";
        let f = sf("crates/kyzo-core/src/store/crypto.rs", src);
        let (violations, _) = check(&[f], &Allowlist::default());
        assert!(
            violations.is_empty(),
            "admit/as_bytes are the newtype's own bytes door: {violations:?}"
        );
    }

    #[test]
    fn from_raw_and_from_bytes_are_not_exempt() {
        // bs_detector's construction_door pattern already bans these names in
        // production; this door law must not grant them a second exemption.
        let src = "fn from_raw(bits: [u8; 32]) -> Self { Self(bits) }";
        let f = sf("crates/kyzo-core/src/store/crypto.rs", src);
        let (violations, _) = check(&[f], &Allowlist::default());
        assert_eq!(violations.len(), 1, "from_raw must not be silently exempt");
    }

    #[test]
    fn a_cited_register_entry_suppresses_exactly_its_own_site() {
        let src = "fn aes_gcm_siv_seal(key: &[u8; 32], nonce: &[u8; 12]) -> Vec<u8> { Vec::new() }";
        let f = sf("crates/kyzo-core/src/store/crypto.rs", src);
        let allow = Allowlist {
            naked_array_sig: vec![NakedArraySigEntry {
                file: "crates/kyzo-core/src/store/crypto.rs".to_string(),
                function: "aes_gcm_siv_seal".to_string(),
                line: 1,
                citation: "RustCrypto aes-gcm-siv edge fixture confession".to_string(),
            }],
            ..Allowlist::default()
        };
        let (violations, stale) = check(&[f], &allow);
        assert!(violations.is_empty(), "a cited edge site must not violate");
        assert!(stale.is_empty());
    }

    #[test]
    fn out_of_scope_file_is_not_scanned() {
        let src = "fn dial(key: [u8; 32]) -> [u8; 12] { unimplemented!() }";
        let f = sf("crates/kyzo-core/src/exec/plan/mod.rs", src);
        let (violations, _) = check(&[f], &Allowlist::default());
        assert!(violations.is_empty(), "exec/ is outside store/+session/");
    }

    #[test]
    fn test_scope_is_skipped() {
        let src =
            "#[cfg(test)] mod tests { fn seal_arm(key: &[u8; 32]) -> Vec<u8> { Vec::new() } }";
        let f = sf("crates/kyzo-core/src/store/crypto.rs", src);
        let (violations, _) = check(&[f], &Allowlist::default());
        assert!(violations.is_empty(), "test scope is out of law");
    }

    #[test]
    fn stale_register_entry_is_reported() {
        let src = "fn seal_arm(key: &Dek) -> Vec<u8> { Vec::new() }";
        let f = sf("crates/kyzo-core/src/store/crypto.rs", src);
        let allow = Allowlist {
            naked_array_sig: vec![NakedArraySigEntry {
                file: "crates/kyzo-core/src/store/crypto.rs".to_string(),
                function: "seal_arm".to_string(),
                line: 1,
                citation: "no longer matches after the role-split landed".to_string(),
            }],
            ..Allowlist::default()
        };
        let (violations, stale) = check(&[f], &allow);
        assert!(violations.is_empty());
        assert_eq!(stale.len(), 1);
    }
}
