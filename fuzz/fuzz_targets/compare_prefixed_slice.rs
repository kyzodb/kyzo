/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

#![no_main]

//! Fuzzes `lsm_tree::table::util::compare_prefixed_slice` — the restart-point
//! comparator every data/index block binary search calls, comparing a
//! (prefix, suffix) pair (a block key reconstructed from its restart point's
//! shared prefix plus its own stored suffix, never materialized as one
//! `Vec`) against a needle without ever concatenating the two. Story #118
//! (own the LSM): this vendored crate is our fork now, so its own fuzz
//! coverage (`vendor/lsm-tree`'s upstream `fuzz/compare_prefixed_slice`,
//! originally an AFL target) is ported onto our libfuzzer harness rather
//! than left behind.
//!
//! Law: `compare_prefixed_slice(prefix, suffix, needle)` equals
//! `[prefix, suffix].concat().cmp(needle)` — the whole point of the
//! function is computing that comparison WITHOUT the allocation the naive
//! right-hand side performs, so the fuzzer's job is proving the split
//! comparison never disagrees with the concatenated one.

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use lsm_tree::table::util::compare_prefixed_slice;

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);

    let Ok(prefix) = Vec::<u8>::arbitrary(&mut u) else {
        return;
    };
    let Ok(suffix) = Vec::<u8>::arbitrary(&mut u) else {
        return;
    };
    let Ok(needle) = Vec::<u8>::arbitrary(&mut u) else {
        return;
    };

    let result = compare_prefixed_slice(&prefix, &suffix, &needle);

    let combined: Vec<u8> = prefix.iter().chain(suffix.iter()).copied().collect();
    let expected = combined.as_slice().cmp(&needle);

    assert_eq!(
        result, expected,
        "compare_prefixed_slice({prefix:?}, {suffix:?}, {needle:?}) = {result:?}, but expected {expected:?}"
    );
});
