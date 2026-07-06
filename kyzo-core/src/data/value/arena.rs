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
//! keeps all three for every observer that exists by scoping them to
//! **epochs**:
//!
//! - The **sealed** dictionary (a list of immutable sorted [`Run`]s over one
//!   append-only [`Heap`]) carries dense, byte-ordered codes `[0,
//!   sealed_len)` — a sealed code *is* the value's rank among sealed
//!   values, and stays so for the whole epoch.
//! - The **delta head** holds values interned since the last seal, with
//!   arrival-stable **tail codes** `[sealed_len, sealed_len + delta_len)`:
//!   exact equality and hash (the fixpoint currency), no order meaning.
//! - [`Arena::seal`] is the transition, ridden at commit boundaries: the
//!   delta drains into the runs, the epoch advances, and the caller
//!   receives an [`EpochRemap`] — monotone over sealed codes, a permutation
//!   over tail codes — the one door through which held codes cross.
//!
//! ## The transition theorem, held by types
//!
//! 1. **Every observable value satisfies its law.** [`Run`]'s only mints
//!    are [`Run::build`] (sorts + dedups: establishes it) and
//!    [`Run::merge`] (preserves it); a [`Remap`] is mintable only by a
//!    merge and is monotone by construction; an [`EpochRemap`] is mintable
//!    only by [`Arena::seal`]. An unsorted run or a non-monotone remap
//!    cannot be written down.
//! 2. **No observer exists during a transition.** `merge` consumes its
//!    inputs; `seal` holds `&mut self`. While the dictionary is between
//!    shapes the old shapes are owned by the transition — the borrow
//!    checker forbids any alias that could ask a question mid-flight.
//!
//! Cascading run merges inside a seal never change codes at all: a sealed
//! code is a rank over the *union* of runs, and reorganizing which run
//! holds a value does not move the union. Only the delta's arrival changes
//! ranks, which is exactly what the [`EpochRemap`] describes.
//!
//! Comparison discipline: run entries carry the shared 4-byte prefix
//! ([`super::prefix`]); every search decides on prefixes wherever they are
//! conclusive and dereferences payload bytes only on the one tie path,
//! which increments the heap's deref counter — the DoD's
//! "dereferences-only-on-tie" is measured, not asserted.

use std::cmp::Ordering;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrd};

use super::code::Code;
use super::prefix::{PrefixCmp, cmp_prefixed, prefix4};

/// Append-only payload storage. A [`Span`] handed out is valid for the
/// heap's whole life: bytes are never moved, mutated, or removed, so spans
/// are stable identities even while sorted shapes above are torn down and
/// rebuilt — transitions shuffle *handles*, never payloads.
pub struct Heap {
    bytes: Vec<u8>,
    /// Payload fetches forced by comparison ties (equal prefixes, both
    /// payloads longer than the prefix). The instrument behind the
    /// "deref only on tie" proof.
    compare_derefs: AtomicU64,
}

/// A byte-string's location in a [`Heap`]. Only [`Heap::push`] mints one.
#[derive(Clone, Copy, Debug)]
pub struct Span {
    off: u32,
    len: u32,
}

impl Heap {
    pub fn new() -> Self {
        Heap {
            bytes: Vec::new(),
            compare_derefs: AtomicU64::new(0),
        }
    }

    /// Store a byte-string, returning its permanent handle.
    ///
    /// # Panics
    ///
    /// Panics if total payload would exceed the `u32` span space.
    pub fn push(&mut self, value: &[u8]) -> Span {
        let off = self.bytes.len();
        assert!(
            off + value.len() <= u32::MAX as usize,
            "heap exceeds u32 span space"
        );
        self.bytes.extend_from_slice(value);
        Span {
            off: off as u32,
            len: value.len() as u32,
        }
    }

    pub fn get(&self, span: Span) -> &[u8] {
        &self.bytes[span.off as usize..span.off as usize + span.len as usize]
    }

    /// Payload fetch on the comparison tie path — counted.
    #[inline]
    fn tie_payload(&self, span: Span) -> &[u8] {
        self.compare_derefs.fetch_add(1, AtomicOrd::Relaxed);
        self.get(span)
    }

    /// Total payload fetches forced by comparison ties so far.
    pub fn compare_derefs(&self) -> u64 {
        self.compare_derefs.load(AtomicOrd::Relaxed)
    }
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

/// A dictionary entry: the shared 4-byte prefix inline beside the payload
/// handle, so searches run on prefixes and touch the heap only on ties.
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
    fn cmp_needle(&self, np: [u8; 4], needle: &[u8], heap: &Heap) -> Ordering {
        match cmp_prefixed(self.prefix, self.span.len, np, needle.len() as u32) {
            PrefixCmp::Decided(o) => o,
            PrefixCmp::NeedPayload => heap.tie_payload(self.span).cmp(needle),
        }
    }

    /// Prefix-first compare against another entry; payload derefs only on tie.
    #[inline]
    fn cmp_entry(&self, other: &Entry, heap: &Heap) -> Ordering {
        match cmp_prefixed(self.prefix, self.span.len, other.prefix, other.span.len) {
            PrefixCmp::Decided(o) => o,
            PrefixCmp::NeedPayload => heap
                .tie_payload(self.span)
                .cmp(heap.tie_payload(other.span)),
        }
    }
}

/// An immutable, strictly-sorted, duplicate-free run of entries: the frozen
/// shape of the dictionary.
///
/// The type is the proof. Both mints establish the law (`build` sorts and
/// dedups; `merge` consumes two lawful runs and preserves it), the fields
/// are private, and no method mutates — every `Run` that can be named
/// anywhere in the program is sorted and unique. The unsorted intermediate
/// inside a mint is a local of the constructor: unobservable by ownership.
pub struct Run {
    entries: Vec<Entry>,
}

impl Run {
    /// Mint a lawful run from arbitrary spans: sorts by payload bytes
    /// (prefix-first) and drops duplicates. The door where the invariant is
    /// established.
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

    /// The transition: consume two lawful runs, emit one lawful run plus
    /// the monotone position maps for each input. While this executes the
    /// inputs are owned here and the output is a local — no alias can
    /// observe the dictionary between shapes. Payloads equal in both inputs
    /// collapse to one output entry; both remaps then point at it.
    pub fn merge(a: Run, b: Run, heap: &Heap) -> (Run, Remap, Remap) {
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
    fn search(&self, np: [u8; 4], needle: &[u8], heap: &Heap) -> Result<usize, usize> {
        self.entries
            .binary_search_by(|e| e.cmp_needle(np, needle, heap))
    }
}

/// The old-position -> new-position map a [`Run::merge`] emits for one of
/// its inputs: strictly monotone by construction (a merge walks its inputs
/// in order and output positions only grow), mintable only by a merge. A
/// `Remap` in hand is proof that applying it to a sorted sequence of
/// positions yields a sorted sequence.
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
/// commit boundaries. Codes are meaningful relative to an epoch; containers
/// that persist codes carry the stamp.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Epoch(pub(crate) u64);

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
/// `sealed_len + arrival index` — arrival-stable, equality-exact, no order
/// meaning.
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

    /// Rank of `needle` among delta values: `Ok(sorted position)` /
    /// `Err(insertion position)`.
    fn search(&self, np: [u8; 4], needle: &[u8], heap: &Heap) -> Result<usize, usize> {
        self.sorted
            .binary_search_by(|&i| self.arrivals[i as usize].cmp_needle(np, needle, heap))
    }

    fn entry_by_rank(&self, rank: usize) -> Entry {
        self.arrivals[self.sorted[rank] as usize]
    }
}

/// The epoch transition's artifact: how every code of the previous epoch
/// reads in the new one. Mintable only by [`Arena::seal`].
///
/// - Over **sealed** codes it is strictly monotone (old sealed values keep
///   their relative order), represented compactly as the sorted new ranks
///   of the values the seal inserted — application is a binary search, and
///   sorted structures of sealed codes survive by one gather.
/// - Over **tail** codes it is the arrival -> new-rank permutation.
pub struct EpochRemap {
    pub from: Epoch,
    pub to: Epoch,
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
    /// Read an old-epoch code in the new epoch.
    ///
    /// # Panics
    ///
    /// Panics if `code` was not live in the *from* epoch.
    pub fn apply(&self, code: Code) -> Code {
        let c = code.0;
        if c < self.from_sealed_len {
            // Old sealed rank r moves to the r-th position not occupied by
            // an inserted value: r + x where x is the fixpoint of
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

/// The shared, order-preserving interning arena: immutable sorted runs plus
/// a delta head over one append-only heap, with epoch transitions at
/// [`Arena::seal`]. See the module docs for the full type-C contract.
pub struct Arena {
    heap: Heap,
    runs: Vec<Run>,
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

    /// Total distinct values (sealed + delta). Live codes are exactly
    /// `0..len()`.
    pub fn len(&self) -> usize {
        self.sealed_len + self.delta.len()
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

    /// Payload fetches forced by comparison ties so far (the
    /// deref-only-on-tie instrument).
    pub fn compare_derefs(&self) -> u64 {
        self.heap.compare_derefs()
    }

    /// Intern a byte-string. A sealed hit returns the value's sealed code
    /// (its rank among sealed values); a delta hit returns its
    /// arrival-stable tail code; a novel value joins the delta and gets the
    /// next tail code. Codes are stable until the next [`Arena::seal`].
    ///
    /// # Panics
    ///
    /// Panics at capacity: `u32::MAX` distinct values or payload bytes.
    pub fn intern(&mut self, value: &[u8]) -> Code {
        assert!(
            self.len() < u32::MAX as usize,
            "arena is full: u32::MAX distinct values"
        );
        let np = prefix4(value);
        // Sealed lookup: global sealed rank accumulates across the disjoint
        // runs; an exact hit in one run plus lower bounds in the rest is
        // the rank.
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
        if found {
            return Code(rank as u32);
        }
        // Delta lookup.
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
    }

    /// Resolve a live code to its bytes. Sealed codes select by rank across
    /// the runs; tail codes index the delta's arrival list.
    ///
    /// # Panics
    ///
    /// Panics if `code` is not live in the current epoch.
    pub fn resolve(&self, code: Code) -> &[u8] {
        let c = code.0 as usize;
        if c < self.sealed_len {
            self.heap.get(self.select_sealed(c).span)
        } else {
            let a = c - self.sealed_len;
            assert!(
                a < self.delta.len(),
                "code {c} not live: arena holds {}",
                self.len()
            );
            self.heap.get(self.delta.arrivals[a].span)
        }
    }

    /// Global ordered rank of `value` across sealed and delta together:
    /// `Ok(rank)` if interned, `Err(rank it would take)` if not. The order
    /// authority for order-sensitive operations.
    pub fn rank(&self, value: &[u8]) -> Result<usize, usize> {
        let np = prefix4(value);
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
        match self.delta.search(np, value, &self.heap) {
            Ok(pos) => {
                rank += pos;
                found = true;
            }
            Err(pos) => rank += pos,
        }
        if found { Ok(rank) } else { Err(rank) }
    }

    /// The `k`-th smallest interned value across sealed and delta together
    /// (the inverse of [`Arena::rank`] over interned values).
    ///
    /// # Panics
    ///
    /// Panics if `k >= len()`.
    pub fn select(&self, k: usize) -> &[u8] {
        assert!(
            k < self.len(),
            "select {k} out of range: arena holds {}",
            self.len()
        );
        self.heap.get(self.select_global(k).span)
    }

    /// Seal the epoch: drain the delta into the runs (with geometric
    /// cascade merges — rank-invariant, since sealed codes rank over the
    /// union), advance the epoch, and mint the [`EpochRemap`] every held
    /// code crosses through. Rides commit boundaries.
    pub fn seal(&mut self) -> EpochRemap {
        let from = self.epoch;
        let from_sealed_len = self.sealed_len as u32;
        let delta_n = self.delta.len();

        // New global ranks of the delta values: old sealed rank + position
        // among the delta itself. Strictly ascending by construction.
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
                    // Delta values are disjoint from sealed by intern-time
                    // dedup; an exact hit would be a broken invariant.
                    Ok(_) => unreachable!("delta value already sealed: dedup invariant broken"),
                    Err(pos) => sealed_rank += pos,
                }
            }
            let new_rank = (sealed_rank + j) as u32;
            inserted.push(new_rank);
            tail[self.delta.sorted[j] as usize] = new_rank;
        }

        // Drain the delta into a lawful run (sorted + unique by the delta's
        // own dedup) and cascade geometrically. Cascades are rank-invariant.
        let delta = std::mem::replace(&mut self.delta, Delta::new());
        if delta_n > 0 {
            let entries: Vec<Entry> = delta
                .sorted
                .iter()
                .map(|&i| delta.arrivals[i as usize])
                .collect();
            self.runs.push(Run::from_sorted(entries, &self.heap));
            while self.runs.len() >= 2 {
                let last = self.runs[self.runs.len() - 1].len();
                let prev = self.runs[self.runs.len() - 2].len();
                if prev > 2 * last {
                    break;
                }
                let b = self.runs.pop().expect("len checked");
                let a = self.runs.pop().expect("len checked");
                let (merged, _, _) = Run::merge(a, b, &self.heap);
                self.runs.push(merged);
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
        let mut hi = self.delta.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            let e = self.delta.entry_by_rank(mid);
            let g = self.global_rank_of_delta_entry(e, mid);
            match g.cmp(&k) {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => return e,
            }
        }
        unreachable!("global rank {k} not found: rank bookkeeping is broken");
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
        for run in &self.runs {
            g += self.lower_bound_in(run, e);
        }
        g
    }

    /// Number of entries in `run` strictly less than `e`.
    fn lower_bound_in(&self, run: &Run, e: Entry) -> usize {
        run.entries
            .partition_point(|x| x.cmp_entry(&e, &self.heap) == Ordering::Less)
    }

    /// Number of delta entries strictly less than `e`.
    fn lower_bound_delta(&self, e: Entry) -> usize {
        self.delta.sorted.partition_point(|&i| {
            self.delta.arrivals[i as usize].cmp_entry(&e, &self.heap) == Ordering::Less
        })
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

    // ------------------------------------------------------------------
    // Naive oracle: the type-C contract stated so simply it is obviously
    // correct. The arena must agree with it on every operation, every
    // epoch.
    // ------------------------------------------------------------------

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
        assert_eq!(arena.len(), naive.len(), "cardinality diverged");
        assert_eq!(
            arena.sealed_len(),
            naive.sealed.len(),
            "sealed boundary diverged"
        );
        assert_eq!(arena.epoch().0, naive.epoch, "epoch diverged");
        // Every live code resolves to the oracle's bytes (dense over
        // 0..len; sealed = sorted ranks, tail = arrivals).
        for c in 0..arena.len() {
            assert_eq!(
                arena.resolve(Code(c as u32)),
                naive.resolve(c as u32),
                "code {c} resolves differently"
            );
        }
        // Sealed codes are strictly byte-ordered.
        let mut prev: Option<&[u8]> = None;
        for c in 0..arena.sealed_len() {
            let v = arena.resolve(Code(c as u32));
            if let Some(p) = prev {
                assert!(p < v, "sealed order broken at {c}");
            }
            prev = Some(v);
        }
        // Global rank/select agree with the sorted union.
        let union = naive.union_sorted();
        for (k, v) in union.iter().enumerate() {
            assert_eq!(arena.select(k), v.as_slice(), "select({k}) wrong");
            assert_eq!(arena.rank(v), Ok(k), "rank of {v:?} wrong");
        }
    }

    /// Drive an op sequence against the oracle with per-op law checks;
    /// full sweeps every `sweep_every` ops and at the end.
    enum Op {
        Intern(Vec<u8>),
        Seal,
    }

    fn drive(ops: &[Op], sweep_every: usize) {
        let mut arena = Arena::new();
        let mut naive = Naive::new();
        for (i, op) in ops.iter().enumerate() {
            match op {
                Op::Intern(b) => {
                    let code = arena.intern(b);
                    assert_eq!(code.0, naive.intern(b), "op {i}: code diverged");
                    assert_eq!(arena.resolve(code), b.as_slice(), "op {i}: round-trip");
                    // Dedup: immediate re-intern is a hit, no growth.
                    let n = arena.len();
                    assert_eq!(arena.intern(b), code, "op {i}: dedup");
                    assert_eq!(arena.len(), n, "op {i}: dedup grew arena");
                }
                Op::Seal => {
                    // Capture every live code's bytes before the transition.
                    let live: Vec<Vec<u8>> = (0..arena.len())
                        .map(|c| arena.resolve(Code(c as u32)).to_vec())
                        .collect();
                    let remap = arena.seal();
                    let expect = naive.seal();
                    assert_eq!(remap.from.0 + 1, remap.to.0);
                    assert_eq!(arena.epoch(), remap.to);
                    // The remap law: every old code, sealed or tail, reads
                    // the same bytes through the door.
                    for (old, bytes) in live.iter().enumerate() {
                        let new = remap.apply(Code(old as u32));
                        assert_eq!(new.0, expect[old], "op {i}: remap diverged at {old}");
                        assert_eq!(
                            arena.resolve(new),
                            bytes.as_slice(),
                            "op {i}: code {old} lost its value crossing the seal"
                        );
                    }
                    // Strictly monotone over the old sealed range.
                    let mut prev = None;
                    for old in 0..remap.from_sealed_len {
                        let new = remap.apply(Code(old)).0;
                        if let Some(p) = prev {
                            assert!(p < new, "op {i}: sealed remap not strictly monotone");
                        }
                        prev = Some(new);
                    }
                }
            }
            if i % sweep_every == 0 {
                check_laws(&arena, &naive);
            }
        }
        check_laws(&arena, &naive);
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

    // ------------------------------------------------------------------
    // Randomized differentials: interleaved interns and seals, three
    // alphabets, dup-heavy, multi-epoch.
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
                } else if roll < 34 && !history.is_empty() {
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
    // The fixpoint contract: tail codes are arrival-stable and
    // equality-exact for the whole epoch, whatever is interned around
    // them.
    // ------------------------------------------------------------------

    #[test]
    fn tail_codes_are_arrival_stable_within_an_epoch() {
        let mut arena = Arena::new();
        arena.intern(b"m");
        let remap = arena.seal();
        assert_eq!(remap.to, arena.epoch());
        let c_z = arena.intern(b"z");
        let c_a = arena.intern(b"a"); // smaller than everything sealed
        let c_q = arena.intern(b"q");
        // Interning smaller values did not move earlier tail codes.
        assert_eq!(arena.intern(b"z"), c_z);
        assert_eq!(arena.intern(b"a"), c_a);
        assert_eq!(arena.intern(b"q"), c_q);
        // Tail codes are consecutive arrivals above the sealed range.
        assert_eq!(c_z.0, 1);
        assert_eq!(c_a.0, 2);
        assert_eq!(c_q.0, 3);
        // The order authority is rank(), not tail-code cmp.
        assert_eq!(arena.rank(b"a"), Ok(0));
        assert_eq!(arena.rank(b"m"), Ok(1));
        assert_eq!(arena.rank(b"q"), Ok(2));
        assert_eq!(arena.rank(b"z"), Ok(3));
    }

    #[test]
    fn seal_remap_carries_sealed_and_tail_codes() {
        let mut arena = Arena::new();
        let mut held: Vec<(Code, Vec<u8>)> = Vec::new();
        for v in [b"delta".as_slice(), b"alpha", b"omega"] {
            held.push((arena.intern(v), v.to_vec()));
        }
        let r1 = arena.seal();
        for (c, _) in held.iter_mut() {
            *c = r1.apply(*c);
        }
        for v in [b"aaaa".as_slice(), b"zzzz"] {
            held.push((arena.intern(v), v.to_vec()));
        }
        let r2 = arena.seal();
        for (c, v) in &held {
            assert_eq!(arena.resolve(r2.apply(*c)), v.as_slice());
        }
        // Post-seal: dense byte order over all five.
        let all: Vec<&[u8]> = (0..arena.len())
            .map(|c| arena.resolve(Code(c as u32)))
            .collect();
        let mut sorted = all.clone();
        sorted.sort();
        assert_eq!(all, sorted, "sealed codes are byte-ordered after seal");
    }

    // ------------------------------------------------------------------
    // The deref instrument: compares that differ in the first four bytes
    // never touch payloads.
    // ------------------------------------------------------------------

    #[test]
    fn distinct_prefix_compares_never_deref() {
        let mut arena = Arena::new();
        // Phase 1: 2000 novel values, all distinct within the first 4
        // bytes, all longer than the prefix. Every compare on every path
        // (delta searches, seal rank computations, cascade merges) is
        // between values with distinct prefixes, so none may fetch a
        // payload.
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
        // Phase 2: ties DO deref, and only ties. An exact-equality hit is
        // the ultimate tie — equality can only be confirmed by payload —
        // and shared-prefix-different-payload compares are the other tie.
        let before = arena.compare_derefs();
        let mut v = 7u32.to_be_bytes().to_vec();
        v.extend_from_slice(b"-payload-tail");
        arena.intern(&v); // sealed dedup hit: prefix tie, payload confirms
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
    // held codes gathered across every boundary.
    // ------------------------------------------------------------------

    fn stress(values: Vec<Vec<u8>>, seal_every: usize) {
        let mut arena = Arena::new();
        let mut live: Vec<(Code, usize)> = Vec::new();
        for (i, v) in values.iter().enumerate() {
            let c = arena.intern(v);
            assert_eq!(arena.resolve(c), v.as_slice());
            live.push((c, i));
            if (i + 1) % seal_every == 0 {
                let remap = arena.seal();
                for (c, _) in live.iter_mut() {
                    *c = remap.apply(*c);
                }
            }
        }
        for (c, i) in &live {
            assert_eq!(
                arena.resolve(*c),
                values[*i].as_slice(),
                "code lost across epochs"
            );
        }
        let final_remap = arena.seal();
        for (c, i) in live.iter_mut() {
            *c = final_remap.apply(*c);
            assert_eq!(arena.resolve(*c), values[*i].as_slice());
        }
        let mut expected = values;
        expected.sort();
        expected.dedup();
        assert_eq!(arena.len(), expected.len());
        for (k, v) in expected.iter().enumerate() {
            assert_eq!(
                arena.resolve(Code(k as u32)),
                v.as_slice(),
                "rank {k} wrong at scale"
            );
            assert_eq!(arena.rank(v), Ok(k));
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
    fn empty_seal_advances_epoch_and_is_identity() {
        let mut arena = Arena::new();
        arena.intern(b"x");
        let r1 = arena.seal();
        assert_eq!(r1.tail_len(), 1);
        let r2 = arena.seal();
        assert_eq!(arena.epoch(), Epoch(2));
        assert_eq!(r2.tail_len(), 0);
        assert_eq!(r2.apply(Code(0)), Code(0));
        assert_eq!(arena.resolve(Code(0)), b"x");
    }

    #[test]
    fn empty_string_is_a_value_across_epochs() {
        let mut arena = Arena::new();
        let c = arena.intern(b"");
        assert_eq!(c, Code(0));
        let remap = arena.seal();
        assert_eq!(remap.apply(c), Code(0));
        assert_eq!(arena.resolve(Code(0)), b"");
        assert_eq!(arena.intern(b""), Code(0));
    }

    #[test]
    fn long_values_round_trip() {
        let mut arena = Arena::new();
        let lens = [0usize, 1, 3, 4, 5, 11, 12, 13, 16, 100, 4096, 65_536];
        for len in lens {
            let v: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let c = arena.intern(&v);
            assert_eq!(arena.resolve(c), v.as_slice());
        }
        arena.seal();
        for len in lens {
            let v: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let c = arena.intern(&v);
            assert!(
                (c.0 as usize) < arena.sealed_len(),
                "re-intern after seal is a sealed hit"
            );
            assert_eq!(arena.resolve(c), v.as_slice());
        }
    }

    #[test]
    #[should_panic(expected = "not live")]
    fn resolve_out_of_range_panics() {
        let mut arena = Arena::new();
        arena.intern(b"x");
        arena.resolve(Code(1));
    }

    #[test]
    #[should_panic(expected = "out of range")]
    fn select_out_of_range_panics() {
        Arena::new().select(0);
    }
}
