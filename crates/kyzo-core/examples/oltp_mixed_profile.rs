/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Attribution instrument for issue #82 (OLTP mixed-op catastrophic
//! slowdown, ~920x SQLite / non-terminating at scale): isolates the
//! current-state segment cache (`engines/segments.rs`) as the mixed-phase
//! cost driver, independent of storage or parse overhead.
//!
//! `StoredRA::prefix_join_batched` (`query/ra/stored.rs`) is how a
//! point-keyed read compiles (the bound key becomes a synthetic join, not a
//! filtered `iter_batched` scan — confirmed by reading `join.rs`'s
//! `join_is_prefix` dispatch). On a segment miss it still calls
//! `segment_at`, which pays a FULL relation scan-and-decode
//! (`Segment::build`) to serve what the caller only needed one row of. A
//! read-only workload pays that once, amortized over every later probe. A
//! MIXED workload does not: every committed write bumps the relation's
//! generation (`SegmentEngine::bump_before_commit`), so the very next read's
//! live stamp never classifies the pre-write sealed segment as fresh and
//! `segment_at` rebuilds from scratch — an O(n) relation scan to answer one
//! point read, every single time a read follows a write.
//!
//! This isolates that shape: phase A interleaves NO writes (steady state,
//! the segment's intended case); phase B alternates one write per read (the
//! bench lane's `phase_mixed` shape). Same op count, same relation, same
//! backend — the only variable is whether phase B's generation ever holds
//! still long enough for the segment to pay off.
//!
//! Run: `cargo run -p kyzo --release --example oltp_mixed_profile`

use std::collections::BTreeMap;
use std::time::Instant;

use kyzo::{Db, new_fjall_storage};

fn params(pairs: &[(&str, i64)]) -> BTreeMap<String, kyzo::DataValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), kyzo::DataValue::from(*v)))
        .collect()
}

fn report(name: &str, ops: usize, secs: f64) {
    println!(
        "{name:<32} {ops:>8} ops  {secs:>10.3}s  {rate:>12.1} ops/s",
        name = name,
        ops = ops,
        secs = secs,
        rate = ops as f64 / secs,
    );
}

/// Load `rows` keyed integer rows into a fresh relation `r` in one bulk
/// `:put` (this is the load phase — not what's being measured).
fn seed(db: &Db<impl kyzo::Storage>, rows: usize) {
    db.run_script("?[k, v] <- [] :create r {k => v}", params(&[]))
        .expect("create");
    // Batches of 1000, matching the bench lane's own bulk-load shape.
    let mut k = 0i64;
    while (k as usize) < rows {
        let batch_end = ((k as usize + 1000).min(rows)) as i64;
        let body: String = (k..batch_end)
            .map(|i| format!("[{i}, {i}]"))
            .collect::<Vec<_>>()
            .join(", ");
        db.run_script(&format!("?[k, v] <- [{body}] :put r {{k, v}}"), params(&[]))
            .expect("seed batch");
        k = batch_end;
    }
}

/// `ops` point reads of existing keys, no writes interleaved — the
/// steady-state case a segment is built for: first read pays the build,
/// every later read is a cache hit.
fn phase_read_only(db: &Db<impl kyzo::Storage>, rows: usize, ops: usize) -> f64 {
    let t0 = Instant::now();
    for i in 0..ops {
        let key = (i % rows) as i64;
        db.run_script("?[v] := *r[$k, v]", params(&[("k", key)]))
            .expect("point read");
    }
    t0.elapsed().as_secs_f64()
}

/// `ops` alternating (point read, point update) pairs on existing keys —
/// the bench lane's mixed-phase shape: every read is immediately preceded
/// by a committed write to the SAME relation, so the relation's generation
/// never holds still between reads.
fn phase_mixed(db: &Db<impl kyzo::Storage>, rows: usize, ops: usize) -> f64 {
    let t0 = Instant::now();
    for i in 0..ops {
        let key = (i % rows) as i64;
        db.run_script(
            "?[k, v] <- [[$k, $v]] :put r {k, v}",
            params(&[("k", key), ("v", key + 1)]),
        )
        .expect("point write");
        db.run_script("?[v] := *r[$k, v]", params(&[("k", key)]))
            .expect("point read");
    }
    t0.elapsed().as_secs_f64()
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

    println!("issue #82 — OLTP mixed-op segment-thrash attribution");
    println!("{}", "-".repeat(78));

    for rows in [2_000usize, 5_000, 10_000] {
        let ops = 500;

        let db = Db::new(new_fjall_storage(dir()).expect("storage")).expect("db");
        seed(&db, rows);
        let secs = phase_read_only(&db, rows, ops);
        report(&format!("read-only  r{rows}"), ops, secs);

        let db = Db::new(new_fjall_storage(dir()).expect("storage")).expect("db");
        seed(&db, rows);
        let secs = phase_mixed(&db, rows, ops);
        report(&format!("mixed r/w  r{rows}"), ops, secs);
        println!();
    }
}
