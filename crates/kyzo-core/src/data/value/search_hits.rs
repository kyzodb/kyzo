/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Admitted index-search hits: row-major arena codes under one [`Domain`].
//!
//! Engine algorithms may build decoded rows internally, but
//! [`RelationIndexSearch::search_relation`] admits them here before they
//! cross the query seam — the same execution currency as [`Rows`] /
//! [`ExecRows`], not a private `Vec<DataValue>` intern table.

use super::admission::Denial;
use super::arena::Arena;
use super::arity::Arity;
use super::canonical::{self, encode_owned, DecodeError};
use super::column::Domain;
use super::row::Rows;
use super::{DataValue, Tuple};

/// Admitted relation-index search hits under one arena [`Domain`].
///
/// @authority SearchHits
/// @layer value
/// @owns admitted search-hit rows as arena-stamped codes; decoded tuples do not cross the search seam
/// @constructs SearchHits::admit_decoded | SearchHits::empty
/// @forbids private Vec<DataValue> + u32 costume standing in for Domain admission
/// @gate RelationIndexSearch returns SearchHits only (#338 D03)
/// @status established #338
pub(crate) struct SearchHits {
    arena: Arena,
    rows: Rows,
}

impl SearchHits {
    /// No hits yet — idle state before the first search invocation.
    pub(crate) fn empty() -> Self {
        let mut arena = Arena::new();
        let rows = Rows::new_in(Arity::ONE, &arena.frame());
        SearchHits { arena, rows }
    }

    /// THE DOOR: admit decoded engine hit rows into one arena domain.
    ///
    /// Consumes logical rows at the engine→query boundary; callers above
    /// the seam receive codes under a proven [`Domain`], never a
    /// `Vec<Tuple>`.
    pub(crate) fn admit_decoded(
        hits: impl IntoIterator<Item = Tuple>,
    ) -> Result<Self, Denial> {
        let hits: Vec<Tuple> = hits.into_iter().collect();
        if hits.is_empty() {
            return Ok(Self::empty());
        }
        let width = hits[0].len();
        let Some(arity) = Arity::try_new(width) else {
            return Err(Denial::EmptyProjection);
        };
        let mut arena = Arena::new();
        let mut rows = Rows::new_in(arity, &arena.frame());
        for hit in hits {
            if hit.len() != width {
                return Err(Denial::ArityMismatch {
                    expected: width,
                    got: hit.len(),
                });
            }
            let mut stamps = Vec::with_capacity(width);
            for cell in hit.as_slice() {
                stamps.push(arena.intern(encode_owned(cell).as_bytes())?);
            }
            rows.push_row(&stamps)?;
        }
        Ok(SearchHits { arena, rows })
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub(crate) fn domain(&self) -> Domain {
        self.rows.domain()
    }

    /// Materialize hit `i` through the owning arena (output boundary).
    pub(crate) fn materialize_hit(&self, i: usize) -> Result<Vec<DataValue>, MaterializeError> {
        if i >= self.rows.len() {
            return Err(MaterializeError::RowOutOfRange {
                index: i,
                len: self.rows.len(),
            });
        }
        let frame = self.arena.frame();
        let admitted = self.rows.admit(&frame)?;
        let w = self.rows.arity().get();
        let mut out = Vec::with_capacity(w);
        for col in 0..w {
            let bytes = admitted.resolve_cell(i, col);
            out.push(canonical::decode(bytes).map_err(MaterializeError::Decode)?);
        }
        Ok(out)
    }

    /// Output boundary: every admitted hit as a logical row.
    pub(crate) fn materialize_all(&self) -> Result<Vec<Vec<DataValue>>, MaterializeError> {
        (0..self.len())
            .map(|i| self.materialize_hit(i))
            .collect()
    }

    /// Output boundary: every admitted hit as row authority.
    pub(crate) fn materialize_all_tuples(&self) -> Result<Vec<Tuple>, MaterializeError> {
        Ok(self
            .materialize_all()?
            .into_iter()
            .map(Tuple::from_vec)
            .collect())
    }
}

/// Typed refusal when materializing an admitted hit row to logical values.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum MaterializeError {
    RowOutOfRange { index: usize, len: usize },
    Decode(DecodeError),
    Denial(Denial),
}

impl From<Denial> for MaterializeError {
    fn from(d: Denial) -> Self {
        MaterializeError::Denial(d)
    }
}
