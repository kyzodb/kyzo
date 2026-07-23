/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Count-Min: frequency estimation whose merge is a monoid, not a lattice.
//!
//! A Count-Min sketch is a `depth × width` table of counters. An element is
//! hashed once per row (seeded [`super::xxh64`] over the value's canonical
//! encoding, one pinned seed per row); each hash picks a column, and adding
//! the element increments that row's counter. The estimated frequency of a
//! value is the *minimum* of its `depth` counters — every counter is an
//! overestimate (collisions only add), so the smallest is the tightest.
//!
//! With `width = ⌈e/ε⌉` and `depth = ⌈ln(1/δ)⌉`, the estimate never
//! underestimates and, with probability at least `1 − δ`, overestimates the
//! true count by at most `ε · N`, where `N` is the total count inserted.
//!
//! ## Why the merge is a monoid but NOT a meet
//!
//! [`CountMinSketch::merge`] adds the two tables element-wise. Addition is
//! commutative and associative, with the all-zero table as identity — a
//! commutative monoid — so merging shards is order-independent and exact.
//! But addition is **not idempotent**: `merge(a, a)` doubles every counter.
//! A meet aggregation folds a row's contribution into the accumulator and
//! must be safe to fold *again* (recursion re-derives rows), so a non-
//! idempotent fold would silently double-count at every fixpoint step.
//! Count-Min is therefore a **normal aggregation only**, never a meet — the
//! same reason `bit_xor` is normal-only in `data/aggr.rs`. The
//! non-idempotence is pinned by a test, so no future change can quietly
//! promote it to a meet.
//!
//! (A *max*-merge variant — take the element-wise maximum — would be a
//! genuine semilattice, but it estimates the peak per-shard frequency, not
//! the total, which is a different question. It is documented here and
//! deliberately not implemented, rather than blurring the two.)
//!
//! Counters are `u64` and the merge is integer addition, so the sketch is
//! exact-to-the-bit deterministic: same input multiset ⇒ same table.

use miette::{Result, bail, ensure};

use kyzo_model::value::DataValue;

/// One pinned hash seed per row. The sketch uses the first `depth` of these,
/// so `depth` is capped at their count. Fixed as part of the sketch format.
const ROW_SEEDS: [u64; 8] = [
    0x0000_0000_0000_0001,
    0x9E37_79B9_7F4A_7C15,
    0xD1B5_4A32_D192_ED03,
    0xA0761D6478BD642F_u64,
    0xE7037ED1A0B428DB_u64,
    0x8EBC6AF09C88C6E3_u64,
    0x5893_5FD7_D75E_2A5B,
    0x2545_F491_4F6C_DD1D,
];

/// Default dimensions: `width = 2048`, `depth = 5`. Gives `ε ≈ e/2048 ≈
/// 0.00133` and `δ = e^-5 ≈ 0.0067`. Pinned as part of the sketch format.
pub(crate) const DEFAULT_WIDTH: usize = 2048;
pub(crate) const DEFAULT_DEPTH: usize = 5;

/// A byte tag leading the serialized form; bump on any layout change.
const FORMAT_TAG: u8 = 0x01;

/// A Count-Min sketch: `depth` rows of `width` `u64` counters, row-major.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct CountMinSketch {
    width: usize,
    depth: usize,
    counters: Vec<u64>,
}

impl CountMinSketch {
    /// An empty sketch of the given dimensions — the identity element of
    /// [`Self::merge`]. `width` must be positive and `depth` in
    /// `1..=ROW_SEEDS.len()`.
    pub(crate) fn new(width: usize, depth: usize) -> Result<Self> {
        ensure!(width > 0, "Count-Min width must be positive");
        ensure!(
            (1..=ROW_SEEDS.len()).contains(&depth),
            "Count-Min depth must be in 1..={}, got {depth}",
            ROW_SEEDS.len()
        );
        Ok(Self {
            width,
            depth,
            counters: vec![0u64; width * depth],
        })
    }

    /// An empty sketch at the default dimensions.
    pub(crate) fn default_dims() -> Self {
        Self {
            width: DEFAULT_WIDTH,
            depth: DEFAULT_DEPTH,
            counters: vec![0u64; DEFAULT_WIDTH * DEFAULT_DEPTH],
        }
    }

    /// The column a value maps to in a given row.
    #[inline]
    fn column(&self, value: &DataValue, row: usize) -> usize {
        {
            let width_u = crate::rules::convert::u64_from_usize_total(self.width);
            let slot = super::hash_value(value, ROW_SEEDS[row]) % width_u;
            crate::rules::convert::usize_from_u64_fitting(slot)
        }
    }

    /// Add `count` occurrences of `value`.
    pub(crate) fn add(&mut self, value: &DataValue, count: u64) {
        for row in 0..self.depth {
            let col = self.column(value, row);
            let cell = &mut self.counters[row * self.width + col];
            *cell = crate::rules::convert::saturating_add_u64(*cell, count);
        }
    }

    /// The estimated frequency of `value`: the minimum of its row counters.
    /// A pure function of the table bytes.
    ///
    /// Tests exercise this door; a named frequency-query stdlib op is not yet
    /// seated — no `#[allow(dead_code)]`, no fabricated discard-caller.
    pub(crate) fn estimate(&self, value: &DataValue) -> u64 {
        match (0..self.depth)
            .map(|row| {
                let col = self.column(value, row);
                self.counters[row * self.width + col]
            })
            .min()
        {
            Some(v) => v,
            None => {
                // Published floor for this absence.
                0
            },
        }
    }

    /// Merge `other` into `self` by element-wise addition — the monoid
    /// operation. Sketches must share dimensions. Returns whether any
    /// counter changed. Live door: [`super::aggr::AggrCountMin`] shard fold.
    pub(crate) fn merge(&mut self, other: &CountMinSketch) -> Result<bool> {
        ensure!(
            self.width == other.width && self.depth == other.depth,
            "cannot merge Count-Min sketches of {}x{} and {}x{}",
            self.depth,
            self.width,
            other.depth,
            other.width
        );
        let mut changed = false;
        for (l, r) in self.counters.iter_mut().zip(other.counters.iter()) {
            if *r != 0 {
                *l = crate::rules::convert::saturating_add_u64(*l, *r);
                changed = true;
            }
        }
        Ok(changed)
    }

    /// Serialize to the portable stored form: `[FORMAT_TAG, depth,
    /// width(8 LE), counters(8 LE each)...]`. The little-endian counter
    /// encoding makes the bytes identical on every platform.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 + 8 + self.counters.len() * 8);
        // Constructor bounds depth to `1..=ROW_SEEDS.len()` (8); low byte is
        // the whole value — no TryFrom::expect on a proven-fit narrow.
        out.extend_from_slice(&[FORMAT_TAG, self.depth.to_le_bytes()[0]]);
        out.extend_from_slice(
            &(crate::rules::convert::u64_from_usize_total(self.width))
            .to_le_bytes(),
        );
        for c in &self.counters {
            out.extend_from_slice(&c.to_le_bytes());
        }
        out
    }

    /// Parse the stored form, validating tag, dimensions, and length.
    /// Live door: [`super::aggr::AggrCountMin`] shard fold + query decode.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        let [tag, depth, w0, w1, w2, w3, w4, w5, w6, w7, rest @ ..] = bytes else {
            bail!("Count-Min bytes too short: {} bytes", bytes.len());
        };
        ensure!(*tag == FORMAT_TAG, "unknown Count-Min format tag {tag:#x}");
        let depth = usize::from(*depth);
        ensure!(
            (1..=ROW_SEEDS.len()).contains(&depth),
            "Count-Min depth out of range: {depth}"
        );
        let width =
            match usize::try_from(u64::from_le_bytes([*w0, *w1, *w2, *w3, *w4, *w5, *w6, *w7])) {
                Ok(v) => v,
                Err(_width_fits_usize) => bail!("Count-Min width does not fit usize"),
            };
        ensure!(width > 0, "Count-Min width must be positive");
        ensure!(
            rest.len() == width * depth * 8,
            "Count-Min counter length {} does not match {depth}x{width}",
            rest.len()
        );
        let counters = rest
            .chunks_exact(8)
            .map(|c| {
                let mut arr = [0u8; 8];
                arr.copy_from_slice(c);
                u64::from_le_bytes(arr)
            })
            .collect();
        Ok(Self {
            width,
            depth,
            counters,
        })
    }

    /// The sketch as a `DataValue::Bytes`.
    pub(crate) fn to_value(&self) -> DataValue {
        DataValue::Bytes(self.to_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use miette::Result;
    use std::collections::BTreeMap;

    fn val(i: i64) -> DataValue {
        DataValue::from(i)
    }

    /// A seeded Zipf-ish stream: element `i` appears `weight(i)` times, for a
    /// spread of frequencies to test the overestimate bound against.
    fn build(
        distinct: i64,
        dims: (usize, usize),
    ) -> Result<(CountMinSketch, BTreeMap<i64, u64>, u64)> {
        let mut cms = CountMinSketch::new(dims.0, dims.1)?;
        let mut exact = BTreeMap::new();
        let mut total = 0u64;
        for i in 0..distinct {
            // i is 0..distinct — nonneg, so the LE bit assemble equals
            // the value (same total-door shape as rules::convert).
            let w = 1 + (u64::from_le_bytes(i.to_le_bytes()) * 2654435761 % 37);
            cms.add(&val(i), w);
            *exact.entry(i).or_insert(0) += w;
            total += w;
        }
        Ok((cms, exact, total))
    }

    /// ACCURACY vs EXACT: the estimate never underestimates, and the number
    /// of items whose overestimate exceeds `ε·N` stays within the `δ`
    /// failure probability. Dimensions and stream are pinned.
    #[test]
    fn overestimate_bound_holds() -> Result<()> {
        let (width, depth) = (2048usize, 5usize);
        let (cms, exact, total) = build(5_000, (width, depth))?;
        let epsilon = std::f64::consts::E / crate::exec::fold::sketch::usize_to_f64(width);
        let delta = (-(crate::exec::fold::sketch::usize_to_f64(depth))).exp();
        let bound = match kyzo_model::value::Num::float(
            (epsilon * crate::exec::fold::sketch::u64_to_f64(total)).ceil(),
        )
        .to_int_coerced()
        {
            // ceil(eps*N) of nonneg floats is nonneg — bit assemble equals value.
            Some(i) => u64::from_le_bytes(i.to_le_bytes()),
            None => {
                // Published floor for this absence.
                0
            },
        };

        let mut violations = 0usize;
        for (&item, &true_count) in &exact {
            let est = cms.estimate(&val(item));
            assert!(
                est >= true_count,
                "item {item}: underestimate {est} < {true_count}"
            );
            if est > true_count + bound {
                violations += 1;
            }
        }
        // With probability >= 1-delta each item is within the bound; over
        // `exact.len()` items the expected violations are <= delta*len.
        let allowed = match kyzo_model::value::Num::float(
            (delta * crate::exec::fold::sketch::usize_to_f64(exact.len())).ceil(),
        )
        .to_int_coerced()
        {
            // ceil of a nonneg float — LE bit assemble equals the value.
            Some(i) => crate::rules::convert::usize_from_u64_fitting(u64::from_le_bytes(i.to_le_bytes())),
            None => {
                // Published floor for this absence.
                0
            },
        } + 1;
        assert!(
            violations <= allowed,
            "{violations} items exceeded the eps*N bound (allowed {allowed})"
        );
        Ok(())
    }

    /// MONOID LAWS for the merge: commutative, associative, identity. Merge
    /// then estimate must equal summing the streams.
    #[test]
    fn merge_is_a_commutative_monoid() -> Result<()> {
        let a = build(300, (256, 4))?.0;
        let b = build(300, (256, 4))?.0;
        // shift b's stream so it differs
        let mut b2 = CountMinSketch::new(256, 4)?;
        for i in 1000..1300 {
            b2.add(&val(i), 3);
        }
        let c = b2;

        // Identity.
        let mut ai = a.clone();
        ai.merge(&CountMinSketch::new(256, 4)?)?;
        assert_eq!(ai, a, "merge(a, empty) != a");

        // Commutative.
        let mut ab = a.clone();
        ab.merge(&b)?;
        let mut ba = b.clone();
        ba.merge(&a)?;
        assert_eq!(ab, ba, "merge not commutative");

        // Associative.
        let mut lhs = a.clone();
        lhs.merge(&b)?;
        lhs.merge(&c)?;
        let mut bc = b.clone();
        bc.merge(&c)?;
        let mut rhs = a.clone();
        rhs.merge(&bc)?;
        assert_eq!(lhs, rhs, "merge not associative");
        Ok(())
    }

    /// NOT A MEET: the merge is deliberately not idempotent — `merge(a, a)`
    /// doubles every counter — which is exactly why Count-Min is a normal
    /// aggregation and must never be registered as a meet. Pinning this
    /// stops a future refactor from silently promoting it.
    #[test]
    fn merge_is_not_idempotent() -> Result<()> {
        let a = build(100, (128, 3))?.0;
        let mut aa = a.clone();
        aa.merge(&a)?;
        assert_ne!(aa, a, "merge(a, a) == a: Count-Min must not be idempotent");
        // Concretely, the counter for a present item doubles.
        assert_eq!(aa.estimate(&val(0)), 2 * a.estimate(&val(0)));
        Ok(())
    }

    /// Merging shard sketches equals sketching the concatenated stream —
    /// the property that makes Count-Min useful across partitions.
    #[test]
    fn merge_equals_concatenated_stream() -> Result<()> {
        let mut left = CountMinSketch::new(512, 4)?;
        let mut right = CountMinSketch::new(512, 4)?;
        let mut whole = CountMinSketch::new(512, 4)?;
        for i in 0..500i64 {
            left.add(&val(i % 50), 1);
            whole.add(&val(i % 50), 1);
        }
        for i in 0..500i64 {
            right.add(&val(i % 70), 2);
            whole.add(&val(i % 70), 2);
        }
        left.merge(&right)?;
        assert_eq!(left, whole, "merged shards != whole-stream sketch");
        Ok(())
    }

    /// BYTE IDENTITY across fold orders: counter add is order-free, so the
    /// same multiset in any order gives byte-identical tables.
    #[test]
    fn byte_identical_across_fold_orders() -> Result<()> {
        let mut asc = CountMinSketch::new(256, 4)?;
        for i in 0..1000i64 {
            asc.add(&val(i % 123), 1);
        }
        let mut desc = CountMinSketch::new(256, 4)?;
        for i in (0..1000i64).rev() {
            desc.add(&val(i % 123), 1);
        }
        assert_eq!(asc.to_bytes(), desc.to_bytes());
        Ok(())
    }

    /// Round-trip through the stored form, and reject corruption.
    #[test]
    fn serialization_round_trips_and_rejects_corruption() -> Result<()> {
        let (cms, _, _) = build(400, (128, 3))?;
        assert_eq!(CountMinSketch::decode(&cms.to_bytes())?, cms);
        assert!(CountMinSketch::decode(&[]).is_err());
        assert!(CountMinSketch::decode(&[0x02, 3, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());
        let mut short = cms.to_bytes();
        short.pop();
        assert!(CountMinSketch::decode(&short).is_err());
        Ok(())
    }

    /// PINNED-LITERAL fingerprint for a fixed input: hash-seed or layout
    /// drift fails loudly.
    #[test]
    fn pinned_sketch_fingerprint() -> Result<()> {
        // INPUT ANCHOR. The sketch buckets each value by hashing its
        // CANONICAL encoding. Pin that encoding to the format law by hand
        // (value tag 0x10 = Tag::Num, then the Num key pinned by
        // `data::value::number::format_v1_golden_vectors`), so the
        // fingerprint below is provably a function of the format-correct
        // input, not an implementation snapshot:
        //   Int(0)   = 10 02 00
        //   Int(1)   = 10 03 04 39 80 00..(9)
        //   Int(999) = 10 03 04 42 f9 c0 00..(8)
        let enc = |v: &DataValue| {
            let mut b = Vec::new();
            kyzo_model::value::append_canonical(&mut b, v);
            b
        };
        assert_eq!(enc(&val(0)), vec![0x10, 0x02, 0x00]);
        assert_eq!(
            enc(&val(1)),
            vec![0x10, 0x03, 0x04, 0x39, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            enc(&val(999)),
            vec![0x10, 0x03, 0x04, 0x42, 0xf9, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0]
        );

        // The fingerprint is xxh64 of the counter array [FORMAT_TAG, depth,
        // width_le, counters_le...] produced by folding the anchored input
        // above through the (unchanged) Count-Min bucket hash. A drift in
        // the counter layout or the bucket hash changes it.
        let mut cms = CountMinSketch::new(2048, 5)?;
        for i in 0..1000i64 {
            cms.add(&val(i), 1);
        }
        assert_eq!(cms.to_bytes()[..2], [FORMAT_TAG, 5]);
        let fingerprint = super::super::xxh64(&cms.to_bytes(), 0);
        assert_eq!(fingerprint, 0x73CC_4C21_CD68_9237);
        Ok(())
    }
}
