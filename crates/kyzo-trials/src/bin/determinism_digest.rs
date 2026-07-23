/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

#![forbid(unsafe_code)]

//! Determinism campaign probe (story #30): one seeded workload through the
//! public [`Engine::run_script`] surface. Canonical ANSWERS must be
//! byte-identical across rayon thread counts, repeated process runs, and
//! CPU architectures.
//!
//! Remade at the trials seat after museum cut `8ba3975` retired the
//! `kyzo-core/examples/determinism_digest.rs` corpse. Cap1
//! (`kyzo_trials::gauntlet`) proves in-process thread determinism; this
//! binary is the outward axis the driver
//! (`scripts/determinism-campaign.sh`) varies — `RAYON_NUM_THREADS` and
//! re-invocation — then diffs across architectures in CI.
//!
//! Workload: graph recursion + aggregation, bitemporal put/rm/as-of, and
//! an `Interval` relation. Every query's returned rows (headers, count,
//! each value's `Debug` rendering, in RETURNED order) feed one running
//! hash printed as `COMBINED`. The museum probe also printed
//! `::merkle_root` (informational, never folded into `COMBINED`); that
//! sys-op is no longer in the public grammar, so this remake omits it.
//!
//! Run: `RAYON_NUM_THREADS=N cargo run -p kyzo-trials --release --bin determinism_digest`

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};


use miette::{Result, miette};

use kyzo::{DataValue, Engine, FjallStorage, NamedRows};

fn no_params() -> BTreeMap<String, DataValue> {
    BTreeMap::new()
}

fn db() -> Result<Engine<FjallStorage>> {
    // One probe door with language_tour (copy_detector).
    Engine::compose_temp_fjall().map_err(|e| miette!("engine: {e}"))
}

/// Rows in RETURNED order — no sorting. Order itself is part of the claim.
fn hash_named_rows(rows: &NamedRows) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    rows.headers().hash(&mut h);
    rows.rows().len().hash(&mut h);
    for row in rows.rows() {
        for v in row {
            format!("{v:?}").hash(&mut h);
        }
    }
    h.finish()
}

fn run(
    db: &Engine<FjallStorage>,
    script: &str,
    tag: &str,
    combined: &mut impl Hasher,
) -> Result<NamedRows> {
    let rows = db.run_script(script, no_params()).map_err(|e| {
        miette!("determinism probe script failed ({tag}); script: {script}: {e}")
    })?;
    let h = hash_named_rows(&rows);
    println!("{tag:<24} rows={:<4} hash={h:016x}", rows.rows().len());
    tag.hash(combined);
    h.hash(combined);
    Ok(rows)
}

fn main() -> Result<()> {
    let threads = match std::env::var("RAYON_NUM_THREADS") { Ok(v) => v, Err(_) => String::from("default") };
    let db = db()?;
    let mut combined = std::collections::hash_map::DefaultHasher::new();

    // Composite key `{a, b}` — multi-edge graph (museum probe used `{a => b}`,
    // which collapses duplicate sources; that was a workload defect, not a
    // digest pin we must preserve).
    run(
        &db,
        "?[a, b] <- [[1,2],[2,3],[3,4],[4,2],[2,5],[5,6],[6,3],[7,7]] :create edge {a, b}",
        "edge/create",
        &mut combined,
    )?;
    run(
        &db,
        "path[x, y] := *edge[x, y]\n\
         path[x, y] := path[x, z], *edge[z, y]\n\
         ?[x, y] := path[x, y]",
        "edge/transitive-closure",
        &mut combined,
    )?;
    run(
        &db,
        "?[a, count(b)] := *edge[a, b]",
        "edge/count-by-source",
        &mut combined,
    )?;
    run(
        &db,
        "?[mn, mx] := mn = min(a), mx = max(b), *edge[a, b]",
        "edge/min-max",
        &mut combined,
    )?;

    run(
        &db,
        "?[k, v] <- [] :create hist {k: Int => v: Any}",
        "hist/create",
        &mut combined,
    )?;
    run(
        &db,
        "?[k, v] <- [[1,'a'],[2,'b']] :put hist {k => v} @ 100",
        "hist/put@100",
        &mut combined,
    )?;
    run(
        &db,
        "?[k, v] <- [[1,'a2'],[3,'c']] :put hist {k => v} @ 200",
        "hist/put@200",
        &mut combined,
    )?;
    run(
        &db,
        "?[k] <- [[2]] :rm hist {k} @ 250",
        "hist/rm@250",
        &mut combined,
    )?;
    run(
        &db,
        "?[k, v] <- [[1,'a3']] :put hist {k => v} @ 300",
        "hist/put@300",
        &mut combined,
    )?;
    for at in [150, 250, 350] {
        run(
            &db,
            &format!("?[k, v] := *hist{{k, v @ {at}}}"),
            &format!("hist/asof@{at}"),
            &mut combined,
        )?;
    }

    run(
        &db,
        "?[k, iv] <- [[1, make_interval(10, 20)], [2, make_interval(5, 15)], \
         [3, make_interval(15, 15000000000)]] :create ivrel {k => iv}",
        "ivrel/create",
        &mut combined,
    )?;
    run(
        &db,
        "?[k, iv] := *ivrel[k, iv]",
        "ivrel/scan",
        &mut combined,
    )?;

    println!("THREADS={threads} COMBINED={:016x}", combined.finish());
    Ok(())
}
