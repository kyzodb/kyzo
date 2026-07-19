/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0); split out of query/ra.rs — see query/ra/mod.rs for the
 * transformation record.
 */

//! Inline rows: the unit relation and literal data blocks.
// ─────────────────────────────────────────────────────────────────────────
// InlineFixedRA: inline rows
// ─────────────────────────────────────────────────────────────────────────

use super::TupleIter;
use kyzo_model::SourceSpan;
use kyzo_model::program::symbol::Symbol;
use kyzo_model::value::DataValue;
use crate::exec::op::batch_ops::{Batch, BatchIter};
use crate::exec::op::join::eliminate_from_tuple;
use itertools::Itertools;
use miette::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::iter;

/// Literal rows known at compile time. `unit` (no columns, one empty row)
/// seeds every rule body; data-bearing forms arrive with the constant-rule
/// wiring in db.rs.
#[derive(Debug)]
pub(crate) struct InlineFixedRA {
    pub(crate) bindings: Vec<Symbol>,
    pub(crate) data: Vec<Vec<DataValue>>,
    pub(crate) to_eliminate: BTreeSet<Symbol>,
    pub(crate) span: SourceSpan,
}

impl InlineFixedRA {
    /// Inline rows as batches: BATCH_ROWS-sized dense chunks built once
    /// from the literal data.
    pub(crate) fn iter_batched<'a>(&'a self) -> Result<BatchIter<'a>> {
        let mut chunks = Vec::new();
        let mut batch = Batch::new();
        for t in &self.data {
            batch.push_with(|buf| {
                buf.extend_from_slice(t);
                Ok(())
            })?;
            if batch.is_full() {
                chunks.push(std::mem::take(&mut batch));
            }
        }
        if !batch.is_empty() {
            chunks.push(batch);
        }
        Ok(Box::new(chunks.into_iter().map(Ok)))
    }

    pub(crate) fn unit(span: SourceSpan) -> Self {
        Self {
            bindings: vec![],
            data: vec![vec![]],
            to_eliminate: Default::default(),
            span,
        }
    }

    pub(crate) fn do_eliminate_temp_vars(&mut self, used: &BTreeSet<Symbol>) -> Result<()> {
        for binding in &self.bindings {
            if !used.contains(binding) {
                self.to_eliminate.insert(binding.clone());
            }
        }
        Ok(())
    }

    pub(crate) fn join_type(&self) -> &'static str {
        if self.data.is_empty() {
            "null_join"
        } else if self.data.len() == 1 {
            "singleton_join"
        } else {
            "fixed_join"
        }
    }

    pub(crate) fn join<'a>(
        &'a self,
        left_iter: TupleIter<'a>,
        (left_join_indices, right_join_indices): (Vec<usize>, Vec<usize>),
        eliminate_indices: BTreeSet<usize>,
    ) -> Result<TupleIter<'a>> {
        Ok(if self.data.is_empty() {
            Box::new(iter::empty())
        } else if self.data.len() == 1 {
            let data = self.data[0].clone();
            let right_join_values = right_join_indices
                .into_iter()
                .map(|v| data[v].clone())
                .collect_vec();
            Box::new(left_iter.filter_map_ok(move |tuple| {
                let left_join_values = left_join_indices.iter().map(|v| &tuple[*v]).collect_vec();
                if left_join_values.into_iter().eq(right_join_values.iter()) {
                    let mut ret = tuple;
                    ret.extend(data.iter().cloned());
                    let ret = eliminate_from_tuple(ret, &eliminate_indices);
                    Some(ret)
                } else {
                    None
                }
            }))
        } else {
            let mut right_mapping = BTreeMap::new();
            for data in &self.data {
                let right_join_values = right_join_indices.iter().map(|v| &data[*v]).collect_vec();
                match right_mapping.get_mut(&right_join_values) {
                    None => {
                        right_mapping.insert(right_join_values, vec![data]);
                    }
                    Some(coll) => {
                        coll.push(data);
                    }
                }
            }
            Box::new(
                left_iter
                    .filter_map_ok(move |tuple| {
                        let left_join_values =
                            left_join_indices.iter().map(|v| &tuple[*v]).collect_vec();
                        right_mapping.get(&left_join_values).map(|v| {
                            v.iter()
                                .map(|right_values| {
                                    let mut left_data = tuple.clone();
                                    left_data.extend(right_values.iter().cloned());
                                    left_data
                                })
                                .collect_vec()
                        })
                    })
                    .flatten_ok(),
            )
        })
    }
}
