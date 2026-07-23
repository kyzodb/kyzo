/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Client-rooted cross-store composition (decisions.md §38, §53).
//!
//! Owns: [`CompositionId`], composition sum BestEffort | Saga | ReadAt,
//! Saga compensation keying, ReadAt per-Store cut vector.
//!
//! Bans: cross-store Committed / TryAtomic / global snapshot; Engine-minted
//! CompositionId; compensations under a different CompositionId.
//!
//! `CompositionId = H(principal, client_operation_id, canonical_composition_digest)`.
//! Caller-durable before the first Store step; Engine may echo, never source.

use sha2::{Digest, Sha256};

use crate::store::open::StoreId;
use crate::store::sweep::CommitOrdinal;
use crate::store::transcript::Digest32;

/// Client-rooted composition identity (§38).
///
/// Engine-minted CompositionId is Unconstructible — only [`CompositionId::derive`]
/// from caller-durable intent exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CompositionId([u8; 32]);

impl CompositionId {
    /// Derive from caller-durable principal + client_operation_id + digest.
    ///
    /// Fresh processes re-derive the same id from the same client intent.
    pub fn derive(
        principal: &[u8],
        client_operation_id: &[u8],
        canonical_composition_digest: Digest32,
    ) -> Self {
        let mut h = Sha256::new();
        h.update(b"kyzo.composition_id.v1");
        h.update(principal);
        h.update(client_operation_id);
        h.update(canonical_composition_digest.as_bytes());
        Self(h.finalize().into())
    }

    /// Borrow the identity digest (Store idempotency organ consumes these bytes).
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Sealed digest carrier for the store idempotency organ (one-way layering).
    pub fn digest32(self) -> Digest32 {
        Digest32::admit(self.0)
    }
}

/// Digest of composition shape + ordered steps + StoreIds + compensations.
pub fn canonical_composition_digest(
    shape_tag: u8,
    steps: &[(StoreId, &[u8])],
    compensations: &[(StoreId, &[u8])],
) -> Digest32 {
    let mut h = Sha256::new();
    h.update(b"kyzo.composition_digest.v1");
    h.update([shape_tag]);
    for (store, step) in steps {
        h.update(store.as_bytes());
        h.update(step);
    }
    for (store, comp) in compensations {
        h.update(b"compensation");
        h.update(store.as_bytes());
        h.update(comp);
    }
    Digest32::admit(h.finalize().into())
}

/// Closed composition sum (§38) — no TryAtomic / global snapshot arm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Composition {
    /// Best-effort multi-store steps; local law only per Store.
    BestEffort {
        /// Client-rooted identity (caller-durable before first step).
        id: CompositionId,
        /// Ordered Store steps.
        steps: Vec<CompositionStep>,
    },
    /// Saga with compensations keyed under the **same** CompositionId.
    Saga {
        /// Client-rooted identity.
        id: CompositionId,
        /// Forward steps.
        steps: Vec<CompositionStep>,
        /// Compensations under the same CompositionId (never a different id).
        compensations: Vec<CompositionStep>,
    },
    /// Read-at composition: per-Store cut vector; mints no OperationKey entries.
    ReadAt {
        /// Client-rooted identity (echo only).
        id: CompositionId,
        /// Per-Store cut vector.
        cuts: Vec<(StoreId, CommitOrdinal)>,
    },
}

impl Composition {
    /// Borrow the CompositionId (Engine may echo; never mint).
    pub fn id(&self) -> CompositionId {
        match self {
            Composition::BestEffort { id, .. }
            | Composition::Saga { id, .. }
            | Composition::ReadAt { id, .. } => *id,
        }
    }
}

/// One Store step inside a composition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositionStep {
    /// Target Store.
    pub store_id: StoreId,
    /// Step identity bytes (feeds OperationKey StepId).
    pub step_id: Vec<u8>,
}

/// Multi-store conflict in a composition (§53) — Engine owns this refuse.
///
/// Store knows only one address's local law.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum CompositionRefuse {
    /// Cross-store conflict detected by Engine composition.
    #[error("FederationConflict: multi-store conflict in a composition")]
    #[diagnostic(code(session::composition::federation_conflict))]
    FederationConflict {
        /// Stores that conflicted.
        stores: Vec<StoreId>,
    },
}
