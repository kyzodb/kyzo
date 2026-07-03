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
//! - **Isolation is SSI, and READS AND WRITES are the conflict surface** —
//!   every read, range, and written key in a write transaction is
//!   conflict-tracked, and commit aborts if anything this transaction READ
//!   (a point read or a scanned range) or WROTE (a `put` or `del` key) was
//!   modified by a transaction committed after this one's snapshot. A
//!   write-write race is therefore first-committer-wins: the second
//!   committer aborts with the typed conflict and reruns on a fresh
//!   snapshot. A commit with an empty write set still never aborts — it
//!   certifies nothing about its reads. Unlike the CozoDB base (snapshot
//!   reads with write-write conflicts only), write skew and phantoms are
//!   aborted too — provided the transaction reads what it depends on. The
//!   cost: write transactions conflict more often, including same-key
//!   races that last-writer-wins would have merged silently. Conflicts are
//!   typed ([`ConflictError`]) and retryable — retry loops are the
//!   engine's job (see [`retry`]).
//! - **Engine implication: uniqueness is enforced by the write itself.**
//!   Because written keys are validated, insert-if-absent / uniqueness
//!   races on the same key abort the losing racer even when it never read
//!   the key — a blind `put` cannot silently swallow a concurrent insert.
//!   Reading inside the transaction is still how logic *observes* current
//!   state; write validation guarantees the race is detected, not that the
//!   old value was seen.
//! - **Bulk import is outside the conflict surface.** [`Storage::batch_put`]
//!   requires exclusive access by precondition and applies its writes
//!   blind; it is a restore/import side door, not a transaction.
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
//!
//! ## Contract history (this contract is SEALED; changes are recorded here)
//!
//! - **v2 — write sets are validated at commit (story #3 ruling).** As first
//!   sealed, the contract validated READS only: a blind write-write race
//!   (neither side read the key) committed both sides, serialized as
//!   last-writer-wins. That is serializable — it is an anomaly only when
//!   logic depended on the old value, which requires a read, and reads were
//!   validated — but it made uniqueness patterns depend on caller
//!   discipline ("uniqueness needs a read") instead of on the contract. The
//!   maintainer ruled that discipline-shaped guarantee an open weakness;
//!   commit now validates written keys exactly like read keys, and the
//!   write-write race aborts its second committer with the typed,
//!   retryable [`ConflictError`]. For the record: FoundationDB- and
//!   badger-class optimistic oracles validate reads only (blind writes are
//!   a deliberate throughput feature there); PostgreSQL SSI and
//!   TiKV/Percolator abort write-write races. KyzoDB sides with the latter.

use std::fmt;

use itertools::Itertools;
use miette::{Result, bail, miette};

use crate::data::tuple::{Tuple, decode_tuple_from_kv};
use crate::data::value::ValidityTs;

pub(crate) mod backup;
pub(crate) mod fjall;
// The cold Merkle state root over the ordered keyspace, driven by the
// `::merkle_root` sys-op dispatcher in runtime/db.rs; not every helper has
// a lib caller yet, so the remainder is allowed rather than expected dead.
#[allow(dead_code)]
pub(crate) mod merkle;
pub(crate) mod retry;
// With `bench-internals` on (and test off), sim.rs compiles into the lib so
// `bench_api` can build mem-backend workloads on `SimStorage`; the module's
// DST-only helpers (scheduler, crash/powercut doubles) are then legitimately
// unused - allow that in exactly that configuration so `clippy -D warnings`
// holds for the feature config too.
#[cfg(any(test, feature = "bench-internals"))]
#[cfg_attr(all(feature = "bench-internals", not(test)), allow(dead_code))]
pub(crate) mod sim;
#[allow(dead_code)]
pub(crate) mod temp;
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
/// modified something this one READ (a point read or a scanned range) or
/// WROTE (a `put` or `del` key).
///
/// Reads and writes are the conflict surface: a write-write race aborts its
/// second committer (first-committer-wins). A commit with an empty write set
/// still never aborts.
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
         was modified by a concurrent commit; the transaction was aborted, \
         rerun it"
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
    impl Sealed for super::temp::TempTx {}
    impl Sealed for super::fjall::FjallReadTx {}
    impl Sealed for super::fjall::FjallWriteTx {}
    // The deterministic simulator (`storage/sim.rs`) is admitted under
    // `cfg(test)` only: the seal exists to keep FOREIGN backends from
    // implementing the contract, and the simulator is not a second backend —
    // it is the contract's own test double, compiled solely into the test
    // harness and never into the shipped library. "One backend by decree"
    // still holds for everything that ships.
    #[cfg(any(test, feature = "bench-internals"))]
    impl Sealed for super::sim::SimStorage {}
    #[cfg(any(test, feature = "bench-internals"))]
    impl Sealed for super::sim::SimReadTx {}
    #[cfg(any(test, feature = "bench-internals"))]
    impl Sealed for super::sim::SimWriteTx {}
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
    /// exclusive; a degenerate range (`lower >= upper`) is EMPTY — never an
    /// error, never a panic.
    ///
    /// Conflict tracking (in a write transaction): the WHOLE requested
    /// range counts as read the moment the scan is opened, even if the
    /// iterator is dropped early — the conservative choice, and the one
    /// phantom protection needs. Reads served from the transaction's own
    /// write set still count as tracked reads.
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
/// all of the transaction's changes — if anything this transaction READ (a
/// point read or a scanned range) or WROTE (a `put` or `del` key) was
/// modified concurrently by a committed transaction. Reads and writes are
/// the conflict surface (contract v2 — see the module docs' history): a
/// write-write race aborts its second committer, first-committer-wins. A
/// commit with an empty write set never aborts — a read-only `WriteTx`
/// commit certifies nothing.
///
/// Consequence for engine code: insert-if-absent / uniqueness races on a
/// key are detected by the write itself — the losing racer aborts with the
/// typed conflict even if it never read the key. Logic that depends on the
/// key's current VALUE must still read it inside the transaction.
pub trait WriteTx: ReadTx {
    /// Set a key to a value, overwriting any existing value. The key joins
    /// the conflict surface: a concurrent committed write to it aborts this
    /// transaction's commit.
    fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()>;

    /// Delete a key. The key joins the conflict surface exactly as in
    /// [`put`](Self::put).
    fn del(&mut self, key: &[u8]) -> Result<()>;

    /// Delete every key in `[lower, upper)` visible to this transaction —
    /// both snapshot data and the transaction's own writes. After commit, no
    /// key in the range that was visible to this transaction survives.
    ///
    /// The range counts toward conflict detection like any read: a
    /// concurrent commit into it conflicts this transaction. Degenerate
    /// bounds (`lower >= upper`) denote the empty interval: they delete
    /// nothing and track nothing — a caller wanting "this interval stays
    /// empty" protection must pass forward bounds.
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
    ///
    /// Failure semantics: if the commit applies but the fsync then fails,
    /// the transaction IS committed — visible, process-crash durable, not
    /// yet power-cut durable. The error reports the durability shortfall,
    /// not a rollback; callers needing all-or-nothing durability must treat
    /// it accordingly.
    fn commit_durable(self) -> Result<()>
    where
        Self: Sized;
}
