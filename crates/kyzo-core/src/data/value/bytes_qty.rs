/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Stored-byte quantity types: offset, length, and chunk ordering.
//!
//! Every field in the heap that holds a byte count lives here.
//! Raw `+`/`-`/`*` on these quantities is absent by design: the only
//! arithmetic is through `checked_*` methods, so overflow is a typed
//! refusal (via `Option` propagated to an `.expect()`) in every build
//! profile — independent of the `overflow-checks` Cargo toggle.
//!
//! The three kinds:
//!
//! - [`ByteLen`]: a stored byte count (the `len` field of a heap span).
//! - [`ByteOff`]: a stored byte offset within a chunk (the `off` field).
//! - [`ChunkId`]: a chunk ordering index (the `chunk` field).

// alive only within the value plane; other targets don't use it
#![allow(dead_code)]

/// A stored byte count: the length of a payload held in the arena heap.
///
/// Raw arithmetic operators are absent; callers either use
/// [`ByteLen::checked_add`] or extract to `usize` via [`ByteLen::as_usize`]
/// for slice indexing (where the usize domain is safe).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ByteLen(u32);

impl ByteLen {
    pub(super) const ZERO: ByteLen = ByteLen(0);

    /// Construct from a `usize`; panics if `n > u32::MAX`.
    ///
    /// This is the one construction point: every byte length that enters a
    /// `Span` passes through here, so the stored field can never silently
    /// truncate.
    pub(super) fn from_usize(n: usize) -> ByteLen {
        ByteLen(u32::try_from(n).expect("byte length exceeds u32 span space"))
    }

    /// Extract to `usize` for slice indexing (not for further stored-byte
    /// arithmetic).
    pub(super) fn as_usize(self) -> usize {
        self.0 as usize
    }

    /// The raw `u32` for read-only cross-boundary uses (prefix comparison
    /// takes `u32`; this is a read, not arithmetic).
    pub(super) fn raw(self) -> u32 {
        self.0
    }

    /// Checked addition: `None` on overflow (caller must `.expect()` or handle).
    pub(super) fn checked_add(self, rhs: ByteLen) -> Option<ByteLen> {
        self.0.checked_add(rhs.0).map(ByteLen)
    }
}

/// A stored byte offset within a heap chunk.
///
/// Raw arithmetic operators are absent; use [`ByteOff::checked_add`] to
/// advance by a [`ByteLen`], or [`ByteOff::as_usize`] for slice indexing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ByteOff(u32);

impl ByteOff {
    pub(super) const ZERO: ByteOff = ByteOff(0);

    /// Construct from a `usize`; panics if `n > u32::MAX`.
    pub(super) fn from_usize(n: usize) -> ByteOff {
        ByteOff(u32::try_from(n).expect("byte offset exceeds u32 span space"))
    }

    /// Extract to `usize` for slice indexing.
    pub(super) fn as_usize(self) -> usize {
        self.0 as usize
    }

    /// Advance this offset by a byte length; `None` on overflow.
    pub(super) fn checked_add(self, len: ByteLen) -> Option<ByteOff> {
        self.0.checked_add(len.0).map(ByteOff)
    }
}

/// A heap chunk ordering index.
///
/// Raw arithmetic operators are absent; use [`ChunkId::from_usize`] and
/// [`ChunkId::as_usize`] at the boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ChunkId(u32);

impl ChunkId {
    /// Construct from a `usize`; panics if `n > u32::MAX`.
    pub(super) fn from_usize(n: usize) -> ChunkId {
        ChunkId(u32::try_from(n).expect("heap chunk id space exhausted"))
    }

    pub(super) fn as_usize(self) -> usize {
        self.0 as usize
    }
}
