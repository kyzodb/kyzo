/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): aggregation kind and implementation are one type; the and/or
 * meet changed-flag inversion is fixed; the bit_and/bit_or meet flags are
 * exact (upstream over-reported: any differing operand counted as a
 * change); min/max compare via the exact `Num` order (upstream compared
 * through `f64`, where distinct integers beyond 2^53 collide and tie);
 * sum/product over all-integer input return exact Int (upstream always
 * returned Float via f64 accumulation); aggregation names are the
 * user-facing lowercase strings (upstream named its consts `AGGR_*` and
 * stored that in `Aggregation::name`, relying on `strip_prefix("AGGR_")`
 * at display sites — a port of those sites must not carry the strip).
 * `max`'s type-error message says 'max' (the original copy-pasted
 * 'min'). Mixed int/float sums keep exact integer subtotals until the
 * first float, so results can differ from the original's all-f64 fold
 * (never less accurately); float addition order-dependence is inherited.
 */

//! Aggregations: folds over the rows a rule produces.
//!
//! An aggregation is a fold over rule outputs, and it comes in two
//! disciplines:
//!
//! - A **meet aggregation** is a semilattice fold — idempotent, associative,
//!   with [`MeetAggrObj::init_val`] as identity — so folding a row again, or
//!   in a different grouping, cannot corrupt the result. That is what makes
//!   it safe *inside* recursion: the fixpoint iteration folds as it derives.
//! - A **normal aggregation** is an ordinary fold, finalized only after the
//!   fixpoint: it sees each row exactly once and produces its answer at the
//!   end.
//!
//! [`AggrKind`] binds an aggregation's kind to its implementation in one
//! type: a `Meet` can only be declared with a meet form (plus the normal
//! form it also supports outside recursion), a `Normal` only with a normal
//! form, and a name is dispatched exactly once, in [`parse_aggr`]. Upstream
//! carried the same information as a `bool` plus two `Option` trait-object
//! fields filled in later by string dispatch — a shape in which kind and
//! implementation could disagree, and in which `Clone` silently dropped the
//! ops. That state is unrepresentable here.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Debug, Formatter};

use miette::{Result, bail, ensure, miette};
use rand::prelude::*;

use crate::data::value::{DataValue, Num, NumRepr};
use crate::data::value::data_value_any;

/// Private supertrait seal for aggregation op traits — crate visibility
/// alone is not the seal.
pub(crate) mod seal {
    pub trait Sealed {}
}

/// An ordinary fold over rows: `set` feeds one row's value, `get` produces
/// the final answer. Runs only after the fixpoint, seeing each row once.
/// Sealed: only admitted structs in this module / sketch wrappers implement it.
pub(crate) trait NormalAggrObj: Send + Sync + seal::Sealed {
    fn set(&mut self, value: &DataValue) -> Result<()>;
    fn get(&self) -> Result<DataValue>;
}

/// Running state of a meet fold. `Empty` is the lattice identity when it
/// has no finite [`DataValue`] spelling (and the uniform "not yet folded"
/// state); `Value` holds the running result — including [`DataValue::Null`]
/// when Null is real data, never when it means "empty."
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MeetAccum {
    Empty,
    Value(DataValue),
}

impl MeetAccum {
    /// Wrap a derived / incoming datum as an accumulator value.
    pub(crate) fn from_derived(v: DataValue) -> Self {
        Self::Value(v)
    }

    /// Materialize for storage outside the fold (identity rows, head
    /// tuples). `Empty` becomes [`DataValue::Null`] as a *result* meaning
    /// "no input," not as an in-fold sentinel — the fold matches on
    /// [`MeetAccum::Empty`], never on `Null`.
    pub(crate) fn to_value(&self) -> DataValue {
        match self {
            MeetAccum::Empty => DataValue::Null,
            MeetAccum::Value(v) => v.clone(),
        }
    }
}

/// A semilattice fold, safe inside recursion. `init_val` is the identity
/// element; `update` folds one accumulator into another in place.
///
/// `update` must return `true` iff the accumulator actually changed. The
/// flag gates delta propagation in recursive evaluation, so it is not
/// cosmetic: a false "unchanged" reaches a premature fixpoint (missing
/// answers), a false "changed" merely re-propagates.
pub(crate) trait MeetAggrObj: Send + Sync + seal::Sealed {
    fn init_val(&self) -> MeetAccum;
    fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool>;
}

/// Sealed exhaustively-matched normal aggregation: the dispatch surface.
/// Kind and implementation cannot drift — every name maps to one variant.
pub(crate) enum NormalAggr {
    And(AggrAnd),
    Or(AggrOr),
    Unique(AggrUnique),
    GroupCount(AggrGroupCount),
    CountUnique(AggrCountUnique),
    Union(AggrUnion),
    Intersection(AggrIntersection),
    Collect(AggrCollect),
    ChoiceRand(AggrChoiceRand),
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
    Hll(crate::data::sketch::aggr::AggrHll),
    HllSketch(crate::data::sketch::aggr::AggrHllSketch),
    HllUnion(crate::data::sketch::aggr::AggrHllUnion),
    CountMin(crate::data::sketch::aggr::AggrCountMin),
    TDigest(crate::data::sketch::aggr::AggrTDigest),
    Quantile(crate::data::sketch::aggr::AggrQuantile),
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
            Self::ChoiceRand(a) => a.set(value),
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
            Self::ChoiceRand(a) => a.get(),
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
    HllUnion(crate::data::sketch::aggr::MeetAggrHllUnion),
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

/// Builds a fresh normal fold. `args` are the aggregation's compile-time
/// arguments (only `collect` takes one today); validation happens at
/// construction, not per row.
pub(crate) type NormalAggrFactory = fn(&[DataValue]) -> Result<NormalAggr>;

/// Builds the meet form of a meet aggregation. Meets take no arguments.
pub(crate) type MeetAggrFactory = fn() -> MeetAggr;

/// What an aggregation *is*. A `Meet` can only be declared with a meet
/// factory (plus the normal form every aggregation has); a `Normal` has no
/// meet form to offer, so asking it for one is unrepresentable rather than
/// a runtime surprise.
#[derive(Clone, Copy)]
pub(crate) enum AggrKind {
    /// A semilattice fold, evaluable inside recursion — and outside it,
    /// via its normal form.
    Meet {
        meet: MeetAggrFactory,
        normal: NormalAggrFactory,
    },
    /// An ordinary fold, evaluable only after the fixpoint.
    Normal { normal: NormalAggrFactory },
}

/// A named aggregation: the name as the user writes it, bound to the kind
/// that says how it may be evaluated. This is a `Copy` descriptor — the
/// folding state lives in the objects its factories build, so cloning is
/// lossless by construction.
#[derive(Clone, Copy)]
pub(crate) struct Aggregation {
    pub(crate) name: &'static str,
    pub(crate) kind: AggrKind,
}

impl Aggregation {
    /// Whether this aggregation may run inside recursion.
    pub(crate) fn is_meet(&self) -> bool {
        matches!(self.kind, AggrKind::Meet { .. })
    }

    /// The meet form, if this is a meet aggregation.
    pub(crate) fn meet_op(&self) -> Option<MeetAggr> {
        match self.kind {
            AggrKind::Meet { meet, .. } => Some(meet()),
            AggrKind::Normal { .. } => None,
        }
    }

    /// A fresh normal fold (every aggregation, meet included, has one).
    pub(crate) fn normal_op(&self, args: &[DataValue]) -> Result<NormalAggr> {
        match self.kind {
            AggrKind::Meet { normal, .. } | AggrKind::Normal { normal } => normal(args),
        }
    }
}

/// Identity is the name alone; the factories are determined by it.
impl PartialEq for Aggregation {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for Aggregation {}

impl Debug for Aggregation {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Aggr<{}>", self.name)
    }
}

/// Declares a meet aggregation: one const binding the name to its meet form
/// and its normal form together, so the kind can never drift from the
/// implementations.
macro_rules! meet_aggr {
    ($aggr:ident, $name:literal, $meet_var:ident, $meet:expr, $norm_var:ident, $normal:ty) => {
        const $aggr: Aggregation = Aggregation {
            name: $name,
            kind: AggrKind::Meet {
                meet: || MeetAggr::$meet_var($meet),
                normal: |_| Ok(NormalAggr::$norm_var(<$normal>::default())),
            },
        };
    };
}

/// Declares a normal-only aggregation.
macro_rules! normal_aggr {
    ($aggr:ident, $name:literal, $norm_var:ident, $normal:ty) => {
        const $aggr: Aggregation = Aggregation {
            name: $name,
            kind: AggrKind::Normal {
                normal: |_| Ok(NormalAggr::$norm_var(<$normal>::default())),
            },
        };
    };
}

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
                // The flag gates delta propagation in recursive evaluation:
                // it must report whether the stored value changed. Upstream
                // returned `old == *l` — inverted relative to every other
                // meet op — so a recursive `and` announced "unchanged" on
                // exactly the update that changed it, reaching a premature
                // fixpoint.
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
                // Same inverted-flag fix as `MeetAggrAnd::update`.
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

/// Set union as a meet: the lattice of sets under `∪`, identity `∅`. The
/// accumulator is kept as a `Set`; a `List` seed is canonicalized on first
/// contact.
pub(crate) struct MeetAggrUnion;

impl seal::Sealed for MeetAggrUnion {}

impl MeetAggrObj for MeetAggrUnion {
    fn init_val(&self) -> MeetAccum {
        MeetAccum::Value(DataValue::Set(BTreeSet::new()))
    }

    fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool> {
        if matches!(right, MeetAccum::Empty) {
            return Ok(false);
        }
        if matches!(left, MeetAccum::Empty) {
            *left = right.clone();
            return Ok(true);
        }
        let (MeetAccum::Value(left), MeetAccum::Value(right)) = (left, right) else {
            unreachable!("Empty handled above");
        };
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
}

meet_aggr!(
    AGGR_INTERSECTION,
    "intersection",
    Intersection,
    MeetAggrIntersection,
    Intersection,
    AggrIntersection
);

/// Set intersection of list-valued rows, as a fold. `None` until the first
/// row: the identity of intersection is "everything", which has no finite
/// representation.
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

/// Set intersection as a meet. The identity is the missing top element
/// ("everything") — [`MeetAccum::Empty`], never [`DataValue::Null`].
pub(crate) struct MeetAggrIntersection;

impl seal::Sealed for MeetAggrIntersection {}

impl MeetAggrObj for MeetAggrIntersection {
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
        let (MeetAccum::Value(left), MeetAccum::Value(right)) = (left, right) else {
            unreachable!("Empty handled above");
        };
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

const AGGR_COLLECT: Aggregation = Aggregation {
    name: "collect",
    kind: AggrKind::Normal {
        normal: collect_factory,
    },
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

normal_aggr!(AGGR_CHOICE_RAND, "choice_rand", ChoiceRand, AggrChoiceRand);

/// A uniformly random row, by reservoir sampling with a reservoir of one:
/// the n-th row replaces the choice with probability 1/n.
pub(crate) struct AggrChoiceRand {
    count: usize,
    value: DataValue,
}

impl Default for AggrChoiceRand {
    fn default() -> Self {
        Self {
            count: 0,
            value: DataValue::Null,
        }
    }
}

impl seal::Sealed for AggrChoiceRand {}

impl NormalAggrObj for AggrChoiceRand {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        self.count += 1;
        let prob = 1. / (self.count as f64);
        let rd = rand::rng().random::<f64>();
        if rd < prob {
            self.value = value.clone();
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(self.value.clone())
    }
}

normal_aggr!(AGGR_COUNT, "count", Count, AggrCount);

/// How many rows there were.
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

/// Sample variance from running sums: n, Σx, Σx², finalized as
/// (Σx² − (Σx)²/n) / (n − 1).
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
            v @ (data_value_any!()) => bail!("cannot compute 'variance': encountered value {:?}", v),
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

/// Sample standard deviation: the square root of [`AggrVariance`]'s result.
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
            v @ (data_value_any!()) => bail!("cannot compute 'std_dev': encountered value {:?}", v),
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

/// Exact-while-possible numeric accumulator for `sum` and `product`:
/// integer inputs fold in `i128`; the first float input — or an `i128`
/// overflow mid-fold — demotes the whole fold to `f64` for good.
/// Finalization yields an exact `Int` when the fold stayed integral and
/// the total fits in `i64`, and a `Float` otherwise. Upstream folded
/// everything through `f64`, so all-integer sums came back as inexact
/// floats.
#[derive(Clone, Copy)]
enum NumAccum {
    Int(i128),
    Float(f64),
}

impl NumAccum {
    /// Folds one input with `int_op`/`float_op`, demoting to `f64` on the
    /// first float or on `i128` overflow.
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

    /// The final value: an exact `Int` if the fold stayed integral and in
    /// `i64` range, a `Float` otherwise.
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

/// The sum, accumulated exactly via [`NumAccum`]: all-integer input sums
/// to an exact `Int` (promoted to `Float` only if it overflows `i64`); any
/// float input makes the result a `Float`, as upstream always returned.
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

/// The product, accumulated exactly via [`NumAccum`]: all-integer input
/// multiplies to an exact `Int` (promoted to `Float` on the first float or
/// past `i64` range).
pub(crate) struct AggrProduct {
    product: NumAccum,
}

impl Default for AggrProduct {
    fn default() -> Self {
        Self {
            product: NumAccum::Int(1),
        }
    }
}

impl seal::Sealed for AggrProduct {}

impl NormalAggrObj for AggrProduct {
    fn set(&mut self, value: &DataValue) -> Result<()> {
        match value {
            DataValue::Num(n) => {
                self.product = self.product.fold(n, i128::checked_mul, |a, b| a * b)
            }
            v @ (data_value_any!()) => bail!("cannot compute 'product': encountered value {:?}", v),
        }
        Ok(())
    }

    fn get(&self) -> Result<DataValue> {
        Ok(self.product.finish())
    }
}

meet_aggr!(AGGR_MIN, "min", Min, MeetAggrMin, Min, AggrMin);

/// The numerical minimum, ignoring nulls; `Null` *result* when no row had
/// a number. In-state absence is [`Option::None`], never a Null sentinel.
pub(crate) struct AggrMin {
    found: Option<DataValue>,
}

impl Default for AggrMin {
    fn default() -> Self {
        Self { found: None }
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
                // Compare via `Num`'s exact total order (the same order the
                // memcmp key encoding preserves): upstream compared through
                // `f64`, where distinct integers beyond 2^53 collide and tie.
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

/// Minimum as a meet: numbers under `min` in `Num`'s exact total order,
/// with [`MeetAccum::Empty`] as the identity (Null inputs are skipped, as
/// in the normal form — they are not the empty sentinel).
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
        // Null inputs are ignored (same as the normal form), not treated as
        // identity — Empty is the only empty state.
        if *right_v == DataValue::Null {
            return Ok(false);
        }
        // A materialized Empty (`Null` result) must not linger as a
        // candidate — Min never accumulates Null.
        if matches!(left, MeetAccum::Empty) || matches!(left, MeetAccum::Value(DataValue::Null)) {
            *left = right.clone();
            return Ok(true);
        }
        let MeetAccum::Value(left_v) = left else {
            unreachable!("Empty/Null handled above");
        };
        // Exact `Num` comparison; see `AggrMin::set`.
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

meet_aggr!(AGGR_MAX, "max", Max, MeetAggrMax, Max, AggrMax);

/// The numerical maximum, ignoring nulls; `Null` *result* when no row had
/// a number. In-state absence is [`Option::None`], never a Null sentinel.
pub(crate) struct AggrMax {
    found: Option<DataValue>,
}

impl Default for AggrMax {
    fn default() -> Self {
        Self { found: None }
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

/// Maximum as a meet: numbers under `max` in `Num`'s exact total order,
/// with [`MeetAccum::Empty`] as the identity (Null inputs are skipped, as
/// in the normal form — they are not the empty sentinel).
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
        // Same as Min: Null is never a Max candidate, only Empty is empty.
        if matches!(left, MeetAccum::Empty) || matches!(left, MeetAccum::Value(DataValue::Null)) {
            *left = right.clone();
            return Ok(true);
        }
        let MeetAccum::Value(left_v) = left else {
            unreachable!("Empty/Null handled above");
        };
        // Exact `Num` comparison; see `AggrMin::set`.
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

normal_aggr!(AGGR_LATEST_BY, "latest_by", LatestBy, AggrLatestBy);

/// Of `[payload, cost]` pairs, the payload whose cost sorts greatest.
/// No-candidate state is [`Option::None`], never [`DataValue::Null`].
pub(crate) struct AggrLatestBy {
    found: Option<DataValue>,
    cost: Option<DataValue>,
}

impl Default for AggrLatestBy {
    fn default() -> Self {
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
        Ok(self.found.clone().unwrap_or(DataValue::Null))
    }
}

normal_aggr!(AGGR_SMALLEST_BY, "smallest_by", SmallestBy, AggrSmallestBy);

/// Of `[payload, cost]` pairs, the payload whose cost sorts least.
/// No-candidate state is [`Option::None`], never [`DataValue::Null`].
pub(crate) struct AggrSmallestBy {
    found: Option<DataValue>,
    cost: Option<DataValue>,
}

impl Default for AggrSmallestBy {
    fn default() -> Self {
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
/// Absence of a candidate is [`Option::None`], never `f64::INFINITY`.
pub(crate) struct AggrMinCost {
    found: Option<DataValue>,
    cost: Option<f64>,
}

impl Default for AggrMinCost {
    fn default() -> Self {
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
        // Empty accumulator → Null pair. Never encode absence as +Inf.
        Ok(DataValue::List(vec![
            self.found.clone().unwrap_or(DataValue::Null),
            self.cost
                .map(DataValue::from)
                .unwrap_or(DataValue::Null),
        ]))
    }
}

/// Least-cost pair as a meet; ties keep the incumbent (a deliberate,
/// order-dependent tie-break, like `choice`).
pub(crate) struct MeetAggrMinCost;

impl seal::Sealed for MeetAggrMinCost {}

impl MeetAggrObj for MeetAggrMinCost {
    fn init_val(&self) -> MeetAccum {
        // Empty lattice — never a Null/Inf sentinel pair.
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

/// The shortest list-valued row (a path, typically); ties keep the
/// incumbent.
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
/// No-candidate state is [`Option::None`], never [`DataValue::Null`].
pub(crate) struct AggrChoice {
    found: Option<DataValue>,
}

impl Default for AggrChoice {
    fn default() -> Self {
        Self { found: None }
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
        Ok(self.found.clone().unwrap_or(DataValue::Null))
    }
}

/// First-seen as a meet: idempotent and associative, deliberately not
/// commutative — which value wins is arbitrary by contract. [`DataValue::Null`]
/// is ordinary data (adopted from Empty); only [`MeetAccum::Empty`] is vacant.
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
/// The changed flag is exact: upstream reported `true` whenever the
/// operands differed, even when the fold left the accumulator unchanged
/// (e.g. `0x00 & 0x0f`).
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
/// The changed flag is exact, as in [`MeetAggrBitAnd`].
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

/// Bytewise XOR of equal-length byte strings. Not a meet: XOR is not
/// idempotent (folding a row twice cancels it), so it must never run
/// inside recursion.
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

/// The one place a name becomes an aggregation: kind and implementations
/// are decided here, together, and never re-dispatched.
pub(crate) fn parse_aggr(name: &str) -> Option<Aggregation> {
    Some(match name {
        "and" => AGGR_AND,
        "or" => AGGR_OR,
        "unique" => AGGR_UNIQUE,
        "group_count" => AGGR_GROUP_COUNT,
        "union" => AGGR_UNION,
        "intersection" => AGGR_INTERSECTION,
        "count" => AGGR_COUNT,
        "count_unique" => AGGR_COUNT_UNIQUE,
        "variance" => AGGR_VARIANCE,
        "std_dev" => AGGR_STD_DEV,
        "sum" => AGGR_SUM,
        "product" => AGGR_PRODUCT,
        "min" => AGGR_MIN,
        "max" => AGGR_MAX,
        "mean" => AGGR_MEAN,
        "choice" => AGGR_CHOICE,
        "collect" => AGGR_COLLECT,
        "shortest" => AGGR_SHORTEST,
        "min_cost" => AGGR_MIN_COST,
        "bit_and" => AGGR_BIT_AND,
        "bit_or" => AGGR_BIT_OR,
        "bit_xor" => AGGR_BIT_XOR,
        "latest_by" => AGGR_LATEST_BY,
        "smallest_by" => AGGR_SMALLEST_BY,
        "choice_rand" => AGGR_CHOICE_RAND,
        // Deterministic sketches (HyperLogLog / Count-Min / t-digest) are
        // dispatched from their own module; only `hll_union` is a meet.
        _ => return crate::data::sketch::aggr::parse_sketch_aggr(name),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn v(d: DataValue) -> MeetAccum {
        MeetAccum::Value(d)
    }

    /// One fold with the changed-flag contract pinned in both directions:
    /// after every `update`, the flag must equal "the stored value
    /// changed". The comparison goes through `canon`, because some
    /// accumulators re-represent without changing meaning (`union`
    /// canonicalizes a `List` seed to a `Set` on first contact).
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

    /// `meet(a, b)` as a binary operation on accumulators: fold `b` into a
    /// copy of `a`, with the changed flag checked.
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

    /// Identity canonicalizer, for ops whose accumulator is already the
    /// value itself.
    fn ident(a: &MeetAccum) -> MeetAccum {
        a.clone()
    }

    /// Canonicalizer for the set-valued ops, whose accumulators may be
    /// `List` or `Set` representations of the same set.
    fn as_set(a: &MeetAccum) -> MeetAccum {
        match a {
            MeetAccum::Empty => MeetAccum::Empty,
            MeetAccum::Value(DataValue::List(l)) => {
                MeetAccum::Value(DataValue::Set(l.iter().cloned().collect()))
            }
            MeetAccum::Value(v) => MeetAccum::Value(v.clone()),
        }
    }

    /// The semilattice laws for one meet op over three sample values:
    /// idempotency (with an exact `false` changed-flag), `init_val` as
    /// identity, associativity, and — for ops without an order-dependent
    /// tie-break — commutativity. Comparisons go through `canon`, and
    /// every single fold pins the changed flag in both directions via
    /// [`update_checked`].
    fn assert_semilattice_laws(
        op: &dyn MeetAggrObj,
        samples: [&MeetAccum; 3],
        canon: fn(&MeetAccum) -> MeetAccum,
        commutative: bool,
    ) {
        let [x, y, z] = samples;
        // Idempotent: meet(x, x) leaves x unchanged and must say so.
        for a in samples {
            let mut acc = a.clone();
            let changed = update_checked(op, &mut acc, a, canon);
            assert!(!changed, "meet(x, x) reported a change for x = {a:?}");
            assert_eq!(canon(&acc), canon(a), "meet(x, x) altered x = {a:?}");
        }
        // Identity: meet(init_val, x) == x.
        for a in samples {
            let mut acc = op.init_val();
            update_checked(op, &mut acc, a, canon);
            assert_eq!(canon(&acc), canon(a), "meet(init, x) != x for x = {a:?}");
        }
        // Associative: meet(meet(x, y), z) == meet(x, meet(y, z)).
        let l = alg_meet(op, &alg_meet(op, x, y, canon), z, canon);
        let r = alg_meet(op, x, &alg_meet(op, y, z, canon), canon);
        assert_eq!(
            canon(&l),
            canon(&r),
            "associativity failed on {x:?}, {y:?}, {z:?}"
        );
        // Commutative: meet(x, y) == meet(y, x). The tie-arbitrary ops
        // (choice, shortest, min_cost) keep the incumbent on ties by
        // contract, so for them only the laws above are checked.
        if commutative {
            assert_eq!(
                canon(&alg_meet(op, x, y, canon)),
                canon(&alg_meet(op, y, x, canon)),
                "commutativity failed on {x:?}, {y:?}"
            );
        }
    }

    /// Any value `choice` may legally see.
    fn arb_value() -> impl Strategy<Value = DataValue> {
        prop_oneof![
            Just(DataValue::Null),
            any::<bool>().prop_map(DataValue::from),
            any::<i64>().prop_map(DataValue::from),
            (-1.0e6..1.0e6f64).prop_map(DataValue::from),
            "[a-z]{0,6}".prop_map(DataValue::from),
        ]
    }

    /// Numbers across the full `i64` range, mixed with floats: min/max
    /// compare via the exact `Num` total order, so huge integers that
    /// collide as `f64` must still be distinguished, and the laws must
    /// hold across the Int/Float boundary.
    fn arb_num() -> impl Strategy<Value = DataValue> {
        prop_oneof![
            any::<i64>().prop_map(DataValue::from),
            (-1.0e9..1.0e9f64).prop_map(DataValue::from),
        ]
    }

    /// Small lists over a small element domain, so intersections are
    /// non-trivially inhabited.
    fn arb_small_list() -> impl Strategy<Value = DataValue> {
        prop::collection::vec((0i64..6).prop_map(DataValue::from), 0..5).prop_map(DataValue::List)
    }

    /// `[payload, cost]` pairs with finite numeric costs.
    fn arb_costed_pair() -> impl Strategy<Value = DataValue> {
        ((0i64..100), (-1.0e3..1.0e3f64)).prop_map(|(payload, cost)| {
            DataValue::List(vec![DataValue::from(payload), DataValue::from(cost)])
        })
    }

    /// Three byte strings of one shared length, as the bit ops require.
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
            z in arb_small_list(),
        ) {
            let (x, y, z) = (v(x), v(y), v(z));
            assert_semilattice_laws(&MeetAggrUnion, [&x, &y, &z], as_set, true);
            assert_semilattice_laws(&MeetAggrIntersection, [&x, &y, &z], as_set, true);
        }

        #[test]
        fn meet_laws_shortest(
            x in arb_small_list(),
            y in arb_small_list(),
            z in arb_small_list(),
        ) {
            let (x, y, z) = (v(x), v(y), v(z));
            assert_semilattice_laws(&MeetAggrShortest, [&x, &y, &z], ident, false);
        }

        #[test]
        fn meet_laws_min_cost(
            x in arb_costed_pair(),
            y in arb_costed_pair(),
            z in arb_costed_pair(),
        ) {
            let (x, y, z) = (v(x), v(y), v(z));
            assert_semilattice_laws(&MeetAggrMinCost, [&x, &y, &z], ident, false);
        }
    }

    /// Regression for the upstream inversion: the changed flag must be
    /// `true` iff the stored value changed — in both directions.
    #[test]
    fn and_or_changed_flag() {
        let t = v(DataValue::from(true));
        let f = v(DataValue::from(false));

        // and: true & false changes the value; the flag must say so.
        let mut acc = t.clone();
        assert!(MeetAggrAnd.update(&mut acc, &f).unwrap());
        assert_eq!(acc, f);
        // and: false & true leaves it false; the flag must say unchanged.
        assert!(!MeetAggrAnd.update(&mut acc, &t).unwrap());
        assert_eq!(acc, f);
        // and: true & true is unchanged.
        let mut acc = t.clone();
        assert!(!MeetAggrAnd.update(&mut acc, &t).unwrap());

        // or: false | true changes the value; the flag must say so.
        let mut acc = f.clone();
        assert!(MeetAggrOr.update(&mut acc, &t).unwrap());
        assert_eq!(acc, t);
        // or: true | false leaves it true; the flag must say unchanged.
        assert!(!MeetAggrOr.update(&mut acc, &f).unwrap());
        assert_eq!(acc, t);
        // or: false | false is unchanged.
        let mut acc = f.clone();
        assert!(!MeetAggrOr.update(&mut acc, &f).unwrap());
    }

    /// Regression: the bit-op meets must report the changed flag exactly.
    /// Upstream returned `true` whenever the operands differed, even when
    /// the fold left the accumulator unchanged.
    #[test]
    fn bit_meet_changed_flag_exact() {
        let zero = v(DataValue::Bytes(vec![0x00]));
        let ones = v(DataValue::Bytes(vec![0xff]));
        let some = v(DataValue::Bytes(vec![0x0f]));

        // and: 0x00 & 0x0f leaves 0x00 — the flag must say unchanged.
        let mut acc = zero.clone();
        assert!(!MeetAggrBitAnd.update(&mut acc, &some).unwrap());
        assert_eq!(acc, zero);
        // and: 0xff & 0x0f changes the value to 0x0f.
        let mut acc = ones.clone();
        assert!(MeetAggrBitAnd.update(&mut acc, &some).unwrap());
        assert_eq!(acc, some);

        // or: 0xff | 0x0f leaves 0xff — the flag must say unchanged.
        let mut acc = ones.clone();
        assert!(!MeetAggrBitOr.update(&mut acc, &some).unwrap());
        assert_eq!(acc, ones);
        // or: 0x00 | 0x0f changes the value to 0x0f.
        let mut acc = zero.clone();
        assert!(MeetAggrBitOr.update(&mut acc, &some).unwrap());
        assert_eq!(acc, some);
    }

    /// Regression: min/max must compare exactly. Upstream compared through
    /// `f64`, where distinct integers beyond 2^53 (`i64::MAX` vs
    /// `i64::MAX - 1`, `2^53 + 1` vs `2^53`) collide and tie, silently
    /// keeping whichever arrived first.
    #[test]
    fn min_max_exact_beyond_f64() {
        for (lo, hi) in [
            (DataValue::from(i64::MAX - 1), DataValue::from(i64::MAX)),
            (
                DataValue::from(1i64 << 53),
                DataValue::from((1i64 << 53) + 1),
            ),
        ] {
            let (lo, hi) = (v(lo), v(hi));
            // Meet forms, both argument orders.
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

            // Normal forms, both arrival orders.
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

        // The `Num` order compares EXACT real values, not f64-rounded
        // ones: `2^53 + 1` (Int, exact) is genuinely GREATER than
        // `2^53.0` (Float), where the old lossy `i as f64` comparison
        // would have collided them. So min picks the float, max the int.
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

    /// Ratifies the deviation from upstream: sum/product over all-integer
    /// input return an exact `Int` (upstream always returned `Float` via
    /// `f64` accumulation); any float input, or an integer total past
    /// `i64` range, yields a `Float`.
    #[test]
    fn sum_product_exact_int() {
        // Sum of all-integer input is an exact Int.
        let mut op = AggrSum::default();
        for i in [1i64, 2, 3] {
            op.set(&DataValue::from(i)).unwrap();
        }
        assert_eq!(op.get().unwrap(), DataValue::from(6i64));

        // Any float input makes the result a Float.
        let mut op = AggrSum::default();
        op.set(&DataValue::from(1i64)).unwrap();
        op.set(&DataValue::from(2.0)).unwrap();
        assert_eq!(op.get().unwrap(), DataValue::from(3.0));

        // Exact beyond 2^53, where f64 accumulation would round.
        let a = (1i64 << 53) + 1;
        let b = (1i64 << 53) + 3;
        let mut op = AggrSum::default();
        op.set(&DataValue::from(a)).unwrap();
        op.set(&DataValue::from(b)).unwrap();
        assert_eq!(op.get().unwrap(), DataValue::from(a + b));

        // A total past i64 range promotes to Float at finalization...
        let mut op = AggrSum::default();
        op.set(&DataValue::from(i64::MAX)).unwrap();
        op.set(&DataValue::from(i64::MAX)).unwrap();
        assert_eq!(op.get().unwrap(), DataValue::from(2.0 * i64::MAX as f64));
        // ...but an i128 running total that returns to i64 range stays
        // exact: MAX + MAX + MIN == MAX - 1.
        let mut op = AggrSum::default();
        op.set(&DataValue::from(i64::MAX)).unwrap();
        op.set(&DataValue::from(i64::MAX)).unwrap();
        op.set(&DataValue::from(i64::MIN)).unwrap();
        assert_eq!(op.get().unwrap(), DataValue::from(i64::MAX - 1));

        // Product of all-integer input is an exact Int.
        let mut op = AggrProduct::default();
        for i in [2i64, 3, 4] {
            op.set(&DataValue::from(i)).unwrap();
        }
        assert_eq!(op.get().unwrap(), DataValue::from(24i64));

        // Any float input makes the result a Float.
        let mut op = AggrProduct::default();
        op.set(&DataValue::from(2i64)).unwrap();
        op.set(&DataValue::from(0.5)).unwrap();
        assert_eq!(op.get().unwrap(), DataValue::from(1.0));

        // An integer product past i64 range promotes to Float.
        let mut op = AggrProduct::default();
        op.set(&DataValue::from(i64::MAX)).unwrap();
        op.set(&DataValue::from(4i64)).unwrap();
        assert_eq!(op.get().unwrap(), DataValue::from(i64::MAX as f64 * 4.0));

        // Empty folds: the identities, as exact Ints.
        assert_eq!(AggrSum::default().get().unwrap(), DataValue::from(0i64));
        assert_eq!(AggrProduct::default().get().unwrap(), DataValue::from(1i64));
    }

    /// Every name resolves to the kind its implementation claims: meets
    /// yield a meet op (and a normal form), normals never yield a meet op.
    #[test]
    fn parse_aggr_kind_agrees_with_ops() {
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
            "choice_rand",
        ];
        for name in MEETS {
            let aggr = parse_aggr(name).unwrap();
            assert!(aggr.is_meet(), "{name} must be a meet");
            assert!(aggr.meet_op().is_some(), "{name} must yield a meet op");
            aggr.normal_op(&[]).unwrap();
        }
        for name in NORMALS {
            let aggr = parse_aggr(name).unwrap();
            assert!(!aggr.is_meet(), "{name} must not be a meet");
            assert!(aggr.meet_op().is_none(), "{name} must yield no meet op");
            aggr.normal_op(&[]).unwrap();
        }
        assert!(parse_aggr("no_such_aggr").is_none());
    }

    /// `collect`'s optional limit is validated at construction and honored
    /// during the fold.
    #[test]
    fn collect_limit_argument() {
        let aggr = parse_aggr("collect").unwrap();
        let mut op = aggr.normal_op(&[DataValue::from(2)]).unwrap();
        for i in 0..5 {
            op.set(&DataValue::from(i)).unwrap();
        }
        assert_eq!(
            op.get().unwrap(),
            DataValue::List(vec![DataValue::from(0), DataValue::from(1)])
        );
        assert!(aggr.normal_op(&[DataValue::from(0)]).is_err());
        assert!(aggr.normal_op(&[DataValue::from("two")]).is_err());
    }

    /// F1 (fix-wave review, mutation-proven hole): the i128-overflow
    /// demotion arm of `NumAccum` is reachable in three rows of `product`;
    /// a corrupted operand there must fail loudly.
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
            other @ (data_value_any!()) => panic!("expected float after i128 overflow, got {other:?}"),
        }
    }

    /// F2 (fix-wave review, mutation-proven hole): the laws alone cannot
    /// tell intersection from union — both are semilattices. Concrete value
    /// oracles pin which operation actually runs, both directions.
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

    /// Closure: `DataValue::Null` is ordinary set-element data under meet
    /// intersection — it round-trips through the fold. Empty is only
    /// [`MeetAccum::Empty`], never Null-as-sentinel.
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

        // Null alone as a singleton set is data, not identity: folding it
        // into Empty adopts Value([Null]), and Empty on the right is a no-op.
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
