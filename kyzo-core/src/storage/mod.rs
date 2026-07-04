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
//! - **v3 — time travel is bitemporal (story #69 ruling: mandatory
//!   bitemporality, one format with no past).** The single-axis skip scan
//!   (newest version at or before one validity) is replaced by the
//!   two-axis [`ReadTx::range_skip_scan_tuple`]: every versioned key ends
//!   with a valid-instant slot and a system-version slot (flags pinned;
//!   `check_key_for_bitemporal` refuses flag-bearing slots), and a row's
//!   polarity — assert / retract / erase — lives in its VALUE, so one
//!   valid instant has exactly one system lineage and contradictory
//!   lineages at an instant are unrepresentable. System timestamps are
//!   minted per write transaction from the storage's monotone clock
//!   (`max(now_µs, last + 1)`), STRICTLY AFTER the transaction's snapshot
//!   is open (in fjall the mint takes the open snapshot as an argument,
//!   so the reverse order is unrepresentable; the sim mints and snapshots
//!   under one state lock). Why the order is load-bearing: the invariant
//!   is that READS-FROM ORDER AGREES WITH STAMP ORDER — if a transaction
//!   can read another's write, its stamp strictly exceeds that writer's.
//!   Proof from the order alone: a writer is visible in my snapshot only
//!   if its commit preceded my snapshot (the backend's own snapshot
//!   machinery is atomic with respect to commits); that writer minted
//!   before it committed, hence before my snapshot, hence before my
//!   mint — and the clock is strictly monotone. Minting BEFORE the
//!   snapshot broke this (a rival could mint later yet commit sooner and
//!   be read, and our write landed at a smaller stamp than a write we
//!   read — shadowed forever, a lost update with zero conflicts; found
//!   live and pinned by
//!   `concurrent_increments_lose_nothing_at_the_storage_layer`).
//!   Anti-dependencies and same-fact races abort one side (the
//!   current-state probe's tracked range read). Every committed history
//!   is therefore serializable in stamp order, and an as-of cut at any
//!   system time is a genuine serial-order prefix — with one named
//!   exception: `batch_put` (bulk import) is OUTSIDE this surface — it
//!   preserves imported stamps and mints nothing, which is sound only
//!   into a fresh store, so both backends REFUSE a non-empty target, and
//!   restore raises the target's clock floor to the dump's before
//!   importing (a target can never re-mint an imported instant).

use std::fmt;

use itertools::Itertools;
use miette::{Result, bail, miette};

use crate::data::tuple::{Tuple, decode_tuple_from_kv};
use crate::data::value::{AsOf, ValidityTs};

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

/// The storage's monotone system clock: mints each write transaction's
/// system timestamp as `max(now_µs, last + 1)`, so stamps never repeat
/// and never regress within a process, whatever the wall clock does.
/// Cross-restart monotonicity is the opener's job: seed the clock with a
/// floor at least as high as any stamp the store already contains (the
/// fjall backend persists a watermark in its meta keyspace; the
/// deterministic backends use logical time and need no floor).
pub(crate) struct SystemClock(std::sync::atomic::AtomicI64);

impl SystemClock {
    /// A clock that will never mint at or below `floor`.
    pub(crate) fn new(floor: i64) -> Self {
        SystemClock(std::sync::atomic::AtomicI64::new(floor))
    }

    /// Mint the next stamp: strictly greater than every stamp minted
    /// before it, and equal to the wall clock whenever the wall clock is
    /// ahead.
    pub(crate) fn stamp(&self, now_micros: i64) -> ValidityTs {
        use std::sync::atomic::Ordering;
        let mut last = self.0.load(Ordering::Relaxed);
        loop {
            let next = now_micros.max(last + 1);
            match self
                .0
                .compare_exchange_weak(last, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => return ValidityTs(std::cmp::Reverse(next)),
                Err(observed) => last = observed,
            }
        }
    }

    /// The current floor: every future mint is strictly above this.
    pub(crate) fn floor(&self) -> i64 {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Raise the floor (never lowers it). Restore uses this so stamps in
    /// imported history can never be minted again by the target store.
    pub(crate) fn raise_floor(&self, to: i64) {
        self.0.fetch_max(to, std::sync::atomic::Ordering::AcqRel);
    }
}

/// The version of the on-disk memcmp/tuple encoding. Stamped into every
/// store and every dump file; a mismatch refuses to open rather than read
/// garbage. Any change to the encoding is a migration and must bump
/// [`FormatVersion::CURRENT`] (see .claude/rules/memcmp.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatVersion(u16);

impl FormatVersion {
    /// The format this build reads and writes. v3 is the bitemporal
    /// format with self-describing fact values: two pinned time slots in
    /// every fact key, polarity in the value, `keyspace_kind` in the
    /// catalog row, and fact payloads as count + offset table + tagged
    /// fields (`data/tuple.rs::encode_fact_payload`) so any field of any
    /// row is one O(1) slice and scalar fields are fixed-width slots the
    /// columnar engine gathers without a parser. One format per version —
    /// an older store refuses to open, it is never migrated in place.
    ///
    /// v4 (story #62) adds ONE new memcmp tag — the first-class `Interval`
    /// `DataValue` (`data/memcmp.rs::INTERVAL_TAG`) — with every existing
    /// tag, field layout, and key shape untouched. Decision: bump anyway,
    /// not skip. `FormatVersion` exists so a mismatched decoder never
    /// silently misinterprets bytes; a v3 decoder handed a key containing
    /// `INTERVAL_TAG` doesn't know that byte, and rejects it as corruption
    /// rather than reading a value — exactly the failure mode this stamp
    /// exists to turn into a refuse-to-open at the door instead. "No
    /// deployed stores" (there are none yet) makes the bump free, not
    /// optional: the decodable tag space is part of the format's identity
    /// same as the tags already in it.
    pub const CURRENT: FormatVersion = FormatVersion(4);

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
    /// The system clock's current floor: every stamp this store will
    /// ever mint is strictly above it. A dump carries this so a restore
    /// can guarantee the target never re-mints a stamp that already
    /// appears in imported history.
    fn clock_floor(&self) -> Result<ValidityTs>;

    /// Raise the system clock's floor (never lowers it). Restore calls
    /// this with the dump's floor before importing rows.
    fn raise_clock_floor(&self, floor: ValidityTs) -> Result<()>;

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

    /// Bitemporal as-of scan: among keys differing only in their two
    /// trailing time slots (valid instant outer, system version inner —
    /// [`EncodedKey::BITEMPORAL_TAIL_LEN`](crate::data::tuple::EncodedKey)),
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
