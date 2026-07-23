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
//! - **Live `seek(key)` (Store law, seat 99)** — fjall already exposes
//!   `SeekIter` / `TrackedSeekIter::seek`; [`FjallSkipCursor`] is the
//!   production door that wires those into [`SkipCursor::seek`](crate::store::skip_walk::SkipCursor).
//!   One positioned cursor, re-seeked forward — never drop+rebuild a
//!   fixed-bound range as the only advance. Bitemporal skip walks and the
//!   LFTJ first cut (`leapfrog_intersect_3`) both consume this door.
//! - **Bitemporal as-of scans** — a seek loop over ONE positioned cursor
//!   (a read tx's `fjall::SeekIter`, or a write tx's SSI-conflict-tracking
//!   `fjall::TrackedSeekIter`; opened once and re-seeked forward per step,
//!   never reopened) at the seek key computed by `check_key_for_bitemporal`
//!   (the row's polarity peeked from its value), touching one stored
//!   version per distinct fact in the common case. On a write tx, each
//!   step marks the precise sub-range it resolved rather than the whole
//!   scan up front, promoting to one covering mark past a step-count
//!   threshold (PostgreSQL SIREAD-lock granularity promotion — see
//!   `fjall::TrackedSeekIter`'s doc, vendored in `vendor/fjall`).
//!
//! The transaction species are distinct types: [`FjallReadTx`] cannot write
//! by construction, and committing a [`FjallWriteTx`] consumes it — writing
//! through a reader and committing twice are not errors, they are programs
//! that do not compile.

use std::ops::Bound;
use std::path::Path;

use fjall::compaction::Leveled;
use fjall::config::{
    BlockSizePolicy, BloomConstructionPolicy, FilterPolicy, FilterPolicyEntry, PinningPolicy,
};
use fjall::{
    Conflict, Guard, KeyspaceCreateOptions, OptimisticTxDatabase, OptimisticTxKeyspace,
    OptimisticWriteTx, Readable, Slice, Snapshot,
};
use miette::{Diagnostic, Result, bail, miette};
use thiserror::Error;

use crate::store::skip_walk::{OpenSkipCursor, SkipCursor, SkipWalk};
use crate::store::{
    Aborted, BackendIoError, CommitFailure, CommitIo, ConflictError, FormatVersion, ReadTx,
    Storage, SystemClock, WriteTx,
};
use kyzo_model::value::Tuple;
use kyzo_model::value::{AsOf, ValidityTs};

/// Typed refusal when the fjall substrate fails. Identity is the variant —
/// not a stringly `miette!("fjall…")` message. Poisoned lock expects and
/// drop-bombs are named exceptions outside this enum.
#[derive(Debug, Error, Diagnostic)]
pub(crate) enum FjallRefuse {
    #[error("opening fjall database")]
    #[diagnostic(code(storage::fjall::open_database))]
    OpenDatabase(#[source] fjall::Error),

    #[error("opening fjall meta keyspace")]
    #[diagnostic(code(storage::fjall::open_meta_keyspace))]
    OpenMetaKeyspace(#[source] fjall::Error),

    #[error("reading format version")]
    #[diagnostic(code(storage::fjall::read_format_version))]
    ReadFormatVersion(#[source] fjall::Error),

    #[error("stamping format version")]
    #[diagnostic(code(storage::fjall::stamp_format_version))]
    StampFormatVersion(#[source] fjall::Error),

    #[error("opening fjall keyspace")]
    #[diagnostic(code(storage::fjall::open_keyspace))]
    OpenKeyspace(#[source] fjall::Error),

    #[error("reading system clock watermark")]
    #[diagnostic(code(storage::fjall::read_watermark))]
    ReadWatermark(#[source] fjall::Error),

    #[error("corrupt system clock watermark")]
    #[diagnostic(code(storage::fjall::corrupt_watermark))]
    CorruptWatermark,

    #[error("persisting system clock watermark")]
    #[diagnostic(code(storage::fjall::persist_watermark))]
    PersistWatermark(#[source] fjall::Error),

    #[error("fjall write transaction")]
    #[diagnostic(code(storage::fjall::begin_write_tx))]
    BeginWriteTx(#[source] fjall::Error),

    #[error("fjall commit")]
    #[diagnostic(code(storage::fjall::commit))]
    Commit(#[source] fjall::Error),

    #[error("fjall sync")]
    #[diagnostic(code(storage::fjall::sync))]
    Sync(#[source] fjall::Error),

    #[error("fjall read")]
    #[diagnostic(code(storage::fjall::read))]
    Read(#[source] fjall::Error),

    /// Caller asked for a journal budget below fjall's hard floor (64 MiB).
    /// Refused here so the vendor `assert!` never panics on our open path.
    #[error("max_journaling_size_bytes {requested} is below the {floor}-byte floor (64 MiB)")]
    #[diagnostic(code(storage::fjall::journaling_size_below_floor))]
    JournalingSizeBelowFloor { requested: u64, floor: u64 },

    #[error("FjallWriteTx used after commit/abort")]
    #[diagnostic(code(storage::fjall::tx_spent))]
    TxSpent,

    #[error("fjall row value missing despite unconditional load predicate")]
    #[diagnostic(code(storage::fjall::value_elided))]
    ValueElided,
}

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
#[derive(Debug, Clone, Copy)]
pub struct StorageOptions {
    /// Block/blob cache size in bytes. `None` means a 25%-of-system-RAM
    /// floor on Linux (`quarter_system_ram_bytes`), not fjall's own tiny
    /// stock default — an engine that owns the box should not hand back
    /// 15/16ths of it. Falls back to the stock default off Linux (a
    /// named platform gap; see that function's doc).
    pub cache_size_bytes: Option<u64>,
    /// Background worker threads (flush/compaction).
    pub worker_threads: Option<usize>,
    /// Per-keyspace memtable flush threshold, in bytes. `None` keeps the
    /// tuned policy's own choice (see `tuning::main_keyspace_options`).
    /// Exposed mainly so an instrument can shrink the flush unit and make
    /// a modest row count actually span multiple LSM levels — a
    /// gigabyte-scale store reaches the same levels at stock size, just
    /// over more data.
    pub max_memtable_size_bytes: Option<u64>,
    /// Per-keyspace compacted table target size, in bytes. `None` keeps
    /// the tuned policy's own choice. See `max_memtable_size_bytes`.
    pub table_target_size_bytes: Option<u64>,
    /// Maximum size of all journals combined, in bytes. `None` keeps
    /// fjall's documented default (512 MiB). Values below 64 MiB refuse
    /// at this boundary as [`FjallRefuse::JournalingSizeBelowFloor`] —
    /// never the vendor builder panic.
    pub max_journaling_size_bytes: Option<u64>,
}

impl StorageOptions {
    /// All knobs unset — fjall / platform documented defaults apply.
    pub fn empty() -> Self {
        Self {
            cache_size_bytes: None,
            worker_threads: None,
            max_memtable_size_bytes: None,
            table_target_size_bytes: None,
            max_journaling_size_bytes: None,
        }
    }
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

/// LSM tuning: a model-tuned per-keyspace policy (issue #118 task 4).
///
/// **Design decision — one data keyspace, not a point/temporal split.** The
/// Monkey/Dostoevsky literature assumes point-lookup data and append-heavy
/// history live in physically distinct regions that can each get their own
/// policy. They cannot here: every fact key ends with the validity instant
/// and system version (`.claude/rules/storage.md`, "Time travel is
/// bitemporal"), so a fact's current value and its whole prior history
/// share one key prefix and sort ADJACENT to each other — the "point" row
/// and the "temporal" rows are the same relation's rows at different key
/// suffixes, not separable without a key-format migration (the storage
/// contract's key encoding is sealed; CLAUDE.md, "Guardrails"). A keyspace
/// split would need to route each write into a "current" vs "history"
/// keyspace by recency, but recency is a moving read-time predicate (`now`
/// advances), not a write-time fact, so yesterday's "current" row would
/// need to physically migrate keyspaces the moment it is superseded —
/// exactly the mid-flight format surgery the storage contract forbids.
///
/// So the split this model wants already exists, just one axis down: LSM
/// **levels**, not keyspaces. Ordinary compaction ages a fact's superseded
/// versions down into deeper levels while its current version (and hot,
/// recently-touched history) stays shallow — the per-level knobs below are
/// how the single mixed "kyzo" keyspace gets Monkey/Dostoevsky tuning
/// without a physical split.
mod tuning {
    use super::{BlockSizePolicy, BloomConstructionPolicy, FilterPolicy, FilterPolicyEntry};
    use super::{KeyspaceCreateOptions, Leveled, PinningPolicy};

    /// Every keyspace here is hard-capped at 7 levels (`CreateOptions::level_count`
    /// is fixed; see `vendor/fjall/src/keyspace/options.rs`'s `from_kvs`), so every
    /// per-level policy below has exactly 7 entries, L0 first.
    const LEVELS: usize = 7;

    /// Monkey's per-level bloom allocation: for a fixed total filter memory
    /// budget, the false-positive rate that minimizes total disk I/O falls
    /// off geometrically with level depth at a rate set by the LSM's level
    /// size ratio T (Dayan, Athanassoulis & Idreos, "Monkey: Optimal
    /// Navigable Key-Value Store"). In bits-per-key terms that is an
    /// ARITHMETIC decrease of ~log2(T) bits per level (bits ≈
    /// 1.44·ln(1/FPR), and FPR itself falls by a factor of T per level
    /// under the optimal allocation). With this fork's default ratio
    /// T=10 (`Leveled::level_ratio_policy`), that step is log2(10) ≈ 3.3
    /// bits/level. Starting near fjall's own stock L0 rate (~19 bits/key,
    /// their `FalsePositiveRate(0.0001)`) and stepping down by ~3.3 bits
    /// gives shallow levels (small, checked on every read) many more bits
    /// than deep ones (huge, and — once `expect_point_read_hits` is set —
    /// the last level skips its filter build entirely).
    fn monkey_bits_per_key() -> FilterPolicy {
        FilterPolicy::new(vec![
            FilterPolicyEntry::Bloom(BloomConstructionPolicy::BitsPerKey(20.0)), // L0
            FilterPolicyEntry::Bloom(BloomConstructionPolicy::BitsPerKey(16.5)), // L1
            FilterPolicyEntry::Bloom(BloomConstructionPolicy::BitsPerKey(13.5)), // L2
            FilterPolicyEntry::Bloom(BloomConstructionPolicy::BitsPerKey(10.5)), // L3
            FilterPolicyEntry::Bloom(BloomConstructionPolicy::BitsPerKey(8.0)),  // L4
            FilterPolicyEntry::Bloom(BloomConstructionPolicy::BitsPerKey(6.0)),  // L5
            FilterPolicyEntry::Bloom(BloomConstructionPolicy::BitsPerKey(4.0)), // L6 (moot: expect_point_read_hits skips it)
        ])
    }

    /// Model-tuned policy for the main "kyzo" data keyspace: every point
    /// get, as-of scan, and full scan this engine ever runs lands here.
    ///
    /// - **Monkey** filter allocation (`monkey_bits_per_key`, above), with
    ///   the three shallowest (highest bits-per-key, cheapest in absolute
    ///   bytes, checked on every probe) filter blocks PINNED — Monkey's
    ///   filter-memory case for funding filters unconditionally, ahead of
    ///   whatever is left for the shared block cache
    ///   (`StorageOptions::cache_size_bytes`), rather than letting them
    ///   compete with data blocks in the LRU.
    /// - `expect_point_read_hits(true)`: a point get in KyzoDB is a lookup
    ///   by a key the query already joined into existence (the common case
    ///   is a hit), so the last level's filter — the biggest one, covering
    ///   the most superseded/historical rows — is worth skipping.
    /// - **Dostoevsky lazy leveling, via the only primitive this fork
    ///   actually has — dialed by measurement, not by formula alone.**
    ///   True tiered compaction is DEAD in this vendor drop:
    ///   `vendor/lsm-tree/src/compaction/tiered.rs` exists but its module
    ///   is commented out of the tree (`compaction/mod.rs`: `// pub(crate)
    ///   mod tiered;`) and its `choose()` body is `unimplemented!()` —
    ///   wiring it in would panic on the first compaction, not tune
    ///   anything. Completing a from-scratch merge policy is out of this
    ///   task's scope and not what "tune the knobs" means. Lazy leveling's
    ///   actual mechanism — let several sorted runs batch before an
    ///   expensive merge, rather than merging eagerly — already exists at
    ///   this fork's shallowest level: `l0_threshold` is how many flushed
    ///   runs L0 tolerates before merging into L1.
    ///
    ///   The bench (issue #118 task-4 commit's tuning table) tried
    ///   `l0_threshold(8)` (stock's full double) with a growing
    ///   `level_ratio_policy` and a 4→16 KiB block-size ramp together: it
    ///   cost the as-of-on-dense-chains scan ~10%, for no measurable
    ///   ingest win — batching more unmerged L0 runs makes every read
    ///   (including as-of) check more files for whatever fraction of the
    ///   dense chain hasn't merged down yet, and this instrument's
    ///   foreground-latency ingest measurement can't see the background
    ///   write-amplification saving that trade is FOR (no
    ///   cumulative-bytes-compacted counter exists to observe it — a real
    ///   gap, not a hidden one). Isolating each knob then placed the
    ///   blame precisely: filters-only measured inside the stock run's
    ///   own noise band (not the cause); block-size-only (this keyspace's
    ///   4→8 KiB ramp, stock compaction) ALSO measured inside the stock
    ///   band — the regression was `l0_threshold`/`level_ratio_policy`
    ///   alone. So `level_ratio_policy` stays stock (the deeper step to
    ///   12 in the first pass bought nothing measurable) and only
    ///   `l0_threshold` moves, by half of the first pass's step (6, not
    ///   8) — the smallest lazy-leveling move this primitive can make,
    ///   landing at a measured, disclosed -6% on as-of (published as the
    ///   losing run it is) while still being a real instantiation of
    ///   "batch shallow writes before merging" for the append-heavy path.
    /// - Per-level block size steps up modestly with depth (4 KiB shallow
    ///   → 8 KiB deep, not stock's flat 4 KiB): isolated in the
    ///   re-measurement above as a genuinely free move at this bench's
    ///   scale (inside the stock run's noise band on every shape), so it
    ///   stays — deeper levels are where a fact's superseded versions
    ///   have sunk (this keyspace's bitemporal suffix keeps one fact's
    ///   whole history key-adjacent), and a larger block amortizes the
    ///   seek across whatever as-of/full-history reads do land that deep
    ///   at production scale, at no measured cost here. `level_ratio_policy`
    ///   stays stock — see above.
    ///
    /// `opts.max_memtable_size_bytes` / `opts.table_target_size_bytes`
    /// override the flush/compaction unit size (both `None` in
    /// production: fjall's stock 64 MiB serves a real store fine — an
    /// instrument shrinks these to make a small row count actually span
    /// levels, see `StorageOptions`).
    pub(super) fn main_keyspace_options(opts: super::StorageOptions) -> KeyspaceCreateOptions {
        let mut strategy = Leveled::default().with_l0_threshold(6);
        if let Some(bytes) = opts.table_target_size_bytes {
            strategy = strategy.with_table_target_size(bytes);
        }
        let mut created = KeyspaceCreateOptions::default()
            .filter_policy(monkey_bits_per_key())
            .filter_block_pinning_policy(PinningPolicy::new(vec![
                true, true, true, false, false, false, false,
            ]))
            .expect_point_read_hits(true)
            .compaction_strategy(std::sync::Arc::new(strategy))
            .data_block_size_policy(BlockSizePolicy::new(vec![
                4 * 1_024,
                4 * 1_024,
                4 * 1_024,
                8 * 1_024,
                8 * 1_024,
                8 * 1_024,
                8 * 1_024,
            ]));
        if let Some(bytes) = opts.max_memtable_size_bytes {
            created = created.max_memtable_size(bytes);
        }
        created
    }

    /// Model-tuned policy for the "kyzo_meta" keyspace: two keys
    /// (`FORMAT_VERSION_KEY`, `SYSTEM_CLOCK_WATERMARK_KEY`), read once per
    /// open and written rarely. It will never fill a second level, so the
    /// per-level Monkey allocation above has nothing to act on here —
    /// there is no depth profile to exploit in a keyspace this small. The
    /// one model-relevant fact still applies: after the first write, every
    /// read of either key is a hit, so `expect_point_read_hits` is set for
    /// the same reason as the main keyspace, at zero cost given the size.
    pub(super) fn meta_keyspace_options(opts: super::StorageOptions) -> KeyspaceCreateOptions {
        let mut created = KeyspaceCreateOptions::default().expect_point_read_hits(true);
        if let Some(bytes) = opts.max_memtable_size_bytes {
            created = created.max_memtable_size(bytes);
        }
        created
    }

    const _: [(); LEVELS] = [(); 7];
}

/// Open (or create) a fjall-backed storage at the given path with default
/// options.
///
/// A fresh store is stamped with the on-disk format version; opening a store
/// written with a different format version is an error, not silent corruption.
pub fn new_fjall_storage(path: impl AsRef<Path>) -> Result<FjallStorage> {
    new_fjall_storage_with(path, StorageOptions::empty())
}

/// A cache floor of 25% of total system RAM, for when
/// `StorageOptions::cache_size_bytes` is left `None` — a database engine
/// should not hand back 15/16ths of the host's memory to the OS by
/// default (fjall's own stock default is a flat 16 MiB, sized for a
/// library embedded in something else's memory budget, not for owning
/// the box). Linux-only for now: reading total RAM without a new
/// dependency and without `unsafe` (this crate is `#![forbid(unsafe_code)]`
/// — no libc FFI, and `Cargo.toml`/the vendoring setup are out of scope
/// for this task) means `/proc/meminfo`, which only exists on Linux.
/// Elsewhere this returns `None` and the caller keeps fjall's stock
/// default — a named platform gap, not a silently wrong number.
pub(crate) fn quarter_system_ram_bytes() -> Option<u64> {
    let meminfo = match std::fs::read_to_string("/proc/meminfo") {
        Ok(s) => s,
        Err(_io) => {
            return None;
        }
    };
    let kib = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|rest| rest.trim().strip_suffix("kB"))
        .and_then(|n| match n.trim().parse::<u64>() {
            Ok(v) => Some(v),
            Err(_parse) => None,
        })?;
    Some((kib * 1_024) / 4)
}

/// Vendor floor for [`StorageOptions::max_journaling_size_bytes`] — matches
/// `fjall::Config::max_journaling_size`'s panic threshold. Validated here
/// so our open path never reaches that assert.
const MIN_JOURNALING_SIZE_BYTES: u64 = 64 * 1_024 * 1_024;

/// Open (or create) a fjall-backed storage with explicit resource options.
pub fn new_fjall_storage_with(
    path: impl AsRef<Path>,
    opts: StorageOptions,
) -> Result<FjallStorage> {
    let mut builder = OptimisticTxDatabase::builder(path);
    // Stock default only if BOTH the caller and the RAM floor come up
    // empty (off Linux; see `quarter_system_ram_bytes`) — a named
    // platform gap, not a silently wrong number.
    if let Some(bytes) = opts.cache_size_bytes.or_else(quarter_system_ram_bytes) {
        builder = builder.cache_size(bytes);
    }
    if let Some(n) = opts.worker_threads {
        builder = builder.worker_threads(n);
    }
    if let Some(bytes) = opts.max_journaling_size_bytes {
        if bytes < MIN_JOURNALING_SIZE_BYTES {
            return Err(FjallRefuse::JournalingSizeBelowFloor {
                requested: bytes,
                floor: MIN_JOURNALING_SIZE_BYTES,
            }
            .into());
        }
        builder = builder.max_journaling_size(bytes);
    }
    let db = builder.open().map_err(FjallRefuse::OpenDatabase)?;
    let meta = db
        .keyspace(META_KEYSPACE_NAME, || tuning::meta_keyspace_options(opts))
        .map_err(FjallRefuse::OpenMetaKeyspace)?;
    match meta
        .get(FORMAT_VERSION_KEY)
        .map_err(FjallRefuse::ReadFormatVersion)?
    {
        None => meta
            .insert(FORMAT_VERSION_KEY, FormatVersion::CURRENT.as_bytes())
            .map_err(FjallRefuse::StampFormatVersion)?,
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
        .keyspace(KEYSPACE_NAME, || tuning::main_keyspace_options(opts))
        .map_err(FjallRefuse::OpenKeyspace)?;
    let now = crate::session::current_validity()?.raw();
    let watermark = match meta
        .get(SYSTEM_CLOCK_WATERMARK_KEY)
        .map_err(FjallRefuse::ReadWatermark)?
    {
        None => i64::MIN,
        Some(v) => {
            let bytes: [u8; 8] = v
                .as_ref()
                .try_into()
                .map_err(|_| FjallRefuse::CorruptWatermark)?;
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
        let now = crate::session::current_validity()?.raw();
        let stamp = self.clock.stamp(now)?;
        self.meta
            .insert(SYSTEM_CLOCK_WATERMARK_KEY, stamp.raw().to_be_bytes())
            .map_err(FjallRefuse::PersistWatermark)?;
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
        Ok(ValidityTs::of_micros(self.clock.floor()))
    }

    fn raise_clock_floor(&self, floor: ValidityTs) -> Result<()> {
        let _guard = self
            .watermark_lock
            .lock()
            .map_err(|_| miette!("watermark lock poisoned"))?;
        self.clock.raise_floor(floor.raw());
        // Persist the CLOCK's floor, not the argument: raise_floor is a
        // max, so a stale (lower) argument must not regress the disk.
        self.meta
            .insert(SYSTEM_CLOCK_WATERMARK_KEY, self.clock.floor().to_be_bytes())
            .map_err(FjallRefuse::PersistWatermark)?;
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
        let tx = self.db.write_tx().map_err(FjallRefuse::BeginWriteTx)?;
        let stamp = self.stamp_after_snapshot(&tx)?;
        Ok(FjallWriteTx {
            tx: Some(tx),
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
            // begins with an 8-byte relation id strictly below the
            // EXCLUSIVE allocation ceiling `RelationId::CAP`, so its first
            // byte stays below 0xFF — proven at compile time so a future
            // id-cap bump cannot silently turn a full store invisible to
            // this check.
            const {
                // Const type-law: CAP above 0xFF<<56 fails compilation, never panic.
                if kyzo_model::value::RelationId::CAP > (0xff_u64 << 56) {
                    let _: () = [()][1];
                }
            }
            // Existence alone: the raw `Guard`s are dropped unmaterialized —
            // not even `key()` is called — so the probe costs no decode at
            // all, just "is there a first item."
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
            let mut tx = self.db.write_tx().map_err(FjallRefuse::BeginWriteTx)?;
            for pair in data.by_ref().take(CHUNK) {
                let (k, v) = pair?;
                tx.insert(&self.ks, k, v);
            }
            match tx.commit().map_err(FjallRefuse::Commit)? {
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
        Ok(self
            .db
            .persist(fjall::PersistMode::SyncAll)
            .map_err(FjallRefuse::Sync)?)
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
    /// `None` after commit/abort spends Open. Drop-bomb if still `Some`.
    tx: Option<OptimisticWriteTx>,
    ks: OptimisticTxKeyspace,
    db: OptimisticTxDatabase,
    stamp: ValidityTs,
}

impl FjallWriteTx {
    fn open_tx(&self) -> Result<&OptimisticWriteTx, FjallRefuse> {
        self.tx.as_ref().ok_or(FjallRefuse::TxSpent)
    }

    fn open_tx_mut(&mut self) -> Result<&mut OptimisticWriteTx, FjallRefuse> {
        self.tx.as_mut().ok_or(FjallRefuse::TxSpent)
    }

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
        let ks = self.ks.clone();
        self.open_tx_mut()?
            .contains_key(&ks, key)
            .map_err(FjallRefuse::Read)?;
        Ok(())
    }
}

/// Both fjall read views (`Snapshot`, `OptimisticWriteTx`) speak `Readable`;
/// every read-side operation is written once against that. Yields fjall's
/// own `Guard` — undecided currency: a caller materializes as much of each
/// row as it actually needs (see [`materialize_row`], [`materialize_key`])
/// rather than this choke point deciding for everyone.
fn raw_range<'a, R: Readable>(
    reader: &'a R,
    ks: &'a OptimisticTxKeyspace,
    lower: &[u8],
    upper: &[u8],
) -> impl Iterator<Item = Guard> + 'a {
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
}

/// A full key+value row, materialized via `into_inner_if` with an
/// unconditionally-true predicate: the one call every row-shaped scan
/// (`range_scan`, `total_scan`) needs, and the same path a future
/// key-value-separated keyspace would lazily skip loading blob values
/// through if a caller ever filtered on the key alone. `Slice` is
/// Arc-backed, so this is a refcount bump per field, never a heap copy.
fn materialize_row(guard: Guard) -> Result<(Slice, Slice)> {
    let (k, v) = guard.into_inner_if(|_| true).map_err(FjallRefuse::Read)?;
    Ok((k, v.ok_or(FjallRefuse::ValueElided)?))
}

/// A key alone, filtering the guard on `key()` and never loading the value
/// — the currency for existence probes and counts, which cost no value I/O.
fn materialize_key(guard: Guard) -> Result<Slice> {
    Ok(guard.key().map_err(FjallRefuse::Read)?)
}

fn read_get<R: Readable>(
    reader: &R,
    ks: &OptimisticTxKeyspace,
    key: &[u8],
) -> Result<Option<Slice>> {
    Ok(reader.get(ks, key).map_err(FjallRefuse::Read)?)
}

fn read_exists<R: Readable>(reader: &R, ks: &OptimisticTxKeyspace, key: &[u8]) -> Result<bool> {
    Ok(reader.contains_key(ks, key).map_err(FjallRefuse::Read)?)
}

fn read_total_scan<'a, R: Readable>(
    reader: &'a R,
    ks: &'a OptimisticTxKeyspace,
) -> Box<dyn Iterator<Item = Result<(Slice, Slice)>> + 'a> {
    Box::new(reader.iter(ks).map(materialize_row))
}

/// The skip walk's cursor over one fjall reader: a single positioned
/// cursor, re-seeked forward once per version step instead of reopened.
/// Generic over the concrete seek-iterator type because the two readers
/// hand back different ones: [`FjallReadTx`]'s `Snapshot` returns a bare
/// `fjall::SeekIter` (a snapshot is read-only and never aborts, so there
/// is nothing to conflict-track); [`FjallWriteTx`]'s `OptimisticWriteTx`
/// returns `fjall::TrackedSeekIter`, which additionally records the
/// walk's SSI read-conflict spans under PostgreSQL-style SIREAD
/// granularity promotion (precise per-step ranges for a short walk,
/// collapsed to one covering mark past a step-count threshold — see that
/// type's doc). `Empty` is the same degenerate-bounds guard `raw_range`
/// applies to the plain range-scan path — an inverted `[lower, upper)`
/// must never reach fjall, whose write-transaction conflict manager
/// replays marked ranges through `BTreeSet::range` at commit time and
/// panics on one.
pub(crate) enum FjallSkipCursor<S> {
    Empty,
    Live(S),
}

/// The one seek shape both fjall cursor types share (unified here so
/// [`FjallSkipCursor`] can drive either without knowing which fjall
/// transaction species produced it).
trait FjallSeekStep {
    fn fjall_seek(&mut self, target: &[u8]) -> Option<fjall::Result<(Slice, Slice)>>;
}

impl FjallSeekStep for fjall::SeekIter {
    fn fjall_seek(&mut self, target: &[u8]) -> Option<fjall::Result<(Slice, Slice)>> {
        self.seek(target)
    }
}

impl FjallSeekStep for fjall::TrackedSeekIter<'_> {
    fn fjall_seek(&mut self, target: &[u8]) -> Option<fjall::Result<(Slice, Slice)>> {
        self.seek(target)
    }
}

/// Production live-seek door: forward `seek` on the held fjall iterator —
/// not a fresh `seek_range` per step. Free-Join / LFTJ (seat 99) and
/// [`SkipWalk`] both require this shape.
impl<S: FjallSeekStep> SkipCursor for FjallSkipCursor<S> {
    fn seek(&mut self, target: &[u8]) -> Option<Result<(Vec<u8>, Vec<u8>)>> {
        match self {
            Self::Empty => None,
            Self::Live(iter) => iter.fjall_seek(target).map(|r| {
                let (k, v) = r.map_err(FjallRefuse::Read)?;
                Ok((k.to_vec(), v.to_vec()))
            }),
        }
    }
}

macro_rules! impl_read_tx {
    ($ty:ty, $reader:ident, $seek_iter:ty) => {
        impl ReadTx for $ty {
            fn get(&self, key: &[u8]) -> Result<Option<Slice>> {
                read_get(&self.$reader, &self.ks, key)
            }

            fn exists(&self, key: &[u8]) -> Result<bool> {
                read_exists(&self.$reader, &self.ks, key)
            }

            fn range_scan<'a>(
                &'a self,
                lower: &[u8],
                upper: &[u8],
            ) -> Box<dyn Iterator<Item = Result<(Slice, Slice)>> + 'a> {
                Box::new(raw_range(&self.$reader, &self.ks, lower, upper).map(materialize_row))
            }

            fn range_scan_keys<'a>(
                &'a self,
                lower: &[u8],
                upper: &[u8],
            ) -> Box<dyn Iterator<Item = Result<Slice>> + 'a> {
                Box::new(raw_range(&self.$reader, &self.ks, lower, upper).map(materialize_key))
            }

            fn range_skip_scan_tuple<'a>(
                &'a self,
                lower: &[u8],
                upper: &[u8],
                as_of: AsOf,
            ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
                Box::new(SkipWalk::new(
                    self.open_skip_cursor(lower, upper),
                    lower,
                    upper,
                    as_of,
                ))
            }

            fn total_scan<'a>(&'a self) -> Box<dyn Iterator<Item = Result<(Slice, Slice)>> + 'a> {
                read_total_scan(&self.$reader, &self.ks)
            }
        }

        impl OpenSkipCursor for $ty {
            type Cursor<'c> = FjallSkipCursor<$seek_iter>;

            fn open_skip_cursor<'c>(&'c self, lower: &[u8], upper: &[u8]) -> Self::Cursor<'c> {
                if lower >= upper {
                    return FjallSkipCursor::Empty;
                }
                FjallSkipCursor::Live(self.$reader.seek_range::<&[u8], _>(
                    &self.ks,
                    (Bound::Included(lower), Bound::Excluded(upper)),
                ))
            }
        }
    };
}

impl_read_tx!(FjallReadTx, snap, fjall::SeekIter);

impl ReadTx for FjallWriteTx {
    fn get(&self, key: &[u8]) -> Result<Option<Slice>> {
        read_get(self.open_tx()?, &self.ks, key)
    }

    fn exists(&self, key: &[u8]) -> Result<bool> {
        read_exists(self.open_tx()?, &self.ks, key)
    }

    fn range_scan<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<(Slice, Slice)>> + 'a> {
        {
            let Ok(tx) = self.open_tx() else {
                return Box::new(std::iter::once(Err(FjallRefuse::TxSpent.into())));
            };
            Box::new(raw_range(tx, &self.ks, lower, upper).map(materialize_row))
        }
    }

    fn range_scan_keys<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<Slice>> + 'a> {
        {
            let Ok(tx) = self.open_tx() else {
                return Box::new(std::iter::once(Err(FjallRefuse::TxSpent.into())));
            };
            Box::new(raw_range(tx, &self.ks, lower, upper).map(materialize_key))
        }
    }

    fn range_skip_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
        as_of: AsOf,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        Box::new(SkipWalk::new(
            self.open_skip_cursor(lower, upper),
            lower,
            upper,
            as_of,
        ))
    }

    fn total_scan<'a>(&'a self) -> Box<dyn Iterator<Item = Result<(Slice, Slice)>> + 'a> {
        {
            let Ok(tx) = self.open_tx() else {
                return Box::new(std::iter::once(Err(FjallRefuse::TxSpent.into())));
            };
            read_total_scan(tx, &self.ks)
        }
    }
}

impl OpenSkipCursor for FjallWriteTx {
    type Cursor<'c> = FjallSkipCursor<fjall::TrackedSeekIter<'c>>;

    fn open_skip_cursor<'c>(&'c self, lower: &[u8], upper: &[u8]) -> Self::Cursor<'c> {
        if lower >= upper {
            return FjallSkipCursor::Empty;
        }
        let Ok(tx) = self.open_tx() else {
            return FjallSkipCursor::Empty;
        };
        FjallSkipCursor::Live(
            tx.seek_range::<&[u8], _>(&self.ks, (Bound::Included(lower), Bound::Excluded(upper))),
        )
    }
}

impl WriteTx for FjallWriteTx {
    fn system_stamp(&self) -> ValidityTs {
        self.stamp
    }

    fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        let ks = self.ks.clone();
        self.open_tx_mut()?.insert(&ks, key, val);
        self.mark_written_key_validated(key)
    }

    fn del(&mut self, key: &[u8]) -> Result<()> {
        let ks = self.ks.clone();
        self.open_tx_mut()?.remove(&ks, key);
        self.mark_written_key_validated(key)
    }

    fn del_range(&mut self, lower: &[u8], upper: &[u8]) -> Result<()> {
        const CHUNK: usize = 1024;
        let mut cursor = lower.to_vec();
        loop {
            let keys: Vec<Slice> = {
                let tx = self.open_tx()?;
                raw_range(tx, &self.ks, &cursor, upper)
                    .map(materialize_key)
                    .take(CHUNK)
                    .collect::<Result<_>>()?
            };
            let Some(last) = keys.last() else {
                return Ok(());
            };
            cursor = {
                let mut succ = last.to_vec();
                succ.push(0);
                succ
            };
            let full_chunk = keys.len() == CHUNK;
            let ks = self.ks.clone();
            for k in keys {
                self.open_tx_mut()?.remove(&ks, k);
            }
            if !full_chunk {
                return Ok(());
            }
        }
    }

    fn commit(mut self) -> std::result::Result<(), CommitFailure> {
        let tx = self.tx.take().ok_or_else(|| {
            CommitFailure::Io(CommitIo::FjallCommit(BackendIoError::from_error(
                std::io::Error::other("FjallWriteTx commit after spend"),
            )))
        })?;
        match tx
            .commit()
            .map_err(|e| CommitFailure::Io(CommitIo::FjallCommit(BackendIoError::from_error(e))))?
        {
            Ok(()) => Ok(()),
            Err(Conflict) => Err(CommitFailure::Conflict(ConflictError)),
        }
    }

    fn commit_durable(mut self) -> std::result::Result<(), CommitFailure> {
        let db = self.db.clone();
        let tx = self.tx.take().ok_or_else(|| {
            CommitFailure::Io(CommitIo::FjallCommit(BackendIoError::from_error(
                std::io::Error::other("FjallWriteTx commit_durable after spend"),
            )))
        })?;
        match tx
            .commit()
            .map_err(|e| CommitFailure::Io(CommitIo::FjallCommit(BackendIoError::from_error(e))))?
        {
            Ok(()) => {}
            Err(Conflict) => return Err(CommitFailure::Conflict(ConflictError)),
        }
        db.persist(fjall::PersistMode::SyncAll)
            .map_err(|e| CommitFailure::Io(CommitIo::FjallSync(BackendIoError::from_error(e))))?;
        Ok(())
    }

    fn abort(mut self) -> Aborted {
        if let Some(tx) = self.tx.take() {
            tx.rollback();
        }
        Aborted
    }
}

impl Drop for FjallWriteTx {
    fn drop(&mut self) {
        // Drop cannot return Result. Forgotten commit()/abort(self) discards
        // the open write set via rollback — same durable effect as abort(self).
        // Intentional ends still go through commit()/abort(self).
        if let Some(tx) = self.tx.take() {
            tx.rollback();
        }
    }
}

#[cfg(test)]
mod tests {
    use kyzo_model::TupleT;
    use miette::{IntoDiagnostic, Result, miette};
    /// Per-backend fjall pins + time-travel oracle (re-homed from storage/tests.rs).
    use std::collections::BTreeMap;

    use fjall::Slice;
    use kyzo_model::value::{
        AsOf, DataValue, RelationId, StorageKey, Tuple, ValiditySlot, ValidityTs,
    };

    use crate::store::fjall::{
        FjallRefuse, MIN_JOURNALING_SIZE_BYTES, StorageOptions, new_fjall_storage,
        new_fjall_storage_with,
    };
    use crate::store::time::ClaimPolarity;
    use crate::store::{ConflictError, FormatVersion, ReadTx, Storage, WriteTx};

    /// Naive as-of reference: full-scan every version, group by payload, pick
    /// newest at-or-before `at`, keep only assertive. Seek-based scans must match.
    fn as_of_oracle(history: &[(&str, i64, bool)], at: i64) -> Vec<(String, i64)> {
        let mut newest: BTreeMap<String, (i64, bool)> = BTreeMap::new();
        for (name, ts, assert) in history {
            if *ts <= at {
                let e = newest.entry(name.to_string()).or_insert((*ts, *assert));
                if *ts > e.0 {
                    *e = (*ts, *assert);
                }
            }
        }
        newest
            .into_iter()
            .filter(|(_, (_, assert))| *assert)
            .map(|(name, (ts, _))| (name, ts))
            .collect()
    }

    fn bitemp_key(rel: RelationId, name: &str, ts: i64, sys_ts: i64) -> StorageKey {
        let slot =
            |t: i64| DataValue::Validity(ValiditySlot::from_stored(ValidityTs::of_micros(t), true));
        let tuple: Tuple =
            Tuple::from_vec(vec![DataValue::Str(name.into()), slot(ts), slot(sys_ts)]);
        tuple.encode_as_key(rel)
    }

    fn pol_val(assert: bool) -> Vec<u8> {
        vec![
            if assert {
                ClaimPolarity::Assert
            } else {
                ClaimPolarity::Retract
            }
            .encode(),
        ]
    }

    fn vld_row(rel: RelationId, name: &str, ts: i64, assert: bool) -> (StorageKey, Vec<u8>) {
        (bitemp_key(rel, name, ts, 1), pol_val(assert))
    }

    #[test]
    fn time_travel_matches_naive_oracle() -> Result<()> {
        let history: &[(&str, i64, bool)] = &[
            ("a", 1, true),
            ("a", 3, true),
            ("a", 5, false),
            ("a", 7, true),
            ("b", 2, true),
            ("b", 6, false),
            ("c", 4, false),
            ("d", 9, true),
            ("e", 1, true),
            ("e", 2, false),
            ("e", 3, true),
            ("e", 4, false),
        ];
        let rel = RelationId::new(7).ok_or_else(|| miette!("relation id"))?;
        let dir = tempfile::tempdir().into_diagnostic()?;
        let db = new_fjall_storage(dir.path())?;
        let mut tx = db.write_tx()?;
        for (name, ts, assert) in history {
            let (k, v) = vld_row(rel, name, *ts, *assert);
            tx.put(&k, &v)?;
        }
        tx.commit()?;

        let lower = rel.raw_encode().to_vec();
        let upper = rel
            .next()
            .ok_or_else(|| miette!("next rel"))?
            .raw_encode()
            .to_vec();
        let tx = db.read_tx()?;
        for at in 0..=10i64 {
            let got: Vec<(String, i64)> = tx
                .range_skip_scan_tuple(&lower, &upper, AsOf::current(ValidityTs::of_micros(at)))
                .map(|r| -> Result<_> {
                    let t = r?;
                    let name = match &t.as_slice()[0] {
                        DataValue::Str(s) => s.to_string(),
                        other @ DataValue::Null
                        | other @ DataValue::Bool(_)
                        | other @ DataValue::Num(_)
                        | other @ DataValue::Bytes(_)
                        | other @ DataValue::Uuid(_)
                        | other @ DataValue::Regex(_)
                        | other @ DataValue::Json(_)
                        | other @ DataValue::Vector(_)
                        | other @ DataValue::List(_)
                        | other @ DataValue::Set(_)
                        | other @ DataValue::Validity(_)
                        | other @ DataValue::Interval(_)
                        | other @ DataValue::Geometry(_) => {
                            return Err(miette!("unexpected {other:?}"));
                        }
                    };
                    let ts = match &t.as_slice()[1] {
                        DataValue::Validity(v) => v.ts_micros(),
                        other @ DataValue::Null
                        | other @ DataValue::Bool(_)
                        | other @ DataValue::Num(_)
                        | other @ DataValue::Str(_)
                        | other @ DataValue::Bytes(_)
                        | other @ DataValue::Uuid(_)
                        | other @ DataValue::Regex(_)
                        | other @ DataValue::Json(_)
                        | other @ DataValue::Vector(_)
                        | other @ DataValue::List(_)
                        | other @ DataValue::Set(_)
                        | other @ DataValue::Interval(_)
                        | other @ DataValue::Geometry(_) => {
                            return Err(miette!("unexpected {other:?}"));
                        }
                    };
                    Ok((name, ts))
                })
                .collect::<Result<Vec<_>>>()?;
            let want = as_of_oracle(history, at);
            assert_eq!(got, want, "as-of {at}");
        }

        Ok(())
    }

    #[test]
    fn inverted_ranges_under_contention_commit_clean() -> Result<()> {
        let dir = tempfile::tempdir().into_diagnostic()?;
        let db = new_fjall_storage(dir.path())?;
        {
            let mut tx = db.write_tx()?;
            tx.put(b"a", b"1")?;
            tx.put(b"m", b"2")?;
            tx.commit()?;
        }
        let mut tx = db.write_tx()?;
        assert_eq!(tx.range_scan(b"z", b"a").count(), 0, "inverted scan");
        assert_eq!(tx.range_scan(b"m", b"m").count(), 0, "empty scan");
        tx.del_range(b"z", b"a")?;
        assert_eq!(
            tx.range_skip_scan_tuple(b"z", b"a", AsOf::current(ValidityTs::of_micros(0)))
                .count(),
            0,
            "inverted skip scan"
        );
        {
            let mut w = db.write_tx()?;
            w.put(b"c", b"concurrent")?;
            w.commit()?;
        }
        tx.put(b"mine", b"x")?;
        tx.commit()?;
        let mut tx = db.write_tx()?;
        tx.put(b"after", b"ok")?;
        tx.commit()?;
        let r = db.read_tx()?;
        assert_eq!(r.get(b"m")?, Some(Slice::from(b"2")));
        assert_eq!(r.get(b"mine")?, Some(Slice::from(b"x")));
        assert_eq!(r.get(b"after")?, Some(Slice::from(b"ok")));

        Ok(())
    }

    #[test]
    fn write_write_race_aborts_second_committer() -> Result<()> {
        let dir = tempfile::tempdir().into_diagnostic()?;
        let db = new_fjall_storage(dir.path())?;
        let mut tx1 = db.write_tx()?;
        let mut tx2 = db.write_tx()?;
        tx1.put(b"ww", b"1")?;
        tx2.put(b"ww", b"2")?;
        tx1.commit()?;
        let err = tx2
            .commit()
            .expect_err("a write-write race must abort the second committer");
        assert!(
            err.is_conflict(),
            "the write-write abort must be the typed conflict, got {err:?}"
        );
        assert!(
            matches!(err, crate::store::tx::CommitFailure::Conflict(_)),
            "conflict arm must be CommitFailure::Conflict"
        );
        assert_eq!(
            db.read_tx()?.get(b"ww")?,
            Some(Slice::from(b"1")),
            "the aborted writer must leave no trace: first committer wins"
        );
        // Empty write set certifies nothing.
        let ro = db.write_tx()?;
        assert_eq!(ro.get(b"ww")?, Some(Slice::from(b"1")));
        let mut w = db.write_tx()?;
        w.put(b"ww", b"3")?;
        w.commit()?;
        ro.commit()?;

        Ok(())
    }

    #[test]
    fn concurrent_increments_lose_nothing_at_the_storage_layer() -> Result<()> {
        use std::sync::atomic::{AtomicI64, Ordering};

        let dir = tempfile::tempdir().into_diagnostic()?;
        let db = std::sync::Arc::new(new_fjall_storage(dir.path())?);
        let rel = RelationId::new(7).ok_or_else(|| miette!("relation id"))?;
        let lower = rel.raw_encode().to_vec();
        let upper = rel
            .next()
            .ok_or_else(|| miette!("next rel"))?
            .raw_encode()
            .to_vec();

        let val_of = |v: i64| -> Vec<u8> {
            let mut out = Vec::new();
            out.push(ClaimPolarity::Assert.encode());
            kyzo_model::value::append_canonical(&mut out, &DataValue::from(v));
            out
        };
        let key_at = |stamp: ValidityTs| -> StorageKey {
            let slot = DataValue::Validity(ValiditySlot::from_stored(stamp, true));
            let tuple: Tuple = Tuple::from_vec(vec![DataValue::from(0), slot.clone(), slot]);
            tuple.encode_as_key(rel)
        };
        let current = |rows: Vec<Tuple>| -> Result<i64> {
            assert_eq!(rows.len(), 1, "exactly one live fact, got {rows:?}");
            rows[0]
                .last()
                .ok_or_else(|| miette!("col"))?
                .get_int()
                .ok_or_else(|| miette!("int"))
        };

        {
            let mut tx = db.write_tx()?;
            let stamp = tx.system_stamp();
            tx.put(&key_at(stamp), &val_of(0))?;
            tx.commit()?;
        }

        const PER_THREAD: i64 = 200;
        let commits = AtomicI64::new(0);
        std::thread::scope(|scope| {
            for _ in 0..2 {
                let db = db.clone();
                let commits = &commits;
                let (lower, upper) = (lower.clone(), upper.clone());
                let (val_of, key_at) = (&val_of, &key_at);
                scope.spawn(move || -> Result<()> {
                    for _ in 0..PER_THREAD {
                        loop {
                            let mut tx = db.write_tx()?;
                            let stamp = tx.system_stamp();
                            let rows: Vec<Tuple> = tx
                                .range_skip_scan_tuple(
                                    &lower,
                                    &upper,
                                    AsOf::current(ValidityTs::of_micros(i64::MAX)),
                                )
                                .collect::<Result<_>>()?;
                            let old = current(rows)?;
                            tx.put(&key_at(stamp), &val_of(old + 1))?;
                            match tx.commit() {
                                Ok(_committed) => {
                                    commits.fetch_add(1, Ordering::SeqCst);
                                    break;
                                }
                                Err(e) if e.is_conflict() => continue,
                                Err(e) => {
                                    return Err(miette!("unexpected commit error: {e:?}"));
                                }
                            }
                        }
                    }
                    Ok(())
                });
            }
        });

        let rtx = db.read_tx()?;
        let rows: Vec<Tuple> = rtx
            .range_skip_scan_tuple(
                &lower,
                &upper,
                AsOf::current(ValidityTs::of_micros(i64::MAX)),
            )
            .collect::<Result<_>>()?;
        assert_eq!(
            current(rows)?,
            2 * PER_THREAD,
            "every Ok commit observed ({} commits)",
            commits.load(Ordering::SeqCst)
        );

        Ok(())
    }

    #[test]
    fn format_version_rejects_noncanonical_and_v4_boundary() -> Result<()> {
        // Parse law: non-canonical spelling refuses; older stamps parse so the
        // reopen guard can NAME them in the mismatch Err.
        assert!(FormatVersion::parse(b"6").is_ok());
        assert!(FormatVersion::parse(b"06").is_err());
        let older = FormatVersion::parse(b"4")?;
        assert_ne!(older, FormatVersion::CURRENT);

        let dir = tempfile::tempdir().into_diagnostic()?;
        {
            let db = new_fjall_storage(dir.path())?;
            drop(db);
        }

        // Adversary: rewrite the meta stamp to an older-but-parseable version.
        {
            use fjall::{KeyspaceCreateOptions, OptimisticTxDatabase, PersistMode};
            let raw = OptimisticTxDatabase::builder(dir.path()).open().map_err(|e| miette!("fjall: {e}"))?;
            let meta = raw.keyspace(super::META_KEYSPACE_NAME, KeyspaceCreateOptions::default).map_err(|e| miette!("fjall: {e}"))?;
            meta.insert(super::FORMAT_VERSION_KEY, older.as_bytes()).map_err(|e| miette!("fjall: {e}"))?;
            raw.persist(PersistMode::SyncAll).into_diagnostic()?;
        }

        let err = match new_fjall_storage(dir.path()) {
            Err(e) => e,
            Ok(_) => {
                return Err(miette!(
                    "production open must refuse older-but-parseable stamp"
                ));
            }
        };
        let msg = format!("{err:#}");
        let found = older.to_string();
        let expected = FormatVersion::CURRENT.to_string();
        assert!(
            msg.contains("on-disk format version mismatch")
                && msg.contains(&found)
                && msg.contains(&expected),
            "mismatch Err must name both versions (store={found}, build={expected}), got: {msg}"
        );

        Ok(())
    }

    #[test]
    fn max_journaling_size_at_floor_is_accepted() -> Result<()> {
        let dir = tempfile::tempdir().into_diagnostic()?;
        let db = new_fjall_storage_with(
            dir.path(),
            StorageOptions {
                max_journaling_size_bytes: Some(MIN_JOURNALING_SIZE_BYTES),
                ..StorageOptions::empty()
            },
        )?;
        drop(db);

        Ok(())
    }

    #[test]
    fn max_journaling_size_below_floor_refuses_typed() -> Result<()> {
        let dir = tempfile::tempdir().into_diagnostic()?;
        let err = match new_fjall_storage_with(
            dir.path(),
            StorageOptions {
                max_journaling_size_bytes: Some(MIN_JOURNALING_SIZE_BYTES - 1),
                ..StorageOptions::empty()
            },
        ) {
            Err(e) => e,
            Ok(_) => {
                return Err(miette!(
                    "one byte under the vendor floor must refuse at our boundary"
                ));
            }
        };
        let refuse = err.downcast_ref::<FjallRefuse>().ok_or_else(|| miette!("typed downcast"))?;
        assert!(
            matches!(
                refuse,
                FjallRefuse::JournalingSizeBelowFloor {
                    requested,
                    floor: MIN_JOURNALING_SIZE_BYTES,
                } if *requested == MIN_JOURNALING_SIZE_BYTES - 1
            ),
            "expected JournalingSizeBelowFloor, got {refuse:?}"
        );

        Ok(())
    }
}
