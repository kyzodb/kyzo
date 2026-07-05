/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Interval derivation (`@spans`) and net diff (`@delta`/`@delta_sys`) as
//! real RA operators over a bitemporal stored relation — story #62 chunk
//! 3. Semantics are judged by `query/laws.rs`'s `derive_intervals`/`diff`;
//! this module is production code and does not depend on that test-only
//! oracle (it is an independently-written twin of the same algebra,
//! proven equal to it by the differential tests in
//! `query/temporal_trials.rs`).
//!
//! ## The scan-direction mismatch, and the buffering decision
//!
//! The storage kernel's governing-version resolution
//! (`data::bitemporal::check_key_for_bitemporal`) is a SKIP scan: it seeks
//! straight to the newest version at or before an `AsOf` coordinate and
//! then splices past every OLDER version of the same fact — it never
//! enumerates a fact's full history. [`SpansRA`] needs the opposite: every
//! stored point-event for a fact, because a maximal-run sweep must see
//! every breakpoint (every distinct valid instant) before it can decide
//! where one run ends and the next begins.
//!
//! No storage-tier primitive yields "every version of one key" today — the
//! contract requires only ordered range scans plus the as-of skip scan
//! (see `storage/mod.rs`'s doc and `data/bitemporal.rs`). Adding one is out
//! of reach for this chunk: `runtime/relation.rs` (the natural home for
//! such a method) carries another builder's uncommitted fix awaiting its
//! own verdict, so this module builds the raw multi-version scan directly
//! against the public storage contract (`ReadTx::range_scan` over the
//! relation's own keyspace bounds, computed here rather than by calling
//! the frozen file's private `keyspace_lower`/`keyspace_upper` — a small,
//! named duplication, not a design choice) and decodes each row itself
//! (`decode_raw_version`, mirroring `check_key_for_bitemporal`'s tail-slot
//! split without its skip-and-splice bound computation, since this scan
//! never skips).
//!
//! **The buffering decision**: [`SpansRA`] streams the ascending raw scan
//! and buffers only ONE fact key's full version set at a time (grouped by
//! the byte-equal key prefix that ascending order already keeps
//! contiguous — `Validity`'s `Reverse` encoding makes each such run
//! newest-first, though the sweep below re-sorts ascending itself and
//! does not rely on that). Memory cost is O(one fact's write history),
//! never O(relation) — the sweep, resolution, and Interval construction
//! for that one key complete before the scan advances to the next key's
//! first row. This is the "buffering strategy per fact key" the brief
//! calls a real design decision: the alternative (materialize the WHOLE
//! relation's raw event log before sweeping any key) was rejected as an
//! avoidable O(relation) working set for no compensating benefit.
//!
//! [`DeltaRA`] needs no such buffering: axis-parameterized diff is defined
//! as the set difference of two already-RESOLVED snapshots
//! (`laws::diff`'s own shape), and each snapshot is exactly what the
//! existing `RelationHandle::skip_scan_all` as-of scan already produces —
//! reused here unchanged, as `patch_naive`'s fallback below. Story #62
//! chunk 3 landed this diff deliberately naive per its own ruling (two
//! full snapshots, materialized, differenced); chunk 4's O(changes)
//! transposed posting-index acceleration has since landed at exactly the
//! seam chunk 3 reserved — this operator's constructor — described in its
//! own section further down this doc. Nothing about `DeltaRA`'s bindings,
//! `from`/`to` coordinates, or its place in the `RelAlgebra` tree changed
//! to add it.
//!
//! ## The signed-fact currency (story #77)
//!
//! [`SignedFact`] is the PRODUCTION twin of `query/laws.rs`'s oracle-only
//! `SignedFact` — the Z-set patch algebra (signed multiplicity with
//! cancellation) `laws.rs` proved as an executable law was, until this
//! story, wired into zero production code: [`DeltaRA`] computed the same
//! two-set difference but tagged its output with a bare `DataValue::Num`
//! sign column, never through a real typed currency. This is a currency
//! change ONLY, not an algorithm change: `iter_batched` below still
//! resolves two full snapshots and differences them (the naive-by-ruling
//! algorithm this module's doc names above, whose acceleration is #62
//! chunk 4's scope, not this story's) — it just carries its intermediate
//! result as `SignedFact`s instead of pre-flattened rows.
//!
//! [`compose`] is proven here — differentialed against `laws.rs::compose`
//! on real engine output, both directions of the cancellation law, in
//! `query/time_travel_trials.rs` — and, as of story #62 chunk 4, has its
//! first real production caller: [`DeltaRA::iter_batched`]'s posting-index
//! fast path (below) builds a `Minus`-tagged patch from every candidate
//! key's `from` row and a `Plus`-tagged patch from every candidate's `to`
//! row, then `compose`s the two together in one call — the same
//! cancellation `laws::diff` itself is defined through. #61's
//! standing-query patch application is still a future consumer, not this
//! one.
//!
//! ## The posting-index fast path (story #62 chunk 4)
//!
//! When the compile tier resolves an `IndexKind::Temporal` posting index
//! for this relation (`query/compile.rs`, the `Delta`-clause arm — the
//! `Valid` axis only; `@delta_sys` has no posting to accelerate off,
//! since the posting's leading column orders by valid instant, not
//! system version), `DeltaRA.posting` is `Some`, and `iter_batched` takes
//! a fundamentally different route than the full-snapshot fallback the
//! rest of this module doc describes:
//!
//! 1. Scan the posting index's own keyspace bounded to exactly the valid
//!    instants that could possibly matter — `(lo, hi]` where `lo`/`hi` are
//!    `from`/`to`'s valid instants in numeric order (an event AT `lo`
//!    itself is already baked into both endpoints' resolutions; an event
//!    outside the window cannot change either). The posting's leading
//!    column is a `Validity` value, which encodes NEWEST-FIRST (see
//!    `data/memcmp.rs`'s bit-flip), so ascending byte order visits `hi`
//!    down to (not including) `lo` — the bound computation in
//!    [`posting_window_bounds`] is this fast path's one genuinely
//!    load-bearing piece of key-encoding reasoning, verified against the
//!    corpus's own ordering fixture, not re-derived from scratch.
//! 2. Decode every posting row's base-key columns (dropping its leading
//!    valid column and bitemporal tail) into a `BTreeSet<Tuple>` of
//!    CANDIDATE keys — a key with zero postings in the window cannot have
//!    changed between `from` and `to`, so it is never a false negative to
//!    omit; a key that DID have a correction-only posting (no net change)
//!    is a false positive the next step resolves away for free.
//! 3. For each candidate key only (never the whole relation), resolve its
//!    row at `from` and at `to` through [`RelationHandle::current_row`] —
//!    the storage kernel's own point read, O(log relation) each — building
//!    two mini-patches (every candidate's `from`-row tagged `Minus`, every
//!    candidate's `to`-row tagged `Plus`). ONE call to [`compose`] combines
//!    them: a candidate whose row is identical at both endpoints (a
//!    redundant re-assert, or a correction with no net effect — exactly
//!    why the candidate set can have false positives) contributes
//!    `Minus(f)` and `Plus(f)` for the same tuple, which `compose` cancels
//!    to nothing, the same guarantee the naive path's set-difference gives
//!    by construction — `compose` finally has its first real production
//!    caller instead of being the tested-but-unused law story #77 left it
//!    as.
//!
//! Output is IDENTICAL to the full-snapshot path by construction (the
//! differential in `query/time_travel_trials.rs` proves fast-path output
//! and full-snapshot output are the same `BTreeSet<SignedFact>` over a
//! seeded generative history) — only the WORK done to reach it differs:
//! O(changes in the window) postings scanned plus O(candidates) point
//! reads, instead of O(whole relation) twice.
use std::collections::{BTreeMap, BTreeSet};

use miette::{Result, bail, miette};

use crate::data::bitemporal::{
    ClaimPolarity, claim_polarity_of_value, extend_tuple_from_bitemporal_v,
};
use crate::data::span::SourceSpan;
use crate::data::symb::Symbol;
use crate::data::tuple::{EncodedKey, Tuple, TupleT, decode_tuple_from_key};
use crate::data::value::{AsOf, DataValue, Interval, StoredValiditySlot, ValidityTs};
use crate::query::batch_ops::{Batch, BatchIter};
use crate::runtime::relation::RelationHandle;
use crate::storage::ReadTx;

/// A signed fact: present in the later snapshot only (`Plus`) or the
/// earlier only (`Minus`) — the production twin of `query/laws.rs`'s
/// oracle-only `SignedFact`. `Ord` is derived in variant-then-payload
/// order, matching the oracle type exactly, so a `BTreeSet<SignedFact>`
/// here and one built from `laws::SignedFact` sort identically. `pub`
/// (not `pub(crate)`): story #61's `Db::register_standing` surfaces
/// this directly as `StandingQuery::apply_pending`'s delta vocabulary —
/// the one name for "a signed delta fact" in this engine, not
/// duplicated under a second, standing-query-specific name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum SignedFact {
    Plus(Tuple),
    Minus(Tuple),
}

impl SignedFact {
    fn tuple(&self) -> &Tuple {
        match self {
            SignedFact::Plus(t) | SignedFact::Minus(t) => t,
        }
    }
}

/// Patch composition with cancellation: tally each tuple's net polarity
/// (`Plus` = +1, `Minus` = -1) across both patches; a tuple whose net is
/// zero cancels out of the result entirely (e.g. a payload that changes
/// and changes back within the composed window). Byte-for-byte the same
/// tally-and-cancel shape as `query/laws.rs::compose` (the executable form
/// of the compositionality law `diff(a,c) == diff(a,b) ⊕ diff(b,c)`,
/// proven there and differentialed against this production copy in
/// `query/time_travel_trials.rs`), independently written so the two never
/// share a bug through shared code.
pub(crate) fn compose(
    first: &BTreeSet<SignedFact>,
    second: &BTreeSet<SignedFact>,
) -> BTreeSet<SignedFact> {
    let mut tally: BTreeMap<&Tuple, i32> = BTreeMap::new();
    for patch in [first, second] {
        for fact in patch {
            let delta = match fact {
                SignedFact::Plus(_) => 1,
                SignedFact::Minus(_) => -1,
            };
            *tally.entry(fact.tuple()).or_insert(0) += delta;
        }
    }
    tally
        .into_iter()
        .filter_map(|(t, net)| match net {
            0 => None,
            n if n > 0 => Some(SignedFact::Plus(t.clone())),
            _ => Some(SignedFact::Minus(t.clone())),
        })
        .collect()
}

/// Interval derivation: `*rel{k, v} @spans iv[, sys]` — one output row per
/// (fact, maximal equal-payload run) along the VALID axis at the fixed
/// system snapshot `sys` (default the record's current belief,
/// `i64::MAX`), `iv` bound to the produced [`Interval`]. Only the valid
/// axis is exposed today (issue #62's ratified surface: system time keeps
/// its own simpler versioning algebra) — see the module doc for why the
/// operator still buffers a full per-fact version set regardless of axis.
#[derive(Debug)]
pub(crate) struct SpansRA {
    /// The base relation's key and payload bindings, plus one trailing
    /// binding for the produced interval — never folded into the base
    /// columns, so relation-arity checks against the base columns are
    /// unaffected (the same shape `SearchAtom::own_bindings` uses for a
    /// search's engine-appended columns).
    pub(crate) bindings: Vec<Symbol>,
    pub(crate) storage: RelationHandle,
    /// The fixed system snapshot the sweep resolves against.
    pub(crate) sys: i64,
    pub(crate) span: SourceSpan,
}

/// Axis-parameterized net diff: `*rel{k} @delta(a, b) sgn` (valid axis,
/// fixed at the current system snapshot) or `@delta_sys(a, b) sgn`
/// (system axis, fixed at the current valid instant). `from`/`to` are
/// already the full bitemporal coordinates (the constructor resolved the
/// axis once); `sgn` binds `+1`/`-1` (new/gone), one trailing binding
/// beyond the base row exactly like [`SpansRA`]'s interval column.
#[derive(Debug)]
pub(crate) struct DeltaRA {
    pub(crate) bindings: Vec<Symbol>,
    pub(crate) storage: RelationHandle,
    pub(crate) from: AsOf,
    pub(crate) to: AsOf,
    pub(crate) span: SourceSpan,
    /// The `IndexKind::Temporal` posting index attached to `storage`, if
    /// the compile tier found and resolved one for this clause (`Valid`
    /// axis only — see the module doc's "posting-index fast path"
    /// section). `None` for every other case, including a fresh
    /// relation with no posting index attached yet: `iter_batched` falls
    /// back to the naive two-snapshot diff exactly as before this chunk.
    pub(crate) posting: Option<RelationHandle>,
}

/// `+1`: the fact holds at `to` but not at `from`.
const SIGN_PLUS: i64 = 1;
/// `-1`: the fact held at `from` but not at `to`.
const SIGN_MINUS: i64 = -1;

/// One stored point-event, decoded from a raw bitemporal row without
/// resolving it against any coordinate: the valid instant, the system
/// version, the claim polarity, and (for [`ClaimPolarity::Assert`] only)
/// the row's non-key payload columns.
///
/// `pub(crate)` (story #80): `runtime/verify.rs`'s `::verify` oracle-feed
/// needs a relation's FULL version history (every assert/retract/erase, not
/// one resolved snapshot) to populate `laws::Program::histories` for
/// as-of/validity queries — this is the one primitive that decodes raw
/// bitemporal rows without resolving them, so it is reused, not
/// re-derived, exactly as this module's own doc argues for itself.
/// Visibility only; the decode logic is unchanged.
pub(crate) struct RawVersion {
    pub(crate) valid: i64,
    pub(crate) sys: i64,
    pub(crate) polarity: ClaimPolarity,
    pub(crate) payload: Tuple,
}

/// The relation's whole keyspace, as raw byte bounds. Duplicates
/// `RelationHandle::keyspace_lower`/`keyspace_upper` (`runtime/relation.rs`,
/// private methods in a file this story does not touch — see the module
/// doc) rather than exposing them; the computation itself
/// (`Tuple::default()` encoded under the relation's id, and the next
/// relation id's raw prefix as the exclusive upper bound) is a few lines
/// of the same public encoding API every caller of this module already
/// uses.
pub(crate) fn relation_keyspace_bounds(storage: &RelationHandle) -> (Vec<u8>, Vec<u8>) {
    let lower = Tuple::default().encode_as_key(storage.id).into_vec();
    let upper = (storage.id.0 + 1).to_be_bytes().to_vec();
    (lower, upper)
}

/// Split a raw bitemporal row into its key-prefix bytes (for grouping),
/// decoded key columns, and [`RawVersion`]. `key_len` is the relation's
/// key arity — used both as a decode-size hint and to prove the decoded
/// key matches the relation's own shape (a mismatch is corruption, refused
/// rather than trusted).
pub(crate) fn decode_raw_version(
    key: &[u8],
    val: &[u8],
    key_len: usize,
) -> Result<(Vec<u8>, Tuple, RawVersion)> {
    if key.len() < EncodedKey::RELATION_PREFIX_LEN + EncodedKey::BITEMPORAL_TAIL_LEN {
        bail!("temporal scan over a key too short to carry its two time slots");
    }
    let prefix_len = key.len() - EncodedKey::BITEMPORAL_TAIL_LEN;
    let mut full = decode_tuple_from_key(key, key_len + 2)?;
    let sys_dv = full
        .pop()
        .ok_or_else(|| miette!("corrupt temporal key: missing its system-time slot"))?;
    let valid_dv = full
        .pop()
        .ok_or_else(|| miette!("corrupt temporal key: missing its valid-time slot"))?;
    let DataValue::Validity(sys_slot) = sys_dv else {
        bail!("corrupt temporal key: system-time slot is not a Validity encoding");
    };
    let DataValue::Validity(valid_slot) = valid_dv else {
        bail!("corrupt temporal key: valid-time slot is not a Validity encoding");
    };
    if !valid_slot.is_assert.0 || !sys_slot.is_assert.0 {
        bail!(
            "corrupt temporal key: a retract flag in a stored time slot \
             (polarity lives in the value; stored slot flags are pinned)"
        );
    }
    if full.len() != key_len {
        bail!(
            "corrupt temporal key: decoded {} key columns, relation declares {key_len}",
            full.len()
        );
    }
    let polarity = claim_polarity_of_value(val)?;
    let mut row = full.clone();
    extend_tuple_from_bitemporal_v(&mut row, val)?;
    let payload = row.split_off(key_len);
    Ok((
        key[..prefix_len].to_vec(),
        full,
        RawVersion {
            valid: valid_slot.timestamp.0.0,
            sys: sys_slot.timestamp.0.0,
            polarity,
            payload,
        },
    ))
}

/// The governing tuple for one fact's already-collected version set, at
/// `(at_valid, at_sys)`: among instants at or before `at_valid`, newest
/// first, the newest version at or before `at_sys` governs that instant;
/// `Assert` holds (`key ++ payload`), `Retract` settles absent (no
/// fall-through), `Erase` is transparent — resolution falls through to
/// the fact's next older instant. The production twin of
/// `laws::resolve_events` (independently written; see the module doc for
/// why production code does not call into the oracle).
fn resolve_at(
    group: &[RawVersion],
    key: &[DataValue],
    at_valid: i64,
    at_sys: i64,
) -> Option<Tuple> {
    let mut instants: Vec<i64> = group
        .iter()
        .map(|e| e.valid)
        .filter(|v| *v <= at_valid)
        .collect();
    instants.sort_unstable();
    instants.dedup();
    for instant in instants.into_iter().rev() {
        let governing = group
            .iter()
            .filter(|e| e.valid == instant && e.sys <= at_sys)
            .max_by_key(|e| e.sys);
        match governing.map(|e| e.polarity) {
            Some(ClaimPolarity::Assert) => {
                let e = governing.expect("just matched Some");
                let mut tuple = key.to_vec();
                tuple.extend(e.payload.iter().cloned());
                return Some(tuple);
            }
            Some(ClaimPolarity::Retract) => return None,
            Some(ClaimPolarity::Erase) | None => {}
        }
    }
    None
}

/// The maximal-run sweep over one fact's fully-collected version set,
/// along the valid axis at the fixed system snapshot `fixed_sys`: every
/// stored valid instant is a candidate breakpoint; the loop closes a run
/// only when the next breakpoint resolves to a DIFFERENT tuple, so
/// coalescing is definitional (un-coalesced output is unrepresentable),
/// exactly mirroring `laws::derive_intervals`. Returns one `key ++
/// payload ++ Interval` row per maximal run.
fn derive_group(group: &[RawVersion], key: &[DataValue], fixed_sys: i64) -> Result<Vec<Tuple>> {
    let mut breaks: Vec<i64> = group.iter().map(|e| e.valid).collect();
    breaks.sort_unstable();
    breaks.dedup();

    let mut out = Vec::new();
    let mut i = 0;
    while i < breaks.len() {
        let start = breaks[i];
        let Some(tuple) = resolve_at(group, key, start, fixed_sys) else {
            i += 1;
            continue;
        };
        let mut j = i;
        while j + 1 < breaks.len()
            && resolve_at(group, key, breaks[j + 1], fixed_sys).as_ref() == Some(&tuple)
        {
            j += 1;
        }
        let end = if j + 1 < breaks.len() {
            breaks[j + 1]
        } else {
            i64::MAX
        };
        // `start` is a real stored valid instant (never the reserved
        // terminal tick — the write path refuses it) and `end` is either
        // a strictly later stored instant or the open-end sentinel, so
        // `start < end` always; a corrupt stored row that defeated the
        // write-side reservation would surface here as a typed error,
        // never a panic (law 5).
        let iv = Interval::new(start, end).map_err(|e| {
            miette!("temporal derivation produced an invalid interval [{start}, {end}): {e}")
        })?;
        let mut row = tuple;
        row.push(DataValue::Interval(iv));
        out.push(row);
        i = j + 1;
    }
    Ok(out)
}

impl SpansRA {
    pub(crate) fn iter_batched<'a>(&'a self, tx: &'a impl ReadTx) -> Result<BatchIter<'a>> {
        let (lower, upper) = relation_keyspace_bounds(&self.storage);
        Ok(Box::new(SpansScanBatches {
            raw: tx.range_scan(&lower, &upper),
            pending_key: None,
            key_len: self.storage.metadata.keys.len(),
            sys_fixed: self.sys,
            done: false,
        }))
    }
}

/// Batch-native derivation scan: pulls the relation's raw ascending
/// key/value stream, groups consecutive rows sharing the same byte-equal
/// key prefix (ascending order keeps one fact's versions contiguous),
/// sweeps each group once it is fully collected, and packs the produced
/// rows into batches of up to [`BATCH_ROWS`] — the same discipline every
/// other batched operator in this crate follows (no row-at-a-time
/// fallback, per `query/ra/mod.rs`'s module doc).
struct SpansScanBatches<'a> {
    raw: Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>,
    /// A row already pulled from `raw` that belongs to the NEXT group
    /// (its key prefix differed from the group being collected) — carried
    /// over so each row is decoded exactly once.
    pending_key: Option<(Vec<u8>, Vec<u8>)>,
    key_len: usize,
    sys_fixed: i64,
    done: bool,
}

impl<'a> SpansScanBatches<'a> {
    /// Collect every raw row sharing one fact's key-prefix bytes, starting
    /// from `first` (already pulled from `raw`), leaving the first row of
    /// the NEXT group (if any) in `self.pending_key`.
    fn collect_group(
        &mut self,
        first: (Vec<u8>, Vec<u8>),
    ) -> Result<(Vec<DataValue>, Vec<RawVersion>)> {
        let (prefix, key, first_ver) = decode_raw_version(&first.0, &first.1, self.key_len)?;
        let mut group = vec![first_ver];
        loop {
            let Some(next) = self.raw.next() else { break };
            let (k, v) = next?;
            if k.len() < EncodedKey::BITEMPORAL_TAIL_LEN
                || k[..k.len() - EncodedKey::BITEMPORAL_TAIL_LEN] != prefix[..]
            {
                self.pending_key = Some((k, v));
                break;
            }
            let (_, _, ver) = decode_raw_version(&k, &v, self.key_len)?;
            group.push(ver);
        }
        Ok((key, group))
    }
}

impl<'a> Iterator for SpansScanBatches<'a> {
    type Item = Result<Batch>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let mut out = Batch::new();
        loop {
            if out.is_full() {
                return Some(Ok(out));
            }
            let first = match self.pending_key.take() {
                Some(kv) => kv,
                None => match self.raw.next() {
                    Some(Ok(kv)) => kv,
                    Some(Err(e)) => {
                        self.done = true;
                        return Some(Err(e));
                    }
                    None => {
                        self.done = true;
                        break;
                    }
                },
            };
            let (key, group) = match self.collect_group(first) {
                Ok(kg) => kg,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            };
            let rows = match derive_group(&group, &key, self.sys_fixed) {
                Ok(rows) => rows,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            };
            for row in rows {
                out.push(row);
            }
        }
        if out.is_empty() { None } else { Some(Ok(out)) }
    }
}

impl DeltaRA {
    /// `from` governs `Minus` facts (present there, gone at `to`); `to`
    /// governs `Plus` facts (absent there, present at `to`) — a payload
    /// change is therefore both a `Minus` (old payload) and a `Plus` (new
    /// payload) fact at the same key, never a "modified" kind — the exact
    /// `SignedFact` shape this module's doc names, computed here through
    /// the production type rather than a bare sign column built ad hoc.
    /// Two routes to the same `BTreeSet<SignedFact>`: the posting-index
    /// fast path when `self.posting` is attached (module doc's "posting-
    /// index fast path" section), the naive full-snapshot diff otherwise —
    /// see [`Self::patch_naive`]/[`Self::patch_via_posting`].
    pub(crate) fn iter_batched<'a>(&'a self, tx: &'a impl ReadTx) -> Result<BatchIter<'a>> {
        let patch = match &self.posting {
            Some(posting) => self.patch_via_posting(tx, posting)?,
            None => self.patch_naive(tx)?,
        };
        let mut rows: Vec<Tuple> = patch
            .into_iter()
            .map(|fact| {
                let (sign, mut row) = match fact {
                    SignedFact::Plus(t) => (SIGN_PLUS, t),
                    SignedFact::Minus(t) => (SIGN_MINUS, t),
                };
                row.push(DataValue::from(sign));
                row
            })
            .collect();
        // Canonical, deterministic output order (the determinism law):
        // every row is sorted by its full content, sign column included.
        rows.sort();
        Ok(Box::new(RowChunks {
            rows: rows.into_iter(),
        }))
    }

    /// Naive by design (per the ruling that landed this operator — the
    /// O(changes) posting-index acceleration was always chunk 4's scope):
    /// resolve both endpoints as full snapshots through the existing as-of
    /// scan, and set-difference them. O(whole relation), twice.
    fn patch_naive(&self, tx: &impl ReadTx) -> Result<BTreeSet<SignedFact>> {
        let mut from_set: BTreeSet<Tuple> = BTreeSet::new();
        for t in self.storage.skip_scan_all(tx, self.from) {
            from_set.insert(t?);
        }
        let mut to_set: BTreeSet<Tuple> = BTreeSet::new();
        for t in self.storage.skip_scan_all(tx, self.to) {
            to_set.insert(t?);
        }
        let mut patch: BTreeSet<SignedFact> = BTreeSet::new();
        for t in from_set.difference(&to_set) {
            patch.insert(SignedFact::Minus(t.clone()));
        }
        for t in to_set.difference(&from_set) {
            patch.insert(SignedFact::Plus(t.clone()));
        }
        Ok(patch)
    }

    /// O(changes): every candidate key comes from a bounded scan of the
    /// posting index (module doc), never a scan of `self.storage` itself;
    /// each candidate is then resolved at both endpoints through
    /// [`RelationHandle::current_row`] — a point read, not a scan. The two
    /// endpoints' results become two mini-patches (every candidate's
    /// `from`-row tagged `Minus`, every candidate's `to`-row tagged
    /// `Plus`), combined with exactly ONE call to [`compose`] — the same
    /// cancellation algebra `laws::diff` itself is defined through,
    /// finally given a real production caller: a candidate whose row is
    /// IDENTICAL at both endpoints (a redundant re-assert, or an
    /// Erase-transparent correction with no net effect) contributes
    /// `Minus(f)` and `Plus(f)` for the same tuple `f`, which `compose`'s
    /// tally cancels to nothing — exactly what the naive path's
    /// `from_set.difference(to_set)`/`to_set.difference(from_set)` pair
    /// achieves by construction, here achieved through the shared algebra
    /// instead of a second hand-rolled implementation of it.
    fn patch_via_posting(
        &self,
        tx: &impl ReadTx,
        posting: &RelationHandle,
    ) -> Result<BTreeSet<SignedFact>> {
        let from_valid = self.from.valid.0.0;
        let to_valid = self.to.valid.0.0;
        let lo = from_valid.min(to_valid);
        let hi = from_valid.max(to_valid);
        let base_key_len = self.storage.metadata.keys.len();
        let candidates = candidate_keys_from_posting(tx, posting, base_key_len, lo, hi)?;

        let mut from_patch: BTreeSet<SignedFact> = BTreeSet::new();
        let mut to_patch: BTreeSet<SignedFact> = BTreeSet::new();
        for key in &candidates {
            if let Some(f) = self.storage.current_row(tx, key, self.from, self.span)? {
                from_patch.insert(SignedFact::Minus(f));
            }
            if let Some(t) = self.storage.current_row(tx, key, self.to, self.span)? {
                to_patch.insert(SignedFact::Plus(t));
            }
        }
        Ok(compose(&from_patch, &to_patch))
    }
}

/// The posting index's own byte bounds for "every posting whose leading
/// valid-instant column lies in `(lo, hi]`" — see the module doc's
/// "posting-index fast path" section for the derivation. `Validity`
/// encodes newest-first (bit-flipped, `data/memcmp.rs`), so the numerically
/// LARGER endpoint (`hi`) is the ASCENDING-byte-order LOWER bound
/// (inclusive), and the smaller endpoint (`lo`) is the upper bound
/// (exclusive) — backwards from what plain integer bounds would suggest,
/// which is exactly why this is factored out and named rather than inlined.
fn posting_window_bounds(posting: &RelationHandle, lo: i64, hi: i64) -> (Vec<u8>, Vec<u8>) {
    let col_at =
        |ts: i64| vec![StoredValiditySlot::new(ValidityTs(std::cmp::Reverse(ts))).as_datavalue()];
    let lower = col_at(hi).encode_as_key(posting.id).into_vec();
    let upper = col_at(lo).encode_as_key(posting.id).into_vec();
    (lower, upper)
}

/// Every DISTINCT base key with at least one posting in `(lo, hi]` — the
/// candidate set the fast path resolves at both endpoints. `lo == hi`
/// (identical snapshots) is the empty window, correctly producing no
/// candidates and hence no patch, matching the naive path's
/// `diff_identical_snapshots_is_empty` law.
fn candidate_keys_from_posting(
    tx: &impl ReadTx,
    posting: &RelationHandle,
    base_key_len: usize,
    lo: i64,
    hi: i64,
) -> Result<BTreeSet<Tuple>> {
    if lo >= hi {
        return Ok(BTreeSet::new());
    }
    let (lower, upper) = posting_window_bounds(posting, lo, hi);
    let mut keys = BTreeSet::new();
    for row in tx.range_scan(&lower, &upper) {
        let (k, _v) = row?;
        // The posting's own declared key arity is `1 (leading valid) +
        // base_key_len`; `decode_tuple_from_key` wants that plus the two
        // mandatory bitemporal tail slots every Facts key carries.
        let full = decode_tuple_from_key(&k, 1 + base_key_len + 2)?;
        if full.len() != 1 + base_key_len + 2 {
            bail!(
                "corrupt posting key: decoded {} columns, expected {}",
                full.len(),
                1 + base_key_len + 2
            );
        }
        keys.insert(full[1..1 + base_key_len].to_vec());
    }
    Ok(keys)
}

/// Packs an already-materialized, already-ordered row sequence into
/// batches of up to [`BATCH_ROWS`] — [`DeltaRA`]'s naive snapshot-diff is
/// fully resolved before any batch is built, so this is plain chunking,
/// not a scan.
struct RowChunks {
    rows: std::vec::IntoIter<Tuple>,
}

impl Iterator for RowChunks {
    type Item = Result<Batch>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut out = Batch::new();
        for row in self.rows.by_ref() {
            out.push(row);
            if out.is_full() {
                return Some(Ok(out));
            }
        }
        if out.is_empty() { None } else { Some(Ok(out)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::relation::{ColType, ColumnDef, NullableColType, StoredRelationMetadata};
    use crate::data::span::SourceSpan;
    use crate::data::symb::Symbol;
    use crate::data::value::{MAX_VALIDITY_TS, ValidityTs};
    use crate::runtime::relation::{KeyspaceKind, create_relation};
    use crate::storage::fjall::new_fjall_storage;
    use crate::storage::{Storage, WriteTx};
    use itertools::Itertools;
    use std::cmp::Reverse;

    fn sp() -> SourceSpan {
        SourceSpan(0, 0)
    }
    fn sym(name: &str) -> Symbol {
        Symbol::new(name, sp())
    }
    fn v(i: i64) -> DataValue {
        DataValue::from(i)
    }
    fn vts(t: i64) -> ValidityTs {
        ValidityTs(Reverse(t))
    }
    fn col(name: &str) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            typing: NullableColType {
                coltype: ColType::Any,
                nullable: false,
            },
            default_gen: None,
        }
    }

    /// A one-key, one-value relation, `key_arity` key columns.
    fn make_relation(
        db: &crate::storage::fjall::FjallStorage,
        name: &str,
        key_arity: usize,
    ) -> RelationHandle {
        let keys: Vec<ColumnDef> = (0..key_arity).map(|i| col(&format!("k{i}"))).collect();
        let key_bindings = keys.iter().map(|c| sym(&c.name)).collect();
        let input = crate::data::program::InputRelationHandle {
            name: sym(name),
            metadata: StoredRelationMetadata {
                keys,
                non_keys: vec![col("val")],
            },
            key_bindings,
            dep_bindings: vec![sym("val")],
            span: sp(),
        };
        let mut tx = db.write_tx().expect("write tx");
        let handle = create_relation(&mut tx, input, KeyspaceKind::Facts).expect("create relation");
        tx.commit().expect("commit");
        handle
    }

    /// Assert `key ++ [val]` at `valid`, its own transaction (so it gets a
    /// genuinely distinct, monotonically increasing system stamp — every
    /// commit mints one).
    fn assert_at(
        db: &crate::storage::fjall::FjallStorage,
        handle: &RelationHandle,
        key: i64,
        valid: i64,
        val: i64,
    ) {
        let mut tx = db.write_tx().expect("write tx");
        handle
            .put_fact(&mut tx, &[v(key), v(val)], vts(valid), sp())
            .expect("put fact");
        tx.commit().expect("commit");
    }

    fn retract_at(
        db: &crate::storage::fjall::FjallStorage,
        handle: &RelationHandle,
        key: i64,
        valid: i64,
    ) {
        let mut tx = db.write_tx().expect("write tx");
        handle
            .retract_fact(&mut tx, &[v(key)], vts(valid), sp())
            .expect("retract fact");
        tx.commit().expect("commit");
    }

    /// Write a raw ERASE row directly (no production write-path exposes
    /// `ClaimPolarity::Erase` today — see the module doc's law list; this
    /// is test-only plumbing through the same `pub(crate)` encoders
    /// `put_fact`/`retract_fact` themselves use).
    fn erase_at(
        db: &crate::storage::fjall::FjallStorage,
        handle: &RelationHandle,
        key: i64,
        valid: i64,
    ) {
        let mut tx = db.write_tx().expect("write tx");
        let sys = tx.system_stamp();
        let encoded_key = handle
            .encode_bitemporal_key_for_store(&[v(key)], vts(valid), sys, sp())
            .expect("encode key");
        let val = handle
            .encode_bitemporal_val_for_store(&[v(key)], ClaimPolarity::Erase, sp())
            .expect("encode erase value");
        tx.put(encoded_key.as_bytes(), &val).expect("put erase row");
        tx.commit().expect("commit");
    }

    fn spans_rows(
        db: &crate::storage::fjall::FjallStorage,
        handle: &RelationHandle,
        sys: i64,
    ) -> Vec<(i64, i64, i64, i64)> {
        let ra = SpansRA {
            bindings: vec![sym("k"), sym("val"), sym("iv")],
            storage: handle.clone(),
            sys,
            span: sp(),
        };
        let rtx = db.read_tx().expect("read tx");
        let mut out = vec![];
        for batch in ra.iter_batched(&rtx).expect("iter") {
            for row in batch.expect("batch").into_rows() {
                let DataValue::Num(crate::data::value::Num::Int(k)) = row[0] else {
                    panic!("key not an int")
                };
                let DataValue::Num(crate::data::value::Num::Int(val)) = row[1] else {
                    panic!("val not an int")
                };
                let DataValue::Interval(iv) = &row[2] else {
                    panic!("third column not an interval: {row:?}")
                };
                out.push((k, val, iv.start(), iv.end()));
            }
        }
        out.sort();
        out
    }

    #[test]
    fn single_assert_is_one_open_interval() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "spans_single", 1);
        assert_at(&db, &h, 1, 10, 100);
        let rows = spans_rows(&db, &h, i64::MAX);
        assert_eq!(rows, vec![(1, 100, 10, i64::MAX)]);
    }

    #[test]
    fn retract_clips_the_interval_exclusive() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "spans_retract", 1);
        assert_at(&db, &h, 1, 10, 100);
        retract_at(&db, &h, 1, 20);
        let rows = spans_rows(&db, &h, i64::MAX);
        assert_eq!(rows, vec![(1, 100, 10, 20)]);
    }

    #[test]
    fn payload_change_splits_into_two_intervals() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "spans_split", 1);
        assert_at(&db, &h, 1, 10, 100);
        assert_at(&db, &h, 1, 20, 200);
        let rows = spans_rows(&db, &h, i64::MAX);
        assert_eq!(rows, vec![(1, 100, 10, 20), (1, 200, 20, i64::MAX)]);
    }

    #[test]
    fn double_assert_same_payload_is_idempotent_one_interval() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "spans_idempotent", 1);
        assert_at(&db, &h, 1, 10, 100);
        assert_at(&db, &h, 1, 20, 100);
        let rows = spans_rows(&db, &h, i64::MAX);
        assert_eq!(rows, vec![(1, 100, 10, i64::MAX)]);
    }

    #[test]
    fn assert_after_retract_opens_a_new_interval() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "spans_reopen", 1);
        assert_at(&db, &h, 1, 10, 100);
        retract_at(&db, &h, 1, 20);
        assert_at(&db, &h, 1, 30, 100);
        let rows = spans_rows(&db, &h, i64::MAX);
        assert_eq!(rows, vec![(1, 100, 10, 20), (1, 100, 30, i64::MAX)]);
    }

    #[test]
    fn dangling_retract_holds_nowhere() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "spans_dangling_retract", 1);
        retract_at(&db, &h, 1, 10);
        let rows = spans_rows(&db, &h, i64::MAX);
        assert!(rows.is_empty());
    }

    /// Erase transparency: a system-time correction that un-records the
    /// instant-10 assertion falls through to the older instant-0 one.
    #[test]
    fn erase_is_transparent_falls_through_to_older_instant() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "spans_erase", 1);
        assert_at(&db, &h, 1, 0, 100);
        assert_at(&db, &h, 1, 10, 200);
        erase_at(&db, &h, 1, 10);
        let rows = spans_rows(&db, &h, i64::MAX);
        // Instant 10 is erased, so the payload is 100 throughout (the
        // instant-0 assert governs everywhere it would have fallen
        // through to) — one interval, not two.
        assert_eq!(rows, vec![(1, 100, 0, i64::MAX)]);
    }

    #[test]
    fn no_zero_width_intervals_at_any_fixed_sys() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "spans_no_zero_width", 1);
        assert_at(&db, &h, 1, 10, 100);
        assert_at(&db, &h, 1, 10, 200); // same instant, later sys: corrects it
        for (start, end, _, _) in spans_rows(&db, &h, i64::MAX)
            .into_iter()
            .map(|(k, val, s, e)| (s, e, k, val))
        {
            assert!(start < end, "zero-width interval [{start}, {end})");
        }
        // At the OLDER system snapshot the first (pre-correction) write
        // governs — still no zero-width run.
        let db2 = new_fjall_storage(tempfile_dir()).expect("storage");
        let h2 = make_relation(&db2, "spans_no_zero_width2", 1);
        assert_at(&db2, &h2, 1, 10, 100);
        let rows = spans_rows(&db2, &h2, i64::MAX);
        assert_eq!(rows, vec![(1, 100, 10, i64::MAX)]);
    }

    #[test]
    fn multiple_facts_derive_independently() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "spans_multi", 1);
        assert_at(&db, &h, 1, 10, 100);
        assert_at(&db, &h, 2, 5, 900);
        retract_at(&db, &h, 2, 15);
        let rows = spans_rows(&db, &h, i64::MAX);
        assert_eq!(
            rows.into_iter().sorted().collect_vec(),
            vec![(1, 100, 10, i64::MAX), (2, 900, 5, 15)]
        );
    }

    fn delta_rows(
        db: &crate::storage::fjall::FjallStorage,
        handle: &RelationHandle,
        from: AsOf,
        to: AsOf,
    ) -> Vec<(i64, i64, i64)> {
        delta_rows_with_posting(db, handle, from, to, None)
    }

    fn delta_rows_with_posting(
        db: &crate::storage::fjall::FjallStorage,
        handle: &RelationHandle,
        from: AsOf,
        to: AsOf,
        posting: Option<RelationHandle>,
    ) -> Vec<(i64, i64, i64)> {
        let ra = DeltaRA {
            bindings: vec![sym("k"), sym("val"), sym("sgn")],
            storage: handle.clone(),
            from,
            to,
            span: sp(),
            posting,
        };
        let rtx = db.read_tx().expect("read tx");
        let mut out = vec![];
        for batch in ra.iter_batched(&rtx).expect("iter") {
            for row in batch.expect("batch").into_rows() {
                let DataValue::Num(crate::data::value::Num::Int(k)) = row[0] else {
                    panic!("key not an int")
                };
                let DataValue::Num(crate::data::value::Num::Int(val)) = row[1] else {
                    panic!("val not an int")
                };
                let DataValue::Num(crate::data::value::Num::Int(sgn)) = row[2] else {
                    panic!("sign not an int")
                };
                out.push((k, val, sgn));
            }
        }
        out
    }

    #[test]
    fn diff_valid_axis_sees_a_new_assertion_as_plus() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "delta_new", 1);
        assert_at(&db, &h, 1, 10, 100);
        let rows = delta_rows(&db, &h, AsOf::current(vts(5)), AsOf::current(vts(20)));
        assert_eq!(rows, vec![(1, 100, SIGN_PLUS)]);
    }

    #[test]
    fn diff_payload_change_is_a_minus_plus_pair_never_modified() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "delta_change", 1);
        assert_at(&db, &h, 1, 10, 100);
        assert_at(&db, &h, 1, 20, 200);
        let rows = delta_rows(&db, &h, AsOf::current(vts(15)), AsOf::current(vts(25)));
        assert_eq!(rows.len(), 2);
        assert!(rows.contains(&(1, 100, SIGN_MINUS)));
        assert!(rows.contains(&(1, 200, SIGN_PLUS)));
    }

    #[test]
    fn diff_identical_snapshots_is_empty() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "delta_empty", 1);
        assert_at(&db, &h, 1, 10, 100);
        let rows = delta_rows(&db, &h, AsOf::current(vts(20)), AsOf::current(vts(20)));
        assert!(rows.is_empty());
    }

    #[test]
    fn diff_sys_axis_sees_a_correction() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let h = make_relation(&db, "delta_sys", 1);
        assert_at(&db, &h, 1, 10, 100); // sys stamp 1 (first commit)
        assert_at(&db, &h, 1, 10, 200); // sys stamp 2 (correction, same instant)
        // Fixed valid = MAX (current belief about "now"); sys axis varies.
        let before = AsOf::at(vts(0), MAX_VALIDITY_TS);
        let after = AsOf::current(MAX_VALIDITY_TS);
        let rows = delta_rows(&db, &h, before, after);
        assert_eq!(rows, vec![(1, 200, SIGN_PLUS)]);
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kyzo-temporal-ra-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    // ─────────────────────────────────────────────────────────────────
    // The posting-index fast path (story #62 chunk 4): every test below
    // proves `DeltaRA` with `posting: Some(idx_handle)` produces the
    // IDENTICAL `BTreeSet<SignedFact>` (via `delta_rows`'s already-sorted
    // output) as `posting: None` — the correctness-first requirement the
    // acceleration is worthless without. `assert_at`/`retract_at` above
    // cannot be reused here: they write straight through `RelationHandle`
    // (`put_fact`/`retract_fact`), which never calls `update_indices`, so
    // a posting index attached to their relation would silently stay
    // empty. These tests drive `SessionTx` directly instead — the same
    // level `runtime/mutate.rs`'s own `temporal_index_tests` module
    // drives it at, `update_indices` widened to `pub(crate)` for exactly
    // this cross-module reuse.
    // ─────────────────────────────────────────────────────────────────

    use crate::runtime::db::{ScriptOptions, SessionTx};
    use crate::runtime::relation::get_relation;

    /// A one-key relation with a temporal posting index attached,
    /// returning `(base handle, posting handle)` — both already resolved,
    /// ready to feed a fresh write session or `DeltaRA` directly.
    fn make_indexed_relation(
        db: &crate::storage::fjall::FjallStorage,
        name: &str,
    ) -> (RelationHandle, RelationHandle) {
        let mut stx =
            SessionTx::new_write(db.write_tx().expect("write tx"), ScriptOptions::default());
        let input = crate::data::program::InputRelationHandle {
            name: sym(name),
            metadata: StoredRelationMetadata {
                keys: vec![col("k")],
                non_keys: vec![col("val")],
            },
            key_bindings: vec![sym("k")],
            dep_bindings: vec![sym("val")],
            span: sp(),
        };
        stx.create_relation(input, KeyspaceKind::Facts)
            .expect("create base relation");
        stx.create_temporal_index(name, "t")
            .expect("create temporal index");
        stx.store.commit().expect("commit setup");
        let rtx = db.read_tx().expect("read tx");
        let base = get_relation(&rtx, name).expect("base handle");
        let idx = get_relation(&rtx, &format!("{name}:t")).expect("index handle");
        (base, idx)
    }

    /// One event, its own fresh write session (so it mints its own
    /// distinct system stamp, exactly like `assert_at`/`retract_at`
    /// above) — writes the base row AND maintains every attached index
    /// through the real `update_indices` seam, so the posting index and
    /// the base relation advance in lockstep.
    fn write_indexed_event(
        db: &crate::storage::fjall::FjallStorage,
        base: &RelationHandle,
        key: i64,
        valid: i64,
        val: Option<i64>,
    ) {
        let mut stx =
            SessionTx::new_write(db.write_tx().expect("write tx"), ScriptOptions::default());
        let sys = stx.store.system_stamp();
        let key_cols = vec![v(key)];
        match val {
            Some(payload) => {
                let full = vec![v(key), v(payload)];
                let enc_key = base
                    .encode_bitemporal_key_for_store(&key_cols, vts(valid), sys, sp())
                    .expect("encode key");
                let enc_val = base
                    .encode_bitemporal_val_for_store(&full, ClaimPolarity::Assert, sp())
                    .expect("encode assert value");
                stx.put_routed(base.is_temp, enc_key.as_bytes(), &enc_val)
                    .expect("put base row");
                stx.update_indices(base, Some(&full), None, vts(valid), sys)
                    .expect("maintain indices");
            }
            None => {
                let enc_key = base
                    .encode_bitemporal_key_for_store(&key_cols, vts(valid), sys, sp())
                    .expect("encode key");
                let enc_val = base
                    .encode_bitemporal_val_for_store(&key_cols, ClaimPolarity::Retract, sp())
                    .expect("encode retract value");
                stx.put_routed(base.is_temp, enc_key.as_bytes(), &enc_val)
                    .expect("put base row");
                stx.update_indices(base, None, Some(&key_cols), vts(valid), sys)
                    .expect("maintain indices");
            }
        }
        stx.store.commit().expect("commit event");
    }

    /// Both paths on the SAME two coordinates: the naive full-snapshot
    /// diff (`posting: None`) and the posting-index fast path
    /// (`posting: Some`) MUST agree — `delta_rows`'s output is already
    /// canonically sorted by `DeltaRA::iter_batched` itself, so direct
    /// `Vec` equality is the whole law.
    fn assert_paths_agree(
        db: &crate::storage::fjall::FjallStorage,
        base: &RelationHandle,
        idx: &RelationHandle,
        from: AsOf,
        to: AsOf,
    ) {
        let naive = delta_rows(db, base, from, to);
        let fast = delta_rows_with_posting(db, base, from, to, Some(idx.clone()));
        assert_eq!(
            naive, fast,
            "fast path disagreed with the naive path at from={from:?} to={to:?}"
        );
    }

    #[test]
    fn posting_fast_path_matches_naive_on_a_new_assertion() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let (base, idx) = make_indexed_relation(&db, "posting_new");
        write_indexed_event(&db, &base, 1, 10, Some(100));
        assert_paths_agree(
            &db,
            &base,
            &idx,
            AsOf::current(vts(5)),
            AsOf::current(vts(20)),
        );
    }

    #[test]
    fn posting_fast_path_matches_naive_on_a_payload_change() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let (base, idx) = make_indexed_relation(&db, "posting_change");
        write_indexed_event(&db, &base, 1, 10, Some(100));
        write_indexed_event(&db, &base, 1, 20, Some(200));
        assert_paths_agree(
            &db,
            &base,
            &idx,
            AsOf::current(vts(15)),
            AsOf::current(vts(25)),
        );
    }

    #[test]
    fn posting_fast_path_matches_naive_on_identical_snapshots() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let (base, idx) = make_indexed_relation(&db, "posting_identical");
        write_indexed_event(&db, &base, 1, 10, Some(100));
        assert_paths_agree(
            &db,
            &base,
            &idx,
            AsOf::current(vts(20)),
            AsOf::current(vts(20)),
        );
    }

    #[test]
    fn posting_fast_path_matches_naive_on_a_retraction() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let (base, idx) = make_indexed_relation(&db, "posting_retract");
        write_indexed_event(&db, &base, 1, 10, Some(100));
        write_indexed_event(&db, &base, 1, 20, None);
        assert_paths_agree(
            &db,
            &base,
            &idx,
            AsOf::current(vts(15)),
            AsOf::current(vts(25)),
        );
        // A window entirely BEFORE any event: empty candidate set either way.
        assert_paths_agree(
            &db,
            &base,
            &idx,
            AsOf::current(vts(1)),
            AsOf::current(vts(5)),
        );
    }

    #[test]
    fn posting_fast_path_matches_naive_on_a_backward_diff() {
        // `to` earlier than `from`: `lo`/`hi` still resolve correctly
        // since the fast path takes them by numeric min/max, not by
        // positional role.
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let (base, idx) = make_indexed_relation(&db, "posting_backward");
        write_indexed_event(&db, &base, 1, 10, Some(100));
        assert_paths_agree(
            &db,
            &base,
            &idx,
            AsOf::current(vts(20)),
            AsOf::current(vts(5)),
        );
    }

    /// A seeded generative campaign (xorshift64*, the same dependency-free
    /// deterministic PRNG `query/ra/stored.rs`'s own accelerated-vs-
    /// unaccelerated differential uses): many keys, many random
    /// assert/retract events, many random coordinate pairs — the two
    /// production paths must agree on every one.
    #[test]
    fn posting_fast_path_matches_naive_generatively() {
        let db = new_fjall_storage(tempfile_dir()).expect("storage");
        let (base, idx) = make_indexed_relation(&db, "posting_generative");

        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next_u64 = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut next_range = |n: u64| next_u64() % n;

        const N_EVENTS: usize = 60;
        const N_KEYS: i64 = 5;
        const MAX_VALID: i64 = 40;
        for _ in 0..N_EVENTS {
            let key = next_range(N_KEYS as u64) as i64;
            let valid = next_range(MAX_VALID as u64) as i64;
            if next_range(4) == 0 {
                write_indexed_event(&db, &base, key, valid, None);
            } else {
                let payload = next_range(1000) as i64;
                write_indexed_event(&db, &base, key, valid, Some(payload));
            }
        }

        const N_PAIRS: usize = 40;
        for _ in 0..N_PAIRS {
            let from = next_range(MAX_VALID as u64 + 2) as i64;
            let to = next_range(MAX_VALID as u64 + 2) as i64;
            assert_paths_agree(
                &db,
                &base,
                &idx,
                AsOf::current(vts(from)),
                AsOf::current(vts(to)),
            );
        }
    }
}
