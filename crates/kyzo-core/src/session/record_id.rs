/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Admitted record identity (#268 T3).
//!
//! [`RecordId`] is minted only at the admission seam
//! ([`crate::session::admit::admit_record`]). Projections and retrieval
//! spans resolve to this id or refuse — never a free-floating truth.

/// Admitted record identity — minted only by [`crate::session::admit::admit_record`].
///
/// Private field: no public / anonymous constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordId([u8; 32]);

impl RecordId {
    /// Admission-only mint from the record content digest.
    ///
    /// Call site law: only [`crate::session::admit::admit_record`].
    pub(crate) fn mint_at_admit(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Borrow the identity digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}
