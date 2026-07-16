/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Time-travel LANGUAGE-surface laws (story #4): the as-of claim proven
 * through the actual public surface a user drives — `Db::run_script`
 * parsing real KyzoScript `@` clauses — rather than through hand-built
 * magic-program ASTs.
 *
 * `time_travel_trials.rs` already proves the full compile→RA→eval path is
 * semantically correct, but it deliberately reconstructs that path's
 * `pub(crate)` seams by hand (see its module doc) and never calls
 * `Db::run_script`. `db.rs::asof_clause_parses_one_and_two_coordinates`
 * proves the `@` clause PARSES and RUNS through the public surface, but
 * checks nothing about the VALUES it returns, and reads only a single fact
 * written at "now". Neither test differences a real, multi-entity,
 * multi-transaction as-of history against an oracle through parsed
 * KyzoScript text. This module is that missing layer.
 *
 * THE GAP THIS MODULE ONCE DOCUMENTED IS FIXED: `:put`/`:rm` now carry the
 * same `@` clause the read side does (`relation_option`'s grammar grew a
 * trailing `validity_clause?`, `parse/query.rs`), restricted at parse time
 * to exactly one coordinate — the valid instant; the system coordinate
 * stays engine-minted (`SessionTx::system_stamp_routed`) unconditionally,
 * with no script syntax able to touch it. `@ <constant>` resolves once,
 * for every row the mutation writes (`data::program::WriteValidity::Fixed`);
 * `@ <output-column-name>` resolves per row, exactly like any other column
 * (`WriteValidity::PerRow`) — the backfill/import shape, where each row
 * carries its own timestamp. See `runtime/mutate.rs`'s write-coordinate
 * comments and `parse/query.rs`'s `resolve_write_validity` for the
 * mechanism; `parse::query::tests` holds the parser-level proof (the
 * syntax parses, the two-coordinate and non-write-op forms are refused).
 *
 * This module now builds its ENTIRE seeded history through pure KyzoScript
 * — `Db::run_script` calls carrying `@ <ts>` clauses — with no internal-API
 * backdoor. (`RelationHandle::put_fact`/`retract_fact` remain a legitimate
 * `pub(crate)` test seam used elsewhere, e.g. `time_travel_trials.rs`; they
 * are simply unnecessary here now that the write side has its own public
 * surface, so this file no longer reaches for them.) One "transaction" is
 * one `db.run_script` call: each event in the seeded history becomes its
 * own script, executed in strict generation order, so same-instant
 * collisions resolve exactly as the oracle computes them (last write, in
 * event order, wins) — see `write_transaction`.
 *
 * HOSTILE-REVIEW FINDING, FIXED: a first cut of write-side `@` derived
 * every write path's "row this write supersedes" (index/trigger old-row
 * collection, `:update`'s carried-forward non-key columns, `:insert`'s
 * existence guard) from `current_row_routed`'s CURRENT belief
 * (`AsOf::current(MAX_VALIDITY_TS)`) — correct before `@` existed, because
 * every write's `valid` was always `stamp`, so "current" and "the instant
 * being written" were the same coordinate by construction. A historical
 * `@ <ts>` breaks that identity: writing at an OLDER instant than the
 * relation's current belief must supersede whatever governed THAT older
 * instant, not the unrelated newer one. `current_row_routed`
 * (`runtime/db.rs`) now takes the probe's `valid` explicitly; the three
 * write paths in `runtime/mutate.rs` pass their own resolved
 * `WriteValidity` coordinate (byte-identical to the old "current" behavior
 * whenever `valid == stamp`, i.e. every `@`-less script, since `stamp` is
 * always at or past any instant an ordinary history could contain);
 * `:ensure`/`:ensure_not` (which can never carry `@`) pass
 * `MAX_VALIDITY_TS` unconditionally, preserving their exact prior meaning.
 * `historical_correction_via_put_stays_consistent_with_its_index`,
 * `historical_update_carries_forward_the_targeted_instants_own_value`, and
 * `historical_insert_checks_existence_at_its_own_instant_not_current`
 * pin the three failure modes the review found.
 */
#![cfg(test)]

use std::collections::BTreeSet;

use crate::data::value::{DataValue, Tuple};
use crate::query::laws;
use crate::runtime::db::Db;
use crate::storage::Storage;
use crate::storage::fjall::{FjallStorage, new_fjall_storage};
use crate::storage::sim::SimRng;

fn no_params() -> std::collections::BTreeMap<String, DataValue> {
    std::collections::BTreeMap::new()
}

// ─────────────────────────────────────────────────────────────────────────
// One event in the synthetic history, and the seeded generator.
// ─────────────────────────────────────────────────────────────────────────

/// One version write: entity, its valid instant, and its polarity. Values
/// are globally unique per assert (`format!("v{version_ctr}")`) so a probe result
/// pins exactly which generation is visible, not merely whether one is.
/// Assert owns its value; retract carries none — a freestanding
/// `is_assert` beside an optional `val` is unrepresentable.
#[derive(Clone, Debug)]
enum Event {
    Assert {
        entity: i64,
        ts: i64,
        val: String,
    },
    Retract {
        entity: i64,
        ts: i64,
    },
}

impl Event {
    fn entity(&self) -> i64 {
        match self {
            Event::Assert { entity, .. } | Event::Retract { entity, .. } => *entity,
        }
    }

    fn ts(&self) -> i64 {
        match self {
            Event::Assert { ts, .. } | Event::Retract { ts, .. } => *ts,
        }
    }

    fn is_assert(&self) -> bool {
        matches!(self, Event::Assert { .. })
    }
}

/// A seeded, wall-clock-free pseudo-random history: `n_events` writes over
/// `n_entities` entities, grouped into transactions of `chunk` events each
/// (so the history is interleaved across several commits, not one). Each
/// event either advances the running clock by a small random step or (to
/// exercise same-instant collisions across independent entities) repeats
/// the previous timestamp. An entity currently live is retracted or
/// superseded; a dead one is (usually) asserted, occasionally hit with a
/// redundant retract to prove that's a harmless no-op.
fn seeded_history(seed: u64, n_entities: i64, n_events: usize, chunk: usize) -> Vec<Vec<Event>> {
    let mut rng = SimRng::new(seed);
    let mut ts = 10i64;
    let mut alive: std::collections::BTreeMap<i64, bool> = std::collections::BTreeMap::new();
    let mut version_ctr = 0u64;
    let mut all = Vec::with_capacity(n_events);
    for _ in 0..n_events {
        // 25% chance: repeat the last timestamp (same-instant collision
        // across entities); otherwise advance by 10, 20, or 30.
        if all.is_empty() || rng.below(4) != 0 {
            ts += 10 * (1 + rng.below(3) as i64);
        }
        let entity = 1 + rng.below(n_entities as u64) as i64;
        let is_live = *alive.get(&entity).unwrap_or(&false);
        let is_assert = if is_live {
            // 70% supersede-with-new-value (still an assert), 30% retract.
            rng.below(10) < 7
        } else {
            // 90% assert-from-dead, 10% redundant retract (no-op, robustness).
            rng.below(10) < 9
        };
        alive.insert(entity, is_assert);
        all.push(if is_assert {
            version_ctr += 1;
            Event::Assert {
                entity,
                ts,
                val: format!("v{version_ctr}"),
            }
        } else {
            Event::Retract { entity, ts }
        });
    }
    all.chunks(chunk).map(|c| c.to_vec()).collect()
}

/// Write one chunk's worth of events against a real relation over a real
/// `Db<FjallStorage>`, through PURE KYZOSCRIPT — one `db.run_script` call
/// per event, each carrying an explicit `@ <ts>` clause, executed in
/// strict generation order. One script call is one committed transaction,
/// so this reproduces the oracle's "last write in event order wins at a
/// shared (entity, ts) coordinate" exactly: each call's system stamp is
/// strictly newer than every call before it (`storage::SystemClock` is
/// monotone), in the same order the oracle folds `events` into its
/// `collapsed` map. Returns the number of scripts run (transactions
/// committed), for the anti-vacuity check.
fn write_transaction(
    db: &Db<FjallStorage>,
    rel_name: &str,
    events: &[Event],
    first: bool,
) -> usize {
    if first {
        db.run_script(
            &format!(":create {rel_name} {{k0: Int => val: Any}}"),
            no_params(),
        )
        .expect("create relation");
    }
    for ev in events {
        let script = match ev {
            Event::Assert { entity, ts, val } => format!(
                "?[k0, val] <- [[{entity}, '{val}']] :put {rel_name} {{k0 => val}} @ {ts}"
            ),
            Event::Retract { entity, ts } => {
                format!("?[k0] <- [[{entity}]] :rm {rel_name} {{k0}} @ {ts}")
            }
        };
        db.run_script(&script, no_params())
            .unwrap_or_else(|e| panic!("write script `{script}` failed: {e}"));
    }
    events.len()
}

// ─────────────────────────────────────────────────────────────────────────
// The naive oracle: obviously correct, no indexes, no engine machinery.
// ─────────────────────────────────────────────────────────────────────────

/// Same (entity, ts) pair written more than once (possibly across
/// transactions): the LAST one in write/commit order governs — the one
/// stored key, last write wins. Then, per entity, the newest surviving
/// instant at or before `at` (inclusive) governs; an assertion emits
/// `(entity, val)`, a retraction emits nothing.
///
/// Routed through the UNIFIED temporal oracle (story #62,
/// `query::laws::resolve_relation`) instead of a bespoke collapse-then-
/// group algorithm: each event becomes a `laws::Event` at its own
/// `(entity, ts)` valid coordinate, with write order riding the SYSTEM
/// axis (`sys = list index`) — "last write in write order governs" is
/// exactly `laws::resolve`'s "newest system version at or before
/// `sys_at` governs," with `sys_at` fixed at "see everything."
/// `boundary_inclusive = false` (the sabotaged form used only by
/// [`asof_script_boundary_mutation_is_caught`]) probes `at - 1` instead
/// of `at`, excluding the queried instant — still the one real
/// resolution function, just a deliberately wrong coordinate.
fn oracle_at(events: &[Event], at: i64, boundary_inclusive: bool) -> BTreeSet<(i64, String)> {
    let history: Vec<laws::Event> = events
        .iter()
        .enumerate()
        .map(|(i, ev)| {
            // Timestamps here are small generated/fixture values: never the
            // reserved terminal tick.
            match ev {
                Event::Assert { entity, ts, val } => laws::Event::assert(
                    Tuple::from_vec(vec![DataValue::from(*entity)]),
                    Tuple::from_vec(vec![DataValue::from(val.clone())]),
                    *ts,
                    i as i64,
                )
                .expect("event timestamps in this file are never the reserved terminal tick"),
                Event::Retract { entity, ts } => {
                    laws::Event::retract(
                        Tuple::from_vec(vec![DataValue::from(*entity)]),
                        *ts,
                        i as i64,
                    )
                    .expect("event timestamps in this file are never the reserved terminal tick")
                }
            }
        })
        .collect();
    let probe_at = if boundary_inclusive { at } else { at - 1 };
    laws::resolve_relation(
        &history,
        laws::AsOf {
            valid: probe_at,
            sys: i64::MAX,
        },
    )
    .into_iter()
    .map(|row| {
        let entity = row[0].get_int().expect("int key");
        let val = match &row[1] {
            DataValue::Str(s) => s.to_string(),
            other => panic!("expected a string value, got {other:?}"),
        };
        (entity, val)
    })
    .collect()
}

// ─────────────────────────────────────────────────────────────────────────
// Bridge differential (story #62): `oracle_at`'s unified-oracle encoding,
// checked against a FROM-SCRATCH reference over hundreds of seeded random
// event sequences and both boundary configurations.
// ─────────────────────────────────────────────────────────────────────────

/// An independent brute-force reference for `oracle_at`'s rule, written
/// without reusing any part of it, old or new: for every entity, scan
/// every event linearly and keep the one the window-and-tiebreak rule
/// picks (newest ts in the probe window; ties broken by list position,
/// last write governing).
fn independent_oracle_at_reference(
    events: &[Event],
    at: i64,
    boundary_inclusive: bool,
) -> BTreeSet<(i64, String)> {
    let mut best: std::collections::BTreeMap<i64, (i64, usize, Event)> =
        std::collections::BTreeMap::new();
    for (i, ev) in events.iter().enumerate() {
        let in_window = if boundary_inclusive {
            ev.ts() <= at
        } else {
            ev.ts() < at
        };
        if !in_window {
            continue;
        }
        let candidate = (ev.ts(), i, ev.clone());
        match best.entry(ev.entity()) {
            std::collections::btree_map::Entry::Vacant(e) => {
                e.insert(candidate);
            }
            std::collections::btree_map::Entry::Occupied(mut e) => {
                let cur = e.get();
                let better = if candidate.0 != cur.0 {
                    candidate.0 > cur.0
                } else {
                    candidate.1 > cur.1
                };
                if better {
                    e.insert(candidate);
                }
            }
        }
    }
    best.into_iter()
        .filter_map(|(entity, (_, _, ev))| match ev {
            Event::Assert { val, .. } => Some((entity, val)),
            Event::Retract { .. } => None,
        })
        .collect()
}

/// A random event sequence over a handful of entities and small,
/// often-colliding timestamps, mixing asserts and retracts.
fn gen_events(rng: &mut SimRng, n_entities: i64, n_events: usize) -> Vec<Event> {
    let mut version_ctr = 0u64;
    (0..n_events)
        .map(|_| {
            let entity = 1 + rng.below(n_entities as u64) as i64;
            let ts = rng.below(8) as i64;
            if rng.below(10) < 7 {
                version_ctr += 1;
                Event::Assert {
                    entity,
                    ts,
                    val: format!("v{version_ctr}"),
                }
            } else {
                Event::Retract { entity, ts }
            }
        })
        .collect()
}

/// The bridge: `oracle_at` (now backed by `laws::resolve_relation`)
/// against the from-scratch reference above, over hundreds of generated
/// event sequences, every probed instant, and both boundary
/// configurations.
#[test]
fn oracle_at_matches_an_independent_reference_generatively() {
    let mut cases = 0usize;
    for seed in 0..300u64 {
        let mut rng = SimRng::new(0xACE0_ACE0_u64 ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let n_entities = 1 + rng.below(4) as i64;
        let n_events = 1 + rng.below(20) as usize;
        let events = gen_events(&mut rng, n_entities, n_events);
        for at in -1..=9 {
            for boundary_inclusive in [true, false] {
                let got = oracle_at(&events, at, boundary_inclusive);
                let want = independent_oracle_at_reference(&events, at, boundary_inclusive);
                assert_eq!(
                    got, want,
                    "seed {seed} at={at} boundary_inclusive={boundary_inclusive}: \
                     events={events:?}"
                );
                cases += 1;
            }
        }
    }
    assert!(cases > 500, "expected a rich bridge campaign, ran {cases}");
}

/// Every distinct timestamp in the history, plus one before the first and
/// one after the last, plus the midpoint of every adjacent pair with a gap
/// (strictly between): before-all, at-every-event, between-every-pair,
/// after-all.
fn probe_instants(events: &[Event]) -> Vec<i64> {
    let mut ts: Vec<i64> = events.iter().map(|e| e.ts()).collect();
    ts.sort_unstable();
    ts.dedup();
    let mut out = vec![];
    if let Some(&first) = ts.first() {
        out.push(first - 1);
    }
    for w in ts.windows(2) {
        out.push(w[0]);
        if w[1] - w[0] >= 2 {
            out.push((w[0] + w[1]) / 2);
        }
    }
    if let Some(&last) = ts.last() {
        out.push(last);
        out.push(last + 1);
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Run the as-of query for `rel_name` at `at` through `Db::run_script` —
/// real KyzoScript text, the real parser, the real compiler and evaluator —
/// and return the rows as `(k0, val)` pairs, sorted.
fn engine_asof(db: &Db<FjallStorage>, rel_name: &str, at: i64) -> BTreeSet<(i64, String)> {
    let script = format!("?[k0, val] := *{rel_name}{{k0, val @ {at}}}");
    let rows = db
        .run_script(&script, no_params())
        .expect("as-of script runs");
    rows.rows
        .into_iter()
        .map(|r| {
            let k0 = r[0].get_int().expect("k0 is an int");
            let val = match &r[1] {
                DataValue::Str(s) => s.to_string(),
                other => panic!("expected a string value, got {other:?}"),
            };
            (k0, val)
        })
        .collect()
}

/// Run the CURRENT (no `@` clause) read through `Db::run_script`.
fn engine_current(db: &Db<FjallStorage>, rel_name: &str) -> BTreeSet<(i64, String)> {
    let script = format!("?[k0, val] := *{rel_name}{{k0, val}}");
    let rows = db
        .run_script(&script, no_params())
        .expect("plain script runs");
    rows.rows
        .into_iter()
        .map(|r| {
            let k0 = r[0].get_int().expect("k0 is an int");
            let val = match &r[1] {
                DataValue::Str(s) => s.to_string(),
                other => panic!("expected a string value, got {other:?}"),
            };
            (k0, val)
        })
        .collect()
}

// ═════════════════════════════════════════════════════════════════════════
// The law: a real, seeded, multi-transaction history, differenced against
// the naive oracle at every interesting instant, through real KyzoScript —
// written AND read entirely through `Db::run_script`.
// ═════════════════════════════════════════════════════════════════════════

#[test]
fn asof_script_matches_naive_oracle_over_seeded_history() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap();

    let history = seeded_history(0xC0FFEE_u64, 6, 60, 5);
    let all_events: Vec<Event> = history.iter().flatten().cloned().collect();

    // Anti-vacuity: a nontrivial number of events, across several commits.
    assert!(
        all_events.len() >= 40,
        "expected a rich history, got {} events",
        all_events.len()
    );

    let mut transactions = 0usize;
    for (i, chunk) in history.iter().enumerate() {
        transactions += write_transaction(&db, "hist", chunk, i == 0);
    }
    assert!(
        transactions >= 40,
        "expected several real script-driven transactions, got {transactions}"
    );

    let probes = probe_instants(&all_events);
    assert!(
        probes.len() >= 40,
        "expected many probe timestamps, got {}",
        probes.len()
    );

    let mut nonempty = 0usize;
    let mut answers: BTreeSet<Vec<(i64, String)>> = BTreeSet::new();
    for &at in &probes {
        let expected = oracle_at(&all_events, at, true);
        let got = engine_asof(&db, "hist", at);
        assert_eq!(got, expected, "as-of mismatch at instant {at}");
        if !expected.is_empty() {
            nonempty += 1;
        }
        answers.insert(expected.into_iter().collect());
    }
    // Anti-vacuity: most probes see a nonempty population, and the answer
    // set actually changes across the timeline — else this harness could
    // pass by every probe returning the same (possibly empty) thing.
    assert!(
        nonempty * 2 >= probes.len(),
        "expected at least half the probes nonempty, got {nonempty} of {}",
        probes.len()
    );
    assert!(
        answers.len() >= 10,
        "expected the as-of answer to change across many probes, saw only {} distinct answers",
        answers.len()
    );

    // The no-`@`-clause read is CURRENT state: it must equal the oracle at
    // (or beyond) the last event's instant — the newest believed claim per
    // entity, exactly like the storage-level as-of tests' plain scan.
    let last_ts = all_events.iter().map(|e| e.ts()).max().unwrap();
    assert_eq!(
        engine_current(&db, "hist"),
        oracle_at(&all_events, last_ts, true),
        "a plain (no `@`) read must equal the oracle at the final instant"
    );
}

/// Mutation-proves the harness is boundary-sensitive: flip the oracle's
/// at-instant boundary from inclusive to exclusive on a real assert event,
/// read through a real `@` script, and require the sabotaged reference to
/// DISAGREE with the engine. If it did not, this suite's differentials
/// could pass with a backwards boundary and nobody would notice.
#[test]
fn asof_script_boundary_mutation_is_caught() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap();

    let history = seeded_history(0x5EED_u64, 6, 60, 5);
    let all_events: Vec<Event> = history.iter().flatten().cloned().collect();
    for (i, chunk) in history.iter().enumerate() {
        write_transaction(&db, "hist2", chunk, i == 0);
    }

    // Pick an exact assert-event timestamp to probe at.
    let assert_ts = all_events
        .iter()
        .find(|e| e.is_assert())
        .expect("history has at least one assert")
        .ts();

    let engine = engine_asof(&db, "hist2", assert_ts);
    let correct = oracle_at(&all_events, assert_ts, true);
    let sabotaged = oracle_at(&all_events, assert_ts, false);
    assert_eq!(engine, correct, "engine must match the inclusive oracle");
    assert_ne!(
        engine, sabotaged,
        "an exclusive-boundary oracle must disagree with the engine at instant {assert_ts} \
         — else the differential is blind to the boundary"
    );
}

/// The two-coordinate `@ system, valid` form, through real KyzoScript: a
/// correction (a second write at the SAME valid instant, in a LATER
/// transaction) is invisible at the earlier transaction's system stamp and
/// governs from its own stamp on — the bitemporal flagship, proven through
/// `Db::run_script` for BOTH the write (`@ <valid>` on `:put`) and the read
/// (`@ <system>, <valid>`) rather than a hand-built `AsOf`/`put_fact`.
///
/// The two writes' system stamps (`s1`, `s2`) are read off
/// `Storage::clock_floor` immediately after each commits — a public,
/// engine-owned watermark (used for real by restore/dump), not a fact-write
/// backdoor: nothing here ever sets a system coordinate, which stays
/// entirely out of script reach by design.
#[test]
fn two_coordinate_asof_script_sees_the_record_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap();

    db.run_script(":create corr {k0: Int => val: Any}", no_params())
        .expect("create relation");

    db.run_script(
        "?[k0, val] <- [[1, 'original']] :put corr {k0 => val} @ 100",
        no_params(),
    )
    .expect("first write");
    let s1 = db
        .storage
        .clock_floor()
        .expect("floor after first write")
        .raw();

    db.run_script(
        "?[k0, val] <- [[1, 'corrected']] :put corr {k0 => val} @ 100",
        no_params(),
    )
    .expect("second write");
    let s2 = db
        .storage
        .clock_floor()
        .expect("floor after second write")
        .raw();

    // `ValidityTs` wraps `Reverse`; the raw counter (`.raw()`) increases with
    // wall/logical time, so a later mint reads numerically LARGER here.
    assert!(s2 > s1, "system stamps strictly increase across commits");

    let read_at = |sys: i64, valid: i64| -> BTreeSet<(i64, String)> {
        let script = format!("?[k0, val] := *corr{{k0, val @ {sys}, {valid}}}");
        db.run_script(&script, no_params())
            .expect("two-coordinate as-of script runs")
            .rows
            .into_iter()
            .map(|r| {
                let k0 = r[0].get_int().expect("int");
                let val = match &r[1] {
                    DataValue::Str(s) => s.to_string(),
                    other => panic!("expected string, got {other:?}"),
                };
                (k0, val)
            })
            .collect()
    };

    assert_eq!(
        read_at(s1, 150),
        BTreeSet::from([(1, "original".to_string())]),
        "before the correction was recorded, the original governs"
    );
    assert_eq!(
        read_at(s2, 150),
        BTreeSet::from([(1, "corrected".to_string())]),
        "from the correction's stamp on, it governs"
    );
    assert_eq!(
        engine_current(&db, "corr"),
        BTreeSet::from([(1, "corrected".to_string())]),
        "current belief is the corrected claim"
    );
}

/// The per-row form (`@ <output-column-name>`): one `:put` statement, three
/// rows, each carrying its OWN valid instant in a `ts` column that is never
/// stored (the target relation's schema only has `k0`/`val`) — proving the
/// clause resolves per row, at RUNTIME, not merely that it parses that way
/// (`parse::query::tests::put_at_output_column_is_per_row` proves the
/// latter). Each row must be invisible strictly before its own instant and
/// visible at or after it, independent of the other rows' instants.
#[test]
fn per_row_at_clause_gives_each_row_its_own_valid_instant() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap();

    db.run_script(":create backfill {k0: Int => val: Any}", no_params())
        .expect("create relation");

    db.run_script(
        "?[k0, val, ts] <- [[1, 'a', 100], [2, 'b', 200], [3, 'c', 300]] \
         :put backfill {k0 => val} @ ts",
        no_params(),
    )
    .expect("per-row backfill put");

    assert_eq!(
        engine_asof(&db, "backfill", 99),
        BTreeSet::new(),
        "before the earliest row's own instant, nothing is visible yet"
    );
    assert_eq!(
        engine_asof(&db, "backfill", 100),
        BTreeSet::from([(1, "a".to_string())]),
        "row 1 lands exactly at its own instant, not the statement's"
    );
    assert_eq!(
        engine_asof(&db, "backfill", 200),
        BTreeSet::from([(1, "a".to_string()), (2, "b".to_string())]),
        "row 2 joins at its own, later instant; row 1 stays visible"
    );
    assert_eq!(
        engine_asof(&db, "backfill", 300),
        BTreeSet::from([
            (1, "a".to_string()),
            (2, "b".to_string()),
            (3, "c".to_string())
        ]),
        "all three rows visible once every row's own instant has passed"
    );
}

// ═════════════════════════════════════════════════════════════════════════
// Hostile-review pins: a historical `@` write must supersede whatever
// governed ITS OWN target instant — never an unrelated later belief.
// ═════════════════════════════════════════════════════════════════════════

/// A `:put @ <historical>` correcting an EARLIER instant than the
/// relation's current belief must retract the value it actually supersedes
/// at that instant and assert the new one — and a plain index attached to
/// the relation must agree with the base exactly, at every as-of
/// coordinate spanning the correction. Before the fix, index maintenance
/// compared against the CURRENT row (an unrelated later instant) instead
/// of whatever governed the instant being corrected, so the index kept
/// showing the superseded value as present.
#[test]
fn historical_correction_via_put_stays_consistent_with_its_index() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap();

    db.run_script(":create ereg {k0: Int => val: Any}", no_params())
        .expect("create");
    db.run_script("::index create ereg:byval {val}", no_params())
        .expect("index create");

    // Two historical versions of entity 1: valid=200 says 'C' (to be
    // corrected below), valid=500 says 'B' (the current belief, later and
    // unrelated to the correction).
    db.run_script(
        "?[k0, val] <- [[1, 'C']] :put ereg {k0 => val} @ 200",
        no_params(),
    )
    .expect("write at 200");
    db.run_script(
        "?[k0, val] <- [[1, 'B']] :put ereg {k0 => val} @ 500",
        no_params(),
    )
    .expect("write at 500");

    // The correction: instant 200 actually said 'A', not 'C'.
    db.run_script(
        "?[k0, val] <- [[1, 'A']] :put ereg {k0 => val} @ 200",
        no_params(),
    )
    .expect("correction at 200");

    // As of valid=300 (between the corrected instant and the unrelated
    // current one), the base must say 'A' — and the index must agree
    // exactly, both that 'A' is present and that 'C' is gone.
    let base = db
        .run_script("?[k0, val] := *ereg{k0, val @ 300}", no_params())
        .expect("base as-of read");
    assert_eq!(
        base.rows,
        vec![crate::data::value::Tuple::from_vec(vec![
            DataValue::from(1),
            DataValue::from("A"),
        ])],
        "the base must reflect the correction, not the unrelated valid=500 belief"
    );

    let idx = db
        .run_script("?[k0, val] := *ereg:byval{val, k0 @ 300}", no_params())
        .expect("index as-of read");
    assert_eq!(
        idx.rows, base.rows,
        "the index must agree with the base exactly at the same as-of coordinate"
    );

    // Explicitly: the superseded 'C' must not still be reachable through
    // the index as of the same instant.
    let idx_c = db
        .run_script(
            "?[k0] := *ereg:byval{val, k0 @ 300}, val = 'C'",
            no_params(),
        )
        .expect("index read for the superseded value");
    assert!(
        idx_c.rows.is_empty(),
        "the index must not still show the superseded 'C' as of instant 300: {idx_c:?}"
    );
}

/// `:update @ <historical>` names only some columns; the rest must carry
/// forward from whatever held AT THE TARGETED INSTANT — never from an
/// unrelated, later "current" belief. Before the fix, the carried value
/// came from `current_row_routed`'s CURRENT row, so a historical `:update`
/// could silently inject a future value into the past.
#[test]
fn historical_update_carries_forward_the_targeted_instants_own_value() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap();

    db.run_script(":create t2 {k0: Int => x: Any, y: Any}", no_params())
        .expect("create");

    // Historical truth at valid=100.
    db.run_script(
        "?[k0, x, y] <- [[1, 'X_old', 'Y_old']] :put t2 {k0 => x, y} @ 100",
        no_params(),
    )
    .expect("historical write");
    // A later, unrelated current truth at valid=500.
    db.run_script(
        "?[k0, x, y] <- [[1, 'X_new', 'Y_new']] :put t2 {k0 => x, y} @ 500",
        no_params(),
    )
    .expect("current write");

    // `:update` targets the HISTORICAL instant and names only `x`: `y`
    // must carry forward 'Y_old' (what held at valid=100), never 'Y_new'.
    db.run_script(
        "?[k0, x] <- [[1, 'X_old_corrected']] :update t2 {k0 => x} @ 100",
        no_params(),
    )
    .expect("historical update");

    let historical = db
        .run_script("?[x, y] := *t2{k0, x, y @ 150}", no_params())
        .expect("as-of read at 150")
        .rows;
    assert_eq!(
        historical,
        vec![crate::data::value::Tuple::from_vec(vec![
            DataValue::from("X_old_corrected"),
            DataValue::from("Y_old")
        ])],
        "the carried-forward `y` must be the value that held at the targeted instant"
    );

    // The later, unrelated current belief must be untouched.
    let current = db
        .run_script("?[x, y] := *t2{k0, x, y}", no_params())
        .expect("current read")
        .rows;
    assert_eq!(
        current,
        vec![crate::data::value::Tuple::from_vec(vec![
            DataValue::from("X_new"),
            DataValue::from("Y_new")
        ])],
        "a historical update must not perturb an unrelated later belief"
    );
}

/// `:insert @ <historical>` must check existence AT ITS OWN TARGET
/// INSTANT, not "does this key exist at all, ever": inserting a historical
/// fact for an entity that currently exists (via a later, unrelated write)
/// must succeed when nothing governed the historical instant yet, and must
/// still refuse a genuine duplicate at that same historical instant.
#[test]
fn historical_insert_checks_existence_at_its_own_instant_not_current() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap();

    db.run_script(":create t3 {k0: Int => val: Any}", no_params())
        .expect("create");

    // Entity 1 currently exists (asserted at valid=500).
    db.run_script(
        "?[k0, val] <- [[1, 'current']] :put t3 {k0 => val} @ 500",
        no_params(),
    )
    .expect("current write");

    // A historical insert at valid=100 — before anything governed that
    // instant — must succeed regardless of what exists now.
    db.run_script(
        "?[k0, val] <- [[1, 'backfilled']] :insert t3 {k0 => val} @ 100",
        no_params(),
    )
    .expect("historical insert must succeed: nothing existed at instant 100");

    // A second insert at the SAME historical instant is a genuine
    // duplicate at that coordinate and must be refused.
    let err = db
        .run_script(
            "?[k0, val] <- [[1, 'dup']] :insert t3 {k0 => val} @ 100",
            no_params(),
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("exists"),
        "expected an existence refusal, got: {err}"
    );
}
