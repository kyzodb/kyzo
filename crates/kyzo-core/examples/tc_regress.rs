/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Issue #75 differential reproducer: tc/sparse-n2k-m6k through the public
//! Db, matching kyzo-bench's actual `kyzo-runner` + `tc.kz` + SplitMix64
//! digraph generator exactly (workload registry:
//! kyzo-bench/benches/datalog/rig/src/workloads.rs, seed 22_101, n=2000,
//! m=6000; program: kyzo-bench/benches/datalog/programs/tc.kz), so this
//! in-repo run is the same workload the reported differential measured,
//! run through the same public front door (`Db::run_script`), without
//! invoking the bench harness itself.
//!
//! Usage: tc_regress <variant>
//!   full  -- ?[x,y] := tc[x,y]              (materializes every closure row)
//!   count -- ?[count(x)] := tc[x,y]         (same fixpoint, 1-row output)
//!
//! Prints one line to stdout:
//!   PHASE variant=<> edges=<> load_ms=<f64> query_ms=<f64> rows=<usize> \
//!     vmhwm_after_load_kb=<u64> vmhwm_after_query_kb=<u64>

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use kyzo::{Db, new_fjall_storage};

const SEED: u64 = 22_101;
const N: u64 = 2_000;
const M: u64 = 6_000;
const LOAD_CHUNK_ROWS: usize = 5_000; // matches kyzo-runner's LOAD_CHUNK_ROWS

/// SplitMix64, byte-for-byte the same algorithm as
/// kyzo-bench/harness/src/seed.rs, so the generated edge set is identical.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        SplitMix64 { state: seed }
    }
    fn next_u64(&mut self) -> u64 {
        // INVARIANT(splitmix64): modular mix per the splitmix64 contract; wrap is the PRNG.
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, bound: u64) -> u64 {
        let zone = u64::MAX - (u64::MAX % bound);
        loop {
            let v = self.next_u64();
            if v < zone {
                return v % bound;
            }
        }
    }
}

fn vmhwm_kb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            return rest
                .trim()
                .trim_end_matches(" kB")
                .trim()
                .parse()
                .expect("parse VmHWM");
        }
    }
    0
}

/// kyzo-bench's `random_digraph`: rejection-sample until exactly `m`
/// distinct non-self-loop edges are collected, in a BTreeSet (dedup +
/// deterministic emission order).
fn random_digraph(seed: u64, n: u64, m: u64) -> Vec<(u64, u64)> {
    let mut rng = SplitMix64::new(seed);
    let mut rows: BTreeSet<(u64, u64)> = BTreeSet::new();
    while (rows.len() as u64) < m {
        let x = rng.below(n);
        let y = rng.below(n);
        if x != y {
            rows.insert((x, y));
        }
    }
    rows.into_iter().collect()
}

fn no_params() -> BTreeMap<String, kyzo::DataValue> {
    BTreeMap::new()
}

fn main() {
    let variant = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "full".to_string());

    let edges = random_digraph(SEED, N, M);
    eprintln!("generated {} edges over {} nodes", edges.len(), N);
    assert_eq!(edges.len() as u64, M, "generator must hit exactly m edges");

    let tmp = tempfile::tempdir().expect("tempdir");
    let storage = new_fjall_storage(tmp.path()).expect("open fjall storage");
    let db = Db::new(storage).expect("open db");

    // ── load phase — same shape as kyzo-runner: :create then :put in
    //    LOAD_CHUNK_ROWS chunks ─────────────────────────────────────────
    let t_load = Instant::now();
    let lit = |rows: &[(u64, u64)]| -> String {
        let body: Vec<String> = rows.iter().map(|(a, b)| format!("[{a},{b}]")).collect();
        format!("[{}]", body.join(","))
    };
    db.run_script("?[c0, c1] <- [] :create edge {c0, c1}", no_params())
        .expect("create edge");
    for chunk in edges.chunks(LOAD_CHUNK_ROWS) {
        db.run_script(
            &format!("?[c0, c1] <- {} :put edge {{c0, c1}}", lit(chunk)),
            no_params(),
        )
        .expect("put edge chunk");
    }
    let load_ms = t_load.elapsed().as_secs_f64() * 1000.0;
    let vmhwm_after_load = vmhwm_kb();

    // ── query phase — kyzo-bench/benches/datalog/programs/tc.kz, exactly ──
    let q = match variant.as_str() {
        "full" => {
            "tc[x, y] := *edge[x, y]\n\
             tc[x, y] := tc[x, z], *edge[z, y]\n\
             ?[x, y] := tc[x, y]"
        }
        "count" => {
            "tc[x, y] := *edge[x, y]\n\
             tc[x, y] := tc[x, z], *edge[z, y]\n\
             ?[count(x)] := tc[x, y]"
        }
        "limit10" => {
            // Same fixpoint (tc is a prior stratum, fully computed before
            // the entry head runs), but :limit truncates the final head
            // relation's row collection to 10 — isolates fixpoint-only
            // cost from the cost of materializing/returning ~3.5M rows.
            "tc[x, y] := *edge[x, y]\n\
             tc[x, y] := tc[x, z], *edge[z, y]\n\
             ?[x, y] := tc[x, y] :limit 10"
        }
        other => panic!("unknown variant {other}"),
    };

    let t_query = Instant::now();
    let result = db.run_script(q, no_params()).expect("closure query");
    let query_ms = t_query.elapsed().as_secs_f64() * 1000.0;
    let vmhwm_after_query = vmhwm_kb();

    let rows = match variant.as_str() {
        "full" => result.rows.len(),
        "count" => result.rows[0][0].get_int().expect("count int") as usize,
        "limit10" => result.rows.len(),
        _ => unreachable!(),
    };

    println!(
        "PHASE variant={variant} edges={} load_ms={load_ms:.1} query_ms={query_ms:.1} \
         rows={rows} vmhwm_after_load_kb={vmhwm_after_load} vmhwm_after_query_kb={vmhwm_after_query}",
        edges.len(),
    );
}
