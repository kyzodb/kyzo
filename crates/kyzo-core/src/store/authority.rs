/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Write authority and incarnation identity (decisions.md §2, §62).
//!
//! Owns: [`WriteAuthority`], [`IncarnationMintCap`], [`IncarnationId`],
//! [`OpenOrdinal`], [`Entropy`], [`RecoveryMatrix`], address fence.
//!
//! Bans: watermark / consume-reissue on WriteAuthority; `derive(KEK,…)` for
//! WriteAuthority; IncarnationMintCap serialization (never in packs); a
//! second floor organ outside the WAL.

use super::open::StoreId;

/// Store-monotonic open ordinal. Minted strictly above every sealed
/// predecessor in the lineage — recycle is constructor-guarded Unconstructible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpenOrdinal(u64);

impl OpenOrdinal {
    /// Genesis / first-open ordinal.
    pub const ZERO: OpenOrdinal = OpenOrdinal(0);

    /// Construct from an already-proven ordinal (WAL / seal decode sites).
    pub(crate) fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw ordinal value.
    pub fn get(self) -> u64 {
        self.0
    }

    /// Next ordinal strictly above `self`. Refuses at `u64::MAX`.
    pub fn successor(self) -> Result<OpenOrdinal, OpenOrdinalRefuse> {
        self.0
            .checked_add(1)
            .map(OpenOrdinal)
            .ok_or(OpenOrdinalRefuse::SpaceExhausted)
    }
}

/// Typed refuse when open-ordinal space cannot advance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum OpenOrdinalRefuse {
    #[error("INVARIANT(OpenOrdinal): ordinal space exhausted at u64::MAX")]
    #[diagnostic(code(store::authority::open_ordinal_exhausted))]
    SpaceExhausted,
}

/// Clone-distinguishing entropy from the approved genesis entropy arm.
/// Distinctness is Entropy-bounded (Unexposed), never claimed Unconstructible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Entropy([u8; 32]);

impl Entropy {
    /// Wrap already-drawn entropy bytes (arm sites that already hold the proof).
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow the entropy bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Durable incarnation identity: `(OpenOrdinal, Entropy)`.
///
/// WAL-headed, seal-bound, included in leave-is-free packs. Historical
/// recycle inside one WAL lineage is Unconstructible — mint only via
/// [`IncarnationMintCap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IncarnationId {
    open_ordinal: OpenOrdinal,
    entropy: Entropy,
}

impl IncarnationId {
    /// Open ordinal half of the identity.
    pub fn open_ordinal(self) -> OpenOrdinal {
        self.open_ordinal
    }

    /// Entropy half of the identity.
    pub fn entropy(self) -> Entropy {
        self.entropy
    }
}

/// Session-only mint capability for [`IncarnationId`].
///
/// Obtained when opening with matching [`WriteAuthority`]. Never exported,
/// never serialized, never present in packs (decisions.md §65).
///
/// No `Serialize` / `Deserialize` — serialization of MintCap is Unconstructible.
#[derive(Debug)]
pub struct IncarnationMintCap {
    store_id: StoreId,
    /// Highest sealed open ordinal observed in this lineage's WAL/seal history.
    highest_sealed: OpenOrdinal,
}

impl IncarnationMintCap {
    /// Session door: mint cap for one Store after WriteAuthority presentation.
    pub(crate) fn issue(store_id: StoreId, highest_sealed: OpenOrdinal) -> Self {
        Self {
            store_id,
            highest_sealed,
        }
    }

    /// Store this cap is scoped to.
    pub fn store_id(&self) -> StoreId {
        self.store_id
    }

    /// Mint an [`IncarnationId`] whose ordinal strictly exceeds every sealed
    /// predecessor. Consumes the cap (affine session use).
    pub fn mint(self, entropy: Entropy) -> Result<IncarnationId, IncarnationMintRefuse> {
        let open_ordinal = self
            .highest_sealed
            .successor()
            .map_err(|_| IncarnationMintRefuse::OrdinalRecycleUnconstructible)?;
        Ok(IncarnationId {
            open_ordinal,
            entropy,
        })
    }
}

/// Typed refuse from [`IncarnationMintCap::mint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum IncarnationMintRefuse {
    #[error("INVARIANT(IncarnationMintCap): OpenOrdinal recycle inside one WAL lineage is Unconstructible")]
    #[diagnostic(code(store::authority::incarnation_recycle))]
    OrdinalRecycleUnconstructible,
}

/// Immutable affine signing capability that alone authorizes the SweepDoor.
///
/// Minted at genesis / recovery / fork. Lives in the client keystore or HSM
/// beside the root KEK — never in Store artifacts or leave-is-free packs,
/// never `derive(KEK, …)`. No mutable watermark; all mutable floors live in
/// the Store WAL. Affine: no `Clone` — HA is token move.
#[derive(Debug, PartialEq, Eq)]
pub struct WriteAuthority {
    /// Opaque token identity (not a KEK derivation).
    token_id: [u8; 32],
    store_id: StoreId,
}

impl WriteAuthority {
    /// Genesis / grant materialize door — private to the store zone.
    pub(crate) fn mint(store_id: StoreId, token_id: [u8; 32]) -> Self {
        Self { token_id, store_id }
    }

    /// Store identity this authority signs for.
    pub fn store_id(&self) -> StoreId {
        self.store_id
    }

    /// Opaque token identity bytes (keystore indexing, never a watermark).
    pub fn token_id(&self) -> &[u8; 32] {
        &self.token_id
    }

    /// Opening with this authority yields a session-only [`IncarnationMintCap`].
    pub fn incarnation_mint_cap(&self, highest_sealed: OpenOrdinal) -> IncarnationMintCap {
        IncarnationMintCap::issue(self.store_id, highest_sealed)
    }
}

/// One recovery custodian public key (distinct class from WriteAuthority).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecoveryPublicKey([u8; 32]);

impl RecoveryPublicKey {
    /// Wrap a recovery-custodian public key.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow the key bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Optional M-of-N recovery matrix sealed at genesis only.
///
/// Post-genesis mutation is Unconstructible — no setters exist. Absent
/// matrix → lost token means fork (not in-place recovery).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryMatrix {
    threshold: u32,
    keys: Box<[RecoveryPublicKey]>,
}

/// Typed refuse constructing a [`RecoveryMatrix`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum RecoveryMatrixRefuse {
    #[error("RecoveryMatrix: threshold must be ≥ 1 and ≤ key count ({key_count})")]
    #[diagnostic(code(store::authority::recovery_matrix_threshold))]
    ThresholdOutOfRange { key_count: usize },
    #[error("RecoveryMatrix: key set must be non-empty")]
    #[diagnostic(code(store::authority::recovery_matrix_empty))]
    EmptyKeySet,
}

impl RecoveryMatrix {
    /// Seal an M-of-N matrix at genesis. No post-genesis mutation path.
    pub fn new(
        threshold: u32,
        keys: impl Into<Vec<RecoveryPublicKey>>,
    ) -> Result<Self, RecoveryMatrixRefuse> {
        let keys = keys.into();
        if keys.is_empty() {
            return Err(RecoveryMatrixRefuse::EmptyKeySet);
        }
        let key_count = keys.len();
        if threshold == 0 || threshold as usize > key_count {
            return Err(RecoveryMatrixRefuse::ThresholdOutOfRange { key_count });
        }
        Ok(Self {
            threshold,
            keys: keys.into_boxed_slice(),
        })
    }

    /// Quorum threshold M.
    pub fn threshold(&self) -> u32 {
        self.threshold
    }

    /// Custodian public keys (N).
    pub fn keys(&self) -> &[RecoveryPublicKey] {
        &self.keys
    }
}

/// Process-local address fence: one writer among Engines sharing one token.
///
/// Not protection against `cp -r` — local mutex only → [`AddressFenceRefuse::StoreFenced`].
#[derive(Debug)]
pub struct AddressFence {
    store_id: StoreId,
    token_id: [u8; 32],
}

/// Typed refuse from the address fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum AddressFenceRefuse {
    #[error("StoreFenced: second writer against a locally fenced address")]
    #[diagnostic(code(store::authority::store_fenced))]
    StoreFenced,
}

/// Process-local registry of held fences (one claim per store address).
///
/// Interior mutability is the stated concurrency need: multiple Engine
/// handles in one process share the fence table.
#[derive(Debug, Default)]
pub struct AddressFenceTable {
    held: std::sync::Mutex<std::collections::BTreeSet<[u8; 32]>>,
}

impl AddressFenceTable {
    /// Empty fence table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim the local address for this WriteAuthority. Second claim → StoreFenced.
    pub fn claim(&self, authority: &WriteAuthority) -> Result<AddressFence, AddressFenceRefuse> {
        let key = fence_key(authority.store_id(), authority.token_id());
        let mut held = self.held.lock().expect("address fence mutex");
        if !held.insert(key) {
            return Err(AddressFenceRefuse::StoreFenced);
        }
        Ok(AddressFence {
            store_id: authority.store_id(),
            token_id: *authority.token_id(),
        })
    }

    /// Release a held fence (Drop path for orderly unlock).
    pub fn release(&self, fence: AddressFence) {
        let key = fence_key(fence.store_id, &fence.token_id);
        let mut held = self.held.lock().expect("address fence mutex");
        held.remove(&key);
    }
}

impl AddressFence {
    /// Store identity under this fence.
    pub fn store_id(&self) -> StoreId {
        self.store_id
    }
}

fn fence_key(store_id: StoreId, token_id: &[u8; 32]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"kyzo.address_fence.v1");
    h.update(store_id.as_bytes());
    h.update(token_id);
    h.finalize().into()
}
