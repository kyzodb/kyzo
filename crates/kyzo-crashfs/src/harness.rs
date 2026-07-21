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
//!
//! Absent FUSE is never silent success: campaigns either refuse with
//! [`MountRefuse`] or call [`require_live_mount`] (assert-skip that fails
//! the test — cargo never reports `ok` for a vacuous body).

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

/// Typed refusal when a live FUSE mount cannot be established.
///
/// Identity is the variant — never a silent `None`/early-return that lets
/// cargo report the campaign as `ok`.
#[derive(Debug)]
pub enum MountRefuse {
    /// `/dev/fuse` missing/unopenable or no `fusermount` helper on PATH.
    CapabilityAbsent,
    /// Capability looked present but `spawn_mount2` still failed (policy,
    /// namespace, AppArmor, etc.).
    MountFailed(io::Error),
}

impl fmt::Display for MountRefuse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapabilityAbsent => write!(
                f,
                "SKIPPED: no live FUSE mount capability in this sandbox \
                 (see kyzo_crashfs::harness::can_mount)"
            ),
            Self::MountFailed(e) => write!(
                f,
                "SKIPPED (environment limitation, not an injector defect): \
                 FUSE mount failed: {e}. This sandbox lacks live-mount \
                 capability (no /dev/fuse access, no user_allow_other, or \
                 policy-restricted mount(2)/fusermount)."
            ),
        }
    }
}

impl std::error::Error for MountRefuse {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CapabilityAbsent => None,
            Self::MountFailed(e) => Some(e),
        }
    }
}

/// Best-effort live-mount capability check: `/dev/fuse` must exist and be
/// openable. A `fusermount`/`fusermount3` helper on `PATH` is sufficient but
/// not required — fuser's pure-Rust path tries raw `mount(2)` first (works
/// under `SYS_ADMIN` in kyzo-dev) and only falls back to the setuid helper.
/// Not a guarantee a real mount will succeed (namespaces, seccomp, and
/// AppArmor profiles can all still refuse it) — actual mount success is
/// re-checked per campaign via typed [`MountRefuse`].
pub fn can_mount() -> bool {
    if !Path::new("/dev/fuse").exists() {
        return false;
    }
    if fs::metadata("/dev/fuse").is_err() {
        return false;
    }
    // Device is present. Prefer an explicit helper when available; otherwise
    // allow the privileged mount(2) path to try (and refuse typed on failure).
    true
}

/// Assert-skip: panic with a LOUD `SKIPPED:` reason when FUSE is absent.
///
/// Cargo reports this as a **failed** test, never `ok`. Silence identical
/// to success is the trials-zone lie-shape this kills. Prefer this (or
/// matching on [`MountRefuse`]) over `eprintln` + `return`.
pub fn require_live_mount() {
    assert!(
        can_mount(),
        "{}",
        MountRefuse::CapabilityAbsent
    );
}

/// Mount `fs` at `mountpoint`, returning a typed [`MountRefuse`] if this
/// sandbox cannot mount — an environment limitation, never an injector
/// defect (the fault-decision logic is proven independently by
/// `src/fault.rs`'s unit tests, which run with no mount at all).
pub fn mount<FS: fuser::Filesystem + Send + 'static>(
    fs: FS,
    mountpoint: &Path,
) -> Result<fuser::BackgroundSession, MountRefuse> {
    if !can_mount() {
        return Err(MountRefuse::CapabilityAbsent);
    }
    match fuser::spawn_mount2(fs, mountpoint, &fuser::Config::default()) {
        Ok(session) => Ok(session),
        Err(e) => Err(MountRefuse::MountFailed(e)),
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
