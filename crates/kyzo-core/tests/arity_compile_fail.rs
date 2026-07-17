/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Trybuild harness: zero arity is a type error (story #306 T3).

#[test]
fn arity_zero_unrepresentable() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/arity_zero_refused.rs");
}
