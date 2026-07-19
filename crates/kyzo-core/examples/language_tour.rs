/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The KyzoScript language tour (story #73): every chapter runs a real
//! script through the real public `Engine::run_script` entry point and asserts
//! on its actual output — this file is not narration *about* the language,
//! it is a KyzoScript program that the engine executes every time `cargo
//! test`/`cargo run --example` touches it. A comment describing a construct
//! this file doesn't also exercise is a defect (CLAUDE.md: doc drift), so
//! every claim below is load-bearing.
//!
//! Progression, in the order a newcomer should learn it (also the order the
//! story asked for): relations → rules → recursion → aggregation → time
//! travel (`@`, both the read side and the new write side) → vector/FTS
//! search → a built-in graph algorithm. Run with:
//!
//!   cargo run -p kyzo --example language_tour
//!
//! `cargo test -p kyzo --example language_tour` also runs it (each chapter
//! is a `#[test]` as well as a `fn` `main` calls), so CI keeps every example
//! honest without a second copy of the scripts to drift from the first.

use std::collections::BTreeMap;

use kyzo::{Catalog, DataValue, Engine, FjallStorage, NamedRows, new_fjall_storage};

fn no_params() -> BTreeMap<String, DataValue> {
    BTreeMap::new()
}

/// A fresh store per chapter, backed by the real pure-Rust engine (not a
/// test-only in-memory stand-in) — this tour runs the same code path a real
/// embedder does. Leaks its tempdir on purpose: an example process is
/// short-lived, and every chapter needs its own store torn down only at
/// exit, not mid-run.
fn db() -> Engine<FjallStorage> {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = new_fjall_storage(dir.path()).expect("fjall storage");
    std::mem::forget(dir);
    Engine::compose(storage, Catalog::new()).expect("engine")
}

fn ints(rows: &NamedRows, col: usize) -> Vec<i64> {
    rows.rows()
        .iter()
        .map(|r| r[col].get_int().expect("int column"))
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────
// Chapter 1: relations. A relation is a named, schema'd table; `:create`
// declares it and seeds it in one script, `=>` separating the key columns
// from the dependent ones (the bitemporal key every row is addressed by).
// ─────────────────────────────────────────────────────────────────────────
fn chapter_1_relations() {
    let db = db();
    db.run_script(
        "?[id, name, age] <- [[1, 'Ada', 36], [2, 'Grace', 34], [3, 'Alan', 41]] \
         :create person {id => name, age}",
        no_params(),
    )
    .expect("create person");

    // Reading a stored relation is a body atom over its `*`-sigiled name —
    // no SELECT, no FROM: the rule head names the output columns directly.
    let out = db
        .run_script("?[name, age] := *person{name, age}, age > 35", no_params())
        .expect("scan person");
    let mut names: Vec<&str> = out.rows().iter().map(|r| r[0].get_str().unwrap()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["Ada", "Alan"], "age > 35 filters to Ada, Alan");
}

// ─────────────────────────────────────────────────────────────────────────
// Chapter 2: rules. A join is two body atoms sharing a variable — `id`
// here — not a JOIN keyword; a rule can be named and reused like a
// function.
// ─────────────────────────────────────────────────────────────────────────
fn chapter_2_rules() {
    let db = db();
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

    // `named_in_math` is an ordinary rule; the entry `?` calls it like any
    // other relation. The shared variable `id` between the two body atoms
    // IS the join.
    let out = db
        .run_script(
            "named_in_math[name] := *person{id, name}, *works_in{id, dept}, dept = 'math'; \
             ?[name] := named_in_math[name]",
            no_params(),
        )
        .expect("joined rule");
    let mut names: Vec<&str> = out.rows().iter().map(|r| r[0].get_str().unwrap()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["Ada", "Alan"], "Ada and Alan both work in math");
}

// ─────────────────────────────────────────────────────────────────────────
// Chapter 3: recursion. A rule that calls itself is a fixpoint, evaluated
// semi-naively to termination — this is Datalog's answer to SQL's
// `WITH RECURSIVE`, and it costs no extra syntax.
// ─────────────────────────────────────────────────────────────────────────
fn chapter_3_recursion() {
    let db = db();
    db.run_script(
        "?[fr, to] <- [['FRA', 'JFK'], ['JFK', 'LAX'], ['LAX', 'YPO'], ['FRA', 'CDG']] \
         :create route {fr, to}",
        no_params(),
    )
    .expect("create route");

    // Every airport reachable from FRA, any number of hops: a base case
    // plus a recursive case that calls its own rule.
    let out = db
        .run_script(
            "reachable[to] := *route{fr: 'FRA', to}; \
             reachable[to] := reachable[stop], *route{fr: stop, to}; \
             ?[to] := reachable[to]",
            no_params(),
        )
        .expect("transitive closure");
    let mut dests: Vec<&str> = out.rows().iter().map(|r| r[0].get_str().unwrap()).collect();
    dests.sort_unstable();
    assert_eq!(
        dests,
        vec!["CDG", "JFK", "LAX", "YPO"],
        "all of FRA's reach"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Chapter 4: aggregation. Wrapping a head variable in an aggregation
// function replaces SQL's GROUP BY — grouping is implicit over the
// variables left bare in the head.
// ─────────────────────────────────────────────────────────────────────────
fn chapter_4_aggregation() {
    let db = db();
    db.run_script(
        "?[dept, name] <- [['math', 'Ada'], ['math', 'Alan'], ['compsci', 'Grace']] \
         :create works_in {dept, name}",
        no_params(),
    )
    .expect("create works_in");

    // `dept` is bare (the grouping key); `count(name)` aggregates within
    // each group.
    let out = db
        .run_script("?[dept, count(name)] := *works_in{dept, name}", no_params())
        .expect("group + count");
    let mut counts: Vec<(String, i64)> = out
        .rows()
        .iter()
        .map(|r| (r[0].get_str().unwrap().to_string(), r[1].get_int().unwrap()))
        .collect();
    counts.sort_unstable();
    assert_eq!(
        counts,
        vec![("compsci".to_string(), 1), ("math".to_string(), 2)],
        "two in math, one in compsci"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Chapter 5: time is a query parameter. Every relation is bitemporal; `@`
// on the WRITE side names the valid instant a fact is recorded at (story
// #62's write-side valid time), and `@` on the READ side asks what held at
// a past instant — an ordinary seek, not a reconstruction.
// ─────────────────────────────────────────────────────────────────────────
fn chapter_5_time_travel() {
    let db = db();
    // The initial write also names its own valid instant (100): every
    // write is at a chosen instant, "now" is simply the default when `@`
    // is omitted, not a distinct write mode.
    db.run_script(
        "?[id, price] <- [[1, 100]] :create quote {id => price} @ 100",
        no_params(),
    )
    .expect("create quote");

    // Two corrections at later named valid instants.
    db.run_script(
        "?[id, price] <- [[1, 150]] :put quote {id => price} @ 200",
        no_params(),
    )
    .expect("price change @200");
    db.run_script(
        "?[id, price] <- [[1, 175]] :put quote {id => price} @ 300",
        no_params(),
    )
    .expect("price change @300");

    // As of instant 250: after the @200 change, before the @300 one.
    let out = db
        .run_script("?[price] := *quote{id, price @ 250}", no_params())
        .expect("as-of read");
    assert_eq!(
        ints(&out, 0),
        vec![150],
        "price as of 250 is the @200 write"
    );

    // As of instant 150: after the original @100 write, before the @200
    // correction — the value the record held at that moment in time.
    let out = db
        .run_script("?[price] := *quote{id, price @ 150}", no_params())
        .expect("as-of read before the first correction");
    assert_eq!(
        ints(&out, 0),
        vec![100],
        "price as of 150 is the original @100 row"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Chapter 6: vector search is a join. `::hnsw create` builds an index over
// a vector column; a `~relation:index{...}` atom is a k-NN search that
// unifies like any other relation, so it composes with the rest of the
// query instead of living behind a separate API.
// ─────────────────────────────────────────────────────────────────────────
fn chapter_6_vector_search() {
    let db = db();
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
            "?[id, dist] := ~doc:emb{id | query: vec([1.0, 0.0]), k: 2, bind_distance: dist} \
             :sort dist",
            no_params(),
        )
        .expect("vector search");
    assert_eq!(ints(&out, 0), vec![1, 3], "nearest-first to [1.0, 0.0]");
}

// ─────────────────────────────────────────────────────────────────────────
// Chapter 7: full-text search, the same shape as vector search — an index,
// then a search atom that joins like a relation.
// ─────────────────────────────────────────────────────────────────────────
fn chapter_7_full_text_search() {
    let db = db();
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
    assert_eq!(ints(&out, 0), vec![1], "only the fox document matches");
}

// ─────────────────────────────────────────────────────────────────────────
// Chapter 8: graphs are relations too. The whole-graph algorithms (shortest
// path, PageRank, community detection, …) run as built-in rules over
// ordinary relations — no export to a separate graph runtime.
// ─────────────────────────────────────────────────────────────────────────
fn chapter_8_graph_algorithms() {
    let db = db();
    db.run_script(
        "?[a, b, dist] <- [['FRA', 'JFK', 5000.0], ['JFK', 'LAX', 4000.0], \
         ['FRA', 'CDG', 900.0], ['CDG', 'LAX', 9000.0]] :create route {a, b => dist}",
        no_params(),
    )
    .expect("create route");

    // `start`/`end` seed the search; the fixed rule's own head names the
    // output columns (`src, dst, cost, path`).
    let out = db
        .run_script(
            "start[] <- [['FRA']]; \
             end[] <- [['LAX']]; \
             ?[src, dst, cost, path] <~ ShortestPathDijkstra(*route[], start[], end[])",
            no_params(),
        )
        .expect("shortest path");
    assert_eq!(out.rows().len(), 1, "one path found");
    let cost = out.rows()[0][2].get_float().expect("cost");
    assert!(
        (cost - 9000.0).abs() < 1e-6,
        "FRA-JFK-LAX costs 9000, got {cost}"
    );
}

fn main() {
    chapter_1_relations();
    println!("chapter 1 (relations): ok");
    chapter_2_rules();
    println!("chapter 2 (rules): ok");
    chapter_3_recursion();
    println!("chapter 3 (recursion): ok");
    chapter_4_aggregation();
    println!("chapter 4 (aggregation): ok");
    chapter_5_time_travel();
    println!("chapter 5 (time travel): ok");
    chapter_6_vector_search();
    println!("chapter 6 (vector search): ok");
    chapter_7_full_text_search();
    println!("chapter 7 (full-text search): ok");
    chapter_8_graph_algorithms();
    println!("chapter 8 (graph algorithms): ok");
    println!("language tour: all chapters pass");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chapter_1_relations_test() {
        chapter_1_relations();
    }
    #[test]
    fn chapter_2_rules_test() {
        chapter_2_rules();
    }
    #[test]
    fn chapter_3_recursion_test() {
        chapter_3_recursion();
    }
    #[test]
    fn chapter_4_aggregation_test() {
        chapter_4_aggregation();
    }
    #[test]
    fn chapter_5_time_travel_test() {
        chapter_5_time_travel();
    }
    #[test]
    fn chapter_6_vector_search_test() {
        chapter_6_vector_search();
    }
    #[test]
    fn chapter_7_full_text_search_test() {
        chapter_7_full_text_search();
    }
    #[test]
    fn chapter_8_graph_algorithms_test() {
        chapter_8_graph_algorithms();
    }
}
