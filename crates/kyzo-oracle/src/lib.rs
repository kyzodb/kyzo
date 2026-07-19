/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Naive reference semantics — the judge's contract.
//!
//! Independent of the production evaluator: same question, independent
//! answer. Aggregation folds cross the crate wall by dependency inversion
//! ([`AggrFold`]); trials injects the engine's real fold ops. Until then
//! the oracle ships a built-in meet/normal set sufficient for its own
//! module tests.

#![forbid(unsafe_code)]

pub mod eval;
pub mod incremental;
pub mod temporal;

use std::sync::Arc;

use kyzo_model::value::DataValue;

pub use eval::{
    Bindings, FixedRule, HeadAggr, HeadClass, Literal, Name, NameIntroduction, OracleBudget,
    Polarity, Program, Rejection, Rel, Rule, Term, body_bindings_from, check_safety,
    check_stratifiable, check_wellformed, dependency_edges, derived_rows, ground, head_classes,
    literal_rows, naive_eval, naive_eval_at, naive_eval_at_budgeted, strata, unify,
    unstratifiable_corpus,
};
pub use incremental::{edb_relations, head_is_derivable, incremental_eval, topological_order};
pub use temporal::{
    AsOf, Axis, ClaimPolarity, ComposeNetOutOfRange, Event, Interval, OPEN_END,
    ReservedValidInstant, SignedFact, compose, derive_intervals, diff, resolve, resolve_events,
    resolve_relation,
};

/// Running state of a meet fold. `Empty` is the lattice identity when it
/// has no finite [`DataValue`] spelling; `Value` holds the running result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MeetAccum {
    Empty,
    Value(DataValue),
}

impl MeetAccum {
    pub fn from_derived(v: DataValue) -> Self {
        Self::Value(v)
    }

    pub fn to_value(&self) -> DataValue {
        match self {
            MeetAccum::Empty => DataValue::Null,
            MeetAccum::Value(v) => v.clone(),
        }
    }
}

/// Ordinary fold accumulator: `set` feeds one row, `get` finalizes.
pub trait NormalAccum: Send {
    fn set(&mut self, value: &DataValue) -> Result<(), String>;
    fn get(&self) -> Result<DataValue, String>;
}

/// Semilattice fold op, safe inside recursion.
pub trait MeetOp: Send {
    fn init_val(&self) -> MeetAccum;
    fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool, String>;
}

/// Aggregation injection seam keyed by aggregate name.
///
/// Trials binds the engine's real fold ops from `exec/fold/aggr.rs`. The
/// oracle's loop stays independent; a simple built-in set
/// ([`builtin_fold`]) covers module tests until that wiring lands.
pub trait AggrFold: Send + Sync {
    fn name(&self) -> &str;
    fn is_meet(&self) -> bool;
    fn fresh_normal(&self, args: &[DataValue]) -> Result<Box<dyn NormalAccum>, String>;
    fn fresh_meet(&self) -> Option<Box<dyn MeetOp>>;
}

/// Look up a built-in fold by the user-facing aggregation name.
pub fn builtin_fold(name: &str) -> Option<Arc<dyn AggrFold>> {
    match name {
        "count" => Some(Arc::new(BuiltinCount)),
        "sum" => Some(Arc::new(BuiltinSum)),
        "min" => Some(Arc::new(BuiltinMin)),
        "max" => Some(Arc::new(BuiltinMax)),
        "and" => Some(Arc::new(BuiltinAnd)),
        "or" => Some(Arc::new(BuiltinOr)),
        _ => None,
    }
}

// ── Built-in meet/normal set (oracle-local; trials replaces for differentials) ──

struct BuiltinCount;
struct CountAccum {
    count: i64,
}
impl NormalAccum for CountAccum {
    fn set(&mut self, _value: &DataValue) -> Result<(), String> {
        self.count += 1;
        Ok(())
    }
    fn get(&self) -> Result<DataValue, String> {
        Ok(DataValue::from(self.count))
    }
}
impl AggrFold for BuiltinCount {
    fn name(&self) -> &str {
        "count"
    }
    fn is_meet(&self) -> bool {
        false
    }
    fn fresh_normal(&self, _args: &[DataValue]) -> Result<Box<dyn NormalAccum>, String> {
        Ok(Box::new(CountAccum { count: 0 }))
    }
    fn fresh_meet(&self) -> Option<Box<dyn MeetOp>> {
        None
    }
}

struct BuiltinSum;
struct SumAccum {
    sum: i64,
}
impl NormalAccum for SumAccum {
    fn set(&mut self, value: &DataValue) -> Result<(), String> {
        match value {
            DataValue::Num(n) => {
                let add = n.as_int().unwrap_or_else(|| n.to_f64() as i64);
                self.sum = self
                    .sum
                    .checked_add(add)
                    .ok_or_else(|| "sum overflow".to_string())?;
                Ok(())
            }
            v => Err(format!("cannot compute 'sum': encountered value {v:?}")),
        }
    }
    fn get(&self) -> Result<DataValue, String> {
        Ok(DataValue::from(self.sum))
    }
}
impl AggrFold for BuiltinSum {
    fn name(&self) -> &str {
        "sum"
    }
    fn is_meet(&self) -> bool {
        false
    }
    fn fresh_normal(&self, _args: &[DataValue]) -> Result<Box<dyn NormalAccum>, String> {
        Ok(Box::new(SumAccum { sum: 0 }))
    }
    fn fresh_meet(&self) -> Option<Box<dyn MeetOp>> {
        None
    }
}

struct BuiltinMin;
struct MinAccum {
    found: Option<DataValue>,
}
impl NormalAccum for MinAccum {
    fn set(&mut self, value: &DataValue) -> Result<(), String> {
        if *value == DataValue::Null {
            return Ok(());
        }
        match &self.found {
            None => {
                self.found = Some(value.clone());
                Ok(())
            }
            Some(found) => match (found, value) {
                (DataValue::Num(l), DataValue::Num(r)) => {
                    if r < l {
                        self.found = Some(value.clone());
                    }
                    Ok(())
                }
                _ => Err("'min' applied to non-numerical values".into()),
            },
        }
    }
    fn get(&self) -> Result<DataValue, String> {
        Ok(self.found.clone().unwrap_or(DataValue::Null))
    }
}
struct MeetMin;
impl MeetOp for MeetMin {
    fn init_val(&self) -> MeetAccum {
        MeetAccum::Empty
    }
    fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool, String> {
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
            MeetAccum::Value(left_v) => match (&*left_v, right_v) {
                (DataValue::Num(l), DataValue::Num(r)) => Ok(if r < l {
                    *left_v = right_v.clone();
                    true
                } else {
                    false
                }),
                _ => Err("'min' applied to non-numerical values".into()),
            },
        }
    }
}
impl AggrFold for BuiltinMin {
    fn name(&self) -> &str {
        "min"
    }
    fn is_meet(&self) -> bool {
        true
    }
    fn fresh_normal(&self, _args: &[DataValue]) -> Result<Box<dyn NormalAccum>, String> {
        Ok(Box::new(MinAccum { found: None }))
    }
    fn fresh_meet(&self) -> Option<Box<dyn MeetOp>> {
        Some(Box::new(MeetMin))
    }
}

struct BuiltinMax;
struct MaxAccum {
    found: Option<DataValue>,
}
impl NormalAccum for MaxAccum {
    fn set(&mut self, value: &DataValue) -> Result<(), String> {
        if *value == DataValue::Null {
            return Ok(());
        }
        match &self.found {
            None => {
                self.found = Some(value.clone());
                Ok(())
            }
            Some(found) => match (found, value) {
                (DataValue::Num(l), DataValue::Num(r)) => {
                    if r > l {
                        self.found = Some(value.clone());
                    }
                    Ok(())
                }
                _ => Err("'max' applied to non-numerical values".into()),
            },
        }
    }
    fn get(&self) -> Result<DataValue, String> {
        Ok(self.found.clone().unwrap_or(DataValue::Null))
    }
}
struct MeetMax;
impl MeetOp for MeetMax {
    fn init_val(&self) -> MeetAccum {
        MeetAccum::Empty
    }
    fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool, String> {
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
            MeetAccum::Value(left_v) => match (&*left_v, right_v) {
                (DataValue::Num(l), DataValue::Num(r)) => Ok(if r > l {
                    *left_v = right_v.clone();
                    true
                } else {
                    false
                }),
                _ => Err("'max' applied to non-numerical values".into()),
            },
        }
    }
}
impl AggrFold for BuiltinMax {
    fn name(&self) -> &str {
        "max"
    }
    fn is_meet(&self) -> bool {
        true
    }
    fn fresh_normal(&self, _args: &[DataValue]) -> Result<Box<dyn NormalAccum>, String> {
        Ok(Box::new(MaxAccum { found: None }))
    }
    fn fresh_meet(&self) -> Option<Box<dyn MeetOp>> {
        Some(Box::new(MeetMax))
    }
}

struct BuiltinAnd;
struct AndAccum {
    accum: bool,
}
impl NormalAccum for AndAccum {
    fn set(&mut self, value: &DataValue) -> Result<(), String> {
        match value {
            DataValue::Bool(v) => {
                self.accum &= *v;
                Ok(())
            }
            v => Err(format!("cannot compute 'and' for {v:?}")),
        }
    }
    fn get(&self) -> Result<DataValue, String> {
        Ok(DataValue::from(self.accum))
    }
}
struct MeetAnd;
impl MeetOp for MeetAnd {
    fn init_val(&self) -> MeetAccum {
        MeetAccum::Value(DataValue::from(true))
    }
    fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool, String> {
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
            (u, v) => Err(format!("cannot compute 'and' for {u:?} and {v:?}")),
        }
    }
}
impl AggrFold for BuiltinAnd {
    fn name(&self) -> &str {
        "and"
    }
    fn is_meet(&self) -> bool {
        true
    }
    fn fresh_normal(&self, _args: &[DataValue]) -> Result<Box<dyn NormalAccum>, String> {
        Ok(Box::new(AndAccum { accum: true }))
    }
    fn fresh_meet(&self) -> Option<Box<dyn MeetOp>> {
        Some(Box::new(MeetAnd))
    }
}

struct BuiltinOr;
struct OrAccum {
    accum: bool,
}
impl NormalAccum for OrAccum {
    fn set(&mut self, value: &DataValue) -> Result<(), String> {
        match value {
            DataValue::Bool(v) => {
                self.accum |= *v;
                Ok(())
            }
            v => Err(format!("cannot compute 'or' for {v:?}")),
        }
    }
    fn get(&self) -> Result<DataValue, String> {
        Ok(DataValue::from(self.accum))
    }
}
struct MeetOr;
impl MeetOp for MeetOr {
    fn init_val(&self) -> MeetAccum {
        MeetAccum::Value(DataValue::from(false))
    }
    fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool, String> {
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
            (u, v) => Err(format!("cannot compute 'or' for {u:?} and {v:?}")),
        }
    }
}
impl AggrFold for BuiltinOr {
    fn name(&self) -> &str {
        "or"
    }
    fn is_meet(&self) -> bool {
        true
    }
    fn fresh_normal(&self, _args: &[DataValue]) -> Result<Box<dyn NormalAccum>, String> {
        Ok(Box::new(OrAccum { accum: false }))
    }
    fn fresh_meet(&self) -> Option<Box<dyn MeetOp>> {
        Some(Box::new(MeetOr))
    }
}
