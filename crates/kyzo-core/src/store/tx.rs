/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Transactions: snapshot isolation, typed conflicts, consuming commit.
//!
//! [`ReadTx`] / [`WriteTx`] / [`ConflictError`] / [`Slice`] (refcount currency)
//! live here. The [`Storage`](super::contract::Storage) trait and sealed
//! admission live in [`super::contract`].

use std::fmt;

use itertools::Itertools;
use miette::Result;

use kyzo_model::value::{AsOf, ValidityTs};
use kyzo_model::value::{Tuple, decode_tuple_from_kv};

use super::contract::{Storage, sealed::Sealed};

/// fjall's Arc-backed byte currency — a clone is a refcount bump, never a
/// heap copy. Absolute path: sibling modules declare `fjall` and would
/// shadow the extern crate for a plain `use fjall::...`.
pub use ::fjall::Slice;

/// A transaction commit failed because a concurrently committed transaction
/// modified something this one READ (a point read or a scanned range) or
/// WROTE (a `put` or `del` key).
///
/// Reads and writes are the conflict surface: a write-write race aborts its
/// second committer (first-committer-wins). A commit with an empty write set
/// still never aborts.
///
/// **Retryable**: rerun the whole transaction. Prefer matching
/// [`CommitFailure::Conflict`] on the commit outcome, or feeding
/// [`CommitFailure`] / [`ConflictError`] into [`super::retry::RetryError`] via
/// [`From`]. Conflict vs fatal in that channel is decided by variant only —
/// never by diagnostic code or string identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, miette::Diagnostic)]
#[diagnostic(code(storage::conflict))]
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

/// Proof that an Open write transaction committed. Carries no Open methods —
/// use-after-commit is a type error, not a runtime guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub struct Committed;

/// Proof that an Open write transaction aborted without applying its writes.
/// Carries no Open methods — use-after-abort is a type error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub struct Aborted;

/// Backend-sourced IO error as `std::error::Error`. Variant identity lives on
/// the outer [`CommitIo`] (or attempt-op) enum — not in a formatted string.
#[derive(Debug)]
pub struct BackendIoError(Box<dyn std::error::Error + Send + Sync>);

impl BackendIoError {
    /// Box any backend error as a commit/op source.
    pub fn from_error(e: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self(Box::new(e))
    }
}

impl fmt::Display for BackendIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for BackendIoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.0)
    }
}

/// Structured IO refusal during commit (or `commit_durable`'s fsync).
///
/// For [`WriteTx::commit_durable`]: if the commit applied and only the fsync
/// failed, the transaction IS committed (visible, process-crash durable) —
/// [`Self::FjallSync`] / [`Self::SimInjectedFsyncAfterCommit`] report the
/// durability shortfall, not a rollback.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[diagnostic(code(storage::commit_io))]
pub enum CommitIo {
    /// fjall optimistic commit returned a non-conflict substrate error.
    #[error("fjall commit failed")]
    FjallCommit(#[source] BackendIoError),

    /// fjall `PersistMode::SyncAll` after a successful commit failed.
    #[error("fjall sync failed")]
    FjallSync(#[source] BackendIoError),

    /// Simulator injected an fsync failure (empty write set / pre-apply).
    #[error("sim: injected fsync failure")]
    SimInjectedFsync,

    /// Simulator injected an fsync failure after the commit applied.
    #[error("sim: injected fsync failure (commit applied, not power-cut durable)")]
    SimInjectedFsyncAfterCommit,
}

/// Structured corruption refusal detected while committing.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[diagnostic(code(storage::commit_corruption))]
pub enum CommitCorruption {
    /// On-disk or internal corruption observed at commit time.
    #[error("storage corruption during commit")]
    Detected,
}

/// Closed commit refusal: conflict, IO, or corruption — never an erased
/// `Result<()>` / stringly dispatch. The Open transaction is spent either way.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum CommitFailure {
    /// SSI conflict — discard the write set; retry on a fresh Open snapshot.
    #[error(transparent)]
    #[diagnostic(transparent)]
    Conflict(#[from] ConflictError),

    /// Storage IO failed during commit (or during `commit_durable`'s fsync).
    #[error(transparent)]
    #[diagnostic(transparent)]
    Io(#[from] CommitIo),

    /// On-disk or internal corruption detected while committing.
    #[error(transparent)]
    #[diagnostic(transparent)]
    Corruption(#[from] CommitCorruption),
}

impl CommitFailure {
    /// Whether this refusal is the retryable concurrent-writer case.
    pub fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict(_))
    }
}

/// The read capabilities of a transaction: a consistent snapshot view. In a
/// write transaction these same operations also see the transaction's own
/// writes and are conflict-tracked.
pub trait ReadTx: Sealed + Sync {
    /// Get the value of a key. [`Slice`] is fjall's Arc-backed byte currency
    /// — a clone is a refcount bump, never a heap copy, so a caller that
    /// only inspects or re-slices the bytes pays no allocation.
    fn get(&self, key: &[u8]) -> Result<Option<Slice>>;

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
    ) -> Box<dyn Iterator<Item = Result<(Slice, Slice)>> + 'a>;

    /// Scan a range, decoding each pair as a [`Tuple`] straight from the
    /// borrowed [`Slice`]s — no intermediate `Vec` copy of either key or
    /// value sits between the backend and the decoder.
    fn range_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        Box::new(self.range_scan(lower, upper).map(|kv| {
            kv.and_then(|(k, v)| decode_tuple_from_kv(&k, &v, None).map_err(miette::Report::from))
        }))
    }

    /// Scan a range yielding KEYS ONLY — the value is never materialized.
    /// The default just discards `range_scan`'s value half; the fjall
    /// backend overrides this to filter its `Guard` currency on `key()`
    /// alone, so a caller that only needs presence or a count (see
    /// [`range_count`](Self::range_count)) never pays for value I/O.
    fn range_scan_keys<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<Slice>> + 'a> {
        Box::new(self.range_scan(lower, upper).map(|kv| kv.map(|(k, _)| k)))
    }

    /// Bitemporal as-of scan: among keys differing only in their two
    /// trailing time slots (valid instant outer, system version inner —
    /// [`StorageKey::BITEMPORAL_TAIL_LEN`](kyzo_model::value::StorageKey)),
    /// resolve each fact to what the record said at the [`AsOf`]
    /// coordinate, and yield only facts whose governing
    /// row asserts them. A row's polarity (assert / retract / erase) is
    /// read from its VALUE (`claim_polarity_of_value`); the resolution
    /// rule and seek algebra are `check_key_for_bitemporal`'s.
    /// Implementations must seek rather than iterate over skipped
    /// versions, and must surface a corrupt key as an `Err` WITHOUT
    /// advancing — every subsequent poll re-yields the error, so a scan
    /// cannot silently step over bytes it could not judge (engine callers
    /// stop at the first `Err`).
    ///
    /// Two-axis time travel is a mandatory part of the KyzoDB storage
    /// contract.
    fn range_skip_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
        as_of: AsOf,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a>;

    /// Count the keys in `[lower, upper)`. Goes through
    /// [`range_scan_keys`](Self::range_scan_keys): a count never needs a
    /// single value byte.
    fn range_count(&self, lower: &[u8], upper: &[u8]) -> Result<usize> {
        self.range_scan_keys(lower, upper)
            .process_results(|it| it.count())
    }

    /// Scan the entire store in ascending order.
    fn total_scan<'a>(&'a self) -> Box<dyn Iterator<Item = Result<(Slice, Slice)>> + 'a>;
}

/// The **Open** write-transaction species: everything a [`ReadTx`] can do —
/// seeing the transaction's own writes, conflict-tracked — plus mutation.
/// Open / [`Committed`] / [`Aborted`] are three types; there is no flag.
///
/// MVCC semantics: `commit` must fail with [`CommitFailure::Conflict`] —
/// discarding all of the transaction's changes — if anything this
/// transaction READ (a point read or a scanned range) or WROTE (a `put` or
/// `del` key) was modified concurrently by a committed transaction. Reads
/// and writes are the conflict surface (contract v2 — see the module docs'
/// history): a write-write race aborts its second committer,
/// first-committer-wins. A commit with an empty write set never aborts — a
/// read-only Open commit certifies nothing.
///
/// Consequence for engine code: insert-if-absent / uniqueness races on a
/// key are detected by the write itself — the losing racer aborts with the
/// typed conflict even if it never read the key. Logic that depends on the
/// key's current VALUE must still read it inside the transaction.
pub trait WriteTx: ReadTx {
    /// This transaction's SYSTEM timestamp: the instant its writes join
    /// recorded history, minted once from the storage's monotone clock
    /// (`max(now_µs, last + 1)`) when the transaction — and therefore its
    /// snapshot — was created. Every bitemporal row this transaction
    /// writes carries this stamp in its system slot. See the contract
    /// history's v3 entry for why snapshot-creation stamping makes every
    /// as-of system cut a genuine serial-order prefix under SSI.
    fn system_stamp(&self) -> ValidityTs;

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

    /// Commit, consuming Open into [`Committed`] (or a closed
    /// [`CommitFailure`]). Durability: survives a process crash; for
    /// power-cut durability use [`commit_durable`](Self::commit_durable) or
    /// [`Storage::sync`].
    fn commit(self) -> std::result::Result<Committed, CommitFailure>
    where
        Self: Sized;

    /// Commit and fsync before returning: the transaction survives a power
    /// cut, not just a process crash. Costs an fsync; the engine chooses per
    /// transaction where that price is worth paying.
    ///
    /// Failure semantics: if the commit applies but the fsync then fails,
    /// the transaction IS committed — visible, process-crash durable, not
    /// yet power-cut durable. The error is [`CommitFailure::Io`], reporting
    /// the durability shortfall, not a rollback.
    fn commit_durable(self) -> std::result::Result<Committed, CommitFailure>
    where
        Self: Sized;

    /// Abort, consuming Open into [`Aborted`] without applying writes.
    /// Named — not Drop-as-abort.
    fn abort(self) -> Aborted
    where
        Self: Sized;
}
