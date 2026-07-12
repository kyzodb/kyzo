/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Determinism campaign probe (issue #30): one seeded workload, driven
//! entirely through the public `Db::run_script` surface, whose canonical
//! ANSWERS must be byte-identical no matter how many rayon threads ran it,
//! how many times it has run before, or which CPU architecture executed it.
//!
//! `query/trials.rs` and `query/time_travel_trials.rs` already prove
//! cross-thread determinism for the in-process evaluator using its
//! `pub(crate)` seams; `ra_determinism.rs` proves it for the batched RA path
//! over `bench_api`'s read-only graph workloads. None of the three commits a
//! MUTATION history (multi-instant `@` writes, a retraction) through the
//! real `fjall` backend, and none touches `Interval` (story #62's newest
//! memcmp tag) at all. This probe closes both gaps, and does it through the
//! same public API surface a user drives — no internal seam, so it can be
//! linked against `kyzo` from any target this workspace's toolchain reaches,
//! including a foreign architecture.
//!
//! The workload: a plain graph relation (recursion + aggregation), a
//! bitemporal relation written and retracted across several validity
//! instants then read as-of three of them, and an `Interval`-typed relation.
//! Every query's result rows (headers, count, and every value's `Debug`
//! rendering, in RETURNED order — order itself is part of the claim) feed
//! one running hash, printed as a single `COMBINED` line the driver diffs
//! across thread counts, repeated runs, and architectures.
//!
//! **On-disk state, honestly scoped.** `::merkle_root` (the cold,
//! content-addressed hash over the whole ordered keyspace,
//! `storage/merkle.rs`) is run too, and its root is printed — but it is
//! deliberately EXCLUDED from `COMBINED`. Every stored fact's key carries a
//! real system-clock timestamp (`storage.md`: "every fact key ends with TWO
//! fixed-width slots"), minted from wall-clock time and never
//! script-settable (`time_travel_script_laws.rs`'s module doc: "the system
//! coordinate stays engine-minted... with no script syntax able to touch
//! it"). Two runs of this exact workload therefore commit genuinely
//! different bytes no matter how correct the engine is — folding the root
//! into `COMBINED` would make the campaign cry wolf on every single
//! invocation, training reviewers to ignore it (the flapping-threshold
//! failure this project's own benchmark policy warns against). A true
//! byte-for-byte on-disk-state comparison across independent runs needs a
//! script-visible, injectable clock, which does not exist in the public
//! surface today; that is this campaign's one named, structural boundary,
//! not a harness bug papered over. What IS asserted here is architecture-
//! and thread-count-invariance of `::merkle_root`'s own scan-and-hash
//! machinery not panicking and returning a well-formed root — a real, if
//! narrower, check — while the byte-identity claim rides entirely on the
//! projected query answers, which never surface the system-time coordinate.
//!
//! This binary is intentionally a single, simple execution — thread-count
//! and repetition are axes the *driver* varies (`RAYON_NUM_THREADS`, and
//! re-invoking the process), exactly like `ra_determinism.rs`. The
//! architecture axis is the same trick one level up: run this binary on two
//! architectures and diff the `COMBINED` line (`scripts/determinism-campaign.sh`,
//! wired into CI on both `ubuntu-latest` and `ubuntu-24.04-arm`).
//!
//! Run: `RAYON_NUM_THREADS=N cargo run -p kyzo --release --example determinism_digest`

use std::hash::{Hash, Hasher};

use kyzo::{DataValue, Db, NamedRows, Storage, new_fjall_storage};

/// No script below takes a bound parameter — every value is an inline
/// literal — so every call site hands `run_script` the same empty map.
fn no_params() -> std::collections::BTreeMap<String, DataValue> {
    std::collections::BTreeMap::new()
}

/// Rows in RETURNED order — no sorting. Order itself is part of the claim:
/// a batched path that answers the right SET in the wrong ROW ORDER under a
/// different thread count is exactly the bug this campaign exists to catch.
fn hash_named_rows(rows: &NamedRows) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    rows.headers.hash(&mut h);
    rows.rows.len().hash(&mut h);
    for row in &rows.rows {
        for v in row {
            format!("{v:?}").hash(&mut h);
        }
    }
    h.finish()
}

/// Run one script, fold its result hash into `combined`, and print the
/// per-query hash (so a CI failure names exactly which query diverged
/// instead of just the final combined line).
fn run(db: &Db<impl Storage>, script: &str, tag: &str, combined: &mut impl Hasher) -> NamedRows {
    let rows = db.run_script(script, no_params()).unwrap_or_else(|e| {
        panic!("determinism probe script failed ({tag}): {e}\nscript: {script}")
    });
    let h = hash_named_rows(&rows);
    println!("{tag:<24} rows={:<4} hash={h:016x}", rows.rows.len());
    tag.hash(combined);
    h.hash(combined);
    rows
}

fn main() {
    let threads = std::env::var("RAYON_NUM_THREADS").unwrap_or_else(|_| "default".into());
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::new(new_fjall_storage(dir.path()).expect("storage")).expect("db");

    let mut combined = std::collections::hash_map::DefaultHasher::new();

    // ── Graph relation: recursion + aggregation ────────────────────────
    run(
        &db,
        "?[a, b] <- [[1,2],[2,3],[3,4],[4,2],[2,5],[5,6],[6,3],[7,7]] :create edge {a => b}",
        "edge/create",
        &mut combined,
    );
    run(
        &db,
        "path[x, y] := *edge[x, y]\n\
         path[x, y] := path[x, z], *edge[z, y]\n\
         ?[x, y] := path[x, y]",
        "edge/transitive-closure",
        &mut combined,
    );
    run(
        &db,
        "?[a, count(b)] := *edge[a, b]",
        "edge/count-by-source",
        &mut combined,
    );
    run(
        &db,
        "?[mn, mx] := mn = min(a), mx = max(b), *edge[a, b]",
        "edge/min-max",
        &mut combined,
    );

    // ── Bitemporal relation: multi-instant writes + a retraction ───────
    run(
        &db,
        "?[k, v] <- [] :create hist {k: Int => v: Any}",
        "hist/create",
        &mut combined,
    );
    run(
        &db,
        "?[k, v] <- [[1,'a'],[2,'b']] :put hist {k => v} @ 100",
        "hist/put@100",
        &mut combined,
    );
    run(
        &db,
        "?[k, v] <- [[1,'a2'],[3,'c']] :put hist {k => v} @ 200",
        "hist/put@200",
        &mut combined,
    );
    run(
        &db,
        "?[k] <- [[2]] :rm hist {k} @ 250",
        "hist/rm@250",
        &mut combined,
    );
    run(
        &db,
        "?[k, v] <- [[1,'a3']] :put hist {k => v} @ 300",
        "hist/put@300",
        &mut combined,
    );
    for at in [150, 250, 350] {
        run(
            &db,
            &format!("?[k, v] := *hist{{k, v @ {at}}}"),
            &format!("hist/asof@{at}"),
            &mut combined,
        );
    }

    // ── Interval relation: story #62's newest memcmp tag ────────────────
    run(
        &db,
        "?[k, iv] <- [[1, make_interval(10, 20)], [2, make_interval(5, 15)], \
         [3, make_interval(15, 15000000000)]] :create ivrel {k => iv}",
        "ivrel/create",
        &mut combined,
    );
    run(
        &db,
        "?[k, iv] := *ivrel[k, iv]",
        "ivrel/scan",
        &mut combined,
    );

    // ── On-disk state: printed, NOT folded into `combined` (see module doc
    // — every committed key carries a real wall-clock system timestamp, so
    // the root legitimately differs run-to-run no matter how correct the
    // engine is). Still executed and printed: a panic or malformed root
    // here is a real finding, and the value is there for a human to eyeball
    // across two runs of the SAME process (where wall-clock content really
    // should differ) versus two thread-count variants of the SAME script
    // run back-to-back in CI (informative, not asserted).
    let root = db
        .run_script("::merkle_root", no_params())
        .expect("merkle_root");
    let root_hex = match root.rows.first().and_then(|r| r.first()) {
        Some(DataValue::Str(s)) => s.to_string(),
        other => panic!("::merkle_root returned an unexpected shape: {other:?}"),
    };
    println!("merkle_root              (informational, not in COMBINED) = {root_hex}");

    println!("THREADS={threads} COMBINED={:016x}", combined.finish());
}
