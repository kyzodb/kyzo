/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Story #88: one scenario that touches graph, vector, text, and
//! aggregation together — the unified engine's whole point is that these
//! are not separate subsystems bolted together but one substrate with
//! several access paths that compose in a single query.
//!
//! The scenario: a small corpus of articles. Each has a topic-embedding
//! vector and a body of text, and a citation graph between them
//! (`cites`). The query finds, for every article within citing-distance
//! of a seed article (graph reachability), whose body mentions "engine"
//! (full-text search) and whose topic vector is close to a reference
//! vector (k-NN), grouped by topic with a count and the best (smallest)
//! distance per topic (aggregation) — one query, four access paths.

mod common;
use common::*;

#[test]
fn graph_vector_text_and_aggregation_compose_in_one_query() {
    let db = fresh_db();

    // Six articles; a citation edge is "cites" (a cites b).
    db.run_script(
        "?[id, topic, body, v] <- [ \
           [1, 'db', 'a fast storage engine for structured data', vec([1.0, 0.0])], \
           [2, 'db', 'query planning over a relational engine', vec([0.9, 0.1])], \
           [3, 'ml', 'gradient descent for neural network training', vec([0.0, 1.0])], \
           [4, 'db', 'unrelated paper about gardening and soil', vec([0.8, 0.2])], \
           [5, 'ml', 'a search engine ranking algorithm', vec([0.1, 0.9])], \
           [6, 'net', 'network routing protocols overview', vec([0.5, 0.5])] \
         ] :create article {id => topic, body: String, v: <F32; 2>}",
        no_params(),
    )
    .expect("create article");
    db.run_script(
        "?[a, b] <- [[1, 2], [2, 3], [1, 4], [4, 5], [5, 6]] :create cites {a, b}",
        no_params(),
    )
    .expect("create cites");

    db.run_script(
        "::fts create article:txt {extractor: body, tokenizer: Simple}",
        no_params(),
    )
    .expect("fts create");
    db.run_script(
        "::hnsw create article:emb {fields: [v], dim: 2, m: 16, ef_construction: 32, \
          distance: L2}",
        no_params(),
    )
    .expect("hnsw create");

    // Graph reachability from article 1, any number of citation hops
    // (recursion): 1 -> {2, 4} -> {3, 5} -> {6}.
    // Full-text: articles whose body mentions "engine": 1, 2, 5.
    // Reachable ∩ mentions-engine: {2, 5} (1 is the seed itself, not
    // counted as "reachable FROM itself" since the base case starts one
    // hop out; 4, 3, 6 don't mention "engine").
    // Vector: k-NN to [1.0, 0.0] over ALL articles, k=6 (everyone),
    // bind_distance.
    // Combine: join reachable ∩ engine-mentioning articles to their
    // vector distance and topic, then group by topic with count and min
    // distance.
    let query = "reachable[to] := *cites{a: 1, b: to}; \
                 reachable[to] := reachable[stop], *cites{a: stop, b: to}; \
                 mentions_engine[id] := ~article:txt{id | query: 'engine', k: 10}; \
                 candidate[id] := reachable[id], mentions_engine[id]; \
                 ?[topic, count(id), min(dist)] := \
                     candidate[id], \
                     *article{id, topic}, \
                     ~article:emb{id | query: vec([1.0, 0.0]), k: 10, bind_distance: dist}";
    let out = db.run_script(query, no_params()).expect("unified query");

    let mut got: Vec<(String, i64, f64)> = out
        .rows()
        .iter()
        .map(|r| {
            (
                r[0].get_str().unwrap().to_string(),
                r[1].get_int().unwrap(),
                r[2].get_float().unwrap(),
            )
        })
        .collect();
    got.sort_by(|a, b| a.0.cmp(&b.0));

    // Only articles 2 and 5 are both reachable from 1 and mention
    // "engine". Article 2 is topic 'db' (dist to [1,0] is
    // 0.1^2+0.1^2=0.02). Article 5 is topic 'ml' (dist to [1,0] is
    // 0.9^2+0.9^2=1.62).
    assert_eq!(got.len(), 2, "exactly two (topic) groups: db and ml");
    let db_row = got.iter().find(|(t, ..)| t == "db").expect("db group");
    assert_eq!(db_row.1, 1, "one db article (id 2) qualifies");
    assert!(
        (db_row.2 - 0.02).abs() < 1e-6,
        "article 2's own distance, got {}",
        db_row.2
    );

    let ml_row = got.iter().find(|(t, ..)| t == "ml").expect("ml group");
    assert_eq!(ml_row.1, 1, "one ml article (id 5) qualifies");
    assert!(
        (ml_row.2 - 1.62).abs() < 1e-6,
        "article 5's own distance, got {}",
        ml_row.2
    );
}
