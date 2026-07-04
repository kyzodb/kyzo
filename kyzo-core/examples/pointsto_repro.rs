/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Issue #68 reopened: kyzo-bench's `datalog-rig` (kyzo-runner, a thin shim
//! over the PUBLIC `Db::run_script`) still OOMs on `pointsto/v3k-a2k-s6k` on
//! commit 7447589, contradicting that commit's own closing claim ("completes
//! at 1.93GiB peak, 4,079,570 rows"), which was measured through
//! `fixpoint_mem_profile.rs` — a crate-internal path
//! (`stratified_magic_compile` → `bind_for_eval` → `stratified_evaluate`,
//! entirely `pub(crate)`) that never goes through `Db::run_script` at all.
//!
//! This example is the other side of that comparison: the SAME facts (same
//! generator algorithm, same seed, same counts) and the SAME program text
//! as `pointsto.kz`, driven through the PUBLIC path exactly as
//! `kyzo-bench`'s `kyzo-runner` does it — a fresh `Db` over `fjall`, facts
//! loaded via chunked literal `:put` scripts, the program run via
//! `Db::run_script`. Load and query phases are timed and peak-metered
//! separately (own counting allocator, reset before each phase) so a
//! blowup attributes to loading vs. evaluation, not just "somewhere".
//!
//! Run: `cargo run -p kyzo --release --features bench-internals --example pointsto_repro`
//! Under a cap (mirrors the bug's death signature):
//! `(ulimit -v 4194304 && timeout 180 cargo run -p kyzo --release --features bench-internals --example pointsto_repro)`

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Instant;

use kyzo::{DataValue, Db, new_fjall_storage};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

// ── counting global allocator: live bytes + high-water mark (mirrors
// fixpoint_mem_profile.rs's instrument, so the two paths' numbers compare
// directly) ──────────────────────────────────────────────────────────────
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

/// Matches `fixpoint_mem_profile.rs`'s `SEED` and its `KYZO_FULL_SCALE`
/// points-to call exactly, so this example generates byte-identical facts.
const SEED: u64 = 0x5EED_1234;

fn vm_hwm_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            return rest.trim().trim_end_matches(" kB").trim().parse().ok();
        }
    }
    None
}

struct Phase {
    peak_bytes: i64,
    allocs: u64,
    nanos: u128,
}

fn measure<T>(f: impl FnOnce() -> T) -> (T, Phase) {
    let base = LIVE_BYTES.load(Ordering::Relaxed);
    PEAK_BYTES.store(base, Ordering::Relaxed);
    let calls0 = ALLOC_CALLS.load(Ordering::Relaxed);
    let t0 = Instant::now();
    let out = f();
    let nanos = t0.elapsed().as_nanos();
    let peak_bytes = PEAK_BYTES.load(Ordering::Relaxed) - base;
    let allocs = ALLOC_CALLS.load(Ordering::Relaxed) - calls0;
    (
        out,
        Phase {
            peak_bytes,
            allocs,
            nanos,
        },
    )
}

fn report(name: &str, p: &Phase) {
    let hwm = vm_hwm_kib().map(|k| k * 1024).unwrap_or(0);
    println!(
        "{name:<16} peak={peak_mib:>9.1}MiB allocs={allocs:>10} {ms:>10.1}ms VmHWM={hwm_mib:>8.1}MiB",
        name = name,
        peak_mib = p.peak_bytes as f64 / (1024.0 * 1024.0),
        allocs = p.allocs,
        ms = p.nanos as f64 / 1e6,
        hwm_mib = hwm as f64 / (1024.0 * 1024.0),
    );
}

/// Same algorithm as `bench_api::points_to`'s `gen_rel` closure: an
/// independent `StdRng` stream per relation (seed XORed with a label),
/// deduped via `BTreeSet`, no self-pairs.
fn gen_rel(seed: u64, label: u64, vars: u64, count: u64) -> Vec<(i64, i64)> {
    let mut rng = StdRng::seed_from_u64(seed ^ (label << 32));
    let mut rows: BTreeSet<(i64, i64)> = BTreeSet::new();
    while (rows.len() as u64) < count {
        let y = rng.random_range(0..vars as i64);
        let x = rng.random_range(0..vars as i64);
        if y != x {
            rows.insert((y, x));
        }
    }
    rows.into_iter().collect()
}

const LOAD_CHUNK_ROWS: usize = 5_000;

/// Mirrors kyzo-bench's `kyzo-runner::load_relation` exactly: `:create`
/// then chunked LITERAL `:put` scripts (not `$data`-param-driven) of up to
/// `LOAD_CHUNK_ROWS` rows each.
fn load_relation(db: &Db<kyzo::FjallStorage>, name: &str, rows: &[(i64, i64)]) {
    let no_params = BTreeMap::<String, DataValue>::new();
    db.run_script(
        &format!("?[c0, c1] <- [] :create {name} {{c0, c1}}"),
        no_params.clone(),
    )
    .expect("create");
    for chunk in rows.chunks(LOAD_CHUNK_ROWS) {
        let mut body = String::new();
        for (y, x) in chunk {
            body.push_str(&format!("[{y},{x}],"));
        }
        let script = format!("?[c0, c1] <- [{body}] :put {name} {{c0, c1}}");
        db.run_script(&script, no_params.clone())
            .expect("put chunk");
    }
}

/// `pointsto.kz`, verbatim (diffed byte-for-byte against
/// kyzo-bench/benches/datalog/programs/pointsto.kz for issue #68's repro).
const POINTSTO_KZ: &str = r#"
pt[y, x] := *addr_of[y, x]
pt[y, x] := *assign[y, z], pt[z, x]
pt[y, w] := *load[y, x], pt[x, z], pt[z, w]
pt[z, w] := *store[y, x], pt[y, z], pt[x, w]
?[y, x] := pt[y, x]
"#;

fn main() {
    let full_scale = std::env::var("KYZO_FULL_SCALE").is_ok();
    let (vars, addrs, assigns, loads, stores) = if full_scale {
        (3_000u64, 2_000u64, 6_000u64, 2_000u64, 2_000u64)
    } else {
        // Same proportioned-down default as fixpoint_mem_profile.rs's
        // scale=4 step, for a quick sanity check before paying for the
        // full workload.
        (800u64, 600u64, 1_600u64, 600u64, 600u64)
    };

    println!(
        "== pointsto/v{vars}-a{addrs}-s{assigns} via PUBLIC Db::run_script (kyzo-bench's kyzo-runner shape) =="
    );

    let (addr_of, (assign, (load, store))) = (
        gen_rel(SEED, 1, vars, addrs),
        (
            gen_rel(SEED, 2, vars, assigns),
            (
                gen_rel(SEED, 3, vars, loads),
                gen_rel(SEED, 4, vars, stores),
            ),
        ),
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let storage = new_fjall_storage(tmp.path()).expect("fjall");
    let db = Db::new(storage).expect("db");

    let (_, load_phase) = measure(|| {
        load_relation(&db, "addr_of", &addr_of);
        load_relation(&db, "assign", &assign);
        load_relation(&db, "load", &load);
        load_relation(&db, "store", &store);
    });
    report("load", &load_phase);

    let no_params = BTreeMap::<String, DataValue>::new();
    let (rows, query_phase) = measure(|| db.run_script(POINTSTO_KZ, no_params).expect("query"));
    report("query", &query_phase);
    println!("rows={}", rows.rows.len());
}
