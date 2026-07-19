/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Story #88: refusal is a typed `Err`, never a panic — an unstratifiable
//! program, a recursive standing query, and a data-type coercion
//! mismatch, each driven through the real public API and checked for a
//! clean `Result::Err` (this file would abort the test process on a
//! panic, which is itself part of what's being verified).

mod common;
use common::*;

/// The classic textbook unstratifiable program: `p` depends on `not q`,
/// `q` depends on `not p` — a negation cycle with no valid stratum
/// order. The compiler must refuse this, typed, never mis-answer it.
#[test]
fn unstratifiable_negation_cycle_is_refused() {
    let db = fresh_db();
    db.run_script(":create base {x: Int =>}", no_params())
        .expect("create base");
    db.run_script("?[x] <- [[1], [2]] :put base {x}", no_params())
        .expect("seed base");

    let query = "p[x] := *base[x], not q[x]; \
                 q[x] := *base[x], not p[x]; \
                 ?[x] := p[x]";
    let err = db
        .run_script(query, no_params())
        .expect_err("a negation cycle must be refused, never silently mis-answered");
    assert!(
        err.to_string().to_lowercase().contains("stratif"),
        "expected a stratification refusal, got: {err}"
    );
}

/// `register_standing` needs a query whose fixpoint terminates without
/// runtime deltas — a genuinely recursive program (transitive closure)
/// is refused at registration time, typed, not silently wrong or a hang.
#[test]
fn recursive_standing_query_is_refused() {
    let db = fresh_db();
    db.run_script(":create edge {a: Int, b: Int =>}", no_params())
        .expect("create edge");

    let query = "path[a, b] := *edge[a, b]; \
                 path[a, b] := *edge[a, c], path[c, b]; \
                 ?[a, b] := path[a, b]";
    let err = match db.register_standing(query, no_params()) {
        Err(e) => e,
        Ok(_) => panic!("a recursive standing query must be refused, got a live registration"),
    };
    assert!(
        err.to_string().to_lowercase().contains("recursive"),
        "expected a recursion refusal, got: {err}"
    );

    // The same query runs FINE as an ordinary one-shot read — the
    // refusal is specific to the standing-query seam, not to recursion
    // in general.
    db.run_script("?[a, b] <- [[1, 2], [2, 3]] :put edge {a, b}", no_params())
        .expect("seed edge");
    let out = db
        .run_script(query, no_params())
        .expect("ordinary recursive read");
    let mut got: Vec<(i64, i64)> = out
        .rows()
        .iter()
        .map(|r| (r[0].get_int().unwrap(), r[1].get_int().unwrap()))
        .collect();
    got.sort_unstable();
    assert_eq!(got, vec![(1, 2), (1, 3), (2, 3)]);
}

/// A column's declared type is a contract: a `String` value can't coerce
/// into an `Int` column, and the mutation must refuse typed, leaving the
/// relation untouched — never a panic, never a silently truncated value.
#[test]
fn bad_type_coercion_is_refused() {
    let db = fresh_db();
    db.run_script(":create t {id: Int => n: Int}", no_params())
        .expect("create t");
    db.run_script("?[id, n] <- [[1, 10]] :put t {id, n}", no_params())
        .expect("seed one good row");

    let err = db
        .run_script(
            "?[id, n] <- [[2, 'not-an-int']] :put t {id, n}",
            no_params(),
        )
        .expect_err("a String into an Int column must be refused");
    let full = format!("{err:?}").to_lowercase();
    assert!(
        full.contains("coerc") || full.contains("type"),
        "expected a type-coercion refusal, got: {full}"
    );

    // The bad row must never have landed.
    let out = db
        .run_script("?[id] := *t{id}", no_params())
        .expect("scan t");
    assert_eq!(ints(&out, 0), vec![1], "only the good row exists");

    // Same story for a vector column: the wrong element count must
    // refuse instead of silently padding/truncating.
    db.run_script(
        "?[id, v] <- [[1, vec([1.0, 2.0])]] :create vt {id => v: <F32; 2>}",
        no_params(),
    )
    .expect("create vt");
    let err = db
        .run_script(
            "?[id, v] <- [[2, vec([1.0, 2.0, 3.0])]] :put vt {id => v}",
            no_params(),
        )
        .expect_err("a 3-element vector into a <F32;2> column must be refused");
    assert!(!err.to_string().is_empty());
    let out = db
        .run_script("?[id] := *vt{id}", no_params())
        .expect("scan vt");
    assert_eq!(ints(&out, 0), vec![1], "the mis-shaped vector never landed");
}
