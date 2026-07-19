/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (`query/sort.rs`, MPL-2.0):
 *
 * - A free function instead of a `SessionTx` method: the original took
 *   `&mut self` and never touched the transaction.
 * - Law 5: the original indexed `head_indices[k]`, so an `:order` clause
 *   naming a variable absent from the entry head panicked the process.
 *   That is now the typed [`SorterNotInHead`] refusal. (The parser
 *   validates this for well-formed scripts; the refusal covers every
 *   other road to a program value.)
 * - Sort semantics are exactly upstream's, preserved deliberately:
 *   compare sort keys in clause order with `DataValue`'s total order,
 *   direction applied per clause; ties keep store order (`sort_by` is
 *   stable, and the store iterates in canonical order) — so the output is
 *   deterministic even under ties.
 */

//! `:order` — sorting a query's result set.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use miette::{Diagnostic, Result, bail};
use thiserror::Error;

use crate::data::program::SortDir;
use kyzo_model::SourceSpan;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::Tuple;
use crate::exec::fixpoint::delta_store::EpochStore;
use crate::exec::fixpoint::delta_store::TupleInIter;

/// An `:order` clause names a variable that is not in the entry head.
/// (The CozoDB original panicked on this shape.)
#[derive(Debug, Error, Diagnostic)]
#[error("':order {0}' does not name a variable of the query head")]
#[diagnostic(code(eval::sorter_not_in_head))]
pub(crate) struct SorterNotInHead(pub(crate) String, #[label] pub(crate) SourceSpan);

/// Collect the result store into a vector sorted by the `:order` clauses.
///
/// Exact upstream semantics: clauses compare in order; the first non-equal
/// comparison decides, reversed for `SortDir::Dsc`; full ties preserve the
/// store's canonical order (stable sort), keeping the output deterministic.
pub(crate) fn sort_and_collect(
    original: &EpochStore,
    sorters: &[(Symbol, SortDir)],
    head: &[Symbol],
) -> Result<Vec<Tuple>> {
    let head_indices: BTreeMap<&Symbol, usize> =
        head.iter().enumerate().map(|(i, k)| (k, i)).collect();
    let mut idx_sorters = Vec::with_capacity(sorters.len());
    for (k, dir) in sorters {
        match head_indices.get(k) {
            Some(idx) => idx_sorters.push((*idx, *dir)),
            None => bail!(SorterNotInHead(k.to_string(), k.span)),
        }
    }

    let mut all_data: Vec<Tuple> = original
        .all_iter()?
        .map(TupleInIter::try_into_tuple)
        .collect::<Result<Vec<_>, _>>()?;
    all_data.sort_by(|a, b| {
        for (idx, dir) in &idx_sorters {
            match a[*idx].cmp(&b[*idx]) {
                Ordering::Equal => {}
                o @ Ordering::Less | o @ Ordering::Greater => {
                    return match dir {
                        SortDir::Asc => o,
                        SortDir::Dsc => o.reverse(),
                    };
                }
            }
        }
        Ordering::Equal
    });

    Ok(all_data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kyzo_model::value::DataValue;
    use crate::exec::fixpoint::delta_store::RegularTempStore;

    fn store_of(rows: &[Vec<i64>]) -> EpochStore {
        let mut fresh = RegularTempStore::default();
        for row in rows {
            fresh.put(row.iter().map(|v| DataValue::from(*v)).collect());
        }
        let mut store = EpochStore::new_normal(rows.first().map_or(0, Vec::len));
        store.merge_in(fresh.wrap(), &mut ()).unwrap();
        store
    }

    fn sym(s: &str) -> Symbol {
        Symbol::new(s, SourceSpan(0, 0))
    }

    #[test]
    fn sorts_by_clause_order_and_direction() {
        let store = store_of(&[vec![1, 10], vec![2, 5], vec![1, 5], vec![2, 10]]);
        let head = [sym("a"), sym("b")];
        // :order -b, a  — descending b first, ascending a to break ties.
        let sorted = sort_and_collect(
            &store,
            &[(sym("b"), SortDir::Dsc), (sym("a"), SortDir::Asc)],
            &head,
        )
        .unwrap();
        let as_ints: Vec<(i64, i64)> = sorted
            .iter()
            .map(|t| (t[0].get_int().expect("int"), t[1].get_int().expect("int")))
            .collect();
        assert_eq!(as_ints, vec![(1, 10), (2, 10), (1, 5), (2, 5)]);
    }

    #[test]
    fn unknown_sorter_is_a_typed_refusal_not_a_panic() {
        let store = store_of(&[vec![1, 2]]);
        let head = [sym("a"), sym("b")];
        let err = sort_and_collect(&store, &[(sym("nope"), SortDir::Asc)], &head).unwrap_err();
        assert!(err.downcast_ref::<SorterNotInHead>().is_some());
    }
}
