/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Story #88: standing queries / incremental view maintenance — the seam
//! where a real bug slipped through (multiple real commits landing
//! between two `apply_pending` polls, all through `Db::register_standing`
//! and `Db::run_script` — the public API only). Every case here drives
//! SEVERAL commits (put/rm/value-update, including two touches of the
//! SAME key) before a single `apply_pending`, then checks the maintained
//! answer against a FRESH `run_script` recompute of the identical query
//! text — the real production evaluator, not a second incremental
//! registration — plus an explicit no-duplicate-key check.

// `DataValue` (inside `Tuple = Vec<DataValue>`) is used as a `BTreeSet`
// element throughout this file, exactly as `kyzo-core/src/lib.rs` itself
// notes for its own crate-wide allow: clippy's interior-mutability check
// is a false positive here (the `Regex`/cache internals it flags are
// never mutated through a shared reference), and that crate-level allow
// does not reach across the crate boundary into this external test
// binary, so it is repeated here.
#![allow(clippy::mutable_key_type)]

mod common;
use common::*;

use std::collections::BTreeSet;

use kyzo::{DataValue, Tuple};

/// A key-value relation behind a plain (non-aggregating) standing query:
/// between ONE poll, one key gets corrected (put twice with different
/// values), a second key gets asserted then retracted (nets to nothing),
/// and a third key is asserted fresh. The maintained answer must match a
/// fresh recompute exactly, and the corrected key must appear exactly
/// ONCE (not twice, old value alongside new).
#[test]
fn multiple_commits_between_one_poll_match_a_fresh_recompute() {
    let db = fresh_db();
    db.run_script(":create q {k: Int => v: Int}", no_params())
        .expect("create q");

    let query = "?[k, v] := *q[k, v]";
    let mut sq = db.register_standing(query, no_params()).expect("register");
    assert!(sq.current_answer().is_empty(), "nothing written yet");

    // Five real commits, all before the one poll below:
    db.run_script("?[k, v] <- [[1, 10]] :put q {k, v}", no_params())
        .expect("put k=1 v=10");
    db.run_script("?[k, v] <- [[1, 20]] :put q {k, v}", no_params())
        .expect("correct k=1 to v=20");
    db.run_script("?[k, v] <- [[2, 5]] :put q {k, v}", no_params())
        .expect("put k=2 v=5");
    db.run_script("?[k] <- [[2]] :rm q {k}", no_params())
        .expect("rm k=2 (nets to nothing)");
    db.run_script("?[k, v] <- [[3, 7]] :put q {k, v}", no_params())
        .expect("put k=3 v=7");

    let deltas = sq.apply_pending().expect("apply_pending");
    assert!(
        !deltas.is_empty(),
        "five real commits must produce a non-empty delta"
    );

    let maintained: BTreeSet<Tuple> = sq.current_answer().clone();
    let fresh: BTreeSet<Tuple> = db
        .run_script(query, no_params())
        .expect("fresh recompute")
        .rows
        .into_iter()
        .collect();
    assert_eq!(
        maintained, fresh,
        "the maintained answer after one multi-commit drain must equal a fresh recompute"
    );

    // No-duplicate-key check: k=1 must appear exactly once, with the
    // corrected value (20), never both 10 and 20.
    let k1_rows: Vec<&Tuple> = maintained
        .iter()
        .filter(|t| t[0].get_int() == Some(1))
        .collect();
    assert_eq!(
        k1_rows.len(),
        1,
        "key 1 must appear exactly once, got {k1_rows:?}"
    );
    assert_eq!(
        k1_rows[0][1].get_int(),
        Some(20),
        "key 1 must hold its corrected value"
    );

    // k=2 was asserted then retracted within the same drain: it must not
    // appear at all.
    assert!(
        !maintained.iter().any(|t| t[0].get_int() == Some(2)),
        "key 2 must be fully absent (assert-then-retract nets to nothing)"
    );

    sq.teardown();
}

/// The aggregation hard case, live: a standing query's grouped `min` must
/// survive an unrelated new group appearing AND correctly recompute when
/// the row holding the CURRENT min is retracted — both in the same
/// multi-commit drain before the one poll.
#[test]
fn standing_query_recomputes_min_after_retracting_it_mid_batch() {
    let db = fresh_db();
    db.run_script(":create p {x: Int, y: Int =>}", no_params())
        .expect("create p");
    db.run_script(
        "?[x, y] <- [[1, 10], [1, 20], [1, 30]] :put p {x, y}",
        no_params(),
    )
    .expect("seed x=1 with y in {10,20,30}");

    let query = "?[x, min(y)] := *p[x, y]";
    let mut sq = db.register_standing(query, no_params()).expect("register");
    let expected_initial: BTreeSet<Tuple> = [vec![DataValue::from(1), DataValue::from(10)].into()]
        .into_iter()
        .collect();
    assert_eq!(
        sq.current_answer().clone(),
        expected_initial,
        "initial min for x=1 is 10"
    );

    // In one drain: retract the current min (10), AND bring in a brand
    // new group (x=2).
    db.run_script("?[x, y] <- [[1, 10]] :rm p {x, y}", no_params())
        .expect("retract the current min");
    db.run_script("?[x, y] <- [[2, 5]] :put p {x, y}", no_params())
        .expect("a brand new group appears");

    sq.apply_pending().expect("apply_pending");

    let maintained: BTreeSet<Tuple> = sq.current_answer().clone();
    let fresh: BTreeSet<Tuple> = db
        .run_script(query, no_params())
        .expect("fresh recompute")
        .rows
        .into_iter()
        .collect();
    assert_eq!(
        maintained, fresh,
        "maintained min-aggregation must equal a fresh recompute after the retract-plus-new-group batch"
    );
    // x=1's min must have moved on to 20 (not stuck at the retracted 10,
    // and not vanished); x=2's new group must show its own min, 5.
    let mut got: Vec<(i64, i64)> = maintained
        .iter()
        .map(|t| (t[0].get_int().unwrap(), t[1].get_int().unwrap()))
        .collect();
    got.sort_unstable();
    assert_eq!(got, vec![(1, 20), (2, 5)]);

    sq.teardown();
}
