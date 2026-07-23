/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! One identity-keyed hash door for every crashfs instrument.
//!
//! Compiled as a private submodule of both [`crate::fault`] and the
//! path-included `store::sim` module (this file lives in crashfs; core
//! pulls `sim.rs` via `#[path]`). A single body — never a second FNV-1a.

/// Wrap-mul for seed/hash mixes (FNV-1a, splitmix). The algorithm defines wrap.
#[inline]
pub(crate) fn wrap_mul(a: u64, b: u64) -> u64 {
    // INVARIANT(SeedMix): wrap is the published mix contract, not silent overflow loss.
    a.wrapping_mul(b)
}

/// Wrap-add for seed/hash mixes (splitmix step). The algorithm defines wrap.
#[inline]
pub(crate) fn wrap_add(a: u64, b: u64) -> u64 {
    // INVARIANT(SeedMix): wrap is the published mix contract, not silent overflow loss.
    a.wrapping_add(b)
}

/// Lossless `usize` → `u64` via little-endian assemble (every supported
/// host has `usize` ≤ 64 bits; pad high bytes with zero).
#[inline]
pub(crate) fn usize_as_u64(n: usize) -> u64 {
    let src = n.to_le_bytes();
    let mut buf = [0u8; 8];
    buf[..src.len()].copy_from_slice(&src);
    u64::from_le_bytes(buf)
}

/// FNV-1a 64 over the op-kind tag and the operation's semantic content,
/// length-delimited so distinct part lists never collide by concatenation.
/// Identity captures WHAT an operation is — never when it runs, what ran
/// before it, or which thread carries it.
pub(crate) fn op_identity(tag: u64, parts: &[&[u8]]) -> u64 {
    const OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01B3;
    let mut h = OFFSET;
    let mut eat = |bytes: &[u8]| {
        for &b in bytes {
            h = wrap_mul(h ^ u64::from(b), PRIME);
        }
    };
    eat(&tag.to_be_bytes());
    for part in parts {
        eat(&usize_as_u64(part.len()).to_be_bytes());
        eat(part);
    }
    h
}
