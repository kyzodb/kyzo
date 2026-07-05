/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Story #88: vector and full-text search as ordinary relational-algebra
//! operators — `::hnsw create` + a `~rel:idx{...}` k-NN atom (plain and
//! filtered), `::fts create` + a search atom (plain and composed inside a
//! join) — all through `Db::run_script`, the same public entry point a
//! user calls.

mod common;
use common::*;

/// Plain k-NN: three 2-D points, nearest-first to `[1.0, 0.0]`.
#[test]
fn hnsw_knn_search() {
    let db = fresh_db();
    db.run_script(
        "?[id, v] <- [[1, vec([1.0, 0.0])], [2, vec([0.0, 1.0])], [3, vec([0.9, 0.1])]] \
         :create doc {id => v: <F32; 2>}",
        no_params(),
    )
    .expect("create doc");
    db.run_script(
        "::hnsw create doc:emb {fields: [v], dim: 2, m: 16, ef_construction: 32, distance: L2}",
        no_params(),
    )
    .expect("hnsw create");

    let out = db
        .run_script(
            "?[id, dist] := ~doc:emb{id | query: vec([1.0, 0.0]), k: 3, bind_distance: dist} \
             :sort dist",
            no_params(),
        )
        .expect("knn");
    assert_eq!(ints(&out, 0), vec![1, 3, 2], "nearest-first order");
}

/// A k-NN search atom composed with an ordinary filter on the bound
/// distance column, exactly like any other relation: only the point
/// within 0.1 squared-L2 distance survives.
#[test]
fn hnsw_filtered_knn() {
    let db = fresh_db();
    db.run_script(
        "?[id, v] <- [[1, vec([1.0, 0.0])], [2, vec([0.0, 1.0])], [3, vec([0.9, 0.1])]] \
         :create doc {id => v: <F32; 2>}",
        no_params(),
    )
    .expect("create doc");
    db.run_script(
        "::hnsw create doc:emb {fields: [v], dim: 2, m: 16, ef_construction: 32, distance: L2}",
        no_params(),
    )
    .expect("hnsw create");

    let out = db
        .run_script(
            "?[id, dist] := ~doc:emb{id | query: vec([1.0, 0.0]), k: 3, bind_distance: dist}, \
             dist < 0.1",
            no_params(),
        )
        .expect("filtered knn");
    // Squared L2: id 1 is an exact match (dist 0.0); id 3 is
    // 0.1^2 + 0.1^2 = 0.02, still under 0.1; id 2 is at squared distance
    // 1^2 + 1^2 = 2.0, filtered out.
    let mut ids = ints(&out, 0);
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec![1, 3],
        "only ids 1 and 3 are within squared-L2 0.1"
    );
}

/// A k-NN search atom joined to an ordinary stored relation on the
/// matched id — search results unify like any other relation.
#[test]
fn hnsw_knn_joined_to_a_relation() {
    let db = fresh_db();
    db.run_script(
        "?[id, v] <- [[1, vec([1.0, 0.0])], [2, vec([0.0, 1.0])], [3, vec([0.9, 0.1])]] \
         :create doc {id => v: <F32; 2>}",
        no_params(),
    )
    .expect("create doc");
    db.run_script(
        "::hnsw create doc:emb {fields: [v], dim: 2, m: 16, ef_construction: 32, distance: L2}",
        no_params(),
    )
    .expect("hnsw create");
    db.run_script(
        "?[id, label] <- [[1, 'north'], [2, 'east'], [3, 'north-ish']] :create label {id => label}",
        no_params(),
    )
    .expect("create label");

    let out = db
        .run_script(
            "?[label, dist] := ~doc:emb{id | query: vec([1.0, 0.0]), k: 1, bind_distance: dist}, \
             *label{id, label}",
            no_params(),
        )
        .expect("knn joined to label");
    assert_eq!(out.rows.len(), 1);
    assert_eq!(out.rows[0][0].get_str(), Some("north"));
}

/// Plain FTS search: one document actually contains the query term.
#[test]
fn fts_search_basic() {
    let db = fresh_db();
    db.run_script(
        "?[id, body] <- [[1, 'the quick brown fox'], [2, 'lazy dogs sleep']] \
         :create doc {id => body: String}",
        no_params(),
    )
    .expect("create doc");
    db.run_script(
        "::fts create doc:txt {extractor: body, tokenizer: Simple}",
        no_params(),
    )
    .expect("fts create");

    let out = db
        .run_script("?[id] := ~doc:txt{id | query: 'fox', k: 5}", no_params())
        .expect("fts search");
    assert_eq!(ints(&out, 0), vec![1]);
}

/// FTS composed inside a join: the search atom shares `id` with an
/// ordinary stored relation in the same rule body.
#[test]
fn fts_composed_in_join() {
    let db = fresh_db();
    db.run_script(
        "?[id, body] <- [[1, 'the quick brown fox'], [2, 'lazy dogs sleep'], \
         [3, 'another fox story']] :create doc {id => body: String}",
        no_params(),
    )
    .expect("create doc");
    db.run_script(
        "::fts create doc:txt {extractor: body, tokenizer: Simple}",
        no_params(),
    )
    .expect("fts create");
    db.run_script(
        "?[id, author] <- [[1, 'ada'], [2, 'bob'], [3, 'cid']] :create authored {id => author}",
        no_params(),
    )
    .expect("create authored");

    let out = db
        .run_script(
            "?[author] := ~doc:txt{id | query: 'fox', k: 5}, *authored{id, author}",
            no_params(),
        )
        .expect("fts joined to authored");
    let mut authors = strs(&out, 0);
    authors.sort();
    assert_eq!(authors, vec!["ada", "cid"], "both fox documents' authors");
}
