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

/// Dense `u32` node id → slice index. Lossless on every supported target.
#[inline]
pub(crate) fn usize_from_u32(id: u32) -> usize {
    let b = id.to_le_bytes();
    usize::from(b[0])
        | (usize::from(b[1]) << 8)
        | (usize::from(b[2]) << 16)
        | (usize::from(b[3]) << 24)
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
