/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Relational-algebra execution benchmarks — the permanent instrumentation
//! the vectorization ascent measures against.
//!
//! Four workload classes, each the home turf of a distinct cost the design
//! must answer:
//!
//! - `tc/*` — transitive closure over chain/dense/random graphs: the
//!   semi-naive recursion, delta scans of the rule store, join-per-row.
//! - `join3/*` — a selective 3-way join: materialized/prefix join strategy
//!   under low match multiplicity.
//! - `scan_filter/*` — a wide scan with a selective predicate: the batched
//!   scan→filter→project pipeline's home turf.
//! - `aggregation/*` — count grouped over many groups: the meet/normal
//!   aggregation fold.
//!
//! Each runs on both backends (`mem` = in-memory MVCC double, `fjall` =
//! on-disk LSM). A single execution path runs today; the benchmark id keeps
//! the historical `batch` tag so its row stays comparable across the
//! iterator twin's deletion in the committed `bench/results/` logs
//! (see `bench/results/README.md`; `bench_workload`). Generators are seeded;
//! the machine is stamped in each `bench/results/` file, not here.
//!
//! Run: `cargo bench -p kyzo --features bench-internals --bench ra_exec`.

use std::fmt::Debug;
use std::hint::black_box;
use std::path::PathBuf;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use kyzo::bench_api::{
    Backend, Graph, Workload, aggregation, scan_filter, three_way_join, transitive_closure,
};

const SEED: u64 = 0x5EED_1234;

fn open_door<T, E: Debug>(r: Result<T, E>, door: &'static str) -> T {
    match r {
        Ok(v) => v,
        Err(e) => std::panic::resume_unwind(Box::new(format!("{door}: {e:?}"))),
    }
}

/// A fresh unique directory for one on-disk workload's store. Criterion
/// keeps workloads alive for the whole run, so these persist until process
/// exit; we root them under one temp dir cleaned up on drop.
struct DirFactory {
    root: tempfile::TempDir,
    n: usize,
}
impl DirFactory {
    fn new() -> Self {
        DirFactory {
            root: open_door(tempfile::tempdir(), "tempdir"),
            n: 0,
        }
    }
    fn next(&mut self) -> PathBuf {
        let p = self.root.path().join(format!("w{}", self.n));
        self.n += 1;
        open_door(std::fs::create_dir_all(&p), "mkdir");
        p
    }
}

fn bench_workload(c: &mut Criterion, group_name: &str, w: &Workload) {
    let mut g = c.benchmark_group(group_name);
    // The id keeps the historical "batch" tag so its `bench/results/` row
    // stays comparable across the twin's deletion.
    g.bench_with_input(BenchmarkId::new(w.label(), "batch"), &(), |b, _| {
        b.iter(|| black_box(w.run()));
    });
    g.finish();
}

fn all(c: &mut Criterion) {
    let mut dirs = DirFactory::new();

    // ── transitive closure ───────────────────────────────────────────────
    // Chain: TC is O(n^2) pairs; keep n modest. Dense/Random larger.
    for backend in [Backend::Mem, Backend::Fjall] {
        let bt = match backend {
            Backend::Mem => "mem",
            Backend::Fjall => "fjall",
        };
        for (shape, n) in [
            (Graph::Chain, 120usize),
            (Graph::Chain, 240),
            (Graph::Dense, 400),
            (Graph::Random, 400),
        ] {
            let w = transitive_closure(backend, shape, n, SEED, &dirs.next());
            bench_workload(c, &format!("ra/{bt}"), &w);
        }

        // ── selective 3-way join ──────────────────────────────────────────
        // Kept modest: high key-collision (low fan) blows the output up
        // quadratically, and join batching is a later camp anyway, so both
        // modes run the iterator join here — we measure parity, not speedup.
        for (n, fan) in [(5_000usize, 4usize), (20_000, 4)] {
            let w = three_way_join(backend, n, fan, SEED, &dirs.next());
            bench_workload(c, &format!("ra/{bt}"), &w);
        }

        // ── wide scan + filter ────────────────────────────────────────────
        for (n, sel) in [(50_000usize, 50i64), (200_000, 50), (200_000, 5)] {
            let w = scan_filter(backend, n, sel, SEED, &dirs.next());
            bench_workload(c, &format!("ra/{bt}"), &w);
        }

        // ── aggregation-heavy ─────────────────────────────────────────────
        for (n, groups) in [(50_000usize, 1_000usize), (200_000, 1_000)] {
            let w = aggregation(backend, n, groups, SEED, &dirs.next());
            bench_workload(c, &format!("ra/{bt}"), &w);
        }
    }
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(20);
    targets = all
}
criterion_main!(benches);
