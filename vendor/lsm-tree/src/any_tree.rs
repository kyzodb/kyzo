// Copyright (c) 2024-present, fjall-rs
// This source code is licensed under both the Apache 2.0 and MIT License
// (found in the LICENSE-* files in the repository)

use crate::{
    iter_guard::{IterGuard, IterGuardImpl},
    range::TreeIter,
    AbstractTree, BlobTree, KvPair, Memtable, SeqNo, Tree, UserKey,
};
use enum_dispatch::enum_dispatch;
use std::{
    ops::{Bound, RangeBounds},
    sync::Arc,
};

/// May be a standard [`Tree`] or a [`BlobTree`]
#[derive(Clone)]
#[enum_dispatch(AbstractTree)]
pub enum AnyTree {
    /// Standard LSM-tree, see [`Tree`]
    Standard(Tree),

    /// Key-value separated LSM-tree, see [`BlobTree`]
    Blob(BlobTree),
}

impl AnyTree {
    /// The seekable counterpart to [`AbstractTree::range`]: opens ONE
    /// cursor over `range` that a caller re-deriving its lower bound many
    /// times (a skip scan) can reposition via [`SeekableRangeIter::seek`]
    /// instead of reopening.
    ///
    /// Standard trees get the real thing: [`Tree::create_seekable_range`]
    /// returns the concrete [`TreeIter`], whose `seek` reuses the
    /// `SuperVersion` this call already looked up. Key-value separated
    /// trees keep today's reopen-per-seek shape — nothing in this
    /// workspace ever creates a blob keyspace (`kv_separation_opts` stays
    /// `None`), so this arm exists only so the type is total, not because
    /// anything exercises it; teaching blob indirection resolution to
    /// seek in place is unjustified machinery for a path with zero
    /// callers.
    pub fn create_seekable_range<K: AsRef<[u8]>, R: RangeBounds<K>>(
        &self,
        range: R,
        seqno: SeqNo,
        ephemeral: Option<(Arc<Memtable>, SeqNo)>,
    ) -> SeekableRangeIter {
        match self {
            Self::Standard(t) => {
                SeekableRangeIter::Standard(t.create_seekable_range(range, seqno, ephemeral))
            }
            Self::Blob(b) => {
                let upper: Bound<UserKey> = match range.end_bound() {
                    Bound::Included(k) => Bound::Included(k.as_ref().into()),
                    Bound::Excluded(k) => Bound::Excluded(k.as_ref().into()),
                    Bound::Unbounded => Bound::Unbounded,
                };
                let current = AbstractTree::range(b, range, seqno, ephemeral.clone());

                SeekableRangeIter::Blob(Box::new(BlobReopenSeek {
                    tree: b.clone(),
                    seqno,
                    ephemeral,
                    upper,
                    current,
                }))
            }
        }
    }
}

/// The seekable counterpart to [`AnyTree::range`]'s plain iterator.
///
/// One open cursor, repositioned forward by [`Self::seek`] rather than
/// rebuilt. See [`AnyTree::create_seekable_range`] for why the [`Blob`]
/// arm is a reopen fallback rather than a real seek.
///
/// [`Blob`]: SeekableRangeIter::Blob
pub enum SeekableRangeIter {
    /// A standard tree's real, `SuperVersion`-reusing cursor.
    Standard(TreeIter),
    /// A blob tree's reopen-per-seek fallback (dead code from kyzo: no
    /// keyspace here is ever key-value separated).
    Blob(Box<BlobReopenSeek>),
}

impl SeekableRangeIter {
    /// Repositions the cursor forward to the first key at or after
    /// `target` and returns its resolved key/value pair (or `None` if
    /// nothing remains). `target` must be non-decreasing across calls.
    pub fn seek(&mut self, target: &[u8]) -> Option<crate::Result<KvPair>> {
        match self {
            Self::Standard(iter) => iter
                .seek(target)
                .map(|r| r.map(|kv| (kv.key.user_key, kv.value))),
            Self::Blob(reopen) => reopen.seek(target),
        }
    }
}

/// The dead-but-total fallback for key-value separated trees: re-derives
/// a fresh bounded iterator from `target` to the ORIGINAL upper bound on
/// every seek, exactly the reopen-per-step shape this story replaces for
/// standard trees. See [`AnyTree::create_seekable_range`].
pub struct BlobReopenSeek {
    tree: BlobTree,
    seqno: SeqNo,
    ephemeral: Option<(Arc<Memtable>, SeqNo)>,
    upper: Bound<UserKey>,
    current: Box<dyn DoubleEndedIterator<Item = IterGuardImpl> + Send + 'static>,
}

impl BlobReopenSeek {
    fn seek(&mut self, target: &[u8]) -> Option<crate::Result<KvPair>> {
        let lower = Bound::Included(UserKey::from(target));
        let bounds = (lower, self.upper.clone());

        self.current = AbstractTree::range(&self.tree, bounds, self.seqno, self.ephemeral.clone());
        self.current.next().map(IterGuard::into_inner)
    }
}
