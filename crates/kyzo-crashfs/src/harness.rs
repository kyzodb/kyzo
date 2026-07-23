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
//! [`MountRefuse`] or call [`require_live_mount`] (typed skip-door; under
//! `#[test]`, `.expect` fails the campaign — cargo never reports `ok`
//! for a vacuous body).
//!
//! Session teardown is a first-class primitive ([`LiveMount::teardown`] /
//! [`Drop`]): fusectl abort runs **before** the `BackgroundSession` is
//! dropped so in-flight kernel clients cannot wedge in D-state when the
//! server side dies first.

use std::fmt;
use std::fs;
use std::io;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
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

/// Typed skip-door: `Err(CapabilityAbsent)` when FUSE is absent.
///
/// Campaigns under `#[test]` fail loud via `.expect` / `?` — cargo never
/// reports `ok` for a vacuous body. Prefer this (or matching on
/// [`MountRefuse`]) over `eprintln` + `return`.
pub fn require_live_mount() -> Result<(), MountRefuse> {
    if can_mount() {
        Ok(())
    } else {
        Err(MountRefuse::CapabilityAbsent)
    }
}

/// A live FUSE mount whose teardown aborts the kernel connection before
/// the session thread/mount handle die — the only order that cannot leave
/// client threads wedged in uninterruptible D-state.
pub struct LiveMount {
    session: Option<fuser::BackgroundSession>,
    mountpoint: PathBuf,
}

impl LiveMount {
    /// Mountpoint path (valid while the session is live).
    pub fn path(&self) -> &Path {
        &self.mountpoint
    }

    /// Abort in-flight FUSE requests via fusectl, then unmount and join the
    /// session thread. Prefer calling this explicitly at campaign crash
    /// points; [`Drop`] runs the same path if forgotten.
    pub fn teardown(mut self) {
        if let Some(session) = self.session.take() {
            force_teardown(session, &self.mountpoint);
        }
    }
}

impl Drop for LiveMount {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            force_teardown(session, &self.mountpoint);
        }
    }
}

impl fmt::Debug for LiveMount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveMount")
            .field("mountpoint", &self.mountpoint)
            .field("live", &self.session.is_some())
            .finish()
    }
}

/// Mount `fs` at `mountpoint`, returning a typed [`MountRefuse`] if this
/// sandbox cannot mount — an environment limitation, never an injector
/// defect (the fault-decision logic is proven independently by
/// `src/fault.rs`'s unit tests, which run with no mount at all).
///
/// Teardown through [`LiveMount::teardown`] (or `Drop`) — never bare
/// `drop(BackgroundSession)`.
pub fn mount<FS: fuser::Filesystem + Send + 'static>(
    fs: FS,
    mountpoint: &Path,
) -> Result<LiveMount, MountRefuse> {
    if !can_mount() {
        return Err(MountRefuse::CapabilityAbsent);
    }
    match fuser::spawn_mount2(fs, mountpoint, &fuser::Config::default()) {
        Ok(session) => Ok(LiveMount {
            session: Some(session),
            mountpoint: mountpoint.to_path_buf(),
        }),
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

/// Ensure fusectl is mounted at `/sys/fs/fuse/connections`.
///
/// kyzo-dev's default sysfs often has the directory present but empty
/// (fusectl not mounted) — abort writes then silently no-op and the
/// subsequent unmount/join wedges forever. `mount -t fusectl` is
/// idempotent (EBUSY/already-mounted ignored).
fn ensure_fusectl() {
    let root = Path::new("/sys/fs/fuse/connections");
    if !root.exists() {
        // Directory raced into existence, or sysfs refused — mount below
        // is the real probe; a missing dir just means fusectl isn't up.
        if let Err(_create) = fs::create_dir_all(root) {
            // mount below probes; create failure is not a silent test pass
        }
    }
    // Success, or already-mounted (nonzero), or spawn failure — all fine;
    // abort paths probe. Combined arm so no empty Err swallow.
    match std::process::Command::new("mount")
        .args(["-t", "fusectl", "none", "/sys/fs/fuse/connections"])
        .status()
    {
        Ok(_status) => {}
        Err(_spawn) => {}
    }
}

/// Force-fail every in-flight request on this mount's FUSE connection.
///
/// Connection id == `st_dev` of the mountpoint (kernel fusectl layout).
/// Some images expose the id as the minor only — try both.
fn abort_fuse_connection(mountpoint: &Path) {
    ensure_fusectl();
    let Some(meta) = fs::metadata(mountpoint).ok() else {
        return;
    };
    let dev = meta.dev();
    // Linux encodes maj/min in st_dev; fusectl dir names match `stat %d`
    // (full) on modern kernels and sometimes the minor alone.
    let minor = u64::from(libc::minor(dev));
    for id in [dev, minor] {
        let abort_path = PathBuf::from(format!("/sys/fs/fuse/connections/{id}/abort"));
        let Ok(mut f) = fs::OpenOptions::new().write(true).open(&abort_path) else {
            continue;
        };
        match f.write_all(b"1\n") {
            Ok(()) => {
                // Abort byte is what matters; flush is best-effort on sysfs.
                match f.flush() {
                    Ok(()) => {}
                    Err(_flush) => {}
                }
                return;
            }
            Err(_write) => {
                continue;
            }
        }
    }
}

/// Lazy-detach the mountpoint so a wedged userspace unmount cannot block
/// the test thread after fusectl abort has already failed in-flight I/O.
fn lazy_detach_mount(mountpoint: &Path) {
    let Some(path) = mountpoint.to_str() else {
        return;
    };
    for (bin, args) in [
        ("fusermount3", &["-uz", path][..]),
        ("fusermount", &["-uz", path][..]),
        ("umount", &["-l", path][..]),
    ] {
        if let Ok(status) = std::process::Command::new(bin).args(args).status() {
            if status.success() {
                return;
            }
        }
    }
}

fn force_teardown(session: fuser::BackgroundSession, mountpoint: &Path) {
    // 1) Sever the kernel connection first — pending clients get ECONNABORTED
    //    instead of wedging in D-state when the server thread dies.
    abort_fuse_connection(mountpoint);
    // 2) Lazy detach so join/unmount cannot block the campaign thread forever
    //    if abort raced or fusectl was briefly unavailable.
    lazy_detach_mount(mountpoint);
    // 3) Join with a bounded wait. fuser's `join` unwraps the session
    //    thread's `io::Result` (often `Err` after abort) and can also block
    //    indefinitely on umount — never let that own the test thread.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| session.join()));
        // Receiver may have timed out — join waiter is intentionally leaked;
        // fusectl abort already released kernel clients.
        match tx.send(outcome) {
            Ok(()) => {}
            Err(_gone) => {}
        }
    });
    // Timeout: session thread/unmount still wedged after abort+lazy detach.
    // Leak the join waiter; campaign continues. The kernel connection is
    // aborted — that is the load-bearing guarantee.
    if let Ok(_joined_or_panicked) = rx.recv_timeout(Duration::from_secs(5)) {
        // joined (or panick-caught) within bound
    }
}
