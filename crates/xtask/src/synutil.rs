/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Small `syn`-tree helpers shared by more than one check. Kept here,
//! rather than duplicated per check, on the same principle check 3 (the
//! copy-detector) enforces on the engine tree itself.

use quote::ToTokens;

/// True if `mod` (by its own `#[cfg(test)]`/`#[cfg(any(test, ...))]`
/// attribute, or by convention — an ident of `tests`/`test`) is a
/// test-only scope. Shadow/hostile-fixture types and test-only helper
/// functions live in these scopes; they are not production surface, so
/// every check that walks the tree skips recursing into them.
pub fn mod_is_test_scope(ident: &syn::Ident, attrs: &[syn::Attribute]) -> bool {
    let cfg_test = attrs.iter().any(|a| {
        a.path().is_ident("cfg")
            && a.parse_args::<syn::Meta>()
                .map(|m| {
                    m.path().is_ident("test") || m.to_token_stream().to_string().contains("test")
                })
                .unwrap_or(false)
    });
    cfg_test || ident == "tests" || ident == "test"
}
