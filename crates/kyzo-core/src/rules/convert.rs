/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Checked numeric doors for the fixed-rule zone — no `as` casts.
//!
//! Rust 1.96 removed `From<u32> for usize` (platform-width coupling). Dense
//! node ids still widen losslessly on every Kyzo target (pointer width ≥ 32);
//! we do that with an explicit little-endian assemble from `u8`s — `From<u8>
//! for usize` remains total.

use miette::{Diagnostic, Result};
use thiserror::Error;

use crate::rules::graph_view::GraphTooLargeError;

/// Dense `u32` node id → slice index — model seat (copy_detector).
#[inline]
pub(crate) fn usize_from_u32(id: u32) -> usize {
    kyzo_model::value::convert::usize_from_u32(id)
}

/// Alias kept for call-site readability at graph indexes.
#[inline]
pub(crate) fn node_idx(id: u32) -> usize {
    usize_from_u32(id)
}

/// `u32::MAX` as `usize` without an `as` cast.
#[inline]
pub(crate) fn u32_max_usize() -> usize {
    usize_from_u32(u32::MAX)
}

#[derive(Debug, Error, Diagnostic)]
#[error("count {count} does not fit in i64")]
#[diagnostic(code(algo::count_overflow_i64))]
pub(crate) struct CountOverflowI64 {
    pub count: usize,
}

#[derive(Debug, Error, Diagnostic)]
#[error("count {count} does not fit in u64")]
#[diagnostic(code(algo::count_overflow_u64))]
pub(crate) struct CountOverflowU64 {
    pub count: usize,
}

#[derive(Debug, Error, Diagnostic)]
#[error("signed value {value} does not fit in usize")]
#[diagnostic(code(algo::isize_overflow_usize))]
pub(crate) struct SignedFitsUsizeError {
    pub value: i64,
}

/// Fallible `usize` → `i64` (output columns, option defaults).
#[inline]
pub(crate) fn i64_from_usize(count: usize) -> Result<i64> {
    i64::try_from(count).map_err(|_| CountOverflowI64 { count }.into())
}

/// Fallible `usize` → `u64` (budget counters).
#[inline]
pub(crate) fn u64_from_usize(count: usize) -> Result<u64> {
    u64::try_from(count).map_err(|_| CountOverflowU64 { count }.into())
}

/// Fallible `usize` → `u32` (graph-size bound).
#[inline]
pub(crate) fn u32_from_usize(count: usize) -> Result<u32> {
    u32::try_from(count).map_err(|_| GraphTooLargeError.into())
}

/// Fallible `i64` → `usize` (option defaults already bound-checked elsewhere).
#[inline]
pub(crate) fn usize_from_i64(value: i64) -> Result<usize> {
    usize::try_from(value).map_err(|_| SignedFitsUsizeError { value }.into())
}

#[inline]
pub(crate) fn i64_from_u32(n: u32) -> i64 {
    i64::from(n)
}

#[inline]
pub(crate) fn f64_from_u32(n: u32) -> f64 {
    f64::from(n)
}

/// Bit-cast `u64` → `i64` (seed option defaults; two's-complement reinterprets).
#[inline]
pub(crate) fn i64_bits_from_u64(bits: u64) -> i64 {
    i64::from_ne_bytes(bits.to_ne_bytes())
}

/// Low 32 bits of a `u64` word (after a right-shift in PRNG / test streams).
#[inline]
pub(crate) fn u32_low(word: u64) -> u32 {
    let b = word.to_le_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// High 32 bits of a `u64` word (`RngCore::next_u32` splitmix path).
#[inline]
pub(crate) fn u32_hi(word: u64) -> u32 {
    let b = word.to_le_bytes();
    u32::from_le_bytes([b[4], b[5], b[6], b[7]])
}

/// Low byte of a `u64` — total (little-endian byte 0). Replaces
/// `u8::try_from(n & 0xFF)` Err→0 costumes.
#[inline]
pub(crate) fn u8_from_u64_low(n: u64) -> u8 {
    n.to_le_bytes()[0]
}

/// Low 32 bits of a `u128` limb (softfloat / approx widen).
#[inline]
pub(crate) fn u32_from_u128_low(n: u128) -> u32 {
    let b = n.to_le_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Non-negative `i64` → `usize` via LE when the value already fits
/// (e.g. `to_int_coerced` of a count).
#[inline]
pub(crate) fn usize_from_i64_nonneg_fitting(n: i64) -> usize {
    usize_from_u64_fitting(u64::from_le_bytes(n.to_le_bytes()))
}


/// `u64` → `usize` when the value already fits — model seat (copy_detector).
/// Lawful for remainders under a `usize` bound: `x % u64_from_usize_total(bound)`.
#[inline]
pub(crate) fn usize_from_u64_fitting(n: u64) -> usize {
    kyzo_model::value::convert::usize_from_u64_fitting(n)
}

/// Lossless `usize` → `u64` via little-endian `From<u8>` assemble (total on
/// pointer width ≤ 64).
#[inline]
pub(crate) fn u64_from_usize_total(n: usize) -> u64 {
    let src = n.to_le_bytes();
    let mut buf = [0u8; 8];
    buf[..src.len()].copy_from_slice(&src);
    u64::from_le_bytes(buf)
}

/// IEEE 11-bit field in a `u64` word → `i32` (value ∈ 0..=0x7FF).
#[inline]
pub(crate) fn i32_from_u11(n: u64) -> i32 {
    let b = n.to_le_bytes();
    i32::from(u16::from_le_bytes([b[0], b[1]]))
}

/// Low 63 bits of a non-negative `u64` that already fits in `i64` (e.g. a
/// modulus remainder `< 2^63`). Total LE assemble — never TryFrom Err→0.
#[inline]
pub(crate) fn i64_from_u64_nonneg_fitting(n: u64) -> i64 {
    i64::from_le_bytes(n.to_le_bytes())
}

/// Floor subtraction — published saturating release (counters/debt cannot
/// go negative). Named door, not an anonymous None→0 costume.
#[inline]
pub(crate) fn saturating_sub_u64(a: u64, b: u64) -> u64 {
    match a.checked_sub(b) {
        Some(n) => n,
        None => {
            // Floor of the published saturating contract.
            0
        }
    }
}

/// Floor subtraction for `usize` indices/counts.
#[inline]
pub(crate) fn saturating_sub_usize(a: usize, b: usize) -> usize {
    match a.checked_sub(b) {
        Some(n) => n,
        None => {
            0
        }
    }
}

/// Ceiling addition for `u64` counters — published saturating climb.
#[inline]
pub(crate) fn saturating_add_u64(a: u64, b: u64) -> u64 {
    match a.checked_add(b) {
        Some(n) => n,
        None => {
            u64::MAX
        }
    }
}

/// Ceiling addition for `u32` counters.
#[inline]
pub(crate) fn saturating_add_u32(a: u32, b: u32) -> u32 {
    match a.checked_add(b) {
        Some(n) => n,
        None => {
            u32::MAX
        }
    }
}

/// Ceiling multiplication / add for `usize` work budgets.
#[inline]
pub(crate) fn saturating_mul_usize(a: usize, b: usize) -> usize {
    match a.checked_mul(b) {
        Some(n) => n,
        None => {
            usize::MAX
        }
    }
}

/// Ceiling addition for `usize` work budgets.
#[inline]
pub(crate) fn saturating_add_usize(a: usize, b: usize) -> usize {
    match a.checked_add(b) {
        Some(n) => n,
        None => {
            usize::MAX
        }
    }
}

/// Supported targets: pointer width 32 or 64 — so `u32` → `usize` and
/// `usize` → `u64` are total.
#[cfg(not(any(target_pointer_width = "32", target_pointer_width = "64")))]
compile_error!("rules convert doors require pointer width 32 or 64");
