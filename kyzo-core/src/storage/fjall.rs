/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The storage backend: `fjall`, a pure-Rust LSM key-value store.
//!
//! Contract mapping:
//! - **Ordered range scans** — fjall keyspaces are LSM trees iterated in raw
//!   byte order, which under the memcmp encoding equals semantic value order.
//! - **MVCC commit with conflict detection** — fjall's optimistic (SSI)
//!   transactions track every read and range; `commit()` surfaces `Conflict`
//!   as an error and abandons the write set.
//! - **Read-your-own-writes** — the write transaction overlays its write set
//!   over a consistent snapshot taken at creation.
//! - **Validity-in-key as-of scans** — a seek loop: each step opens a fresh
//!   range at the seek key computed by `check_key_for_validity`, touching one
//!   stored version per distinct tuple in the common case.

use std::ops::Bound;
use std::path::Path;

use fjall::{
    Conflict, KeyspaceCreateOptions, OptimisticTxDatabase, OptimisticTxKeyspace, OptimisticWriteTx,
    Readable, Snapshot,
};
use miette::{Result, bail, miette};

use crate::data::tuple::{Tuple, check_key_for_validity, extend_tuple_from_v};
use crate::data::value::ValidityTs;
use crate::storage::{ConflictError, FORMAT_VERSION, Storage, StoreTx};

const KEYSPACE_NAME: &str = "kyzo";
const META_KEYSPACE_NAME: &str = "kyzo_meta";
const FORMAT_VERSION_KEY: &[u8] = b"format_version";

/// Resource configuration for a fjall store. A database engine does not get
/// to inherit invisible defaults: the knobs that govern memory and
/// parallelism are explicit, and `None` means fjall's documented default.
#[derive(Debug, Clone, Copy, Default)]
pub struct StorageOptions {
    /// Block/blob cache size in bytes.
    pub cache_size_bytes: Option<u64>,
    /// Background worker threads (flush/compaction).
    pub worker_threads: Option<usize>,
}

/// Point-in-time observability counters, straight from the storage engine.
#[derive(Debug, Clone, Copy)]
pub struct StorageStats {
    pub cache_size_bytes: u64,
    pub cache_capacity_bytes: u64,
    pub write_buffer_size_bytes: u64,
    pub active_compactions: usize,
    pub journal_count: usize,
}

/// Open (or create) a fjall-backed storage at the given path with default
/// options.
///
/// A fresh store is stamped with the on-disk format version; opening a store
/// written with a different format version is an error, not silent corruption.
pub fn new_fjall_storage(path: impl AsRef<Path>) -> Result<FjallStorage> {
    new_fjall_storage_with(path, StorageOptions::default())
}

/// Open (or create) a fjall-backed storage with explicit resource options.
pub fn new_fjall_storage_with(
    path: impl AsRef<Path>,
    opts: StorageOptions,
) -> Result<FjallStorage> {
    let mut builder = OptimisticTxDatabase::builder(path);
    if let Some(bytes) = opts.cache_size_bytes {
        builder = builder.cache_size(bytes);
    }
    if let Some(n) = opts.worker_threads {
        builder = builder.worker_threads(n);
    }
    let db = builder
        .open()
        .map_err(|e| miette!("opening fjall database: {e}"))?;
    let meta = db
        .keyspace(META_KEYSPACE_NAME, KeyspaceCreateOptions::default)
        .map_err(|e| miette!("opening fjall meta keyspace: {e}"))?;
    match meta
        .get(FORMAT_VERSION_KEY)
        .map_err(|e| miette!("reading format version: {e}"))?
    {
        None => meta
            .insert(FORMAT_VERSION_KEY, FORMAT_VERSION)
            .map_err(|e| miette!("stamping format version: {e}"))?,
        Some(v) if v.as_ref() == FORMAT_VERSION => {}
        Some(v) => bail!(
            "on-disk format version mismatch: store is v{}, this build reads v{}",
            String::from_utf8_lossy(v.as_ref()),
            String::from_utf8_lossy(FORMAT_VERSION),
        ),
    }
    let ks = db
        .keyspace(KEYSPACE_NAME, KeyspaceCreateOptions::default)
        .map_err(|e| miette!("opening fjall keyspace: {e}"))?;
    Ok(FjallStorage { db, ks })
}

/// The fjall storage engine.
///
/// `Clone` is cheap and shallow: clones are handles to the **same store**,
/// sharing one commit oracle — SSI conflict detection spans all clones and
/// all threads. This is what makes handing a clone to each worker thread
/// correct.
#[derive(Clone)]
pub struct FjallStorage {
    db: OptimisticTxDatabase,
    ks: OptimisticTxKeyspace,
}

impl Storage for FjallStorage {
    type Tx = FjallTx;

    fn storage_kind(&self) -> &'static str {
        "fjall"
    }

    fn transact(&self, write: bool) -> Result<Self::Tx> {
        Ok(if write {
            FjallTx::Writer {
                tx: Some(Box::new(
                    self.db
                        .write_tx()
                        .map_err(|e| miette!("fjall write tx: {e}"))?,
                )),
                ks: self.ks.clone(),
                db: self.db.clone(),
            }
        } else {
            FjallTx::Reader {
                snap: self.db.read_tx(),
                ks: self.ks.clone(),
            }
        })
    }

    fn batch_put<'a>(
        &'a self,
        data: Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>,
    ) -> Result<()> {
        // Atomic chunks: each chunk is one transaction committed as a unit,
        // so an interrupted import leaves a clean prefix of the input rather
        // than a torn write, with bounded memory. Conflicts are impossible
        // under the exclusive-access precondition.
        const CHUNK: usize = 32_768;
        let mut data = data.peekable();
        while data.peek().is_some() {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| miette!("fjall write tx: {e}"))?;
            for pair in data.by_ref().take(CHUNK) {
                let (k, v) = pair?;
                tx.insert(&self.ks, k, v);
            }
            match tx.commit().map_err(|e| miette!("fjall commit: {e}"))? {
                Ok(()) => {}
                Err(Conflict) => {
                    // Under the exclusive-access precondition this is caller
                    // error, but it still surfaces as the typed conflict.
                    return Err(ConflictError.into());
                }
            }
        }
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        self.db
            .persist(fjall::PersistMode::SyncAll)
            .map_err(|e| miette!("fjall sync: {e}"))
    }
}

impl FjallStorage {
    /// Point-in-time engine counters (cache, write buffer, compactions,
    /// journal). Cheap; safe to poll.
    pub fn stats(&self) -> StorageStats {
        let inner = self.db.inner();
        StorageStats {
            cache_size_bytes: inner.cache_size(),
            cache_capacity_bytes: inner.cache_capacity(),
            write_buffer_size_bytes: inner.write_buffer_size(),
            active_compactions: inner.active_compactions(),
            journal_count: inner.journal_count(),
        }
    }
}

/// A fjall transaction: a consistent snapshot for readers, an optimistic
/// (SSI) write transaction for writers.
pub enum FjallTx {
    Reader {
        snap: Snapshot,
        ks: OptimisticTxKeyspace,
    },
    Writer {
        tx: Option<Box<OptimisticWriteTx>>,
        ks: OptimisticTxKeyspace,
        db: OptimisticTxDatabase,
    },
}

impl FjallTx {
    fn writer_mut(&mut self) -> Result<(&mut OptimisticWriteTx, &OptimisticTxKeyspace)> {
        match self {
            FjallTx::Reader { .. } => bail!("write in read transaction"),
            FjallTx::Writer { tx, ks, .. } => Ok((
                tx.as_mut()
                    .ok_or_else(|| miette!("transaction already committed"))?,
                ks,
            )),
        }
    }

    fn raw_range<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Result<impl Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a> {
        // fjall clones bounds internally; borrowing here avoids two Vec
        // allocations per call on the skip scan's hottest path.
        let bounds: (Bound<&[u8]>, Bound<&[u8]>) = (Bound::Included(lower), Bound::Excluded(upper));
        let iter = match self {
            FjallTx::Reader { snap, ks } => snap.range::<&[u8], _>(ks, bounds),
            FjallTx::Writer { tx, ks, .. } => tx
                .as_ref()
                .ok_or_else(|| miette!("transaction already committed"))?
                .range::<&[u8], _>(ks, bounds),
        };
        Ok(iter.map(|guard| {
            let (k, v) = guard.into_inner().map_err(|e| miette!("fjall read: {e}"))?;
            Ok((k.to_vec(), v.to_vec()))
        }))
    }
}

impl StoreTx for FjallTx {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let res = match self {
            FjallTx::Reader { snap, ks } => snap.get(ks, key),
            FjallTx::Writer { tx, ks, .. } => tx
                .as_ref()
                .ok_or_else(|| miette!("transaction already committed"))?
                .get(ks, key),
        };
        Ok(res
            .map_err(|e| miette!("fjall read: {e}"))?
            .map(|v| v.to_vec()))
    }

    fn exists(&self, key: &[u8]) -> Result<bool> {
        let res = match self {
            FjallTx::Reader { snap, ks } => snap.contains_key(ks, key),
            FjallTx::Writer { tx, ks, .. } => tx
                .as_ref()
                .ok_or_else(|| miette!("transaction already committed"))?
                .contains_key(ks, key),
        };
        res.map_err(|e| miette!("fjall read: {e}"))
    }

    fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        let (tx, ks) = self.writer_mut()?;
        tx.insert(ks, key, val);
        Ok(())
    }

    fn del(&mut self, key: &[u8]) -> Result<()> {
        let (tx, ks) = self.writer_mut()?;
        tx.remove(ks, key);
        Ok(())
    }

    fn del_range(&mut self, lower: &[u8], upper: &[u8]) -> Result<()> {
        // Everything visible to this transaction in the range dies: snapshot
        // data and the transaction's own writes alike. Chunked with a resuming cursor, so
        // scratch memory is bounded and no pass re-walks the tombstones of
        // previous passes (a naive rescan-from-lower is quadratic in range
        // size). The write set itself necessarily holds one tombstone per
        // deleted key until commit.
        const CHUNK: usize = 1024;
        let mut cursor = lower.to_vec();
        loop {
            let keys: Vec<Vec<u8>> = self
                .raw_range(&cursor, upper)?
                .map(|kv| kv.map(|(k, _)| k))
                .take(CHUNK)
                .collect::<Result<_>>()?;
            let Some(last) = keys.last() else {
                return Ok(());
            };
            cursor = {
                let mut succ = last.clone();
                succ.push(0);
                succ
            };
            let full_chunk = keys.len() == CHUNK;
            let (tx, ks) = self.writer_mut()?;
            for k in keys {
                tx.remove(ks, k);
            }
            if !full_chunk {
                return Ok(());
            }
        }
    }

    fn commit(&mut self) -> Result<()> {
        match self {
            FjallTx::Reader { .. } => Ok(()),
            FjallTx::Writer { tx, .. } => {
                let tx = tx
                    .take()
                    .ok_or_else(|| miette!("transaction already committed"))?;
                match tx.commit().map_err(|e| miette!("fjall commit: {e}"))? {
                    Ok(()) => Ok(()),
                    Err(Conflict) => Err(ConflictError.into()),
                }
            }
        }
    }

    fn commit_durable(&mut self) -> Result<()> {
        self.commit()?;
        match self {
            FjallTx::Reader { .. } => Ok(()),
            FjallTx::Writer { db, .. } => db
                .persist(fjall::PersistMode::SyncAll)
                .map_err(|e| miette!("fjall sync: {e}")),
        }
    }

    fn range_scan<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a> {
        match self.raw_range(lower, upper) {
            Ok(iter) => Box::new(iter),
            Err(e) => Box::new(std::iter::once(Err(e))),
        }
    }

    fn range_skip_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
        valid_at: ValidityTs,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        Box::new(FjallSkipIterator {
            tx: self,
            upper: upper.to_vec(),
            valid_at,
            next_bound: lower.to_vec(),
        })
    }

    fn total_scan<'a>(&'a self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a> {
        let iter = match self {
            FjallTx::Reader { snap, ks } => snap.iter(ks),
            FjallTx::Writer { tx, ks, .. } => match tx.as_ref() {
                Some(tx) => tx.iter(ks),
                None => {
                    return Box::new(std::iter::once(Err(miette!(
                        "transaction already committed"
                    ))));
                }
            },
        };
        Box::new(iter.map(|guard| {
            let (k, v) = guard.into_inner().map_err(|e| miette!("fjall read: {e}"))?;
            Ok((k.to_vec(), v.to_vec()))
        }))
    }
}

/// Validity-aware skip scan: seek to the next candidate key, decide with
/// `check_key_for_validity`, re-seek at the bound it returns.
struct FjallSkipIterator<'a> {
    tx: &'a FjallTx,
    upper: Vec<u8>,
    valid_at: ValidityTs,
    next_bound: Vec<u8>,
}

impl Iterator for FjallSkipIterator<'_> {
    type Item = Result<Tuple>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.next_bound.as_slice() >= self.upper.as_slice() {
                return None;
            }
            let mut range = match self.tx.raw_range(&self.next_bound, &self.upper) {
                Ok(r) => r,
                Err(e) => return Some(Err(e)),
            };
            let (k, v) = match range.next() {
                None => return None,
                Some(Err(e)) => return Some(Err(e)),
                Some(Ok(kv)) => kv,
            };
            drop(range);
            let (ret, nxt_bound) = match check_key_for_validity(&k, self.valid_at, None) {
                Ok(pair) => pair,
                Err(e) => return Some(Err(e)),
            };
            // Termination guarantee: the seek bound must advance STRICTLY
            // past the key just examined, whatever the stored bytes contain.
            // A stored retraction at ts == i64::MIN collides with the
            // TERMINAL_VALIDITY sentinel and would otherwise re-seek to the
            // same key forever; hostile keys must not be able to livelock a
            // scan either. The byte-successor of k (k ++ 0x00) is the
            // smallest key strictly greater than k.
            self.next_bound = if nxt_bound.as_slice() > k.as_slice() {
                nxt_bound
            } else {
                let mut succ = k.clone();
                succ.push(0);
                succ
            };
            if let Some(mut tup) = ret {
                if let Err(e) = extend_tuple_from_v(&mut tup, &v) {
                    return Some(Err(e));
                }
                return Some(Ok(tup));
            }
        }
    }
}
