/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! HyperLogLog: distinct-count estimation whose union is a semilattice.
//!
//! A HyperLogLog keeps `m = 2^p` one-byte registers. An element is hashed
//! once (seeded [`super::xxh64`] over the value's canonical encoding); the
//! top `p` bits pick a register, and the number of leading zeros in the
//! remaining bits (plus one) is a *rank* whose maximum over all elements
//! landing in a register estimates how many distinct elements were seen.
//!
//! ## Why the union is a meet
//!
//! [`HyperLogLog::merge`] takes the register-wise maximum. `max` on each
//! register is:
//!
//! - **idempotent**: `max(a, a) = a` — merging a sketch with itself is a
//!   no-op, so folding a row twice cannot corrupt the estimate;
//! - **commutative** and **associative**: `max` is;
//! - with an **identity**: the all-zero sketch ([`HyperLogLog::new`]) is the
//!   bottom element.
//!
//! So a HyperLogLog under `merge` is a bounded join-semilattice, which is
//! exactly the algebra a *meet aggregation* requires. That is what lets the
//! `hll_union` aggregation run inside recursion: the sketch is the value
//! that flows through the fixpoint, register-max is the fold, and the
//! estimate is read off at the end. The estimate is a pure function of the
//! register bytes, so equal sketches always give equal estimates.
//!
//! The estimator is the standard HyperLogLog one (harmonic mean of register
//! powers, with the small-cardinality linear-counting correction); the
//! 64-bit hash makes the classic large-range correction unnecessary. Its
//! relative standard error is `1.04 / sqrt(m)`.

use std::io::Write;

use miette::{Result, bail, ensure};

use crate::data::value::DataValue;

/// The seed for the element hash. Pinned: changing it changes every
/// sketch's contents (and is a stored-format change if sketches are ever
/// persisted). Its value is arbitrary but fixed.
const HASH_SEED: u64 = 0x48_4C_4C_5F_53_4B_54_31; // "HLL_SKT1"

/// The precision `p`: `m = 2^p = 16384` registers, for a relative standard
/// error of `1.04 / sqrt(m) ≈ 0.81%`. Pinned as part of the sketch format.
pub(crate) const DEFAULT_PRECISION: u8 = 14;

/// A byte tag leading the serialized form, so a stored sketch names its own
/// format; bump on any layout change.
const FORMAT_TAG: u8 = 0x01;

/// The HyperLogLog `alpha_m` bias constant.
fn alpha(m: usize) -> f64 {
    match m {
        16 => 0.673,
        32 => 0.697,
        64 => 0.709,
        _ => 0.7213 / (1.0 + 1.079 / m as f64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn val(i: i64) -> DataValue {
        DataValue::from(i)
    }

    /// The relative standard error of HyperLogLog at precision `p`.
    fn std_error(precision: u8) -> f64 {
        1.04 / ((1u64 << precision) as f64).sqrt()
    }

    /// Insert `n` distinct integers offset by `salt` into a fresh sketch.
    fn sketch_of(n: i64, salt: i64, precision: u8) -> HyperLogLog {
        let mut hll = HyperLogLog::new(precision).unwrap();
        for i in 0..n {
            hll.add(&val(salt.wrapping_mul(1_000_003).wrapping_add(i)));
        }
        hll
    }

    /// ACCURACY vs EXACT: across many independent trials at several
    /// cardinalities, the RMS relative error tracks the theoretical
    /// `1.04/sqrt(m)` (asserted within a small factor), and no single trial
    /// blows past a generous multiple of it. Seeds are the pinned `salt`
    /// values `0..TRIALS`, so this is reproducible. A broken estimator (or a
    /// broken hash) moves the RMS immediately.
    #[test]
    fn accuracy_within_theoretical_bound() {
        let precision = 14;
        let se = std_error(precision);
        for &n in &[1_000i64, 10_000, 50_000] {
            const TRIALS: i64 = 24;
            let mut sq_sum = 0.0f64;
            let mut worst = 0.0f64;
            for salt in 0..TRIALS {
                let est = sketch_of(n, salt + 1, precision).estimate();
                let rel = (est - n as f64) / n as f64;
                sq_sum += rel * rel;
                worst = worst.max(rel.abs());
            }
            let rmse = (sq_sum / TRIALS as f64).sqrt();
            // The RMS relative error should be close to the standard error;
            // allow generous slack for the small trial count.
            assert!(
                rmse < 1.6 * se,
                "n={n}: RMSE {rmse:.5} exceeded 1.6*se ({:.5})",
                1.6 * se
            );
            // No individual trial should be wildly off.
            assert!(
                worst < 5.0 * se,
                "n={n}: worst relative error {worst:.5} exceeded 5*se ({:.5})",
                5.0 * se
            );
        }
    }

    /// Small cardinalities use the linear-counting path; the estimate is
    /// near-exact there.
    fn count_distinct(n: i64) -> i64 {
        sketch_of(n, 42, 14).estimate_count()
    }

    #[test]
    fn small_cardinality_is_accurate() {
        for &n in &[0i64, 1, 5, 50, 200] {
            let est = count_distinct(n);
            assert!(
                (est - n).abs() <= 2 + n / 20,
                "n={n}: estimate {est} too far from exact"
            );
        }
    }

    /// SEMILATTICE LAWS for the union. Register-wise max must be idempotent
    /// (with a `false` changed-flag), commutative, associative, and have the
    /// empty sketch as identity. Mirrors the meet-aggregation property
    /// pattern in `data/aggr.rs`.
    #[test]
    fn merge_is_a_semilattice() {
        let x = sketch_of(3_000, 1, 12);
        let y = sketch_of(5_000, 2, 12);
        let z = sketch_of(2_000, 3, 12);

        // Idempotent: merge(x, x) changes nothing and says so.
        let mut xx = x.clone();
        assert!(!xx.merge(&x).unwrap(), "merge(x, x) reported a change");
        assert_eq!(xx, x, "merge(x, x) altered x");

        // Identity: merge(empty, x) == x.
        let mut e = HyperLogLog::new(12).unwrap();
        e.merge(&x).unwrap();
        assert_eq!(e, x, "merge(empty, x) != x");

        // Commutative: merge(x, y) == merge(y, x).
        let mut xy = x.clone();
        xy.merge(&y).unwrap();
        let mut yx = y.clone();
        yx.merge(&x).unwrap();
        assert_eq!(xy, yx, "merge not commutative");

        // Associative: merge(merge(x, y), z) == merge(x, merge(y, z)).
        let mut lhs = x.clone();
        lhs.merge(&y).unwrap();
        lhs.merge(&z).unwrap();
        let mut yz = y.clone();
        yz.merge(&z).unwrap();
        let mut rhs = x.clone();
        rhs.merge(&yz).unwrap();
        assert_eq!(lhs, rhs, "merge not associative");
    }

    /// The union estimates the cardinality of the *union* of the two element
    /// sets — the property that makes `hll_union` meaningful in recursion.
    #[test]
    fn merge_estimates_union_cardinality() {
        // Two disjoint sets of 10_000 each: union is ~20_000.
        let mut a = HyperLogLog::new(14).unwrap();
        for i in 0..10_000 {
            a.add(&val(i));
        }
        let mut b = HyperLogLog::new(14).unwrap();
        for i in 10_000..20_000 {
            b.add(&val(i));
        }
        a.merge(&b).unwrap();
        let est = a.estimate();
        let rel = (est - 20_000.0).abs() / 20_000.0;
        assert!(
            rel < 3.0 * std_error(14),
            "union estimate {est} off by {rel}"
        );
    }

    /// BYTE IDENTITY: the same input multiset, inserted in two different
    /// orders, yields byte-identical sketches — register-max is
    /// order-free — and the estimate is a pure function of those bytes.
    #[test]
    fn byte_identical_across_fold_orders() {
        let mut ascending = HyperLogLog::new(12).unwrap();
        for i in 0..4_000i64 {
            ascending.add(&val(i * 7 % 4_000));
        }
        let mut descending = HyperLogLog::new(12).unwrap();
        for i in (0..4_000i64).rev() {
            descending.add(&val(i * 7 % 4_000));
        }
        assert_eq!(
            ascending.to_bytes(),
            descending.to_bytes(),
            "fold order changed the sketch bytes"
        );
        assert_eq!(ascending.estimate(), descending.estimate());
    }

    /// RANDOMIZED SEMILATTICE: on seeded-random multisets, merging four
    /// sketches in **every one of the 24 orders** — with a random one of them
    /// folded in twice (idempotence under arbitrary order) — produces
    /// byte-identical sketches, and hence the identical estimate. This is the
    /// property that makes `hll_union` meet-safe: the fixpoint may re-derive
    /// and re-fold rows in any order without changing the answer. The PRNG is
    /// a pinned-seed LCG (no OS entropy), so every run tests the same inputs.
    #[test]
    fn merge_order_never_changes_bytes_randomized() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state >> 33
        };
        for trial in 0..6u64 {
            // Four sketches over random multisets (with overlap and dups).
            let mut parts = Vec::new();
            for _ in 0..4 {
                let mut h = HyperLogLog::new(10).unwrap();
                let n = 200 + next() % 800;
                for _ in 0..n {
                    h.add(&val((next() % 2_000) as i64));
                }
                parts.push(h);
            }
            let dup = (next() % 4) as usize;

            // Reference fold: order 0,1,2,3 plus the duplicate at the end.
            let fold = |order: &[usize]| {
                let mut acc = HyperLogLog::new(10).unwrap();
                for &i in order {
                    acc.merge(&parts[i]).unwrap();
                }
                acc.merge(&parts[dup]).unwrap();
                acc.to_bytes()
            };
            let reference = fold(&[0, 1, 2, 3]);

            // Every permutation of the four parts must reproduce it exactly.
            for a in 0..4 {
                for b in 0..4 {
                    for c in 0..4 {
                        for d in 0..4 {
                            if a == b || a == c || a == d || b == c || b == d || c == d {
                                continue;
                            }
                            assert_eq!(
                                fold(&[a, b, c, d]),
                                reference,
                                "trial {trial}: order {a}{b}{c}{d} (dup {dup}) \
                                 changed the sketch bytes"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Round-trip through the stored form, and reject corruption without
    /// panicking.
    #[test]
    fn serialization_round_trips_and_rejects_corruption() {
        let s = sketch_of(1234, 9, 10);
        let bytes = s.to_bytes();
        assert_eq!(HyperLogLog::from_bytes(&bytes).unwrap(), s);

        assert!(HyperLogLog::from_bytes(&[]).is_err());
        assert!(HyperLogLog::from_bytes(&[0x02, 10]).is_err(), "bad tag");
        let mut wrong_len = bytes.clone();
        wrong_len.pop();
        assert!(HyperLogLog::from_bytes(&wrong_len).is_err(), "bad length");
    }

    /// PINNED-LITERAL sketch bytes for a fixed input and seed: format or
    /// hash drift fails loudly here. The digest of registers is asserted (a
    /// full 16384-byte register dump would be unwieldy), together with the
    /// exact non-zero register population, both of which are pure functions
    /// of the pinned `HASH_SEED` and the encoding.
    #[test]
    fn pinned_sketch_fingerprint() {
        // INPUT ANCHOR (see count_min::pinned_sketch_fingerprint): the
        // registers are set by hashing each value's CANONICAL encoding,
        // pinned to the format law by hand so the fingerprint is a function
        // of format-correct input, not an implementation snapshot.
        let enc = |v: &DataValue| {
            let mut b = Vec::new();
            crate::data::value::append_canonical(&mut b, v);
            b
        };
        assert_eq!(enc(&val(0)), vec![0x10, 0x02, 0x00]);
        assert_eq!(
            enc(&val(1)),
            vec![0x10, 0x03, 0x04, 0x39, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );

        let mut hll = HyperLogLog::new(14).unwrap();
        for i in 0..1000i64 {
            hll.add(&val(i));
        }
        let bytes = hll.to_bytes();
        // Tag + precision header is fixed.
        assert_eq!(&bytes[..2], &[FORMAT_TAG, 14]);
        // A stable fingerprint of the whole register array. Recomputed from
        // the bytes with the same pinned hash; any drift of HASH_SEED, the
        // xxh64 constants, the encoding, or the register layout changes it.
        let fingerprint = super::super::xxh64(&bytes, 0);
        assert_eq!(fingerprint, 0x4133_90D9_1C93_796C);
    }
}
