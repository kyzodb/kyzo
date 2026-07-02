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
//! Transactions are two species of one genus, expressed as two traits: a
//! [`ReadTx`] is one consistent snapshot and *cannot* write — the operations
//! do not exist on it; a [`WriteTx`] extends reading with a conflict-tracked
//! write set, and committing **consumes** it, so a committed transaction is
//! not an invalid state to guard against but a value that no longer exists.
//!
//! ## Concurrency economics, stated plainly
//!
//! - **Isolation is full SSI** — every read and range in a write transaction
//!   is conflict-tracked. This is *stricter* than the CozoDB base (snapshot
//!   reads with write-write conflicts only), which permitted write skew and
//!   phantom anomalies; KyzoDB does not. The cost: read-heavy write
//!   transactions conflict more often. Conflicts are typed
//!   ([`ConflictError`]) and retryable — retry loops are the engine's job
//!   (see [`retry`]).
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

use std::fmt;

use itertools::Itertools;
use miette::{Result, bail, miette};

use crate::data::tuple::{Tuple, decode_tuple_from_kv};
use crate::data::value::ValidityTs;

pub(crate) mod backup;
pub(crate) mod fjall;
pub(crate) mod retry;
#[cfg(test)]
mod tests;
pub(crate) mod verify;

/// The version of the on-disk memcmp/tuple encoding. Stamped into every
/// store and every dump file; a mismatch refuses to open rather than read
/// garbage. Any change to the encoding is a migration and must bump
/// [`FormatVersion::CURRENT`] (see .claude/rules/memcmp.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatVersion(u16);

impl FormatVersion {
    /// The format this build reads and writes.
    pub const CURRENT: FormatVersion = FormatVersion(1);

    /// The stored representation: ASCII decimal.
    pub fn as_bytes(self) -> Vec<u8> {
        self.0.to_string().into_bytes()
    }

    /// Parse a stored representation; corrupt bytes are an error. Exactly
    /// canonical spellings are accepted: a stamp must be byte-identical to
    /// what some version of this code writes (no leading zeros or signs).
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let s =
            std::str::from_utf8(bytes).map_err(|_| miette!("corrupt format version: not UTF-8"))?;
        let n: u16 = s
            .parse()
            .map_err(|_| miette!("corrupt format version: {s:?}"))?;
        let v = FormatVersion(n);
        if v.as_bytes() != bytes {
            bail!("corrupt format version: non-canonical spelling {s:?}");
        }
        Ok(v)
    }
}

impl fmt::Display for FormatVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.0)
    }
}

/// A transaction commit failed because a concurrently committed transaction
/// modified something this one read or wrote.
///
/// **Retryable**: rerun the whole transaction. This is a typed error so the
/// engine can distinguish it programmatically (via `Report::downcast_ref`)
/// from fatal conditions like corruption or IO failure — retry-on-conflict
/// is a control-flow decision, not a string match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConflictError;

impl fmt::Display for ConflictError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
    impl Sealed for super::fjall::FjallReadTx {}
    impl Sealed for super::fjall::FjallWriteTx {}
}

/// A storage engine: hands out transactions and supports bulk import.
///
/// Concurrent writes are a core requirement, not an option: many write
/// transactions proceed in parallel from different threads, and conflicts
/// are resolved at commit time — never by blocking writers against each
/// other. (`fjall` was chosen over single-writer designs for exactly this.)
pub trait Storage: Send + Sync + Clone + sealed::Sealed {
    /// The read-transaction species: one consistent snapshot, no writes.
    type ReadTx: ReadTx + Send;
    /// The write-transaction species: snapshot + tracked write set; commit
    /// consumes it. Moves freely across threads (`Send`).
    type WriteTx: WriteTx + Send;

    /// A string identifying the storage kind.
    fn storage_kind(&self) -> &'static str;

    /// Open a read transaction over one consistent snapshot.
    fn read_tx(&self) -> Result<Self::ReadTx>;

    /// Open a write transaction: a snapshot plus a conflict-tracked write set.
    fn write_tx(&self) -> Result<Self::WriteTx>;

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
    /// Durability levels, stated precisely: a successful [`WriteTx::commit`]
    /// survives a *process* crash (the write reaches OS buffers before commit
    /// returns); surviving a *power cut* additionally requires `sync()` or a
    /// [`WriteTx::commit_durable`]. The engine decides where to place that
    /// cost.
    fn sync(&self) -> Result<()>;
}

/// The read capabilities of a transaction: a consistent snapshot view. In a
/// write transaction these same operations also see the transaction's own
/// writes and are conflict-tracked.
pub trait ReadTx: sealed::Sealed + Sync {
    /// Get the value of a key.
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// Check whether a key exists.
    fn exists(&self, key: &[u8]) -> Result<bool>;

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

/// The write-transaction species: everything a [`ReadTx`] can do — seeing
/// the transaction's own writes, conflict-tracked — plus mutation and
/// commit.
///
/// MVCC semantics: `commit` must fail with [`ConflictError`] — discarding
/// all of the transaction's changes — if any key or range this transaction
/// read or wrote was modified concurrently by a committed transaction.
pub trait WriteTx: ReadTx {
    /// Set a key to a value, overwriting any existing value.
    fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()>;

    /// Delete a key.
    fn del(&mut self, key: &[u8]) -> Result<()>;

    /// Delete every key in `[lower, upper)` visible to this transaction —
    /// both snapshot data and the transaction's own writes. After commit, no
    /// key in the range that was visible to this transaction survives.
    fn del_range(&mut self, lower: &[u8], upper: &[u8]) -> Result<()>;

    /// Commit, consuming the transaction: there is no committed-but-alive
    /// state. Durability: survives a process crash; for power-cut durability
    /// use [`commit_durable`](Self::commit_durable) or [`Storage::sync`].
    fn commit(self) -> Result<()>
    where
        Self: Sized;

    /// Commit and fsync before returning: the transaction survives a power
    /// cut, not just a process crash. Costs an fsync; the engine chooses per
    /// transaction where that price is worth paying.
    fn commit_durable(self) -> Result<()>
    where
        Self: Sized;
}
