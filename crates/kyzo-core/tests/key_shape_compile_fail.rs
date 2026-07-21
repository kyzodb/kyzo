/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Compile-fail proofs for #303 T2 key-shape split.
#[test]
fn storage_key_rejects_tuple_key() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/storage_key_rejects_tuple_key.rs");
}
