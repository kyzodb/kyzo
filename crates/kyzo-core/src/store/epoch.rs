/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Epochs and same-principal advance (decisions.md §56, §72).
//!
//! Owns: [`FenceEpoch`], [`CryptoDomain`], [`EpochGrant`], [`IntentClear`],
//! advance [`EpochAdvanceCommitted`] event.
//!
//! Bans: fabric-minted write continuity; epoch advance under live
//! current-epoch footprints without [`IntentClear`] (RecoveryGrant exempt).

use super::authority::WriteAuthority;
use super::open::StoreId;

/// Store-local fence epoch. Genesis is sealed into the genesis digest /
/// [`CryptoDomain`] as a verification fact — fabric never mints write continuity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FenceEpoch(u64);

impl FenceEpoch {
    /// Epoch zero for a Store identity — sealed at genesis.
    pub fn genesis(_store_id: StoreId) -> Self {
        Self(0)
    }

    /// Construct from an already-proven epoch (WAL / seal decode).
    pub(crate) fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Raw epoch counter.
    pub fn get(self) -> u64 {
        self.0
    }

    /// Successor epoch counter (not the advance ceremony — see [`advance`]).
    pub fn successor(self) -> Result<FenceEpoch, EpochAdvanceRefuse> {
        self.0
            .checked_add(1)
            .map(FenceEpoch)
            .ok_or(EpochAdvanceRefuse::EpochSpaceExhausted)
    }
}

/// Sealed crypto domain: `(StoreId, FenceEpoch)`.
///
/// Separates DEK/nonce space per store×epoch. Dual-use lineage under one
/// CryptoDomain → poison at chain-meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CryptoDomain {
    store_id: StoreId,
    fence_epoch: FenceEpoch,
}

impl CryptoDomain {
    /// Bind a Store identity to its current fence epoch.
    pub fn new(store_id: StoreId, fence_epoch: FenceEpoch) -> Self {
        Self {
            store_id,
            fence_epoch,
        }
    }

    /// Store identity half.
    pub fn store_id(self) -> StoreId {
        self.store_id
    }

    /// Fence epoch half.
    pub fn fence_epoch(self) -> FenceEpoch {
        self.fence_epoch
    }
}

/// Same-principal live-token grant to advance [`FenceEpoch`] on the same StoreId.
///
/// Distinct from [`super::grants::RecoveryGrant`] and [`super::authority::WriteAuthority`].
/// Fabric carries this message; it never mints write continuity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochGrant {
    store_id: StoreId,
    predecessor_epoch: FenceEpoch,
}

impl EpochGrant {
    /// Host / federation admission of a same-principal advance grant.
    pub fn new(store_id: StoreId, predecessor_epoch: FenceEpoch) -> Self {
        Self {
            store_id,
            predecessor_epoch,
        }
    }

    /// Store this grant advances.
    pub fn store_id(&self) -> StoreId {
        self.store_id
    }

    /// Predecessor epoch named in the grant.
    pub fn predecessor_epoch(&self) -> FenceEpoch {
        self.predecessor_epoch
    }
}

/// Witness that current-epoch current-incarnation Fenced footprints are clear.
///
/// Ordinary [`advance`] requires this. RecoveryGrant advance is IntentClear-exempt
/// under epoch-indexed footprint law (decisions.md §36/§72).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntentClear {
    store_id: StoreId,
    fence_epoch: FenceEpoch,
}

impl IntentClear {
    /// Attest that no live current-epoch Fenced footprints remain.
    ///
    /// Session/footprint seat (T12) is the sole honest caller; the type is
    /// public so the advance door is reachable without a forge path through
    /// fabric.
    pub fn attest(store_id: StoreId, fence_epoch: FenceEpoch) -> Self {
        Self {
            store_id,
            fence_epoch,
        }
    }

    /// Store attested clear.
    pub fn store_id(self) -> StoreId {
        self.store_id
    }

    /// Epoch attested clear.
    pub fn fence_epoch(self) -> FenceEpoch {
        self.fence_epoch
    }
}

/// Advance is itself a Committed event: last mint in the predecessor
/// [`CryptoDomain`], its root the mandatory chain predecessor of the
/// successor domain's genesis root. Recovery advances seal a typed recovery
/// link auditors can distinguish. Seals never span an epoch transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochAdvanceCommitted {
    predecessor: CryptoDomain,
    successor: CryptoDomain,
    /// Present when advance was RecoveryGrant (auditor-distinguishable link).
    recovery_link: Option<RecoveryEpochLink>,
}

impl EpochAdvanceCommitted {
    /// Predecessor CryptoDomain (last mint domain).
    pub fn predecessor(&self) -> CryptoDomain {
        self.predecessor
    }

    /// Successor CryptoDomain (new genesis root chains from predecessor).
    pub fn successor(&self) -> CryptoDomain {
        self.successor
    }

    /// Optional typed recovery link.
    pub fn recovery_link(&self) -> Option<&RecoveryEpochLink> {
        self.recovery_link.as_ref()
    }
}

/// Auditor-distinguishable recovery link sealed into a recovery advance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryEpochLink {
    predecessor_epoch: FenceEpoch,
}

impl RecoveryEpochLink {
    /// Bind the predecessor epoch the recovery revoked.
    pub(crate) fn new(predecessor_epoch: FenceEpoch) -> Self {
        Self { predecessor_epoch }
    }

    /// Predecessor epoch named by the link.
    pub fn predecessor_epoch(&self) -> FenceEpoch {
        self.predecessor_epoch
    }
}

/// Typed refuse from epoch advance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum EpochAdvanceRefuse {
    #[error(
        "EpochAdvanceBlocked: current-epoch Fenced footprints still live (IntentClear required)"
    )]
    #[diagnostic(code(store::epoch::advance_blocked))]
    EpochAdvanceBlocked,
    #[error("EpochGrant store identity does not match WriteAuthority / current domain")]
    #[diagnostic(code(store::epoch::grant_store_mismatch))]
    GrantStoreMismatch,
    #[error("EpochGrant predecessor epoch does not match current FenceEpoch")]
    #[diagnostic(code(store::epoch::grant_epoch_mismatch))]
    GrantEpochMismatch,
    #[error("INVARIANT(FenceEpoch): epoch space exhausted at u64::MAX")]
    #[diagnostic(code(store::epoch::space_exhausted))]
    EpochSpaceExhausted,
}

/// Same-principal live-token advance: new CryptoDomain, same KEK, new IncarnationId
/// at the next open — WriteAuthority remains the immutable signing capability.
///
/// Requires [`IntentClear`]. Consumes the predecessor epoch counter into the
/// [`EpochAdvanceCommitted`] event (the advance *is* a Committed event).
pub fn advance(
    current: CryptoDomain,
    grant: EpochGrant,
    intent_clear: IntentClear,
    authority: &WriteAuthority,
) -> Result<EpochAdvanceCommitted, EpochAdvanceRefuse> {
    if authority.store_id() != current.store_id() || grant.store_id() != current.store_id() {
        return Err(EpochAdvanceRefuse::GrantStoreMismatch);
    }
    if grant.predecessor_epoch() != current.fence_epoch() {
        return Err(EpochAdvanceRefuse::GrantEpochMismatch);
    }
    if intent_clear.store_id() != current.store_id()
        || intent_clear.fence_epoch() != current.fence_epoch()
    {
        return Err(EpochAdvanceRefuse::EpochAdvanceBlocked);
    }
    let next_epoch = current.fence_epoch().successor()?;
    let successor = CryptoDomain::new(current.store_id(), next_epoch);
    Ok(EpochAdvanceCommitted {
        predecessor: current,
        successor,
        recovery_link: None,
    })
}

/// RecoveryGrant advance: IntentClear-exempt under epoch-indexed footprint law.
/// Seals a typed recovery link. New WriteAuthority is minted by grants materialize;
/// this door only advances the epoch domain.
pub fn advance_recovery(
    current: CryptoDomain,
    predecessor_epoch: FenceEpoch,
) -> Result<EpochAdvanceCommitted, EpochAdvanceRefuse> {
    if predecessor_epoch != current.fence_epoch() {
        return Err(EpochAdvanceRefuse::GrantEpochMismatch);
    }
    let next_epoch = current.fence_epoch().successor()?;
    let successor = CryptoDomain::new(current.store_id(), next_epoch);
    Ok(EpochAdvanceCommitted {
        predecessor: current,
        successor,
        recovery_link: Some(RecoveryEpochLink::new(predecessor_epoch)),
    })
}
