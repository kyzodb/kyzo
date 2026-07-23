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

use core::fmt;

/// Supported targets: pointer width 32 or 64 — so `u32` → `usize` and
/// `usize` → `u64` are total. Other widths refuse at compile time (typed
/// target gate), never an `assert!` costume on a live path.
#[cfg(not(any(target_pointer_width = "32", target_pointer_width = "64")))]
compile_error!("value plane convert doors require pointer width 32 or 64");

/// A width conversion that cannot be proven lossless — typed refuse, never
/// an Err→0 costume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WidthRefuse {
    pub from_bits: u128,
    pub into: &'static str,
}

impl fmt::Display for WidthRefuse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "value {} does not fit in {}",
            self.from_bits, self.into
        )
    }
}

impl std::error::Error for WidthRefuse {}

/// Dense `u32` quantity → slice index / length compare. Lossless on every
/// supported target.
#[inline]
pub fn usize_from_u32(n: u32) -> usize {
    let b = n.to_le_bytes();
    usize::from(b[0])
        | (usize::from(b[1]) << 8)
        | (usize::from(b[2]) << 16)
        | (usize::from(b[3]) << 24)
}

/// `u32::MAX` as `usize` without an `as` cast.
#[inline]
pub fn u32_max_usize() -> usize {
    usize_from_u32(u32::MAX)
}

/// Lossless `usize` → `u64` via little-endian `From<u8>` assemble.
#[inline]
pub fn u64_from_usize(n: usize) -> u64 {
    let src = n.to_le_bytes();
    let mut buf = [0u8; 8];
    buf[..src.len()].copy_from_slice(&src);
    u64::from_le_bytes(buf)
}

/// Fallible `usize` → `i64` (counts into signed columns / RNG ranges).
#[inline]
pub fn i64_from_usize(n: usize) -> Result<i64, WidthRefuse> {
    i64::try_from(n).map_err(|_| WidthRefuse {
        from_bits: u64_from_usize(n).into(),
        into: "i64",
    })
}

/// Fallible `u64` → `i64` when the value is a non-negative magnitude that
/// already fits (remainder under an `i64` bound, small counters).
#[inline]
pub fn i64_from_u64_fitting(n: u64) -> Result<i64, WidthRefuse> {
    i64::try_from(n).map_err(|_| WidthRefuse {
        from_bits: u128::from(n),
        into: "i64",
    })
}

/// Two's-complement re-interpret `u64` → `i64` (seed / LCG bit lanes).
#[inline]
pub fn i64_bits_from_u64(n: u64) -> i64 {
    i64::from_le_bytes(n.to_le_bytes())
}

/// Low byte of a `u64` — total (little-endian byte 0). Replaces
/// `u8::try_from(n & 0xFF)` Err→0 costumes.
#[inline]
pub fn u8_from_u64_low(n: u64) -> u8 {
    n.to_le_bytes()[0]
}

/// Low 32 bits of a `u64` — total (little-endian assemble). Replaces
/// `u32::try_from(n & 0xFFFF_FFFF)` Err→0 costumes.
#[inline]
pub fn u32_from_u64_low(n: u64) -> u32 {
    let b = n.to_le_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Low 32 bits of a `u128` limb (softfloat / approx widen).
#[inline]
pub fn u32_from_u128_low(n: u128) -> u32 {
    let b = n.to_le_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// `u64` → `usize` when the value already fits (`n ≤ usize::MAX`).
/// Lawful for remainders under a `usize` bound: `x % u64_from_usize(bound)`.
#[inline]
pub fn usize_from_u64_fitting(n: u64) -> usize {
    let src = n.to_le_bytes();
    let mut buf = [0u8; std::mem::size_of::<usize>()];
    buf.copy_from_slice(&src[..std::mem::size_of::<usize>()]);
    usize::from_le_bytes(buf)
}

/// Fallible `u64` → `usize` (index / capacity doors).
#[inline]
pub fn usize_from_u64(n: u64) -> Result<usize, WidthRefuse> {
    usize::try_from(n).map_err(|_| WidthRefuse {
        from_bits: u128::from(n),
        into: "usize",
    })
}

/// Fallible `u128` → `u64` (Duration nanos, rank products).
#[inline]
pub fn u64_from_u128(n: u128) -> Result<u64, WidthRefuse> {
    u64::try_from(n).map_err(|_| WidthRefuse {
        from_bits: n,
        into: "u64",
    })
}

/// Fallible `u128` → `usize` (percentile rank → index).
#[inline]
pub fn usize_from_u128(n: u128) -> Result<usize, WidthRefuse> {
    usize::try_from(n).map_err(|_| WidthRefuse {
        from_bits: n,
        into: "usize",
    })
}

/// Approximate `i128` as `f64` without a numeric `as` cast (limb widen).
/// Same IEEE sitofp approximation the engine sum fold publishes on overflow.
#[inline]
pub fn i128_approx_f64(n: i128) -> f64 {
    let neg = n < 0;
    let mut x = n.unsigned_abs();
    let mut result = 0.0_f64;
    let mut scale = 1.0_f64;
    while x > 0 {
        let limb = u32_from_u128_low(x);
        result += f64::from(limb) * scale;
        x >>= 32;
        scale *= 4_294_967_296.0; // 2^32
    }
    if neg { -result } else { result }
}
