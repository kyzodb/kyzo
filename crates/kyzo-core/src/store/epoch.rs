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
//! current-epoch footprints without footprint-proven [`IntentClear`]
//! (RecoveryGrant exempt); bare-arg [`IntentClear`] self-attestation.

use super::authority::WriteAuthority;
use super::open::StoreId;
use crate::session::footprint::{FootprintClearEvidence, LiveFootprintTable};

/// Store-local fence epoch. Genesis is sealed into the genesis digest /
/// [`CryptoDomain`] as a verification fact — fabric never mints write continuity.
///
/// Binds [`StoreId`]: [`FenceEpoch::genesis`] does not discard the identity —
/// epoch counters are store-scoped verification facts, never free-floating u64s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FenceEpoch {
    store_id: StoreId,
    epoch: u64,
}

impl FenceEpoch {
    /// Epoch zero for a Store identity — sealed at genesis; binds `store_id`.
    pub fn genesis(store_id: StoreId) -> Self {
        Self { store_id, epoch: 0 }
    }

    /// Construct from an already-proven epoch (WAL / seal decode) under `store_id`.
    pub(crate) fn of_u64(store_id: StoreId, raw: u64) -> Self {
        Self {
            store_id,
            epoch: raw,
        }
    }

    /// Store identity this epoch counter belongs to.
    pub fn store_id(self) -> StoreId {
        self.store_id
    }

    /// Raw epoch counter.
    pub fn get(self) -> u64 {
        self.epoch
    }

    /// Successor epoch counter (not the advance ceremony — see [`advance`]).
    /// Preserves the bound [`StoreId`].
    pub fn successor(self) -> Result<FenceEpoch, EpochAdvanceRefuse> {
        self.epoch
            .checked_add(1)
            .map(|epoch| FenceEpoch {
                store_id: self.store_id,
                epoch,
            })
            .ok_or(EpochAdvanceRefuse::EpochSpaceExhausted)
    }
}

/// Sealed crypto domain: `(StoreId, FenceEpoch)`.
///
/// Separates DEK/nonce space per store×epoch. Dual-use lineage under one
/// CryptoDomain → poison at chain-meet. StoreId is always the fence epoch's
/// own binding — a disagreeing `store_id` witness cannot forge a dual-identity
/// domain (mismatched bind is Unconstructible).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CryptoDomain {
    store_id: StoreId,
    fence_epoch: FenceEpoch,
}

impl CryptoDomain {
    /// Bind a fence epoch into a crypto domain.
    ///
    /// `store_id` is a call-site witness; the sealed StoreId is always
    /// `fence_epoch.store_id()`. A disagreeing witness cannot construct a
    /// foreign-domain pair (Unconstructible) — never a panic costume.
    pub fn new(store_id: StoreId, fence_epoch: FenceEpoch) -> Self {
        let _witness = store_id;
        Self {
            store_id: fence_epoch.store_id(),
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
    /// Consumes [`FootprintClearEvidence`] minted by
    /// [`LiveFootprintTable::prove_epoch_clear`] — bare `(StoreId, FenceEpoch)`
    /// self-attestation is Unconstructible. Store identity is taken from the
    /// evidence's bound [`FenceEpoch`].
    pub fn attest(evidence: FootprintClearEvidence) -> Self {
        let fence_epoch = evidence.fence_epoch();
        Self {
            store_id: fence_epoch.store_id(),
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
/// Requires [`IntentClear`] (footprint-proven) and a live footprint table with
/// no current-epoch Fenced rows — [`EpochAdvanceRefuse::EpochAdvanceBlocked`]
/// when live Fenced footprints remain. Consumes the predecessor epoch counter
/// into the [`EpochAdvanceCommitted`] event (the advance *is* a Committed event).
pub fn advance(
    current: CryptoDomain,
    grant: EpochGrant,
    intent_clear: IntentClear,
    authority: &WriteAuthority,
    footprints: &LiveFootprintTable,
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
    // Fresh check: IntentClear proves a prior clear; live Fenced still block.
    if footprints.has_live_fenced_in_epoch(current.fence_epoch()) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::footprint::{
        AskShape, ByteRange, FencedFootprint, Footprint, FootprintIndexKey, LiveFootprintTable,
    };
    use crate::store::authority::{Entropy, OpenOrdinal, WriteAuthority};
    use miette::{IntoDiagnostic, Result, miette};

    /// Nasty: attest clear (via empty-table evidence) then insert a live Fenced
    /// footprint — ordinary advance must refuse EpochAdvanceBlocked. Contentless
    /// bare-arg attest is Unconstructible; the forge path is TOCTOU after attest.
    #[test]
    fn intent_clear_attest_while_live_fenced_blocks_epoch_advance() -> Result<()> {
        let store_id = StoreId::from_digest([0xE6; 32]);
        let fence_epoch = FenceEpoch::genesis(store_id);
        let current = CryptoDomain::new(store_id, fence_epoch);
        let grant = EpochGrant::new(store_id, fence_epoch);
        let authority = WriteAuthority::mint(store_id, [0xA7; 32]);

        // Attest clear against an empty live table (honest at attest time).
        let mut footprints = LiveFootprintTable::new();
        let evidence = footprints.prove_epoch_clear(fence_epoch)?;
        let intent_clear = IntentClear::attest(evidence);

        // Live Fenced footprint appears after attest — advance must still refuse.
        let incarnation = authority
            .incarnation_mint_cap(OpenOrdinal::ZERO)
            .mint(Entropy::admit([0xF6; 32]))?;
        let fenced = FencedFootprint::seal(
            Footprint::Exact(vec![ByteRange {
                start: b"a".to_vec(),
                end: b"z".to_vec(),
            }]),
            0,
        )?;
        footprints.insert(
            FootprintIndexKey {
                fence_epoch,
                incarnation_id: incarnation,
            },
            AskShape::Fenced(fenced),
        )?;
        assert!(footprints.has_live_fenced_in_epoch(fence_epoch));

        assert_eq!(
            advance(current, grant, intent_clear, &authority, &footprints),
            Err(EpochAdvanceRefuse::EpochAdvanceBlocked)
        );

        // Prove door itself refuses while the live Fenced remains.
        assert_eq!(
            footprints.prove_epoch_clear(fence_epoch),
            Err(EpochAdvanceRefuse::EpochAdvanceBlocked)
        );

        Ok(())
    }
}
