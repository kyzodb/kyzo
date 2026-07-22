/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Typed digest and region identities for the private record model (#268 purity).
//!
//! Raw `[u8; 32]` / `[u8; 16]` bags are not semantic identity on admission
//! surfaces — each meaning gets a newtype with a private field.

/// Content-addressed digest of an admitted record's typed body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordContentDigest([u8; 32]);

impl RecordContentDigest {
    /// Wrap an already-proven record content digest.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Borrow the digest bytes.
    pub fn as_digest(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Content hash of an evidence span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    /// Wrap an already-proven evidence content hash.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Borrow the hash bytes.
    pub fn as_digest(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Provenance digest bound to evidence coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProvenanceDigest([u8; 32]);

impl ProvenanceDigest {
    /// Wrap an already-proven provenance digest.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Borrow the digest bytes.
    pub fn as_digest(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Geography / residency region identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegionId([u8; 16]);

impl RegionId {
    /// Wrap an already-proven region identity.
    pub fn admit(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Borrow the region identity bytes.
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}
