/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Story #88: time travel through the real public API — `:create ... @
//! t`, `:put ... @ t`, an as-of read (`*rel{.. @ t}`), interval derivation
//! (`@spans iv`, read out via the public `interval_start`/`interval_end`
//! KyzoScript functions since `Interval`'s own Rust accessors are
//! crate-internal — exactly the boundary an external embedder sits at
//! too), and the axis-parameterized net diff (`@delta(lo, hi) sgn`).

mod common;
use common::*;
use kyzo::DataValue;

/// The write side names its own valid instant; the read side seeks to
/// what held at a chosen past instant — an ordinary seek, not a
/// reconstruction.
#[test]
fn as_of_read_across_corrections() {
    let db = fresh_db();
    db.run_script(
        "?[id, price] <- [[1, 100]] :create quote {id => price} @ 100",
        no_params(),
    )
    .expect("create at 100");
    db.run_script(
        "?[id, price] <- [[1, 150]] :put quote {id => price} @ 200",
        no_params(),
    )
    .expect("correction at 200");
    db.run_script(
        "?[id, price] <- [[1, 175]] :put quote {id => price} @ 300",
        no_params(),
    )
    .expect("correction at 300");

    let before_any = db
        .run_script("?[price] := *quote{id, price @ 50}", no_params())
        .expect("as-of before creation");
    assert!(before_any.rows().is_empty(), "nothing existed yet at 50");

    let at_150 = db
        .run_script("?[price] := *quote{id, price @ 150}", no_params())
        .expect("as-of 150");
    assert_eq!(ints(&at_150, 0), vec![100]);

    let at_250 = db
        .run_script("?[price] := *quote{id, price @ 250}", no_params())
        .expect("as-of 250");
    assert_eq!(ints(&at_250, 0), vec![150]);

    let at_now = db
        .run_script("?[price] := *quote{id, price}", no_params())
        .expect("current read");
    assert_eq!(
        ints(&at_now, 0),
        vec![175],
        "an unqualified read is as-of now"
    );
}

/// `:rm` at a later valid instant closes the current run instead of
/// simply vanishing from history: an as-of read before the removal still
/// sees the value, and after it sees nothing.
#[test]
fn as_of_read_survives_a_later_removal() {
    let db = fresh_db();
    db.run_script(
        "?[id, v] <- [[1, 'first']] :create rec {id => v} @ 100",
        no_params(),
    )
    .expect("create at 100");
    db.run_script("?[id] <- [[1]] :rm rec {id} @ 200", no_params())
        .expect("remove at 200");

    let before = db
        .run_script("?[v] := *rec{id, v @ 150}", no_params())
        .expect("as-of before removal");
    assert_eq!(strs(&before, 0), vec!["first"]);

    let after = db
        .run_script("?[v] := *rec{id, v @ 250}", no_params())
        .expect("as-of after removal");
    assert!(after.rows().is_empty(), "removed by 250");
}

/// `@spans iv`: one output row per maximal equal-payload run along the
/// valid axis. Three writes to the same key produce three runs;
/// `interval_start`/`interval_end` (public KyzoScript functions) read the
/// bounds back out without needing `Interval`'s crate-internal Rust
/// accessors.
#[test]
fn spans_derives_maximal_runs() {
    let db = fresh_db();
    db.run_script(
        "?[id, v] <- [[1, 'a']] :create hist {id => v} @ 100",
        no_params(),
    )
    .expect("create at 100");
    db.run_script(
        "?[id, v] <- [[1, 'b']] :put hist {id => v} @ 200",
        no_params(),
    )
    .expect("correction at 200");
    db.run_script(
        "?[id, v] <- [[1, 'c']] :put hist {id => v} @ 300",
        no_params(),
    )
    .expect("correction at 300");

    // The final run is genuinely OPEN: `interval_end` returns Null, not a
    // sentinel. i64::MAX is a finite instant, not infinity; the value plane
    // exposes real unboundedness, so the scripting surface must too.
    let out = db
        .run_script(
            "?[v, istart, iend, has_end, end_unbounded] := *hist{id, v @spans iv}, \
             istart = interval_start(iv), iend = interval_end(iv), \
             has_end = interval_has_end(iv), \
             end_unbounded = interval_is_end_unbounded(iv) \
             :order istart",
            no_params(),
        )
        .expect("spans");
    assert_eq!(out.rows().len(), 3, "three maximal runs, one per correction");
    let got: Vec<(String, i64, DataValue, bool, bool)> = out
        .rows()
        .iter()
        .map(|r| {
            (
                r[0].get_str().unwrap().to_string(),
                r[1].get_int().unwrap(),
                r[2].clone(),
                r[3].get_bool().unwrap(),
                r[4].get_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            // #119 intervals are CLOSED on the discrete grid: a run
            // corrected at 200 last holds at instant 199, so interval_end
            // is 199 (the last included instant), not the exclusive 200.
            ("a".to_string(), 100, DataValue::from(199i64), true, false),
            ("b".to_string(), 200, DataValue::from(299i64), true, false),
            // The open run: end is Null (no upper endpoint), and the
            // topology predicates confirm it is genuinely unbounded.
            ("c".to_string(), 300, DataValue::Null, false, true),
        ],
        "runs [100,199] 'a', [200,299] 'b', [300, ∞) 'c' — the last is open, \
         so interval_end is Null and interval_is_end_unbounded is true"
    );
}

/// The i64::MAX instant is RESERVED as the legacy `@'END'` / open sentinel,
/// so no user write can name it: a real validity fact at i64::MAX is
/// unrepresentable. Enforced at every user-facing construction path
/// (`ValidityTs::for_assertion`, reached by both the `@ <ts>` parser
/// coordinate and the per-row mutation loop). This is the public-API proof.
#[test]
fn user_cannot_assert_a_fact_at_the_reserved_end_instant() {
    let db = fresh_db();
    db.run_script("?[id, v] <- [[1, 'a']] :create res {id => v}", no_params())
        .expect("create res");
    // i64::MAX = 9223372036854775807 — the reserved terminal tick.
    let err = db
        .run_script(
            "?[id, v] <- [[1, 'b']] :put res {id => v} @ 9223372036854775807",
            no_params(),
        )
        .expect_err("asserting a fact at the reserved END instant must be refused");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("terminal tick") || msg.contains("END") || msg.contains("i64::MAX"),
        "refusal must name the reserved terminal tick, got: {msg}"
    );
}

/// The sentinel does not leak: across a full `@spans` derivation with an
/// open run, NO `interval_end` value is ever i64::MAX. An open end is Null;
/// a finite end is strictly below the reserved tick. The old sentinel model
/// (i64::MAX standing for "forever") cannot reappear at the query surface.
#[test]
fn end_sentinel_never_leaks_through_interval_end() {
    let db = fresh_db();
    db.run_script(
        "?[id, v] <- [[1, 'a']] :create leak {id => v} @ 100",
        no_params(),
    )
    .expect("create at 100");
    db.run_script(
        "?[id, v] <- [[1, 'b']] :put leak {id => v} @ 200",
        no_params(),
    )
    .expect("correct at 200");
    let out = db
        .run_script(
            "?[iend] := *leak{id, v @spans iv}, iend = interval_end(iv)",
            no_params(),
        )
        .expect("spans");
    assert!(
        out.rows().len() >= 2,
        "at least the clipped run and the open run"
    );
    for r in out.rows() {
        match &r[0] {
            DataValue::Null => {} // the open run — correct
            other @ (DataValue::Bool(_)
                | DataValue::Num(_)
                | DataValue::Str(_)
                | DataValue::Bytes(_)
                | DataValue::Uuid(_)
                | DataValue::Regex(_)
                | DataValue::Json(_)
                | DataValue::Vector(_)
                | DataValue::List(_)
                | DataValue::Set(_)
                | DataValue::Validity(_)
                | DataValue::Interval(_)
                | DataValue::Geometry(_)) => {
                assert_ne!(
                    other.get_int(),
                    Some(i64::MAX),
                    "interval_end leaked the reserved END sentinel as a real value"
                );
            }
        }
    }
    // And exactly one open run (the last), so Null actually occurred.
    let nulls = out
        .rows()
        .iter()
        .filter(|r| matches!(r[0], DataValue::Null))
        .count();
    assert_eq!(
        nulls, 1,
        "the single open run's end must be Null, not a sentinel"
    );
}

/// `@delta(lo, hi) sgn`: the axis-parameterized net diff between two
/// valid-time coordinates. `sgn` binds `+1` for a row present at `hi` but
/// not `lo`, `-1` for one present at `lo` but not `hi` — a correction
/// shows up as BOTH (the old value retracted, the new value asserted),
/// and an unrelated key that never changed in the window contributes no
/// row at all.
#[test]
fn delta_reports_signed_net_change() {
    let db = fresh_db();
    // k=1: created @100 with 'old', corrected @200 to 'new'.
    db.run_script(
        "?[k, v] <- [[1, 'old']] :create acct {k => v} @ 100",
        no_params(),
    )
    .expect("create at 100");
    db.run_script(
        "?[k, v] <- [[1, 'new']] :put acct {k => v} @ 200",
        no_params(),
    )
    .expect("correction at 200");
    // k=2: created @100, removed @150 — gone throughout the (120, 250)
    // window's endpoints, but present at 120 and absent at 250.
    db.run_script(
        "?[k, v] <- [[2, 'gone-soon']] :put acct {k => v} @ 100",
        no_params(),
    )
    .expect("k=2 created at 100");
    db.run_script("?[k] <- [[2]] :rm acct {k} @ 150", no_params())
        .expect("k=2 removed at 150");
    // k=3: created @100, never touched again — unchanged across the
    // whole (120, 250) window, must contribute nothing.
    db.run_script(
        "?[k, v] <- [[3, 'stable']] :put acct {k => v} @ 100",
        no_params(),
    )
    .expect("k=3 stable");

    let out = db
        .run_script(
            "?[k, v, sgn] := *acct{k, v @delta(120, 250) sgn}",
            no_params(),
        )
        .expect("delta");
    let mut got: Vec<(i64, String, i64)> = out
        .rows()
        .iter()
        .map(|r| {
            (
                r[0].get_int().unwrap(),
                r[1].get_str().unwrap().to_string(),
                r[2].get_int().unwrap(),
            )
        })
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            (1, "new".to_string(), 1),
            (1, "old".to_string(), -1),
            (2, "gone-soon".to_string(), -1),
        ],
        "k=1 retracts its old value and asserts the new one; k=2 only \
         retracts (gone by 250); k=3 is untouched and contributes nothing"
    );
}
