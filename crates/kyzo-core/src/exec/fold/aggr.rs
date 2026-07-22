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


/// i64 count → f64 via [`Num::to_f64`] (cast lives in kyzo-model, not here).
fn count_to_f64(count: i64) -> f64 {
    kyzo_model::value::Num::int(count).to_f64()
}

use miette::{Result, bail, ensure, miette};

use crate::exec::stdlib::convert::i128_approx_f64;

use kyzo_model::data_value_any;
use kyzo_model::program::aggregate::Aggregation;
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
        _unknown => None,
    }
}

/// A fresh normal fold for a model [`Aggregation`] (every aggregation,
/// meet included, has one).
pub(crate) fn normal_op(a: &Aggregation, args: &[DataValue]) -> Result<NormalAggr> {
    match a.name {
        "and" => Ok(NormalAggr::And(AggrAnd::empty())),
        "or" => Ok(NormalAggr::Or(AggrOr::empty())),
        "unique" => Ok(NormalAggr::Unique(AggrUnique::empty())),
        "group_count" => Ok(NormalAggr::GroupCount(AggrGroupCount::empty())),
        "count_unique" => Ok(NormalAggr::CountUnique(AggrCountUnique::empty())),
        "union" => Ok(NormalAggr::Union(AggrUnion::empty())),
        "intersection" => Ok(NormalAggr::Intersection(AggrIntersection::empty())),
        "collect" => collect_factory(args),
        "count" => Ok(NormalAggr::Count(AggrCount::empty())),
        "variance" => Ok(NormalAggr::Variance(AggrVariance::empty())),
        "std_dev" => Ok(NormalAggr::StdDev(AggrStdDev::empty())),
        "mean" => Ok(NormalAggr::Mean(AggrMean::empty())),
        "sum" => Ok(NormalAggr::Sum(AggrSum::empty())),
        "product" => Ok(NormalAggr::Product(AggrProduct::empty())),
        "min" => Ok(NormalAggr::Min(AggrMin::empty())),
        "max" => Ok(NormalAggr::Max(AggrMax::empty())),
        "latest_by" => Ok(NormalAggr::LatestBy(AggrLatestBy::empty())),
        "smallest_by" => Ok(NormalAggr::SmallestBy(AggrSmallestBy::empty())),
        "min_cost" => Ok(NormalAggr::MinCost(AggrMinCost::empty())),
        "shortest" => Ok(NormalAggr::Shortest(AggrShortest::empty())),
        "choice" => Ok(NormalAggr::Choice(AggrChoice::empty())),
        "bit_and" => Ok(NormalAggr::BitAnd(AggrBitAnd::empty())),
        "bit_or" => Ok(NormalAggr::BitOr(AggrBitOr::empty())),
        "bit_xor" => Ok(NormalAggr::BitXor(AggrBitXor::empty())),
        "hll" => Ok(NormalAggr::Hll(AggrHll::empty())),
        "hll_sketch" => Ok(NormalAggr::HllSketch(AggrHllSketch::empty())),
        "hll_union" => Ok(NormalAggr::HllUnion(AggrHllUnion::empty())),
        "count_min" => Ok(NormalAggr::CountMin(AggrCountMin::empty())),
        "tdigest" => Ok(NormalAggr::TDigest(AggrTDigest::empty())),
        "quantile" => crate::exec::fold::sketch::aggr::quantile_factory(args),
        other => bail!("no fold factory for aggregation '{other}'"),
    }
}

// ── Fold bodies (from condemned data/aggr.rs) ────────────────────────────
// Aggregation descriptors live solely in kyzo_model::program::aggregate::parse_aggr.

/// Conjunction as a fold: `true` until any row is `false`.
pub(crate) struct AggrAnd {
    accum: bool,
}

impl AggrAnd {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
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

/// Disjunction as a fold: `false` until any row is `true`.
pub(crate) struct AggrOr {
    accum: bool,
}

impl AggrOr {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            accum: false,
        }
    }
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

/// The distinct values seen, as a sorted list.
pub(crate) struct AggrUnique {
    accum: BTreeSet<DataValue>,
}

impl AggrUnique {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            accum: BTreeSet::new(),
        }
    }
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

/// Each distinct value with its multiplicity, as a sorted list of pairs.
pub(crate) struct AggrGroupCount {
    accum: BTreeMap<DataValue, i64>,
}

impl AggrGroupCount {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            accum: BTreeMap::new(),
        }
    }
}

impl seal::Sealed for AggrGroupCount {}

impl NormalAggrObj for AggrGroupCount {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        let entry = self.accum.entry(value.clone()).or_insert(0);
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

/// How many distinct values were seen.
pub(crate) struct AggrCountUnique {
    count: i64,
    accum: BTreeSet<DataValue>,
}

impl AggrCountUnique {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            count: 0,
            accum: BTreeSet::new(),
        }
    }
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

/// Set union of list-valued rows, as a fold.
pub(crate) struct AggrUnion {
    accum: BTreeSet<DataValue>,
}

impl AggrUnion {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            accum: BTreeSet::new(),
        }
    }
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

/// Set intersection of list-valued rows, as a fold.
pub(crate) struct AggrIntersection {
    accum: Option<BTreeSet<DataValue>>,
}

impl AggrIntersection {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            accum: None,
        }
    }
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
        None => NormalAggr::Collect(AggrCollect::empty()),
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
            NormalAggr::Collect(AggrCollect::new(usize::try_from(limit).map_err(|_| {
                miette!("'collect' limit does not fit usize: {limit}")
            })?))
        }
    })
}

/// The values in arrival order, as a list, optionally truncated to a limit.
pub(crate) struct AggrCollect {
    limit: Option<usize>,
    accum: Vec<DataValue>,
}

impl AggrCollect {
    /// Empty accumulator — unlimited collect identity.
    pub(crate) fn empty() -> Self {
        Self {
            limit: None,
            accum: Vec::new(),
        }
    }

    fn new(limit: usize) -> Self {
        Self {
            limit: Some(limit),
            accum: Vec::new(),
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

/// How many rows were seen (including nulls).
pub(crate) struct AggrCount {
    count: i64,
}

impl AggrCount {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            count: 0,
        }
    }
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

/// Sample variance (Bessel-corrected), accumulated in floating point.
pub(crate) struct AggrVariance {
    count: i64,
    sum: f64,
    sum_sq: f64,
}

impl AggrVariance {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            count: 0,
            sum: 0.0,
            sum_sq: 0.0,
        }
    }
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
        let ct = count_to_f64(self.count);
        Ok(DataValue::from(
            (self.sum_sq - self.sum * self.sum / ct) / (ct - 1.),
        ))
    }
}

/// Sample standard deviation (sqrt of sample variance).
pub(crate) struct AggrStdDev {
    count: i64,
    sum: f64,
    sum_sq: f64,
}

impl AggrStdDev {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            count: 0,
            sum: 0.0,
            sum_sq: 0.0,
        }
    }
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
        let ct = count_to_f64(self.count);
        let var = (self.sum_sq - self.sum * self.sum / ct) / (ct - 1.);
        Ok(DataValue::from(var.sqrt()))
    }
}

/// The arithmetic mean, accumulated in floating point.
pub(crate) struct AggrMean {
    count: i64,
    sum: f64,
}

impl AggrMean {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            count: 0,
            sum: 0.0,
        }
    }
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
        Ok(DataValue::from(self.sum / count_to_f64(self.count)))
    }
}

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
            (NumAccum::Int(acc), NumRepr::Int(i)) => match int_op(acc, i128::from(i)) {
                Some(acc) => NumAccum::Int(acc),
                None => NumAccum::Float(float_op(i128_approx_f64(acc), kyzo_model::value::Num::int(i).to_f64())),
            },
            (NumAccum::Int(acc), NumRepr::Float(f)) => NumAccum::Float(float_op(i128_approx_f64(acc), f)),
            (NumAccum::Float(acc), _) => NumAccum::Float(float_op(acc, n.to_f64())),
        }
    }

    fn finish(self) -> DataValue {
        match self {
            NumAccum::Int(acc) => match i64::try_from(acc) {
                Ok(i) => DataValue::from(i),
                Err(_acc_exceeds_i64) => DataValue::from(i128_approx_f64(acc)),
            },
            NumAccum::Float(f) => DataValue::from(f),
        }
    }
}

/// The sum, accumulated exactly via [`NumAccum`].
pub(crate) struct AggrSum {
    sum: NumAccum,
}

impl AggrSum {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
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

/// The product, accumulated exactly via [`NumAccum`].
pub(crate) struct AggrProduct {
    prod: NumAccum,
}

impl AggrProduct {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
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

/// The numerical minimum, ignoring nulls; `Null` *result* when no row had
/// a number. In-state absence is [`Option::None`], never a Null sentinel.
pub(crate) struct AggrMin {
    found: Option<DataValue>,
}

impl AggrMin {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            found: None,
        }
    }
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
                    (data_value_any!(), data_value_any!()) => bail!("'min' applied to non-numerical values"),
                };
                if new < found_n {
                    self.found = Some(value.clone());
                }
            }
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(match self.found.clone() { Some(v) => v, None => DataValue::Null })
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
                    (data_value_any!(), data_value_any!()) => bail!("'min' applied to non-numerical values"),
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

/// The greatest numeric value, via exact [`Num`] order. Nulls are skipped.
pub(crate) struct AggrMax {
    found: Option<DataValue>,
}

impl AggrMax {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            found: None,
        }
    }
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
                    (data_value_any!(), data_value_any!()) => bail!("'max' applied to non-numerical values"),
                };
                if new > found_n {
                    self.found = Some(value.clone());
                }
            }
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(match self.found.clone() { Some(v) => v, None => DataValue::Null })
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
                    (data_value_any!(), data_value_any!()) => bail!("'max' applied to non-numerical values"),
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

/// Of `[payload, cost]` pairs, the payload whose cost sorts greatest.
pub(crate) struct AggrLatestBy {
    found: Option<DataValue>,
    cost: Option<DataValue>,
}

impl AggrLatestBy {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            found: None,
            cost: None,
        }
    }
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
        Ok(match self.found.clone() { Some(v) => v, None => DataValue::Null })
    }
}

/// Of `[payload, cost]` pairs, the payload whose cost sorts least.
pub(crate) struct AggrSmallestBy {
    found: Option<DataValue>,
    cost: Option<DataValue>,
}

impl AggrSmallestBy {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            found: None,
            cost: None,
        }
    }
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
        Ok(match self.found.clone() { Some(v) => v, None => DataValue::Null })
    }
}

/// Of `[payload, cost]` pairs, the pair with the numerically least cost.
pub(crate) struct AggrMinCost {
    found: Option<DataValue>,
    cost: Option<f64>,
}

impl AggrMinCost {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            found: None,
            cost: None,
        }
    }
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
            match self.found.clone() { Some(v) => v, None => DataValue::Null },
            match self.cost { Some(c) => DataValue::from(c), None => DataValue::Null },
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

/// The shortest list-valued row; ties keep the incumbent.
pub(crate) struct AggrShortest {
    found: Option<Vec<DataValue>>,
}

impl AggrShortest {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            found: None,
        }
    }
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

/// An arbitrary non-null row: the first one seen wins.
pub(crate) struct AggrChoice {
    found: Option<DataValue>,
}

impl AggrChoice {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            found: None,
        }
    }
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
        Ok(match self.found.clone() { Some(v) => v, None => DataValue::Null })
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

/// Bytewise AND of equal-length byte strings, as a fold.
pub(crate) struct AggrBitAnd {
    res: Vec<u8>,
}

impl AggrBitAnd {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            res: Vec::new(),
        }
    }
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

/// Bytewise OR of equal-length byte strings, as a fold.
pub(crate) struct AggrBitOr {
    res: Vec<u8>,
}

impl AggrBitOr {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            res: Vec::new(),
        }
    }
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

/// Bytewise XOR of equal-length byte strings. Not a meet.
pub(crate) struct AggrBitXor {
    res: Vec<u8>,
}

impl AggrBitXor {
    /// Empty accumulator — the fold identity for this aggregation.
    pub(crate) fn empty() -> Self {
        Self {
            res: Vec::new(),
        }
    }
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
    use miette::{Result, miette};
    use proptest::prelude::*;
    use proptest::test_runner::TestCaseError;

    fn lattice_ok(r: Result<()>) -> std::result::Result<(), TestCaseError> {
        r.map_err(|e| TestCaseError::fail(format!("{e}")))
    }

    fn v(d: DataValue) -> MeetAccum {
        MeetAccum::Value(d)
    }

    fn parse_ok(name: &str) -> Result<Aggregation> {
        parse_aggr(name)
            .map_err(|e| miette!("parse_aggr result: {e}"))?
            .ok_or_else(|| miette!("known aggregation"))
    }

    /// One fold with the changed-flag contract pinned in both directions.
    fn update_checked(
        op: &dyn MeetAggrObj,
        acc: &mut MeetAccum,
        x: &MeetAccum,
        canon: fn(&MeetAccum) -> MeetAccum,
    ) -> Result<bool> {
        let before = acc.clone();
        let changed = op.update(acc, x).map_err(|e| miette!("meet update failed: {e}"))?;
        assert_eq!(
            changed,
            canon(&before) != canon(acc),
            "changed-flag mismatch folding {x:?} into {before:?} (accumulator now {acc:?})"
        );
        Ok(changed)
    }

    fn alg_meet(
        op: &dyn MeetAggrObj,
        a: &MeetAccum,
        b: &MeetAccum,
        canon: fn(&MeetAccum) -> MeetAccum,
    ) -> Result<MeetAccum> {
        let mut acc = a.clone();
        update_checked(op, &mut acc, b, canon)?;
        Ok(acc)
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
    ) -> Result<()> {
        let [x, y, z] = samples;
        for a in samples {
            let mut acc = a.clone();
            let changed = update_checked(op, &mut acc, a, canon)?;
            assert!(!changed, "meet(x, x) reported a change for x = {a:?}");
            assert_eq!(canon(&acc), canon(a), "meet(x, x) altered x = {a:?}");
        }
        for a in samples {
            let mut acc = op.init_val();
            update_checked(op, &mut acc, a, canon)?;
            assert_eq!(canon(&acc), canon(a), "meet(init, x) != x for x = {a:?}");
        }
        let l = alg_meet(op, &alg_meet(op, x, y, canon)?, z, canon)?;
        let r = alg_meet(op, x, &alg_meet(op, y, z, canon)?, canon)?;
        assert_eq!(
            canon(&l),
            canon(&r),
            "associativity failed on {x:?}, {y:?}, {z:?}"
        );
        if commutative {
            assert_eq!(
                canon(&alg_meet(op, x, y, canon)?),
                canon(&alg_meet(op, y, x, canon)?),
                "commutativity failed on {x:?}, {y:?}"
            );
        }
        Ok(())
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
            lattice_ok(assert_semilattice_laws(
                &MeetAggrAnd,
                [&x, &y, &z],
                ident,
                true,
            ))?;
            lattice_ok(assert_semilattice_laws(
                &MeetAggrOr,
                [&x, &y, &z],
                ident,
                true,
            ))?;
        }

        #[test]
        fn meet_laws_min_max(x in arb_num(), y in arb_num(), z in arb_num()) {
            let (x, y, z) = (v(x), v(y), v(z));
            lattice_ok(assert_semilattice_laws(
                &MeetAggrMin,
                [&x, &y, &z],
                ident,
                true,
            ))?;
            lattice_ok(assert_semilattice_laws(
                &MeetAggrMax,
                [&x, &y, &z],
                ident,
                true,
            ))?;
        }

        #[test]
        fn meet_laws_choice(x in arb_value(), y in arb_value(), z in arb_value()) {
            let (x, y, z) = (v(x), v(y), v(z));
            lattice_ok(assert_semilattice_laws(
                &MeetAggrChoice,
                [&x, &y, &z],
                ident,
                false,
            ))?;
        }

        #[test]
        fn meet_laws_bit_and_or((x, y, z) in arb_bytes_triple()) {
            let (x, y, z) = (v(x), v(y), v(z));
            lattice_ok(assert_semilattice_laws(
                &MeetAggrBitAnd,
                [&x, &y, &z],
                ident,
                true,
            ))?;
            lattice_ok(assert_semilattice_laws(
                &MeetAggrBitOr,
                [&x, &y, &z],
                ident,
                true,
            ))?;
        }

        #[test]
        fn meet_laws_union_intersection(
            x in arb_small_list(),
            y in arb_small_list(),
            z in arb_small_list()
        ) {
            let (x, y, z) = (v(x), v(y), v(z));
            lattice_ok(assert_semilattice_laws(
                &MeetAggrUnion,
                [&x, &y, &z],
                as_set,
                true,
            ))?;
            lattice_ok(assert_semilattice_laws(
                &MeetAggrIntersection,
                [&x, &y, &z],
                as_set,
                true,
            ))?;
        }

        #[test]
        fn meet_laws_shortest(
            x in arb_small_list(),
            y in arb_small_list(),
            z in arb_small_list()
        ) {
            let (x, y, z) = (v(x), v(y), v(z));
            lattice_ok(assert_semilattice_laws(
                &MeetAggrShortest,
                [&x, &y, &z],
                ident,
                false,
            ))?;
        }

        #[test]
        fn meet_laws_min_cost(
            x in arb_costed_pair(),
            y in arb_costed_pair(),
            z in arb_costed_pair()
        ) {
            let (x, y, z) = (v(x), v(y), v(z));
            lattice_ok(assert_semilattice_laws(
                &MeetAggrMinCost,
                [&x, &y, &z],
                ident,
                false,
            ))?;
        }
    }

    /// Exact `Num` order: integers beyond 2^53 that collide as `f64` stay
    /// distinct under min/max.
    #[test]
    fn min_max_exact_beyond_2_pow_53() -> Result<()> {
        for (lo, hi) in [
            (DataValue::from(i64::MAX - 1), DataValue::from(i64::MAX)),
            (
                DataValue::from(1i64 << 53),
                DataValue::from((1i64 << 53) + 1),
            ),
        ] {
            let (lo, hi) = (v(lo), v(hi));
            let mut acc = lo.clone();
            assert!(!MeetAggrMin.update(&mut acc, &hi)?);
            assert_eq!(acc, lo);
            let mut acc = hi.clone();
            assert!(MeetAggrMin.update(&mut acc, &lo)?);
            assert_eq!(acc, lo);
            let mut acc = lo.clone();
            assert!(MeetAggrMax.update(&mut acc, &hi)?);
            assert_eq!(acc, hi);
            let mut acc = hi.clone();
            assert!(!MeetAggrMax.update(&mut acc, &lo)?);
            assert_eq!(acc, hi);

            for order in [[&lo, &hi], [&hi, &lo]] {
                let mut min = AggrMin::empty();
                let mut max = AggrMax::empty();
                for a in order {
                    min.set(&a.to_value())?;
                    max.set(&a.to_value())?;
                }
                assert_eq!(min.get()?, lo.to_value());
                assert_eq!(max.get()?, hi.to_value());
            }
        }

        let int_val = v(DataValue::from((1i64 << 53) + 1));
        let float_val = v(DataValue::from(Num::int(1i64 << 53).to_f64()));
        let mut acc = int_val.clone();
        assert!(MeetAggrMin.update(&mut acc, &float_val)?);
        assert_eq!(acc, float_val);
        let mut acc = float_val.clone();
        assert!(!MeetAggrMin.update(&mut acc, &int_val)?);
        assert_eq!(acc, float_val);
        let mut acc = float_val.clone();
        assert!(MeetAggrMax.update(&mut acc, &int_val)?);
        assert_eq!(acc, int_val);
        Ok(())
    }

    /// Regression for the upstream and/or changed-flag inversion.
    #[test]
    fn and_or_changed_flag() -> Result<()> {
        let t = v(DataValue::from(true));
        let f = v(DataValue::from(false));

        let mut acc = t.clone();
        assert!(MeetAggrAnd.update(&mut acc, &f)?);
        assert_eq!(acc, f);
        assert!(!MeetAggrAnd.update(&mut acc, &t)?);
        assert_eq!(acc, f);
        let mut acc = t.clone();
        assert!(!MeetAggrAnd.update(&mut acc, &t)?);

        let mut acc = f.clone();
        assert!(MeetAggrOr.update(&mut acc, &t)?);
        assert_eq!(acc, t);
        assert!(!MeetAggrOr.update(&mut acc, &f)?);
        assert_eq!(acc, t);
        let mut acc = f.clone();
        assert!(!MeetAggrOr.update(&mut acc, &f)?);
        Ok(())
    }

    /// Regression: bit-op meets report the changed flag exactly.
    #[test]
    fn bit_meet_changed_flag_exact() -> Result<()> {
        let zero = v(DataValue::Bytes(vec![0x00]));
        let ones = v(DataValue::Bytes(vec![0xff]));
        let some = v(DataValue::Bytes(vec![0x0f]));

        let mut acc = zero.clone();
        assert!(!MeetAggrBitAnd.update(&mut acc, &some)?);
        assert_eq!(acc, zero);
        let mut acc = ones.clone();
        assert!(MeetAggrBitAnd.update(&mut acc, &some)?);
        assert_eq!(acc, some);

        let mut acc = ones.clone();
        assert!(!MeetAggrBitOr.update(&mut acc, &some)?);
        assert_eq!(acc, ones);
        let mut acc = zero.clone();
        assert!(MeetAggrBitOr.update(&mut acc, &some)?);
        assert_eq!(acc, some);
        Ok(())
    }

    /// All-integer sum/product return exact Int; float input demotes.
    #[test]
    fn sum_product_exact_int() -> Result<()> {
        let mut op = AggrSum::empty();
        for i in [1i64, 2, 3] {
            op.set(&DataValue::from(i))?;
        }
        assert_eq!(op.get()?, DataValue::from(6i64));

        let mut op = AggrSum::empty();
        op.set(&DataValue::from(1i64))?;
        op.set(&DataValue::from(2.0))?;
        assert_eq!(op.get()?, DataValue::from(3.0));

        let a = (1i64 << 53) + 1;
        let b = (1i64 << 53) + 3;
        let mut op = AggrSum::empty();
        op.set(&DataValue::from(a))?;
        op.set(&DataValue::from(b))?;
        assert_eq!(op.get()?, DataValue::from(a + b));

        let mut op = AggrSum::empty();
        op.set(&DataValue::from(i64::MAX))?;
        op.set(&DataValue::from(i64::MAX))?;
        assert_eq!(op.get()?, DataValue::from(2.0 * Num::int(i64::MAX).to_f64()));
        let mut op = AggrSum::empty();
        op.set(&DataValue::from(i64::MAX))?;
        op.set(&DataValue::from(i64::MAX))?;
        op.set(&DataValue::from(i64::MIN))?;
        assert_eq!(op.get()?, DataValue::from(i64::MAX - 1));

        let mut op = AggrProduct::empty();
        for i in [2i64, 3, 4] {
            op.set(&DataValue::from(i))?;
        }
        assert_eq!(op.get()?, DataValue::from(24i64));

        let mut op = AggrProduct::empty();
        op.set(&DataValue::from(2i64))?;
        op.set(&DataValue::from(0.5))?;
        assert_eq!(op.get()?, DataValue::from(1.0));

        let mut op = AggrProduct::empty();
        op.set(&DataValue::from(i64::MAX))?;
        op.set(&DataValue::from(4i64))?;
        assert_eq!(op.get()?, DataValue::from(Num::int(i64::MAX).to_f64() * 4.0));

        assert_eq!(AggrSum::empty().get()?, DataValue::from(0i64));
        assert_eq!(AggrProduct::empty().get()?, DataValue::from(1i64));
        Ok(())
    }

    /// Fold ops agree with model kind for every registered name.
    #[test]
    fn fold_ops_agree_with_model_kind() -> Result<()> {
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
            let aggr = parse_ok(name)?;
            assert!(aggr.is_meet(), "{name} must be a meet");
            assert!(meet_op(&aggr).is_some(), "{name} must yield a meet op");
            normal_op(&aggr, &[])?;
        }
        for name in NORMALS {
            let aggr = parse_ok(name)?;
            assert!(!aggr.is_meet(), "{name} must not be a meet");
            assert!(meet_op(&aggr).is_none(), "{name} must yield no meet op");
            normal_op(&aggr, &[])?;
        }
        Ok(())
    }

    /// `collect`'s optional limit is validated at construction and honored.
    #[test]
    fn collect_limit_argument() -> Result<()> {
        let aggr = parse_ok("collect")?;
        let mut op = normal_op(&aggr, &[DataValue::from(2)])?;
        for i in 0..5 {
            op.set(&DataValue::from(i))?;
        }
        assert_eq!(
            op.get()?,
            DataValue::List(vec![DataValue::from(0), DataValue::from(1)])
        );
        assert!(normal_op(&aggr, &[DataValue::from(0)]).is_err());
        assert!(normal_op(&aggr, &[DataValue::from("two")]).is_err());
        Ok(())
    }

    /// F1: i128-overflow demotion arm of `NumAccum` is reachable in product.
    #[test]
    fn product_overflowing_i128_demotes_with_both_operands() -> Result<()> {
        let mut acc = AggrProduct::empty();
        for _ in 0..3 {
            acc.set(&DataValue::from(i64::MAX))?;
        }
        match acc.get()? {
            DataValue::Num(n) if n.as_float().is_some() => {
                let f = n.as_float().ok_or_else(|| miette!("guarded float"))?;
                let expected = Num::int(i64::MAX).to_f64().powi(3);
                assert!(
                    (f - expected).abs() / expected < 1e-9,
                    "product demotion lost an operand: got {f:e}, expected {expected:e}"
                );
            }
            other @ (data_value_any!()) => {
                return Err(miette!(
                    "expected float after i128 overflow, got {other:?}"
                ));
            }
        }
        Ok(())
    }

    /// F2: intersection vs union value oracles.
    #[test]
    fn set_ops_compute_the_right_operation() -> Result<()> {
        let two = |a: i64, b: i64| {
            v(DataValue::List(vec![
                DataValue::from(a),
                DataValue::from(b),
            ]))
        };
        let meet = MeetAggrIntersection;
        let mut acc = meet.init_val();
        assert!(matches!(acc, MeetAccum::Empty));
        meet.update(&mut acc, &two(1, 2))?;
        meet.update(&mut acc, &two(2, 3))?;
        assert_eq!(
            acc,
            v(DataValue::Set([DataValue::from(2)].into_iter().collect())),
            "intersection of {{1,2}} and {{2,3}} must be {{2}}"
        );
        let meet = MeetAggrUnion;
        let mut acc = meet.init_val();
        meet.update(&mut acc, &two(1, 2))?;
        meet.update(&mut acc, &two(2, 3))?;
        assert_eq!(
            acc,
            v(DataValue::Set(
                [DataValue::from(1), DataValue::from(2), DataValue::from(3)]
                    .into_iter()
                    .collect()
            )),
            "union of {{1,2}} and {{2,3}} must be {{1,2,3}}"
        );
        Ok(())
    }

    /// Null as ordinary set-element data under meet intersection.
    #[test]
    fn meet_intersection_null_as_data_round_trips() -> Result<()> {
        let meet = MeetAggrIntersection;
        assert!(matches!(meet.init_val(), MeetAccum::Empty));

        let with_null = |elems: &[DataValue]| v(DataValue::List(elems.to_vec()));
        let mut acc = meet.init_val();
        meet.update(
            &mut acc,
            &with_null(&[DataValue::Null, DataValue::from(1), DataValue::from(2)]),
        )
        ?;
        meet.update(
            &mut acc,
            &with_null(&[DataValue::Null, DataValue::from(2), DataValue::from(3)]),
        )
        ?;
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
                ?
        );
        assert_eq!(
            acc,
            v(DataValue::List(vec![DataValue::Null])),
            "adopting [Null] from Empty must yield Value, not stay Empty"
        );
        assert!(!meet.update(&mut acc, &MeetAccum::Empty)?);
        assert_eq!(acc, v(DataValue::List(vec![DataValue::Null])));
        Ok(())
    }
}
