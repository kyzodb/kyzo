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
//!   transactions track every read and range; at commit the oracle validates
//!   the READ set against transactions committed after this one's snapshot
//!   and surfaces `Conflict` as an error, abandoning the write set. fjall's
//!   own oracle validates reads only, so [`WriteTx::put`] and
//!   [`WriteTx::del`] additionally mark the written key READ (contract v2:
//!   writes are validated too — see `storage/mod.rs`, "Contract history"),
//!   which puts every written key on the validated surface and makes a
//!   write-write race abort its second committer. The marking read is a
//!   `contains_key` issued AFTER the write, so it resolves in the
//!   transaction's own memtable — no disk I/O — while still registering in
//!   the conflict manager. A commit with an empty write set returns before
//!   the oracle runs — it never aborts and certifies nothing about what was
//!   read.
//! - **Read-your-own-writes** — the write transaction overlays its write set
//!   over a consistent snapshot taken at creation.
//! - **Bitemporal as-of scans** — a seek loop: each step opens a fresh
//!   range at the seek key computed by `check_key_for_bitemporal` (the
//!   row's polarity peeked from its value), touching one stored version
//!   per distinct fact in the common case.
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

use crate::data::bitemporal::{
    check_key_for_bitemporal, claim_polarity_of_value, extend_tuple_from_bitemporal_v,
};
use crate::data::tuple::Tuple;
use crate::data::value::{AsOf, ValidityTs};
use crate::storage::{ConflictError, FormatVersion, ReadTx, Storage, SystemClock, WriteTx};

const KEYSPACE_NAME: &str = "kyzo";
const META_KEYSPACE_NAME: &str = "kyzo_meta";
const FORMAT_VERSION_KEY: &[u8] = b"format_version";
/// Meta-keyspace key holding the system clock's crash-recovery floor: the
/// highest stamp ever minted by this store, persisted non-transactionally
/// at each mint. On open the clock is seeded with `max(now, watermark)`,
/// so stamps stay monotone across restarts even under backward wall-clock
/// skew. A watermark ahead of the last COMMITTED stamp (an aborted
/// transaction's mint) is harmless: floors only need to be high enough.
const SYSTEM_CLOCK_WATERMARK_KEY: &[u8] = b"system_clock_watermark";

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
    let now = crate::data::value::current_validity()?.0.0;
    let watermark = match meta
        .get(SYSTEM_CLOCK_WATERMARK_KEY)
        .map_err(|e| miette!("reading system clock watermark: {e}"))?
    {
        None => i64::MIN,
        Some(v) => {
            let bytes: [u8; 8] = v
                .as_ref()
                .try_into()
                .map_err(|_| miette!("corrupt system clock watermark"))?;
            i64::from_be_bytes(bytes)
        }
    };
    Ok(FjallStorage {
        db,
        ks,
        meta,
        clock: std::sync::Arc::new(SystemClock::new(now.max(watermark))),
        watermark_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
    })
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
    meta: OptimisticTxKeyspace,
    clock: std::sync::Arc<SystemClock>,
    /// Serializes {mint, watermark persist} pairs. Without it the
    /// PERSISTED floor can regress: fjall resolves the watermark key by
    /// internal commit order, which is decoupled from mint order, so a
    /// smaller stamp's insert landing last would leave the on-disk floor
    /// below a stamp already used — and a crash then lets the reopened
    /// clock re-mint it (hostile-review finding). Held only around the
    /// mint+insert pair; snapshots and commits never take it.
    watermark_lock: std::sync::Arc<std::sync::Mutex<()>>,
}

impl FjallStorage {
    /// Mint this transaction's system stamp. Takes the OPEN SNAPSHOT by
    /// reference: the signature is the enforcement — a stamp cannot be
    /// minted before the snapshot it must follow exists, so the
    /// snapshot-then-mint ordering `write_tx` relies on is
    /// unrepresentable to violate (the reproducer catches wide
    /// reorderings; this catches every one at compile time). Persists
    /// the crash-recovery watermark as a side effect — a floor, not a
    /// record.
    fn stamp_after_snapshot(&self, _snapshot: &OptimisticWriteTx) -> Result<ValidityTs> {
        // One guard around mint AND persist: inserts land in mint order,
        // so the newest persisted watermark is always the largest minted
        // stamp (see `watermark_lock`).
        let _guard = self
            .watermark_lock
            .lock()
            .map_err(|_| miette!("watermark lock poisoned"))?;
        let now = crate::data::value::current_validity()?.0.0;
        let stamp = self.clock.stamp(now);
        self.meta
            .insert(SYSTEM_CLOCK_WATERMARK_KEY, stamp.0.0.to_be_bytes())
            .map_err(|e| miette!("persisting system clock watermark: {e}"))?;
        Ok(stamp)
    }
}

impl Storage for FjallStorage {
    type ReadTx = FjallReadTx;
    type WriteTx = FjallWriteTx;

    fn storage_kind(&self) -> &'static str {
        "fjall"
    }

    fn clock_floor(&self) -> Result<ValidityTs> {
        Ok(ValidityTs(std::cmp::Reverse(self.clock.floor())))
    }

    fn raise_clock_floor(&self, floor: ValidityTs) -> Result<()> {
        let _guard = self
            .watermark_lock
            .lock()
            .map_err(|_| miette!("watermark lock poisoned"))?;
        self.clock.raise_floor(floor.0.0);
        // Persist the CLOCK's floor, not the argument: raise_floor is a
        // max, so a stale (lower) argument must not regress the disk.
        self.meta
            .insert(SYSTEM_CLOCK_WATERMARK_KEY, self.clock.floor().to_be_bytes())
            .map_err(|e| miette!("persisting system clock watermark: {e}"))?;
        Ok(())
    }

    fn read_tx(&self) -> Result<FjallReadTx> {
        Ok(FjallReadTx {
            snap: self.db.read_tx(),
            ks: self.ks.clone(),
        })
    }

    fn write_tx(&self) -> Result<FjallWriteTx> {
        // SNAPSHOT FIRST, MINT SECOND — that ordering alone carries THE
        // serialization invariant (contract v3): if this transaction can
        // read another's write, its stamp strictly exceeds that writer's.
        // Proof: fjall's own `write_tx()` takes its commit oracle's lock
        // while opening the snapshot, so a writer is visible here only if
        // its commit fully preceded this snapshot; that writer minted its
        // stamp before its commit, hence before this snapshot, hence
        // before this mint — and the clock is strictly monotone. Minting
        // BEFORE the snapshot broke this (a rival could mint later and
        // commit sooner, and our write landed at a smaller stamp than a
        // write we read — shadowed forever, a lost update with zero
        // conflicts; pinned by
        // `concurrent_increments_lose_nothing_at_the_storage_layer`).
        // The watermark persists non-transactionally — a floor, not a
        // record.
        let tx = self
            .db
            .write_tx()
            .map_err(|e| miette!("fjall write tx: {e}"))?;
        let stamp = self.stamp_after_snapshot(&tx)?;
        Ok(FjallWriteTx {
            tx,
            ks: self.ks.clone(),
            db: self.db.clone(),
            stamp,
        })
    }

    fn batch_put<'a>(
        &'a self,
        data: Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>,
    ) -> Result<()> {
        // Bulk import is OUTSIDE the stamp/SSI conflict surface: rows keep
        // the stamps they carry and no transaction machinery runs. The
        // precondition making that sound — a fresh, otherwise-idle store —
        // is refused here, not just documented: importing into a store
        // holding data is the one shape under which an unstamped import
        // could race a live minting writer.
        {
            // The probe's upper bound must clear every real key. Every key
            // begins with an 8-byte relation id capped at MAX_RELATION_ID,
            // so its first byte stays below 0xFF — proven at compile time
            // so a future id-cap bump cannot silently turn a full store
            // invisible to this check.
            const {
                assert!(
                    crate::data::tuple::MAX_RELATION_ID < (0xff_u64 << 56),
                    "emptiness probe bound must exceed every relation-id prefix"
                );
            }
            let probe = self.db.read_tx();
            if raw_range(&probe, &self.ks, &[], &[0xff; 9])
                .next()
                .is_some()
            {
                bail!("bulk import target is not empty: import only into a fresh store");
            }
        }
        // Atomic chunks: each chunk is one transaction committed as a unit,
        // so an interrupted import leaves a clean prefix of the input rather
        // than a torn write, with bounded memory.
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
    stamp: ValidityTs,
}

impl FjallWriteTx {
    /// Contract v2 (write-set validation): put every written key on the
    /// commit-time conflict surface. fjall's `insert`/`remove` register the
    /// key only as a conflict SOURCE (something that aborts *others*); the
    /// oracle validates a transaction's READ set alone. The only public path
    /// into fjall's read tracking is an actual read, so issue a
    /// `contains_key` for the key just written: it resolves against the
    /// transaction's own memtable — the key was written a moment ago, value
    /// or tombstone alike — so it costs no disk I/O, and the side effect is
    /// exactly the `mark_read` that makes the oracle abort this commit if a
    /// concurrent transaction committed a write to the same key. Mutating
    /// this call away breaks `write_write_race_aborts_second_committer`.
    fn mark_written_key_validated(&mut self, key: &[u8]) -> Result<()> {
        self.tx
            .contains_key(&self.ks, key)
            .map_err(|e| miette!("fjall read: {e}"))?;
        Ok(())
    }
}

/// Both fjall read views (`Snapshot`, `OptimisticWriteTx`) speak `Readable`;
/// every read-side operation is written once against that.
fn raw_range<'a, R: Readable>(
    reader: &'a R,
    ks: &'a OptimisticTxKeyspace,
    lower: &[u8],
    upper: &[u8],
) -> impl Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a {
    // Degenerate-bounds guard, at the single choke point every range scan,
    // skip scan, and del_range pass through. The contract says `[lower,
    // upper)` with `lower >= upper` is simply EMPTY — but the bounds must
    // never reach fjall: a write transaction records the requested bounds
    // verbatim in its conflict manager and replays them through
    // `BTreeSet::range` at COMMIT time, which panics on an inverted range —
    // inside the commit oracle, while holding the global write-serialize
    // lock, poisoning the whole store for every later transaction. An
    // inverted range is answered here (empty, nothing tracked) and fjall
    // never sees it. Skipping the read tracking is sound: an empty range
    // has no phantoms to protect against.
    let bounds_valid = lower < upper;
    // fjall clones bounds internally; borrowing here avoids two Vec
    // allocations per call on the skip scan's hottest path.
    let bounds: (Bound<&[u8]>, Bound<&[u8]>) = (Bound::Included(lower), Bound::Excluded(upper));
    bounds_valid
        .then(|| reader.range::<&[u8], _>(ks, bounds))
        .into_iter()
        .flatten()
        .map(|guard| {
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
                as_of: AsOf,
            ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
                Box::new(SkipIterator {
                    reader: &self.$reader,
                    ks: &self.ks,
                    upper: upper.to_vec(),
                    as_of,
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
    fn system_stamp(&self) -> ValidityTs {
        self.stamp
    }

    fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        self.tx.insert(&self.ks, key, val);
        self.mark_written_key_validated(key)
    }

    fn del(&mut self, key: &[u8]) -> Result<()> {
        self.tx.remove(&self.ks, key);
        self.mark_written_key_validated(key)
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

/// Bitemporal skip scan: seek to the next candidate key, peek the row's
/// polarity from its value, decide with `check_key_for_bitemporal`,
/// re-seek at the bound it returns. A corrupt key or value is surfaced as
/// `Err` WITHOUT advancing (the contract's rule): every subsequent poll
/// re-yields it, so the scan cannot step over bytes it could not judge.
struct SkipIterator<'a, R: Readable> {
    reader: &'a R,
    ks: &'a OptimisticTxKeyspace,
    upper: Vec<u8>,
    as_of: AsOf,
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
            let polarity = match claim_polarity_of_value(&v) {
                Ok(p) => p,
                Err(e) => return Some(Err(e)),
            };
            let (ret, nxt_bound) = match check_key_for_bitemporal(&k, polarity, self.as_of, None) {
                Ok(pair) => pair,
                Err(e) => return Some(Err(e)),
            };
            // Termination guarantee: the seek bound must advance STRICTLY
            // past the key just examined, whatever the stored bytes
            // contain. Every bound the kernel returns for a parseable key
            // already advances strictly (its slot flags are pinned, so no
            // stored key can equal a TERMINAL splice); the byte-successor
            // of k (k ++ 0x00, the smallest key strictly greater) is
            // belt-and-braces against bytes no argument anticipated.
            self.next_bound = if nxt_bound.as_slice() > k.as_slice() {
                nxt_bound
            } else {
                let mut succ = k.clone();
                succ.push(0);
                succ
            };
            if let Some(mut tup) = ret {
                if let Err(e) = extend_tuple_from_bitemporal_v(&mut tup, &v) {
                    return Some(Err(e));
                }
                return Some(Ok(tup));
            }
        }
    }
}
