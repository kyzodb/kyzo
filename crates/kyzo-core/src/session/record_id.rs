/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Admitted record identity (#268 T3) and federation namespacing (#270 T2).
//!
//! [`RecordId`] is a derived view of the one stored
//! [`RecordContentDigest`](crate::data::digest::RecordContentDigest) —
//! minted only at the admission seam
//! ([`crate::session::admit::admit_record`]). Projections and retrieval
//! spans resolve to this id or refuse — never a free-floating truth.
//!
//! Across instance boundaries, [`RecordId`] alone is not federation identity:
//! use [`RecordId::namespaced`] → [`NamespacedRecordIdentity`] so distinct
//! origins never collapse (local id + origin authority + tenant + content).

use crate::data::digest::RecordContentDigest;
use crate::store::replica::{
    AuthorizingKeyId, LocalRecordId, NamespacedRecordIdentity, TenantId,
};

/// Admitted record identity — derived view of the record content digest.
///
/// Private field: no public / anonymous constructor. One 32-byte value with
/// the digest; [`RecordId`] does not store a second copy on the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordId([u8; 32]);

impl RecordId {
    /// Admission-only derived view of the record content digest.
    ///
    /// Call site law: only [`crate::session::admit::admit_record`] (and
    /// readers of an already-admitted digest).
    pub(crate) fn view_of(digest: RecordContentDigest) -> Self {
        Self(*digest.as_bytes())
    }

    /// Borrow the identity digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Federation-stable namespaced identity (#270 T2).
    ///
    /// Binds this local id with origin authority, tenant, and content so
    /// distinct origins never collapse into one. Custody still keys through
    /// [`crate::store::replica::ReplicaKey`] — one authority, not a fork.
    pub fn namespaced(
        self,
        origin_authority: AuthorizingKeyId,
        tenant: TenantId,
        content: RecordContentDigest,
    ) -> NamespacedRecordIdentity {
        NamespacedRecordIdentity::bind(
            LocalRecordId::from_digest(self.0),
            origin_authority,
            tenant,
            content,
        )
    }
}
