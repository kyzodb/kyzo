/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The phase-1 deliverable: mount the injector over a real backing
//! directory and prove, with a plain `std::fs` writer and no knowledge of
//! `kyzo-crashfs` internals, that it implements the power-cut and
//! torn-write model — not just that the fault-decision logic is correct in
//! isolation (that half lives in `src/fault.rs`'s unit tests).
//!
//! Every test here mounts real FUSE (`/dev/fuse`, `fusermount`/
//! `fusermount3`). If the sandbox cannot mount — no `/dev/fuse`, no
//! `user_allow_other`, no setuid `fusermount` — the campaign **refuses**
//! via [`kyzo_crashfs::harness::require_live_mount`] (assert-skip) or
//! typed [`kyzo_crashfs::MountRefuse`]. Never `eprintln` + `return` that
//! lets cargo report `ok` for a vacuous body.
//! [`kyzo_crashfs::harness::can_mount`] is the single detector all three
//! tests share (moved into the library so `kyzo-core`'s crash-matrix
//! harness, story #31 phase 2, can reuse it rather than re-implementing
//! the same mount/skip dance).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use kyzo_crashfs::harness::{
    mount as harness_mount, require_live_mount, wait_for_mount, LiveMount,
};
use kyzo_crashfs::{Fault, FaultPlan, OpKind, PassthroughFs, Trigger};

/// This file's own thin wrapper: build the fault-injecting filesystem and
/// hand it to the shared [`harness::mount`](kyzo_crashfs::harness::mount).
fn mount(backing: &Path, mountpoint: &Path, plan: FaultPlan) -> LiveMount {
    harness_mount(PassthroughFs::new(backing, plan), mountpoint)
        .unwrap_or_else(|refuse| panic!("{refuse}"))
}

#[test]
fn clear_cache_implements_the_power_cut_model() {
    require_live_mount();
    let backing = tempfile::tempdir().expect("backing tempdir");
    let mnt = tempfile::tempdir().expect("mountpoint tempdir");

    // The 2nd Write on data.bin (the second write() call — the one issued
    // *without* a following fsync) triggers a power cut: whatever is
    // pending for that handle is wiped the instant that write completes.
    let plan = FaultPlan::new(0xC0FFEE).with_trigger(Trigger::new(
        "data.bin",
        OpKind::Write,
        2,
        Fault::ClearCache,
    ));

    let session = mount(backing.path(), mnt.path(), plan);
    wait_for_mount(mnt.path());

    let file_path = mnt.path().join("data.bin");
    {
        let mut f = File::create(&file_path).expect("create through mount");
        f.write_all(b"DURABLE-FSYNCED-PAYLOAD").expect("write #1");
        f.sync_all().expect("fsync #1 (durability barrier)");
        f.write_all(b"THIS-SHOULD-VANISH")
            .expect("write #2, never fsynced");
        // No fsync here — write #2 is the one the trigger wipes.
    } // close: drop the fd, matching a process about to be power-cut

    session.teardown();

    // "Unmount/remount the backing view": read the real backing file
    // directly, bypassing FUSE entirely — this is what a fresh process
    // reopening the store after a crash would actually see on disk.
    let backing_file = backing.path().join("data.bin");
    let mut observed = Vec::new();
    File::open(&backing_file)
        .expect("backing file must exist")
        .read_to_end(&mut observed)
        .expect("read backing file");

    assert_eq!(
        observed, b"DURABLE-FSYNCED-PAYLOAD",
        "the fsynced prefix must survive the power cut byte-for-byte, and \
         nothing past it may appear on disk"
    );
}

#[test]
fn torn_op_splits_a_write_at_the_seed_dictated_point() {
    require_live_mount();
    let backing = tempfile::tempdir().expect("backing tempdir");
    let mnt = tempfile::tempdir().expect("mountpoint tempdir");

    let payload = b"0123456789ABCDEF"; // 16 bytes, len - 1 = 15 possible split points
    let seed = 0xA5A5_1234;
    let expected_split = kyzo_crashfs::fault::decide_write_outcome(
        &FaultPlan::new(seed).with_trigger(Trigger::new(
            "data.bin",
            OpKind::Write,
            1,
            Fault::TornOp,
        )),
        "data.bin",
        0,
        payload.len() as u64,
        1,
    );
    let expected_split_at = match expected_split {
        kyzo_crashfs::WriteOutcome::Split { split_at } => split_at,
        other => panic!("TornOp trigger must decide Split, got {other:?}"),
    };

    let plan = FaultPlan::new(seed).with_trigger(Trigger::new(
        "data.bin",
        OpKind::Write,
        1,
        Fault::TornOp,
    ));
    let session = mount(backing.path(), mnt.path(), plan);
    wait_for_mount(mnt.path());

    let file_path = mnt.path().join("data.bin");
    {
        let mut f = File::create(&file_path).expect("create through mount");
        f.write_all(payload).expect("write that TornOp must split");
        f.sync_all()
            .expect("fsync so torn bytes reach the backing file");
    }
    session.teardown();

    let backing_file = backing.path().join("data.bin");
    let mut observed = Vec::new();
    File::open(&backing_file)
        .expect("backing file must exist")
        .read_to_end(&mut observed)
        .expect("read backing file");

    assert_eq!(
        observed,
        &payload[..expected_split_at as usize],
        "exactly the seed-dictated prefix must persist and nothing past the split point"
    );
    assert!(
        observed.len() < payload.len(),
        "a torn-op write must actually be shorter than the original — otherwise this test \
         would pass vacuously even if TornOp silently degraded to Clean"
    );
}

#[test]
fn read_your_own_write_survives_pre_fsync_through_the_live_mount() {
    // A live process must still see its own unsynced bytes through the
    // mount (ordinary page-cache semantics) — only a crash reveals the
    // durability boundary. This guards against an injector bug where the
    // read-overlay is missing and every read appears to silently rewind
    // to the last fsync even while the process is still running.
    require_live_mount();
    let backing = tempfile::tempdir().expect("backing tempdir");
    let mnt = tempfile::tempdir().expect("mountpoint tempdir");

    let plan = FaultPlan::new(1); // no triggers, no ambient faults: pure passthrough
    let session = mount(backing.path(), mnt.path(), plan);
    wait_for_mount(mnt.path());

    let file_path = mnt.path().join("data.bin");
    // read(true) is required here: a write-only fd (what File::create
    // gives you) is rejected by the kernel's own file table on a read()
    // syscall on ANY filesystem, FUSE or not — nothing to do with the
    // injector, so the client must open read+write to exercise read-your-
    // own-write at all.
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&file_path)
        .expect("create read+write through mount");
    f.write_all(b"unsynced-bytes")
        .expect("write, deliberately not fsynced");
    f.seek(SeekFrom::Start(0))
        .expect("seek back to read our own write");
    let mut observed = Vec::new();
    f.read_to_end(&mut observed)
        .expect("read through the same handle");
    assert_eq!(observed, b"unsynced-bytes");
    drop(f);
    session.teardown();
}
