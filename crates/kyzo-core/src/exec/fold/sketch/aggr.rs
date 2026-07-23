/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The sketches, wrapped as aggregations.
//!
//! Each wrapper maps a sketch onto the [`NormalAggrObj`] / [`MeetAggrObj`]
//! contracts of `exec/fold/aggr.rs`, and the *kind* of each aggregation is
//! dictated by the lattice analysis in the sketch modules:
//!
//! | name         | kind   | value in → out            | why                                   |
//! |--------------|--------|---------------------------|---------------------------------------|
//! | `hll`        | normal | elements → Int estimate   | output is a scalar count (cf `count_unique`) |
//! | `hll_sketch` | normal | elements → Bytes sketch   | the builder that feeds `hll_union`    |
//! | `hll_union`  | **meet** | sketches → merged sketch | register-max is a semilattice (cf `union`) |
//! | `count_min`  | normal | elements → Bytes sketch   | add-merge is a monoid, not idempotent |
//! | `tdigest`    | normal | elements → Bytes digest   | merge not associative-exact           |
//! | `quantile`   | normal | elements → Float at q     | buffer-and-sort, order-independent    |
//!
//! Only `hll_union` is a meet. Model [`parse_aggr`] admits the sketch names;
//! fold binding is via [`crate::exec::fold::aggr::meet_op`] /
//! [`crate::exec::fold::aggr::normal_op`].

use miette::{Result, bail, ensure, miette};

use crate::exec::fold::aggr::{MeetAccum, MeetAggrObj, NormalAggr, NormalAggrObj};
use crate::exec::fold::sketch::count_min::CountMinSketch;
use crate::exec::fold::sketch::hll::HyperLogLog;
use crate::exec::fold::sketch::tdigest::{DEFAULT_COMPRESSION, TDigest};
use kyzo_model::data_value_any;
use kyzo_model::value::DataValue;

// ── HyperLogLog: approximate distinct count (normal) ─────────────────────

/// `hll(x)`: fold raw elements, return the estimated distinct count as an
/// `Int`. Normal, not meet: its output is a scalar, and two counts cannot be
/// meet-combined — exactly the `count_unique` story.
pub(crate) struct AggrHll {
    hll: HyperLogLog,
}

impl AggrHll {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            hll: HyperLogLog::default_precision(),
        }
    }
}

impl crate::exec::fold::aggr::seal::Sealed for AggrHll {}

impl NormalAggrObj for AggrHll {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        self.hll.add(value);
        Ok(())
    }
    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::from(self.hll.estimate_count()))
    }
}

/// `hll_sketch(x)`: fold raw elements, return the sketch itself as `Bytes` —
/// the builder whose output is fed to `hll_union`.
pub(crate) struct AggrHllSketch {
    hll: HyperLogLog,
}

impl AggrHllSketch {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            hll: HyperLogLog::default_precision(),
        }
    }
}

impl crate::exec::fold::aggr::seal::Sealed for AggrHllSketch {}

impl NormalAggrObj for AggrHllSketch {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        self.hll.add(value);
        Ok(())
    }
    fn get(&self) -> Result<DataValue> {
        Ok(self.hll.to_value())
    }
}

// ── HyperLogLog union: the semilattice meet ──────────────────────────────

/// Decode a `Bytes` value as a HyperLogLog sketch.
fn as_hll(v: &DataValue) -> Result<HyperLogLog> {
    match v {
        DataValue::Bytes(b) => HyperLogLog::decode(b),
        other @ (data_value_any!()) => bail!("hll_union expects sketch bytes, got {other:?}"),
    }
}

/// `hll_union` as a meet: fold sketch `Bytes` by register-wise maximum.
pub(crate) struct MeetAggrHllUnion;

impl crate::exec::fold::aggr::seal::Sealed for MeetAggrHllUnion {}

impl MeetAggrObj for MeetAggrHllUnion {
    fn init_val(&self) -> MeetAccum {
        MeetAccum::Value(HyperLogLog::default_precision().to_value())
    }
    fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool> {
        match left {
            MeetAccum::Empty => {
                if matches!(right, MeetAccum::Empty) {
                    return Ok(false);
                }
                *left = right.clone();
                Ok(true)
            }
            MeetAccum::Value(left_v) => match right {
                MeetAccum::Empty => Ok(false),
                MeetAccum::Value(right_v) => {
                    let mut l = as_hll(left_v)?;
                    let r = as_hll(right_v)?;
                    let changed = l.merge(&r);
                    if changed {
                        *left_v = l.to_value();
                    }
                    Ok(changed)
                }
            },
        }
    }
}

/// The normal form of `hll_union`, for use outside recursion.
pub(crate) struct AggrHllUnion {
    acc: Option<HyperLogLog>,
}

impl AggrHllUnion {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self { acc: None }
    }
}

impl crate::exec::fold::aggr::seal::Sealed for AggrHllUnion {}

impl NormalAggrObj for AggrHllUnion {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        let incoming = as_hll(value)?;
        match &mut self.acc {
            None => self.acc = Some(incoming),
            Some(acc) => {
                acc.merge(&incoming);
            }
        }
        Ok(())
    }
    fn get(&self) -> Result<DataValue> {
        Ok(match &self.acc {
            Some(h) => h.to_value(),
            None => HyperLogLog::default_precision().to_value(),
        })
    }
}

// ── Count-Min: frequency table (normal, monoid) ──────────────────────────

/// `count_min(x)`: fold raw elements into a frequency table, returned as
/// `Bytes`. Normal only: the merge is a monoid but not idempotent.
pub(crate) struct AggrCountMin {
    cms: CountMinSketch,
}

impl AggrCountMin {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            cms: CountMinSketch::default_dims(),
        }
    }
}

impl crate::exec::fold::aggr::seal::Sealed for AggrCountMin {}

impl NormalAggrObj for AggrCountMin {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            // Shard merge: fold a stored sketch into this builder.
            DataValue::Bytes(b) => {
                let other = CountMinSketch::decode(b)?;
                self.cms.merge(&other)?;
                Ok(())
            }
            other @ (data_value_any!()) => {
                self.cms.add(other, 1);
                Ok(())
            }
        }
    }
    fn get(&self) -> Result<DataValue> {
        Ok(self.cms.to_value())
    }
}

// ── t-digest: quantiles (normal, buffer-and-sort) ────────────────────────

/// `tdigest(x)`: buffer raw numeric elements and, at finalization, build the
/// digest from them **sorted** — returning the digest as `Bytes`.
pub(crate) struct AggrTDigest {
    buf: Vec<DataValue>,
}

impl AggrTDigest {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self { buf: Vec::new() }
    }
}

impl crate::exec::fold::aggr::seal::Sealed for AggrTDigest {}

impl NormalAggrObj for AggrTDigest {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        self.buf.push(value.clone());
        Ok(())
    }
    fn get(&self) -> Result<DataValue> {
        let mut raw = Vec::new();
        let mut digests = Vec::new();
        for v in &self.buf {
            match v {
                // Shard fold: stored digest Bytes decode + merge.
                DataValue::Bytes(b) => digests.push(TDigest::decode(b)?),
                other @ (data_value_any!()) => raw.push(other.clone()),
            }
        }
        let mut acc = TDigest::from_values(&raw, DEFAULT_COMPRESSION)?;
        for d in digests {
            acc = acc.merge(&d)?;
        }
        Ok(acc.to_value())
    }
}

/// `quantile(x, q)`: buffer raw numeric elements and return the estimated
/// value at quantile `q ∈ [0, 1]`.
pub(crate) struct AggrQuantile {
    buf: Vec<DataValue>,
    q: f64,
}

/// Factory for the `quantile` normal fold (compile-time `q` argument).
pub(crate) fn quantile_factory(args: &[DataValue]) -> Result<NormalAggr> {
    let q = args
        .first()
        .ok_or_else(|| miette!("'quantile' requires a quantile level argument in [0, 1]"))?
        .get_float()
        .ok_or_else(|| miette!("the quantile level for 'quantile' must be numeric"))?;
    ensure!(
        (0.0..=1.0).contains(&q),
        "the quantile level for 'quantile' must be in [0, 1], got {q}"
    );
    Ok(NormalAggr::Quantile(AggrQuantile { buf: vec![], q }))
}

impl crate::exec::fold::aggr::seal::Sealed for AggrQuantile {}

impl NormalAggrObj for AggrQuantile {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        self.buf.push(value.clone());
        Ok(())
    }
    fn get(&self) -> Result<DataValue> {
        let digest = TDigest::from_values(&self.buf, DEFAULT_COMPRESSION)?;
        Ok(match digest.quantile(self.q) {
            Some(v) => DataValue::from(v),
            None => crate::exec::fold::aggr::MeetAccum::Empty.to_value(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::fold::aggr::{meet_op, normal_op};
    use crate::exec::fold::sketch::hll::HyperLogLog;
    use kyzo_model::program::aggregate::{Aggregation, parse_aggr};
    use miette::{Result, miette};

    fn val(i: i64) -> DataValue {
        DataValue::from(i)
    }

    fn aggr(name: &str) -> Result<Aggregation> {
        parse_aggr(name)
            .map_err(|e| miette!("sketch name is lawful: {e}"))?
            .ok_or_else(|| miette!("sketch name is known"))
    }

    fn run_normal(mut op: NormalAggr, vals: &[DataValue]) -> Result<DataValue> {
        for v in vals {
            op.set(v)?;
        }
        op.get()
    }

    /// `hll` end-to-end through the aggregation trait.
    #[test]
    fn hll_aggregation_estimates_distinct() -> Result<()> {
        let mut stream = vec![];
        for i in 0..20_000i64 {
            stream.push(val(i % 5000));
        }
        let out = run_normal(normal_op(&aggr("hll")?, &[])?, &stream)?;
        let est = out.get_int().ok_or_else(|| miette!("int"))?;
        let rel = crate::exec::fold::sketch::u64_to_f64((est - 5000).unsigned_abs()) / 5000.0;
        assert!(rel < 0.05, "hll estimate {est} off by {rel}");
        Ok(())
    }

    /// `hll_union` meet form obeys the semilattice laws over sketch-bytes.
    #[test]
    fn hll_union_meet_obeys_semilattice_laws() -> Result<()> {
        let op = meet_op(&aggr("hll_union")?).ok_or_else(|| miette!("hll_union is a meet"))?;

        let mk = |lo: i64, hi: i64| {
            let mut h = HyperLogLog::default_precision();
            for i in lo..hi {
                h.add(&val(i));
            }
            h.to_value()
        };
        let x = MeetAccum::Value(mk(0, 3000));
        let y = MeetAccum::Value(mk(1000, 5000));
        let z = MeetAccum::Value(mk(4000, 6000));

        let meet = |a: &MeetAccum, b: &MeetAccum| -> Result<MeetAccum> {
            let mut acc = a.clone();
            op.update(&mut acc, b)?;
            Ok(acc)
        };

        let mut acc = x.clone();
        assert!(!op.update(&mut acc, &x)?, "meet(x,x) reported change");
        assert_eq!(acc, x, "meet(x,x) altered x");

        let mut id = op.init_val();
        op.update(&mut id, &x)?;
        assert_eq!(id, x, "meet(init,x) != x");

        assert_eq!(
            meet(&meet(&x, &y)?, &z)?,
            meet(&x, &meet(&y, &z)?)?,
            "assoc"
        );
        assert_eq!(meet(&x, &y)?, meet(&y, &x)?, "commutative");
        Ok(())
    }

    /// Meet form and normal form of `hll_union` agree.
    #[test]
    fn hll_union_meet_and_normal_agree() -> Result<()> {
        let mk = |lo: i64, hi: i64| {
            let mut h = HyperLogLog::default_precision();
            for i in lo..hi {
                h.add(&val(i));
            }
            h.to_value()
        };
        let sketches = [mk(0, 2000), mk(1500, 4000), mk(3000, 5000)];

        let meet = meet_op(&aggr("hll_union")?).ok_or_else(|| miette!("hll_union meet"))?;
        let mut acc = meet.init_val();
        for s in &sketches {
            meet.update(&mut acc, &MeetAccum::Value(s.clone()))?;
        }

        let normal_out = run_normal(normal_op(&aggr("hll_union")?, &[])?, &sketches)?;
        assert_eq!(acc.to_value(), normal_out, "meet and normal folds disagree");
        Ok(())
    }

    /// Only `hll_union` is a meet among the sketch family.
    #[test]
    fn only_hll_union_is_meet() -> Result<()> {
        assert!(aggr("hll_union")?.is_meet());
        assert!(!aggr("hll")?.is_meet());
        assert!(!aggr("count_min")?.is_meet());
        assert!(!aggr("tdigest")?.is_meet());
        assert!(!aggr("quantile")?.is_meet());
        assert!(meet_op(&aggr("count_min")?).is_none());
        Ok(())
    }

    /// `count_min` builds a queryable frequency table through the trait.
    #[test]
    fn count_min_aggregation_builds_table() -> Result<()> {
        let mut stream = vec![];
        for i in 0..1000i64 {
            for _ in 0..(i % 7 + 1) {
                stream.push(val(i));
            }
        }
        let out = run_normal(normal_op(&aggr("count_min")?, &[])?, &stream)?;
        let DataValue::Bytes(bytes) = out else {
            return Err(miette!("count_min should return bytes"));
        };
        let cms = CountMinSketch::decode(&bytes)?;
        assert!(cms.estimate(&val(6)) >= 7);
        Ok(())
    }

    /// `quantile(x, q)` returns the estimated value at q, order-independently.
    #[test]
    fn quantile_aggregation_is_order_independent() -> Result<()> {
        let asc: Vec<DataValue> = (0..10_000)
            .map(|i| DataValue::from(crate::exec::fold::sketch::usize_to_f64(i)))
            .collect();
        let desc: Vec<DataValue> = (0..10_000)
            .rev()
            .map(|i| DataValue::from(crate::exec::fold::sketch::usize_to_f64(i)))
            .collect();

        let q_asc = run_normal(quantile_factory(&[DataValue::from(0.9f64)])?, &asc)?;
        let q_desc = run_normal(quantile_factory(&[DataValue::from(0.9f64)])?, &desc)?;
        assert_eq!(q_asc, q_desc, "quantile depended on input order");

        let est = q_asc.get_float().ok_or_else(|| miette!("float"))?;
        assert!((est - 9000.0).abs() < 200.0, "p90 estimate {est} off");
        Ok(())
    }

    /// `quantile` rejects an out-of-range level at construction.
    #[test]
    fn quantile_rejects_bad_level() {
        assert!(quantile_factory(&[DataValue::from(1.5f64)]).is_err());
        assert!(quantile_factory(&[]).is_err());
    }

    /// Model [`parse_aggr`] admits exactly the sketch names.
    #[test]
    fn model_admits_sketch_names() -> Result<()> {
        for name in [
            "hll",
            "hll_sketch",
            "hll_union",
            "count_min",
            "tdigest",
            "quantile",
        ] {
            assert_eq!(aggr(name)?.name, name);
        }
        assert!(parse_aggr("not_a_sketch")?.is_none());
        Ok(())
    }
}
