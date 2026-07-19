/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Story #88: recursion and graph shapes a real user writes — transitive
//! closure, same-generation (the classic two-recursive-rule Datalog
//! textbook example), stratified negation, and two built-in graph fixed
//! rules (`ShortestPathDijkstra`, `ConnectedComponents`) — all through
//! `Db::run_script`.

mod common;
use common::*;

/// A rule that calls itself is a fixpoint: every airport reachable from
/// FRA, any number of hops.
#[test]
fn transitive_closure() {
    let db = fresh_db();
    db.run_script(
        "?[fr, to] <- [['FRA', 'JFK'], ['JFK', 'LAX'], ['LAX', 'YPO'], ['FRA', 'CDG'], \
         ['CDG', 'SIN']] :create route {fr, to}",
        no_params(),
    )
    .expect("create route");

    let out = db
        .run_script(
            "reachable[to] := *route{fr: 'FRA', to}; \
             reachable[to] := reachable[stop], *route{fr: stop, to}; \
             ?[to] := reachable[to]",
            no_params(),
        )
        .expect("transitive closure");
    let mut dests = strs(&out, 0);
    dests.sort();
    assert_eq!(dests, vec!["CDG", "JFK", "LAX", "SIN", "YPO"]);
}

/// Same-generation: two people are "same generation" if they're the same
/// person, or if their respective parents are (one hop up on each side,
/// recursively) — the classic non-linear recursive Datalog rule. Family:
/// alice and bob are siblings (same parent carol); carol and dave are
/// siblings (same parent eve); so alice/bob are one level down from
/// carol/dave, and eve's two children carol/dave are the same generation
/// as each other but NOT as alice/bob's generation-mates unless walked
/// through a shared ancestor at the same depth.
#[test]
fn same_generation() {
    let db = fresh_db();
    // parent[child, parent]
    db.run_script(
        "?[child, parent] <- [['alice', 'carol'], ['bob', 'carol'], \
         ['carol', 'eve'], ['dave', 'eve'], ['carol2', 'eve2'], ['dave2', 'eve2']] \
         :create parent {child, parent}",
        no_params(),
    )
    .expect("create parent");
    // The base case's reflexive set must include the ROOT ancestors too
    // (eve, eve2), not just the people who themselves have a parent row —
    // otherwise carol/dave's shared-parent recursion has no sg[eve, eve]
    // to bottom out on.
    db.run_script(
        "?[p] <- [['alice'], ['bob'], ['carol'], ['dave'], ['eve'], ['carol2'], ['dave2'], \
         ['eve2']] :create person {p}",
        no_params(),
    )
    .expect("create person");

    // sg[a, b] := a = b (base case: reflexive over every known person)
    // sg[a, b] := parent[a, pa], parent[b, pb], sg[pa, pb], a != b
    let query = "sg[a, b] := *person{p: a}, b = a; \
                 sg[a, b] := *parent{child: a, parent: pa}, *parent{child: b, parent: pb}, \
                             sg[pa, pb], a != b; \
                 ?[a, b] := sg[a, b], a != b";
    let out = db.run_script(query, no_params()).expect("same generation");
    let mut got: Vec<(String, String)> = out
        .rows()
        .iter()
        .map(|r| {
            (
                r[0].get_str().unwrap().to_string(),
                r[1].get_str().unwrap().to_string(),
            )
        })
        .collect();
    got.sort();
    // alice/bob are siblings (same generation as each other); carol/dave
    // are siblings; carol2/dave2 are siblings. alice and carol are NOT
    // the same generation (parent/child), and alice is not same-gen with
    // carol2 (unrelated lineage, no shared ancestor at all so the
    // recursive case never even fires for that pair).
    assert_eq!(
        got,
        vec![
            ("alice".to_string(), "bob".to_string()),
            ("bob".to_string(), "alice".to_string()),
            ("carol".to_string(), "dave".to_string()),
            ("carol2".to_string(), "dave2".to_string()),
            ("dave".to_string(), "carol".to_string()),
            ("dave2".to_string(), "carol2".to_string()),
        ]
    );
}

/// `ShortestPathDijkstra`: the cheapest FRA→LAX route by summed edge
/// weight, not by hop count (the direct-looking route is not the
/// cheapest one here).
#[test]
fn shortest_path_dijkstra() {
    let db = fresh_db();
    db.run_script(
        "?[a, b, dist] <- [['FRA', 'JFK', 5000.0], ['JFK', 'LAX', 4000.0], \
         ['FRA', 'CDG', 900.0], ['CDG', 'LAX', 3000.0]] :create route {a, b => dist}",
        no_params(),
    )
    .expect("create route");

    let out = db
        .run_script(
            "start[] <- [['FRA']]; \
             end[] <- [['LAX']]; \
             ?[src, dst, cost, path] <~ ShortestPathDijkstra(*route[], start[], end[])",
            no_params(),
        )
        .expect("shortest path");
    assert_eq!(out.rows().len(), 1);
    let cost = out.rows()[0][2].get_float().expect("cost");
    // FRA-CDG-LAX = 900 + 3000 = 3900, cheaper than FRA-JFK-LAX = 9000.
    assert!(
        (cost - 3900.0).abs() < 1e-6,
        "expected the cheaper route, got {cost}"
    );
}

/// `ConnectedComponents`, built undirected: two disjoint triangles plus
/// one isolated pair form three components.
#[test]
fn connected_components() {
    let db = fresh_db();
    db.run_script(
        "?[a, b] <- [[1, 2], [2, 3], [3, 1], [4, 5], [5, 6], [6, 4], [7, 8]] \
         :create edge {a, b}",
        no_params(),
    )
    .expect("create edge");

    let out = db
        .run_script("?[node, grp] <~ ConnectedComponents(*edge[])", no_params())
        .expect("connected components");
    let mut by_group: std::collections::BTreeMap<i64, Vec<i64>> = Default::default();
    for r in out.rows() {
        let node = r[0].get_int().expect("node");
        let grp = r[1].get_int().expect("group");
        by_group.entry(grp).or_default().push(node);
    }
    let mut groups: Vec<Vec<i64>> = by_group.into_values().collect();
    for g in groups.iter_mut() {
        g.sort_unstable();
    }
    groups.sort();
    assert_eq!(groups, vec![vec![1, 2, 3], vec![4, 5, 6], vec![7, 8]]);
}

/// Stratified negation: `not *rel{...}` — students enrolled in a course
/// who never submitted an assignment for it.
#[test]
fn stratified_negation() {
    let db = fresh_db();
    db.run_script(
        "?[student, course] <- [['ada', 'algebra'], ['bob', 'algebra'], ['cid', 'algebra']] \
         :create enrolled {student, course}",
        no_params(),
    )
    .expect("create enrolled");
    db.run_script(
        "?[student, course] <- [['ada', 'algebra'], ['bob', 'algebra']] \
         :create submitted {student, course}",
        no_params(),
    )
    .expect("create submitted");

    let out = db
        .run_script(
            "?[student] := *enrolled{student, course}, not *submitted{student, course}",
            no_params(),
        )
        .expect("negation");
    assert_eq!(strs(&out, 0), vec!["cid"], "only cid never submitted");
}
