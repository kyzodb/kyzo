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
//! ## Scope (maintainer-ratified, see the story #78/#79 dictation)
//!
//! Three arms, and nothing else:
//! - **Contract laws**: the KV+MVCC surface every [`Storage`] must honor —
//!   ordered scans agree with a `BTreeMap` model, SSI aborts the second
//!   committer of a read/write conflict (first-committer-wins), snapshots
//!   are isolated, phantom reads are conflict-tracked, `del_range` kills a
//!   transaction's own uncommitted writes too, and concurrent real threads
//!   detect every conflict with zero lost updates.
//! - **DST fault campaigns**: [`dst_fault_campaign_kv_survives_crash`] reuses
//!   [`SimStorage`]'s seeded fault/crash/power-cut controls
//!   (`storage/sim.rs`) to drive the SAME generic KV law as a workload under
//!   injected faults, so the property that certifies quiescent correctness
//!   is the property the fault campaign tortures — one definition, not two.
//! - **Cross-backend differentials**: [`assert_ops_agree`] runs an identical
//!   op stream against N write-transaction species at once and demands
//!   byte-identical observations, generalizing what used to be a hand-rolled
//!   three-way comparison (`storage/temp.rs`) into a function any future
//!   backend can call with itself as one more entry.
//!
//! **Explicitly out, by construction**: per-backend time-travel re-proofs.
//! `range_skip_scan_tuple`'s seek algebra is proven ONCE, generically, by
//! the shared skip-scan driver (`storage/skip_walk.rs`, story #78) that
//! every backend ports onto; restating that proof per backend here would be
//! exactly the duplication this kit exists to end. Backup/restore and the
//! clock floor are a separate surface (`storage/backup.rs`) and stay out too.
//!
//! ## How a new backend adopts this kit
//!
//! Implement [`Storage`]/[`ReadTx`]/[`WriteTx`] (sealed to this crate; see
//! `storage/mod.rs`), then call [`run_full_battery`] with a factory that
//! hands back a fresh, empty instance. That is the whole integration.

use std::collections::BTreeMap;

use fjall::Slice;
use miette::Result;

use crate::storage::sim::{FaultConfig, SimRng, SimStorage, for_each_seed};
use crate::storage::{ReadTx, Storage, WriteTx};

// ==================== contract laws: generic over any Storage ====================

/// Compiler-enforced law: transactions move across threads (`Send`) and are
/// shared by reference across threads (`Sync`) — the engine's parallel
/// query evaluation depends on both. Nothing to run; a backend that fails
/// this fails to compile, which is the point (compiler > constructor >
/// test).
pub(crate) fn law_send_sync_bounds_are_compiler_checked<S: Storage>() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<S>();
    assert_send_sync::<S::ReadTx>();
    assert_send_sync::<S::WriteTx>();
}

/// Law: a mixed put/overwrite/delete/del_range workload, committed across
/// several transactions, leaves the store in EXACTLY the state a `BTreeMap`
/// model executing the identical operations would — full scan and bounded
/// scan alike.
pub(crate) fn law_kv_matches_model_oracle<S: Storage>(db: &S) {
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
    let got: Vec<_> = tx.total_scan().map(|r| r.unwrap()).collect();
    let want: Vec<_> = model
        .iter()
        .map(|(k, v)| (Slice::from(k), Slice::from(v)))
        .collect();
    assert_eq!(got, want, "store diverged from the model oracle");

    let got: Vec<_> = tx
        .range_scan(b"k005", b"k030")
        .map(|r| r.unwrap())
        .collect();
    let want: Vec<_> = model
        .range(b"k005".to_vec()..b"k030".to_vec())
        .map(|(k, v)| (Slice::from(k), Slice::from(v)))
        .collect();
    assert_eq!(got, want, "bounded scan diverged from the model oracle");
    assert_eq!(tx.range_count(b"k005", b"k030").unwrap(), want.len());
}

/// Law (contract v2 — SSI, `storage/mod.rs`'s history): a write-write race
/// on a key EACH side reads first-committer-wins — the second committer's
/// commit fails with the typed [`crate::storage::ConflictError`], and the
/// abort leaves no trace.
pub(crate) fn law_mvcc_first_committer_wins<S: Storage>(db: &S) {
    {
        let mut tx = db.write_tx().unwrap();
        tx.put(b"counter", b"0").unwrap();
        tx.commit().unwrap();
    }
    let mut tx1 = db.write_tx().unwrap();
    let mut tx2 = db.write_tx().unwrap();
    assert_eq!(tx1.get(b"counter").unwrap(), Some(Slice::from(b"0")));
    assert_eq!(tx2.get(b"counter").unwrap(), Some(Slice::from(b"0")));
    tx1.put(b"counter", b"1").unwrap();
    tx2.put(b"counter", b"2").unwrap();
    tx1.commit().unwrap();
    assert!(
        tx2.commit().is_err(),
        "second writer read a concurrently-modified key and must abort"
    );
    let tx = db.read_tx().unwrap();
    assert_eq!(
        tx.get(b"counter").unwrap(),
        Some(Slice::from(b"1")),
        "aborted transaction must leave no trace"
    );
}

/// Law: a write transaction sees its own uncommitted writes (RYOW); a
/// snapshot opened before a commit never observes it, one opened after
/// always does.
pub(crate) fn law_read_your_own_writes_and_snapshot_isolation<S: Storage>(db: &S) {
    let reader_before = db.read_tx().unwrap();
    let mut w = db.write_tx().unwrap();
    w.put(b"x", b"1").unwrap();
    assert_eq!(w.get(b"x").unwrap(), Some(Slice::from(b"1")), "RYOW");
    assert!(w.exists(b"x").unwrap());
    w.commit().unwrap();

    assert_eq!(reader_before.get(b"x").unwrap(), None, "snapshot isolation");
    let reader_after = db.read_tx().unwrap();
    assert_eq!(reader_after.get(b"x").unwrap(), Some(Slice::from(b"1")));
}

/// Law: `del_range` removes every key visible to the transaction in
/// `[lower, upper)` — including keys the SAME transaction just wrote,
/// uncommitted — while leaving keys outside the range untouched.
pub(crate) fn law_del_range_kills_own_writes<S: Storage>(db: &S) {
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
    assert_eq!(tx.get(b"z-outside").unwrap(), Some(Slice::from(b"stays")));
}

/// Law: a range READ inside a write transaction is conflict-tracked as a
/// whole — phantom protection — so a concurrent insert into that range
/// aborts the reader's commit even though it wrote to a disjoint key.
pub(crate) fn law_phantom_protection<S: Storage>(db: &S) {
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
pub(crate) fn law_concurrent_writers_across_threads<S: Storage>(db: &S) {
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
pub(crate) fn law_del_range_chunk_boundaries<S: Storage>(make: &impl Fn() -> S) {
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
pub(crate) fn run_full_battery<S: Storage>(make: impl Fn() -> S) {
    law_send_sync_bounds_are_compiler_checked::<S>();
    law_kv_matches_model_oracle(&make());
    law_mvcc_first_committer_wins(&make());
    law_read_your_own_writes_and_snapshot_isolation(&make());
    law_del_range_kills_own_writes(&make());
    law_phantom_protection(&make());
    law_concurrent_writers_across_threads(&make());
    law_del_range_chunk_boundaries(&make);
}

// ==================== cross-backend differential ====================
//
// An identical op stream driven against N write-transaction SPECIES at
// once (not necessarily N `Storage` backends — `TempTx` is a bare `WriteTx`
// with no separate storage handle, and belongs here exactly as it belongs
// in `storage/temp.rs`'s own three-way check). Every method used below
// lacks a `Self: Sized` bound, so `dyn WriteTx` is a legal trait object
// (`commit`/`commit_durable` are the two exceptions and are never called
// mid-differential — matching how the ad hoc three-way check in
// `storage/temp.rs` already uses these species: applied, never committed,
// compared by `total_scan`).

/// One observable outcome, normalized so backends whose error MESSAGES
/// differ still compare equal on error PRESENCE.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Obs {
    Val(Option<Slice>),
    Flag(bool),
    Rows(Vec<(Slice, Slice)>),
    Count(usize),
    Err,
}

/// One operation in a differential op stream.
#[derive(Debug, Clone)]
pub(crate) enum Op {
    Put(Vec<u8>, Vec<u8>),
    Del(Vec<u8>),
    DelRange(Vec<u8>, Vec<u8>),
    Get(Vec<u8>),
    Exists(Vec<u8>),
    Scan(Vec<u8>, Vec<u8>),
    ScanCount(Vec<u8>, Vec<u8>),
    Total,
}

/// A scan or total-scan is capped at this many yielded rows: proof that the
/// iterator terminates rather than merely a slow assertion.
const SCAN_CAP: usize = 10_000;

fn collect_rows(it: Box<dyn Iterator<Item = Result<(Slice, Slice)>> + '_>) -> Obs {
    let mut rows = vec![];
    for (i, kv) in it.enumerate() {
        assert!(
            i < SCAN_CAP,
            "scan yielded {SCAN_CAP}+ items: non-terminating iterator"
        );
        match kv {
            Ok(kv) => rows.push(kv),
            Err(_) => return Obs::Err,
        }
    }
    Obs::Rows(rows)
}

/// Apply one op to any write-transaction species, through the trait object,
/// recording its observable outcome.
pub(crate) fn apply_op(tx: &mut dyn WriteTx, op: &Op) -> Vec<Obs> {
    match op {
        Op::Put(k, v) => {
            tx.put(k, v).unwrap();
            vec![]
        }
        Op::Del(k) => {
            tx.del(k).unwrap();
            vec![]
        }
        Op::DelRange(lo, hi) => {
            tx.del_range(lo, hi).unwrap();
            vec![]
        }
        Op::Get(k) => vec![match tx.get(k) {
            Ok(v) => Obs::Val(v),
            Err(_) => Obs::Err,
        }],
        Op::Exists(k) => vec![match tx.exists(k) {
            Ok(b) => Obs::Flag(b),
            Err(_) => Obs::Err,
        }],
        Op::Scan(lo, hi) => vec![collect_rows(tx.range_scan(lo, hi))],
        Op::ScanCount(lo, hi) => vec![match tx.range_count(lo, hi) {
            Ok(n) => Obs::Count(n),
            Err(_) => Obs::Err,
        }],
        Op::Total => vec![collect_rows(tx.total_scan())],
    }
}

/// Keys from an alphabet that straddles type-tag boundaries and shares
/// prefixes; length 0..=4 (the empty key included).
pub(crate) fn gen_key(rng: &mut SimRng) -> Vec<u8> {
    const ALPHABET: [u8; 8] = [0x00, 0x01, 0x07, 0x0D, 0x41, 0x42, 0xFE, 0xFF];
    let len = rng.below(5) as usize;
    (0..len)
        .map(|_| ALPHABET[rng.below(ALPHABET.len() as u64) as usize])
        .collect()
}

pub(crate) fn gen_val(rng: &mut SimRng) -> Vec<u8> {
    let len = rng.below(12) as usize;
    (0..len).map(|_| (rng.next_u64() & 0xFF) as u8).collect()
}

pub(crate) fn gen_op(rng: &mut SimRng) -> Op {
    match rng.below(16) {
        0..=4 => Op::Put(gen_key(rng), gen_val(rng)),
        5..=6 => Op::Del(gen_key(rng)),
        7..=8 => Op::DelRange(gen_key(rng), gen_key(rng)),
        9..=10 => Op::Get(gen_key(rng)),
        11 => Op::Exists(gen_key(rng)),
        12..=13 => Op::Scan(gen_key(rng), gen_key(rng)),
        14 => Op::ScanCount(gen_key(rng), gen_key(rng)),
        _ => Op::Total,
    }
}

/// Run an identical op stream against every backend in `backends` and
/// demand byte-identical observations at every step, naming the first
/// disagreeing pair. This is the reusable form of a cross-backend
/// differential: a future backend joins by handing itself in as one more
/// `(name, Box<dyn WriteTx>)` entry, never by hand-rolling a new pairwise
/// comparison.
pub(crate) fn assert_ops_agree(backends: &mut [(&'static str, Box<dyn WriteTx>)], ops: &[Op]) {
    assert!(backends.len() >= 2, "nothing to differential against");
    for (step, op) in ops.iter().enumerate() {
        let first_name = backends[0].0;
        let first_obs = apply_op(backends[0].1.as_mut(), op);
        for entry in &mut backends[1..] {
            let name = entry.0;
            let obs = apply_op(entry.1.as_mut(), op);
            assert_eq!(
                first_obs, obs,
                "{first_name} vs {name} diverged at step {step} on {op:?}"
            );
        }
    }
}

// ==================== DST fault campaign ====================

/// `total_scan` is itself a read op subject to `read_fail_ppm` (`sim.rs`'s
/// `TAG_TOTAL` fault site) — reads are transient BY DOCTRINE (see
/// `storage/sim.rs`'s "Fault injection" module-doc arm and
/// `sim_read_faults_transient_and_deterministic`), so a bare `.unwrap()`
/// here would fail the campaign on an injected fault that has nothing to do
/// with a correctness bug. Retry a bounded number of times instead: each
/// attempt advances the identity's attempt counter, so with `read_fail_ppm`
/// well under 100% the retry converges almost immediately — this loop is
/// itself the liveness half of the same retry doctrine `retry.rs` documents
/// for conflicts.
fn read_total_scan_retrying(db: &SimStorage) -> BTreeMap<Vec<u8>, Vec<u8>> {
    const ATTEMPTS: usize = 50;
    for _ in 0..ATTEMPTS {
        let tx = db.read_tx().unwrap();
        if let Ok(m) = tx
            .total_scan()
            .map(|kv| kv.map(|(k, v)| (k.to_vec(), v.to_vec())))
            .collect::<Result<BTreeMap<_, _>>>()
        {
            return m;
        }
    }
    panic!("total_scan kept failing after {ATTEMPTS} retries: fault rate too high, or a real bug");
}

/// Drive [`law_kv_matches_model_oracle`]'s own KV workload — puts and
/// deletes tracked against a `BTreeMap` model — as the payload of a fault
/// campaign: every read/sync/commit is subject to injected faults, and the
/// run is periodically torn down by a simulated crash or power cut. After
/// EVERY reopen the store's full contents must equal exactly the model's
/// committed (crash) or fsynced (power cut) prefix — never more, never
/// less, never a panic. This is the bridge the kit's scope calls for: the
/// same generic property that certifies quiescent correctness becomes the
/// property a fault campaign tortures, instead of the DST arm inventing its
/// own bespoke assertion.
pub(crate) fn dst_fault_campaign_kv_survives_crash(seeds: std::ops::Range<u64>) {
    let faults = FaultConfig {
        read_fail_ppm: 20_000,
        spurious_conflict_ppm: 20_000,
        sync_fail_ppm: 20_000,
    };
    for_each_seed(seeds, |seed| {
        let mut db = SimStorage::with_faults(seed, faults);
        let mut rng = SimRng::new(seed ^ 0xC0FF_EE00_C0FF_EE00);
        let mut committed: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut synced: BTreeMap<Vec<u8>, Vec<u8>> = committed.clone();

        for round in 0..40u64 {
            let mut tx = db.write_tx().unwrap();
            let mut staged = committed.clone();
            for _ in 0..5 {
                match gen_op(&mut rng) {
                    Op::Put(k, v) if !k.is_empty() && tx.put(&k, &v).is_ok() => {
                        staged.insert(k, v);
                    }
                    Op::Del(k) if !k.is_empty() && tx.del(&k).is_ok() => {
                        staged.remove(&k);
                    }
                    _ => {}
                }
            }
            if tx.commit().is_ok() {
                committed = staged;
            }

            if rng.below(4) == 0 && db.sync().is_ok() {
                synced = committed.clone();
            }

            match rng.below(20) {
                0 => {
                    db = db.sim_crash();
                    let got = read_total_scan_retrying(&db);
                    assert_eq!(
                        got, committed,
                        "seed {seed} round {round}: crash diverged from the committed prefix"
                    );
                }
                1 => {
                    db = db.sim_powercut();
                    let got = read_total_scan_retrying(&db);
                    assert_eq!(
                        got, synced,
                        "seed {seed} round {round}: power cut diverged from the fsynced prefix"
                    );
                    // A power cut can only ever drop back to a PAST
                    // committed state, never invent one ahead of it.
                    committed = synced.clone();
                }
                _ => {}
            }
        }
    });
}

// ==================== proof: the kit runs green ====================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::fjall::new_fjall_storage;
    use crate::storage::temp::TempTx;

    /// The kit's real client today: the fjall backend earns conformance by
    /// one call, not by a bespoke test file.
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

    /// The contract's own test double earns the same conformance the real
    /// backend does — proof the battery is backend-agnostic, not fjall-shaped
    /// in disguise.
    #[test]
    fn sim_passes_the_full_battery() {
        run_full_battery(|| SimStorage::new(0x5117_5117));
    }

    /// Three write-transaction species — the real backend, the contract's
    /// DST double, and the session-scratch species that has no separate
    /// `Storage` handle at all — driven by one identical seeded op stream
    /// each, through the kit's generic differential engine.
    #[test]
    fn cross_backend_differential_fjall_sim_temp() {
        for seed in 0..12u64 {
            let dir = tempfile::tempdir().unwrap();
            let fjall_store = new_fjall_storage(dir.path()).unwrap();
            let fjall_tx = fjall_store.write_tx().unwrap();
            let sim_store = SimStorage::new(seed);
            let sim_tx = sim_store.write_tx().unwrap();
            let temp_tx = TempTx::default();

            let mut rng = SimRng::new(seed ^ 0x00D1_FFEE);
            let ops: Vec<Op> = (0..120)
                .map(|_| gen_op(&mut rng))
                // fjall rejects the empty key at the API level; the engine
                // never writes one (every key carries an 8-byte prefix).
                .filter(|op| !matches!(op, Op::Put(k, _) | Op::Del(k) if k.is_empty()))
                .collect();

            let mut backends: Vec<(&'static str, Box<dyn WriteTx>)> = vec![
                ("fjall", Box::new(fjall_tx)),
                ("sim", Box::new(sim_tx)),
                ("temp", Box::new(temp_tx)),
            ];
            assert_ops_agree(&mut backends, &ops);
        }
    }

    /// The DST fault campaign, run for real: 60 seeds, faults injected on
    /// every read/sync/commit, crashes and power cuts sprinkled throughout.
    ///
    /// kyzodb/kyzo#91 (fixed): `SimStorage::sim_crash` used to conflate the
    /// synced watermark with `commit_seq` on reopen, so a power cut
    /// simulated after an intervening crash wrongly retained buffer-tier
    /// data that was never fsynced. This campaign's random seeds hit that
    /// bug directly (first observed at seed 18); `reopen` now takes
    /// `commit_seq`/`synced_seq` as separate parameters and `sim_crash`
    /// carries the true pre-crash `synced_seq` through unchanged.
    #[test]
    fn dst_fault_campaign_kv_survives_crash_and_powercut() {
        dst_fault_campaign_kv_survives_crash(0..60);
    }

    /// Minimal, seed-free pin for kyzodb/kyzo#91: a crash between an
    /// fsynced write and an unsynced one must not let the power cut that
    /// follows keep the unsynced write.
    #[test]
    fn repro_crash_then_powercut_loses_the_synced_watermark() {
        let db = SimStorage::new(1);
        let mut tx = db.write_tx().unwrap();
        tx.put(b"A", b"1").unwrap();
        tx.commit().unwrap();
        db.sync().unwrap();
        let mut tx = db.write_tx().unwrap();
        tx.put(b"B", b"1").unwrap();
        tx.commit().unwrap();
        let db = db.sim_crash();
        let tx = db.read_tx().unwrap();
        let got: BTreeMap<_, _> = tx.total_scan().map(|r| r.unwrap()).collect();
        assert!(got.contains_key(b"A".as_slice()) && got.contains_key(b"B".as_slice()));
        let db = db.sim_powercut();
        let tx = db.read_tx().unwrap();
        let got: BTreeMap<_, _> = tx.total_scan().map(|r| r.unwrap()).collect();
        assert!(
            got.contains_key(b"A".as_slice()),
            "A was synced, must survive a power cut"
        );
        assert!(
            !got.contains_key(b"B".as_slice()),
            "B was NEVER synced -- a power cut after an intervening crash must still drop it"
        );
    }
}
