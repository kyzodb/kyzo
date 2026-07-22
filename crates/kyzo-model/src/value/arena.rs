/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The order-preserving interning dictionary: dedups values, assigns dense `Code`; observers resolve admitted stamps to canonical bytes. The out-of-line home.
//!
//! ## The epoch-scoped interning architecture
//!
//! Dense codes, code-order-equals-byte-order, and validity under growth
//! cannot all hold at every *absolute* instant (pigeonhole). This arena
//! keeps all three for every observer by making the observer the unit of
//! meaning: **a code is only valid as observed through a scoped [`Frame`]
//! (live) or [`Snapshot`] (pinned), never as a bare handle.** Sealed codes
//! are global ranks over the union of immutable sorted runs (merges of runs
//! preserve the union, so sealed codes never change); tail codes are
//! arrival-ordered (identity only). A seal mints an [`EpochRemap`] —
//! the compact morphism carrying codes from one epoch to the next.
//!
//! - [`Arena`] is minting and transition only: `intern`, `seal`, and the
//!   two ways to open an observer. It has no read methods — there is no
//!   unnamed frame to smuggle a code through.
//! - [`Frame`] is the live observer: a borrow of the arena's current
//!   state. Nest scopes open an invariant-lifetime [`NestedDomainCtx`]
//!   for raw-handle identity/order ([`Frame::with_nested_ctx`]). Every
//!   spend still verifies the stamp via mint-checked [`Admission`] —
//!   deliberately no lifetime-branded *spend* witness, because a borrow
//!   lifetime cannot prove frame identity across coexisting arenas.
//!   `intern` and `seal` take `&mut Arena`, so the borrow checker retires
//!   every frame at the next mutation.
//! - [`Snapshot`] is the pinned observer: run references + a delta cut +
//!   frozen heap chunks + the epoch — the dictionary as one moment saw
//!   it, owned and `Send + Sync`. Same nest-brand / mint-checked-spend
//!   split as [`Frame`]. It answers identically forever while the writer
//!   interns and seals past it.
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
//! - For a live [`Frame`], every spend verifies arena identity, epoch,
//!   AND liveness bounds exactly ([`Frame::admits`] reports the same
//!   three facts, so `admits == true` means the spend cannot refuse on
//!   domain identity). The
//!   bounds half is also a theorem for lawfully minted stamps: mints are
//!   in bounds when issued, the arena only grows within an epoch, and no
//!   `&mut Arena` can coexist with the frame — the hard assert makes the
//!   theorem executable rather than trusted.
//! - For a [`Snapshot`], visibility is **not** implied by the epoch: the
//!   delta cut is part of the observer, and a same-epoch code minted
//!   after the cut is invisible to it. Snapshots therefore verify both
//!   stamp and cut, exactly and loudly, on every spend. Nest brands
//!   ([`Snapshot::with_nested_ctx`]) cover compare/identity under one
//!   pinned observer; spend stays mint-checked because two owned
//!   snapshots can coexist with unifiable lifetimes.
//!
//! ## The transition theorem, held by types
//!
//! 1. **Every observable value satisfies its law.** A [`Run`] is minted
//!    only sorted-unique (the delta drains pre-sorted; merges preserve
//!    the law); an [`EpochRemap`] only by [`Arena::seal`]; a
//!    [`StampedCode`] only by holders of the arena's mint token. None of
//!    the unlawful states can be written down, and every spend verifies
//!    its stamp.
//! 2. **No observer exists during a transition.** `seal` holds
//!    `&mut self`, so no `Frame` (or witness) survives into it, and
//!    `Snapshot`s hold only immutable structure — frozen chunks and runs
//!    the transition never mutates. Runs are shared (`Arc`), not
//!    consumed: old shapes legitimately outlive the transition *in old
//!    frames* — this is the epoch-scoped architecture in action — and
//!    their immutability is what makes it sound.
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
//! Arena identity is part of the stamp, not a convention: every
//! [`StampedCode`] carries the [`ArenaId`] that minted it, and every spend
//! ([`Frame::resolve`], the snapshot checks, [`EpochRemap::apply`]) proves
//! arena+epoch via [`Admission::prove_shared`] (typed [`Denial`], never
//! panic). Two arenas at the same epoch reject each other's stamps instead
//! of silently resolving wrong values. The shared vocabulary is
//! [`super::admission`].

use std::cmp::Ordering;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrd};

use super::bytes_qty::{ByteLen, ByteOff, ChunkId};
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
pub(super) struct Heap {
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
///
/// **The zero-span law**: a zero-length span owns no bytes, and its chunk
/// id is MEANINGLESS — it may address a chunk that was never materialized
/// (empty values append nothing). Every consumer must branch on
/// `len == ByteLen::ZERO` before interpreting `chunk`/`off`; serialization,
/// equality, or debug tooling that reads those fields of an empty span is
/// wrong by definition.
#[derive(Clone, Copy, Debug)]
pub(super) struct Span {
    chunk: ChunkId,
    off: ByteOff,
    len: ByteLen,
}

impl Span {
    /// The exclusive end offset within the chunk (`off + len`).
    ///
    /// Spans are minted only by [`Heap::push`], which refuses when
    /// `off + len` overflows `u32`. Widening each field to `usize` before
    /// adding therefore cannot wrap on our targets — the overflow state
    /// is unrepresentable.
    #[inline]
    fn end_off(self) -> usize {
        self.off.as_usize() + self.len.as_usize()
    }
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
    /// Refuses with [`Denial::ExtentOverflow`] when a single value exceeds
    /// `u32::MAX` bytes, the live offset exceeds `u32` span space, `off +
    /// len` would overflow `u32`, or the chunk id space is exhausted —
    /// never a process abort.
    pub fn push(&mut self, value: &[u8]) -> Result<Span, Denial> {
        let vlen = ByteLen::from_usize(value.len()).ok_or(Denial::ExtentOverflow)?;
        if value.len() >= CHUNK_SIZE {
            // Oversize value: a chunk of its own.
            self.freeze_live();
            let chunk = self.chunk_id()?;
            // Prove end_off: ZERO + vlen is the length itself when vlen is
            // a lawful ByteLen; keep the checked door for one mint law.
            match ByteOff::ZERO.checked_add(vlen) {
                Some(end) => core::mem::drop(end),
                None => return Err(Denial::ExtentOverflow),
            }
            self.frozen.push(Arc::from(value));
            return Ok(Span {
                chunk,
                off: ByteOff::ZERO,
                len: vlen,
            });
        }
        if self.live.len() + value.len() > CHUNK_SIZE {
            self.freeze_live();
        }
        let chunk = self.chunk_id()?;
        let off = ByteOff::from_usize(self.live.len()).ok_or(Denial::ExtentOverflow)?;
        match off.checked_add(vlen) {
            Some(end) => core::mem::drop(end),
            None => return Err(Denial::ExtentOverflow),
        }
        self.live.extend_from_slice(value);
        Ok(Span {
            chunk,
            off,
            len: vlen,
        })
    }

    /// Freeze the live chunk (if non-empty) into the shared set. Its chunk
    /// id is unchanged: it lands at exactly the index it was addressed by.
    fn freeze_live(&mut self) {
        if !self.live.is_empty() {
            let done = std::mem::take(&mut self.live);
            self.frozen.push(done.into());
        }
    }

    fn chunk_id(&self) -> Result<ChunkId, Denial> {
        ChunkId::from_usize(self.frozen.len()).ok_or(Denial::ExtentOverflow)
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
        if span.len == ByteLen::ZERO {
            return &[];
        }
        let off = span.off.as_usize();
        let end = span.end_off();
        let c = span.chunk.as_usize();
        if c < self.frozen.len() {
            &self.frozen[c][off..end]
        } else if c == self.frozen.len() {
            &self.live[off..end]
        } else {
            // Span names a chunk that never existed — empty rather than
            // aliasing a foreign live buffer.
            &[]
        }
    }

    fn deref_counter(&self) -> &AtomicU64 {
        &self.compare_derefs
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
        if span.len == ByteLen::ZERO {
            return &[];
        }
        let off = span.off.as_usize();
        let end = span.end_off();
        &self.chunks[span.chunk.as_usize()][off..end]
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
        match cmp_prefixed(self.prefix, self.span.len.raw(), np, match u32::try_from(needle.len()) { Ok(n) => n, Err(_) => u32::MAX }) {
            PrefixCmp::Decided(o) => o,
            PrefixCmp::NeedPayload => store.tie_payload(self.span).cmp(needle),
        }
    }

    /// Prefix-first compare against another entry; payload derefs only on
    /// tie.
    #[inline]
    fn cmp_entry<S: Store>(&self, other: &Entry, store: &S) -> Ordering {
        match cmp_prefixed(
            self.prefix,
            self.span.len.raw(),
            other.prefix,
            other.span.len.raw(),
        ) {
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
pub(super) struct Run {
    entries: Vec<Entry>,
}

impl Run {
    /// Mint from entries already sorted and unique (the delta drains
    /// through here). The precondition is the caller's law: every
    /// production caller (`seal` drain, merge output) establishes sorted
    /// uniqueness before this door. Refuses [`Denial::BookkeepingBroken`]
    /// when that law is violated — never a release-elided assert.
    fn from_sorted(entries: Vec<Entry>, heap: &Heap) -> Result<Run, Denial> {
        if !entries
            .windows(2)
            .all(|w| w[0].cmp_entry(&w[1], heap) == Ordering::Less)
        {
            return Err(Denial::BookkeepingBroken);
        }
        match heap {
            value => core::mem::drop(value),
        };
        Ok(Run { entries })
    }

    /// The merge: two lawful runs in, one lawful run out. Borrows its
    /// inputs — runs are immutable and may be shared with older frames,
    /// which keep observing them unchanged. Payloads equal in both inputs
    /// collapse to one output entry. (No per-merge position maps exist:
    /// sealed codes rank over the RUN UNION, so cascades are
    /// rank-invariant and the only remap anyone needs is the seal's
    /// compact [`EpochRemap`].)
    fn merge(a: &Run, b: &Run, heap: &Heap) -> Result<Run, Denial> {
        if a.entries.len() + b.entries.len() > match usize::try_from(u32::MAX) { Ok(n) => n, Err(_) => usize::MAX } {
            return Err(Denial::ExtentOverflow);
        }
        let mut merged = Vec::with_capacity(a.entries.len() + b.entries.len());
        let (mut i, mut j) = (0, 0);
        while i < a.entries.len() && j < b.entries.len() {
            match a.entries[i].cmp_entry(&b.entries[j], heap) {
                Ordering::Less => {
                    merged.push(a.entries[i]);
                    i += 1;
                }
                Ordering::Greater => {
                    merged.push(b.entries[j]);
                    j += 1;
                }
                Ordering::Equal => {
                    merged.push(a.entries[i]);
                    i += 1;
                    j += 1;
                }
            }
        }
        merged.extend_from_slice(&a.entries[i..]);
        merged.extend_from_slice(&b.entries[j..]);
        Ok(Run { entries: merged })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Rank of `needle` within this run: `Ok(rank)` if present, `Err(rank
    /// it would take)` if absent. Binary search's precondition is the
    /// type's postcondition.
    fn search<S: Store>(&self, np: [u8; 4], needle: &[u8], store: &S) -> Result<usize, usize> {
        self.entries
            .binary_search_by(|e| e.cmp_needle(np, needle, store))
    }
}

/// The stamp-minting authority token: a zero-sized proof whose only
/// constructor is private to this file. `StampedCode::mint` demands one,
/// so the set of modules that can mint stamps is exactly the set that can
/// write `StampMintAuthority(())` — this file, and nothing else, however
/// large the value plane grows. Deliberately not `Clone`/`Default`: no
/// path manufactures one from nothing.
pub(super) struct StampMintAuthority(pub(self) ());

/// Process-unique arena identity: minted once per [`Arena::try_new`] /
/// [`Arena::new`] from a monotone counter (creation order — deterministic,
/// no clock), carried by every stamp and observer, verified on every spend.
/// Never part of any answer, so determinism of results is untouched.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub struct ArenaId(u64);

const _: () = assert!(std::mem::size_of::<ArenaId>() == std::mem::size_of::<u64>());
const _: () = assert!(std::mem::align_of::<ArenaId>() == std::mem::align_of::<u64>());

static NEXT_ARENA_ID: AtomicU64 = AtomicU64::new(0);

impl ArenaId {
    /// Mint a process-unique id. Refuses with [`Denial::ExtentOverflow`]
    /// when the half-space of assignable ids is exhausted — never a panic
    /// at this door. [`Arena::new`] uses this through [`Arena::try_new`];
    /// wrap-around must not silently recycle identities.
    fn try_mint() -> Result<ArenaId, Denial> {
        let id = NEXT_ARENA_ID.fetch_add(1, AtomicOrd::Relaxed);
        if id >= u64::MAX / 2 {
            return Err(Denial::ExtentOverflow);
        }
        Ok(ArenaId(id))
    }
}

/// The arena's epoch: advances exactly at [`Arena::seal`], which rides
/// commit boundaries. Codes mean something relative to an epoch; every
/// spend verifies the stamp.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct Epoch(pub(super) u64);

const _: () = assert!(std::mem::size_of::<Epoch>() == std::mem::size_of::<u64>());
const _: () = assert!(std::mem::align_of::<Epoch>() == std::mem::align_of::<u64>());

impl Epoch {
    /// The raw counter, for display and diagnostics. Minting stays with
    /// the arena.
    #[inline]
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// Proof that a caller holds one `(arena, epoch)` context — the durable
/// **admission** token. Raw-handle equality and order — and physical word
/// identity — are lawful only under a reference to this token. `Copy`: a
/// durable re-checkable fact, not a consumable permission
/// ([`BulkSpendAuthority`] is the move-only spend token).
///
/// Paired with [`Denial`]: one discipline, opposite directions. A token
/// proves why an operation was allowed; a witness proves why it was
/// refused — never a bare boolean. The shared vocabulary door is
/// [`super::admission`].
///
/// ## Coexisting-arena boundary (why this token is not lifetime-branded)
///
/// KyzoDB's executor holds **multiple arenas live simultaneously**, and
/// epoch-stamped containers ([`super::column::Domain`], [`ExecRows`](super::exec::ExecRows))
/// **outlive** any one [`Frame`] borrow. An invariant-lifetime brand
/// ([`NestId`] / [`NestedDomainCtx`]) mints a compiler-unique identity per
/// nest scope and is applied where a single live observer nest is
/// provable — see [`Frame::with_nested_ctx`] / [`Snapshot::with_nested_ctx`].
/// A brand cannot prove frame identity across coexisting arenas (two
/// frames over different arenas can share a borrow lifetime), so full
/// branding of this durable token would claim a safety it cannot deliver.
/// Measurement and rejection rationale: [`super::code`] module docs.
///
/// Where instances coexist or values outlive the nest, this mint-checked
/// token is the ceiling: [`Admission::prove_shared`] (typed [`Denial`]) or
/// [`Admission::from_observer`] / plane-internal [`Admission::at`].
///
/// Mint paths: [`Admission::from_observer`] (infallible — the observer
/// *is* the context) and [`Admission::prove_shared`] (typed refusal when
/// two sides disagree). No `Default`: a context cannot be conjured empty.
///
/// Thin call-site alias: [`DomainCtx`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Admission {
    arena: ArenaId,
    epoch: Epoch,
}

/// Why an admission/spend/write door refused — the **denial** witness.
/// Opposite of [`Admission`]: same discipline, refuse direction. Never a
/// bare boolean, never a process abort for a reachable cut/bounds miss
/// or capacity/bookkeeping failure.
///
/// Thin call-site alias: [`DomainCtxRefusal`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Denial {
    ArenaMismatch {
        left: ArenaId,
        right: ArenaId,
    },
    EpochMismatch {
        left: Epoch,
        right: Epoch,
    },
    /// Code or domain extent exceeds the observer's live cut / length.
    VisibilityOverflow {
        required: usize,
        visible: usize,
    },
    /// Empty projection cannot invent a tuple width.
    EmptyProjection,
    /// Tuple width does not match the container / sink arity.
    ArityMismatch {
        expected: usize,
        got: usize,
    },
    /// A `u32`/`u64` extent would wrap: domain absorb growth, arena
    /// distinct-value capacity (`len == u32::MAX`), a value's byte length /
    /// heap offset / chunk id exceeding span encoding space, `off + len`
    /// overflow at span mint, the epoch counter exhausting `u64`, or
    /// [`ArenaId`] process-unique mint space (`2^63`) exhausting.
    ExtentOverflow,
    /// Epoch remap produced a code past `u32::MAX` (checked add overflow).
    CodeRemapOverflow,
    /// Rank select or seal dedup contradicted proven bounds: run/delta
    /// bookkeeping is corrupt — typed refuse, never `unreachable!`.
    BookkeepingBroken,
}

impl std::fmt::Display for Denial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Denial::ArenaMismatch { left, right } => {
                write!(f, "arena mismatch ({left:?} vs {right:?})")
            }
            Denial::EpochMismatch { left, right } => {
                write!(f, "epoch mismatch ({left:?} vs {right:?})")
            }
            Denial::VisibilityOverflow { required, visible } => {
                write!(
                    f,
                    "visibility overflow (required {required}, visible {visible})"
                )
            }
            Denial::EmptyProjection => write!(f, "empty projection"),
            Denial::ArityMismatch { expected, got } => {
                write!(f, "arity mismatch (expected {expected}, got {got})")
            }
            Denial::ExtentOverflow => write!(f, "extent overflow"),
            Denial::CodeRemapOverflow => write!(f, "code remap overflow"),
            Denial::BookkeepingBroken => write!(f, "arena bookkeeping broken"),
        }
    }
}

impl std::error::Error for Denial {}

impl miette::Diagnostic for Denial {
    fn code<'a>(&'a self) -> Option<Box<dyn std::fmt::Display + 'a>> {
        Some(Box::new("value::denial"))
    }
}

/// Thin alias for [`Admission`] — existing call sites keep this name; the
/// vocabulary module ([`super::admission`]) is the one door.
pub type DomainCtx = Admission;

/// Thin alias for [`Denial`] — existing call sites keep this name; the
/// vocabulary module ([`super::admission`]) is the one door.
pub type DomainCtxRefusal = Denial;

/// Invariant lifetime brand: each nest scope mints a compiler-unique `'id`
/// at zero runtime cost (GhostCell / generativity).
///
/// Variance: `fn(&'id ()) -> &'id ()` is **invariant** in `'id`, so a
/// value branded `'a` cannot coerce to `'b`. Brands from different
/// [`Frame::with_nested_ctx`] / [`Snapshot::with_nested_ctx`] nests stay
/// unmixable. Private constructor — only those nest doors mint one.
#[derive(Clone, Copy, Debug)]
pub struct NestId<'id>(PhantomData<fn(&'id ()) -> &'id ()>);

/// Domain context branded to one nest scope under a single live observer.
/// Cross-nest mixing of `NestedDomainCtx` fails to compile.
///
/// An admission token under a nest brand — same vocabulary as
/// [`Admission`], scoped. Applied only where scopes nest (one live
/// [`Frame`] or [`Snapshot`] nest). For coexisting arenas and outliving
/// containers, use unbranded [`Admission`] — see that type's
/// coexisting-arena boundary.
#[derive(Clone, Copy, Debug)]
pub struct NestedDomainCtx<'id> {
    ctx: Admission,
    _nest: NestId<'id>,
}

impl<'id> NestedDomainCtx<'id> {
    /// The durable unbranded fact (for APIs that must outlive the nest or
    /// cross coexisting arenas under a later [`Admission::prove_shared`]).
    #[inline]
    pub fn ctx(self) -> Admission {
        self.ctx
    }

    #[inline]
    pub fn arena(self) -> ArenaId {
        self.ctx.arena
    }

    #[inline]
    pub fn epoch(self) -> Epoch {
        self.ctx.epoch
    }

    /// Physical handle equality under this nest brand.
    #[inline]
    pub fn same_handle(self, a: Code, b: Code) -> bool {
        self.ctx.same_handle(a, b)
    }

    /// Identity order of packed handles under this nest brand.
    #[inline]
    pub fn cmp_identity(self, a: Code, b: Code) -> Ordering {
        self.ctx.cmp_identity(a, b)
    }
}

impl Admission {
    /// Mint from a bulk observer: the observer's arena and epoch *are*
    /// the context. Infallible.
    ///
    /// **Coexisting-arena boundary:** returns the unbranded durable token
    /// — observers from different arenas can be named in one scope, and
    /// the result may outlive a nest. For a compiler-unique nest brand
    /// under one live frame/snapshot, use [`Frame::with_nested_ctx`] /
    /// [`Snapshot::with_nested_ctx`].
    pub fn from_observer<O: BulkObserver>(o: &O) -> Admission {
        Admission {
            arena: o.bulk_arena(),
            epoch: o.bulk_epoch(),
        }
    }

    /// Mint proving two `(arena, epoch)` pairs name one context. Typed
    /// [`Denial`] on mismatch — never a panic, never a bare boolean.
    /// Cross-context comparison cannot obtain an [`Admission`], so it
    /// cannot call `same_word` / handle equality under a forged shared
    /// context.
    ///
    /// **Coexisting-arena boundary:** this is the deliberate unbranded
    /// door — join/admit/gather must name two sides that may have been
    /// built under different nest brands (or no brand). A lifetime brand
    /// cannot unify those sides; mint-checked equality is the ceiling.
    pub fn prove_shared(
        left_arena: ArenaId,
        left_epoch: Epoch,
        right_arena: ArenaId,
        right_epoch: Epoch,
    ) -> Result<Admission, Denial> {
        if left_arena != right_arena {
            return Err(Denial::ArenaMismatch {
                left: left_arena,
                right: right_arena,
            });
        }
        if left_epoch != right_epoch {
            return Err(Denial::EpochMismatch {
                left: left_epoch,
                right: right_epoch,
            });
        }
        Ok(Admission {
            arena: left_arena,
            epoch: left_epoch,
        })
    }

    /// Plane-internal: mint from already-proven domain parts (a
    /// [`super::column::Domain`] or gather product). The caller holds the
    /// proof; this does not re-check.
    ///
    /// **Coexisting-arena boundary:** containers carry unbranded domain
    /// identity across seals and across coexisting arenas; branding here
    /// would fake a nest that no longer exists.
    pub(super) fn at(arena: ArenaId, epoch: Epoch) -> Admission {
        Admission { arena, epoch }
    }

    #[inline]
    pub fn arena(self) -> ArenaId {
        self.arena
    }

    #[inline]
    pub fn epoch(self) -> Epoch {
        self.epoch
    }

    /// Physical handle equality under this proven context. Without an
    /// [`Admission`], raw-handle equality has no lawful API — cross-context
    /// comparison cannot affirmatively lie.
    #[inline]
    pub fn same_handle(self, a: Code, b: Code) -> bool {
        a.0 == b.0
    }

    /// Identity order of packed handles under this proven context
    /// (sealed-rank numeric order when the domain is sealed — never a
    /// substitute for observer value-order over tail codes).
    #[inline]
    pub fn cmp_identity(self, a: Code, b: Code) -> Ordering {
        a.0.cmp(&b.0)
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
            .binary_search_by(|&i| self.arrivals[(match usize::try_from(i) { Ok(n) => n, Err(_) => 0 })].cmp_needle(np, needle, store))
    }

    fn entry_by_rank(&self, rank: usize) -> Entry {
        self.arrivals[(match usize::try_from(self.sorted[rank]) { Ok(n) => n, Err(_) => 0 })]
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
    arena: ArenaId,
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
    /// The arena this remap belongs to (plane-internal: container gather
    /// doors verify it).
    pub(super) fn arena_id(&self) -> ArenaId {
        self.arena
    }

    /// The epoch this remap reads codes from.
    pub fn source_epoch(&self) -> Epoch {
        self.from
    }

    /// The epoch this remap restamps codes into.
    pub fn target_epoch(&self) -> Epoch {
        self.to
    }

    /// Restamp an old-epoch code into the new epoch.
    ///
    /// Typed refusal on a foreign arena, wrong-epoch stamp, remap overflow,
    /// or a code not live in the source epoch.
    ///
    /// **Coexisting-arena boundary:** remaps and stamps are owned values
    /// that outlive any nest brand; arena/epoch proof stays mint-checked
    /// [`Admission::prove_shared`].
    pub fn apply(&self, sc: StampedCode) -> Result<StampedCode, Denial> {
        Admission::prove_shared(self.arena, self.from, sc.arena(), sc.epoch())?;
        let code = self.apply_raw(sc.code())?;
        Ok(StampedCode::mint(
            code,
            self.to,
            self.arena,
            StampMintAuthority(()),
        ))
    }

    /// The raw morphism, for bulk gathers by epoch-stamped containers
    /// (which carry one stamp for all their codes and verify it once).
    /// Typed [`Denial`] on overflow or a code not live in the source epoch.
    pub(super) fn apply_raw(&self, code: Code) -> Result<Code, Denial> {
        let c = code.0;
        let visible = (match usize::try_from(self.from_sealed_len) { Ok(n) => n, Err(_) => 0 }) + self.tail.len();
        if c < self.from_sealed_len {
            // Old sealed rank r moves to the r-th position not occupied
            // by an inserted value: r + k, where k counts the inserted
            // entries with D[i] - i <= r. (D[i] - i is the number of
            // non-inserted positions below D[i]; it is non-decreasing
            // because D is strictly increasing, so k is one binary
            // search.)
            let r = (match usize::try_from(c) { Ok(n) => n, Err(_) => 0 });
            let (mut lo, mut hi) = (0usize, self.inserted.len());
            while lo < hi {
                let mid = (lo + hi) / 2;
                if (match usize::try_from(self.inserted[mid]) { Ok(n) => n, Err(_) => 0 }) - mid <= r {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            r.checked_add(lo)
                .and_then(|n| match u32::try_from(n) { Ok(v) => Some(v), Err(_overflow) => None })
                .map(Code)
                .ok_or(Denial::CodeRemapOverflow)
        } else {
            let a = (match usize::try_from(c - self.from_sealed_len) { Ok(n) => n, Err(_) => 0 });
            if a >= self.tail.len() {
                return Err(Denial::VisibilityOverflow {
                    required: (match usize::try_from(c) { Ok(n) => n, Err(_) => 0 }) + 1,
                    visible,
                });
            }
            Ok(Code(self.tail[a]))
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
    fn entry_of(&self, c: usize) -> Result<Entry, Denial> {
        if c < self.sealed_len {
            // Steady state after cascades collapse: one run, and a sealed
            // code is a literal index — the O(1) read the sealed scope
            // promises at rest.
            if self.runs.len() == 1 {
                return Ok(self.runs[0].entries[c]);
            }
            self.select_sealed(c)
        } else {
            let a = c - self.sealed_len;
            if a >= self.arrivals.len() {
                return Err(Denial::VisibilityOverflow {
                    required: c + 1,
                    visible: self.len(),
                });
            }
            Ok(self.arrivals[a])
        }
    }

    fn resolve(&self, c: usize) -> Result<&'a [u8], Denial> {
        Ok(self.store.payload(self.entry_of(c)?.span))
    }

    /// Semantic comparison of two live codes: rank order is byte order
    /// when both are sealed; any tail code involved goes prefix-first.
    fn cmp(&self, ca: usize, cb: usize) -> Result<Ordering, Denial> {
        if ca >= self.len() {
            return Err(Denial::VisibilityOverflow {
                required: ca + 1,
                visible: self.len(),
            });
        }
        if cb >= self.len() {
            return Err(Denial::VisibilityOverflow {
                required: cb + 1,
                visible: self.len(),
            });
        }
        if ca == cb {
            return Ok(Ordering::Equal);
        }
        if ca < self.sealed_len && cb < self.sealed_len {
            return Ok(ca.cmp(&cb));
        }
        let ea = self.entry_of(ca)?;
        let eb = self.entry_of(cb)?;
        Ok(ea.cmp_entry(&eb, self.store))
    }

    /// Global ordered rank of `value` across sealed and delta together:
    /// `Ok(Ok(rank))` if interned, `Ok(Err(rank it would take))` if not.
    /// Refuses with [`Denial::ExtentOverflow`] when the needle exceeds the
    /// `u32` compare space — never a process abort.
    fn rank(&self, value: &[u8]) -> Result<Result<usize, usize>, Denial> {
        if value.len() > match usize::try_from(u32::MAX) { Ok(n) => n, Err(_) => usize::MAX } {
            return Err(Denial::ExtentOverflow);
        }
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
            .binary_search_by(|&i| self.arrivals[(match usize::try_from(i) { Ok(n) => n, Err(_) => 0 })].cmp_needle(np, value, self.store))
        {
            Ok(pos) => {
                rank += pos;
                found = true;
            }
            Err(pos) => rank += pos,
        }
        if found { Ok(Ok(rank)) } else { Ok(Err(rank)) }
    }

    /// The `k`-th smallest interned value across sealed and delta.
    fn select(&self, k: usize) -> Result<&'a [u8], Denial> {
        let visible = self.len();
        if k >= visible {
            return Err(Denial::VisibilityOverflow {
                required: k + 1,
                visible,
            });
        }
        Ok(self.store.payload(self.select_global(k)?.span))
    }

    /// Select the sealed value of rank `k` across the disjoint runs: in
    /// exactly one run, an index `i` has `i` + (lower bounds elsewhere)
    /// equal to `k`; that predicate is monotone in `i` per run.
    ///
    /// Refuses with [`Denial::BookkeepingBroken`] when no run yields rank
    /// `k` despite the caller having proven `k` in sealed range.
    fn select_sealed(&self, k: usize) -> Result<Entry, Denial> {
        for (r, run) in self.runs.iter().enumerate() {
            if let Some(e) = self.select_in(run, r, k, false) {
                return Ok(e);
            }
        }
        Err(Denial::BookkeepingBroken)
    }

    /// Select rank `k` across runs and delta together.
    ///
    /// Refuses with [`Denial::BookkeepingBroken`] when neither runs nor
    /// delta yields rank `k` despite the caller having proven `k` in
    /// visible range.
    fn select_global(&self, k: usize) -> Result<Entry, Denial> {
        for (r, run) in self.runs.iter().enumerate() {
            if let Some(e) = self.select_in(run, r, k, true) {
                return Ok(e);
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
                Ordering::Equal => return Ok(e),
            }
        }
        Err(Denial::BookkeepingBroken)
    }

    fn entry_by_delta_rank(&self, rank: usize) -> Entry {
        self.arrivals[(match usize::try_from(self.sorted[rank]) { Ok(n) => n, Err(_) => 0 })]
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
            self.arrivals[(match usize::try_from(i) { Ok(n) => n, Err(_) => 0 })].cmp_entry(&e, self.store) == Ordering::Less
        })
    }
}

/// The live observer frame: a borrow of the arena's current state, and
/// the only place a code can be spent live. Valid for exactly one
/// quiescent stretch of one epoch — the borrow checker retires it at the
/// next mutation.
///
/// **Nest brands vs spend witnesses.** [`Frame::with_nested_ctx`] opens an
/// invariant-lifetime [`NestedDomainCtx`] for raw-handle identity/order
/// under this single live frame (one nest, compiler-unique `'id`). There
/// is deliberately **no lifetime-branded spendable witness**: a borrow
/// lifetime cannot prove frame identity across coexisting arenas (two
/// frames over *different* arenas can unify lifetimes), so a spend
/// witness "admitted" by one frame would compile as spendable in another.
/// Every spend therefore proves the stamp via [`Admission::prove_shared`]
/// (typed refusal), symmetric with [`Snapshot`]. Bulk amortization of
/// that check belongs to the epoch-stamped containers.
#[derive(Clone, Copy)]
pub struct Frame<'a> {
    arena: ArenaId,
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

    /// Open an invariant-lifetime brand nest under this single live frame.
    ///
    /// `'id` cannot unify with any other nest's brand — including a nest
    /// opened on a coexisting frame that happens to share borrow lifetime
    /// `'a`. The branded token is for nested raw-handle compare/identity
    /// inside `f`; it cannot escape the closure (HRTB). Spend paths stay
    /// on mint-checked [`DomainCtx`] — see the frame-level coexisting-arena
    /// boundary.
    pub fn with_nested_ctx<R>(&self, f: impl for<'id> FnOnce(NestedDomainCtx<'id>) -> R) -> R {
        f(NestedDomainCtx {
            ctx: Admission {
                arena: self.arena,
                epoch: self.epoch,
            },
            _nest: NestId(PhantomData),
        })
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

    /// Verify a stamp against this frame: arena identity and epoch, both
    /// exact. Validity is stamp equality **plus visibility**; for a live
    /// frame the visibility half is a theorem rather than a check: stamps
    /// are mintable only by this plane, every mint is in bounds when
    /// issued, the arena only grows within an epoch, and no transition
    /// (`&mut Arena`) can coexist with this frame. The bounds theorem is
    /// re-checked in debug builds.
    ///
    /// Arena/epoch mismatch and out-of-bounds codes are typed
    /// [`Denial`] — cross through [`EpochRemap::apply`] for a stale epoch.
    ///
    /// **Coexisting-arena boundary:** stamps are owned and may arrive from
    /// any arena; proof is mint-checked [`Admission::prove_shared`], not a
    /// nest brand (a brand cannot refuse a foreign stamp at compile time).
    fn check(&self, sc: StampedCode) -> Result<usize, Denial> {
        Admission::prove_shared(self.arena, self.epoch, sc.arena(), sc.epoch())?;
        let c = (match usize::try_from(sc.code().raw()) { Ok(n) => n, Err(_) => 0 });
        let visible = self.len();
        if c >= visible {
            return Err(Denial::VisibilityOverflow {
                required: c + 1,
                visible,
            });
        }
        Ok(c)
    }

    /// Whether a stamp is spendable in this frame — the non-panicking
    /// probe for callers that branch. Exact: arena identity, epoch, AND
    /// liveness bounds, the same three facts [`Frame::resolve`] checks,
    /// so `admits(sc)` true means the spend cannot refuse on domain identity.
    pub fn admits(&self, sc: StampedCode) -> bool {
        sc.arena() == self.arena
            && sc.epoch() == self.epoch
            && ((match usize::try_from(sc.code().raw()) { Ok(n) => n, Err(_) => 0 })) < self.len()
    }

    /// Resolve a stamped code to its bytes.
    ///
    /// Typed refusal on a foreign-arena or stale stamp (see [`Frame::admits`]).
    pub fn resolve(&self, sc: StampedCode) -> Result<&'a [u8], Denial> {
        let c = self.check(sc)?;
        self.view().resolve(c)
    }

    /// Semantic comparison of two stamped codes: integer compare when
    /// both sealed (rank order is byte order), prefix-first bytes
    /// otherwise.
    ///
    /// Typed refusal on foreign-arena or stale stamps.
    pub fn cmp_codes(&self, a: StampedCode, b: StampedCode) -> Result<Ordering, Denial> {
        let (ca, cb) = (self.check(a)?, self.check(b)?);
        self.view().cmp(ca, cb)
    }

    /// Global ordered rank of `value` across sealed and delta: `Ok(Ok(rank))`
    /// if interned, `Ok(Err(rank it would take))` if not. Refuses with
    /// [`Denial::ExtentOverflow`] when the needle exceeds the `u32` compare
    /// space — never a process abort.
    pub fn rank(&self, value: &[u8]) -> Result<Result<usize, usize>, Denial> {
        self.view().rank(value)
    }

    /// The `k`-th smallest interned value (inverse of [`Frame::rank`]).
    ///
    /// # Panics
    ///
    /// Panics if `k >= len()`.
    pub fn select(&self, k: usize) -> Result<&'a [u8], Denial> {
        self.view().select(k)
    }
}

/// The pinned observer frame: run references + a delta cut + frozen heap
/// chunks + the epoch — a snapshot of the arena's state at one moment,
/// owned and `Send + Sync` (everything it holds is immutable). It answers
/// identically forever while the writer interns and seals past it.
///
/// Visibility is **not** implied by the epoch here: the delta cut is part
/// of the observer, so every spend verifies both the stamp and the cut,
/// exactly and loudly.
///
/// **Nest brands vs spend witnesses.** [`Snapshot::with_nested_ctx`] brands
/// raw-handle identity/order under this one pinned observer. Spend stays
/// mint-checked: two owned snapshots of different epochs (or arenas) can
/// coexist with unifiable lifetimes, and a branded spend witness that
/// unified across them would claim a safety it cannot deliver.
pub struct Snapshot {
    arena: ArenaId,
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

    /// Open an invariant-lifetime brand nest under this pinned snapshot
    /// (see [`Frame::with_nested_ctx`]). Spend paths remain mint-checked.
    pub fn with_nested_ctx<R>(&self, f: impl for<'id> FnOnce(NestedDomainCtx<'id>) -> R) -> R {
        f(NestedDomainCtx {
            ctx: Admission {
                arena: self.arena,
                epoch: self.epoch,
            },
            _nest: NestId(PhantomData),
        })
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
    /// Arena/epoch mismatch and cut overflow are typed [`Denial`].
    ///
    /// **Coexisting-arena boundary:** owned snapshots and stamps outlive
    /// nest brands; domain identity is mint-checked [`Admission::prove_shared`].
    fn check(&self, sc: StampedCode) -> Result<usize, Denial> {
        Admission::prove_shared(self.arena, self.epoch, sc.arena(), sc.epoch())?;
        let c = (match usize::try_from(sc.code().raw()) { Ok(n) => n, Err(_) => 0 });
        let visible = self.len();
        if c >= visible {
            return Err(Denial::VisibilityOverflow {
                required: c + 1,
                visible,
            });
        }
        Ok(c)
    }

    /// Resolve a stamped code to its bytes.
    ///
    /// Typed refusal on a wrong-epoch, foreign-arena, or past-cut stamp.
    pub fn resolve(&self, sc: StampedCode) -> Result<&[u8], Denial> {
        let c = self.check(sc)?;
        self.view().resolve(c)
    }

    /// Semantic comparison of two stamped codes (see
    /// [`Frame::cmp_codes`]).
    ///
    /// Typed refusal on wrong-epoch or foreign-arena stamps.
    pub fn cmp_codes(&self, a: StampedCode, b: StampedCode) -> Result<Ordering, Denial> {
        let (ca, cb) = (self.check(a)?, self.check(b)?);
        self.view().cmp(ca, cb)
    }

    /// Global ordered rank of `value` as of this snapshot. Refuses with
    /// [`Denial::ExtentOverflow`] when the needle exceeds the `u32` compare
    /// space — never a process abort.
    pub fn rank(&self, value: &[u8]) -> Result<Result<usize, usize>, Denial> {
        self.view().rank(value)
    }

    /// The `k`-th smallest value as of this snapshot.
    ///
    /// # Panics
    ///
    /// Panics if `k >= len()`.
    pub fn select(&self, k: usize) -> Result<&[u8], Denial> {
        self.view().select(k)
    }
}

mod sealed {
    use std::cmp::Ordering;

    use super::Denial;

    /// Observer sealing PLUS the unchecked raw read core. The raw methods
    /// are reachable only inside this module and from
    /// [`super::BulkSpendAuthority`] spend paths / [`super::BulkPass`] —
    /// never as a public forge surface on a bare [`super::Frame`].
    pub trait Sealed {
        fn raw_bytes(&self, c: usize) -> Result<&[u8], Denial>;
        fn raw_cmp(&self, a: usize, b: usize) -> Result<Ordering, Denial>;
    }

    impl Sealed for super::Frame<'_> {
        #[inline]
        fn raw_bytes(&self, c: usize) -> Result<&[u8], Denial> {
            self.view().resolve(c)
        }

        #[inline]
        fn raw_cmp(&self, a: usize, b: usize) -> Result<Ordering, Denial> {
            self.view().cmp(a, b)
        }
    }

    impl Sealed for super::Snapshot {
        #[inline]
        fn raw_bytes(&self, c: usize) -> Result<&[u8], Denial> {
            self.view().resolve(c)
        }

        #[inline]
        fn raw_cmp(&self, a: usize, b: usize) -> Result<Ordering, Denial> {
            self.view().cmp(a, b)
        }
    }
}

/// Consumable permission that a container-domain admission verified arena
/// identity, epoch, and visibility extent against an observer — an
/// admission token in the [`super::admission`] vocabulary (paired with
/// [`Denial`] for the refuse direction). The only mint is plane-internal
/// ([`BulkSpendAuthority::after_domain_admission`]). Holding a [`Frame`]
/// is not enough — sealing [`BulkObserver`] guards who observes; this
/// token guards who spends.
///
/// Move-only consume-on-spend: no `Clone`/`Copy`/`Default`, no accessor
/// ever returns one. Spend either (a) into a single
/// [`BulkObserver::resolve_raw`] / [`BulkObserver::cmp_raw`] call, or (b)
/// into [`BulkSpendAuthority::open_pass`] for an amortized bulk pass. After
/// either spend the token is gone — reuse is a move error (and duplication
/// is refused by the absence proofs in [`super::proofs`]).
pub struct BulkSpendAuthority(());

impl BulkSpendAuthority {
    /// Minted exactly once per container admission, after the domain
    /// checks pass.
    pub(super) fn after_domain_admission() -> BulkSpendAuthority {
        BulkSpendAuthority(())
    }

    /// Spend this authority into a [`BulkPass`] under `o`. The consumable
    /// token disappears; the pass is the lasting capability for that
    /// admission (many raw reads, zero remints).
    pub(super) fn open_pass<'a, O: BulkObserver>(self, o: &'a O) -> BulkPass<'a, O> {
        let BulkSpendAuthority(()) = self;
        BulkPass { obs: o }
    }
}

/// Bulk-pass capability opened by spending [`BulkSpendAuthority`]. The
/// consumable permission is gone; this handle proves admission for the
/// observer borrow and may resolve/cmp raw codes without further tokens.
///
/// Not a second mint path: private fields, only
/// [`BulkSpendAuthority::open_pass`] constructs one. Not `Clone` of the
/// *consumable* — the authority was already spent; this is the opened
/// capability, used by shared reference across the pass.
pub(super) struct BulkPass<'a, O: BulkObserver> {
    obs: &'a O,
}

impl<'a, O: BulkObserver> BulkPass<'a, O> {
    #[inline]
    pub(super) fn resolve(&self, c: usize) -> Result<&'a [u8], Denial> {
        sealed::Sealed::raw_bytes(self.obs, c)
    }

    #[inline]
    pub(super) fn cmp(&self, a: usize, b: usize) -> Result<Ordering, Denial> {
        sealed::Sealed::raw_cmp(self.obs, a, b)
    }
}

/// The bulk-spend observer capability for stamped containers — the facts
/// a container-domain admission verifies (arena identity, epoch,
/// visibility extent) and the raw spends that are lawful ONLY under such
/// an admission. Sealed: exactly [`Frame`] and [`Snapshot`] observe;
/// nothing else can implement this and forge an observer.
///
/// One-shot raw spends take owned [`BulkSpendAuthority`] (consume-on-spend).
/// Amortized bulk reads spend that authority once into [`BulkPass`] via
/// [`BulkSpendAuthority::open_pass`] — never by reminting a fresh authority
/// per resolve.
pub trait BulkObserver: sealed::Sealed {
    fn bulk_arena(&self) -> ArenaId;
    fn bulk_epoch(&self) -> Epoch;
    /// Total visible codes (for a snapshot this includes its cut).
    fn bulk_len(&self) -> usize;
    /// Sealed prefix bound (codes below it compare numerically).
    fn bulk_sealed_len(&self) -> usize;

    /// One-shot spend: consumes `proof`. Reuse of the same binding after
    /// this call is a move error.
    fn resolve_raw(&self, c: usize, proof: BulkSpendAuthority) -> Result<&[u8], Denial> {
        let BulkSpendAuthority(()) = proof;
        sealed::Sealed::raw_bytes(self, c)
    }

    /// One-shot spend: consumes `proof`. Reuse of the same binding after
    /// this call is a move error.
    fn cmp_raw(&self, a: usize, b: usize, proof: BulkSpendAuthority) -> Result<Ordering, Denial> {
        let BulkSpendAuthority(()) = proof;
        sealed::Sealed::raw_cmp(self, a, b)
    }
}

impl BulkObserver for Frame<'_> {
    fn bulk_arena(&self) -> ArenaId {
        self.arena
    }

    fn bulk_epoch(&self) -> Epoch {
        self.epoch
    }

    fn bulk_len(&self) -> usize {
        self.len()
    }

    fn bulk_sealed_len(&self) -> usize {
        self.sealed_len
    }
}

impl BulkObserver for Snapshot {
    fn bulk_arena(&self) -> ArenaId {
        self.arena
    }

    fn bulk_epoch(&self) -> Epoch {
        self.epoch
    }

    fn bulk_len(&self) -> usize {
        self.len()
    }

    fn bulk_sealed_len(&self) -> usize {
        self.sealed_len
    }
}

/// The shared, order-preserving interning arena: minting and transition
/// only. Reads happen through the observer frames — [`Arena::frame`] for
/// the live borrow, [`Arena::snapshot`] for the pinned owner. See the
/// module docs for the full epoch-scoped interning architecture.
pub struct Arena {
    id: ArenaId,
    heap: Heap,
    runs: Vec<Arc<Run>>,
    /// Total sealed values (= sum of run lengths; runs are disjoint).
    sealed_len: usize,
    delta: Delta,
    epoch: Epoch,
}

impl Arena {
    /// Infallible mint for the reachable process lifetime. Exhaustion of
    /// the ArenaId half-space (`2^63`) is typed at [`Arena::try_new`]; this
    /// face maps that refuse to a colliding `ArenaId(0)` placeholder so the
    /// process stays alive for diagnostics — hosts that care must call
    /// [`try_new`] and refuse.
    pub fn new() -> Self {
        match Self::try_new() {
            Ok(arena) => arena,
            Err(
                Denial::ExtentOverflow
                | Denial::EmptyProjection
                | Denial::BookkeepingBroken
                | Denial::CodeRemapOverflow
                | Denial::ArenaMismatch { .. }
                | Denial::EpochMismatch { .. }
                | Denial::VisibilityOverflow { .. }
                | Denial::ArityMismatch { .. },
            ) => Arena {
                id: ArenaId(0),
                heap: Heap::new(),
                runs: Vec::new(),
                sealed_len: 0,
                delta: Delta::new(),
                epoch: Epoch(0),
            },
        }
    }

    /// Evidence-bearing constructor: refuses with
    /// [`Denial::ExtentOverflow`] when ArenaId space is exhausted — never
    /// a bare panic at the mint law itself.
    pub fn try_new() -> Result<Self, Denial> {
        Ok(Arena {
            id: ArenaId::try_mint()?,
            heap: Heap::new(),
            runs: Vec::new(),
            sealed_len: 0,
            delta: Delta::new(),
            epoch: Epoch(0),
        })
    }

    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// Plane-internal identity (typed refusal surfaces compare it).
    pub(super) fn id(&self) -> ArenaId {
        self.id
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
            arena: self.id,
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
            arena: self.id,
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
    /// Plane-internal: the arena is generic byte substrate, and raw bytes
    /// are not the value plane's production currency. The value layer's
    /// door is `Value::mint`, which spends a `CanonicalBytes` witness.
    ///
    /// Refuses with [`Denial::ExtentOverflow`] when the arena already holds
    /// `u32::MAX` distinct values, when `value.len()` exceeds the `u32`
    /// heap-span encoding space, or when heap chunk/offset capacity is
    /// exhausted — never a process abort.
    pub(super) fn intern(&mut self, value: &[u8]) -> Result<StampedCode, Denial> {
        if self.len() >= match usize::try_from(u32::MAX) { Ok(n) => n, Err(_) => usize::MAX } {
            return Err(Denial::ExtentOverflow);
        }
        if value.len() > match usize::try_from(u32::MAX) { Ok(n) => n, Err(_) => usize::MAX } {
            return Err(Denial::ExtentOverflow);
        }
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
            Code(match u32::try_from(rank) { Ok(v) => v, Err(_) => return Err(Denial::ExtentOverflow) })
        } else {
            match self.delta.search(np, value, &self.heap) {
                Ok(pos) => {
                    let arrival = self.delta.sorted[pos];
                    Code(match u32::try_from(self.sealed_len + match usize::try_from(arrival) { Ok(n) => n, Err(_) => 0 }) { Ok(v) => v, Err(_) => return Err(Denial::ExtentOverflow) })
                }
                Err(pos) => {
                    let span = self.heap.push(value)?;
                    let entry = Entry::new(span, &self.heap);
                    let arrival = match u32::try_from(self.delta.arrivals.len()) { Ok(v) => v, Err(_) => return Err(Denial::ExtentOverflow) };
                    self.delta.arrivals.push(entry);
                    self.delta.sorted.insert(pos, arrival);
                    Code(match u32::try_from(self.sealed_len + match usize::try_from(arrival) { Ok(n) => n, Err(_) => 0 }) { Ok(v) => v, Err(_) => return Err(Denial::ExtentOverflow) })
                }
            }
        };
        Ok(StampedCode::mint(
            code,
            self.epoch,
            self.id,
            StampMintAuthority(()),
        ))
    }

    /// Seal the epoch: drain the delta into the runs (with geometric
    /// cascade merges — rank-invariant, since sealed codes rank over the
    /// union), advance the epoch, and mint the [`EpochRemap`] every held
    /// code crosses through. Rides commit boundaries.
    ///
    /// Refuses with [`Denial::ExtentOverflow`] when a cascade merge would
    /// exceed the `u32` position space, or when the epoch counter would
    /// wrap; refuses with [`Denial::BookkeepingBroken`] when a delta value
    /// is already present in sealed runs (dedup invariant) — never a
    /// process abort. Geometric cascade takes its two-run pair only when
    /// that pair is structurally present; [`Denial::BookkeepingBroken`]
    /// does not cover "cannot obtain two runs."
    pub fn seal(&mut self) -> Result<EpochRemap, Denial> {
        let from = self.epoch;
        let from_sealed_len = match u32::try_from(self.sealed_len) { Ok(v) => v, Err(_) => return Err(Denial::ExtentOverflow) };
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
                    // intern-time dedup; an exact hit is corrupt bookkeeping.
                    Ok(_) => return Err(Denial::BookkeepingBroken),
                    Err(pos) => sealed_rank += pos,
                }
            }
            let new_rank = match u32::try_from(sealed_rank + j) { Ok(v) => v, Err(_) => return Err(Denial::ExtentOverflow) };
            inserted.push(new_rank);
            tail[(match usize::try_from(self.delta.sorted[j]) { Ok(n) => n, Err(_) => 0 })] = new_rank;
        }

        // Drain the delta into a lawful run (sorted + unique by the
        // delta's own dedup) and cascade geometrically. Cascades are
        // rank-invariant.
        let delta = std::mem::replace(&mut self.delta, Delta::new());
        if delta_n > 0 {
            let entries: Vec<Entry> = delta
                .sorted
                .iter()
                .map(|&i| delta.arrivals[(match usize::try_from(i) { Ok(n) => n, Err(_) => 0 })])
                .collect();
            self.runs
                .push(Arc::new(Run::from_sorted(entries, &self.heap)?));
            while let Some([a, b]) = take_geometric_merge_pair(&mut self.runs) {
                let merged = Run::merge(&a, &b, &self.heap)?;
                self.runs.push(Arc::new(merged));
            }
            self.sealed_len += delta_n;
        }

        self.epoch = Epoch(self.epoch.0.checked_add(1).ok_or(Denial::ExtentOverflow)?);
        Ok(EpochRemap {
            arena: self.id,
            from,
            to: self.epoch,
            from_sealed_len,
            inserted,
            tail,
        })
    }
}

/// When a geometric cascade is due, take the last two runs as
/// `[Arc<Run>; 2]`. Returns `None` if fewer than two runs exist or the
/// geometric stop holds (`prev > 2 * last`). The pair comes from
/// `split_off` + array conversion — pop failure is unrepresentable.
fn take_geometric_merge_pair(runs: &mut Vec<Arc<Run>>) -> Option<[Arc<Run>; 2]> {
    let n = runs.len();
    if n < 2 {
        return None;
    }
    let last = runs[n - 1].len();
    let prev = runs[n - 2].len();
    if prev > 2 * last {
        return None;
    }
    let pair = runs.split_off(n - 2);
    match <[Arc<Run>; 2]>::try_from(pair) {
        Ok(arr) => Some(arr),
        Err(_len) => None,
    }
}

#[cfg(test)]
mod tests {
    use miette::{IntoDiagnostic, Result, miette};

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
            // INVARIANT(xorshift_finalizer): xorshift* final mul is defined wrapping on u64.
            (std::num::Wrapping(x) * std::num::Wrapping(0x2545_F491_4F6C_DD1D)).0
        }

        fn below(&mut self, n: usize) -> usize {
            match u64::try_from(n) {
                Ok(n_u) => match usize::try_from(self.next() % n_u) { Ok(v) => v, Err(_) => 0 },
                Err(_) => 0,
            }
        }
    }

    fn rand_value(rng: &mut Rng, alphabet: &[u8], max_len: usize) -> Vec<u8> {
        let len = rng.below(max_len + 1);
        (0..len)
            .map(|_| {
                if alphabet.is_empty() {
                    match u8::try_from(rng.next() & 0xFF) { Ok(b) => b, Err(_) => 0 }
                } else {
                    alphabet[rng.below(alphabet.len())]
                }
            })
            .collect()
    }

    /// In-plane stamp mint for law sweeps (tests are part of the plane's
    /// minting authority; production stamps come from intern/apply).
    fn stamp(c: usize, epoch: Epoch, arena: ArenaId) -> StampedCode {
        StampedCode::mint(Code(match u32::try_from(c) { Ok(v) => v, Err(_) => 0 }), epoch, arena, StampMintAuthority(()))
    }

    /// Test-plane intern — capacity/span [`Denial`] is unreachable at
    /// test sizes; production callers propagate [`Result`].
    #[track_caller]
    fn must_intern(arena: &mut Arena, b: &[u8]) -> Result<StampedCode> {
        Ok(Arena::intern(arena, b).into_diagnostic()?)
    }

    // ------------------------------------------------------------------
    // Naive oracle: the epoch-scoped interning architecture stated so
    // simply it is obviously correct. The arena must agree with it on
    // every operation, every epoch, and so must every snapshot, forever.
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
                return match u32::try_from(i) { Ok(v) => v, Err(_) => 0 };
            }
            if let Some(i) = self.tail.iter().position(|v| v.as_slice() == b) {
                return match u32::try_from(self.sealed.len() + i) { Ok(v) => v, Err(_) => 0 };
            }
            self.tail.push(b.to_vec());
            match u32::try_from(self.sealed.len() + self.tail.len() - 1) { Ok(v) => v, Err(_) => 0 }
        }

        fn resolve(&self, code: u32) -> &[u8] {
            let c = (match usize::try_from(code) { Ok(n) => n, Err(_) => 0 });
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
        fn seal(&mut self) -> Result<Vec<u32>> {
            let old: Vec<Vec<u8>> = self
                .sealed
                .iter()
                .chain(self.tail.iter())
                .cloned()
                .collect();
            let mut new_sealed = old.clone();
            new_sealed.sort();
            let mut remap = Vec::with_capacity(old.len());
            for v in &old {
                let idx = new_sealed
                    .binary_search_by(|x| x.as_slice().cmp(v))
                    .map_err(|_| miette!("survives seal"))?;
                remap.push(match u32::try_from(idx) { Ok(n) => n, Err(_) => 0 });
            }
            self.sealed = new_sealed;
            self.tail.clear();
            self.epoch += 1;
            Ok(remap)
        }
    }

    // ------------------------------------------------------------------
    // The laws, checked as full sweeps against the oracle.
    // ------------------------------------------------------------------

    fn check_laws(arena: &Arena, naive: &Naive) -> Result<()> {
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
            assert_eq!(
                f.resolve(stamp(c, f.epoch(), f.arena)).into_diagnostic()?,
                naive.resolve(match u32::try_from(c) { Ok(v) => v, Err(_) => 0 }),
                "code {c} resolves differently"
            );
        }
        // Sealed codes are strictly byte-ordered.
        let mut prev: Option<&[u8]> = None;
        for c in 0..f.sealed_len() {
            let v = f.resolve(stamp(c, f.epoch(), f.arena)).into_diagnostic()?;
            if let Some(p) = prev {
                assert!(p < v, "sealed order broken at {c}");
            }
            prev = Some(v);
        }
        // Global rank/select agree with the sorted union.
        let union = naive.union_sorted();
        for (k, v) in union.iter().enumerate() {
            assert_eq!(
                f.select(k).into_diagnostic()?,
                v.as_slice(),
                "select({k}) wrong"
            );
            assert_eq!(f.rank(v), Ok(Ok(k)), "rank of {v:?} wrong");
        }
        // cmp_codes is the byte order, over every live pair.
        for i in 0..f.len() {
            for j in 0..f.len() {
                let a = stamp(i, f.epoch(), f.arena);
                let b = stamp(j, f.epoch(), f.arena);
                assert_eq!(
                    f.cmp_codes(a, b).into_diagnostic()?,
                    naive.resolve(match u32::try_from(i) { Ok(v) => v, Err(_) => 0 }).cmp(naive.resolve(match u32::try_from(j) { Ok(v) => v, Err(_) => 0 })),
                    "cmp_codes({i},{j}) diverged from byte order"
                );
            }
        }
        Ok(())
    }

    /// Verify a pinned snapshot against a frozen copy of the oracle taken
    /// at the same moment — the "answers identically forever" law.
    fn check_snapshot(snap: &Snapshot, frozen: &Naive) -> Result<()> {
        assert_eq!(snap.len(), frozen.len(), "snapshot cardinality drifted");
        assert_eq!(
            snap.sealed_len(),
            frozen.sealed.len(),
            "snapshot boundary drifted"
        );
        assert_eq!(snap.epoch().0, frozen.epoch, "snapshot epoch drifted");
        for c in 0..snap.len() {
            assert_eq!(
                snap.resolve(stamp(c, snap.epoch(), snap.arena))
                    .into_diagnostic()?,
                frozen.resolve(match u32::try_from(c) { Ok(v) => v, Err(_) => 0 }),
                "snapshot code {c} drifted"
            );
        }
        let union = frozen.union_sorted();
        for (k, v) in union.iter().enumerate() {
            assert_eq!(
                snap.select(k).into_diagnostic()?,
                v.as_slice(),
                "snapshot select({k}) drifted"
            );
            assert_eq!(snap.rank(v), Ok(Ok(k)), "snapshot rank drifted");
        }
        if snap.len() <= 64 {
            for i in 0..snap.len() {
                for j in 0..snap.len() {
                    assert_eq!(
                        snap.cmp_codes(
                            stamp(i, snap.epoch(), snap.arena),
                            stamp(j, snap.epoch(), snap.arena)
                        )
                        .into_diagnostic()?,
                        frozen.resolve(match u32::try_from(i) { Ok(v) => v, Err(_) => 0 }).cmp(frozen.resolve(match u32::try_from(j) { Ok(v) => v, Err(_) => 0 })),
                        "snapshot cmp drifted"
                    );
                }
            }
        }
        Ok(())
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

    fn drive(ops: &[Op], sweep_every: usize) -> Result<()> {
        let mut arena = Arena::new();
        let mut naive = Naive::new();
        let mut pinned: Vec<(Snapshot, Naive)> = Vec::new();
        for (i, op) in ops.iter().enumerate() {
            match op {
                Op::Intern(b) => {
                    let sc = must_intern(&mut arena, b)?;
                    assert_eq!(sc.code().raw(), naive.intern(b), "op {i}: code diverged");
                    assert_eq!(sc.epoch(), arena.epoch(), "op {i}: stamp epoch wrong");
                    {
                        let f = arena.frame();
                        assert_eq!(
                            f.resolve(sc).into_diagnostic()?,
                            b.as_slice(),
                            "op {i}: round-trip"
                        );
                    }
                    // Dedup: immediate re-intern is a hit, no growth.
                    let n = arena.len();
                    assert_eq!(must_intern(&mut arena, b)?, sc, "op {i}: dedup");
                    assert_eq!(arena.len(), n, "op {i}: dedup grew arena");
                }
                Op::Seal => {
                    // Capture every live code's bytes before the
                    // transition.
                    let source_epoch = arena.epoch();
                    let live: Vec<Vec<u8>> = {
                        let f = arena.frame();
                        let mut live = Vec::with_capacity(f.len());
                        for c in 0..f.len() {
                            live.push(
                                f.resolve(stamp(c, source_epoch, f.arena))
                                    .into_diagnostic()?
                                    .to_vec(),
                            );
                        }
                        live
                    };
                    let from_sealed = arena.sealed_len();
                    let remap = arena.seal().into_diagnostic()?;
                    let expect = naive.seal()?;
                    assert_eq!(remap.source_epoch(), source_epoch);
                    assert_eq!(remap.target_epoch(), arena.epoch());
                    // The remap law: every old code, sealed or tail,
                    // reads the same bytes through the door — and the
                    // door restamps it into the new epoch.
                    let f = arena.frame();
                    for (old, bytes) in live.iter().enumerate() {
                        let new = remap
                            .apply(stamp(old, source_epoch, arena.id))
                            .into_diagnostic()?;
                        assert_eq!(
                            new.code().raw(),
                            expect[old],
                            "op {i}: remap diverged at {old}"
                        );
                        assert_eq!(new.epoch(), arena.epoch(), "op {i}: restamp wrong");
                        assert_eq!(
                            f.resolve(new).into_diagnostic()?,
                            bytes.as_slice(),
                            "op {i}: code {old} lost its value crossing the seal"
                        );
                    }
                    // Strictly monotone over the old sealed range.
                    let mut prev = None;
                    for old in 0..from_sealed {
                        let new = remap
                            .apply(stamp(old, source_epoch, arena.id))
                            .into_diagnostic()?
                            .code()
                            .raw();
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
                check_laws(&arena, &naive)?;
            }
        }
        check_laws(&arena, &naive)?;
        // Every snapshot still answers exactly as the world stood when it
        // was pinned.
        for (snap, frozen) in &pinned {
            check_snapshot(snap, frozen)?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Exhaustive: every intern sequence of length 3 over a 13-value
    // universe, under every seal placement, laws checked after every op.
    // ------------------------------------------------------------------

    #[test]
    fn laws_exhaustive_small_universe_all_seal_placements() -> Result<()> {
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
                        drive(&ops, 1)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// The same exhaustive core with a snapshot pinned at every possible
    /// position, verified after the drive has moved past it.
    #[test]
    fn laws_exhaustive_snapshot_placements() -> Result<()> {
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
                            drive(&ops, 2)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Randomized differentials: interleaved interns, seals, and
    // snapshots; three alphabets; dup-heavy; multi-epoch.
    // ------------------------------------------------------------------

    #[test]
    fn laws_random_differential_multi_epoch() -> Result<()> {
        for seed in 1u64..=9 {
            // INVARIANT(test_seed_mix): property-test seed diffusion uses modular golden mix.
            let mut rng = Rng((std::num::Wrapping(seed) * std::num::Wrapping(0x9E37_79B9_7F4A_7C15)).0);
            let alphabet: &[u8] = match seed % 3 {
                0 => &[0x00, 0x01],
                1 => b"abcdefghij",
                _other => &[],
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
            drive(&ops, 149)?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // The reviewer's exploit, pinned: a stamped code held across a seal
    // is refused by the new frame; only the remap door readmits it.
    // ------------------------------------------------------------------

    #[test]
    fn stale_stamp_is_refused_not_misread() -> Result<()> {
        let mut arena = Arena::new();
        let sc_b = must_intern(&mut arena, b"b")?;
        let remap = arena.seal().into_diagnostic()?;
        // Post-seal: intern something smaller so the old code's rank is
        // genuinely wrong if smuggled.
        must_intern(&mut arena, b"a")?;
        let f = arena.frame();
        assert!(
            !f.admits(sc_b),
            "stale stamp crossed a seal without the remap door"
        );
        let crossed = remap.apply(sc_b).into_diagnostic()?;
        assert!(f.admits(crossed), "remapped stamp admits");
        assert_eq!(f.resolve(crossed).into_diagnostic()?, b"b");
        Ok(())
    }

    #[test]
    fn remap_refuses_wrong_epoch_input() -> Result<()> {
        let mut arena = Arena::new();
        must_intern(&mut arena, b"x")?;
        let r1 = arena.seal().into_diagnostic()?;
        let sc_new = must_intern(&mut arena, b"y")?; // epoch 1
        assert!(
            matches!(r1.apply(sc_new), Err(Denial::EpochMismatch { .. })),
            "r1 reads epoch-0 codes only — typed refusal"
        );
        Ok(())
    }

    #[test]
    fn admits_is_exact_including_bounds() -> Result<()> {
        let mut arena = Arena::new();
        let sc = must_intern(&mut arena, b"x")?;
        let f = arena.frame();
        assert!(f.admits(sc));
        // A forged in-epoch stamp beyond the frame's length is NOT
        // spendable: admits must agree with what resolve would refuse.
        let forged = stamp(7, f.epoch(), f.arena);
        assert!(
            !f.admits(forged),
            "admits claimed spendability beyond bounds"
        );
        Ok(())
    }

    #[test]
    fn cross_arena_stamp_refuses_in_frame_spend() -> Result<()> {
        let mut a = Arena::new();
        let mut b = Arena::new();
        let sa = must_intern(&mut a, b"alpha")?;
        must_intern(&mut b, b"beta")?;
        assert!(
            matches!(b.frame().resolve(sa), Err(Denial::ArenaMismatch { .. })),
            "foreign-arena stamp must refuse typed"
        );
        Ok(())
    }

    /// Nest brands apply under one live frame: handle identity is lawful
    /// inside the nest, and the durable unbranded token remains available
    /// via [`NestedDomainCtx::ctx`] for coexisting-arena APIs.
    /// Consumable authority is spent by move into a bulk pass; the pass
    /// then amortizes many raw reads with zero remints.
    #[test]
    fn bulk_spend_authority_open_pass_is_consume_on_spend() -> Result<()> {
        let mut arena = Arena::new();
        let sc = must_intern(&mut arena, b"x")?;
        let f = arena.frame();
        let auth = BulkSpendAuthority::after_domain_admission();
        let pass = auth.open_pass(&f);
        assert_eq!(
            pass.resolve((match usize::try_from(sc.code().raw()) { Ok(n) => n, Err(_) => 0 })).into_diagnostic()?,
            b"x"
        );
        assert_eq!(
            pass.resolve((match usize::try_from(sc.code().raw()) { Ok(n) => n, Err(_) => 0 })).into_diagnostic()?,
            b"x"
        );
        // `auth` was moved into `open_pass` — a second use would be E0382.
        // Absence of Clone/Copy is locked in `super::proofs`.
        Ok(())
    }

    /// One-shot `resolve_raw` likewise consumes the authority by value.
    #[test]
    fn bulk_spend_authority_resolve_raw_is_consume_on_spend() -> Result<()> {
        let mut arena = Arena::new();
        let sc = must_intern(&mut arena, b"y")?;
        let f = arena.frame();
        let auth = BulkSpendAuthority::after_domain_admission();
        assert_eq!(
            f.resolve_raw((match usize::try_from(sc.code().raw()) { Ok(n) => n, Err(_) => 0 }), auth)
                .into_diagnostic()?,
            b"y"
        );
        // `auth` spent — reuse would be E0382 (see `super::proofs`).
        Ok(())
    }

    #[test]
    fn nested_ctx_brands_handle_identity_under_one_frame() -> Result<()> {
        let mut arena = Arena::new();
        let a = must_intern(&mut arena, b"a")?;
        let b = must_intern(&mut arena, b"b")?;
        let f = arena.frame();
        f.with_nested_ctx(|ctx| {
            assert!(ctx.same_handle(a.code(), a.code()));
            assert!(!ctx.same_handle(a.code(), b.code()));
            assert_eq!(
                ctx.cmp_identity(a.code(), b.code()),
                std::cmp::Ordering::Less
            );
            // Nest can project the durable mint-checked token; that token
            // is what coexisting-arena sites accept.
            let durable = ctx.ctx();
            assert_eq!(durable.arena(), f.arena);
            assert_eq!(durable.epoch(), f.epoch);
        });
        // Snapshot nest is the same shape.
        let mut arena = Arena::new();
        let sc = must_intern(&mut arena, b"x")?;
        let snap = arena.snapshot();
        snap.with_nested_ctx(|ctx| {
            assert!(ctx.same_handle(sc.code(), sc.code()));
        });
        Ok(())
    }

    #[test]
    fn stale_stamp_refuses_in_frame_spend() -> Result<()> {
        let mut arena = Arena::new();
        let sc = must_intern(&mut arena, b"x")?;
        arena.seal().into_diagnostic()?;
        assert!(
            matches!(arena.frame().resolve(sc), Err(Denial::EpochMismatch { .. })),
            "stale stamp must refuse typed — cross through the remap door"
        );
        Ok(())
    }

    #[test]
    fn snapshot_refuses_wrong_epoch_stamp() -> Result<()> {
        let mut arena = Arena::new();
        let sc = must_intern(&mut arena, b"x")?;
        let _remap = arena.seal().into_diagnostic()?;
        let snap = arena.snapshot();
        assert!(
            matches!(snap.resolve(sc), Err(Denial::EpochMismatch { .. })),
            "stamped epoch 0 into epoch-1 snapshot must refuse typed"
        );
        Ok(())
    }

    #[test]
    fn snapshot_refuses_codes_past_its_cut() -> Result<()> {
        let mut arena = Arena::new();
        must_intern(&mut arena, b"x")?;
        let snap = arena.snapshot();
        let later = must_intern(&mut arena, b"y")?; // same epoch, after the cut
        assert!(
            matches!(snap.resolve(later), Err(Denial::VisibilityOverflow { .. })),
            "same-epoch code past the snapshot cut must refuse typed"
        );
        Ok(())
    }

    // ------------------------------------------------------------------
    // Cross-arena rejection: two arenas at the same epoch must refuse each
    // other's stamps loudly, never resolve them to their own values.
    // ------------------------------------------------------------------

    #[test]
    fn cross_arena_stamps_are_refused_by_frames() -> Result<()> {
        let mut a = Arena::new();
        let mut b = Arena::new();
        let sa = must_intern(&mut a, b"alpha")?;
        let sb = must_intern(&mut b, b"beta")?;
        assert_eq!(a.epoch(), b.epoch(), "both at epoch 0: the dangerous case");
        let fb = b.frame();
        assert!(
            !fb.admits(sa),
            "arena A's stamp admitted into arena B's frame"
        );
        let fa = a.frame();
        assert!(
            !fa.admits(sb),
            "arena B's stamp admitted into arena A's frame"
        );
        Ok(())
    }

    #[test]
    fn cross_arena_stamp_refuses_in_snapshots() -> Result<()> {
        let mut a = Arena::new();
        let mut b = Arena::new();
        let sa = must_intern(&mut a, b"alpha")?;
        must_intern(&mut b, b"beta")?;
        assert!(
            matches!(b.snapshot().resolve(sa), Err(Denial::ArenaMismatch { .. })),
            "foreign-arena stamp in snapshot must refuse typed"
        );
        Ok(())
    }

    #[test]
    fn cross_arena_stamp_refuses_in_remaps() -> Result<()> {
        let mut a = Arena::new();
        let mut b = Arena::new();
        must_intern(&mut a, b"alpha")?;
        let sb = must_intern(&mut b, b"beta")?;
        let remap = a.seal().into_diagnostic()?;
        assert!(
            matches!(remap.apply(sb), Err(Denial::ArenaMismatch { .. })),
            "foreign-arena stamp in remap must refuse typed"
        );
        Ok(())
    }

    // ------------------------------------------------------------------
    // The same-epoch coherence law: observers of one epoch agree on every
    // code both can see, whatever their cuts.
    // ------------------------------------------------------------------

    #[test]
    fn same_epoch_observers_agree_on_shared_codes() -> Result<()> {
        let mut arena = Arena::new();
        must_intern(&mut arena, b"m")?;
        arena.seal().into_diagnostic()?;
        let a = must_intern(&mut arena, b"zz")?;
        let early = arena.snapshot();
        let b = must_intern(&mut arena, b"aa")?;
        let late = arena.snapshot();
        // Codes visible to both answer identically in both.
        assert_eq!(
            early.resolve(a).into_diagnostic()?,
            late.resolve(a).into_diagnostic()?
        );
        assert_eq!(early.resolve(a).into_diagnostic()?, b"zz");
        // The later observer sees more; the earlier refuses what it
        // cannot see (tested above); nothing shared ever disagrees.
        assert_eq!(late.resolve(b).into_diagnostic()?, b"aa");
        let f = arena.frame();
        assert_eq!(f.resolve(a).into_diagnostic()?, b"zz");
        Ok(())
    }

    // ------------------------------------------------------------------
    // The fixpoint contract: tail codes are arrival-stable and
    // equality-exact for the whole epoch, whatever is interned around
    // them.
    // ------------------------------------------------------------------

    #[test]
    fn tail_codes_are_arrival_stable_within_an_epoch() -> Result<()> {
        let mut arena = Arena::new();
        must_intern(&mut arena, b"m")?;
        arena.seal().into_diagnostic()?;
        let c_z = must_intern(&mut arena, b"z")?;
        let c_a = must_intern(&mut arena, b"a")?; // smaller than everything sealed
        let c_q = must_intern(&mut arena, b"q")?;
        // Interning smaller values did not move earlier stamps.
        assert_eq!(must_intern(&mut arena, b"z")?, c_z);
        assert_eq!(must_intern(&mut arena, b"a")?, c_a);
        assert_eq!(must_intern(&mut arena, b"q")?, c_q);
        // Tail codes are consecutive arrivals above the sealed range.
        assert_eq!(c_z.code().raw(), 1);
        assert_eq!(c_a.code().raw(), 2);
        assert_eq!(c_q.code().raw(), 3);
        // The order authority is rank(), not tail-code arithmetic.
        let f = arena.frame();
        assert_eq!(f.rank(b"a"), Ok(Ok(0)));
        assert_eq!(f.rank(b"m"), Ok(Ok(1)));
        assert_eq!(f.rank(b"q"), Ok(Ok(2)));
        assert_eq!(f.rank(b"z"), Ok(Ok(3)));
        Ok(())
    }

    #[test]
    fn seal_remap_carries_sealed_and_tail_codes() -> Result<()> {
        let mut arena = Arena::new();
        let mut held: Vec<(StampedCode, Vec<u8>)> = Vec::new();
        for v in [b"delta".as_slice(), b"alpha", b"omega"] {
            held.push((must_intern(&mut arena, v)?, v.to_vec()));
        }
        let r1 = arena.seal().into_diagnostic()?;
        for (sc, _) in held.iter_mut() {
            *sc = r1.apply(*sc).into_diagnostic()?;
        }
        for v in [b"aaaa".as_slice(), b"zzzz"] {
            held.push((must_intern(&mut arena, v)?, v.to_vec()));
        }
        let r2 = arena.seal().into_diagnostic()?;
        let f = arena.frame();
        for (sc, v) in &held {
            let crossed = r2.apply(*sc).into_diagnostic()?;
            assert_eq!(f.resolve(crossed).into_diagnostic()?, v.as_slice());
        }
        // Post-seal: dense byte order over all five.
        let mut all: Vec<&[u8]> = Vec::with_capacity(f.len());
        for c in 0..f.len() {
            all.push(f.resolve(stamp(c, f.epoch(), f.arena)).into_diagnostic()?);
        }
        let mut sorted = all.clone();
        sorted.sort();
        assert_eq!(all, sorted, "sealed codes are byte-ordered after seal");
        Ok(())
    }

    #[test]
    fn observer_cmp_derefs_only_on_prefix_tie() -> Result<()> {
        let mut arena = Arena::new();
        // Delta (unsealed) codes: comparison must go prefix-first.
        let a = must_intern(&mut arena, b"AAAA-tail-1")?; // distinct prefix
        let b = must_intern(&mut arena, b"BBBB-tail-2")?; // distinct prefix
        let c = must_intern(&mut arena, b"SAME-tail-x")?; // shared prefix with d
        let d = must_intern(&mut arena, b"SAME-tail-y")?;
        let f = arena.frame();

        let base = arena.compare_derefs();
        assert_eq!(f.cmp_codes(a, b).into_diagnostic()?, std::cmp::Ordering::Less);
        assert_eq!(
            arena.compare_derefs() - base,
            0,
            "distinct-prefix compare dereferenced payload"
        );

        let base = arena.compare_derefs();
        assert_eq!(f.cmp_codes(c, d).into_diagnostic()?, std::cmp::Ordering::Less);
        assert!(
            arena.compare_derefs() > base,
            "shared-prefix tie must deref to break the tie"
        );
        Ok(())
    }

    /// The sealed fast lane: sorting an all-sealed CodeColumn is RAW
    /// NUMERIC order over codes -- sealed codes ARE global ranks == byte
    /// order -- so it compares u32s and dereferences the arena ZERO times,
    /// no matter how many prefixes tie. This is the execution currency: no
    /// durable-encoding work in the ordered-iteration hot path.
    #[test]
    fn sealed_code_column_sort_never_derefs() -> Result<()> {
        use super::super::column::CodeColumn;
        let mut arena = Arena::new();
        // Many shared-prefix values (would all be prefix-ties under a
        // byte compare) plus distinct ones.
        let mut interned = Vec::new();
        for i in 0..500u32 {
            interned.push(must_intern(&mut arena, format!("SAME-{i:08}").as_bytes())?);
        }
        for i in 0..500u32 {
            interned.push(must_intern(&mut arena, &i.to_be_bytes())?);
        }
        let remap = arena.seal().into_diagnostic()?;
        let mut codes = Vec::with_capacity(interned.len());
        for c in interned {
            codes.push(remap.apply(c).into_diagnostic()?);
        }
        let f = arena.frame();
        let mut col = CodeColumn::new_in(&f);
        for c in &codes {
            col.push(*c).into_diagnostic()?;
        }
        let base = arena.compare_derefs();
        let perm = col
            .admit(&f)
            .into_diagnostic()?
            .sort_permutation()
            .into_diagnostic()?;
        assert_eq!(perm.len(), 1000);
        assert_eq!(
            arena.compare_derefs() - base,
            0,
            "the sealed fast lane must sort by raw code order with zero derefs"
        );
        // And the order it produced is the true value (byte) order.
        let mut ranks: Vec<usize> = Vec::with_capacity(perm.len());
        for &i in &perm {
            let idx = match usize::try_from(i) {
                Ok(n) => n,
                Err(_) => 0,
            };
            ranks.push(
                f.rank(f.resolve(codes[idx]).into_diagnostic()?)
                    .into_diagnostic()?
                    .map_err(|_| miette!("found"))?,
            );
        }
        assert!(
            ranks.windows(2).all(|w| w[0] <= w[1]),
            "sealed sort not value-ordered"
        );
        Ok(())
    }

    #[test]
    fn distinct_prefix_compares_never_deref() -> Result<()> {
        let mut arena = Arena::new();
        for i in 0..2000u32 {
            let mut v = i.to_be_bytes().to_vec();
            v.extend_from_slice(b"-payload-tail");
            must_intern(&mut arena, &v)?;
        }
        arena.seal().into_diagnostic()?;
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
        must_intern(&mut arena, &v)?;
        assert!(
            arena.compare_derefs() > before,
            "equality tie never counted"
        );
        let before = arena.compare_derefs();
        must_intern(&mut arena, b"same-prefix-AAAA")?;
        must_intern(&mut arena, b"same-prefix-BBBB")?;
        assert!(
            arena.compare_derefs() > before,
            "shared-prefix tie never counted"
        );
        Ok(())
    }

    // ------------------------------------------------------------------
    // Scale: multiple epochs, pathological insertion orders, 100k values,
    // held stamps crossed through every boundary, snapshots verified
    // after the writer has moved 90k+ values past them.
    // ------------------------------------------------------------------

    fn stress(values: Vec<Vec<u8>>, seal_every: usize) -> Result<()> {
        let mut arena = Arena::new();
        let mut live: Vec<(StampedCode, usize)> = Vec::new();
        let mut pinned: Option<(Snapshot, Vec<Vec<u8>>)> = None;
        for (i, v) in values.iter().enumerate() {
            let sc = must_intern(&mut arena, v)?;
            live.push((sc, i));
            if (i + 1) % seal_every == 0 {
                let remap = arena.seal().into_diagnostic()?;
                for (sc, _) in live.iter_mut() {
                    *sc = remap.apply(*sc).into_diagnostic()?;
                }
            }
            if i + 1 == seal_every / 2 {
                // Pin one early snapshot with its expected contents.
                let expect: Vec<Vec<u8>> = {
                    let f = arena.frame();
                    let mut expect = Vec::with_capacity(f.len());
                    for c in 0..f.len() {
                        expect.push(
                            f.resolve(stamp(c, f.epoch(), f.arena))
                                .into_diagnostic()?
                                .to_vec(),
                        );
                    }
                    expect
                };
                pinned = Some((arena.snapshot(), expect));
            }
        }
        {
            let f = arena.frame();
            for (sc, i) in &live {
                assert_eq!(
                    f.resolve(*sc).into_diagnostic()?,
                    values[*i].as_slice(),
                    "stamp lost across epochs"
                );
            }
        }
        let final_remap = arena.seal().into_diagnostic()?;
        let f = arena.frame();
        for (sc, i) in live.iter_mut() {
            *sc = final_remap.apply(*sc).into_diagnostic()?;
            assert_eq!(f.resolve(*sc).into_diagnostic()?, values[*i].as_slice());
        }
        let mut expected = values;
        expected.sort();
        expected.dedup();
        assert_eq!(f.len(), expected.len());
        for (k, v) in expected.iter().enumerate() {
            assert_eq!(
                f.resolve(stamp(k, f.epoch(), f.arena)).into_diagnostic()?,
                v.as_slice(),
                "rank {k} wrong at scale"
            );
            assert_eq!(f.rank(v), Ok(Ok(k)));
        }
        // The early snapshot still answers its pinned world exactly.
        if let Some((snap, expect)) = pinned {
            assert_eq!(snap.len(), expect.len(), "snapshot drifted at scale");
            for (c, v) in expect.iter().enumerate() {
                assert_eq!(
                    snap.resolve(stamp(c, snap.epoch(), snap.arena))
                        .into_diagnostic()?,
                    v.as_slice()
                );
            }
        }
        Ok(())
    }

    #[test]
    fn stress_ascending_100k_multi_epoch() -> Result<()> {
        stress(
            (0u32..100_000).map(|i| i.to_be_bytes().to_vec()).collect(),
            9_973,
        )?;
        Ok(())
    }

    #[test]
    fn stress_descending_100k_multi_epoch() -> Result<()> {
        stress(
            (0u32..100_000)
                .rev()
                .map(|i| i.to_be_bytes().to_vec())
                .collect(),
            9_973,
        )?;
        Ok(())
    }

    #[test]
    fn stress_random_dups_100k_multi_epoch() -> Result<()> {
        let mut rng = Rng(0x5EED);
        stress(
            (0..100_000)
                .map(|_| (rng.next() % 60_000).to_be_bytes().to_vec())
                .collect(),
            7_919,
        )?;
        Ok(())
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
    fn empty_seal_advances_epoch_and_is_identity() -> Result<()> {
        let mut arena = Arena::new();
        let sc = must_intern(&mut arena, b"x")?;
        let r1 = arena.seal().into_diagnostic()?;
        assert_eq!(r1.tail_len(), 1);
        let crossed = r1.apply(sc).into_diagnostic()?;
        let r2 = arena.seal().into_diagnostic()?;
        assert_eq!(arena.epoch(), Epoch(2));
        assert_eq!(r2.tail_len(), 0);
        let twice = r2.apply(crossed).into_diagnostic()?;
        assert_eq!(
            twice.code().raw(),
            crossed.code().raw(),
            "empty seal moved a code"
        );
        let f = arena.frame();
        assert_eq!(f.resolve(twice).into_diagnostic()?, b"x");
        Ok(())
    }

    #[test]
    fn empty_string_is_a_value_across_epochs() -> Result<()> {
        let mut arena = Arena::new();
        let sc = must_intern(&mut arena, b"")?;
        assert_eq!(sc.code().raw(), 0);
        let remap = arena.seal().into_diagnostic()?;
        let crossed = remap.apply(sc).into_diagnostic()?;
        assert_eq!(crossed.code().raw(), 0);
        let f = arena.frame();
        assert_eq!(f.resolve(crossed).into_diagnostic()?, b"");
        Ok(())
    }

    #[test]
    fn values_around_chunk_boundaries_round_trip() -> Result<()> {
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
            let v: Vec<u8> = (0..len).map(|i| match u8::try_from(i % 251) { Ok(b) => b, Err(_) => 0 }).collect();
            held.push((must_intern(&mut arena, &v)?, v));
        }
        // Fill across many shared chunks too.
        for i in 0..40_000u32 {
            must_intern(&mut arena, format!("filler-{i}").as_bytes())?;
        }
        {
            let f = arena.frame();
            for (sc, v) in &held {
                assert_eq!(f.resolve(*sc).into_diagnostic()?, v.as_slice());
            }
        }
        let remap = arena.seal().into_diagnostic()?;
        let f = arena.frame();
        for (sc, v) in &held {
            assert_eq!(
                f.resolve(remap.apply(*sc).into_diagnostic()?)
                    .into_diagnostic()?,
                v.as_slice()
            );
        }
        Ok(())
    }

    #[test]
    fn snapshot_survives_writer_progress_and_chunk_freezes() -> Result<()> {
        let mut arena = Arena::new();
        for i in 0..5_000u32 {
            must_intern(&mut arena, format!("v-{i:05}").as_bytes())?;
        }
        arena.seal().into_diagnostic()?;
        must_intern(&mut arena, b"tail-one")?;
        let snap = arena.snapshot();
        let mut world: Vec<Vec<u8>> = Vec::with_capacity(snap.len());
        for c in 0..snap.len() {
            world.push(
                snap.resolve(stamp(c, snap.epoch(), snap.arena))
                    .into_diagnostic()?
                    .to_vec(),
            );
        }
        // Writer moves far past the snapshot: new values, seals, chunk
        // rollovers, cascades.
        for round in 0..3 {
            for i in 0..5_000u32 {
                must_intern(&mut arena, format!("post-{round}-{i}").as_bytes())?;
            }
            arena.seal().into_diagnostic()?;
        }
        for (c, v) in world.iter().enumerate() {
            assert_eq!(
                snap.resolve(stamp(c, snap.epoch(), snap.arena))
                    .into_diagnostic()?,
                v.as_slice(),
                "snapshot drifted"
            );
        }
        Ok(())
    }

    #[test]
    fn forged_in_epoch_stamp_beyond_len_refuses_typed() -> Result<()> {
        let mut arena = Arena::new();
        must_intern(&mut arena, b"x")?;
        let f = arena.frame();
        // A forged in-epoch stamp beyond len is a visibility refusal —
        // same vocabulary as cut overflow, never a process abort.
        assert!(
            matches!(
                f.resolve(stamp(7, f.epoch(), f.arena)),
                Err(Denial::VisibilityOverflow { .. })
            ),
            "forged beyond-len stamp must refuse typed"
        );
        Ok(())
    }

    #[test]
    fn select_out_of_range_refuses_typed() {
        let arena = Arena::new();
        assert!(
            matches!(
                arena.frame().select(0),
                Err(Denial::VisibilityOverflow {
                    required: 1,
                    visible: 0
                })
            ),
            "OOB select must refuse typed — never abort the process"
        );
    }
}
