/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The passthrough filesystem itself: every op forwards to a backing
//! directory except where the [`FaultPlan`](crate::fault::FaultPlan) says
//! to corrupt.
//!
//! ## The write-buffer model (why this is not a one-line `pwrite` shim)
//!
//! A plain passthrough that calls `pwrite` on every `write()` has nothing
//! for a fault to act on: the bytes are already on the backing filesystem
//! the instant the syscall returns, indistinguishable from durable. The
//! LazyFS model this crate implements requires an actual write-buffer tier
//! standing in for the OS page cache: `write()` only appends to an
//! in-memory `pending` list on the open [`Handle`]; a read through the same
//! handle overlays that list atop the backing file's real bytes (so a live
//! writer sees exactly what it wrote — ordinary page-cache read-your-write
//! semantics); and only `fsync()` walks `pending` and actually lands bytes
//! on the backing file, one entry at a time, each subject to whatever
//! [`WriteOutcome`](crate::fault::WriteOutcome) was decided for it back
//! when it was buffered. A crash between two `fsync`s is exactly "the
//! `pending` list for every open handle evaporates" — which is what
//! [`Fault::ClearCache`](crate::fault::Fault::ClearCache) does directly,
//! and what a torn `fsync` (some pending entries `Dropped` or `Split`)
//! approximates for the entries that *were* being flushed.
//!
//! `FOPEN_DIRECT_IO` is set on every open file so every read/write syscall
//! reaches this filesystem — the kernel page cache never shortcuts around
//! the injector and silently serves a cached page instead.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
};

use crate::fault::{
    Counters, Fault, FaultPlan, OpKind, WriteOutcome, decide_write_outcome, resolve_trigger,
};

const ROOT_INO: u64 = 1;
const TTL_ZERO: Duration = Duration::from_secs(0);

struct PendingWrite {
    offset: u64,
    data: Vec<u8>,
    outcome: WriteOutcome,
}

struct Handle {
    rel_path: PathBuf,
    file: File,
    pending: Vec<PendingWrite>,
}

impl Handle {
    /// The file's length as a live reader through this handle would see
    /// it: the backing file's real length, extended by whatever pending
    /// writes reach past it. Pending writes never shrink the reported
    /// length (a real OS write-buffer doesn't retract a prior extend
    /// either), matching ordinary page-cache size semantics.
    fn logical_len(&self, backing_len: u64) -> u64 {
        self.pending
            .iter()
            .map(|pw| pw.offset + u64_from_usize(pw.data.len()))
            .fold(backing_len, u64::max)
    }
}

struct InodeTable {
    path_to_ino: HashMap<PathBuf, u64>,
    ino_to_path: HashMap<u64, PathBuf>,
    next_ino: u64,
}

impl InodeTable {
    fn new() -> Self {
        let mut t = InodeTable {
            path_to_ino: HashMap::new(),
            ino_to_path: HashMap::new(),
            next_ino: ROOT_INO + 1,
        };
        t.path_to_ino.insert(PathBuf::new(), ROOT_INO);
        t.ino_to_path.insert(ROOT_INO, PathBuf::new());
        t
    }

    fn ino_for(&mut self, rel: &Path) -> u64 {
        if let Some(&ino) = self.path_to_ino.get(rel) {
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.path_to_ino.insert(rel.to_path_buf(), ino);
        self.ino_to_path.insert(ino, rel.to_path_buf());
        ino
    }

    fn path_for(&self, ino: u64) -> Option<PathBuf> {
        self.ino_to_path.get(&ino).cloned()
    }

    /// Migrate an inode's recorded path after a successful backing-file
    /// rename. A no-op if `old` was never looked up (no inode minted for
    /// it yet) — a later `lookup` on `new` simply mints a fresh one, which
    /// is fine, since nothing upstream is holding the stale inode number.
    fn rename(&mut self, old: &Path, new: &Path) {
        if let Some(ino) = self.path_to_ino.remove(old) {
            self.path_to_ino.insert(new.to_path_buf(), ino);
            self.ino_to_path.insert(ino, new.to_path_buf());
        }
    }
}

/// Typed refuse when campaign-side counter sampling hits a poisoned mutex.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultCountersRefuse {
    /// Counters mutex poisoned after a panic under lock — never into_inner continue.
    LockPoisoned,
}

impl fmt::Display for FaultCountersRefuse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LockPoisoned => write!(
                f,
                "FaultCountersLockPoisoned: passthrough counters mutex poisoned"
            ),
        }
    }
}

impl std::error::Error for FaultCountersRefuse {}

/// A shareable read handle onto one mount's live [`Counters`], obtained via
/// [`PassthroughFs::shared_counters`] *before* the filesystem is handed to
/// `fuser::spawn_mount2` (which takes it by value) — the seam a campaign
/// driver needs to sample "how many fsyncs has this path seen so far" while
/// a mount is running, without owning the filesystem itself.
#[derive(Clone)]
pub struct FaultCounters(Arc<Mutex<Counters>>);

impl FaultCounters {
    /// The current fsync count for `rel_path` (`0` if it has never fired).
    pub fn fsync_count(&self, rel_path: &str) -> Result<u64, FaultCountersRefuse> {
        self.0
            .lock()
            .map(|c| c.count(rel_path, OpKind::Fsync))
            .map_err(|_| FaultCountersRefuse::LockPoisoned)
    }

    /// The current write count for `rel_path` (`0` if it has never fired).
    pub fn write_count(&self, rel_path: &str) -> Result<u64, FaultCountersRefuse> {
        self.0
            .lock()
            .map(|c| c.count(rel_path, OpKind::Write))
            .map_err(|_| FaultCountersRefuse::LockPoisoned)
    }
}

/// The FUSE passthrough fault injector. One instance per mount; `plan`
/// (and therefore every fault decision made through it) is fixed for the
/// instance's lifetime — a fresh campaign is a fresh instance.
pub struct PassthroughFs {
    backing_root: PathBuf,
    plan: FaultPlan,
    inodes: Mutex<InodeTable>,
    handles: Mutex<HashMap<u64, Handle>>,
    next_fh: AtomicU64,
    counters: Arc<Mutex<Counters>>,
}

fn io_errno(err: std::io::Error) -> Errno {
    err.into()
}

fn lock<'a, T>(m: &'a Mutex<T>) -> Result<std::sync::MutexGuard<'a, T>, Errno> {
    m.lock().map_err(|_| Errno::EIO)
}

/// FUSE success reply — `Reply*::ok` is the kernel-success door, not `Result::ok`.
macro_rules! fuse_ok {
    ($reply:expr) => {
        $reply.ok()
    };
}

fn usize_from_u64(n: u64) -> Result<usize, Errno> {
    usize::try_from(n).map_err(|_| Errno::EINVAL)
}

/// Lossless `usize` → `u64` via little-endian assemble.
fn u64_from_usize(n: usize) -> u64 {
    let src = n.to_le_bytes();
    let mut buf = [0u8; 8];
    buf[..src.len()].copy_from_slice(&src);
    u64::from_le_bytes(buf)
}

/// FUSE `u32` field — refuse typed when the host value exceeds wire width.
fn u32_from_u64(n: u64) -> Result<u32, Errno> {
    u32::try_from(n).map_err(|_| Errno::EOVERFLOW)
}

/// Mode bits already masked to `0o7777` — always fits `u16`; LE assemble.
fn u16_from_mode_bits(mode: u32) -> u16 {
    let masked = mode & 0o7777;
    let b = masked.to_le_bytes();
    u16::from_le_bytes([b[0], b[1]])
}

fn u32_from_usize(n: usize) -> Result<u32, Errno> {
    u32::try_from(n).map_err(|_| Errno::EIO)
}

impl PassthroughFs {
    pub fn new(backing_root: impl Into<PathBuf>, plan: FaultPlan) -> Self {
        PassthroughFs {
            backing_root: backing_root.into(),
            plan,
            inodes: Mutex::new(InodeTable::new()),
            handles: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            counters: Arc::new(Mutex::new(Counters::fresh())),
        }
    }

    /// A cloneable read handle onto this instance's live fsync/write
    /// counters. Call this **before** handing `self` to
    /// `fuser::spawn_mount2` (which consumes it), so a campaign driver
    /// retains a way to sample counts while the mount is running — the
    /// two-pass crash-matrix design (`kyzo-core`'s `storage::crash_matrix`)
    /// records a fault-free run's counts this way, then arms an exact
    /// [`crate::fault::Trigger`] for a later, faulted run.
    pub fn shared_counters(&self) -> FaultCounters {
        FaultCounters(Arc::clone(&self.counters))
    }

    fn real_path(&self, rel: &Path) -> PathBuf {
        self.backing_root.join(rel)
    }

    fn rel_key(rel: &Path) -> String {
        rel.to_string_lossy().into_owned()
    }

    fn alloc_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::Relaxed)
    }

    fn attr_from_metadata(ino: u64, meta: &fs::Metadata, logical_size: u64) -> Result<FileAttr, Errno> {
        let kind = if meta.is_dir() {
            FileType::Directory
        } else if meta.file_type().is_symlink() {
            FileType::Symlink
        } else {
            FileType::RegularFile
        };
        Ok(FileAttr {
            ino: INodeNo(ino),
            size: logical_size,
            blocks: logical_size.div_ceil(512),
            atime: match meta.accessed() {
                Ok(t) => t,
                Err(_) => UNIX_EPOCH,
            },
            mtime: match meta.modified() {
                Ok(t) => t,
                Err(_) => UNIX_EPOCH,
            },
            ctime: match meta.modified() {
                Ok(t) => t,
                Err(_) => UNIX_EPOCH,
            },
            crtime: match meta.created() {
                Ok(t) => t,
                Err(_) => UNIX_EPOCH,
            },
            kind,
            perm: u16_from_mode_bits(meta.mode()),
            nlink: u32_from_u64(meta.nlink())?,
            uid: meta.uid(),
            gid: meta.gid(),
            rdev: u32_from_u64(meta.rdev())?,
            blksize: 4096,
            flags: 0,
        })
    }

    fn stat_entry(&self, ino: u64, rel: &Path) -> Result<FileAttr, Errno> {
        let meta = fs::symlink_metadata(self.real_path(rel)).map_err(io_errno)?;
        Self::attr_from_metadata(ino, &meta, meta.len())
    }

    fn inodes_lock(&self) -> Result<std::sync::MutexGuard<'_, InodeTable>, Errno> {
        lock(&self.inodes)
    }

    fn handles_lock(&self) -> Result<std::sync::MutexGuard<'_, HashMap<u64, Handle>>, Errno> {
        lock(&self.handles)
    }

    fn counters_lock(&self) -> Result<std::sync::MutexGuard<'_, Counters>, Errno> {
        lock(&self.counters)
    }

    /// One door for `rmdir` / `unlink`: resolve parent, apply `remove` on the child path.
    fn remove_child(
        &self,
        parent: INodeNo,
        name: &OsStr,
        reply: ReplyEmpty,
        remove: fn(&Path) -> std::io::Result<()>,
    ) {
        let Ok(inodes) = self.inodes_lock() else {
            reply.error(Errno::EIO);
            return;
        };
        let Some(parent_rel) = inodes.path_for(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match remove(&self.real_path(&parent_rel.join(name))) {
            Ok(()) => fuse_ok!(reply),
            Err(e) => reply.error(io_errno(e)),
        }
    }
}

impl Filesystem for PassthroughFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Ok(inodes) = self.inodes_lock() else {
            reply.error(Errno::EIO);
            return;
        };
        let Some(parent_rel) = inodes.path_for(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let child_rel = parent_rel.join(name);
        let ino = {
            let Ok(mut inodes) = self.inodes_lock() else {
                reply.error(Errno::EIO);
                return;
            };
            inodes.ino_for(&child_rel)
        };
        match self.stat_entry(ino, &child_rel) {
            Ok(attr) => reply.entry(&TTL_ZERO, &attr, Generation(0)),
            Err(errno) => reply.error(errno),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        let Ok(inodes) = self.inodes_lock() else {
            reply.error(Errno::EIO);
            return;
        };
        let Some(rel) = inodes.path_for(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let meta = match fs::symlink_metadata(self.real_path(&rel)) {
            Ok(m) => m,
            Err(e) => {
                reply.error(io_errno(e));
                return;
            }
        };
        let logical_size = match fh {
            Some(fh) => match self.handles_lock() {
                Ok(handles) => match handles.get(&fh.0) {
                    Some(h) => h.logical_len(meta.len()),
                    None => meta.len(),
                },
                Err(_) => {
                    reply.error(Errno::EIO);
                    return;
                }
            },
            None => meta.len(),
        };
        match Self::attr_from_metadata(ino.0, &meta, logical_size) {
            Ok(attr) => reply.attr(&TTL_ZERO, &attr),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let Ok(inodes) = self.inodes_lock() else {
            reply.error(Errno::EIO);
            return;
        };
        let Some(rel) = inodes.path_for(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if let Some(new_len) = size {
            // Truncation is a structural op, applied immediately to the
            // backing file (it is not part of the buffered-write/fsync
            // durability model this crate exists to fault-inject).
            if let Err(e) = (|| -> std::io::Result<()> {
                let f = OpenOptions::new().write(true).open(self.real_path(&rel))?;
                f.set_len(new_len)
            })() {
                reply.error(io_errno(e));
                return;
            }
            if let Some(fh) = fh {
                let Ok(mut handles) = self.handles_lock() else {
                    reply.error(Errno::EIO);
                    return;
                };
                if let Some(handle) = handles.get_mut(&fh.0) {
                    handle.pending.retain(|pw| pw.offset < new_len);
                }
            }
        }
        match self.stat_entry(ino.0, &rel) {
            Ok(attr) => reply.attr(&TTL_ZERO, &attr),
            Err(errno) => reply.error(errno),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Ok(inodes) = self.inodes_lock() else {
            reply.error(Errno::EIO);
            return;
        };
        let Some(parent_rel) = inodes.path_for(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let child_rel = parent_rel.join(name);
        let real = self.real_path(&child_rel);
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&real)
        {
            Ok(f) => f,
            Err(e) => {
                reply.error(io_errno(e));
                return;
            }
        };
        let Ok(mut inodes) = self.inodes_lock() else {
            reply.error(Errno::EIO);
            return;
        };
        let ino = inodes.ino_for(&child_rel);
        let attr = match self.stat_entry(ino, &child_rel) {
            Ok(a) => a,
            Err(errno) => {
                reply.error(errno);
                return;
            }
        };
        let fh = self.alloc_fh();
        {
            let Ok(mut handles) = self.handles_lock() else {
                reply.error(Errno::EIO);
                return;
            };
            handles.insert(
                fh,
                Handle {
                    rel_path: child_rel,
                    file,
                    pending: Vec::new(),
                },
            );
        }
        reply.created(
            &TTL_ZERO,
            &attr,
            Generation(0),
            FileHandle(fh),
            FopenFlags::FOPEN_DIRECT_IO,
        );
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: fuser::OpenFlags, reply: ReplyOpen) {
        let Ok(inodes) = self.inodes_lock() else {
            reply.error(Errno::EIO);
            return;
        };
        let Some(rel) = inodes.path_for(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.real_path(&rel))
        {
            Ok(f) => f,
            Err(e) => {
                reply.error(io_errno(e));
                return;
            }
        };
        let fh = self.alloc_fh();
        {
            let Ok(mut handles) = self.handles_lock() else {
                reply.error(Errno::EIO);
                return;
            };
            handles.insert(
                fh,
                Handle {
                    rel_path: rel,
                    file,
                    pending: Vec::new(),
                },
            );
        }
        reply.opened(FileHandle(fh), FopenFlags::FOPEN_DIRECT_IO);
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let Ok(handles) = self.handles_lock() else {
            reply.error(Errno::EIO);
            return;
        };
        let Some(handle) = handles.get(&fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        let want_end = offset + u64::from(size);
        let buf_len = match usize::try_from(size) {
            Ok(n) => n,
            Err(_) => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let mut buf = vec![0u8; buf_len];
        let mut filled = match handle.file.read_at(&mut buf, offset) {
            Ok(n) => n,
            Err(e) => {
                reply.error(io_errno(e));
                return;
            }
        };
        for pw in &handle.pending {
            let pw_end = pw.offset + u64_from_usize(pw.data.len());
            let ov_start = pw.offset.max(offset);
            let ov_end = pw_end.min(want_end);
            if ov_start < ov_end {
                let Ok(dst) = usize_from_u64(ov_start - offset) else {
                    reply.error(Errno::EINVAL);
                    return;
                };
                let Ok(src) = usize_from_u64(ov_start - pw.offset) else {
                    reply.error(Errno::EINVAL);
                    return;
                };
                let Ok(len) = usize_from_u64(ov_end - ov_start) else {
                    reply.error(Errno::EINVAL);
                    return;
                };
                buf[dst..dst + len].copy_from_slice(&pw.data[src..src + len]);
                filled = filled.max(dst + len);
            }
        }
        buf.truncate(filled);
        reply.data(&buf);
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        let Ok(mut handles) = self.handles_lock() else {
            reply.error(Errno::EIO);
            return;
        };
        let Some(handle) = handles.get_mut(&fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        let rel_key = Self::rel_key(&handle.rel_path);
        let count = {
            let Ok(mut counters) = self.counters_lock() else {
                reply.error(Errno::EIO);
                return;
            };
            counters.bump(&rel_key, OpKind::Write)
        };
        let outcome = decide_write_outcome(&self.plan, &rel_key, offset, u64_from_usize(data.len()), count);
        handle.pending.push(PendingWrite {
            offset,
            data: data.to_vec(),
            outcome,
        });
        // ClearCache is an immediate event, not a per-write outcome tag:
        // the power cut lands right after this write, wiping everything
        // buffered for this handle — including the write that triggered it.
        if resolve_trigger(&self.plan, &rel_key, OpKind::Write, count) == Some(Fault::ClearCache) {
            handle.pending.clear();
        }
        match u32_from_usize(data.len()) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(e),
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let Ok(mut handles) = self.handles_lock() else {
            reply.error(Errno::EIO);
            return;
        };
        let Some(handle) = handles.get_mut(&fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        let rel_key = Self::rel_key(&handle.rel_path);
        let count = {
            let Ok(mut counters) = self.counters_lock() else {
                reply.error(Errno::EIO);
                return;
            };
            counters.bump(&rel_key, OpKind::Fsync)
        };
        if resolve_trigger(&self.plan, &rel_key, OpKind::Fsync, count) == Some(Fault::ClearCache) {
            // The power cut lands at this fsync boundary: nothing queued
            // since the last real fsync reaches the backing file.
            handle.pending.clear();
            fuse_ok!(reply);
            return;
        }
        for pw in handle.pending.drain(..) {
            let write_result = match pw.outcome {
                WriteOutcome::Clean => handle.file.write_at(&pw.data, pw.offset),
                WriteOutcome::Dropped => Ok(0),
                WriteOutcome::Split { split_at } => {
                    let Ok(n) = usize_from_u64(split_at) else {
                        reply.error(Errno::EINVAL);
                        return;
                    };
                    handle.file.write_at(&pw.data[..n], pw.offset)
                }
            };
            if let Err(e) = write_result {
                reply.error(io_errno(e));
                return;
            }
        }
        if let Err(e) = handle.file.sync_all() {
            reply.error(io_errno(e));
            return;
        }
        fuse_ok!(reply);
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        // flush() (close(2)/dup path) carries no durability guarantee in
        // POSIX and none in this model either: only fsync materializes.
        fuse_ok!(reply);
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // A normal close does NOT lose unsynced writes on a real OS — the
        // data sits in the page cache and stays visible to any later
        // read/reopen (even by a fresh handle on the same path) regardless
        // of whether fsync was ever called; only an actual crash loses it.
        // This model's crash IS the whole mount session dying (every
        // STILL-open handle's `pending` drops with the `PassthroughFs`
        // instance) or an explicit ClearCache/TornOp/TornSeq decided back
        // in `write()`. Earlier this method just discarded `pending`
        // unconditionally on every ordinary close — which silently lost
        // data on EVERY release, fault-free runs included (caught the hard
        // way: a keyspace's own manifest bootstrap write, closed via
        // flush+release with no intervening fsync, came back permanently
        // empty even with no fault plan active at all). Fixed: drain and
        // materialize each write's ALREADY-DECIDED outcome (from `write()`
        // — there is no separate `Release` op kind, so no new fault
        // decision happens here), the same per-entry application `fsync`
        // performs, just without the `sync_all` (release claims no
        // power-cut durability, only "the bytes are on the backing file
        // now," matching a real close).
        let Ok(mut handles) = self.handles_lock() else {
            reply.error(Errno::EIO);
            return;
        };
        if let Some(mut handle) = handles.remove(&fh.0) {
            for pw in handle.pending.drain(..) {
                let write_result = match pw.outcome {
                    WriteOutcome::Clean => handle.file.write_at(&pw.data, pw.offset),
                    WriteOutcome::Dropped => Ok(0),
                    WriteOutcome::Split { split_at } => {
                        let Ok(n) = usize_from_u64(split_at) else {
                            continue;
                        };
                        handle.file.write_at(&pw.data[..n], pw.offset)
                    }
                };
                if let Err(e) = write_result {
                    reply.error(io_errno(e));
                    return;
                }
            }
        }
        fuse_ok!(reply);
    }

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: fuser::OpenFlags, reply: ReplyOpen) {
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Ok(inodes) = self.inodes_lock() else {
            reply.error(Errno::EIO);
            return;
        };
        let Some(rel) = inodes.path_for(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let real_dir = self.real_path(&rel);
        let entries = match fs::read_dir(&real_dir) {
            Ok(rd) => rd,
            Err(e) => {
                reply.error(io_errno(e));
                return;
            }
        };
        let mut all: Vec<(u64, FileType, std::ffi::OsString)> =
            vec![(ino.0, FileType::Directory, ".".into())];
        let parent_ino = if rel.as_os_str().is_empty() {
            ROOT_INO
        } else {
            let parent = match rel.parent() {
                Some(p) => p,
                None => Path::new(""),
            };
            let Ok(mut inodes) = self.inodes_lock() else {
                reply.error(Errno::EIO);
                return;
            };
            inodes.ino_for(parent)
        };
        all.push((parent_ino, FileType::Directory, "..".into()));
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    continue;
                }
            };
            let child_rel = rel.join(entry.file_name());
            let child_ino = {
                let Ok(mut inodes) = self.inodes_lock() else {
                    reply.error(Errno::EIO);
                    return;
                };
                inodes.ino_for(&child_rel)
            };
            let kind = if entry.path().is_dir() {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            all.push((child_ino, kind, entry.file_name()));
        }
        let Ok(skip) = usize_from_u64(offset) else {
            reply.error(Errno::EINVAL);
            return;
        };
        for (idx, (ino, kind, name)) in all.into_iter().enumerate().skip(skip) {
            let next_off = match u64::try_from(idx + 1) {
                Ok(n) => n,
                Err(_) => {
                    reply.error(Errno::EINVAL);
                    return;
                }
            };
            if reply.add(INodeNo(ino), next_off, kind, &name) {
                break;
            }
        }
        fuse_ok!(reply);
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let Ok(inodes) = self.inodes_lock() else {
            reply.error(Errno::EIO);
            return;
        };
        let Some(parent_rel) = inodes.path_for(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let child_rel = parent_rel.join(name);
        if let Err(e) = fs::create_dir(self.real_path(&child_rel)) {
            reply.error(io_errno(e));
            return;
        }
        let Ok(mut inodes) = self.inodes_lock() else {
            reply.error(Errno::EIO);
            return;
        };
        let ino = inodes.ino_for(&child_rel);
        match self.stat_entry(ino, &child_rel) {
            Ok(attr) => reply.entry(&TTL_ZERO, &attr, Generation(0)),
            Err(errno) => reply.error(errno),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.remove_child(parent, name, reply, |p| fs::remove_dir(p));
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.remove_child(parent, name, reply, |p| fs::remove_file(p));
    }

    /// Atomic rename — the pointer-swap pattern a real LSM keyspace uses to
    /// publish a new manifest/`current` file (write the new file, fsync it,
    /// then rename it over the old pointer): plain `fs::rename`, ignoring
    /// `flags` (`RENAME_NOREPLACE`/`RENAME_EXCHANGE`), since no scenario
    /// this injector drives requests them. Structural, like `mkdir`/
    /// `unlink` above — not part of the buffered-write/fsync durability
    /// model this crate faults, so it is never itself a fault target.
    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        let Ok(inodes) = self.inodes_lock() else {
            reply.error(Errno::EIO);
            return;
        };
        let Some(parent_rel) = inodes.path_for(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Ok(inodes) = self.inodes_lock() else {
            reply.error(Errno::EIO);
            return;
        };
        let Some(newparent_rel) = inodes.path_for(newparent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let old_rel = parent_rel.join(name);
        let new_rel = newparent_rel.join(newname);
        if let Err(e) = fs::rename(self.real_path(&old_rel), self.real_path(&new_rel)) {
            reply.error(io_errno(e));
            return;
        }
        let Ok(mut inodes) = self.inodes_lock() else {
            reply.error(Errno::EIO);
            return;
        };
        inodes.rename(&old_rel, &new_rel);
        fuse_ok!(reply);
    }

    /// Grant every lock/unlock request unconditionally. This injector's
    /// fault surface is write()/fsync() durability, never lock contention
    /// (every test scenario is single-writer), and `fuser`'s default reply
    /// here is `ENOSYS` — which real callers do hit: `fjall`'s own
    /// `LockedFileGuard::create_new` calls `File::try_lock()` on its `lock`
    /// file at every open, and a real database (phase 2's whole point) must
    /// be able to open through this mount at all.
    fn setlk(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: fuser::LockOwner,
        _start: u64,
        _end: u64,
        _typ: i32,
        _pid: u32,
        _sleep: bool,
        reply: ReplyEmpty,
    ) {
        fuse_ok!(reply);
    }
}
