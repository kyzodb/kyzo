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

use crate::exec::fold::aggr::{
    MeetAccum, MeetAggrObj, NormalAggr, NormalAggrObj,
};
use crate::exec::fold::sketch::count_min::CountMinSketch;
use crate::exec::fold::sketch::hll::HyperLogLog;
use crate::exec::fold::sketch::tdigest::{DEFAULT_COMPRESSION, TDigest};
use kyzo_model::data_value_any;
use kyzo_model::program::aggregate::{AggrKind, Aggregation};
use kyzo_model::value::DataValue;

// ── HyperLogLog: approximate distinct count (normal) ─────────────────────

/// `hll(x)`: fold raw elements, return the estimated distinct count as an
/// `Int`. Normal, not meet: its output is a scalar, and two counts cannot be
/// meet-combined — exactly the `count_unique` story.
pub(crate) struct AggrHll {
    hll: HyperLogLog,
}

impl Default for AggrHll {
    fn default() -> Self {
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
        Ok(DataValue::from(self.hll.estimate().round_count()))
    }
}

/// `hll_sketch(x)`: fold raw elements, return the sketch itself as `Bytes` —
/// the builder whose output is fed to `hll_union`.
pub(crate) struct AggrHllSketch {
    hll: HyperLogLog,
}

impl Default for AggrHllSketch {
    fn default() -> Self {
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
        DataValue::Bytes(b) => HyperLogLog::from_bytes(b),
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
#[derive(Default)]
pub(crate) struct AggrHllUnion {
    acc: Option<HyperLogLog>,
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

impl Default for AggrCountMin {
    fn default() -> Self {
        Self {
            cms: CountMinSketch::default_dims(),
        }
    }
}

impl crate::exec::fold::aggr::seal::Sealed for AggrCountMin {}

impl NormalAggrObj for AggrCountMin {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        self.cms.add(value, 1);
        Ok(())
    }
    fn get(&self) -> Result<DataValue> {
        Ok(self.cms.to_value())
    }
}

// ── t-digest: quantiles (normal, buffer-and-sort) ────────────────────────

/// `tdigest(x)`: buffer raw numeric elements and, at finalization, build the
/// digest from them **sorted** — returning the digest as `Bytes`.
#[derive(Default)]
pub(crate) struct AggrTDigest {
    buf: Vec<DataValue>,
}

impl crate::exec::fold::aggr::seal::Sealed for AggrTDigest {}

impl NormalAggrObj for AggrTDigest {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        self.buf.push(value.clone());
        Ok(())
    }
    fn get(&self) -> Result<DataValue> {
        Ok(TDigest::from_values(&self.buf, DEFAULT_COMPRESSION)?.to_value())
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
            None => DataValue::Null,
        })
    }
}

// ── Registry descriptors (kind-as-data; folds via meet_op/normal_op) ─────

const AGGR_HLL: Aggregation = Aggregation {
    name: "hll",
    kind: AggrKind::Normal,
};

const AGGR_HLL_SKETCH: Aggregation = Aggregation {
    name: "hll_sketch",
    kind: AggrKind::Normal,
};

const AGGR_HLL_UNION: Aggregation = Aggregation {
    name: "hll_union",
    kind: AggrKind::Meet,
};

const AGGR_COUNT_MIN: Aggregation = Aggregation {
    name: "count_min",
    kind: AggrKind::Normal,
};

const AGGR_TDIGEST: Aggregation = Aggregation {
    name: "tdigest",
    kind: AggrKind::Normal,
};

const AGGR_QUANTILE: Aggregation = Aggregation {
    name: "quantile",
    kind: AggrKind::Normal,
};

/// Sketch-name helper. Model [`kyzo_model::program::aggregate::parse_aggr`]
/// already admits these names; this remains for internal/test dispatch.
pub(crate) fn parse_sketch_aggr(name: &str) -> Option<Aggregation> {
    Some(match name {
        "hll" => AGGR_HLL,
        "hll_sketch" => AGGR_HLL_SKETCH,
        "hll_union" => AGGR_HLL_UNION,
        "count_min" => AGGR_COUNT_MIN,
        "tdigest" => AGGR_TDIGEST,
        "quantile" => AGGR_QUANTILE,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::fold::aggr::{meet_op, normal_op};
    use crate::exec::fold::sketch::hll::HyperLogLog;

    fn val(i: i64) -> DataValue {
        DataValue::from(i)
    }

    fn run_normal(mut op: NormalAggr, vals: &[DataValue]) -> DataValue {
        for v in vals {
            op.set(v).unwrap();
        }
        op.get().unwrap()
    }

    /// `hll` end-to-end through the aggregation trait.
    #[test]
    fn hll_aggregation_estimates_distinct() {
        let mut stream = vec![];
        for i in 0..20_000i64 {
            stream.push(val(i % 5000));
        }
        let out = run_normal(normal_op(&AGGR_HLL, &[]).unwrap(), &stream);
        let est = out.get_int().unwrap();
        let rel = (est - 5000).abs() as f64 / 5000.0;
        assert!(rel < 0.05, "hll estimate {est} off by {rel}");
    }

    /// `hll_union` meet form obeys the semilattice laws over sketch-bytes.
    #[test]
    fn hll_union_meet_obeys_semilattice_laws() {
        let op = meet_op(&AGGR_HLL_UNION).expect("hll_union is a meet");

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

        let meet = |a: &MeetAccum, b: &MeetAccum| {
            let mut acc = a.clone();
            op.update(&mut acc, b).unwrap();
            acc
        };

        let mut acc = x.clone();
        assert!(
            !op.update(&mut acc, &x).unwrap(),
            "meet(x,x) reported change"
        );
        assert_eq!(acc, x, "meet(x,x) altered x");

        let mut id = op.init_val();
        op.update(&mut id, &x).unwrap();
        assert_eq!(id, x, "meet(init,x) != x");

        assert_eq!(meet(&meet(&x, &y), &z), meet(&x, &meet(&y, &z)), "assoc");
        assert_eq!(meet(&x, &y), meet(&y, &x), "commutative");
    }

    /// Meet form and normal form of `hll_union` agree.
    #[test]
    fn hll_union_meet_and_normal_agree() {
        let mk = |lo: i64, hi: i64| {
            let mut h = HyperLogLog::default_precision();
            for i in lo..hi {
                h.add(&val(i));
            }
            h.to_value()
        };
        let sketches = [mk(0, 2000), mk(1500, 4000), mk(3000, 5000)];

        let meet = meet_op(&AGGR_HLL_UNION).unwrap();
        let mut acc = meet.init_val();
        for s in &sketches {
            meet.update(&mut acc, &MeetAccum::Value(s.clone())).unwrap();
        }

        let normal_out = run_normal(normal_op(&AGGR_HLL_UNION, &[]).unwrap(), &sketches);
        assert_eq!(acc.to_value(), normal_out, "meet and normal folds disagree");
    }

    /// Only `hll_union` is a meet among the sketch family.
    #[test]
    fn only_hll_union_is_meet() {
        assert!(AGGR_HLL_UNION.is_meet());
        assert!(!AGGR_HLL.is_meet());
        assert!(!AGGR_COUNT_MIN.is_meet());
        assert!(!AGGR_TDIGEST.is_meet());
        assert!(!AGGR_QUANTILE.is_meet());
        assert!(meet_op(&AGGR_COUNT_MIN).is_none());
    }

    /// `count_min` builds a queryable frequency table through the trait.
    #[test]
    fn count_min_aggregation_builds_table() {
        let mut stream = vec![];
        for i in 0..1000i64 {
            for _ in 0..(i % 7 + 1) {
                stream.push(val(i));
            }
        }
        let out = run_normal(normal_op(&AGGR_COUNT_MIN, &[]).unwrap(), &stream);
        let DataValue::Bytes(bytes) = out else {
            panic!("count_min should return bytes")
        };
        let cms = CountMinSketch::from_bytes(&bytes).unwrap();
        assert!(cms.estimate(&val(6)) >= 7);
    }

    /// `quantile(x, q)` returns the estimated value at q, order-independently.
    #[test]
    fn quantile_aggregation_is_order_independent() {
        let asc: Vec<DataValue> = (0..10_000).map(|i| DataValue::from(i as f64)).collect();
        let desc: Vec<DataValue> = (0..10_000)
            .rev()
            .map(|i| DataValue::from(i as f64))
            .collect();

        let q_asc = run_normal(quantile_factory(&[DataValue::from(0.9f64)]).unwrap(), &asc);
        let q_desc = run_normal(quantile_factory(&[DataValue::from(0.9f64)]).unwrap(), &desc);
        assert_eq!(q_asc, q_desc, "quantile depended on input order");

        let est = q_asc.get_float().unwrap();
        assert!((est - 9000.0).abs() < 200.0, "p90 estimate {est} off");
    }

    /// `quantile` rejects an out-of-range level at construction.
    #[test]
    fn quantile_rejects_bad_level() {
        assert!(quantile_factory(&[DataValue::from(1.5f64)]).is_err());
        assert!(quantile_factory(&[]).is_err());
    }

    /// The dispatch table resolves exactly the sketch names.
    #[test]
    fn dispatch_resolves_names() {
        for name in [
            "hll",
            "hll_sketch",
            "hll_union",
            "count_min",
            "tdigest",
            "quantile",
        ] {
            assert_eq!(parse_sketch_aggr(name).unwrap().name, name);
        }
        assert!(parse_sketch_aggr("not_a_sketch").is_none());
    }
}
