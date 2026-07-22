/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (MPL-2.0): the meet operations of a [`MeetAggrStore`] are resolved once,
 * at construction, into live `MeetAggrObj`s (the original stored
 * `Option<Box<dyn MeetAggrObj>>` on every `Aggregation` and unwrapped it
 * per row); handing a normal-only aggregation to a meet store is a
 * constructor error, not a downstream panic. `merge_in` is the **admission
 * seam**: it takes an [`AdmissionSink`] (with `()` as the zero-cost
 * off-state) and returns the [`Admitted`] count, per the ratified
 * provenance and budget designs (story #3) — the seam only, no provenance
 * or accounting logic lives here. The kind-mismatch `unreachable!` in
 * `EpochStore::merge_in` is a typed internal error; the meet range scan
 * compares slices through `DataValue`'s total order instead of
 * `partial_cmp(..).unwrap()`. `either::{Left, Right}` becomes
 * `itertools::Either` (the workspace carries no direct `either`
 * dependency). The ported meet forms take no arguments (see `data/aggr.rs`),
 * so the argument lists in the constructor spec are carried for eval's
 * interface and ignored here. A `MeetAggrStore` groups by the head's
 * non-aggregated positions **wherever they sit** (a constructed
 * `MeetLayout` proof) — full oracle positional-meet parity
 * (`query/laws.rs`). Fold/delta authority is solely `by_group`; head-tuple
 * order for joins is derived via [`MeetLayout::interleave`] at scan time
 * (P036 — no `by_row` twin). The delta propagation in `MeetAggrStore`
 * rides the *corrected* `MeetAggrObj::update` changed-flag contract — the
 * original's inverted `and`/`or` flags could announce "unchanged" on
 * exactly the update that changed a value and reach a premature fixpoint;
 * the store-level regression is pinned by a test below. The original file
 * had no unit tests; all tests here are new.
 */

//! The delta stores: the engine's working memory during the fixpoint.
//!
//! Every rule of a magic program evaluates into one of these stores, and
//! the total/delta discipline of [`EpochStore`] **is** semi-naive
//! evaluation:
//!
//! - `total` holds every tuple the rule has derived in any epoch so far;
//! - `delta` holds exactly the tuples that are *new as of the latest
//!   epoch's merge* — a new key for a regular store, a new group or a
//!   changed meet value for a meet store.
//!
//! Each epoch, eval joins at least one body atom of every recursive rule
//! against a `delta` instead of a `total` (see `query/eval.rs` and the
//! delta-driven iterators in `query/ra.rs`), merges each rule's freshly
//! derived tuples back in with [`EpochStore::merge_in`], and stops when no
//! store has a delta ([`EpochStore::has_delta`] is false for all). Because
//! a tuple enters `delta` exactly when it first enters `total`, and a
//! derivation whose premises are all old was already produced in an
//! earlier epoch, the iteration reaches **the same fixpoint as naive
//! evaluation** — that equivalence is the semi-naive law, and empty deltas
//! everywhere are the termination certificate.
//!
//! The three stores:
//!
//! - [`RegularTempStore`]: a set of tuples (plus a per-tuple [`LimiterSkip`]
//!   disposition — named Include/PastLimit, never a bare `bool` (P093) — for
//!   early-returned entry rules). Own-byte keys are [`OwnBareKey`]
//!   sealed at the encode door (P094); foreign bytes are unrepresentable.
//! - [`MeetAggrStore`]: grouped tuples folded through meet (semilattice)
//!   aggregations as they arrive; recursion through such aggregations is
//!   sound because the fold is idempotent, associative, and monotone in
//!   its lattice (see `data/aggr.rs`). Its delta is driven by the
//!   `MeetAggrObj::update` changed flag.
//! - [`EpochStore`]: the total/delta pair, one per rule, keyed by
//!   `MagicSymbol` in eval's store map and dropped per `StoreLifetimes`
//!   (`data/program.rs`) when no later stratum reads them.
//!
//! # The admission seam (provenance & budget, story #3)
//!
//! A tuple is **admitted** when it first enters a store's `total`: a
//! vacant-key insert, a whole-store fast-path swap into an empty total, or
//! a meet update that changed a group's value. Admission happens only
//! inside `merge_in`, at the epoch barrier, where eval merges the epoch's
//! out-stores sequentially and each merge walks the incoming store in
//! canonical key order — so the sequence of admissions is
//! schedule-independent. That makes this the deterministic point where
//! both ratified designs attach:
//!
//! - **Provenance**: first-witness recording binds a witness to each
//!   admitted tuple. `merge_in` takes an [`AdmissionSink`] which observes
//!   every admission in canonical order; `()` is the recording-off state
//!   and compiles to nothing.
//! - **Budget**: derived-tuple accounting counts admissions. `merge_in`
//!   returns [`Admitted`], the count of derivations admitted by that
//!   merge; eval sums these per epoch and checks the ceiling at the epoch
//!   barrier, where the total is deterministic.
//!
//! Only the seam lives here. Witness construction and ceiling checks land
//! with eval's port.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt::{Debug, Formatter};

use itertools::{Either, Itertools};
use miette::{Diagnostic, Result, bail, ensure, miette};
use thiserror::Error;

use crate::exec::fold::aggr::{MeetAccum, MeetAggr};
use kyzo_model::program::aggregate::Aggregation;
use kyzo_model::program::rule::HeadAggrSlot;
use kyzo_model::value::DataValue;
use kyzo_model::value::{
    DecodeError, ScanBound, Tuple, bare_bounds_lower, bare_bounds_upper, bare_prefix_len,
    decode_tuple_bare, encode_tuple_bare,
};

#[derive(Debug, Error, Diagnostic)]
#[error("level arena overflow: flattened byte length {len} exceeds u32::MAX")]
#[diagnostic(code(query::level_arena_overflow))]
struct LevelArenaOverflow {
    len: usize,
}

/// A level-stack proof failed (peek/next or merge winner) — typed refuse,
/// never an abort on the product path.
#[derive(Debug, Error, Diagnostic)]
#[error("level invariant violated: {0}")]
#[diagnostic(code(query::level_invariant), help("This is a bug. Please report it."))]
struct LevelInvariantError(&'static str);

// ─────────────────────────────────────────────────────────────────────────
// The level tier: a rule's total as sealed sorted runs
// ─────────────────────────────────────────────────────────────────────────
//
// A rule's TOTAL is a stack of immutable sorted levels, one sealed per
// epoch barrier (plus compaction products). Between barriers nothing
// mutates, so every scan is a dense walk and every probe a binary search
// — the shape the semi-naive inner loop wants, instead of pointer-chasing
// a tree that is rebalancing under an insert-heavy fixpoint.
//
// THE DELTA IS THE NEWEST LEVEL: a tuple enters a level exactly when the
// barrier admits it, so the level and the epoch's delta are the same
// bytes (flag-refresh rows are the one exception — present in the level
// to carry the refreshed flag, marked so delta iteration skips them).
// Shadowing resolves duplicates across levels: the NEWEST level owning a
// key (Normal) or a group (Meet) speaks for it; compaction merges levels
// newest-wins and drops shadowed copies. Meet folds happen AT the
// barrier: the folded value is written into the new level and older
// copies of the group are shadowed — a group's value is always whole in
// one level, never split across levels.

/// One sealed level of a normal rule's total.
///
/// Story #77 chunk 2: rows are memcmp bytes (chunk 1's bare codec), not
/// `DataValue`s — `values` is a byte arena, `offsets[i]` a byte position.
/// Comparisons become raw byte comparisons (`Ord` on `[u8]`), which
/// `encode_tuple_bare`'s order-embedding law makes IDENTICAL to the
/// `DataValue`-slice comparisons this replaces, without decoding the
/// stored side at all — a probe value is encoded once per call, not once
/// per row visited.
/// Flattened row-byte arena for a normal query level.
/// Field is private: arbitrary `Vec<u8>` is not a level arena (P028).
/// Mint empty via [`Self::new`]; grow only via branded append doors
/// ([`Self::append_bare`], [`Self::append_row`]). No Deref/AsRef<[u8]>.
#[derive(Debug, Clone, PartialEq, Eq)]
#[repr(transparent)]
pub(crate) struct LevelArenaBytes(Vec<u8>);

const _: () = assert!(std::mem::size_of::<LevelArenaBytes>() == std::mem::size_of::<Vec<u8>>());
const _: () = assert!(std::mem::align_of::<LevelArenaBytes>() == std::mem::align_of::<Vec<u8>>());

/// Borrowed row extent inside a sealed [`LevelArenaBytes`].
/// Mintable only by [`NormalLevel::row_at`] — never from foreign `&[u8]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LevelRowRef<'a>(&'a [u8]);

impl<'a> LevelRowRef<'a> {
    /// Named peel — no Deref/AsRef<[u8]> silent coerce.
    pub(crate) fn as_bytes(self) -> &'a [u8] {
        self.0
    }

    /// Named peel alias.
    pub(crate) fn as_slice(self) -> &'a [u8] {
        self.0
    }
}

impl LevelArenaBytes {
    /// Empty arena — the only open mint beside branded append doors.
    pub(crate) fn new() -> Self {
        Self(Vec::new())
    }

    /// Named peel — no Deref/AsRef<[u8]> silent coerce.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Named peel alias.
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Append a bare-encoded key minted by [`OwnBareKey::encode`].
    pub(crate) fn append_bare(&mut self, row: &OwnBareKey) {
        self.0.extend_from_slice(row.as_bytes());
    }

    /// Append a row extent that only [`NormalLevel::row_at`] can mint.
    pub(crate) fn append_row(&mut self, row: LevelRowRef<'_>) {
        self.0.extend_from_slice(row.as_bytes());
    }
}

/// Inclusive/exclusive scan bound key bytes for stored levels.
/// Mint only via the bare-bound / bare-tuple encode doors below.
#[derive(Debug, Clone, PartialEq, Eq)]
#[repr(transparent)]
pub(crate) struct LevelBoundKey(Vec<u8>);

const _: () = assert!(std::mem::size_of::<LevelBoundKey>() == std::mem::size_of::<Vec<u8>>());
const _: () = assert!(std::mem::align_of::<LevelBoundKey>() == std::mem::align_of::<Vec<u8>>());

impl LevelBoundKey {
    /// Lower scan bound through [`bare_bounds_lower`].
    pub(crate) fn from_lower_bounds(bounds: &[ScanBound]) -> Self {
        Self(bare_bounds_lower(bounds))
    }

    /// Upper scan bound through [`bare_bounds_upper`].
    pub(crate) fn from_upper_bounds(bounds: &[ScanBound]) -> Self {
        Self(bare_bounds_upper(bounds))
    }

    /// Prefix / full-tuple bound through [`encode_tuple_bare`].
    pub(crate) fn from_tuple_prefix(prefix: &[DataValue]) -> Self {
        Self(encode_tuple_bare(prefix))
    }

    /// Extend an exclusive upper bound (successor sentinel byte).
    pub(crate) fn push_exclusive_sentinel(&mut self) {
        self.0.push(0xFF);
    }

    /// Named peel — no Deref/AsRef<[u8]> silent coerce.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Whether a sealed normal-level row is an epoch admission or a
/// flag-refresh shadow (P027). Bare `bool` refresh is unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlagRefresh {
    /// Admitted this epoch — visible to delta iteration.
    Admitted,
    /// Present only to carry a refreshed limiter flag; invisible to delta.
    Refresh,
}

impl FlagRefresh {
    #[inline]
    pub(crate) fn hides_from_delta(self) -> bool {
        matches!(self, Self::Refresh)
    }
}

/// Per-row skip/refresh flags for a sealed normal level. Limiter disposition
/// is [`LimiterSkip`] (P093); compiled-position / peeked-iterator expects
/// are `INVARIANT(...)` at the seal (P095). Refresh is [`FlagRefresh`] —
/// never an anonymous `(bool, bool)` soup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RowFlags {
    pub(crate) skip: LimiterSkip,
    pub(crate) refresh: FlagRefresh,
}

/// Non-empty sealed-level stack (P030): `bottom` is always present, so an
/// empty stack is unrepresentable and [`Self::last`] needs no `expect`.
#[derive(Debug)]
pub(crate) struct LevelStack<L> {
    /// Oldest level — always present.
    bottom: L,
    /// Strictly newer levels, oldest-first; the last element is newest.
    above: Vec<L>,
}

impl<L> LevelStack<L> {
    pub(crate) fn singleton(first: L) -> Self {
        Self {
            bottom: first,
            above: Vec::new(),
        }
    }

    #[inline]
    pub(crate) fn last(&self) -> &L {
        match self.above.last() {
            Some(l) => l,
            None => &self.bottom,
        }
    }

    pub fn len(&self) -> usize {
        self.above.len() + 1
    }

    /// Push a newer level. `bottom` stays the oldest.
    pub(crate) fn push(&mut self, level: L) {
        self.above.push(level);
    }

    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = &L> + Clone {
        std::iter::once(&self.bottom).chain(self.above.iter())
    }

    /// Drop the newest level when it is empty and an older level remains.
    pub(crate) fn drop_consumed_empty_delta(&mut self, is_empty: impl Fn(&L) -> bool) {
        if let Some(top) = self.above.last()
            && is_empty(top)
        {
            self.above.pop();
        }
    }

    /// Compact while the newest level is at least half the size of the one
    /// beneath it. `merge` borrows both levels so a refuse cannot empty the
    /// stack — pops happen only after a successful merge.
    pub(crate) fn compact_while(
        &mut self,
        should_merge: impl Fn(&L, &L) -> bool,
        mut merge: impl FnMut(&L, &L) -> Result<L>,
    ) -> Result<()> {
        while !self.above.is_empty() {
            let older_was_bottom = self.above.len() == 1;
            if older_was_bottom {
                if !should_merge(&self.bottom, &self.above[0]) {
                    break;
                }
                let merged = merge(&self.bottom, &self.above[0])?;
                self.above.pop();
                self.bottom = merged;
            } else {
                let n = self.above.len();
                if !should_merge(&self.above[n - 2], &self.above[n - 1]) {
                    break;
                }
                let merged = merge(&self.above[n - 2], &self.above[n - 1])?;
                self.above.pop();
                self.above.pop();
                self.above.push(merged);
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct NormalLevel {
    /// Rows FLATTENED into one dense byte arena: a probe or scan walks
    /// contiguous memory instead of chasing one heap allocation per row.
    /// Rows ascend by memcmp bytes; per row, `skip` is the limiter flag and
    /// `refresh` marks a re-derived row present only to carry a
    /// refreshed flag — shadowing, not admitted, invisible to delta
    /// iteration.
    values: LevelArenaBytes,
    /// `offsets[i]` is the END of row `i` in `values` (row 0 starts at 0).
    offsets: Vec<u32>,
    flags: Vec<RowFlags>,
}

impl NormalLevel {
    /// Empty sealed level — no rows.
    pub(crate) fn empty() -> Self {
        Self {
            values: LevelArenaBytes::new(),
            offsets: Vec::new(),
            flags: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.offsets.len()
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }
    /// Row bytes at index `i`, or `None` when out of bounds.
    pub(crate) fn row(&self, i: usize) -> Option<LevelRowRef<'_>> {
        (i < self.len()).then(|| self.row_at(i))
    }

    /// INVARIANT(level_row_in_bounds): `i < self.len()`.
    /// The sole mint of [`LevelRowRef`] — foreign `&[u8]` cannot enter the arena.
    fn row_at(&self, i: usize) -> LevelRowRef<'_> {
        // Bound is the caller's proof — public door is `row()` → None OOB.
        // Offset/slice indexing refuses OOB in every build (not debug-only).
        let (start, end) = crate::project::current::offset_row_span(&self.offsets, i);
        LevelRowRef(&self.values.as_bytes()[start..end])
    }

    /// INVARIANT(level_row_in_bounds): `i < self.len()`.
    fn row_flags_at(&self, i: usize) -> RowFlags {
        // Same bound proof as `row_at`; flags len tracks offsets len by push.
        self.flags[i]
    }
    /// Seal an admitted row's bytes into the level — one arena append, no
    /// per-row heap allocation beyond the byte string `RegularTempStore`
    /// already minted at derivation. Refuses when the arena end would
    /// overflow the `u32` offset encoding.
    fn push(&mut self, row: Box<OwnBareKey>, flags: RowFlags) -> Result<()> {
        self.values.append_bare(&row);
        let end = u32::try_from(self.values.len()).map_or_else(
            |_| {
                bail!(LevelArenaOverflow {
                    len: self.values.len()
                })
            },
            Ok,
        )?;
        self.offsets.push(end);
        self.flags.push(flags);
        Ok(())
    }

    /// Copy a row across a compaction merge (the survivor outlives its
    /// source level, so this one copy per compacted row is the cost of
    /// dropping the shadowed copy).
    fn push_from(&mut self, other: &NormalLevel, i: usize, flags: RowFlags) -> Result<()> {
        self.values.append_row(other.row_at(i));
        let end = u32::try_from(self.values.len()).map_or_else(
            |_| {
                bail!(LevelArenaOverflow {
                    len: self.values.len()
                })
            },
            Ok,
        )?;
        self.offsets.push(end);
        self.flags.push(flags);
        Ok(())
    }
    fn find(&self, key_bytes: &[u8]) -> Option<usize> {
        let mut lo = 0usize;
        let mut hi = self.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.row_at(mid).as_bytes().cmp(key_bytes) {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => return Some(mid),
            }
        }
        None
    }
    /// The row range whose leading columns equal the PROJECTION of `row`
    /// through `cols` — the probe is encoded once, up front; a stored row
    /// shorter than the probe (fewer than `cols.len()` columns) precedes
    /// every extension of its own prefix, exactly as the per-column
    /// comparison this replaces defined it.
    fn prefix_bounds(&self, row: &[DataValue], cols: &[usize]) -> (usize, usize) {
        let projected: Vec<DataValue> = cols.iter().map(|&c| row[c].clone()).collect();
        let probe = encode_tuple_bare(&projected);
        let cmp = |i: usize| -> Ordering {
            let stored = self.row_at(i).as_bytes();
            match bare_prefix_len(stored, cols.len()) {
                Some(boundary) => stored[..boundary].cmp(probe.as_slice()),
                None => Ordering::Less,
            }
        };
        let mut lo = 0usize;
        let mut hi = self.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if cmp(mid) == Ordering::Less {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let start = lo;
        let mut hi = self.len();
        let mut lo = start;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if cmp(mid) != Ordering::Greater {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        (start, lo.max(start))
    }

    fn bounds(
        &self,
        lower: &LevelBoundKey,
        upper: &LevelBoundKey,
        upper_inclusive: bool,
    ) -> (usize, usize) {
        let lower = lower.as_bytes();
        let upper = upper.as_bytes();
        let mut lo = 0usize;
        let mut hi = self.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.row_at(mid).as_bytes() < lower {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let start = lo;
        let mut hi = self.len();
        let mut lo = start;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let below = if upper_inclusive {
                self.row_at(mid).as_bytes() <= upper
            } else {
                self.row_at(mid).as_bytes() < upper
            };
            if below {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        (start, lo.max(start))
    }
}

/// One sealed level of a meet rule's total: groups ascending by group
/// key, each holding its WHOLE folded accumulator. Head-tuple order for
/// interleaved layouts is derived from `groups` via
/// [`MeetLayout::interleave`] at scan time (P036 — no `by_row` twin).
#[derive(Debug)]
pub(crate) struct MeetLevel {
    /// Group key bytes (story #77, same [`encode_tuple_bare`] treatment as
    /// `NormalLevel`'s rows) → folded meet accumulators.
    pub(crate) groups: Vec<(Box<OwnBareKey>, Vec<MeetAccum>)>,
}

impl MeetLevel {
    /// Empty sealed meet level — no groups.
    pub(crate) fn empty() -> Self {
        Self { groups: Vec::new() }
    }

    fn find(&self, group_key: &[u8]) -> Option<&(Box<OwnBareKey>, Vec<MeetAccum>)> {
        match self
            .groups
            .binary_search_by(|(k, _)| k.as_bytes().cmp(group_key))
        {
            Ok(i) => Some(&self.groups[i]),
            Err(_) => None,
        }
    }
}

/// The meet spec a level stack folds with: the layout and the per-position
/// meet operations, shared by every level and every probe.
pub(crate) struct MeetSpec {
    pub(crate) layout: MeetLayout,
    meets: Vec<(Aggregation, MeetAggr)>,
}

impl Debug for MeetSpec {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeetSpec")
            .field("layout", &self.layout)
            .field(
                "meets",
                &self.meets.iter().map(|(aggr, _)| aggr).collect_vec(),
            )
            .finish()
    }
}

impl MeetSpec {
    /// Whether folding `incoming_vals` into the CURRENT total value of
    /// `group_key` (newest level owning it) would change it — the
    /// admission oracle the mid-epoch spend guard shares with the barrier.
    fn would_admit(
        &self,
        levels: &LevelStack<MeetLevel>,
        group_key: &[u8],
        incoming_vals: &[MeetAccum],
    ) -> Result<bool> {
        match levels.iter().rev().find_map(|l| l.find(group_key)) {
            None => Ok(true),
            Some((_, target)) => {
                let mut probe = target.clone();
                let mut changed = false;
                for (i, (_aggr, op)) in self.meets.iter().enumerate() {
                    changed |= op.update(&mut probe[i], &incoming_vals[i])?;
                }
                Ok(changed)
            }
        }
    }
}

/// The mid-epoch guard's view of a meet rule's running total: the spec
/// plus the sealed levels (see [`MeetAggrStore::meet_put_admission_faithful`]).
pub(crate) struct MeetTotalView<'a> {
    spec: &'a MeetSpec,
    levels: &'a LevelStack<MeetLevel>,
}

impl MeetTotalView<'_> {
    pub(crate) fn would_admit(
        &self,
        group_key: &[u8],
        incoming_vals: &[MeetAccum],
    ) -> Result<bool> {
        self.spec.would_admit(self.levels, group_key, incoming_vals)
    }
}

/// A rule's total/delta as sealed levels — the unit of semi-naive
/// evaluation.
///
/// INVARIANT (the semi-naive discipline): after every [`Self::merge_in`],
/// the levels jointly hold every tuple derived for this rule so far
/// (newest level shadowing older per key/group), and the newest level's
/// non-refresh rows are **exactly the tuples admitted this epoch** — the
/// delta. Eval joins recursive rules against deltas and stops when every
/// store's delta is empty ([`Self::has_delta`]); the contents then equal
/// the naive fixpoint.
#[derive(Debug)]
pub struct EpochStore {
    pub(crate) kind: LevelKind,
    pub(crate) arity: usize,
}

#[derive(Debug)]
pub(crate) enum LevelKind {
    Normal(LevelStack<NormalLevel>),
    Meet {
        spec: MeetSpec,
        levels: LevelStack<MeetLevel>,
    },
}

impl EpochStore {
    pub(crate) fn new_normal(arity: usize) -> Self {
        Self {
            kind: LevelKind::Normal(LevelStack::singleton(NormalLevel::empty())),
            arity,
        }
    }
    pub(crate) fn new_meet(aggrs: &[HeadAggrSlot]) -> Result<Self> {
        let probe = MeetAggrStore::new(aggrs.to_vec())?;
        Ok(Self {
            kind: LevelKind::Meet {
                spec: MeetSpec {
                    layout: probe.layout.clone(),
                    meets: probe.meets,
                },
                levels: LevelStack::singleton(MeetLevel::empty()),
            },
            arity: aggrs.len(),
        })
    }

    pub(crate) fn exists(&self, key: &[DataValue]) -> bool {
        match &self.kind {
            LevelKind::Normal(levels) => {
                let key = encode_tuple_bare(key);
                levels.iter().rev().any(|l| l.find(&key).is_some())
            }
            LevelKind::Meet { spec, levels } => {
                // Group-key membership, exactly as the map-backed store
                // defined it: project the probe onto the grouping
                // positions and test ownership across levels.
                let group = spec.layout.borrow_key(key);
                levels
                    .iter()
                    .rev()
                    .any(|l| l.find(group.as_bytes()).is_some())
            }
        }
    }

    /// The mid-epoch spend guard's total-side probe (meet rules only; a
    /// normal rule here is an engine bug, refused).
    pub(crate) fn meet_total(&self) -> Result<MeetTotalView<'_>> {
        match &self.kind {
            LevelKind::Meet { spec, levels } => Ok(MeetTotalView { spec, levels }),
            LevelKind::Normal(_) => {
                bail!("internal invariant violated: meet_total on a non-meet EpochStore")
            }
        }
    }

    /// The epoch barrier: seal the epoch's out-store as a new level —
    /// admitting what is genuinely new (per key for normal rules; per
    /// changed fold for meet rules), reporting every admission to `sink`
    /// in canonical order — then compact levels of comparable size so the
    /// stack stays logarithmic.
    pub(crate) fn merge_in<S: AdmissionSink>(
        &mut self,
        new: TempStore,
        sink: &mut S,
    ) -> Result<Admitted> {
        let admitted = match (&mut self.kind, new) {
            (LevelKind::Normal(levels), TempStore::Normal(new)) => {
                let mut level = NormalLevel::empty();
                let mut admitted = 0usize;
                for (row_bytes, skip) in new.inner {
                    let existing = levels
                        .iter()
                        .rev()
                        .find_map(|l| l.find(row_bytes.as_bytes()).map(|i| l.row_flags_at(i)));
                    match existing {
                        None => {
                            if S::RECORDING {
                                sink.admit(TupleInIter::new_bytes(row_bytes.as_bytes(), skip))?;
                            }
                            level.push(
                                row_bytes,
                                RowFlags {
                                    skip,
                                    refresh: FlagRefresh::Admitted,
                                },
                            )?;
                            admitted += 1;
                        }
                        Some(old) if old.skip != skip => {
                            // Re-derivation refreshing the limiter flag:
                            // shadows the old row, admitted nowhere.
                            level.push(
                                row_bytes,
                                RowFlags {
                                    skip,
                                    refresh: FlagRefresh::Refresh,
                                },
                            )?;
                        }
                        Some(_) => {}
                    }
                }
                // The previous delta has been consumed; an empty one
                // contributes nothing and drops (or a converging fixpoint
                // would stack one empty level per epoch). Then compact
                // the PRE-epoch stack only: the level just sealed IS the
                // delta and must survive whole until the next barrier.
                levels.drop_consumed_empty_delta(NormalLevel::is_empty);
                compact_normal(levels)?;
                levels.push(level);
                admitted
            }
            (LevelKind::Meet { spec, levels }, TempStore::MeetAggr(new)) => {
                let mut level = MeetLevel::empty();
                let mut admitted = 0usize;
                for (group, incoming) in new.by_group {
                    let folded = match levels.iter().rev().find_map(|l| l.find(group.as_bytes())) {
                        None => Some(incoming),
                        Some((_, target)) => {
                            let mut probe = target.clone();
                            let mut changed = false;
                            for (i, (_aggr, op)) in spec.meets.iter().enumerate() {
                                changed |= op.update(&mut probe[i], &incoming[i])?;
                            }
                            changed.then_some(probe)
                        }
                    };
                    if let Some(vals) = folded {
                        if S::RECORDING {
                            let row = spec.layout.interleave(&group, vals.as_slice())?;
                            sink.admit(TupleInIter::owned(row, LimiterSkip::Include))?;
                        }
                        level.groups.push((group, vals));
                        admitted += 1;
                    }
                }
                levels.drop_consumed_empty_delta(|l| l.groups.is_empty());
                compact_meet(spec, levels)?;
                levels.push(level);
                admitted
            }
            (LevelKind::Normal(_), TempStore::MeetAggr(_))
            | (LevelKind::Meet { .. }, TempStore::Normal(_)) => bail!(
                "internal invariant violated: mismatched temp-store kinds \
                 in EpochStore::merge_in"
            ),
        };
        Ok(Admitted(admitted))
    }

    /// Whether anything new was derived in the epoch just merged. All
    /// stores answering `false` is the fixpoint.
    pub(crate) fn has_delta(&self) -> bool {
        match &self.kind {
            LevelKind::Normal(levels) => {
                let l = levels.last();
                (0..l.len()).any(|i| !l.row_flags_at(i).refresh.hides_from_delta())
            }
            LevelKind::Meet { levels, .. } => !levels.last().groups.is_empty(),
        }
    }

    pub(crate) fn range_iter<'s>(
        &'s self,
        lower: &[ScanBound],
        upper: &[ScanBound],
        upper_inclusive: bool,
    ) -> Result<impl Iterator<Item = TupleInIter<'s>> + use<'s>, TempStoreCorruptRefuse> {
        self.ranged(
            LevelBoundKey::from_lower_bounds(lower),
            LevelBoundKey::from_upper_bounds(upper),
            upper_inclusive,
            false,
        )
    }
    pub(crate) fn delta_range_iter<'s>(
        &'s self,
        lower: &[ScanBound],
        upper: &[ScanBound],
        upper_inclusive: bool,
    ) -> Result<impl Iterator<Item = TupleInIter<'s>> + use<'s>, TempStoreCorruptRefuse> {
        self.ranged(
            LevelBoundKey::from_lower_bounds(lower),
            LevelBoundKey::from_upper_bounds(upper),
            upper_inclusive,
            true,
        )
    }
    pub fn prefix_iter<'s>(
        &'s self,
        prefix: &Tuple,
    ) -> Result<impl Iterator<Item = TupleInIter<'s>> + use<'s>, TempStoreCorruptRefuse> {
        // The 0xFF tail (which no canonical encoding begins) bounds every
        // extension of `prefix`, inclusively.
        let lower = LevelBoundKey::from_tuple_prefix(prefix.as_slice());
        let mut upper = lower.clone();
        upper.push_exclusive_sentinel();
        self.ranged(lower, upper, true, false)
    }
    pub(crate) fn delta_prefix_iter<'s>(
        &'s self,
        prefix: &Tuple,
    ) -> Result<impl Iterator<Item = TupleInIter<'s>> + use<'s>, TempStoreCorruptRefuse> {
        let lower = LevelBoundKey::from_tuple_prefix(prefix.as_slice());
        let mut upper = lower.clone();
        upper.push_exclusive_sentinel();
        self.ranged(lower, upper, true, true)
    }
    /// [`prefix_iter`](Self::prefix_iter)/[`delta_prefix_iter`](Self::delta_prefix_iter)
    /// reading the prefix THROUGH a projection of `row` — the zero-clone
    /// probe. Normal stores build their level cursors up front and retain
    /// nothing; meet stores build owned bounds internally (their k-way
    /// merge filters per row against the bounds, so the bounds must own —
    /// one small build per probe, confined to the aggregating fragment).
    pub(crate) fn prefix_iter_projected<'s>(
        &'s self,
        row: &[DataValue],
        cols: &[usize],
        delta_only: bool,
    ) -> Result<impl Iterator<Item = TupleInIter<'s>> + use<'s>, TempStoreCorruptRefuse> {
        match &self.kind {
            LevelKind::Normal(levels) => {
                let picked: Vec<&'s NormalLevel> = if delta_only {
                    vec![levels.last()]
                } else {
                    levels.iter().collect()
                };
                let mut cursors: Vec<(&'s NormalLevel, usize, usize)> = picked
                    .into_iter()
                    .map(|l| {
                        let (lo, hi) = l.prefix_bounds(row, cols);
                        (l, lo, hi)
                    })
                    .collect();
                Ok(Either::Left(std::iter::from_fn(move || {
                    normal_merge_next(&mut cursors, delta_only)
                })))
            }
            LevelKind::Meet { spec, levels } => {
                let prefix: Tuple = cols.iter().map(|&c| row[c].clone()).collect();
                let lower = LevelBoundKey::from_tuple_prefix(prefix.as_slice());
                let mut upper = lower.clone();
                upper.push_exclusive_sentinel();
                let all: Vec<&'s MeetLevel> = levels.iter().collect();
                let picked: Vec<&'s MeetLevel> = if delta_only {
                    vec![levels.last()]
                } else {
                    all.clone()
                };
                let newer: Vec<&'s MeetLevel> = if delta_only { vec![] } else { all };
                Ok(Either::Right(meet_ranged(
                    spec, picked, newer, lower, upper, true,
                )?))
            }
        }
    }

    pub fn all_iter(
        &self,
    ) -> Result<impl Iterator<Item = TupleInIter<'_>>, TempStoreCorruptRefuse> {
        self.prefix_iter(&Tuple::new())
    }
    pub fn delta_all_iter(
        &self,
    ) -> Result<impl Iterator<Item = TupleInIter<'_>>, TempStoreCorruptRefuse> {
        self.delta_prefix_iter(&Tuple::new())
    }
    /// The rows an early-returned (`:limit`-satisfied) entry rule actually
    /// returns: everything not flagged limiter-skipped.
    pub fn early_returned_iter(
        &self,
    ) -> Result<impl Iterator<Item = TupleInIter<'_>>, TempStoreCorruptRefuse> {
        Ok(self.all_iter()?.filter(|t| !t.should_skip()))
    }

    /// The one iterator both tiers and both scopes (total, delta) share:
    /// a k-way newest-wins merge over the level stack, restricted to a
    /// bound window. Delta scope reads the newest level alone.
    fn ranged<'s>(
        &'s self,
        lower: LevelBoundKey,
        upper: LevelBoundKey,
        upper_inclusive: bool,
        delta_only: bool,
    ) -> Result<impl Iterator<Item = TupleInIter<'s>> + use<'s>, TempStoreCorruptRefuse> {
        match &self.kind {
            LevelKind::Normal(levels) => {
                let picked: Vec<&'s NormalLevel> = if delta_only {
                    vec![levels.last()]
                } else {
                    levels.iter().collect()
                };
                // Index cursors over the flat rows: (level, next, end).
                let mut cursors: Vec<(&'s NormalLevel, usize, usize)> = picked
                    .into_iter()
                    .map(|l| {
                        let (lo, hi) = l.bounds(&lower, &upper, upper_inclusive);
                        (l, lo, hi)
                    })
                    .collect();
                Ok(Either::Left(std::iter::from_fn(move || {
                    normal_merge_next(&mut cursors, delta_only)
                })))
            }
            LevelKind::Meet { spec, levels } => {
                let all: Vec<&'s MeetLevel> = levels.iter().collect();
                let picked: Vec<&'s MeetLevel> = if delta_only {
                    vec![levels.last()]
                } else {
                    all.clone()
                };
                let newer: Vec<&'s MeetLevel> = if delta_only { vec![] } else { all };
                Ok(Either::Right(meet_ranged(
                    spec,
                    picked,
                    newer,
                    lower,
                    upper,
                    upper_inclusive,
                )?))
            }
        }
    }
}

/// One step of the k-way newest-wins merge over normal-level cursors:
/// the smallest key across cursors (among equals the LATEST cursor —
/// newest level — speaks); every cursor sharing the winning key advances;
/// refresh rows are invisible to delta scans. Shared by the ranged and
/// projected probe iterators.
fn normal_merge_next<'s>(
    cursors: &mut [(&'s NormalLevel, usize, usize)],
    delta_only: bool,
) -> Option<TupleInIter<'s>> {
    loop {
        let mut best: Option<(&'s [u8], usize)> = None;
        for (ci, (l, next, end)) in cursors.iter().enumerate() {
            if next < end {
                let t = l.row_at(*next).as_bytes();
                best = match best {
                    None => Some((t, ci)),
                    Some((bt, bci)) => {
                        if t < bt || (t == bt && ci > bci) {
                            Some((t, ci))
                        } else {
                            Some((bt, bci))
                        }
                    }
                };
            }
        }
        let (_, win_ci) = best?;
        // Advance every cursor sharing the winning key; the winner's row
        // index is remembered before advancing.
        let win_row = cursors[win_ci].1;
        let key: Vec<u8> = cursors[win_ci].0.row_at(win_row).as_bytes().to_vec();
        for (l, next, end) in cursors.iter_mut() {
            while *next < *end && l.row_at(*next).as_bytes() == key.as_slice() {
                *next += 1;
            }
        }
        let l = cursors[win_ci].0;
        let flags = l.row_flags_at(win_row);
        if delta_only && flags.refresh.hides_from_delta() {
            continue;
        }
        return Some(TupleInIter::new_bytes(
            l.row_at(win_row).as_bytes(),
            flags.skip,
        ));
    }
}

/// Compact adjacent normal levels while the newer is at least half the
/// older — the classic logarithmic-merging schedule, a pure function of
/// sizes (deterministic on every run). Newest wins per key; shadowed
/// copies drop; a surviving refresh row becomes the row (its flag is the
/// current one) and stops being refresh-marked once its shadowed victim
/// is gone.
fn compact_normal(levels: &mut LevelStack<NormalLevel>) -> Result<()> {
    levels.compact_while(
        |older, newer| newer.len() * 2 >= older.len(),
        |older, newer| {
            let mut merged = NormalLevel::empty();
            let (mut a, mut b) = (0usize, 0usize);
            while a < older.len() || b < newer.len() {
                if a >= older.len() {
                    merged.push_from(newer, b, newer.row_flags_at(b))?;
                    b += 1;
                } else if b >= newer.len() {
                    merged.push_from(older, a, older.row_flags_at(a))?;
                    a += 1;
                } else {
                    match older.row_at(a).as_bytes().cmp(newer.row_at(b).as_bytes()) {
                        Ordering::Less => {
                            merged.push_from(older, a, older.row_flags_at(a))?;
                            a += 1;
                        }
                        Ordering::Greater => {
                            merged.push_from(newer, b, newer.row_flags_at(b))?;
                            b += 1;
                        }
                        Ordering::Equal => {
                            // The shadowed copy is gone; the survivor is a
                            // plain row again whichever mark it carried.
                            let flags = newer.row_flags_at(b);
                            merged.push_from(
                                newer,
                                b,
                                RowFlags {
                                    skip: flags.skip,
                                    refresh: FlagRefresh::Admitted,
                                },
                            )?;
                            a += 1;
                            b += 1;
                        }
                    }
                }
            }
            Ok(merged)
        },
    );
    Ok(())
}

/// As [`compact_normal`], for meet levels: newest group value wins whole.
fn compact_meet(_spec: &MeetSpec, levels: &mut LevelStack<MeetLevel>) -> Result<()> {
    levels.compact_while(
        |older, newer| newer.groups.len() * 2 >= older.groups.len(),
        |older, newer| {
            let mut merged = MeetLevel {
                groups: Vec::with_capacity(older.groups.len() + newer.groups.len()),
            };
            let (mut a, mut b) = (
                older.groups.iter().peekable(),
                newer.groups.iter().peekable(),
            );
            let peeked = || -> miette::Report { LevelInvariantError("peeked").into() };
            loop {
                match (a.peek(), b.peek()) {
                    (Some((ka, _)), Some((kb, _))) => match ka.cmp(kb) {
                        Ordering::Less => merged.groups.push(a.next().ok_or_else(peeked)?.clone()),
                        Ordering::Greater => {
                            merged.groups.push(b.next().ok_or_else(peeked)?.clone())
                        }
                        Ordering::Equal => {
                            let g = b.next().ok_or_else(peeked)?.clone();
                            a.next();
                            merged.groups.push(g);
                        }
                    },
                    (Some(_), None) => merged.groups.push(a.next().ok_or_else(peeked)?.clone()),
                    (None, Some(_)) => merged.groups.push(b.next().ok_or_else(peeked)?.clone()),
                    (None, None) => break,
                }
            }
            Ok(merged)
        },
    );
    Ok(())
}

/// Row-ordered iteration over meet levels within a bound window, newest
/// group owner winning. Suffix layouts walk `groups` directly (group
/// order IS row order there); interleaved layouts derive head-tuple order
/// from `groups` via interleave (P036 — no stored twin).
fn meet_ranged<'s>(
    spec: &'s MeetSpec,
    picked: Vec<&'s MeetLevel>,
    all: Vec<&'s MeetLevel>,
    lower: LevelBoundKey,
    upper: LevelBoundKey,
    upper_inclusive: bool,
) -> Result<Box<dyn Iterator<Item = TupleInIter<'s>> + 's>, TempStoreCorruptRefuse> {
    let within = move |row: &TupleInIter<'_>| -> bool {
        row.cmp_bare(lower.as_bytes()) != Ordering::Less
            && match row.cmp_bare(upper.as_bytes()) {
                Ordering::Less => true,
                Ordering::Equal => upper_inclusive,
                Ordering::Greater => false,
            }
    };
    if spec.layout.is_suffix() {
        // Group order is row order: k-way over `groups`, newest wins.
        let mut cursors: Vec<(
            std::iter::Peekable<std::slice::Iter<'s, (Box<OwnBareKey>, Vec<MeetAccum>)>>,
            usize,
        )> = picked
            .iter()
            .enumerate()
            .map(|(idx, l)| (l.groups.iter().peekable(), idx))
            .collect();
        Ok(Box::new(
            std::iter::from_fn(move || {
                let mut best: Option<(&'s OwnBareKey, usize)> = None;
                for (cur, idx) in cursors.iter_mut() {
                    if let Some((k, _)) = cur.peek() {
                        let k: &'s OwnBareKey = k.as_ref();
                        best = match best {
                            None => Some((k, *idx)),
                            Some((bk, bidx)) => {
                                if k < bk || (k == bk && *idx > bidx) {
                                    Some((k, *idx))
                                } else {
                                    Some((bk, bidx))
                                }
                            }
                        };
                    }
                }
                let (key, win_idx) = best?;
                let mut winner: Option<&'s (Box<OwnBareKey>, Vec<MeetAccum>)> = None;
                for (cur, idx) in cursors.iter_mut() {
                    while cur.peek().is_some_and(|(k, _)| k.as_ref() == key) {
                        // peek proved Some; Peekable/slice next is the same element.
                        let Some(g) = cur.next() else { break };
                        if *idx == win_idx {
                            winner = Some(g);
                        }
                    }
                }
                // `best` was chosen from win_idx's peek of `key`, so a missing
                // winner is unrepresentable under Peekable's contract — end the
                // stream rather than panic.
                let (k, v) = winner?;
                Some(TupleInIter::new_meet_suffix(
                    k.as_bytes(),
                    v.as_slice(),
                    LimiterSkip::Include,
                ))
            })
            .filter(move |row| within(row)),
        ))
    } else {
        // Interleaved: derive head-tuple rows from each picked level's
        // groups; a row speaks only if no newer level owns its group.
        let derived: Vec<Vec<Tuple>> = picked
            .iter()
            .map(|l| {
                let mut rows: Vec<Tuple> = l
                    .groups
                    .iter()
                    .map(|(k, v)| spec.layout.interleave(k.as_ref(), v.as_slice()))
                    .collect::<Result<Vec<_>, _>>()?;
                rows.sort();
                Ok(rows)
            })
            .collect::<Result<Vec<_>, _>>()?;
        // Per-level slice of newer refs so the inner filter_map owns its
        // capture — nested `move` cannot both take `all` (E0507).
        let iters = derived.into_iter().enumerate().map(move |(idx, rows)| {
            let newer: Vec<&'s MeetLevel> = all.iter().skip(idx + 1).copied().collect();
            rows.into_iter().filter_map(move |row| {
                let group = spec.layout.borrow_key(row.as_slice());
                let owned_by_newer = newer.iter().any(|nl| nl.find(group.as_bytes()).is_some());
                if owned_by_newer {
                    None
                } else {
                    Some(TupleInIter::owned(row, LimiterSkip::Include))
                }
            })
        });
        Ok(Box::new(
            iters
                .kmerge_by(|a, b| a.cmp(b) == Ordering::Less)
                .filter(move |row| within(row)),
        ))
    }
}

#[cfg(test)]
mod level_stack_tests {
    use super::*;
    use miette::{Result, miette};

    /// A converging fixpoint's level stack stays bounded: epochs that
    /// admit nothing must not accumulate levels (the consumed empty delta
    /// drops when the next seals), and admit-heavy runs stay logarithmic
    /// through compaction.
    #[test]
    fn level_stack_stays_bounded() -> Result<()>  {
        let mut store = EpochStore::new_normal(1);
        // Ten productive epochs...
        for i in 0..10i64 {
            let mut out = RegularTempStore::new();
            out.put(Tuple::from_vec(vec![DataValue::from(i)]));
            store.merge_in(out.wrap(), &mut ())?;
        }
        // ...then fifty empty (converged) epochs.
        for _ in 0..50 {
            let out = RegularTempStore::new();
            store.merge_in(out.wrap(), &mut ())?;
            assert!(!store.has_delta());
        }
        let LevelKind::Normal(levels) = &store.kind else {
            unreachable!()
        };
        assert!(
            levels.len() <= 6,
            "level stack must stay logarithmic, got {}",
            levels.len()
        );
        // Totals intact through it all.
        assert_eq!(store.all_iter()?.count(), 10);
        Ok(())
    }
}
// ─────────────────────────────────────────────────────────────────────────
// Own-bytes row keys (P094)
// ─────────────────────────────────────────────────────────────────────────

/// Bare-encoded row bytes minted only via [`Self::encode`]. Foreign slices
/// are unrepresentable — [`TempStoreCorruptRefuse`] is the decode refusal.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct OwnBareKey(Box<[u8]>);

#[derive(Debug, Error, Diagnostic)]
#[error("temp store row bytes failed bare decode")]
#[diagnostic(code(query::temp_store_corrupt))]
pub struct TempStoreCorruptRefuse(#[source] DecodeError);

/// `meet_put` must leave the folded group resident — typed refuse if not.
#[derive(Debug, Error, Diagnostic)]
#[error("meet_put did not leave the group resident")]
#[diagnostic(
    code(query::meet_put_resident),
    help("This is a bug. Please report it.")
)]
struct MeetPutResidentInvariant;

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum TempStoreAccessRefuse {
    #[error(transparent)]
    Corrupt(#[from] TempStoreCorruptRefuse),
    #[error("head position {idx} out of range")]
    Position { idx: usize },
}

impl Debug for OwnBareKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("OwnBareKey").field(&self.0).finish()
    }
}

impl OwnBareKey {
    /// Mint from a value slice through the bare encode door.
    pub(crate) fn encode(values: &[DataValue]) -> Box<Self> {
        Box::new(Self(encode_tuple_bare(values).into_boxed_slice()))
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub(crate) fn decode_tuple(&self) -> Result<Tuple, TempStoreCorruptRefuse> {
        decode_tuple_bare(self.as_bytes()).map_err(TempStoreCorruptRefuse)
    }
}

// ─────────────────────────────────────────────────────────────────────────
// The admission seam
// ─────────────────────────────────────────────────────────────────────────

/// The number of derivations admitted to a store's `total` by one
/// `merge_in` — the budget design's unit of account for the
/// `derived_tuple_ceiling`. Deterministic: it is a function of the sets
/// being merged, not of any schedule, so summing it per epoch and checking
/// at the epoch barrier refuses identically on every run.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct Admitted(pub(crate) usize);

impl Admitted {
    /// Zero admissions — the empty merge account.
    pub(crate) const ZERO: Self = Self(0);
}

/// Observer of the admission seam: called once per tuple admitted to a
/// store's `total`, in canonical key order (see the module doc). This is
/// where the provenance design's first-witness recording attaches — the
/// sink implementation eval passes when provenance is on will bind the
/// pending witness for each admitted tuple.
///
/// The off-state is `()`: `RECORDING = false` lets the stores skip
/// admission enumeration entirely (the fast-path swap admits a whole store
/// without walking it), so provenance-off is zero-cost by
/// monomorphization, not by a runtime branch.
pub(crate) trait AdmissionSink {
    /// Compile-time switch: when `false`, `admit` is never called and the
    /// stores skip the enumeration that would feed it. [`Admitted`] counts
    /// are unaffected — accounting is always on.
    const RECORDING: bool;
    /// One admitted tuple. For a meet store this is the group key together
    /// with its *current* (post-update) aggregate value — matching the
    /// provenance boundary that a meet aggregation's witness is per group,
    /// not per contributing row.
    fn admit(&mut self, tuple: TupleInIter<'_>) -> Result<(), TempStoreCorruptRefuse>;
}

/// Recording off: the default state, compiled away.
impl AdmissionSink for () {
    const RECORDING: bool = false;
    #[inline(always)]
    fn admit(&mut self, _tuple: TupleInIter<'_>) -> Result<(), TempStoreCorruptRefuse> {
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// RegularTempStore
// ─────────────────────────────────────────────────────────────────────────

/// Per-tuple limiter disposition in a [`RegularTempStore`].
///
/// Named token (not a bare `bool`): a row either participates in the entry
/// rule's returned set, or was derived past `:limit` and joins only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum LimiterSkip {
    /// Within `:limit` — joins and early-returned rows.
    Include,
    /// Past `:limit` — joins only; filtered from [`EpochStore::early_returned_iter`].
    PastLimit,
}

impl LimiterSkip {
    #[inline]
    pub(crate) fn excludes_from_return(self) -> bool {
        matches!(self, Self::PastLimit)
    }
}

/// A store holding temp data during evaluation of queries: a set of tuples,
/// each with a [`LimiterSkip`] disposition (past `:limit` rows join but are
/// not returned; see [`EpochStore::early_returned_iter`]).
/// The public interface is used in custom implementations of
/// fixed rules (algorithms/utilities).
///
/// Story #77 chunk 2: keyed by [`encode_tuple_bare`]'s memcmp bytes, not
/// `Tuple` — one `Box<[u8]>` allocation per distinct derived row instead of
/// a `Vec<DataValue>` (whose own heap buffer plus any nested `Str`/`List`/
/// `Set`/`Json` sub-allocations is what the story's measured "~415 B/row"
/// tax is made of). `BTreeMap` ordering is unaffected: `encode_tuple_bare`'s
/// order-embedding law (`data/tuple.rs`'s generative proof) means byte order
/// here is EXACTLY the `Vec<DataValue>` order this replaces — every
/// admission-order/determinism test below keeps its own assertions
/// unchanged, which is the adversarial check that the swap is
/// representation-only, not a semantic change.
#[derive(Debug)]
pub struct RegularTempStore {
    pub(crate) inner: BTreeMap<Box<OwnBareKey>, LimiterSkip>,
}

impl RegularTempStore {
    /// Empty store — no derived rows.
    pub fn new() -> Self {
        Self {
            inner: BTreeMap::new(),
        }
    }
}

/// The value-part placeholder for a [`TupleInIter::Values`] whose whole
/// row is its key (a regular store's admitted tuple, and a suffix-layout
/// meet store's group key): still `DataValue`-shaped, since the meet path
/// is unconverted this chunk (`MeetAggrStore`/`MeetLevel` stay as-is; see
/// this module's doc).
pub(crate) fn empty_tuple_ref() -> &'static Tuple {
    static EMPTY: std::sync::OnceLock<Tuple> = std::sync::OnceLock::new();
    EMPTY.get_or_init(Tuple::new)
}

impl RegularTempStore {
    pub(crate) fn wrap(self) -> TempStore {
        TempStore::Normal(self)
    }
    /// Tests if a key already exists in the store. A slice probe: the
    /// eval seam dedups batch-resident rows without minting them.
    pub fn exists(&self, key: &[DataValue]) -> bool {
        let probe = OwnBareKey::encode(key);
        self.inner.contains_key(probe.as_ref())
    }
    /// The number of distinct tuples materialized so far. On the plain and
    /// normal-aggregation paths the out-store holds only genuinely-new,
    /// deduped tuples (the caller filters each derivation against the running
    /// total before putting), so this is *both* the resident memory and the
    /// count the barrier will admit — the budget's mid-epoch spend guard
    /// reads it directly as the rule's in-flight admission count (see
    /// `exec::fixpoint::eval::InterruptTicker`). Contrast
    /// [`MeetAggrStore::len`](MeetAggrStore::len), whose fresh out-store also
    /// holds re-derived unchanged groups, so it is resident memory but NOT an
    /// admission count.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// True when the store holds no tuples.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    // Edition-2024 note: the `use<'s>` capture lists on the iterator
    // returns are load-bearing — the bounds are copied into owned values,
    // so the returned iterator borrows the store only, not the bound
    // arguments (the original relied on edition-2021's default).

    /// Add a tuple to the store.
    pub fn put(&mut self, tuple: Tuple) {
        self.inner
            .insert(OwnBareKey::encode(tuple.as_slice()), LimiterSkip::Include);
    }
    pub(crate) fn put_with_skip(&mut self, tuple: Tuple) {
        self.inner
            .insert(OwnBareKey::encode(tuple.as_slice()), LimiterSkip::PastLimit);
    }
}

// ─────────────────────────────────────────────────────────────────────────
// MeetAggrStore
// ─────────────────────────────────────────────────────────────────────────

/// A proven index into a rule head — minted only from
/// `aggr.iter().enumerate()` at layout / eval rule-set construction.
/// Bare `usize` head positions are not admitted on the Meet path (P101).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HeadPos(usize);

impl HeadPos {
    /// Door used when the enumerate index is already a head position.
    pub fn from_index(i: usize) -> Self {
        Self(i)
    }

    pub(crate) fn get(self) -> usize {
        self.0
    }
}

/// The positional layout of a meet-aggregation head, resolved once at
/// construction from the head's per-position aggregation signature. It is
/// the constructed proof that carries *where* the grouping keys and the
/// meet values sit, so the projection/interleave arithmetic lives in
/// exactly one place instead of scattered `split_at`/`grouping_len` call
/// sites.
///
/// `key_positions` are the head positions with no aggregation (the grouping
/// key), `val_positions` the meet-aggregated positions — both [`HeadPos`]
/// in ascending head order, and together a partition of `0..arity`.
/// Upstream cozo (and the store this replaces) required the aggregated
/// positions to form a *suffix* so the group key was a byte prefix of the
/// encoded tuple; this layout groups by the projection onto
/// `key_positions` **wherever they sit** — position 0, interleaved, or
/// split across the head — matching the oracle's full positional meet
/// semantics (see the divergence note in `query/laws.rs`).
#[derive(Debug, Clone)]
pub(crate) struct MeetLayout {
    key_positions: Vec<HeadPos>,
    val_positions: Vec<HeadPos>,
    arity: usize,
}

impl MeetLayout {
    /// Build from the head signature: [`HeadAggrSlot::Plain`] positions
    /// group, [`HeadAggrSlot::Aggregated`] positions aggregate. Total over
    /// any signature — the partition of `0..arity` is exhaustive, so
    /// `interleave` never leaves a `Null`.
    fn from_signature(aggrs: &[HeadAggrSlot]) -> Self {
        let arity = aggrs.len();
        let mut key_positions = Vec::new();
        let mut val_positions = Vec::new();
        for (i, a) in aggrs.iter().enumerate() {
            if a.is_aggregated() {
                val_positions.push(HeadPos::from_index(i));
            } else {
                key_positions.push(HeadPos::from_index(i));
            }
        }
        Self {
            key_positions,
            val_positions,
            arity,
        }
    }

    /// The grouping key of a head tuple: its projection onto the
    /// non-aggregated positions, in head order. `row` must cover every
    /// grouping position (a full head tuple does).
    fn project_key(&self, row: &[DataValue]) -> Tuple {
        self.key_positions
            .iter()
            .map(|p| row[p.get()].clone())
            .collect()
    }

    /// The meet values of a head tuple: its projection onto the aggregated
    /// positions, in head order (aligned one-to-one with `meets`), each
    /// wrapped as [`MeetAccum::Value`]. Production admit folds through
    /// `init_val` instead; this stays for layout round-trip tests only.
    #[cfg(test)]
    fn project_vals(&self, row: &[DataValue]) -> Vec<MeetAccum> {
        self.val_positions
            .iter()
            .map(|p| MeetAccum::from_derived(row[p.get()].clone()))
            .collect()
    }

    /// Whether the aggregated positions form a *suffix* — equivalently, the
    /// grouping positions are exactly the prefix `0..key_positions.len()`.
    /// When true, a head tuple is byte-for-byte `group_key ++ folded_vals`,
    /// so the group map alone reconstructs every row in head-tuple order (a
    /// distinct group has a distinct key, and the key is the head prefix).
    /// Non-suffix layouts derive head-tuple order from `by_group` via
    /// [`Self::interleave`] at scan time — never a stored twin (P036).
    pub(crate) fn is_suffix(&self) -> bool {
        self.key_positions
            .iter()
            .map(|p| p.get())
            .eq(0..self.key_positions.len())
    }

    /// The grouping key of a head tuple, as [`encode_tuple_bare`]'s memcmp
    /// bytes (story #77: the group map keys on bytes, not `Tuple` — same
    /// footprint argument as [`RegularTempStore`], and group-key ORDER is
    /// unaffected by the same order-embedding law). Always an encode, even
    /// on a suffix layout: a byte key has nothing in `row` to borrow from
    /// (row is `DataValue`s, not bytes) — the zero-alloc suffix borrow the
    /// previous `Cow`-returning form had is traded for a smaller resident
    /// key, one encode per fold instead of zero.
    pub(crate) fn borrow_key(&self, row: &[DataValue]) -> Box<OwnBareKey> {
        if self.is_suffix() {
            OwnBareKey::encode(&row[..self.key_positions.len()])
        } else {
            OwnBareKey::encode(self.project_key(row).as_slice())
        }
    }

    /// Rebuild the logical head tuple from a group key and its folded meet
    /// values — the inverse of the two projections. Every position is
    /// either a key or a value position (they partition `0..arity`), so no
    /// `Null` placeholder survives. [`MeetAccum::Empty`] materializes as
    /// [`DataValue::Null`] (result meaning "no input"), distinct from the
    /// in-fold Empty state.
    pub(crate) fn interleave(
        &self,
        key: &OwnBareKey,
        vals: &[MeetAccum],
    ) -> Result<Tuple, TempStoreCorruptRefuse> {
        let key = key.decode_tuple()?;
        let mut row = Tuple::from_vec(vec![DataValue::Null; self.arity]);
        for (slot, p) in self.key_positions.iter().enumerate() {
            row[p.get()] = key[slot].clone();
        }
        for (slot, p) in self.val_positions.iter().enumerate() {
            row[p.get()] = vals[slot].to_value();
        }
        Ok(row)
    }
}

/// A store for rules whose heads carry only meet (semilattice)
/// aggregations: tuples are grouped by their non-aggregated positions
/// (wherever they sit — see [`MeetLayout`]) and the aggregated positions
/// are folded through the meet operations as rows arrive.
///
/// Delta discipline: a group is in the delta when it is new, or when a
/// merge *changed* its folded value — as reported by
/// [`MeetAggrObj::update`]'s changed flag. That flag is therefore
/// load-bearing for termination and completeness both ways: a false
/// "unchanged" reaches a premature fixpoint (the changed value never
/// propagates through the recursion — the original's inverted `and`/`or`
/// flags had exactly this bug, fixed in `data/aggr.rs` and pinned by
/// `meet_delta_regression_*` below); a false "changed" merely costs
/// re-propagation. That authority lives entirely in `by_group`, so the
/// changed-flag semantics are byte-identical to the suffix-only store this
/// replaces.
pub(crate) struct MeetAggrStore {
    /// Group key bytes ([`encode_tuple_bare`], story #77 — same footprint
    /// argument as [`RegularTempStore`]) → folded meet accumulators
    /// (projection onto the aggregated positions, in head order). The
    /// fold/delta authority, iterated in canonical group-key order at the
    /// merge barrier so admissions stay schedule-independent. Accumulators
    /// are [`MeetAccum`] — `Empty | Value(v)` — so a domain `Null` is never
    /// the empty sentinel.
    pub(crate) by_group: BTreeMap<Box<OwnBareKey>, Vec<MeetAccum>>,
    /// The meet operations, one per aggregated head position, in head
    /// order, resolved at construction. (The original stored `Option`s and
    /// unwrapped per row.)
    pub(crate) meets: Vec<(Aggregation, MeetAggr)>,
    pub(crate) layout: MeetLayout,
}

impl Debug for MeetAggrStore {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeetAggrStore")
            .field("by_group", &self.by_group)
            .field(
                "meets",
                &self.meets.iter().map(|(aggr, _)| aggr).collect_vec(),
            )
            .field("layout", &self.layout)
            .finish()
    }
}

impl MeetAggrStore {
    pub(crate) fn wrap(self) -> TempStore {
        TempStore::MeetAggr(self)
    }

    /// The number of distinct groups materialized so far. This is the meet
    /// store's *resident* size — **not** its admission count. A fresh epoch
    /// out-store folds every re-derived group, including ones whose value
    /// equals the running total's, so `len` can exceed what the barrier will
    /// admit (an epoch re-deriving N unchanged groups has `len == N`,
    /// admitted `== 0`). The budget's mid-epoch spend guard must therefore
    /// count with [`Self::meet_put_admission_faithful`], not `len`; `len`
    /// remains the honest memory-resident measure for the boundedness law.
    pub fn len(&self) -> usize {
        self.by_group.len()
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.by_group.is_empty()
    }
    /// Group-key membership — the law tests' observer (the engine probes
    /// totals through [`EpochStore::exists`]).
    #[cfg(test)]
    pub(crate) fn exists(&self, key: &[DataValue]) -> bool {
        let group_key = self.layout.borrow_key(key);
        self.by_group.contains_key(group_key.as_ref())
    }
    /// Fold `tuple` into `self` (the epoch's fresh out-store) exactly as
    /// [`Self::meet_put`], and report whether *this fold made the group newly
    /// admissible against `total`* — i.e. whether the group crossed from "the
    /// barrier would not admit it" to "the barrier would admit it".
    ///
    /// This is the admission-faithful in-flight count for the mid-epoch spend
    /// guard: the meet twin of the plain path's `!prev_store.exists(item)`
    /// filter. Summing the `true`s over an epoch's derivations yields exactly
    /// the count [`Self::merge_in`] will admit against the same `total` —
    /// never more — so `in_flight ≤ admitted_r` holds BY CONSTRUCTION on the
    /// meet path.
    ///
    /// Why the sum is exact, not merely an upper bound: a group is admissible
    /// iff folding the out-store's value into `total`'s value moves it, and
    /// the meet is **monotone** — as this epoch folds more derivations into a
    /// group, the out-store value only descends, so `meet(total, out[g])`
    /// only descends, so admissibility flips `false → true` at most once and
    /// never back. Each admitted group therefore contributes exactly one
    /// `true` here, matching `merge_in`'s one `admitted += 1` for it. (The
    /// plain path gets this free because its out-store only ever holds
    /// genuinely-new tuples; the meet out-store holds re-derived unchanged
    /// groups too, which is precisely why counting `len` overcounts and
    /// refused completing programs — the refuted theorem.)
    pub(crate) fn meet_put_admission_faithful(
        &mut self,
        tuple: &[DataValue],
        total: &MeetTotalView<'_>,
    ) -> Result<bool> {
        // The group key is encoded once and reused for both probes.
        let group_key = self.layout.borrow_key(tuple);
        // Admissibility BEFORE this fold: a group absent from the out-store
        // contributes nothing to the barrier yet, so it is not admissible.
        let was_admissible = match self.by_group.get(group_key.as_ref()) {
            Some(vals) => total.would_admit(group_key.as_bytes(), vals.as_slice())?,
            None => false,
        };
        self.meet_put(tuple)?;
        // After folding, the group is certainly resident in the out-store.
        let now_vals = self
            .by_group
            .get(group_key.as_ref())
            .ok_or(MeetPutResidentInvariant)?;
        let now_admissible = total.would_admit(group_key.as_bytes(), now_vals.as_slice())?;
        Ok(now_admissible && !was_admissible)
    }
    /// Build a meet store from a rule head's aggregation spec: one entry
    /// per head position, [`HeadAggrSlot::Plain`] for grouping positions,
    /// [`HeadAggrSlot::Aggregated`] for aggregated ones. Compilation only
    /// routes rules whose aggregations are all meets here
    /// (`Aggregation::is_meet`); a normal-only aggregation in the spec is
    /// an engine bug and is refused with an error rather than unwrapped.
    /// The argument lists are part of eval's aggregation-spec shape but
    /// meet forms take no arguments (see `data/aggr.rs`), so they are
    /// ignored.
    pub(crate) fn new(aggrs: Vec<HeadAggrSlot>) -> Result<Self> {
        let layout = MeetLayout::from_signature(&aggrs);
        let mut meets = Vec::new();
        for a in aggrs {
            if let HeadAggrSlot::Aggregated { aggr, args: _ } = a {
                let op = crate::exec::fold::aggr::meet_op(&aggr).ok_or_else(|| {
                    miette!(
                        "internal invariant violated: normal-only aggregation '{}' \
                         routed to a meet store",
                        aggr.name
                    )
                })?;
                meets.push((aggr, op));
            }
        }
        Ok(Self {
            by_group: BTreeMap::new(),
            meets,
            layout,
        })
    }
    /// Fold one derived tuple into the store. Returns whether the store
    /// changed: a new group, or an existing group whose folded value moved
    /// in its lattice. Idempotent by the meet laws: folding the same tuple
    /// again returns `false` and changes nothing.
    ///
    /// Structural guarantee: `tuple` has the arity of the rule head this
    /// store was built from (`key_positions + val_positions` partition it),
    /// because eval only puts a rule's own head tuples here — the projection
    /// below cannot go out of bounds on user data.
    ///
    /// A slice consumer: the store only ever reads projections of the row
    /// (group key, meet values), so it never demands ownership — a
    /// batch-resident row folds in without being minted, and only a NEW
    /// group allocates (its key and values).
    pub(crate) fn meet_put(&mut self, tuple: &[DataValue]) -> Result<bool> {
        // The grouping projection, encoded once (story #77: `borrow_key` is
        // always an encode now, never a zero-alloc slice borrow — see its
        // doc), and fold incoming values straight from `tuple` by position
        // so no `incoming` tuple is built either way.
        let key = self.layout.borrow_key(tuple);
        match self.by_group.get_mut(key.as_ref()) {
            Some(vals) => {
                let mut changed = false;
                for (i, (_aggr, op)) in self.meets.iter().enumerate() {
                    let incoming =
                        MeetAccum::from_derived(tuple[self.layout.val_positions[i].get()].clone());
                    changed |= op.update(&mut vals[i], &incoming)?;
                }
                Ok(changed)
            }
            None => {
                // Fold the first row through each meet's identity — never
                // install `Value(v)` raw. A Null first input must run the
                // same skip/absorb path as every later row (Min/Max skip
                // Null and stay Empty; Intersection takes Null as data).
                let mut vals: Vec<MeetAccum> =
                    self.meets.iter().map(|(_, op)| op.init_val()).collect();
                for (i, (_aggr, op)) in self.meets.iter().enumerate() {
                    let incoming =
                        MeetAccum::from_derived(tuple[self.layout.val_positions[i].get()].clone());
                    op.update(&mut vals[i], &incoming)?;
                }
                self.by_group.insert(key, vals);
                Ok(true)
            }
        }
    }

    /// Seed the all-aggregated empty-head identity row: each meet's
    /// [`MeetAggrObj::init_val`] stored as a typed accumulator (so
    /// [`MeetAccum::Empty`] stays Empty, never `Value(Null)`).
    pub(crate) fn seed_identity(&mut self) -> Result<bool> {
        ensure!(
            self.is_empty(),
            "seed_identity requires an empty meet store"
        );
        let vals: Vec<MeetAccum> = self.meets.iter().map(|(_, op)| op.init_val()).collect();
        let key = OwnBareKey::encode(&[]);
        self.by_group.insert(key, vals);
        Ok(true)
    }
}

// ─────────────────────────────────────────────────────────────────────────
// TempStore and EpochStore
// ─────────────────────────────────────────────────────────────────────────

/// One epoch's worth of derived tuples for one rule: regular or
/// meet-aggregated, matching how the rule's head aggregates.
#[derive(Debug)]
pub(crate) enum TempStore {
    Normal(RegularTempStore),
    MeetAggr(MeetAggrStore),
}

impl TempStore {}

// ─────────────────────────────────────────────────────────────────────────
// TupleInIter
// ─────────────────────────────────────────────────────────────────────────

/// A view of one stored tuple, in whichever representation its store
/// currently uses: the key part and (for meet stores) the value part,
/// exposed as one logical tuple without concatenating.
///
/// Story #77: both stores' KEYS are memcmp bytes now
/// ([`RegularTempStore`]/`NormalLevel`'s whole row, `MeetAggrStore`/
/// `MeetLevel`'s group-key projection, `query/levels.rs`) — pure
/// comparison data, never computed on, so the footprint cut applies to
/// both. Meet's FOLDED VALUES are [`MeetAccum`] (`Empty | Value`) so a
/// domain `Null` is never the empty sentinel; iteration materializes
/// via [`MeetAccum::to_value`]. One enum keeps every consumer
/// (`AdmissionSink`, the RA join
/// probes, the provenance/trials iteration surfaces) working over ONE
/// type regardless of which
/// store produced the row and which layout (suffix or interleaved) it
/// used. The consequence: `try_get`/`try_into_tuple`/iteration return OWNED
/// `DataValue`s, never `&'a DataValue` — a byte-backed key has nothing to
/// reference; decoding produces a value, not a borrow. Byte-backed rows
/// were sealed at an encode door before reaching this view.
#[allow(private_interfaces)] // LimiterSkip/MeetAccum stay crate-private; TupleInIter is the public view
#[derive(Clone, Debug)]
pub enum TupleInIter<'a> {
    /// A regular store's row: memcmp bytes (chunk 1's bare codec), whole
    /// row in `key` — a regular row has no separate value region.
    Bytes { key: &'a [u8], skip: LimiterSkip },
    /// A meet store's SUFFIX-layout row: group-key bytes (the row's own
    /// head prefix — no interleave needed) + folded meet accumulators.
    MeetSuffix {
        key: &'a [u8],
        val: &'a [MeetAccum],
        skip: LimiterSkip,
    },
    /// A meet store's INTERLEAVED-layout row: [`MeetLayout::interleave`]
    /// has already rebuilt the full logical row as `DataValue`s (the key
    /// projection may sit anywhere in head order, so there is no prefix
    /// shortcut), matching the pre-byte-conversion shape exactly.
    Values {
        key: &'a [DataValue],
        val: &'a [DataValue],
        skip: LimiterSkip,
    },
    /// Owned head tuple — interleaved meet scans derive this from `groups`
    /// / `by_group` so no stored `by_row` twin is required (P036).
    Owned { row: Tuple, skip: LimiterSkip },
}

impl<'a> TupleInIter<'a> {
    /// Construct a view over an already-interleaved (key-part, value-part,
    /// skip) triple — the non-suffix meet path, where `key`/`val` are
    /// `DataValue` projections of a fully rebuilt logical row.
    pub(crate) fn new(key: &'a [DataValue], val: &'a [DataValue], skip: LimiterSkip) -> Self {
        TupleInIter::Values { key, val, skip }
    }
    /// Owned head-tuple view (interleaved meet derive-at-scan).
    pub(crate) fn owned(row: Tuple, skip: LimiterSkip) -> Self {
        TupleInIter::Owned { row, skip }
    }
    /// Construct a view over one regular store's row bytes.
    pub(crate) fn new_bytes(key: &'a [u8], skip: LimiterSkip) -> Self {
        TupleInIter::Bytes { key, skip }
    }
    /// Construct a view over a suffix-layout meet store's group-key bytes
    /// plus its typed folded accumulators.
    pub(crate) fn new_meet_suffix(key: &'a [u8], val: &'a [MeetAccum], skip: LimiterSkip) -> Self {
        TupleInIter::MeetSuffix { key, val, skip }
    }
}

impl TupleInIter<'_> {
    pub(crate) fn try_get(&self, idx: usize) -> Result<DataValue, TempStoreAccessRefuse> {
        match self {
            TupleInIter::Bytes { key, .. } => {
                bare_nth(key, idx)?.ok_or(TempStoreAccessRefuse::Position { idx })
            }
            TupleInIter::MeetSuffix { key, val, .. } => {
                let key = decode_row_bare(key)?;
                key.get(idx)
                    .cloned()
                    .or_else(|| val.get(idx - key.len()).map(|a| a.to_value()))
                    .ok_or(TempStoreAccessRefuse::Position { idx })
            }
            TupleInIter::Values { key, val, .. } => key
                .get(idx)
                .or_else(|| val.get(idx - key.len()))
                .cloned()
                .ok_or(TempStoreAccessRefuse::Position { idx }),
            TupleInIter::Owned { row, .. } => row
                .get(idx)
                .cloned()
                .ok_or(TempStoreAccessRefuse::Position { idx }),
        }
    }
    pub(crate) fn should_skip(&self) -> bool {
        match self {
            TupleInIter::Bytes { skip, .. }
            | TupleInIter::MeetSuffix { skip, .. }
            | TupleInIter::Values { skip, .. }
            | TupleInIter::Owned { skip, .. } => skip.excludes_from_return(),
        }
    }
    pub fn try_into_tuple(self) -> Result<Tuple, TempStoreCorruptRefuse> {
        match self {
            TupleInIter::Owned { row, .. } => Ok(row),
            other @ TupleInIter::Bytes { .. }
            | other @ TupleInIter::MeetSuffix { .. }
            | other @ TupleInIter::Values { .. } => other.try_materialize(),
        }
    }

    fn try_materialize(&self) -> Result<Tuple, TempStoreCorruptRefuse> {
        match self {
            TupleInIter::Bytes { key, .. } => decode_row_bare(key),
            TupleInIter::MeetSuffix { key, val, .. } => {
                let mut key = decode_row_bare(key)?;
                key.extend(val.iter().map(|a| a.to_value()));
                Ok(key)
            }
            TupleInIter::Values { key, val, .. } => {
                Ok(key.iter().chain(val.iter()).cloned().collect())
            }
            TupleInIter::Owned { row, .. } => Ok(row.clone()),
        }
    }
    /// Total comparison against a plain slice, through `DataValue`'s total
    /// order — for a byte-backed key, through the memcmp bytes of an
    /// encoded PROBE instead: `encode_tuple_bare`'s order-embedding law
    /// makes that byte comparison exactly `DataValue`'s slice order,
    /// without decoding the stored side at all where the whole row is
    /// bytes; a mixed key still decodes its own (small) key half since the
    /// value half is not comparably encoded.
    /// Compare this row's BARE byte form to a bare bound (byte order is
    /// value order by the mirror law; 0xFF-closed bounds sort past every
    /// row extension).
    pub(crate) fn cmp_bare(&self, probe: &[u8]) -> Ordering {
        match self {
            TupleInIter::Bytes { key, .. } => (*key).cmp(probe),
            TupleInIter::MeetSuffix { key, val, .. } => {
                let mut full = key.to_vec();
                for a in val.iter() {
                    kyzo_model::value::append_canonical(&mut full, &a.to_value());
                }
                full.as_slice().cmp(probe)
            }
            TupleInIter::Values { key, val, .. } => {
                let mut full = Vec::new();
                for v in key.iter().chain(val.iter()) {
                    kyzo_model::value::append_canonical(&mut full, v);
                }
                full.as_slice().cmp(probe)
            }
            TupleInIter::Owned { row, .. } => {
                let full = encode_tuple_bare(row.as_slice());
                full.as_slice().cmp(probe)
            }
        }
    }

    pub(crate) fn cmp_slice(&self, other: &[DataValue]) -> Ordering {
        match self {
            TupleInIter::Bytes { key, .. } => {
                let probe = encode_tuple_bare(other);
                (*key).cmp(probe.as_slice())
            }
            TupleInIter::MeetSuffix { key, val, .. } => {
                let key = match decode_row_bare(key) {
                    Ok(key) => key,
                    Err(_) => {
                        let probe = encode_tuple_bare(other);
                        return (*key).cmp(probe.as_slice());
                    }
                };
                key.iter()
                    .cloned()
                    .chain(val.iter().map(|a| a.to_value()))
                    .cmp(other.iter().cloned())
            }
            TupleInIter::Values { key, val, .. } => key.iter().chain(val.iter()).cmp(other.iter()),
            TupleInIter::Owned { row, .. } => row.iter().cmp(other.iter()),
        }
    }
}

/// Decode bare row bytes; refuse corrupt encodings.
fn decode_row_bare(bytes: &[u8]) -> Result<Tuple, TempStoreCorruptRefuse> {
    decode_tuple_bare(bytes).map_err(TempStoreCorruptRefuse)
}

/// The `idx`-th self-delimiting value in `bytes`, walking from the start.
fn bare_nth(bytes: &[u8], idx: usize) -> Result<Option<DataValue>, TempStoreCorruptRefuse> {
    let mut remaining = bytes;
    for _ in 0..idx {
        let (_, next) = DataValue::decode_from_key(remaining).map_err(TempStoreCorruptRefuse)?;
        remaining = next;
    }
    if remaining.is_empty() {
        return Ok(None);
    }
    let (val, _) = DataValue::decode_from_key(remaining).map_err(TempStoreCorruptRefuse)?;
    Ok(Some(val))
}

/// Materialize every view in a store scan.
pub fn collect_materialized<'a>(
    iter: impl IntoIterator<Item = TupleInIter<'a>>,
) -> Result<Vec<Tuple>, TempStoreCorruptRefuse> {
    iter.into_iter().map(TupleInIter::try_into_tuple).collect()
}

impl<'a> IntoIterator for TupleInIter<'a> {
    type Item = Result<DataValue, TempStoreCorruptRefuse>;
    type IntoIter = TupleInIterIterator<'a>;

    fn into_iter(self) -> Self::IntoIter {
        TupleInIterIterator {
            state: match self {
                TupleInIter::Bytes { key, .. } => TupleInIterState::Bytes(key),
                TupleInIter::MeetSuffix { key, val, .. } => TupleInIterState::MeetSuffix {
                    key_remaining: key,
                    val,
                    val_idx: 0,
                },
                TupleInIter::Values { key, val, .. } => {
                    TupleInIterState::Values { key, val, idx: 0 }
                }
                TupleInIter::Owned { row, .. } => TupleInIterState::Owned { row, idx: 0 },
            },
        }
    }
}

enum TupleInIterState<'a> {
    Bytes(&'a [u8]),
    /// Decodes `key_remaining` down to empty (self-delimiting, same walk as
    /// `Bytes`), then falls through to `val` — the meet-suffix key's
    /// values always precede the meet-suffix value's in head order.
    MeetSuffix {
        key_remaining: &'a [u8],
        val: &'a [MeetAccum],
        val_idx: usize,
    },
    Values {
        key: &'a [DataValue],
        val: &'a [DataValue],
        idx: usize,
    },
    Owned {
        row: Tuple,
        idx: usize,
    },
}

pub struct TupleInIterIterator<'a> {
    state: TupleInIterState<'a>,
}

impl PartialEq for TupleInIter<'_> {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self.try_materialize(), other.try_materialize()),
            (Ok(a), Ok(b)) if a == b
        )
    }
}

impl Eq for TupleInIter<'_> {}

impl Ord for TupleInIter<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.try_materialize(), other.try_materialize()) {
            (Ok(a), Ok(b)) => a.cmp(&b),
            (Err(_), Ok(_)) => Ordering::Less,
            (Ok(_), Err(_)) => Ordering::Greater,
            (Err(_), Err(_)) => Ordering::Equal,
        }
    }
}

impl PartialOrd for TupleInIter<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq<[DataValue]> for TupleInIter<'_> {
    fn eq(&self, other: &'_ [DataValue]) -> bool {
        matches!(self.try_materialize(), Ok(row) if row.as_slice() == other)
    }
}

impl PartialOrd<[DataValue]> for TupleInIter<'_> {
    fn partial_cmp(&self, other: &'_ [DataValue]) -> Option<Ordering> {
        Some(self.cmp_slice(other))
    }
}

impl Iterator for TupleInIterIterator<'_> {
    type Item = Result<DataValue, TempStoreCorruptRefuse>;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.state {
            TupleInIterState::Bytes(remaining) => {
                if remaining.is_empty() {
                    return None;
                }
                let (val, next) = match DataValue::decode_from_key(remaining) {
                    Ok(pair) => pair,
                    Err(e) => return Some(Err(TempStoreCorruptRefuse(e))),
                };
                *remaining = next;
                Some(Ok(val))
            }
            TupleInIterState::MeetSuffix {
                key_remaining,
                val,
                val_idx,
            } => {
                if !key_remaining.is_empty() {
                    let (v, next) = match DataValue::decode_from_key(key_remaining) {
                        Ok(pair) => pair,
                        Err(e) => return Some(Err(TempStoreCorruptRefuse(e))),
                    };
                    *key_remaining = next;
                    return Some(Ok(v));
                }
                let ret = val.get(*val_idx)?.to_value();
                *val_idx += 1;
                Some(Ok(ret))
            }
            TupleInIterState::Values { key, val, idx } => {
                let ret = match key.get(*idx) {
                    Some(d) => d.clone(),
                    None => val.get(*idx - key.len())?.clone(),
                };
                *idx += 1;
                Some(Ok(ret))
            }
            TupleInIterState::Owned { row, idx } => {
                let ret = row.get(*idx)?.clone();
                *idx += 1;
                Some(Ok(ret))
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Tests (new in KyzoDB; the original file had none)
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use miette::{Result, miette};
    use super::*;
    use kyzo_model::program::aggregate::parse_aggr;
    use kyzo_model::value::ScanBound;

    fn t(vals: &[i64]) -> Tuple {
        vals.iter().map(|v| DataValue::from(*v)).collect()
    }

    fn gv(group: &str, val: DataValue) -> Tuple {
        Tuple::from_vec(vec![DataValue::from(group), val])
    }

    fn plain() -> HeadAggrSlot {
        HeadAggrSlot::Plain
    }

    fn aggr_slot(a: Aggregation) -> HeadAggrSlot {
        HeadAggrSlot::Aggregated {
            aggr: a,
            args: vec![],
        }
    }

    /// A recording sink: collects every admission, in the order reported.
    struct Recorder(Vec<Tuple>);

    impl Recorder {
        fn new() -> Self {
            Self(Vec::new())
        }
    }

    impl AdmissionSink for Recorder {
        const RECORDING: bool = true;
        fn admit(&mut self, tuple: TupleInIter<'_>) -> Result<(), TempStoreCorruptRefuse> {
            self.0.push(tuple.try_into_tuple()?);
            Ok(())
        }
    }

    fn all(store: &EpochStore) -> Result<Vec<Tuple>> {
        Ok(store
            .all_iter()?
            .map(TupleInIter::try_into_tuple)
            .collect::<Result<Vec<_>, _>>()?)
    }

    fn delta(store: &EpochStore) -> Result<Vec<Tuple>> {
        Ok(store
            .delta_all_iter()?
            .map(TupleInIter::try_into_tuple)
            .collect::<Result<Vec<_>, _>>()?)
    }

    // ── total/delta discipline ───────────────────────────────────────────

    /// The semi-naive discipline on a regular store: a first epoch's tuples
    /// are all delta; re-derivation produces an empty delta (fixpoint);
    /// a genuinely new tuple produces exactly itself as the delta.
    #[test]
    fn regular_total_delta_discipline() -> Result<()>  {
        let mut store = EpochStore::new_normal(2);
        assert_eq!(store.arity, 2);
        assert!(!store.has_delta());

        // Epoch 0: two fresh tuples — the fast path swaps the whole
        // out-store into the empty total, and total doubles as delta.
        let mut out0 = RegularTempStore::new();
        out0.put(t(&[1, 1]));
        out0.put(t(&[1, 2]));
        let admitted = store.merge_in(out0.wrap(), &mut ())?;
        assert_eq!(admitted, Admitted(2));
        assert!(store.has_delta());
        assert_eq!(all(&store)?, vec![t(&[1, 1]), t(&[1, 2])]);
        assert_eq!(delta(&store)?, all(&store)?); // first epoch: delta == total

        // Epoch 1: pure re-derivation — empty delta, nothing admitted.
        // This is the termination certificate: eval stops here.
        let mut out1 = RegularTempStore::new();
        out1.put(t(&[1, 1]));
        let admitted = store.merge_in(out1.wrap(), &mut ())?;
        assert_eq!(admitted, Admitted(0));
        assert!(!store.has_delta());
        assert!(delta(&store)?.is_empty());
        assert_eq!(all(&store)?.len(), 2); // total unharmed

        // Epoch 2: one re-derived + one genuinely new — the delta is
        // exactly the new tuple, never the re-derived one.
        let mut out2 = RegularTempStore::new();
        out2.put(t(&[1, 1]));
        out2.put(t(&[2, 3]));
        let admitted = store.merge_in(out2.wrap(), &mut ())?;
        assert_eq!(admitted, Admitted(1));
        assert!(store.has_delta());
        assert_eq!(delta(&store)?, vec![t(&[2, 3])]);
        assert_eq!(all(&store)?, vec![t(&[1, 1]), t(&[1, 2]), t(&[2, 3])]);
        Ok(())
    }

    /// An empty epoch is a fixpoint signal even against a non-empty total.
    #[test]
    fn empty_epoch_is_fixpoint() -> Result<()>  {
        let mut store = EpochStore::new_normal(1);
        let mut out = RegularTempStore::new();
        out.put(t(&[7]));
        store.merge_in(out.wrap(), &mut ())?;
        assert!(store.has_delta());

        store
            .merge_in(RegularTempStore::new().wrap(), &mut ())
            ?;
        assert!(!store.has_delta());
        assert_eq!(all(&store)?, vec![t(&[7])]); // total survives
        Ok(())
    }

    /// The admission sink observes every admission, in canonical key
    /// order, on both the swap fast path and the incremental path — the
    /// deterministic sequence provenance witnesses will bind to.
    #[test]
    fn admission_sink_sees_admissions_in_canonical_order() -> Result<()>  {
        let mut store = EpochStore::new_normal(1);

        // Swap path: puts arrive out of order, admissions are reported in
        // key order because the store is a BTreeMap.
        let mut out0 = RegularTempStore::new();
        out0.put(t(&[3]));
        out0.put(t(&[1]));
        let mut rec = Recorder::new();
        let admitted = store.merge_in(out0.wrap(), &mut rec)?;
        assert_eq!(admitted, Admitted(2));
        assert_eq!(rec.0, vec![t(&[1]), t(&[3])]);

        // Incremental path: only the genuinely new tuple is admitted.
        let mut out1 = RegularTempStore::new();
        out1.put(t(&[3])); // re-derived
        out1.put(t(&[2])); // new
        let mut rec = Recorder::new();
        let admitted = store.merge_in(out1.wrap(), &mut rec)?;
        assert_eq!(admitted, Admitted(1));
        assert_eq!(rec.0, vec![t(&[2])]);
        Ok(())
    }

    // ── the meet changed-flag regression ─────────────────────────────────

    /// REGRESSION (store-level half of the recursive differential): the
    /// delta of a meet merge is driven by `MeetAggrObj::update`'s changed
    /// flag, and the original's `and`/`or` flags were inverted
    /// (`Ok(old == *l)` at upstream aggr.rs:106/146).
    ///
    /// Compute the failure with the OLD flag, `or` case: total holds
    /// {g: false}; the epoch derives (g, true). `update` folds
    /// false | true = true — the value CHANGED — but the old flag returns
    /// `old == *l` = (false == true) = `false`, i.e. "unchanged". So
    /// `merge_in` puts nothing in the delta, `has_delta()` is false, eval
    /// breaks its epoch loop, and every rule recursing through this store
    /// never sees g turn true: a premature fixpoint with silently missing
    /// answers. With the landed corrected ops the delta MUST be non-empty.
    #[test]
    fn meet_delta_regression_changed_flag_drives_delta() -> Result<()>  {
        let or_aggr = parse_aggr("or")?.ok_or_else(|| miette!("parse_aggr"))?;
        let spec = vec![plain(), aggr_slot(or_aggr)];
        let mut store = EpochStore::new_meet(&spec)?;

        // Epoch 0: g starts false.
        let mut out0 = MeetAggrStore::new(spec.clone())?;
        out0.meet_put(gv("g", DataValue::from(false)).as_slice())
            ?;
        let admitted = store.merge_in(out0.wrap(), &mut ())?;
        assert_eq!(admitted, Admitted(1));
        assert!(store.has_delta());
        assert_eq!(all(&store)?, vec![gv("g", DataValue::from(false))]);

        // Epoch 1: the epoch derives (g, true) — the value changes.
        // Old inverted flag: empty delta here (the bug). Landed contract:
        // the changed group, with its UPDATED value, is the delta.
        let mut out1 = MeetAggrStore::new(spec.clone())?;
        out1.meet_put(gv("g", DataValue::from(true)).as_slice())
            ?;
        let mut rec = Recorder::new();
        let admitted = store.merge_in(out1.wrap(), &mut rec)?;
        assert_eq!(admitted, Admitted(1));
        assert!(
            store.has_delta(),
            "changed meet value must produce a delta; empty means premature fixpoint"
        );
        assert_eq!(delta(&store)?, vec![gv("g", DataValue::from(true))]);
        assert_eq!(all(&store)?, vec![gv("g", DataValue::from(true))]);
        // The admission (where a provenance witness would bind) carries the
        // updated value too.
        assert_eq!(rec.0, vec![gv("g", DataValue::from(true))]);

        // Epoch 2: re-deriving (g, false) leaves true | false = true
        // unchanged — genuinely no delta, the fixpoint. (The old inverted
        // flag would have reported "changed" here: spurious epochs, the
        // benign direction of the same bug.)
        let mut out2 = MeetAggrStore::new(spec)?;
        out2.meet_put(gv("g", DataValue::from(false)).as_slice())
            ?;
        let admitted = store.merge_in(out2.wrap(), &mut ())?;
        assert_eq!(admitted, Admitted(0));
        assert!(!store.has_delta());
        assert_eq!(all(&store)?, vec![gv("g", DataValue::from(true))]);
        Ok(())
    }

    /// The same contract at the `meet_put` level: a fold that moves the
    /// value reports `true`, a fold that doesn't reports `false` —
    /// exercised for both a lattice where the bug lived (`or`) and an
    /// always-correct one (`min`).
    #[test]
    fn meet_put_changed_flag() -> Result<()>  {
        let or_aggr = parse_aggr("or")?.ok_or_else(|| miette!("parse_aggr"))?;
        let spec = vec![plain(), aggr_slot(or_aggr)];
        let mut store = MeetAggrStore::new(spec)?;
        assert!(store.is_empty());
        // New group: changed.
        assert!(
            store
                .meet_put(gv("g", DataValue::from(false)).as_slice())
                ?
        );
        // false | true = true: CHANGED (the old inverted flag said false).
        assert!(
            store
                .meet_put(gv("g", DataValue::from(true)).as_slice())
                ?
        );
        // true | true = true: unchanged (the old flag said changed).
        assert!(
            !store
                .meet_put(gv("g", DataValue::from(true)).as_slice())
                ?
        );
        assert!(!store.is_empty());

        let min_aggr = parse_aggr("min")?.ok_or_else(|| miette!("parse_aggr"))?;
        let spec = vec![plain(), aggr_slot(min_aggr)];
        let mut store = MeetAggrStore::new(spec)?;
        assert!(
            store
                .meet_put(gv("g", DataValue::from(5i64)).as_slice())
                ?
        );
        assert!(
            !store
                .meet_put(gv("g", DataValue::from(7i64)).as_slice())
                ?
        ); // 5 stays
        assert!(
            store
                .meet_put(gv("g", DataValue::from(3i64)).as_slice())
                ?
        ); // 3 wins
        assert!(store.exists(gv("g", DataValue::from(999i64)).as_slice())); // group-key lookup
        Ok(())
    }

    /// A meet store refuses a normal-only aggregation at construction —
    /// a typed error where the original unwrapped an `Option` per row.
    #[test]
    fn meet_store_rejects_normal_aggregation() -> Result<()>  {
        let count = parse_aggr("count")?.ok_or_else(|| miette!("parse_aggr"))?;
        assert!(!count.is_meet());
        let res = MeetAggrStore::new(vec![plain(), aggr_slot(count)]);
        assert!(res.is_err());
        Ok(())
    }

    // ── iteration surface ────────────────────────────────────────────────

    /// Meet stores fold per group but iterate whole head tuples (derived
    /// from `by_group` via interleave); prefix iteration and indexed access
    /// see one logical tuple. For this suffix layout the group key is a
    /// prefix, so scans look like a regular store — the non-suffix cases
    /// below prove the same surface holds when the group key is not a prefix.
    #[test]
    fn meet_iteration_spans_key_and_value() -> Result<()>  {
        let min_aggr = parse_aggr("min")?.ok_or_else(|| miette!("parse_aggr"))?;
        let spec = vec![plain(), aggr_slot(min_aggr)];
        let mut store = EpochStore::new_meet(&spec)?;
        assert_eq!(store.arity, 2);

        let mut out = MeetAggrStore::new(spec)?;
        out.meet_put(gv("a", DataValue::from(4i64)).as_slice())
            ?;
        out.meet_put(gv("b", DataValue::from(2i64)).as_slice())
            ?;
        store.merge_in(out.wrap(), &mut ())?;

        assert!(store.exists(gv("a", DataValue::from(0i64)).as_slice()));
        assert!(!store.exists(gv("c", DataValue::from(0i64)).as_slice()));

        let got = store
            .prefix_iter(&Tuple::from_vec(vec![DataValue::from("b")]))
            ?
            .map(TupleInIter::try_into_tuple)
            .collect::<Result<Vec<_>, _>>()
            ?;
        assert_eq!(got, vec![gv("b", DataValue::from(2i64))]);

        // Indexed access crosses the key/value seam.
        let row = store.all_iter()?.next().ok_or_else(|| miette!("row"))?;
        assert_eq!(row.try_get(0)?, DataValue::from("a"));
        assert_eq!(row.try_get(1)?, DataValue::from(4i64));

        // Bounded range scans over the whole head tuple (derived order).
        let bounds = |group: &str, v: i64| {
            vec![
                ScanBound::Value(DataValue::from(group)),
                ScanBound::Value(DataValue::from(v)),
            ]
        };
        let lower = bounds("a", 5); // above (a, 4)
        let upper = bounds("b", 2); // exactly (b, 2)
        let got = store
            .range_iter(&lower, &upper, true)
            ?
            .map(TupleInIter::try_into_tuple)
            .collect::<Result<Vec<_>, _>>()
            ?;
        assert_eq!(got, vec![gv("b", DataValue::from(2i64))]);
        let got = store
            .range_iter(&lower, &upper, false)
            ?
            .map(TupleInIter::try_into_tuple)
            .collect::<Result<Vec<_>, _>>()
            ?;
        assert!(got.is_empty());
        Ok(())
    }

    /// Limiter-skipped tuples participate in joins (all_iter) but not in
    /// the early-returned rows.
    #[test]
    fn skip_flags_gate_early_return_only() -> Result<()>  {
        let mut store = EpochStore::new_normal(1);
        let mut out = RegularTempStore::new();
        out.put(t(&[1]));
        out.put_with_skip(t(&[2]));
        assert!(out.exists(t(&[2]).as_slice()));
        store.merge_in(out.wrap(), &mut ())?;

        assert!(store.exists(t(&[2]).as_slice()));
        assert_eq!(all(&store)?.len(), 2);
        let returned = store
            .early_returned_iter()
            ?
            .map(TupleInIter::try_into_tuple)
            .collect::<Result<Vec<_>, _>>()
            ?;
        assert_eq!(returned, vec![t(&[1])]);

        // Delta iteration honors the same store (first epoch: via total).
        let d = store
            .delta_prefix_iter(&Tuple::from_vec(vec![DataValue::from(2i64)]))
            ?
            .map(TupleInIter::try_into_tuple)
            .collect::<Result<Vec<_>, _>>()
            ?;
        assert_eq!(d, vec![t(&[2])]);
        Ok(())
    }

    /// Mismatched store kinds at a merge are an error, not an abort.
    #[test]
    fn kind_mismatch_is_error_not_panic() -> Result<()>  {
        let mut store = EpochStore::new_normal(1);
        let min_aggr = parse_aggr("min")?.ok_or_else(|| miette!("parse_aggr"))?;
        let meet_out = MeetAggrStore::new(vec![aggr_slot(min_aggr)])?;
        assert!(store.merge_in(meet_out.wrap(), &mut ()).is_err());
        Ok(())
    }

    // ── non-suffix meet layouts: positional grouping ─────────────────────

    /// A head tuple with the meet value at position 0 and the grouping key
    /// at position 1 — the layout the suffix-prefix store could not hold.
    fn vg(val: DataValue, group: &str) -> Tuple {
        Tuple::from_vec(vec![val, DataValue::from(group)])
    }

    /// [`MeetLayout`] projections and their inverse round-trip for an
    /// interleaved layout (a grouping column between two meet columns): the
    /// key and value projections partition the row and `interleave` rebuilds
    /// it exactly, leaving no `Null`. This is the layout proof the whole
    /// positional grouping rests on — the mutation target.
    #[test]
    fn meet_layout_projection_round_trips_interleaved() -> Result<()>  {
        let min_aggr = parse_aggr("min")?.ok_or_else(|| miette!("parse_aggr"))?;
        let max_aggr = parse_aggr("max")?.ok_or_else(|| miette!("parse_aggr"))?;
        let spec = vec![aggr_slot(min_aggr), plain(), aggr_slot(max_aggr)];
        let layout = MeetLayout::from_signature(&spec);
        assert_eq!(layout.key_positions, vec![HeadPos::from_index(1)]);
        assert_eq!(
            layout.val_positions,
            vec![HeadPos::from_index(0), HeadPos::from_index(2)]
        );

        let row: Tuple = Tuple::from_vec(vec![
            DataValue::from(2i64),
            DataValue::from("g"),
            DataValue::from(9i64),
        ]);
        let key = layout.project_key(row.as_slice());
        let vals = layout.project_vals(row.as_slice());
        assert_eq!(key, Tuple::from_vec(vec![DataValue::from("g")]));
        assert_eq!(
            vals,
            vec![
                MeetAccum::from_derived(DataValue::from(2i64)),
                MeetAccum::from_derived(DataValue::from(9i64)),
            ]
        );
        assert_eq!(
            layout
                .interleave(&OwnBareKey::encode(key.as_slice()), vals.as_slice())
                ?,
            row,
            "interleave is the exact inverse of the two projections"
        );
        Ok(())
    }

    /// Meet at position 0: the fold groups on position 1, values fold at
    /// position 0, `exists` is group membership projected to position 1, and
    /// the scan surface iterates whole head tuples in canonical (value-first)
    /// order.
    #[test]
    fn meet_put_and_scan_non_suffix() -> Result<()>  {
        let min_aggr = parse_aggr("min")?.ok_or_else(|| miette!("parse_aggr"))?;
        let spec = vec![aggr_slot(min_aggr), plain()];
        let mut out = MeetAggrStore::new(spec.clone())?;
        // group "a": min(4, 2, 9) = 2 ; group "b": 5.
        assert!(
            out.meet_put(vg(DataValue::from(4i64), "a").as_slice())
                ?
        );
        assert!(
            out.meet_put(vg(DataValue::from(2i64), "a").as_slice())
                ?
        ); // 2 < 4
        assert!(
            !out.meet_put(vg(DataValue::from(9i64), "a").as_slice())
                ?
        ); // 2 stays
        assert!(
            out.meet_put(vg(DataValue::from(5i64), "b").as_slice())
                ?
        );
        // `exists` is group membership, projected to position 1 (the value
        // in the probe is irrelevant).
        assert!(out.exists(vg(DataValue::from(999i64), "a").as_slice()));
        assert!(!out.exists(vg(DataValue::from(0i64), "c").as_slice()));

        let mut store = EpochStore::new_meet(&spec)?;
        store.merge_in(out.wrap(), &mut ())?;
        // Head-tuple order (derived from by_group): (2,"a") < (5,"b") on position 0.
        assert_eq!(
            all(&store)?,
            vec![
                vg(DataValue::from(2i64), "a"),
                vg(DataValue::from(5i64), "b")
            ]
        );
        // Indexed access spans the whole logical tuple.
        let row = store.all_iter()?.next().ok_or_else(|| miette!("row"))?;
        assert_eq!(row.try_get(0)?, DataValue::from(2i64));
        assert_eq!(row.try_get(1)?, DataValue::from("a"));
        Ok(())
    }

    /// The determinism-critical seam: for a non-suffix layout the group-key
    /// order and the head-tuple order genuinely differ, and admissions are
    /// reported in **group-key** order (the `by_group` iteration the merge
    /// barrier and provenance witnesses depend on), while the scan surface
    /// (derived via interleave) stays in head-tuple order. A store that
    /// admitted in row order would reorder every witness table.
    #[test]
    fn meet_admissions_follow_group_key_order_not_row_order_non_suffix() -> Result<()>  {
        let min_aggr = parse_aggr("min")?.ok_or_else(|| miette!("parse_aggr"))?;
        let spec = vec![aggr_slot(min_aggr), plain()];
        let mut out = MeetAggrStore::new(spec.clone())?;
        // Group "a" holds value 9, group "z" holds value 1. Group-key order
        // is a < z; head-tuple (value-first) order is (1,z) < (9,a) — the
        // two orders disagree, which is the whole point.
        out.meet_put(vg(DataValue::from(9i64), "a").as_slice())
            ?;
        out.meet_put(vg(DataValue::from(1i64), "z").as_slice())
            ?;

        let mut store = EpochStore::new_meet(&spec)?;
        let mut rec = Recorder::new();
        store.merge_in(out.wrap(), &mut rec)?;
        assert_eq!(
            rec.0,
            vec![
                vg(DataValue::from(9i64), "a"),
                vg(DataValue::from(1i64), "z")
            ],
            "admissions are in group-key order (a before z)"
        );
        assert_eq!(
            all(&store)?,
            vec![
                vg(DataValue::from(1i64), "z"),
                vg(DataValue::from(9i64), "a")
            ],
            "the scan surface is in head-tuple order (value-first)"
        );
        Ok(())
    }

    /// The changed-flag delta discipline holds at a non-suffix layout too:
    /// a genuinely lower value at position 0 changes the group and is the
    /// delta (carrying the updated value); a non-improving value changes
    /// nothing and yields an empty delta (the fixpoint). The scan surface
    /// stays the interleave of `by_group` across the update.
    #[test]
    fn meet_merge_delta_non_suffix() -> Result<()>  {
        let min_aggr = parse_aggr("min")?.ok_or_else(|| miette!("parse_aggr"))?;
        let spec = vec![aggr_slot(min_aggr), plain()];
        let mut store = EpochStore::new_meet(&spec)?;

        let mut out0 = MeetAggrStore::new(spec.clone())?;
        out0.meet_put(vg(DataValue::from(5i64), "a").as_slice())
            ?;
        assert_eq!(store.merge_in(out0.wrap(), &mut ())?, Admitted(1));
        assert_eq!(all(&store)?, vec![vg(DataValue::from(5i64), "a")]);

        // Epoch 1: a lower value moves the group — delta carries it, and the
        // scan surface reflects the new row, not the old.
        let mut out1 = MeetAggrStore::new(spec.clone())?;
        out1.meet_put(vg(DataValue::from(3i64), "a").as_slice())
            ?;
        let mut rec = Recorder::new();
        assert_eq!(store.merge_in(out1.wrap(), &mut rec)?, Admitted(1));
        assert!(store.has_delta());
        assert_eq!(delta(&store)?, vec![vg(DataValue::from(3i64), "a")]);
        assert_eq!(all(&store)?, vec![vg(DataValue::from(3i64), "a")]);
        assert_eq!(rec.0, vec![vg(DataValue::from(3i64), "a")]);

        // Epoch 2: a non-improving value changes nothing — empty delta.
        let mut out2 = MeetAggrStore::new(spec)?;
        out2.meet_put(vg(DataValue::from(8i64), "a").as_slice())
            ?;
        assert_eq!(store.merge_in(out2.wrap(), &mut ())?, Admitted(0));
        assert!(!store.has_delta());
        assert_eq!(all(&store)?, vec![vg(DataValue::from(3i64), "a")]);
        Ok(())
    }

    // ── adversarial reviewer attacks (adopted from the hostile pass) ──────
    //
    // P036: no by_row twin. The scan surface of a sealed meet store must
    // equal `{ interleave(k, v) : (k, v) ∈ groups }` in head-tuple order
    // for every regime; the out-store's by_group is the sole authority.

    /// P036: `by_group` is the sole MeetAggrStore authority — every group
    /// interleaves to a head tuple that `exists` recognizes.
    fn rev_assert_lockstep(store: &MeetAggrStore) -> Result<()> {
        for (k, v) in &store.by_group {
            let row = store.layout.interleave(k.as_ref(), v.as_slice())?;
            assert!(
                store.exists(row.as_slice()),
                "interleaved head must exist for its group"
            );
        }
        Ok(())
    }

    /// Scan surface equals the interleave of every sealed meet level's groups
    /// (newest owner wins per group key).
    fn rev_assert_scan_from_groups(store: &EpochStore) -> Result<()> {
        let LevelKind::Meet { spec, levels } = &store.kind else {
            // #[cfg(test)] helper — wrong kind is a test bug, not a product path.
            panic!("rev_assert_scan_from_groups: expected meet EpochStore");
        };
        let mut by_group: BTreeMap<Box<OwnBareKey>, Tuple> = BTreeMap::new();
        for level in levels.iter() {
            for (k, v) in &level.groups {
                by_group.insert(
                    k.clone(),
                    spec.layout.interleave(k.as_ref(), v.as_slice())?,
                );
            }
        }
        let mut derived: Vec<Tuple> = by_group.into_values().collect();
        derived.sort();
        assert_eq!(
            all(store)?,
            derived,
            "scan surface diverged from interleave of groups (P036)"
        );
        Ok(())
    }

    /// ATTACK 2a/2c: drive a non-suffix meet EpochStore through every
    /// mutation path — put (vacant, changed, unchanged), fast-path swap,
    /// incremental merge (vacant, changed, unchanged), empty-epoch merge —
    /// asserting the scan surface stays the interleave of groups.
    #[test]
    fn rev_meet_views_lockstep_through_all_paths() -> Result<()>  {
        let min_aggr = parse_aggr("min")?.ok_or_else(|| miette!("parse_aggr"))?;
        let spec = vec![aggr_slot(min_aggr), plain()];

        // meet_put: vacant, changed, unchanged.
        let mut out = MeetAggrStore::new(spec.clone())?;
        assert!(
            out.meet_put(vg(DataValue::from(9i64), "a").as_slice())
                ?
        );
        assert!(
            out.meet_put(vg(DataValue::from(4i64), "a").as_slice())
                ?
        );
        assert!(
            !out.meet_put(vg(DataValue::from(7i64), "a").as_slice())
                ?
        );
        assert!(
            out.meet_put(vg(DataValue::from(1i64), "z").as_slice())
                ?
        );

        // Fast-path swap into an empty total (use_total_for_delta epoch).
        let mut store = EpochStore::new_meet(&spec)?;
        store.merge_in(out.wrap(), &mut ())?;
        rev_assert_scan_from_groups(&store)?;

        // Incremental merge: one changed group, one unchanged, one vacant.
        let mut out1 = MeetAggrStore::new(spec.clone())?;
        out1.meet_put(vg(DataValue::from(2i64), "a").as_slice())
            ?; // changes 4 -> 2
        out1.meet_put(vg(DataValue::from(5i64), "z").as_slice())
            ?; // no change
        out1.meet_put(vg(DataValue::from(8i64), "q").as_slice())
            ?; // vacant
        store.merge_in(out1.wrap(), &mut ())?;
        rev_assert_scan_from_groups(&store)?;
        assert_eq!(
            all(&store)?,
            vec![
                vg(DataValue::from(1i64), "z"),
                vg(DataValue::from(2i64), "a"),
                vg(DataValue::from(8i64), "q"),
            ],
            "scan surface reflects post-merge rows in head-tuple order"
        );
        assert_eq!(
            delta(&store)?,
            vec![
                vg(DataValue::from(2i64), "a"),
                vg(DataValue::from(8i64), "q"),
            ],
            "delta carries exactly the changed + new groups"
        );

        // Empty epoch: fixpoint; totals untouched, delta empty.
        let out2 = MeetAggrStore::new(spec.clone())?;
        store.merge_in(out2.wrap(), &mut ())?;
        rev_assert_scan_from_groups(&store)?;
        assert!(!store.has_delta());

        // The stale old row must be GONE from the scan surface (not shadowed).
        assert_eq!(all(&store)?.len(), 3);
        assert!(!all(&store)?.contains(&vg(DataValue::from(4i64), "a")));
        Ok(())
    }

    /// ATTACK 1: empty group key (all positions aggregated) — the group key
    /// is the empty tuple, one group total; exists() answers for any probe;
    /// scans see the single interleaved row.
    #[test]
    fn rev_meet_all_aggregated_single_group() -> Result<()>  {
        let min_aggr = parse_aggr("min")?.ok_or_else(|| miette!("parse_aggr"))?;
        let max_aggr = parse_aggr("max")?.ok_or_else(|| miette!("parse_aggr"))?;
        let spec = vec![aggr_slot(min_aggr), aggr_slot(max_aggr)];
        let mut out = MeetAggrStore::new(spec.clone())?;
        assert!(out.meet_put(t(&[5, 5]).as_slice())?);
        assert!(out.meet_put(t(&[3, 9]).as_slice())?); // min 3, max 9
        assert!(!out.meet_put(t(&[4, 6]).as_slice())?); // no change
        assert_eq!(out.by_group.len(), 1, "one group keyed by the empty tuple");
        assert!(
            out.exists(t(&[100, -100]).as_slice()),
            "any probe hits the one group"
        );
        rev_assert_lockstep(&out)?;

        let mut store = EpochStore::new_meet(&spec)?;
        store.merge_in(out.wrap(), &mut ())?;
        assert_eq!(all(&store)?, vec![t(&[3, 9])]);
        Ok(())
    }

    /// ATTACK 3: admissions across WILDLY different insertion orders are
    /// identical (group-key order), and the merged store is byte-identical
    /// — insertion order must be laundered out by both views.
    #[test]
    fn rev_meet_admissions_insertion_order_independent() -> Result<()>  {
        let min_aggr = parse_aggr("min")?.ok_or_else(|| miette!("parse_aggr"))?;
        let spec = vec![aggr_slot(min_aggr), plain()];
        let rows: Vec<Tuple> = vec![
            vg(DataValue::from(9i64), "m"),
            vg(DataValue::from(1i64), "z"),
            vg(DataValue::from(5i64), "a"),
            vg(DataValue::from(3i64), "m"),
            vg(DataValue::from(7i64), "b"),
            vg(DataValue::from(2i64), "a"),
        ];
        let run = |order: &[usize]| -> Result<(Vec<Tuple>, Vec<Tuple>, Vec<Tuple>)> {
            let mut out = MeetAggrStore::new(spec.clone())?;
            for i in order {
                out.meet_put(rows[*i].as_slice())?;
            }
            let mut store = EpochStore::new_meet(&spec)?;
            let mut rec = Recorder::new();
            store.merge_in(out.wrap(), &mut rec)?;
            Ok((rec.0, all(&store)?, delta(&store)?))
        };
        let baseline = run(&[0, 1, 2, 3, 4, 5])?;
        for order in [[5, 4, 3, 2, 1, 0], [3, 0, 5, 2, 4, 1], [2, 5, 0, 3, 1, 4]] {
            assert_eq!(run(&order)?, baseline, "order {order:?} diverged");
        }
        // And the admission sequence really is group-key order.
        assert_eq!(
            baseline.0,
            vec![
                vg(DataValue::from(2i64), "a"),
                vg(DataValue::from(7i64), "b"),
                vg(DataValue::from(3i64), "m"),
                vg(DataValue::from(1i64), "z"),
            ]
        );
        Ok(())
    }

    /// TupleInIter comparison: against other views and plain slices.
    #[test]
    fn tuple_in_iter_ordering() -> Result<()>  {
        let k1 = t(&[1]);
        let v1 = t(&[5]);
        let k2 = t(&[1, 5]);
        let joined = TupleInIter::new(k1.as_slice(), v1.as_slice(), LimiterSkip::Include);
        let flat = TupleInIter::new(
            k2.as_slice(),
            empty_tuple_ref().as_slice(),
            LimiterSkip::Include,
        );
        assert_eq!(joined, flat);
        assert_eq!(joined.cmp(&flat), Ordering::Equal);
        assert_eq!(
            joined.partial_cmp(t(&[1, 6]).as_slice()),
            Some(Ordering::Less)
        );
        assert!(joined.eq(t(&[1, 5]).as_slice()));
        Ok(())
    }
}
