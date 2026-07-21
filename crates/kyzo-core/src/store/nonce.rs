/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Pipelined NonceLease (decisions.md §25, §62).
//!
//! Owns: [`NonceLease`], [`MintDomain`], [`DomainCounter`], [`nonce`].
//!
//! Bans: raw/volatile nonce constructors; per-intention leases; shared
//! counters across MintDomains.
//!
//! Nonce = pure(MintDomain, DomainCounter, CryptoDomain, IncarnationId).
//! Signature unfrozen until campaigns green (carried obligation).

use sha2::{Digest, Sha256};

use super::authority::IncarnationId;
use super::epoch::CryptoDomain;

/// Closed mint domain — Commit, Compact, and Rotate run the same pipeline
/// shape on independent [`DomainCounter`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MintDomain {
    /// Ordinary commit-path AEAD counters.
    Commit,
    /// Compaction MergeProof / Compact-domain counters.
    Compact,
    /// Key-rotation domain counters.
    Rotate,
}

impl MintDomain {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Stable tag byte for the pure nonce transcript.
    fn tag(self) -> u8 {
        match self {
            MintDomain::Commit => 1,
            MintDomain::Compact => 2,
            MintDomain::Rotate => 3,
        }
    }
}

/// Per-[`MintDomain`] counter. Never shared across domains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DomainCounter(u64);

impl DomainCounter {
    /// Counter zero for a fresh incarnation domain.
    pub const ZERO: DomainCounter = DomainCounter(0);

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Wrap an already-proven counter (WAL / seal decode).
    pub(crate) fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Raw counter value.
    pub fn get(self) -> u64 {
        self.0
    }

    /// Strict successor. Refuses at `u64::MAX`.
    pub fn successor(self) -> Result<DomainCounter, DomainCounterRefuse> {
        self.0
            .checked_add(1)
            .map(DomainCounter)
            .ok_or(DomainCounterRefuse::SpaceExhausted)
    }
}

/// Typed refuse when a domain counter cannot advance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum DomainCounterRefuse {
    #[error("INVARIANT(DomainCounter): counter space exhausted at u64::MAX")]
    #[diagnostic(code(store::nonce::domain_counter_exhausted))]
    SpaceExhausted,
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Durable counter-block lease for AEAD. Reserve-before-encrypt by construction.
///
/// Only constructor consumes a durably-reserved counter block — volatile
/// reservation is Unconstructible. Encrypt takes a [`NonceLease`]; unused
/// remainder burns; crash mid-lease resumes strictly above the highest
/// durably reserved ceiling.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NonceLease {
    domain: MintDomain,
    /// Inclusive start of the reserved block.
    floor: DomainCounter,
    /// Exclusive ceiling of the reserved block (resume strictly above).
    ceiling: DomainCounter,
    crypto_domain: CryptoDomain,
    incarnation_id: IncarnationId,
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
impl NonceLease {
    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Mint a lease over an already-durable reserved counter block.
    ///
    /// `ceiling` must be strictly above `floor`. The encrypt door takes the
    /// returned lease — there is no volatile/raw nonce constructor.
    pub(crate) fn mint(
        domain: MintDomain,
        floor: DomainCounter,
        ceiling: DomainCounter,
        crypto_domain: CryptoDomain,
        incarnation_id: IncarnationId,
    ) -> Result<NonceLease, NonceLeaseRefuse> {
        if ceiling.get() <= floor.get() {
            return Err(NonceLeaseRefuse::EmptyBlock);
        }
        Ok(NonceLease {
            domain,
            floor,
            ceiling,
            crypto_domain,
            incarnation_id,
        })
    }

    /// Mint domain this lease reserves under.
    pub fn domain(&self) -> MintDomain {
        self.domain
    }

    #[allow(dead_code)] // mid-wiring Spec seat — lands with callers
    /// Inclusive floor of the reserved block.
    pub fn floor(&self) -> DomainCounter {
        self.floor
    }

    /// Exclusive ceiling — resume mints strictly above this.
    pub fn ceiling(&self) -> DomainCounter {
        self.ceiling
    }

    /// Crypto domain bound into the lease.
    pub fn crypto_domain(&self) -> CryptoDomain {
        self.crypto_domain
    }

    /// Incarnation bound into the lease.
    pub fn incarnation_id(&self) -> IncarnationId {
        self.incarnation_id
    }

    /// Derive the AEAD nonce for one counter inside this lease.
    pub fn nonce_at(&self, counter: DomainCounter) -> Result<[u8; 12], NonceLeaseRefuse> {
        if counter.get() < self.floor.get() || counter.get() >= self.ceiling.get() {
            return Err(NonceLeaseRefuse::CounterOutsideLease);
        }
        Ok(nonce(
            self.domain,
            counter,
            self.crypto_domain,
            self.incarnation_id,
        ))
    }
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Typed refuse from lease mint / nonce-at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum NonceLeaseRefuse {
    #[error("NonceLease: reserved block must have ceiling strictly above floor")]
    #[diagnostic(code(store::nonce::empty_block))]
    EmptyBlock,
    #[error("NonceLease: counter is outside the reserved [floor, ceiling) block")]
    #[diagnostic(code(store::nonce::counter_outside_lease))]
    CounterOutsideLease,
}

#[allow(dead_code)] // mid-wiring Spec seat — lands with callers
/// Pure nonce derivation: `Nonce = H(MintDomain, DomainCounter, CryptoDomain, IncarnationId)`.
///
/// Signature unfrozen until campaigns green — this is the provisional pure fn.
pub fn nonce(
    domain: MintDomain,
    counter: DomainCounter,
    crypto_domain: CryptoDomain,
    incarnation_id: IncarnationId,
) -> [u8; 12] {
    let mut h = Sha256::new();
    h.update(b"kyzo.nonce.v1");
    h.update([domain.tag()]);
    h.update(u64::to_be_bytes(counter.get()));
    h.update(crypto_domain.store_id().as_bytes());
    h.update(u64::to_be_bytes(crypto_domain.fence_epoch().get()));
    h.update(u64::to_be_bytes(incarnation_id.open_ordinal().get()));
    h.update(incarnation_id.entropy().as_bytes());
    let digest = h.finalize();
    let mut out = [0u8; 12];
    out.copy_from_slice(&digest[..12]);
    out
}
