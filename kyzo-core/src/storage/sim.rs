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
//! It is declared `#[cfg(test)]` in `storage/mod.rs`: compiled only into the
//! test harness, never into the published library — yet visible to every
//! in-crate `#[cfg(test)]` module (storage tests today, query/runtime tests
//! tomorrow) as `crate::storage::sim`. The alternative — a `test-support`
//! cargo feature — would leak a fake backend into the shipped API surface
//! and invite depending on it; rejected.
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

use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::sync::{Arc, Condvar, Mutex};

use miette::{Result, miette};

use crate::data::tuple::Tuple;
use crate::data::value::{AsOf, ValidityTs};
use crate::storage::skip_walk::{SkipSeek, SkipWalk};
use crate::storage::{ConflictError, ReadTx, Storage, WriteTx};

const POISONED: &str = "sim lock poisoned: a holder panicked";

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
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `[0, n)`. Modulo bias is irrelevant at test scales.
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        debug_assert!(n > 0);
        self.next_u64() % n
    }
}

// ---------- the fault plan: a pure function of (seed, op-identity, attempt) ----------

/// Injection rates in parts-per-million. `1_000_000` = always fire;
/// `Default` = no faults, pure semantics.
#[derive(Debug, Clone, Copy, Default)]
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

/// The identity of a faultable operation: FNV-1a 64 over the op-kind tag and
/// the operation's semantic content, length-delimited so distinct part lists
/// never collide by concatenation. Identity captures WHAT an operation is —
/// never when it runs, what ran before it, or which thread carries it.
fn op_identity(tag: u64, parts: &[&[u8]]) -> u64 {
    const OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01B3;
    fn eat(h: &mut u64, bytes: &[u8]) {
        for &b in bytes {
            *h = (*h ^ u64::from(b)).wrapping_mul(PRIME);
        }
    }
    let mut h = OFFSET;
    eat(&mut h, &tag.to_be_bytes());
    for part in parts {
        eat(&mut h, &(part.len() as u64).to_be_bytes());
        eat(&mut h, part);
    }
    h
}

/// The fault decision: splitmix-style finalizer over
/// (seed, identity, attempt, salt), where `seed` is the crash-epoch-salted
/// seed from [`SimCtx::fault_seed`] and `attempt` is the per-identity
/// occurrence number. Deterministic, stateless, replayable — and the same
/// (identity, attempt) draws the same decision at any thread count.
fn fault_hit(seed: u64, identity: u64, attempt: u64, salt: u64, ppm: u32) -> bool {
    if ppm == 0 {
        return false;
    }
    let mut z = seed
        ^ identity.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ attempt.wrapping_mul(0xA24B_AED4_963E_E407)
        ^ salt.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
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
    /// Installed by [`run_interleaved`] for the duration of a drive.
    sched: Option<Arc<Scheduler>>,
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

// ---------- the deterministic scheduler ----------

thread_local! {
    /// Set by [`run_interleaved`] in each participant thread; ops on sim
    /// transactions yield at this identity. Non-participant threads (test
    /// setup, assertions) pass through untouched.
    static PARTICIPANT: Cell<Option<usize>> = const { Cell::new(None) };
}

struct SchedState {
    /// Registered participants not yet finished.
    live: usize,
    /// Participants parked at a yield point, waiting for a turn.
    parked: BTreeSet<usize>,
    /// The one participant currently allowed to run.
    current: Option<usize>,
    rng: SimRng,
}

/// Token-barrier scheduler: a turn is granted only when **every** live
/// participant is parked, so from each participant's first yield point
/// onward exactly one thread runs at a time and the parked set at each
/// decision is deterministic — which makes the seeded pick the *only*
/// source of interleaving order. (Before its first sim op a participant
/// has never parked and runs unserialized; bodies must reach shared state
/// only through sim ops.) Real threads, replayable schedules.
pub(crate) struct Scheduler {
    st: Mutex<SchedState>,
    cv: Condvar,
}

impl Scheduler {
    fn new(seed: u64, participants: usize) -> Self {
        Scheduler {
            st: Mutex::new(SchedState {
                live: participants,
                parked: BTreeSet::new(),
                current: None,
                rng: SimRng::new(seed),
            }),
            cv: Condvar::new(),
        }
    }

    /// Park at a yield point; return when granted the next turn.
    fn yield_here(&self, id: usize) {
        let mut st = self.st.lock().expect(POISONED);
        if st.current == Some(id) {
            st.current = None; // release the turn we were running on
        }
        st.parked.insert(id);
        Self::dispatch(&mut st);
        self.cv.notify_all();
        while st.current != Some(id) {
            st = self.cv.wait(st).expect(POISONED);
        }
        st.parked.remove(&id);
    }

    /// A participant finished (or died — called from a drop guard, so a
    /// panicking body cannot hang the other participants).
    fn done(&self, id: usize) {
        let mut st = self.st.lock().expect(POISONED);
        st.live -= 1;
        st.parked.remove(&id);
        if st.current == Some(id) {
            st.current = None;
        }
        Self::dispatch(&mut st);
        self.cv.notify_all();
    }

    fn dispatch(st: &mut SchedState) {
        if st.current.is_none() && st.live > 0 && st.parked.len() == st.live {
            let k = st.rng.below(st.parked.len() as u64) as usize;
            let id = *st.parked.iter().nth(k).expect("parked set is non-empty");
            st.current = Some(id);
        }
    }
}

struct DoneGuard<'a> {
    sched: &'a Scheduler,
    id: usize,
}

impl Drop for DoneGuard<'_> {
    fn drop(&mut self) {
        self.sched.done(self.id);
    }
}

/// One transaction body for the interleaving driver.
pub(crate) type TxBody<'a> = Box<dyn FnOnce() + Send + 'a>;

/// The adversarial schedule driver: run `bodies` on real threads, with every
/// sim-storage operation as a yield point, interleaved deterministically
/// from `seed`. A body that panics propagates its panic out of this call
/// (via `std::thread::scope`) after the other bodies are released.
pub(crate) fn run_interleaved(db: &SimStorage, seed: u64, bodies: Vec<TxBody<'_>>) {
    let sched = Arc::new(Scheduler::new(seed, bodies.len()));
    db.ctx.state.lock().expect(POISONED).sched = Some(Arc::clone(&sched));
    std::thread::scope(|s| {
        for (id, body) in bodies.into_iter().enumerate() {
            let sched = Arc::clone(&sched);
            s.spawn(move || {
                PARTICIPANT.with(|c| c.set(Some(id)));
                let _done = DoneGuard { sched: &sched, id };
                body();
            });
        }
    });
    db.ctx.state.lock().expect(POISONED).sched = None;
}

/// Campaign harness: run `f` once per seed; on failure, re-panic with the
/// seed stamped on the report, so the exact schedule and fault plan can be
/// replayed by rerunning with that one seed.
pub(crate) fn for_each_seed(seeds: std::ops::Range<u64>, f: impl Fn(u64)) {
    for seed in seeds {
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(seed))) {
            let msg = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("<non-string panic payload>");
            panic!(
                "[sim] FAILING SEED = {seed} — rerun this test with the loop pinned to \
                 seed {seed} to replay the schedule and fault plan exactly.\n{msg}"
            );
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
        self.seed ^ self.epoch.wrapping_mul(0xE703_37A9_D9C8_2D95)
    }

    /// Yield point: no-op unless this thread is a registered participant of
    /// an installed scheduler.
    fn yield_turn(&self) {
        let Some(id) = PARTICIPANT.with(Cell::get) else {
            return;
        };
        let sched = self.state.lock().expect(POISONED).sched.clone();
        if let Some(sched) = sched {
            sched.yield_here(id);
        }
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
        let mut st = self.state.lock().expect(POISONED);
        if self.roll_fault(&mut st, identity, SALT_READ, self.faults.read_fail_ppm) {
            return Err(miette!("sim: injected transient read fault"));
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
        Self::with_faults(seed, FaultConfig::default())
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
                    sched: None,
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
    pub(crate) fn sim_crash(&self) -> SimStorage {
        let st = self.ctx.state.lock().expect(POISONED);
        Self::reopen(
            st.versions.clone(),
            st.commit_seq,
            st.synced_seq,
            st.next_system_stamp,
            self.ctx.seed,
            self.ctx.epoch + 1,
            self.ctx.faults,
        )
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
    pub(crate) fn put_call_count(&self) -> u64 {
        self.ctx.state.lock().expect(POISONED).total_puts
    }

    /// The `del`/`del_range` counterpart of [`put_call_count`].
    ///
    /// [`put_call_count`]: SimStorage::put_call_count
    pub(crate) fn del_call_count(&self) -> u64 {
        self.ctx.state.lock().expect(POISONED).total_dels
    }

    /// [`sim_crash`]: SimStorage::sim_crash
    pub(crate) fn sim_powercut(&self) -> SimStorage {
        let st = self.ctx.state.lock().expect(POISONED);
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
        Self::reopen(
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
        )
    }
}

impl Storage for SimStorage {
    type ReadTx = SimReadTx;
    type WriteTx = SimWriteTx;

    fn storage_kind(&self) -> &'static str {
        "sim"
    }

    fn clock_floor(&self) -> Result<ValidityTs> {
        let st = self.ctx.state.lock().expect(POISONED);
        Ok(ValidityTs(std::cmp::Reverse(st.next_system_stamp)))
    }

    fn raise_clock_floor(&self, floor: ValidityTs) -> Result<()> {
        let mut st = self.ctx.state.lock().expect(POISONED);
        st.next_system_stamp = st.next_system_stamp.max(floor.0.0);
        Ok(())
    }

    fn read_tx(&self) -> Result<SimReadTx> {
        self.ctx.yield_turn();
        let st = self.ctx.state.lock().expect(POISONED);
        Ok(SimReadTx {
            snapshot: snapshot_at(&st, st.commit_seq),
            ctx: self.ctx.clone(),
        })
    }

    fn write_tx(&self) -> Result<SimWriteTx> {
        self.ctx.yield_turn();
        let mut st = self.ctx.state.lock().expect(POISONED);
        st.next_system_stamp += 1;
        let stamp = ValidityTs(std::cmp::Reverse(st.next_system_stamp));
        Ok(SimWriteTx {
            snapshot: snapshot_at(&st, st.commit_seq),
            stamp,
            start_seq: st.commit_seq,
            writes: BTreeMap::new(),
            reads: Mutex::new(ReadSet::default()),
            put_calls: 0,
            del_calls: 0,
            ctx: self.ctx.clone(),
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
            let st = self.ctx.state.lock().expect(POISONED);
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
        self.ctx.yield_turn();
        let mut st = self.ctx.state.lock().expect(POISONED);
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

#[derive(Default)]
struct ReadSet {
    keys: BTreeSet<Vec<u8>>,
    /// `(lower, upper)`; `None` upper = unbounded (total scans).
    ranges: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

/// A write transaction: snapshot + overlay write set + tracked read set.
/// The read set lives behind a `Mutex` because the trait reads through
/// `&self` (and requires `Sync`) while conflict tracking must record.
pub(crate) struct SimWriteTx {
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

impl SimWriteTx {
    /// The transaction's visible view of `[lower, upper)`, LAZY: a
    /// sorted-merge of two cursors (`self.snapshot`'s range, `self.writes`'s
    /// range) rather than the eager "clone the whole snapshot range into a
    /// `BTreeMap`, then overlay writes" `visible` used to do. That eager
    /// build cost O(range size) per call regardless of how many items the
    /// caller actually consumes — the same catastrophic shape
    /// `SimReadTx::range_scan` had (see its doc comment): the skip walk's
    /// `SkipSeek` impl below reopens a fresh range at every seek step and
    /// drops all but the first item, so an O(n)-key skip scan paid O(n²)
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
        let mut snap = map_range(&self.snapshot, lower, upper).peekable();
        let mut writes = map_range(&self.writes, lower, upper).peekable();
        std::iter::from_fn(move || {
            loop {
                let snap_key = snap.peek().map(|(k, _)| *k);
                let write_key = writes.peek().map(|(k, _)| *k);
                return match (snap_key, write_key) {
                    (None, None) => None,
                    // Only the snapshot has keys left: yield as is.
                    (Some(_), None) => {
                        let (k, v) = snap.next().expect("peeked Some");
                        Some((k.clone(), v.clone()))
                    }
                    // Only the write set has keys left.
                    (None, Some(_)) => {
                        let (k, w) = writes.next().expect("peeked Some");
                        match w {
                            Some(v) => Some((k.clone(), v.clone())),
                            None => continue,
                        }
                    }
                    (Some(sk), Some(wk)) => match sk.cmp(wk) {
                        // The snapshot's next key is strictly before the
                        // write set's: not shadowed, yield it as is.
                        Ordering::Less => {
                            let (k, v) = snap.next().expect("peeked Some");
                            Some((k.clone(), v.clone()))
                        }
                        // The write set's next key is strictly before the
                        // snapshot's (an insert with nothing shadowed, or a
                        // tombstone over a key the snapshot never had).
                        Ordering::Greater => {
                            let (k, w) = writes.next().expect("peeked Some");
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
                            let (k, w) = writes.next().expect("peeked Some");
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
    /// per-seek-step reopening the skip walk's `SkipSeek` impl does).
    fn visible(&self, lower: &[u8], upper: Option<&[u8]>) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.visible_lazy(lower, upper).collect()
    }

    fn track_key(&self, key: &[u8]) {
        self.reads.lock().expect(POISONED).keys.insert(key.to_vec());
    }

    fn track_range(&self, lower: &[u8], upper: Option<&[u8]>) {
        self.reads
            .lock()
            .expect(POISONED)
            .ranges
            .push((lower.to_vec(), upper.map(<[u8]>::to_vec)));
    }

    fn commit_inner(self, durable: bool) -> Result<()> {
        self.ctx.yield_turn();
        let SimWriteTx {
            snapshot: _,
            stamp: _,
            start_seq,
            writes,
            reads,
            put_calls,
            del_calls,
            ctx,
        } = self;
        let reads = reads.into_inner().expect(POISONED);
        // Fault identities, fixed before any dice roll. The commit identity
        // is the write-set KEYS (already sorted: `writes` is a BTreeMap) —
        // not values, so a retry that recomputes new values over the same
        // keys is still the same logical commit, on its next attempt.
        let commit_identity = {
            let keys: Vec<&[u8]> = writes.keys().map(Vec::as_slice).collect();
            op_identity(TAG_COMMIT, &keys)
        };
        let sync_identity = op_identity(TAG_SYNC, &[]);
        let mut st = ctx.state.lock().expect(POISONED);

        // An empty write set commits vacuously, matching the real backend
        // exactly: fjall returns Ok before its oracle ever runs, so a
        // read-only WriteTx commit never aborts, certifies nothing about
        // what it read, is not a fault-injection point, and advances no
        // sequence. (commit_durable still performs its fsync step: the real
        // backend's commit-then-persist syncs unconditionally.)
        if writes.is_empty() {
            if durable {
                if ctx.roll_fault(&mut st, sync_identity, SALT_SYNC, ctx.faults.sync_fail_ppm) {
                    return Err(miette!("sim: injected fsync failure"));
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
            return Err(ConflictError.into());
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
            return Err(ConflictError.into());
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
                return Err(miette!(
                    "sim: injected fsync failure (commit applied, not power-cut durable)"
                ));
            }
            st.synced_seq = st.commit_seq;
        }
        Ok(())
    }
}

/// [`SkipSeek`] for [`SimReadTx`]: the one seam the driver needs, inlining
/// exactly what `range_scan` already does per call (yield to the scheduler,
/// roll the read-fault die) so the skip walk keeps participating in DST
/// scheduling and fault injection PER SEEK STEP — losing that would silently
/// narrow the fault surface the sim exists to stress — while no longer
/// boxing through `range_scan` itself (issue #78's phase-2 map).
impl SkipSeek for SimReadTx {
    fn seek_first(&self, lower: &[u8], upper: &[u8]) -> Option<Result<(Vec<u8>, Vec<u8>)>> {
        self.ctx.yield_turn();
        if let Err(e) = self
            .ctx
            .check_read_fault(op_identity(TAG_RANGE, &[lower, upper]))
        {
            return Some(Err(e));
        }
        map_range(&self.snapshot, lower, Some(upper))
            .next()
            .map(|(k, v)| Ok((k.clone(), v.clone())))
    }
}

/// [`SkipSeek`] for [`SimWriteTx`]: same shape as [`SimReadTx`]'s plus the
/// conservative "one per seek step" range tracking the contract documents
/// for as-of scans inside write transactions, over the lazy visible-range
/// cursor (snapshot merged with this transaction's own writes).
impl SkipSeek for SimWriteTx {
    fn seek_first(&self, lower: &[u8], upper: &[u8]) -> Option<Result<(Vec<u8>, Vec<u8>)>> {
        self.ctx.yield_turn();
        self.track_range(lower, Some(upper));
        if let Err(e) = self
            .ctx
            .check_read_fault(op_identity(TAG_RANGE, &[lower, upper]))
        {
            return Some(Err(e));
        }
        self.visible_lazy(lower, Some(upper)).next().map(Ok)
    }
}

impl ReadTx for SimReadTx {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.ctx.yield_turn();
        self.ctx.check_read_fault(op_identity(TAG_GET, &[key]))?;
        Ok(self.snapshot.get(key).cloned())
    }

    fn exists(&self, key: &[u8]) -> Result<bool> {
        self.ctx.yield_turn();
        self.ctx.check_read_fault(op_identity(TAG_EXISTS, &[key]))?;
        Ok(self.snapshot.contains_key(key))
    }

    fn range_scan<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a> {
        self.ctx.yield_turn();
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
        // actually consumes — catastrophic under the skip walk's `SkipSeek`
        // impl below, which opens a fresh range at every seek step and
        // consumes only the FIRST item: an O(n) skip scan over a range of
        // n keys paid O(n²) total instead of O(n).
        Box::new(
            map_range(&self.snapshot, lower, Some(upper)).map(|(k, v)| Ok((k.clone(), v.clone()))),
        )
    }

    fn range_skip_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
        as_of: AsOf,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        Box::new(SkipWalk::new(self, lower, upper, as_of))
    }

    fn total_scan<'a>(&'a self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a> {
        self.ctx.yield_turn();
        if let Err(e) = self.ctx.check_read_fault(op_identity(TAG_TOTAL, &[])) {
            return Box::new(std::iter::once(Err(e)));
        }
        let items: Vec<_> = self
            .snapshot
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Box::new(items.into_iter().map(Ok))
    }
}

impl ReadTx for SimWriteTx {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.ctx.yield_turn();
        self.track_key(key);
        self.ctx.check_read_fault(op_identity(TAG_GET, &[key]))?;
        Ok(match self.writes.get(key) {
            Some(w) => w.clone(),
            None => self.snapshot.get(key).cloned(),
        })
    }

    fn exists(&self, key: &[u8]) -> Result<bool> {
        self.ctx.yield_turn();
        self.track_key(key);
        self.ctx.check_read_fault(op_identity(TAG_EXISTS, &[key]))?;
        Ok(match self.writes.get(key) {
            Some(w) => w.is_some(),
            None => self.snapshot.contains_key(key),
        })
    }

    fn range_scan<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a> {
        self.ctx.yield_turn();
        // Track the whole requested range even if iteration stops early:
        // conservative (more false conflicts) and therefore legal under SSI.
        self.track_range(lower, Some(upper));
        if let Err(e) = self
            .ctx
            .check_read_fault(op_identity(TAG_RANGE, &[lower, upper]))
        {
            return Box::new(std::iter::once(Err(e)));
        }
        Box::new(self.visible_lazy(lower, Some(upper)).map(Ok))
    }

    fn range_skip_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
        as_of: AsOf,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        Box::new(SkipWalk::new(self, lower, upper, as_of))
    }

    fn total_scan<'a>(&'a self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a> {
        self.ctx.yield_turn();
        self.track_range(&[], None);
        if let Err(e) = self.ctx.check_read_fault(op_identity(TAG_TOTAL, &[])) {
            return Box::new(std::iter::once(Err(e)));
        }
        Box::new(self.visible_lazy(&[], None).map(Ok))
    }
}

impl WriteTx for SimWriteTx {
    fn system_stamp(&self) -> ValidityTs {
        self.stamp
    }

    fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        self.ctx.yield_turn();
        self.writes.insert(key.to_vec(), Some(val.to_vec()));
        self.put_calls += 1;
        Ok(())
    }

    fn del(&mut self, key: &[u8]) -> Result<()> {
        self.ctx.yield_turn();
        self.writes.insert(key.to_vec(), None);
        self.del_calls += 1;
        Ok(())
    }

    fn del_range(&mut self, lower: &[u8], upper: &[u8]) -> Result<()> {
        self.ctx.yield_turn();
        // Deleting "everything visible" reads the range: tracked, so a
        // concurrent insert into it is a conflict (matching the real
        // backend, whose del_range scans through the transaction).
        self.track_range(lower, Some(upper));
        let doomed: Vec<Vec<u8>> = self
            .visible(lower, Some(upper))
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        // Each doomed key is its own del CALL for the write-count law, same
        // as a caller looping `del` over the same keys one at a time.
        self.del_calls += doomed.len() as u64;
        for k in doomed {
            self.writes.insert(k, None);
        }
        Ok(())
    }

    fn commit(self) -> Result<()> {
        self.commit_inner(false)
    }

    fn commit_durable(self) -> Result<()> {
        self.commit_inner(true)
    }
}
