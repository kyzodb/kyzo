/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The KyzoDB storage contract.
//!
//! One backend implements this: `fjall`, a pure-Rust LSM store with
//! optimistic (SSI) transactions. The contract is written for that machine —
//! owned transactions, conflict-tracked reads, seek-based scans — not for any
//! historical backend's shape.
//!
//! ## Concurrency economics, stated plainly
//!
//! - **Isolation is full SSI** — every read and range in a write transaction
//!   is conflict-tracked. This is *stricter* than the CozoDB base (snapshot
//!   reads with write-write conflicts only), which permitted write skew and
//!   phantom anomalies; KyzoDB does not. The cost: read-heavy write
//!   transactions conflict more often. Conflicts are typed
//!   ([`ConflictError`]) and retryable — retry loops are the engine's job.
//! - **Transaction preparation is parallel; commit application is serial.**
//!   fjall's oracle validates and applies commits one at a time under a
//!   global lock. Reads, scans, and computation run genuinely concurrently
//!   across threads; the commit pipeline is the throughput ceiling.
//! - **Long-lived read transactions delay version GC** (LSM versions and
//!   oracle bookkeeping are retained while any snapshot that old is open).
//!   Keep analytical readers and backup dumps in mind under sustained writes.
//! - **As-of scans inside write transactions** mark conservatively coarse
//!   read ranges (one per seek step); prefer read transactions for
//!   time-travel queries unless the write genuinely depends on them.

use itertools::Itertools;
use miette::Result;

use crate::data::tuple::{Tuple, decode_tuple_from_kv};
use crate::data::value::ValidityTs;

pub(crate) mod backup;
pub(crate) mod fjall;
pub(crate) mod retry;
#[cfg(test)]
mod tests;
pub(crate) mod verify;

/// The on-disk format version of the memcmp/tuple encoding. Stamped into
/// every store and every dump file; any change to the encoding is a
/// migration and must bump this (see .claude/rules/memcmp.md).
pub(crate) const FORMAT_VERSION: &[u8] = b"1";

/// A transaction commit failed because a concurrently committed transaction
/// modified something this one read or wrote.
///
/// **Retryable**: rerun the whole transaction. This is a typed error so the
/// engine can distinguish it programmatically (via `Report::downcast_ref`)
/// from fatal conditions like corruption or IO failure — retry-on-conflict
/// is a control-flow decision, not a string match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConflictError;

impl std::fmt::Display for ConflictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        "transaction conflict: a key or range this transaction read or wrote \
         was modified concurrently; the transaction was aborted, rerun it"
            .fmt(f)
    }
}

impl std::error::Error for ConflictError {}
impl miette::Diagnostic for ConflictError {}

mod sealed {
    /// One backend by decree (see .claude/rules/storage.md): these traits are
    /// sealed — external crates read the contract, they do not implement it.
    pub trait Sealed {}
    impl Sealed for super::fjall::FjallStorage {}
    impl Sealed for super::fjall::FjallTx {}
}

/// A storage engine: hands out transactions and supports bulk import.
///
/// Concurrent writes are a core requirement, not an option: many write
/// transactions proceed in parallel from different threads, and conflicts
/// are resolved at commit time — never by blocking writers against each
/// other. (`fjall` was chosen over single-writer designs for exactly this.)
pub trait Storage: Send + Sync + Clone + sealed::Sealed {
    /// The transaction type. Transactions are owned values: they carry their
    /// own snapshot and (for writers) their own write set, and move freely
    /// across threads (`Send`).
    type Tx: StoreTx + Send;

    /// A string identifying the storage kind.
    fn storage_kind(&self) -> &'static str;

    /// Create a transaction. Write operations are only permitted when
    /// `write == true`; a read transaction sees one consistent snapshot.
    fn transact(&self, write: bool) -> Result<Self::Tx>;

    /// Bulk-import key-value pairs. Callers guarantee: no duplicates, strictly
    /// ascending key order, and exclusive access to the database while this
    /// runs. Used by restore/import paths. Implementations must apply the
    /// data in atomic chunks, so that an interrupted import leaves a clean
    /// prefix of the input, never a torn write.
    fn batch_put<'a>(
        &'a self,
        data: Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>,
    ) -> Result<()>;

    /// Force everything committed so far to durable storage (fsync).
    ///
    /// Durability levels, stated precisely: a successful [`StoreTx::commit`]
    /// survives a *process* crash (the write reaches OS buffers before commit
    /// returns); surviving a *power cut* additionally requires `sync()`. The
    /// engine decides where to place that cost.
    fn sync(&self) -> Result<()>;
}

/// A storage transaction.
///
/// MVCC semantics: a transaction reads from a consistent snapshot taken at
/// creation, overlaid with its own writes. **Every read in a write
/// transaction is conflict-tracked**: `commit()` must fail — discarding all
/// of the transaction's changes — if any key or range this transaction read
/// or wrote was modified concurrently by a committed transaction.
pub trait StoreTx: sealed::Sealed + Sync {
    /// Get the value of a key, seeing the transaction's own writes.
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// Check whether a key exists, seeing the transaction's own writes.
    fn exists(&self, key: &[u8]) -> Result<bool>;

    /// Set a key to a value, overwriting any existing value.
    fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()>;

    /// Delete a key.
    fn del(&mut self, key: &[u8]) -> Result<()>;

    /// Delete every key in `[lower, upper)` visible to this transaction —
    /// both snapshot data and the transaction's own writes. After commit, no
    /// key in the range that was visible to this transaction survives.
    fn del_range(&mut self, lower: &[u8], upper: &[u8]) -> Result<()>;

    /// Commit the transaction. Returns `Err` — with all changes discarded —
    /// if MVCC consistency cannot be guaranteed (a tracked read or write
    /// conflicted with a concurrently committed transaction).
    ///
    /// Durability: a successful commit survives a *process* crash. For
    /// power-cut durability use [`commit_durable`](Self::commit_durable) or a
    /// store-wide [`Storage::sync`].
    fn commit(&mut self) -> Result<()>;

    /// Commit and fsync before returning: the transaction survives a power
    /// cut, not just a process crash. Costs an fsync; the engine chooses per
    /// transaction where that price is worth paying.
    fn commit_durable(&mut self) -> Result<()>;

    /// Scan a range in ascending byte order (which, under the memcmp
    /// encoding, is ascending semantic order). `lower` is inclusive, `upper`
    /// exclusive.
    fn range_scan<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>;

    /// Scan a range, decoding each pair as a [`Tuple`].
    fn range_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        Box::new(
            self.range_scan(lower, upper)
                .map(|kv| kv.and_then(|(k, v)| decode_tuple_from_kv(&k, &v, None))),
        )
    }

    /// Time-travel scan: among keys differing only in their trailing validity
    /// (the last key slot), yield only the newest version at or before
    /// `valid_at`, and only if that version is assertive. Implementations
    /// must seek rather than iterate over skipped versions.
    ///
    /// Time travel is a mandatory part of the KyzoDB storage contract.
    fn range_skip_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
        valid_at: ValidityTs,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a>;

    /// Count the keys in `[lower, upper)`.
    fn range_count(&self, lower: &[u8], upper: &[u8]) -> Result<usize> {
        self.range_scan(lower, upper)
            .process_results(|it| it.count())
    }

    /// Scan the entire store in ascending order.
    fn total_scan<'a>(&'a self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>;
}
