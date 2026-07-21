/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Trybuild harness: stdlib seals (OpDecl / BoundOp / TagOrdered).

#[test]
fn opdecl_body_unconstructible() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/opdecl_body_refused.rs");
}

#[test]
fn bound_op_mint_unconstructible() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/bound_op_mint_refused.rs");
}

#[test]
fn tag_ordered_score_unconstructible() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/tag_ordered_score_refused.rs");
}
