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
 * `MeetLayout` proof), not only when they form a prefix as upstream cozo
 * required — full oracle positional-meet parity (`query/laws.rs`), which
 * retires the `MeetNotSuffix` refusal removed from `query/eval.rs`. It
 * keeps two views — `by_group` for the fold/delta (group-key order) and
 * `by_row` for the head-tuple-ordered scans joins issue — because for a
 * non-suffix layout those orders differ. The delta propagation in
 * `MeetAggrStore`
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
//! - [`RegularTempStore`]: a set of tuples (plus a per-tuple limiter-skip
//!   flag for early-returned entry rules).
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
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Debug, Formatter};

use itertools::Itertools;
use miette::{Result, miette};

use crate::data::aggr::{Aggregation, MeetAggrObj};
use crate::data::value::DataValue;
use crate::data::value::{ScanBound, Tuple, decode_tuple_bare, encode_tuple_bare};

// ─────────────────────────────────────────────────────────────────────────
// The admission seam
// ─────────────────────────────────────────────────────────────────────────

/// The number of derivations admitted to a store's `total` by one
/// `merge_in` — the budget design's unit of account for the
/// `derived_tuple_ceiling`. Deterministic: it is a function of the sets
/// being merged, not of any schedule, so summing it per epoch and checking
/// at the epoch barrier refuses identically on every run.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub(crate) struct Admitted(pub(crate) usize);

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
    fn admit(&mut self, tuple: TupleInIter<'_>);
}

/// Recording off: the default state, compiled away.
impl AdmissionSink for () {
    const RECORDING: bool = false;
    #[inline(always)]
    fn admit(&mut self, _tuple: TupleInIter<'_>) {}
}

// ─────────────────────────────────────────────────────────────────────────
// RegularTempStore
// ─────────────────────────────────────────────────────────────────────────

/// A store holding temp data during evaluation of queries: a set of tuples,
/// each with a limiter-skip flag (`true` = the tuple was derived past an
/// entry rule's `:limit` and participates in joins but not in the returned
/// rows; see [`EpochStore::early_returned_iter`]).
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
#[derive(Default, Debug)]
pub struct RegularTempStore {
    pub(crate) inner: BTreeMap<Box<[u8]>, bool>,
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
        self.inner.contains_key(encode_tuple_bare(key).as_slice())
    }

    /// The number of distinct tuples materialized so far. On the plain and
    /// normal-aggregation paths the out-store holds only genuinely-new,
    /// deduped tuples (the caller filters each derivation against the running
    /// total before putting), so this is *both* the resident memory and the
    /// count the barrier will admit — the budget's mid-epoch spend guard
    /// reads it directly as the rule's in-flight admission count (see
    /// `query::eval::InterruptTicker`). Contrast
    /// [`MeetAggrStore::len`](MeetAggrStore::len), whose fresh out-store also
    /// holds re-derived unchanged groups, so it is resident memory but NOT an
    /// admission count.
    pub(crate) fn len(&self) -> usize {
        self.inner.len()
    }

    // Edition-2024 note: the `use<'s>` capture lists on the iterator
    // returns are load-bearing — the bounds are copied into owned values,
    // so the returned iterator borrows the store only, not the bound
    // arguments (the original relied on edition-2021's default).

    /// Add a tuple to the store.
    pub fn put(&mut self, tuple: Tuple) {
        self.inner
            .insert(encode_tuple_bare(&tuple).into_boxed_slice(), false);
    }
    pub(crate) fn put_with_skip(&mut self, tuple: Tuple) {
        self.inner
            .insert(encode_tuple_bare(&tuple).into_boxed_slice(), true);
    }
}

// ─────────────────────────────────────────────────────────────────────────
// MeetAggrStore
// ─────────────────────────────────────────────────────────────────────────

/// The positional layout of a meet-aggregation head, resolved once at
/// construction from the head's per-position aggregation signature. It is
/// the constructed proof that carries *where* the grouping keys and the
/// meet values sit, so the projection/interleave arithmetic lives in
/// exactly one place instead of scattered `split_at`/`grouping_len` call
/// sites.
///
/// `key_positions` are the head positions with no aggregation (the grouping
/// key), `val_positions` the meet-aggregated positions — both in ascending
/// head order, and together a partition of `0..arity`. Upstream cozo (and
/// the store this replaces) required the aggregated positions to form a
/// *suffix* so the group key was a byte prefix of the encoded tuple; this
/// layout groups by the projection onto `key_positions` **wherever they
/// sit** — position 0, interleaved, or split across the head — matching the
/// oracle's full positional meet semantics (see the divergence note in
/// `query/laws.rs`).
#[derive(Debug, Clone)]
pub(crate) struct MeetLayout {
    key_positions: Vec<usize>,
    val_positions: Vec<usize>,
    arity: usize,
}

impl MeetLayout {
    /// Build from the head signature: `None` positions group, `Some`
    /// positions aggregate. Total over any signature — the partition of
    /// `0..arity` is exhaustive, so `interleave` never leaves a `Null`.
    fn from_signature(aggrs: &[Option<(Aggregation, Vec<DataValue>)>]) -> Self {
        let arity = aggrs.len();
        let mut key_positions = Vec::new();
        let mut val_positions = Vec::new();
        for (i, a) in aggrs.iter().enumerate() {
            if a.is_none() {
                key_positions.push(i);
            } else {
                val_positions.push(i);
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
        self.key_positions.iter().map(|i| row[*i].clone()).collect()
    }

    /// The meet values of a head tuple: its projection onto the aggregated
    /// positions, in head order (aligned one-to-one with `meets`).
    fn project_vals(&self, row: &[DataValue]) -> Tuple {
        self.val_positions.iter().map(|i| row[*i].clone()).collect()
    }

    /// Whether the aggregated positions form a *suffix* — equivalently, the
    /// grouping positions are exactly the prefix `0..key_positions.len()`.
    /// When true, a head tuple is byte-for-byte `group_key ++ folded_vals`,
    /// so the group map alone reconstructs every row in head-tuple order (a
    /// distinct group has a distinct key, and the key is the head prefix)
    /// and the `by_row` mirror is redundant. This is the pre-fork store's
    /// layout — the only shape that existed before positional grouping — so
    /// a suffix store skips the mirror entirely and keeps that footprint;
    /// only a genuinely non-suffix layout pays for `by_row`.
    pub(crate) fn is_suffix(&self) -> bool {
        self.key_positions
            .iter()
            .copied()
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
    pub(crate) fn borrow_key(&self, row: &[DataValue]) -> Vec<u8> {
        if self.is_suffix() {
            encode_tuple_bare(&row[..self.key_positions.len()])
        } else {
            encode_tuple_bare(&self.project_key(row))
        }
    }

    /// Rebuild the logical head tuple from a group key's bytes and its
    /// folded meet values — the inverse of the two projections (the key
    /// decodes back through [`decode_tuple_bare`]; the fold values were
    /// never bytes, per this module's doc). Every position is either a key
    /// or a value position (they partition `0..arity`), so no `Null`
    /// placeholder survives.
    pub(crate) fn interleave(&self, key: &[u8], vals: &[DataValue]) -> Tuple {
        let key = decode_tuple_bare(key).expect("this store's own bytes decode");
        let mut row: Tuple = vec![DataValue::Null; self.arity];
        for (slot, i) in self.key_positions.iter().enumerate() {
            row[*i] = key[slot].clone();
        }
        for (slot, i) in self.val_positions.iter().enumerate() {
            row[*i] = vals[slot].clone();
        }
        row
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
    /// argument as [`RegularTempStore`]) → folded meet values (projection
    /// onto the aggregated positions, in head order). The fold/delta
    /// authority, iterated in canonical group-key order at the merge
    /// barrier so admissions stay schedule-independent. The VALUES stay
    /// `DataValue`-typed — not because every meet kind NEEDS typed
    /// computation: byte-backing wins where a value is only ever COMPARED
    /// (the key, and — since memcomparable order embeds value order — the
    /// order-based `min`/`max` folds too), and loses nothing where it is.
    /// `set union/intersection`, `bitand/bitor`, and tropical `min-cost`
    /// genuinely need decode to compute (no byte-level union or bitwise
    /// op exists), so SOME meet kinds have no byte path regardless. Given
    /// that, one typed value representation serving every kind uniformly
    /// beats a bytes-for-min/max-only special case for a fold that is not
    /// this store's hot path (the hot path is the key comparison, already
    /// byte-native) — a marginal win traded for less code, not a wall.
    pub(crate) by_group: BTreeMap<Box<[u8]>, Tuple>,
    /// The current logical head tuples — each the [`MeetLayout::interleave`]
    /// of a group key with its folded values — kept in canonical head-tuple
    /// order for the range/prefix scans joins issue (`query/ra.rs`).
    ///
    /// Materialized **only for a non-suffix layout**, where head-tuple order
    /// and group-key order genuinely differ. For that case it is a pure
    /// mirror of `by_group`: every mutation updates both, so it is always
    /// exactly `{ interleave(k, v) : (k, v) ∈ by_group }`. For a suffix
    /// layout it stays empty — the group key is a head prefix, so `by_group`
    /// alone scans in head-tuple order (see [`MeetLayout::is_suffix`]) and
    /// the mirror would only duplicate the fold authority's memory.
    by_row: BTreeSet<Tuple>,
    /// The meet operations, one per aggregated head position, in head
    /// order, resolved at construction. (The original stored `Option`s and
    /// unwrapped per row.)
    pub(crate) meets: Vec<(Aggregation, Box<dyn MeetAggrObj>)>,
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
    pub(crate) fn len(&self) -> usize {
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
        self.by_group.contains_key(group_key.as_slice())
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
        total: &crate::query::levels::MeetTotalView<'_>,
    ) -> Result<bool> {
        // The group key is encoded once and reused for both probes.
        let group_key = self.layout.borrow_key(tuple);
        // Admissibility BEFORE this fold: a group absent from the out-store
        // contributes nothing to the barrier yet, so it is not admissible.
        let was_admissible = match self.by_group.get(group_key.as_slice()) {
            Some(vals) => total.would_admit(&group_key, vals)?,
            None => false,
        };
        self.meet_put(tuple)?;
        // After folding, the group is certainly resident in the out-store.
        let now_vals = self
            .by_group
            .get(group_key.as_slice())
            .expect("meet_put inserts or updates the group");
        let now_admissible = total.would_admit(&group_key, now_vals)?;
        Ok(now_admissible && !was_admissible)
    }
    /// Build a meet store from a rule head's aggregation spec: one entry
    /// per head position, `None` for grouping positions, `Some` for
    /// aggregated ones. Compilation only routes rules whose aggregations
    /// are all meets here (`Aggregation::is_meet`); a normal-only
    /// aggregation in the spec is an engine bug and is refused with an
    /// error rather than unwrapped. The argument lists are part of eval's
    /// aggregation-spec shape but meet forms take no arguments (see
    /// `data/aggr.rs`), so they are ignored.
    pub(crate) fn new(aggrs: Vec<Option<(Aggregation, Vec<DataValue>)>>) -> Result<Self> {
        let layout = MeetLayout::from_signature(&aggrs);
        let mut meets = Vec::new();
        for (aggr, _args) in aggrs.into_iter().flatten() {
            let op = aggr.meet_op().ok_or_else(|| {
                miette!(
                    "internal invariant violated: normal-only aggregation '{}' \
                     routed to a meet store",
                    aggr.name
                )
            })?;
            meets.push((aggr, op));
        }
        Ok(Self {
            by_group: Default::default(),
            by_row: Default::default(),
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
        let materialize = !self.layout.is_suffix();
        // The grouping projection, encoded once (story #77: `borrow_key` is
        // always an encode now, never a zero-alloc slice borrow — see its
        // doc), and fold incoming values straight from `tuple` by position
        // so no `incoming` tuple is built either way.
        let key = self.layout.borrow_key(tuple);
        match self.by_group.get_mut(key.as_slice()) {
            Some(vals) => {
                // Snapshot the pre-fold values only when the row mirror
                // needs the old row to retract (F2b): a no-change put never
                // materializes the interleaved clone.
                let old_vals = materialize.then(|| vals.clone());
                let mut changed = false;
                for (i, (_aggr, op)) in self.meets.iter().enumerate() {
                    changed |= op.update(&mut vals[i], &tuple[self.layout.val_positions[i]])?;
                }
                if changed && materialize {
                    let old_vals = old_vals.expect("materialize implies a snapshot");
                    let old_row = self.layout.interleave(&key, &old_vals);
                    let new_row = self.layout.interleave(&key, vals);
                    self.by_row.remove(&old_row);
                    self.by_row.insert(new_row);
                }
                Ok(changed)
            }
            None => {
                let vals = self.layout.project_vals(tuple);
                if materialize {
                    self.by_row.insert(self.layout.interleave(&key, &vals));
                }
                self.by_group.insert(key.into_boxed_slice(), vals);
                Ok(true)
            }
        }
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
/// both. Meet's FOLDED VALUES stay `DataValue`-typed uniformly across
/// every meet kind — see [`MeetAggrStore`]'s doc for the precise
/// reasoning (byte-backing wins on comparison, not computation; `min`/
/// `max` could go either way since memcomparable order embeds value
/// order, but one typed representation for every kind beats a
/// bytes-for-order-only special case on a fold that isn't this store's
/// hot path). One enum keeps every consumer (`AdmissionSink`, the RA join
/// probes, the provenance/trials iteration surfaces) working over ONE
/// type regardless of which
/// store produced the row and which layout (suffix or interleaved) it
/// used. The consequence: `get`/`into_tuple`/iteration now return OWNED
/// `DataValue`s, never `&'a DataValue` — a byte-backed key has nothing to
/// reference; decoding produces a value, not a borrow. Checked against
/// every non-test consumer in the tree: all but one already only used
/// `.into_tuple()` (whose signature is unchanged), and the one exception
/// (`query/ra/temp.rs`'s filter-less join arm) already had a
/// materialize-first fallback one branch away for the filtered case.
#[derive(Copy, Clone, Debug)]
pub(crate) enum TupleInIter<'a> {
    /// A regular store's row: memcmp bytes (chunk 1's bare codec), whole
    /// row in `key` — a regular row has no separate value region.
    Bytes { key: &'a [u8], skip: bool },
    /// A meet store's SUFFIX-layout row: group-key bytes (the row's own
    /// head prefix — no interleave needed) + folded meet values, typed.
    MeetSuffix {
        key: &'a [u8],
        val: &'a [DataValue],
        skip: bool,
    },
    /// A meet store's INTERLEAVED-layout row: [`MeetLayout::interleave`]
    /// has already rebuilt the full logical row as `DataValue`s (the key
    /// projection may sit anywhere in head order, so there is no prefix
    /// shortcut), matching the pre-byte-conversion shape exactly.
    Values {
        key: &'a [DataValue],
        val: &'a [DataValue],
        skip: bool,
    },
}

impl<'a> TupleInIter<'a> {
    /// Construct a view over an already-interleaved (key-part, value-part,
    /// skip) triple — the non-suffix meet path, where `key`/`val` are
    /// `DataValue` projections of a fully rebuilt logical row.
    pub(crate) fn new(key: &'a [DataValue], val: &'a [DataValue], skip: bool) -> Self {
        TupleInIter::Values { key, val, skip }
    }
    /// Construct a view over one regular store's row bytes.
    pub(crate) fn new_bytes(key: &'a [u8], skip: bool) -> Self {
        TupleInIter::Bytes { key, skip }
    }
    /// Construct a view over a suffix-layout meet store's group-key bytes
    /// plus its typed folded values.
    pub(crate) fn new_meet_suffix(key: &'a [u8], val: &'a [DataValue], skip: bool) -> Self {
        TupleInIter::MeetSuffix { key, val, skip }
    }
}

impl TupleInIter<'_> {
    /// Structural guarantee: callers index by positions of the rule head
    /// this store was built for (`idx < EpochStore::arity`), which is
    /// compiled knowledge, not data — the `expect`s cannot fire on user
    /// data.
    pub(crate) fn get(self, idx: usize) -> DataValue {
        match self {
            TupleInIter::Bytes { key, .. } => {
                bare_nth(key, idx).expect("compiled position within this row's arity")
            }
            TupleInIter::MeetSuffix { key, val, .. } => {
                let key = decode_tuple_bare(key).expect("this store's own bytes decode");
                key.get(idx)
                    .cloned()
                    .or_else(|| val.get(idx - key.len()).cloned())
                    .expect("compiled position within this row's arity")
            }
            TupleInIter::Values { key, val, .. } => key
                .get(idx)
                .or_else(|| val.get(idx - key.len()))
                .cloned()
                .expect("compiled position within this row's arity"),
        }
    }
    pub(crate) fn should_skip(&self) -> bool {
        match self {
            TupleInIter::Bytes { skip, .. }
            | TupleInIter::MeetSuffix { skip, .. }
            | TupleInIter::Values { skip, .. } => *skip,
        }
    }
    pub(crate) fn into_tuple(self) -> Tuple {
        match self {
            TupleInIter::Bytes { key, .. } => {
                decode_tuple_bare(key).expect("this store's own bytes decode")
            }
            TupleInIter::MeetSuffix { key, val, .. } => {
                let mut key = decode_tuple_bare(key).expect("this store's own bytes decode");
                key.extend(val.iter().cloned());
                key
            }
            TupleInIter::Values { key, val, .. } => key.iter().chain(val.iter()).cloned().collect(),
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
                for v in val.iter() {
                    crate::data::value::append_canonical(&mut full, v);
                }
                full.as_slice().cmp(probe)
            }
            TupleInIter::Values { key, val, .. } => {
                let mut full = Vec::new();
                for v in key.iter().chain(val.iter()) {
                    crate::data::value::append_canonical(&mut full, v);
                }
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
                let key = decode_tuple_bare(key).expect("this store's own bytes decode");
                key.iter().chain(val.iter()).cmp(other.iter())
            }
            TupleInIter::Values { key, val, .. } => key.iter().chain(val.iter()).cmp(other.iter()),
        }
    }
}

/// The `idx`-th self-delimiting value in `bytes`, walking from the start
/// (bare-encoded rows carry no per-column offset table: one contiguous,
/// self-delimiting region is the whole point). `None` if fewer than
/// `idx + 1` values are present.
fn bare_nth(bytes: &[u8], idx: usize) -> Option<DataValue> {
    let mut remaining = bytes;
    for _ in 0..idx {
        let (_, next) = DataValue::decode_from_key(remaining).ok()?;
        remaining = next;
    }
    let (val, _) = DataValue::decode_from_key(remaining).ok()?;
    Some(val)
}

impl<'a> IntoIterator for TupleInIter<'a> {
    type Item = DataValue;
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
        val: &'a [DataValue],
        val_idx: usize,
    },
    Values {
        key: &'a [DataValue],
        val: &'a [DataValue],
        idx: usize,
    },
}

pub(crate) struct TupleInIterIterator<'a> {
    state: TupleInIterState<'a>,
}

impl PartialEq for TupleInIter<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.into_iter().eq(*other)
    }
}

impl Eq for TupleInIter<'_> {}

impl Ord for TupleInIter<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.into_iter().cmp(*other)
    }
}

impl PartialOrd for TupleInIter<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq<[DataValue]> for TupleInIter<'_> {
    fn eq(&self, other: &'_ [DataValue]) -> bool {
        self.into_iter().eq(other.iter().cloned())
    }
}

impl PartialOrd<[DataValue]> for TupleInIter<'_> {
    fn partial_cmp(&self, other: &'_ [DataValue]) -> Option<Ordering> {
        Some(self.cmp_slice(other))
    }
}

impl Iterator for TupleInIterIterator<'_> {
    type Item = DataValue;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.state {
            TupleInIterState::Bytes(remaining) => {
                if remaining.is_empty() {
                    return None;
                }
                let (val, next) =
                    DataValue::decode_from_key(remaining).expect("this store's own bytes decode");
                *remaining = next;
                Some(val)
            }
            TupleInIterState::MeetSuffix {
                key_remaining,
                val,
                val_idx,
            } => {
                if !key_remaining.is_empty() {
                    let (v, next) = DataValue::decode_from_key(key_remaining)
                        .expect("this store's own bytes decode");
                    *key_remaining = next;
                    return Some(v);
                }
                let ret = val.get(*val_idx)?.clone();
                *val_idx += 1;
                Some(ret)
            }
            TupleInIterState::Values { key, val, idx } => {
                let ret = match key.get(*idx) {
                    Some(d) => d.clone(),
                    None => val.get(*idx - key.len())?.clone(),
                };
                *idx += 1;
                Some(ret)
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Tests (new in KyzoDB; the original file had none)
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::aggr::parse_aggr;
    use crate::query::levels::{EpochStore, LevelKind};

    fn t(vals: &[i64]) -> Tuple {
        vals.iter().map(|v| DataValue::from(*v)).collect()
    }

    fn gv(group: &str, val: DataValue) -> Tuple {
        vec![DataValue::from(group), val]
    }

    /// A recording sink: collects every admission, in the order reported.
    #[derive(Default)]
    struct Recorder(Vec<Tuple>);

    impl AdmissionSink for Recorder {
        const RECORDING: bool = true;
        fn admit(&mut self, tuple: TupleInIter<'_>) {
            self.0.push(tuple.into_tuple());
        }
    }

    fn all(store: &EpochStore) -> Vec<Tuple> {
        store.all_iter().map(TupleInIter::into_tuple).collect_vec()
    }

    fn delta(store: &EpochStore) -> Vec<Tuple> {
        store
            .delta_all_iter()
            .map(TupleInIter::into_tuple)
            .collect_vec()
    }

    // ── total/delta discipline ───────────────────────────────────────────

    /// The semi-naive discipline on a regular store: a first epoch's tuples
    /// are all delta; re-derivation produces an empty delta (fixpoint);
    /// a genuinely new tuple produces exactly itself as the delta.
    #[test]
    fn regular_total_delta_discipline() {
        let mut store = EpochStore::new_normal(2);
        assert_eq!(store.arity, 2);
        assert!(!store.has_delta());

        // Epoch 0: two fresh tuples — the fast path swaps the whole
        // out-store into the empty total, and total doubles as delta.
        let mut out0 = RegularTempStore::default();
        out0.put(t(&[1, 1]));
        out0.put(t(&[1, 2]));
        let admitted = store.merge_in(out0.wrap(), &mut ()).unwrap();
        assert_eq!(admitted, Admitted(2));
        assert!(store.has_delta());
        assert_eq!(all(&store), vec![t(&[1, 1]), t(&[1, 2])]);
        assert_eq!(delta(&store), all(&store)); // first epoch: delta == total

        // Epoch 1: pure re-derivation — empty delta, nothing admitted.
        // This is the termination certificate: eval stops here.
        let mut out1 = RegularTempStore::default();
        out1.put(t(&[1, 1]));
        let admitted = store.merge_in(out1.wrap(), &mut ()).unwrap();
        assert_eq!(admitted, Admitted(0));
        assert!(!store.has_delta());
        assert!(delta(&store).is_empty());
        assert_eq!(all(&store).len(), 2); // total unharmed

        // Epoch 2: one re-derived + one genuinely new — the delta is
        // exactly the new tuple, never the re-derived one.
        let mut out2 = RegularTempStore::default();
        out2.put(t(&[1, 1]));
        out2.put(t(&[2, 3]));
        let admitted = store.merge_in(out2.wrap(), &mut ()).unwrap();
        assert_eq!(admitted, Admitted(1));
        assert!(store.has_delta());
        assert_eq!(delta(&store), vec![t(&[2, 3])]);
        assert_eq!(all(&store), vec![t(&[1, 1]), t(&[1, 2]), t(&[2, 3])]);
    }

    /// An empty epoch is a fixpoint signal even against a non-empty total.
    #[test]
    fn empty_epoch_is_fixpoint() {
        let mut store = EpochStore::new_normal(1);
        let mut out = RegularTempStore::default();
        out.put(t(&[7]));
        store.merge_in(out.wrap(), &mut ()).unwrap();
        assert!(store.has_delta());

        store
            .merge_in(RegularTempStore::default().wrap(), &mut ())
            .unwrap();
        assert!(!store.has_delta());
        assert_eq!(all(&store), vec![t(&[7])]); // total survives
    }

    /// The admission sink observes every admission, in canonical key
    /// order, on both the swap fast path and the incremental path — the
    /// deterministic sequence provenance witnesses will bind to.
    #[test]
    fn admission_sink_sees_admissions_in_canonical_order() {
        let mut store = EpochStore::new_normal(1);

        // Swap path: puts arrive out of order, admissions are reported in
        // key order because the store is a BTreeMap.
        let mut out0 = RegularTempStore::default();
        out0.put(t(&[3]));
        out0.put(t(&[1]));
        let mut rec = Recorder::default();
        let admitted = store.merge_in(out0.wrap(), &mut rec).unwrap();
        assert_eq!(admitted, Admitted(2));
        assert_eq!(rec.0, vec![t(&[1]), t(&[3])]);

        // Incremental path: only the genuinely new tuple is admitted.
        let mut out1 = RegularTempStore::default();
        out1.put(t(&[3])); // re-derived
        out1.put(t(&[2])); // new
        let mut rec = Recorder::default();
        let admitted = store.merge_in(out1.wrap(), &mut rec).unwrap();
        assert_eq!(admitted, Admitted(1));
        assert_eq!(rec.0, vec![t(&[2])]);
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
    fn meet_delta_regression_changed_flag_drives_delta() {
        let or_aggr = parse_aggr("or").unwrap();
        let spec = vec![None, Some((or_aggr, vec![]))];
        let mut store = EpochStore::new_meet(&spec).unwrap();

        // Epoch 0: g starts false.
        let mut out0 = MeetAggrStore::new(spec.clone()).unwrap();
        out0.meet_put(&gv("g", DataValue::from(false))).unwrap();
        let admitted = store.merge_in(out0.wrap(), &mut ()).unwrap();
        assert_eq!(admitted, Admitted(1));
        assert!(store.has_delta());
        assert_eq!(all(&store), vec![gv("g", DataValue::from(false))]);

        // Epoch 1: the epoch derives (g, true) — the value changes.
        // Old inverted flag: empty delta here (the bug). Landed contract:
        // the changed group, with its UPDATED value, is the delta.
        let mut out1 = MeetAggrStore::new(spec.clone()).unwrap();
        out1.meet_put(&gv("g", DataValue::from(true))).unwrap();
        let mut rec = Recorder::default();
        let admitted = store.merge_in(out1.wrap(), &mut rec).unwrap();
        assert_eq!(admitted, Admitted(1));
        assert!(
            store.has_delta(),
            "changed meet value must produce a delta; empty means premature fixpoint"
        );
        assert_eq!(delta(&store), vec![gv("g", DataValue::from(true))]);
        assert_eq!(all(&store), vec![gv("g", DataValue::from(true))]);
        // The admission (where a provenance witness would bind) carries the
        // updated value too.
        assert_eq!(rec.0, vec![gv("g", DataValue::from(true))]);

        // Epoch 2: re-deriving (g, false) leaves true | false = true
        // unchanged — genuinely no delta, the fixpoint. (The old inverted
        // flag would have reported "changed" here: spurious epochs, the
        // benign direction of the same bug.)
        let mut out2 = MeetAggrStore::new(spec).unwrap();
        out2.meet_put(&gv("g", DataValue::from(false))).unwrap();
        let admitted = store.merge_in(out2.wrap(), &mut ()).unwrap();
        assert_eq!(admitted, Admitted(0));
        assert!(!store.has_delta());
        assert_eq!(all(&store), vec![gv("g", DataValue::from(true))]);
    }

    /// The same contract at the `meet_put` level: a fold that moves the
    /// value reports `true`, a fold that doesn't reports `false` —
    /// exercised for both a lattice where the bug lived (`or`) and an
    /// always-correct one (`min`).
    #[test]
    fn meet_put_changed_flag() {
        let or_aggr = parse_aggr("or").unwrap();
        let spec = vec![None, Some((or_aggr, vec![]))];
        let mut store = MeetAggrStore::new(spec).unwrap();
        assert!(store.is_empty());
        // New group: changed.
        assert!(store.meet_put(&gv("g", DataValue::from(false))).unwrap());
        // false | true = true: CHANGED (the old inverted flag said false).
        assert!(store.meet_put(&gv("g", DataValue::from(true))).unwrap());
        // true | true = true: unchanged (the old flag said changed).
        assert!(!store.meet_put(&gv("g", DataValue::from(true))).unwrap());
        assert!(!store.is_empty());

        let min_aggr = parse_aggr("min").unwrap();
        let spec = vec![None, Some((min_aggr, vec![]))];
        let mut store = MeetAggrStore::new(spec).unwrap();
        assert!(store.meet_put(&gv("g", DataValue::from(5i64))).unwrap());
        assert!(!store.meet_put(&gv("g", DataValue::from(7i64))).unwrap()); // 5 stays
        assert!(store.meet_put(&gv("g", DataValue::from(3i64))).unwrap()); // 3 wins
        assert!(store.exists(&gv("g", DataValue::from(999i64)))); // group-key lookup
    }

    /// A meet store refuses a normal-only aggregation at construction —
    /// a typed error where the original unwrapped an `Option` per row.
    #[test]
    fn meet_store_rejects_normal_aggregation() {
        let count = parse_aggr("count").unwrap();
        assert!(!count.is_meet());
        let res = MeetAggrStore::new(vec![None, Some((count, vec![]))]);
        assert!(res.is_err());
    }

    // ── iteration surface ────────────────────────────────────────────────

    /// Meet stores fold per group but iterate whole head tuples (the
    /// `by_row` view); prefix iteration and indexed access see one logical
    /// tuple. For this suffix layout the group key is a prefix, so scans
    /// look like a regular store — the non-suffix cases below prove the
    /// same surface holds when the group key is not a prefix.
    #[test]
    fn meet_iteration_spans_key_and_value() {
        let min_aggr = parse_aggr("min").unwrap();
        let spec = vec![None, Some((min_aggr, vec![]))];
        let mut store = EpochStore::new_meet(&spec).unwrap();
        assert_eq!(store.arity, 2);

        let mut out = MeetAggrStore::new(spec).unwrap();
        out.meet_put(&gv("a", DataValue::from(4i64))).unwrap();
        out.meet_put(&gv("b", DataValue::from(2i64))).unwrap();
        store.merge_in(out.wrap(), &mut ()).unwrap();

        assert!(store.exists(&gv("a", DataValue::from(0i64))));
        assert!(!store.exists(&gv("c", DataValue::from(0i64))));

        let got = store
            .prefix_iter(&vec![DataValue::from("b")])
            .map(TupleInIter::into_tuple)
            .collect_vec();
        assert_eq!(got, vec![gv("b", DataValue::from(2i64))]);

        // Indexed access crosses the key/value seam.
        let row = store.all_iter().next().unwrap();
        assert_eq!(row.get(0), DataValue::from("a"));
        assert_eq!(row.get(1), DataValue::from(4i64));

        // Bounded range scans over the whole head tuple in `by_row`.
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
            .map(TupleInIter::into_tuple)
            .collect_vec();
        assert_eq!(got, vec![gv("b", DataValue::from(2i64))]);
        let got = store
            .range_iter(&lower, &upper, false)
            .map(TupleInIter::into_tuple)
            .collect_vec();
        assert!(got.is_empty());
    }

    /// Limiter-skipped tuples participate in joins (all_iter) but not in
    /// the early-returned rows.
    #[test]
    fn skip_flags_gate_early_return_only() {
        let mut store = EpochStore::new_normal(1);
        let mut out = RegularTempStore::default();
        out.put(t(&[1]));
        out.put_with_skip(t(&[2]));
        assert!(out.exists(&t(&[2])));
        store.merge_in(out.wrap(), &mut ()).unwrap();

        assert!(store.exists(&t(&[2])));
        assert_eq!(all(&store).len(), 2);
        let returned = store
            .early_returned_iter()
            .map(TupleInIter::into_tuple)
            .collect_vec();
        assert_eq!(returned, vec![t(&[1])]);

        // Delta iteration honors the same store (first epoch: via total).
        let d = store
            .delta_prefix_iter(&vec![DataValue::from(2i64)])
            .map(TupleInIter::into_tuple)
            .collect_vec();
        assert_eq!(d, vec![t(&[2])]);
    }

    /// Mismatched store kinds at a merge are an error, not an abort.
    #[test]
    fn kind_mismatch_is_error_not_panic() {
        let mut store = EpochStore::new_normal(1);
        let min_aggr = parse_aggr("min").unwrap();
        let meet_out = MeetAggrStore::new(vec![Some((min_aggr, vec![]))]).unwrap();
        assert!(store.merge_in(meet_out.wrap(), &mut ()).is_err());
    }

    // ── non-suffix meet layouts: positional grouping ─────────────────────

    /// A head tuple with the meet value at position 0 and the grouping key
    /// at position 1 — the layout the suffix-prefix store could not hold.
    fn vg(val: DataValue, group: &str) -> Tuple {
        vec![val, DataValue::from(group)]
    }

    /// [`MeetLayout`] projections and their inverse round-trip for an
    /// interleaved layout (a grouping column between two meet columns): the
    /// key and value projections partition the row and `interleave` rebuilds
    /// it exactly, leaving no `Null`. This is the layout proof the whole
    /// positional grouping rests on — the mutation target.
    #[test]
    fn meet_layout_projection_round_trips_interleaved() {
        let min_aggr = parse_aggr("min").unwrap();
        let max_aggr = parse_aggr("max").unwrap();
        let spec = vec![Some((min_aggr, vec![])), None, Some((max_aggr, vec![]))];
        let layout = MeetLayout::from_signature(&spec);
        assert_eq!(layout.key_positions, vec![1]);
        assert_eq!(layout.val_positions, vec![0, 2]);

        let row: Tuple = vec![
            DataValue::from(2i64),
            DataValue::from("g"),
            DataValue::from(9i64),
        ];
        let key = layout.project_key(&row);
        let vals = layout.project_vals(&row);
        assert_eq!(key, Tuple::from(vec![DataValue::from("g")]));
        assert_eq!(
            vals,
            Tuple::from(vec![DataValue::from(2i64), DataValue::from(9i64)])
        );
        assert_eq!(
            layout.interleave(&encode_tuple_bare(&key), &vals),
            row,
            "interleave is the exact inverse of the two projections"
        );
    }

    /// Meet at position 0: the fold groups on position 1, values fold at
    /// position 0, `exists` is group membership projected to position 1, and
    /// the scan surface iterates whole head tuples in canonical (value-first)
    /// order.
    #[test]
    fn meet_put_and_scan_non_suffix() {
        let min_aggr = parse_aggr("min").unwrap();
        let spec = vec![Some((min_aggr, vec![])), None];
        let mut out = MeetAggrStore::new(spec.clone()).unwrap();
        // group "a": min(4, 2, 9) = 2 ; group "b": 5.
        assert!(out.meet_put(&vg(DataValue::from(4i64), "a")).unwrap());
        assert!(out.meet_put(&vg(DataValue::from(2i64), "a")).unwrap()); // 2 < 4
        assert!(!out.meet_put(&vg(DataValue::from(9i64), "a")).unwrap()); // 2 stays
        assert!(out.meet_put(&vg(DataValue::from(5i64), "b")).unwrap());
        // `exists` is group membership, projected to position 1 (the value
        // in the probe is irrelevant).
        assert!(out.exists(&vg(DataValue::from(999i64), "a")));
        assert!(!out.exists(&vg(DataValue::from(0i64), "c")));

        let mut store = EpochStore::new_meet(&spec).unwrap();
        store.merge_in(out.wrap(), &mut ()).unwrap();
        // by_row (head-tuple) order: (2,"a") < (5,"b") on position 0.
        assert_eq!(
            all(&store),
            vec![
                vg(DataValue::from(2i64), "a"),
                vg(DataValue::from(5i64), "b")
            ]
        );
        // Indexed access spans the whole logical tuple.
        let row = store.all_iter().next().unwrap();
        assert_eq!(row.get(0), DataValue::from(2i64));
        assert_eq!(row.get(1), DataValue::from("a"));
    }

    /// The determinism-critical seam: for a non-suffix layout the group-key
    /// order and the head-tuple order genuinely differ, and admissions are
    /// reported in **group-key** order (the `by_group` iteration the merge
    /// barrier and provenance witnesses depend on), while the scan surface
    /// (`by_row`) stays in head-tuple order. A store that admitted in row
    /// order would reorder every witness table.
    #[test]
    fn meet_admissions_follow_group_key_order_not_row_order_non_suffix() {
        let min_aggr = parse_aggr("min").unwrap();
        let spec = vec![Some((min_aggr, vec![])), None];
        let mut out = MeetAggrStore::new(spec.clone()).unwrap();
        // Group "a" holds value 9, group "z" holds value 1. Group-key order
        // is a < z; head-tuple (value-first) order is (1,z) < (9,a) — the
        // two orders disagree, which is the whole point.
        out.meet_put(&vg(DataValue::from(9i64), "a")).unwrap();
        out.meet_put(&vg(DataValue::from(1i64), "z")).unwrap();

        let mut store = EpochStore::new_meet(&spec).unwrap();
        let mut rec = Recorder::default();
        store.merge_in(out.wrap(), &mut rec).unwrap();
        assert_eq!(
            rec.0,
            vec![
                vg(DataValue::from(9i64), "a"),
                vg(DataValue::from(1i64), "z")
            ],
            "admissions are in group-key order (a before z)"
        );
        assert_eq!(
            all(&store),
            vec![
                vg(DataValue::from(1i64), "z"),
                vg(DataValue::from(9i64), "a")
            ],
            "the scan surface is in head-tuple order (value-first)"
        );
    }

    /// The changed-flag delta discipline holds at a non-suffix layout too:
    /// a genuinely lower value at position 0 changes the group and is the
    /// delta (carrying the updated value); a non-improving value changes
    /// nothing and yields an empty delta (the fixpoint). The incremental
    /// path keeps `by_group` and `by_row` in lockstep across the update.
    #[test]
    fn meet_merge_delta_non_suffix() {
        let min_aggr = parse_aggr("min").unwrap();
        let spec = vec![Some((min_aggr, vec![])), None];
        let mut store = EpochStore::new_meet(&spec).unwrap();

        let mut out0 = MeetAggrStore::new(spec.clone()).unwrap();
        out0.meet_put(&vg(DataValue::from(5i64), "a")).unwrap();
        assert_eq!(store.merge_in(out0.wrap(), &mut ()).unwrap(), Admitted(1));
        assert_eq!(all(&store), vec![vg(DataValue::from(5i64), "a")]);

        // Epoch 1: a lower value moves the group — delta carries it, and the
        // scan surface reflects the new row, not the old.
        let mut out1 = MeetAggrStore::new(spec.clone()).unwrap();
        out1.meet_put(&vg(DataValue::from(3i64), "a")).unwrap();
        let mut rec = Recorder::default();
        assert_eq!(store.merge_in(out1.wrap(), &mut rec).unwrap(), Admitted(1));
        assert!(store.has_delta());
        assert_eq!(delta(&store), vec![vg(DataValue::from(3i64), "a")]);
        assert_eq!(all(&store), vec![vg(DataValue::from(3i64), "a")]);
        assert_eq!(rec.0, vec![vg(DataValue::from(3i64), "a")]);

        // Epoch 2: a non-improving value changes nothing — empty delta.
        let mut out2 = MeetAggrStore::new(spec).unwrap();
        out2.meet_put(&vg(DataValue::from(8i64), "a")).unwrap();
        assert_eq!(store.merge_in(out2.wrap(), &mut ()).unwrap(), Admitted(0));
        assert!(!store.has_delta());
        assert_eq!(all(&store), vec![vg(DataValue::from(3i64), "a")]);
    }

    // ── adversarial reviewer attacks (adopted from the hostile pass) ──────
    //
    // The reviewer's `rev_*` store tests, adopted verbatim except this one
    // helper: F2 (their own memory finding) makes `by_row` exist only for a
    // non-suffix layout, so the raw-field mirror law holds only there. The
    // helper therefore checks the exposed scan surface (which must equal the
    // interleave of `by_group` in *both* regimes — this strengthens the
    // original) plus the raw `by_row` invariant per regime.

    /// The mirror law, regime-aware: the scan surface is exactly
    /// `{ interleave(k, v) : (k, v) ∈ by_group }`, materialized in `by_row`
    /// for a non-suffix layout and reconstructed from `by_group` (with
    /// `by_row` left empty) for a suffix layout.
    fn rev_assert_lockstep(store: &MeetAggrStore) {
        let derived: BTreeSet<Tuple> = store
            .by_group
            .iter()
            .map(|(k, v)| store.layout.interleave(k, v))
            .collect();
        if store.layout.is_suffix() {
            assert!(
                store.by_row.is_empty(),
                "a suffix layout must not materialize the by_row mirror (F2)"
            );
        } else {
            assert_eq!(
                store.by_row, derived,
                "by_row diverged from the interleave of by_group"
            );
        }
    }

    fn rev_lockstep_of(store: &EpochStore) {
        // The mirror invariant, per level: every interleaved meet level's
        // `by_row` is exactly its groups interleaved, sorted.
        let LevelKind::Meet { spec, levels } = &store.kind else {
            panic!("expected meet stores")
        };
        for level in levels {
            if spec.layout.is_suffix() {
                assert!(level.by_row.is_empty(), "suffix layouts keep no mirror");
                continue;
            }
            let mut derived: Vec<Tuple> = level
                .groups
                .iter()
                .map(|(k, v)| spec.layout.interleave(k, v))
                .collect();
            derived.sort();
            assert_eq!(
                level.by_row, derived,
                "by_row diverged from the interleave of groups"
            );
        }
    }

    /// ATTACK 2a/2c: drive a non-suffix meet EpochStore through every
    /// mutation path — put (vacant, changed, unchanged), fast-path swap,
    /// incremental merge (vacant, changed, unchanged), empty-epoch merge —
    /// asserting the two views stay in lockstep at every step, and that the
    /// delta view (epoch turnover) is lockstep too.
    #[test]
    fn rev_meet_views_lockstep_through_all_paths() {
        let min_aggr = parse_aggr("min").unwrap();
        let spec = vec![Some((min_aggr, vec![])), None];

        // meet_put: vacant, changed, unchanged.
        let mut out = MeetAggrStore::new(spec.clone()).unwrap();
        assert!(out.meet_put(&vg(DataValue::from(9i64), "a")).unwrap());
        rev_assert_lockstep(&out);
        assert!(out.meet_put(&vg(DataValue::from(4i64), "a")).unwrap());
        rev_assert_lockstep(&out);
        assert!(!out.meet_put(&vg(DataValue::from(7i64), "a")).unwrap());
        rev_assert_lockstep(&out);
        assert!(out.meet_put(&vg(DataValue::from(1i64), "z")).unwrap());
        rev_assert_lockstep(&out);

        // Fast-path swap into an empty total (use_total_for_delta epoch).
        let mut store = EpochStore::new_meet(&spec).unwrap();
        store.merge_in(out.wrap(), &mut ()).unwrap();
        rev_lockstep_of(&store);

        // Incremental merge: one changed group, one unchanged, one vacant.
        let mut out1 = MeetAggrStore::new(spec.clone()).unwrap();
        out1.meet_put(&vg(DataValue::from(2i64), "a")).unwrap(); // changes 4 -> 2
        out1.meet_put(&vg(DataValue::from(5i64), "z")).unwrap(); // no change
        out1.meet_put(&vg(DataValue::from(8i64), "q")).unwrap(); // vacant
        store.merge_in(out1.wrap(), &mut ()).unwrap();
        rev_lockstep_of(&store);
        assert_eq!(
            all(&store),
            vec![
                vg(DataValue::from(1i64), "z"),
                vg(DataValue::from(2i64), "a"),
                vg(DataValue::from(8i64), "q"),
            ],
            "scan surface reflects post-merge rows in head-tuple order"
        );
        assert_eq!(
            delta(&store),
            vec![
                vg(DataValue::from(2i64), "a"),
                vg(DataValue::from(8i64), "q"),
            ],
            "delta carries exactly the changed + new groups"
        );

        // Empty epoch: fixpoint; totals untouched, delta empty, lockstep.
        let out2 = MeetAggrStore::new(spec.clone()).unwrap();
        store.merge_in(out2.wrap(), &mut ()).unwrap();
        rev_lockstep_of(&store);
        assert!(!store.has_delta());

        // The stale old_row must be GONE from by_row (not shadowed).
        assert_eq!(all(&store).len(), 3);
        assert!(!all(&store).contains(&vg(DataValue::from(4i64), "a")));
    }

    /// ATTACK 1: empty group key (all positions aggregated) — the group key
    /// is the empty tuple, one group total; exists() answers for any probe;
    /// scans see the single interleaved row.
    #[test]
    fn rev_meet_all_aggregated_single_group() {
        let min_aggr = parse_aggr("min").unwrap();
        let max_aggr = parse_aggr("max").unwrap();
        let spec = vec![Some((min_aggr, vec![])), Some((max_aggr, vec![]))];
        let mut out = MeetAggrStore::new(spec.clone()).unwrap();
        assert!(out.meet_put(&t(&[5, 5])).unwrap());
        assert!(out.meet_put(&t(&[3, 9])).unwrap()); // min 3, max 9
        assert!(!out.meet_put(&t(&[4, 6])).unwrap()); // no change
        assert_eq!(out.by_group.len(), 1, "one group keyed by the empty tuple");
        assert!(out.exists(&t(&[100, -100])), "any probe hits the one group");
        rev_assert_lockstep(&out);

        let mut store = EpochStore::new_meet(&spec).unwrap();
        store.merge_in(out.wrap(), &mut ()).unwrap();
        assert_eq!(all(&store), vec![t(&[3, 9])]);
    }

    /// ATTACK 3: admissions across WILDLY different insertion orders are
    /// identical (group-key order), and the merged store is byte-identical
    /// — insertion order must be laundered out by both views.
    #[test]
    fn rev_meet_admissions_insertion_order_independent() {
        let min_aggr = parse_aggr("min").unwrap();
        let spec = vec![Some((min_aggr, vec![])), None];
        let rows: Vec<Tuple> = vec![
            vg(DataValue::from(9i64), "m"),
            vg(DataValue::from(1i64), "z"),
            vg(DataValue::from(5i64), "a"),
            vg(DataValue::from(3i64), "m"),
            vg(DataValue::from(7i64), "b"),
            vg(DataValue::from(2i64), "a"),
        ];
        let run = |order: &[usize]| -> (Vec<Tuple>, Vec<Tuple>, Vec<Tuple>) {
            let mut out = MeetAggrStore::new(spec.clone()).unwrap();
            for i in order {
                out.meet_put(&rows[*i]).unwrap();
            }
            let mut store = EpochStore::new_meet(&spec).unwrap();
            let mut rec = Recorder::default();
            store.merge_in(out.wrap(), &mut rec).unwrap();
            (rec.0, all(&store), delta(&store))
        };
        let baseline = run(&[0, 1, 2, 3, 4, 5]);
        for order in [[5, 4, 3, 2, 1, 0], [3, 0, 5, 2, 4, 1], [2, 5, 0, 3, 1, 4]] {
            assert_eq!(run(&order), baseline, "order {order:?} diverged");
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
    }

    /// TupleInIter comparison: against other views and plain slices.
    #[test]
    fn tuple_in_iter_ordering() {
        let k1 = t(&[1]);
        let v1 = t(&[5]);
        let k2 = t(&[1, 5]);
        let joined = TupleInIter::new(&k1, &v1, false);
        let flat = TupleInIter::new(&k2, empty_tuple_ref(), false);
        assert_eq!(joined, flat);
        assert_eq!(joined.cmp(&flat), Ordering::Equal);
        assert_eq!(
            joined.partial_cmp(t(&[1, 6]).as_slice()),
            Some(Ordering::Less)
        );
        assert!(joined.eq(t(&[1, 5]).as_slice()));
    }
}
