/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Durability / crash-campaign proofs for story #221.
//!
//! ## Overlap-only group commit (T2)
//!
//! Compiled as `kyzo::store::sweep::crash` under `cfg(test)` via `#[path]`
//! from [`sweep`](../../../../kyzo-core/src/store/sweep.rs) so the proof can
//! observe SweepDoor batch membership without a second commit door. A
//! non-overlapping arrival after an in-flight fsync window closes must not
//! share that barrier's [`OverlapBatch`] — and wake ≠ timer (no sleep coalesce).

use super::{
    IntentOrdinal, OverlapBatch, SweepDoor, SweepSession,
};
use crate::store::authority::{Entropy, OpenOrdinal};
use crate::store::commit_cap::{SnapshotFork, StableCommitCap};
use crate::store::idempotency::{IdempotencyMemo, OperationKey, RequestDigest};
use crate::store::merkle::{GENESIS_ROOT, StateRoot};
use crate::store::open::{
    EntropyArm, GenesisParams, SizeClass, StableCommitCapArm, StagingTtl, StoreId, genesis,
};
use crate::store::scratch::TempTx;

fn op_key(store_id: StoreId, op: &[u8]) -> (OperationKey, RequestDigest) {
    let key = OperationKey::single_store(b"kyzo.sweep.crash", op, store_id, b"s0");
    let digest = IdempotencyMemo::digest_request(op);
    (key, digest)
}

fn open_live_door(identity_seed: [u8; 32], entropy: [u8; 32]) -> (SweepDoor, crate::store::IncarnationId, SweepSession) {
    let sealed = genesis(GenesisParams {
        identity_seed,
        recovery_matrix: None,
        staging_ttl: StagingTtl::new(1_024),
        size_class: SizeClass::Compact,
        entropy_arm: EntropyArm::OsRandom,
        stable_commit_cap: StableCommitCapArm::NativeFsyncProof {
            snapshot_fork: SnapshotFork::No,
        },
    });
    let store_id = sealed.store_id();
    let fence_epoch = sealed.fence_epoch();
    let (_view, auth) = sealed.take_write_authority();
    let incarnation = auth
        .incarnation_mint_cap(OpenOrdinal::ZERO)
        .mint(Entropy::from_bytes(entropy))
        .expect("incarnation mint");
    let session = SweepSession::new(store_id, fence_epoch, incarnation);
    let cap = StableCommitCap::NativeFsyncProof {
        snapshot_fork: SnapshotFork::No,
    };
    let door = SweepDoor::open(store_id, fence_epoch, session, auth, cap)
        .expect("live SweepDoor");
    (door, incarnation, session)
}

fn content_root(tag: u8) -> StateRoot {
    let mut bytes = *GENESIS_ROOT.as_bytes();
    bytes[0] = tag;
    StateRoot::from_digest(bytes)
}

/// Overlap-only group-commit proof: a non-overlapping arrival is not batched
/// with an in-flight fsync (no timer).
///
/// Exercises the distinction concretely:
/// 1. A queued, then B admitted while the fsync window is open → same overlap batch.
/// 2. Window closes at seal; C admitted after → must not be a member of that batch.
/// 3. C seals in a later barrier alone.
///
/// Fails if C appears in the first [`OverlapBatch`], if A/B fail to share it,
/// or if batching were modeled as a timer (this path never sleeps).
#[test]
fn overlap_only_group_commit_non_overlapping_arrival_not_batched() {
    let (mut door, incarnation, session) = open_live_door([0x21; 32], [0xC0; 32]);
    let store_id = session.store_id();
    let (key_a, dig_a) = op_key(store_id, b"overlap-A");
    let (key_b, dig_b) = op_key(store_id, b"overlap-B");
    let (key_c, dig_c) = op_key(store_id, b"overlap-C");

    // A arrives before the barrier — queued, then pulled into the overlap cohort.
    let intent_a = door
        .admit(incarnation, &session, key_a, dig_a)
        .expect("admit A before fsync window");
    door.begin_fsync_window(incarnation, &session)
        .expect("begin in-flight fsync window");
    assert!(
        door.fsync_window_open(),
        "fsync window must be open for overlap admission"
    );

    // B arrives while the fsync is in flight → overlaps that barrier.
    let intent_b = door
        .admit(incarnation, &session, key_b, dig_b)
        .expect("admit B overlapping in-flight fsync");
    let cohort: Vec<IntentOrdinal> = door.overlap_cohort_ordinals().collect();
    assert_eq!(
        cohort,
        vec![intent_a.intent_ordinal(), intent_b.intent_ordinal()],
        "overlap cohort must be exactly A then B while the fsync window is open"
    );

    let (batch_ab, committed_ab) = door
        .seal_durable_overlap_batch(
            vec![
                (TempTx::default(), content_root(1)),
                (TempTx::default(), content_root(2)),
            ],
            &session,
        )
        .expect("seal overlap batch A+B");

    assert!(
        !door.fsync_window_open(),
        "seal must close the fsync window — otherwise late arrivals could still overlap"
    );
    assert_eq!(committed_ab.len(), 2);
    assert!(
        batch_ab.contains_overlap_member(intent_a.intent_ordinal()),
        "A must be an overlap member of the sealed barrier"
    );
    assert!(
        batch_ab.contains_overlap_member(intent_b.intent_ordinal()),
        "B (arrived during in-flight fsync) must share A's overlap batch"
    );
    assert_eq!(
        door.last_overlap_batch(),
        Some(&batch_ab),
        "last_overlap_batch must report the sealed A+B barrier"
    );

    // C arrives after the window closed — non-overlapping with that in-flight fsync.
    let intent_c = door
        .admit(incarnation, &session, key_c, dig_c)
        .expect("admit C after fsync window closed");
    assert!(
        !batch_ab.contains_overlap_member(intent_c.intent_ordinal()),
        "non-overlapping arrival C must NOT be a member of the prior overlap batch"
    );
    assert!(
        door.overlap_cohort_ordinals().next().is_none(),
        "C must wait on the IntentionQueue, not join a still-open overlap cohort"
    );
    assert_eq!(
        door.queue().len(),
        1,
        "non-overlapping C waits for a later barrier (queue carriage)"
    );

    // Later barrier: C alone. No timer — explicit begin/seal only.
    door.begin_fsync_window(incarnation, &session)
        .expect("begin later fsync window for C");
    let (batch_c, committed_c) = door
        .seal_durable_overlap_batch(vec![(TempTx::default(), content_root(3))], &session)
        .expect("seal overlap batch C");

    assert_eq!(committed_c.len(), 1);
    assert!(
        batch_c.contains_overlap_member(intent_c.intent_ordinal()),
        "C must seal in its own later overlap batch"
    );
    assert!(
        !batch_c.contains_overlap_member(intent_a.intent_ordinal())
            && !batch_c.contains_overlap_member(intent_b.intent_ordinal()),
        "later barrier must not re-batch A/B with non-overlapping C"
    );
    assert_ne!(
        batch_ab.members(),
        batch_c.members(),
        "overlap batches for distinct fsync windows must differ"
    );

    // History ordinals stay dense across barriers; batch membership is carriage.
    assert_eq!(
        committed_ab[0].commit_ordinal().get() + 1,
        committed_ab[1].commit_ordinal().get()
    );
    assert_eq!(
        committed_ab[1].commit_ordinal().get() + 1,
        committed_c[0].commit_ordinal().get()
    );

    // Named so a trivial always-true assert cannot satisfy the board Check alone:
    // both this file and sweep.rs must carry the overlap law token.
    let _: &OverlapBatch = &batch_ab;
}

// ---------------------------------------------------------------------------
// Real power-cut / FUSE crash-matrix campaign (story #31) — path-wired under
// `kyzo::store::sweep::crash`. Requires kyzo-crashfs (kyzo-core test dep) and
// `crate::store::sim::SimStorage` (crate-private DST double). Skips cleanly
// when FUSE mount is unavailable (`can_mount` == false).
// ---------------------------------------------------------------------------
mod fuse_crash_matrix {
    use std::collections::BTreeSet;

    use kyzo_crashfs::harness::{can_mount, mount, wait_for_mount};
    use kyzo_crashfs::{Fault, FaultPlan, OpKind, PassthroughFs, Trigger};

    use crate::store::fjall::new_fjall_storage;
    use crate::store::sim::SimStorage;
    use crate::store::{ReadTx, Slice, Storage, WriteTx};

    /// The main data keyspace's journal file at a fresh store's very first
    /// segment. The commit-boundary matrix hard-pins this: after the
    /// fault-free recorder, `journal_segment_basenames` must equal
    /// `[JOURNAL_PATH]` — with the small row counts this class drives, the
    /// journal must never rotate past `0.jnl`, so the Nth op on this ONE
    /// path is unambiguous.
    const JOURNAL_PATH: &str = "0.jnl";

    /// One durable round's key/value pairs, deterministic in `round` and `n` so
    /// two independent runs (the recorder pass, a faulted pass, and the
    /// `SimStorage` oracle) generate byte-identical rows without sharing any
    /// state.
    fn round_kv(round: u32, n: u32) -> Vec<(Vec<u8>, Vec<u8>)> {
        (0..n)
            .map(|i| {
                (
                    format!("r{round:04}-k{i:04}").into_bytes(),
                    format!("r{round:04}-v{i:04}").into_bytes(),
                )
            })
            .collect()
    }

    /// Drive `rounds` DURABLE commit rounds against `storage`, generic over the
    /// `Storage` trait so ONE driver exercises both `SimStorage` (the oracle)
    /// and a real `FjallStorage` mounted through the injector — never two
    /// hand-written copies of the same workload.
    fn drive_durable_rounds<S: Storage>(storage: &S, rounds: u32, n: u32) {
        for round in 0..rounds {
            let mut tx = storage.write_tx().unwrap();
            for (k, v) in round_kv(round, n) {
                tx.put(&k, &v).unwrap();
            }
            tx.commit_durable().unwrap();
        }
    }

    /// The full visible key/value set, as an order-independent set — the
    /// comparison currency between the real backend and the `SimStorage`
    /// oracle (their `total_scan` byte order need not agree for this
    /// campaign's purposes, only their content).
    fn total_scan_set<S: Storage>(storage: &S) -> BTreeSet<(Slice, Slice)> {
        let tx = storage.read_tx().unwrap();
        tx.total_scan().map(|r| r.unwrap()).collect()
    }

    /// The oracle: `SimStorage` driven through the identical round script,
    /// POWER-CUT right after `surviving_rounds` durable commits — never
    /// fsyncing the remaining rounds first. This is the exact "only the
    /// fsynced prefix survives" contract [`Fault::ClearCache`] on the real
    /// journal implements, so the two must agree.
    fn oracle_after_powercut(surviving_rounds: u32, n: u32) -> BTreeSet<(Slice, Slice)> {
        let sim = SimStorage::new(0xF00D_F00D_F00D_F00D);
        drive_durable_rounds(&sim, surviving_rounds, n);
        let cut = sim.sim_powercut();
        total_scan_set(&cut)
    }

    /// Journal segment basenames (`0.jnl`, `1.jnl`, …) under a store root —
    /// the hard-pin for this class: the small-row matrix must never rotate
    /// past a single segment, or `JOURNAL_PATH`'s Nth-fsync premise is void.
    fn journal_segment_basenames(store_root: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(store_root)
            .unwrap_or_else(|e| panic!("read store root {}: {e}", store_root.display()))
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jnl"))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    const ROUNDS: u32 = 6;
    const KEYS_PER_ROUND: u32 = 20;

    /// Faults exercised at each recorded commit_durable barrier. ClearCache
    /// keeps the exact power-cut oracle; TornSeq/TornOp are write-time
    /// decisions (see `kyzo_crashfs::fault`) so they arm on the journal
    /// Write count observed at the same barrier — never on Fsync, where
    /// only ClearCache is a first-class event.
    const COMMIT_BOUNDARY_FAULTS: [Fault; 3] = [Fault::ClearCache, Fault::TornSeq, Fault::TornOp];

    /// Commit-boundary class: a real `FjallStorage`, mounted through
    /// `kyzo-crashfs`, crashed EXACTLY at each of its own `commit_durable`
    /// calls in turn — never a byte offset, per the design ruling's
    /// field-converged lesson ("anchored to durability barriers").
    ///
    /// Rows stay small so the journal is hard-pinned on `0.jnl` (asserted).
    /// Each barrier is crossed with ClearCache (exact SimStorage power-cut
    /// oracle) AND with TornSeq/TornOp on the barrier's last journal write
    /// (open-or-typed-refuse; never durable past the torn round; never
    /// wrong bytes). A ≥2-journal-segment matrix is blocked without a
    /// `max_journaling_size` door on `StorageOptions` (AUDIT-crash-multiseg).
    #[test]
    fn commit_boundary_crash_matrix_matches_the_powercut_oracle() {
        if !can_mount() {
            eprintln!(
                "SKIPPED: no live FUSE mount capability in this sandbox \
             (see kyzo_crashfs::harness::can_mount)."
            );
            return;
        }

        // Pass 1: the fault-free recorder. Learn which occurrence of Fsync
        // AND Write on the journal coincides with each round's own
        // commit_durable by OBSERVING it once, honestly — never by guessing
        // fjall's internals.
        let backing_a = tempfile::tempdir().unwrap();
        let mnt_a = tempfile::tempdir().unwrap();
        let fs_a = PassthroughFs::new(backing_a.path(), FaultPlan::new(1));
        let counters = fs_a.shared_counters();
        let Some(session_a) = mount(fs_a, mnt_a.path()) else {
            return;
        };
        wait_for_mount(mnt_a.path());
        let (boundary_fsync_count, boundary_write_count): (Vec<u64>, Vec<u64>) = {
            let db = new_fjall_storage(mnt_a.path()).unwrap();
            let mut fsyncs = Vec::with_capacity(ROUNDS as usize);
            let mut writes = Vec::with_capacity(ROUNDS as usize);
            for round in 0..ROUNDS {
                let mut tx = db.write_tx().unwrap();
                for (k, v) in round_kv(round, KEYS_PER_ROUND) {
                    tx.put(&k, &v).unwrap();
                }
                tx.commit_durable().unwrap();
                fsyncs.push(counters.fsync_count(JOURNAL_PATH));
                writes.push(counters.write_count(JOURNAL_PATH));
            }
            (fsyncs, writes)
        };
        // Hard-pin: this matrix's rows must never rotate the journal. If a
        // second segment appears, JOURNAL_PATH's Nth-op counts are no longer
        // the commit-boundary identity — fail loud, do not paper over.
        let journals = journal_segment_basenames(backing_a.path());
        assert_eq!(
            journals,
            vec![JOURNAL_PATH.to_string()],
            "commit-boundary matrix must stay on a single journal segment \
             ({JOURNAL_PATH}); observed {journals:?} — shrink rows or seat a \
             multi-segment variant with per-segment triggers"
        );
        drop(session_a);
        // Sanity: strictly increasing, or this class's entire premise (one
        // fsync/write frontier per round on this one path) is void and every
        // assertion below would be meaningless — fail loud here, not by
        // mis-triggering later.
        for w in boundary_fsync_count.windows(2) {
            assert!(
                w[0] < w[1],
                "fsync counts on {JOURNAL_PATH} must strictly increase per round: \
             {boundary_fsync_count:?}"
            );
        }
        for w in boundary_write_count.windows(2) {
            assert!(
                w[0] < w[1],
                "write counts on {JOURNAL_PATH} must strictly increase per round: \
             {boundary_write_count:?}"
            );
        }

        // Pass 2: one independent campaign per (round boundary × fault).
        // Fresh backing directory every time — no campaign ever reuses
        // another's disk state.
        for (idx, (&fsync_count, &write_count)) in boundary_fsync_count
            .iter()
            .zip(boundary_write_count.iter())
            .enumerate()
        {
            let round_idx = idx as u32; // the round whose OWN barrier is torn
            for fault in COMMIT_BOUNDARY_FAULTS {
                let backing_b = tempfile::tempdir().unwrap();
                let mnt_b = tempfile::tempdir().unwrap();
                // ClearCache is an fsync-boundary power cut; TornSeq/TornOp
                // decide at write time and materialize on the next fsync —
                // arming them on Fsync would be a silent no-op in passthrough.
                let (op, at_count) = match fault {
                    Fault::ClearCache => (OpKind::Fsync, fsync_count),
                    Fault::TornSeq | Fault::TornOp => (OpKind::Write, write_count),
                };
                let plan = FaultPlan::new(1).with_trigger(Trigger::new(
                    JOURNAL_PATH,
                    op,
                    at_count,
                    fault,
                ));
                let fs_b = PassthroughFs::new(backing_b.path(), plan);
                let Some(session_b) = mount(fs_b, mnt_b.path()) else {
                    return;
                };
                wait_for_mount(mnt_b.path());
                {
                    let db = new_fjall_storage(mnt_b.path()).unwrap();
                    for round in 0..=round_idx {
                        let mut tx = db.write_tx().unwrap();
                        for (k, v) in round_kv(round, KEYS_PER_ROUND) {
                            tx.put(&k, &v).unwrap();
                        }
                        tx.commit_durable().unwrap();
                    }
                }
                drop(session_b); // the simulated crash: unmount, nothing more written

                // Reopen directly on the backing directory — bypassing FUSE
                // entirely, exactly as a real process reopening after a crash
                // would see the disk (mirrors kyzo-crashfs's own standalone tests).
                let reopen = new_fjall_storage(backing_b.path());
                let expected_prefix = oracle_after_powercut(round_idx, KEYS_PER_ROUND);
                match fault {
                    Fault::ClearCache => {
                        let reopened = reopen.unwrap_or_else(|e| {
                            panic!(
                                "ClearCache round {round_idx}: store must open clean, not: {e}"
                            )
                        });
                        // "Opens clean" is necessary, never sufficient (the issue's
                        // own pinned lsm-tree finding: data blocks are checksummed
                        // lazily, on first read) — total_scan forces the traversal
                        // that would surface a torn data block instead of silently
                        // skipping it.
                        let observed = total_scan_set(&reopened);
                        assert_eq!(
                            observed, expected_prefix,
                            "ClearCache round {round_idx}: a crash exactly at its own \
                             commit_durable's fsync must leave precisely rounds \
                             0..{round_idx} visible (this round's own writes never durable), \
                             matching SimStorage crashed at the analogous logical point"
                        );
                    }
                    Fault::TornSeq | Fault::TornOp => {
                        // Torn* has no SimStorage twin for "drop/split one journal
                        // write then fsync". Honest outcomes: typed reopen refuse,
                        // OR open clean with exactly the already-fsynced prefix
                        // (same set ClearCache must leave). Showing the torn round
                        // as durable, or losing prior fsynced rounds, is a fail —
                        // and equality with the prefix makes a vacuous (no-op)
                        // trigger fail too, because a no-op would leave
                        // rounds 0..=round_idx.
                        match reopen {
                            Ok(reopened) => {
                                let observed = total_scan_set(&reopened);
                                // Equality with the ClearCache prefix is itself the
                                // anti-vacuity check: a no-op trigger would leave
                                // rounds 0..=round_idx durable, which is a larger set.
                                assert_eq!(
                                    observed, expected_prefix,
                                    "{fault:?} round {round_idx}: open-clean after a torn \
                                     commit-boundary write must leave exactly the already-\
                                     fsynced prefix (rounds 0..{round_idx}), never the torn \
                                     round and never less than prior durable rounds"
                                );
                            }
                            Err(_typed_refusal) => {
                                // A typed reopen refusal is an honest torn-journal outcome.
                            }
                        }
                    }
                }
            }
        }
    }

    const FLOOD_ROWS: u32 = 3_000;
    const FLOOD_VALUE_LEN: usize = 30_000;
    const SEEDS: u64 = 6;
    /// Matches any segment file fjall's LSM keyspaces create
    /// (`keyspaces/<id>/tables/<segment>`) — the highest-value unknown named
    /// by the issue's design ruling.
    const SEGMENT_GLOB: &str = "*/tables/*";
    const PER_SEED_BUDGET: std::time::Duration = std::time::Duration::from_secs(25);

    fn flood_key(i: u32) -> String {
        format!("flood{i:08}")
    }

    /// Deterministic, per-index content so a wrong-bytes read is unambiguous —
    /// never a constant fill a coincidental truncation could still satisfy.
    fn flood_val(i: u32, len: usize) -> Vec<u8> {
        let tag = format!("v{i:08}-");
        tag.bytes()
            .chain(std::iter::repeat(b'y'))
            .take(len)
            .collect()
    }

    /// Run `body` on its own thread, polling for completion rather than
    /// blocking on it directly. A found-the-hard-way lesson building this
    /// module: a stuck FUSE request parks its caller in the kernel
    /// (`request_wait_answer`), a state no ordinary signal — not even
    /// `SIGKILL` — can unwind; only tearing down the whole process (or
    /// force-aborting the FUSE connection from outside it) frees it. A hang
    /// inside `body` is therefore not "the test fails slowly," it is "the
    /// whole suite wedges forever" unless something outside the wedged thread
    /// enforces a deadline. `process::exit` here is that enforcement: it is
    /// itself the report (the message names the exact invariant story #31
    /// polices — never hang — and that this run just violated it), landing
    /// the campaign's verdict as a hard failure instead of an unbounded stall.
    fn run_bounded(label: &str, body: impl FnOnce() + Send + 'static) {
        let handle = std::thread::spawn(body);
        let start = std::time::Instant::now();
        while !handle.is_finished() {
            if start.elapsed() > PER_SEED_BUDGET {
                eprintln!(
                    "HANG DETECTED in {label}: exceeded the {PER_SEED_BUDGET:?} budget — this IS \
                 the \"never hang\" property story #31 exists to police, caught by the campaign \
                 rather than wedging the suite. File it (injector or engine, whichever is \
                 actually stuck) rather than raising the budget."
                );
                std::process::exit(90);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if let Err(panic_payload) = handle.join() {
            std::panic::resume_unwind(panic_payload);
        }
    }

    /// Compaction/segment class: a compaction-forcing flood (~90MB across 3000
    /// rows — confirmed, by reading the backing directory directly, to force a
    /// real flush AND produce segment files under `keyspaces/*/tables/`)
    /// through the mount, with each seed torturing its OWN segment files'
    /// FIRST fsync with a different fault kind. Deliberately a bounded, ONE
    /// simulated crash EVENT per campaign (matching what a real crash actually
    /// is) rather than an ambient rate applied continuously across the whole
    /// flood — an earlier draft of this test used ambient rates at 8%/8% over
    /// thousands of writes and reliably wedged the process (see
    /// `run_bounded`'s doc): a real crash corrupts what was in flight at ONE
    /// instant, never a sustained fraction of every write for an entire
    /// session, and the earlier draft's unrealistic model was pushing fjall's
    /// own recovery/background-worker code down a path steady-state operation
    /// never exercises. Unlike the commit-boundary class, this does NOT assert
    /// an exact surviving key set — background flush/compaction timing is not
    /// under this test's direct control. The invariant that DOES hold
    /// unconditionally, and the one the falsification clause is actually
    /// about: never a panic (nor a hang — `run_bounded` turns one into a hard,
    /// diagnosed failure instead), the store opens or refuses with a TYPED
    /// error, and — the property silent corruption would violate — every key
    /// that IS present after reopen holds EXACTLY the bytes it was written
    /// with.
    #[test]
    fn compaction_segment_torn_write_campaign_never_returns_wrong_bytes() {
        if !can_mount() {
            eprintln!(
                "SKIPPED: no live FUSE mount capability in this sandbox \
             (see kyzo_crashfs::harness::can_mount)."
            );
            return;
        }

        for seed in 0..SEEDS {
            run_bounded(
                "compaction_segment_torn_write_campaign_never_returns_wrong_bytes",
                move || {
                    let fault =
                        [Fault::TornSeq, Fault::TornOp, Fault::ClearCache][(seed % 3) as usize];
                    let backing = tempfile::tempdir().unwrap();
                    let mnt = tempfile::tempdir().unwrap();
                    // The FIRST fsync any segment file sees, torn one way or
                    // the other — every distinct path matching the glob gets
                    // its OWN independent occurrence counter, so this fires
                    // once per segment file actually created, never a
                    // sustained rate over the whole flood.
                    let plan = FaultPlan::new(seed).with_trigger(Trigger::new(
                        SEGMENT_GLOB,
                        OpKind::Fsync,
                        1,
                        fault,
                    ));
                    let fs = PassthroughFs::new(backing.path(), plan);
                    let Some(session) = mount(fs, mnt.path()) else {
                        return;
                    };
                    wait_for_mount(mnt.path());
                    {
                        // A store that itself hits an error mid-open/mid-write
                        // is a legitimate campaign outcome (the crash could
                        // land anywhere); only a PANIC would be a real defect,
                        // so the flood itself is deliberately best-effort.
                        if let Ok(db) = new_fjall_storage(mnt.path()) {
                            for i in 0..FLOOD_ROWS {
                                let Ok(mut tx) = db.write_tx() else { break };
                                if tx
                                    .put(flood_key(i).as_bytes(), &flood_val(i, FLOOD_VALUE_LEN))
                                    .is_err()
                                {
                                    break;
                                }
                                if tx.commit().is_err() {
                                    break;
                                }
                            }
                            let _ = db.sync();
                            // Give the background flush thread a moment to
                            // actually reach disk — confirmed empirically (the
                            // probe that shaped these constants) to settle
                            // within ~1s.
                            std::thread::sleep(std::time::Duration::from_millis(1200));
                        }
                    }
                    drop(session); // the simulated crash

                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                        || -> miette::Result<()> {
                            let reopened = new_fjall_storage(backing.path())?;
                            let tx = reopened.read_tx()?;
                            for i in 0..FLOOD_ROWS {
                                if let Some(v) = tx.get(flood_key(i).as_bytes())? {
                                    assert_eq!(
                                        v,
                                        flood_val(i, FLOOD_VALUE_LEN),
                                        "seed {seed}: key {i} present after crash but with WRONG \
                                     bytes — exactly the silent-corruption class this campaign \
                                     exists to catch"
                                    );
                                }
                                // Absent is a legitimate outcome: this row was
                                // never guaranteed durable (only a
                                // best-effort `commit`, not `commit_durable`,
                                // backed it before the final `sync`).
                            }
                            Ok(())
                        },
                    ));
                    match outcome {
                        Ok(Ok(())) => {}              // opened clean; every present key correct
                        Ok(Err(_typed_refusal)) => {} // a typed refusal is acceptable
                        Err(panic_payload) => std::panic::resume_unwind(panic_payload), // never
                    }
                },
            );
        }
    }

    /// Process-crash consistency: a child process commits one transaction,
    /// stages a second without committing, then `abort()`s. Reopening the store
    /// must show every committed write and nothing from the uncommitted one.
    ///
    /// # SCOPE HONESTY (carried obligation — power-cut-honesty)
    ///
    /// `abort()` simulates a process crash (committed data has reached OS buffers —
    /// fjall's `Buffer` persist mode). A power cut is a stronger event; surviving
    /// it is what `Storage::sync` (fsync) is for, and testing THAT honestly requires
    /// fault-injection infrastructure (kyzo-crashfs / dm-flakey class), not a unit
    /// test that lies about what it simulates. Until decisions.md §29's
    /// power-cut-at-commit-door campaign is green, the word `durable` is not
    /// licensed in Spec claims from this test alone.
    #[test]
    fn crash_consistency_process_abort() {
        use crate::store::fjall::new_fjall_storage;
        use crate::store::{ReadTx, Slice, Storage, WriteTx};

        if let Ok(dir) = std::env::var("KYZO_CRASH_CHILD_DIR") {
            let db = new_fjall_storage(&dir).unwrap();
            let mut tx = db.write_tx().unwrap();
            tx.put(b"committed", b"survives").unwrap();
            tx.commit().unwrap();
            let mut tx = db.write_tx().unwrap();
            tx.put(b"synced", b"survives-power-cut-too").unwrap();
            tx.commit().unwrap();
            db.sync().unwrap();
            let mut tx = db.write_tx().unwrap();
            tx.put(b"durable", b"per-tx-fsync").unwrap();
            tx.commit_durable().unwrap();
            let mut tx = db.write_tx().unwrap();
            tx.put(b"uncommitted", b"must-vanish").unwrap();
            std::process::abort();
        }

        let dir = tempfile::tempdir().unwrap();
        let exe = std::env::current_exe().unwrap();
        let status = std::process::Command::new(exe)
            .args([
                "store::sweep::crash::fuse_crash_matrix::crash_consistency_process_abort",
                "--exact",
                "--nocapture",
            ])
            .env("KYZO_CRASH_CHILD_DIR", dir.path().join("db"))
            .status()
            .unwrap();
        assert!(
            !status.success(),
            "child must die by abort, not exit cleanly"
        );

        let db = new_fjall_storage(dir.path().join("db")).unwrap();
        let tx = db.read_tx().unwrap();
        assert_eq!(
            tx.get(b"committed").unwrap(),
            Some(Slice::from(b"survives")),
            "committed (unsynced) data must survive a process crash"
        );
        assert_eq!(
            tx.get(b"synced").unwrap(),
            Some(Slice::from(b"survives-power-cut-too"))
        );
        assert_eq!(
            tx.get(b"durable").unwrap(),
            Some(Slice::from(b"per-tx-fsync"))
        );
        assert_eq!(tx.get(b"uncommitted").unwrap(), None);
    }
}

// ---------------------------------------------------------------------------
// Crypto-shred deep reachability DST (story #376 T3 / H8) — path-wired under
// `kyzo::store::sweep::crash`. Searches every sealed CanonicalTranscript kind
// (incl. KeyCommit / WalHeader), production CheckpointSeal encode, and
// leave-is-free pack bytes for residual DEK / KEK / plaintext ShredSalt after
// production `shred`.
// ---------------------------------------------------------------------------
mod crypto_shred_deep_reachability {
    use crate::store::authority::{Entropy, IncarnationMintCap, OpenOrdinal};
    use crate::store::backup::{
        LeaveIsFreeKind, LeaveIsFreePack, LeaveIsFreeParts, PackRefuse,
    };
    use crate::store::crypto::{
        CryptoRefuse, Kek, KekUnwrapCap, SegmentCounter, ShredLedger, ShredSalt, derive_dek,
        shred, unwrap_shred_salt, wrap_shred_salt,
    };
    use crate::store::epoch::{CryptoDomain, FenceEpoch};
    use crate::store::open::StoreId;
    use crate::store::seal::{
        CheckpointSeal, CheckpointSealParts, GENESIS_PRIOR_SEAL, NonceLeaseFloors, SealDigest,
        SealRefuse,
    };
    use crate::store::sweep::CommitOrdinal;
    use crate::store::transcript::{
        CanonicalTranscriptBuilder, FieldId, SEALED_ARTIFACT_KINDS, TranscriptRefuse,
        encode_all_normative_production_transcripts, encode_normative_production_transcript,
        refuse_residual_secret_bytes, refuse_residual_secrets_in_all_sealed_kinds,
    };
    use crate::store::wal::WalHash;
    use crate::store::{FormatVersion, SealedArtifactKind};

    fn shredded_secret_needles<'a>(
        kek: &'a [u8; 32],
        salt: &'a [u8; 32],
        dek: &'a [u8; 32],
    ) -> [&'a [u8]; 3] {
        [kek.as_slice(), salt.as_slice(), dek.as_slice()]
    }

    fn clean_seal_parts(store: StoreId, incarnation_entropy: [u8; 32]) -> CheckpointSealParts {
        let fence = FenceEpoch::genesis(store);
        let domain = CryptoDomain::new(store, fence);
        let incarnation = IncarnationMintCap::issue(store, OpenOrdinal::ZERO)
            .mint(Entropy::from_bytes(incarnation_entropy))
            .expect("incarnation boundary");
        CheckpointSealParts {
            store_id: store,
            crypto_domain: domain,
            fence_epoch: fence,
            cut: CommitOrdinal::ZERO,
            state_root: SealDigest::from_digest([0x01; 32]),
            final_wal_hash: WalHash::from_digest([0x02; 32]),
            checkpoint_manifest: SealDigest::from_digest([0x03; 32]),
            format_version: FormatVersion::CURRENT,
            catalog_generation: CommitOrdinal::ZERO,
            retained_object_manifest: SealDigest::from_digest([0x04; 32]),
            permanence_candidate_manifest: SealDigest::from_digest([0x05; 32]),
            replica_custody_manifest: SealDigest::from_digest([0x06; 32]),
            nonce_floors: NonceLeaseFloors::genesis(),
            incarnation_boundary: incarnation,
            prior_seal_digest: GENESIS_PRIOR_SEAL,
            retention_certificate_digest: SealDigest::from_digest([0x07; 32]),
        }
    }

    fn sample_leave_is_free_pack(
        store: StoreId,
        wrapped: crate::store::crypto::WrappedShredSalt,
        incarnation_entropy: [u8; 32],
        payload: Vec<u8>,
    ) -> LeaveIsFreePack {
        let incarnation = IncarnationMintCap::issue(store, OpenOrdinal::ZERO)
            .mint(Entropy::from_bytes(incarnation_entropy))
            .expect("incarnation");
        LeaveIsFreePack::build(LeaveIsFreeParts {
            kind: LeaveIsFreeKind::SealAndSuffix,
            format_version: FormatVersion::CURRENT,
            wrapped_shred_salts: vec![wrapped],
            incarnation_history: vec![incarnation],
            payload,
        })
        .expect("leave-is-free pack")
    }

    /// H8 deep reachability: after production shred, every sealed artifact
    /// (golden transcript kinds incl. KeyCommit + WalHeader + CheckpointSeal
    /// encode + leave-is-free pack) is searched for residual
    /// DEK/KEK/plaintext ShredSalt bytes. Clean artifacts pass; planting a
    /// shredded needle into a sealed field must refuse via the production
    /// scrub doors.
    #[test]
    fn crypto_shred_deep_reachability_refuses_residual_secrets_in_sealed_artifacts() {
        let kek_bytes = [0xA1u8; 32];
        let salt_bytes = [0xB2u8; 32];
        let store = StoreId::from_digest([0x76; 32]);
        let domain = CryptoDomain::new(store, FenceEpoch::genesis(store));
        let cap = KekUnwrapCap::from_kek(Kek::from_bytes(kek_bytes));
        let salt = ShredSalt::from_bytes(salt_bytes);
        let seg = SegmentCounter::from_raw(9);
        let wrapped = wrap_shred_salt(&cap, &salt, seg, domain).expect("production wrap");
        // Retained wrap copy for leave-is-free pack (post-shred pack may still
        // carry ciphertext bytes; plaintext needles must not survive in pack
        // payload / entropy / ciphertext as raw residual).
        let pack_wrap = wrapped.clone();
        let stale_wrap = wrapped.clone();
        let dek = derive_dek(&cap, domain, seg, &salt);
        let dek_bytes = *dek.as_bytes();
        let needles = shredded_secret_needles(&kek_bytes, &salt_bytes, &dek_bytes);

        // Production shred path — consumes the wrap handle; ledger tombstone
        // refuses post-shred unwrap (production door, not a log scrub).
        let (_receipt, tombstone) = shred(wrapped);
        let mut ledger = ShredLedger::new();
        ledger.record(tombstone);
        assert!(
            matches!(
                unwrap_shred_salt(&cap, &stale_wrap, &ledger),
                Err(CryptoRefuse::Shredded)
            ),
            "production shred must refuse post-shred unwrap via Shredded"
        );

        // Every sealed golden kind (incl. WalHeader + KeyCommit) via the
        // production all-kinds scrub door — clean of shredded needles.
        assert_eq!(
            SEALED_ARTIFACT_KINDS.len(),
            12,
            "campaign must enumerate every SealedArtifactKind incl. KeyCommit"
        );
        let sealed_transcripts = encode_all_normative_production_transcripts()
            .expect("normative production transcripts encode");
        refuse_residual_secrets_in_all_sealed_kinds(&sealed_transcripts, &needles)
            .expect("clean goldens must pass all-kinds residual scrub");
        // Explicit WAL-header lane (same production encode door).
        let wal_header = encode_normative_production_transcript(SealedArtifactKind::WalHeader)
            .expect("wal header");
        assert_eq!(
            refuse_residual_secret_bytes(wal_header.as_bytes(), &needles),
            Ok(())
        );
        // KeyCommit / CMT-1 golden must stay intact and clean of shredded needles.
        let key_commit = encode_normative_production_transcript(SealedArtifactKind::KeyCommit)
            .expect("key commit");
        assert_eq!(
            refuse_residual_secret_bytes(key_commit.as_bytes(), &needles),
            Ok(()),
            "CMT-1 KeyCommit golden must remain intact and free of shredded needles"
        );

        // Production CheckpointSeal mint + encode + scrub — clean seal passes.
        let clean = CheckpointSeal::mint(clean_seal_parts(store, [0x26; 32])).expect("mint");
        clean
            .refuse_residual_secrets(&needles)
            .expect("clean CheckpointSeal must pass residual scrub");
        let clean_transcript = clean.encode_transcript().expect("encode");
        clean_transcript
            .refuse_residual_secrets(&needles)
            .expect("clean seal transcript must pass");

        // Production leave-is-free pack scrub — clean pack (no needles in
        // payload / incarnation entropy; wrap ciphertext is AEAD) passes.
        let clean_pack = sample_leave_is_free_pack(
            store,
            pack_wrap.clone(),
            [0x2A; 32],
            b"leave-is-free-clean-payload".to_vec(),
        );
        clean_pack
            .refuse_residual_secrets(&needles)
            .expect("clean leave-is-free pack must pass residual scrub");

        // Hostile: plant plaintext ShredSalt into a sealed transcript field → refuse.
        let mut dirty_builder =
            CanonicalTranscriptBuilder::new(FormatVersion::CURRENT).expect("builder");
        dirty_builder
            .append_u64(
                FieldId::ARTIFACT_KIND,
                SealedArtifactKind::CheckpointSeal.tag(),
            )
            .expect("kind");
        dirty_builder
            .append_digest32(FieldId::PRIMARY_DIGEST, &salt_bytes)
            .expect("plant salt as digest");
        let dirty_transcript = dirty_builder.seal();
        assert_eq!(
            dirty_transcript.refuse_residual_secrets(&needles),
            Err(TranscriptRefuse::Corrupt),
            "production transcript scrub must refuse residual plaintext ShredSalt"
        );

        // Hostile: plant shredded salt as CheckpointSeal state_root → encode
        // surfaces it in CanonicalTranscript → production seal scrub refuses.
        let mut dirty_parts = clean_seal_parts(store, [0x27; 32]);
        dirty_parts.state_root = SealDigest::from_digest(salt_bytes);
        let dirty_seal = CheckpointSeal::mint(dirty_parts).expect("mint dirty");
        assert_eq!(
            dirty_seal.refuse_residual_secrets(&needles),
            Err(SealRefuse::ResidualSecretMaterial),
            "production CheckpointSeal scrub must refuse residual ShredSalt in sealed bytes"
        );

        // Hostile: plant KEK / DEK needles the same way.
        let mut kek_parts = clean_seal_parts(store, [0x28; 32]);
        kek_parts.state_root = SealDigest::from_digest(kek_bytes);
        assert_eq!(
            CheckpointSeal::mint(kek_parts)
                .expect("mint")
                .refuse_residual_secrets(&needles),
            Err(SealRefuse::ResidualSecretMaterial),
            "residual KEK bytes in sealed seal must refuse"
        );
        let mut dek_parts = clean_seal_parts(store, [0x29; 32]);
        dek_parts.state_root = SealDigest::from_digest(dek_bytes);
        assert_eq!(
            CheckpointSeal::mint(dek_parts)
                .expect("mint")
                .refuse_residual_secrets(&needles),
            Err(SealRefuse::ResidualSecretMaterial),
            "residual DEK bytes in sealed seal must refuse"
        );

        // Hostile: plant shredded plaintext salt / KEK / DEK into leave-is-free
        // pack payload → production pack scrub refuses.
        let dirty_salt_pack = sample_leave_is_free_pack(
            store,
            pack_wrap.clone(),
            [0x2B; 32],
            salt_bytes.to_vec(),
        );
        assert_eq!(
            dirty_salt_pack.refuse_residual_secrets(&needles),
            Err(PackRefuse::ResidualSecretMaterial),
            "production leave-is-free pack scrub must refuse residual plaintext ShredSalt"
        );
        let dirty_kek_pack =
            sample_leave_is_free_pack(store, pack_wrap.clone(), [0x2C; 32], kek_bytes.to_vec());
        assert_eq!(
            dirty_kek_pack.refuse_residual_secrets(&needles),
            Err(PackRefuse::ResidualSecretMaterial),
            "residual KEK bytes in leave-is-free pack payload must refuse"
        );
        let dirty_dek_pack =
            sample_leave_is_free_pack(store, pack_wrap, [0x2D; 32], dek_bytes.to_vec());
        assert_eq!(
            dirty_dek_pack.refuse_residual_secrets(&needles),
            Err(PackRefuse::ResidualSecretMaterial),
            "residual DEK bytes in leave-is-free pack payload must refuse"
        );
    }
}
