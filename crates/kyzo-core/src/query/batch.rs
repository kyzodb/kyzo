/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The columnar evaluator's execution vocabulary: a row batch pivoted to
//! columns, a sorted row selection, and the row-ordered minimum-error
//! keeper that gives the columnar lane row-lane error identity.
//!
//! Values-based v1: columns hold owned [`DataValue`]s. Story #120's
//! packed-`u32` relations replace these internals with code columns over
//! the value plane's arena (`data::value::column`) — this module is the
//! seam it swaps behind.

use crate::data::value::{DataValue, Tuple};

/// A row set pivoted to columns for the columnar expression evaluator.
pub(crate) struct ColumnBatch {
    cols: Vec<BatchColumn>,
    height: usize,
}

impl ColumnBatch {
    pub(crate) fn from_rows(rows: Vec<Tuple>, width: usize) -> ColumnBatch {
        let height = rows.len();
        let mut cols: Vec<Vec<DataValue>> =
            (0..width).map(|_| Vec::with_capacity(height)).collect();
        for row in rows {
            debug_assert_eq!(row.len(), width, "ragged batch row");
            for (i, v) in row.into_iter().enumerate().take(width) {
                cols[i].push(v);
            }
        }
        ColumnBatch {
            cols: cols.into_iter().map(BatchColumn).collect(),
            height,
        }
    }

    pub(crate) fn width(&self) -> usize {
        self.cols.len()
    }

    pub(crate) fn height(&self) -> usize {
        self.height
    }

    pub(crate) fn column(&self, i: usize) -> &BatchColumn {
        &self.cols[i]
    }
}

/// One batch column; `get` clones the row's value (the packed-code form
/// replaces this with a spend through an admitted observer, per #120).
pub(crate) struct BatchColumn(Vec<DataValue>);

impl BatchColumn {
    pub(crate) fn get(&self, row: usize) -> DataValue {
        self.0[row].clone()
    }
}

/// A sorted set of live row indices.
#[derive(Clone)]
pub(crate) struct Selection(Vec<u32>);

impl Selection {
    pub(crate) fn all(n: usize) -> Selection {
        assert!(u32::try_from(n).is_ok(), "batch beyond u32 rows");
        Selection((0..n as u32).collect())
    }

    /// From ascending row ids (debug-asserted: sortedness is the caller's
    /// construction, not a hidden re-sort).
    pub(crate) fn from_sorted(rows: Vec<u32>) -> Selection {
        debug_assert!(
            rows.windows(2).all(|w| w[0] < w[1]),
            "selection not ascending"
        );
        Selection(rows)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.0.iter().map(|&r| r as usize)
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The row-ordered minimum-error keeper: among every offered error, the
/// one with the smallest `(row, node)` wins — exactly the error row-major
/// evaluation would raise first. `offer` constructs the error lazily and
/// only when it improves the minimum.
pub(crate) struct ErrorMin<E> {
    best: Option<(u32, u32, E)>,
}

impl<E> Default for ErrorMin<E> {
    fn default() -> Self {
        ErrorMin { best: None }
    }
}

impl<E> ErrorMin<E> {
    pub(crate) fn offer(&mut self, row: u32, node: u32, make: impl FnOnce() -> E) {
        let better = match &self.best {
            None => true,
            Some((br, bn, _)) => (row, node) < (*br, *bn),
        };
        if better {
            self.best = Some((row, node, make()));
        }
    }

    pub(crate) fn into_error(self) -> Option<E> {
        self.best.map(|(_, _, e)| e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_min_keeps_row_major_order() {
        let mut em: ErrorMin<&'static str> = ErrorMin::default();
        em.offer(5, 0, || "later row");
        em.offer(2, 7, || "earlier row, later node");
        em.offer(2, 3, || "earlier node wins");
        em.offer(2, 4, || "not better");
        assert_eq!(em.into_error(), Some("earlier node wins"));
    }

    #[test]
    fn batch_pivots_and_selects() {
        let rows = vec![
            vec![DataValue::from(1i64), DataValue::from("a")],
            vec![DataValue::from(2i64), DataValue::from("b")],
        ];
        let b = ColumnBatch::from_rows(rows, 2);
        assert_eq!((b.width(), b.height()), (2, 2));
        assert_eq!(b.column(1).get(1), DataValue::from("b"));
        let sel = Selection::from_sorted(vec![1]);
        assert_eq!(sel.iter().collect::<Vec<_>>(), vec![1usize]);
        assert!(!sel.is_empty());
        assert_eq!(Selection::all(2).len(), 2);
    }
}
