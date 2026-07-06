// Copyright (c) 2024-present, fjall-rs
// This source code is licensed under both the Apache 2.0 and MIT License
// (found in the LICENSE-* files in the repository)

use crate::{
    key::InternalKey,
    memtable::Memtable,
    merge::Merger,
    mvcc_stream::MvccStream,
    run_reader::RunReader,
    value::{SeqNo, UserKey},
    version::SuperVersion,
    BoxedIterator, InternalValue,
};
use self_cell::self_cell;
use std::{
    ops::{Bound, RangeBounds},
    sync::Arc,
};

#[must_use]
pub fn seqno_filter(item_seqno: SeqNo, seqno: SeqNo) -> bool {
    item_seqno < seqno
}

/// Calculates the prefix's upper range.
///
/// # Panics
///
/// Panics if the prefix is empty.
pub(crate) fn prefix_upper_range(prefix: &[u8]) -> Bound<UserKey> {
    use std::ops::Bound::{Excluded, Unbounded};

    assert!(!prefix.is_empty(), "prefix may not be empty");

    let mut end = prefix.to_vec();
    let len = end.len();

    for (idx, byte) in end.iter_mut().rev().enumerate() {
        let idx = len - 1 - idx;

        if *byte < 255 {
            *byte += 1;
            end.truncate(idx + 1);
            return Excluded(end.into());
        }
    }

    Unbounded
}

/// Converts a prefix to range bounds.
#[must_use]
#[expect(clippy::module_name_repetitions)]
pub fn prefix_to_range(prefix: &[u8]) -> (Bound<UserKey>, Bound<UserKey>) {
    use std::ops::Bound::{Included, Unbounded};

    if prefix.is_empty() {
        return (Unbounded, Unbounded);
    }

    (Included(prefix.into()), prefix_upper_range(prefix))
}

/// The iter state references the memtables used while the range is open
///
/// Because of Rust rules, the state is referenced using `self_cell`, see below.
pub struct IterState {
    pub(crate) version: SuperVersion,
    pub(crate) ephemeral: Option<(Arc<Memtable>, SeqNo)>,
}

type BoxedMerge<'a> = Box<dyn DoubleEndedIterator<Item = crate::Result<InternalValue>> + Send + 'a>;

self_cell!(
    struct MergedIter {
        owner: IterState,

        #[covariant]
        dependent: BoxedMerge,
    }
);

impl Iterator for MergedIter {
    type Item = crate::Result<InternalValue>;

    fn next(&mut self) -> Option<Self::Item> {
        self.with_dependent_mut(|_, iter| iter.next())
    }
}

impl DoubleEndedIterator for MergedIter {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.with_dependent_mut(|_, iter| iter.next_back())
    }
}

/// Builds the merged, MVCC-resolved, tombstone-filtered stream over
/// `bounds` from an ALREADY-OPEN `IterState` — no `SuperVersion` lookup,
/// no lock, just walking the runs/tables/memtables this `IterState`
/// already pins. This is the one place that logic lives: both the first
/// open ([`TreeIter::create_range`]) and every later re-seek
/// ([`TreeIter::seek`]) call it, so a re-seek pays only for repositioning
/// each backing run/table/memtable at the new lower bound (their own
/// range entry point: index block, then restart-point binary search,
/// then linear scan — never the point-get hash index), not for
/// rediscovering which runs/tables/memtables exist.
fn build_merged(
    lock: &IterState,
    bounds: (Bound<UserKey>, Bound<UserKey>),
    seqno: SeqNo,
) -> BoxedMerge<'_> {
    // NOTE: See memtable.rs for range explanation
    let lo = match bounds.0 {
        Bound::Included(key) => Bound::Included(InternalKey::new(
            key,
            SeqNo::MAX,
            crate::ValueType::Tombstone,
        )),
        Bound::Excluded(key) => {
            Bound::Excluded(InternalKey::new(key, 0, crate::ValueType::Tombstone))
        }
        Bound::Unbounded => Bound::Unbounded,
    };

    // NOTE: See memtable.rs for range explanation, this is the reverse case
    // where we need to go all the way to the last seqno of an item
    //
    // Example: We search for (Unbounded..Excluded(abdef))
    //
    // key -> seqno
    //
    // a   -> 7 <<< This is the lowest key that matches the range
    // abc -> 5
    // abc -> 4
    // abc -> 3 <<< This is the highest key that matches the range
    // abcdef -> 6
    // abcdef -> 5
    //
    let hi = match bounds.1 {
        Bound::Included(key) => Bound::Included(InternalKey::new(key, 0, crate::ValueType::Value)),
        Bound::Excluded(key) => {
            Bound::Excluded(InternalKey::new(key, SeqNo::MAX, crate::ValueType::Value))
        }
        Bound::Unbounded => Bound::Unbounded,
    };

    let range = (lo, hi);

    let mut iters: Vec<BoxedIterator<'_>> = Vec::with_capacity(5);

    for run in lock
        .version
        .version
        .iter_levels()
        .flat_map(|lvl| lvl.iter())
    {
        match run.len() {
            0 => {
                // Do nothing
            }
            1 => {
                #[expect(clippy::expect_used, reason = "we checked for length")]
                let table = run.first().expect("should exist");

                if table.check_key_range_overlap(&(
                    range.start_bound().map(|x| &*x.user_key),
                    range.end_bound().map(|x| &*x.user_key),
                )) {
                    let reader = table
                        .range((
                            range.start_bound().map(|x| &x.user_key).cloned(),
                            range.end_bound().map(|x| &x.user_key).cloned(),
                        ))
                        .filter(move |item| match item {
                            Ok(item) => seqno_filter(item.key.seqno, seqno),
                            Err(_) => true,
                        });

                    iters.push(Box::new(reader));
                }
            }
            _ => {
                if let Some(reader) = RunReader::new(
                    run.clone(),
                    (
                        range.start_bound().map(|x| &x.user_key).cloned(),
                        range.end_bound().map(|x| &x.user_key).cloned(),
                    ),
                ) {
                    iters.push(Box::new(reader.filter(move |item| match item {
                        Ok(item) => seqno_filter(item.key.seqno, seqno),
                        Err(_) => true,
                    })));
                }
            }
        }
    }

    // Sealed memtables
    for memtable in lock.version.sealed_memtables.iter() {
        let iter = memtable.range(range.clone());

        iters.push(Box::new(
            iter.filter(move |item| seqno_filter(item.key.seqno, seqno))
                .map(Ok),
        ));
    }

    // Active memtable
    {
        let iter = lock.version.active_memtable.range(range.clone());

        iters.push(Box::new(
            iter.filter(move |item| seqno_filter(item.key.seqno, seqno))
                .map(Ok),
        ));
    }

    if let Some((mt, seqno)) = &lock.ephemeral {
        let iter = Box::new(
            mt.range(range)
                .filter(move |item| seqno_filter(item.key.seqno, *seqno))
                .map(Ok),
        );
        iters.push(iter);
    }

    let merged = Merger::new(iters);
    let iter = MvccStream::new(merged);

    Box::new(iter.filter(|x| match x {
        Ok(value) => !value.key.is_tombstone(),
        Err(_) => true,
    }))
}

/// A single positioned merge cursor over one bounded range of one open
/// `SuperVersion` — the seekable counterpart to a plain range iterator.
///
/// [`TreeIter::seek`] repositions this cursor forward to a new lower
/// bound IN PLACE: it reuses the `SuperVersion`/ephemeral memtable this
/// `TreeIter` was opened with (no new version-history lookup, no new
/// lock — the expensive part of "opening a fresh range"), rebuilding only
/// the merge/heap/tombstone-filter stack from that same owner. Each
/// backing run/table/memtable repositions itself at the new lower bound
/// through its own range entry point (index block, then restart-point
/// binary search, then linear scan), never through the point-get hash
/// index.
pub struct TreeIter {
    inner: MergedIter,
    upper: Bound<UserKey>,
    seqno: SeqNo,
}

impl Iterator for TreeIter {
    type Item = crate::Result<InternalValue>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

impl DoubleEndedIterator for TreeIter {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.inner.next_back()
    }
}

impl TreeIter {
    pub fn create_range<K: AsRef<[u8]>, R: RangeBounds<K>>(
        guard: IterState,
        range: R,
        seqno: SeqNo,
    ) -> Self {
        let lo: Bound<UserKey> = match range.start_bound() {
            Bound::Included(key) => Bound::Included(key.as_ref().into()),
            Bound::Excluded(key) => Bound::Excluded(key.as_ref().into()),
            Bound::Unbounded => Bound::Unbounded,
        };
        let hi: Bound<UserKey> = match range.end_bound() {
            Bound::Included(key) => Bound::Included(key.as_ref().into()),
            Bound::Excluded(key) => Bound::Excluded(key.as_ref().into()),
            Bound::Unbounded => Bound::Unbounded,
        };

        let upper = hi.clone();
        let inner = MergedIter::new(guard, |lock| build_merged(lock, (lo, hi), seqno));

        Self {
            inner,
            upper,
            seqno,
        }
    }

    /// Repositions this cursor forward to the first key at or after
    /// `target`, within the range it was opened with, and returns that
    /// key/value (or `None` if nothing remains). `target` must be
    /// non-decreasing across calls on the same cursor — this is a
    /// forward-only seek, not a general reposition.
    pub fn seek(&mut self, target: &[u8]) -> Option<crate::Result<InternalValue>> {
        let lower = Bound::Included(target.into());
        let upper = self.upper.clone();
        let seqno = self.seqno;

        self.inner.with_dependent_mut(|lock, dependent| {
            *dependent = build_merged(lock, (lower, upper), seqno);
        });

        self.inner.next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Slice;
    use std::ops::Bound::{Excluded, Included, Unbounded};
    use test_log::test;

    fn test_prefix(prefix: &[u8], upper_bound: Bound<&[u8]>) {
        let range = prefix_to_range(prefix);
        assert_eq!(
            range,
            (
                match prefix {
                    _ if prefix.is_empty() => Unbounded,
                    _ => Included(Slice::from(prefix)),
                },
                upper_bound.map(Slice::from),
            ),
        );
    }

    #[test]
    fn prefix_to_range_basic() {
        test_prefix(b"abc", Excluded(b"abd"));
    }

    #[test]
    fn prefix_to_range_empty() {
        test_prefix(b"", Unbounded);
    }

    #[test]
    fn prefix_to_range_single_char() {
        test_prefix(b"a", Excluded(b"b"));
    }

    #[test]
    fn prefix_to_range_1() {
        test_prefix(&[0, 250], Excluded(&[0, 251]));
    }

    #[test]
    fn prefix_to_range_2() {
        test_prefix(&[0, 250, 50], Excluded(&[0, 250, 51]));
    }

    #[test]
    fn prefix_to_range_3() {
        test_prefix(&[255, 255, 255], Unbounded);
    }

    #[test]
    fn prefix_to_range_char_max() {
        test_prefix(&[0, 255], Excluded(&[1]));
    }

    #[test]
    fn prefix_to_range_char_max_2() {
        test_prefix(&[0, 2, 255], Excluded(&[0, 3]));
    }
}
