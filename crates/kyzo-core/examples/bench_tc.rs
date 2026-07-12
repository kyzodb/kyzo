/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Standard benchmark: transitive closure over a real published graph.
//!
//! This is the vanilla, community-standard recursive-Datalog workload — the
//! canonical two-rule transitive-closure program run over a **real SNAP
//! graph** (Stanford Network Analysis Project, snap.stanford.edu), the edge
//! lists the Datalog community actually benchmarks on. We invent no data and
//! no query: the graph is a published file (fetched by
//! `scripts/fetch-bench-data.sh`), and the program is the textbook TC.
//!
//! Everything runs through the public front door (`Db::run_script`), so the
//! identical file measures any engine revision.
//!
//! Usage: bench_tc <edge-list-file> [full|count]
//!   count (default) -- ?[count(x)] := tc[x,y]   full fixpoint, 1-row output
//!   full            -- ?[x,y]      := tc[x,y]    materializes every closure row
//!
//! SNAP edge-list format: lines of `<from>\t<to>`; `#` comment lines skipped.
//!
//! Prints one machine-readable line to stdout:
//!   TC graph=<name> edges=<n> nodes=<n> variant=<> load_ms=<f64>
//!      query_ms=<f64> closure_rows=<usize> peak_rss_kb=<u64>

use std::collections::BTreeMap;
use std::time::Instant;

use kyzo::{Db, new_fjall_storage};

const LOAD_CHUNK_ROWS: usize = 5_000;

fn no_params() -> BTreeMap<String, kyzo::DataValue> {
    BTreeMap::new()
}

/// Peak resident set size (VmHWM) in kB — the honest memory high-water mark.
fn peak_rss_kb() -> u64 {
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

/// Parse a SNAP edge list: `<from>\t<to>` per line, `#` comments skipped.
fn read_snap_edges(path: &str) -> (Vec<(i64, i64)>, usize) {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let mut edges = Vec::new();
    let mut nodes = std::collections::BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        let (a, b) = (it.next(), it.next());
        if let (Some(a), Some(b)) = (a, b) {
            let a: i64 = a.parse().expect("from-node");
            let b: i64 = b.parse().expect("to-node");
            edges.push((a, b));
            nodes.insert(a);
            nodes.insert(b);
        }
    }
    (edges, nodes.len())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: bench_tc <edge-list-file> [full|count]");
    let variant = args.next().unwrap_or_else(|| "count".to_string());
    let name = std::path::Path::new(&path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("graph")
        .to_string();

    let (edges, nodes) = read_snap_edges(&path);

    let tmp = tempfile::tempdir().expect("tempdir");
    let storage = new_fjall_storage(tmp.path()).expect("open fjall storage");
    let db = Db::new(storage).expect("open db");

    // ── load ────────────────────────────────────────────────────────────
    let t_load = Instant::now();
    let lit = |rows: &[(i64, i64)]| -> String {
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

    // ── query: canonical transitive closure ─────────────────────────────
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
        other => panic!("unknown variant {other}"),
    };

    let t_query = Instant::now();
    let result = db.run_script(q, no_params()).expect("closure query");
    let query_ms = t_query.elapsed().as_secs_f64() * 1000.0;

    let closure_rows = match variant.as_str() {
        "full" => result.rows.len(),
        "count" => result.rows[0][0].get_int().expect("count int") as usize,
        _ => unreachable!(),
    };

    println!(
        "TC graph={name} edges={} nodes={nodes} variant={variant} \
         load_ms={load_ms:.1} query_ms={query_ms:.1} closure_rows={closure_rows} \
         peak_rss_kb={}",
        edges.len(),
        peak_rss_kb(),
    );
}
