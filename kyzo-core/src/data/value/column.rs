/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The columnar batch: native-typed arrays, `Code` columns, and string views — the execution currency.
//!
//! ## The admission theorem (the second transition theorem at scale)
//!
//! A kernel verifies one **container domain** — arena identity, epoch,
//! and extent within the observer's visibility (a snapshot's cut counts)
//! — and then spends a million raw codes with **zero per-code checks**.
//! That amortization is sound only because the container's **write
//! doors** prove every entering code belongs to the domain: pushes take
//! [`StampedCode`]s or [`Minted`] words and verify their stamps, and the
//! domain's extent grows to cover them. If arbitrary raw codes could
//! enter, the single admission check would guard nothing.
//!
//! ## The gather law
//!
//! Epoch crossing happens only through the consuming
//! [`CodeColumn::gather`] / [`WordColumn::gather`] doors. Stale
//! containers may still exist — their old epoch simply makes them
//! **inadmissible** to new-epoch observers — and the consuming door is
//! the only mint of a new-epoch container. The remap is monotone over
//! sealed codes, so sorted containers stay sorted under one gather.
//!
//! ## Native arrays
//!
//! Decoded primitive columns ([`Column::Ints`] etc.) carry plain values,
//! not handles: they are stamp-free by construction and never touch the
//! admission machinery — exactly why they exist as the vectorizable fast
//! lane.

use std::cmp::Ordering;

use super::arena::{ArenaId, BulkObserver, BulkSpendAuthority, Epoch, EpochRemap};
use super::cell::{Minted, Value};
use super::code::StampedCode;

/// The container domain: the one fact a kernel admission verifies.
/// Extent is `max code + 1` over the contents (0 when empty), so
/// `extent <= observer.bulk_len()` proves every code inside is visible —
/// including against a snapshot's cut.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Domain {
    arena: ArenaId,
    epoch: Epoch,
    extent: u32,
}

impl Domain {
    fn for_observer<O: BulkObserver>(o: &O) -> Domain {
        Domain {
            arena: o.bulk_arena(),
            epoch: o.bulk_epoch(),
            extent: 0,
        }
    }

    fn absorb_stamp(&mut self, sc: StampedCode, what: &str) {
        assert_eq!(
            sc.arena(),
            self.arena,
            "{what} of arena {:?} fed a stamp from foreign arena {:?}",
            self.arena,
            sc.arena()
        );
        assert_eq!(
            sc.epoch(),
            self.epoch,
            "{what} of epoch {:?} fed a stamp of epoch {:?}: cross through the gather door",
            self.epoch,
            sc.epoch()
        );
        let raw = sc.code().raw();
        self.extent = self.extent.max(raw + 1);
    }

    /// The plane-internal arena identity (row containers verify typed).
    pub(super) fn arena_id(&self) -> ArenaId {
        self.arena
    }

    /// The admission check: arena identity, epoch, AND visibility. Mints
    /// the spend authority the raw observer methods demand.
    fn admit_to<O: BulkObserver>(&self, o: &O, what: &str) -> BulkSpendAuthority {
        assert_eq!(
            o.bulk_arena(),
            self.arena,
            "{what} of arena {:?} admitted to an observer of foreign arena {:?}",
            self.arena,
            o.bulk_arena()
        );
        assert_eq!(
            o.bulk_epoch(),
            self.epoch,
            "{what} of epoch {:?} admitted to an observer of epoch {:?}: gather first",
            self.epoch,
            o.bulk_epoch()
        );
        assert!(
            self.extent as usize <= o.bulk_len(),
            "{what} extent {} exceeds the observer's visibility ({} codes): \
             contents were minted beyond this observer's cut",
            self.extent,
            o.bulk_len()
        );
        BulkSpendAuthority::after_domain_admission()
    }

    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    pub fn extent(&self) -> u32 {
        self.extent
    }
}

/// A stamped column of raw codes: the packed execution currency. Codes
/// enter only through the stamp-verifying write doors; kernels read only
/// through [`CodeColumn::admit`].
pub struct CodeColumn {
    domain: Domain,
    codes: Vec<u32>,
}

impl CodeColumn {
    /// An empty column in the observer's domain.
    pub fn new_in<O: BulkObserver>(o: &O) -> CodeColumn {
        CodeColumn {
            domain: Domain::for_observer(o),
            codes: Vec::new(),
        }
    }

    /// The write door: a stamp-verified push. This per-push check is what
    /// the kernels' zero-per-code reads are amortizing.
    pub fn push(&mut self, sc: StampedCode) {
        self.domain.absorb_stamp(sc, "code column");
        self.codes.push(sc.code().raw());
    }

    pub fn len(&self) -> usize {
        self.codes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.codes.is_empty()
    }

    pub fn domain(&self) -> Domain {
        self.domain
    }

    /// The admission: one container-domain check (arena + epoch +
    /// visibility extent), then every read is check-free.
    pub fn admit<'a, O: BulkObserver>(&'a self, o: &'a O) -> AdmittedCodes<'a, O> {
        let proof = self.domain.admit_to(o, "code column");
        AdmittedCodes {
            codes: &self.codes,
            obs: o,
            proof,
            all_sealed: self.domain.extent as usize <= o.bulk_sealed_len(),
        }
    }

    /// The gather door: consume this column into the next epoch. The only
    /// mint of a new-epoch container; the old one ceases to exist here.
    pub fn gather(self, remap: &EpochRemap) -> CodeColumn {
        assert_eq!(
            remap.arena_id(),
            self.domain.arena,
            "gather fed a remap of a foreign arena"
        );
        assert_eq!(
            self.domain.epoch,
            remap.from_epoch(),
            "gather fed a remap reading epoch {:?}, container is epoch {:?}",
            remap.from_epoch(),
            self.domain.epoch
        );
        let mut extent = 0u32;
        let codes: Vec<u32> = self
            .codes
            .into_iter()
            .map(|c| {
                let n = remap.apply_raw(super::code::Code(c)).raw();
                extent = extent.max(n + 1);
                n
            })
            .collect();
        CodeColumn {
            domain: Domain {
                arena: self.domain.arena,
                epoch: remap.to_epoch(),
                extent,
            },
            codes,
        }
    }
}

/// An admitted code column: the domain is proven against `obs`, so every
/// read here spends raw codes with no further checks.
pub struct AdmittedCodes<'a, O: BulkObserver> {
    codes: &'a [u32],
    obs: &'a O,
    proof: BulkSpendAuthority,
    all_sealed: bool,
}

impl<'a, O: BulkObserver> AdmittedCodes<'a, O> {
    pub fn len(&self) -> usize {
        self.codes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.codes.is_empty()
    }

    /// Raw codes for identity operations (equality, hashing, dedup) —
    /// lawful within this one domain. Not an ordering surface.
    pub fn raw(&self) -> &'a [u32] {
        self.codes
    }

    /// Whether every code is sealed, making raw numeric order the byte
    /// order — the vectorizable fast lane.
    pub fn all_sealed(&self) -> bool {
        self.all_sealed
    }

    /// The order-lawful numeric view: `Some` only when all codes are
    /// sealed (rank order IS byte order). Tail-bearing columns get
    /// `None` and go through [`AdmittedCodes::cmp_at`].
    pub fn raw_sealed(&self) -> Option<&'a [u32]> {
        if self.all_sealed {
            Some(self.codes)
        } else {
            None
        }
    }

    /// The canonical bytes of the value at `i`.
    pub fn resolve(&self, i: usize) -> &'a [u8] {
        self.obs.resolve_raw(self.codes[i] as usize, &self.proof)
    }

    /// Semantic (byte-order) comparison of two positions: numeric when
    /// the fast lane holds, prefix-first through the observer otherwise.
    pub fn cmp_at(&self, i: usize, j: usize) -> Ordering {
        let (a, b) = (self.codes[i], self.codes[j]);
        if a == b {
            return Ordering::Equal;
        }
        if self.all_sealed {
            return a.cmp(&b);
        }
        self.obs.cmp_raw(a as usize, b as usize, &self.proof)
    }

    /// A deterministic sort permutation by value order (the kernel the
    /// laws exercise; takes the fast lane when it may).
    pub fn sort_permutation(&self) -> Vec<u32> {
        assert!(
            self.codes.len() <= u32::MAX as usize,
            "column exceeds u32 index space"
        );
        let mut idx: Vec<u32> = (0..self.codes.len() as u32).collect();
        if self.all_sealed {
            idx.sort_by_key(|&i| self.codes[i as usize]);
        } else {
            idx.sort_by(|&i, &j| self.cmp_at(i as usize, j as usize));
        }
        idx
    }
}

/// A stamped column of 16-byte words. Inline words are self-contained;
/// wide words carry codes, so the write door consumes the unforgeable
/// [`Minted`] pairing — a wide word cannot enter without the stamp it was
/// minted with, and the stamp is verified into the domain.
///
/// THE UNIFORM CONTAINER LAW: a column is an epoch-domain container even
/// when every word happens to be inline — after a seal it is inadmissible
/// until gathered, like every container. Inline words are self-contained;
/// columns never are. One law, no special cases.
pub struct WordColumn {
    domain: Domain,
    words: Vec<Value>,
}

impl WordColumn {
    pub fn new_in<O: BulkObserver>(o: &O) -> WordColumn {
        WordColumn {
            domain: Domain::for_observer(o),
            words: Vec::new(),
        }
    }

    /// The write door: consumes the minted pairing. Inline words carry no
    /// context and pass freely; wide words verify their stamp into the
    /// domain.
    pub fn push(&mut self, m: Minted) {
        let value = m.value();
        match m.stamp() {
            None => {
                debug_assert!(value.is_inline(), "Minted coherence broken");
                self.words.push(value);
            }
            Some(stamp) => {
                debug_assert_eq!(value.code(), Some(stamp.code()), "Minted coherence broken");
                self.domain.absorb_stamp(stamp, "word column");
                self.words.push(value);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.words.len()
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    pub fn domain(&self) -> Domain {
        self.domain
    }

    pub fn admit<'a, O: BulkObserver>(&'a self, o: &'a O) -> AdmittedWords<'a, O> {
        let proof = self.domain.admit_to(o, "word column");
        AdmittedWords {
            words: &self.words,
            obs: o,
            proof,
        }
    }

    /// The gather door: consume into the next epoch, rewriting every wide
    /// word's handle through the remap (values, tags, prefixes unchanged).
    pub fn gather(self, remap: &EpochRemap) -> WordColumn {
        assert_eq!(
            remap.arena_id(),
            self.domain.arena,
            "gather fed a remap of a foreign arena"
        );
        assert_eq!(
            self.domain.epoch,
            remap.from_epoch(),
            "gather fed a remap reading epoch {:?}, container is epoch {:?}",
            remap.from_epoch(),
            self.domain.epoch
        );
        let mut extent = 0u32;
        let words: Vec<Value> = self
            .words
            .into_iter()
            .map(|w| {
                let g = w.gathered(remap);
                if let Some(code) = g.code() {
                    extent = extent.max(code.raw() + 1);
                }
                g
            })
            .collect();
        WordColumn {
            domain: Domain {
                arena: self.domain.arena,
                epoch: remap.to_epoch(),
                extent,
            },
            words,
        }
    }
}

/// An admitted word column: reads and comparisons under the proven
/// domain.
pub struct AdmittedWords<'a, O: BulkObserver> {
    words: &'a [Value],
    obs: &'a O,
    proof: BulkSpendAuthority,
}

impl<'a, O: BulkObserver> AdmittedWords<'a, O> {
    pub fn len(&self) -> usize {
        self.words.len()
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    pub fn get(&self, i: usize) -> Value {
        self.words[i]
    }

    /// The canonical bytes of the word at `i` (inline: rebuilt; wide:
    /// resolved through the observer).
    pub fn canonical(&self, i: usize) -> Vec<u8> {
        let w = &self.words[i];
        match w.inline_canonical() {
            Some(bytes) => bytes,
            None => self
                .obs
                .resolve_raw(
                    w.code().expect("non-inline word carries a code").raw() as usize,
                    &self.proof,
                )
                .to_vec(),
        }
    }

    /// Storage-order comparison: the word's local knowledge first
    /// (tags, inline bytes, prefixes), the observer only on a tie.
    pub fn cmp_at(&self, i: usize, j: usize) -> Ordering {
        let (a, b) = (&self.words[i], &self.words[j]);
        match a.try_cmp_storage(b) {
            Some(o) => o,
            None => self.canonical(i).cmp(&self.canonical(j)),
        }
    }
}

/// The execution currency: one batch column. Native arrays are decoded
/// plain values (stamp-free, vectorizable); code and word columns are
/// stamped containers.
pub enum Column {
    Ints(Vec<i64>),
    Floats(Vec<f64>),
    Bools(Vec<bool>),
    Codes(CodeColumn),
    Words(WordColumn),
}

impl Column {
    pub fn len(&self) -> usize {
        match self {
            Column::Ints(v) => v.len(),
            Column::Floats(v) => v.len(),
            Column::Bools(v) => v.len(),
            Column::Codes(c) => c.len(),
            Column::Words(w) => w.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::super::arena::Arena;
    use super::super::canonical::{Datum, encode};
    use super::super::number::Num;
    use super::*;

    fn intern_num(arena: &mut Arena, n: i64) -> StampedCode {
        let cb = encode(Datum::Num(Num::int(n)));
        match Value::mint(&cb, arena).stamp() {
            None => arena_intern_direct(arena, &cb),
            Some(stamp) => stamp,
        }
    }

    /// Numbers are always inline as words, but columns of CODES intern
    /// everything — the fixpoint currency is codes. Plane-internal intern
    /// is the door for that.
    fn arena_intern_direct(
        arena: &mut Arena,
        cb: &super::super::canonical::CanonicalBytes,
    ) -> StampedCode {
        arena.intern(cb.as_bytes())
    }

    fn str_datum_stamp(arena: &mut Arena, s: &str) -> StampedCode {
        arena.intern(encode(Datum::Str(s)).as_bytes())
    }

    // ------------------------------------------------------------------
    // Write door: only domain-lawful stamps enter.
    // ------------------------------------------------------------------

    #[test]
    #[should_panic(expected = "cross through the gather door")]
    fn write_door_refuses_stale_stamps() {
        let mut arena = Arena::new();
        let sc = intern_num(&mut arena, 1);
        arena.seal();
        let f = arena.frame();
        let mut col = CodeColumn::new_in(&f);
        col.push(sc); // epoch 0 stamp into an epoch 1 column
    }

    #[test]
    #[should_panic(expected = "foreign arena")]
    fn write_door_refuses_foreign_stamps() {
        let mut a = Arena::new();
        let mut b = Arena::new();
        let sa = intern_num(&mut a, 1);
        b.intern(b"x");
        let fb = b.frame();
        let mut col = CodeColumn::new_in(&fb);
        col.push(sa);
    }

    // ------------------------------------------------------------------
    // Admission: arena + epoch + VISIBILITY. The snapshot-cut case is the
    // one the theorem names.
    // ------------------------------------------------------------------

    #[test]
    #[should_panic(expected = "gather first")]
    fn admission_refuses_stale_containers() {
        let mut arena = Arena::new();
        let sc = intern_num(&mut arena, 1);
        let col = {
            let f = arena.frame();
            let mut c = CodeColumn::new_in(&f);
            c.push(sc);
            c
        };
        arena.seal();
        let f = arena.frame();
        col.admit(&f);
    }

    #[test]
    #[should_panic(expected = "exceeds the observer's visibility")]
    fn admission_refuses_contents_beyond_a_snapshot_cut() {
        let mut arena = Arena::new();
        intern_num(&mut arena, 1);
        let early = arena.snapshot();
        // Same epoch, but this code was minted after the cut.
        let late = intern_num(&mut arena, 2);
        let mut col = CodeColumn::new_in(&arena.frame());
        col.push(late);
        col.admit(&early);
    }

    #[test]
    fn admission_is_one_check_then_free_spends() {
        let mut arena = Arena::new();
        let stamps: Vec<StampedCode> = (0..100).map(|i| intern_num(&mut arena, i)).collect();
        let f = arena.frame();
        let mut col = CodeColumn::new_in(&f);
        for sc in stamps {
            col.push(sc);
        }
        let adm = col.admit(&f);
        for i in 0..adm.len() {
            assert!(!adm.resolve(i).is_empty());
        }
    }

    // ------------------------------------------------------------------
    // The fast lane and its boundary: sealed-only columns compare
    // numerically; tail-bearing columns must not claim the lane.
    // ------------------------------------------------------------------

    #[test]
    fn sealed_fast_lane_agrees_with_byte_order() {
        let mut arena = Arena::new();
        for i in [5i64, -3, 99, 0, 42] {
            intern_num(&mut arena, i);
        }
        let remap = arena.seal();
        let _ = remap;
        // Re-intern: all sealed hits now.
        let stamps: Vec<StampedCode> = [5i64, -3, 99, 0, 42, -3]
            .iter()
            .map(|&i| intern_num(&mut arena, i))
            .collect();
        let f = arena.frame();
        let mut col = CodeColumn::new_in(&f);
        for sc in stamps {
            col.push(sc);
        }
        let adm = col.admit(&f);
        assert!(adm.all_sealed());
        assert!(adm.raw_sealed().is_some());
        let perm = adm.sort_permutation();
        // The permutation must equal the byte-order sort, exactly.
        let mut expect: Vec<u32> = (0..adm.len() as u32).collect();
        expect.sort_by(|&i, &j| adm.resolve(i as usize).cmp(adm.resolve(j as usize)));
        // Stable comparison: resolve-order and permutation order agree
        // pairwise (indices with equal values may swap; compare values).
        let by_perm: Vec<&[u8]> = perm.iter().map(|&i| adm.resolve(i as usize)).collect();
        let by_expect: Vec<&[u8]> = expect.iter().map(|&i| adm.resolve(i as usize)).collect();
        assert_eq!(by_perm, by_expect);
    }

    #[test]
    fn tail_bearing_columns_leave_the_fast_lane_and_still_order_correctly() {
        let mut arena = Arena::new();
        intern_num(&mut arena, 10);
        arena.seal();
        // Mixed: sealed hit + fresh tail codes, deliberately out of
        // arrival order vs value order.
        let stamps = vec![
            intern_num(&mut arena, 10),         // sealed
            str_datum_stamp(&mut arena, "zzz"), // tail
            intern_num(&mut arena, -7),         // tail — numerically before 10
        ];
        let f = arena.frame();
        let mut col = CodeColumn::new_in(&f);
        for sc in stamps {
            col.push(sc);
        }
        let adm = col.admit(&f);
        assert!(!adm.all_sealed());
        assert!(adm.raw_sealed().is_none());
        let perm = adm.sort_permutation();
        let sorted: Vec<&[u8]> = perm.iter().map(|&i| adm.resolve(i as usize)).collect();
        let mut expect = sorted.clone();
        expect.sort();
        assert_eq!(sorted, expect, "tail path diverged from byte order");
        // -7 (Num) sorts before 10 (Num) sorts before "zzz" (Str tag).
        assert_eq!(perm[0], 2);
        assert_eq!(perm[1], 0);
        assert_eq!(perm[2], 1);
    }

    // ------------------------------------------------------------------
    // The gather law: consuming, value-preserving, order-preserving,
    // and the only path to a new-epoch container.
    // ------------------------------------------------------------------

    #[test]
    fn gather_preserves_values_and_sortedness_and_readmits() {
        let mut arena = Arena::new();
        let stamps: Vec<StampedCode> = (0..50)
            .map(|i| intern_num(&mut arena, i * 3 % 17))
            .collect();
        let mut col = CodeColumn::new_in(&arena.frame());
        for sc in stamps {
            col.push(sc);
        }
        // Record values + a sorted-by-value permutation before the seal.
        let before: Vec<Vec<u8>> = {
            let f = arena.frame();
            let adm = col.admit(&f);
            (0..adm.len()).map(|i| adm.resolve(i).to_vec()).collect()
        };
        let remap = arena.seal();
        let col = col.gather(&remap);
        let f = arena.frame();
        let adm = col.admit(&f); // readmits in the new epoch
        for (i, b) in before.iter().enumerate() {
            assert_eq!(adm.resolve(i), b.as_slice(), "gather moved a value");
        }
        // Monotone remap: a sorted sealed container stays sorted.
        let mut arena2 = Arena::new();
        let mut sorted_col = CodeColumn::new_in(&arena2.frame());
        for i in 0..20 {
            let sc = intern_num(&mut arena2, i);
            sorted_col.push(sc);
        }
        let r1 = arena2.seal();
        let sorted_col = sorted_col.gather(&r1);
        // Everything was tail (arrival = insertion order 0..20 which is
        // also value order); after seal, codes are sealed ranks.
        intern_num(&mut arena2, -1000); // will re-rank on next seal
        let r2 = arena2.seal();
        let sorted_col = sorted_col.gather(&r2);
        let f2 = arena2.frame();
        let adm2 = sorted_col.admit(&f2);
        let raw = adm2.raw_sealed().expect("sealed after gathers");
        assert!(
            raw.windows(2).all(|w| w[0] < w[1]),
            "monotone gather broke sortedness"
        );
    }

    #[test]
    #[should_panic(expected = "remap reading epoch")]
    fn gather_refuses_the_wrong_remap() {
        let mut arena = Arena::new();
        let sc = intern_num(&mut arena, 1);
        let mut col = CodeColumn::new_in(&arena.frame());
        col.push(sc);
        let r1 = arena.seal();
        let col = col.gather(&r1);
        let _r2_skipped = arena.seal();
        let r3 = arena.seal();
        // col is at epoch 1; r3 reads epoch 2.
        let _ = col.gather(&r3);
    }

    // ------------------------------------------------------------------
    // Word columns: inline words free, wide words stamped, gather
    // rewrites handles only.
    // ------------------------------------------------------------------

    #[test]
    fn word_column_holds_mixed_residency_and_gathers() {
        let mut arena = Arena::new();
        let mut col = WordColumn::new_in(&arena.frame());
        let small = encode(Datum::Str("hi"));
        let big = encode(Datum::Str("a string well past the inline max"));
        col.push(Value::mint(&small, &mut arena));
        col.push(Value::mint(&big, &mut arena));
        {
            let f = arena.frame();
            let adm = col.admit(&f);
            assert_eq!(adm.canonical(0), small.as_bytes());
            assert_eq!(adm.canonical(1), big.as_bytes());
            assert_eq!(adm.cmp_at(0, 1), small.as_bytes().cmp(big.as_bytes()));
        }
        let remap = arena.seal();
        let col = col.gather(&remap);
        let f = arena.frame();
        let adm = col.admit(&f);
        assert_eq!(adm.canonical(0), small.as_bytes());
        assert_eq!(
            adm.canonical(1),
            big.as_bytes(),
            "gather moved a word's value"
        );
    }

    #[test]
    #[should_panic(expected = "cross through the gather door")]
    fn word_column_write_door_refuses_stale_wide_words() {
        let mut arena = Arena::new();
        let big = encode(Datum::Str("a string well past the inline max"));
        let minted = Value::mint(&big, &mut arena);
        arena.seal();
        let mut col = WordColumn::new_in(&arena.frame());
        col.push(minted);
    }
}
