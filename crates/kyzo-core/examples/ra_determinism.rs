/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Cross-thread determinism probe for the batched RA path.
//!
//! Runs a set of workloads that actually parallelize (semi-naive TC recursion
//! runs its per-stratum rule batch on rayon) in the batched execution mode and
//! prints a stable hash of the canonical (key-ordered) serialized output. The
//! driver re-runs this under `RAYON_NUM_THREADS ∈ {1,2,4,8}` and diffs the
//! hashes: identical hashes at every thread count == byte-identical output ==
//! the batched path is order/content deterministic under parallelism.
//!
//! Run: `RAYON_NUM_THREADS=N cargo run -p kyzo --release \
//!        --features bench-internals --example ra_determinism`

use std::hash::{Hash, Hasher};

use kyzo::bench_api::{Backend, Graph, aggregation, scan_filter, transitive_closure};

const SEED: u64 = 0x5EED_1234;

/// A stable content hash of the workload's canonical output rows. `collect`
/// returns rows already in the store's key order; we hash their `Debug`
/// serialization (a total, deterministic function of the DataValues).
fn hash_output(rows: &[kyzo::DataValue]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for v in rows {
        format!("{v:?}").hash(&mut h);
    }
    h.finish()
}

fn main() {
    let threads = std::env::var("RAYON_NUM_THREADS").unwrap_or_else(|_| "default".into());
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut n = 0usize;
    let mut dir = || {
        let p = tmp.path().join(format!("w{n}"));
        n += 1;
        std::fs::create_dir_all(&p).expect("mkdir");
        p
    };

    let mut combined = std::collections::hash_map::DefaultHasher::new();
    let workloads: Vec<(&str, Vec<kyzo::Tuple>)> = vec![
        (
            "tc/dense/400/fjall",
            transitive_closure(Backend::Fjall, Graph::Dense, 400, SEED, &dir()).collect(),
        ),
        (
            "tc/random/400/fjall",
            transitive_closure(Backend::Fjall, Graph::Random, 400, SEED, &dir()).collect(),
        ),
        (
            "tc/chain/240/mem",
            transitive_closure(Backend::Mem, Graph::Chain, 240, SEED, &dir()).collect(),
        ),
        (
            "scan_filter/200k/sel50/fjall",
            scan_filter(Backend::Fjall, 200_000, 50, SEED, &dir()).collect(),
        ),
        (
            "aggregation/200k/g1k/mem",
            aggregation(Backend::Mem, 200_000, 1_000, SEED, &dir()).collect(),
        ),
    ];

    for (name, rows) in &workloads {
        let flat: Vec<kyzo::DataValue> = rows.iter().flatten().cloned().collect();
        let h = hash_output(&flat);
        rows.len().hash(&mut combined);
        h.hash(&mut combined);
        println!("{name:<32} rows={:<9} hash={h:016x}", rows.len());
    }
    println!("THREADS={threads} COMBINED={:016x}", combined.finish());
}
