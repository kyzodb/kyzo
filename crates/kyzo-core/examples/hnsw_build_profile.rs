/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! HNSW insert-path scaling profile (chasing the superlinear build-time
//! bug: bench lane measured ~O(n^1.5) at M=16, ef_construction=200 — 1k
//! vectors in 1.85s, 3k in 9.85s, 10k in 50.4s, where hnswlib/FAISS build
//! 1M in ~34s).
//!
//! Builds a fresh index at N in {1000, 2000, 4000, 8000} (seeded random
//! vectors, deterministic), times ONLY the incremental insert path (the
//! index is created empty, then every row is `:put` through the mutation
//! hook that drives `hnsw_put` per row — this is the same code path a
//! bulk load or a backfill takes), and prints per-N wall time plus the
//! fitted exponent between consecutive doublings.
//!
//! Run: `cargo run -p kyzo --release --example hnsw_build_profile`

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Instant;

use kyzo::{DataValue, Db, new_fjall_storage};

const SEED: u64 = 0xC0FF_EE15_2026;
const DIM: usize = 32;

fn splitmix64(state: &mut u64) -> u64 {
    // INVARIANT(splitmix64): modular mix per the splitmix64 contract; wrap is the PRNG.
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A deterministic pseudo-random unit-ish f32 in [-1, 1) from a running
/// splitmix64 state — no `rand` Rng trait needed, just reproducible bytes.
fn next_f32(state: &mut u64) -> f32 {
    let bits = splitmix64(state);
    let u = (bits >> 11) as f64 / (1u64 << 53) as f64; // [0, 1)
    (u * 2.0 - 1.0) as f32
}

fn gen_vec(state: &mut u64) -> Vec<f32> {
    (0..DIM).map(|_| next_f32(state)).collect()
}

fn no_params() -> BTreeMap<String, DataValue> {
    BTreeMap::new()
}

/// Build one `:put` script inserting rows `[lo, hi)`, each `id => vec([...])`.
fn put_script(lo: usize, hi: usize, state: &mut u64) -> String {
    let mut s = String::from("?[id, v] <- [");
    for id in lo..hi {
        if id > lo {
            s.push(',');
        }
        write!(s, "[{id}, vec([").unwrap();
        let v = gen_vec(state);
        for (i, x) in v.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            write!(s, "{x}").unwrap();
        }
        s.push_str("])]");
    }
    s.push_str("] :put doc {id => v}");
    s
}

/// One fresh index, insert N vectors through the incremental put path,
/// return (build_secs, recall-irrelevant — this profile is timing only).
fn run_one(n: usize, m: usize, ef_construction: usize) -> f64 {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::new(new_fjall_storage(dir.path()).expect("storage")).expect("db");
    db.run_script(
        "?[id, v] <- [] :create doc {id => v: <F32; 32>}",
        no_params(),
    )
    .expect("create doc");
    db.run_script(
        &format!(
            "::hnsw create doc:emb {{fields: [v], dim: {DIM}, m: {m}, \
             ef_construction: {ef_construction}, distance: L2}}"
        ),
        no_params(),
    )
    .expect("hnsw create");

    let mut state = SEED;
    // Insert in batches so the script string stays a manageable size;
    // each batch still drives `hnsw_put` once per row via the mutation
    // hook, same as a single giant `:put` would.
    const BATCH: usize = 500;
    let start = Instant::now();
    let mut lo = 0;
    while lo < n {
        let hi = (lo + BATCH).min(n);
        let script = put_script(lo, hi, &mut state);
        db.run_script(&script, no_params()).expect("put batch");
        lo = hi;
    }
    start.elapsed().as_secs_f64()
}

fn main() {
    let m = 16;
    let ef_construction = 200;
    println!("HNSW insert-path scaling (M={m}, ef_construction={ef_construction}, dim={DIM})");
    println!("{:>8}  {:>10}  {:>10}", "n", "seconds", "exponent");
    let ns = [1000usize, 2000, 4000, 8000];
    let mut prev: Option<(usize, f64)> = None;
    for &n in &ns {
        let secs = run_one(n, m, ef_construction);
        let exponent = match prev {
            Some((prev_n, prev_secs)) if prev_secs > 0.0 && secs > 0.0 => {
                let ratio_t = secs / prev_secs;
                let ratio_n = n as f64 / prev_n as f64;
                format!("{:.3}", ratio_t.ln() / ratio_n.ln())
            }
            _ => "-".to_string(),
        };
        println!("{n:>8}  {secs:>10.3}  {exponent:>10}");
        prev = Some((n, secs));
    }
}
