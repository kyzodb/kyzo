/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Story #88: the catalog system ops a real embedder inspects a store
//! with — `::relations`, `::columns <rel>`, `::index create`/`::indices
//! <rel>`/`::index drop` — through `Db::run_script`, asserting on the
//! actual `NamedRows` headers and rows, not just "it didn't error".

mod common;
use common::*;

/// `::relations` lists every stored relation with its name and arity;
/// `::columns <rel>` lists one relation's columns and which are keys.
#[test]
fn relations_and_columns_listing() {
    let db = fresh_db();
    db.run_script(
        "?[id, name, age] <- [[1, 'Ada', 36]] :create person {id => name, age}",
        no_params(),
    )
    .expect("create person");
    db.run_script("?[a, b] <- [[1, 2]] :create edge {a, b}", no_params())
        .expect("create edge");

    let rels = db
        .run_script("::relations", no_params())
        .expect("::relations");
    assert_eq!(rels.headers(), vec!["name", "arity", "access_level"]);
    let mut names: Vec<String> = rels
        .rows()
        .iter()
        .map(|r| r[0].get_str().unwrap().to_string())
        .collect();
    names.sort();
    assert_eq!(names, vec!["edge", "person"]);
    for r in rels.rows() {
        let name = r[0].get_str().unwrap();
        let arity = r[1].get_int().unwrap();
        match name {
            "person" => assert_eq!(arity, 3, "id, name, age"),
            "edge" => assert_eq!(arity, 2, "a, b"),
            other => panic!("unexpected relation {other}"),
        }
    }

    let cols = db
        .run_script("::columns person", no_params())
        .expect("::columns");
    assert_eq!(cols.headers(), vec!["column", "is_key"]);
    let mut got: Vec<(String, bool)> = cols
        .rows()
        .iter()
        .map(|r| {
            (
                r[0].get_str().unwrap().to_string(),
                r[1].get_bool().unwrap(),
            )
        })
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            ("age".to_string(), false),
            ("id".to_string(), true),
            ("name".to_string(), false),
        ]
    );
}

/// `::index create` builds a plain secondary index over a non-key
/// column, `::indices <rel>` lists it back by name and kind, and
/// `::index drop` removes it — verified by round-tripping through all
/// three, not just checking each op returns `Ok`.
#[test]
fn index_create_list_and_drop() {
    let db = fresh_db();
    db.run_script(
        "?[id, v] <- [[1, 10], [2, 20], [3, 10]] :create t {id => v}",
        no_params(),
    )
    .expect("create t");

    db.run_script("::index create t:by_v {v}", no_params())
        .expect("::index create");

    let idxs = db
        .run_script("::indices t", no_params())
        .expect("::indices");
    assert_eq!(idxs.headers(), vec!["name", "kind"]);
    let names: Vec<String> = idxs
        .rows()
        .iter()
        .map(|r| r[0].get_str().unwrap().to_string())
        .collect();
    assert!(
        names.contains(&"by_v".to_string()),
        "by_v must be listed, got {names:?}"
    );
    let kind = idxs
        .rows()
        .iter()
        .find(|r| r[0].get_str() == Some("by_v"))
        .map(|r| r[1].get_str().unwrap().to_string())
        .unwrap();
    assert_eq!(kind, "plain");

    // The index doesn't change ordinary query results — same rows either
    // way, it's an acceleration structure, not a second source of truth.
    let out = db
        .run_script("?[id] := *t{id, v}, v = 10 :order id", no_params())
        .expect("scan using the indexed column");
    assert_eq!(ints(&out, 0), vec![1, 3]);

    db.run_script("::index drop t:by_v", no_params())
        .expect("::index drop");
    let idxs_after = db
        .run_script("::indices t", no_params())
        .expect("::indices after drop");
    let names_after: Vec<String> = idxs_after
        .rows()
        .iter()
        .map(|r| r[0].get_str().unwrap().to_string())
        .collect();
    assert!(
        !names_after.contains(&"by_v".to_string()),
        "by_v must be gone after drop"
    );

    // Data itself is untouched by the drop.
    let out = db
        .run_script("?[id] := *t{id, v}, v = 10 :order id", no_params())
        .expect("scan after drop");
    assert_eq!(ints(&out, 0), vec![1, 3]);
}
