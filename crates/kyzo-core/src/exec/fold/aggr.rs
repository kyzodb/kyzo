/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): fold half re-homed from condemned `data/aggr.rs`. Aggregation
 * names + AggrKind (Meet|Normal data) live in
 * `kyzo_model::program::aggregate`; this module owns fold objects, factories,
 * NumAccum, and name→op lookup. `choice_rand` does not migrate (unseeded rng).
 */

//! Aggregation folds: the implementation half.
//!
//! - A **meet aggregation** is a semilattice fold — idempotent, associative,
//!   with [`MeetAggrObj::init_val`] as identity — safe *inside* recursion.
//! - A **normal aggregation** is an ordinary fold, finalized only after the
//!   fixpoint.
//!
//! Name and kind authority live in the model (`parse_aggr`). This module
//! binds names to fold factories via [`meet_op`] / [`normal_op`].

use std::collections::{BTreeMap, BTreeSet};

use miette::{Result, bail, ensure, miette};

use kyzo_model::data_value_any;
use kyzo_model::program::aggregate::{AggrKind, Aggregation};
use kyzo_model::value::{DataValue, Num, NumRepr};

use crate::exec::fold::sketch::aggr::{
    AggrCountMin, AggrHll, AggrHllSketch, AggrHllUnion, AggrQuantile, AggrTDigest, MeetAggrHllUnion,
};

/// Private supertrait seal for aggregation op traits — crate visibility
/// alone is not the seal.
pub(crate) mod seal {
    pub trait Sealed {}
}

/// An ordinary fold over rows: `set` feeds one row's value, `get` produces
/// the final answer. Runs only after the fixpoint, seeing each row once.
pub(crate) trait NormalAggrObj: Send + Sync + seal::Sealed {
    fn set(&mut self, value: &DataValue) -> Result<()>;
    fn get(&self) -> Result<DataValue>;
}

/// Running state of a meet fold. `Empty` is the lattice identity when it
/// has no finite [`DataValue`] spelling; `Value` holds the running result —
/// including [`DataValue::Null`] when Null is real data, never when it means
/// "empty."
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MeetAccum {
    Empty,
    Value(DataValue),
}

impl MeetAccum {
    pub(crate) fn from_derived(v: DataValue) -> Self {
        Self::Value(v)
    }

    pub(crate) fn to_value(&self) -> DataValue {
        match self {
            MeetAccum::Empty => DataValue::Null,
            MeetAccum::Value(v) => v.clone(),
        }
    }
}

/// A semilattice fold, safe inside recursion.
pub(crate) trait MeetAggrObj: Send + Sync + seal::Sealed {
    fn init_val(&self) -> MeetAccum;
    fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool>;
}

/// Sealed exhaustively-matched normal aggregation: the dispatch surface.
pub(crate) enum NormalAggr {
    And(AggrAnd),
    Or(AggrOr),
    Unique(AggrUnique),
    GroupCount(AggrGroupCount),
    CountUnique(AggrCountUnique),
    Union(AggrUnion),
    Intersection(AggrIntersection),
    Collect(AggrCollect),
    Count(AggrCount),
    Variance(AggrVariance),
    StdDev(AggrStdDev),
    Mean(AggrMean),
    Sum(AggrSum),
    Product(AggrProduct),
    Min(AggrMin),
    Max(AggrMax),
    LatestBy(AggrLatestBy),
    SmallestBy(AggrSmallestBy),
    MinCost(AggrMinCost),
    Shortest(AggrShortest),
    Choice(AggrChoice),
    BitAnd(AggrBitAnd),
    BitOr(AggrBitOr),
    BitXor(AggrBitXor),
    Hll(AggrHll),
    HllSketch(AggrHllSketch),
    HllUnion(AggrHllUnion),
    CountMin(AggrCountMin),
    TDigest(AggrTDigest),
    Quantile(AggrQuantile),
}

impl NormalAggr {
    pub(crate) fn set(&mut self, value: &DataValue) -> Result<()> {
        match self {
            Self::And(a) => a.set(value),
            Self::Or(a) => a.set(value),
            Self::Unique(a) => a.set(value),
            Self::GroupCount(a) => a.set(value),
            Self::CountUnique(a) => a.set(value),
            Self::Union(a) => a.set(value),
            Self::Intersection(a) => a.set(value),
            Self::Collect(a) => a.set(value),
            Self::Count(a) => a.set(value),
            Self::Variance(a) => a.set(value),
            Self::StdDev(a) => a.set(value),
            Self::Mean(a) => a.set(value),
            Self::Sum(a) => a.set(value),
            Self::Product(a) => a.set(value),
            Self::Min(a) => a.set(value),
            Self::Max(a) => a.set(value),
            Self::LatestBy(a) => a.set(value),
            Self::SmallestBy(a) => a.set(value),
            Self::MinCost(a) => a.set(value),
            Self::Shortest(a) => a.set(value),
            Self::Choice(a) => a.set(value),
            Self::BitAnd(a) => a.set(value),
            Self::BitOr(a) => a.set(value),
            Self::BitXor(a) => a.set(value),
            Self::Hll(a) => a.set(value),
            Self::HllSketch(a) => a.set(value),
            Self::HllUnion(a) => a.set(value),
            Self::CountMin(a) => a.set(value),
            Self::TDigest(a) => a.set(value),
            Self::Quantile(a) => a.set(value),
        }
    }

    pub(crate) fn get(&self) -> Result<DataValue> {
        match self {
            Self::And(a) => a.get(),
            Self::Or(a) => a.get(),
            Self::Unique(a) => a.get(),
            Self::GroupCount(a) => a.get(),
            Self::CountUnique(a) => a.get(),
            Self::Union(a) => a.get(),
            Self::Intersection(a) => a.get(),
            Self::Collect(a) => a.get(),
            Self::Count(a) => a.get(),
            Self::Variance(a) => a.get(),
            Self::StdDev(a) => a.get(),
            Self::Mean(a) => a.get(),
            Self::Sum(a) => a.get(),
            Self::Product(a) => a.get(),
            Self::Min(a) => a.get(),
            Self::Max(a) => a.get(),
            Self::LatestBy(a) => a.get(),
            Self::SmallestBy(a) => a.get(),
            Self::MinCost(a) => a.get(),
            Self::Shortest(a) => a.get(),
            Self::Choice(a) => a.get(),
            Self::BitAnd(a) => a.get(),
            Self::BitOr(a) => a.get(),
            Self::BitXor(a) => a.get(),
            Self::Hll(a) => a.get(),
            Self::HllSketch(a) => a.get(),
            Self::HllUnion(a) => a.get(),
            Self::CountMin(a) => a.get(),
            Self::TDigest(a) => a.get(),
            Self::Quantile(a) => a.get(),
        }
    }
}

/// Sealed exhaustively-matched meet aggregation: the dispatch surface.
pub(crate) enum MeetAggr {
    And(MeetAggrAnd),
    Or(MeetAggrOr),
    Union(MeetAggrUnion),
    Intersection(MeetAggrIntersection),
    Min(MeetAggrMin),
    Max(MeetAggrMax),
    MinCost(MeetAggrMinCost),
    Shortest(MeetAggrShortest),
    Choice(MeetAggrChoice),
    BitAnd(MeetAggrBitAnd),
    BitOr(MeetAggrBitOr),
    HllUnion(MeetAggrHllUnion),
}

impl MeetAggr {
    pub(crate) fn init_val(&self) -> MeetAccum {
        match self {
            Self::And(a) => a.init_val(),
            Self::Or(a) => a.init_val(),
            Self::Union(a) => a.init_val(),
            Self::Intersection(a) => a.init_val(),
            Self::Min(a) => a.init_val(),
            Self::Max(a) => a.init_val(),
            Self::MinCost(a) => a.init_val(),
            Self::Shortest(a) => a.init_val(),
            Self::Choice(a) => a.init_val(),
            Self::BitAnd(a) => a.init_val(),
            Self::BitOr(a) => a.init_val(),
            Self::HllUnion(a) => a.init_val(),
        }
    }

    pub(crate) fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool> {
        match self {
            Self::And(a) => a.update(left, right),
            Self::Or(a) => a.update(left, right),
            Self::Union(a) => a.update(left, right),
            Self::Intersection(a) => a.update(left, right),
            Self::Min(a) => a.update(left, right),
            Self::Max(a) => a.update(left, right),
            Self::MinCost(a) => a.update(left, right),
            Self::Shortest(a) => a.update(left, right),
            Self::Choice(a) => a.update(left, right),
            Self::BitAnd(a) => a.update(left, right),
            Self::BitOr(a) => a.update(left, right),
            Self::HllUnion(a) => a.update(left, right),
        }
    }
}

/// Declares a meet aggregation descriptor (kind-as-data). Fold binding is
/// via [`meet_op`] / [`normal_op`]. The `$meet_var` / `$normal` tokens keep
/// the historical macro signature; factories live in the match arms.
#[allow(unused_macros)]
macro_rules! meet_aggr {
    ($aggr:ident, $name:literal, $_meet_var:ident, $_meet:expr, $_norm_var:ident, $_normal:ty) => {
        #[allow(dead_code)] // mid-wiring seat; lands with host doors (epic #348)
        const $aggr: Aggregation = Aggregation {
            name: $name,
            kind: AggrKind::Meet,
        };
    };
}

/// Declares a normal-only aggregation descriptor (kind-as-data).
#[allow(unused_macros)]
macro_rules! normal_aggr {
    ($aggr:ident, $name:literal, $_norm_var:ident, $_normal:ty) => {
        #[allow(dead_code)] // mid-wiring seat; lands with host doors (epic #348)
        const $aggr: Aggregation = Aggregation {
            name: $name,
            kind: AggrKind::Normal,
        };
    };
}

/// The meet form for a model [`Aggregation`], if it is a meet.
pub(crate) fn meet_op(a: &Aggregation) -> Option<MeetAggr> {
    match a.name {
        "and" => Some(MeetAggr::And(MeetAggrAnd)),
        "or" => Some(MeetAggr::Or(MeetAggrOr)),
        "union" => Some(MeetAggr::Union(MeetAggrUnion)),
        "intersection" => Some(MeetAggr::Intersection(MeetAggrIntersection)),
        "min" => Some(MeetAggr::Min(MeetAggrMin)),
        "max" => Some(MeetAggr::Max(MeetAggrMax)),
        "min_cost" => Some(MeetAggr::MinCost(MeetAggrMinCost)),
        "shortest" => Some(MeetAggr::Shortest(MeetAggrShortest)),
        "choice" => Some(MeetAggr::Choice(MeetAggrChoice)),
        "bit_and" => Some(MeetAggr::BitAnd(MeetAggrBitAnd)),
        "bit_or" => Some(MeetAggr::BitOr(MeetAggrBitOr)),
        "hll_union" => Some(MeetAggr::HllUnion(MeetAggrHllUnion)),
        _ => None,
    }
}

/// A fresh normal fold for a model [`Aggregation`] (every aggregation,
/// meet included, has one).
pub(crate) fn normal_op(a: &Aggregation, args: &[DataValue]) -> Result<NormalAggr> {
    match a.name {
        "and" => Ok(NormalAggr::And(AggrAnd::default())),
        "or" => Ok(NormalAggr::Or(AggrOr::default())),
        "unique" => Ok(NormalAggr::Unique(AggrUnique::default())),
        "group_count" => Ok(NormalAggr::GroupCount(AggrGroupCount::default())),
        "count_unique" => Ok(NormalAggr::CountUnique(AggrCountUnique::default())),
        "union" => Ok(NormalAggr::Union(AggrUnion::default())),
        "intersection" => Ok(NormalAggr::Intersection(AggrIntersection::default())),
        "collect" => collect_factory(args),
        "count" => Ok(NormalAggr::Count(AggrCount::default())),
        "variance" => Ok(NormalAggr::Variance(AggrVariance::default())),
        "std_dev" => Ok(NormalAggr::StdDev(AggrStdDev::default())),
        "mean" => Ok(NormalAggr::Mean(AggrMean::default())),
        "sum" => Ok(NormalAggr::Sum(AggrSum::default())),
        "product" => Ok(NormalAggr::Product(AggrProduct::default())),
        "min" => Ok(NormalAggr::Min(AggrMin::default())),
        "max" => Ok(NormalAggr::Max(AggrMax::default())),
        "latest_by" => Ok(NormalAggr::LatestBy(AggrLatestBy::default())),
        "smallest_by" => Ok(NormalAggr::SmallestBy(AggrSmallestBy::default())),
        "min_cost" => Ok(NormalAggr::MinCost(AggrMinCost::default())),
        "shortest" => Ok(NormalAggr::Shortest(AggrShortest::default())),
        "choice" => Ok(NormalAggr::Choice(AggrChoice::default())),
        "bit_and" => Ok(NormalAggr::BitAnd(AggrBitAnd::default())),
        "bit_or" => Ok(NormalAggr::BitOr(AggrBitOr::default())),
        "bit_xor" => Ok(NormalAggr::BitXor(AggrBitXor::default())),
        "hll" => Ok(NormalAggr::Hll(AggrHll::default())),
        "hll_sketch" => Ok(NormalAggr::HllSketch(AggrHllSketch::default())),
        "hll_union" => Ok(NormalAggr::HllUnion(AggrHllUnion::default())),
        "count_min" => Ok(NormalAggr::CountMin(AggrCountMin::default())),
        "tdigest" => Ok(NormalAggr::TDigest(AggrTDigest::default())),
        "quantile" => crate::exec::fold::sketch::aggr::quantile_factory(args),
        other => bail!("no fold factory for aggregation '{other}'"),
    }
}

// ── Fold bodies (from condemned data/aggr.rs) ────────────────────────────

meet_aggr!(AGGR_AND, "and", And, MeetAggrAnd, And, AggrAnd);

/// Conjunction as a fold: `true` until any row is `false`.
pub(crate) struct AggrAnd {
    accum: bool,
}

impl Default for AggrAnd {
    fn default() -> Self {
        Self { accum: true }
    }
}

impl seal::Sealed for AggrAnd {}

impl NormalAggrObj for AggrAnd {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::Bool(v) => self.accum &= *v,
            v @ (data_value_any!()) => bail!("cannot compute 'and' for {:?}", v),
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::from(self.accum))
    }
}

/// Conjunction as a meet: the two-point lattice `true > false` under `&`.
pub(crate) struct MeetAggrAnd;

impl seal::Sealed for MeetAggrAnd {}

impl MeetAggrObj for MeetAggrAnd {
    fn init_val(&self) -> MeetAccum {
        MeetAccum::Value(DataValue::from(true))
    }

    fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool> {
        if matches!(right, MeetAccum::Empty) {
            return Ok(false);
        }
        if matches!(left, MeetAccum::Empty) {
            *left = right.clone();
            return Ok(true);
        }
        match (left, right) {
            (MeetAccum::Value(DataValue::Bool(l)), MeetAccum::Value(DataValue::Bool(r))) => {
                let old = *l;
                *l &= *r;
                Ok(old != *l)
            }
            (u, v) => bail!("cannot compute 'and' for {:?} and {:?}", u, v),
        }
    }
}

meet_aggr!(AGGR_OR, "or", Or, MeetAggrOr, Or, AggrOr);

/// Disjunction as a fold: `false` until any row is `true`.
#[derive(Default)]
pub(crate) struct AggrOr {
    accum: bool,
}

impl seal::Sealed for AggrOr {}

impl NormalAggrObj for AggrOr {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::Bool(v) => self.accum |= *v,
            v @ (data_value_any!()) => bail!("cannot compute 'or' for {:?}", v),
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::from(self.accum))
    }
}

/// Disjunction as a meet: the two-point lattice `false < true` under `|`.
pub(crate) struct MeetAggrOr;

impl seal::Sealed for MeetAggrOr {}

impl MeetAggrObj for MeetAggrOr {
    fn init_val(&self) -> MeetAccum {
        MeetAccum::Value(DataValue::from(false))
    }

    fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool> {
        if matches!(right, MeetAccum::Empty) {
            return Ok(false);
        }
        if matches!(left, MeetAccum::Empty) {
            *left = right.clone();
            return Ok(true);
        }
        match (left, right) {
            (MeetAccum::Value(DataValue::Bool(l)), MeetAccum::Value(DataValue::Bool(r))) => {
                let old = *l;
                *l |= *r;
                Ok(old != *l)
            }
            (u, v) => bail!("cannot compute 'or' for {:?} and {:?}", u, v),
        }
    }
}

normal_aggr!(AGGR_UNIQUE, "unique", Unique, AggrUnique);

/// The distinct values seen, as a sorted list.
#[derive(Default)]
pub(crate) struct AggrUnique {
    accum: BTreeSet<DataValue>,
}

impl seal::Sealed for AggrUnique {}

impl NormalAggrObj for AggrUnique {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        self.accum.insert(value.clone());
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::List(self.accum.iter().cloned().collect()))
    }
}

normal_aggr!(AGGR_GROUP_COUNT, "group_count", GroupCount, AggrGroupCount);

/// Each distinct value with its multiplicity, as a sorted list of pairs.
#[derive(Default)]
pub(crate) struct AggrGroupCount {
    accum: BTreeMap<DataValue, i64>,
}

impl seal::Sealed for AggrGroupCount {}

impl NormalAggrObj for AggrGroupCount {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        let entry = self.accum.entry(value.clone()).or_default();
        *entry += 1;
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::List(
            self.accum
                .iter()
                .map(|(k, v)| DataValue::List(vec![k.clone(), DataValue::from(*v)]))
                .collect(),
        ))
    }
}

normal_aggr!(
    AGGR_COUNT_UNIQUE,
    "count_unique",
    CountUnique,
    AggrCountUnique
);

/// How many distinct values were seen.
#[derive(Default)]
pub(crate) struct AggrCountUnique {
    count: i64,
    accum: BTreeSet<DataValue>,
}

impl seal::Sealed for AggrCountUnique {}

impl NormalAggrObj for AggrCountUnique {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        if !self.accum.contains(value) {
            self.accum.insert(value.clone());
            self.count += 1;
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::from(self.count))
    }
}

meet_aggr!(AGGR_UNION, "union", Union, MeetAggrUnion, Union, AggrUnion);

/// Set union of list-valued rows, as a fold.
#[derive(Default)]
pub(crate) struct AggrUnion {
    accum: BTreeSet<DataValue>,
}

impl seal::Sealed for AggrUnion {}

impl NormalAggrObj for AggrUnion {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::List(v) => self.accum.extend(v.iter().cloned()),
            v @ (data_value_any!()) => bail!("cannot compute 'union' for value {:?}", v),
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::List(self.accum.iter().cloned().collect()))
    }
}

/// Set union as a meet: the lattice of sets under `∪`, identity `∅`.
pub(crate) struct MeetAggrUnion;

impl seal::Sealed for MeetAggrUnion {}

impl MeetAggrObj for MeetAggrUnion {
    fn init_val(&self) -> MeetAccum {
        MeetAccum::Value(DataValue::Set(BTreeSet::new()))
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
            MeetAccum::Value(left) => match right {
                MeetAccum::Empty => Ok(false),
                MeetAccum::Value(right) => {
                    if let DataValue::List(l) = left {
                        let set: BTreeSet<_> = l.iter().cloned().collect();
                        *left = DataValue::Set(set);
                    }
                    Ok(match (left, right) {
                        (DataValue::Set(l), DataValue::Set(s)) => {
                            let mut inserted = false;
                            for v in s.iter() {
                                inserted |= l.insert(v.clone());
                            }
                            inserted
                        }
                        (DataValue::Set(l), DataValue::List(s)) => {
                            let mut inserted = false;
                            for v in s.iter() {
                                inserted |= l.insert(v.clone());
                            }
                            inserted
                        }
                        (_, v) => bail!("cannot compute 'union' for value {:?}", v),
                    })
                }
            },
        }
    }
}

meet_aggr!(
    AGGR_INTERSECTION,
    "intersection",
    Intersection,
    MeetAggrIntersection,
    Intersection,
    AggrIntersection
);

/// Set intersection of list-valued rows, as a fold.
#[derive(Default)]
pub(crate) struct AggrIntersection {
    accum: Option<BTreeSet<DataValue>>,
}

impl seal::Sealed for AggrIntersection {}

impl NormalAggrObj for AggrIntersection {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::List(v) => {
                if let Some(accum) = &mut self.accum {
                    let new = accum
                        .intersection(&v.iter().cloned().collect())
                        .cloned()
                        .collect();
                    *accum = new;
                } else {
                    self.accum = Some(v.iter().cloned().collect())
                }
            }
            v @ (data_value_any!()) => bail!("cannot compute 'intersection' for value {:?}", v),
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        match &self.accum {
            None => Ok(DataValue::List(vec![])),
            Some(l) => Ok(DataValue::List(l.iter().cloned().collect())),
        }
    }
}

/// Set intersection as a meet. Identity is [`MeetAccum::Empty`].
pub(crate) struct MeetAggrIntersection;

impl seal::Sealed for MeetAggrIntersection {}

impl MeetAggrObj for MeetAggrIntersection {
    fn init_val(&self) -> MeetAccum {
        MeetAccum::Empty
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
            MeetAccum::Value(left) => match right {
                MeetAccum::Empty => Ok(false),
                MeetAccum::Value(right) => {
                    if let DataValue::List(l) = left {
                        let set: BTreeSet<_> = l.iter().cloned().collect();
                        *left = DataValue::Set(set);
                    }
                    Ok(match (left, right) {
                        (DataValue::Set(l), DataValue::Set(s)) => {
                            let old_len = l.len();
                            let new_set = l.intersection(s).cloned().collect::<BTreeSet<_>>();
                            if old_len == new_set.len() {
                                false
                            } else {
                                *l = new_set;
                                true
                            }
                        }
                        (DataValue::Set(l), DataValue::List(s)) => {
                            let old_len = l.len();
                            let s: BTreeSet<_> = s.iter().cloned().collect();
                            let new_set = l.intersection(&s).cloned().collect::<BTreeSet<_>>();
                            if old_len == new_set.len() {
                                false
                            } else {
                                *l = new_set;
                                true
                            }
                        }
                        (_, v) => bail!("cannot compute 'intersection' for value {:?}", v),
                    })
                }
            },
        }
    }
}

/// `collect` takes an optional positive limit as its compile-time argument.
fn collect_factory(args: &[DataValue]) -> Result<NormalAggr> {
    Ok(match args.first() {
        None => NormalAggr::Collect(AggrCollect::default()),
        Some(arg) => {
            let limit = arg.get_int().ok_or_else(|| {
                miette!(
                    "the argument to 'collect' must be an integer, got {:?}",
                    arg
                )
            })?;
            ensure!(
                limit > 0,
                "argument to 'collect' must be positive, got {}",
                limit
            );
            NormalAggr::Collect(AggrCollect::new(limit as usize))
        }
    })
}

#[allow(dead_code)] // mid-wiring seat; lands with host doors (epic #348)
const AGGR_COLLECT: Aggregation = Aggregation {
    name: "collect",
    kind: AggrKind::Normal,
};

/// The values in arrival order, as a list, optionally truncated to a limit.
#[derive(Default)]
pub(crate) struct AggrCollect {
    limit: Option<usize>,
    accum: Vec<DataValue>,
}

impl AggrCollect {
    fn new(limit: usize) -> Self {
        Self {
            limit: Some(limit),
            accum: vec![],
        }
    }
}

impl seal::Sealed for AggrCollect {}

impl NormalAggrObj for AggrCollect {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        if let Some(limit) = self.limit
            && self.accum.len() >= limit
        {
            return Ok(());
        }
        self.accum.push(value.clone());
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::List(self.accum.clone()))
    }
}

normal_aggr!(AGGR_COUNT, "count", Count, AggrCount);

/// How many rows were seen (including nulls).
#[derive(Default)]
pub(crate) struct AggrCount {
    count: i64,
}

impl seal::Sealed for AggrCount {}

impl NormalAggrObj for AggrCount {
    fn set(&mut self, _value: &DataValue) -> Result<()> {
        self.count += 1;
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::from(self.count))
    }
}

normal_aggr!(AGGR_VARIANCE, "variance", Variance, AggrVariance);

/// Sample variance (Bessel-corrected), accumulated in floating point.
#[derive(Default)]
pub(crate) struct AggrVariance {
    count: i64,
    sum: f64,
    sum_sq: f64,
}

impl seal::Sealed for AggrVariance {}

impl NormalAggrObj for AggrVariance {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::Num(n) => {
                let f = n.to_f64();
                self.sum += f;
                self.sum_sq += f * f;
                self.count += 1;
            }
            v @ (data_value_any!()) => {
                bail!("cannot compute 'variance': encountered value {:?}", v)
            }
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        let ct = self.count as f64;
        Ok(DataValue::from(
            (self.sum_sq - self.sum * self.sum / ct) / (ct - 1.),
        ))
    }
}

normal_aggr!(AGGR_STD_DEV, "std_dev", StdDev, AggrStdDev);

/// Sample standard deviation (sqrt of sample variance).
#[derive(Default)]
pub(crate) struct AggrStdDev {
    count: i64,
    sum: f64,
    sum_sq: f64,
}

impl seal::Sealed for AggrStdDev {}

impl NormalAggrObj for AggrStdDev {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::Num(n) => {
                let f = n.to_f64();
                self.sum += f;
                self.sum_sq += f * f;
                self.count += 1;
            }
            v @ (data_value_any!()) => {
                bail!("cannot compute 'std_dev': encountered value {:?}", v)
            }
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        let ct = self.count as f64;
        let var = (self.sum_sq - self.sum * self.sum / ct) / (ct - 1.);
        Ok(DataValue::from(var.sqrt()))
    }
}

normal_aggr!(AGGR_MEAN, "mean", Mean, AggrMean);

/// The arithmetic mean, accumulated in floating point.
#[derive(Default)]
pub(crate) struct AggrMean {
    count: i64,
    sum: f64,
}

impl seal::Sealed for AggrMean {}

impl NormalAggrObj for AggrMean {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::Num(n) => {
                self.sum += n.to_f64();
                self.count += 1;
            }
            v @ (data_value_any!()) => bail!("cannot compute 'mean': encountered value {:?}", v),
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::from(self.sum / (self.count as f64)))
    }
}

normal_aggr!(AGGR_SUM, "sum", Sum, AggrSum);

/// Exact-while-possible numeric accumulator for `sum` and `product`.
#[derive(Clone, Copy)]
pub(crate) enum NumAccum {
    Int(i128),
    Float(f64),
}

impl NumAccum {
    fn fold(
        self,
        n: &Num,
        int_op: fn(i128, i128) -> Option<i128>,
        float_op: fn(f64, f64) -> f64,
    ) -> Self {
        match (self, n.repr()) {
            (NumAccum::Int(acc), NumRepr::Int(i)) => match int_op(acc, i as i128) {
                Some(acc) => NumAccum::Int(acc),
                None => NumAccum::Float(float_op(acc as f64, i as f64)),
            },
            (NumAccum::Int(acc), NumRepr::Float(f)) => NumAccum::Float(float_op(acc as f64, f)),
            (NumAccum::Float(acc), _) => NumAccum::Float(float_op(acc, n.to_f64())),
        }
    }

    fn finish(self) -> DataValue {
        match self {
            NumAccum::Int(acc) => match i64::try_from(acc) {
                Ok(i) => DataValue::from(i),
                Err(_) => DataValue::from(acc as f64),
            },
            NumAccum::Float(f) => DataValue::from(f),
        }
    }
}

/// The sum, accumulated exactly via [`NumAccum`].
pub(crate) struct AggrSum {
    sum: NumAccum,
}

impl Default for AggrSum {
    fn default() -> Self {
        Self {
            sum: NumAccum::Int(0),
        }
    }
}

impl seal::Sealed for AggrSum {}

impl NormalAggrObj for AggrSum {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::Num(n) => self.sum = self.sum.fold(n, i128::checked_add, |a, b| a + b),
            v @ (data_value_any!()) => bail!("cannot compute 'sum': encountered value {:?}", v),
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(self.sum.finish())
    }
}

normal_aggr!(AGGR_PRODUCT, "product", Product, AggrProduct);

/// The product, accumulated exactly via [`NumAccum`].
pub(crate) struct AggrProduct {
    prod: NumAccum,
}

impl Default for AggrProduct {
    fn default() -> Self {
        Self {
            prod: NumAccum::Int(1),
        }
    }
}

impl seal::Sealed for AggrProduct {}

impl NormalAggrObj for AggrProduct {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::Num(n) => self.prod = self.prod.fold(n, i128::checked_mul, |a, b| a * b),
            v @ (data_value_any!()) => {
                bail!("cannot compute 'product': encountered value {:?}", v)
            }
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(self.prod.finish())
    }
}

meet_aggr!(AGGR_MIN, "min", Min, MeetAggrMin, Min, AggrMin);

/// The numerical minimum, ignoring nulls; `Null` *result* when no row had
/// a number. In-state absence is [`Option::None`], never a Null sentinel.
#[derive(Default)]
pub(crate) struct AggrMin {
    found: Option<DataValue>,
}

impl seal::Sealed for AggrMin {}

impl NormalAggrObj for AggrMin {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        if *value == DataValue::Null {
            return Ok(());
        }
        match &self.found {
            None => {
                self.found = Some(value.clone());
                return Ok(());
            }
            Some(found) => {
                let (found_n, new) = match (found, value) {
                    (DataValue::Num(l), DataValue::Num(r)) => (*l, *r),
                    _ => bail!("'min' applied to non-numerical values"),
                };
                if new < found_n {
                    self.found = Some(value.clone());
                }
            }
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(self.found.clone().unwrap_or(DataValue::Null))
    }
}

/// Least numeric value as a meet; Null is never a candidate.
pub(crate) struct MeetAggrMin;

impl seal::Sealed for MeetAggrMin {}

impl MeetAggrObj for MeetAggrMin {
    fn init_val(&self) -> MeetAccum {
        MeetAccum::Empty
    }

    fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool> {
        let MeetAccum::Value(right_v) = right else {
            return Ok(false);
        };
        if *right_v == DataValue::Null {
            return Ok(false);
        }
        match left {
            MeetAccum::Empty | MeetAccum::Value(DataValue::Null) => {
                *left = right.clone();
                Ok(true)
            }
            MeetAccum::Value(left_v) => {
                let (l, r) = match (&*left_v, right_v) {
                    (DataValue::Num(l), DataValue::Num(r)) => (*l, *r),
                    _ => bail!("'min' applied to non-numerical values"),
                };
                Ok(if r < l {
                    *left_v = right_v.clone();
                    true
                } else {
                    false
                })
            }
        }
    }
}

meet_aggr!(AGGR_MAX, "max", Max, MeetAggrMax, Max, AggrMax);

/// The greatest numeric value, via exact [`Num`] order. Nulls are skipped.
#[derive(Default)]
pub(crate) struct AggrMax {
    found: Option<DataValue>,
}

impl seal::Sealed for AggrMax {}

impl NormalAggrObj for AggrMax {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        if *value == DataValue::Null {
            return Ok(());
        }
        match &self.found {
            None => {
                self.found = Some(value.clone());
                return Ok(());
            }
            Some(found) => {
                let (found_n, new) = match (found, value) {
                    (DataValue::Num(l), DataValue::Num(r)) => (*l, *r),
                    _ => bail!("'max' applied to non-numerical values"),
                };
                if new > found_n {
                    self.found = Some(value.clone());
                }
            }
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(self.found.clone().unwrap_or(DataValue::Null))
    }
}

/// Greatest numeric value as a meet; Null is never a candidate.
pub(crate) struct MeetAggrMax;

impl seal::Sealed for MeetAggrMax {}

impl MeetAggrObj for MeetAggrMax {
    fn init_val(&self) -> MeetAccum {
        MeetAccum::Empty
    }

    fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool> {
        let MeetAccum::Value(right_v) = right else {
            return Ok(false);
        };
        if *right_v == DataValue::Null {
            return Ok(false);
        }
        match left {
            MeetAccum::Empty | MeetAccum::Value(DataValue::Null) => {
                *left = right.clone();
                Ok(true)
            }
            MeetAccum::Value(left_v) => {
                let (l, r) = match (&*left_v, right_v) {
                    (DataValue::Num(l), DataValue::Num(r)) => (*l, *r),
                    _ => bail!("'max' applied to non-numerical values"),
                };
                Ok(if r > l {
                    *left_v = right_v.clone();
                    true
                } else {
                    false
                })
            }
        }
    }
}

normal_aggr!(AGGR_LATEST_BY, "latest_by", LatestBy, AggrLatestBy);

/// Of `[payload, cost]` pairs, the payload whose cost sorts greatest.
#[derive(Default)]
pub(crate) struct AggrLatestBy {
    found: Option<DataValue>,
    cost: Option<DataValue>,
}

impl seal::Sealed for AggrLatestBy {}

impl NormalAggrObj for AggrLatestBy {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::List(l) => {
                let [payload, cost] = &l[..] else {
                    bail!("'latest_by' requires a list of exactly two items as argument")
                };
                let take = match &self.cost {
                    None => true,
                    Some(prev) => *cost > *prev,
                };
                if take {
                    self.cost = Some(cost.clone());
                    self.found = Some(payload.clone());
                }
                Ok(())
            }
            v @ (data_value_any!()) => bail!("cannot compute 'latest_by' on {:?}", v),
        }
    }

    fn get(&self) -> Result<DataValue> {
        Ok(self.found.clone().unwrap_or(DataValue::Null))
    }
}

normal_aggr!(AGGR_SMALLEST_BY, "smallest_by", SmallestBy, AggrSmallestBy);

/// Of `[payload, cost]` pairs, the payload whose cost sorts least.
#[derive(Default)]
pub(crate) struct AggrSmallestBy {
    found: Option<DataValue>,
    cost: Option<DataValue>,
}

impl seal::Sealed for AggrSmallestBy {}

impl NormalAggrObj for AggrSmallestBy {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::List(l) => {
                let [payload, cost] = &l[..] else {
                    bail!("'smallest_by' requires a list of exactly two items as argument")
                };
                let take = match &self.cost {
                    None => true,
                    Some(prev) => *cost < *prev,
                };
                if take {
                    self.cost = Some(cost.clone());
                    self.found = Some(payload.clone());
                }
                Ok(())
            }
            v @ (data_value_any!()) => bail!("cannot compute 'smallest_by' on {:?}", v),
        }
    }

    fn get(&self) -> Result<DataValue> {
        Ok(self.found.clone().unwrap_or(DataValue::Null))
    }
}

meet_aggr!(
    AGGR_MIN_COST,
    "min_cost",
    MinCost,
    MeetAggrMinCost,
    MinCost,
    AggrMinCost
);

/// Of `[payload, cost]` pairs, the pair with the numerically least cost.
#[derive(Default)]
pub(crate) struct AggrMinCost {
    found: Option<DataValue>,
    cost: Option<f64>,
}

impl seal::Sealed for AggrMinCost {}

impl NormalAggrObj for AggrMinCost {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::List(l) => {
                let [payload, cost] = &l[..] else {
                    bail!("'min_cost' requires a list of exactly two items as argument")
                };
                let cost = cost
                    .get_float()
                    .ok_or_else(|| miette!("Cost must be numeric"))?;
                let take = match self.cost {
                    None => true,
                    Some(prev) => cost < prev,
                };
                if take {
                    self.cost = Some(cost);
                    self.found = Some(payload.clone());
                }
                Ok(())
            }
            v @ (data_value_any!()) => bail!("cannot compute 'min_cost' on {:?}", v),
        }
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::List(vec![
            self.found.clone().unwrap_or(DataValue::Null),
            self.cost.map(DataValue::from).unwrap_or(DataValue::Null),
        ]))
    }
}

/// Least-cost pair as a meet; ties keep the incumbent.
pub(crate) struct MeetAggrMinCost;

impl seal::Sealed for MeetAggrMinCost {}

impl MeetAggrObj for MeetAggrMinCost {
    fn init_val(&self) -> MeetAccum {
        MeetAccum::Empty
    }

    fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool> {
        if matches!(right, MeetAccum::Empty) {
            return Ok(false);
        }
        if matches!(left, MeetAccum::Empty) {
            *left = right.clone();
            return Ok(true);
        }
        Ok(match (left, right) {
            (MeetAccum::Value(DataValue::List(prev)), MeetAccum::Value(DataValue::List(l))) => {
                let [_, cur_cost] = &l[..] else {
                    bail!(
                        "'min_cost' requires a list of length 2 as argument, got {:?}, {:?}",
                        prev,
                        l
                    )
                };
                let cur_cost = cur_cost
                    .get_float()
                    .ok_or_else(|| miette!("'min_cost' must have numerical costs"))?;
                let [_, prev_cost] = &prev[..] else {
                    bail!(
                        "'min_cost' requires a list of length 2 as argument, got {:?}, {:?}",
                        prev,
                        l
                    )
                };
                let prev_cost = prev_cost
                    .get_float()
                    .ok_or_else(|| miette!("'min_cost' must have numerical costs"))?;

                if prev_cost <= cur_cost {
                    false
                } else {
                    *prev = l.clone();
                    true
                }
            }
            (u, v) => bail!("cannot compute 'min_cost' on {:?}, {:?}", u, v),
        })
    }
}

meet_aggr!(
    AGGR_SHORTEST,
    "shortest",
    Shortest,
    MeetAggrShortest,
    Shortest,
    AggrShortest
);

/// The shortest list-valued row; ties keep the incumbent.
#[derive(Default)]
pub(crate) struct AggrShortest {
    found: Option<Vec<DataValue>>,
}

impl seal::Sealed for AggrShortest {}

impl NormalAggrObj for AggrShortest {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::List(l) => {
                match self.found {
                    None => self.found = Some(l.clone()),
                    Some(ref mut found) => {
                        if l.len() < found.len() {
                            *found = l.clone();
                        }
                    }
                }
                Ok(())
            }
            v @ (data_value_any!()) => bail!("cannot compute 'shortest' on {:?}", v),
        }
    }

    fn get(&self) -> Result<DataValue> {
        Ok(match self.found {
            None => DataValue::Null,
            Some(ref l) => DataValue::List(l.clone()),
        })
    }
}

/// Shortest list as a meet, with [`MeetAccum::Empty`] as the identity.
pub(crate) struct MeetAggrShortest;

impl seal::Sealed for MeetAggrShortest {}

impl MeetAggrObj for MeetAggrShortest {
    fn init_val(&self) -> MeetAccum {
        MeetAccum::Empty
    }

    fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool> {
        if matches!(right, MeetAccum::Empty) {
            return Ok(false);
        }
        if matches!(left, MeetAccum::Empty) {
            *left = right.clone();
            return Ok(true);
        }
        match (left, right) {
            (MeetAccum::Value(DataValue::List(l)), MeetAccum::Value(DataValue::List(r))) => {
                Ok(if r.len() < l.len() {
                    *l = r.clone();
                    true
                } else {
                    false
                })
            }
            (l, v) => bail!("cannot compute 'shortest' on {:?} and {:?}", l, v),
        }
    }
}

meet_aggr!(
    AGGR_CHOICE,
    "choice",
    Choice,
    MeetAggrChoice,
    Choice,
    AggrChoice
);

/// An arbitrary non-null row: the first one seen wins.
#[derive(Default)]
pub(crate) struct AggrChoice {
    found: Option<DataValue>,
}

impl seal::Sealed for AggrChoice {}

impl NormalAggrObj for AggrChoice {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        if self.found.is_none() {
            self.found = Some(value.clone());
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(self.found.clone().unwrap_or(DataValue::Null))
    }
}

/// First-seen as a meet: idempotent and associative, deliberately not
/// commutative.
pub(crate) struct MeetAggrChoice;

impl seal::Sealed for MeetAggrChoice {}

impl MeetAggrObj for MeetAggrChoice {
    fn init_val(&self) -> MeetAccum {
        MeetAccum::Empty
    }

    fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool> {
        if matches!(right, MeetAccum::Empty) {
            return Ok(false);
        }
        Ok(if matches!(left, MeetAccum::Empty) {
            *left = right.clone();
            true
        } else {
            false
        })
    }
}

meet_aggr!(
    AGGR_BIT_AND,
    "bit_and",
    BitAnd,
    MeetAggrBitAnd,
    BitAnd,
    AggrBitAnd
);

/// Bytewise AND of equal-length byte strings, as a fold.
#[derive(Default)]
pub(crate) struct AggrBitAnd {
    res: Vec<u8>,
}

impl seal::Sealed for AggrBitAnd {}

impl NormalAggrObj for AggrBitAnd {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::Bytes(bs) => {
                if self.res.is_empty() {
                    self.res = bs.clone();
                } else {
                    ensure!(
                        self.res.len() == bs.len(),
                        "operands of 'bit_and' must have the same lengths, got {:x?} and {:x?}",
                        self.res,
                        bs
                    );
                    for (l, r) in self.res.iter_mut().zip(bs.iter()) {
                        *l &= *r;
                    }
                }
                Ok(())
            }
            v @ (data_value_any!()) => bail!("cannot apply 'bit_and' to {:?}", v),
        }
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::Bytes(self.res.clone()))
    }
}

/// Bytewise AND as a meet; the empty byte string is the identity seed.
pub(crate) struct MeetAggrBitAnd;

impl seal::Sealed for MeetAggrBitAnd {}

impl MeetAggrObj for MeetAggrBitAnd {
    fn init_val(&self) -> MeetAccum {
        MeetAccum::Value(DataValue::Bytes(Vec::new()))
    }

    fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool> {
        if matches!(right, MeetAccum::Empty) {
            return Ok(false);
        }
        if matches!(left, MeetAccum::Empty) {
            *left = right.clone();
            return Ok(true);
        }
        match (left, right) {
            (
                MeetAccum::Value(DataValue::Bytes(left)),
                MeetAccum::Value(DataValue::Bytes(right)),
            ) => {
                if left.is_empty() {
                    *left = right.clone();
                    return Ok(!left.is_empty());
                }
                ensure!(
                    left.len() == right.len(),
                    "operands of 'bit_and' must have the same lengths, got {:x?} and {:x?}",
                    left,
                    right
                );
                let mut changed = false;
                for (l, r) in left.iter_mut().zip(right.iter()) {
                    let folded = *l & *r;
                    changed |= folded != *l;
                    *l = folded;
                }
                Ok(changed)
            }
            v => bail!("cannot apply 'bit_and' to {:?}", v),
        }
    }
}

meet_aggr!(
    AGGR_BIT_OR,
    "bit_or",
    BitOr,
    MeetAggrBitOr,
    BitOr,
    AggrBitOr
);

/// Bytewise OR of equal-length byte strings, as a fold.
#[derive(Default)]
pub(crate) struct AggrBitOr {
    res: Vec<u8>,
}

impl seal::Sealed for AggrBitOr {}

impl NormalAggrObj for AggrBitOr {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::Bytes(bs) => {
                if self.res.is_empty() {
                    self.res = bs.clone();
                } else {
                    ensure!(
                        self.res.len() == bs.len(),
                        "operands of 'bit_or' must have the same lengths, got {:x?} and {:x?}",
                        self.res,
                        bs
                    );
                    for (l, r) in self.res.iter_mut().zip(bs.iter()) {
                        *l |= *r;
                    }
                }
                Ok(())
            }
            v @ (data_value_any!()) => bail!("cannot apply 'bit_or' to {:?}", v),
        }
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::Bytes(self.res.clone()))
    }
}

/// Bytewise OR as a meet; the empty byte string is the identity seed.
pub(crate) struct MeetAggrBitOr;

impl seal::Sealed for MeetAggrBitOr {}

impl MeetAggrObj for MeetAggrBitOr {
    fn init_val(&self) -> MeetAccum {
        MeetAccum::Value(DataValue::Bytes(Vec::new()))
    }

    fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool> {
        if matches!(right, MeetAccum::Empty) {
            return Ok(false);
        }
        if matches!(left, MeetAccum::Empty) {
            *left = right.clone();
            return Ok(true);
        }
        match (left, right) {
            (
                MeetAccum::Value(DataValue::Bytes(left)),
                MeetAccum::Value(DataValue::Bytes(right)),
            ) => {
                if left.is_empty() {
                    *left = right.clone();
                    return Ok(!left.is_empty());
                }
                ensure!(
                    left.len() == right.len(),
                    "operands of 'bit_or' must have the same lengths, got {:x?} and {:x?}",
                    left,
                    right
                );
                let mut changed = false;
                for (l, r) in left.iter_mut().zip(right.iter()) {
                    let folded = *l | *r;
                    changed |= folded != *l;
                    *l = folded;
                }
                Ok(changed)
            }
            v => bail!("cannot apply 'bit_or' to {:?}", v),
        }
    }
}

normal_aggr!(AGGR_BIT_XOR, "bit_xor", BitXor, AggrBitXor);

/// Bytewise XOR of equal-length byte strings. Not a meet.
#[derive(Default)]
pub(crate) struct AggrBitXor {
    res: Vec<u8>,
}

impl seal::Sealed for AggrBitXor {}

impl NormalAggrObj for AggrBitXor {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::Bytes(bs) => {
                if self.res.is_empty() {
                    self.res = bs.clone();
                } else {
                    ensure!(
                        self.res.len() == bs.len(),
                        "operands of 'bit_xor' must have the same lengths, got {:x?} and {:x?}",
                        self.res,
                        bs
                    );
                    for (l, r) in self.res.iter_mut().zip(bs.iter()) {
                        *l ^= *r;
                    }
                }
                Ok(())
            }
            v @ (data_value_any!()) => bail!("cannot apply 'bit_xor' to {:?}", v),
        }
    }

    fn get(&self) -> Result<DataValue> {
        Ok(DataValue::Bytes(self.res.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kyzo_model::program::aggregate::parse_aggr;
    use proptest::prelude::*;

    fn v(d: DataValue) -> MeetAccum {
        MeetAccum::Value(d)
    }

    fn parse_ok(name: &str) -> Aggregation {
        parse_aggr(name)
            .expect("parse_aggr result")
            .expect("known aggregation")
    }

    /// One fold with the changed-flag contract pinned in both directions.
    fn update_checked(
        op: &dyn MeetAggrObj,
        acc: &mut MeetAccum,
        x: &MeetAccum,
        canon: fn(&MeetAccum) -> MeetAccum,
    ) -> bool {
        let before = acc.clone();
        let changed = op.update(acc, x).expect("meet update failed");
        assert_eq!(
            changed,
            canon(&before) != canon(acc),
            "changed-flag mismatch folding {x:?} into {before:?} (accumulator now {acc:?})"
        );
        changed
    }

    fn alg_meet(
        op: &dyn MeetAggrObj,
        a: &MeetAccum,
        b: &MeetAccum,
        canon: fn(&MeetAccum) -> MeetAccum,
    ) -> MeetAccum {
        let mut acc = a.clone();
        update_checked(op, &mut acc, b, canon);
        acc
    }

    fn ident(a: &MeetAccum) -> MeetAccum {
        a.clone()
    }

    fn as_set(a: &MeetAccum) -> MeetAccum {
        match a {
            MeetAccum::Empty => MeetAccum::Empty,
            MeetAccum::Value(DataValue::List(l)) => {
                MeetAccum::Value(DataValue::Set(l.iter().cloned().collect()))
            }
            MeetAccum::Value(v) => MeetAccum::Value(v.clone()),
        }
    }

    fn assert_semilattice_laws(
        op: &dyn MeetAggrObj,
        samples: [&MeetAccum; 3],
        canon: fn(&MeetAccum) -> MeetAccum,
        commutative: bool,
    ) {
        let [x, y, z] = samples;
        for a in samples {
            let mut acc = a.clone();
            let changed = update_checked(op, &mut acc, a, canon);
            assert!(!changed, "meet(x, x) reported a change for x = {a:?}");
            assert_eq!(canon(&acc), canon(a), "meet(x, x) altered x = {a:?}");
        }
        for a in samples {
            let mut acc = op.init_val();
            update_checked(op, &mut acc, a, canon);
            assert_eq!(canon(&acc), canon(a), "meet(init, x) != x for x = {a:?}");
        }
        let l = alg_meet(op, &alg_meet(op, x, y, canon), z, canon);
        let r = alg_meet(op, x, &alg_meet(op, y, z, canon), canon);
        assert_eq!(
            canon(&l),
            canon(&r),
            "associativity failed on {x:?}, {y:?}, {z:?}"
        );
        if commutative {
            assert_eq!(
                canon(&alg_meet(op, x, y, canon)),
                canon(&alg_meet(op, y, x, canon)),
                "commutativity failed on {x:?}, {y:?}"
            );
        }
    }

    fn arb_value() -> impl Strategy<Value = DataValue> {
        prop_oneof![
            Just(DataValue::Null),
            any::<bool>().prop_map(DataValue::from),
            any::<i64>().prop_map(DataValue::from),
            (-1.0e6..1.0e6f64).prop_map(DataValue::from),
            "[a-z]{0,6}".prop_map(DataValue::from),
        ]
    }

    fn arb_num() -> impl Strategy<Value = DataValue> {
        prop_oneof![
            any::<i64>().prop_map(DataValue::from),
            (-1.0e9..1.0e9f64).prop_map(DataValue::from),
        ]
    }

    fn arb_small_list() -> impl Strategy<Value = DataValue> {
        prop::collection::vec((0i64..6).prop_map(DataValue::from), 0..5).prop_map(DataValue::List)
    }

    fn arb_costed_pair() -> impl Strategy<Value = DataValue> {
        ((0i64..100), (-1.0e3..1.0e3f64)).prop_map(|(payload, cost)| {
            DataValue::List(vec![DataValue::from(payload), DataValue::from(cost)])
        })
    }

    fn arb_bytes_triple() -> impl Strategy<Value = (DataValue, DataValue, DataValue)> {
        (0usize..4).prop_flat_map(|len| {
            let bytes = move || prop::collection::vec(any::<u8>(), len);
            (bytes(), bytes(), bytes()).prop_map(|(x, y, z)| {
                (
                    DataValue::Bytes(x.clone()),
                    DataValue::Bytes(y.clone()),
                    DataValue::Bytes(z.clone()),
                )
            })
        })
    }

    proptest! {
        #[test]
        fn meet_laws_and_or(x in any::<bool>(), y in any::<bool>(), z in any::<bool>()) {
            let (x, y, z) = (v(DataValue::from(x)), v(DataValue::from(y)), v(DataValue::from(z)));
            assert_semilattice_laws(&MeetAggrAnd, [&x, &y, &z], ident, true);
            assert_semilattice_laws(&MeetAggrOr, [&x, &y, &z], ident, true);
        }

        #[test]
        fn meet_laws_min_max(x in arb_num(), y in arb_num(), z in arb_num()) {
            let (x, y, z) = (v(x), v(y), v(z));
            assert_semilattice_laws(&MeetAggrMin, [&x, &y, &z], ident, true);
            assert_semilattice_laws(&MeetAggrMax, [&x, &y, &z], ident, true);
        }

        #[test]
        fn meet_laws_choice(x in arb_value(), y in arb_value(), z in arb_value()) {
            let (x, y, z) = (v(x), v(y), v(z));
            assert_semilattice_laws(&MeetAggrChoice, [&x, &y, &z], ident, false);
        }

        #[test]
        fn meet_laws_bit_and_or((x, y, z) in arb_bytes_triple()) {
            let (x, y, z) = (v(x), v(y), v(z));
            assert_semilattice_laws(&MeetAggrBitAnd, [&x, &y, &z], ident, true);
            assert_semilattice_laws(&MeetAggrBitOr, [&x, &y, &z], ident, true);
        }

        #[test]
        fn meet_laws_union_intersection(
            x in arb_small_list(),
            y in arb_small_list(),
            z in arb_small_list()
        ) {
            let (x, y, z) = (v(x), v(y), v(z));
            assert_semilattice_laws(&MeetAggrUnion, [&x, &y, &z], as_set, true);
            assert_semilattice_laws(&MeetAggrIntersection, [&x, &y, &z], as_set, true);
        }

        #[test]
        fn meet_laws_shortest(
            x in arb_small_list(),
            y in arb_small_list(),
            z in arb_small_list()
        ) {
            let (x, y, z) = (v(x), v(y), v(z));
            assert_semilattice_laws(&MeetAggrShortest, [&x, &y, &z], ident, false);
        }

        #[test]
        fn meet_laws_min_cost(
            x in arb_costed_pair(),
            y in arb_costed_pair(),
            z in arb_costed_pair()
        ) {
            let (x, y, z) = (v(x), v(y), v(z));
            assert_semilattice_laws(&MeetAggrMinCost, [&x, &y, &z], ident, false);
        }
    }

    /// Exact `Num` order: integers beyond 2^53 that collide as `f64` stay
    /// distinct under min/max.
    #[test]
    fn min_max_exact_beyond_2_pow_53() {
        for (lo, hi) in [
            (DataValue::from(i64::MAX - 1), DataValue::from(i64::MAX)),
            (
                DataValue::from(1i64 << 53),
                DataValue::from((1i64 << 53) + 1),
            ),
        ] {
            let (lo, hi) = (v(lo), v(hi));
            let mut acc = lo.clone();
            assert!(!MeetAggrMin.update(&mut acc, &hi).unwrap());
            assert_eq!(acc, lo);
            let mut acc = hi.clone();
            assert!(MeetAggrMin.update(&mut acc, &lo).unwrap());
            assert_eq!(acc, lo);
            let mut acc = lo.clone();
            assert!(MeetAggrMax.update(&mut acc, &hi).unwrap());
            assert_eq!(acc, hi);
            let mut acc = hi.clone();
            assert!(!MeetAggrMax.update(&mut acc, &lo).unwrap());
            assert_eq!(acc, hi);

            for order in [[&lo, &hi], [&hi, &lo]] {
                let mut min = AggrMin::default();
                let mut max = AggrMax::default();
                for a in order {
                    min.set(&a.to_value()).unwrap();
                    max.set(&a.to_value()).unwrap();
                }
                assert_eq!(min.get().unwrap(), lo.to_value());
                assert_eq!(max.get().unwrap(), hi.to_value());
            }
        }

        let int_val = v(DataValue::from((1i64 << 53) + 1));
        let float_val = v(DataValue::from((1i64 << 53) as f64));
        let mut acc = int_val.clone();
        assert!(MeetAggrMin.update(&mut acc, &float_val).unwrap());
        assert_eq!(acc, float_val);
        let mut acc = float_val.clone();
        assert!(!MeetAggrMin.update(&mut acc, &int_val).unwrap());
        assert_eq!(acc, float_val);
        let mut acc = float_val.clone();
        assert!(MeetAggrMax.update(&mut acc, &int_val).unwrap());
        assert_eq!(acc, int_val);
    }

    /// Regression for the upstream and/or changed-flag inversion.
    #[test]
    fn and_or_changed_flag() {
        let t = v(DataValue::from(true));
        let f = v(DataValue::from(false));

        let mut acc = t.clone();
        assert!(MeetAggrAnd.update(&mut acc, &f).unwrap());
        assert_eq!(acc, f);
        assert!(!MeetAggrAnd.update(&mut acc, &t).unwrap());
        assert_eq!(acc, f);
        let mut acc = t.clone();
        assert!(!MeetAggrAnd.update(&mut acc, &t).unwrap());

        let mut acc = f.clone();
        assert!(MeetAggrOr.update(&mut acc, &t).unwrap());
        assert_eq!(acc, t);
        assert!(!MeetAggrOr.update(&mut acc, &f).unwrap());
        assert_eq!(acc, t);
        let mut acc = f.clone();
        assert!(!MeetAggrOr.update(&mut acc, &f).unwrap());
    }

    /// Regression: bit-op meets report the changed flag exactly.
    #[test]
    fn bit_meet_changed_flag_exact() {
        let zero = v(DataValue::Bytes(vec![0x00]));
        let ones = v(DataValue::Bytes(vec![0xff]));
        let some = v(DataValue::Bytes(vec![0x0f]));

        let mut acc = zero.clone();
        assert!(!MeetAggrBitAnd.update(&mut acc, &some).unwrap());
        assert_eq!(acc, zero);
        let mut acc = ones.clone();
        assert!(MeetAggrBitAnd.update(&mut acc, &some).unwrap());
        assert_eq!(acc, some);

        let mut acc = ones.clone();
        assert!(!MeetAggrBitOr.update(&mut acc, &some).unwrap());
        assert_eq!(acc, ones);
        let mut acc = zero.clone();
        assert!(MeetAggrBitOr.update(&mut acc, &some).unwrap());
        assert_eq!(acc, some);
    }

    /// All-integer sum/product return exact Int; float input demotes.
    #[test]
    fn sum_product_exact_int() {
        let mut op = AggrSum::default();
        for i in [1i64, 2, 3] {
            op.set(&DataValue::from(i)).unwrap();
        }
        assert_eq!(op.get().unwrap(), DataValue::from(6i64));

        let mut op = AggrSum::default();
        op.set(&DataValue::from(1i64)).unwrap();
        op.set(&DataValue::from(2.0)).unwrap();
        assert_eq!(op.get().unwrap(), DataValue::from(3.0));

        let a = (1i64 << 53) + 1;
        let b = (1i64 << 53) + 3;
        let mut op = AggrSum::default();
        op.set(&DataValue::from(a)).unwrap();
        op.set(&DataValue::from(b)).unwrap();
        assert_eq!(op.get().unwrap(), DataValue::from(a + b));

        let mut op = AggrSum::default();
        op.set(&DataValue::from(i64::MAX)).unwrap();
        op.set(&DataValue::from(i64::MAX)).unwrap();
        assert_eq!(op.get().unwrap(), DataValue::from(2.0 * i64::MAX as f64));
        let mut op = AggrSum::default();
        op.set(&DataValue::from(i64::MAX)).unwrap();
        op.set(&DataValue::from(i64::MAX)).unwrap();
        op.set(&DataValue::from(i64::MIN)).unwrap();
        assert_eq!(op.get().unwrap(), DataValue::from(i64::MAX - 1));

        let mut op = AggrProduct::default();
        for i in [2i64, 3, 4] {
            op.set(&DataValue::from(i)).unwrap();
        }
        assert_eq!(op.get().unwrap(), DataValue::from(24i64));

        let mut op = AggrProduct::default();
        op.set(&DataValue::from(2i64)).unwrap();
        op.set(&DataValue::from(0.5)).unwrap();
        assert_eq!(op.get().unwrap(), DataValue::from(1.0));

        let mut op = AggrProduct::default();
        op.set(&DataValue::from(i64::MAX)).unwrap();
        op.set(&DataValue::from(4i64)).unwrap();
        assert_eq!(op.get().unwrap(), DataValue::from(i64::MAX as f64 * 4.0));

        assert_eq!(AggrSum::default().get().unwrap(), DataValue::from(0i64));
        assert_eq!(AggrProduct::default().get().unwrap(), DataValue::from(1i64));
    }

    /// Fold ops agree with model kind for every registered name.
    #[test]
    fn fold_ops_agree_with_model_kind() {
        const MEETS: &[&str] = &[
            "and",
            "or",
            "min",
            "max",
            "choice",
            "bit_and",
            "bit_or",
            "union",
            "intersection",
            "shortest",
            "min_cost",
            "hll_union",
        ];
        const NORMALS: &[&str] = &[
            "unique",
            "group_count",
            "count",
            "count_unique",
            "variance",
            "std_dev",
            "sum",
            "product",
            "mean",
            "collect",
            "bit_xor",
            "latest_by",
            "smallest_by",
            "hll",
            "hll_sketch",
            "count_min",
            "tdigest",
        ];
        for name in MEETS {
            let aggr = parse_ok(name);
            assert!(aggr.is_meet(), "{name} must be a meet");
            assert!(meet_op(&aggr).is_some(), "{name} must yield a meet op");
            normal_op(&aggr, &[]).unwrap();
        }
        for name in NORMALS {
            let aggr = parse_ok(name);
            assert!(!aggr.is_meet(), "{name} must not be a meet");
            assert!(meet_op(&aggr).is_none(), "{name} must yield no meet op");
            normal_op(&aggr, &[]).unwrap();
        }
    }

    /// `collect`'s optional limit is validated at construction and honored.
    #[test]
    fn collect_limit_argument() {
        let aggr = parse_ok("collect");
        let mut op = normal_op(&aggr, &[DataValue::from(2)]).unwrap();
        for i in 0..5 {
            op.set(&DataValue::from(i)).unwrap();
        }
        assert_eq!(
            op.get().unwrap(),
            DataValue::List(vec![DataValue::from(0), DataValue::from(1)])
        );
        assert!(normal_op(&aggr, &[DataValue::from(0)]).is_err());
        assert!(normal_op(&aggr, &[DataValue::from("two")]).is_err());
    }

    /// F1: i128-overflow demotion arm of `NumAccum` is reachable in product.
    #[test]
    fn product_overflowing_i128_demotes_with_both_operands() {
        let mut acc = AggrProduct::default();
        for _ in 0..3 {
            acc.set(&DataValue::from(i64::MAX)).unwrap();
        }
        match acc.get().unwrap() {
            DataValue::Num(n) if n.as_float().is_some() => {
                let f = n.as_float().expect("guarded float");
                let expected = (i64::MAX as f64).powi(3);
                assert!(
                    (f - expected).abs() / expected < 1e-9,
                    "product demotion lost an operand: got {f:e}, expected {expected:e}"
                );
            }
            other @ (data_value_any!()) => {
                panic!("expected float after i128 overflow, got {other:?}")
            }
        }
    }

    /// F2: intersection vs union value oracles.
    #[test]
    fn set_ops_compute_the_right_operation() {
        let two = |a: i64, b: i64| {
            v(DataValue::List(vec![
                DataValue::from(a),
                DataValue::from(b),
            ]))
        };
        let meet = MeetAggrIntersection;
        let mut acc = meet.init_val();
        assert!(matches!(acc, MeetAccum::Empty));
        meet.update(&mut acc, &two(1, 2)).unwrap();
        meet.update(&mut acc, &two(2, 3)).unwrap();
        assert_eq!(
            acc,
            v(DataValue::Set([DataValue::from(2)].into_iter().collect())),
            "intersection of {{1,2}} and {{2,3}} must be {{2}}"
        );
        let meet = MeetAggrUnion;
        let mut acc = meet.init_val();
        meet.update(&mut acc, &two(1, 2)).unwrap();
        meet.update(&mut acc, &two(2, 3)).unwrap();
        assert_eq!(
            acc,
            v(DataValue::Set(
                [DataValue::from(1), DataValue::from(2), DataValue::from(3)]
                    .into_iter()
                    .collect()
            )),
            "union of {{1,2}} and {{2,3}} must be {{1,2,3}}"
        );
    }

    /// Null as ordinary set-element data under meet intersection.
    #[test]
    fn meet_intersection_null_as_data_round_trips() {
        let meet = MeetAggrIntersection;
        assert!(matches!(meet.init_val(), MeetAccum::Empty));

        let with_null = |elems: &[DataValue]| v(DataValue::List(elems.to_vec()));
        let mut acc = meet.init_val();
        meet.update(
            &mut acc,
            &with_null(&[DataValue::Null, DataValue::from(1), DataValue::from(2)]),
        )
        .unwrap();
        meet.update(
            &mut acc,
            &with_null(&[DataValue::Null, DataValue::from(2), DataValue::from(3)]),
        )
        .unwrap();
        assert_eq!(
            acc,
            v(DataValue::Set(
                [DataValue::Null, DataValue::from(2)].into_iter().collect()
            )),
            "Null as a set element must survive meet intersection"
        );

        let mut acc = meet.init_val();
        assert!(
            meet.update(&mut acc, &with_null(&[DataValue::Null]))
                .unwrap()
        );
        assert_eq!(
            acc,
            v(DataValue::List(vec![DataValue::Null])),
            "adopting [Null] from Empty must yield Value, not stay Empty"
        );
        assert!(!meet.update(&mut acc, &MeetAccum::Empty).unwrap());
        assert_eq!(acc, v(DataValue::List(vec![DataValue::Null])));
    }
}
