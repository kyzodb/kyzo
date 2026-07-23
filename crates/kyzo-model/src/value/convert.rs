/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Checked numeric widen doors for the value plane — no `as` casts, no
//! `TryFrom::expect` on total widens.
//!
//! Rust 1.96 removed `From<u32> for usize` (platform-width coupling). On every
//! Kyzo target (pointer width ≥ 32) a `u32` still widens losslessly into
//! `usize`; we do that with an explicit little-endian assemble from `u8`s —
//! `From<u8> for usize` remains total. Same shape for `usize` → `u64` on
//! pointer width ≤ 64.

/// Supported targets: pointer width ≥ 32, so `u32` → `usize` is total.
const _: () = assert!(
    std::mem::size_of::<usize>() >= std::mem::size_of::<u32>(),
    "value plane requires usize wide enough for u32"
);

/// Supported targets: pointer width ≤ 64, so `usize` → `u64` is total.
const _: () = assert!(
    std::mem::size_of::<usize>() <= std::mem::size_of::<u64>(),
    "value plane requires usize to fit in u64"
);

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
