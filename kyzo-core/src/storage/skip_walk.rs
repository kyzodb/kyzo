/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The bitemporal skip-scan walk — ONE implementation, generic over the
//! backend's seek primitive, driving a SINGLE positioned cursor across the
//! whole scan rather than reopening a fresh range per version step. This
//! module previously drove `storage/fjall.rs`, `storage/temp.rs`, and
//! `storage/sim.rs` through a stateless `seek_first(lower, upper)` seam
//! that each backend implemented by opening a brand-new bounded range and
//! taking its first item — cheap for `temp`/`sim`'s `BTreeMap`, but on
//! `fjall` a fresh range re-derives the whole read path from scratch: a
//! version-history lock, a lookup of which runs/tables/memtables are
//! live, and a new merge/heap/tombstone-filter stack, ALL repeated on
//! every single version step even though the fact being resolved rarely
//! changes. The walk over `data::bitemporal::check_key_for_bitemporal`
//! (the resolution kernel this module never reimplements, only calls) is
//! unchanged: seek to the next candidate key, peek its polarity from its
//! value, let the kernel decide (emit or not) and hand back the next seek
//! bound, then advance strictly past the key just examined — a corrupt
//! key or value surfaces as `Err` WITHOUT advancing, so a scan cannot
//! step over bytes it could not judge.
//!
//! ## The seek seam
//!
//! ```text
//! pub(crate) trait SkipSeek {
//!     type Cursor<'c>: SkipCursor where Self: 'c;
//!     fn open_skip_cursor<'c>(&'c self, lower: &[u8], upper: &[u8]) -> Self::Cursor<'c>;
//! }
//!
//! pub(crate) trait SkipCursor {
//!     fn seek(&mut self, target: &[u8]) -> Option<Result<(Vec<u8>, Vec<u8>)>>;
//! }
//! ```
//!
//! `open_skip_cursor` runs EXACTLY ONCE per walk — it is where a backend
//! pays whatever one-time cost real positioning requires (on `fjall`,
//! that is `TreeIter`'s `SuperVersion` lookup and locating which
//! runs/tables/memtables the scan will touch). `seek` then runs once per
//! version step, repositioning that SAME cursor forward to the first key
//! at or after `target` — on `fjall` this reuses the held `SuperVersion`
//! (no relock, no re-lookup) and repositions each backing run/table/
//! memtable through its OWN range entry point (index block, then
//! restart-point binary search, then linear scan — never the point-get
//! hash index; see `vendor/lsm-tree/src/range.rs`'s `TreeIter::seek`).
//! `temp`/`sim` (`BTreeMap`-backed) have no cheaper primitive than a fresh
//! `BTreeMap::range` call per seek — for a `BTreeMap` that call already
//! IS the real seek (an O(log n) descent, not a rebuild of read-path
//! machinery) — so their `SkipCursor::seek` legitimately re-derives a
//! `Range` each call; what every backend now shares is that
//! `open_skip_cursor` runs once, never once per version.
//!
//! Degenerate bounds (`lower >= upper`) never reach a cursor at all:
//! [`SkipWalk::next`]'s own loop guard (below) returns `None` before ever
//! calling `seek` when `next_bound >= upper`, so `open_skip_cursor` and
//! `seek` are free to assume a well-formed, non-empty range.
//!
//! ## The walk (`SkipWalk`)
//!
//! `SkipWalk<C: SkipCursor>` OWNS the opened cursor (built once, by the
//! backend's `range_skip_scan_tuple`, via `open_skip_cursor`), the fixed
//! upper bound, the `AsOf` coordinate, and the mutable re-seek bound.
//! Each `next()` call:
//!
//! 1. Exits if the seek bound has reached (or passed) the upper bound —
//!    the loop's own termination for an exhausted range.
//! 2. Calls `cursor.seek(&next_bound)`. `None` ends the scan;
//!    `Some(Err(e))` surfaces the error WITHOUT moving `next_bound` — the
//!    next poll re-seeks the identical (already-known-bad) range and
//!    re-yields the same error, so a caller that keeps polling past an
//!    `Err` cannot silently skip bytes it never judged.
//! 3. Peeks the row's [`ClaimPolarity`](crate::data::bitemporal::ClaimPolarity)
//!    from its value (`claim_polarity_of_value`) and hands `(key, polarity,
//!    as_of)` to the kernel (`check_key_for_bitemporal`), which returns
//!    an optional tuple to emit and the SPLICED bound for the next seek —
//!    the kernel owns the splice algebra entirely (re-seek within the same
//!    instant on a system-time miss, re-seek at the query's valid instant
//!    on a valid-time miss, skip to `TERMINAL_VALIDITY` on a settled hit);
//!    this module never recomputes it, only applies it.
//! 4. **The termination guard, stated once**: the kernel's returned bound
//!    is trusted ONLY if it strictly exceeds the key just examined
//!    (`nxt_bound.as_slice() > k.as_slice()`); every bound the kernel
//!    returns for a key that parsed already satisfies this (pinned slot
//!    flags mean no stored key can equal a splice targeting the
//!    `TERMINAL_VALIDITY` sentinel), so the branch is belt-and-braces
//!    against a case no argument anticipated — the byte-successor of the
//!    examined key (`k ++ 0x00`, the smallest key strictly greater under
//!    memcmp order) is the fallback that makes forward progress
//!    unconditional on ANY stored bytes, honest or hostile. Without this
//!    fallback a corrupt-but-parseable key whose splice bound happened to
//!    equal or precede itself would spin the walk forever (a livelock,
//!    not a crash — worse, because nothing panics to report it).
//! 5. On a hit (`Some(tuple)`), extends the tuple with the value's
//!    non-key columns (`extend_tuple_from_bitemporal_v`) and yields it;
//!    on a miss, loops back to step 1 with the advanced bound.
//!
//! ## Per-backend wiring
//!
//! **`storage/fjall.rs`**: `FjallSkipCursor` wraps `fjall::SeekIter`
//! (itself a thin `SnapshotNonce`-holding wrapper around
//! `lsm_tree::SeekableRangeIter`, whose `Standard` arm is the real
//! `TreeIter`). `open_skip_cursor` guards `lower >= upper` itself (never
//! letting an inverted range reach fjall's conflict manager, which panics
//! on one at commit — the same guard `raw_range` already applies to the
//! plain range-scan path) and otherwise calls `self.$reader.seek_range(..)`
//! once; each `seek` call is `SeekIter::seek`.
//!
//! **`storage/temp.rs`**: `TempSkipCursor` borrows the `BTreeMap` and the
//! fixed upper bound; `seek` is `self.map.range((Included(target),
//! Excluded(upper))).next()` — a fresh `Range` per call, which for a
//! `BTreeMap` already is the real O(log n) seek this walk needs (there is
//! no stable-Rust primitive to hold a `BTreeMap` cursor across calls
//! cheaper than this).
//!
//! **`storage/sim.rs`**: `open_skip_cursor` itself does nothing — there is
//! no expensive one-time setup to save for an in-memory `BTreeMap`, so
//! this backend gets no efficiency win from the split. What matters here
//! is fidelity, not speed: `SimReadSkipCursor`/`SimWriteSkipCursor` keep
//! doing the DST bookkeeping (`ctx.yield_turn()`, `ctx.check_read_fault(..)`,
//! `track_range(..)` for the write side) ONCE PER SEEK STEP, inside
//! `seek`, exactly as the old per-step `seek_first` did — collapsing it
//! into `open_skip_cursor` would silently narrow the fault-injection and
//! scheduling-interleaving surface the sim exists to stress down to one
//! decision point per walk instead of one per version step.

use miette::Result;

use crate::data::bitemporal::{
    check_key_for_bitemporal, claim_polarity_of_value, extend_tuple_from_bitemporal_v,
};
use crate::data::tuple::Tuple;
use crate::data::value::AsOf;

/// A single positioned cursor, repositioned forward by [`Self::seek`]
/// rather than rebuilt. `target` is always non-decreasing across calls on
/// the same cursor (the walk only ever moves forward); a cursor may
/// assume this and is never asked to seek backward.
pub(crate) trait SkipCursor {
    fn seek(&mut self, target: &[u8]) -> Option<Result<(Vec<u8>, Vec<u8>)>>;
}

/// The seam a backend implements: "open one cursor over `[lower, upper)`,
/// paying whatever one-time setup cost real positioning requires." See
/// the module doc for why splitting this from [`SkipCursor::seek`] (which
/// runs once per version step) is what makes a skip scan seek instead of
/// reopen.
pub(crate) trait SkipSeek {
    type Cursor<'c>: SkipCursor
    where
        Self: 'c;

    fn open_skip_cursor<'c>(&'c self, lower: &[u8], upper: &[u8]) -> Self::Cursor<'c>;
}

/// THE bitemporal skip-scan walk: generic over one backend's
/// [`SkipCursor`], so every implementor inherits this algorithm — and,
/// per issue #78's dictation, the property proof over it — verbatim. See
/// the module doc for the step-by-step algorithm and the termination
/// guard's rationale.
pub(crate) struct SkipWalk<C: SkipCursor> {
    cursor: C,
    upper: Vec<u8>,
    as_of: AsOf,
    next_bound: Vec<u8>,
}

impl<C: SkipCursor> SkipWalk<C> {
    pub(crate) fn new(cursor: C, lower: &[u8], upper: &[u8], as_of: AsOf) -> Self {
        SkipWalk {
            cursor,
            upper: upper.to_vec(),
            as_of,
            next_bound: lower.to_vec(),
        }
    }
}

/// The termination guarantee, stated once and pulled out as its own named
/// law so it is directly testable independent of whether
/// `check_key_for_bitemporal`'s splice algebra can ever hand back a
/// non-advancing bound for a key that decoded successfully (by
/// construction of that algebra it currently cannot: every early-return
/// splices in the QUERY's own out-of-range coordinate, which the branch
/// condition already proves sorts strictly past the examined key's, and a
/// governing hit splices to `TERMINAL_VALIDITY`, whose `is_assert = false`
/// tail sorts strictly after any stored `is_assert = true` tail at an
/// equal timestamp — see the module doc's guard rationale). The guard is
/// belt-and-braces for bytes no argument anticipated, not a path the
/// current honest kernel exercises; [`advance_past`] is tested directly,
/// as its own law, precisely because that unreachability is a property of
/// today's algebra, not a proof obligation this driver gets to assume.
///
/// `candidate_bound` is trusted ONLY if it strictly exceeds `examined`;
/// otherwise the byte-successor of `examined` (`examined ++ 0x00`, the
/// smallest key strictly greater under memcmp order) is the unconditional
/// forward-progress fallback.
fn advance_past(examined: &[u8], candidate_bound: Vec<u8>) -> Vec<u8> {
    if candidate_bound.as_slice() > examined {
        candidate_bound
    } else {
        let mut succ = examined.to_vec();
        succ.push(0);
        succ
    }
}

impl<C: SkipCursor> Iterator for SkipWalk<C> {
    type Item = Result<Tuple>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.next_bound.as_slice() >= self.upper.as_slice() {
                return None;
            }
            let (k, v) = match self.cursor.seek(&self.next_bound) {
                None => return None,
                Some(Err(e)) => return Some(Err(e)),
                Some(Ok(kv)) => kv,
            };
            let polarity = match claim_polarity_of_value(&v) {
                Ok(p) => p,
                Err(e) => return Some(Err(e)),
            };
            let (ret, nxt_bound) = match check_key_for_bitemporal(&k, polarity, self.as_of, None) {
                Ok(pair) => pair,
                Err(e) => return Some(Err(e)),
            };
            self.next_bound = advance_past(&k, nxt_bound);
            if let Some(mut tup) = ret {
                if let Err(e) = extend_tuple_from_bitemporal_v(&mut tup, &v) {
                    return Some(Err(e));
                }
                return Some(Ok(tup));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::cmp::Reverse;
    use std::collections::BTreeMap;
    use std::ops::Bound;

    use super::*;
    use crate::data::bitemporal::ClaimPolarity;
    use crate::data::tuple::{RelationId, TupleT};
    use crate::data::value::{DataValue, Validity, ValidityTs};

    const REL: RelationId = RelationId(9);

    /// `advance_past` as its own pinned law: a tie, a regression, and a
    /// genuine advance, plus the empty-key edge — independent of whether
    /// `check_key_for_bitemporal` can ever hand the driver a non-advancing
    /// bound (see the function's doc comment for why that unreachability
    /// is a fact about today's kernel, not something this guard gets to
    /// assume away).
    #[test]
    fn advance_past_falls_back_to_byte_successor_when_bound_does_not_advance() {
        assert_eq!(
            advance_past(b"abc", b"abc".to_vec()),
            b"abc\x00".to_vec(),
            "tie"
        );
        assert_eq!(
            advance_past(b"abc", b"aaa".to_vec()),
            b"abc\x00".to_vec(),
            "regression"
        );
        assert_eq!(
            advance_past(b"abc", b"abd".to_vec()),
            b"abd".to_vec(),
            "genuine advance is trusted as-is"
        );
        assert_eq!(advance_past(b"", Vec::new()), vec![0], "empty key edge");
    }

    /// The proof's own backend: nothing but a `BTreeMap`, ~30 lines,
    /// standing in for fjall/temp/sim so the driver is exercised with NO
    /// dependency on any of the three production backends. `opens` counts
    /// `open_skip_cursor` calls — the counter
    /// `skip_walk_opens_exactly_one_cursor_per_walk` pins to exactly one
    /// per walk, however many version steps the walk takes internally.
    #[derive(Default)]
    struct MapSeek {
        map: BTreeMap<Vec<u8>, Vec<u8>>,
        opens: Cell<usize>,
    }

    struct MapSeekCursor<'a> {
        map: &'a BTreeMap<Vec<u8>, Vec<u8>>,
        upper: Vec<u8>,
    }

    impl SkipCursor for MapSeekCursor<'_> {
        fn seek(&mut self, target: &[u8]) -> Option<Result<(Vec<u8>, Vec<u8>)>> {
            self.map
                .range::<[u8], _>((
                    Bound::Included(target),
                    Bound::Excluded(self.upper.as_slice()),
                ))
                .next()
                .map(|(k, v)| Ok((k.clone(), v.clone())))
        }
    }

    impl SkipSeek for MapSeek {
        type Cursor<'c> = MapSeekCursor<'c>;

        fn open_skip_cursor<'c>(&'c self, _lower: &[u8], upper: &[u8]) -> Self::Cursor<'c> {
            self.opens.set(self.opens.get() + 1);
            MapSeekCursor {
                map: &self.map,
                upper: upper.to_vec(),
            }
        }
    }

    fn vts(t: i64) -> ValidityTs {
        ValidityTs::from_raw(t)
    }

    fn slot(t: i64) -> Validity {
        Validity {
            timestamp: vts(t),
            is_assert: Reverse(true),
        }
    }

    /// A bitemporal key: `[int fact, valid(ts), sys(ts)]` under `REL`,
    /// slot flags pinned to assert (polarity rides in the value) — the
    /// same shape `data/bitemporal.rs`'s own tests and `storage/temp.rs`'s
    /// `bk` build.
    fn bikey(fact: i64, valid_ts: i64, sys_ts: i64) -> Vec<u8> {
        vec![
            DataValue::from(fact),
            DataValue::Validity(slot(valid_ts)),
            DataValue::Validity(slot(sys_ts)),
        ]
        .encode_as_key(REL)
        .into_vec()
    }

    fn bval(polarity: ClaimPolarity) -> Vec<u8> {
        let mut v = REL.raw_encode().to_vec();
        v.push(polarity.encode());
        v
    }

    fn rel_bounds() -> (Vec<u8>, Vec<u8>) {
        (
            Tuple::default().encode_as_key(REL).into_vec(),
            (REL.0 + 1).to_be_bytes().to_vec(),
        )
    }

    fn facts_of(tuples: &[Tuple]) -> Vec<i64> {
        tuples
            .iter()
            .map(|t| match &t[0] {
                DataValue::Num(crate::data::value::Num::Int(i)) => *i,
                other => panic!("non-integer fact column: {other:?}"),
            })
            .collect()
    }

    fn walk(store: &MapSeek, sys_at: i64, valid_at: i64) -> Result<Vec<Tuple>> {
        let (lo, hi) = rel_bounds();
        let as_of = AsOf::at(vts(sys_at), vts(valid_at));
        let cursor = store.open_skip_cursor(&lo, &hi);
        SkipWalk::new(cursor, &lo, &hi, as_of).take(1000).collect()
    }

    /// The independent reference model: for each fact, walk its stored
    /// instants newest-to-oldest (`<= valid_at`), and within the first
    /// instant that has ANY version at `<= sys_at`, take that instant's
    /// NEWEST such version's polarity as the verdict (Assert = present,
    /// Retract = absent-settled, Erase/none = fall through older). Written
    /// completely independently of `SkipWalk`/`check_key_for_bitemporal` —
    /// it never seeks, splices, or byte-compares a key; it re-derives the
    /// bitemporal resolution rule from the (fact, valid, sys, polarity)
    /// tuples directly, the same brute-force discipline
    /// `data/bitemporal.rs::oracle` and `query/laws.rs` both use for the
    /// same reason: an oracle that shares the kernel's algorithm proves
    /// nothing about the kernel.
    fn oracle(rows: &[(i64, i64, i64, ClaimPolarity)], sys_at: i64, valid_at: i64) -> Vec<i64> {
        let mut facts: Vec<i64> = rows.iter().map(|r| r.0).collect();
        facts.sort_unstable();
        facts.dedup();
        let mut out = vec![];
        for f in facts {
            let mut instants: Vec<i64> = rows
                .iter()
                .filter(|r| r.0 == f && r.1 <= valid_at)
                .map(|r| r.1)
                .collect();
            instants.sort_unstable();
            instants.dedup();
            let mut verdict = None;
            for instant in instants.into_iter().rev() {
                let governing = rows
                    .iter()
                    .filter(|r| r.0 == f && r.1 == instant && r.2 <= sys_at)
                    .max_by_key(|r| r.2)
                    .map(|r| r.3);
                match governing {
                    Some(ClaimPolarity::Assert) => {
                        verdict = Some(true);
                        break;
                    }
                    Some(ClaimPolarity::Retract) => {
                        verdict = Some(false);
                        break;
                    }
                    Some(ClaimPolarity::Erase) | None => {}
                }
            }
            if verdict == Some(true) {
                out.push(f);
            }
        }
        out
    }

    /// The proof, standalone: 2000 seeded histories, driven through the
    /// PRODUCTION `SkipWalk` over the test-only `MapSeek` backend, judged
    /// against the from-scratch `oracle` above — both axes, negative
    /// coordinates, assert/retract/erase, mirroring
    /// `bitemporal_skip_scan_matches_oracle`'s discipline (issue #78's
    /// dictation: this driver IS #79's first theorem, so the property is
    /// stated once here and every backend inherits it).
    #[test]
    fn skip_walk_matches_independent_oracle_over_2000_seeded_histories() {
        let mut state: u64 = 0x5EED_9E52_5E15_C0DE;
        let mut next = move |m: usize| -> usize {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) as usize) % m
        };
        let valids = [-30i64, -10, -3, 0, 10, 20, 30];
        let syss = [-25i64, -5, 0, 5, 15, 25];
        for _case in 0..2000 {
            let n_rows = 1 + next(10);
            let mut rows: Vec<(i64, i64, i64, ClaimPolarity)> = vec![];
            for _ in 0..n_rows {
                rows.push((
                    next(3) as i64,
                    valids[next(valids.len())],
                    syss[next(syss.len())],
                    [
                        ClaimPolarity::Assert,
                        ClaimPolarity::Retract,
                        ClaimPolarity::Erase,
                    ][next(3)],
                ));
            }
            rows.sort_unstable_by_key(|r| (r.0, r.1, r.2));
            rows.dedup_by_key(|r| (r.0, r.1, r.2));
            let mut store = MapSeek::default();
            for (f, v, s, p) in &rows {
                store.map.insert(bikey(*f, *v, *s), bval(*p));
            }
            for sys_at in [-40i64, -25, -5, 0, 5, 15, 25, 40] {
                for valid_at in [-40i64, -30, -10, -3, 0, 10, 20, 30, 40] {
                    let got = facts_of(&walk(&store, sys_at, valid_at).unwrap());
                    let want = oracle(&rows, sys_at, valid_at);
                    assert_eq!(
                        got, want,
                        "divergence at sys_at={sys_at} valid_at={valid_at} rows={rows:?}"
                    );
                }
            }
        }
    }

    /// Corruption refusal: hostile bytes surface a typed `Err` and the
    /// walk terminates (never hangs, never panics) — planted alongside
    /// honest rows so the walk must both skip past good data and stop
    /// cleanly at bad data. Mirrors `storage/temp.rs`'s
    /// `three_way_differential_skip_scan` hostile fixtures.
    #[test]
    fn skip_walk_refuses_corrupt_bytes_and_terminates() {
        let (lower, upper) = rel_bounds();
        let hostile_short: Vec<u8> = [&REL.0.to_be_bytes()[..], &[0x41, 0x42, 0x43]].concat();
        let mut hostile_sys_tag = bikey(5, 100, 1);
        let n = hostile_sys_tag.len();
        hostile_sys_tag[n - 10] = 0xFE;
        let mut unknown_polarity = REL.raw_encode().to_vec();
        unknown_polarity.push(0xEE);

        let scenarios: Vec<Vec<(Vec<u8>, Vec<u8>)>> = vec![
            vec![
                (bikey(1, 10, 1), bval(ClaimPolarity::Assert)),
                (hostile_short.clone(), bval(ClaimPolarity::Assert)),
                (bikey(9, 10, 1), bval(ClaimPolarity::Assert)),
            ],
            vec![
                (bikey(1, 10, 1), bval(ClaimPolarity::Assert)),
                (hostile_sys_tag.clone(), bval(ClaimPolarity::Assert)),
                (bikey(9, 10, 1), bval(ClaimPolarity::Assert)),
            ],
            vec![
                (bikey(1, 10, 1), unknown_polarity.clone()),
                (bikey(9, 10, 1), bval(ClaimPolarity::Assert)),
            ],
            vec![
                (bikey(1, 10, 1), vec![]), // missing polarity byte entirely
                (bikey(9, 10, 1), bval(ClaimPolarity::Assert)),
            ],
        ];
        for rows in &scenarios {
            let mut store = MapSeek::default();
            for (k, v) in rows {
                store.map.insert(k.clone(), v.clone());
            }
            let as_of = AsOf::current(vts(50));
            let cursor = store.open_skip_cursor(&lower, &upper);
            let mut w = SkipWalk::new(cursor, &lower, &upper, as_of);
            let mut saw_err = false;
            for _ in 0..1000 {
                match w.next() {
                    None => break,
                    Some(Err(_)) => {
                        saw_err = true;
                        break;
                    }
                    Some(Ok(_)) => {}
                }
            }
            assert!(saw_err, "hostile bytes must surface as a typed Err");
            // Polling again re-yields (does not silently move past) the error.
            assert!(w.next().unwrap().is_err());
        }
    }

    /// Extreme stored instants (`i64::MIN` in both slots, adjacent to the
    /// `TERMINAL_VALIDITY` seek sentinel) still terminate — the `.take`
    /// cap in `walk`/the explicit loop bound above is the mutation-tested
    /// backstop for the strict-advance guard; this pins the honest-bytes
    /// boundary case the fuzz-shaped test above samples only by chance.
    #[test]
    fn skip_walk_terminates_on_min_ts_retraction() {
        let mut store = MapSeek::default();
        store
            .map
            .insert(bikey(1, 5, 1), bval(ClaimPolarity::Assert));
        store
            .map
            .insert(bikey(9, i64::MIN, i64::MIN), bval(ClaimPolarity::Retract));
        let got = facts_of(&walk(&store, i64::MAX, 10).unwrap());
        assert_eq!(got, vec![1]);
    }

    /// Degenerate bounds (inverted, equal) are empty, never a panic — and
    /// never even reach the cursor: `SkipWalk::next`'s own loop guard
    /// returns `None` before calling `seek` when `next_bound >= upper`.
    #[test]
    fn skip_walk_degenerate_bounds_are_empty() {
        let mut store = MapSeek::default();
        store
            .map
            .insert(bikey(1, 5, 1), bval(ClaimPolarity::Assert));
        let (lo, hi) = rel_bounds();
        let as_of = AsOf::current(vts(10));
        assert_eq!(
            SkipWalk::new(store.open_skip_cursor(&hi, &lo), &hi, &lo, as_of).count(),
            0
        );
        assert_eq!(
            SkipWalk::new(store.open_skip_cursor(&lo, &lo), &lo, &lo, as_of).count(),
            0
        );
    }

    /// The law this story exists to prove: however many version steps a
    /// walk takes internally, it opens exactly ONE cursor. This is the
    /// structural guarantee — `SkipWalk::next` has no path back to
    /// `open_skip_cursor`, only to `cursor.seek` — pinned against
    /// regression: a hundred facts, ten stacked versions each, driven
    /// through to exhaustion, with the open counter checked before AND
    /// after the walk runs.
    #[test]
    fn skip_walk_opens_exactly_one_cursor_per_walk() {
        let mut store = MapSeek::default();
        for f in 0..100i64 {
            for v in 0..10i64 {
                store
                    .map
                    .insert(bikey(f, v, v), bval(ClaimPolarity::Assert));
            }
        }
        let (lo, hi) = rel_bounds();
        let cursor = store.open_skip_cursor(&lo, &hi);
        assert_eq!(store.opens.get(), 1, "opening the walk's cursor");

        let as_of = AsOf::current(vts(1_000));
        let results: Vec<_> = SkipWalk::new(cursor, &lo, &hi, as_of)
            .collect::<Result<Vec<_>>>()
            .unwrap();

        assert_eq!(facts_of(&results).len(), 100, "every fact resolved");
        assert_eq!(
            store.opens.get(),
            1,
            "the walk drove ONE cursor across all 100 facts' version steps, never reopened"
        );
    }
}
