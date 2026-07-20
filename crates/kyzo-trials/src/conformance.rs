/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The storage contract as a conformance kit (story #79): one battery of
//! generic properties, quantified over `S: Storage` (or, for the
//! differential arm, over any bare [`WriteTx`] species), so a new backend
//! passes exactly the same torture the fjall backend does — by CALLING this
//! module, not by a maintainer hand-copying fjall's test file and swapping
//! type names.
//!
//! ## Public law for second backends (decisions.md §85)
//!
//! This module is a **crate-public** surface. Any workspace crate (or
//! stranger crate) that depends on `kyzo-trials` invokes the kit as:
//!
//! ```rust,ignore
//! use kyzo_trials::conformance::run_full_battery;
//! // or the re-export: kyzo_trials::run_full_battery
//!
//! run_full_battery(|| my_fresh_empty_storage());
//! ```
//!
//! That out-of-crate call is the adoption path — not an in-crate copy of
//! the scenarios. The §85 closure proof is the integration test
//! `tests/conformance_public.rs` (not a second backend crate). The
//! `S: Storage` laws below are likewise `pub` so a caller can run a single
//! law when debugging a specific refusal.
//!
//! ## Necessary-not-sufficient (carried obligation)
//!
//! A green [`run_full_battery`] pass is **necessary but not sufficient** for
//! production `Committed` durability. Per decisions.md §27/§85 the kit is
//! public law for second backends; the StableCommitCap arm requirement lands
//! with `07-storage-seats.json` (`store/commit_cap.rs`). Do not read kit-green
//! as a durability license.
//!
//! ## Scope (maintainer-ratified, see the story #78/#79 dictation)
//!
//! Three arms, and nothing else:
//! - **Contract laws**: the KV+MVCC surface every [`Storage`] must honor —
//!   ordered scans agree with a `BTreeMap` model, SSI aborts the second
//!   committer of a read/write conflict (first-committer-wins), snapshots
//!   are isolated, phantom reads are conflict-tracked, `del_range` kills a
//!   transaction's own uncommitted writes too, and concurrent real threads
//!   detect every conflict with zero lost updates.
//! - **DST fault campaigns / cross-backend differentials**: require a
//!   trials-visible `SimStorage` / `TempTx` factory. Those seats are
//!   `pub(crate)` on `kyzo::store::sim` / `kyzo::store::scratch` — not on the
//!   `kyzo-trials` public door. Deleted the prior `#[cfg(any())]` dead
//!   `sim_backed` module (pre-peel `crate::storage::…` paths). Re-arm when
//!   SimStorage is exported for out-of-crate conformance callers.
//!
//! **Explicitly out, by construction**: per-backend time-travel re-proofs.
//! `range_skip_scan_tuple`'s seek algebra is proven ONCE, generically, by
//! the shared skip-scan driver (`store/skip_walk.rs`, story #78) that
//! every backend ports onto; restating that proof per backend here would be
//! exactly the duplication this kit exists to end. Backup/restore and the
//! clock floor are a separate surface (`store/backup.rs`) and stay out too.
//!
//! ## How a new backend adopts this kit
//!
//! Implement [`Storage`]/[`ReadTx`]/[`WriteTx`] (sealed admission is an
//! engine ruling — see `store/contract.rs`), then call [`run_full_battery`]
//! with a factory that hands back a fresh, empty instance. That is the whole
//! integration.

use std::collections::BTreeMap;

use kyzo::{ReadTx, Storage, WriteTx};

// ==================== contract laws: generic over any Storage ====================

/// Compiler-enforced law: transactions move across threads (`Send`) and are
/// shared by reference across threads (`Sync`) — the engine's parallel
/// query evaluation depends on both. Nothing to run; a backend that fails
/// this fails to compile, which is the point (compiler > constructor >
/// test).
pub fn law_send_sync_bounds_are_compiler_checked<S: Storage>() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<S>();
    assert_send_sync::<S::ReadTx>();
    assert_send_sync::<S::WriteTx>();
}

/// Law: a mixed put/overwrite/delete/del_range workload, committed across
/// several transactions, leaves the store in EXACTLY the state a `BTreeMap`
/// model executing the identical operations would — full scan and bounded
/// scan alike.
pub fn law_kv_matches_model_oracle<S: Storage>(db: &S) {
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for round in 0u32..3 {
        let mut tx = db.write_tx().unwrap();
        for i in 0..40u32 {
            let n = (i * 7 + round * 13) % 50;
            let k = format!("k{n:03}").into_bytes();
            if n % 5 == round % 5 {
                tx.del(&k).unwrap();
                model.remove(&k);
            } else {
                let v = format!("v{round}-{n}").into_bytes();
                tx.put(&k, &v).unwrap();
                model.insert(k, v);
            }
        }
        if round == 2 {
            tx.del_range(b"k010", b"k020").unwrap();
            let doomed: Vec<_> = model
                .range(b"k010".to_vec()..b"k020".to_vec())
                .map(|(k, _)| k.clone())
                .collect();
            for k in doomed {
                model.remove(&k);
            }
        }
        tx.commit().unwrap();
    }

    let tx = db.read_tx().unwrap();
    let got: Vec<_> = tx
        .total_scan()
        .map(|r| {
            let (k, v) = r.unwrap();
            (k.to_vec(), v.to_vec())
        })
        .collect();
    let want: Vec<_> = model
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    assert_eq!(got, want, "store diverged from the model oracle");

    let got: Vec<_> = tx
        .range_scan(b"k005", b"k030")
        .map(|r| {
            let (k, v) = r.unwrap();
            (k.to_vec(), v.to_vec())
        })
        .collect();
    let want: Vec<_> = model
        .range(b"k005".to_vec()..b"k030".to_vec())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    assert_eq!(got, want, "bounded scan diverged from the model oracle");
    assert_eq!(tx.range_count(b"k005", b"k030").unwrap(), want.len());
}

/// Law (contract v2 — SSI, `store/contract.rs`'s history): a write-write race
/// on a key EACH side reads first-committer-wins — the second committer's
/// commit fails with the typed [`kyzo::ConflictError`], and the abort leaves
/// no trace.
pub fn law_mvcc_first_committer_wins<S: Storage>(db: &S) {
    {
        let mut tx = db.write_tx().unwrap();
        tx.put(b"counter", b"0").unwrap();
        tx.commit().unwrap();
    }
    let mut tx1 = db.write_tx().unwrap();
    let mut tx2 = db.write_tx().unwrap();
    assert_eq!(
        tx1.get(b"counter").unwrap().as_deref(),
        Some(b"0".as_slice())
    );
    assert_eq!(
        tx2.get(b"counter").unwrap().as_deref(),
        Some(b"0".as_slice())
    );
    tx1.put(b"counter", b"1").unwrap();
    tx2.put(b"counter", b"2").unwrap();
    tx1.commit().unwrap();
    assert!(
        tx2.commit().is_err(),
        "second writer read a concurrently-modified key and must abort"
    );
    let tx = db.read_tx().unwrap();
    assert_eq!(
        tx.get(b"counter").unwrap().as_deref(),
        Some(b"1".as_slice()),
        "aborted transaction must leave no trace"
    );
}

/// Law: a write transaction sees its own uncommitted writes (RYOW); a
/// snapshot opened before a commit never observes it, one opened after
/// always does.
pub fn law_read_your_own_writes_and_snapshot_isolation<S: Storage>(db: &S) {
    let reader_before = db.read_tx().unwrap();
    let mut w = db.write_tx().unwrap();
    w.put(b"x", b"1").unwrap();
    assert_eq!(
        w.get(b"x").unwrap().as_deref(),
        Some(b"1".as_slice()),
        "RYOW"
    );
    assert!(w.exists(b"x").unwrap());
    w.commit().unwrap();

    assert_eq!(reader_before.get(b"x").unwrap(), None, "snapshot isolation");
    let reader_after = db.read_tx().unwrap();
    assert_eq!(
        reader_after.get(b"x").unwrap().as_deref(),
        Some(b"1".as_slice())
    );
}

/// Law: `del_range` removes every key visible to the transaction in
/// `[lower, upper)` — including keys the SAME transaction just wrote,
/// uncommitted — while leaving keys outside the range untouched.
pub fn law_del_range_kills_own_writes<S: Storage>(db: &S) {
    {
        let mut tx = db.write_tx().unwrap();
        tx.put(b"k1", b"1").unwrap();
        tx.put(b"k2", b"2").unwrap();
        tx.commit().unwrap();
    }
    let mut tx = db.write_tx().unwrap();
    tx.put(b"k3", b"3").unwrap();
    tx.put(b"z-outside", b"stays").unwrap();
    tx.del_range(b"k0", b"k9").unwrap();
    tx.commit().unwrap();

    let tx = db.read_tx().unwrap();
    assert_eq!(tx.get(b"k1").unwrap(), None);
    assert_eq!(tx.get(b"k2").unwrap(), None);
    assert_eq!(tx.get(b"k3").unwrap(), None, "own writes in range die too");
    assert_eq!(
        tx.get(b"z-outside").unwrap().as_deref(),
        Some(b"stays".as_slice())
    );
}

/// Law: a range READ inside a write transaction is conflict-tracked as a
/// whole — phantom protection — so a concurrent insert into that range
/// aborts the reader's commit even though it wrote to a disjoint key.
pub fn law_phantom_protection<S: Storage>(db: &S) {
    let mut tx1 = db.write_tx().unwrap();
    let seen: usize = tx1.range_scan(b"r", b"s").count();
    assert_eq!(seen, 0);
    let mut tx2 = db.write_tx().unwrap();
    tx2.put(b"r-phantom", b"x").unwrap();
    tx2.commit().unwrap();
    tx1.put(b"elsewhere", b"y").unwrap();
    assert!(
        tx1.commit().is_err(),
        "a scanned range was modified concurrently: SSI must abort the scanner"
    );
}

/// Law: real concurrent writers across threads never lose an update.
/// Disjoint keys must never conflict; a contended read-modify-write counter
/// must retry through every conflict and land every increment exactly once.
pub fn law_concurrent_writers_across_threads<S: Storage>(db: &S) {
    {
        let mut tx = db.write_tx().unwrap();
        tx.put(b"counter", b"0").unwrap();
        tx.commit().unwrap();
    }

    const THREADS: usize = 8;
    const OPS: usize = 25;
    std::thread::scope(|s| {
        for t in 0..THREADS {
            let db = db.clone();
            s.spawn(move || {
                for i in 0..OPS {
                    let mut tx = db.write_tx().unwrap();
                    tx.put(format!("t{t}-k{i}").as_bytes(), b"x").unwrap();
                    tx.commit()
                        .unwrap_or_else(|e| panic!("disjoint writers must not conflict: {e}"));
                }
                for _ in 0..OPS {
                    loop {
                        let mut tx = db.write_tx().unwrap();
                        let cur: u64 = std::str::from_utf8(&tx.get(b"counter").unwrap().unwrap())
                            .unwrap()
                            .parse()
                            .unwrap();
                        tx.put(b"counter", (cur + 1).to_string().as_bytes())
                            .unwrap();
                        if tx.commit().is_ok() {
                            break;
                        }
                    }
                }
            });
        }
    });

    let tx = db.read_tx().unwrap();
    let total: u64 = std::str::from_utf8(&tx.get(b"counter").unwrap().unwrap())
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        total,
        (THREADS * OPS) as u64,
        "every increment must land exactly once: conflicts detected, no lost updates"
    );
    assert_eq!(
        tx.range_count(b"t", b"u").unwrap(),
        THREADS * OPS,
        "all disjoint writes must be present"
    );
}

/// Law: `del_range`'s deletion must be exact at a chunked implementation's
/// resuming-cursor boundary — probed at one-under, exactly-at, one-over,
/// and twice the chunk size fjall uses (1024), so a backend with a
/// differently-sized chunk still gets an off-by-one probe near its own
/// boundary along with three sizes nowhere near any plausible chunk size.
pub fn law_del_range_chunk_boundaries<S: Storage>(make: &impl Fn() -> S) {
    for n in [1023usize, 1024, 1025, 2048] {
        let db = make();
        let iter = (0..n).map(|i| Ok((format!("k{i:08}").into_bytes(), b"v".to_vec())));
        db.batch_put(Box::new(iter)).unwrap();
        let mut tx = db.write_tx().unwrap();
        tx.put(b"z-survivor", b"x").unwrap();
        tx.del_range(b"k", b"l").unwrap();
        tx.commit().unwrap();
        let tx = db.read_tx().unwrap();
        assert_eq!(tx.range_count(b"k", b"l").unwrap(), 0, "n={n}: all deleted");
        assert!(
            tx.exists(b"z-survivor").unwrap(),
            "n={n}: outside range survives"
        );
    }
}

/// Run every generic contract law against one fresh-store-producing
/// factory. This is the whole of what a new backend calls to earn
/// conformance.
pub fn run_full_battery<S: Storage>(make: impl Fn() -> S) {
    law_send_sync_bounds_are_compiler_checked::<S>();
    law_kv_matches_model_oracle(&make());
    law_mvcc_first_committer_wins(&make());
    law_read_your_own_writes_and_snapshot_isolation(&make());
    law_del_range_kills_own_writes(&make());
    law_phantom_protection(&make());
    law_concurrent_writers_across_threads(&make());
    law_del_range_chunk_boundaries(&make);
}

/// In-crate proof that the public battery is callable with a real backend.
/// Out-of-crate §85 proof: `tests/conformance_public.rs` invokes
/// [`run_full_battery`] (and a `law_*`) via the crate's `pub` surface.
#[cfg(test)]
mod tests {
    use super::*;
    use kyzo::new_fjall_storage;

    #[test]
    fn fjall_passes_the_full_battery() {
        run_full_battery(|| {
            // Each law wants a fresh, empty store; a fjall store's live
            // file handles keep working after its directory is unlinked
            // from the tree, so leaking the `TempDir` guard (rather than
            // threading a handle through every law) is the right call for
            // test scaffolding that must hand back a bare `S`.
            let dir = tempfile::tempdir().unwrap();
            let db = new_fjall_storage(dir.path()).unwrap();
            std::mem::forget(dir);
            db
        });
    }
}

// ==================== sim-backed arms (deleted) ====================
//
// Deleted the prior `#[cfg(any())]` `sim_backed` dead module (pre-peel
// `crate::storage::…` paths that never type-checked). Cannot re-arm inside
// kyzo-trials: `SimStorage` lives at `kyzo::store::sim` as `pub(crate)` and
// `TempTx` at `kyzo::store::scratch` as `pub(crate)` — neither is on the
// trials public door. Re-arm when those factories are exported for out-of-crate
// conformance callers (DST fault campaign + cross-backend differential).
