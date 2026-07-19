/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! ONE smoke test that drives the WHOLE public `kyzo::` façade end to end —
//! the surface every external embedder and every workspace member (kyzo-bin,
//! the bindings) depends on. It reaches only through public exports: if the
//! public API reshapes and breaks a consumer, this test breaks first, at the
//! contract, rather than silently downstream. It deliberately touches every
//! major access path in one run: relational CRUD, all value kinds, the
//! Datalog query surface, recursion, aggregation, a vector index + k-NN
//! search, bitemporal time travel, the typed-refusal error path, the
//! dump→restore backup round-trip, and `verify_storage` — the last two are
//! covered by NO other integration test.

use std::collections::BTreeMap;

use kyzo::{
    Catalog, DataValue, Engine, FjallStorage, dump_storage, new_fjall_storage, restore_storage,
    verify_storage,
};

fn np() -> BTreeMap<String, DataValue> {
    BTreeMap::new()
}

/// A fresh real fjall store at a leaked tempdir (an `#[test]` process is
/// short-lived; the dir is reclaimed at exit).
fn fresh_storage() -> FjallStorage {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = new_fjall_storage(dir.path()).expect("fjall storage");
    std::mem::forget(dir);
    storage
}

#[test]
fn public_api_full_surface_smoke() {
    let storage = fresh_storage();
    let db = Engine::compose(storage.clone(), Catalog::new()).expect("Engine::compose");

    // ---- relational CRUD + a spread of value kinds through the API ----
    db.run_script(
        "?[id, name, score, flag, tags, j] <- [[1, 'alpha', 3.5, true, ['x', 'y'], \
         parse_json('{\"k\": 1}')]] \
         :create item {id => name: String, score: Float, flag: Bool, tags: [String], j: Json}",
        np(),
    )
    .expect("create item");
    db.run_script(
        "?[id, name, score, flag, tags, j] <- [[2, 'beta', 9.0, false, ['z'], \
         parse_json('[1,2,3]')]] :put item {id => name, score, flag, tags, j}",
        np(),
    )
    .expect("put item");
    let all = db
        .run_script("?[id, name] := *item{id, name} :order id", np())
        .expect("scan item");
    assert_eq!(all.rows().len(), 2);
    assert_eq!(all.rows()[0][0].get_int(), Some(1));
    assert_eq!(all.rows()[1][1].get_str(), Some("beta"));

    // an integral float coerces into the Int column (the coercion contract)
    db.run_script(
        "?[k, n] <- [[1, 3.0], [2, 42]] :create ints {k => n: Int}",
        np(),
    )
    .expect("integral floats coerce to Int");

    // ---- recursion (transitive closure) through the query surface ----
    db.run_script(
        "?[a, b] <- [[1, 2], [2, 3], [3, 4]] :create edge {a, b}",
        np(),
    )
    .expect("create edge");
    let reach = db
        .run_script(
            "reach[a, b] := *edge{a, b}; \
             reach[a, c] := reach[a, b], *edge{a: b, b: c}; \
             ?[a, b] := reach[a, b]",
            np(),
        )
        .expect("transitive closure");
    // 1->2,1->3,1->4, 2->3,2->4, 3->4 = 6 pairs
    assert_eq!(reach.rows().len(), 6);

    // ---- aggregation ----
    db.run_script(
        "?[g, v] <- [['a', 1], ['a', 2], ['b', 5]] :create agg {g, v}",
        np(),
    )
    .expect("create agg");
    let agg = db
        .run_script(
            "?[g, count(v), sum(v), max(v)] := *agg{g, v} :order g",
            np(),
        )
        .expect("aggregation");
    assert_eq!(agg.rows().len(), 2);
    assert_eq!(agg.rows()[0][1].get_int(), Some(2)); // count for 'a'
    assert_eq!(agg.rows()[0][2].get_int(), Some(3)); // sum for 'a'

    // ---- a vector index + k-NN search (a derived-index access path) ----
    db.run_script(
        "?[id, v] <- [[1, [1.0, 0.0]], [2, [0.0, 1.0]], [3, [0.9, 0.1]]] \
         :create emb {id => v: <F32; 2>}",
        np(),
    )
    .expect("create emb");
    db.run_script(
        "::hnsw create emb:idx {fields: [v], dim: 2, m: 16, ef_construction: 32, distance: L2}",
        np(),
    )
    .expect("create hnsw index");
    let knn = db
        .run_script(
            "?[id, dist] := ~emb:idx{id | query: vec([1.0, 0.0]), k: 2, bind_distance: dist} \
             :order dist",
            np(),
        )
        .expect("k-NN search");
    assert!(!knn.rows().is_empty(), "k-NN returns neighbours");
    assert_eq!(
        knn.rows()[0][0].get_int(),
        Some(1),
        "nearest to [1,0] is id 1"
    );

    // ---- bitemporal time travel ----
    db.run_script("?[k, v] <- [[1, 100]] :create hist {k => v} @ 100", np())
        .expect("create @ 100");
    db.run_script("?[k, v] <- [[1, 200]] :put hist {k => v} @ 200", np())
        .expect("put @ 200");
    let at150 = db
        .run_script("?[v] := *hist{k, v @ 150}", np())
        .expect("as-of 150");
    assert_eq!(
        at150.rows()[0][0].get_int(),
        Some(100),
        "as-of 150 sees 100"
    );
    let now = db
        .run_script("?[v] := *hist{k, v}", np())
        .expect("current read");
    assert_eq!(now.rows()[0][0].get_int(), Some(200), "current sees 200");

    // ---- the typed-refusal path: a bad coercion is refused, not silent ----
    let refused = db.run_script(
        "?[k, n] <- [[9, 'not-an-int']] :put ints {k => n: Int}",
        np(),
    );
    assert!(
        refused.is_err(),
        "a String into an Int column must be refused"
    );

    // ---- dump -> restore backup round-trip (public backup API) ----
    let dump_dir = tempfile::tempdir().expect("dump dir");
    let dump_path = dump_dir.path().join("dump.kyzo");
    dump_storage(&storage, &dump_path).expect("dump_storage");

    let restored_storage = fresh_storage();
    restore_storage(&restored_storage, &dump_path).expect("restore_storage");
    let db2 = Engine::compose(restored_storage.clone(), Catalog::new())
        .expect("Engine::compose on restored");
    let restored = db2
        .run_script("?[id, name] := *item{id, name} :order id", np())
        .expect("scan restored item");
    assert_eq!(
        restored.rows().len(),
        2,
        "every relation survives dump/restore"
    );
    assert_eq!(restored.rows()[1][1].get_str(), Some("beta"));
    // the vector index rebuilds and still searches after restore
    let restored_knn = db2
        .run_script("?[id] := ~emb:idx{id | query: vec([1.0, 0.0]), k: 1}", np())
        .expect("k-NN on restored index");
    assert_eq!(restored_knn.rows().len(), 1);

    // ---- verify_storage: the store is structurally sound, no corruption ----
    let report = verify_storage(&storage).expect("verify_storage");
    assert!(
        report.corrupt.is_empty(),
        "a healthy store reports no corruption, got: {:?}",
        report.corrupt
    );
}
