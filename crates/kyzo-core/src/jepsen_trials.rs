/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Trial (issue #34): a single-node, elle/Adya-style serializability checker
//! over the real `fjall`-backed [`Storage`] — the SSI core, driven directly
//! (`write_tx`/`get`/`put`/`commit`), never through [`crate::Db::run_script`]
//! (whose mutation path retries a whole script on conflict, so it never
//! surfaces a raw abort to the caller — unsuitable for a harness that needs
//! the true commit/abort truth of every attempt).
//!
//! ## Scope, stated plainly
//!
//! This is the SINGLE-NODE half of the Jepsen trial the issue asks for:
//! concurrent transaction histories against one in-process engine, checked
//! for serializability anomalies. Two things the full issue calls for are
//! **explicitly out of scope here, each for a named reason**:
//!
//! - **The distributed rig** (partitions, replica divergence) — KyzoDB has no
//!   replication story yet; that trial returns with the replication work
//!   post-1.0, per the issue's own ruling.
//! - **Fault injection (process-crash, power-cut) through the public
//!   `kyzo-bin` HTTP surface** — that leg shares fault infrastructure with
//!   issue #31's crash matrix and is sequenced after #31's injector lands;
//!   wiring this checker to drive `kyzo-bin` over HTTP instead of the
//!   in-process `Storage` trait is separate follow-on work, not a gap in
//!   this module's own claim.
//!
//! What IS built: a seeded, reproducible workload generator; real concurrent
//! execution against `FjallStorage` (the one shipped backend); a recorded
//! history of every COMMITTED transaction's reads and writes; and an
//! independent checker — importing no storage-tier internals, only the
//! public [`Storage`]/[`ReadTx`]/[`WriteTx`] surface plus the recorded
//! history — that builds the Adya dependency graph (write-write,
//! write-read, read-write/anti-dependency edges) and reports any cycle,
//! classified exactly as elle does:
//!
//! - **G0** (dirty write): a cycle using only ww-edges.
//! - **G1c** (aberrant read of a stale-but-plausible value): a cycle using
//!   only ww/wr-edges.
//! - **G-single**: a cycle with exactly one rw-edge.
//! - **G2**: a cycle with two or more rw-edges.
//!
//! Two Adya anomalies (**G1a**, reading an aborted write; **G1b**, reading a
//! non-final write of the same key by the same transaction) are not
//! separately modeled as graph classes: the storage contract makes them
//! structurally unrepresentable rather than merely untested — `commit`
//! *consumes* the transaction (`WriteTx::commit(self) -> Result<()>`), so
//! there is no committed-but-aborted state for a reader to observe (no G1a),
//! and every read in this workload targets a register no earlier op in the
//! same transaction wrote (see `plan_txn`), so no transaction ever produces
//! two versions of one key to disagree about (no G1b). The checker still
//! catches an actual violation of the first claim: a read whose observed
//! write-id has no matching COMMITTED writer anywhere in the history is
//! reported directly, as a dirty/phantom read, before the graph is even
//! built.
//!
//! Any anomaly this checker finds is a real engine defect (SSI is the
//! storage contract's sealed guarantee — `storage/mod.rs`'s "every committed
//! history is therefore serializable in stamp order") — this module never
//! fixes one, only reports it (seed, history, cycle) for a filed issue to
//! reproduce against.
//!
//! ## Reproducibility, precisely
//!
//! One `u64` seed pins the WORKLOAD deterministically: the exact set of
//! transactions attempted, each one's ops, in program order — the seeded RNG
//! is drawn single-threaded, before any worker spawns. It does **not** pin
//! the interleaving: real OS thread scheduling decides which attempts race,
//! abort, and retry, so a genuine timing-sensitive defect may take several
//! reruns of the same seed to reproduce — the honest caveat of testing real
//! concurrency, not a determinism gap in this harness (contrast the
//! thread-count-parameterized determinism campaign in `query/trials.rs`,
//! whose claim is exact byte-identical output at a chosen thread count).

#![cfg(test)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{ConflictError, ReadTx, Storage, WriteTx, new_fjall_storage};

// ════════════════════════════════════════════════════════════════════════
// Seeded RNG — the splitmix64 of `storage/sim.rs`, transcribed exactly as
// `query/trials.rs` and `query/time_travel_trials.rs` already do (each
// harness owns its copy; the generator is private to its file).
// ════════════════════════════════════════════════════════════════════════

struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        Rng { state: seed }
    }
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        debug_assert!(n > 0);
        self.next_u64() % n
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        debug_assert!(hi > lo);
        lo + self.below((hi - lo) as u64) as i64
    }
    fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }
}

// ════════════════════════════════════════════════════════════════════════
// The workload: transactions over a small pool of shared registers.
//
// Each register is one raw KV key (no relation/tuple encoding — the
// `Storage`/`WriteTx` contract is backend-agnostic over arbitrary bytes;
// see `storage/mod.rs`'s trait docs). Its value is the 8-byte id of the
// write that produced it: not the register's "business value" — identity is
// all a serializability checker needs, and a unique id per write removes any
// ambiguity a reader's observed value could have (two writes never produce
// indistinguishable values, so every read attributes to exactly one write).
// ════════════════════════════════════════════════════════════════════════

const NUM_REGISTERS: u32 = 8;
const THREAD_COUNT: usize = 4;
const TXNS_PER_THREAD: usize = 40;
/// Reserved write-id for the pre-campaign seeding write of every register —
/// never issued by the campaign's write-id counter (which starts at 1).
const GENESIS_WRITE_ID: u64 = 0;

fn reg_key(reg: u32) -> [u8; 4] {
    reg.to_be_bytes()
}
fn encode_write_id(id: u64) -> [u8; 8] {
    id.to_be_bytes()
}
fn decode_write_id(bytes: &[u8]) -> u64 {
    u64::from_be_bytes(
        bytes
            .try_into()
            .expect("register value is an 8-byte write-id"),
    )
}

#[derive(Clone, Copy, Debug)]
enum OpKind {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug)]
struct PlannedOp {
    reg: u32,
    kind: OpKind,
}

/// One transaction's plan: 2..=4 ops over DISTINCT registers (never read
/// *and* write the same register in one transaction — see the module docs'
/// G1b argument), each independently a read or a write.
fn plan_txn(rng: &mut Rng) -> Vec<PlannedOp> {
    let n_ops = rng.range(2, 5) as usize;
    let mut pool: Vec<u32> = (0..NUM_REGISTERS).collect();
    let mut ops = Vec::with_capacity(n_ops);
    for _ in 0..n_ops {
        let idx = rng.below(pool.len() as u64) as usize;
        let reg = pool.remove(idx);
        let kind = if rng.chance(1, 2) {
            OpKind::Write
        } else {
            OpKind::Read
        };
        ops.push(PlannedOp { reg, kind });
    }
    ops
}

#[derive(Clone, Copy, Debug)]
enum ExecutedOp {
    Read { reg: u32, write_id: u64 },
    Write { reg: u32, write_id: u64 },
}

/// One COMMITTED transaction, as the checker sees it: its executed ops (in
/// program order) and its total-order key — the storage's OWN system
/// stamp (`WriteTx::system_stamp`, minted at `write_tx()` open under the
/// sealed snapshot-then-mint rule), the ONE witness `storage/mod.rs` itself
/// seals as valid: "every committed history is therefore serializable in
/// stamp order."
///
/// **kyzodb/kyzo#95, fixed here.** An earlier version used a locally
/// computed `commit_seq` instead — an `AtomicU64` incremented in the
/// CALLING thread strictly AFTER `commit()` already returned `Ok`, i.e.
/// after fjall's internally-serialized commit application (`storage/mod.rs`:
/// "commit application is serial ... under a global lock") had already
/// happened. That left a real window: thread A's commit could be
/// internally serialized before thread B's, yet if A got descheduled
/// before running its OWN post-commit incr, B's could run first — so the
/// recorded `commit_seq` order could legitimately invert relative to the
/// TRUE internal order, an external race with nothing to do with SSI.
/// Confirmed directly: forcing that window open (an injected delay at the
/// old post-commit site) produced a false G0/G1c/GSingle/G2 cycle in 19 of
/// 60 seeds via `commit_seq` ordering, and ZERO via `system_stamp` ordering
/// on the IDENTICAL recorded executions — same checker, same data, only
/// the ordering witness differed. `system_stamp` has no such window: it is
/// a VALUE captured once at transaction-open time, not a side effect
/// racing anything after commit returns — the class is unrepresentable
/// here, not merely avoided.
#[derive(Clone, Debug)]
struct CommittedTxn {
    ops: Vec<ExecutedOp>,
    stamp: i64,
}

fn seed_registers<S: Storage>(storage: &S) {
    let mut tx = storage.write_tx().expect("open genesis write_tx");
    for reg in 0..NUM_REGISTERS {
        tx.put(&reg_key(reg), &encode_write_id(GENESIS_WRITE_ID))
            .expect("seed register");
    }
    tx.commit().expect("genesis commit (uncontended)");
}

/// Run one planned transaction to commit, retrying on `ConflictError` exactly
/// as `storage/tests.rs`'s own contention tests do — a fresh attempt against
/// a fresh snapshot each time, so a retried attempt's writes get fresh
/// write-ids (the discarded attempt's ids simply never appear in history).
fn run_txn<S: Storage>(storage: &S, plan: &[PlannedOp], write_id_ctr: &AtomicU64) -> CommittedTxn {
    loop {
        let mut tx = storage.write_tx().expect("open write_tx");
        // Captured now, at open time (snapshot-then-mint) — a VALUE, not a
        // side effect racing anything after `commit()` returns (see
        // `CommittedTxn`'s doc for why that distinction is the whole fix).
        let stamp = tx.system_stamp().raw();
        let mut ops = Vec::with_capacity(plan.len());
        for p in plan {
            match p.kind {
                OpKind::Read => {
                    let bytes = tx
                        .get(&reg_key(p.reg))
                        .expect("read op")
                        .expect("every register is seeded before the campaign starts");
                    ops.push(ExecutedOp::Read {
                        reg: p.reg,
                        write_id: decode_write_id(&bytes),
                    });
                }
                OpKind::Write => {
                    let id = write_id_ctr.fetch_add(1, Ordering::SeqCst);
                    tx.put(&reg_key(p.reg), &encode_write_id(id))
                        .expect("write op");
                    ops.push(ExecutedOp::Write {
                        reg: p.reg,
                        write_id: id,
                    });
                }
            }
        }
        match tx.commit() {
            Ok(()) => return CommittedTxn { ops, stamp },
            Err(e) if e.downcast_ref::<ConflictError>().is_some() => continue,
            Err(e) => panic!("unexpected commit error (not a SSI conflict): {e:?}"),
        }
    }
}

/// Run the whole campaign for one seed: plan every transaction up front
/// (single-threaded, from the seed alone), then execute them for real across
/// `THREAD_COUNT` worker threads sharing one `FjallStorage`.
fn run_campaign(seed: u64) -> Vec<CommittedTxn> {
    let mut rng = Rng::new(seed);
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = new_fjall_storage(dir.path()).expect("open fjall storage");
    seed_registers(&storage);

    let write_id_ctr = AtomicU64::new(1); // 0 is GENESIS_WRITE_ID, never reissued.

    let plans: Vec<Vec<Vec<PlannedOp>>> = (0..THREAD_COUNT)
        .map(|_| (0..TXNS_PER_THREAD).map(|_| plan_txn(&mut rng)).collect())
        .collect();

    std::thread::scope(|scope| {
        let handles: Vec<_> = plans
            .into_iter()
            .map(|thread_plans| {
                let storage = storage.clone();
                let write_id_ctr = &write_id_ctr;
                scope.spawn(move || {
                    thread_plans
                        .iter()
                        .map(|plan| run_txn(&storage, plan, write_id_ctr))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("worker thread panicked"))
            .collect()
    })
}

// ════════════════════════════════════════════════════════════════════════
// The independent checker: Adya's dependency graph over the recorded
// history, built from the ops alone — no storage-tier symbol beyond what
// the workload above already used to produce the history.
// ════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EdgeKind {
    /// Write-write: this key's next committed version, in real commit order.
    Ww,
    /// Write-read: a transaction read a version this transaction wrote.
    Wr,
    /// Read-write (anti-dependency): a transaction read a version that this
    /// transaction later overwrote.
    Rw,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Anomaly {
    G0,
    G1c,
    GSingle,
    G2,
}

impl Anomaly {
    fn classify(cycle: &[(usize, usize, EdgeKind)]) -> Anomaly {
        let rw_count = cycle.iter().filter(|(_, _, k)| *k == EdgeKind::Rw).count();
        match rw_count {
            0 if cycle.iter().all(|(_, _, k)| *k == EdgeKind::Ww) => Anomaly::G0,
            0 => Anomaly::G1c,
            1 => Anomaly::GSingle,
            _ => Anomaly::G2,
        }
    }
}

/// Per-register version order: the genesis write (owner `None`) followed by
/// every real committed write to that register, sorted by `stamp` — the
/// storage's own sealed serialization witness (see `CommittedTxn`'s doc for
/// why this, and not a locally-computed sequence, is the right key).
/// `(owner, write_id)`.
fn version_chains(txns: &[CommittedTxn]) -> BTreeMap<u32, Vec<(Option<usize>, u64)>> {
    let mut real_writes: BTreeMap<u32, Vec<(i64, usize, u64)>> = BTreeMap::new();
    for (idx, txn) in txns.iter().enumerate() {
        for op in &txn.ops {
            if let ExecutedOp::Write { reg, write_id } = *op {
                real_writes
                    .entry(reg)
                    .or_default()
                    .push((txn.stamp, idx, write_id));
            }
        }
    }
    let mut chains: BTreeMap<u32, Vec<(Option<usize>, u64)>> = BTreeMap::new();
    for reg in 0..NUM_REGISTERS {
        chains.insert(reg, vec![(None, GENESIS_WRITE_ID)]);
    }
    for (reg, mut writes) in real_writes {
        writes.sort_by_key(|(seq, _, _)| *seq);
        chains
            .get_mut(&reg)
            .expect("every register pre-seeded")
            .extend(writes.into_iter().map(|(_, idx, wid)| (Some(idx), wid)));
    }
    chains
}

struct HistoryCheck {
    /// A read observed a write-id with no matching committed writer anywhere
    /// in this key's version chain: a dirty/phantom read (the storage
    /// contract's G1a-equivalent, checked directly rather than assumed).
    integrity_findings: Vec<String>,
    /// The first serializability-violating cycle found, if any, classified.
    cycle: Option<(Anomaly, Vec<(usize, usize, EdgeKind)>)>,
}

fn check_history(txns: &[CommittedTxn]) -> HistoryCheck {
    let chains = version_chains(txns);
    let mut edges: Vec<(usize, usize, EdgeKind)> = Vec::new();

    // ww: consecutive real writers in each key's version order.
    for chain in chains.values() {
        for pair in chain.windows(2) {
            if let (Some(from), Some(to)) = (pair[0].0, pair[1].0) {
                edges.push((from, to, EdgeKind::Ww));
            }
        }
    }

    // wr + rw: every read, attributed to the writer whose id it observed.
    let mut integrity_findings = Vec::new();
    for (reader_idx, txn) in txns.iter().enumerate() {
        for op in &txn.ops {
            let ExecutedOp::Read { reg, write_id } = *op else {
                continue;
            };
            let chain = &chains[&reg];
            let Some(pos) = chain.iter().position(|(_, wid)| *wid == write_id) else {
                integrity_findings.push(format!(
                    "txn {reader_idx} read reg {reg} write-id {write_id}: no committed writer \
                     anywhere in this key's history — dirty or phantom read"
                ));
                continue;
            };
            if let Some(writer_idx) = chain[pos].0 {
                edges.push((writer_idx, reader_idx, EdgeKind::Wr));
            }
            if let Some(&(Some(next_writer_idx), _)) = chain.get(pos + 1)
                && next_writer_idx != reader_idx
            {
                edges.push((reader_idx, next_writer_idx, EdgeKind::Rw));
            }
        }
    }

    let cycle = find_cycle(txns.len(), &edges).map(|c| (Anomaly::classify(&c), c));
    HistoryCheck {
        integrity_findings,
        cycle,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Color {
    White,
    Gray,
    Black,
}

/// A witness cycle in the dependency graph, if one exists: standard
/// DFS cycle detection (white/gray/black), returning the edges of the first
/// cycle found — `v -> ... -> u -> v` — the moment a gray (on-path) node is
/// re-reached. Any cycle at all is the anomaly: a serializable history's
/// dependency graph is acyclic (the classic conflict-serializability
/// theorem, Adya's generalization of it to SI/SSI).
fn find_cycle(
    n: usize,
    edges: &[(usize, usize, EdgeKind)],
) -> Option<Vec<(usize, usize, EdgeKind)>> {
    let mut adj: Vec<Vec<(usize, EdgeKind)>> = vec![Vec::new(); n];
    for &(from, to, kind) in edges {
        adj[from].push((to, kind));
    }
    let mut color = vec![Color::White; n];
    let mut parent_edge: Vec<Option<(usize, EdgeKind)>> = vec![None; n];

    fn visit(
        u: usize,
        adj: &[Vec<(usize, EdgeKind)>],
        color: &mut [Color],
        parent_edge: &mut [Option<(usize, EdgeKind)>],
    ) -> Option<Vec<(usize, usize, EdgeKind)>> {
        color[u] = Color::Gray;
        for &(v, kind) in &adj[u] {
            match color[v] {
                Color::White => {
                    parent_edge[v] = Some((u, kind));
                    if let Some(cyc) = visit(v, adj, color, parent_edge) {
                        return Some(cyc);
                    }
                }
                Color::Gray => {
                    let mut cyc = Vec::new();
                    let mut cur = u;
                    while cur != v {
                        let (p, k) = parent_edge[cur].expect("on-path node has a parent edge");
                        cyc.push((p, cur, k));
                        cur = p;
                    }
                    cyc.reverse();
                    cyc.push((u, v, kind));
                    return Some(cyc);
                }
                Color::Black => {}
            }
        }
        color[u] = Color::Black;
        None
    }

    for start in 0..n {
        if color[start] == Color::White
            && let Some(cyc) = visit(start, adj.as_slice(), &mut color, &mut parent_edge)
        {
            return Some(cyc);
        }
    }
    None
}

/// Run the full battery for one seed. `Ok(())` means the history is
/// serializable and every read attributed to a real committed writer; the
/// `Err` string names the anomaly (a campaign pins it against its seed).
fn run_seed(seed: u64) -> Result<(), String> {
    let txns = run_campaign(seed);
    let check = check_history(&txns);
    if !check.integrity_findings.is_empty() {
        return Err(format!(
            "integrity violation(s): {:?}",
            check.integrity_findings
        ));
    }
    if let Some((anomaly, cycle)) = check.cycle {
        return Err(format!(
            "{anomaly:?} serializability violation: cycle {cycle:?}"
        ));
    }
    Ok(())
}

/// How many synthetic CPU stressor threads
/// [`single_node_serializability_campaign_under_synthetic_cpu_pressure`]
/// spawns: a hint at real parallelism, so the pressure scales with the
/// machine instead of a fixed guess.
fn stressor_thread_count() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(4)
}

/// Regression pin for kyzodb/kyzo#95. The original finding surfaced ONLY
/// under `--test-threads=8` alongside other concurrently-running tests —
/// external CPU pressure perturbing thread scheduling enough to widen the
/// (now-fixed) `commit_seq` race. Reproduces that condition directly, on
/// every default run, with real contention (busy-loop stressor threads
/// racing the campaign's own worker threads for CPU) rather than the
/// synthetic single-site delay used to DIAGNOSE the bug (that delay forced
/// open a race window this fix removed entirely — `system_stamp` is a
/// value captured at open time, with no post-commit step left to perturb).
#[test]
fn single_node_serializability_campaign_under_synthetic_cpu_pressure() {
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        let stressors: Vec<_> = (0..stressor_thread_count())
            .map(|_| {
                let stop = &stop;
                scope.spawn(move || {
                    let mut acc = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        acc = std::hint::black_box((0..10_000u64).fold(acc, u64::wrapping_add));
                    }
                    acc
                })
            })
            .collect();

        let base = seed_base();
        let count = seed_count();
        let mut failures: Vec<(u64, String)> = Vec::new();
        for i in 0..count {
            let seed = Rng::new(base ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15)).next_u64();
            if let Err(f) = run_seed(seed) {
                failures.push((seed, f));
            }
        }

        stop.store(true, Ordering::Relaxed);
        for s in stressors {
            s.join().expect("stressor thread panicked");
        }
        assert!(
            failures.is_empty(),
            "Jepsen campaign FINDINGS under synthetic CPU pressure ({} of {count}): {failures:?}",
            failures.len()
        );
    });
}

/// How many seeds to sweep. Bounded by default (seconds); a campaign run
/// scales it up via the environment (the `KYZO_TRIALS_SEEDS` pattern of
/// `query/trials.rs`).
fn seed_count() -> u64 {
    std::env::var("KYZO_JEPSEN_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6)
}

fn seed_base() -> u64 {
    std::env::var("KYZO_JEPSEN_BASE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

#[test]
fn single_node_serializability_campaign() {
    let base = seed_base();
    let count = seed_count();
    let mut failures: Vec<(u64, String)> = Vec::new();
    for i in 0..count {
        let seed = Rng::new(base ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15)).next_u64();
        if let Err(f) = run_seed(seed) {
            failures.push((seed, f));
        }
    }
    assert!(
        failures.is_empty(),
        "Jepsen single-node campaign FINDINGS ({} of {count}): {failures:?}",
        failures.len()
    );
}

/// Falsification seal for kyzo#95's fix, mandatory per the finding's own
/// discipline: removing a false positive is only correct if the checker
/// hasn't ALSO gone blind to a true one — "0 cycles" must mean "the engine
/// is correct," never "the checker is now vacuous." Hand-built history, no
/// real storage involved (the two witnesses this issue was ever about,
/// `commit_seq` and `stamp`, do not even enter into it): two transactions
/// each read the OTHER's register at its GENESIS value and write their OWN
/// — the classic write-skew shape (each is consistent run alone; run
/// together, both cannot be, since each's read is stale by the time the
/// other's write lands) — which produces exactly two anti-dependency
/// (`Rw`) edges forming a 2-cycle: the canonical G2. `check_history` (the
/// SAME function `run_seed` calls, completely unmodified by this fix) must
/// still report it under the now-stamp-ordered `CommittedTxn` shape.
#[test]
fn check_history_still_flags_a_genuine_write_skew_g2_cycle() {
    const REG_A: u32 = 0;
    const REG_B: u32 = 1;
    let txns = vec![
        CommittedTxn {
            // T0: reads B at its genesis value, writes A.
            ops: vec![
                ExecutedOp::Read {
                    reg: REG_B,
                    write_id: GENESIS_WRITE_ID,
                },
                ExecutedOp::Write {
                    reg: REG_A,
                    write_id: 100,
                },
            ],
            stamp: 10,
        },
        CommittedTxn {
            // T1: reads A at its genesis value, writes B.
            ops: vec![
                ExecutedOp::Read {
                    reg: REG_A,
                    write_id: GENESIS_WRITE_ID,
                },
                ExecutedOp::Write {
                    reg: REG_B,
                    write_id: 200,
                },
            ],
            stamp: 20,
        },
    ];
    let check = check_history(&txns);
    assert!(
        check.integrity_findings.is_empty(),
        "no integrity findings expected in this hand-built history: {:?}",
        check.integrity_findings
    );
    let (anomaly, cycle) = check
        .cycle
        .expect("a genuine write-skew history must be flagged, never silently accepted");
    assert_eq!(
        anomaly,
        Anomaly::G2,
        "two independent anti-dependency edges (each txn reads the other's stale value) \
         is the canonical G2 shape, got {anomaly:?}: {cycle:?}"
    );
}

// Regression pins for seeds a campaign has surfaced go here, each as a named
// test asserting `run_seed(SEED).is_ok()`. None to date.
