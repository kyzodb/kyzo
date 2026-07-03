/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Copyright 2026, The KyzoDB Authors. Modified from the CozoDB original
 * (`storage/temp.rs`, MPL-2.0), re-architected for the KyzoDB kernel:
 *
 * - The original was a `Storage` backend (`TempStorage = Arc<MemStorage>`)
 *   shared by every session of one `Db`, handing out transactions over a
 *   `ShardedLock<BTreeMap>`. Here the temp store is a plain [`ReadTx`]/
 *   [`WriteTx`] species over a `BTreeMap`, meant to be owned inline by a
 *   `SessionTx`: no lock, no sharing, no `Arc` — a session is
 *   single-threaded by construction, so the interior synchronization had
 *   nothing left to guard.
 * - It implements the kernel's [`ReadTx`]/[`WriteTx`] contract directly, so
 *   the catalog functions and `RelationHandle`'s scan surface
 *   (`runtime/relation.rs`) can route to it unchanged once the router that
 *   reaches it lands.
 * - Law 5: the skip-scan (time travel over temp relations) is the same
 *   seek loop as the fjall backend's, with the same strict-advance
 *   guarantee on the Ok path (see the method doc for the Err caveat);
 *   nothing here panics on stored bytes, and degenerate bounds are empty.
 * - The original's `del_range` deferred deletion by storing the bounds;
 *   here it deletes eagerly (a session-local BTreeMap needs no deferral).
 *
 * WHAT THIS SPECIES IS, THIS TIER (verified, not aspirational):
 * `TempTx` is the storage species for the *coming* session router — the
 * transaction type temporary (`_`-prefixed) relations will live in once
 * multi-script sessions outlive a single script. It is proven at the
 * storage-species level: the `tests` module below runs a three-way
 * differential against the fjall backend and the `sim` model over seeded
 * op streams (identical answers on hostile keys, extreme timestamps, and
 * degenerate bounds), plus targeted oracles a mutation campaign confirms
 * kill the strict-advance fallback, the degenerate-bounds guards,
 * inclusive re-seek at an exact query timestamp, last-write-wins,
 * half-open ranges, and as-of plumbing.
 *
 * It is NOT reachable from the public API this tier: every route in is a
 * TYPED REFUSAL, not a silent misplacement (both verified end-to-end in
 * `runtime/db_battery.rs`):
 *   - a temp *mutation* (`:create _t`, `:put _t`, …) is refused by `db.rs`
 *     with `TempRelationNotReachableError` — "temp relation '_t' cannot be
 *     stored to yet: sessions do not outlive a script";
 *   - a temp *read* (`*_t[a]`) is resolved by the compile/eval scan seam
 *     against the persistent catalog only, so it errors
 *     `StoredRelationNotFoundError` — "Cannot find requested stored
 *     relation '_t'".
 * No production path constructs a `TempTx` that holds data; the `tests`
 * module is its only instantiator. The file ships sealed so the router can
 * adopt it later without a format migration, not because the feature is
 * live.
 *
 * LANDING NOTE (wiring): this file homes at `kyzo-core/src/storage/temp.rs`
 * and needs one line in `storage/mod.rs`'s `sealed` module:
 * `impl Sealed for super::temp::TempTx {}` — the same in-crate admission
 * the sealing comment already anticipates for engine-internal stores.
 */

//! The session's scratch store: the transaction species that temporary
//! (`_`-prefixed) relations will live in.
//!
//! A temp relation is a fact a session is *entertaining*, not one the
//! universe has committed to. [`TempTx`] gives such facts the same shape a
//! stored relation has — same key encoding, same [`ReadTx`]/[`WriteTx`]
//! contract, same scan and skip-scan surface — over an in-memory
//! `BTreeMap`, so the catalog and mutation pipeline can route to it by the
//! relation's name (`Symbol::is_temp_relation_name`) and its handle's
//! `is_temp` flag once the session router that reaches it lands. Until then
//! it is unreachable through the public API by typed refusal (see the
//! module header for the exact errors a temp read and a temp write
//! produce); what is proven here is the storage species itself, by
//! differential and mutation (`tests`).

use std::collections::BTreeMap;
use std::ops::Bound;

use miette::Result;

use crate::data::tuple::{Tuple, check_key_for_validity, extend_tuple_from_v};
use crate::data::value::ValidityTs;
use crate::storage::{ReadTx, WriteTx};

/// One session's temp keyspace: an ordered map with the kernel's
/// transaction interface. "Transaction" is honorary — the map IS the
/// session's private state, so reads are trivially consistent, writes are
/// immediate, and `commit` is vacuous (the session's life is the
/// transaction).
#[derive(Debug, Default)]
pub(crate) struct TempTx {
    map: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl TempTx {
    /// Whether nothing has ever been written (used by tests/diagnostics).
    pub(crate) fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl ReadTx for TempTx {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.map.get(key).cloned())
    }

    fn exists(&self, key: &[u8]) -> Result<bool> {
        Ok(self.map.contains_key(key))
    }

    fn range_scan<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a> {
        if lower >= upper {
            // Degenerate bounds denote the empty interval (the kernel
            // contract; BTreeMap::range would panic on start > end).
            return Box::new(std::iter::empty());
        }
        Box::new(
            self.map
                .range::<[u8], _>((Bound::Included(lower), Bound::Excluded(upper)))
                .map(|(k, v)| Ok((k.clone(), v.clone()))),
        )
    }

    /// The same seek loop as the fjall backend's `SkipIterator`, over the
    /// map: examine the next candidate, let `check_key_for_validity` decide
    /// and give the re-seek bound, then on the Ok path advance strictly past
    /// the examined key — with a byte-successor fallback for when the
    /// returned bound would not move (the `TERMINAL_VALIDITY` collision at
    /// `ts == i64::MIN`) — so hostile stored bytes cannot livelock the scan.
    /// A `check_key_for_validity` Err is surfaced WITHOUT advancing
    /// `next_bound` (parity with the fjall loop); engine callers stop at the
    /// first Err, so re-examining the same key never arises in practice.
    fn range_skip_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
        valid_at: ValidityTs,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        let mut next_bound = lower.to_vec();
        let upper = upper.to_vec();
        Box::new(std::iter::from_fn(move || {
            loop {
                if next_bound.as_slice() >= upper.as_slice() {
                    return None;
                }
                let (k, v) = match self
                    .map
                    .range::<[u8], _>((
                        Bound::Included(next_bound.as_slice()),
                        Bound::Excluded(upper.as_slice()),
                    ))
                    .next()
                {
                    None => return None,
                    Some((k, v)) => (k.clone(), v.clone()),
                };
                let (ret, nxt_bound) = match check_key_for_validity(&k, valid_at, None) {
                    Ok(pair) => pair,
                    Err(e) => return Some(Err(e)),
                };
                // Strict advance past the examined key, whatever the bytes.
                next_bound = if nxt_bound.as_slice() > k.as_slice() {
                    nxt_bound
                } else {
                    let mut succ = k;
                    succ.push(0);
                    succ
                };
                if let Some(mut tup) = ret {
                    if let Err(e) = extend_tuple_from_v(&mut tup, &v) {
                        return Some(Err(e));
                    }
                    return Some(Ok(tup));
                }
            }
        }))
    }

    fn total_scan<'a>(&'a self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a> {
        Box::new(self.map.iter().map(|(k, v)| Ok((k.clone(), v.clone()))))
    }
}

impl WriteTx for TempTx {
    fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        self.map.insert(key.to_vec(), val.to_vec());
        Ok(())
    }

    fn del(&mut self, key: &[u8]) -> Result<()> {
        self.map.remove(key);
        Ok(())
    }

    fn del_range(&mut self, lower: &[u8], upper: &[u8]) -> Result<()> {
        if lower >= upper {
            return Ok(()); // the kernel's degenerate-bounds contract
        }
        let doomed: Vec<Vec<u8>> = self
            .map
            .range::<[u8], _>((Bound::Included(lower), Bound::Excluded(upper)))
            .map(|(k, _)| k.clone())
            .collect();
        for k in doomed {
            self.map.remove(&k);
        }
        Ok(())
    }

    /// Vacuous: a temp store's transaction IS the session's lifetime. The
    /// method exists because the species contract requires it; consuming
    /// self here would only ever be called by generic code that is about
    /// to drop the store anyway.
    fn commit(self) -> Result<()> {
        Ok(())
    }

    fn commit_durable(self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Reverse;

    use super::*;
    use crate::data::tuple::{RelationId, TupleT};
    use crate::data::value::{DataValue, Validity};

    const REL: RelationId = RelationId(7);

    /// A time-travel key: `[int x, validity(ts, assert)]` under `REL`.
    fn vk(x: i64, ts: i64, assert: bool) -> Vec<u8> {
        vec![
            DataValue::from(x),
            DataValue::Validity(Validity {
                timestamp: ValidityTs(Reverse(ts)),
                is_assert: Reverse(assert),
            }),
        ]
        .encode_as_key(REL)
        .to_vec()
    }

    /// The half-open byte range covering the whole of `REL`'s keyspace.
    fn rel_bounds() -> (Vec<u8>, Vec<u8>) {
        (
            Tuple::default().encode_as_key(REL).to_vec(),
            (REL.0 + 1).to_be_bytes().to_vec(),
        )
    }

    /// Skip-scan as-of `ts`, projected to `(x, version_ts)` pairs — the
    /// ACTUAL returned tuple values, not a count. `.take` caps emission so a
    /// mutant that emits forever fails fast rather than merely hanging.
    fn scan_at(t: &TempTx, ts: i64) -> Vec<(i64, i64)> {
        let (lo, hi) = rel_bounds();
        t.range_skip_scan_tuple(&lo, &hi, ValidityTs(Reverse(ts)))
            .take(1000)
            .map(|r| {
                let tup = r.expect("engine-shaped rows decode cleanly");
                let x = tup[0].get_int().expect("int key column");
                let version_ts = match &tup[1] {
                    DataValue::Validity(v) => v.timestamp.0.0,
                    other => panic!("expected a trailing validity, got {other:?}"),
                };
                (x, version_ts)
            })
            .collect()
    }

    #[test]
    fn basic_kv_and_ranges() {
        let mut t = TempTx::default();
        assert!(t.is_empty());
        t.put(b"a", b"1").unwrap();
        t.put(b"b", b"2").unwrap();
        t.put(b"c", b"3").unwrap();
        assert_eq!(t.get(b"b").unwrap(), Some(b"2".to_vec()));
        assert!(t.exists(b"a").unwrap());
        // Degenerate ranges are empty, never a panic (law 5).
        assert_eq!(t.range_scan(b"z", b"a").count(), 0);
        t.del_range(b"z", b"a").unwrap();
        let keys: Vec<_> = t.range_scan(b"a", b"c").map(|kv| kv.unwrap().0).collect();
        assert_eq!(keys, vec![b"a".to_vec(), b"b".to_vec()]);
        t.del_range(b"a", b"c").unwrap();
        assert_eq!(t.total_scan().count(), 1);
        t.del(b"c").unwrap();
        assert!(t.is_empty());
    }

    /// `put` is last-write-wins: a second write to a key REPLACES the value.
    /// (Kills the `entry().or_insert_with` mutant that keeps the first.)
    #[test]
    fn put_overwrites_last_write_wins() {
        let mut t = TempTx::default();
        t.put(b"k", b"first").unwrap();
        t.put(b"k", b"second").unwrap();
        assert_eq!(t.get(b"k").unwrap(), Some(b"second".to_vec()));
        let rows: Vec<_> = t.total_scan().map(|kv| kv.unwrap()).collect();
        assert_eq!(rows, vec![(b"k".to_vec(), b"second".to_vec())]);
    }

    /// The skip scan honors validity semantics and its returned VALUES are
    /// asserted (not merely counted): newest version at or before the query
    /// time, retractions are honest absences. Stored values are EMPTY — the
    /// shape the engine actually writes for a validity relation (the earlier
    /// draft stored `[0u8; 8]`, a shape `extend_tuple_from_v` rejects, and
    /// asserted a `.count()` that cannot tell an `Ok` tuple from a decode
    /// `Err`). Querying at different timestamps returns different rows, so a
    /// mutant that ignores `valid_at` dies here.
    #[test]
    fn skip_scan_honors_validity_with_asserted_values() {
        let mut t = TempTx::default();
        for k in [vk(1, 10, true), vk(1, 20, false), vk(2, 8, true)] {
            t.put(&k, b"").unwrap();
        }
        assert_eq!(scan_at(&t, 5), vec![], "before any assertion");
        assert_eq!(scan_at(&t, 9), vec![(2, 8)], "only tuple 2 asserted yet");
        assert_eq!(
            scan_at(&t, 15),
            vec![(1, 10), (2, 8)],
            "both live; newest version of 1 at or before 15 is ts=10"
        );
        assert_eq!(scan_at(&t, 25), vec![(2, 8)], "tuple 1 retracted at 20");
    }

    /// The re-seek bound is INCLUSIVE: a version whose timestamp exactly
    /// equals the query time is returned, not skipped. (Kills the
    /// `Bound::Included -> Excluded` re-seek mutant, a silent wrong answer.)
    #[test]
    fn skip_scan_returns_version_at_exact_query_ts() {
        let mut t = TempTx::default();
        for k in [vk(1, 20, true), vk(1, 10, true)] {
            t.put(&k, b"").unwrap();
        }
        // ts=10: the first candidate (ts=20) is in the future, so the loop
        // re-seeks to exactly `valid(10)` — which must land on the ts=10 key.
        assert_eq!(
            scan_at(&t, 10),
            vec![(1, 10)],
            "version exactly at the query ts must be the answer"
        );
        assert_eq!(scan_at(&t, 20), vec![(1, 20)], "newest at 20");
        assert_eq!(scan_at(&t, 15), vec![(1, 10)], "newest at or before 15");
    }

    /// A retraction stored at `ts == i64::MIN` has validity equal to the
    /// `TERMINAL_VALIDITY` seek sentinel: without the byte-successor
    /// fallback the re-seek bound never advances and the scan livelocks. The
    /// honest row must return and the scan must TERMINATE. (Kills the
    /// strict-advance mutant — by timeout, the same way `storage/tests.rs`
    /// pins the fjall backend.)
    #[test]
    fn skip_scan_terminates_on_min_ts_retraction() {
        let mut t = TempTx::default();
        t.put(&vk(1, 5, true), b"").unwrap();
        t.put(&vk(9, i64::MIN, false), b"").unwrap();
        assert_eq!(scan_at(&t, 10), vec![(1, 5)]);
    }

    /// Degenerate skip-scan bounds (inverted, equal) denote the empty
    /// interval — never a `BTreeMap` start>end panic. (Kills the removed
    /// upper-guard mutant.)
    #[test]
    fn skip_scan_degenerate_bounds_are_empty() {
        let mut t = TempTx::default();
        t.put(&vk(1, 5, true), b"").unwrap();
        let (lo, hi) = rel_bounds();
        let at = ValidityTs(Reverse(10));
        assert_eq!(t.range_skip_scan_tuple(&hi, &lo, at).count(), 0, "inverted");
        assert_eq!(t.range_skip_scan_tuple(&lo, &lo, at).count(), 0, "equal");
    }

    // ---------- three-way differential (adopted review pin) ----------
    //
    // `TempTx` vs the real fjall backend vs the `sim` DST model, driven by
    // identical seeded op streams and identical time-travel scenarios. Any
    // disagreement is a finding. Sized to run in seconds (the review harness
    // it adopts ran 60x300; this runs 12x120 plus the skip-scan and
    // del_range oracles) while still forcing cross-backend agreement, which
    // independently backstops every mutant above.

    use crate::storage::Storage;
    use crate::storage::fjall::new_fjall_storage;
    use crate::storage::sim::{SimRng, SimStorage};

    const CAP: usize = 10_000;

    /// One observable answer, normalized. Errors compare by presence only
    /// (messages differ per backend).
    #[derive(Debug, PartialEq, Eq)]
    enum Obs {
        Val(Option<Vec<u8>>),
        Flag(bool),
        Rows(Vec<(Vec<u8>, Vec<u8>)>),
        Count(usize),
        Err,
    }

    fn collect_rows(it: Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + '_>) -> Obs {
        let mut rows = vec![];
        for (i, kv) in it.enumerate() {
            assert!(
                i < CAP,
                "scan yielded {CAP}+ items: non-terminating iterator"
            );
            match kv {
                Ok(kv) => rows.push(kv),
                Err(_) => return Obs::Err,
            }
        }
        Obs::Rows(rows)
    }

    /// Skip-scan: Ok tuples until the first Err (errors do not advance the
    /// seek bound in ANY implementation, so we stop there), capped to prove
    /// termination.
    fn collect_skip(it: Box<dyn Iterator<Item = Result<Tuple>> + '_>) -> (Vec<Tuple>, bool) {
        let mut rows = vec![];
        let mut erred = false;
        for (i, t) in it.enumerate() {
            assert!(i < CAP, "skip scan yielded {CAP}+ items: non-terminating");
            match t {
                Ok(t) => rows.push(t),
                Err(_) => {
                    erred = true;
                    break;
                }
            }
        }
        (rows, erred)
    }

    #[derive(Debug, Clone)]
    enum Op {
        Put(Vec<u8>, Vec<u8>),
        Del(Vec<u8>),
        DelRange(Vec<u8>, Vec<u8>),
        Get(Vec<u8>),
        Exists(Vec<u8>),
        Scan(Vec<u8>, Vec<u8>),
        ScanCount(Vec<u8>, Vec<u8>),
        Total,
    }

    /// Keys from an alphabet that straddles type-tag boundaries and shares
    /// prefixes; length 0..=4 (the empty key included).
    fn gen_key(rng: &mut SimRng) -> Vec<u8> {
        const ALPHABET: [u8; 8] = [0x00, 0x01, 0x07, 0x0D, 0x41, 0x42, 0xFE, 0xFF];
        let len = rng.below(5) as usize;
        (0..len)
            .map(|_| ALPHABET[rng.below(ALPHABET.len() as u64) as usize])
            .collect()
    }

    fn gen_val(rng: &mut SimRng) -> Vec<u8> {
        let len = rng.below(12) as usize;
        (0..len).map(|_| (rng.next_u64() & 0xFF) as u8).collect()
    }

    fn gen_op(rng: &mut SimRng) -> Op {
        match rng.below(16) {
            0..=4 => Op::Put(gen_key(rng), gen_val(rng)),
            5..=6 => Op::Del(gen_key(rng)),
            7..=8 => Op::DelRange(gen_key(rng), gen_key(rng)),
            9..=10 => Op::Get(gen_key(rng)),
            11 => Op::Exists(gen_key(rng)),
            12..=13 => Op::Scan(gen_key(rng), gen_key(rng)),
            14 => Op::ScanCount(gen_key(rng), gen_key(rng)),
            _ => Op::Total,
        }
    }

    fn apply<T: WriteTx>(tx: &mut T, op: &Op) -> Vec<Obs> {
        match op {
            Op::Put(k, v) => {
                tx.put(k, v).unwrap();
                vec![]
            }
            Op::Del(k) => {
                tx.del(k).unwrap();
                vec![]
            }
            Op::DelRange(lo, hi) => {
                tx.del_range(lo, hi).unwrap();
                vec![]
            }
            Op::Get(k) => vec![match tx.get(k) {
                Ok(v) => Obs::Val(v),
                Err(_) => Obs::Err,
            }],
            Op::Exists(k) => vec![match tx.exists(k) {
                Ok(b) => Obs::Flag(b),
                Err(_) => Obs::Err,
            }],
            Op::Scan(lo, hi) => vec![collect_rows(tx.range_scan(lo, hi))],
            Op::ScanCount(lo, hi) => vec![match tx.range_count(lo, hi) {
                Ok(n) => Obs::Count(n),
                Err(_) => Obs::Err,
            }],
            Op::Total => vec![collect_rows(tx.total_scan())],
        }
    }

    #[test]
    fn three_way_differential_kv_ops() {
        for seed in 0..12u64 {
            let dir = tempfile::tempdir().unwrap();
            let fjall_store = new_fjall_storage(dir.path().join("d")).unwrap();
            let mut fjall_tx = fjall_store.write_tx().unwrap();
            let sim_store = SimStorage::new(seed);
            let mut sim_tx = sim_store.write_tx().unwrap();
            let mut temp_tx = TempTx::default();

            let mut rng = SimRng::new(seed ^ 0x00D1_FFEE);
            for step in 0..120 {
                let op = gen_op(&mut rng);
                // fjall rejects the empty key at the API level; the engine
                // never writes one (every key carries an 8-byte prefix).
                if let Op::Put(k, _) | Op::Del(k) = &op
                    && k.is_empty()
                {
                    continue;
                }
                let a = apply(&mut temp_tx, &op);
                let b = apply(&mut fjall_tx, &op);
                let c = apply(&mut sim_tx, &op);
                assert_eq!(a, b, "temp vs fjall: seed {seed} step {step} op {op:?}");
                assert_eq!(a, c, "temp vs sim: seed {seed} step {step} op {op:?}");
            }
            let a = collect_rows(temp_tx.total_scan());
            let b = collect_rows(fjall_tx.total_scan());
            let c = collect_rows(sim_tx.total_scan());
            assert_eq!(a, b, "final state temp vs fjall: seed {seed}");
            assert_eq!(a, c, "final state temp vs sim: seed {seed}");
            if let Obs::Rows(rows) = &a {
                for w in rows.windows(2) {
                    assert!(w[0].0 < w[1].0, "total_scan not strictly memcmp-ascending");
                }
            }
        }
    }

    /// Object-safe shim so one loop drives all three write transactions.
    trait DynW {
        fn dput(&mut self, k: &[u8], v: &[u8]);
        fn ddel_range(&mut self, lo: &[u8], hi: &[u8]);
    }
    impl<T: WriteTx> DynW for T {
        fn dput(&mut self, k: &[u8], v: &[u8]) {
            self.put(k, v).unwrap();
        }
        fn ddel_range(&mut self, lo: &[u8], hi: &[u8]) {
            self.del_range(lo, hi).unwrap();
        }
    }

    #[test]
    fn del_range_degenerate_and_own_writes() {
        let dir = tempfile::tempdir().unwrap();
        let fjall_store = new_fjall_storage(dir.path().join("d")).unwrap();
        let sim_store = SimStorage::new(1);
        // (lower, upper): equal, inverted, adjacent-byte, forward, empty.
        let probes: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (b"a".to_vec(), b"a".to_vec()),
            (b"b".to_vec(), b"a".to_vec()),
            (b"a\x00".to_vec(), b"a".to_vec()),
            (b"a".to_vec(), b"a\x00".to_vec()), // forward: kills exactly "a"
            (vec![], vec![]),
            (b"a".to_vec(), vec![]),
            (vec![0xFF, 0xFF], vec![0x00]),
        ];
        for (lo, hi) in &probes {
            let mut fjall_tx = fjall_store.write_tx().unwrap();
            let mut sim_tx = sim_store.write_tx().unwrap();
            let mut temp_tx = TempTx::default();
            for tx in [&mut temp_tx as &mut dyn DynW, &mut fjall_tx, &mut sim_tx] {
                tx.dput(b"a", b"1");
                tx.dput(b"a\x00", b"2");
                tx.dput(b"b", b"3");
                tx.ddel_range(lo, hi);
            }
            let a = collect_rows(temp_tx.total_scan());
            let b = collect_rows(fjall_tx.total_scan());
            let c = collect_rows(sim_tx.total_scan());
            assert_eq!(a, b, "del_range({lo:?},{hi:?}) temp vs fjall");
            assert_eq!(a, c, "del_range({lo:?},{hi:?}) temp vs sim");
            if lo >= hi
                && let Obs::Rows(rows) = &a
            {
                assert_eq!(rows.len(), 3, "degenerate del_range deleted something");
            }
        }
    }

    #[test]
    fn three_way_differential_skip_scan() {
        // Honest versioned rows planted alongside hostile inhabitants:
        // a too-short key, a full-length key whose tail is not a validity,
        // extreme timestamps, and a garbage rmp value (hits only on emit).
        let (lower, upper) = rel_bounds();
        let hostile_short: Vec<u8> = [&REL.0.to_be_bytes()[..], &[0x41, 0x42, 0x43]].concat();
        let mut hostile_tail = vk(5, 100, true);
        let n = hostile_tail.len();
        hostile_tail[n - 10] = 0xFE; // clobber the validity tag
        let scenarios: Vec<(Vec<(Vec<u8>, Vec<u8>)>, &str)> = vec![
            (
                vec![
                    (vk(1, 10, true), vec![]),
                    (vk(1, 20, false), vec![]),
                    (vk(2, i64::MIN, false), vec![]),
                    (vk(2, 15, true), vec![]),
                    (vk(3, i64::MAX, true), vec![]),
                    (vk(4, 0, true), vec![]),
                    (vk(4, 0, false), vec![]),
                ],
                "honest versions + extreme timestamps",
            ),
            (
                vec![
                    (vk(1, 10, true), vec![]),
                    (hostile_short.clone(), vec![]),
                    (vk(9, 10, true), vec![]),
                ],
                "short key planted mid-range",
            ),
            (
                vec![
                    (vk(1, 10, true), vec![]),
                    (hostile_tail.clone(), vec![]),
                    (vk(9, 10, true), vec![]),
                ],
                "garbage validity tag mid-range",
            ),
            (
                vec![
                    (
                        vk(1, 10, true),
                        b"\x00\x00\x00\x00\x00\x00\x00\x00garbage".to_vec(),
                    ),
                    (vk(9, 10, true), vec![]),
                ],
                "garbage rmp value on an emitted hit",
            ),
        ];
        let queries: Vec<i64> = vec![i64::MIN, i64::MIN + 1, -1, 0, 5, 10, 15, 20, 25, i64::MAX];

        for (rows, label) in &scenarios {
            let dir = tempfile::tempdir().unwrap();
            let fjall_store = new_fjall_storage(dir.path().join("d")).unwrap();
            let mut fjall_tx = fjall_store.write_tx().unwrap();
            let sim_store = SimStorage::new(2);
            let mut sim_tx = sim_store.write_tx().unwrap();
            let mut temp_tx = TempTx::default();
            for (k, v) in rows {
                temp_tx.put(k, v).unwrap();
                fjall_tx.put(k, v).unwrap();
                sim_tx.put(k, v).unwrap();
            }
            for ts in &queries {
                let at = ValidityTs(Reverse(*ts));
                let a = collect_skip(temp_tx.range_skip_scan_tuple(&lower, &upper, at));
                let b = collect_skip(fjall_tx.range_skip_scan_tuple(&lower, &upper, at));
                let c = collect_skip(sim_tx.range_skip_scan_tuple(&lower, &upper, at));
                assert_eq!(a, b, "skip scan temp vs fjall: {label}, ts {ts}");
                assert_eq!(a, c, "skip scan temp vs sim: {label}, ts {ts}");
            }
            let at = ValidityTs(Reverse(5));
            assert_eq!(temp_tx.range_skip_scan_tuple(&upper, &lower, at).count(), 0);
            assert_eq!(temp_tx.range_skip_scan_tuple(&lower, &lower, at).count(), 0);
        }
    }
}
