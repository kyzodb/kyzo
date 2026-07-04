/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The bitemporal skip-scan walk — ONE implementation, generic over the
//! backend's seek primitive, replacing the three hand-copied seek loops
//! this story found byte-for-byte identical in `storage/fjall.rs`
//! (`SkipIterator`, lines 552-610: `raw_range(...).next()` per step),
//! `storage/temp.rs` (`range_skip_scan_tuple`'s closure, lines 157-205:
//! `self.map.range(...).next()` per step), and `storage/sim.rs`
//! (`SimSkipIter`, lines 884-935: `self.tx.range_scan(...).next()` per
//! step, boxed and dyn-dispatched through `ReadTx`). All three walk the
//! same algorithm over `data::bitemporal::check_key_for_bitemporal` (the
//! resolution kernel this module never reimplements, only calls): seek to
//! the next candidate key, peek its polarity from its value, let the
//! kernel decide (emit or not) and hand back the next seek bound, then
//! advance strictly past the key just examined — a corrupt key or value
//! surfaces as `Err` WITHOUT advancing, so a scan cannot step over bytes
//! it could not judge.
//!
//! ## The seek seam
//!
//! ```text
//! pub(crate) trait SkipSeek {
//!     fn seek_first(&self, lower: &[u8], upper: &[u8])
//!         -> Option<Result<(Vec<u8>, Vec<u8>)>>;
//! }
//! ```
//!
//! "Seek to the first key at or after `lower` within `[lower, upper)` and
//! return it, or `None` if the range is empty." That is the whole
//! contract — deliberately not "return an iterator": every one of the
//! three copies above calls `.next()` on a fresh range and immediately
//! drops it (`raw_range(...).next()`; `range.next()` then `drop(range)`
//! in fjall's `SkipIterator::next`; `range.next()` then `drop(range)` in
//! sim's `SimSkipIter::next`) — the walk NEVER consumes a second item
//! from one seek. Shrinking the seam to that single-item return, instead
//! of an associated iterator type, is why this is zero-cost with no `dyn`
//! anywhere on the hot path:
//!
//! - **No allocation beyond what each backend already pays.** The method
//!   returns `Option<Result<(Vec<u8>, Vec<u8>)>>` by value — the SAME two
//!   `Vec<u8>` clones every existing copy already makes turning a borrowed
//!   key/value pair into owned bytes (fjall's `raw_range` calls
//!   `guard.into_inner()` then `.to_vec()` twice; temp's closure calls
//!   `.clone()` twice on the `BTreeMap` entry; sim's `map_range` clones
//!   twice). No new heap traffic is introduced — the seam is a pass
//!   through, not a wrapper.
//! - **No `dyn` dispatch.** `SkipWalk<'a, S: SkipSeek + ?Sized>` is
//!   generic over the seam, so each backend's `seek_first` monomorphizes
//!   inline into the walk's `next()` — never boxed, never vtable-called.
//!   This is a real fix, not a paper one: sim's CURRENT `SimSkipIter`
//!   already calls through `self.tx.range_scan(...)`, which returns
//!   `Box<dyn Iterator<...>>` — a heap allocation AND a dynamic dispatch
//!   on every seek step. Phase 2 retires that box by giving `SimReadTx`/
//!   `SimWriteTx` a direct `SkipSeek` impl that talks to their native
//!   `map_range`/`visible_lazy` cursors, exactly as `SkipSeek::seek_first`
//!   requires, with no detour through the boxed `ReadTx::range_scan`.
//! - **No named associated-iterator type needed.** A GAT or
//!   return-position-impl-trait-in-traits (RPITIT, stable on this
//!   toolchain — `rustc 1.96.1`, workspace MSRV 1.93, edition 2024 — see
//!   `Cargo.toml`) would ALSO have worked and would ALSO be zero-cost, but
//!   would force every implementor to name or infer a concrete iterator
//!   type for a value the walk never iterates past its first item. The
//!   single-shot `Option` return is the leaner seam for the actual shape
//!   of the algorithm; nothing here forecloses widening it later if a
//!   consumer legitimately needs to iterate more than one step's worth of
//!   one seek (none of the three backends do, and the whole point of a
//!   skip scan is that most candidates are skipped, not iterated).
//!
//! ## The walk (`SkipWalk`)
//!
//! `SkipWalk<'a, S: SkipSeek + ?Sized>` holds a borrowed seam, the
//! (fixed) upper bound, the `AsOf` coordinate, and the mutable re-seek
//! bound. Each `next()` call:
//!
//! 1. Exits if the seek bound has reached (or passed) the upper bound —
//!    the loop's own termination for an exhausted range.
//! 2. Calls `seek.seek_first(&next_bound, &upper)`. `None` ends the scan;
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
//! ## Phase-2 map (mechanical swap per backend)
//!
//! **`storage/fjall.rs`**: delete `SkipIterator` (lines 552-610) entirely.
//! In the `impl_read_tx!` macro, add
//! `impl SkipSeek for $ty { fn seek_first(&self, lower, upper) { raw_range(&self.$reader, &self.ks, lower, upper).next() } }`
//! (one impl per macro expansion, i.e. for both `FjallReadTx` and
//! `FjallWriteTx`, reusing the existing `raw_range` helper unchanged).
//! Replace the `range_skip_scan_tuple` body (lines 463-476) with
//! `Box::new(SkipWalk::new(self, lower, upper, as_of))`.
//!
//! **`storage/temp.rs`**: delete the `range_skip_scan_tuple` closure body
//! (lines 157-205). Add
//! `impl SkipSeek for TempTx { fn seek_first(&self, lower, upper) { if lower >= upper { return None } self.map.range::<[u8],_>((Included(lower), Excluded(upper))).next().map(|(k,v)| Ok((k.clone(), v.clone()))) } }`.
//! Replace the method body with `Box::new(SkipWalk::new(self, lower, upper, as_of))`.
//!
//! **`storage/sim.rs`**: delete `SimSkipIter` (lines 884-935) and its two
//! call sites (`SimReadTx::range_skip_scan_tuple`, lines 975-987;
//! `SimWriteTx::range_skip_scan_tuple`, lines 1042-1054). Add, per
//! species:
//! - `impl SkipSeek for SimReadTx`: `ctx.yield_turn()`, then
//!   `ctx.check_read_fault(op_identity(TAG_RANGE, &[lower, upper]))` —
//!   returning `Some(Err(e))` on a fault hit — then
//!   `map_range(&self.snapshot, lower, Some(upper)).next().map(|(k,v)| Ok((k.clone(), v.clone())))`.
//!   This is the one backend where the seam does MORE than a bare range
//!   probe: it inlines exactly what `range_scan` already does per call
//!   (yield to the scheduler, roll the read-fault die) so the skip walk
//!   keeps participating in DST scheduling and fault injection PER SEEK
//!   STEP — losing that would silently narrow the fault surface the sim
//!   exists to stress — while no longer boxing through `range_scan`
//!   itself.
//! - `impl SkipSeek for SimWriteTx`: same shape plus `track_range(lower,
//!   Some(upper))` before the fault check (conservative per-step range
//!   tracking, matching the contract's documented "one per seek step"
//!   coarse tracking for as-of scans inside write transactions), then
//!   `self.visible_lazy(lower, Some(upper)).next().map(Ok)`.
//!
//! In every backend the swap is purely subtractive plus one small trait
//! impl: the walk itself, the termination guard, and the kernel call are
//! never rewritten again.

use miette::Result;

use crate::data::bitemporal::{
    check_key_for_bitemporal, claim_polarity_of_value, extend_tuple_from_bitemporal_v,
};
use crate::data::tuple::Tuple;
use crate::data::value::AsOf;

/// The seek seam a backend implements: "seek to the first key at or after
/// `lower`, within the half-open range `[lower, upper)`, and return it —
/// or `None` if the range holds nothing." See the module doc for why this
/// is the whole contract (never a full iterator) and why that shape is
/// what makes [`SkipWalk`] zero-cost over every backend.
///
/// Degenerate bounds (`lower >= upper`) MUST answer `None` — the storage
/// contract's empty-range rule applies here exactly as it does to
/// `ReadTx::range_scan`.
pub(crate) trait SkipSeek {
    fn seek_first(&self, lower: &[u8], upper: &[u8]) -> Option<Result<(Vec<u8>, Vec<u8>)>>;
}

/// THE bitemporal skip-scan walk: generic over one backend's [`SkipSeek`],
/// so every implementor inherits this algorithm — and, per issue #78's
/// dictation, the property proof over it — verbatim. See the module doc
/// for the step-by-step algorithm and the termination guard's rationale.
pub(crate) struct SkipWalk<'a, S: SkipSeek + ?Sized> {
    seek: &'a S,
    upper: Vec<u8>,
    as_of: AsOf,
    next_bound: Vec<u8>,
}

impl<'a, S: SkipSeek + ?Sized> SkipWalk<'a, S> {
    pub(crate) fn new(seek: &'a S, lower: &[u8], upper: &[u8], as_of: AsOf) -> Self {
        SkipWalk {
            seek,
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

impl<S: SkipSeek + ?Sized> Iterator for SkipWalk<'_, S> {
    type Item = Result<Tuple>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.next_bound.as_slice() >= self.upper.as_slice() {
                return None;
            }
            let (k, v) = match self.seek.seek_first(&self.next_bound, &self.upper) {
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

    /// The proof's own backend: nothing but a `BTreeMap`, ~20 lines,
    /// standing in for fjall/temp/sim so the driver is exercised with NO
    /// dependency on any of the three production backends this story will
    /// port onto it in phase 2.
    #[derive(Default)]
    struct MapSeek(BTreeMap<Vec<u8>, Vec<u8>>);

    impl SkipSeek for MapSeek {
        fn seek_first(&self, lower: &[u8], upper: &[u8]) -> Option<Result<(Vec<u8>, Vec<u8>)>> {
            if lower >= upper {
                return None;
            }
            self.0
                .range::<[u8], _>((Bound::Included(lower), Bound::Excluded(upper)))
                .next()
                .map(|(k, v)| Ok((k.clone(), v.clone())))
        }
    }

    fn vts(t: i64) -> ValidityTs {
        ValidityTs(Reverse(t))
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
        SkipWalk::new(store, &lo, &hi, as_of).take(1000).collect()
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
                store.0.insert(bikey(*f, *v, *s), bval(*p));
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
                store.0.insert(k.clone(), v.clone());
            }
            let as_of = AsOf::current(vts(50));
            let mut w = SkipWalk::new(&store, &lower, &upper, as_of);
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
        store.0.insert(bikey(1, 5, 1), bval(ClaimPolarity::Assert));
        store
            .0
            .insert(bikey(9, i64::MIN, i64::MIN), bval(ClaimPolarity::Retract));
        let got = facts_of(&walk(&store, i64::MAX, 10).unwrap());
        assert_eq!(got, vec![1]);
    }

    /// Degenerate bounds (inverted, equal) are empty, never a panic.
    #[test]
    fn skip_walk_degenerate_bounds_are_empty() {
        let mut store = MapSeek::default();
        store.0.insert(bikey(1, 5, 1), bval(ClaimPolarity::Assert));
        let (lo, hi) = rel_bounds();
        let as_of = AsOf::current(vts(10));
        assert_eq!(SkipWalk::new(&store, &hi, &lo, as_of).count(), 0);
        assert_eq!(SkipWalk::new(&store, &lo, &lo, as_of).count(), 0);
    }
}
