//! The session tier's adversarial battery, written by its hostile reviewer
//! (review-session-3) and ADOPTED into the suite as a review requirement:
//! the author's tests were mutation-proven blind to a collector that isn't
//! rebuilt per retry attempt, a trigger-cache keyed only by source text,
//! and pre-commit callback delivery — the tests here kill all three
//! deterministically (seeded spurious conflicts, no thread races). Also
//! carries the reviewer's independent e2e scenario, contention, and
//! cross-backend determinism checks, plus the F2 refusal pin.

use std::collections::BTreeMap;

use crate::data::value::DataValue;
use crate::fixed_rule::NamedRows;
use crate::runtime::db::{Db, ScriptOptions};
use crate::storage::Storage;
use crate::storage::fjall::new_fjall_storage;
use crate::storage::sim::{FaultConfig, SimStorage};

fn no_params() -> BTreeMap<String, DataValue> {
    BTreeMap::new()
}

fn int_rows(nr: &NamedRows) -> Vec<Vec<i64>> {
    let mut out: Vec<Vec<i64>> = nr
        .rows
        .iter()
        .map(|r| r.iter().map(|v| v.get_int().expect("int")).collect())
        .collect();
    out.sort();
    out
}

/// Rows in RETURNED order (no sorting) — for determinism assertions.
fn raw_int_rows(nr: &NamedRows) -> Vec<Vec<i64>> {
    nr.rows
        .iter()
        .map(|r| r.iter().map(|v| v.get_int().expect("int")).collect())
        .collect()
}

/// Reviewer's own end-to-end scenario over fjall: schema with keyed relation,
/// multi-script inserts, aggregation, :order/:limit, :update, :insert
/// conflict, :ensure, :rm — the stored.rs arms the author's tests never touch.
#[test]
fn rs3_independent_e2e_scenario() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap();

    db.run_script(
        "?[a, b] <- [[1, 10], [2, 20]] :create sal {a => b}",
        no_params(),
    )
    .expect("create");
    db.run_script("?[a, b] <- [[3, 30], [4, 20]] :put sal {a, b}", no_params())
        .expect("second put");

    // Aggregation through the public API.
    let agg = db
        .run_script("?[sum(b)] := *sal[_, b]", no_params())
        .expect("aggregation");
    assert_eq!(int_rows(&agg), vec![vec![80]]);

    // :order desc by value, tie broken asc by key, :limit 2.
    let top = db
        .run_script("?[a, b] := *sal[a, b] :order -b, a :limit 2", no_params())
        .expect("order+limit");
    assert_eq!(raw_int_rows(&top), vec![vec![3, 30], vec![2, 20]]);

    // :update rewrites the dependent column of an existing key.
    db.run_script("?[a, b] <- [[1, 11]] :update sal {a, b}", no_params())
        .expect("update");
    let after_update = db
        .run_script("?[b] := *sal[1, b]", no_params())
        .expect("read back");
    assert_eq!(int_rows(&after_update), vec![vec![11]]);

    // :insert on an existing key is a typed refusal.
    let err = db
        .run_script("?[a, b] <- [[1, 99]] :insert sal {a, b}", no_params())
        .expect_err(":insert must refuse an existing key");
    assert!(
        format!("{err:?}").contains("exists"),
        "expected key-exists refusal, got {err:?}"
    );

    // :ensure passes on a matching row, refuses a mismatch.
    db.run_script("?[a, b] <- [[1, 11]] :ensure sal {a, b}", no_params())
        .expect(":ensure matching row");
    db.run_script("?[a, b] <- [[1, 12]] :ensure sal {a, b}", no_params())
        .expect_err(":ensure must refuse a mismatched value");

    // :rm removes by key; survivors are exactly the rest.
    db.run_script("?[a] <- [[1], [4]] :rm sal {a}", no_params())
        .expect("rm");
    let rest = db
        .run_script("?[a, b] := *sal[a, b]", no_params())
        .expect("scan");
    assert_eq!(int_rows(&rest), vec![vec![2, 20], vec![3, 30]]);

    // :returning on a put reports the mutated rows.
    let ret = db
        .run_script(
            "?[a, b] <- [[7, 70]] :put sal {a, b} :returning",
            no_params(),
        )
        .expect("put returning");
    assert_eq!(int_rows(&ret), vec![vec![7, 70]]);
}

/// Two `on put` triggers on one relation fire in ONE session, and each runs
/// its own program (the trigger-parse cache must key by source). Also proves
/// the trigger pipeline works at all — nothing else in the tree tests it.
#[test]
fn rs3_two_put_triggers_fire_distinctly_in_one_session() {
    let db = Db::new(SimStorage::new(41)).unwrap();
    db.run_script("?[a, b] <- [[0, 0]] :create src {a => b}", no_params())
        .expect("create src");
    db.run_script("?[a, b] <- [[0, 0]] :create mirror {a => b}", no_params())
        .expect("create mirror");
    db.run_script("?[a, b] <- [[0, 0]] :create mirror2 {a => b}", no_params())
        .expect("create mirror2");
    db.run_script(
        "::set_triggers src \
         on put { ?[a, b] := _new[a, b] :put mirror {a, b} } \
         on put { ?[a, b] := _new[a, b] :put mirror2 {a, b} }",
        no_params(),
    )
    .expect("set triggers");

    db.run_script("?[a, b] <- [[1, 10], [2, 20]] :put src {a, b}", no_params())
        .expect("put fires triggers");

    let mirror = db
        .run_script("?[a, b] := *mirror[a, b]", no_params())
        .expect("mirror scan");
    assert_eq!(
        int_rows(&mirror),
        vec![vec![0, 0], vec![1, 10], vec![2, 20]],
        "first on-put trigger must mirror the new rows"
    );
    let mirror2 = db
        .run_script("?[a, b] := *mirror2[a, b]", no_params())
        .expect("mirror2 scan");
    assert_eq!(
        int_rows(&mirror2),
        vec![vec![0, 0], vec![1, 10], vec![2, 20]],
        "second on-put trigger must run ITS program, not a cache-collided one"
    );
}

/// The phantom-event law, actually exercised: a callback registered on a
/// contended counter must deliver exactly one Put event per COMMITTED
/// increment — the new values are exactly {1..=N}, no duplicates from
/// conflicted-and-retried attempts, none missing.
#[test]
fn rs3_callbacks_exactly_once_under_contention() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap();
    db.run_script("?[k, v] <- [[0, 0]] :create ctr {k => v}", no_params())
        .expect("create counter");

    let (_id, receiver) = db.register_callback("ctr");

    const PER_THREAD: i64 = 15;
    const THREADS: i64 = 2;
    std::thread::scope(|scope| {
        for _ in 0..THREADS {
            let db = db.clone();
            scope.spawn(move || {
                for _ in 0..PER_THREAD {
                    db.run_script(
                        "?[k, v] := *ctr[k, old], v = old + 1 :put ctr {k, v}",
                        no_params(),
                    )
                    .expect("increment");
                }
            });
        }
    });

    // Every send happened post-commit on the session threads, which joined.
    let mut new_values: Vec<i64> = vec![];
    while let Ok((op, new, _old)) = receiver.try_recv() {
        assert_eq!(op.as_str(), "Put");
        for row in &new.rows {
            new_values.push(row[1].get_int().expect("int"));
        }
    }
    new_values.sort();
    let want: Vec<i64> = (1..=THREADS * PER_THREAD).collect();
    assert_eq!(
        new_values, want,
        "exactly one event per committed increment: a conflicted attempt must \
         leak nothing and a committed one must lose nothing"
    );
}

/// Reviewer's own contention shape (3 writers, distinct from the author's 2).
#[test]
fn rs3_three_writer_contention_loses_no_update() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::new(new_fjall_storage(dir.path()).unwrap()).unwrap();
    db.run_script("?[k, v] <- [[0, 0]] :create ctr {k => v}", no_params())
        .expect("create counter");

    const PER_THREAD: i64 = 10;
    std::thread::scope(|scope| {
        for _ in 0..3 {
            let db = db.clone();
            scope.spawn(move || {
                for _ in 0..PER_THREAD {
                    db.run_script(
                        "?[k, v] := *ctr[k, old], v = old + 1 :put ctr {k, v}",
                        no_params(),
                    )
                    .expect("increment");
                }
            });
        }
    });
    let out = db
        .run_script("?[v] := *ctr[0, v]", no_params())
        .expect("read");
    assert_eq!(int_rows(&out), vec![vec![30]]);
}

/// Determinism: the same scenario on two fresh databases — and across the
/// fjall and sim backends — returns byte-identical rows in identical order;
/// a budget refusal renders identically on repeated runs.
#[test]
fn rs3_determinism_across_backends_and_repeats() {
    fn scenario<S: Storage>(db: &Db<S>) -> (Vec<Vec<i64>>, Vec<Vec<i64>>) {
        db.run_script(
            "?[a, b] <- [[1, 2], [2, 3], [3, 4], [4, 2], [5, 6]] :create edge {a, b}",
            no_params(),
        )
        .expect("create");
        let q = "
            path[a, b] := *edge[a, b]
            path[a, b] := *edge[a, c], path[c, b]
            ?[a, b] := path[a, b]
        ";
        let first = raw_int_rows(&db.run_script(q, no_params()).expect("closure"));
        let second = raw_int_rows(&db.run_script(q, no_params()).expect("closure again"));
        (first, second)
    }

    let dir1 = tempfile::tempdir().unwrap();
    let db1 = Db::new(new_fjall_storage(dir1.path()).unwrap()).unwrap();
    let dir2 = tempfile::tempdir().unwrap();
    let db2 = Db::new(new_fjall_storage(dir2.path()).unwrap()).unwrap();
    let db3 = Db::new(SimStorage::new(99)).unwrap();

    let (a1, a2) = scenario(&db1);
    let (b1, _) = scenario(&db2);
    let (c1, _) = scenario(&db3);
    assert_eq!(a1, a2, "same db, repeated run: identical rows in order");
    assert_eq!(a1, b1, "fresh fjall dbs: identical rows in order");
    assert_eq!(a1, c1, "fjall vs sim: identical rows in order");

    // Budget refusal is reproducible, including its rendered content.
    let refusal = |db: &Db<SimStorage>| -> String {
        let opts = ScriptOptions {
            derived_tuple_ceiling: Some(3),
            ..Default::default()
        };
        let err = db
            .run_script_with(
                "
                path[a, b] := *edge[a, b]
                path[a, b] := *edge[a, c], path[c, b]
                ?[a, b] := path[a, b]
                ",
                no_params(),
                opts,
            )
            .expect_err("must refuse");
        format!("{err:?}")
    };
    let r1 = refusal(&db3);
    let r2 = refusal(&db3);
    let r3 = refusal(&db3);
    assert_eq!(r1, r2);
    assert_eq!(r2, r3);
}

/// F2 FIXED (was: silently dropped): a mutation targeting a `_`-prefixed
/// (temp) relation would be routed down the read-only path by
/// `needs_write_lock() == None` and its `store_relation` silently ignored.
/// It is now a typed, spanned refusal (`TempRelationNotReachableError`)
/// until multi-script sessions make temp relations observable. Weakening
/// the refusal back to the silent drop makes this test fail on the
/// `unwrap_err`.
#[test]
fn rs3_temp_relation_mutation_is_a_typed_refusal() {
    let db = Db::new(SimStorage::new(23)).unwrap();
    let err = db
        .run_script("?[a] <- [[1]] :create _scratch {a}", no_params())
        .unwrap_err();
    assert!(
        err.to_string().contains("cannot be stored to yet"),
        "expected the typed temp-relation refusal, got: {err}"
    );
    // The refusal really was a refusal: nothing half-created.
    db.run_script("?[a] := *_scratch[a]", no_params())
        .expect_err("the temp relation must not exist after the refusal");
}

/// DETERMINISTIC phantom-event detector: seeded spurious conflicts force the
/// retry loop to replay commits with no thread races. A collector that is
/// not rebuilt per attempt (or delivered pre-commit) duplicates events; the
/// callback stream must be exactly one Put event per committed increment.
#[test]
fn rs3_callbacks_exactly_once_under_seeded_spurious_conflicts() {
    let faults = FaultConfig {
        spurious_conflict_ppm: 400_000, // ~40% of commits conflict spuriously
        ..Default::default()
    };
    let db = Db::new(SimStorage::with_faults(77, faults)).unwrap();
    db.run_script("?[k, v] <- [[0, 0]] :create ctr {k => v}", no_params())
        .expect("create counter (retries through spurious conflicts)");

    let (_id, receiver) = db.register_callback("ctr");
    const N: i64 = 20;
    for _ in 0..N {
        db.run_script(
            "?[k, v] := *ctr[k, old], v = old + 1 :put ctr {k, v}",
            no_params(),
        )
        .expect("increment (retries through spurious conflicts)");
    }

    let mut new_values: Vec<i64> = vec![];
    while let Ok((_op, new, _old)) = receiver.try_recv() {
        for row in &new.rows {
            new_values.push(row[1].get_int().expect("int"));
        }
    }
    new_values.sort();
    let want: Vec<i64> = (1..=N).collect();
    assert_eq!(
        new_values, want,
        "spurious-conflict retries must leak no phantom events and lose none"
    );
}
