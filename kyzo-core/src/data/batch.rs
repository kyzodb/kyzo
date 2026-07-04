/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The execution currency: typed column batches.
//!
//! A [`ColumnBatch`] is a rectangle of rows held column-major, each column
//! either a dense TYPED vector (`Vec<i64>`, `Vec<f64>`, …) or the
//! [`Mixed`](ColumnVec::Mixed) floor (a `Vec<DataValue>`, one tag branch
//! per element). Typed columns are what make vectorized kernels fast —
//! monomorphized loops over dense memory with no per-element tag branch —
//! and `Mixed` is what makes the representation TOTAL: every value the
//! language can produce has a column home, so no operator ever needs a
//! row-at-a-time escape hatch.
//!
//! A [`Selection`] is the live-row set of a batch: a sorted vector of row
//! indices. Filters refine selections instead of copying batches; the lazy
//! connectives and `Cond` partition them. Kernels iterate the selection,
//! not the full height, so a highly selective prefix makes later work
//! proportionally cheap.
//!
//! Construction is where typing is decided, ONCE per batch: a column whose
//! every value is a non-null `Int` becomes `I64`; anything else falls to
//! `Mixed`. The decision never lies — `ColumnVec::get` returns exactly the
//! `DataValue` the row held — so batch execution and row execution are
//! observationally identical, which is what the VM's differential judge
//! checks.

use smartstring::{LazyCompact, SmartString};

use crate::data::value::{DataValue, Num};

/// One column of a batch: a dense typed vector when every element fits one
/// concrete type, the `Mixed` floor otherwise.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnVec {
    /// Every element a non-null integer.
    I64(Vec<i64>),
    /// Every element a non-null float.
    F64(Vec<f64>),
    /// Every element a non-null boolean.
    Bool(Vec<bool>),
    /// Every element a non-null string.
    Str(Vec<SmartString<LazyCompact>>),
    /// The total floor: any values at all, tags per element.
    Mixed(Vec<DataValue>),
}

impl ColumnVec {
    /// The value at `row`, exactly as the source row held it.
    ///
    /// Callers indexing beyond the height hold a corrupt selection; that
    /// is a caller bug, so this panics like slice indexing does.
    pub fn get(&self, row: usize) -> DataValue {
        match self {
            ColumnVec::I64(v) => DataValue::from(v[row]),
            ColumnVec::F64(v) => DataValue::from(v[row]),
            ColumnVec::Bool(v) => DataValue::from(v[row]),
            ColumnVec::Str(v) => DataValue::Str(v[row].clone()),
            ColumnVec::Mixed(v) => v[row].clone(),
        }
    }

    /// Build the tightest column for `values`: a typed vector when every
    /// element fits one concrete type, `Mixed` otherwise. The scan is one
    /// pass; the common all-one-type case allocates exactly once.
    pub fn from_values(values: Vec<DataValue>) -> Self {
        #[derive(Clone, Copy, PartialEq)]
        enum Fit {
            I64,
            F64,
            Bool,
            Str,
            Mixed,
        }
        let fit = values
            .iter()
            .map(|v| match v {
                DataValue::Num(Num::Int(_)) => Fit::I64,
                DataValue::Num(Num::Float(_)) => Fit::F64,
                DataValue::Bool(_) => Fit::Bool,
                DataValue::Str(_) => Fit::Str,
                _ => Fit::Mixed,
            })
            .try_fold(None, |acc, f| match acc {
                None => Ok(Some(f)),
                Some(prev) if prev == f => Ok(Some(f)),
                Some(_) => Err(()),
            });
        match fit {
            Ok(Some(Fit::I64)) => ColumnVec::I64(
                values
                    .into_iter()
                    .map(|v| match v {
                        DataValue::Num(Num::Int(i)) => i,
                        _ => unreachable!("fit proved all-int"),
                    })
                    .collect(),
            ),
            Ok(Some(Fit::F64)) => ColumnVec::F64(
                values
                    .into_iter()
                    .map(|v| match v {
                        DataValue::Num(Num::Float(f)) => f,
                        _ => unreachable!("fit proved all-float"),
                    })
                    .collect(),
            ),
            Ok(Some(Fit::Bool)) => ColumnVec::Bool(
                values
                    .into_iter()
                    .map(|v| match v {
                        DataValue::Bool(b) => b,
                        _ => unreachable!("fit proved all-bool"),
                    })
                    .collect(),
            ),
            Ok(Some(Fit::Str)) => ColumnVec::Str(
                values
                    .into_iter()
                    .map(|v| match v {
                        DataValue::Str(s) => s,
                        _ => unreachable!("fit proved all-str"),
                    })
                    .collect(),
            ),
            Ok(Some(Fit::Mixed)) | Ok(None) | Err(()) => ColumnVec::Mixed(values),
        }
    }
}

/// A rectangle of rows, column-major. Every column has the same height.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnBatch {
    cols: Vec<ColumnVec>,
    height: usize,
}

impl ColumnBatch {
    /// Transpose row-major tuples into the tightest columns. `width` rules
    /// even when `rows` is empty (an empty batch still has a shape).
    pub fn from_rows<R: AsRef<[DataValue]>>(rows: &[R], width: usize) -> Self {
        let mut cols: Vec<Vec<DataValue>> =
            (0..width).map(|_| Vec::with_capacity(rows.len())).collect();
        for row in rows {
            let row = row.as_ref();
            debug_assert_eq!(row.len(), width, "ragged row in batch transpose");
            for (c, v) in row.iter().enumerate() {
                cols[c].push(v.clone());
            }
        }
        ColumnBatch {
            cols: cols.into_iter().map(ColumnVec::from_values).collect(),
            height: rows.len(),
        }
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn width(&self) -> usize {
        self.cols.len()
    }

    pub fn column(&self, i: usize) -> &ColumnVec {
        &self.cols[i]
    }

    /// One logical row, materialized — the round-trip law's observer
    /// (inner loops go through columns).
    #[cfg(test)]
    pub fn row(&self, r: usize) -> Vec<DataValue> {
        self.cols.iter().map(|c| c.get(r)).collect()
    }
}

/// The live rows of a batch: strictly increasing row indices. Filters
/// REFINE selections; nothing copies the batch.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection(Vec<u32>);

impl Selection {
    /// Every row of a batch of the given height.
    pub fn all(height: usize) -> Self {
        debug_assert!(u32::try_from(height).is_ok(), "batch beyond u32 rows");
        #[allow(clippy::cast_possible_truncation)]
        Selection((0..height as u32).collect())
    }

    /// From raw indices — must be strictly increasing (callers refine
    /// existing selections, which preserves order by construction).
    pub fn from_sorted(rows: Vec<u32>) -> Self {
        debug_assert!(rows.windows(2).all(|w| w[0] < w[1]), "unsorted selection");
        Selection(rows)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.0.iter().map(|&r| r as usize)
    }
}

/// A deterministic error candidate during batched evaluation: the batch
/// must raise exactly the error row-at-a-time evaluation would — the
/// FIRST failing row in row order, and within a row the first failing
/// instruction in evaluation order. Kernels record candidates; the
/// minimum wins; it raises only at extraction.
#[derive(Debug)]
pub struct ErrorCandidate<E> {
    row: u32,
    instr: u32,
    err: E,
}

/// The running minimum over error candidates. O(1) state; the happy path
/// allocates nothing.
#[derive(Debug)]
pub struct ErrorMin<E>(Option<ErrorCandidate<E>>);

impl<E> Default for ErrorMin<E> {
    fn default() -> Self {
        ErrorMin(None)
    }
}

impl<E> ErrorMin<E> {
    /// Offer a candidate; it is kept iff it precedes the current minimum
    /// in (row, instruction) order.
    pub fn offer(&mut self, row: u32, instr: u32, err: impl FnOnce() -> E) {
        let beats = match &self.0 {
            None => true,
            Some(cur) => (row, instr) < (cur.row, cur.instr),
        };
        if beats {
            self.0 = Some(ErrorCandidate {
                row,
                instr,
                err: err(),
            });
        }
    }

    /// The winning error, if any row failed.
    pub fn into_error(self) -> Option<E> {
        self.0.map(|c| c.err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_is_tight_and_total() {
        let ints = ColumnVec::from_values(vec![DataValue::from(1), DataValue::from(2)]);
        assert!(matches!(ints, ColumnVec::I64(_)));
        let mixed = ColumnVec::from_values(vec![DataValue::from(1), DataValue::Null]);
        assert!(matches!(mixed, ColumnVec::Mixed(_)));
        let strs = ColumnVec::from_values(vec![DataValue::from("a"), DataValue::from("b")]);
        assert!(matches!(strs, ColumnVec::Str(_)));
        // Floats and ints do NOT unify: Num’s ordering laws differ per
        // variant and a silent widen would change kernel semantics.
        let nums = ColumnVec::from_values(vec![DataValue::from(1), DataValue::from(1.5)]);
        assert!(matches!(nums, ColumnVec::Mixed(_)));
    }

    #[test]
    fn transpose_round_trips_exactly() {
        let rows = vec![
            vec![DataValue::from(1), DataValue::from("x"), DataValue::Null],
            vec![
                DataValue::from(2),
                DataValue::from("y"),
                DataValue::from(3.5),
            ],
        ];
        let batch = ColumnBatch::from_rows(&rows, 3);
        assert_eq!(batch.height(), 2);
        assert_eq!(batch.width(), 3);
        for (r, row) in rows.iter().enumerate() {
            assert_eq!(&batch.row(r), row, "row {r} round-trips");
        }
    }

    #[test]
    fn error_min_is_row_major_then_eval_order() {
        let mut min: ErrorMin<&'static str> = ErrorMin::default();
        min.offer(5, 0, || "row5/instr0");
        min.offer(2, 7, || "row2/instr7");
        min.offer(2, 3, || "row2/instr3");
        min.offer(2, 9, || "row2/instr9-late");
        assert_eq!(min.into_error(), Some("row2/instr3"));
    }
}
