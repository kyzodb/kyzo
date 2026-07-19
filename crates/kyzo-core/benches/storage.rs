/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Storage-kernel benchmarks, designed around the decisions they inform:
//!
//! - `commit_parallel/*` — the commit-ceiling question: fjall applies commits
//!   serially under a global lock. Whether throughput scales with writer
//!   threads here informs how transaction commit must evolve under
//!   write-heavy load.
//! - `scan_tracking_overhead/*` — the SSI question: what a range scan costs
//!   in a write transaction (conflict-tracked) vs a read transaction
//!   (untracked snapshot).
//! - `asof/*` — proves the seek-based time-travel scan earns its complexity
//!   over the naive scan-everything-and-filter oracle.
//! - `ops/*` — the baseline numbers every other claim rests on.
//!
//! Run: `cargo bench -p kyzo` (results under `target/criterion/`).

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use kyzo::{
    DataValue, StorageKey, ReadTx, RelationId, Storage, TupleT, ValiditySlot, ValidityTs, WriteTx,
    new_fjall_storage,
};
fn key(i: u64) -> StorageKey {
    [DataValue::from(i as i64)].encode_as_key(RelationId::new(7).expect("below cap"))
}

fn bitemp_key(name: i64, valid_ts: i64, sys_ts: i64) -> StorageKey {
    let slot = |t: i64| DataValue::Validity(ValiditySlot::from_stored(ValidityTs::from_raw(t), true));
    [DataValue::from(name), slot(valid_ts), slot(sys_ts)]
        .encode_as_key(RelationId::new(9).expect("below cap"))
}

/// Relation-9 header + assert polarity byte: the value every versioned
/// bench row carries.
fn assert_val() -> Vec<u8> {
    let mut v = 9u64.to_be_bytes().to_vec();
    v.push(0); // ClaimPolarity::Assert
    v
}

fn ops(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let mut tx = db.write_tx().unwrap();
    for i in 0..10_000u64 {
        tx.put(&key(i), b"value").unwrap();
    }
    tx.commit().unwrap();

    let mut g = c.benchmark_group("ops");
    g.bench_function("get_hit", |b| {
        let tx = db.read_tx().unwrap();
        let k = key(5_000);
        b.iter(|| black_box(tx.get(black_box(&k)).unwrap()))
    });
    g.bench_function("get_miss", |b| {
        let tx = db.read_tx().unwrap();
        let k = key(999_999);
        b.iter(|| black_box(tx.get(black_box(&k)).unwrap()))
    });
    g.bench_function("put_1k_commit", |b| {
        b.iter(|| {
            let mut tx = db.write_tx().unwrap();
            for i in 0..1_000u64 {
                tx.put(&key(100_000 + i), b"value").unwrap();
            }
            tx.commit().unwrap();
        })
    });
    g.finish();
}

fn scan_tracking_overhead(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    let mut tx = db.write_tx().unwrap();
    for i in 0..10_000u64 {
        tx.put(&key(i), b"value").unwrap();
    }
    tx.commit().unwrap();
    let (lo, hi) = (key(0), key(9_999));

    let mut g = c.benchmark_group("scan_tracking_overhead");
    g.bench_function("read_tx_scan_10k", |b| {
        let tx = db.read_tx().unwrap();
        b.iter(|| {
            black_box(tx.range_scan(&lo, &hi).fold(0usize, |n, r| {
                r.unwrap();
                n + 1
            }))
        })
    });
    g.bench_function("write_tx_scan_10k", |b| {
        // Fresh write tx per iteration: read marks accumulate per tx, and an
        // honest number includes that cost.
        b.iter(|| {
            let tx = db.write_tx().unwrap();
            black_box(tx.range_scan(&lo, &hi).fold(0usize, |n, r| {
                r.unwrap();
                n + 1
            }))
        })
    });
    g.finish();
}

fn commit_parallel(c: &mut Criterion) {
    let mut g = c.benchmark_group("commit_parallel");
    g.sample_size(10);
    for threads in [1usize, 2, 4, 8] {
        g.bench_with_input(
            BenchmarkId::from_parameter(threads),
            &threads,
            |b, &threads| {
                b.iter_with_setup(
                    || {
                        let dir = tempfile::tempdir().unwrap();
                        let db = new_fjall_storage(dir.path()).unwrap();
                        (dir, db)
                    },
                    |(_dir, db)| {
                        // Fixed total work (256 disjoint-key commits) split across
                        // N threads: if commits applied in parallel, wall time
                        // would fall with N; the ceiling shows as a flat line.
                        const TOTAL: usize = 256;
                        let per = TOTAL / threads;
                        std::thread::scope(|s| {
                            for t in 0..threads {
                                let db = db.clone();
                                s.spawn(move || {
                                    for i in 0..per {
                                        let mut tx = db.write_tx().unwrap();
                                        tx.put(&key((t * per + i) as u64), b"v").unwrap();
                                        tx.commit().unwrap();
                                    }
                                });
                            }
                        });
                    },
                )
            },
        );
    }
    g.finish();
}

fn asof(c: &mut Criterion) {
    // Two shapes, same total entries (8k): shallow history (many tuples, few
    // versions) where naive streaming can win on iterator-setup costs, and
    // deep history (few tuples, many versions) where seeking must win — the
    // crossover is the honest characterization of the seek design.
    for (label, tuples, versions) in [("shallow_1000x8", 1_000i64, 8i64), ("deep_50x160", 50, 160)]
    {
        let dir = tempfile::tempdir().unwrap();
        let db = new_fjall_storage(dir.path()).unwrap();
        let mut tx = db.write_tx().unwrap();
        for name in 0..tuples {
            for ts in 1..=versions {
                tx.put(&bitemp_key(name, ts, 1), &assert_val()).unwrap();
            }
        }
        tx.commit().unwrap();
        let lo = &[].encode_as_key(RelationId::new(9).expect("below cap"));
        let hi = &[].encode_as_key(RelationId::new(10).expect("below cap"));
        let at = kyzo::AsOf::current(ValidityTs::from_raw(versions / 2));

        let mut g = c.benchmark_group(format!("asof_{label}"));
        g.bench_function("seek_skip_scan", |b| {
            let tx = db.read_tx().unwrap();
            b.iter(|| {
                black_box(tx.range_skip_scan_tuple(lo, hi, at).fold(0usize, |n, r| {
                    r.unwrap();
                    n + 1
                }))
            })
        });
        g.bench_function("naive_scan_filter", |b| {
            // The obviously-correct oracle: walk all versions, keep newest <= at.
            let tx = db.read_tx().unwrap();
            let cutoff = versions / 2;
            b.iter(|| {
                let mut newest: std::collections::BTreeMap<i64, (i64, bool)> = Default::default();
                for r in tx.range_scan(lo, hi) {
                    let (k, v) = r.unwrap();
                    let t = kyzo::decode_tuple_from_key(&k, 4).unwrap();
                    let (DataValue::Num(name_n), DataValue::Validity(vld)) = (&t[0], &t[1]) else {
                        unreachable!()
                    };
                    let name = name_n.as_int().unwrap();
                    // Assert polarity byte opens the stored value.
                    let assert = v[0] == 0;
                    let ts = vld.timestamp().raw();
                    if ts <= cutoff {
                        let e = newest.entry(name).or_insert((ts, assert));
                        if ts > e.0 {
                            *e = (ts, assert);
                        }
                    }
                }
                black_box(newest.values().filter(|(_, a)| *a).count())
            })
        });
        g.finish();
    }
}

criterion_group!(benches, ops, scan_tracking_overhead, commit_parallel, asof);
criterion_main!(benches);
