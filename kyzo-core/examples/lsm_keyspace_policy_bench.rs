/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Issue #118 task 4 instrument: measures the four shapes the Monkey/
//! Dostoevsky per-keyspace policy (`storage/fjall.rs`'s `tuning` module) is
//! tuned for, against the SAME storage backend (`new_fjall_storage`) that
//! carries whatever `KeyspaceCreateOptions` are currently wired in. Run
//! once against a pre-tuning tree and once after to get a before/after
//! pair — the numbers are not meaningful in isolation, only as that pair.
//!
//! - **ingest**: bulk `:put` of fresh point rows — the append path every
//!   write takes.
//! - **point-get**: random-key reads of existing rows — what
//!   `expect_point_read_hits` and the shallow-level bloom allocation bet on.
//! - **full-scan**: an unbound current-state scan — sensitive to data block
//!   size and to how much of the tree the scan has to walk.
//! - **as-of on dense chains**: many superseded versions per key
//!   (bitemporal history), read at an arbitrary past instant — the shape
//!   that pushes deep-level reads, which the per-level block size and
//!   filter allocation targets.
//!
//! Run: `cargo run -p kyzo --release --example lsm_keyspace_policy_bench`

use std::collections::BTreeMap;
use std::time::Instant;

use kyzo::{Db, StorageOptions, new_fjall_storage_with};

fn params(pairs: &[(&str, i64)]) -> BTreeMap<String, kyzo::DataValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), kyzo::DataValue::from(*v)))
        .collect()
}

fn report(name: &str, ops: usize, secs: f64) {
    println!(
        "{name:<28} {ops:>8} ops  {secs:>10.3}s  {rate:>12.1} ops/s",
        name = name,
        ops = ops,
        secs = secs,
        rate = ops as f64 / secs,
    );
}

/// Bulk-load `rows` keyed integer rows into a fresh relation `pts`, batched
/// 1000 rows per `:put` (load phase — not itself measured as "ingest";
/// `phase_ingest` below repeats this shape under the clock).
fn seed_points(db: &Db<impl kyzo::Storage>, rows: usize) {
    db.run_script("?[k, v] <- [] :create pts {k => v}", params(&[]))
        .expect("create pts");
    let mut k = 0i64;
    while (k as usize) < rows {
        let batch_end = ((k as usize + 1000).min(rows)) as i64;
        let body: String = (k..batch_end)
            .map(|i| format!("[{i}, {i}]"))
            .collect::<Vec<_>>()
            .join(", ");
        db.run_script(
            &format!("?[k, v] <- [{body}] :put pts {{k, v}}"),
            params(&[]),
        )
        .expect("seed pts batch");
        k = batch_end;
    }
}

/// Fresh bulk ingest into a NEW relation, same batching shape as
/// `seed_points`, under the clock — the append path.
fn phase_ingest(db: &Db<impl kyzo::Storage>, rows: usize) -> f64 {
    db.run_script("?[k, v] <- [] :create ingested {k => v}", params(&[]))
        .expect("create ingested");
    let t0 = Instant::now();
    let mut k = 0i64;
    while (k as usize) < rows {
        let batch_end = ((k as usize + 1000).min(rows)) as i64;
        let body: String = (k..batch_end)
            .map(|i| format!("[{i}, {i}]"))
            .collect::<Vec<_>>()
            .join(", ");
        db.run_script(
            &format!("?[k, v] <- [{body}] :put ingested {{k, v}}"),
            params(&[]),
        )
        .expect("ingest batch");
        k = batch_end;
    }
    t0.elapsed().as_secs_f64()
}

/// `ops` point reads of existing keys in `pts`, bound-param each time —
/// the shape `expect_point_read_hits` targets.
fn phase_point_get(db: &Db<impl kyzo::Storage>, rows: usize, ops: usize) -> f64 {
    let t0 = Instant::now();
    for i in 0..ops {
        let key = (i % rows) as i64;
        db.run_script("?[v] := *pts[$k, v]", params(&[("k", key)]))
            .expect("point get");
    }
    t0.elapsed().as_secs_f64()
}

/// `passes` unbound full scans over `pts` (current state, all rows).
fn phase_full_scan(db: &Db<impl kyzo::Storage>, passes: usize) -> f64 {
    let t0 = Instant::now();
    for _ in 0..passes {
        let out = db
            .run_script("?[k, v] := *pts[k, v]", params(&[]))
            .expect("full scan");
        std::hint::black_box(out.rows.len());
    }
    t0.elapsed().as_secs_f64()
}

/// Builds a dense-bitemporal relation `hist`: `keys` distinct keys, each
/// with `generations` superseded versions at explicit, increasing validity
/// instants — one `:put` per generation, batched across all `keys` in that
/// generation (so `generations` script calls, not `keys * generations`).
/// Every key's whole version chain sits key-adjacent on disk (validity
/// rides in the key suffix), so this is the "dense chain" shape the
/// as-of read below has to walk into.
fn seed_dense_chains(db: &Db<impl kyzo::Storage>, keys: usize, generations: usize) {
    db.run_script("?[k, v] <- [] :create hist {k => v}", params(&[]))
        .expect("create hist");
    for g in 0..generations {
        let ts = 100 + (g as i64) * 10;
        let body: String = (0..keys)
            .map(|k| format!("[{k}, {}]", g * 1000 + k))
            .collect::<Vec<_>>()
            .join(", ");
        db.run_script(
            &format!("?[k, v] <- [{body}] :put hist {{k => v}} @ {ts}"),
            params(&[]),
        )
        .expect("hist generation batch");
    }
}

/// `ops` as-of scans over `hist` at scattered past instants within the
/// chain's range — every scan resolves EVERY key's chain at that instant,
/// so this touches however deep the dense chain sank on disk.
fn phase_asof_dense_chains(db: &Db<impl kyzo::Storage>, generations: usize, ops: usize) -> f64 {
    let t0 = Instant::now();
    for i in 0..ops {
        // Scatter across the whole generation range, not just the newest
        // (newest-only would never exercise the deep-level reads dense
        // chains create).
        let g = (i * 37) % generations;
        let at = 100 + (g as i64) * 10 + 5; // between generation `g` and `g+1`
        let out = db
            .run_script(&format!("?[k, v] := *hist[k, v @ {at}]"), params(&[]))
            .expect("as-of scan");
        std::hint::black_box(out.rows.len());
    }
    t0.elapsed().as_secs_f64()
}

fn main() {
    let tmp = tempfile::tempdir().expect("tempdir");

    const POINT_ROWS: usize = 20_000;
    const POINT_GET_OPS: usize = 3_000;
    const FULL_SCAN_PASSES: usize = 5;
    const INGEST_ROWS: usize = 20_000;
    const CHAIN_KEYS: usize = 300;
    const CHAIN_GENERATIONS: usize = 150;
    const ASOF_OPS: usize = 60;

    println!("issue #118 task 4 — Monkey/Dostoevsky keyspace policy bench");
    println!(
        "point_rows={POINT_ROWS} ingest_rows={INGEST_ROWS} chain_keys={CHAIN_KEYS} \
         chain_generations={CHAIN_GENERATIONS} (={} stored versions)",
        CHAIN_KEYS * CHAIN_GENERATIONS
    );
    println!("{}", "-".repeat(78));

    // Shrink the flush/compaction unit so this bench's modest row counts
    // still span several LSM levels — a real store reaches the same
    // levels at fjall's stock 64 MiB unit, just over more data. See
    // `StorageOptions::max_memtable_size_bytes`.
    let opts = StorageOptions {
        max_memtable_size_bytes: Some(64 * 1_024),
        table_target_size_bytes: Some(64 * 1_024),
        ..Default::default()
    };
    let db =
        Db::new(new_fjall_storage_with(tmp.path().join("w"), opts).expect("storage")).expect("db");

    seed_points(&db, POINT_ROWS);
    seed_dense_chains(&db, CHAIN_KEYS, CHAIN_GENERATIONS);

    let secs = phase_ingest(&db, INGEST_ROWS);
    report("ingest", INGEST_ROWS, secs);

    let secs = phase_point_get(&db, POINT_ROWS, POINT_GET_OPS);
    report("point-get", POINT_GET_OPS, secs);

    let secs = phase_full_scan(&db, FULL_SCAN_PASSES);
    report("full-scan", FULL_SCAN_PASSES * POINT_ROWS, secs);

    let secs = phase_asof_dense_chains(&db, CHAIN_GENERATIONS, ASOF_OPS);
    report("as-of dense chains", ASOF_OPS * CHAIN_KEYS, secs);
}
