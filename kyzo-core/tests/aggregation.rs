/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Story #88: aggregation, grouped and global, through the real public
//! API — `count`/`sum`/`min`/`max`/`mean`/`collect`, plus the retraction
//! hard case (`:rm`-ing the row that currently holds the group's `min`
//! and confirming a fresh query recomputes it, not a stale cached value).

mod common;
use common::*;

/// Wrapping a head variable in an aggregation function replaces GROUP BY;
/// grouping is implicit over the bare head variables.
#[test]
fn grouped_count_sum_min_max_mean() {
    let db = fresh_db();
    db.run_script(
        "?[dept, salary] <- [['math', 100], ['math', 200], ['math', 300], ['compsci', 500]] \
         :create emp {dept, salary}",
        no_params(),
    )
    .expect("create emp");

    let out = db
        .run_script(
            "?[dept, count(salary), sum(salary), min(salary), max(salary), mean(salary)] := \
             *emp{dept, salary}",
            no_params(),
        )
        .expect("grouped aggregation");
    let mut got: Vec<(String, i64, i64, i64, i64, f64)> = out
        .rows
        .iter()
        .map(|r| {
            (
                r[0].get_str().unwrap().to_string(),
                r[1].get_int().unwrap(),
                r[2].get_int().unwrap(),
                r[3].get_int().unwrap(),
                r[4].get_int().unwrap(),
                r[5].get_float().unwrap(),
            )
        })
        .collect();
    got.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        got,
        vec![
            ("compsci".to_string(), 1, 500, 500, 500, 500.0),
            ("math".to_string(), 3, 600, 100, 300, 200.0),
        ]
    );
}

/// No bare head variable at all: a single global aggregate over the whole
/// relation.
#[test]
fn global_aggregation() {
    let db = fresh_db();
    db.run_script(
        "?[x] <- [[10], [20], [30], [40]] :create nums {x}",
        no_params(),
    )
    .expect("create nums");

    let out = db
        .run_script(
            "?[count(x), sum(x), min(x), max(x)] := *nums{x}",
            no_params(),
        )
        .expect("global aggregation");
    assert_eq!(out.rows.len(), 1);
    assert_eq!(out.rows[0][0].get_int(), Some(4));
    assert_eq!(out.rows[0][1].get_int(), Some(100));
    assert_eq!(out.rows[0][2].get_int(), Some(10));
    assert_eq!(out.rows[0][3].get_int(), Some(40));
}

/// `collect` gathers every group member into a list — checked by sorting
/// the collected list, since collection order isn't part of the contract
/// we're pinning here.
#[test]
fn collect_aggregation() {
    let db = fresh_db();
    db.run_script(
        "?[dept, name] <- [['math', 'Ada'], ['math', 'Alan'], ['compsci', 'Grace']] \
         :create emp {dept, name}",
        no_params(),
    )
    .expect("create emp");

    let out = db
        .run_script("?[dept, collect(name)] := *emp{dept, name}", no_params())
        .expect("collect");
    let mut got: Vec<(String, Vec<String>)> = out
        .rows
        .iter()
        .map(|r| {
            let dept = r[0].get_str().unwrap().to_string();
            let mut names: Vec<String> = r[1]
                .get_slice()
                .unwrap()
                .iter()
                .map(|v| v.get_str().unwrap().to_string())
                .collect();
            names.sort();
            (dept, names)
        })
        .collect();
    got.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        got,
        vec![
            ("compsci".to_string(), vec!["Grace".to_string()]),
            (
                "math".to_string(),
                vec!["Ada".to_string(), "Alan".to_string()]
            ),
        ]
    );
}

/// The retraction hard case: `min(y)` for a group currently sits on one
/// particular row; `:rm`-ing exactly that row must make the NEXT query
/// recompute the true new minimum from what remains, not keep serving a
/// stale value (there is no incremental "subtract from running min").
#[test]
fn min_recomputes_after_retracting_the_current_minimum() {
    let db = fresh_db();
    db.run_script(
        "?[x, y] <- [[1, 10], [1, 20], [1, 30]] :create p {x, y}",
        no_params(),
    )
    .expect("create p");

    let out = db
        .run_script("?[x, min(y)] := *p{x, y}", no_params())
        .expect("initial min");
    assert_eq!(out.rows[0][1].get_int(), Some(10), "min starts at 10");

    // Retract the row holding the current min (10).
    db.run_script("?[x, y] <- [[1, 10]] :rm p {x, y}", no_params())
        .expect("rm the min row");

    let out = db
        .run_script("?[x, min(y)] := *p{x, y}", no_params())
        .expect("recomputed min");
    assert_eq!(
        out.rows[0][1].get_int(),
        Some(20),
        "min must recompute to 20 after 10 is gone, not stay stale at 10"
    );

    // Retract the new min too, leaving only 30 — a single-row group.
    db.run_script("?[x, y] <- [[1, 20]] :rm p {x, y}", no_params())
        .expect("rm the new min row");
    let out = db
        .run_script("?[x, min(y)] := *p{x, y}", no_params())
        .expect("recomputed min again");
    assert_eq!(out.rows[0][1].get_int(), Some(30));

    // Retract the last row: the group itself must vanish, not report
    // min of nothing.
    db.run_script("?[x, y] <- [[1, 30]] :rm p {x, y}", no_params())
        .expect("rm the last row");
    let out = db
        .run_script("?[x, min(y)] := *p{x, y}", no_params())
        .expect("empty relation");
    assert!(out.rows.is_empty(), "an empty group must vanish entirely");
}
