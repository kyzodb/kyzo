/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `kyzo-crashfs`: a pure-Rust FUSE passthrough fault injector.
//!
//! Story #31, phase 1. Design ruling pinned on the issue: a purpose-built
//! FUSE passthrough on the `fuser` crate (LazyFS's torn-seq / torn-op /
//! clear-cache vocabulary), seed-deterministic per the identity-keyed
//! discipline `kyzo-core`'s `storage/sim.rs` already proves at the
//! trait-level `Storage` seam — lifted here to the real FUSE op level so a
//! later phase can crash a *real* `FjallStorage`, not just its trait
//! double.
//!
//! This crate is test tooling: never shipped, never a `kyzo-core`
//! dependency (the dependency edge, if it ever existed, would run the
//! other way — a future storage-test harness depending on this crate — and
//! even that is phase 2's concern, not this crate's).
//!
//! - [`fault`]: the pure fault-plan core — trigger points and ambient
//!   rates, both a pure function of `(seed, path, op kind, byte range,
//!   attempt count)`. No I/O, no FUSE dependency; exhaustively unit-tested
//!   here so the decision logic is provably correct independent of whether
//!   a live FUSE mount is available in a given sandbox.
//! - [`passthrough`]: the `fuser::Filesystem` implementation wiring that
//!   plan to a real backing directory via a buffered-write/fsync
//!   durability model (see the module doc for why a naive `pwrite` shim
//!   cannot express these faults at all).
//!
//! ## Phase 2 (not this crate's scope yet): driving `FjallStorage`
//!
//! Phase 1 proves the injector standalone. Phase 2 mounts it, points a
//! real `FjallStorage` at the mountpoint instead of a plain directory, and
//! runs the crash-recovery campaign the story is actually for:
//!
//! 1. **Drive**: open `FjallStorage` on the mount; run a compaction-forcing
//!    write flood (the highest-value unknown per the design ruling is
//!    segment-file torn-write behavior, so the first load-bearing workload
//!    must force compaction, not just journal writes) while a
//!    [`FaultPlan`](fault::FaultPlan) with triggers anchored to durability
//!    barriers (`commit` / `commit_durable` / flush-compaction boundaries —
//!    never raw byte offsets, per the design ruling's field-converged
//!    lesson) is armed.
//! 2. **Crash**: at each trigger, the mounted view goes dark exactly as
//!    this crate's `ClearCache`/`TornSeq`/`TornOp` dictate; reopen
//!    `FjallStorage` against the same (now-corrupted-per-plan) backing
//!    directory.
//! 3. **Oracle**: `SimStorage` (already sealed at the trait level) is fed
//!    the identical logical op log and crashed/power-cut at the analogous
//!    point — its resulting key set is the expected answer. Assertions per
//!    crash point: opens clean or refuses with a typed error (never a
//!    panic or silently-wrong bytes), the visible state equals *some*
//!    committed prefix, and `FormatVersion`/catalog bytes are never
//!    half-written.
//! 4. **The lsm-tree finding, binding on phase 2's assertions**: `Table::
//!    recover` only verifies meta/TLI/pinned-filter blocks at open time;
//!    data blocks are checksummed lazily, on first read. A reopen-only
//!    assertion would walk right past a torn data block untouched on disk.
//!    Every phase-2 assertion after a crash point **must read every key in
//!    the affected range** (forcing data-block traversal), not just assert
//!    the store opened — "opens clean" is necessary, never sufficient.
//! 5. **Falsification clause**: the campaign only earns its keep if it can
//!    catch what `SimStorage`'s trait-level injection cannot — bugs in
//!    `fjall`'s own recovery code. A large clean run is a reportable
//!    result on its own, not a reason to inflate seeds looking for a hit.

pub mod fault;
pub mod harness;
pub mod passthrough;

pub use fault::{AmbientRates, Counters, Fault, FaultPlan, OpKind, Trigger, WriteOutcome};
pub use passthrough::{FaultCounters, PassthroughFs};
