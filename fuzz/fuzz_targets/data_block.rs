/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

#![no_main]

//! Fuzzes `lsm_tree::table::DataBlock` — the leaf block format `TreeIter`'s
//! seek (`vendor/lsm-tree/src/range.rs`, the machinery `storage/skip_walk.rs`
//! drives per version step) bottoms out on: restart-point prefix
//! compression, the optional hash index, and the binary-search/linear-scan
//! decode that recovers exact rows from it. Story #118 (own the LSM): ported
//! from this vendored crate's own upstream fuzz target
//! (`vendor/lsm-tree`'s `fuzz/data_block`, an AFL harness) onto our
//! libfuzzer harness — the block-level code this story now owns gets our
//! coverage, not none.
//!
//! Note on the port: the upstream target derives `Ord`/`Eq` on its
//! `FuzzyValue(InternalValue)` wrapper directly, which requires
//! `InternalValue: Ord` — true against whatever revision that harness was
//! last written against, but NOT true of the `InternalValue` this exact
//! vendored version ships (only `Clone` and `Debug` are unconditional;
//! `PartialEq`/`Eq`/`Ord` don't exist outside `lsm-tree`'s own `#[cfg(test)]`
//! build). The fix here is the same one `table_read.rs`'s upstream target
//! already uses for the identical problem: order and dedup by `.key`
//! (`InternalKey`, which DOES implement `Ord` unconditionally — user key
//! ascending, then seqno DESCENDING, `key.rs`) rather than deriving from a
//! type that doesn't offer it.
//!
//! Laws (all from the upstream target, preserved): encoding `item_count`
//! deduped, sorted `InternalValue`s at a fuzzer-chosen restart interval and
//! hash ratio, then
//! 1. `point_read` at `seqno + 1` recovers each stored item exactly, and at
//!    `SeqNo::MAX` recovers whichever stored version is newest at or below
//!    that ceiling;
//! 2. forward and reverse full iteration round-trip the sorted item list;
//! 3. `next()`/`next_back()` interleaved in a seeded "ping-pong" pattern
//!    (both from the front and from the back) matches the same interleave
//!    over the plain `Vec`;
//! 4. a seeked, `seek_upper`-bounded range round-trips the corresponding
//!    slice of the sorted item list (expanding the sampled bounds outward to
//!    the full same-key run first, since a mid-run bound is not what the
//!    block's own key-only seek predicate resolves to).

use arbitrary::{Arbitrary, Result, Unstructured};
use libfuzzer_sys::fuzz_target;
use lsm_tree::table::block::decoder::ParsedItem;
use lsm_tree::table::{Block, DataBlock};
use lsm_tree::{InternalValue, SeqNo, ValueType};

#[derive(Arbitrary, Clone, Debug, PartialEq, Eq)]
enum FuzzyValueType {
    Value,
    Tombstone,
}

impl From<FuzzyValueType> for ValueType {
    fn from(v: FuzzyValueType) -> Self {
        match v {
            FuzzyValueType::Value => ValueType::Value,
            FuzzyValueType::Tombstone => ValueType::Tombstone,
        }
    }
}

/// Wraps `InternalValue` so the generator can order/dedup it — see the
/// module doc for why this is a hand-written `Ord`/`PartialEq` over `.key`
/// rather than a derive.
#[derive(Clone, Debug)]
struct FuzzyValue(InternalValue);

impl PartialEq for FuzzyValue {
    fn eq(&self, other: &Self) -> bool {
        self.0.key == other.0.key
    }
}
impl Eq for FuzzyValue {}
impl PartialOrd for FuzzyValue {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for FuzzyValue {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.key.cmp(&other.0.key)
    }
}

impl<'a> Arbitrary<'a> for FuzzyValue {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        let key = Vec::<u8>::arbitrary(u)?;
        let value = Vec::<u8>::arbitrary(u)?;
        let seqno = u64::arbitrary(u)?;
        let vtype = FuzzyValueType::arbitrary(u)?;

        let key = if key.is_empty() { vec![0] } else { key };
        // A tombstone's value is never written on encode (`encode_full_into`/
        // `encode_truncated_into`, `data_block/mod.rs`: `if !self.is_tombstone()
        // { write value }` — a tombstone needs no payload). Zeroing it here
        // keeps the round-trip law honest about what the format actually
        // stores, rather than asserting a value the encoder itself discards.
        let value = if matches!(vtype, FuzzyValueType::Tombstone) {
            vec![]
        } else {
            value
        };

        Ok(Self(InternalValue::from_components(
            key,
            value,
            seqno,
            vtype.into(),
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

/// Comparable projection used everywhere an `InternalValue` would otherwise
/// need `PartialEq` — it only has one inside `lsm-tree`'s own
/// `#[cfg(test)]` build (and even there it compares by `.key` alone), so
/// `assert_eq!` on the type itself doesn't compile from this crate.
fn ival_tup(v: &InternalValue) -> (Vec<u8>, SeqNo, ValueType, Vec<u8>) {
    (
        v.key.user_key.to_vec(),
        v.key.seqno,
        v.key.value_type,
        v.value.to_vec(),
    )
}

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);

    let Ok(seed) = u64::arbitrary(&mut u) else {
        return;
    };
    let Ok(restart_interval) = u8::arbitrary(&mut u) else {
        return;
    };
    let restart_interval = restart_interval.max(1);

    let item_count = {
        use rand::SeedableRng;
        use rand::prelude::*;
        use rand_chacha::ChaCha8Rng;

        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        rng.random_range(1..100)
    };

    let hash_ratio: f32 = {
        use rand::SeedableRng;
        use rand::prelude::*;
        use rand_chacha::ChaCha8Rng;

        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        rng.random_range(0.0..8.0)
    };

    let mut items = Vec::with_capacity(item_count);
    for _ in 0..item_count {
        let Ok(v) = FuzzyValue::arbitrary(&mut u) else {
            return;
        };
        items.push(v);
    }

    items.sort();
    items.dedup();
    if items.is_empty() {
        return;
    }
    let items = items.into_iter().map(|v| v.0).collect::<Vec<_>>();

    let bytes =
        DataBlock::encode_into_vec(&items, restart_interval, hash_ratio).unwrap_or_else(|e| {
            panic!("encode must not fail on well-formed sorted, deduped items: {e:?}")
        });

    let data_block = DataBlock::new(Block {
        data: bytes.into(),
        header: lsm_tree::table::block::Header {
            block_type: lsm_tree::table::block::BlockType::Data,
            checksum: lsm_tree::Checksum::from_raw(0),
            data_length: 0,
            uncompressed_length: 0,
        },
    });

    assert_eq!(data_block.len(), items.len());

    if data_block.binary_index_len() > 254 {
        assert!(data_block.hash_bucket_count().is_none());
    } else if hash_ratio > 0.0 {
        assert!(data_block.hash_bucket_count().unwrap() > 0);
    }

    // Law 1: point_read at exactly one past a stored version's own seqno
    // recovers that version; at `SeqNo::MAX` it recovers whichever stored
    // version is newest overall for that user key.
    for needle in &items {
        if needle.key.seqno == SeqNo::MAX {
            continue;
        }
        assert_eq!(
            Some(ival_tup(needle)),
            data_block
                .point_read(&needle.key.user_key, needle.key.seqno + 1)
                .as_ref()
                .map(ival_tup),
        );
        assert_eq!(
            ival_tup(
                &data_block
                    .point_read(&needle.key.user_key, u64::MAX)
                    .unwrap()
            ),
            ival_tup(
                items
                    .iter()
                    .find(|item| item.key.user_key == needle.key.user_key
                        && item.key.seqno < u64::MAX)
                    .unwrap()
            ),
        );
    }

    // Law 2: forward and reverse full round-trip.
    assert_eq!(
        items.iter().map(ival_tup).collect::<Vec<_>>(),
        data_block
            .iter()
            .map(|x| ival_tup(&x.materialize(data_block.as_slice())))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        items.iter().rev().map(ival_tup).collect::<Vec<_>>(),
        data_block
            .iter()
            .map(|x| x.materialize(data_block.as_slice()))
            .rev()
            .map(|x| ival_tup(&x))
            .collect::<Vec<_>>(),
    );

    // Law 3: seeded ping-pong next()/next_back() interleave, both
    // directions.
    for reversed in [false, true] {
        let ping_pongs = generate_ping_pong_code(seed, items.len());

        let expected = {
            let mut it: Box<dyn DoubleEndedIterator<Item = &InternalValue>> = if reversed {
                Box::new(items.iter().rev())
            } else {
                Box::new(items.iter())
            };
            let mut v = vec![];
            for &x in &ping_pongs {
                if x == 0 {
                    v.push(it.next().cloned().unwrap());
                } else {
                    v.push(it.next_back().cloned().unwrap());
                }
            }
            v
        };

        let real = {
            let base = data_block
                .iter()
                .map(|x| x.materialize(data_block.as_slice()));
            let mut it: Box<dyn DoubleEndedIterator<Item = InternalValue>> = if reversed {
                Box::new(base.rev())
            } else {
                Box::new(base)
            };
            let mut v = vec![];
            for &x in &ping_pongs {
                if x == 0 {
                    v.push(it.next().unwrap());
                } else {
                    v.push(it.next_back().unwrap());
                }
            }
            v
        };

        assert_eq!(
            expected.iter().map(ival_tup).collect::<Vec<_>>(),
            real.iter().map(ival_tup).collect::<Vec<_>>(),
        );
    }

    // Law 4: a seek/seek_upper-bounded range round-trips the corresponding
    // slice — expanding the sampled endpoints to their full same-key run
    // first (a mid-run bound is not a stable seek target: see the seek's
    // own comment on `SkipCursor` in `storage/skip_walk.rs`, the same
    // "land on the whole run, not a fraction of it" shape this block-level
    // seek has).
    {
        use rand::SeedableRng;
        use rand::prelude::*;
        use rand_chacha::ChaCha8Rng;

        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let mut lo = rng.random_range(0..items.len());
        let mut hi = rng.random_range(0..items.len());
        if lo > hi {
            std::mem::swap(&mut lo, &mut hi);
        }

        while lo > 0 && items[lo - 1].key.user_key == items[lo].key.user_key {
            lo -= 1;
        }
        while hi < items.len() - 1 && items[hi + 1].key.user_key == items[hi].key.user_key {
            hi += 1;
        }

        let lo_key = &items[lo].key.user_key;
        let hi_key = &items[hi].key.user_key;
        let expected_range: Vec<_> = items[lo..=hi].to_vec();

        let mut iter = data_block.iter();
        assert!(iter.seek(lo_key), "should seek");
        assert!(iter.seek_upper(hi_key), "should seek");

        assert_eq!(
            expected_range.iter().map(ival_tup).collect::<Vec<_>>(),
            iter.map(|x| ival_tup(&x.materialize(data_block.as_slice())))
                .collect::<Vec<_>>(),
        );
    }
});
