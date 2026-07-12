/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Shared live-mount lifecycle helpers: mount-capability detection, a
//! settle-then-proceed wait, and a `spawn_mount2` wrapper that treats a
//! failed mount as an environment property rather than an injector defect.
//!
//! Lifted out of this crate's own `tests/standalone_mount.rs` (phase 1's
//! proof) so a second consumer — `kyzo-core`'s crash-matrix harness (story
//! #31 phase 2), which drives a real `FjallStorage` through this crate's
//! [`crate::PassthroughFs`] — does not hand-copy the same mount/skip
//! dance. This is exactly the dependency edge phase 1's design doc
//! anticipated: a test harness depending on this crate, never the reverse.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Best-effort live-mount capability check: `/dev/fuse` must exist and be
/// openable, and a `fusermount`-family binary must be on `PATH` (the
/// pure-Rust mount path tries a raw `mount(2)` first and falls back to
/// shelling out to the setuid helper — see `fuser`'s `fuse_pure.rs`). Not a
/// guarantee a real mount will succeed (namespaces, seccomp, and AppArmor
/// profiles can all still refuse it) — actual mount success is re-checked
/// per test/campaign and reported honestly either way.
pub fn can_mount() -> bool {
    if !Path::new("/dev/fuse").exists() {
        return false;
    }
    if fs::metadata("/dev/fuse").is_err() {
        return false;
    }
    ["fusermount3", "fusermount"]
        .iter()
        .any(|bin| Command::new(bin).arg("-V").output().is_ok())
}

/// Mount `fs` at `mountpoint`, returning `None` (with a printed reason) if
/// this sandbox cannot mount — an environment limitation, never an
/// injector defect (the fault-decision logic is proven independently by
/// `src/fault.rs`'s unit tests, which run with no mount at all).
pub fn mount<FS: fuser::Filesystem + Send + 'static>(
    fs: FS,
    mountpoint: &Path,
) -> Option<fuser::BackgroundSession> {
    match fuser::spawn_mount2(fs, mountpoint, &fuser::Config::default()) {
        Ok(session) => Some(session),
        Err(e) => {
            eprintln!(
                "SKIPPED (environment limitation, not an injector defect): \
                 FUSE mount failed: {e}. This sandbox lacks live-mount \
                 capability (no /dev/fuse access, no user_allow_other, or \
                 policy-restricted mount(2)/fusermount)."
            );
            None
        }
    }
}

/// Give the kernel a moment to settle the mount before the first op; a
/// bare `spawn_mount2` return does not guarantee the mountpoint is already
/// resolvable by a fresh process — in practice it always is on Linux, but
/// this loop makes callers robust rather than racy.
pub fn wait_for_mount(mountpoint: &Path) {
    for _ in 0..50 {
        if fs::read_dir(mountpoint).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
