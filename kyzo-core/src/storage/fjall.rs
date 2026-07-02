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
//!
//! The transaction species are distinct types: [`FjallReadTx`] cannot write
//! by construction, and committing a [`FjallWriteTx`] consumes it — writing
//! through a reader and committing twice are not errors, they are programs
//! that do not compile.

use std::ops::Bound;
use std::path::Path;

use fjall::{
    Conflict, KeyspaceCreateOptions, OptimisticTxDatabase, OptimisticTxKeyspace, OptimisticWriteTx,
    Readable, Snapshot,
};
use miette::{Result, bail, miette};

use crate::data::tuple::{Tuple, check_key_for_validity, extend_tuple_from_v};
use crate::data::value::ValidityTs;
use crate::storage::{ConflictError, FormatVersion, ReadTx, Storage, WriteTx};

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
            .insert(FORMAT_VERSION_KEY, FormatVersion::CURRENT.as_bytes())
            .map_err(|e| miette!("stamping format version: {e}"))?,
        Some(v) => {
            let found = FormatVersion::parse(v.as_ref())?;
            if found != FormatVersion::CURRENT {
                bail!(
                    "on-disk format version mismatch: store is {found}, this build reads {}",
                    FormatVersion::CURRENT,
                );
            }
        }
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
    type ReadTx = FjallReadTx;
    type WriteTx = FjallWriteTx;

    fn storage_kind(&self) -> &'static str {
        "fjall"
    }

    fn read_tx(&self) -> Result<FjallReadTx> {
        Ok(FjallReadTx {
            snap: self.db.read_tx(),
            ks: self.ks.clone(),
        })
    }

    fn write_tx(&self) -> Result<FjallWriteTx> {
        Ok(FjallWriteTx {
            tx: self
                .db
                .write_tx()
                .map_err(|e| miette!("fjall write tx: {e}"))?,
            ks: self.ks.clone(),
            db: self.db.clone(),
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

/// A read transaction: one consistent snapshot. Writing through it is not an
/// error path — the operations do not exist on this type.
pub struct FjallReadTx {
    snap: Snapshot,
    ks: OptimisticTxKeyspace,
}

/// A write transaction: a consistent snapshot plus a conflict-tracked write
/// set. Reads see the transaction's own writes; `commit` consumes the value,
/// so a committed transaction cannot be touched again by construction.
pub struct FjallWriteTx {
    tx: OptimisticWriteTx,
    ks: OptimisticTxKeyspace,
    db: OptimisticTxDatabase,
}

/// Both fjall read views (`Snapshot`, `OptimisticWriteTx`) speak `Readable`;
/// every read-side operation is written once against that.
fn raw_range<'a, R: Readable>(
    reader: &'a R,
    ks: &'a OptimisticTxKeyspace,
    lower: &[u8],
    upper: &[u8],
) -> impl Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a {
    // fjall clones bounds internally; borrowing here avoids two Vec
    // allocations per call on the skip scan's hottest path.
    let bounds: (Bound<&[u8]>, Bound<&[u8]>) = (Bound::Included(lower), Bound::Excluded(upper));
    reader.range::<&[u8], _>(ks, bounds).map(|guard| {
        let (k, v) = guard.into_inner().map_err(|e| miette!("fjall read: {e}"))?;
        Ok((k.to_vec(), v.to_vec()))
    })
}

fn read_get<R: Readable>(
    reader: &R,
    ks: &OptimisticTxKeyspace,
    key: &[u8],
) -> Result<Option<Vec<u8>>> {
    Ok(reader
        .get(ks, key)
        .map_err(|e| miette!("fjall read: {e}"))?
        .map(|v| v.to_vec()))
}

fn read_exists<R: Readable>(reader: &R, ks: &OptimisticTxKeyspace, key: &[u8]) -> Result<bool> {
    reader
        .contains_key(ks, key)
        .map_err(|e| miette!("fjall read: {e}"))
}

fn read_total_scan<'a, R: Readable>(
    reader: &'a R,
    ks: &'a OptimisticTxKeyspace,
) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a> {
    Box::new(reader.iter(ks).map(|guard| {
        let (k, v) = guard.into_inner().map_err(|e| miette!("fjall read: {e}"))?;
        Ok((k.to_vec(), v.to_vec()))
    }))
}

macro_rules! impl_read_tx {
    ($ty:ty, $reader:ident) => {
        impl ReadTx for $ty {
            fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
                read_get(&self.$reader, &self.ks, key)
            }

            fn exists(&self, key: &[u8]) -> Result<bool> {
                read_exists(&self.$reader, &self.ks, key)
            }

            fn range_scan<'a>(
                &'a self,
                lower: &[u8],
                upper: &[u8],
            ) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a> {
                Box::new(raw_range(&self.$reader, &self.ks, lower, upper))
            }

            fn range_skip_scan_tuple<'a>(
                &'a self,
                lower: &[u8],
                upper: &[u8],
                valid_at: ValidityTs,
            ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
                Box::new(SkipIterator {
                    reader: &self.$reader,
                    ks: &self.ks,
                    upper: upper.to_vec(),
                    valid_at,
                    next_bound: lower.to_vec(),
                })
            }

            fn total_scan<'a>(
                &'a self,
            ) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a> {
                read_total_scan(&self.$reader, &self.ks)
            }
        }
    };
}

impl_read_tx!(FjallReadTx, snap);
impl_read_tx!(FjallWriteTx, tx);

impl WriteTx for FjallWriteTx {
    fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        self.tx.insert(&self.ks, key, val);
        Ok(())
    }

    fn del(&mut self, key: &[u8]) -> Result<()> {
        self.tx.remove(&self.ks, key);
        Ok(())
    }

    fn del_range(&mut self, lower: &[u8], upper: &[u8]) -> Result<()> {
        // Everything visible to this transaction in the range dies: snapshot
        // data and the transaction's own writes alike. Chunked with a
        // resuming cursor, so scratch memory is bounded and no pass re-walks
        // the tombstones of previous passes (a naive rescan-from-lower is
        // quadratic in range size). The write set itself necessarily holds
        // one tombstone per deleted key until commit.
        const CHUNK: usize = 1024;
        let mut cursor = lower.to_vec();
        loop {
            let keys: Vec<Vec<u8>> = raw_range(&self.tx, &self.ks, &cursor, upper)
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
            for k in keys {
                self.tx.remove(&self.ks, k);
            }
            if !full_chunk {
                return Ok(());
            }
        }
    }

    fn commit(self) -> Result<()> {
        match self.tx.commit().map_err(|e| miette!("fjall commit: {e}"))? {
            Ok(()) => Ok(()),
            Err(Conflict) => Err(ConflictError.into()),
        }
    }

    fn commit_durable(self) -> Result<()> {
        let db = self.db.clone();
        self.commit()?;
        db.persist(fjall::PersistMode::SyncAll)
            .map_err(|e| miette!("fjall sync: {e}"))
    }
}

/// Validity-aware skip scan: seek to the next candidate key, decide with
/// `check_key_for_validity`, re-seek at the bound it returns.
struct SkipIterator<'a, R: Readable> {
    reader: &'a R,
    ks: &'a OptimisticTxKeyspace,
    upper: Vec<u8>,
    valid_at: ValidityTs,
    next_bound: Vec<u8>,
}

impl<R: Readable> Iterator for SkipIterator<'_, R> {
    type Item = Result<Tuple>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.next_bound.as_slice() >= self.upper.as_slice() {
                return None;
            }
            let mut range = raw_range(self.reader, self.ks, &self.next_bound, &self.upper);
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
