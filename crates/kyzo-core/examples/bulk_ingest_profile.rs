/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Attribution instrument for issue #74: where a bulk `:put` batch's time
//! goes. The bench lane found bulk load 7-9x slower than SQLite (vs a 2.5-3x
//! premium on autocommit point ops) — this profiles the ingest path as its
//! own subsystem: script parse (literal-in-text vs `$param`-substituted),
//! the mutation pipeline's per-row work (extract/encode/SSI-probe/write)
//! through the public `Db`, and the bare storage floor (raw fjall
//! put+commit, no relation/session/catalog at all).
//!
//! Every phase is measured by wall time over a warmed-up region (each
//! function does exactly one thing, so times subtract cleanly); see
//! `bench_api.rs`'s "bulk-ingest attribution" section for what each call
//! isolates.
//!
//! Run: `cargo run -p kyzo --release --features bench-internals --example bulk_ingest_profile`

use std::time::{Duration, Instant};

use kyzo::bench_api::{
    Backend, bare_fjall_put_batches, encode_only, parse_put_literal, parse_put_param,
    probe_only_not_found, run_put_batches,
};

const BATCH: usize = 1_000;
const BATCHES: usize = 100; // 100k rows total, matching the bench lane's r100k shape.

fn time_it<F: FnMut()>(mut f: F) -> Duration {
    f(); // warm up
    let t0 = Instant::now();
    f();
    t0.elapsed()
}

fn rows_per_sec(rows: usize, d: Duration) -> f64 {
    rows as f64 / d.as_secs_f64()
}

fn report(name: &str, rows: usize, d: Duration) {
    println!(
        "{name:<32} {rows:>8} rows  {ms:>10.2}ms  {rps:>12.0} rows/s",
        name = name,
        rows = rows,
        ms = d.as_secs_f64() * 1000.0,
        rps = rows_per_sec(rows, d),
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

    println!("issue #74 — bulk :put attribution ({BATCHES} batches of {BATCH} rows)");
    println!("{}", "-".repeat(78));

    // (a) Script parse alone, no execution: literal-in-text vs
    // $param-substituted, over BATCHES iterations (comparable to the
    // full-path totals below). Answers "is the 1000-row literal re-parsed
    // every batch, and how much of the wall is that."
    let d = time_it(|| {
        for _ in 0..BATCHES {
            parse_put_literal(BATCH).expect("parse literal");
        }
    });
    report("parse: literal (text)", BATCH * BATCHES, d);
    let d = time_it(|| {
        for _ in 0..BATCHES {
            parse_put_param(BATCH).expect("parse param");
        }
    });
    report("parse: $param-substituted", BATCH * BATCHES, d);

    // (c) Key+value encode alone (SimStorage-free of disk I/O, no probe,
    // no write, no commit): the exact per-row encode calls
    // `put_into_relation` makes.
    let d = encode_only(BATCH * BATCHES).expect("encode_only");
    report("encode-only (key+val)", BATCH * BATCHES, d);

    // (c) The SSI current-row probe alone, against an empty relation (the
    // genuine bulk-INSERT shape: every probed key is absent) — every put
    // row pays this UNCONDITIONALLY (SSI soundness), so its cost floors
    // the whole path regardless of any other fix.
    let d = probe_only_not_found(BATCH * BATCHES).expect("probe_only_not_found");
    report("probe-only (not-found)", BATCH * BATCHES, d);

    // (f) The bare storage floor: raw fjall put+commit, nothing else.
    let (rows, d) = bare_fjall_put_batches(BATCH, BATCHES, &dir()).expect("bare_fjall_put_batches");
    report("bare fjall floor", rows, d);

    // (a)+(b)+(c)+(d)+(e)+(f) together: the full path through the public
    // `Db`, literal vs param-driven, on the real fjall backend.
    let (rows, d) = run_put_batches(Backend::Fjall, BATCH, BATCHES, false, &dir())
        .expect("run_put_batches literal");
    report("full path: literal (fjall)", rows, d);
    let (rows, d) = run_put_batches(Backend::Fjall, BATCH, BATCHES, true, &dir())
        .expect("run_put_batches param");
    report("full path: param (fjall)", rows, d);

    // Same on the in-memory backend: isolates the engine's own cost from
    // fjall's disk-backed commit/SSI machinery.
    let (rows, d) = run_put_batches(Backend::Mem, BATCH, BATCHES, false, &dir())
        .expect("run_put_batches literal mem");
    report("full path: literal (mem)", rows, d);
    let (rows, d) = run_put_batches(Backend::Mem, BATCH, BATCHES, true, &dir())
        .expect("run_put_batches param mem");
    report("full path: param (mem)", rows, d);
}
