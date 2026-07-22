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
//! [`ReadTx`](super::tx::ReadTx) is one consistent snapshot and *cannot* write — the operations
//! do not exist on it; a [`WriteTx`](super::tx::WriteTx) is the **Open** phase of a write
//! transaction — put/del exist only there. [`WriteTx::commit`](super::tx::WriteTx::commit) and
//! [`WriteTx::abort`](super::tx::WriteTx::abort) are consuming verbs: Open is spent and the successor is
//! [`Committed`](super::tx::Committed) or [`Aborted`](super::tx::Aborted). Use-after-commit/abort cannot compile —
//! the Open type has disappeared. There is no `finished` flag and no
//! Drop-as-abort; dropping an unfinished Open is a drop-bomb.
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
//!   typed ([`ConflictError`](super::tx::ConflictError)) and retryable — retry loops are the
//!   engine's job (see [`super::retry`]).
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
//!   retryable [`ConflictError`](super::tx::ConflictError). For the record: FoundationDB- and
//!   badger-class optimistic oracles validate reads only (blind writes are
//!   a deliberate throughput feature there); PostgreSQL SSI and
//!   TiKV/Percolator abort write-write races. KyzoDB sides with the latter.
//! - **v3 — time travel is bitemporal (story #69 ruling: mandatory
//!   bitemporality, one format with no past).** The single-axis skip scan
//!   (newest version at or before one validity) is replaced by the
//!   two-axis [`ReadTx::range_skip_scan_tuple`](super::tx::ReadTx::range_skip_scan_tuple): every versioned key ends
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
//! - **v4 — the as-of skip scan seeks one cursor per walk, never reopens
//!   (story #118 task 1 ruling).** [`ReadTx::range_skip_scan_tuple`](super::tx::ReadTx::range_skip_scan_tuple)'s
//!   per-version-step behavior changed, not its resolution semantics: all
//!   three backends now drive [`skip_walk::SkipWalk`](super::skip_walk::SkipWalk) over one cursor
//!   opened ONCE per walk (`OpenSkipCursor::open_skip_cursor`) and
//!   repositioned forward per step (`SkipCursor::seek`), instead of the
//!   previous shape of reopening a fresh bounded range per version step.
//!   On `fjall` this is the difference between paying a `SuperVersion`
//!   lookup and live-run/table/memtable resolution once per WALK versus
//!   once per STEP; `scratch`/`sim` see no efficiency change (a `BTreeMap`
//!   range call already is the real O(log n) seek) but share the same
//!   driver so the one theorem
//!   (`skip_walk_matches_independent_oracle_over_2000_seeded_histories`,
//!   `skip_walk_opens_exactly_one_cursor_per_walk`) covers every backend.
//!   No caller-visible semantics moved: the resolved tuples, their order,
//!   and the bitemporal resolution rule are unchanged; see `skip_walk.rs`'s
//!   module doc for the full per-backend wiring and the termination
//!   guarantee.

use std::fmt;

use miette::{Result, bail, miette};

use kyzo_model::value::ValidityTs;

use super::tx::{ReadTx, WriteTx};

/// The storage's monotone system clock: mints each write transaction's
/// system timestamp as `max(now_µs, last + 1)`, so stamps never repeat
/// and never regress within a process, whatever the wall clock does.
/// Cross-restart monotonicity is the opener's job: seed the clock with a
/// floor at least as high as any stamp the store already contains (the
/// fjall backend persists a watermark in its meta keyspace; the
/// deterministic backends use logical time and need no floor).
pub(crate) struct SystemClock(std::sync::atomic::AtomicI64);

/// Typed refusal when [`SystemClock::stamp`] cannot mint another stamp.
/// Identity is the variant — never a process abort at `i64::MAX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub(crate) enum SystemClockRefuse {
    /// Stamp space is `i64`; the floor is already `i64::MAX`, so
    /// `last + 1` would wrap. Refuse rather than abort the process.
    #[error("INVARIANT(SystemClock): system timestamp space exhausted at i64::MAX")]
    #[diagnostic(code(storage::system_clock::stamp_space_exhausted))]
    StampSpaceExhausted,
}

impl SystemClock {
    /// A clock that will never mint at or below `floor`.
    pub(crate) fn new(floor: i64) -> Self {
        SystemClock(std::sync::atomic::AtomicI64::new(floor))
    }

    /// Mint the next stamp: strictly greater than every stamp minted
    /// before it, and equal to the wall clock whenever the wall clock is
    /// ahead. Refuses with [`SystemClockRefuse::StampSpaceExhausted`] when
    /// the floor is already `i64::MAX` — never panics, never wraps.
    pub(crate) fn stamp(
        &self,
        now_micros: i64,
    ) -> std::result::Result<ValidityTs, SystemClockRefuse> {
        use std::sync::atomic::Ordering;
        let mut last = self.0.load(Ordering::Relaxed);
        loop {
            // INVARIANT(SystemClock): stamp space is i64; refuse rather than
            // wrap when the floor is already i64::MAX.
            let after_last = match last.checked_add(1) {
                Some(n) => n,
                None => return Err(SystemClockRefuse::StampSpaceExhausted),
            };
            let next = now_micros.max(after_last);
            match self
                .0
                .compare_exchange_weak(last, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => return Ok(ValidityTs::of_micros(next)),
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
#[repr(transparent)]
pub struct FormatVersion(u16);

const _: () = assert!(std::mem::size_of::<FormatVersion>() == std::mem::size_of::<u16>());
const _: () = assert!(std::mem::align_of::<FormatVersion>() == std::mem::align_of::<u16>());

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
    /// v5: the value plane. Row VALUES are canonical `DataValue`
    /// encodings with no relation-id header (v4 carried an 8-byte prefix
    /// and msgpack payloads); catalog metadata is msgpack through the
    /// sealed catalog door only. A v4 store's values are unreadable under
    /// v5's decoder, so the stamp turns any pre-existing store into a
    /// refuse-to-open rather than a silent misread.
    ///
    /// v6: catalog constraints and triggers persist sealed InputProgram
    /// substance (msgpack), not re-parseable source strings; decode admits
    /// each program through `InputProgram::new`. A v5 catalog row that
    /// stored `source` text is unreadable under v6's decoder.
    pub const CURRENT: FormatVersion = FormatVersion(6);

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

pub(crate) mod sealed {
    /// One backend by decree (see .claude/rules/storage.md): these traits are
    /// sealed — external crates read the contract, they do not implement it.
    pub trait Sealed {}
    impl Sealed for super::super::fjall::FjallStorage {}
    impl Sealed for super::super::scratch::TempTx {}
    impl Sealed for super::super::fjall::FjallReadTx {}
    impl Sealed for super::super::fjall::FjallWriteTx {}
    // The deterministic simulator (`kyzo-crashfs/src/sim.rs`, path-included
    // as `store::sim`) is admitted under `cfg(test)` only: the seal exists
    // to keep FOREIGN backends from implementing the contract, and the
    // simulator is not a second backend — it is the contract's own test
    // double, compiled solely into the test harness and never into the
    // shipped library. "One backend by decree" still holds for everything
    // that ships.
    #[cfg(any(test, feature = "bench-internals"))]
    impl Sealed for super::super::sim::SimStorage {}
    #[cfg(any(test, feature = "bench-internals"))]
    impl Sealed for super::super::sim::SimReadTx {}
    #[cfg(any(test, feature = "bench-internals"))]
    impl Sealed for super::super::sim::SimWriteTx {}
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
