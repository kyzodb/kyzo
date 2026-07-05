/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Story #88: the relational core, exercised the way a user actually would
//! — `:create`/`:put`/`:rm`/`:replace`/`:ensure`, a join between two
//! relations, comparison and `in` filters, constants folded into a rule,
//! and `:order`/`:limit`/`:offset` — through the real public API
//! (`kyzo::Db::run_script`), one fresh store per test, every expected
//! value hand-computed from the literal rows written in the same test.

mod common;
use common::*;

/// `:create` + a filtered projection over a scanned relation — the exact
/// shape `examples/language_tour.rs` chapter 1 teaches, re-verified here
/// with its own numbers so this file stands alone.
#[test]
fn create_put_project_and_filter() {
    let db = fresh_db();
    db.run_script(
        "?[id, name, age] <- [[1, 'Ada', 36], [2, 'Grace', 34], [3, 'Alan', 41]] \
         :create person {id => name, age}",
        no_params(),
    )
    .expect("create person");

    let out = db
        .run_script("?[name] := *person{name, age}, age > 35", no_params())
        .expect("age > 35");
    let mut names = strs(&out, 0);
    names.sort();
    assert_eq!(names, vec!["Ada", "Alan"]);
}

/// A join is two body atoms sharing a variable — no JOIN keyword. Also
/// covers a three-way join (person ⋈ works_in ⋈ office).
#[test]
fn multi_relation_join() {
    let db = fresh_db();
    db.run_script(
        "?[id, name] <- [[1, 'Ada'], [2, 'Grace'], [3, 'Alan']] :create person {id => name}",
        no_params(),
    )
    .expect("create person");
    db.run_script(
        "?[id, dept] <- [[1, 'math'], [2, 'compsci'], [3, 'math']] :create works_in {id => dept}",
        no_params(),
    )
    .expect("create works_in");
    db.run_script(
        "?[dept, room] <- [['math', 101], ['compsci', 202]] :create office {dept => room}",
        no_params(),
    )
    .expect("create office");

    let out = db
        .run_script(
            "?[name, room] := *person{id, name}, *works_in{id, dept}, *office{dept, room}",
            no_params(),
        )
        .expect("three-way join");
    let mut got: Vec<(String, i64)> = out
        .rows
        .iter()
        .map(|r| (r[0].get_str().unwrap().to_string(), r[1].get_int().unwrap()))
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            ("Ada".to_string(), 101),
            ("Alan".to_string(), 101),
            ("Grace".to_string(), 202),
        ]
    );
}

/// `>`, `<`, `==`, and `in` filters, all in one body, over hand-picked
/// rows whose pass/fail is obvious by inspection.
#[test]
fn filter_operators() {
    let db = fresh_db();
    db.run_script(
        "?[id, score] <- [[1, 10], [2, 20], [3, 30], [4, 40], [5, 50]] :create m {id => score}",
        no_params(),
    )
    .expect("create m");

    let gt = db
        .run_script("?[id] := *m{id, score}, score > 25", no_params())
        .expect("gt");
    let mut got = ints(&gt, 0);
    got.sort_unstable();
    assert_eq!(got, vec![3, 4, 5], "score > 25 keeps ids 3,4,5");

    let lt = db
        .run_script("?[id] := *m{id, score}, score < 25", no_params())
        .expect("lt");
    let mut got = ints(&lt, 0);
    got.sort_unstable();
    assert_eq!(got, vec![1, 2], "score < 25 keeps ids 1,2");

    let eq = db
        .run_script("?[id] := *m{id, score}, score == 30", no_params())
        .expect("eq");
    assert_eq!(ints(&eq, 0), vec![3], "score == 30 keeps only id 3");

    let in_list = db
        .run_script("?[id] := *m{id, score}, score in [10, 30, 50]", no_params())
        .expect("in");
    let mut got = ints(&in_list, 0);
    got.sort_unstable();
    assert_eq!(got, vec![1, 3, 5], "in [10,30,50] keeps ids 1,3,5");
}

/// A constant row set (no stored relation at all) joined against a real
/// stored relation — constants are just another body atom.
#[test]
fn constants_join_stored_relation() {
    let db = fresh_db();
    db.run_script(
        "?[id, name] <- [[1, 'Ada'], [2, 'Grace'], [3, 'Alan']] :create person {id => name}",
        no_params(),
    )
    .expect("create person");

    let out = db
        .run_script(
            "wanted[id] <- [[1], [3]] \
             ?[name] := wanted[id], *person{id, name}",
            no_params(),
        )
        .expect("constants join");
    let mut names = strs(&out, 0);
    names.sort();
    assert_eq!(names, vec!["Ada", "Alan"]);
}

/// `:put` upserts a key, `:rm` removes it — verified by direct query
/// after each step, not by the shape of the write alone.
#[test]
fn put_then_rm() {
    let db = fresh_db();
    db.run_script(":create widget {id: Int => qty: Int}", no_params())
        .expect("create widget");
    db.run_script(
        "?[id, qty] <- [[1, 5], [2, 9]] :put widget {id, qty}",
        no_params(),
    )
    .expect("initial put");

    let out = db
        .run_script("?[id, qty] := *widget{id, qty}", no_params())
        .expect("scan after put");
    let mut got: Vec<(i64, i64)> = out
        .rows
        .iter()
        .map(|r| (r[0].get_int().unwrap(), r[1].get_int().unwrap()))
        .collect();
    got.sort_unstable();
    assert_eq!(got, vec![(1, 5), (2, 9)]);

    // A second :put on the same key overwrites the value, not adds a row.
    db.run_script("?[id, qty] <- [[1, 12]] :put widget {id, qty}", no_params())
        .expect("overwrite put");
    let out = db
        .run_script("?[qty] := *widget{id: 1, qty}", no_params())
        .expect("re-read id 1");
    assert_eq!(ints(&out, 0), vec![12], "put on existing key overwrites");

    // :rm removes the key entirely.
    db.run_script("?[id] <- [[2]] :rm widget {id}", no_params())
        .expect("rm id 2");
    let out = db
        .run_script("?[id] := *widget{id}", no_params())
        .expect("scan after rm");
    assert_eq!(ints(&out, 0), vec![1], "id 2 is gone after :rm");
}

/// `:replace` swaps the WHOLE relation's contents for the new rows — a
/// row from before that isn't in the new set must disappear, unlike
/// `:put` which only ever touches the keys it names.
#[test]
fn replace_swaps_whole_relation() {
    let db = fresh_db();
    db.run_script(
        "?[id, name] <- [[1, 'a'], [2, 'b'], [3, 'c']] :create tags {id => name}",
        no_params(),
    )
    .expect("create tags");

    db.run_script(
        "?[id, name] <- [[4, 'd'], [5, 'e']] :replace tags {id => name}",
        no_params(),
    )
    .expect("replace tags");

    let out = db
        .run_script("?[id] := *tags{id}", no_params())
        .expect("scan after replace");
    let mut got = ints(&out, 0);
    got.sort_unstable();
    assert_eq!(
        got,
        vec![4, 5],
        ":replace must leave ONLY the new rows — ids 1,2,3 must be gone"
    );
}

/// `:ensure` asserts a fact holds with an exact value (typed `Err` if the
/// key is absent, or present with a different value); `:ensure_not`
/// asserts a key's absence. Neither mutates.
#[test]
fn ensure_and_ensure_not() {
    let db = fresh_db();
    db.run_script(
        "?[id, bal] <- [[1, 100]] :create acct {id => bal}",
        no_params(),
    )
    .expect("create acct");

    // The exact current value: passes silently.
    db.run_script(
        "?[id, bal] <- [[1, 100]] :ensure acct {id => bal}",
        no_params(),
    )
    .expect(":ensure must pass when the value matches");

    // A different value at the same key: must refuse, typed, not panic.
    let err = db
        .run_script(
            "?[id, bal] <- [[1, 999]] :ensure acct {id => bal}",
            no_params(),
        )
        .expect_err(":ensure must refuse a value mismatch");
    assert!(!err.to_string().is_empty());

    // A key that was never written: must refuse, typed, not panic.
    let err = db
        .run_script(
            "?[id, bal] <- [[2, 1]] :ensure acct {id => bal}",
            no_params(),
        )
        .expect_err(":ensure must refuse a missing key");
    assert!(!err.to_string().is_empty());

    // :ensure_not on a genuinely absent key passes.
    db.run_script("?[id] <- [[2]] :ensure_not acct {id}", no_params())
        .expect(":ensure_not must pass on an absent key");

    // :ensure_not on a key that DOES exist must refuse.
    let err = db
        .run_script("?[id] <- [[1]] :ensure_not acct {id}", no_params())
        .expect_err(":ensure_not must refuse an existing key");
    assert!(!err.to_string().is_empty());

    // Neither op mutated anything: acct{1} is still 100.
    let out = db
        .run_script("?[bal] := *acct{id: 1, bal}", no_params())
        .expect("re-read");
    assert_eq!(
        ints(&out, 0),
        vec![100],
        ":ensure/:ensure_not must never mutate"
    );
}

/// `:order`/`:sort` (ascending default, `-col` descending), `:limit`,
/// `:offset` — composed together, over rows whose sorted order is
/// unambiguous by hand.
#[test]
fn sort_limit_offset() {
    let db = fresh_db();
    db.run_script(
        "?[id, score] <- [[1, 30], [2, 10], [3, 50], [4, 20], [5, 40]] :create m {id => score}",
        no_params(),
    )
    .expect("create m");

    let asc = db
        .run_script("?[id, score] := *m{id, score} :order score", no_params())
        .expect("ascending sort");
    assert_eq!(ints(&asc, 1), vec![10, 20, 30, 40, 50]);

    let desc = db
        .run_script("?[id, score] := *m{id, score} :order -score", no_params())
        .expect("descending sort");
    assert_eq!(ints(&desc, 1), vec![50, 40, 30, 20, 10]);

    let top2 = db
        .run_script(
            "?[id, score] := *m{id, score} :order -score :limit 2",
            no_params(),
        )
        .expect("top 2");
    assert_eq!(ints(&top2, 1), vec![50, 40]);

    let page2 = db
        .run_script(
            "?[id, score] := *m{id, score} :order -score :limit 2 :offset 2",
            no_params(),
        )
        .expect("page 2 (skip 2, take 2)");
    assert_eq!(page2.rows.len(), 2, "one page of 2 rows");
    assert_eq!(ints(&page2, 1), vec![30, 20], "third and fourth highest");
}
