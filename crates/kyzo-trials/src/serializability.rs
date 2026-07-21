/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! # External isolation-checking tier (Elle-style) — #376 T6 first milestone
//!
//! Black-box serializability / isolation-anomaly checker over the SSI claim:
//! record an Elle history from the real concurrent campaign, then detect
//! Adya/Elle cycles (G0 / G1 / G2) independently of storage-tier internals.
//! This is the external tier TigerBeetle admits found what in-process DST
//! structurally could not — the checker sees only committed reads/writes and
//! the engine's sealed stamp order, never SSI's internal conflict machinery.
//!
//! Trial roots (issue #34): a single-node, elle/Adya-style campaign over the
//! real `fjall`-backed [`Storage`] — the SSI core, driven directly
//! (`write_tx`/`get`/`put`/`commit`), never through [`crate::Db::run_script`]
//! (whose mutation path retries a whole script on conflict, so it never
//! surfaces a raw abort to the caller — unsuitable for a harness that needs
//! the true commit/abort truth of every attempt).
//!
//! ## First milestone (this module)
//!
//! 1. **Elle history recording** — [`ElleHistory`]: every COMMITTED txn's
//!    reads and writes, ordered by `system_stamp`.
//! 2. **G0 / G1 / G2 anomaly detection** against that history (and against
//!    the live serializability campaign).
//! 3. **Cycle detection a la Elle** — Adya dependency graph (ww / wr / rw)
//!    over the SSI claim; any cycle is a serializability violation.
//!
//! Classification (Elle / Adya):
//!
//! - **G0** (dirty write): a cycle using only ww-edges.
//! - **G1** (Adya G1c): a cycle using only ww/wr-edges (no rw).
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
//! two versions of one key to disagree about (no G1b). Range reads claim a
//! half-open register span and remove those registers from the same-txn
//! pool — the same distinct-register law, now over a predicate footprint.
//! The checker still catches an actual violation of the first claim: a read
//! whose observed write-id has no matching COMMITTED writer anywhere in the
//! history is reported directly, as a dirty/phantom read, before the graph
//! is even built.
//!
//! Predicate / phantom anti-dependencies are first-class: a [`ExecutedOp::RangeRead`]
//! induces item wr/rw edges on every version it observes inside `[lo, hi)`,
//! plus a predicate rw-edge to the first real writer of any register in the
//! span that the range read did **not** observe (Adya phantom / predicate
//! rw — the insert that should have matched). Without `RangeRead` in the
//! history model those edges are structurally unreachable.
//!
//! ## Out of scope (named)
//!
//! - **The distributed rig** (partitions, replica divergence) — returns with
//!   replication post-1.0.
//! - **Fault injection through `kyzo-bin` HTTP** — sequenced after the crash
//!   injector; not a gap in this module's black-box claim.
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

use kyzo::{ReadTx, Storage, WriteTx, new_fjall_storage};

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
        // INVARIANT(splitmix64): modular mix per the splitmix64 contract; wrap is the PRNG.
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

/// One planned micro-op. `RangeRead` claims every register in `[lo, hi)` —
/// those ids are removed from the same-txn pool so a later Write cannot
/// target a register the range already observed (G1b unrepresentable).
#[derive(Clone, Copy, Debug)]
enum PlannedOp {
    Read {
        reg: u32,
    },
    Write {
        reg: u32,
    },
    /// Half-open register span `[lo, hi)`, executed via [`ReadTx::range_scan`].
    RangeRead {
        lo: u32,
        hi: u32,
    },
}

/// One transaction's plan: 2..=4 ops over DISTINCT registers (never read
/// *and* write the same register in one transaction — see the module docs'
/// G1b argument). Ops are point reads, point writes, or range reads over a
/// contiguous free span.
fn plan_txn(rng: &mut Rng) -> Vec<PlannedOp> {
    let n_ops = rng.range(2, 5) as usize;
    let mut available = [true; NUM_REGISTERS as usize];
    let mut ops = Vec::with_capacity(n_ops);
    for _ in 0..n_ops {
        let free: Vec<u32> = (0..NUM_REGISTERS)
            .filter(|&r| available[r as usize])
            .collect();
        if free.is_empty() {
            break;
        }
        // Prefer some range reads so predicate/phantom rw-edges are reachable
        // in live campaign histories, not only in hand-built checker seals.
        if free.len() >= 2 && rng.chance(1, 3) {
            let start_idx = rng.below(free.len() as u64) as usize;
            let lo = free[start_idx];
            let mut max_hi = lo + 1;
            while max_hi < NUM_REGISTERS && available[max_hi as usize] {
                max_hi += 1;
            }
            let width = max_hi - lo;
            let take = 1 + rng.below(width as u64) as u32;
            let hi = lo + take;
            for r in lo..hi {
                available[r as usize] = false;
            }
            ops.push(PlannedOp::RangeRead { lo, hi });
        } else {
            let idx = rng.below(free.len() as u64) as usize;
            let reg = free[idx];
            available[reg as usize] = false;
            if rng.chance(1, 2) {
                ops.push(PlannedOp::Write { reg });
            } else {
                ops.push(PlannedOp::Read { reg });
            }
        }
    }
    ops
}

#[derive(Clone, Debug)]
enum ExecutedOp {
    Read {
        reg: u32,
        write_id: u64,
    },
    Write {
        reg: u32,
        write_id: u64,
    },
    /// Observed `(reg, write_id)` pairs inside `[lo, hi)`, in ascending reg order.
    /// A register in the span missing from `observed` is an Adya phantom /
    /// predicate non-match at read time — the checker draws a predicate rw
    /// to the first real writer of that register.
    RangeRead {
        lo: u32,
        hi: u32,
        observed: Vec<(u32, u64)>,
    },
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
/// old post-commit site) produced a false G0/G1/GSingle/G2 cycle in 19 of
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

/// Black-box Elle history: every COMMITTED transaction's reads and writes.
/// The external isolation-checking tier (#376 T6) consumes only this shape —
/// what an out-of-process Jepsen/Elle client would observe — never SSI's
/// internal conflict sets or abort reasons.
#[derive(Clone, Debug)]
struct ElleHistory {
    txns: Vec<CommittedTxn>,
}

impl ElleHistory {
    fn record(txns: Vec<CommittedTxn>) -> Self {
        Self { txns }
    }

    fn check(&self) -> HistoryCheck {
        check_history(&self.txns)
    }
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
            match *p {
                PlannedOp::Read { reg } => {
                    let bytes = tx
                        .get(&reg_key(reg))
                        .expect("read op")
                        .expect("every register is seeded before the campaign starts");
                    ops.push(ExecutedOp::Read {
                        reg,
                        write_id: decode_write_id(&bytes),
                    });
                }
                PlannedOp::Write { reg } => {
                    let id = write_id_ctr.fetch_add(1, Ordering::SeqCst);
                    tx.put(&reg_key(reg), &encode_write_id(id))
                        .expect("write op");
                    ops.push(ExecutedOp::Write { reg, write_id: id });
                }
                PlannedOp::RangeRead { lo, hi } => {
                    // Inclusive lower / exclusive upper over memcmp-ordered keys
                    // (u32 BE) — the storage contract's range_scan shape, and the
                    // footprint SSI conflict-tracks for phantom protection.
                    let mut observed = Vec::new();
                    for item in tx.range_scan(&reg_key(lo), &reg_key(hi)) {
                        let (k, v) = item.expect("range_scan item");
                        let reg = u32::from_be_bytes(
                            k.as_ref().try_into().expect("register key is 4 bytes"),
                        );
                        observed.push((reg, decode_write_id(v.as_ref())));
                    }
                    ops.push(ExecutedOp::RangeRead { lo, hi, observed });
                }
            }
        }
        match tx.commit() {
            Ok(_committed) => return CommittedTxn { ops, stamp },
            Err(e) if e.is_conflict() => continue,
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
    /// Dirty write: cycle of only ww-edges (Elle G0).
    G0,
    /// Cycle of only ww/wr-edges — Adya G1c, the G1 graph class for this tier.
    G1,
    /// Exactly one rw-edge in the cycle (Elle G-single).
    GSingle,
    /// Two or more rw-edges (Elle G2).
    G2,
}

impl Anomaly {
    fn classify(cycle: &[(usize, usize, EdgeKind)]) -> Anomaly {
        let rw_count = cycle.iter().filter(|(_, _, k)| *k == EdgeKind::Rw).count();
        match rw_count {
            0 if cycle.iter().all(|(_, _, k)| *k == EdgeKind::Ww) => Anomaly::G0,
            0 => Anomaly::G1,
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
            if let ExecutedOp::Write { reg, write_id } = op {
                real_writes
                    .entry(*reg)
                    .or_default()
                    .push((txn.stamp, idx, *write_id));
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

/// Item wr + adjacent rw for one observed `(reg, write_id)` — shared by point
/// [`ExecutedOp::Read`] and each version a [`ExecutedOp::RangeRead`] selects.
fn push_item_read_edges(
    edges: &mut Vec<(usize, usize, EdgeKind)>,
    integrity_findings: &mut Vec<String>,
    chains: &BTreeMap<u32, Vec<(Option<usize>, u64)>>,
    reader_idx: usize,
    reg: u32,
    write_id: u64,
) {
    let chain = &chains[&reg];
    let Some(pos) = chain.iter().position(|(_, wid)| *wid == write_id) else {
        integrity_findings.push(format!(
            "txn {reader_idx} read reg {reg} write-id {write_id}: no committed writer \
             anywhere in this key's history — dirty or phantom read"
        ));
        return;
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

    // wr + rw: every point/range read, attributed to the writer whose id it
    // observed; range reads also emit predicate rw-edges for unobserved keys
    // in the scanned span (Adya phantom / predicate anti-dependency).
    let mut integrity_findings = Vec::new();
    for (reader_idx, txn) in txns.iter().enumerate() {
        for op in &txn.ops {
            match op {
                ExecutedOp::Write { .. } => {}
                ExecutedOp::Read { reg, write_id } => {
                    push_item_read_edges(
                        &mut edges,
                        &mut integrity_findings,
                        &chains,
                        reader_idx,
                        *reg,
                        *write_id,
                    );
                }
                ExecutedOp::RangeRead { lo, hi, observed } => {
                    let mut seen = std::collections::BTreeSet::new();
                    for &(reg, write_id) in observed {
                        seen.insert(reg);
                        push_item_read_edges(
                            &mut edges,
                            &mut integrity_findings,
                            &chains,
                            reader_idx,
                            reg,
                            write_id,
                        );
                    }
                    // Predicate / phantom rw: a register in [lo, hi) the range
                    // did not observe — first real writer installs a matching
                    // version the predicate read missed.
                    for reg in *lo..*hi {
                        if seen.contains(&reg) {
                            continue;
                        }
                        let chain = &chains[&reg];
                        if let Some(&(Some(writer_idx), _)) =
                            chain.iter().find(|(owner, _)| owner.is_some())
                            && writer_idx != reader_idx
                        {
                            edges.push((reader_idx, writer_idx, EdgeKind::Rw));
                        }
                    }
                }
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

/// Run the full battery for one seed: campaign → Elle history → G0/G1/G2
/// cycle check. `Ok(())` means the history is serializable and every read
/// attributed to a real committed writer; the `Err` string names the anomaly
/// (a campaign pins it against its seed).
fn run_seed(seed: u64) -> Result<(), String> {
    let history = ElleHistory::record(run_campaign(seed));
    let check = history.check();
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
            // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
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
        // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
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

/// #376 T6: Elle history recording captures every committed read/write/range
/// from a live campaign — the black-box input the anomaly checker consumes.
#[test]
fn elle_history_recording_captures_serializability_campaign_ops() {
    let history = ElleHistory::record(run_campaign(0xE11E_0001));
    assert!(
        !history.txns.is_empty(),
        "campaign must commit at least one transaction into the Elle history"
    );
    let mut saw_read = false;
    let mut saw_write = false;
    let mut saw_range = false;
    for txn in &history.txns {
        for op in &txn.ops {
            match op {
                ExecutedOp::Read { .. } => saw_read = true,
                ExecutedOp::Write { .. } => saw_write = true,
                ExecutedOp::RangeRead { .. } => saw_range = true,
            }
        }
    }
    assert!(
        saw_read && saw_write && saw_range,
        "Elle history must record reads, writes, and range reads \
         (saw_read={saw_read}, saw_write={saw_write}, saw_range={saw_range})"
    );
    // Recording alone is not the claim — the recorded history must check clean
    // under the same G0/G1/G2 cycle detector the campaign uses.
    let check = history.check();
    assert!(
        check.integrity_findings.is_empty() && check.cycle.is_none(),
        "recorded Elle history must be serializable under SSI: integrity={:?} cycle={:?}",
        check.integrity_findings,
        check.cycle
    );
}

/// #376 T6: G0 — ww-only cycle. Stamp-ordered version chains make G0
/// unrepresentable in a real campaign (ww edges always follow stamp order),
/// so this seals the classifier + cycle detector directly — the same path
/// `check_history` uses after building edges.
#[test]
fn elle_anomaly_detection_flags_g0_ww_only_cycle() {
    let edges = [
        (0, 1, EdgeKind::Ww),
        (1, 2, EdgeKind::Ww),
        (2, 0, EdgeKind::Ww),
    ];
    let cycle = find_cycle(3, &edges).expect("ww-only triangle must be a cycle");
    assert_eq!(
        Anomaly::classify(&cycle),
        Anomaly::G0,
        "ww-only cycle is Elle G0, got cycle {cycle:?}"
    );
}

/// #376 T6: G1 — wr-only cycle (Adya G1c). Two committed writers each read
/// the other's write: a cycle with only wr-edges, no rw.
#[test]
fn elle_anomaly_detection_flags_g1_wr_cycle_in_hand_built_history() {
    const REG_A: u32 = 0;
    const REG_B: u32 = 1;
    let history = ElleHistory::record(vec![
        CommittedTxn {
            // T0: writes A, reads B observing T1's write.
            ops: vec![
                ExecutedOp::Write {
                    reg: REG_A,
                    write_id: 100,
                },
                ExecutedOp::Read {
                    reg: REG_B,
                    write_id: 200,
                },
            ],
            stamp: 10,
        },
        CommittedTxn {
            // T1: writes B, reads A observing T0's write.
            ops: vec![
                ExecutedOp::Write {
                    reg: REG_B,
                    write_id: 200,
                },
                ExecutedOp::Read {
                    reg: REG_A,
                    write_id: 100,
                },
            ],
            stamp: 20,
        },
    ]);
    let check = history.check();
    assert!(
        check.integrity_findings.is_empty(),
        "no integrity findings expected: {:?}",
        check.integrity_findings
    );
    let (anomaly, cycle) = check
        .cycle
        .expect("wr cycle (each reads the other's write) must be flagged as G1");
    assert_eq!(
        anomaly,
        Anomaly::G1,
        "wr-only cycle is Elle G1 (Adya G1c), got {anomaly:?}: {cycle:?}"
    );
}

/// Falsification seal for kyzo#95's fix + #376 T6 G2: removing a false
/// positive is only correct if the checker hasn't ALSO gone blind to a true
/// one — "0 cycles" must mean "the engine is correct," never "the checker is
/// now vacuous." Hand-built history, no real storage: two transactions each
/// read the OTHER's register at its GENESIS value and write their OWN — the
/// classic write-skew shape — which produces exactly two anti-dependency
/// (`Rw`) edges forming a 2-cycle: the canonical G2.
#[test]
fn elle_anomaly_detection_flags_g2_write_skew_cycle_in_serializability_history() {
    const REG_A: u32 = 0;
    const REG_B: u32 = 1;
    let history = ElleHistory::record(vec![
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
    ]);
    let check = history.check();
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

/// Predicate / phantom G2 via range reads: each txn range-scans a span it
/// records as empty, then writes into the other's span — mutual Adya
/// predicate rw-edges, unreachable before `RangeRead` existed in the model.
#[test]
fn elle_anomaly_detection_flags_g2_predicate_rw_via_range_read() {
    let history = ElleHistory::record(vec![
        CommittedTxn {
            // T0: range [0,2) observes nothing; writes reg 2 (into T1's span).
            ops: vec![
                ExecutedOp::RangeRead {
                    lo: 0,
                    hi: 2,
                    observed: vec![],
                },
                ExecutedOp::Write {
                    reg: 2,
                    write_id: 100,
                },
            ],
            stamp: 10,
        },
        CommittedTxn {
            // T1: range [2,4) observes nothing; writes reg 0 (into T0's span).
            ops: vec![
                ExecutedOp::RangeRead {
                    lo: 2,
                    hi: 4,
                    observed: vec![],
                },
                ExecutedOp::Write {
                    reg: 0,
                    write_id: 200,
                },
            ],
            stamp: 20,
        },
    ]);
    let check = history.check();
    assert!(
        check.integrity_findings.is_empty(),
        "no integrity findings expected: {:?}",
        check.integrity_findings
    );
    let (anomaly, cycle) = check
        .cycle
        .expect("mutual phantom inserts into each other's empty range reads must form a G2 cycle");
    assert_eq!(
        anomaly,
        Anomaly::G2,
        "two predicate/phantom rw-edges is G2, got {anomaly:?}: {cycle:?}"
    );
    let rw = cycle.iter().filter(|(_, _, k)| *k == EdgeKind::Rw).count();
    assert!(
        rw >= 2,
        "predicate G2 requires ≥2 rw-edges, got {rw} in {cycle:?}"
    );
}

/// Item-level G2 through a range read's selected versions (write skew on a
/// predicate footprint): each txn range-reads [0,2) at genesis and writes one
/// key inside that span — two anti-dependencies, same shape as point-read
/// write skew, but the read half is a single RangeRead.
#[test]
fn elle_anomaly_detection_flags_g2_write_skew_via_range_read() {
    const LO: u32 = 0;
    const HI: u32 = 2;
    let history = ElleHistory::record(vec![
        CommittedTxn {
            ops: vec![
                ExecutedOp::RangeRead {
                    lo: LO,
                    hi: HI,
                    observed: vec![(0, GENESIS_WRITE_ID), (1, GENESIS_WRITE_ID)],
                },
                ExecutedOp::Write {
                    reg: 0,
                    write_id: 100,
                },
            ],
            stamp: 10,
        },
        CommittedTxn {
            ops: vec![
                ExecutedOp::RangeRead {
                    lo: LO,
                    hi: HI,
                    observed: vec![(0, GENESIS_WRITE_ID), (1, GENESIS_WRITE_ID)],
                },
                ExecutedOp::Write {
                    reg: 1,
                    write_id: 200,
                },
            ],
            stamp: 20,
        },
    ]);
    let check = history.check();
    assert!(
        check.integrity_findings.is_empty(),
        "no integrity findings expected: {:?}",
        check.integrity_findings
    );
    let (anomaly, cycle) = check
        .cycle
        .expect("range-read write skew must be flagged as G2");
    assert_eq!(
        anomaly,
        Anomaly::G2,
        "range-read write skew is G2, got {anomaly:?}: {cycle:?}"
    );
}

/// #376 T6: the live serializability campaign is checked through the Elle
/// history recorder + G0/G1/G2 cycle detector (not a separate in-process
/// oracle that could share SSI's blind spots).
#[test]
fn external_elle_isolation_tier_against_serializability_campaign() {
    let base = seed_base();
    let count = seed_count();
    let mut failures: Vec<(u64, String)> = Vec::new();
    for i in 0..count {
        // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
        let seed = Rng::new(base ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15)).next_u64();
        let history = ElleHistory::record(run_campaign(seed));
        let check = history.check();
        if !check.integrity_findings.is_empty() {
            failures.push((
                seed,
                format!("integrity violation(s): {:?}", check.integrity_findings),
            ));
        } else if let Some((anomaly, cycle)) = check.cycle {
            failures.push((
                seed,
                format!("{anomaly:?} serializability violation: cycle {cycle:?}"),
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "Elle external isolation tier FINDINGS against serializability campaign \
         ({} of {count}): {failures:?}",
        failures.len()
    );
}

// Regression pins for seeds a campaign has surfaced go here, each as a named
// test asserting `run_seed(SEED).is_ok()`. None to date.
