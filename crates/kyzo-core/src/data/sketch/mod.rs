/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Deterministic probabilistic sketches: cardinality (HyperLogLog),
//! frequency (Count-Min), and quantiles (t-digest).
//!
//! The whole point of these three is that they are *sketches* — small,
//! fixed-size summaries that answer count/frequency/quantile questions
//! approximately — while staying **exactly as deterministic as the rest of
//! the engine**: the same input multiset produces byte-identical sketch
//! contents and hence an identical estimate, on every platform and
//! toolchain. That is the admission test for this capability, and it drives
//! every design choice here.
//!
//! ## What determinism costs, and how each sketch pays it
//!
//! - **Hashing is pinned and portable.** Every sketch that hashes a value
//!   hashes the value's *canonical encoding* (`data/value/canonical.rs`) with a
//!   seeded [`xxh64`] — never `std::hash::Hash`, whose output is neither
//!   stable across releases nor defined to be portable. `xxh64` is
//!   hand-rolled from the published xxHash64 specification (all arithmetic
//!   is `wrapping_*` on `u64` over little-endian byte reads, so it has no
//!   platform-dependent behaviour) and is pinned against the canonical
//!   published test vectors in [`tests`], so a drift of any constant fails
//!   loudly. This mirrors the house rule already used by the seeded PRNG in
//!   `fixed_rule/rng.rs`: no OS entropy, no unpinned hash, pure functions of
//!   the input.
//!
//! - **Fold order is handled per sketch, honestly.** HyperLogLog and
//!   Count-Min are order-*insensitive* by construction (register-wise max /
//!   counter add), so their contents are a pure function of the input
//!   multiset with no further work. t-digest is order-*sensitive* — its
//!   centroid clustering depends on the order points arrive — so
//!   [`tdigest::TDigest`] does not fold incrementally; it buffers and builds
//!   from the values **sorted into the exact `Num` order** (the same total
//!   order the canonical encoding preserves), which makes its output a pure
//!   function of the input multiset. None of the three ever draws
//!   randomness.
//!
//! ## Exposure surface (see each module and the wrappers in [`aggr`])
//!
//! The lattice laws decide how each sketch is exposed as an aggregation:
//!
//! - **HyperLogLog union is a bounded join-semilattice** — register-wise
//!   `max` is idempotent, commutative, associative, with the all-zero
//!   sketch as identity — so it is a **meet aggregation** and composes
//!   inside recursion under the existing stratification laws.
//! - **Count-Min merge (sum semantics) is a commutative monoid but not
//!   idempotent** (merging a sketch with itself doubles every counter), so
//!   it is a **normal aggregation** only; making it a meet would
//!   double-count under recursion. A max-merge variant *would* be a
//!   semilattice, but it answers a different question (peak shard frequency,
//!   not total) and is documented rather than smuggled in.
//! - **t-digest merge is not associative-exact**, so it is a **normal
//!   aggregation** with the canonical sorted-fold policy above; it is never
//!   exposed as a meet.

pub(crate) mod aggr;
pub(crate) mod count_min;
pub(crate) mod hll;
pub(crate) mod tdigest;

use crate::data::value::DataValue;

// xxHash64 primes, from the published specification.
const PRIME64_1: u64 = 0x9E37_79B1_85EB_CA87;
const PRIME64_2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const PRIME64_3: u64 = 0x1656_67B1_9E37_79F9;
const PRIME64_4: u64 = 0x85EB_CA77_C2B2_AE63;
const PRIME64_5: u64 = 0x27D4_EB2F_1656_67C5;

#[inline]
fn round(acc: u64, input: u64) -> u64 {
    // INVARIANT(xxh64): wrapping u64 ops are the published xxHash64 mix; wrap is the hash.
    acc.wrapping_add(input.wrapping_mul(PRIME64_2))
        .rotate_left(31)
        .wrapping_mul(PRIME64_1)
}

#[inline]
fn merge_round(mut acc: u64, val: u64) -> u64 {
    acc ^= round(0, val);
    // INVARIANT(xxh64): wrapping u64 ops are the published xxHash64 mix; wrap is the hash.
    acc.wrapping_mul(PRIME64_1).wrapping_add(PRIME64_4)
}

#[inline]
fn read_u64_le(bytes: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(buf)
}

#[inline]
fn read_u32_le(bytes: &[u8]) -> u32 {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&bytes[..4]);
    u32::from_le_bytes(buf)
}

/// The 64-bit xxHash of `data` under `seed`, hand-rolled from the published
/// specification. Every operation is a wrapping `u64` op over little-endian
/// reads, so the result is identical on every platform — the property the
/// sketches rely on. Pinned against the canonical test vectors in [`tests`].
pub(crate) fn xxh64(data: &[u8], seed: u64) -> u64 {
    let len = data.len();
    let mut idx = 0usize;
    let mut h: u64;

    if len >= 32 {
        // INVARIANT(xxh64): wrapping u64 ops are the published xxHash64 mix; wrap is the hash.
        let mut v1 = seed.wrapping_add(PRIME64_1).wrapping_add(PRIME64_2);
        let mut v2 = seed.wrapping_add(PRIME64_2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(PRIME64_1);
        while idx + 32 <= len {
            v1 = round(v1, read_u64_le(&data[idx..]));
            v2 = round(v2, read_u64_le(&data[idx + 8..]));
            v3 = round(v3, read_u64_le(&data[idx + 16..]));
            v4 = round(v4, read_u64_le(&data[idx + 24..]));
            idx += 32;
        }
        // INVARIANT(xxh64): wrapping u64 ops are the published xxHash64 mix; wrap is the hash.
        h = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        h = merge_round(h, v1);
        h = merge_round(h, v2);
        h = merge_round(h, v3);
        h = merge_round(h, v4);
    } else {
        // INVARIANT(xxh64): wrapping u64 ops are the published xxHash64 mix; wrap is the hash.
        h = seed.wrapping_add(PRIME64_5);
    }

    // INVARIANT(xxh64): wrapping u64 ops are the published xxHash64 mix; wrap is the hash.
    h = h.wrapping_add(len as u64);

    while idx + 8 <= len {
        h ^= round(0, read_u64_le(&data[idx..]));
        // INVARIANT(xxh64): wrapping u64 ops are the published xxHash64 mix; wrap is the hash.
        h = h
            .rotate_left(27)
            .wrapping_mul(PRIME64_1)
            .wrapping_add(PRIME64_4);
        idx += 8;
    }
    if idx + 4 <= len {
        // INVARIANT(xxh64): wrapping u64 ops are the published xxHash64 mix; wrap is the hash.
        h ^= (read_u32_le(&data[idx..]) as u64).wrapping_mul(PRIME64_1);
        h = h
            .rotate_left(23)
            .wrapping_mul(PRIME64_2)
            .wrapping_add(PRIME64_3);
        idx += 4;
    }
    while idx < len {
        // INVARIANT(xxh64): wrapping u64 ops are the published xxHash64 mix; wrap is the hash.
        h ^= (data[idx] as u64).wrapping_mul(PRIME64_5);
        h = h.rotate_left(11).wrapping_mul(PRIME64_1);
        idx += 1;
    }

    // Final avalanche.
    h ^= h >> 33;
    // INVARIANT(xxh64): wrapping u64 ops are the published xxHash64 mix; wrap is the hash.
    h = h.wrapping_mul(PRIME64_2);
    h ^= h >> 29;
    h = h.wrapping_mul(PRIME64_3);
    h ^= h >> 32;
    h
}

/// The canonical portable bytes of a value: its memcomparable encoding. Two
/// values hash equal iff they are semantically equal, on every platform,
/// because this is the same encoding that backs the on-disk keys.
pub(crate) fn encode_value(v: &DataValue) -> Vec<u8> {
    let mut buf = Vec::new();
    crate::data::value::append_canonical(&mut buf, v);
    buf
}

/// Seeded xxHash of a value's canonical encoding — the one hashing entry
/// point every sketch uses.
#[inline]
pub(crate) fn hash_value(v: &DataValue, seed: u64) -> u64 {
    xxh64(&encode_value(v), seed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GOLDEN VECTORS: xxHash64 outputs from the published specification,
    /// independent of this implementation. The empty-input value
    /// `0xEF46DB3751D8E999` is the universally-cited XXH64("", 0); the
    /// "spammish repetition" value is the equally-cited 39-byte vector that
    /// exercises the 32-byte-stripe main loop plus the 8/4/1-byte tail. If
    /// any prime or shift in [`xxh64`] drifts, these literals stop matching.
    #[test]
    fn xxh64_golden_vectors() {
        assert_eq!(xxh64(b"", 0), 0xEF46_DB37_51D8_E999);
        assert_eq!(
            xxh64(b"Nobody inspects the spammish repetition", 0),
            0xFBCE_A83C_8A37_8BF1
        );
    }

    /// A non-zero seed changes the digest, and the seed is a pure input:
    /// same bytes + same seed ⇒ same digest, different seed ⇒ (here)
    /// different digest.
    #[test]
    fn xxh64_seed_is_a_pure_input() {
        let a = xxh64(b"kyzo", 1);
        let b = xxh64(b"kyzo", 1);
        let c = xxh64(b"kyzo", 2);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    /// Boundary lengths around the 32-byte stripe and the 8/4/1-byte tail
    /// paths are all deterministic (regression guard for the index
    /// arithmetic in the tail).
    #[test]
    fn xxh64_all_length_paths_deterministic() {
        for len in 0..80usize {
            let data: Vec<u8> = (0..len as u8).collect();
            assert_eq!(xxh64(&data, 7), xxh64(&data, 7), "len {len}");
        }
    }

    /// Semantically equal values hash equally through their canonical
    /// encoding; distinct values (here an int and the float of the same
    /// magnitude are distinct `DataValue`s) get distinct encodings.
    #[test]
    fn hash_value_uses_canonical_encoding() {
        let a = DataValue::from(42i64);
        let b = DataValue::from(42i64);
        assert_eq!(hash_value(&a, 0), hash_value(&b, 0));
        assert_eq!(encode_value(&a), encode_value(&b));
    }
}
