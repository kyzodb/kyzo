/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Deterministic xorshift64* for `cfg(test)` corpora — ONE seat.
//!
//! Four value-plane test modules used to paste the same `next` body.
//! That was a second authority by copy-paste (copy_detector). Callers
//! seed and draw; they do not re-own the mixer.

use super::convert::{u64_from_usize, usize_from_u64_fitting};

/// Seeded PRNG: reproducible, no clock.
#[derive(Debug, Clone)]
pub struct Rng(pub u64);

impl Rng {
    /// Advance and return the next u64 (xorshift64* + finalizer).
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        // INVARIANT(xorshift_finalizer): xorshift* final mul is defined wrapping on u64.
        (std::num::Wrapping(x) * std::num::Wrapping(0x2545_F491_4F6C_DD1D)).0
    }

    /// Uniform index in `0..n` (`n > 0`).
    pub fn below(&mut self, n: usize) -> usize {
        let n_u = u64_from_usize(n);
        usize_from_u64_fitting(self.next() % n_u)
    }
}
