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
    assert!(before_any.rows.is_empty(), "nothing existed yet at 50");

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
    assert!(after.rows.is_empty(), "removed by 250");
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

    let out = db
        .run_script(
            "?[v, istart, iend] := *hist{id, v @spans iv}, \
             istart = interval_start(iv), iend = interval_end(iv) \
             :order istart",
            no_params(),
        )
        .expect("spans");
    assert_eq!(out.rows.len(), 3, "three maximal runs, one per correction");
    let got: Vec<(String, i64, i64)> = out
        .rows
        .iter()
        .map(|r| {
            (
                r[0].get_str().unwrap().to_string(),
                r[1].get_int().unwrap(),
                r[2].get_int().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("a".to_string(), 100, 200),
            ("b".to_string(), 200, 300),
            ("c".to_string(), 300, i64::MAX),
        ],
        "runs [100,200) 'a', [200,300) 'b', [300, MAX) 'c'"
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
        .rows
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
