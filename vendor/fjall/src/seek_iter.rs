// Copyright (c) 2024-present, fjall-rs
// This source code is licensed under both the Apache 2.0 and MIT License
// (found in the LICENSE-* files in the repository)

use crate::snapshot_nonce::SnapshotNonce;
use lsm_tree::{SeekableRangeIter, UserKey, UserValue};

/// A single positioned merge cursor over a keyspace range.
///
/// Unlike [`crate::Iter`], which walks straight through its range once,
/// `SeekIter` is meant for a caller that re-derives its own lower bound
/// many times over the SAME open range — a skip scan. [`Self::seek`]
/// repositions the cursor forward in place (reusing the version this
/// cursor was opened against) instead of paying to reopen a fresh range
/// per step.
pub struct SeekIter {
    inner: SeekableRangeIter,

    #[expect(unused)]
    nonce: SnapshotNonce,
}

impl SeekIter {
    pub(crate) fn new(nonce: SnapshotNonce, inner: SeekableRangeIter) -> Self {
        Self { inner, nonce }
    }

    /// Repositions the cursor forward to the first key at or after
    /// `target`, within the range it was opened with, and returns that
    /// key/value pair — or `None` if nothing remains.
    ///
    /// `target` must be non-decreasing across calls on the same cursor:
    /// this is a forward-only seek, not a general reposition.
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn seek(&mut self, target: &[u8]) -> Option<crate::Result<(UserKey, UserValue)>> {
        self.inner.seek(target).map(|r| r.map_err(Into::into))
    }
}
