/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Story #119's falsifiable claim, measured rather than cited: `GermanStr`
//! (the hand-built 16-byte inline/prefixed string that replaced
//! `DataValue::Str`'s old `SmartString` payload) must be >=3x faster at
//! string equality/compare than what it replaced. `db_scan.rs`'s existing
//! benches are integer-keyed and prove nothing about this claim; this file
//! is the missing string-keyed instrument. This number goes public — a
//! rigged or unrepresentative bench is worse than none, so:
//!
//! - Three input distributions, not just `GermanStr`'s best case (see
//!   `DISTRIBUTIONS` below) — reported honestly even where the win is
//!   small or absent.
//! - Inputs and results are `black_box`ed; every timed operation's cost
//!   scales with `N` across multiple sizes, so a flat line would itself be
//!   evidence the bench measures nothing.
//! - The applied filter predicate targets a non-key column so it forces a
//!   real per-row string compare across a full scan, not a point seek on
//!   the indexed key.
//!
//! Deliberately branch-agnostic: every construction here goes through
//! `DataValue::Str(<string>.into())` and public `Db` scripts — nothing
//! `GermanStr`-specific appears in this file's body. That is what makes it
//! usable as its own A/B baseline: the identical file compiles against
//! `origin/main` (where `DataValue::Str` holds `SmartString<LazyCompact>`)
//! and against this branch's HEAD (where it holds `GermanStr`), so
//! `criterion --save-baseline main` on the former and `--baseline main` on
//! the latter measures the real delta, not two different benchmarks.
//!
//! Two groups:
//! - `germanstr_eq` — the PRIMITIVE: raw `DataValue::Str` equality/compare
//!   in memory, no engine involved, across the three distributions and two
//!   sizes.
//! - `string_scan` — the APPLIED proof: a string-keyed relation through a
//!   real seeded `Db`, mirroring `db_scan.rs`'s structure (one relation,
//!   `sample_size(20)`, one warm read before timing), across a full scan, a
//!   non-key string-equality filter, and a self-join on the string key.
//!
//! Run: `cargo bench -p kyzo --bench string_eq`.

use std::collections::BTreeMap;
use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use kyzo::{DataValue, Db, new_fjall_storage};
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;

const SEED: u64 = 0x5EED_1234;

// ── input distributions (germanstr_eq) ──────────────────────────────────
//
// `GermanStr::cmp_impl` (data/germanstr.rs) compares only the cached first
// `min(4, len_a, len_b)` bytes ("head4") without ever touching the heap;
// if that already decides the order, the heap payload is never read. If
// it doesn't, BOTH representations fall through to a real byte compare of
// the tail. So the three distributions below are NOT "best case, medium,
// near-neutral" by construction — which one wins, and by how much, is
// exactly what running this bench answers, not what this comment asserts.

/// `long_shared_prefix`: a ~15-byte constant, tenant-scoped prefix plus a
/// zero-padded counter (~27 bytes total — past both representations'
/// inline capacity, so every value here is heap-resident either way).
/// Every value shares the same first 4 bytes, so `head4` never resolves
/// anything on its own: every comparison falls through to a real tail
/// compare, for both `GermanStr` and the old `SmartString`.
fn long_shared_prefix_key(i: u64) -> String {
    format!("user:tenant-42:{i:012}")
}

/// `short_strings`: 8 ASCII bytes, comfortably inside BOTH representations'
/// inline capacity (`GermanStr`: 12 bytes; `SmartString<LazyCompact>`: ~23
/// bytes) — neither ever allocates. Isolates whatever inline-compare cost
/// difference the two layouts have on their own, with no heap-avoidance
/// argument available to either side.
fn short_string_key(i: u64) -> String {
    format!("{i:08}")
}

/// `early_differ`: a long (heap-forcing, like `long_shared_prefix`) string
/// whose first 5 bytes carry the varying index, followed by a long
/// constant tail — so for the large majority of pairs, the differing byte
/// falls inside `head4`'s 4-byte window. If `GermanStr`'s design claim
/// holds, this is where it should show the largest gap: the head4 check
/// alone resolves the comparison without any heap dereference, where the
/// old boxed representation must dereference at least once just to read
/// its first byte.
const EARLY_DIFFER_TAIL: &str =
    "-constant-tail-padding-forcing-heap-allocation-on-both-sides-of-the-comparison";
fn early_differ_key(i: u64) -> String {
    format!("{i:05}{EARLY_DIFFER_TAIL}")
}

struct Distribution {
    label: &'static str,
    make: fn(u64) -> String,
}

const DISTRIBUTIONS: &[Distribution] = &[
    Distribution {
        label: "long_shared_prefix",
        make: long_shared_prefix_key,
    },
    Distribution {
        label: "short_strings",
        make: short_string_key,
    },
    Distribution {
        label: "early_differ",
        make: early_differ_key,
    },
];

/// Two sizes per distribution/operation so the report itself shows (or
/// fails to show) cost scaling with `N` — a flat line across these two
/// would be evidence the bench isn't measuring the compare path at all.
const EQ_SIZES: [u64; 2] = [10_000, 100_000];

fn no_params() -> BTreeMap<String, DataValue> {
    BTreeMap::new()
}

// ── germanstr_eq: the primitive, in memory, no engine ───────────────────

fn make_keys(n: u64, make: fn(u64) -> String) -> Vec<DataValue> {
    (0..n).map(|i| DataValue::Str(make(i).into())).collect()
}

fn bench_germanstr_eq(c: &mut Criterion) {
    let mut g = c.benchmark_group("germanstr_eq");
    g.sample_size(20);

    for dist in DISTRIBUTIONS {
        for &n in &EQ_SIZES {
            let values = make_keys(n, dist.make);
            let mut rng = StdRng::seed_from_u64(SEED);
            let mut shuffled = values.clone();
            shuffled.shuffle(&mut rng);

            // (a) Equality sweep: position-by-position `==` against a
            // seeded shuffle of the same multiset — the sharpest exercise
            // of the compare path alone (no allocation, no ordering, just
            // `PartialEq`). Inputs are `black_box`ed on every iteration so
            // nothing about them is knowable at compile time.
            g.bench_with_input(
                BenchmarkId::new(format!("{}/equality_sweep", dist.label), n),
                &n,
                |b, _| {
                    b.iter(|| {
                        let values = black_box(&values);
                        let shuffled = black_box(&shuffled);
                        let matches = values
                            .iter()
                            .zip(shuffled.iter())
                            .filter(|(a, bb)| a == bb)
                            .count();
                        black_box(matches)
                    });
                },
            );

            // (b) Sort: drives `Ord`, not just `PartialEq`. `iter_batched`
            // re-clones the shuffled vector every sample so each iteration
            // sorts a fresh unsorted copy rather than a no-op re-sort of
            // the previous iteration's now-sorted output.
            g.bench_with_input(
                BenchmarkId::new(format!("{}/sort", dist.label), n),
                &n,
                |b, _| {
                    b.iter_batched(
                        || black_box(shuffled.clone()),
                        |mut v| {
                            v.sort();
                            black_box(v)
                        },
                        BatchSize::LargeInput,
                    );
                },
            );
        }
    }

    g.finish();
}

// ── string_scan: the applied proof, through the public engine ──────────

/// The non-key payload column: also a string (not the original int), so
/// the `eq_filter` bench below can filter on it as a genuine per-row
/// string compare across a full scan — filtering on the indexed key `k`
/// instead would risk the query planner turning it into a point seek that
/// never compares most rows at all, proving nothing about the claim.
fn value_str(i: u64) -> String {
    format!("payload:{i:012}")
}

fn seeded_string_db(n: u64, dir: &std::path::Path) -> Db<kyzo::FjallStorage> {
    let db = Db::new(new_fjall_storage(dir).expect("storage")).expect("db");
    let mut script = String::from("?[k, v] <- [");
    for i in 0..n {
        script.push_str(&format!(
            "[\"{}\", \"{}\"],",
            long_shared_prefix_key(i),
            value_str(i)
        ));
    }
    script.push_str("] :create s {k => v}");
    db.run_script(&script, no_params()).expect("seed");
    db
}

fn bench_string_scan(c: &mut Criterion) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut g = c.benchmark_group("string_scan");
    g.sample_size(20);

    for n in [50_000u64, 200_000] {
        let db = seeded_string_db(n, &tmp.path().join(format!("s{n}")));
        // `v` (not `k`) is the filter target: `v` carries no index, so
        // this predicate can only be answered by a full scan comparing
        // every row's string value — never a point lookup. Picks roughly
        // the upper half of the table via string `>` ordering.
        let threshold = value_str(n / 2);

        // Warm: the first read builds the segment (same posture as
        // `db_scan.rs`'s `seeded_db`).
        db.run_script("?[k, v] := *s[k, v]", no_params()).unwrap();

        g.bench_function(format!("full/{n}"), |b| {
            b.iter(|| black_box(db.run_script("?[k, v] := *s[k, v]", no_params()).unwrap()));
        });

        g.bench_function(format!("eq_filter/{n}"), |b| {
            b.iter(|| {
                black_box(
                    db.run_script(
                        &format!("?[k, v] := *s[k, v], v > \"{threshold}\""),
                        no_params(),
                    )
                    .unwrap(),
                )
            });
        });

        // Self-join on the string key: `k` is `s`'s primary key, so this is
        // cardinality-preserving (one row per `k`, `v1 == v2` always) — but
        // the join operator still performs a full string-key
        // compare/probe per row on the way there, which is exactly the
        // path this claim is about; cardinality of the OUTPUT isn't.
        g.bench_function(format!("self_join/{n}"), |b| {
            b.iter(|| {
                black_box(
                    db.run_script("?[k, v1, v2] := *s[k, v1], *s[k, v2]", no_params())
                        .unwrap(),
                )
            });
        });
    }
    g.finish();
}

criterion_group!(benches, bench_germanstr_eq, bench_string_scan);
criterion_main!(benches);
