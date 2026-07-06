/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Story #118's second law: the fjall adapter's byte currency is `Slice`
//! (Arc-backed, `kyzo-core/src/storage/mod.rs`'s `ReadTx::get`/`range_scan`/
//! `total_scan`), never a per-row `Vec<u8>` copy. The prior design converted
//! every scanned key AND value with `.to_vec()` (`storage/fjall.rs`'s
//! `raw_range`/`read_get`/`read_total_scan`) — two heap allocations on EVERY
//! row, an allocation count that grew linearly with however many rows a
//! scan touched. The fix hands back the `Slice` fjall's own `Guard` already
//! produced (`materialize_row`/`materialize_key`), adding none of its own.
//!
//! This is a real regression test, not an assertion on memory from
//! memory: a counting global allocator (mirrors
//! `examples/pointsto_repro.rs`'s instrument) measures actual `alloc`/
//! `alloc_zeroed`/`realloc` CALLS around a full-store scan, at two very
//! different row counts within the SAME store (so both measurements see
//! the identical on-disk structure — no cross-store confound from
//! compaction state or file layout). A single integration-test binary with
//! exactly one `#[test]` function: `cargo test` runs test BINARIES one at a
//! time, so nothing else in this process is allocating while the
//! measurement window is open, and this file's own single test can install
//! a process-wide `#[global_allocator]` without racing unrelated tests.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use kyzo::{ReadTx, Storage, WriteTx, new_fjall_storage};

static ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Write `n` small rows, committed as one transaction (well under fjall's
/// own internal flush thresholds, so both scans below hit the same
/// memtable/SSTable shape modulo row count).
fn write_rows(db: &impl Storage, n: u32) {
    let mut tx = db.write_tx().unwrap();
    for i in 0..n {
        let k = format!("k{i:08}").into_bytes();
        let v = format!("v{i:08}").into_bytes();
        tx.put(&k, &v).unwrap();
    }
    tx.commit().unwrap();
}

/// Allocation calls spent scanning exactly `[lower, upper)` of an already
/// open read snapshot — opening the snapshot itself is excluded, so only
/// the scan's own materialization is measured.
fn allocs_for_range(tx: &impl ReadTx, lower: &[u8], upper: &[u8]) -> (usize, u64) {
    let before = ALLOC_CALLS.load(Ordering::Relaxed);
    let mut n = 0usize;
    for row in tx.range_scan(lower, upper) {
        row.unwrap();
        n += 1;
    }
    let after = ALLOC_CALLS.load(Ordering::Relaxed);
    (n, after - before)
}

/// The law: scanning 100x more rows must not cost anywhere near 100x the
/// allocations. The old `.to_vec()`-per-field adapter scaled exactly
/// linearly (2 allocations per row, on top of whatever fjall's own guard
/// construction costs); this adapter's marginal cost per additional row is
/// bounded by a small constant instead, so the ratio of allocations stays
/// far below the ratio of rows. (Measured on this machine: both the
/// 100-row and the 10,000-row scan cost the SAME 7 allocations — the
/// thresholds below are deliberately looser than that exact result, so the
/// law is about the shape of the scaling, not a brittle pin on one number.)
#[test]
fn range_scan_allocation_count_does_not_scale_with_row_count() {
    const SMALL: u32 = 100;
    const LARGE: u32 = 10_000;

    let dir = tempfile::tempdir().unwrap();
    let db = new_fjall_storage(dir.path()).unwrap();
    write_rows(&db, LARGE);

    let tx = db.read_tx().unwrap();
    let lower = b"".as_slice();
    let upper = b"k\xff\xff\xff\xff\xff\xff\xff\xff".as_slice();

    let small_upper = format!("k{SMALL:08}").into_bytes();
    let (n_small, allocs_small) = allocs_for_range(&tx, lower, &small_upper);
    assert_eq!(
        n_small, SMALL as usize,
        "sanity: the bounded scan saw exactly SMALL rows"
    );

    let (n_large, allocs_large) = allocs_for_range(&tx, lower, upper);
    assert_eq!(
        n_large, LARGE as usize,
        "sanity: the full scan saw exactly LARGE rows"
    );

    let row_ratio = f64::from(LARGE) / f64::from(SMALL);
    let alloc_ratio = allocs_large as f64 / allocs_small.max(1) as f64;
    assert!(
        alloc_ratio < row_ratio / 4.0,
        "{LARGE} rows cost {allocs_large} allocations vs {SMALL} rows costing \
         {allocs_small} ({alloc_ratio:.1}x for {row_ratio:.0}x the rows) — the \
         adapter is back to allocating per scanned row instead of handing \
         back fjall's own Arc-backed Slice"
    );

    // Stated directly, not just as a ratio: a marginal rate anywhere close
    // to the old design's 2-per-row (a 100x larger scan paying ~2*(LARGE -
    // SMALL) MORE allocations) would fail this outright.
    let marginal_per_row =
        (allocs_large.saturating_sub(allocs_small)) as f64 / f64::from(LARGE - SMALL);
    assert!(
        marginal_per_row < 1.0,
        "marginal allocation cost per additional scanned row is {marginal_per_row:.3} \
         (expected well under 1.0 — the old design cost 2.0)"
    );
}
