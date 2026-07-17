/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::cmp::Ordering;
use std::fmt::{Debug, Formatter};

use itertools::{Either, Itertools};
use miette::{Diagnostic, Result, bail};
use thiserror::Error;

use crate::data::aggr::{Aggregation, MeetAccum, MeetAggr};
use crate::data::program::HeadAggrSlot;
use crate::data::value::DataValue;
use crate::data::value::{
    ScanBound, Tuple, bare_bounds_lower, bare_bounds_upper, bare_prefix_len, encode_tuple_bare,
};
use crate::query::temp_store::{
    AdmissionSink, Admitted, LimiterSkip, MeetAggrStore, MeetLayout, TempStore, TupleInIter,
};

#[derive(Debug, Error, Diagnostic)]
#[error("level arena overflow: flattened byte length {len} exceeds u32::MAX")]
#[diagnostic(code(query::level_arena_overflow))]
struct LevelArenaOverflow {
    len: usize,
}


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
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[repr(transparent)]
pub(crate) struct LevelArenaBytes(pub(crate) Vec<u8>);

const _: () = assert!(std::mem::size_of::<LevelArenaBytes>() == std::mem::size_of::<Vec<u8>>());
const _: () = assert!(std::mem::align_of::<LevelArenaBytes>() == std::mem::align_of::<Vec<u8>>());

impl std::ops::Deref for LevelArenaBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] { &self.0 }
}
impl std::ops::DerefMut for LevelArenaBytes {
    fn deref_mut(&mut self) -> &mut [u8] { &mut self.0 }
}
impl AsRef<[u8]> for LevelArenaBytes {
    fn as_ref(&self) -> &[u8] { &self.0 }
}
impl LevelArenaBytes {
    /// Empty arena — the only open mint beside appending encoded row bytes.
    pub(crate) fn new() -> Self {
        Self(Vec::new())
    }
}

/// Inclusive/exclusive scan bound key bytes for stored levels.
/// Mint only via [`LevelBoundKey::from_encoded`] (encode-door bytes).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[repr(transparent)]
pub(crate) struct LevelBoundKey(Vec<u8>);

const _: () = assert!(std::mem::size_of::<LevelBoundKey>() == std::mem::size_of::<Vec<u8>>());
const _: () = assert!(std::mem::align_of::<LevelBoundKey>() == std::mem::align_of::<Vec<u8>>());

impl LevelBoundKey {
    /// Bound key from bytes already produced by the bare tuple encode door.
    pub(crate) fn from_encoded(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Extend an exclusive upper bound (successor sentinel byte).
    pub(crate) fn push_exclusive_sentinel(&mut self) {
        self.0.push(0xFF);
    }
}

impl std::ops::Deref for LevelBoundKey {
    type Target = [u8];
    fn deref(&self) -> &[u8] { &self.0 }
}
impl AsRef<[u8]> for LevelBoundKey {
    fn as_ref(&self) -> &[u8] { &self.0 }
}


/// Per-row skip/refresh flags for a sealed normal level. Limiter disposition
/// is [`LimiterSkip`]; refresh is a named field — never an anonymous
/// `(bool, bool)` soup.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RowFlags {
    pub(crate) skip: LimiterSkip,
    pub(crate) refresh: bool,
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
        self.above.last().unwrap_or(&self.bottom)
    }

    pub(crate) fn len(&self) -> usize {
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

#[derive(Debug, Default)]
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
    pub(crate) fn len(&self) -> usize {
        self.offsets.len()
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }
    /// Row bytes at index `i`, or `None` when out of bounds.
    pub(crate) fn row(&self, i: usize) -> Option<&[u8]> {
        (i < self.len()).then(|| self.row_at(i))
    }

    /// INVARIANT(level_row_in_bounds): `i < self.len()`.
    fn row_at(&self, i: usize) -> &[u8] {
        debug_assert!(i < self.len());
        let start = if i == 0 {
            0
        } else {
            self.offsets[i - 1] as usize
        };
        &self.values[start..self.offsets[i] as usize]
    }

    /// INVARIANT(level_row_in_bounds): `i < self.len()`.
    fn row_flags_at(&self, i: usize) -> RowFlags {
        debug_assert!(i < self.flags.len());
        self.flags[i]
    }
    /// Seal an admitted row's bytes into the level — one arena append, no
    /// per-row heap allocation beyond the byte string `RegularTempStore`
    /// already minted at derivation. Refuses when the arena end would
    /// overflow the `u32` offset encoding.
    fn push(&mut self, row: Box<[u8]>, flags: RowFlags) -> Result<()> {
        self.values.0.extend_from_slice(&row);
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
        self.values.0.extend_from_slice(other.row_at(i));
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
            match self.row_at(mid).cmp(key_bytes) {
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
            let stored = self.row_at(i);
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

    fn bounds(&self, lower: &LevelBoundKey, upper: &LevelBoundKey, upper_inclusive: bool) -> (usize, usize) {
        let (lower, upper) = (lower.0.to_vec(), upper.0.to_vec());
        let mut lo = 0usize;
        let mut hi = self.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.row_at(mid) < lower.as_slice() {
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
                self.row_at(mid) <= upper.as_slice()
            } else {
                self.row_at(mid) < upper.as_slice()
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
#[derive(Debug, Default)]
pub(crate) struct MeetLevel {
    /// Group key bytes (story #77, same [`encode_tuple_bare`] treatment as
    /// `NormalLevel`'s rows) → folded meet accumulators.
    pub(crate) groups: Vec<(Box<[u8]>, Vec<MeetAccum>)>,
}

impl MeetLevel {
    fn find(&self, group_key: &[u8]) -> Option<&(Box<[u8]>, Vec<MeetAccum>)> {
        self.groups
            .binary_search_by(|(k, _)| k.as_ref().cmp(group_key))
            .ok()
            .map(|i| &self.groups[i])
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
pub(crate) struct EpochStore {
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
            kind: LevelKind::Normal(LevelStack::singleton(NormalLevel::default())),
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
                levels: LevelStack::singleton(MeetLevel::default()),
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
                    .any(|l| l.find(group.as_ref()).is_some())
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
                let mut level = NormalLevel::default();
                let mut admitted = 0usize;
                for (row_bytes, skip) in new.inner {
                    let existing = levels
                        .iter()
                        .rev()
                        .find_map(|l| l.find(&row_bytes).map(|i| l.row_flags_at(i)));
                    match existing {
                        None => {
                            if S::RECORDING {
                                sink.admit(TupleInIter::new_bytes(&row_bytes, skip));
                            }
                            level.push(
                                row_bytes,
                                RowFlags {
                                    skip,
                                    refresh: false,
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
                                    refresh: true,
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
                let mut level = MeetLevel::default();
                let mut admitted = 0usize;
                for (group, incoming) in new.by_group {
                    let folded = match levels.iter().rev().find_map(|l| l.find(&group)) {
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
                            let row = spec.layout.interleave(&group, vals.as_slice());
                            sink.admit(TupleInIter::owned(row, LimiterSkip::Include));
                        }
                        level.groups.push((group, vals));
                        admitted += 1;
                    }
                }
                levels.drop_consumed_empty_delta(|l| l.groups.is_empty());
                compact_meet(spec, levels);
                levels.push(level);
                admitted
            }
            _ => bail!(
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
                (0..l.len()).any(|i| !l.row_flags_at(i).refresh)
            }
            LevelKind::Meet { levels, .. } => !levels.last().groups.is_empty(),
        }
    }

    pub(crate) fn range_iter<'s>(
        &'s self,
        lower: &[ScanBound],
        upper: &[ScanBound],
        upper_inclusive: bool,
    ) -> impl Iterator<Item = TupleInIter<'s>> + use<'s> {
        self.ranged(
            LevelBoundKey::from_encoded(bare_bounds_lower(lower)),
            LevelBoundKey::from_encoded(bare_bounds_upper(upper)),
            upper_inclusive,
            false,
        )
    }
    pub(crate) fn delta_range_iter<'s>(
        &'s self,
        lower: &[ScanBound],
        upper: &[ScanBound],
        upper_inclusive: bool,
    ) -> impl Iterator<Item = TupleInIter<'s>> + use<'s> {
        self.ranged(
            LevelBoundKey::from_encoded(bare_bounds_lower(lower)),
            LevelBoundKey::from_encoded(bare_bounds_upper(upper)),
            upper_inclusive,
            true,
        )
    }
    pub(crate) fn prefix_iter<'s>(
        &'s self,
        prefix: &Tuple,
    ) -> impl Iterator<Item = TupleInIter<'s>> + use<'s> {
        // The 0xFF tail (which no canonical encoding begins) bounds every
        // extension of `prefix`, inclusively.
        let lower = LevelBoundKey::from_encoded(encode_tuple_bare(prefix.as_slice()));
        let mut upper = lower.clone();
        upper.push_exclusive_sentinel();
        self.ranged(lower, upper, true, false)
    }
    pub(crate) fn delta_prefix_iter<'s>(
        &'s self,
        prefix: &Tuple,
    ) -> impl Iterator<Item = TupleInIter<'s>> + use<'s> {
        let lower = LevelBoundKey::from_encoded(encode_tuple_bare(prefix.as_slice()));
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
    ) -> impl Iterator<Item = TupleInIter<'s>> + use<'s> {
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
                Either::Left(std::iter::from_fn(move || {
                    normal_merge_next(&mut cursors, delta_only)
                }))
            }
            LevelKind::Meet { spec, levels } => {
                let prefix: Tuple = cols.iter().map(|&c| row[c].clone()).collect();
                let lower = LevelBoundKey::from_encoded(encode_tuple_bare(prefix.as_slice()));
                let mut upper = lower.clone();
                upper.push_exclusive_sentinel();
                let all: Vec<&'s MeetLevel> = levels.iter().collect();
                let picked: Vec<&'s MeetLevel> = if delta_only {
                    vec![levels.last()]
                } else {
                    all.clone()
                };
                let newer: Vec<&'s MeetLevel> = if delta_only { vec![] } else { all };
                Either::Right(meet_ranged(spec, picked, newer, lower, upper, true))
            }
        }
    }

    pub(crate) fn all_iter(&self) -> impl Iterator<Item = TupleInIter<'_>> {
        self.prefix_iter(&Tuple::new())
    }
    pub(crate) fn delta_all_iter(&self) -> impl Iterator<Item = TupleInIter<'_>> {
        self.delta_prefix_iter(&Tuple::new())
    }
    /// The rows an early-returned (`:limit`-satisfied) entry rule actually
    /// returns: everything not flagged limiter-skipped.
    pub(crate) fn early_returned_iter(&self) -> impl Iterator<Item = TupleInIter<'_>> {
        self.all_iter().filter(|t| !t.should_skip())
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
    ) -> impl Iterator<Item = TupleInIter<'s>> + use<'s> {
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
                Either::Left(std::iter::from_fn(move || {
                    normal_merge_next(&mut cursors, delta_only)
                }))
            }
            LevelKind::Meet { spec, levels } => {
                let all: Vec<&'s MeetLevel> = levels.iter().collect();
                let picked: Vec<&'s MeetLevel> = if delta_only {
                    vec![levels.last()]
                } else {
                    all.clone()
                };
                let newer: Vec<&'s MeetLevel> = if delta_only { vec![] } else { all };
                let it = meet_ranged(spec, picked, newer, lower, upper, upper_inclusive);
                Either::Right(it)
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
                let t = l.row_at(*next);
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
        let key: Vec<u8> = cursors[win_ci].0.row_at(win_row).to_vec();
        for (l, next, end) in cursors.iter_mut() {
            while *next < *end && l.row_at(*next) == key.as_slice() {
                *next += 1;
            }
        }
        let l = cursors[win_ci].0;
        let flags = l.row_flags_at(win_row);
        if delta_only && flags.refresh {
            continue;
        }
        return Some(TupleInIter::new_bytes(l.row_at(win_row), flags.skip));
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
            let mut merged = NormalLevel::default();
            let (mut a, mut b) = (0usize, 0usize);
            while a < older.len() || b < newer.len() {
                if a >= older.len() {
                    merged.push_from(newer, b, newer.row_flags_at(b))?;
                    b += 1;
                } else if b >= newer.len() {
                    merged.push_from(older, a, older.row_flags_at(a))?;
                    a += 1;
                } else {
                    match older.row_at(a).cmp(newer.row_at(b)) {
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
                                    refresh: false,
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
    )
}

/// As [`compact_normal`], for meet levels: newest group value wins whole.
fn compact_meet(_spec: &MeetSpec, levels: &mut LevelStack<MeetLevel>) {
    levels
        .compact_while(
            |older, newer| newer.groups.len() * 2 >= older.groups.len(),
            |older, newer| {
                let mut merged = MeetLevel {
                    groups: Vec::with_capacity(older.groups.len() + newer.groups.len()),
                };
                let (mut a, mut b) = (
                    older.groups.iter().peekable(),
                    newer.groups.iter().peekable(),
                );
                loop {
                    match (a.peek(), b.peek()) {
                        (Some((ka, _)), Some((kb, _))) => match ka.cmp(kb) {
                            Ordering::Less => merged.groups.push(
                                a.next()
                                    .expect("INVARIANT(peeked): peek proved Some")
                                    .clone(),
                            ),
                            Ordering::Greater => merged.groups.push(
                                b.next()
                                    .expect("INVARIANT(peeked): peek proved Some")
                                    .clone(),
                            ),
                            Ordering::Equal => {
                                let g = b
                                    .next()
                                    .expect("INVARIANT(peeked): peek proved Some")
                                    .clone();
                                a.next();
                                merged.groups.push(g);
                            }
                        },
                        (Some(_), None) => merged.groups.push(
                            a.next()
                                .expect("INVARIANT(peeked): peek proved Some")
                                .clone(),
                        ),
                        (None, Some(_)) => merged.groups.push(
                            b.next()
                                .expect("INVARIANT(peeked): peek proved Some")
                                .clone(),
                        ),
                        (None, None) => break,
                    }
                }
                Ok(merged)
            },
        )
        .expect("INVARIANT(meet_compact): merge arm is infallible");
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
) -> Box<dyn Iterator<Item = TupleInIter<'s>> + 's> {
    let within = move |row: &TupleInIter<'_>| -> bool {
        row.cmp_bare(&lower) != Ordering::Less
            && match row.cmp_bare(&upper) {
                Ordering::Less => true,
                Ordering::Equal => upper_inclusive,
                Ordering::Greater => false,
            }
    };
    if spec.layout.is_suffix() {
        // Group order is row order: k-way over `groups`, newest wins.
        let mut cursors: Vec<(
            std::iter::Peekable<std::slice::Iter<'s, (Box<[u8]>, Vec<MeetAccum>)>>,
            usize,
        )> = picked
            .iter()
            .enumerate()
            .map(|(idx, l)| (l.groups.iter().peekable(), idx))
            .collect();
        Box::new(
            std::iter::from_fn(move || {
                let mut best: Option<(&'s [u8], usize)> = None;
                for (cur, idx) in cursors.iter_mut() {
                    if let Some((k, _)) = cur.peek() {
                        let k: &'s [u8] = k.as_ref();
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
                let mut winner: Option<&'s (Box<[u8]>, Vec<MeetAccum>)> = None;
                for (cur, idx) in cursors.iter_mut() {
                    while cur.peek().is_some_and(|(k, _)| k.as_ref() == key) {
                        let g = cur.next().expect("INVARIANT(peeked): peek proved Some");
                        if *idx == win_idx {
                            winner = Some(g);
                        }
                    }
                }
                let (k, v) = winner.expect("INVARIANT(meet_merge_winner): win_idx drained a group");
                Some(TupleInIter::new_meet_suffix(
                    k,
                    v.as_slice(),
                    LimiterSkip::Include,
                ))
            })
            .filter(move |row| within(row)),
        )
    } else {
        // Interleaved: derive head-tuple rows from each picked level's
        // groups; a row speaks only if no newer level owns its group.
        let derived: Vec<Vec<Tuple>> = picked
            .iter()
            .map(|l| {
                let mut rows: Vec<Tuple> = l
                    .groups
                    .iter()
                    .map(|(k, v)| spec.layout.interleave(k, v.as_slice()))
                    .collect();
                rows.sort();
                rows
            })
            .collect();
        // Per-level slice of newer refs so the inner filter_map owns its
        // capture — nested `move` cannot both take `all` (E0507).
        let iters = derived.into_iter().enumerate().map(move |(idx, rows)| {
            let newer: Vec<&'s MeetLevel> = all.iter().skip(idx + 1).copied().collect();
            rows.into_iter().filter_map(move |row| {
                let group = spec.layout.borrow_key(row.as_slice());
                let owned_by_newer = newer.iter().any(|nl| nl.find(&group).is_some());
                if owned_by_newer {
                    None
                } else {
                    Some(TupleInIter::owned(row, LimiterSkip::Include))
                }
            })
        });
        Box::new(
            iters
                .kmerge_by(|a, b| a.cmp(b) == Ordering::Less)
                .filter(move |row| within(row)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::temp_store::RegularTempStore;

    /// A converging fixpoint's level stack stays bounded: epochs that
    /// admit nothing must not accumulate levels (the consumed empty delta
    /// drops when the next seals), and admit-heavy runs stay logarithmic
    /// through compaction.
    #[test]
    fn level_stack_stays_bounded() {
        let mut store = EpochStore::new_normal(1);
        // Ten productive epochs...
        for i in 0..10i64 {
            let mut out = RegularTempStore::default();
            out.put(Tuple::from_vec(vec![DataValue::from(i)]));
            store.merge_in(out.wrap(), &mut ()).unwrap();
        }
        // ...then fifty empty (converged) epochs.
        for _ in 0..50 {
            let out = RegularTempStore::default();
            store.merge_in(out.wrap(), &mut ()).unwrap();
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
        assert_eq!(store.all_iter().count(), 10);
    }
}
