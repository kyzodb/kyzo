/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Memory/allocation attribution for issue #68 (semi-naive fixpoint memory
//! blowup): peak live heap bytes and allocation churn, via the same
//! counting-allocator technique `ra_profile.rs` uses, but tracking the
//! running high-water mark of (allocated − freed) rather than the running
//! total — a peak-*memory* instrument, not merely a churn-*count* one.
//!
//! Two workload families, both from `bench_api`:
//! - `transitive_closure` (`tc[x,y] := edge[x,z], tc[z,y]`): `tc` occurs
//!   ONCE per recursive rule body — the well-behaved semi-naive case (a
//!   delta-driven prefix join against the stored `edge` relation) — the
//!   baseline for "how fat is a row" independent of any delta-narrowing
//!   defect. On `fjall` this scales linearly (~35 allocs/row, flat B/row);
//!   an earlier run against `Backend::Mem` (`SimStorage`, the in-memory
//!   test double) showed catastrophic superlinear scaling that turned out
//!   to be a SEPARATE bug in `SimStorage::range_scan` (fixed in
//!   `storage/sim.rs`), not a query-engine defect — `Backend::Fjall` is
//!   the production path and the one that matters here.
//! - `points_to` (mirrors `kyzo-bench`'s `pointsto.kz`): the `load`/`store`
//!   rules mention `pt` at TWO body positions each — the self-join shape
//!   that, before this issue's fix, collapsed
//!   `compile.rs::contained_rules()` to one name-keyed entry and disabled
//!   semi-naive delta narrowing for those rules entirely (every epoch
//!   re-derived from the FULL accumulated `pt` relation). Fixed by
//!   `AtomOccurrence`-keyed (positional) delta selection.
//!
//! Peak is measured only across `Workload::run()` (the baseline seed data
//! is excluded by resetting the peak tracker to the current live-byte level
//! immediately before the timed call), so it isolates the query engine's
//! own working-set growth from the fixed cost of the input facts.
//!
//! Run: `cargo run -p kyzo --release --features bench-internals --example fixpoint_mem_profile`
//! Under a cap (recommended, mirrors the bug's death signature):
//! `(ulimit -v 2097152 && cargo run -p kyzo --release --features bench-internals --example fixpoint_mem_profile)`
//! Set `KYZO_FULL_SCALE=1` to also run kyzo-bench's full `v3k-a2k-s6k`
//! points-to workload (slow; omitted by default to keep this example quick).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Instant;

use kyzo::bench_api::{Backend, Graph, points_to, transitive_closure};

// ── counting global allocator: live bytes + high-water mark ──────────────
static LIVE_BYTES: AtomicI64 = AtomicI64::new(0);
static PEAK_BYTES: AtomicI64 = AtomicI64::new(0);
static ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);

struct Counting;

fn bump(delta: i64) {
    let live = LIVE_BYTES.fetch_add(delta, Ordering::Relaxed) + delta;
    PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        bump(layout.size() as i64);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        bump(-(layout.size() as i64));
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        bump(new_size as i64 - layout.size() as i64);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const SEED: u64 = 0x5EED_1234;

/// Peak-tracked run of a zero-argument closure: resets the high-water mark
/// to the current live level first, so the returned peak is this call's
/// OWN growth, not whatever the caller already had resident.
fn measure_peak<T>(f: impl FnOnce() -> T) -> (T, i64, u64, u128) {
    let base = LIVE_BYTES.load(Ordering::Relaxed);
    PEAK_BYTES.store(base, Ordering::Relaxed);
    let calls0 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t0 = Instant::now();
    let out = f();
    let nanos = t0.elapsed().as_nanos();
    let peak = PEAK_BYTES.load(Ordering::Relaxed) - base;
    let calls = ALLOC_CALLS.load(Ordering::Relaxed) - calls0;
    (out, peak, calls, nanos)
}

fn vm_hwm_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            return rest.trim().trim_end_matches(" kB").trim().parse().ok();
        }
    }
    None
}

fn report(name: &str, rows: usize, peak_bytes: i64, allocs: u64, nanos: u128) {
    let per_row = if rows == 0 {
        0.0
    } else {
        peak_bytes as f64 / rows as f64
    };
    let allocs_per_row = if rows == 0 {
        0.0
    } else {
        allocs as f64 / rows as f64
    };
    let hwm = vm_hwm_kib().map(|k| k * 1024).unwrap_or(0);
    println!(
        "{name:<28} rows={rows:>9} peak={peak_mib:>9.1}MiB {per_row:>8.1}B/row \
         allocs={allocs:>10} ({allocs_per_row:>7.1}/row) {ms:>9.1}ms VmHWM={hwm_mib:>8.1}MiB",
        name = name,
        rows = rows,
        peak_mib = peak_bytes as f64 / (1024.0 * 1024.0),
        per_row = per_row,
        allocs = allocs,
        allocs_per_row = allocs_per_row,
        ms = nanos as f64 / 1e6,
        hwm_mib = hwm as f64 / (1024.0 * 1024.0),
    );
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

    // Chain: exactly n*(n-1)/2 closure pairs, n-1 epochs (one extra hop per
    // epoch — the pathological recursion-depth case), no dependence on RNG
    // density. `tc` occurs ONCE per recursive rule body: this is the
    // well-behaved semi-naive case (delta-driven prefix join against the
    // stored `edge` relation) — the baseline bytes/row, independent of any
    // delta-narrowing defect.
    println!("== transitive_closure/chain (single recursive atom, prefix-joinable) ==");
    for n in [500usize, 1_000, 2_000] {
        let d = dir();
        let (rows, peak, allocs, nanos) = measure_peak(|| {
            let w = transitive_closure(Backend::Fjall, Graph::Chain, n, SEED, &d);
            w.run()
        });
        report(&format!("tc/chain/n{n}"), rows, peak, allocs, nanos);
    }

    println!();
    println!("== points_to (pt occurs TWICE in load/store rule bodies — the self-join shape) ==");
    // Proportioned down from kyzo-bench's v3k-a2k-s6k (vars=3000, addrs=2000,
    // assigns=6000, loads=2000, stores=2000) by a constant factor, scaled up
    // to see the growth exponent.
    for scale in [1u64, 2, 4] {
        let vars = 200 * scale;
        let addrs = 150 * scale;
        let assigns = 400 * scale;
        let loads = 150 * scale;
        let stores = 150 * scale;
        let d = dir();
        let (rows, peak, allocs, nanos) = measure_peak(|| {
            let w = points_to(
                Backend::Fjall,
                vars,
                addrs,
                assigns,
                loads,
                stores,
                SEED,
                &d,
            );
            w.run()
        });
        report(
            &format!("pointsto/v{vars}-a{addrs}-s{assigns}"),
            rows,
            peak,
            allocs,
            nanos,
        );
    }

    if std::env::var("KYZO_FULL_SCALE").is_ok() {
        println!();
        println!(
            "== pointsto/v3k-a2k-s6k (kyzo-bench's full workload, issue #68's named repro) =="
        );
        let d = dir();
        let (rows, peak, allocs, nanos) = measure_peak(|| {
            let w = points_to(Backend::Fjall, 3_000, 2_000, 6_000, 2_000, 2_000, SEED, &d);
            w.run()
        });
        report("pointsto/v3k-a2k-s6k", rows, peak, allocs, nanos);
    }

    if std::env::var("KYZO_TC_SPARSE").is_ok() {
        println!();
        println!(
            "== tc/sparse-n10k-m30k (single recursive atom per rule — checks whether this \
             OOM shares the same-store-occurrence cause the points_to fix targets, or is a \
             separate per-row representation cost) =="
        );
        let d = dir();
        let (rows, peak, allocs, nanos) = measure_peak(|| {
            let w = transitive_closure(Backend::Fjall, Graph::Random, 10_000, SEED, &d);
            w.run()
        });
        report("tc/sparse-n10k-m30k", rows, peak, allocs, nanos);
    }
}
