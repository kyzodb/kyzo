/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Deterministic simulation testing (DST) at the storage seam.
//!
//! [`SimStorage`] is the contract's own test double: a second implementation
//! of [`Storage`]/[`ReadTx`]/[`WriteTx`] over an in-memory map of versions,
//! built so that *everything* — thread interleavings, injected faults,
//! crashes, power cuts — is a pure function of one `u64` seed. A failing
//! campaign prints its seed; rerunning with that seed replays the failure
//! exactly. (The practice is credited to Turso/antithesis-style DST; the
//! contract it checks is KyzoDB's own.)
//!
//! ## Why this module ships nowhere
//!
//! It is path-included as `crate::store::sim` from kyzo-core's store module
//! (`lib.rs`): compiled into the test harness (and `bench-internals`), never
//! into the published library as a foreign backend — yet visible to every
//! in-crate `#[cfg(test)]` module as `crate::store::sim`. The alternative —
//! a `test-support` cargo feature — would leak a fake backend into the
//! shipped API surface and invite depending on it; rejected.
//!
//! ## Determinism doctrine
//!
//! No wall clock, no OS randomness, no iteration over unordered maps:
//! every decision flows from [`SimRng`] (an inline splitmix64 — zero new
//! dependencies) or from the fault plan, which is a pure function of
//! `(seed, crash-epoch, op-identity, attempt, salt)`. Fault injection is
//! **identity-keyed**: an operation's identity hashes *what it is* (op kind
//! plus its key / range bounds / write-set keys) and the attempt number
//! counts how many times that identity has executed — so the plan is
//! positional in nothing, and the same seed yields the byte-identical fault
//! schedule at ANY thread count, scheduler or no scheduler. The attempt
//! component is the retry-liveness guarantee by construction: a retried
//! operation keeps its identity but advances its attempt, drawing a fresh
//! decision instead of re-drawing the identical fault forever. The crash
//! epoch increments on every simulated crash/power-cut reopen (attempt
//! counters restart with the store), so post-crash execution explores a
//! *different* — still fully deterministic — fault sequence instead of
//! replaying the pre-crash one. Under the interleaving driver the schedule
//! itself is seed-derived, so the composite is seed-reproducible end to end.
//!
//! ## What is modeled
//!
//! - **MVCC/SSI per the contract**: a transaction snapshots at open; a write
//!   transaction tracks every read key and read range (conservatively — the
//!   whole requested range, which is legal: SSI false positives are
//!   permitted); commit takes the state lock, aborts with the typed
//!   [`ConflictError`] if anything READ or WRITTEN was committed past the
//!   snapshot, then applies the write set atomically at the next sequence.
//!   Reads and writes are the conflict surface, matching the real backend
//!   exactly (contract v2 — see `storage/mod.rs`, "Contract history"):
//!   write-write races abort the second committer, first-committer-wins.
//!   A commit with an empty write set returns Ok immediately — no
//!   validation, no injection point, no sequence advance.
//! - **Two durability tiers**: `commit` advances `commit_seq` (buffer tier —
//!   survives a process crash); `commit_durable`/`sync` advance `synced_seq`
//!   (fsync tier — survives a power cut; `SyncAll` semantics, covering every
//!   earlier buffered commit, matching the fjall backend). [`sim_crash`]
//!   reopens with everything committed; [`sim_powercut`] reopens with only
//!   the fsynced prefix. Uncommitted transactions die with the old handle in
//!   both cases.
//! - **Fault injection**: reads/scans may fail transiently; `sync` and the
//!   fsync step of `commit_durable` may fail (leaving the commit applied but
//!   not power-cut durable — exactly the fjall commit-then-persist shape);
//!   commit may return a *spurious* [`ConflictError`] with no contention at
//!   all, which SSI legality permits and retry loops must survive.
//! - **Adversarial schedules**: [`run_interleaved`] runs N transaction-body
//!   closures on real threads under a token-barrier scheduler — a turn is
//!   granted only when every live participant is parked at a yield point, so
//!   execution is serialized from each participant's FIRST sim operation
//!   onward and the seed-driven pick is the *only* source of ordering from
//!   there. The segment of a body before its first sim op runs concurrently
//!   with the other bodies' pre-first-yield segments and must not touch
//!   shared test state except through sim ops. Different seeds explore
//!   different interleavings; the same seed replays the same one, exactly.
//!
//! [`sim_crash`]: SimStorage::sim_crash
//! [`sim_powercut`]: SimStorage::sim_powercut

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::sync::{Arc, Mutex, MutexGuard};

use fjall::Slice;
use miette::{Diagnostic, Result, miette};
use thiserror::Error;

use crate::store::retry::StorageOpFailure;
use crate::store::skip_walk::{OpenSkipCursor, SkipCursor, SkipWalk};
use crate::store::{Aborted, CommitFailure, CommitIo, ConflictError, ReadTx, Storage, WriteTx};
use kyzo_model::value::Tuple;
use kyzo_model::value::{AsOf, ValidityTs};

#[path = "identity.rs"]
mod identity;
use identity::{op_identity, wrap_add, wrap_mul, usize_as_u64};

/// Typed refuse for sim-storage mutex poison — never into_inner continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error, Diagnostic)]
pub(crate) enum SimRefuse {
    /// A prior holder panicked while holding a sim mutex.
    #[error("SimLockPoisoned: sim storage mutex poisoned")]
    #[diagnostic(code(store::sim::lock_poisoned))]
    LockPoisoned,
}

fn lock<'a, T>(m: &'a Mutex<T>) -> Result<MutexGuard<'a, T>, SimRefuse> {
    m.lock().map_err(|_| SimRefuse::LockPoisoned)
}

fn commit_lock<'a, T>(m: &'a Mutex<T>) -> std::result::Result<MutexGuard<'a, T>, CommitFailure> {
    lock(m).map_err(|_| CommitFailure::Io(CommitIo::SimLockPoisoned))
}

// ---------- seed-reproducible randomness ----------

/// Inline splitmix64: tiny, statistically fine for schedules and fault
/// plans, and — the point — a pure function of its seed. No new deps.
pub(crate) struct SimRng {
    state: u64,
}

impl SimRng {
    pub(crate) fn new(seed: u64) -> Self {
        SimRng { state: seed }
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        // INVARIANT(splitmix64): modular mix per the splitmix64 contract; wrap is the PRNG.
        self.state = wrap_add(self.state, 0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = wrap_mul(z ^ (z >> 30), 0xBF58_476D_1CE4_E5B9);
        z = wrap_mul(z ^ (z >> 27), 0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `[0, n)`. Modulo bias is irrelevant at test scales.
    /// `n == 0` refuses the modulo and returns `0` (no positive residue exists).
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        self.next_u64() % n
    }
}

// ---------- the fault plan: a pure function of (seed, op-identity, attempt) ----------

/// Injection rates in parts-per-million. `1_000_000` = always fire;
/// [`FaultConfig::none`] = no faults, pure semantics.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FaultConfig {
    /// Reads and scans fail transiently (IO error) at this rate.
    pub(crate) read_fail_ppm: u32,
    /// Commits fail with a spurious [`ConflictError`] — no contention
    /// required — at this rate. SSI permits false positives; retry loops
    /// must survive them. Commits with an empty write set are exempt: they
    /// never abort (matching fjall) and are not injection points.
    pub(crate) spurious_conflict_ppm: u32,
    /// `sync` / the fsync step of `commit_durable` fail (IO error) at this
    /// rate. A failed `commit_durable` leaves the commit applied but not
    /// power-cut durable, matching the real backend's commit-then-persist.
    pub(crate) sync_fail_ppm: u32,
}

impl FaultConfig {
    /// No faults — pure MVCC/SSI semantics.
    pub(crate) const fn none() -> Self {
        FaultConfig {
            read_fail_ppm: 0,
            spurious_conflict_ppm: 0,
            sync_fail_ppm: 0,
        }
    }
}

impl Default for FaultConfig {
    fn default() -> Self {
        Self::none()
    }
}

/// Salts keep the read/conflict/sync fault streams independent.
const SALT_READ: u64 = 0x5EED_0001;
const SALT_CONFLICT: u64 = 0x5EED_0002;
const SALT_SYNC: u64 = 0x5EED_0003;

/// Op-kind tags feeding [`op_identity`]: operations of different kinds never
/// share a fault identity, even over identical bytes.
const TAG_GET: u64 = 0xFA01;
const TAG_EXISTS: u64 = 0xFA02;
const TAG_RANGE: u64 = 0xFA03;
const TAG_TOTAL: u64 = 0xFA04;
const TAG_COMMIT: u64 = 0xFA05;
const TAG_SYNC: u64 = 0xFA06;

/// The fault decision: splitmix-style finalizer over
/// (seed, identity, attempt, salt), where `seed` is the crash-epoch-salted
/// seed from [`SimCtx::fault_seed`] and `attempt` is the per-identity
/// occurrence number. Deterministic, stateless, replayable — and the same
/// (identity, attempt) draws the same decision at any thread count.
fn fault_hit(seed: u64, identity: u64, attempt: u64, salt: u64, ppm: u32) -> bool {
    if ppm == 0 {
        return false;
    }
    // INVARIANT(fault_ppm): deterministic fault draw mixes identity/attempt/salt via wrap.
    let mut z = seed
        ^ wrap_mul(identity, 0x9E37_79B9_7F4A_7C15)
        ^ wrap_mul(attempt, 0xA24B_AED4_963E_E407)
        ^ wrap_mul(salt, 0xD6E8_FEB8_6659_FD93);
    z = wrap_mul(z ^ (z >> 30), 0xBF58_476D_1CE4_E5B9);
    z = wrap_mul(z ^ (z >> 27), 0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    z % 1_000_000 < u64::from(ppm)
}

// ---------- shared state: a BTreeMap of versions + two watermarks ----------

/// Per-key version chain, ascending by commit sequence; `None` = tombstone.
type VersionMap = BTreeMap<Vec<u8>, Vec<(u64, Option<Vec<u8>>)>>;

struct SimState {
    versions: VersionMap,
    /// The monotone logical system clock: DST determinism requires stamps
    /// to be a pure function of the schedule, never the wall clock. Minted
    /// under this state lock at write-transaction creation, matching the
    /// contract's snapshot-creation stamping.
    next_system_stamp: i64,
    /// Buffer-tier watermark: everything `<=` this survives a process crash.
    commit_seq: u64,
    /// Fsync-tier watermark: everything `<=` this survives a power cut.
    synced_seq: u64,
    /// Per-identity attempt counters — the fault plan's per-logical-attempt
    /// input: how many times each faultable operation identity has executed
    /// on this store handle. Restart with the store on crash reopen; the
    /// epoch salt keeps post-crash streams distinct regardless.
    attempts: BTreeMap<u64, u64>,
    /// The write-count law's oracle: the running total of `put`/`del` CALLS
    /// (not the post-collapse entry count applied to `versions`) across
    /// every transaction that has successfully committed on this handle. A
    /// transaction that calls `put`/`del` twice on the same key in-tx
    /// applies as one version-chain entry — the two calls are indistinguishable
    /// on disk — so only counting CALLS, never bytes, can catch a caller that
    /// wastefully double-fires a write whose second call clobbers the first
    /// (see `total_puts`/`total_dels` accessors on [`SimStorage`]).
    /// Observation-only: read nowhere in commit/conflict/fault logic, so it
    /// perturbs no behavior — a pure counter alongside the state it counts.
    /// Resets on crash/power-cut reopen, matching `attempts`.
    total_puts: u64,
    total_dels: u64,
}

fn snapshot_at(st: &SimState, seq: u64) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut snap = BTreeMap::new();
    for (k, vs) in &st.versions {
        if let Some((_, Some(v))) = vs.iter().rev().find(|(s, _)| *s <= seq) {
            snap.insert(k.clone(), v.clone());
        }
    }
    snap
}

/// Was `key` committed to (value or tombstone) after `start_seq`? Version
/// chains are ascending, so the last entry carries the newest sequence.
fn modified_since(st: &SimState, start_seq: u64, key: &[u8]) -> bool {
    st.versions
        .get(key)
        .is_some_and(|vs| vs.last().is_some_and(|(s, _)| *s > start_seq))
}

fn range_modified_since(st: &SimState, start_seq: u64, lower: &[u8], upper: Option<&[u8]>) -> bool {
    map_range(&st.versions, lower, upper)
        .any(|(_, vs)| vs.last().is_some_and(|(s, _)| *s > start_seq))
}

/// Range over a byte-keyed map with an optional exclusive upper bound,
/// safe against inverted bounds (`BTreeMap::range` panics on start > end;
/// the contract pins inverted ranges as simply empty).
fn map_range<'m, V>(
    map: &'m BTreeMap<Vec<u8>, V>,
    lower: &[u8],
    upper: Option<&[u8]>,
) -> impl Iterator<Item = (&'m Vec<u8>, &'m V)> + use<'m, V> {
    let inverted = upper.is_some_and(|u| lower >= u);
    let bounds: (Bound<&[u8]>, Bound<&[u8]>) = if inverted {
        // Included(x)..Excluded(x) is a legal, empty range.
        (Bound::Included(&[][..]), Bound::Excluded(&[][..]))
    } else {
        (
            Bound::Included(lower),
            match upper {
                Some(u) => Bound::Excluded(u),
                None => Bound::Unbounded,
            },
        )
    };
    map.range::<[u8], _>(bounds)
}

/// Campaign harness: run `f` once per seed; on failure, re-panic with the
/// seed stamped on the report, so the exact schedule and fault plan can be
/// replayed by rerunning with that one seed.
pub(crate) fn for_each_seed(seeds: std::ops::Range<u64>, f: impl Fn(u64)) {
    for seed in seeds {
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(seed))) {
            let msg = match payload.downcast_ref::<String>() {
                Some(s) => s.as_str(),
                None => match payload.downcast_ref::<&str>() {
                    Some(s) => *s,
                    None => "<non-string panic payload>",
                },
            };
            std::panic::resume_unwind(Box::new(format!(
                "[sim] FAILING SEED = {seed} — rerun this test with the loop pinned to \
                 seed {seed} to replay the schedule and fault plan exactly.\n{msg}"
            )));
        }
    }
}

// ---------- the storage double ----------

/// Everything a transaction needs to reach shared state, roll fault dice,
/// and yield to the scheduler.
#[derive(Clone)]
struct SimCtx {
    state: Arc<Mutex<SimState>>,
    seed: u64,
    /// Crash-epoch counter: 0 for a fresh store, +1 on every simulated
    /// crash/power-cut reopen. Salts the fault plan so each epoch explores
    /// a different fault sequence against its restarted op counter, while
    /// staying a pure function of the original seed.
    epoch: u64,
    faults: FaultConfig,
}

impl SimCtx {
    /// The fault plan's seed input: the campaign seed salted by the crash
    /// epoch. Epoch 0 leaves the seed untouched, so pre-crash fault streams
    /// are unchanged by the salting.
    fn fault_seed(&self) -> u64 {
        // INVARIANT(epoch_salt): epoch salt is a modular mix into the fault seed stream.
        self.seed ^ wrap_mul(self.epoch, 0xE703_37A9_D9C8_2D95)
    }

    /// Advance `identity`'s attempt counter and consult the fault plan.
    /// Counters advance even at rate 0, so attempt numbering is identical
    /// between fault-free and faulted runs of the same logical operations.
    fn roll_fault(&self, st: &mut SimState, identity: u64, salt: u64, ppm: u32) -> bool {
        let attempt = st.attempts.entry(identity).or_insert(0);
        *attempt += 1;
        fault_hit(self.fault_seed(), identity, *attempt, salt, ppm)
    }

    /// Consult the read arm of the fault plan for the operation `identity`.
    fn check_read_fault(&self, identity: u64) -> Result<()> {
        let mut st = lock(&self.state)?;
        if self.roll_fault(&mut st, identity, SALT_READ, self.faults.read_fail_ppm) {
            return Err(StorageOpFailure::SimInjectedReadFault.into());
        }
        Ok(())
    }
}

/// The deterministic in-memory storage double. `Clone` is shallow, like the
/// real backend: clones share one state and one commit oracle.
#[derive(Clone)]
pub(crate) struct SimStorage {
    ctx: SimCtx,
}

impl SimStorage {
    /// A fault-free simulator: pure MVCC/SSI semantics, seeded schedules.
    pub(crate) fn new(seed: u64) -> Self {
        Self::with_faults(seed, FaultConfig::none())
    }

    pub(crate) fn with_faults(seed: u64, faults: FaultConfig) -> Self {
        Self::reopen(VersionMap::new(), 0, 0, 0, seed, 0, faults)
    }

    /// `commit_seq` and `synced_seq` are separate parameters, never one
    /// shared `seq` collapsed into both (kyzodb/kyzo#91: a prior single-`seq`
    /// signature let `sim_crash` silently promote the synced watermark to
    /// the commit sequence, so a power cut simulated after an intervening
    /// crash wrongly retained buffer-tier writes that were never fsynced —
    /// a real bug the #79 DST arm caught at seed 18). A process crash and a
    /// power cut land the two tiers at DIFFERENT sequences in general; only
    /// a fresh store and a just-power-cut store happen to have them equal,
    /// and even those callers now say so explicitly rather than relying on
    /// one value doing double duty.
    fn reopen(
        versions: VersionMap,
        commit_seq: u64,
        synced_seq: u64,
        stamp_floor: i64,
        seed: u64,
        epoch: u64,
        faults: FaultConfig,
    ) -> Self {
        SimStorage {
            ctx: SimCtx {
                state: Arc::new(Mutex::new(SimState {
                    versions,
                    // Stamps stay monotone across simulated restarts: the
                    // floor carries the pre-crash clock, playing the role
                    // fjall's persisted watermark plays for real crashes.
                    next_system_stamp: stamp_floor,
                    commit_seq,
                    synced_seq,
                    attempts: BTreeMap::new(),
                    total_puts: 0,
                    total_dels: 0,
                })),
                seed,
                epoch,
                faults,
            },
        }
    }

    /// Simulated **process crash** + reopen: every committed transaction
    /// survives (buffer tier — the write reached "OS buffers" at commit);
    /// uncommitted transactions die with the old handle. The reopened store
    /// treats survivors as settled on disk, restarts its op counter at 0,
    /// keeps the same seed and fault plan, and bumps the crash epoch — so
    /// the post-crash epoch explores a different (still seed-deterministic)
    /// fault sequence instead of replaying the pre-crash one. The synced
    /// watermark carries over UNCHANGED (kyzodb/kyzo#91): a process crash
    /// neither advances nor loses which prefix was ever fsynced, so a power
    /// cut simulated on the reopened store must still see only the TRUE
    /// pre-crash fsync frontier, not everything this crash's commit_seq
    /// happens to cover.
    pub(crate) fn sim_crash(&self) -> Result<SimStorage, SimRefuse> {
        let st = lock(&self.ctx.state)?;
        Ok(Self::reopen(
            st.versions.clone(),
            st.commit_seq,
            st.synced_seq,
            st.next_system_stamp,
            self.ctx.seed,
            self.ctx.epoch + 1,
            self.ctx.faults,
        ))
    }

    /// Simulated **power cut** + reopen: only fsynced commits survive —
    /// buffer-tier commits made after the last successful `sync` /
    /// `commit_durable` are lost. This is the tier distinction the contract
    /// documents, made testable. Bumps the crash epoch like [`sim_crash`].
    ///
    /// Caveat, stated honestly: this models the contract's guaranteed FLOOR.
    /// A real power cut may preserve *more* than the fsynced prefix (the OS
    /// flushes buffers on its own schedule), so "unsynced data is absent" is
    /// a property of the simulation, not a promise about real hardware. The
    /// contract promises only that the fsynced prefix survives; the sim pins
    /// exactly that promise by keeping nothing else.
    ///
    /// The write-count law's read side: the running total of `put` CALLS
    /// across every transaction that has committed successfully on this
    /// handle. Observation-only — see `SimState::total_puts`. (Like the rest
    /// of this module, reachable only from `#[cfg(test)]` code: `sim.rs`
    /// itself is declared `#[cfg(test)]` in `storage/mod.rs`.)
    pub(crate) fn put_call_count(&self) -> Result<u64, SimRefuse> {
        Ok(lock(&self.ctx.state)?.total_puts)
    }

    /// The `del`/`del_range` counterpart of [`put_call_count`].
    ///
    /// [`put_call_count`]: SimStorage::put_call_count
    pub(crate) fn del_call_count(&self) -> Result<u64, SimRefuse> {
        Ok(lock(&self.ctx.state)?.total_dels)
    }

    /// [`sim_crash`]: SimStorage::sim_crash
    pub(crate) fn sim_powercut(&self) -> Result<SimStorage, SimRefuse> {
        let st = lock(&self.ctx.state)?;
        let mut versions = VersionMap::new();
        for (k, vs) in &st.versions {
            let kept: Vec<_> = vs
                .iter()
                .filter(|(s, _)| *s <= st.synced_seq)
                .cloned()
                .collect();
            if !kept.is_empty() {
                versions.insert(k.clone(), kept);
            }
        }
        Ok(Self::reopen(
            versions,
            st.synced_seq, // commit_seq: nothing past the synced watermark
            // survives a power cut, so it degenerates to that watermark
            st.synced_seq, // synced_seq: everything kept above WAS synced,
            // by construction of the filter — the one case where the two
            // parameters legitimately share a value
            st.next_system_stamp,
            self.ctx.seed,
            self.ctx.epoch + 1,
            self.ctx.faults,
        ))
    }
}

impl Storage for SimStorage {
    type ReadTx = SimReadTx;
    type WriteTx = SimWriteTx;

    fn storage_kind(&self) -> &'static str {
        "sim"
    }

    fn clock_floor(&self) -> Result<ValidityTs> {
        let st = lock(&self.ctx.state)?;
        Ok(ValidityTs::of_micros(st.next_system_stamp))
    }

    fn raise_clock_floor(&self, floor: ValidityTs) -> Result<()> {
        let mut st = lock(&self.ctx.state)?;
        st.next_system_stamp = st.next_system_stamp.max(floor.raw());
        Ok(())
    }

    fn read_tx(&self) -> Result<SimReadTx> {
        let st = lock(&self.ctx.state)?;
        Ok(SimReadTx {
            snapshot: snapshot_at(&st, st.commit_seq),
            ctx: self.ctx.clone(),
        })
    }

    fn write_tx(&self) -> Result<SimWriteTx> {
        let mut st = lock(&self.ctx.state)?;
        st.next_system_stamp += 1;
        let stamp = ValidityTs::of_micros(st.next_system_stamp);
        Ok(SimWriteTx {
            inner: Some(SimWriteInner {
                snapshot: snapshot_at(&st, st.commit_seq),
                stamp,
                start_seq: st.commit_seq,
                writes: BTreeMap::new(),
                reads: Mutex::new(ReadSet::empty()),
                put_calls: 0,
                del_calls: 0,
                ctx: self.ctx.clone(),
            }),
        })
    }

    fn batch_put<'a>(
        &'a self,
        data: Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>,
    ) -> Result<()> {
        // The fresh-store precondition is refused, not just documented —
        // same contract as the real backend, including its ORACLE: live
        // keys only. A written-then-deleted store is genuinely empty,
        // exactly as fjall's range probe sees it (tombstones must not
        // make the test double stricter than reality).
        {
            let st = lock(&self.ctx.state)?;
            let has_live = st
                .versions
                .iter()
                .any(|(_, versions)| versions.last().is_some_and(|(_, v)| v.is_some()));
            if has_live {
                miette::bail!("bulk import target is not empty: import only into a fresh store");
            }
        }
        // Atomic chunks, like the real backend: an interrupted import leaves
        // a clean prefix of the input, never a torn chunk. Chunk size
        // deliberately differs from fjall's (sim 1024 vs fjall 32_768) so
        // chunk-boundary behavior is testable without 100k-row fixtures; the
        // clean-prefix LAW is the shared contract, the chunk size is not.
        const CHUNK: usize = 1024;
        let mut data = data.peekable();
        while data.peek().is_some() {
            let mut tx = self.write_tx()?;
            for pair in data.by_ref().take(CHUNK) {
                let (k, v) = pair?;
                tx.put(&k, &v)?;
            }
            tx.commit()?;
        }
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        let mut st = lock(&self.ctx.state)?;
        if self.ctx.roll_fault(
            &mut st,
            op_identity(TAG_SYNC, &[]),
            SALT_SYNC,
            self.ctx.faults.sync_fail_ppm,
        ) {
            return Err(miette!("sim: injected fsync failure"));
        }
        st.synced_seq = st.commit_seq;
        Ok(())
    }
}

// ---------- transactions ----------

/// A read transaction: the snapshot is materialized at open — simple and
/// obviously correct beats clever, and it makes snapshot stability trivially
/// true (a later commit cannot touch this map).
pub(crate) struct SimReadTx {
    snapshot: BTreeMap<Vec<u8>, Vec<u8>>,
    ctx: SimCtx,
}

struct ReadSet {
    keys: BTreeSet<Vec<u8>>,
    /// `(lower, upper)`; `None` upper = unbounded (total scans).
    ranges: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

impl ReadSet {
    fn empty() -> Self {
        ReadSet {
            keys: BTreeSet::new(),
            ranges: Vec::new(),
        }
    }
}

/// A write transaction: snapshot + overlay write set + tracked read set.
/// Open write-transaction payload. Presence of [`SimWriteTx::inner`] is Open;
/// `take` on commit/abort spends it (Fjall's `Option` pattern).
struct SimWriteInner {
    snapshot: BTreeMap<Vec<u8>, Vec<u8>>,
    stamp: ValidityTs,
    start_seq: u64,
    /// `None` = tombstone. Read-your-own-writes overlays this on `snapshot`.
    writes: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    reads: Mutex<ReadSet>,
    /// Write-count law bookkeeping: how many `put`/`del`(-via-`del_range`)
    /// CALLS this transaction has made, counted at the call site — so two
    /// calls landing on the same key (the second clobbering the first in
    /// `writes`) still count as two, never collapsed to `writes.len()`'s
    /// one entry. Folded into `SimState::total_puts`/`total_dels` only if
    /// this transaction's commit actually succeeds.
    put_calls: u64,
    del_calls: u64,
    ctx: SimCtx,
}

/// The read set lives behind a `Mutex` because the trait reads through
/// `&self` (and requires `Sync`) while conflict tracking must record.
pub(crate) struct SimWriteTx {
    /// `None` after commit/abort spends Open. Drop-bomb if still `Some`.
    inner: Option<SimWriteInner>,
}

impl SimWriteTx {
    fn open(&self) -> &SimWriteInner {
        match self.inner.as_ref() {
            Some(inner) => inner,
            None => std::panic::resume_unwind(Box::new(
                "INVARIANT(WriteTxOpen): SimWriteTx used after commit/abort".to_string(),
            )),
        }
    }

    fn open_mut(&mut self) -> &mut SimWriteInner {
        match self.inner.as_mut() {
            Some(inner) => inner,
            None => std::panic::resume_unwind(Box::new(
                "INVARIANT(WriteTxOpen): SimWriteTx used after commit/abort".to_string(),
            )),
        }
    }

    /// The transaction's visible view of `[lower, upper)`, LAZY: a
    /// sorted-merge of two cursors (`self.snapshot`'s range, `self.writes`'s
    /// range) rather than the eager "clone the whole snapshot range into a
    /// `BTreeMap`, then overlay writes" `visible` used to do. That eager
    /// build cost O(range size) per call regardless of how many items the
    /// caller actually consumes — the same catastrophic shape
    /// `SimReadTx::range_scan` had (see its doc comment): the skip walk's
    /// `SkipCursor` impl below reopens a fresh range at every seek step
    /// and drops all but the first item, so an O(n)-key skip scan paid O(n²)
    /// here too, on the ordinary
    /// "insert/update from a Datalog query" write-lock script path
    /// (`runtime/db.rs` routes every write-lock script's WHOLE query body
    /// through a `SimWriteTx`, so any recursive/skip-scanning read inside
    /// such a script hit this). On a key present in both cursors, the write
    /// entry shadows the snapshot entry (read-your-own-writes); a `None`
    /// write is a tombstone (erased, not yielded) whether or not a snapshot
    /// entry exists under it.
    fn visible_lazy<'a>(
        &'a self,
        lower: &[u8],
        upper: Option<&[u8]>,
    ) -> impl Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a {
        let open = self.open();
        let mut snap = map_range(&open.snapshot, lower, upper).peekable();
        let mut writes = map_range(&open.writes, lower, upper).peekable();
        std::iter::from_fn(move || {
            loop {
                let snap_key = snap.peek().map(|(k, _)| *k);
                let write_key = writes.peek().map(|(k, _)| *k);
                return match (snap_key, write_key) {
                    (None, None) => None,
                    // Only the snapshot has keys left: yield as is.
                    (Some(_), None) => {
                        // peek was Some ⇒ next must yield that entry.
                        let Some((k, v)) = snap.next() else {
                            return None;
                        };
                        Some((k.clone(), v.clone()))
                    }
                    // Only the write set has keys left.
                    (None, Some(_)) => {
                        let Some((k, w)) = writes.next() else {
                            return None;
                        };
                        match w {
                            Some(v) => Some((k.clone(), v.clone())),
                            None => continue,
                        }
                    }
                    (Some(sk), Some(wk)) => match sk.cmp(wk) {
                        // The snapshot's next key is strictly before the
                        // write set's: not shadowed, yield it as is.
                        Ordering::Less => {
                            let Some((k, v)) = snap.next() else {
                                return None;
                            };
                            Some((k.clone(), v.clone()))
                        }
                        // The write set's next key is strictly before the
                        // snapshot's (an insert with nothing shadowed, or a
                        // tombstone over a key the snapshot never had).
                        Ordering::Greater => {
                            let Some((k, w)) = writes.next() else {
                                return None;
                            };
                            match w {
                                Some(v) => Some((k.clone(), v.clone())),
                                None => continue,
                            }
                        }
                        // Same key in both: the write shadows the snapshot
                        // entry — advance BOTH cursors, and only yield if
                        // the write isn't a tombstone.
                        Ordering::Equal => {
                            snap.next();
                            let Some((k, w)) = writes.next() else {
                                return None;
                            };
                            match w {
                                Some(v) => Some((k.clone(), v.clone())),
                                None => continue,
                            }
                        }
                    },
                };
            }
        })
    }

    /// The transaction's visible view of `[lower, upper)`: snapshot with the
    /// write set overlaid, tombstones erased. Eager (used only by
    /// `del_range`, which must collect the doomed keys before mutating
    /// `self.writes` anyway — a single pass over the range, not the
    /// per-seek-step reopening the skip walk's `OpenSkipCursor` impl does).
    fn visible(&self, lower: &[u8], upper: Option<&[u8]>) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.visible_lazy(lower, upper).collect()
    }

    fn track_key(&self, key: &[u8]) -> Result<(), SimRefuse> {
        lock(&self.open().reads)?.keys.insert(key.to_vec());
        Ok(())
    }

    fn track_range(&self, lower: &[u8], upper: Option<&[u8]>) -> Result<(), SimRefuse> {
        lock(&self.open().reads)?
            .ranges
            .push((lower.to_vec(), upper.map(<[u8]>::to_vec)));
        Ok(())
    }

    fn commit_inner(mut self, durable: bool) -> std::result::Result<(), CommitFailure> {
        let mut inner = match self.inner.take() {
            Some(inner) => inner,
            None => std::panic::resume_unwind(Box::new(
                "INVARIANT(WriteTxOpen): SimWriteTx commit after spend".to_string(),
            )),
        };
        let start_seq = inner.start_seq;
        let writes = std::mem::take(&mut inner.writes);
        let reads = {
            let reads_mutex =
                std::mem::replace(&mut inner.reads, Mutex::new(ReadSet::empty()));
            let mut guard = commit_lock(&reads_mutex)?;
            std::mem::replace(&mut *guard, ReadSet::empty())
        };
        let put_calls = inner.put_calls;
        let del_calls = inner.del_calls;
        let ctx = inner.ctx.clone();
        // Fault identities, fixed before any dice roll. The commit identity
        // is the write-set KEYS (already sorted: `writes` is a BTreeMap) —
        // not values, so a retry that recomputes new values over the same
        // keys is still the same logical commit, on its next attempt.
        let commit_identity = {
            let keys: Vec<&[u8]> = writes.keys().map(Vec::as_slice).collect();
            op_identity(TAG_COMMIT, &keys)
        };
        let sync_identity = op_identity(TAG_SYNC, &[]);
        let mut st = commit_lock(&ctx.state)?;

        // An empty write set commits vacuously, matching the real backend
        // exactly: fjall returns Ok before its oracle ever runs, so a
        // read-only WriteTx commit never aborts, certifies nothing about
        // what it read, is not a fault-injection point, and advances no
        // sequence. (commit_durable still performs its fsync step: the real
        // backend's commit-then-persist syncs unconditionally.)
        if writes.is_empty() {
            if durable {
                if ctx.roll_fault(&mut st, sync_identity, SALT_SYNC, ctx.faults.sync_fail_ppm) {
                    return Err(CommitFailure::Io(CommitIo::SimInjectedFsync));
                }
                st.synced_seq = st.commit_seq;
            }
            return Ok(());
        }

        // Spurious-conflict injection: SSI permits false positives, so a
        // conflict with zero contention is a *legal* outcome the engine's
        // retry loops must absorb. The write set is discarded (self is
        // consumed and the state was never touched). Identity-keyed: a
        // retried commit of the same keys advances its attempt and draws a
        // fresh decision — it cannot be pinned to a permanent conflict.
        if ctx.roll_fault(
            &mut st,
            commit_identity,
            SALT_CONFLICT,
            ctx.faults.spurious_conflict_ppm,
        ) {
            return Err(CommitFailure::Conflict(ConflictError));
        }

        // Real SSI validation — READS AND WRITES, matching the real backend
        // (contract v2, see storage/mod.rs "Contract history"): anything this
        // transaction read (point or range) OR wrote (a put/del key) that was
        // committed past its snapshot aborts it. Write-write races are
        // first-committer-wins: the second committer gets the typed conflict
        // and reruns on a fresh snapshot. (On the real backend the same
        // predicate arises from put/del marking their key read in fjall's
        // conflict manager; here the write set is validated directly.)
        let key_conflict = reads.keys.iter().any(|k| modified_since(&st, start_seq, k));
        let range_conflict = reads
            .ranges
            .iter()
            .any(|(lo, hi)| range_modified_since(&st, start_seq, lo, hi.as_deref()));
        let write_conflict = writes.keys().any(|k| modified_since(&st, start_seq, k));
        if key_conflict || range_conflict || write_conflict {
            return Err(CommitFailure::Conflict(ConflictError));
        }

        // Apply atomically at the next sequence: the buffer durability tier.
        // The write-count law's oracle accumulates HERE, on the success
        // path only: an aborted commit's calls never reached real storage,
        // so they must never reach the running total either.
        st.total_puts += put_calls;
        st.total_dels += del_calls;
        let seq = st.commit_seq + 1;
        for (k, v) in writes {
            st.versions.entry(k).or_default().push((seq, v));
        }
        st.commit_seq = seq;

        if durable {
            // The fsync step. On injected failure the commit stays applied
            // but the fsync watermark does not advance — committed, not
            // power-cut durable, exactly like the real commit-then-persist.
            if ctx.roll_fault(&mut st, sync_identity, SALT_SYNC, ctx.faults.sync_fail_ppm) {
                return Err(CommitFailure::Io(CommitIo::SimInjectedFsyncAfterCommit));
            }
            st.synced_seq = st.commit_seq;
        }
        Ok(())
    }
}

/// The skip walk's cursor over a [`SimReadTx`]: `open_skip_cursor` does
/// nothing (there is no expensive one-time setup to save over an
/// in-memory `BTreeMap`) — `seek` alone carries forward exactly what
/// `range_scan` already does per call (yield to the scheduler, roll the
/// read-fault die), so the skip walk keeps participating in DST
/// scheduling and fault injection PER SEEK STEP; collapsing that into
/// `open_skip_cursor` would silently narrow the fault surface the sim
/// exists to stress down to one decision per walk instead of one per
/// version step.
pub(crate) struct SimReadSkipCursor<'a> {
    tx: &'a SimReadTx,
    upper: Vec<u8>,
}

impl SkipCursor for SimReadSkipCursor<'_> {
    fn seek(&mut self, target: &[u8]) -> Option<Result<(Vec<u8>, Vec<u8>)>> {
        if let Err(e) = self
            .tx
            .ctx
            .check_read_fault(op_identity(TAG_RANGE, &[target, &self.upper]))
        {
            return Some(Err(e));
        }
        map_range(&self.tx.snapshot, target, Some(&self.upper))
            .next()
            .map(|(k, v)| Ok((k.clone(), v.clone())))
    }
}

impl OpenSkipCursor for SimReadTx {
    type Cursor<'c> = SimReadSkipCursor<'c>;

    fn open_skip_cursor<'c>(&'c self, _lower: &[u8], upper: &[u8]) -> Self::Cursor<'c> {
        SimReadSkipCursor {
            tx: self,
            upper: upper.to_vec(),
        }
    }
}

/// The skip walk's cursor over a [`SimWriteTx`]: same shape as
/// [`SimReadSkipCursor`]'s, plus the conservative "one per seek step"
/// range tracking the contract documents for as-of scans inside write
/// transactions, over the lazy visible-range cursor (snapshot merged with
/// this transaction's own writes) — all still per `seek` call, not
/// collapsed into `open_skip_cursor`.
pub(crate) struct SimWriteSkipCursor<'a> {
    tx: &'a SimWriteTx,
    upper: Vec<u8>,
}

impl SkipCursor for SimWriteSkipCursor<'_> {
    fn seek(&mut self, target: &[u8]) -> Option<Result<(Vec<u8>, Vec<u8>)>> {
        let open = self.tx.open();
        if let Err(e) = self.tx.track_range(target, Some(&self.upper)) {
            return Some(Err(e.into()));
        }
        if let Err(e) = open
            .ctx
            .check_read_fault(op_identity(TAG_RANGE, &[target, &self.upper]))
        {
            return Some(Err(e));
        }
        self.tx
            .visible_lazy(target, Some(&self.upper))
            .next()
            .map(Ok)
    }
}

impl OpenSkipCursor for SimWriteTx {
    type Cursor<'c> = SimWriteSkipCursor<'c>;

    fn open_skip_cursor<'c>(&'c self, _lower: &[u8], upper: &[u8]) -> Self::Cursor<'c> {
        SimWriteSkipCursor {
            tx: self,
            upper: upper.to_vec(),
        }
    }
}

impl ReadTx for SimReadTx {
    fn get(&self, key: &[u8]) -> Result<Option<Slice>> {
        self.ctx.check_read_fault(op_identity(TAG_GET, &[key]))?;
        Ok(self.snapshot.get(key).map(Slice::from))
    }

    fn exists(&self, key: &[u8]) -> Result<bool> {
        self.ctx.check_read_fault(op_identity(TAG_EXISTS, &[key]))?;
        Ok(self.snapshot.contains_key(key))
    }

    fn range_scan<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<(Slice, Slice)>> + 'a> {
        if let Err(e) = self
            .ctx
            .check_read_fault(op_identity(TAG_RANGE, &[lower, upper]))
        {
            return Box::new(std::iter::once(Err(e)));
        }
        // Lazy: `BTreeMap::range` is already a cursor, so cloning per
        // element as it's pulled costs O(1) amortized per item. The
        // predecessor's `.collect()` into a `Vec` up front made every call
        // O(remaining range size) regardless of how many items the caller
        // actually consumes — catastrophic under the skip walk's `SkipCursor`
        // impl below, which opens a fresh range at every seek step and
        // consumes only the FIRST item: an O(n) skip scan over a range of
        // n keys paid O(n²) total instead of O(n).
        Box::new(
            map_range(&self.snapshot, lower, Some(upper))
                .map(|(k, v)| Ok((Slice::from(k), Slice::from(v)))),
        )
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
        if let Err(e) = self.ctx.check_read_fault(op_identity(TAG_TOTAL, &[])) {
            return Box::new(std::iter::once(Err(e)));
        }
        let items: Vec<_> = self
            .snapshot
            .iter()
            .map(|(k, v)| (Slice::from(k), Slice::from(v)))
            .collect();
        Box::new(items.into_iter().map(Ok))
    }
}

impl ReadTx for SimWriteTx {
    fn get(&self, key: &[u8]) -> Result<Option<Slice>> {
        let open = self.open();
        self.track_key(key)?;
        open.ctx.check_read_fault(op_identity(TAG_GET, &[key]))?;
        Ok(match open.writes.get(key) {
            Some(w) => w.as_deref().map(Slice::from),
            None => open.snapshot.get(key).map(Slice::from),
        })
    }

    fn exists(&self, key: &[u8]) -> Result<bool> {
        let open = self.open();
        self.track_key(key)?;
        open.ctx.check_read_fault(op_identity(TAG_EXISTS, &[key]))?;
        Ok(match open.writes.get(key) {
            Some(w) => w.is_some(),
            None => open.snapshot.contains_key(key),
        })
    }

    fn range_scan<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<(Slice, Slice)>> + 'a> {
        let open = self.open();
        // Track the whole requested range even if iteration stops early:
        // conservative (more false conflicts) and therefore legal under SSI.
        if let Err(e) = self.track_range(lower, Some(upper)) {
            return Box::new(std::iter::once(Err(e.into())));
        }
        if let Err(e) = open
            .ctx
            .check_read_fault(op_identity(TAG_RANGE, &[lower, upper]))
        {
            return Box::new(std::iter::once(Err(e)));
        }
        Box::new(
            self.visible_lazy(lower, Some(upper))
                .map(|(k, v)| Ok((Slice::from(k), Slice::from(v)))),
        )
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
        let open = self.open();
        if let Err(e) = self.track_range(&[], None) {
            return Box::new(std::iter::once(Err(e.into())));
        }
        if let Err(e) = open.ctx.check_read_fault(op_identity(TAG_TOTAL, &[])) {
            return Box::new(std::iter::once(Err(e)));
        }
        Box::new(
            self.visible_lazy(&[], None)
                .map(|(k, v)| Ok((Slice::from(k), Slice::from(v)))),
        )
    }
}

impl WriteTx for SimWriteTx {
    fn system_stamp(&self) -> ValidityTs {
        self.open().stamp
    }

    fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        let open = self.open_mut();
        open.writes.insert(key.to_vec(), Some(val.to_vec()));
        open.put_calls += 1;
        Ok(())
    }

    fn del(&mut self, key: &[u8]) -> Result<()> {
        let open = self.open_mut();
        open.writes.insert(key.to_vec(), None);
        open.del_calls += 1;
        Ok(())
    }

    fn del_range(&mut self, lower: &[u8], upper: &[u8]) -> Result<()> {
        // Deleting "everything visible" reads the range: tracked, so a
        // concurrent insert into it is a conflict (matching the real
        // backend, whose del_range scans through the transaction).
        self.track_range(lower, Some(upper))?;
        let doomed: Vec<Vec<u8>> = self
            .visible(lower, Some(upper))
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        // Each doomed key is its own del CALL for the write-count law, same
        // as a caller looping `del` over the same keys one at a time.
        let open = self.open_mut();
        open.del_calls += usize_as_u64(doomed.len());
        for k in doomed {
            open.writes.insert(k, None);
        }
        Ok(())
    }

    fn commit(self) -> std::result::Result<(), CommitFailure> {
        self.commit_inner(false)
    }

    fn commit_durable(self) -> std::result::Result<(), CommitFailure> {
        self.commit_inner(true)
    }

    fn abort(mut self) -> Aborted {
        // Idempotent: a second abort after spend is still Aborted.
        let spent_inner = self.inner.take();
        drop(spent_inner);
        Aborted
    }
}

impl Drop for SimWriteTx {
    fn drop(&mut self) {
        if self.inner.is_some() && !std::thread::panicking() {
            std::panic::resume_unwind(Box::new(
                "INVARIANT(WriteTxDrop): Open WriteTx dropped without commit() or abort(self)"
                    .to_string(),
            ));
        }
    }
}

#[cfg(test)]
mod battery {
    /// DST instrument proof battery (re-homed from storage/tests.rs).
    use std::collections::BTreeMap;
    use std::num::NonZeroUsize;

    use fjall::Slice;
    use kyzo_model::TupleT;
    use kyzo_model::value::{
        AsOf, DataValue, RelationId, StorageKey, Tuple, ValiditySlot, ValidityTs,
    };

    use crate::store::retry::{get_attempt, put_attempt, retry_on_conflict, write_tx_attempt};
    use crate::store::sim::{FaultConfig, SimStorage};
    use crate::store::time::ClaimPolarity;
    use crate::store::{ReadTx, Storage, WriteTx};

    fn must<T, E: std::fmt::Debug>(r: Result<T, E>, what: &str) -> T {
        match r {
            Ok(v) => v,
            Err(e) => {
                assert!(false, "{what}: {e:?}");
                loop {
                    std::hint::spin_loop();
                }
            }
        }
    }

    fn retry_cap() -> NonZeroUsize {
        match NonZeroUsize::new(10_000) {
            Some(n) => n,
            None => {
                assert!(false, "INVARIANT(NonZero): 10_000 is non-zero");
                loop {
                    std::hint::spin_loop();
                }
            }
        }
    }

    fn must_put(tx: &mut impl WriteTx, k: &[u8], v: &[u8]) {
        must(tx.put(k, v), "put");
    }

    /// Naive as-of oracle — same reference the fjall pin uses.
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

    fn relation(id: u64) -> RelationId {
        match RelationId::new(id) {
            Some(r) => r,
            None => {
                assert!(false, "INVARIANT(RelationIdCap): id {id} below cap");
                loop {
                    std::hint::spin_loop();
                }
            }
        }
    }

    #[test]
    fn write_write_race_aborts_second_committer() {
        let db = SimStorage::new(1);
        let mut tx1 = must(db.write_tx(), "write_tx");
        let mut tx2 = must(db.write_tx(), "write_tx");
        must_put(&mut tx1, b"ww", b"1");
        must_put(&mut tx2, b"ww", b"2");
        match tx1.commit() {
            Ok(()) => {}
            Err(e) => assert!(false, "the FIRST committer must never abort: {e:?}"),
        }
        let err = match tx2.commit() {
            Err(e) => e,
            Ok(()) => {
                assert!(
                    false,
                    "a write-write race must abort the second committer"
                );
                loop {
                    std::hint::spin_loop();
                }
            }
        };
        assert!(err.is_conflict(), "typed conflict, got {err:?}");
        assert_eq!(
            must(must(db.read_tx(), "read_tx").get(b"ww"), "get"),
            Some(Slice::from(b"1"))
        );
    }

    #[test]
    fn sim_time_travel_matches_naive_oracle() {
        let history: &[(&str, i64, bool)] = &[
            ("a", 1, true),
            ("a", 3, true),
            ("a", 5, false),
            ("a", 7, true),
            ("b", 2, true),
            ("b", 6, false),
        ];
        let rel = relation(7);
        let db = SimStorage::new(2);
        let mut tx = must(db.write_tx(), "write_tx");
        for (name, ts, assert_flag) in history {
            let (k, v) = vld_row(rel, name, *ts, *assert_flag);
            must_put(&mut tx, &k, &v);
        }
        match tx.commit() {
            Ok(()) => {}
            Err(e) => assert!(false, "commit: {e:?}"),
        }
        let lower = rel.raw_encode().to_vec();
        let upper = match rel.next() {
            Some(r) => r.raw_encode().to_vec(),
            None => {
                assert!(false, "INVARIANT(RelationIdCap): next below cap");
                loop {
                    std::hint::spin_loop();
                }
            }
        };
        let tx = must(db.read_tx(), "read_tx");
        for at in 0..=8i64 {
            let got: Vec<(String, i64)> = tx
                .range_skip_scan_tuple(&lower, &upper, AsOf::current(ValidityTs::of_micros(at)))
                .map(|r| {
                    let t = must(r, "range row");
                    let name = match &t.as_slice()[0] {
                        DataValue::Str(s) => s.to_string(),
                        other => {
                            assert!(false, "unexpected {other:?}");
                            loop {
                                std::hint::spin_loop();
                            }
                        }
                    };
                    let ts = match &t.as_slice()[1] {
                        DataValue::Validity(v) => v.ts_micros(),
                        other => {
                            assert!(false, "unexpected {other:?}");
                            loop {
                                std::hint::spin_loop();
                            }
                        }
                    };
                    (name, ts)
                })
                .collect();
            assert_eq!(got, as_of_oracle(history, at), "as-of {at}");
        }
    }

    #[test]
    fn sim_fault_plan_identical_at_any_thread_count() {
        const KEYS: usize = 32;
        const ATTEMPTS: usize = 4;
        type Matrix = Vec<Vec<bool>>;

        fn observe(seed: u64, threads: usize) -> (Matrix, Matrix) {
            let db = SimStorage::with_faults(
                seed,
                FaultConfig {
                    read_fail_ppm: 400_000,
                    spurious_conflict_ppm: 400_000,
                    sync_fail_ppm: 0,
                },
            );
            match retry_on_conflict(retry_cap(), || {
                let mut tx = write_tx_attempt(&db)?;
                for i in 0..KEYS {
                    put_attempt(&mut tx, format!("r{i:02}").as_bytes(), b"v")?;
                }
                {
                    tx.commit()?;
                    Ok(())
                }
            }) {
                Ok(()) => {}
                Err(e) => assert!(false, "seed load: {e:?}"),
            }

            let mut reads: Matrix = vec![vec![]; KEYS];
            let mut commits: Matrix = vec![vec![]; KEYS];
            std::thread::scope(|s| {
                let handles: Vec<_> = (0..threads)
                    .map(|t| {
                        let db = db.clone();
                        s.spawn(move || {
                            let mut out = Vec::new();
                            let tx = must(db.read_tx(), "read_tx");
                            for i in (t..KEYS).step_by(threads) {
                                let key = format!("r{i:02}").into_bytes();
                                let r: Vec<bool> =
                                    (0..ATTEMPTS).map(|_| tx.get(&key).is_err()).collect();
                                let c: Vec<bool> = (0..ATTEMPTS)
                                    .map(|_| {
                                        let mut w = must(db.write_tx(), "write_tx");
                                        must_put(&mut w, format!("c{i:02}").as_bytes(), b"v");
                                        w.commit().is_err()
                                    })
                                    .collect();
                                out.push((i, r, c));
                            }
                            out
                        })
                    })
                    .collect();
                for h in handles {
                    let items = match h.join() {
                        Ok(items) => items,
                        Err(_) => {
                            assert!(
                                false,
                                "INVARIANT(ThreadJoin): participant panicked"
                            );
                            loop {
                                std::hint::spin_loop();
                            }
                        }
                    };
                    for (i, r, c) in items {
                        reads[i] = r;
                        commits[i] = c;
                    }
                }
            });
            (reads, commits)
        }

        let base = observe(42, 1);
        let fired = |m: &Matrix| m.iter().flatten().any(|b| *b);
        let missed = |m: &Matrix| m.iter().flatten().any(|b| !*b);
        assert!(
            fired(&base.0) && missed(&base.0) && fired(&base.1) && missed(&base.1),
            "both streams must contain hits and misses"
        );
        for threads in [2usize, 4, 8] {
            assert_eq!(base, observe(42, threads), "identical at {threads} threads");
        }
        assert_ne!(base, observe(43, 1), "different seeds differ");
    }

    #[test]
    fn sim_retry_liveness_escapes_injected_faults() {
        let db = SimStorage::with_faults(
            7,
            FaultConfig {
                read_fail_ppm: 900_000,
                spurious_conflict_ppm: 900_000,
                sync_fail_ppm: 0,
            },
        );
        match retry_on_conflict(retry_cap(), || {
            let mut tx = write_tx_attempt(&db)?;
            put_attempt(&mut tx, b"k", b"v")?;
            tx.commit()?;
            Ok(())
        }) {
            Ok(()) => {}
            Err(e) => assert!(
                false,
                "90% storms must not pin a bounded retry forever: {e:?}"
            ),
        }
        match get_attempt(&must(db.read_tx(), "read_tx"), b"k") {
            Ok(_) | Err(_) => {}
        }
    }
}
