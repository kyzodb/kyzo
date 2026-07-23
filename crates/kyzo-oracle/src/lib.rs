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

use kyzo_model::value::convert::i128_approx_f64;
use kyzo_model::value::{DataValue, Num, NumRepr};

pub use eval::{
    Bindings, FixedRule, HeadAggr, HeadClass, Literal, Name, NameIntroduction, OracleBudget,
    Polarity, Program, Rejection, Rel, Rule, Term, body_bindings_from, check_safety,
    check_stratifiable, check_wellformed, dependency_edges, derived_rows, edge_facts, ground,
    head_classes, literal_rows, naive_eval, naive_eval_at, naive_eval_at_budgeted, strata,
    transitive_closure, unify, unstratifiable_corpus,
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
    if name == "count" {
        return Some(Arc::new(BuiltinCount));
    }
    if name == "sum" {
        return Some(Arc::new(BuiltinSum));
    }
    if name == "min" {
        return Some(Arc::new(BuiltinMin));
    }
    if name == "max" {
        return Some(Arc::new(BuiltinMax));
    }
    if name == "and" {
        return Some(Arc::new(BuiltinAnd));
    }
    if name == "or" {
        return Some(Arc::new(BuiltinOr));
    }
    None
}

/// Built-in fold, or an unknown-name fold that refuses on every use.
/// Panic-free construction for [`eval::HeadAggr::named`].
pub(crate) fn fold_named(name: &str) -> Arc<dyn AggrFold> {
    match builtin_fold(name) {
        Some(fold) => fold,
        None => Arc::new(UnknownBuiltin {
            name: name.to_string(),
        }),
    }
}

/// Fold that refuses every use — stands in when [`eval::HeadAggr::named`] is
/// given an unknown aggregation so construction stays panic-free and
/// evaluation returns a typed aggr error.
struct UnknownBuiltin {
    name: String,
}
impl AggrFold for UnknownBuiltin {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_meet(&self) -> bool {
        false
    }
    fn fresh_normal(&self, _args: &[DataValue]) -> Result<Box<dyn NormalAccum>, String> {
        Err(format!("unknown aggregation: {}", self.name))
    }
    fn fresh_meet(&self) -> Option<Box<dyn MeetOp>> {
        None
    }
}

// ── Oracle-local fold bodies ──────────────────────────────────────────────
// Re-derived over model vocabulary only. Control flow, naming, and error
// construction deliberately diverge from kyzo-core `exec/fold/aggr.rs` so
// the engine↔oracle differential is not a tautology (zone-oracle law).

// ── count ────────────────────────────────────────────────────────────────

struct BuiltinCount;

/// Row tally: every feed increments, value is ignored (nulls count).
struct CountAccum {
    rows_seen: u64,
}

impl NormalAccum for CountAccum {
    fn set(&mut self, _value: &DataValue) -> Result<(), String> {
        self.rows_seen = self
            .rows_seen
            .checked_add(1)
            .ok_or_else(|| "count fold overflowed u64".to_string())?;
        Ok(())
    }
    fn get(&self) -> Result<DataValue, String> {
        let as_i64 = i64::try_from(self.rows_seen)
            .map_err(|_| "count fold result does not fit i64".to_string())?;
        Ok(DataValue::from(as_i64))
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
        Ok(Box::new(CountAccum { rows_seen: 0 }))
    }
    fn fresh_meet(&self) -> Option<Box<dyn MeetOp>> {
        None
    }
}

// ── sum ──────────────────────────────────────────────────────────────────

struct BuiltinSum;

/// Running total that prefers exact integers, promoting to float only when
/// a float arrives or an i128 add would overflow.
#[derive(Clone, Copy)]
enum RunningTotal {
    Exact(i128),
    Approx(f64),
}

impl RunningTotal {
    fn zero() -> Self {
        RunningTotal::Exact(0)
    }

    fn absorb(self, n: Num) -> Self {
        match (self, n.repr()) {
            (RunningTotal::Exact(acc), NumRepr::Int(i)) => {
                let addend = i128::from(i);
                match acc.checked_add(addend) {
                    Some(sum) => RunningTotal::Exact(sum),
                    None => RunningTotal::Approx(
                        i128_approx_f64(acc) + Num::int(i).to_f64(),
                    ),
                }
            }
            (RunningTotal::Exact(acc), NumRepr::Float(f)) => {
                RunningTotal::Approx(i128_approx_f64(acc) + f)
            }
            (RunningTotal::Approx(acc), NumRepr::Int(i)) => {
                RunningTotal::Approx(acc + Num::int(i).to_f64())
            }
            (RunningTotal::Approx(acc), NumRepr::Float(f)) => RunningTotal::Approx(acc + f),
        }
    }

    fn finish(self) -> DataValue {
        match self {
            RunningTotal::Exact(acc) => match i64::try_from(acc) {
                Ok(i) => DataValue::from(i),
                Err(_acc_exceeds_i64) => DataValue::from(i128_approx_f64(acc)),
            },
            RunningTotal::Approx(f) => DataValue::from(f),
        }
    }
}

struct SumAccum {
    total: RunningTotal,
}

impl NormalAccum for SumAccum {
    fn set(&mut self, value: &DataValue) -> Result<(), String> {
        let DataValue::Num(n) = value else {
            return Err(format!("sum fold: non-numeric input {value:?}"));
        };
        self.total = self.total.absorb(*n);
        Ok(())
    }
    fn get(&self) -> Result<DataValue, String> {
        Ok(self.total.finish())
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
        Ok(Box::new(SumAccum {
            total: RunningTotal::zero(),
        }))
    }
    fn fresh_meet(&self) -> Option<Box<dyn MeetOp>> {
        None
    }
}

// ── min (normal + meet) ──────────────────────────────────────────────────

struct BuiltinMin;

/// Least non-null observation so far; vacant until the first real value.
struct MinAccum {
    least: Option<DataValue>,
}

/// Prefer the numerically extreme of two values — ONE seat (copy_detector).
fn extreme_number(
    a: &DataValue,
    b: &DataValue,
    prefer_b: impl Fn(&Num, &Num) -> bool,
    refuse: &'static str,
) -> Result<DataValue, String> {
    match (a, b) {
        (DataValue::Num(x), DataValue::Num(y)) => {
            if prefer_b(x, y) {
                Ok(b.clone())
            } else {
                Ok(a.clone())
            }
        }
        (kyzo_model::data_value_any!(), kyzo_model::data_value_any!()) => Err(refuse.into()),
    }
}

/// Prefer the numerically smaller of two values; refuse non-numbers.
fn lesser_number(a: &DataValue, b: &DataValue) -> Result<DataValue, String> {
    extreme_number(a, b, |x, y| y < x, "min fold requires numeric operands")
}

/// Absorb one non-null extremum — ONE seat for min/max normal set.
fn set_numeric_extremum(
    cell: &mut Option<DataValue>,
    value: &DataValue,
    pick: fn(&DataValue, &DataValue) -> Result<DataValue, String>,
) -> Result<(), String> {
    if matches!(value, DataValue::Null) {
        return Ok(());
    }
    // `take` forces an explicit re-seat rather than in-place Option mutation.
    let prior = cell.take();
    *cell = Some(match prior {
        None => value.clone(),
        Some(prev) => pick(&prev, value)?,
    });
    Ok(())
}

/// Meet update for numeric extremum — ONE seat for MeetMin/MeetMax.
fn update_numeric_extremum(
    left: &mut MeetAccum,
    right: &MeetAccum,
    pick: fn(&DataValue, &DataValue) -> Result<DataValue, String>,
) -> Result<bool, String> {
    let Some(incoming) = meet_offer(right) else {
        return Ok(false);
    };
    if meet_cell_is_vacant(left) {
        *left = MeetAccum::Value(incoming.clone());
        return Ok(true);
    }
    let MeetAccum::Value(resident) = left else {
        return Ok(false);
    };
    let winner = pick(resident, incoming)?;
    if winner == *resident {
        return Ok(false);
    }
    *resident = winner;
    Ok(true)
}

impl NormalAccum for MinAccum {
    fn set(&mut self, value: &DataValue) -> Result<(), String> {
        set_numeric_extremum(&mut self.least, value, lesser_number)
    }
    fn get(&self) -> Result<DataValue, String> {
        Ok(match &self.least {
            Some(v) => v.clone(),
            None => {
                // Absent fold witness — SQL NULL render via named empty door.
                DataValue::Null
            }
        })
    }
}

/// Meet form: Empty is identity; Null is never a candidate; otherwise numeric ≤.
struct MeetMin;

fn meet_cell_is_vacant(cell: &MeetAccum) -> bool {
    match cell {
        MeetAccum::Empty => true,
        MeetAccum::Value(DataValue::Null) => true,
        MeetAccum::Value(_) => false,
    }
}

fn meet_offer(right: &MeetAccum) -> Option<&DataValue> {
    match right {
        MeetAccum::Empty => None,
        MeetAccum::Value(DataValue::Null) => None,
        MeetAccum::Value(v) => Some(v),
    }
}

impl MeetOp for MeetMin {
    fn init_val(&self) -> MeetAccum {
        MeetAccum::Empty
    }
    fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool, String> {
        update_numeric_extremum(left, right, lesser_number)
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
        Ok(Box::new(MinAccum { least: None }))
    }
    fn fresh_meet(&self) -> Option<Box<dyn MeetOp>> {
        Some(Box::new(MeetMin))
    }
}

// ── max (normal + meet) ──────────────────────────────────────────────────

struct BuiltinMax;

/// Greatest non-null observation so far; vacant until the first real value.
struct MaxAccum {
    greatest: Option<DataValue>,
}

/// Prefer the numerically larger of two values; refuse non-numbers.
fn greater_number(a: &DataValue, b: &DataValue) -> Result<DataValue, String> {
    extreme_number(a, b, |x, y| y > x, "max fold requires numeric operands")
}

impl NormalAccum for MaxAccum {
    fn set(&mut self, value: &DataValue) -> Result<(), String> {
        set_numeric_extremum(&mut self.greatest, value, greater_number)
    }
    fn get(&self) -> Result<DataValue, String> {
        Ok(match &self.greatest {
            Some(v) => v.clone(),
            None => {
                // Absent fold witness — SQL NULL render via named empty door.
                DataValue::Null
            }
        })
    }
}

/// Meet form: Empty is identity; Null is never a candidate; otherwise numeric ≥.
struct MeetMax;

impl MeetOp for MeetMax {
    fn init_val(&self) -> MeetAccum {
        MeetAccum::Empty
    }
    fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool, String> {
        update_numeric_extremum(left, right, greater_number)
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
        Ok(Box::new(MaxAccum { greatest: None }))
    }
    fn fresh_meet(&self) -> Option<Box<dyn MeetOp>> {
        Some(Box::new(MeetMax))
    }
}

// ── and (normal + meet) ──────────────────────────────────────────────────

struct BuiltinAnd;

/// Conjunction over a stream of bools; starts true, false is absorbing.
struct AndAccum {
    all_true_so_far: bool,
}

impl NormalAccum for AndAccum {
    fn set(&mut self, value: &DataValue) -> Result<(), String> {
        let DataValue::Bool(bit) = value else {
            return Err(format!("and fold rejects non-bool input: {value:?}"));
        };
        // Absorbing false: only a false observation can change the cell.
        if !*bit {
            self.all_true_so_far = false;
        }
        Ok(())
    }
    fn get(&self) -> Result<DataValue, String> {
        Ok(DataValue::from(self.all_true_so_far))
    }
}

/// Two-point meet lattice with `true` as identity and `false` as bottom.
struct MeetAnd;

/// Pull a bool contribution out of a meet cell; `Empty` is no contribution.
fn bool_contribution(cell: &MeetAccum, side: &str) -> Result<Option<bool>, String> {
    match cell {
        MeetAccum::Empty => Ok(None),
        MeetAccum::Value(DataValue::Bool(b)) => Ok(Some(*b)),
        MeetAccum::Value(other) => Err(format!(
            "and meet: {side} side expected bool, got {other:?}"
        )),
    }
}

/// Bool lattice meet/join update — ONE seat for MeetAnd / MeetOr.
fn update_bool_lattice(
    left: &mut MeetAccum,
    right: &MeetAccum,
    contribute: fn(&MeetAccum, &str) -> Result<Option<bool>, String>,
    moves: fn(current: bool, incoming: bool) -> bool,
    landed: bool,
) -> Result<bool, String> {
    if matches!(right, MeetAccum::Empty) {
        return Ok(false);
    }
    if matches!(left, MeetAccum::Empty) {
        *left = right.clone();
        return Ok(true);
    }
    let Some(incoming) = contribute(right, "right")? else {
        return Ok(false);
    };
    let Some(current) = contribute(left, "left")? else {
        return Ok(false);
    };
    if moves(current, incoming) {
        *left = MeetAccum::Value(DataValue::from(landed));
        Ok(true)
    } else {
        Ok(false)
    }
}

impl MeetOp for MeetAnd {
    fn init_val(&self) -> MeetAccum {
        MeetAccum::Value(DataValue::from(true))
    }
    fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool, String> {
        // Lattice meet = ∧. Only true→false moves the cell.
        update_bool_lattice(left, right, bool_contribution, |c, i| c && !i, false)
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
        Ok(Box::new(AndAccum {
            all_true_so_far: true,
        }))
    }
    fn fresh_meet(&self) -> Option<Box<dyn MeetOp>> {
        Some(Box::new(MeetAnd))
    }
}

// ── or (normal + meet) ───────────────────────────────────────────────────

struct BuiltinOr;

/// Disjunction over a stream of bools; starts false, true is absorbing.
struct OrAccum {
    any_true_so_far: bool,
}

impl NormalAccum for OrAccum {
    fn set(&mut self, value: &DataValue) -> Result<(), String> {
        let DataValue::Bool(bit) = value else {
            return Err(format!("or fold rejects non-bool input: {value:?}"));
        };
        // Absorbing true: only a true observation can change the cell.
        if *bit {
            self.any_true_so_far = true;
        }
        Ok(())
    }
    fn get(&self) -> Result<DataValue, String> {
        Ok(DataValue::from(self.any_true_so_far))
    }
}

/// Two-point join lattice with `false` as identity and `true` as top.
struct MeetOr;

/// Pull a bool contribution for or-meet; errors name the op, not and-meet.
fn or_bool_contribution(cell: &MeetAccum, side: &str) -> Result<Option<bool>, String> {
    match cell {
        MeetAccum::Empty => Ok(None),
        MeetAccum::Value(DataValue::Bool(b)) => Ok(Some(*b)),
        MeetAccum::Value(other) => Err(format!(
            "or meet: {side} side expected bool, got {other:?}"
        )),
    }
}

impl MeetOp for MeetOr {
    fn init_val(&self) -> MeetAccum {
        MeetAccum::Value(DataValue::from(false))
    }
    fn update(&self, left: &mut MeetAccum, right: &MeetAccum) -> Result<bool, String> {
        // Lattice join = ∨. Only false→true moves the cell.
        update_bool_lattice(left, right, or_bool_contribution, |c, i| !c && i, true)
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
        Ok(Box::new(OrAccum {
            any_true_so_far: false,
        }))
    }
    fn fresh_meet(&self) -> Option<Box<dyn MeetOp>> {
        Some(Box::new(MeetOr))
    }
}
