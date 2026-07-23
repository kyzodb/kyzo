/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Repeated read-path scans through the public [`Engine`] session — the
//! current-state segment engine's kill-gate instrument. Steady-state
//! iterations here run entirely against the served segment; the same bench
//! on a segments-reverted tree is the A/B baseline.

use std::collections::BTreeMap;
use std::fmt::Debug;

use criterion::{Criterion, criterion_group, criterion_main};
use kyzo::{Catalog, DataValue, Engine, new_fjall_storage};
use std::hint::black_box;

fn open_door<T, E: Debug>(r: Result<T, E>, door: &'static str) -> T {
    match r {
        Ok(v) => v,
        Err(e) => std::panic::resume_unwind(Box::new(format!("{door}: {e:?}"))),
    }
}

fn no_params() -> BTreeMap<String, DataValue> {
    BTreeMap::new()
}

fn seeded_db(n: i64, dir: &std::path::Path) -> Engine<kyzo::FjallStorage> {
    let storage = open_door(new_fjall_storage(dir), "storage");
    let db = open_door(Engine::compose(storage, Catalog::new()), "engine");
    let mut script = String::from("?[k, v] <- [");
    for i in 0..n {
        script.push_str(&format!("[{i}, {}],", i * 3));
    }
    script.push_str("] :create w {k => v}");
    open_door(db.run_script(&script, no_params()), "seed");
    db
}

fn bench_scans(c: &mut Criterion) {
    let tmp = open_door(tempfile::tempdir(), "tempdir");
    let mut g = c.benchmark_group("db_scan");
    g.sample_size(20);

    for n in [50_000i64, 200_000] {
        // One seeded store per size drives both queries: the relation is
        // identical and the scans are read-only, so full/ and filtered/
        // measure the same served segment — no reason to pay the seed twice.
        let db = seeded_db(n, &tmp.path().join(format!("w{n}")));
        // Warm: the first read builds the segment.
        open_door(db.run_script("?[k, v] := *w[k, v]", no_params()), "warm");
        g.bench_function(format!("full/{n}"), |b| {
            b.iter(|| {
                black_box(open_door(
                    db.run_script("?[k, v] := *w[k, v]", no_params()),
                    "full scan",
                ))
            });
        });

        g.bench_function(format!("filtered/{n}"), |b| {
            b.iter(|| {
                black_box(open_door(
                    db.run_script("?[k, v] := *w[k, v], v > 500", no_params()),
                    "filtered scan",
                ))
            });
        });
    }
    g.finish();
}

criterion_group!(benches, bench_scans);
criterion_main!(benches);
