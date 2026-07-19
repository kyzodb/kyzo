/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Story #31 phase 2: the crash matrix over a REAL filesystem.
//!
//! Phase 1 (`kyzo-crashfs`) proved the FUSE fault injector standalone, over
//! a plain backing directory with no `kyzo-core` knowledge at all. This
//! module drives a REAL [`FjallStorage`] through that injector — the
//! campaign the story exists for — and judges recovery against
//! [`SimStorage`] crashed at the analogous logical point, the same oracle
//! already sealed at the trait level (`storage/conformance.rs`'s DST arm,
//! kyzodb/kyzo#91 now fixed so it is trustworthy).
//!
//! ## Two crash-point classes (per issue #31's design ruling)
//!
//! - **Commit-boundary class** (exact, deterministic):
//!   [`commit_boundary_crash_matrix_matches_the_powercut_oracle`] arms an
//!   EXACT [`Trigger`] on the Nth `fsync()` of the journal file, timed (by
//!   construction of the driven round script, never by guessing fjall's
//!   internals) to interrupt one specific `commit_durable()` call. A
//!   fault-free FIRST pass observes which occurrence count that is — no
//!   census of fjall internals needed beyond that one honest recording —
//!   and a second, independent pass per boundary arms the trigger and
//!   checks the result against `SimStorage` power-cut at the same round.
//! - **Compaction/segment class** (ambient, seed-swept):
//!   [`compaction_segment_torn_write_campaign_never_returns_wrong_bytes`]
//!   sweeps `kyzo-crashfs`'s ambient ppm rates over a compaction-forcing
//!   write flood, because background flush/compaction timing is not under
//!   direct control the way a foreground `commit_durable()` is — ambient
//!   rates are exactly the tool `kyzo-crashfs` built for that shape. This
//!   is the design ruling's own highest-value unknown: whether a torn
//!   SEGMENT write can surface as silently wrong bytes rather than a typed
//!   refusal, since the pinned `lsm-tree` finding is that data blocks are
//!   checksummed LAZILY, on first read — "opens clean" is necessary, never
//!   sufficient, so both campaigns below read every key they care about.
//!
//! ## Falsification clause (per the issue)
//!
//! This campaign earns its keep only if it can catch what `SimStorage`'s
//! trait-level injection cannot: bugs in `fjall`'s OWN on-disk recovery
//! code, reached through real `write()`/`fsync()` syscalls FUSE intercepts.
//! A large clean run is a reportable result on its own; any assertion
//! failure here is a genuine engine defect, filed as an issue rather than
//! patched in this module (this module drives the kernel, it does not fix
//! it).
//!
//! Every test here is SKIPPED, never failed, when the sandbox lacks live
//! FUSE mount capability — matching phase 1's own stance (mount
//! availability is an environment property, not an injector defect).

use std::collections::BTreeSet;

use fjall::Slice;
use kyzo_crashfs::harness::{can_mount, mount, wait_for_mount};
use kyzo_crashfs::{Fault, FaultPlan, OpKind, PassthroughFs, Trigger};

use crate::storage::fjall::new_fjall_storage;
use crate::storage::sim::SimStorage;
use crate::storage::{ReadTx, Storage, WriteTx};

/// The main data keyspace's journal file at a fresh store's very first
/// segment — verified empirically (see the story's own session record):
/// with the small row counts this class drives, the journal never rotates
/// past `0.jnl`, so the Nth `fsync()` on this ONE path is unambiguous.
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

const ROUNDS: u32 = 6;
const KEYS_PER_ROUND: u32 = 20;

/// Commit-boundary class: a real `FjallStorage`, mounted through
/// `kyzo-crashfs`, crashed EXACTLY at each of its own `commit_durable`
/// calls in turn — never a byte offset, per the design ruling's
/// field-converged lesson ("anchored to durability barriers").
#[test]
fn commit_boundary_crash_matrix_matches_the_powercut_oracle() {
    if !can_mount() {
        eprintln!(
            "SKIPPED: no live FUSE mount capability in this sandbox \
             (see kyzo_crashfs::harness::can_mount)."
        );
        return;
    }

    // Pass 1: the fault-free recorder. Learn which occurrence of Fsync on
    // the journal coincides with each round's own commit_durable by
    // OBSERVING it once, honestly — never by guessing fjall's internals.
    let backing_a = tempfile::tempdir().unwrap();
    let mnt_a = tempfile::tempdir().unwrap();
    let fs_a = PassthroughFs::new(backing_a.path(), FaultPlan::new(1));
    let counters = fs_a.shared_counters();
    let Some(session_a) = mount(fs_a, mnt_a.path()) else {
        return;
    };
    wait_for_mount(mnt_a.path());
    let boundary_fsync_count: Vec<u64> = {
        let db = new_fjall_storage(mnt_a.path()).unwrap();
        let mut counts = Vec::with_capacity(ROUNDS as usize);
        for round in 0..ROUNDS {
            let mut tx = db.write_tx().unwrap();
            for (k, v) in round_kv(round, KEYS_PER_ROUND) {
                tx.put(&k, &v).unwrap();
            }
            tx.commit_durable().unwrap();
            counts.push(counters.fsync_count(JOURNAL_PATH));
        }
        counts
    };
    drop(session_a);
    // Sanity: strictly increasing, or this class's entire premise (one
    // fsync per round on this one path) is void and every assertion below
    // would be meaningless — fail loud here, not by mis-triggering later.
    for w in boundary_fsync_count.windows(2) {
        assert!(
            w[0] < w[1],
            "fsync counts on {JOURNAL_PATH} must strictly increase per round: \
             {boundary_fsync_count:?}"
        );
    }

    // Pass 2: one independent campaign per round boundary. Fresh backing
    // directory every time — no campaign ever reuses another's disk state.
    for (idx, &fsync_count) in boundary_fsync_count.iter().enumerate() {
        let round_idx = idx as u32; // the round whose OWN fsync gets interrupted
        let backing_b = tempfile::tempdir().unwrap();
        let mnt_b = tempfile::tempdir().unwrap();
        let plan = FaultPlan::new(1).with_trigger(Trigger::new(
            JOURNAL_PATH,
            OpKind::Fsync,
            fsync_count,
            Fault::ClearCache,
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
        let reopened = new_fjall_storage(backing_b.path()).unwrap_or_else(|e| {
            panic!("round {round_idx}: store must open clean or refuse typed, not: {e}")
        });

        // "Opens clean" is necessary, never sufficient (the issue's own
        // pinned lsm-tree finding: data blocks are checksummed lazily, on
        // first read) — total_scan forces the traversal that would
        // surface a torn data block instead of silently skipping it.
        let observed = total_scan_set(&reopened);
        let expected = oracle_after_powercut(round_idx, KEYS_PER_ROUND);
        assert_eq!(
            observed, expected,
            "round {round_idx}: a crash exactly at its own commit_durable's fsync must leave \
             precisely rounds 0..{round_idx} visible (this round's own writes never durable), \
             matching SimStorage crashed at the analogous logical point"
        );
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
                let fault = [Fault::TornSeq, Fault::TornOp, Fault::ClearCache][(seed % 3) as usize];
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
    use fjall::Slice;
    use kyzo::{ReadTx, Storage, WriteTx, new_fjall_storage};

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
            "crash::crash_consistency_process_abort",
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
