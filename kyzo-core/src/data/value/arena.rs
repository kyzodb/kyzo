/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The order-preserving interning dictionary: dedups values, assigns dense `Code`, resolves `Code` -> canonical bytes. The out-of-line home.
//!
//! ## The type-C contract (ratified on #119)
//!
//! Dense codes, code-order-equals-byte-order, and validity under growth
//! cannot all hold at every *absolute* instant (pigeonhole). This arena
//! keeps all three for every observer that exists by making the observer
//! the unit of meaning: **a code is only valid inside a scoped observer
//! frame.**
//!
//! - [`Arena`] is minting and transition only: `intern`, `seal`, and the
//!   two ways to open an observer. It has no read methods — there is no
//!   unnamed frame to smuggle a code through.
//! - [`Frame`] is the live observer: a borrow of the arena's current
//!   state. Reads take a [`FrameCode`] witness, minted by
//!   [`Frame::admit`]. `intern` and `seal` take `&mut Arena`, so the
//!   borrow checker retires every frame *and every witness minted from
//!   it* at the next mutation.
//! - [`Snapshot`] is the pinned observer: run references + a delta cut +
//!   frozen heap chunks + the epoch — exactly the ruling's "snapshot's
//!   dictionary", owned and `Send + Sync`. It answers identically forever
//!   while the writer interns and seals past it.
//! - [`EpochRemap`] is the morphism between frames: minted only by
//!   [`Arena::seal`], it restamps a [`StampedCode`] from its epoch into
//!   the next — strictly monotone over sealed codes, a permutation over
//!   tail codes.
//!
//! ## Validity = epoch equality + observer visibility
//!
//! **The same-epoch coherence law**: within one epoch, every observer
//! agrees on every code *both can see* — sealed contents are identical
//! across same-epoch observers, and tail codes are arrival-stable, so two
//! same-epoch views differ only in how far their delta prefix extends,
//! never in what a shared code means. Epoch equality alone is therefore
//! *agreement*, not *visibility*; visibility is the observer's own extent:
//!
//! - For a live [`Frame`], visibility is implied rather than checked:
//!   stamps are mintable only by this plane (`Arena::intern`,
//!   [`EpochRemap::apply`]), every mint is in bounds when issued, within
//!   an epoch the arena only grows, and no `&mut Arena` (the only way to
//!   transition) can coexist with the frame. Epoch equality at
//!   [`Frame::admit`] is the whole runtime test, with the bounds theorem
//!   re-checked in debug builds.
//! - For a [`Snapshot`], visibility is **not** implied by the epoch: the
//!   delta cut is part of the observer, and a same-epoch code minted
//!   after the cut is invisible to it. Snapshots therefore verify both
//!   stamp and cut, exactly and loudly, on every spend. (A snapshot is
//!   deliberately not a lifetime-witness observer: two owned snapshots of
//!   different epochs can coexist with unifiable lifetimes, and a
//!   compile-time brand that unifies across them would claim a safety it
//!   cannot deliver.)
//!
//! ## The transition theorem, held by types
//!
//! 1. **Every observable value satisfies its law.** [`Run`]'s only mints
//!    are [`Run::build`] (sorts + dedups: establishes it) and
//!    [`Run::merge`] (preserves it); a [`Remap`] is mintable only by a
//!    merge; an [`EpochRemap`] only by [`Arena::seal`]; a [`FrameCode`]
//!    only by [`Frame::admit`]; a [`StampedCode`] only by this plane.
//!    None of the unlawful states can be written down.
//! 2. **No observer exists during a transition.** `seal` holds
//!    `&mut self`, so no `Frame` (or witness) survives into it, and
//!    `Snapshot`s hold only immutable structure — frozen chunks and runs
//!    the transition never mutates. Runs are shared (`Arc`), not
//!    consumed: old shapes legitimately outlive the transition *in old
//!    frames* — that is type-C itself — and their immutability is what
//!    makes it sound.
//!
//! Cascading run merges inside a seal never change codes: a sealed code is
//! a rank over the *union* of runs, and reorganizing which run holds a
//! value does not move the union. Only the delta's arrival changes ranks,
//! which is exactly what the [`EpochRemap`] describes.
//!
//! Comparison discipline: entries carry the shared 4-byte prefix
//! ([`super::prefix`]); every search decides on prefixes wherever they are
//! conclusive and dereferences payload bytes only on the one tie path,
//! which increments the deref counter — "dereferences only on a tie" is
//! measured, not asserted.
//!
//! Honest limit: epochs are per-`Arena`. Stamps do not distinguish two
//! distinct `Arena` instances; the value plane owns exactly one.

use std::cmp::Ordering;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrd};

use super::code::{Code, StampedCode};
use super::prefix::{PrefixCmp, cmp_prefixed, prefix4};

/// Payload chunk size. Values at or past this size get a chunk of their
/// own; smaller values pack into shared chunks.
const CHUNK_SIZE: usize = 64 * 1024;

/// Read access to payload bytes, implemented by the live [`Heap`] and by a
/// snapshot's frozen chunk set. `tie_payload` is the counted
/// comparison-tie path.
trait Store {
    fn payload(&self, span: Span) -> &[u8];
    fn deref_counter(&self) -> &AtomicU64;

    #[inline]
    fn tie_payload(&self, span: Span) -> &[u8] {
        self.deref_counter().fetch_add(1, AtomicOrd::Relaxed);
        self.payload(span)
    }
}

/// Append-only payload storage as immutable chunks. Frozen chunks are
/// shared (`Arc`) with snapshots and never mutated again; the live chunk
/// fills until it spills or a snapshot freezes it. Payload bytes never
/// move once written, so spans are stable identities for the heap's whole
/// life — transitions shuffle *handles*, never payloads.
pub struct Heap {
    frozen: Vec<Arc<[u8]>>,
    /// The chunk being filled; its chunk id is always `frozen.len()`, and
    /// freezing pushes it at exactly that index, so ids never move.
    live: Vec<u8>,
    /// Payload fetches forced by comparison ties: the instrument behind
    /// the "deref only on tie" proof. Shared with snapshots.
    compare_derefs: Arc<AtomicU64>,
}

/// A byte-string's location in a [`Heap`]: chunk id, offset, length. Only
/// [`Heap::push`] mints one.
#[derive(Clone, Copy, Debug)]
pub struct Span {
    chunk: u32,
    off: u32,
    len: u32,
}

impl Heap {
    pub fn new() -> Self {
        Heap {
            frozen: Vec::new(),
            live: Vec::new(),
            compare_derefs: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Store a byte-string, returning its permanent handle.
    ///
    /// # Panics
    ///
    /// Panics if a single value exceeds `u32::MAX` bytes or the chunk id
    /// space is exhausted.
    pub fn push(&mut self, value: &[u8]) -> Span {
        assert!(
            value.len() <= u32::MAX as usize,
            "value exceeds u32 span space"
        );
        if value.len() >= CHUNK_SIZE {
            // Oversize value: a chunk of its own.
            self.freeze_live();
            let chunk = self.chunk_id();
            self.frozen.push(Arc::from(value));
            return Span {
                chunk,
                off: 0,
                len: value.len() as u32,
            };
        }
        if self.live.len() + value.len() > CHUNK_SIZE {
            self.freeze_live();
        }
        let chunk = self.chunk_id();
        let off = self.live.len() as u32;
        self.live.extend_from_slice(value);
        Span {
            chunk,
            off,
            len: value.len() as u32,
        }
    }

    /// Freeze the live chunk (if non-empty) into the shared set. Its chunk
    /// id is unchanged: it lands at exactly the index it was addressed by.
    fn freeze_live(&mut self) {
        if !self.live.is_empty() {
            let done = std::mem::take(&mut self.live);
            self.frozen.push(done.into());
        }
    }

    fn chunk_id(&self) -> u32 {
        let id = self.frozen.len();
        assert!(id < u32::MAX as usize, "heap chunk id space exhausted");
        id as u32
    }

    pub fn get(&self, span: Span) -> &[u8] {
        self.payload(span)
    }

    /// Total payload fetches forced by comparison ties so far.
    pub fn compare_derefs(&self) -> u64 {
        self.compare_derefs.load(AtomicOrd::Relaxed)
    }
}

impl Store for Heap {
    fn payload(&self, span: Span) -> &[u8] {
        // A zero-length span owns no bytes and may address a chunk that
        // was never materialized (empty values append nothing).
        if span.len == 0 {
            return &[];
        }
        let (off, len) = (span.off as usize, span.len as usize);
        let c = span.chunk as usize;
        if c < self.frozen.len() {
            &self.frozen[c][off..off + len]
        } else {
            debug_assert_eq!(
                c,
                self.frozen.len(),
                "span addresses a chunk that never existed"
            );
            &self.live[off..off + len]
        }
    }

    fn deref_counter(&self) -> &AtomicU64 {
        &self.compare_derefs
    }
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

/// A snapshot's view of the heap: the frozen chunks as of the snapshot.
/// Spans minted after the snapshot address chunks beyond this set and
/// panic rather than aliasing.
struct FrozenStore {
    chunks: Vec<Arc<[u8]>>,
    compare_derefs: Arc<AtomicU64>,
}

impl Store for FrozenStore {
    fn payload(&self, span: Span) -> &[u8] {
        // Zero-length spans own no bytes (see `Heap::payload`).
        if span.len == 0 {
            return &[];
        }
        let (off, len) = (span.off as usize, span.len as usize);
        &self.chunks[span.chunk as usize][off..off + len]
    }

    fn deref_counter(&self) -> &AtomicU64 {
        &self.compare_derefs
    }
}

/// A dictionary entry: the shared 4-byte prefix inline beside the payload
/// handle, so searches run on prefixes and touch the heap only on ties.
/// Exactly 16 bytes — the plane's word.
#[derive(Clone, Copy, Debug)]
struct Entry {
    prefix: [u8; 4],
    span: Span,
}

impl Entry {
    fn new(span: Span, heap: &Heap) -> Entry {
        Entry {
            prefix: prefix4(heap.get(span)),
            span,
        }
    }

    /// Prefix-first compare against a needle; payload deref only on tie.
    #[inline]
    fn cmp_needle<S: Store>(&self, np: [u8; 4], needle: &[u8], store: &S) -> Ordering {
        match cmp_prefixed(self.prefix, self.span.len, np, needle.len() as u32) {
            PrefixCmp::Decided(o) => o,
            PrefixCmp::NeedPayload => store.tie_payload(self.span).cmp(needle),
        }
    }

    /// Prefix-first compare against another entry; payload derefs only on
    /// tie.
    #[inline]
    fn cmp_entry<S: Store>(&self, other: &Entry, store: &S) -> Ordering {
        match cmp_prefixed(self.prefix, self.span.len, other.prefix, other.span.len) {
            PrefixCmp::Decided(o) => o,
            PrefixCmp::NeedPayload => store
                .tie_payload(self.span)
                .cmp(store.tie_payload(other.span)),
        }
    }
}

/// An immutable, strictly-sorted, duplicate-free run of entries: the
/// frozen shape of the dictionary.
///
/// The type is the proof. Both mints establish the law (`build` sorts and
/// dedups; `merge` preserves it), the fields are private, and no method
/// mutates — every `Run` that can be named anywhere in the program is
/// sorted and unique. Runs are shared across observer frames behind
/// `Arc`; their immutability is what makes an old frame's continued view
/// of them sound.
pub struct Run {
    entries: Vec<Entry>,
}

impl Run {
    /// Mint a lawful run from arbitrary spans: sorts by payload bytes
    /// (prefix-first) and drops duplicates. The door where the invariant
    /// is established.
    pub fn build(spans: Vec<Span>, heap: &Heap) -> Run {
        let mut entries: Vec<Entry> = spans.into_iter().map(|s| Entry::new(s, heap)).collect();
        entries.sort_by(|a, b| a.cmp_entry(b, heap));
        entries.dedup_by(|a, b| a.cmp_entry(b, heap) == Ordering::Equal);
        Run { entries }
    }

    /// Mint from entries already sorted and unique (the delta drains
    /// through here). The precondition is the caller's law, re-checked in
    /// debug builds.
    fn from_sorted(entries: Vec<Entry>, heap: &Heap) -> Run {
        debug_assert!(
            entries
                .windows(2)
                .all(|w| w[0].cmp_entry(&w[1], heap) == Ordering::Less),
            "from_sorted precondition violated"
        );
        let _ = heap;
        Run { entries }
    }

    /// The merge: two lawful runs in, one lawful run plus the monotone
    /// position maps out. Borrows its inputs — runs are immutable and may
    /// be shared with older frames, which keep observing them unchanged.
    /// Payloads equal in both inputs collapse to one output entry; both
    /// remaps then point at it.
    pub fn merge(a: &Run, b: &Run, heap: &Heap) -> (Run, Remap, Remap) {
        let mut merged = Vec::with_capacity(a.entries.len() + b.entries.len());
        let mut remap_a = Vec::with_capacity(a.entries.len());
        let mut remap_b = Vec::with_capacity(b.entries.len());
        let (mut i, mut j) = (0, 0);
        while i < a.entries.len() && j < b.entries.len() {
            match a.entries[i].cmp_entry(&b.entries[j], heap) {
                Ordering::Less => {
                    remap_a.push(merged.len() as u32);
                    merged.push(a.entries[i]);
                    i += 1;
                }
                Ordering::Greater => {
                    remap_b.push(merged.len() as u32);
                    merged.push(b.entries[j]);
                    j += 1;
                }
                Ordering::Equal => {
                    remap_a.push(merged.len() as u32);
                    remap_b.push(merged.len() as u32);
                    merged.push(a.entries[i]);
                    i += 1;
                    j += 1;
                }
            }
        }
        for &e in &a.entries[i..] {
            remap_a.push(merged.len() as u32);
            merged.push(e);
        }
        for &e in &b.entries[j..] {
            remap_b.push(merged.len() as u32);
            merged.push(e);
        }
        (Run { entries: merged }, Remap(remap_a), Remap(remap_b))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Rank of `needle` within this run: `Ok(rank)` if present, `Err(rank
    /// it would take)` if absent. Binary search's precondition is the
    /// type's postcondition.
    fn search<S: Store>(&self, np: [u8; 4], needle: &[u8], store: &S) -> Result<usize, usize> {
        self.entries
            .binary_search_by(|e| e.cmp_needle(np, needle, store))
    }
}

/// The old-position -> new-position map a [`Run::merge`] emits for one of
/// its inputs: strictly monotone by construction, mintable only by a
/// merge. A `Remap` in hand is proof that applying it to a sorted
/// sequence of positions yields a sorted sequence.
pub struct Remap(Vec<u32>);

impl Remap {
    /// Where the entry that sat at `old` in the merge input sits now.
    pub fn apply(&self, old: u32) -> u32 {
        self.0[old as usize]
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The arena's epoch: advances exactly at [`Arena::seal`], which rides
/// commit boundaries. Codes mean something relative to an epoch; every
/// spend verifies the stamp.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Epoch(pub(super) u64);

impl Epoch {
    /// The raw counter, for display and diagnostics. Minting stays with
    /// the arena.
    #[inline]
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// The delta head: values interned since the last seal, in arrival order,
/// with a small sorted index for dedup and ordered queries. Tail code =
/// `sealed_len + arrival index` — arrival-stable, equality-exact, no
/// order meaning. Bounded by the commit batch (the seal drains it).
struct Delta {
    /// Arrival order; index = tail-code offset.
    arrivals: Vec<Entry>,
    /// Indices into `arrivals`, sorted by payload byte order.
    sorted: Vec<u32>,
}

impl Delta {
    fn new() -> Delta {
        Delta {
            arrivals: Vec::new(),
            sorted: Vec::new(),
        }
    }

    fn len(&self) -> usize {
        self.arrivals.len()
    }

    fn search<S: Store>(&self, np: [u8; 4], needle: &[u8], store: &S) -> Result<usize, usize> {
        self.sorted
            .binary_search_by(|&i| self.arrivals[i as usize].cmp_needle(np, needle, store))
    }

    fn entry_by_rank(&self, rank: usize) -> Entry {
        self.arrivals[self.sorted[rank] as usize]
    }
}

/// The epoch transition's artifact — the morphism between frames. Minted
/// only by [`Arena::seal`]; [`EpochRemap::apply`] restamps a code from
/// the old epoch into the new one.
///
/// - Over **sealed** codes it is strictly monotone (old sealed values
///   keep their relative order), represented compactly as the sorted new
///   ranks of the values the seal inserted — application is a binary
///   search, and sorted structures of sealed codes survive by one gather.
/// - Over **tail** codes it is the arrival -> new-rank permutation.
pub struct EpochRemap {
    from: Epoch,
    to: Epoch,
    /// Sealed length of the *from* epoch: the boundary between sealed and
    /// tail codes in the old code space.
    from_sealed_len: u32,
    /// New (post-seal) global ranks of the values the seal inserted,
    /// strictly ascending.
    inserted: Vec<u32>,
    /// Arrival index -> new sealed code, for the old tail codes.
    tail: Vec<u32>,
}

impl EpochRemap {
    /// The epoch this remap reads codes from.
    pub fn from_epoch(&self) -> Epoch {
        self.from
    }

    /// The epoch this remap restamps codes into.
    pub fn to_epoch(&self) -> Epoch {
        self.to
    }

    /// Restamp an old-epoch code into the new epoch.
    ///
    /// # Panics
    ///
    /// Panics if the stamp is not this remap's `from` epoch, or the code
    /// was not live in it.
    pub fn apply(&self, sc: StampedCode) -> StampedCode {
        assert_eq!(
            sc.epoch(),
            self.from,
            "remap {:?}->{:?} fed a code stamped {:?}",
            self.from,
            self.to,
            sc.epoch()
        );
        StampedCode::mint(self.apply_raw(sc.code()), self.to)
    }

    /// The raw morphism, for bulk gathers by epoch-stamped containers
    /// (which carry one stamp for all their codes and verify it once).
    pub(super) fn apply_raw(&self, code: Code) -> Code {
        let c = code.0;
        if c < self.from_sealed_len {
            // Old sealed rank r moves to the r-th position not occupied
            // by an inserted value: r + x where x is the fixpoint of
            // x = |{d in inserted : d < r + x + 1}|.
            let r = c as usize;
            let mut x = 0usize;
            loop {
                let nx = self.inserted.partition_point(|&d| (d as usize) < r + x + 1);
                if nx == x {
                    break;
                }
                x = nx;
            }
            Code((r + x) as u32)
        } else {
            let a = (c - self.from_sealed_len) as usize;
            assert!(
                a < self.tail.len(),
                "code {} was not live in epoch {:?}",
                c,
                self.from
            );
            Code(self.tail[a])
        }
    }

    /// Number of old tail codes this remap carries.
    pub fn tail_len(&self) -> usize {
        self.tail.len()
    }
}

/// The shared read core over any store: every code-consuming algorithm
/// lives here, used by both the live [`Frame`] and the pinned
/// [`Snapshot`].
struct View<'a, S: Store> {
    runs: &'a [Arc<Run>],
    sealed_len: usize,
    arrivals: &'a [Entry],
    sorted: &'a [u32],
    store: &'a S,
}

impl<'a, S: Store> View<'a, S> {
    fn len(&self) -> usize {
        self.sealed_len + self.arrivals.len()
    }

    /// The entry behind a live code (sealed: rank-select; tail: arrival).
    fn entry_of(&self, c: usize) -> Entry {
        if c < self.sealed_len {
            // Steady state after cascades collapse: one run, and a sealed
            // code is a literal index — the O(1) read the sealed scope
            // promises at rest.
            if self.runs.len() == 1 {
                return self.runs[0].entries[c];
            }
            self.select_sealed(c)
        } else {
            let a = c - self.sealed_len;
            assert!(
                a < self.arrivals.len(),
                "code {c} not live: view holds {}",
                self.len()
            );
            self.arrivals[a]
        }
    }

    fn resolve(&self, c: usize) -> &'a [u8] {
        self.store.payload(self.entry_of(c).span)
    }

    /// Semantic comparison of two live codes: rank order is byte order
    /// when both are sealed; any tail code involved goes prefix-first.
    fn cmp(&self, ca: usize, cb: usize) -> Ordering {
        assert!(
            ca < self.len(),
            "code {ca} not live: view holds {}",
            self.len()
        );
        assert!(
            cb < self.len(),
            "code {cb} not live: view holds {}",
            self.len()
        );
        if ca == cb {
            return Ordering::Equal;
        }
        if ca < self.sealed_len && cb < self.sealed_len {
            return ca.cmp(&cb);
        }
        let ea = self.entry_of(ca);
        let eb = self.entry_of(cb);
        ea.cmp_entry(&eb, self.store)
    }

    /// Global ordered rank of `value` across sealed and delta together:
    /// `Ok(rank)` if interned, `Err(rank it would take)` if not.
    fn rank(&self, value: &[u8]) -> Result<usize, usize> {
        let np = prefix4(value);
        let mut rank = 0usize;
        let mut found = false;
        for run in self.runs {
            match run.search(np, value, self.store) {
                Ok(pos) => {
                    rank += pos;
                    found = true;
                }
                Err(pos) => rank += pos,
            }
        }
        match self
            .sorted
            .binary_search_by(|&i| self.arrivals[i as usize].cmp_needle(np, value, self.store))
        {
            Ok(pos) => {
                rank += pos;
                found = true;
            }
            Err(pos) => rank += pos,
        }
        if found { Ok(rank) } else { Err(rank) }
    }

    /// The `k`-th smallest interned value across sealed and delta.
    fn select(&self, k: usize) -> &'a [u8] {
        assert!(
            k < self.len(),
            "select {k} out of range: view holds {}",
            self.len()
        );
        self.store.payload(self.select_global(k).span)
    }

    /// Select the sealed value of rank `k` across the disjoint runs: in
    /// exactly one run, an index `i` has `i` + (lower bounds elsewhere)
    /// equal to `k`; that predicate is monotone in `i` per run.
    fn select_sealed(&self, k: usize) -> Entry {
        debug_assert!(k < self.sealed_len);
        for (r, run) in self.runs.iter().enumerate() {
            if let Some(e) = self.select_in(run, r, k, false) {
                return e;
            }
        }
        unreachable!("sealed rank {k} not found: rank bookkeeping is broken");
    }

    /// Select rank `k` across runs and delta together.
    fn select_global(&self, k: usize) -> Entry {
        for (r, run) in self.runs.iter().enumerate() {
            if let Some(e) = self.select_in(run, r, k, true) {
                return e;
            }
        }
        // Not in any run: it is a delta value. Binary search the delta's
        // sorted view for the position whose global rank is k.
        let mut lo = 0usize;
        let mut hi = self.sorted.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            let e = self.entry_by_delta_rank(mid);
            let g = self.global_rank_of_delta_entry(e, mid);
            match g.cmp(&k) {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => return e,
            }
        }
        unreachable!("global rank {k} not found: rank bookkeeping is broken");
    }

    fn entry_by_delta_rank(&self, rank: usize) -> Entry {
        self.arrivals[self.sorted[rank] as usize]
    }

    /// Binary search run `r` for an index whose global rank equals `k`.
    /// `with_delta` includes the delta in the rank sum.
    fn select_in(&self, run: &Run, r: usize, k: usize, with_delta: bool) -> Option<Entry> {
        let (mut lo, mut hi) = (0usize, run.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            let e = run.entries[mid];
            let mut g = mid;
            for (i, other) in self.runs.iter().enumerate() {
                if i != r {
                    g += self.lower_bound_in(other, e);
                }
            }
            if with_delta {
                g += self.lower_bound_delta(e);
            }
            match g.cmp(&k) {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => return Some(e),
            }
        }
        None
    }

    /// Global rank of a delta entry at sorted position `pos`: `pos` plus
    /// lower bounds across every run.
    fn global_rank_of_delta_entry(&self, e: Entry, delta_pos: usize) -> usize {
        let mut g = delta_pos;
        for run in self.runs {
            g += self.lower_bound_in(run, e);
        }
        g
    }

    /// Number of entries in `run` strictly less than `e`.
    fn lower_bound_in(&self, run: &Run, e: Entry) -> usize {
        run.entries
            .partition_point(|x| x.cmp_entry(&e, self.store) == Ordering::Less)
    }

    /// Number of delta entries strictly less than `e`.
    fn lower_bound_delta(&self, e: Entry) -> usize {
        self.sorted.partition_point(|&i| {
            self.arrivals[i as usize].cmp_entry(&e, self.store) == Ordering::Less
        })
    }
}

/// A code admitted into a specific live [`Frame`]: the spendable witness.
/// Mintable only by [`Frame::admit`]; dies with its frame — any `intern`
/// or `seal` takes `&mut Arena` and retires the frame *and every witness
/// carrying its lifetime*.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FrameCode<'f> {
    code: Code,
    _frame: PhantomData<&'f ()>,
}

impl FrameCode<'_> {
    /// The raw identity (for packing results under the frame's epoch).
    #[inline]
    pub fn code(self) -> Code {
        self.code
    }
}

/// The live observer frame: a borrow of the arena's current state, and
/// the only place a code can be spent live. Valid for exactly one
/// quiescent stretch of one epoch — the borrow checker retires it at the
/// next mutation.
#[derive(Clone, Copy)]
pub struct Frame<'a> {
    runs: &'a [Arc<Run>],
    sealed_len: usize,
    arrivals: &'a [Entry],
    sorted: &'a [u32],
    heap: &'a Heap,
    epoch: Epoch,
}

impl<'a> Frame<'a> {
    fn view(&self) -> View<'a, Heap> {
        View {
            runs: self.runs,
            sealed_len: self.sealed_len,
            arrivals: self.arrivals,
            sorted: self.sorted,
            store: self.heap,
        }
    }

    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    pub fn len(&self) -> usize {
        self.sealed_len + self.arrivals.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Sealed prefix of the code space: codes `< sealed_len()` are dense
    /// byte-order ranks; codes `>= sealed_len()` are arrival-stable tail
    /// codes.
    pub fn sealed_len(&self) -> usize {
        self.sealed_len
    }

    /// Admit a stamped code into this frame, turning raw identity into a
    /// spendable witness.
    ///
    /// Validity is epoch equality **plus visibility**; for a live frame
    /// the visibility half is implied rather than checked: stamps are
    /// mintable only by this plane, every mint is in bounds when issued,
    /// the arena only grows within an epoch, and no transition
    /// (`&mut Arena`) can coexist with this frame. The bounds theorem is
    /// re-checked in debug builds. Returns `None` for a stale stamp: the
    /// code must cross through [`EpochRemap::apply`] instead.
    pub fn admit(&self, sc: StampedCode) -> Option<FrameCode<'a>> {
        if sc.epoch() == self.epoch {
            debug_assert!(
                (sc.code().raw() as usize) < self.len(),
                "minting discipline broken: in-epoch stamp not live in the frame"
            );
            Some(FrameCode {
                code: sc.code(),
                _frame: PhantomData,
            })
        } else {
            None
        }
    }

    /// Resolve an admitted code to its bytes.
    pub fn resolve(&self, fc: FrameCode<'a>) -> &'a [u8] {
        self.view().resolve(fc.code.0 as usize)
    }

    /// Semantic comparison of two admitted codes: integer compare when
    /// both sealed (rank order is byte order), prefix-first bytes
    /// otherwise.
    pub fn cmp_codes(&self, a: FrameCode<'a>, b: FrameCode<'a>) -> Ordering {
        self.view().cmp(a.code.0 as usize, b.code.0 as usize)
    }

    /// Global ordered rank of `value` across sealed and delta: `Ok(rank)`
    /// if interned, `Err(rank it would take)` if not. The order
    /// authority.
    pub fn rank(&self, value: &[u8]) -> Result<usize, usize> {
        self.view().rank(value)
    }

    /// The `k`-th smallest interned value (inverse of [`Frame::rank`]).
    ///
    /// # Panics
    ///
    /// Panics if `k >= len()`.
    pub fn select(&self, k: usize) -> &'a [u8] {
        self.view().select(k)
    }
}

/// The pinned observer frame: run references + a delta cut + frozen heap
/// chunks + the epoch — the ruling's "snapshot's dictionary", owned. It
/// answers identically forever while the writer interns and seals past
/// it, and it is `Send + Sync` (everything it holds is immutable).
///
/// Visibility is **not** implied by the epoch here: the delta cut is part
/// of the observer, so every spend verifies both the stamp and the cut,
/// exactly and loudly. (Deliberately not a lifetime witness: two owned
/// snapshots of different epochs can coexist with unifiable lifetimes,
/// and a compile-time brand that unifies across them would claim a safety
/// it cannot deliver.)
pub struct Snapshot {
    runs: Vec<Arc<Run>>,
    sealed_len: usize,
    arrivals: Vec<Entry>,
    sorted: Vec<u32>,
    store: FrozenStore,
    epoch: Epoch,
}

impl Snapshot {
    fn view(&self) -> View<'_, FrozenStore> {
        View {
            runs: &self.runs,
            sealed_len: self.sealed_len,
            arrivals: &self.arrivals,
            sorted: &self.sorted,
            store: &self.store,
        }
    }

    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    pub fn len(&self) -> usize {
        self.sealed_len + self.arrivals.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn sealed_len(&self) -> usize {
        self.sealed_len
    }

    /// Verify stamp + visibility against this snapshot: the epoch must
    /// match and the code must be within the snapshot's delta cut (a
    /// same-epoch code minted *after* the snapshot is beyond its view).
    fn check(&self, sc: StampedCode) -> usize {
        assert_eq!(
            sc.epoch(),
            self.epoch,
            "snapshot of epoch {:?} fed a code stamped {:?}",
            self.epoch,
            sc.epoch()
        );
        let c = sc.code().raw() as usize;
        assert!(
            c < self.len(),
            "code {c} is beyond this snapshot's cut ({} values)",
            self.len()
        );
        c
    }

    /// Resolve a stamped code to its bytes.
    ///
    /// # Panics
    ///
    /// Panics on a wrong-epoch stamp or a code beyond the snapshot's cut.
    pub fn resolve(&self, sc: StampedCode) -> &[u8] {
        let c = self.check(sc);
        self.view().resolve(c)
    }

    /// Semantic comparison of two stamped codes (see
    /// [`Frame::cmp_codes`]).
    ///
    /// # Panics
    ///
    /// Panics on wrong-epoch stamps or codes beyond the cut.
    pub fn cmp_codes(&self, a: StampedCode, b: StampedCode) -> Ordering {
        let (ca, cb) = (self.check(a), self.check(b));
        self.view().cmp(ca, cb)
    }

    /// Global ordered rank of `value` as of this snapshot.
    pub fn rank(&self, value: &[u8]) -> Result<usize, usize> {
        self.view().rank(value)
    }

    /// The `k`-th smallest value as of this snapshot.
    ///
    /// # Panics
    ///
    /// Panics if `k >= len()`.
    pub fn select(&self, k: usize) -> &[u8] {
        self.view().select(k)
    }
}

/// The shared, order-preserving interning arena: minting and transition
/// only. Reads happen through the observer frames — [`Arena::frame`] for
/// the live borrow, [`Arena::snapshot`] for the pinned owner. See the
/// module docs for the full type-C contract.
pub struct Arena {
    heap: Heap,
    runs: Vec<Arc<Run>>,
    /// Total sealed values (= sum of run lengths; runs are disjoint).
    sealed_len: usize,
    delta: Delta,
    epoch: Epoch,
}

impl Arena {
    pub fn new() -> Self {
        Arena {
            heap: Heap::new(),
            runs: Vec::new(),
            sealed_len: 0,
            delta: Delta::new(),
            epoch: Epoch(0),
        }
    }

    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// Total distinct values (sealed + delta).
    pub fn len(&self) -> usize {
        self.sealed_len + self.delta.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn sealed_len(&self) -> usize {
        self.sealed_len
    }

    /// Payload fetches forced by comparison ties so far (the
    /// deref-only-on-tie instrument; shared with all snapshots).
    pub fn compare_derefs(&self) -> u64 {
        self.heap.compare_derefs()
    }

    /// Open the live observer frame over the current state. Retired by
    /// the borrow checker at the next `intern`/`seal`/`snapshot`.
    pub fn frame(&self) -> Frame<'_> {
        Frame {
            runs: &self.runs,
            sealed_len: self.sealed_len,
            arrivals: &self.delta.arrivals,
            sorted: &self.delta.sorted,
            heap: &self.heap,
            epoch: self.epoch,
        }
    }

    /// Pin the current state as an owned snapshot: run references + the
    /// delta cut + frozen chunks + the epoch. Near-zero cost (Arc bumps,
    /// a bounded delta copy, and freezing the live chunk); the snapshot
    /// answers identically forever while this arena moves on.
    pub fn snapshot(&mut self) -> Snapshot {
        self.heap.freeze_live();
        Snapshot {
            runs: self.runs.clone(),
            sealed_len: self.sealed_len,
            arrivals: self.delta.arrivals.clone(),
            sorted: self.delta.sorted.clone(),
            store: FrozenStore {
                chunks: self.heap.frozen.clone(),
                compare_derefs: Arc::clone(&self.heap.compare_derefs),
            },
            epoch: self.epoch,
        }
    }

    /// Intern a byte-string, returning its identity stamped with the
    /// current epoch. A sealed hit returns the value's sealed code (its
    /// rank among sealed values); a delta hit returns its arrival-stable
    /// tail code; a novel value joins the delta and gets the next tail
    /// code. Stamps stay spendable until the next [`Arena::seal`].
    ///
    /// # Panics
    ///
    /// Panics at capacity (`u32::MAX` distinct values).
    pub fn intern(&mut self, value: &[u8]) -> StampedCode {
        assert!(
            self.len() < u32::MAX as usize,
            "arena is full: u32::MAX distinct values"
        );
        let np = prefix4(value);
        // Sealed lookup: global sealed rank accumulates across the
        // disjoint runs; an exact hit in one run plus lower bounds in the
        // rest is the rank.
        let mut rank = 0usize;
        let mut found = false;
        for run in &self.runs {
            match run.search(np, value, &self.heap) {
                Ok(pos) => {
                    rank += pos;
                    found = true;
                }
                Err(pos) => rank += pos,
            }
        }
        let code = if found {
            Code(rank as u32)
        } else {
            match self.delta.search(np, value, &self.heap) {
                Ok(pos) => {
                    let arrival = self.delta.sorted[pos];
                    Code((self.sealed_len + arrival as usize) as u32)
                }
                Err(pos) => {
                    let span = self.heap.push(value);
                    let entry = Entry::new(span, &self.heap);
                    let arrival = self.delta.arrivals.len() as u32;
                    self.delta.arrivals.push(entry);
                    self.delta.sorted.insert(pos, arrival);
                    Code((self.sealed_len + arrival as usize) as u32)
                }
            }
        };
        StampedCode::mint(code, self.epoch)
    }

    /// Seal the epoch: drain the delta into the runs (with geometric
    /// cascade merges — rank-invariant, since sealed codes rank over the
    /// union), advance the epoch, and mint the [`EpochRemap`] every held
    /// code crosses through. Rides commit boundaries.
    pub fn seal(&mut self) -> EpochRemap {
        let from = self.epoch;
        let from_sealed_len = self.sealed_len as u32;
        let delta_n = self.delta.len();

        // New global ranks of the delta values: old sealed rank +
        // position among the delta itself. Strictly ascending by
        // construction.
        let mut inserted = Vec::with_capacity(delta_n);
        // Arrival index -> new sealed code.
        let mut tail = vec![0u32; delta_n];
        for j in 0..delta_n {
            let entry = self.delta.entry_by_rank(j);
            let bytes = self.heap.get(entry.span);
            let np = entry.prefix;
            let mut sealed_rank = 0usize;
            for run in &self.runs {
                match run.search(np, bytes, &self.heap) {
                    // Delta values are disjoint from sealed by
                    // intern-time dedup; an exact hit would be a broken
                    // invariant.
                    Ok(_) => unreachable!("delta value already sealed: dedup invariant broken"),
                    Err(pos) => sealed_rank += pos,
                }
            }
            let new_rank = (sealed_rank + j) as u32;
            inserted.push(new_rank);
            tail[self.delta.sorted[j] as usize] = new_rank;
        }

        // Drain the delta into a lawful run (sorted + unique by the
        // delta's own dedup) and cascade geometrically. Cascades are
        // rank-invariant.
        let delta = std::mem::replace(&mut self.delta, Delta::new());
        if delta_n > 0 {
            let entries: Vec<Entry> = delta
                .sorted
                .iter()
                .map(|&i| delta.arrivals[i as usize])
                .collect();
            self.runs
                .push(Arc::new(Run::from_sorted(entries, &self.heap)));
            while self.runs.len() >= 2 {
                let last = self.runs[self.runs.len() - 1].len();
                let prev = self.runs[self.runs.len() - 2].len();
                if prev > 2 * last {
                    break;
                }
                let b = self.runs.pop().expect("len checked");
                let a = self.runs.pop().expect("len checked");
                let (merged, _, _) = Run::merge(&a, &b, &self.heap);
                self.runs.push(Arc::new(merged));
            }
            self.sealed_len += delta_n;
        }

        self.epoch = Epoch(self.epoch.0 + 1);
        EpochRemap {
            from,
            to: self.epoch,
            from_sealed_len,
            inserted,
            tail,
        }
    }
}

impl Default for Arena {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic PRNG (xorshift64*): seeded, reproducible, no clock.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    fn rand_value(rng: &mut Rng, alphabet: &[u8], max_len: usize) -> Vec<u8> {
        let len = rng.below(max_len + 1);
        (0..len)
            .map(|_| {
                if alphabet.is_empty() {
                    rng.next() as u8
                } else {
                    alphabet[rng.below(alphabet.len())]
                }
            })
            .collect()
    }

    /// In-plane stamp mint for law sweeps (tests are part of the plane's
    /// minting authority; production stamps come from intern/apply).
    fn stamp(c: usize, epoch: Epoch) -> StampedCode {
        StampedCode::mint(Code(c as u32), epoch)
    }

    // ------------------------------------------------------------------
    // Naive oracle: the type-C contract stated so simply it is obviously
    // correct. The arena must agree with it on every operation, every
    // epoch, and so must every snapshot, forever.
    // ------------------------------------------------------------------

    #[derive(Clone)]
    struct Naive {
        sealed: Vec<Vec<u8>>, // sorted, unique
        tail: Vec<Vec<u8>>,   // arrival order, unique, disjoint from sealed
        epoch: u64,
    }

    impl Naive {
        fn new() -> Naive {
            Naive {
                sealed: Vec::new(),
                tail: Vec::new(),
                epoch: 0,
            }
        }

        fn len(&self) -> usize {
            self.sealed.len() + self.tail.len()
        }

        fn intern(&mut self, b: &[u8]) -> u32 {
            if let Ok(i) = self.sealed.binary_search_by(|v| v.as_slice().cmp(b)) {
                return i as u32;
            }
            if let Some(i) = self.tail.iter().position(|v| v.as_slice() == b) {
                return (self.sealed.len() + i) as u32;
            }
            self.tail.push(b.to_vec());
            (self.sealed.len() + self.tail.len() - 1) as u32
        }

        fn resolve(&self, code: u32) -> &[u8] {
            let c = code as usize;
            if c < self.sealed.len() {
                &self.sealed[c]
            } else {
                &self.tail[c - self.sealed.len()]
            }
        }

        fn union_sorted(&self) -> Vec<Vec<u8>> {
            let mut all = self.sealed.clone();
            all.extend(self.tail.iter().cloned());
            all.sort();
            all
        }

        /// Seal, returning old-code -> new-code.
        fn seal(&mut self) -> Vec<u32> {
            let old: Vec<Vec<u8>> = self
                .sealed
                .iter()
                .chain(self.tail.iter())
                .cloned()
                .collect();
            let mut new_sealed = old.clone();
            new_sealed.sort();
            let remap: Vec<u32> = old
                .iter()
                .map(|v| {
                    new_sealed
                        .binary_search_by(|x| x.as_slice().cmp(v))
                        .expect("survives seal") as u32
                })
                .collect();
            self.sealed = new_sealed;
            self.tail.clear();
            self.epoch += 1;
            remap
        }
    }

    // ------------------------------------------------------------------
    // The laws, checked as full sweeps against the oracle.
    // ------------------------------------------------------------------

    fn check_laws(arena: &Arena, naive: &Naive) {
        let f = arena.frame();
        assert_eq!(f.len(), naive.len(), "cardinality diverged");
        assert_eq!(
            f.sealed_len(),
            naive.sealed.len(),
            "sealed boundary diverged"
        );
        assert_eq!(f.epoch().0, naive.epoch, "epoch diverged");
        // Every live code admits and resolves to the oracle's bytes
        // (dense over 0..len; sealed = sorted ranks, tail = arrivals).
        for c in 0..f.len() {
            let fc = f.admit(stamp(c, f.epoch())).expect("live stamp admits");
            assert_eq!(
                f.resolve(fc),
                naive.resolve(c as u32),
                "code {c} resolves differently"
            );
        }
        // Sealed codes are strictly byte-ordered.
        let mut prev: Option<&[u8]> = None;
        for c in 0..f.sealed_len() {
            let v = f.resolve(f.admit(stamp(c, f.epoch())).expect("sealed admits"));
            if let Some(p) = prev {
                assert!(p < v, "sealed order broken at {c}");
            }
            prev = Some(v);
        }
        // Global rank/select agree with the sorted union.
        let union = naive.union_sorted();
        for (k, v) in union.iter().enumerate() {
            assert_eq!(f.select(k), v.as_slice(), "select({k}) wrong");
            assert_eq!(f.rank(v), Ok(k), "rank of {v:?} wrong");
        }
        // cmp_codes is the byte order, over every live pair.
        for i in 0..f.len() {
            for j in 0..f.len() {
                let a = f.admit(stamp(i, f.epoch())).expect("admits");
                let b = f.admit(stamp(j, f.epoch())).expect("admits");
                assert_eq!(
                    f.cmp_codes(a, b),
                    naive.resolve(i as u32).cmp(naive.resolve(j as u32)),
                    "cmp_codes({i},{j}) diverged from byte order"
                );
            }
        }
    }

    /// Verify a pinned snapshot against a frozen copy of the oracle taken
    /// at the same moment — the "answers identically forever" law.
    fn check_snapshot(snap: &Snapshot, frozen: &Naive) {
        assert_eq!(snap.len(), frozen.len(), "snapshot cardinality drifted");
        assert_eq!(
            snap.sealed_len(),
            frozen.sealed.len(),
            "snapshot boundary drifted"
        );
        assert_eq!(snap.epoch().0, frozen.epoch, "snapshot epoch drifted");
        for c in 0..snap.len() {
            assert_eq!(
                snap.resolve(stamp(c, snap.epoch())),
                frozen.resolve(c as u32),
                "snapshot code {c} drifted"
            );
        }
        let union = frozen.union_sorted();
        for (k, v) in union.iter().enumerate() {
            assert_eq!(snap.select(k), v.as_slice(), "snapshot select({k}) drifted");
            assert_eq!(snap.rank(v), Ok(k), "snapshot rank drifted");
        }
        if snap.len() <= 64 {
            for i in 0..snap.len() {
                for j in 0..snap.len() {
                    assert_eq!(
                        snap.cmp_codes(stamp(i, snap.epoch()), stamp(j, snap.epoch())),
                        frozen.resolve(i as u32).cmp(frozen.resolve(j as u32)),
                        "snapshot cmp drifted"
                    );
                }
            }
        }
    }

    /// Drive an op sequence against the oracle with per-op law checks;
    /// full sweeps every `sweep_every` ops and at the end. Snapshots
    /// taken along the way are all re-verified at the end, after the
    /// writer has moved arbitrarily far past them.
    enum Op {
        Intern(Vec<u8>),
        Seal,
        Snapshot,
    }

    fn drive(ops: &[Op], sweep_every: usize) {
        let mut arena = Arena::new();
        let mut naive = Naive::new();
        let mut pinned: Vec<(Snapshot, Naive)> = Vec::new();
        for (i, op) in ops.iter().enumerate() {
            match op {
                Op::Intern(b) => {
                    let sc = arena.intern(b);
                    assert_eq!(sc.code().raw(), naive.intern(b), "op {i}: code diverged");
                    assert_eq!(sc.epoch(), arena.epoch(), "op {i}: stamp epoch wrong");
                    {
                        let f = arena.frame();
                        let fc = f.admit(sc).expect("fresh stamp admits");
                        assert_eq!(f.resolve(fc), b.as_slice(), "op {i}: round-trip");
                    }
                    // Dedup: immediate re-intern is a hit, no growth.
                    let n = arena.len();
                    assert_eq!(arena.intern(b), sc, "op {i}: dedup");
                    assert_eq!(arena.len(), n, "op {i}: dedup grew arena");
                }
                Op::Seal => {
                    // Capture every live code's bytes before the
                    // transition.
                    let from_epoch = arena.epoch();
                    let live: Vec<Vec<u8>> = {
                        let f = arena.frame();
                        (0..f.len())
                            .map(|c| {
                                f.resolve(f.admit(stamp(c, from_epoch)).expect("live"))
                                    .to_vec()
                            })
                            .collect()
                    };
                    let from_sealed = arena.sealed_len();
                    let remap = arena.seal();
                    let expect = naive.seal();
                    assert_eq!(remap.from_epoch(), from_epoch);
                    assert_eq!(remap.to_epoch(), arena.epoch());
                    // The remap law: every old code, sealed or tail,
                    // reads the same bytes through the door — and the
                    // door restamps it into the new epoch.
                    let f = arena.frame();
                    for (old, bytes) in live.iter().enumerate() {
                        let new = remap.apply(stamp(old, from_epoch));
                        assert_eq!(
                            new.code().raw(),
                            expect[old],
                            "op {i}: remap diverged at {old}"
                        );
                        assert_eq!(new.epoch(), arena.epoch(), "op {i}: restamp wrong");
                        let fc = f.admit(new).expect("restamped admits");
                        assert_eq!(
                            f.resolve(fc),
                            bytes.as_slice(),
                            "op {i}: code {old} lost its value crossing the seal"
                        );
                    }
                    // Strictly monotone over the old sealed range.
                    let mut prev = None;
                    for old in 0..from_sealed {
                        let new = remap.apply(stamp(old, from_epoch)).code().raw();
                        if let Some(p) = prev {
                            assert!(p < new, "op {i}: sealed remap not strictly monotone");
                        }
                        prev = Some(new);
                    }
                }
                Op::Snapshot => {
                    pinned.push((arena.snapshot(), naive.clone()));
                }
            }
            if i % sweep_every == 0 {
                check_laws(&arena, &naive);
            }
        }
        check_laws(&arena, &naive);
        // Every snapshot still answers exactly as the world stood when it
        // was pinned.
        for (snap, frozen) in &pinned {
            check_snapshot(snap, frozen);
        }
    }

    // ------------------------------------------------------------------
    // Exhaustive: every intern sequence of length 3 over a 13-value
    // universe, under every seal placement, laws checked after every op.
    // ------------------------------------------------------------------

    #[test]
    fn laws_exhaustive_small_universe_all_seal_placements() {
        let a = [0x00u8, 0x61, 0xff];
        let mut universe: Vec<Vec<u8>> = vec![vec![]];
        for &x in &a {
            universe.push(vec![x]);
            for &y in &a {
                universe.push(vec![x, y]);
            }
        }
        assert_eq!(universe.len(), 13);
        let n = universe.len();
        for i0 in 0..n {
            for i1 in 0..n {
                for i2 in 0..n {
                    for mask in 0..16u32 {
                        let mut ops = Vec::new();
                        for (slot, idx) in [i0, i1, i2].into_iter().enumerate() {
                            if mask & (1 << slot) != 0 {
                                ops.push(Op::Seal);
                            }
                            ops.push(Op::Intern(universe[idx].clone()));
                        }
                        if mask & 8 != 0 {
                            ops.push(Op::Seal);
                        }
                        drive(&ops, 1);
                    }
                }
            }
        }
    }

    /// The same exhaustive core with a snapshot pinned at every possible
    /// position, verified after the drive has moved past it.
    #[test]
    fn laws_exhaustive_snapshot_placements() {
        let universe: [&[u8]; 5] = [b"", b"\x00", b"a", b"ab", b"\xff"];
        let n = universe.len();
        for i0 in 0..n {
            for i1 in 0..n {
                for i2 in 0..n {
                    for seal_mask in 0..8u32 {
                        for snap_pos in 0..4usize {
                            let mut ops = Vec::new();
                            for (slot, idx) in [i0, i1, i2].into_iter().enumerate() {
                                if snap_pos == slot {
                                    ops.push(Op::Snapshot);
                                }
                                if seal_mask & (1 << slot) != 0 {
                                    ops.push(Op::Seal);
                                }
                                ops.push(Op::Intern(universe[idx].to_vec()));
                            }
                            if snap_pos == 3 {
                                ops.push(Op::Snapshot);
                            }
                            ops.push(Op::Seal);
                            drive(&ops, 2);
                        }
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Randomized differentials: interleaved interns, seals, and
    // snapshots; three alphabets; dup-heavy; multi-epoch.
    // ------------------------------------------------------------------

    #[test]
    fn laws_random_differential_multi_epoch() {
        for seed in 1u64..=9 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let alphabet: &[u8] = match seed % 3 {
                0 => &[0x00, 0x01],
                1 => b"abcdefghij",
                _ => &[],
            };
            let mut history: Vec<Vec<u8>> = Vec::new();
            let mut ops = Vec::new();
            for _ in 0..1200 {
                let roll = rng.below(100);
                if roll < 4 {
                    ops.push(Op::Seal);
                } else if roll < 6 {
                    ops.push(Op::Snapshot);
                } else if roll < 36 && !history.is_empty() {
                    ops.push(Op::Intern(history[rng.below(history.len())].clone()));
                } else {
                    let v = rand_value(&mut rng, alphabet, 24);
                    history.push(v.clone());
                    ops.push(Op::Intern(v));
                }
            }
            ops.push(Op::Seal);
            drive(&ops, 149);
        }
    }

    // ------------------------------------------------------------------
    // The reviewer's exploit, pinned: a stamped code held across a seal
    // is refused by the new frame; only the remap door readmits it.
    // ------------------------------------------------------------------

    #[test]
    fn stale_stamp_is_refused_not_misread() {
        let mut arena = Arena::new();
        let sc_b = arena.intern(b"b");
        let remap = arena.seal();
        // Post-seal: intern something smaller so the old code's rank is
        // genuinely wrong if smuggled.
        arena.intern(b"a");
        let f = arena.frame();
        assert!(
            f.admit(sc_b).is_none(),
            "stale stamp crossed a seal without the remap door"
        );
        let crossed = remap.apply(sc_b);
        let fc = f.admit(crossed).expect("remapped stamp admits");
        assert_eq!(f.resolve(fc), b"b");
    }

    #[test]
    #[should_panic(expected = "fed a code stamped")]
    fn remap_refuses_wrong_epoch_input() {
        let mut arena = Arena::new();
        arena.intern(b"x");
        let r1 = arena.seal();
        let sc_new = arena.intern(b"y"); // epoch 1
        r1.apply(sc_new); // r1 reads epoch-0 codes only
    }

    #[test]
    #[should_panic(expected = "fed a code stamped")]
    fn snapshot_refuses_wrong_epoch_stamp() {
        let mut arena = Arena::new();
        let sc = arena.intern(b"x");
        let _remap = arena.seal();
        let snap = arena.snapshot();
        snap.resolve(sc); // stamped epoch 0, snapshot is epoch 1
    }

    #[test]
    #[should_panic(expected = "beyond this snapshot's cut")]
    fn snapshot_refuses_codes_past_its_cut() {
        let mut arena = Arena::new();
        arena.intern(b"x");
        let snap = arena.snapshot();
        let later = arena.intern(b"y"); // same epoch, after the cut
        snap.resolve(later);
    }

    // ------------------------------------------------------------------
    // The same-epoch coherence law: observers of one epoch agree on every
    // code both can see, whatever their cuts.
    // ------------------------------------------------------------------

    #[test]
    fn same_epoch_observers_agree_on_shared_codes() {
        let mut arena = Arena::new();
        arena.intern(b"m");
        arena.seal();
        let a = arena.intern(b"zz");
        let early = arena.snapshot();
        let b = arena.intern(b"aa");
        let late = arena.snapshot();
        // Codes visible to both answer identically in both.
        assert_eq!(early.resolve(a), late.resolve(a));
        assert_eq!(early.resolve(a), b"zz");
        // The later observer sees more; the earlier refuses what it
        // cannot see (tested above); nothing shared ever disagrees.
        assert_eq!(late.resolve(b), b"aa");
        let f = arena.frame();
        assert_eq!(f.resolve(f.admit(a).expect("live")), b"zz");
    }

    // ------------------------------------------------------------------
    // The fixpoint contract: tail codes are arrival-stable and
    // equality-exact for the whole epoch, whatever is interned around
    // them.
    // ------------------------------------------------------------------

    #[test]
    fn tail_codes_are_arrival_stable_within_an_epoch() {
        let mut arena = Arena::new();
        arena.intern(b"m");
        arena.seal();
        let c_z = arena.intern(b"z");
        let c_a = arena.intern(b"a"); // smaller than everything sealed
        let c_q = arena.intern(b"q");
        // Interning smaller values did not move earlier stamps.
        assert_eq!(arena.intern(b"z"), c_z);
        assert_eq!(arena.intern(b"a"), c_a);
        assert_eq!(arena.intern(b"q"), c_q);
        // Tail codes are consecutive arrivals above the sealed range.
        assert_eq!(c_z.code().raw(), 1);
        assert_eq!(c_a.code().raw(), 2);
        assert_eq!(c_q.code().raw(), 3);
        // The order authority is rank(), not tail-code arithmetic.
        let f = arena.frame();
        assert_eq!(f.rank(b"a"), Ok(0));
        assert_eq!(f.rank(b"m"), Ok(1));
        assert_eq!(f.rank(b"q"), Ok(2));
        assert_eq!(f.rank(b"z"), Ok(3));
    }

    #[test]
    fn seal_remap_carries_sealed_and_tail_codes() {
        let mut arena = Arena::new();
        let mut held: Vec<(StampedCode, Vec<u8>)> = Vec::new();
        for v in [b"delta".as_slice(), b"alpha", b"omega"] {
            held.push((arena.intern(v), v.to_vec()));
        }
        let r1 = arena.seal();
        for (sc, _) in held.iter_mut() {
            *sc = r1.apply(*sc);
        }
        for v in [b"aaaa".as_slice(), b"zzzz"] {
            held.push((arena.intern(v), v.to_vec()));
        }
        let r2 = arena.seal();
        let f = arena.frame();
        for (sc, v) in &held {
            let crossed = r2.apply(*sc);
            let fc = f.admit(crossed).expect("crossed stamp admits");
            assert_eq!(f.resolve(fc), v.as_slice());
        }
        // Post-seal: dense byte order over all five.
        let all: Vec<&[u8]> = (0..f.len())
            .map(|c| f.resolve(f.admit(stamp(c, f.epoch())).expect("live")))
            .collect();
        let mut sorted = all.clone();
        sorted.sort();
        assert_eq!(all, sorted, "sealed codes are byte-ordered after seal");
    }

    // ------------------------------------------------------------------
    // The deref instrument: compares that differ in the first four bytes
    // never touch payloads; ties (including equality) do.
    // ------------------------------------------------------------------

    #[test]
    fn distinct_prefix_compares_never_deref() {
        let mut arena = Arena::new();
        for i in 0..2000u32 {
            let mut v = i.to_be_bytes().to_vec();
            v.extend_from_slice(b"-payload-tail");
            arena.intern(&v);
        }
        arena.seal();
        assert_eq!(
            arena.compare_derefs(),
            0,
            "a compare dereferenced payload despite distinct prefixes"
        );
        // Ties DO deref, and only ties: an exact-equality hit is the
        // ultimate tie (equality is only confirmable by payload), and
        // shared-prefix-different-payload is the other.
        let before = arena.compare_derefs();
        let mut v = 7u32.to_be_bytes().to_vec();
        v.extend_from_slice(b"-payload-tail");
        arena.intern(&v);
        assert!(
            arena.compare_derefs() > before,
            "equality tie never counted"
        );
        let before = arena.compare_derefs();
        arena.intern(b"same-prefix-AAAA");
        arena.intern(b"same-prefix-BBBB");
        assert!(
            arena.compare_derefs() > before,
            "shared-prefix tie never counted"
        );
    }

    // ------------------------------------------------------------------
    // Scale: multiple epochs, pathological insertion orders, 100k values,
    // held stamps crossed through every boundary, snapshots verified
    // after the writer has moved 90k+ values past them.
    // ------------------------------------------------------------------

    fn stress(values: Vec<Vec<u8>>, seal_every: usize) {
        let mut arena = Arena::new();
        let mut live: Vec<(StampedCode, usize)> = Vec::new();
        let mut pinned: Option<(Snapshot, Vec<Vec<u8>>)> = None;
        for (i, v) in values.iter().enumerate() {
            let sc = arena.intern(v);
            live.push((sc, i));
            if (i + 1) % seal_every == 0 {
                let remap = arena.seal();
                for (sc, _) in live.iter_mut() {
                    *sc = remap.apply(*sc);
                }
            }
            if i + 1 == seal_every / 2 {
                // Pin one early snapshot with its expected contents.
                let expect: Vec<Vec<u8>> = {
                    let f = arena.frame();
                    (0..f.len())
                        .map(|c| {
                            f.resolve(f.admit(stamp(c, f.epoch())).expect("live"))
                                .to_vec()
                        })
                        .collect()
                };
                pinned = Some((arena.snapshot(), expect));
            }
        }
        {
            let f = arena.frame();
            for (sc, i) in &live {
                let fc = f.admit(*sc).expect("held stamp is current");
                assert_eq!(
                    f.resolve(fc),
                    values[*i].as_slice(),
                    "stamp lost across epochs"
                );
            }
        }
        let final_remap = arena.seal();
        let f = arena.frame();
        for (sc, i) in live.iter_mut() {
            *sc = final_remap.apply(*sc);
            let fc = f.admit(*sc).expect("crossed stamp admits");
            assert_eq!(f.resolve(fc), values[*i].as_slice());
        }
        let mut expected = values;
        expected.sort();
        expected.dedup();
        assert_eq!(f.len(), expected.len());
        for (k, v) in expected.iter().enumerate() {
            let fc = f.admit(stamp(k, f.epoch())).expect("live");
            assert_eq!(f.resolve(fc), v.as_slice(), "rank {k} wrong at scale");
            assert_eq!(f.rank(v), Ok(k));
        }
        // The early snapshot still answers its pinned world exactly.
        if let Some((snap, expect)) = pinned {
            assert_eq!(snap.len(), expect.len(), "snapshot drifted at scale");
            for (c, v) in expect.iter().enumerate() {
                assert_eq!(snap.resolve(stamp(c, snap.epoch())), v.as_slice());
            }
        }
    }

    #[test]
    fn stress_ascending_100k_multi_epoch() {
        stress(
            (0u32..100_000).map(|i| i.to_be_bytes().to_vec()).collect(),
            9_973,
        );
    }

    #[test]
    fn stress_descending_100k_multi_epoch() {
        stress(
            (0u32..100_000)
                .rev()
                .map(|i| i.to_be_bytes().to_vec())
                .collect(),
            9_973,
        );
    }

    #[test]
    fn stress_random_dups_100k_multi_epoch() {
        let mut rng = Rng(0x5EED);
        stress(
            (0..100_000)
                .map(|_| (rng.next() % 60_000).to_be_bytes().to_vec())
                .collect(),
            7_919,
        );
    }

    // ------------------------------------------------------------------
    // Contract edges.
    // ------------------------------------------------------------------

    #[test]
    fn snapshot_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Snapshot>();
    }

    #[test]
    fn empty_seal_advances_epoch_and_is_identity() {
        let mut arena = Arena::new();
        let sc = arena.intern(b"x");
        let r1 = arena.seal();
        assert_eq!(r1.tail_len(), 1);
        let crossed = r1.apply(sc);
        let r2 = arena.seal();
        assert_eq!(arena.epoch(), Epoch(2));
        assert_eq!(r2.tail_len(), 0);
        let twice = r2.apply(crossed);
        assert_eq!(
            twice.code().raw(),
            crossed.code().raw(),
            "empty seal moved a code"
        );
        let f = arena.frame();
        assert_eq!(f.resolve(f.admit(twice).expect("admits")), b"x");
    }

    #[test]
    fn empty_string_is_a_value_across_epochs() {
        let mut arena = Arena::new();
        let sc = arena.intern(b"");
        assert_eq!(sc.code().raw(), 0);
        let remap = arena.seal();
        let crossed = remap.apply(sc);
        assert_eq!(crossed.code().raw(), 0);
        let f = arena.frame();
        assert_eq!(f.resolve(f.admit(crossed).expect("admits")), b"");
    }

    #[test]
    fn values_around_chunk_boundaries_round_trip() {
        let mut arena = Arena::new();
        let lens = [
            0usize,
            1,
            3,
            4,
            5,
            CHUNK_SIZE - 1,
            CHUNK_SIZE,
            CHUNK_SIZE + 1,
            3 * CHUNK_SIZE + 17,
        ];
        let mut held = Vec::new();
        for len in lens {
            let v: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            held.push((arena.intern(&v), v));
        }
        // Fill across many shared chunks too.
        for i in 0..40_000u32 {
            arena.intern(format!("filler-{i}").as_bytes());
        }
        {
            let f = arena.frame();
            for (sc, v) in &held {
                assert_eq!(f.resolve(f.admit(*sc).expect("live")), v.as_slice());
            }
        }
        let remap = arena.seal();
        let f = arena.frame();
        for (sc, v) in &held {
            let fc = f.admit(remap.apply(*sc)).expect("crossed");
            assert_eq!(f.resolve(fc), v.as_slice());
        }
    }

    #[test]
    fn snapshot_survives_writer_progress_and_chunk_freezes() {
        let mut arena = Arena::new();
        for i in 0..5_000u32 {
            arena.intern(format!("v-{i:05}").as_bytes());
        }
        arena.seal();
        arena.intern(b"tail-one");
        let snap = arena.snapshot();
        let world: Vec<Vec<u8>> = (0..snap.len())
            .map(|c| snap.resolve(stamp(c, snap.epoch())).to_vec())
            .collect();
        // Writer moves far past the snapshot: new values, seals, chunk
        // rollovers, cascades.
        for round in 0..3 {
            for i in 0..5_000u32 {
                arena.intern(format!("post-{round}-{i}").as_bytes());
            }
            arena.seal();
        }
        for (c, v) in world.iter().enumerate() {
            assert_eq!(
                snap.resolve(stamp(c, snap.epoch())),
                v.as_slice(),
                "snapshot drifted"
            );
        }
    }

    #[test]
    #[should_panic(expected = "not live")]
    fn forged_in_epoch_stamp_beyond_len_panics() {
        let mut arena = Arena::new();
        arena.intern(b"x");
        let f = arena.frame();
        // A forged in-epoch stamp beyond len violates the minting
        // discipline; the debug bound catches it at admit, the view's
        // liveness assert in release.
        let forged = stamp(7, f.epoch());
        if let Some(fc) = f.admit(forged) {
            f.resolve(fc);
        } else {
            panic!("not live (refused at admit)");
        }
    }

    #[test]
    #[should_panic(expected = "out of range")]
    fn select_out_of_range_panics() {
        let arena = Arena::new();
        arena.frame().select(0);
    }
}
