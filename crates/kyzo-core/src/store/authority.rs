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
//! [`OpenOrdinal`], [`Entropy`], [`RecoveryMatrix`] (FROST group verifying key),
//! address fence.
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
    pub(crate) fn of_u64(raw: u64) -> Self {
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
    pub fn admit(bytes: [u8; 32]) -> Self {
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
    #[error(
        "INVARIANT(IncarnationMintCap): OpenOrdinal recycle inside one WAL lineage is Unconstructible"
    )]
    #[diagnostic(code(store::authority::incarnation_recycle))]
    OrdinalRecycleUnconstructible,
}

/// Opaque WriteAuthority token identity (not a KEK derivation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WriteTokenId([u8; 32]);

impl WriteTokenId {
    /// Wrap an already-proven token identity digest.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Borrow the token identity bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
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
    token_id: WriteTokenId,
    store_id: StoreId,
}

impl WriteAuthority {
    /// Genesis / grant materialize door — private to the store zone.
    pub(crate) fn mint(store_id: StoreId, token_id: WriteTokenId) -> Self {
        Self {
            token_id,
            store_id,
        }
    }

    /// Store identity this authority signs for.
    pub fn store_id(&self) -> StoreId {
        self.store_id
    }

    /// Typed token identity (keystore indexing, never a watermark).
    pub fn write_token_id(&self) -> WriteTokenId {
        self.token_id
    }

    /// Opaque token identity bytes at the wire / index edge only.
    pub fn token_id(&self) -> &WriteTokenId {
        &self.token_id
    }

    /// Opening with this authority yields a session-only [`IncarnationMintCap`].
    pub fn incarnation_mint_cap(&self, highest_sealed: OpenOrdinal) -> IncarnationMintCap {
        IncarnationMintCap::issue(self.store_id, highest_sealed)
    }
}

/// Sealed FROST group verifying key for recovery quorum (RFC 9591).
///
/// Distinct class from WriteAuthority. This is the **group** verifying key
/// from `frost-ed25519` (ZcashFoundation) — not an enumerated per-custodian
/// ed25519 key. Custodian shares live off-artifact; only this group key is
/// sealed into [`RecoveryMatrix`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecoveryPublicKey([u8; 32]);

impl RecoveryPublicKey {
    /// Wrap already-proven FROST group verifying-key bytes (32-byte compressed).
    pub fn admit(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow the group verifying-key bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Optional FROST recovery matrix sealed at genesis only.
///
/// Seals a FROST (RFC 9591 / `frost-ed25519`) **group verifying key** plus
/// threshold metadata (`threshold` = min_signers, `max_signers` = n) so
/// proactive share refresh (`frost_ed25519::keys::refresh`) can be seated
/// without re-enumerating custodian public keys. Post-genesis mutation is
/// Unconstructible — no setters exist. Absent matrix → lost token means fork
/// (not in-place recovery).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryMatrix {
    /// FROST min_signers (t).
    threshold: u32,
    /// FROST max_signers (n) — participant cardinality for proactive refresh.
    max_signers: u32,
    /// Sole sealed public material: the FROST group verifying key.
    group_verifying_key: RecoveryPublicKey,
}

/// Typed refuse constructing a [`RecoveryMatrix`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum RecoveryMatrixRefuse {
    #[error(
        "RecoveryMatrix: threshold must be ≥ 1 and ≤ max_signers ({max_signers}); max_signers must be in 1..=u16::MAX"
    )]
    #[diagnostic(code(store::authority::recovery_matrix_threshold))]
    ThresholdOutOfRange { max_signers: u32 },
    #[error("RecoveryMatrix: group verifying key is not a valid frost-ed25519 VerifyingKey")]
    #[diagnostic(code(store::authority::recovery_matrix_group_key))]
    InvalidGroupVerifyingKey,
}

impl RecoveryMatrix {
    /// Seal a FROST recovery matrix at genesis. No post-genesis mutation path.
    ///
    /// `group_verifying_key` must deserialize as a `frost-ed25519` VerifyingKey
    /// (RFC 9591 group key). Enumerated N-of-M ed25519 custodian public keys
    /// are condemned — threshold crypto is FROST only.
    pub fn new(
        threshold: u32,
        max_signers: u32,
        group_verifying_key: RecoveryPublicKey,
    ) -> Result<Self, RecoveryMatrixRefuse> {
        if max_signers == 0 || max_signers > u32::from(u16::MAX) {
            return Err(RecoveryMatrixRefuse::ThresholdOutOfRange { max_signers });
        }
        if threshold == 0 || threshold > max_signers {
            return Err(RecoveryMatrixRefuse::ThresholdOutOfRange { max_signers });
        }
        frost_ed25519::VerifyingKey::deserialize(group_verifying_key.as_bytes().as_slice())
            .map_err(|_| RecoveryMatrixRefuse::InvalidGroupVerifyingKey)?;
        Ok(Self {
            threshold,
            max_signers,
            group_verifying_key,
        })
    }

    /// FROST min_signers (t).
    pub fn threshold(&self) -> u32 {
        self.threshold
    }

    /// FROST max_signers (n) — proactive-refresh participant cardinality.
    pub fn max_signers(&self) -> u32 {
        self.max_signers
    }

    /// Sealed FROST group verifying key.
    pub fn group_verifying_key(&self) -> &RecoveryPublicKey {
        &self.group_verifying_key
    }

    /// Genesis / digest surface: the sealed group verifying key as a one-element
    /// slice. Not an enumerated custodian set — FROST does not seal N public keys.
    pub fn keys(&self) -> &[RecoveryPublicKey] {
        std::slice::from_ref(&self.group_verifying_key)
    }
}

/// Process-local address fence: one writer among Engines sharing one token.
///
/// Not protection against `cp -r` — local mutex only → [`AddressFenceRefuse::StoreFenced`].
#[derive(Debug)]
pub struct AddressFence {
    store_id: StoreId,
    token_id: WriteTokenId,
}

/// Typed refuse from the address fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum AddressFenceRefuse {
    #[error("StoreFenced: second writer against a locally fenced address")]
    #[diagnostic(code(store::authority::store_fenced))]
    StoreFenced,
    #[error("AddressFenceLockPoisoned: process-local fence mutex poisoned")]
    #[diagnostic(code(store::authority::fence_lock_poisoned))]
    LockPoisoned,
}

/// Process-local registry of held fences (one claim per store address).
///
/// Interior mutability is the stated concurrency need: multiple Engine
/// handles in one process share the fence table.
#[derive(Debug)]
pub struct AddressFenceTable {
    held: std::sync::Mutex<std::collections::BTreeSet<[u8; 32]>>,
}

impl AddressFenceTable {
    /// Empty fence table.
    pub fn new() -> Self {
        Self {
            held: std::sync::Mutex::new(std::collections::BTreeSet::new()),
        }
    }

    /// Claim the local address for this WriteAuthority. Second claim → StoreFenced.
    pub fn claim(&self, authority: &WriteAuthority) -> Result<AddressFence, AddressFenceRefuse> {
        let key = fence_key(authority.store_id(), &authority.write_token_id());
        let mut held = self
            .held
            .lock()
            .map_err(|_| AddressFenceRefuse::LockPoisoned)?;
        if !held.insert(key) {
            return Err(AddressFenceRefuse::StoreFenced);
        }
        Ok(AddressFence {
            store_id: authority.store_id(),
            token_id: authority.write_token_id(),
        })
    }

    /// Release a held fence (Drop path for orderly unlock).
    ///
    /// Poisoned mutex → no-op release (process already crashed a holder);
    /// never panics on the Drop path.
    pub fn release(&self, fence: AddressFence) {
        let key = fence_key(fence.store_id, &fence.token_id);
        let Ok(mut held) = self.held.lock() else {
            return;
        };
        held.remove(&key);
    }
}

impl AddressFence {
    /// Store identity under this fence.
    pub fn store_id(&self) -> StoreId {
        self.store_id
    }
}

fn fence_key(store_id: StoreId, token_id: &WriteTokenId) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"kyzo.address_fence.v1");
    h.update(store_id.as_bytes());
    h.update(token_id.as_bytes());
    h.finalize().into()
}
