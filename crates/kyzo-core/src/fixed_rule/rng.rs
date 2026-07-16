/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A seed-reproducible PRNG for the fixed rules that draw randomness.
//!
//! Upstream's `LabelPropagation` and `RandomWalk` seeded `rand` from the OS
//! entropy pool (`rand::rng()`), so the *same* facts and query could answer
//! differently run to run — a direct violation of the determinism guarantee
//! the rest of the engine holds (the README's "same inputs, same output").
//! Those rules now take an explicit `seed` option with a fixed default and
//! draw from [`SeededRng`]: same seed ⇒ byte-identical output.
//!
//! [`SeededRng`] is the storage/sim.rs `SimRng` house pattern — inline
//! splitmix64, a pure function of its seed, no new dependency — wrapped as a
//! [`rand::RngCore`] so the algorithms keep using `rand`'s Fisher–Yates
//! `shuffle`, `choose`, and `WeightedIndex` sampling unchanged; only the
//! entropy source moves from the OS to a pinned seed. The splitmix64 output
//! stream is portable (no platform-dependent word size or endianness in the
//! math), so the seed pins the stream on every target.

use rand::RngCore;

/// Inline splitmix64 wrapped as a [`rand::RngCore`]. See the module docs.
pub(crate) struct SeededRng {
    state: u64,
}

impl SeededRng {
    /// The seed used when a rule carries no explicit `seed` option. Its
    /// exact value is arbitrary but **pinned**: changing it changes every
    /// default-seed output. It is genuinely test-guarded — `golden_stream`
    /// below asserts the literal first words of this seed's stream, and
    /// `default_seed_output_is_golden` in `random_walk.rs` pins a
    /// default-seed algorithm row, so any drift of this constant (or of the
    /// splitmix64 constants in [`Self::step`]) fails loudly.
    pub(crate) const DEFAULT_SEED: u64 = 0x1234_5678_9abc_def0;

    /// Build a generator over `seed`'s splitmix64 stream. The rules obtain
    /// their seed from an `i64` option and pass it here as `u64`, so a
    /// **negative** seed is accepted and wraps two's-complement into the
    /// `u64` space (e.g. `-1` ⇒ `u64::MAX`); every distinct `i64` still maps
    /// to a distinct, reproducible stream, which is all the determinism law
    /// needs.
    pub(crate) fn new(seed: u64) -> Self {
        SeededRng { state: seed }
    }

    /// One splitmix64 step: mirrors `storage::sim::SimRng::next_u64`.
    #[inline]
    fn step(&mut self) -> u64 {
        // INVARIANT(splitmix64): modular mix per the splitmix64 contract; wrap is the PRNG.
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

impl RngCore for SeededRng {
    fn next_u32(&mut self) -> u32 {
        // High bits of a splitmix64 word; the finalizer already diffuses the
        // whole word, so either half is equidistributed.
        (self.step() >> 32) as u32
    }

    fn next_u64(&mut self) -> u64 {
        self.step()
    }

    fn fill_bytes(&mut self, dst: &mut [u8]) {
        let mut chunks = dst.chunks_exact_mut(8);
        for chunk in &mut chunks {
            chunk.copy_from_slice(&self.step().to_le_bytes());
        }
        let rem = chunks.into_remainder();
        if !rem.is_empty() {
            let bytes = self.step().to_le_bytes();
            rem.copy_from_slice(&bytes[..rem.len()]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::seq::{IndexedRandom, SliceRandom};

    /// GOLDEN VECTOR: the exact splitmix64 words the *default* seed emits,
    /// as literals. This is what makes `DEFAULT_SEED` and the three
    /// splitmix64 constants in `step` actually test-guarded: flip any one of
    /// them and these fixed values no longer match. (A `seed_determines_stream`
    /// / round-trip test cannot catch such a drift — both sides move
    /// together.) The values were computed offline from the splitmix64
    /// definition, independent of this implementation.
    #[test]
    fn golden_stream() {
        let mut rng = SeededRng::new(SeededRng::DEFAULT_SEED);
        let got: Vec<u64> = (0..4).map(|_| rng.next_u64()).collect();
        assert_eq!(
            got,
            vec![
                0x1619_22c6_45ce_50e8,
                0xad76_0caf_a169_7b60,
                0x3501_ff44_902c_a50d,
                0x417c_b9a8_26d8_31df,
            ]
        );
        // `next_u32` takes the high 32 bits of a fresh step, so the first
        // `u32` is the top half of the first golden word.
        let mut rng32 = SeededRng::new(SeededRng::DEFAULT_SEED);
        assert_eq!(rng32.next_u32(), 0x1619_22c6);
    }

    /// A negative `i64` seed is accepted and wraps into `u64` (the rules pass
    /// their seed option through as `u64`); `-1` is `u64::MAX`, and it yields
    /// its own reproducible stream distinct from the default.
    #[test]
    fn negative_seed_wraps_to_u64() {
        let from_neg1: Vec<u64> = {
            let mut r = SeededRng::new((-1i64) as u64);
            (0..4).map(|_| r.next_u64()).collect()
        };
        let from_max: Vec<u64> = {
            let mut r = SeededRng::new(u64::MAX);
            (0..4).map(|_| r.next_u64()).collect()
        };
        assert_eq!(from_neg1, from_max);
    }

    /// The stream is a pure function of the seed: two generators built from
    /// the same seed emit byte-identical words; a different seed diverges.
    #[test]
    fn seed_determines_stream() {
        let mut a = SeededRng::new(42);
        let mut b = SeededRng::new(42);
        let mut c = SeededRng::new(43);
        let sa: Vec<u64> = (0..64).map(|_| a.next_u64()).collect();
        let sb: Vec<u64> = (0..64).map(|_| b.next_u64()).collect();
        let sc: Vec<u64> = (0..64).map(|_| c.next_u64()).collect();
        assert_eq!(sa, sb);
        assert_ne!(sa, sc);
    }

    /// The point of implementing `RngCore`: `rand`'s consumers (shuffle,
    /// choose) are reproducible when driven by a seeded generator.
    #[test]
    fn rand_consumers_are_reproducible() {
        let shuffled = |seed: u64| {
            let mut rng = SeededRng::new(seed);
            let mut v: Vec<u32> = (0..50).collect();
            v.shuffle(&mut rng);
            let chosen = *(0..50).collect::<Vec<u32>>().choose(&mut rng).unwrap();
            (v, chosen)
        };
        assert_eq!(shuffled(7), shuffled(7));
    }

    /// `fill_bytes` on a non-multiple-of-8 length is deterministic and fills
    /// every byte (the remainder path).
    #[test]
    fn fill_bytes_deterministic_including_remainder() {
        let mut a = SeededRng::new(99);
        let mut b = SeededRng::new(99);
        let mut ba = [0u8; 13];
        let mut bb = [0u8; 13];
        a.fill_bytes(&mut ba);
        b.fill_bytes(&mut bb);
        assert_eq!(ba, bb);
    }
}
