/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

#![no_main]

//! Fuzzes `lsm_tree::table::IndexBlock` — the restart-point block index
//! `TreeIter`'s seek (`vendor/lsm-tree/src/range.rs`, the machinery
//! `storage/skip_walk.rs` drives once per version step) uses to locate
//! WHICH data block a key falls in before ever touching `DataBlock` itself.
//! Story #118 (own the LSM): ported from this vendored crate's own upstream
//! fuzz target (`vendor/lsm-tree`'s `fuzz/index_block`, an AFL harness)
//! onto our libfuzzer harness.
//!
//! Two API surfaces drifted between that harness and the `lsm-tree` source
//! actually vendored at the SAME published version (3.1.5) — the upstream
//! fuzz crate isn't part of the published/tested artifact, so it lags:
//! - `KeyedBlockHandle::new` takes `(end_key, seqno, BlockHandle)`, not the
//!   upstream target's `(end_key, BlockOffset, u32)`.
//! - `Iter::seek` takes an extra `seqno: SeqNo` (MVCC-aware: an index entry
//!   answers a query only if the query's seqno exceeds the entry's own —
//!   see the law below); `Iter::seek_upper`'s `seqno` parameter is
//!   ignored (`_seqno`), so the upper bound is end-key-only.
//! - `KeyedBlockHandle` has no public `Eq`/`Ord`/`PartialEq` at all (only
//!   `Clone`/`Debug` are unconditional); this target orders/dedups its own
//!   `FuzzyHandle` wrapper by `(end_key, seqno)` directly, mirroring
//!   `InternalKey`'s own convention (`key.rs`: user key ascending, THEN
//!   seqno descending) — the same convention `index_block`'s own unit
//!   fixture (`index_block_mvcc_slab`) packs entries in.
//!
//! Laws:
//! 1. forward and reverse full iteration round-trip the sorted handle list;
//! 2. `next()`/`next_back()` interleaved in a seeded "ping-pong" pattern
//!    matches the same interleave over the plain `Vec`;
//! 3. **the seek law, derived from `index_block_mvcc_slab`'s own fixture**:
//!    `seek(h.end_key(), h.seqno() + 1)` lands EXACTLY on `h`'s own entry.
//!    An entry's own seqno is the newest version behind it; querying one
//!    past it is the smallest query that lands here rather than an
//!    even-newer duplicate sharing the same end key (whose seqno is
//!    strictly greater, since entries are seqno-descending within a run —
//!    `h.seqno() + 1` can never satisfy that entry's own "newer" test);
//! 4. a positional range round-trips via `seek`/`seek_upper`, matching
//!    `index_block_iter_range_1`'s own fixture (`index_block/iter.rs`):
//!    unlike `data_block`'s `seek_upper` (an exact "<= needle" cutoff),
//!    `IndexBlock::Iter::seek_upper` deliberately looks ONE ENTRY PAST
//!    the needle — an index entry's end key is a data BLOCK's boundary,
//!    not a single stored key, so "which blocks might still hold
//!    something at or before the upper bound" always includes the first
//!    block whose own end key exceeds it (that block's start could still
//!    be `<= hi`). `hi` is snapped forward to the last entry sharing its
//!    end key first (a mid-run bound is not a stable seek target — see
//!    law 3), then the expected range is `[lo, hi + 1]` when a further
//!    entry exists, else `[lo, hi]`.

use arbitrary::{Arbitrary, Result, Unstructured};
use libfuzzer_sys::fuzz_target;
use lsm_tree::SeqNo;
use lsm_tree::table::block::decoder::ParsedItem;
use lsm_tree::table::{Block, BlockHandle, BlockOffset, IndexBlock, KeyedBlockHandle};

/// Wraps `KeyedBlockHandle` so the generator can order/dedup it — see the
/// module doc for why this is a hand-written `Ord`/`PartialEq` over
/// `(end_key, seqno)` rather than a derive.
#[derive(Clone, Debug)]
struct FuzzyHandle(KeyedBlockHandle);

/// Comparable projection used everywhere a `KeyedBlockHandle` would
/// otherwise need `PartialEq` (which it only has inside `lsm-tree`'s own
/// `#[cfg(test)]` build, and even there compares by offset only).
fn tup(h: &KeyedBlockHandle) -> (Vec<u8>, SeqNo, u32) {
    (h.end_key().to_vec(), h.seqno(), h.size())
}

impl PartialEq for FuzzyHandle {
    fn eq(&self, other: &Self) -> bool {
        // Matches `Ord`'s own key exactly (`(end_key, seqno)`, `size`
        // excluded): two generated handles claiming the identical
        // `(end_key, seqno)` position are indistinguishable duplicates for
        // dedup purposes — a real index never encodes two entries at one
        // position, so `size` alone must not make them compare unequal
        // (that would let `dedup()` miss the pair, feeding the encoder an
        // ambiguous fixture it was never meant to represent).
        self.0.end_key() == other.0.end_key() && self.0.seqno() == other.0.seqno()
    }
}
impl Eq for FuzzyHandle {}
impl PartialOrd for FuzzyHandle {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for FuzzyHandle {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.0.end_key(), std::cmp::Reverse(self.0.seqno()))
            .cmp(&(other.0.end_key(), std::cmp::Reverse(other.0.seqno())))
    }
}

impl<'a> Arbitrary<'a> for FuzzyHandle {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        let key = Vec::<u8>::arbitrary(u)?;
        let key = if key.is_empty() { vec![0] } else { key };
        let seqno = u64::arbitrary(u)?;
        let size = u32::arbitrary(u)?;

        Ok(Self(KeyedBlockHandle::new(
            key.into(),
            seqno,
            BlockHandle::new(BlockOffset(0), size),
        )))
    }
}

fn generate_ping_pong_code(seed: u64, len: usize) -> Vec<u8> {
    use rand::SeedableRng;
    use rand::prelude::*;
    use rand_chacha::ChaCha8Rng;

    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    (0..len).map(|_| rng.random_range(0..=1)).collect()
}

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);

    let Ok(seed) = u64::arbitrary(&mut u) else {
        return;
    };
    let Ok(mut items) = <Vec<FuzzyHandle> as Arbitrary>::arbitrary(&mut u) else {
        return;
    };
    if items.is_empty() {
        return;
    }

    items.sort();
    items.dedup();
    let items = items.into_iter().map(|v| v.0).collect::<Vec<_>>();

    let bytes = IndexBlock::encode_into_vec(&items)
        .unwrap_or_else(|e| panic!("encode must not fail on well-formed items: {e:?}"));

    let index_block = IndexBlock::new(Block {
        data: bytes.into(),
        header: lsm_tree::table::block::Header {
            block_type: lsm_tree::table::block::BlockType::Index,
            checksum: lsm_tree::Checksum::from_raw(0),
            data_length: 0,
            uncompressed_length: 0,
        },
    });

    assert_eq!(index_block.len(), items.len());

    // Law 1: forward and reverse full round-trip.
    assert_eq!(
        items.iter().map(tup).collect::<Vec<_>>(),
        index_block
            .iter()
            .map(|x| tup(&x.materialize(index_block.as_slice())))
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        items.iter().rev().map(tup).collect::<Vec<_>>(),
        index_block
            .iter()
            .rev()
            .map(|x| tup(&x.materialize(index_block.as_slice())))
            .collect::<Vec<_>>(),
    );

    // Law 2: seeded ping-pong next()/next_back() interleave.
    {
        let ping_pongs = generate_ping_pong_code(seed, items.len());

        let expected = {
            let mut it = items.iter().rev();
            let mut v = vec![];
            for &x in &ping_pongs {
                if x == 0 {
                    v.push(tup(it.next().unwrap()));
                } else {
                    v.push(tup(it.next_back().unwrap()));
                }
            }
            v
        };

        let real = {
            let mut it = index_block
                .iter()
                .rev()
                .map(|x| x.materialize(index_block.as_slice()));
            let mut v = vec![];
            for &x in &ping_pongs {
                if x == 0 {
                    v.push(tup(&it.next().unwrap()));
                } else {
                    v.push(tup(&it.next_back().unwrap()));
                }
            }
            v
        };

        assert_eq!(expected, real);
    }

    // Law 3: the per-entry point-seek law (see module doc). Skipped at
    // `SeqNo::MAX`: the query `h.seqno() + 1` is meant to strictly exceed
    // the entry's own seqno, but at the ceiling there is no successor to
    // saturate to — `Decoder::seek`'s own "predicate still true at the
    // final restart head" bail-out (seeking strictly beyond everything)
    // fires instead, since `s >= seqno` is `MAX >= MAX`, still true.
    for h in items.iter().filter(|h| h.seqno() != SeqNo::MAX) {
        let mut iter = index_block.iter();
        assert!(iter.seek(h.end_key(), h.seqno() + 1), "should seek");
        let first = iter.next().expect("seek landed inside the block");
        assert_eq!(tup(&first.materialize(index_block.as_slice())), tup(h));
    }

    // Law 4: a positional range round-trips via seek/seek_upper. Skipped
    // whenever the sampled lower bound sits at `SeqNo::MAX` (see law 3's
    // comment on why `seqno + 1` isn't meaningful there).
    {
        use rand::SeedableRng;
        use rand::prelude::*;
        use rand_chacha::ChaCha8Rng;

        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let lo = rng.random_range(0..items.len());
        let mut hi = rng.random_range(0..items.len());
        let lo = lo.min(hi);
        hi = lo.max(hi);

        if items[lo].seqno() == SeqNo::MAX {
            return;
        }

        // `seek_upper` cannot stop mid-run (its predicate is end-key-only),
        // so snap `hi` forward to the last entry sharing its end key.
        while hi < items.len() - 1 && items[hi + 1].end_key() == items[hi].end_key() {
            hi += 1;
        }

        // `seek_upper` deliberately looks one entry PAST the needle (see the
        // module doc's law 4) — the expected range includes it too, when it
        // exists.
        let hi_inclusive = (hi + 1).min(items.len() - 1);
        let expected_range: Vec<_> = items[lo..=hi_inclusive].iter().map(tup).collect();

        let mut iter = index_block.iter();
        assert!(
            iter.seek(items[lo].end_key(), items[lo].seqno() + 1),
            "should seek"
        );
        assert!(iter.seek_upper(items[hi].end_key(), 0), "should seek");

        assert_eq!(
            expected_range,
            iter.map(|x| tup(&x.materialize(index_block.as_slice())))
                .collect::<Vec<_>>(),
        );
    }
});
