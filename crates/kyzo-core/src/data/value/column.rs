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

// #119 execution-currency foundation / naive oracle: exercised by its own tests (and, for
// laws, by runtime/verify.rs); #120 wires the foundation into the RA engine. dead_code is
// target-split (used in one target, dead in another), so #[expect] cannot be satisfied uniformly.
#![allow(dead_code)]

use std::cmp::Ordering;

use super::arena::{
    ArenaId, BulkObserver, BulkSpendAuthority, DomainCtx, DomainCtxRefusal, Epoch, EpochRemap,
};
use super::cell::{Minted, Value};
use super::code::{Code, StampedCode};

/// The container domain: the one fact a kernel admission verifies.
/// Extent is `max code + 1` over the contents (0 when empty), so
/// `extent <= observer.bulk_len()` proves every code inside is visible —
/// including against a snapshot's cut.
///
/// **Coexisting-arena boundary:** a `Domain` is owned container state — it
/// outlives [`Frame`](super::arena::Frame) nests and coexists with other
/// arenas' domains. Identity is therefore mint-checked
/// [`DomainCtx`](super::arena::DomainCtx), not an invariant-lifetime nest
/// brand (see [`super::code`] module measurement). Nest brands apply only
/// while a single live observer nest is open
/// ([`Frame::with_nested_ctx`](super::arena::Frame::with_nested_ctx)).
///
/// @authority Domain
/// @layer value
/// @owns arena+epoch+visibility admission; a raw u32 code is meaningful only under a proven Domain; cross-Domain code comparison is invalid
/// @constructs the arena/observer authority (BulkObserver admission)
/// @forbids fabricating a Domain to bless arbitrary codes | comparing or joining codes across differing arena or epoch
/// @converts Domain -> ExecRows (Rows::admit(observer) -> AdmittedRows, under this Domain)
/// @gate join_project / admit refuse cross-arena/epoch typed; raw-code use requires admission (value-plane.md)
/// @status established #119
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

    /// Cover `sc` in this domain's extent. Consumes `self` and returns a
    /// new [`Domain`] — extent growth is construction of a larger proof,
    /// never in-place mutation of an existing one (`rust-verbs` consuming
    /// rebuild; `Domain` is a proven value, not a live handle).
    ///
    /// Typed refusal on foreign arena or wrong epoch — never a panic.
    ///
    /// **Coexisting-arena boundary:** stamps arrive from any mint path;
    /// proof is [`DomainCtx::prove_shared`], not a nest brand.
    fn absorb_stamp(self, sc: StampedCode) -> Result<Domain, DomainCtxRefusal> {
        DomainCtx::prove_shared(self.arena, self.epoch, sc.arena(), sc.epoch())?;
        let raw = sc.code().raw();
        Ok(Domain {
            arena: self.arena,
            epoch: self.epoch,
            extent: self.extent.max(raw + 1),
        })
    }

    /// The plane-internal arena identity (row containers verify typed).
    pub(super) fn arena_id(&self) -> ArenaId {
        self.arena
    }

    /// Admit this domain to `o` (arena + epoch + visibility), minting the
    /// spend authority for a code resolve at a proven boundary. Used by
    /// the execution-row boundary door.
    pub(super) fn admit<O: BulkObserver>(
        &self,
        o: &O,
    ) -> Result<BulkSpendAuthority, DomainCtxRefusal> {
        self.admit_to(o, "domain resolve")
    }

    /// The admission check: arena/epoch via [`DomainCtx::prove_shared`]
    /// (typed refusal, never panic); visibility extent remains an
    /// observer-cut assert (not a domain-identity mixup).
    ///
    /// **Coexisting-arena boundary:** container domain and observer may
    /// have been created under different nest brands (or none); shared
    /// identity is mint-checked here.
    fn admit_to<O: BulkObserver>(
        &self,
        o: &O,
        what: &str,
    ) -> Result<BulkSpendAuthority, DomainCtxRefusal> {
        DomainCtx::prove_shared(self.arena, self.epoch, o.bulk_arena(), o.bulk_epoch())?;
        assert!(
            self.extent as usize <= o.bulk_len(),
            "{what} extent {} exceeds the observer's visibility ({} codes): \
             contents were minted beyond this observer's cut",
            self.extent,
            o.bulk_len()
        );
        Ok(BulkSpendAuthority::after_domain_admission())
    }

    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    pub fn extent(&self) -> u32 {
        self.extent
    }

    /// The compare/identity context for raw handles under this domain.
    /// Durable fact token — not a spend authority.
    ///
    /// **Coexisting-arena boundary:** returns unbranded [`DomainCtx`] —
    /// domains outlive observer nests; use
    /// [`Frame::with_nested_ctx`](super::arena::Frame::with_nested_ctx)
    /// when a compiler-unique nest brand is available.
    pub fn ctx(&self) -> DomainCtx {
        DomainCtx::at(self.arena, self.epoch)
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
    /// the kernels' zero-per-code reads are amortizing. Typed refusal on
    /// foreign/stale stamps — never a panic.
    pub fn push(&mut self, sc: StampedCode) -> Result<(), DomainCtxRefusal> {
        self.domain = self.domain.absorb_stamp(sc)?;
        self.codes.push(sc.code().raw());
        Ok(())
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
    /// visibility extent), then every read is check-free. Arena/epoch
    /// mismatch is a typed refusal.
    pub fn admit<'a, O: BulkObserver>(
        &'a self,
        o: &'a O,
    ) -> Result<AdmittedCodes<'a, O>, DomainCtxRefusal> {
        let _proof = self.domain.admit_to(o, "code column")?;
        Ok(AdmittedCodes {
            codes: &self.codes,
            obs: o,
            ctx: self.domain.ctx(),
            all_sealed: self.domain.extent as usize <= o.bulk_sealed_len(),
        })
    }

    /// The gather door: consume this column into the next epoch. The only
    /// mint of a new-epoch container; the old one ceases to exist here.
    /// Typed refusal when the remap is not this container's arena/epoch.
    ///
    /// **Coexisting-arena boundary:** gather joins owned container + owned
    /// remap; identity is mint-checked [`DomainCtx::prove_shared`].
    pub fn gather(self, remap: &EpochRemap) -> Result<CodeColumn, DomainCtxRefusal> {
        DomainCtx::prove_shared(
            self.domain.arena,
            self.domain.epoch,
            remap.arena_id(),
            remap.source_epoch(),
        )?;
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
        Ok(CodeColumn {
            domain: Domain {
                arena: self.domain.arena,
                epoch: remap.target_epoch(),
                extent,
            },
            codes,
        })
    }
}

/// An admitted code column: the domain is proven against `obs`, so every
/// read here spends raw codes with no further checks. Identity and
/// identity-order of packed handles go through [`DomainCtx`].
///
/// **Coexisting-arena boundary:** `ctx` is the unbranded durable token —
/// admission returns a value that callers store and pass across sites
/// where nest brands cannot unify (multiple arenas / outliving the
/// `with_nested_ctx` closure). Nest brands stay on
/// [`Frame::with_nested_ctx`](super::arena::Frame::with_nested_ctx).
pub struct AdmittedCodes<'a, O: BulkObserver> {
    codes: &'a [u32],
    obs: &'a O,
    ctx: DomainCtx,
    all_sealed: bool,
}

impl<'a, O: BulkObserver> AdmittedCodes<'a, O> {
    pub fn len(&self) -> usize {
        self.codes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.codes.is_empty()
    }

    /// The proven compare context for this admission.
    pub fn ctx(&self) -> &DomainCtx {
        &self.ctx
    }

    /// Raw codes for identity operations (equality, hashing, dedup) —
    /// lawful within this one domain under [`AdmittedCodes::ctx`]. Not a
    /// value-ordering surface.
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
        // Interim one-shot spend mint after admission already proved the
        // domain — T5 owns true consume-on-spend multiplicity.
        self.obs
            .resolve_raw(self.codes[i] as usize, BulkSpendAuthority::after_domain_admission())
    }

    /// Semantic (byte-order) comparison of two positions: raw-handle
    /// identity / sealed identity-order under [`DomainCtx`], else
    /// prefix-first through the observer.
    pub fn cmp_at(&self, i: usize, j: usize) -> Ordering {
        let a = Code(self.codes[i]);
        let b = Code(self.codes[j]);
        if self.ctx.same_handle(a, b) {
            return Ordering::Equal;
        }
        if self.all_sealed {
            return self.ctx.cmp_identity(a, b);
        }
        // Interim one-shot spend mint — T5 owns consume-on-spend.
        self.obs.cmp_raw(
            a.raw() as usize,
            b.raw() as usize,
            BulkSpendAuthority::after_domain_admission(),
        )
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
            idx.sort_by(|&i, &j| {
                self.ctx.cmp_identity(
                    Code(self.codes[i as usize]),
                    Code(self.codes[j as usize]),
                )
            });
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
    /// domain. Typed refusal on foreign/stale wide stamps — never a panic.
    pub fn push(&mut self, m: Minted) -> Result<(), DomainCtxRefusal> {
        let value = m.value();
        match m.stamp() {
            None => {
                debug_assert!(value.is_inline(), "Minted coherence broken");
                self.words.push(value);
                Ok(())
            }
            Some(stamp) => {
                debug_assert_eq!(value.code(), Some(stamp.code()), "Minted coherence broken");
                self.domain = self.domain.absorb_stamp(stamp)?;
                self.words.push(value);
                Ok(())
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

    pub fn admit<'a, O: BulkObserver>(
        &'a self,
        o: &'a O,
    ) -> Result<AdmittedWords<'a, O>, DomainCtxRefusal> {
        let _proof = self.domain.admit_to(o, "word column")?;
        Ok(AdmittedWords {
            words: &self.words,
            obs: o,
            ctx: self.domain.ctx(),
        })
    }

    /// The gather door: consume into the next epoch, rewriting every wide
    /// word's handle through the remap (values, tags, prefixes unchanged).
    /// Typed refusal when the remap is not this container's arena/epoch.
    ///
    /// **Coexisting-arena boundary:** owned column + owned remap; mint-checked
    /// [`DomainCtx::prove_shared`].
    pub fn gather(self, remap: &EpochRemap) -> Result<WordColumn, DomainCtxRefusal> {
        DomainCtx::prove_shared(
            self.domain.arena,
            self.domain.epoch,
            remap.arena_id(),
            remap.source_epoch(),
        )?;
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
        Ok(WordColumn {
            domain: Domain {
                arena: self.domain.arena,
                epoch: remap.target_epoch(),
                extent,
            },
            words,
        })
    }
}

/// An admitted word column: reads and comparisons under the proven
/// domain. Physical word identity goes through [`DomainCtx`].
///
/// **Coexisting-arena boundary:** same as [`AdmittedCodes`] — unbranded
/// durable [`DomainCtx`], not a nest brand that cannot escape admission.
pub struct AdmittedWords<'a, O: BulkObserver> {
    words: &'a [Value],
    obs: &'a O,
    ctx: DomainCtx,
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

    /// The proven compare context for this admission.
    pub fn ctx(&self) -> &DomainCtx {
        &self.ctx
    }

    /// The canonical bytes of the word at `i` (inline: rebuilt; wide:
    /// resolved through the observer).
    pub fn canonical(&self, i: usize) -> Vec<u8> {
        let w = &self.words[i];
        match w.inline_canonical() {
            Some(bytes) => bytes,
            // Interim one-shot spend mint — T5 owns consume-on-spend.
            None => self
                .obs
                .resolve_raw(
                    w.code().expect("non-inline word carries a code").raw() as usize,
                    BulkSpendAuthority::after_domain_admission(),
                )
                .to_vec(),
        }
    }

    /// Storage-order comparison: physical word identity under
    /// [`DomainCtx`] first, then the word's local knowledge (tags, inline
    /// bytes, prefixes), the observer only on a remaining tie.
    pub fn cmp_at(&self, i: usize, j: usize) -> Ordering {
        let (a, b) = (&self.words[i], &self.words[j]);
        if a.same_word(b, &self.ctx) {
            return Ordering::Equal;
        }
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
    use super::super::arena::{Arena, DomainCtxRefusal};
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
    // Write door: only domain-lawful stamps enter (typed refusal).
    // ------------------------------------------------------------------

    #[test]
    fn write_door_refuses_stale_stamps() {
        let mut arena = Arena::new();
        let sc = intern_num(&mut arena, 1);
        arena.seal();
        let f = arena.frame();
        let mut col = CodeColumn::new_in(&f);
        assert!(
            matches!(
                col.push(sc),
                Err(DomainCtxRefusal::EpochMismatch { .. })
            ),
            "epoch 0 stamp into an epoch 1 column must refuse typed"
        );
    }

    #[test]
    fn write_door_refuses_foreign_stamps() {
        let mut a = Arena::new();
        let mut b = Arena::new();
        let sa = intern_num(&mut a, 1);
        b.intern(b"x");
        let fb = b.frame();
        let mut col = CodeColumn::new_in(&fb);
        assert!(
            matches!(col.push(sa), Err(DomainCtxRefusal::ArenaMismatch { .. })),
            "foreign-arena stamp must refuse typed"
        );
    }

    // ------------------------------------------------------------------
    // Admission: arena + epoch + VISIBILITY. The snapshot-cut case is the
    // one the theorem names.
    // ------------------------------------------------------------------

    #[test]
    fn admission_refuses_stale_containers() {
        let mut arena = Arena::new();
        let sc = intern_num(&mut arena, 1);
        let col = {
            let f = arena.frame();
            let mut c = CodeColumn::new_in(&f);
            c.push(sc).expect("lawful push");
            c
        };
        arena.seal();
        let f = arena.frame();
        assert!(
            matches!(
                col.admit(&f),
                Err(DomainCtxRefusal::EpochMismatch { .. })
            ),
            "stale container must refuse typed — gather first"
        );
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
        col.push(late).expect("lawful push");
        // Arena/epoch match; visibility extent is still an assert.
        let _ = col.admit(&early);
    }

    #[test]
    fn admission_is_one_check_then_free_spends() {
        let mut arena = Arena::new();
        let stamps: Vec<StampedCode> = (0..100).map(|i| intern_num(&mut arena, i)).collect();
        let f = arena.frame();
        let mut col = CodeColumn::new_in(&f);
        for sc in stamps {
            col.push(sc).expect("lawful push");
        }
        let adm = col.admit(&f).expect("lawful admit");
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
            col.push(sc).expect("lawful push");
        }
        let adm = col.admit(&f).expect("lawful admit");
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
            col.push(sc).expect("lawful push");
        }
        let adm = col.admit(&f).expect("lawful admit");
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
            col.push(sc).expect("lawful push");
        }
        // Record values + a sorted-by-value permutation before the seal.
        let before: Vec<Vec<u8>> = {
            let f = arena.frame();
            let adm = col.admit(&f).expect("lawful admit");
            (0..adm.len()).map(|i| adm.resolve(i).to_vec()).collect()
        };
        let remap = arena.seal();
        let col = col.gather(&remap).expect("lawful gather");
        let f = arena.frame();
        let adm = col.admit(&f).expect("lawful admit"); // readmits in the new epoch
        for (i, b) in before.iter().enumerate() {
            assert_eq!(adm.resolve(i), b.as_slice(), "gather moved a value");
        }
        // Monotone remap: a sorted sealed container stays sorted.
        let mut arena2 = Arena::new();
        let mut sorted_col = CodeColumn::new_in(&arena2.frame());
        for i in 0..20 {
            let sc = intern_num(&mut arena2, i);
            sorted_col.push(sc).expect("lawful push");
        }
        let r1 = arena2.seal();
        let sorted_col = sorted_col.gather(&r1).expect("lawful gather");
        // Everything was tail (arrival = insertion order 0..20 which is
        // also value order); after seal, codes are sealed ranks.
        intern_num(&mut arena2, -1000); // will re-rank on next seal
        let r2 = arena2.seal();
        let sorted_col = sorted_col.gather(&r2).expect("lawful gather");
        let f2 = arena2.frame();
        let adm2 = sorted_col.admit(&f2).expect("lawful admit");
        let raw = adm2.raw_sealed().expect("sealed after gathers");
        assert!(
            raw.windows(2).all(|w| w[0] < w[1]),
            "monotone gather broke sortedness"
        );
    }

    #[test]
    fn gather_refuses_the_wrong_remap() {
        let mut arena = Arena::new();
        let sc = intern_num(&mut arena, 1);
        let mut col = CodeColumn::new_in(&arena.frame());
        col.push(sc).expect("lawful push");
        let r1 = arena.seal();
        let col = col.gather(&r1).expect("lawful gather");
        let _r2_skipped = arena.seal();
        let r3 = arena.seal();
        // col is at epoch 1; r3 reads epoch 2.
        assert!(
            matches!(
                col.gather(&r3),
                Err(DomainCtxRefusal::EpochMismatch { .. })
            ),
            "wrong-epoch remap must refuse typed"
        );
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
        col.push(Value::mint(&small, &mut arena))
            .expect("lawful push");
        col.push(Value::mint(&big, &mut arena))
            .expect("lawful push");
        {
            let f = arena.frame();
            let adm = col.admit(&f).expect("lawful admit");
            assert_eq!(adm.canonical(0), small.as_bytes());
            assert_eq!(adm.canonical(1), big.as_bytes());
            assert_eq!(adm.cmp_at(0, 1), small.as_bytes().cmp(big.as_bytes()));
        }
        let remap = arena.seal();
        let col = col.gather(&remap).expect("lawful gather");
        let f = arena.frame();
        let adm = col.admit(&f).expect("lawful admit");
        assert_eq!(adm.canonical(0), small.as_bytes());
        assert_eq!(
            adm.canonical(1),
            big.as_bytes(),
            "gather moved a word's value"
        );
    }

    #[test]
    fn word_column_write_door_refuses_stale_wide_words() {
        let mut arena = Arena::new();
        let big = encode(Datum::Str("a string well past the inline max"));
        let minted = Value::mint(&big, &mut arena);
        arena.seal();
        let mut col = WordColumn::new_in(&arena.frame());
        assert!(
            matches!(
                col.push(minted),
                Err(DomainCtxRefusal::EpochMismatch { .. })
            ),
            "stale wide word must refuse typed"
        );
    }
}
