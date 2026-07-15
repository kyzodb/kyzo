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
use miette::{Result, bail};

use crate::data::aggr::{Aggregation, MeetAccum, MeetAggrObj};
use crate::data::value::DataValue;
use crate::data::value::{
    ScanBound, Tuple, bare_bounds_lower, bare_bounds_upper, bare_prefix_len, encode_tuple_bare,
};
use crate::query::temp_store::{
    AdmissionSink, Admitted, MeetAggrStore, MeetLayout, TempStore, TupleInIter, empty_tuple_ref,
};

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
#[derive(Debug, Default)]
pub(crate) struct NormalLevel {
    /// Rows FLATTENED into one dense byte arena: a probe or scan walks
    /// contiguous memory instead of chasing one heap allocation per row.
    /// Rows ascend by memcmp bytes; per row, `skip` is the limiter flag and
    /// `refresh` marks a re-derived row present only to carry a
    /// refreshed flag — shadowing, not admitted, invisible to delta
    /// iteration.
    values: Vec<u8>,
    /// `offsets[i]` is the END of row `i` in `values` (row 0 starts at 0).
    offsets: Vec<u32>,
    flags: Vec<(bool, bool)>,
}

impl NormalLevel {
    pub(crate) fn len(&self) -> usize {
        self.offsets.len()
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }
    pub(crate) fn row(&self, i: usize) -> &[u8] {
        let start = if i == 0 {
            0
        } else {
            self.offsets[i - 1] as usize
        };
        &self.values[start..self.offsets[i] as usize]
    }
    fn row_flags(&self, i: usize) -> (bool, bool) {
        self.flags[i]
    }
    /// Seal an admitted row's bytes into the level — one arena append, no
    /// per-row heap allocation beyond the byte string `RegularTempStore`
    /// already minted at derivation.
    fn push(&mut self, row: Box<[u8]>, skip: bool, refresh: bool) {
        self.values.extend_from_slice(&row);
        self.offsets.push(self.values.len() as u32);
        self.flags.push((skip, refresh));
    }

    /// Copy a row across a compaction merge (the survivor outlives its
    /// source level, so this one copy per compacted row is the cost of
    /// dropping the shadowed copy).
    fn push_from(&mut self, other: &NormalLevel, i: usize, skip: bool, refresh: bool) {
        self.values.extend_from_slice(other.row(i));
        self.offsets.push(self.values.len() as u32);
        self.flags.push((skip, refresh));
    }
    fn find(&self, key_bytes: &[u8]) -> Option<usize> {
        let mut lo = 0usize;
        let mut hi = self.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.row(mid).cmp(key_bytes) {
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
            let stored = self.row(i);
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

    fn bounds(&self, lower: &[u8], upper: &[u8], upper_inclusive: bool) -> (usize, usize) {
        let (lower, upper) = (lower.to_vec(), upper.to_vec());
        let mut lo = 0usize;
        let mut hi = self.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.row(mid) < lower.as_slice() {
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
                self.row(mid) <= upper.as_slice()
            } else {
                self.row(mid) < upper.as_slice()
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
/// key, each holding its WHOLE folded accumulator; a row-order mirror rides
/// along only when the layout interleaves (row order ≠ group order).
#[derive(Debug, Default)]
pub(crate) struct MeetLevel {
    /// Group key bytes (story #77, same [`encode_tuple_bare`] treatment as
    /// `NormalLevel`'s rows) → folded meet accumulators.
    pub(crate) groups: Vec<(Box<[u8]>, Vec<MeetAccum>)>,
    pub(crate) by_row: Vec<Tuple>,
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
    meets: Vec<(Aggregation, Box<dyn MeetAggrObj>)>,
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
        levels: &[MeetLevel],
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
    levels: &'a [MeetLevel],
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
    Normal(Vec<NormalLevel>),
    Meet {
        spec: MeetSpec,
        levels: Vec<MeetLevel>,
    },
}

impl EpochStore {
    pub(crate) fn new_normal(arity: usize) -> Self {
        Self {
            kind: LevelKind::Normal(vec![NormalLevel::default()]),
            arity,
        }
    }
    pub(crate) fn new_meet(aggrs: &[Option<(Aggregation, Vec<DataValue>)>]) -> Result<Self> {
        let probe = MeetAggrStore::new(aggrs.to_vec())?;
        Ok(Self {
            kind: LevelKind::Meet {
                spec: MeetSpec {
                    layout: probe.layout.clone(),
                    meets: probe.meets,
                },
                levels: vec![MeetLevel::default()],
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
                        .find_map(|l| l.find(&row_bytes).map(|i| l.row_flags(i)));
                    match existing {
                        None => {
                            if S::RECORDING {
                                sink.admit(TupleInIter::new_bytes(&row_bytes, skip));
                            }
                            level.push(row_bytes, skip, false);
                            admitted += 1;
                        }
                        Some((old_skip, _)) if old_skip != skip => {
                            // Re-derivation refreshing the limiter flag:
                            // shadows the old row, admitted nowhere.
                            level.push(row_bytes, skip, true);
                        }
                        Some(_) => {}
                    }
                }
                // The previous delta has been consumed; an empty one
                // contributes nothing and drops (or a converging fixpoint
                // would stack one empty level per epoch). Then compact
                // the PRE-epoch stack only: the level just sealed IS the
                // delta and must survive whole until the next barrier.
                drop_consumed_empty_delta(levels, NormalLevel::is_empty);
                compact_normal(levels);
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
                        let row = spec.layout.interleave(&group, vals.as_slice());
                        if S::RECORDING {
                            sink.admit(TupleInIter::new(
                                row.as_slice(),
                                empty_tuple_ref().as_slice(),
                                false,
                            ));
                        }
                        if !spec.layout.is_suffix() {
                            level.by_row.push(row);
                        }
                        level.groups.push((group, vals));
                        admitted += 1;
                    }
                }
                if !spec.layout.is_suffix() {
                    level.by_row.sort();
                }
                drop_consumed_empty_delta(levels, |l| l.groups.is_empty());
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
            LevelKind::Normal(levels) => levels
                .last()
                .is_some_and(|l| (0..l.len()).any(|i| !l.row_flags(i).1)),
            LevelKind::Meet { levels, .. } => levels.last().is_some_and(|l| !l.groups.is_empty()),
        }
    }

    pub(crate) fn range_iter<'s>(
        &'s self,
        lower: &[ScanBound],
        upper: &[ScanBound],
        upper_inclusive: bool,
    ) -> impl Iterator<Item = TupleInIter<'s>> + use<'s> {
        self.ranged(
            bare_bounds_lower(lower),
            bare_bounds_upper(upper),
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
            bare_bounds_lower(lower),
            bare_bounds_upper(upper),
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
        let lower = encode_tuple_bare(prefix.as_slice());
        let mut upper = lower.clone();
        upper.push(0xFF);
        self.ranged(lower, upper, true, false)
    }
    pub(crate) fn delta_prefix_iter<'s>(
        &'s self,
        prefix: &Tuple,
    ) -> impl Iterator<Item = TupleInIter<'s>> + use<'s> {
        let lower = encode_tuple_bare(prefix.as_slice());
        let mut upper = lower.clone();
        upper.push(0xFF);
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
                let picked: &[NormalLevel] = if delta_only {
                    std::slice::from_ref(levels.last().expect("a level always exists"))
                } else {
                    levels.as_slice()
                };
                let mut cursors: Vec<(&'s NormalLevel, usize, usize)> = picked
                    .iter()
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
                let lower = encode_tuple_bare(prefix.as_slice());
                let mut upper = lower.clone();
                upper.push(0xFF);
                let picked: &[MeetLevel] = if delta_only {
                    std::slice::from_ref(levels.last().expect("a level always exists"))
                } else {
                    levels.as_slice()
                };
                let newer: &[MeetLevel] = if delta_only { &[] } else { levels.as_slice() };
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
        lower: Vec<u8>,
        upper: Vec<u8>,
        upper_inclusive: bool,
        delta_only: bool,
    ) -> impl Iterator<Item = TupleInIter<'s>> + use<'s> {
        match &self.kind {
            LevelKind::Normal(levels) => {
                let picked: &[NormalLevel] = if delta_only {
                    std::slice::from_ref(levels.last().expect("a level always exists"))
                } else {
                    levels.as_slice()
                };
                // Index cursors over the flat rows: (level, next, end).
                let mut cursors: Vec<(&'s NormalLevel, usize, usize)> = picked
                    .iter()
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
                let picked: &[MeetLevel] = if delta_only {
                    std::slice::from_ref(levels.last().expect("a level always exists"))
                } else {
                    levels.as_slice()
                };
                let newer: &[MeetLevel] = if delta_only { &[] } else { levels.as_slice() };
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
                let t = l.row(*next);
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
        let key: Vec<u8> = cursors[win_ci].0.row(win_row).to_vec();
        for (l, next, end) in cursors.iter_mut() {
            while *next < *end && l.row(*next) == key.as_slice() {
                *next += 1;
            }
        }
        let l = cursors[win_ci].0;
        let (skip, refresh) = l.row_flags(win_row);
        if delta_only && refresh {
            continue;
        }
        return Some(TupleInIter::new_bytes(l.row(win_row), skip));
    }
}

/// Drop the just-consumed delta level when it is empty — the shared step
/// both barrier arms take before compacting (see the comment at the
/// Normal arm's call site).
fn drop_consumed_empty_delta<L>(levels: &mut Vec<L>, is_empty: impl Fn(&L) -> bool) {
    if levels.len() > 1 && levels.last().is_some_and(is_empty) {
        levels.pop();
    }
}

/// Compact adjacent normal levels while the newer is at least half the
/// older — the classic logarithmic-merging schedule, a pure function of
/// sizes (deterministic on every run). Newest wins per key; shadowed
/// copies drop; a surviving refresh row becomes the row (its flag is the
/// current one) and stops being refresh-marked once its shadowed victim
/// is gone.
fn compact_normal(levels: &mut Vec<NormalLevel>) {
    while levels.len() >= 2 {
        let n = levels.len();
        if levels[n - 1].len() * 2 < levels[n - 2].len() {
            break;
        }
        let newer = levels.pop().expect("len >= 2");
        let older = levels.pop().expect("len >= 2");
        let mut merged = NormalLevel::default();
        let (mut a, mut b) = (0usize, 0usize);
        while a < older.len() || b < newer.len() {
            if a >= older.len() {
                let (skip, refresh) = newer.row_flags(b);
                merged.push_from(&newer, b, skip, refresh);
                b += 1;
            } else if b >= newer.len() {
                let (skip, refresh) = older.row_flags(a);
                merged.push_from(&older, a, skip, refresh);
                a += 1;
            } else {
                match older.row(a).cmp(newer.row(b)) {
                    Ordering::Less => {
                        let (skip, refresh) = older.row_flags(a);
                        merged.push_from(&older, a, skip, refresh);
                        a += 1;
                    }
                    Ordering::Greater => {
                        let (skip, refresh) = newer.row_flags(b);
                        merged.push_from(&newer, b, skip, refresh);
                        b += 1;
                    }
                    Ordering::Equal => {
                        // The shadowed copy is gone; the survivor is a
                        // plain row again whichever mark it carried.
                        let (skip, _) = newer.row_flags(b);
                        merged.push_from(&newer, b, skip, false);
                        a += 1;
                        b += 1;
                    }
                }
            }
        }
        levels.push(merged);
    }
}

/// As [`compact_normal`], for meet levels: newest group value wins whole.
fn compact_meet(spec: &MeetSpec, levels: &mut Vec<MeetLevel>) {
    while levels.len() >= 2 {
        let n = levels.len();
        if levels[n - 1].groups.len() * 2 < levels[n - 2].groups.len() {
            break;
        }
        let newer = levels.pop().expect("len >= 2");
        let older = levels.pop().expect("len >= 2");
        let mut merged = MeetLevel {
            groups: Vec::with_capacity(older.groups.len() + newer.groups.len()),
            by_row: vec![],
        };
        let (mut a, mut b) = (
            older.groups.into_iter().peekable(),
            newer.groups.into_iter().peekable(),
        );
        loop {
            match (a.peek(), b.peek()) {
                (Some((ka, _)), Some((kb, _))) => match ka.cmp(kb) {
                    Ordering::Less => merged.groups.push(a.next().expect("peeked")),
                    Ordering::Greater => merged.groups.push(b.next().expect("peeked")),
                    Ordering::Equal => {
                        let g = b.next().expect("peeked");
                        a.next();
                        merged.groups.push(g);
                    }
                },
                (Some(_), None) => merged.groups.push(a.next().expect("peeked")),
                (None, Some(_)) => merged.groups.push(b.next().expect("peeked")),
                (None, None) => break,
            }
        }
        if !spec.layout.is_suffix() {
            merged.by_row = merged
                .groups
                .iter()
                .map(|(k, v)| spec.layout.interleave(k, v.as_slice()))
                .collect();
            merged.by_row.sort();
        }
        levels.push(merged);
    }
}

/// Row-ordered iteration over meet levels within a bound window, newest
/// group owner winning. Suffix layouts walk `groups` directly (group
/// order IS row order there); interleaved layouts walk the per-level
/// row mirrors and skip rows whose group a newer level owns.
fn meet_ranged<'s>(
    spec: &'s MeetSpec,
    picked: &'s [MeetLevel],
    all: &'s [MeetLevel],
    lower: Vec<u8>,
    upper: Vec<u8>,
    upper_inclusive: bool,
) -> Box<dyn Iterator<Item = TupleInIter<'s>> + 's> {
    let within = move |row: TupleInIter<'_>| -> bool {
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
                        let g = cur.next().expect("peeked");
                        if *idx == win_idx {
                            winner = Some(g);
                        }
                    }
                }
                let (k, v) = winner.expect("winner drained");
                Some(TupleInIter::new_meet_suffix(k, v.as_slice(), false))
            })
            .filter(move |row| within(*row)),
        )
    } else {
        // Interleaved: walk each picked level's row mirror; a row speaks
        // only if no newer level owns its group.
        let iters = picked.iter().enumerate().map(move |(idx, l)| {
            l.by_row.iter().filter_map(move |row| {
                let group = spec.layout.borrow_key(row.as_slice());
                let owned_by_newer = all.iter().skip(idx + 1).any(|nl| nl.find(&group).is_some());
                if owned_by_newer {
                    None
                } else {
                    Some(TupleInIter::new(
                        row.as_slice(),
                        empty_tuple_ref().as_slice(),
                        false,
                    ))
                }
            })
        });
        Box::new(
            iters
                .kmerge_by(|a, b| a.into_iter().lt(*b))
                .filter(move |row| within(*row)),
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
