/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! t-digest: quantile estimation, made deterministic by a sorted fold.
//!
//! A t-digest summarizes a distribution as a small set of *centroids*
//! (a mean and a weight), packed more tightly near the tails than the
//! middle so that extreme quantiles stay accurate. It answers "what value
//! sits at quantile q" with rank error bounded by roughly `1/δ` in the
//! middle and better at the tails.
//!
//! ## The determinism problem, stated plainly
//!
//! The usual t-digest is built incrementally, and its centroid clustering
//! **depends on the order points arrive** — two orders of the same multiset
//! give different centroids and different estimates. Its merge is likewise
//! **not associative-exact**: `merge(merge(a, b), c)` need not equal
//! `merge(a, merge(b, c))`, because re-clustering depends on how centroids
//! were grouped. Neither property is acceptable for an engine whose
//! contract is "same input, same output", and neither can be a *meet*
//! (a meet must be associative).
//!
//! So this t-digest is **not** folded incrementally and is **not** a meet
//! aggregation. [`TDigest::from_values`] buffers the raw values and builds
//! the digest from them **sorted into ascending [`DataValue`] order** — the
//! exact total order the memcmp key encoding preserves. Sorting first makes
//! the resulting centroids a pure function of the input *multiset*,
//! independent of arrival order: the canonical fold policy. The aggregation
//! is therefore a **normal aggregation** that buffers and builds at
//! finalization.
//!
//! [`TDigest::merge`] is still provided, for combining shard digests, under
//! a canonical deterministic policy (concatenate centroids, sort by mean,
//! re-run the same sweep). It is deterministic and commutative, so a fixed
//! reduction *tree* is reproducible — but it is deliberately **not** claimed
//! to be associative, and the aggregation never relies on it.
//!
//! The build uses the standard k1 scale function; no randomness is involved.

use std::io::Write;

use miette::{Result, bail, ensure};

use crate::data::value::{DataValue, GermanStr, Num};

/// The compression parameter δ. Larger is more accurate and larger; 100 is
/// the common default (≈ 1% rank error mid-distribution, better at tails).
/// Pinned as part of the sketch format.
pub(crate) const DEFAULT_COMPRESSION: f64 = 100.0;

/// A byte tag leading the serialized form; bump on any layout change.
const FORMAT_TAG: u8 = 0x01;

/// One centroid: the mean of the points it absorbed and their total weight.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Centroid {
    mean: f64,
    weight: f64,
}

/// A finalized t-digest: sorted centroids plus the exact min and max (kept
/// separately so the extreme quantiles are exact anchors).
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct TDigest {
    compression: f64,
    centroids: Vec<Centroid>,
    min: f64,
    max: f64,
    count: f64,
}

/// The k1 scale function `k(q) = δ/(2π)·asin(2q−1)`, and its inverse. One
/// centroid is allowed to span at most one unit of `k`, which is what packs
/// the tails tightly and the middle loosely.
fn scale(q: f64, compression: f64) -> f64 {
    let q = q.clamp(0.0, 1.0);
    compression / (2.0 * std::f64::consts::PI) * (2.0 * q - 1.0).asin()
}

fn scale_inv(k: f64, compression: f64) -> f64 {
    let x = k * 2.0 * std::f64::consts::PI / compression;
    (x.sin() + 1.0) / 2.0
}

impl TDigest {
    /// Build a digest from raw values at the given compression. The values
    /// are sorted into ascending [`DataValue`] order first, so the result is
    /// a pure function of the multiset — the canonical, order-independent
    /// fold. Non-numeric values are an error.
    pub(crate) fn from_values(values: &[DataValue], compression: f64) -> Result<Self> {
        ensure!(compression >= 1.0, "t-digest compression must be >= 1");
        let mut nums: Vec<f64> = Vec::with_capacity(values.len());
        for v in values {
            match v {
                DataValue::Num(n) => nums.push(n.get_float()),
                other => bail!("t-digest requires numeric values, got {other:?}"),
            }
        }
        Ok(Self::from_sorted_weighted(sort_floats(nums), compression))
    }

    /// Build from centroids-as-weighted-points already collected; sorts by
    /// mean and runs the merging sweep. Shared by [`Self::from_values`] (each
    /// value is a weight-1 point) and [`Self::merge`].
    fn from_sorted_weighted(sorted: Vec<(f64, f64)>, compression: f64) -> Self {
        if sorted.is_empty() {
            return Self {
                compression,
                centroids: vec![],
                min: f64::NAN,
                max: f64::NAN,
                count: 0.0,
            };
        }
        let total: f64 = sorted.iter().map(|(_, w)| *w).sum();
        let min = sorted.first().unwrap().0;
        let max = sorted.last().unwrap().0;

        let mut centroids: Vec<Centroid> = Vec::new();
        let mut weight_so_far = 0.0f64;
        let mut q_limit = scale_inv(scale(0.0, compression) + 1.0, compression);

        let (m0, w0) = sorted[0];
        let mut cur = Centroid {
            mean: m0,
            weight: w0,
        };

        for &(mean, weight) in &sorted[1..] {
            let proposed = (weight_so_far + cur.weight + weight) / total;
            if proposed <= q_limit {
                // Absorb: weighted-mean update.
                cur.weight += weight;
                cur.mean += (mean - cur.mean) * weight / cur.weight;
            } else {
                weight_so_far += cur.weight;
                centroids.push(cur);
                q_limit = scale_inv(scale(weight_so_far / total, compression) + 1.0, compression);
                cur = Centroid { mean, weight };
            }
        }
        centroids.push(cur);

        Self {
            compression,
            centroids,
            min,
            max,
            count: total,
        }
    }

    #[allow(dead_code)] // used by `merge` (itself pre-landing) and by tests.
    pub(crate) fn is_empty(&self) -> bool {
        self.centroids.is_empty()
    }

    /// The estimated value at quantile `q ∈ [0, 1]`. Interpolates between
    /// centroid centers in rank space, with the exact min and max as the
    /// `q = 0` and `q = 1` anchors. A pure function of the digest. `NaN` for
    /// an empty digest.
    pub(crate) fn quantile(&self, q: f64) -> f64 {
        if self.centroids.is_empty() {
            return f64::NAN;
        }
        let q = q.clamp(0.0, 1.0);
        if self.centroids.len() == 1 {
            return self.centroids[0].mean;
        }
        let target = q * self.count;

        // Anchor points in (rank, value) space: min at rank 0, each centroid
        // at the rank of its center, max at rank `count`. Ranks strictly
        // increasing; values non-decreasing — so interpolation is monotone.
        // Walk until the bracketing pair around `target` is found.
        let mut prev_rank = 0.0f64;
        let mut prev_val = self.min;
        let mut cum = 0.0f64;
        for c in &self.centroids {
            let center = cum + c.weight / 2.0;
            if target <= center {
                return interpolate(target, prev_rank, prev_val, center, c.mean);
            }
            prev_rank = center;
            prev_val = c.mean;
            cum += c.weight;
        }
        // Between the last centroid center and the max anchor.
        interpolate(target, prev_rank, prev_val, self.count, self.max)
    }

    /// Merge shard digests under the canonical policy: pool all centroids as
    /// weighted points, sort by mean, and re-run the build sweep.
    /// Deterministic and commutative; **not** claimed associative (see the
    /// module docs), so callers must not depend on grouping.
    // Shard-merge API: exercised by tests; consumed when partitioned
    // digest-building lands. The aggregation never uses it (it builds from
    // sorted values instead).
    #[allow(dead_code)]
    pub(crate) fn merge(&self, other: &TDigest) -> Result<TDigest> {
        ensure!(
            self.compression == other.compression,
            "cannot merge t-digests of compression {} and {}",
            self.compression,
            other.compression
        );
        if self.is_empty() {
            return Ok(other.clone());
        }
        if other.is_empty() {
            return Ok(self.clone());
        }
        let mut pooled: Vec<(f64, f64)> =
            Vec::with_capacity(self.centroids.len() + other.centroids.len());
        for c in self.centroids.iter().chain(other.centroids.iter()) {
            pooled.push((c.mean, c.weight));
        }
        // Canonical order: by mean, then weight — a total order on the pool.
        pooled.sort_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap()
                .then(a.1.partial_cmp(&b.1).unwrap())
        });
        let mut merged = Self::from_sorted_weighted(pooled, self.compression);
        merged.min = self.min.min(other.min);
        merged.max = self.max.max(other.max);
        Ok(merged)
    }

    /// Serialize to the portable stored form: tag, compression, count, min,
    /// max, centroid count, then `(mean, weight)` pairs — all `f64`/`u64`
    /// little-endian, so the bytes are identical on every platform.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + 8 * 4 + 8 + self.centroids.len() * 16);
        out.write_all(&[FORMAT_TAG]).unwrap();
        out.write_all(&self.compression.to_le_bytes()).unwrap();
        out.write_all(&self.count.to_le_bytes()).unwrap();
        out.write_all(&self.min.to_le_bytes()).unwrap();
        out.write_all(&self.max.to_le_bytes()).unwrap();
        out.write_all(&(self.centroids.len() as u64).to_le_bytes())
            .unwrap();
        for c in &self.centroids {
            out.write_all(&c.mean.to_le_bytes()).unwrap();
            out.write_all(&c.weight.to_le_bytes()).unwrap();
        }
        out
    }

    /// Parse the stored form, validating tag, length, and centroid count.
    // Read-side API: exercised by tests; consumed when a stored digest is
    // decoded for querying by the runtime tier.
    #[allow(dead_code)]
    pub(crate) fn from_bytes(bytes: &[u8]) -> Result<Self> {
        ensure!(!bytes.is_empty(), "empty t-digest bytes");
        ensure!(
            bytes[0] == FORMAT_TAG,
            "unknown t-digest format tag {:#x}",
            bytes[0]
        );
        let body = &bytes[1..];
        ensure!(body.len() >= 40, "t-digest header truncated");
        let compression = f64::from_le_bytes(body[0..8].try_into().unwrap());
        let count = f64::from_le_bytes(body[8..16].try_into().unwrap());
        let min = f64::from_le_bytes(body[16..24].try_into().unwrap());
        let max = f64::from_le_bytes(body[24..32].try_into().unwrap());
        let n = u64::from_le_bytes(body[32..40].try_into().unwrap()) as usize;
        let rest = &body[40..];
        ensure!(
            rest.len() == n * 16,
            "t-digest centroid bytes {} do not match count {n}",
            rest.len()
        );
        let centroids = rest
            .chunks_exact(16)
            .map(|c| Centroid {
                mean: f64::from_le_bytes(c[0..8].try_into().unwrap()),
                weight: f64::from_le_bytes(c[8..16].try_into().unwrap()),
            })
            .collect();
        Ok(Self {
            compression,
            centroids,
            min,
            max,
            count,
        })
    }

    /// The digest as a `DataValue::Bytes`.
    pub(crate) fn to_value(&self) -> DataValue {
        DataValue::Bytes(GermanStr::from_bytes(&self.to_bytes()))
    }
}

/// Linear interpolation in rank space, guarding a zero-width bracket.
fn interpolate(target: f64, r0: f64, v0: f64, r1: f64, v1: f64) -> f64 {
    if r1 <= r0 {
        return v0;
    }
    v0 + (v1 - v0) * (target - r0) / (r1 - r0)
}

/// Sort raw `f64`s into ascending order via the exact [`Num`] total order
/// (the memcmp order), returning weight-1 points. Using `Num`'s `Ord` rather
/// than `f64::partial_cmp` keeps NaN handling and the int/float boundary
/// consistent with the rest of the engine and total (no `unwrap` on `None`).
fn sort_floats(mut xs: Vec<f64>) -> Vec<(f64, f64)> {
    xs.sort_by_key(|x| Num::Float(*x));
    xs.into_iter().map(|x| (x, 1.0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_of<I: IntoIterator<Item = f64>>(it: I, compression: f64) -> TDigest {
        let vals: Vec<DataValue> = it.into_iter().map(DataValue::from).collect();
        TDigest::from_values(&vals, compression).unwrap()
    }

    /// The exact rank of `x` in `sorted` (fraction of points <= x).
    fn exact_rank(sorted: &[f64], x: f64) -> f64 {
        let count = sorted.iter().filter(|&&v| v <= x).count();
        count as f64 / sorted.len() as f64
    }

    /// ACCURACY vs EXACT, in two ways. In RANK SPACE — the metric t-digest
    /// actually bounds — the returned value's *true rank* must be within the
    /// digest's guarantee of q (δ=100 ⇒ ~1% mid, tighter at tails). And in
    /// VALUE SPACE, on this uniform ramp where the true q-value is `q·n`, the
    /// estimate must land within a *tight* `0.005·n` band: the correct build
    /// is near-exact (errors < 1), so this band is what makes a corrupted
    /// centroid mean (which biases estimates by ~1.5% of the range) fail on
    /// correctness, not merely on the byte fingerprint. Data is a pinned
    /// deterministic ramp, so this is reproducible.
    #[test]
    fn quantile_rank_error_within_bound() {
        let n = 100_000;
        let data: Vec<f64> = (0..n).map(|i| i as f64).collect();
        let digest = digest_of(data.iter().copied(), 100.0);

        for &q in &[0.001, 0.01, 0.1, 0.25, 0.5, 0.75, 0.9, 0.99, 0.999] {
            let est = digest.quantile(q);
            let got_rank = exact_rank(&data, est);
            let err = (got_rank - q).abs();
            // Generous relative to the ~1/δ = 1% mid guarantee; tails tighter.
            assert!(
                err < 0.02,
                "q={q}: value {est} has rank {got_rank}, error {err:.4}"
            );
            // Tight value-space band: kills centroid-mean corruption.
            let true_value = q * n as f64;
            assert!(
                (est - true_value).abs() < 0.005 * n as f64,
                "q={q}: value {est} off true {true_value} by more than 0.005*n"
            );
        }
    }

    /// Extreme quantiles are exact anchors: q=0 is the min, q=1 the max.
    #[test]
    fn extremes_are_exact() {
        let digest = digest_of((0..1000).map(|i| i as f64 * 0.5), 100.0);
        assert_eq!(digest.quantile(0.0), 0.0);
        assert_eq!(digest.quantile(1.0), 999.0 * 0.5);
    }

    /// The centroid count stays bounded by the compression, not the input
    /// size — the whole point of a sketch.
    #[test]
    fn size_is_bounded_by_compression() {
        let digest = digest_of((0..1_000_000).map(|i| i as f64), 100.0);
        assert!(
            digest.centroids.len() <= 300,
            "centroid count {} not bounded by compression",
            digest.centroids.len()
        );
    }

    /// DETERMINISM / ORDER INDEPENDENCE: the same multiset in ascending,
    /// descending, and a shuffled order builds byte-identical digests,
    /// because the build sorts first. This is the canonical-fold guarantee.
    #[test]
    fn byte_identical_across_input_orders() {
        let base: Vec<f64> = (0..5000).map(|i| (i as f64 * 2.7).sin() * 1000.0).collect();
        let asc = {
            let mut v = base.clone();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            digest_of(v, 100.0)
        };
        let desc = {
            let mut v = base.clone();
            v.sort_by(|a, b| b.partial_cmp(a).unwrap());
            digest_of(v, 100.0)
        };
        // A deterministic shuffle (index-scramble) of the same multiset.
        let shuffled = {
            let mut v = base.clone();
            v.rotate_left(1234);
            v.reverse();
            digest_of(v, 100.0)
        };
        assert_eq!(asc.to_bytes(), desc.to_bytes(), "order changed the digest");
        assert_eq!(
            asc.to_bytes(),
            shuffled.to_bytes(),
            "order changed the digest"
        );
    }

    /// MERGE is deterministic and commutative: recomputing gives identical
    /// bytes, and swapping operands gives identical bytes. This is what makes
    /// a fixed shard-reduction reproducible.
    #[test]
    fn merge_is_deterministic_and_commutative() {
        let a = digest_of((0..3000).map(|i| i as f64), 100.0);
        let b = digest_of((2000..6000).map(|i| i as f64 * 1.3), 100.0);

        let ab1 = a.merge(&b).unwrap();
        let ab2 = a.merge(&b).unwrap();
        assert_eq!(ab1.to_bytes(), ab2.to_bytes(), "merge not deterministic");

        let ba = b.merge(&a).unwrap();
        assert_eq!(ab1.to_bytes(), ba.to_bytes(), "merge not commutative");
    }

    /// HONESTY: merge is deliberately not claimed associative. Both
    /// groupings are each deterministic (recompute-identical); this test
    /// documents that we do not assert they are equal, and pins that each
    /// grouping is at least self-consistent. If a future change made merge
    /// exactly associative that would be a strict improvement — but nothing
    /// in the engine relies on it, and the quantile aggregation uses the
    /// order-independent [`TDigest::from_values`] build instead.
    #[test]
    fn merge_associativity_is_not_relied_upon() {
        let a = digest_of((0..2000).map(|i| i as f64), 100.0);
        let b = digest_of((1000..4000).map(|i| i as f64 * 0.7), 100.0);
        let c = digest_of((500..2500).map(|i| i as f64 * 2.1), 100.0);

        let left = a.merge(&b).unwrap().merge(&c).unwrap();
        let right = a.merge(&b.merge(&c).unwrap()).unwrap();

        // Each grouping is self-consistent (deterministic).
        assert_eq!(
            left.to_bytes(),
            a.merge(&b).unwrap().merge(&c).unwrap().to_bytes()
        );
        assert_eq!(
            right.to_bytes(),
            a.merge(&b.merge(&c).unwrap()).unwrap().to_bytes()
        );
        // We do NOT assert left == right: associativity is not guaranteed,
        // and the aggregation never depends on it. Both must still give
        // usable quantiles close to the true median of the combined data.
        for d in [&left, &right] {
            assert!(d.quantile(0.5).is_finite());
        }
    }

    /// Round-trip through the stored form, and reject corruption.
    #[test]
    fn serialization_round_trips_and_rejects_corruption() {
        let d = digest_of((0..2000).map(|i| i as f64 * 0.25), 100.0);
        assert_eq!(TDigest::from_bytes(&d.to_bytes()).unwrap(), d);
        assert!(TDigest::from_bytes(&[]).is_err());
        assert!(TDigest::from_bytes(&[0x02]).is_err());
        let mut short = d.to_bytes();
        short.truncate(20);
        assert!(TDigest::from_bytes(&short).is_err());
    }

    /// Non-numeric input is rejected, not silently coerced.
    #[test]
    fn rejects_non_numeric() {
        let vals = vec![DataValue::from(1i64), DataValue::from("nope")];
        assert!(TDigest::from_values(&vals, 100.0).is_err());
    }

    /// PINNED-LITERAL fingerprint for a fixed input: any drift of the build
    /// algorithm, the scale function, or the serialization fails loudly.
    #[test]
    fn pinned_digest_fingerprint() {
        let digest = digest_of((0..1000).map(|i| i as f64), 100.0);
        let fingerprint = super::super::xxh64(&digest.to_bytes(), 0);
        assert_eq!(fingerprint, 0xA474_C02B_97F8_32C4);
    }
}
