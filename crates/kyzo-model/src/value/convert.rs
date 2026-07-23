/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Checked numeric widen/narrow doors for the value plane — no `as` casts, no
//! `TryFrom` Err→costume on total conversions.
//!
//! Rust 1.96 removed `From<u32> for usize` (platform-width coupling). On every
//! Kyzo target (pointer width ≥ 32) a `u32` still widens losslessly into
//! `usize`; we do that with an explicit little-endian assemble from `u8`s —
//! `From<u8> for usize` remains total. Same shape for `usize` → `u64` on
//! pointer width ≤ 64. Low-byte / fitting narrows take the proven LE slice
//! rather than laundering a `TryFrom` Err into `0`.

/// Supported targets: pointer width 32 or 64 — so `u32` → `usize` and
/// `usize` → `u64` are total. Other widths refuse at compile time (typed
/// target gate), never an `assert!` costume on a live path.
#[cfg(not(any(target_pointer_width = "32", target_pointer_width = "64")))]
compile_error!("value plane convert doors require pointer width 32 or 64");

/// Dense `u32` quantity → slice index / length compare. Lossless on every
/// supported target.
#[inline]
pub(crate) fn usize_from_u32(n: u32) -> usize {
    let b = n.to_le_bytes();
    usize::from(b[0])
        | (usize::from(b[1]) << 8)
        | (usize::from(b[2]) << 16)
        | (usize::from(b[3]) << 24)
}

/// `u32::MAX` as `usize` without an `as` cast.
#[inline]
pub(crate) fn u32_max_usize() -> usize {
    usize_from_u32(u32::MAX)
}

/// Lossless `usize` → `u64` via little-endian `From<u8>` assemble.
#[inline]
pub(crate) fn u64_from_usize(n: usize) -> u64 {
    let src = n.to_le_bytes();
    let mut buf = [0u8; 8];
    buf[..src.len()].copy_from_slice(&src);
    u64::from_le_bytes(buf)
}

/// Low byte of a `u64` — total (little-endian byte 0). Replaces
/// `u8::try_from(n & 0xFF)` Err→0 costumes.
#[inline]
pub(crate) fn u8_from_u64_low(n: u64) -> u8 {
    n.to_le_bytes()[0]
}

/// Low 32 bits of a `u64` — total (little-endian assemble). Replaces
/// `u32::try_from(n & 0xFFFF_FFFF)` Err→0 costumes.
#[inline]
pub(crate) fn u32_from_u64_low(n: u64) -> u32 {
    let b = n.to_le_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// `u64` → `usize` when the value already fits (`n ≤ usize::MAX`).
/// Lawful for remainders under a `usize` bound: `x % u64_from_usize(bound)`.
#[inline]
pub(crate) fn usize_from_u64_fitting(n: u64) -> usize {
    let src = n.to_le_bytes();
    let mut buf = [0u8; std::mem::size_of::<usize>()];
    buf.copy_from_slice(&src[..std::mem::size_of::<usize>()]);
    usize::from_le_bytes(buf)
}
