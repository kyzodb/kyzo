/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

#![no_main]

//! Fuzzes `lsm_tree::Table` end to end — write a real on-disk table
//! (`table::Writer`), `Table::recover` it, and read it back. This is one
//! level above `data_block`/`index_block`: it exercises the full table
//! shape (full vs. two-level index, full vs. partitioned filter, real
//! restart intervals and block sizes) as one on-disk unit rather than a
//! single block in isolation. Story #118 (own the LSM): ported from this
//! vendored crate's own upstream fuzz target (`vendor/lsm-tree`'s
//! `fuzz/table_read`, an AFL harness) onto our libfuzzer harness.
//!
//! Adapted for this exact vendored revision (3.1.5): `Table::recover`'s
//! `descriptor_table` parameter is `Option<Arc<DescriptorTable>>` here (the
//! upstream target passes a bare `Arc::new(..)`, which doesn't type-check
//! against this signature — wrapped in `Some(..)` below), and the unused
//! `SequenceNumberCounter` import is dropped.
//!
//! Laws (all from the upstream target): recovering a table written from
//! `item_count` deduped, sorted `InternalValue`s reports the right item
//! count; a point `get` at `seqno + 1` recovers each stored item exactly,
//! and at `SeqNo::MAX` recovers whichever stored version is newest overall;
//! full forward/reverse `iter()` round-trips the sorted item list; a seeded
//! "ping-pong" `next()`/`next_back()` interleave (both directions) matches
//! the same interleave over the plain `Vec`; a `range` bounded by two
//! same-key-run-expanded endpoints round-trips the corresponding slice.

use std::sync::Arc;

use arbitrary::{Arbitrary, Result, Unstructured};
use libfuzzer_sys::fuzz_target;
use lsm_tree::{InternalValue, SeqNo, ValueType};

#[derive(Arbitrary, Eq, PartialEq, Debug, Copy, Clone)]
enum IndexType {
    Full,
    Volatile,
    TwoLevel,
}

#[derive(Arbitrary, Eq, PartialEq, Debug, Copy, Clone)]
enum FilterType {
    Full,
    Volatile,
    Partitioned,
}

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

/// Wraps `InternalValue` so the generator can order/dedup it — `InternalValue`
/// itself only implements `Ord`/`Eq`/`PartialEq` inside `lsm-tree`'s own
/// `#[cfg(test)]` build (see `data_block.rs`'s module doc for the same
/// gap); ordering by `.key` (`InternalKey`, unconditionally `Ord`) is the
/// same fix used there.
#[derive(Clone, Debug)]
struct FuzzyValue(InternalValue);

impl PartialEq for FuzzyValue {
    fn eq(&self, other: &Self) -> bool {
        self.0.key == other.0.key
    }
}
impl Eq for FuzzyValue {}

impl Ord for FuzzyValue {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.key.cmp(&other.0.key)
    }
}

impl PartialOrd for FuzzyValue {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<'a> Arbitrary<'a> for FuzzyValue {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        let key = Vec::<u8>::arbitrary(u)?;
        let value = Vec::<u8>::arbitrary(u)?;

        // Seqnos never have a leading 1 (they are 63-bit numbers, not 64).
        let seqno = u64::arbitrary(u)? & 0x7FFF_FFFF_FFFF_FFFF;

        let vtype = FuzzyValueType::arbitrary(u)?;
        let key = if key.is_empty() { vec![0] } else { key };
        // A tombstone's value is never written on encode (see
        // `data_block.rs`'s copy of this same fix for the exact code path);
        // zeroing it here keeps the round-trip laws honest about what a
        // recovered table actually stores.
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
/// need `PartialEq` — see `data_block.rs`'s copy of this same helper for
/// why (`InternalValue`'s own `PartialEq` only exists inside `lsm-tree`'s
/// `#[cfg(test)]` build).
fn ival_tup(v: &InternalValue) -> (Vec<u8>, SeqNo, lsm_tree::ValueType, Vec<u8>) {
    (
        v.key.user_key.to_vec(),
        v.key.seqno,
        v.key.value_type,
        v.value.to_vec(),
    )
}

fuzz_target!(|data: &[u8]| {
    use rand::SeedableRng;
    use rand::prelude::*;

    let mut u = Unstructured::new(data);

    let Ok(seed) = u64::arbitrary(&mut u) else {
        return;
    };
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(seed);

    let Ok(restart_interval) = u8::arbitrary(&mut u) else {
        return;
    };
    let restart_interval = restart_interval.max(1);

    let Ok(index_type) = IndexType::arbitrary(&mut u) else {
        return;
    };
    let Ok(filter_type) = FilterType::arbitrary(&mut u) else {
        return;
    };

    let data_block_size = rng.random_range(1..64_000);
    let index_block_size = rng.random_range(1..64_000);
    let item_count = rng.random_range(1..200);
    let hash_ratio: f32 = rng.random_range(0.0..8.0);

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

    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("table_fuzz");

    {
        let mut writer = lsm_tree::table::Writer::new(file.clone(), 0, 0)
            .unwrap()
            .use_data_block_restart_interval(restart_interval)
            .use_data_block_size(data_block_size)
            .use_data_block_hash_ratio(hash_ratio);
        let _ = index_block_size; // sampled for parity with upstream; no direct setter exists yet.

        if index_type == IndexType::TwoLevel {
            writer = writer.use_partitioned_index();
        }
        if filter_type == FilterType::Partitioned {
            writer = writer.use_partitioned_filter();
        }

        for item in items.iter().cloned() {
            writer.write(item.0).unwrap();
        }

        writer.finish().unwrap();
    }

    let table = lsm_tree::Table::recover(
        file,
        lsm_tree::Checksum::from_raw(0),
        0,
        0,
        Arc::new(lsm_tree::Cache::with_capacity_bytes(0)),
        Some(Arc::new(lsm_tree::DescriptorTable::new(10))),
        filter_type == FilterType::Full,
        index_type == IndexType::Full,
    )
    .unwrap();

    assert_eq!(table.metadata.item_count as usize, items.len());

    let items = items.into_iter().map(|v| v.0).collect::<Vec<_>>();

    // Law: point get at exactly one past a stored version's own seqno
    // recovers that version; at `SeqNo::MAX` it recovers the newest overall.
    for needle in &items {
        if needle.key.seqno == SeqNo::MAX {
            continue;
        }
        let key_hash =
            lsm_tree::table::filter::standard_bloom::Builder::get_hash(&needle.key.user_key);

        assert_eq!(
            Some(ival_tup(needle)),
            table
                .get(&needle.key.user_key, needle.key.seqno + 1, key_hash)
                .unwrap()
                .as_ref()
                .map(ival_tup),
        );
        assert_eq!(
            ival_tup(
                &table
                    .get(&needle.key.user_key, u64::MAX, key_hash)
                    .unwrap()
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

    // Law: full forward round-trip via `scan`.
    assert_eq!(
        items.iter().map(ival_tup).collect::<Vec<_>>(),
        table
            .scan()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .iter()
            .map(ival_tup)
            .collect::<Vec<_>>(),
    );

    // Law: seeded ping-pong next()/next_back() interleave, both directions.
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
            let mut it: Box<dyn DoubleEndedIterator<Item = lsm_tree::Result<InternalValue>>> =
                if reversed {
                    Box::new(table.iter().rev())
                } else {
                    Box::new(table.iter())
                };
            let mut v = vec![];
            for &x in &ping_pongs {
                if x == 0 {
                    v.push(it.next().unwrap().unwrap());
                } else {
                    v.push(it.next_back().unwrap().unwrap());
                }
            }
            v
        };

        assert_eq!(
            expected.iter().map(ival_tup).collect::<Vec<_>>(),
            real.iter().map(ival_tup).collect::<Vec<_>>(),
        );
    }

    // Law: a positional range [lo, hi] (expanded outward to the full
    // same-key run at both ends) round-trips via `range`.
    {
        let mut lo = rng.random_range(0..items.len());
        let mut hi = rng.random_range(0..items.len());
        if lo > hi {
            std::mem::swap(&mut lo, &mut hi);
        }

        let lo_key = &items[lo].key.user_key;
        let lo = items
            .iter()
            .position(|it| &it.key.user_key == lo_key)
            .unwrap();

        let hi_key = &items[hi].key.user_key;
        let hi = items
            .iter()
            .rposition(|it| &it.key.user_key == hi_key)
            .unwrap();

        let expected_range: Vec<_> = items[lo..=hi].iter().map(ival_tup).collect();
        let iter = table.range(lo_key..=hi_key);

        assert_eq!(
            expected_range,
            iter.collect::<Result<Vec<_>, _>>()
                .unwrap()
                .iter()
                .map(ival_tup)
                .collect::<Vec<_>>(),
        );
    }
});
