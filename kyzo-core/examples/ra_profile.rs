/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Allocation + wall profile of the iterator RA path vs the batched path.
//!
//! `perf`/`valgrind` are not available in the proving environment, so the
//! "where does the iterator time go" question is answered by a process-wide
//! **counting allocator** (isolates the allocation-churn tax the design names)
//! plus wall time per mode (isolates the residual dispatch/scratch tax). Each
//! workload runs once per mode after a warm-up; the timed/counted region is the
//! evaluation only (compile + seed happen at build time).
//!
//! Run: `cargo run -p kyzo --release --features bench-internals --example ra_profile`

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use kyzo::bench_api::{
    Backend, Exec, Graph, Workload, aggregation, scan_filter, three_way_join, transitive_closure,
};

// ── counting global allocator ────────────────────────────────────────────
static ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const SEED: u64 = 0x5EED_1234;

struct Measured {
    rows: usize,
    allocs: u64,
    bytes: u64,
    nanos: u128,
}

/// Run `w` in `exec` mode once, counting allocations and wall time over just
/// the evaluation. A warm-up run (untimed) first pays any one-time lazy costs.
fn measure(w: &Workload, exec: Exec) -> Measured {
    let _ = w.run(exec); // warm up

    let calls0 = ALLOC_CALLS.load(Ordering::Relaxed);
    let bytes0 = ALLOC_BYTES.load(Ordering::Relaxed);
    let t0 = Instant::now();
    let rows = w.run(exec);
    let nanos = t0.elapsed().as_nanos();
    let allocs = ALLOC_CALLS.load(Ordering::Relaxed) - calls0;
    let bytes = ALLOC_BYTES.load(Ordering::Relaxed) - bytes0;

    Measured {
        rows,
        allocs,
        bytes,
        nanos,
    }
}

fn row(name: &str, it: &Measured, ba: &Measured) {
    let per = |m: &Measured| {
        if m.rows == 0 {
            0.0
        } else {
            m.allocs as f64 / m.rows as f64
        }
    };
    let spd = if ba.nanos == 0 {
        0.0
    } else {
        it.nanos as f64 / ba.nanos as f64
    };
    println!(
        "{name:<26} {rows:>8} | it {ia:>10} a {ib:>11} B {ins:>10}ns {ipr:>6.2}a/row \
         | ba {ba_a:>10} a {ba_b:>11} B {bns:>10}ns {bpr:>6.2}a/row | x{spd:>4.2}",
        name = name,
        rows = it.rows,
        ia = it.allocs,
        ib = it.bytes,
        ins = it.nanos,
        ipr = per(it),
        ba_a = ba.allocs,
        ba_b = ba.bytes,
        bns = ba.nanos,
        bpr = per(ba),
        spd = spd,
    );
}

fn profile(name: &str, w: &Workload) {
    let it = measure(w, Exec::Iterator);
    let ba = measure(w, Exec::Batched);
    assert_eq!(
        it.rows, ba.rows,
        "iterator/batched row count disagree on {name}"
    );
    row(name, &it, &ba);
}

fn main() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut n = 0usize;
    let mut dir = || {
        let p = tmp.path().join(format!("w{n}"));
        n += 1;
        std::fs::create_dir_all(&p).expect("mkdir");
        p
    };

    println!(
        "workload                       rows |  iterator (allocs/bytes/ns/per-row) \
              | batched (allocs/bytes/ns/per-row) | speedup"
    );
    println!("{}", "-".repeat(150));

    // scan_filter is the camp's home turf — its iter/batch delta is the
    // clean measurement of the per-row dispatch+alloc tax on a pure pipeline.
    for backend in [Backend::Mem, Backend::Fjall] {
        let bt = if matches!(backend, Backend::Mem) {
            "mem"
        } else {
            "fjall"
        };
        profile(
            &format!("scan_filter/200k/sel50 {bt}"),
            &scan_filter(backend, 200_000, 50, SEED, &dir()),
        );
        profile(
            &format!("scan_filter/200k/sel5 {bt}"),
            &scan_filter(backend, 200_000, 5, SEED, &dir()),
        );
        profile(
            &format!("tc/chain/240 {bt}"),
            &transitive_closure(backend, Graph::Chain, 240, SEED, &dir()),
        );
        profile(
            &format!("tc/dense/400 {bt}"),
            &transitive_closure(backend, Graph::Dense, 400, SEED, &dir()),
        );
        profile(
            &format!("tc/random/400 {bt}"),
            &transitive_closure(backend, Graph::Random, 400, SEED, &dir()),
        );
        profile(
            &format!("join3/20k/fan4 {bt}"),
            &three_way_join(backend, 20_000, 4, SEED, &dir()),
        );
        profile(
            &format!("aggregation/200k/g1k {bt}"),
            &aggregation(backend, 200_000, 1_000, SEED, &dir()),
        );
    }
}
