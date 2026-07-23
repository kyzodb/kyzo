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

use super::admission::{Admission, BulkPass, BulkSpendAuthority, Denial, SpendAdmission};
use super::arena::{ArenaId, BulkObserver, Epoch, EpochRemap};
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
/// [`Admission`](super::admission::Admission), not an invariant-lifetime nest
/// brand (see [`super::code`] module measurement). Nest brands apply only
/// while a single live observer nest is open
/// ([`Frame::with_nested_ctx`](super::arena::Frame::with_nested_ctx)).
/// Admission and [`Denial`](super::admission::Denial) speak one vocabulary
/// ([`super::admission`]).
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
    /// Typed [`Denial`] on foreign arena, wrong epoch, or extent wrap —
    /// never a panic, never a bare boolean.
    ///
    /// **Coexisting-arena boundary:** stamps arrive from any mint path;
    /// proof is [`Admission::prove_shared`], not a nest brand.
    fn absorb_stamp(self, sc: StampedCode) -> Result<Domain, Denial> {
        Admission::prove_shared(self.arena, self.epoch, sc.arena(), sc.epoch())?;
        let next = sc
            .code()
            .raw()
            .checked_add(1)
            .ok_or(Denial::ExtentOverflow)?;
        Ok(Domain {
            arena: self.arena,
            epoch: self.epoch,
            extent: self.extent.max(next),
        })
    }

    /// The plane-internal arena identity (row containers verify typed).
    pub(super) fn arena_id(&self) -> ArenaId {
        self.arena
    }

    /// Admit this domain to `o` (arena + epoch + visibility), minting the
    /// spend authority for a code resolve at a proven boundary. Used by
    /// the execution-row boundary door.
    pub(super) fn admit<O: BulkObserver>(&self, o: &O) -> Result<SpendAdmission, Denial> {
        self.admit_to(o)
    }

    /// The admission check: arena/epoch via [`Admission::prove_shared`]
    /// and visibility extent against the observer cut — both typed
    /// [`Denial`], never panic.
    ///
    /// **Coexisting-arena boundary:** container domain and observer may
    /// have been created under different nest brands (or none); shared
    /// identity is mint-checked here.
    fn admit_to<O: BulkObserver>(&self, o: &O) -> Result<SpendAdmission, Denial> {
        Admission::prove_shared(self.arena, self.epoch, o.bulk_arena(), o.bulk_epoch())?;
        let visible = o.bulk_len();
        let required = crate::value::convert::usize_from_u32(self.extent);
        if required > visible {
            return Err(Denial::VisibilityOverflow { required, visible });
        }
        Ok(BulkSpendAuthority::after_domain_admission())
    }

    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    pub fn extent(&self) -> u32 {
        self.extent
    }

    /// The compare/identity context for raw handles under this domain.
    /// Durable admission token — not a spend authority.
    ///
    /// **Coexisting-arena boundary:** returns unbranded [`Admission`] —
    /// domains outlive observer nests; use
    /// [`Frame::with_nested_ctx`](super::arena::Frame::with_nested_ctx)
    /// when a compiler-unique nest brand is available.
    pub fn ctx(&self) -> Admission {
        Admission::at(self.arena, self.epoch)
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
    /// the kernels' zero-per-code reads are amortizing. Typed [`Denial`] on
    /// foreign/stale stamps — never a panic.
    pub fn push(&mut self, sc: StampedCode) -> Result<(), Denial> {
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
    /// mismatch and cut overflow are typed [`Denial`].
    pub fn admit<'a, O: BulkObserver>(&'a self, o: &'a O) -> Result<AdmittedCodes<'a, O>, Denial> {
        // One admission authority, spent by value into the bulk pass —
        // not discarded and reminted per resolve.
        let proof = self.domain.admit_to(o)?;
        Ok(AdmittedCodes {
            codes: &self.codes,
            pass: proof.open_pass(o),
            ctx: self.domain.ctx(),
            all_sealed: crate::value::convert::usize_from_u32(self.domain.extent) <= o.bulk_sealed_len(),
        })
    }

    /// The gather door: consume this column into the next epoch. The only
    /// mint of a new-epoch container; the old one ceases to exist here.
    /// Typed [`Denial`] when the remap is not this container's arena/epoch.
    ///
    /// **Coexisting-arena boundary:** gather joins owned container + owned
    /// remap; identity is mint-checked [`Admission::prove_shared`].
    pub fn gather(self, remap: &EpochRemap) -> Result<CodeColumn, Denial> {
        Admission::prove_shared(
            self.domain.arena,
            self.domain.epoch,
            remap.arena_id(),
            remap.source_epoch(),
        )?;
        let mut extent = 0u32;
        let mut codes = Vec::with_capacity(self.codes.len());
        for c in self.codes {
            let n = remap.apply_raw(super::code::Code(c))?.raw();
            let next = n.checked_add(1).ok_or(Denial::CodeRemapOverflow)?;
            extent = extent.max(next);
            codes.push(n);
        }
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
/// identity-order of packed handles go through [`Admission`].
///
/// **Coexisting-arena boundary:** `ctx` is the unbranded durable token —
/// admission returns a value that callers store and pass across sites
/// where nest brands cannot unify (multiple arenas / outliving the
/// `with_nested_ctx` closure). Nest brands stay on
/// [`Frame::with_nested_ctx`](super::arena::Frame::with_nested_ctx).
pub struct AdmittedCodes<'a, O: BulkObserver> {
    codes: &'a [u32],
    /// Admission authority spent into this pass at [`CodeColumn::admit`].
    pass: BulkPass<'a, O>,
    ctx: Admission,
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
    pub fn ctx(&self) -> &Admission {
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
    pub fn resolve(&self, i: usize) -> Result<&'a [u8], Denial> {
        self.pass
            .resolve(crate::value::convert::usize_from_u32(self.codes[i]))
    }

    /// Semantic (byte-order) comparison of two positions: raw-handle
    /// identity / sealed identity-order under [`Admission`], else
    /// prefix-first through the observer.
    pub fn cmp_at(&self, i: usize, j: usize) -> Result<Ordering, Denial> {
        let a = Code(self.codes[i]);
        let b = Code(self.codes[j]);
        if self.ctx.same_handle(a, b) {
            return Ok(Ordering::Equal);
        }
        if self.all_sealed {
            return Ok(self.ctx.cmp_identity(a, b));
        }
        self.pass.cmp(
            crate::value::convert::usize_from_u32(a.raw()),
            crate::value::convert::usize_from_u32(b.raw()),
        )
    }

    /// A deterministic sort permutation by value order (the kernel the
    /// laws exercise; takes the fast lane when it may).
    ///
    /// Refuses with [`Denial::ExtentOverflow`] when the column exceeds
    /// `u32` index space; propagates typed [`Denial`] from
    /// [`AdmittedCodes::cmp_at`] on a corrupt admission extent — never a
    /// process abort.
    pub fn sort_permutation(&self) -> Result<Vec<u32>, Denial> {
        if u32::try_from(self.codes.len()).is_err() {
            return Err(Denial::ExtentOverflow);
        }
        let mut idx: Vec<u32> = (0..match u32::try_from(self.codes.len()) {
            Ok(n) => n,
            Err(_) => {
                return Err(Denial::ExtentOverflow);
            }
        })
        .collect();
        if self.all_sealed {
            idx.sort_by(|&i, &j| {
                self.ctx.cmp_identity(
                    Code(self.codes[crate::value::convert::usize_from_u32(i)]),
                    Code(self.codes[crate::value::convert::usize_from_u32(j)]),
                )
            });
        } else {
            let mut denied = None;
            idx.sort_by(|&i, &j| {
                match self.cmp_at(
                    crate::value::convert::usize_from_u32(i),
                    crate::value::convert::usize_from_u32(j),
                ) {
                    Ok(o) => o,
                    Err(e) => {
                        denied = Some(e);
                        Ordering::Equal
                    }
                }
            });
            if let Some(e) = denied {
                return Err(e);
            }
        }
        Ok(idx)
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
    /// domain. Typed [`Denial`] on foreign/stale wide stamps or a broken
    /// [`Minted`] pairing — never a panic.
    pub fn push(&mut self, m: Minted) -> Result<(), Denial> {
        let value = m.value();
        match m.stamp() {
            None => {
                if !value.is_inline() {
                    return Err(Denial::BookkeepingBroken);
                }
                self.words.push(value);
                Ok(())
            }
            Some(stamp) => {
                if value.code().map(Code::raw) != Some(stamp.code().raw()) {
                    return Err(Denial::BookkeepingBroken);
                }
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

    pub fn admit<'a, O: BulkObserver>(&'a self, o: &'a O) -> Result<AdmittedWords<'a, O>, Denial> {
        let proof = self.domain.admit_to(o)?;
        Ok(AdmittedWords {
            words: &self.words,
            pass: proof.open_pass(o),
            ctx: self.domain.ctx(),
        })
    }

    /// The gather door: consume into the next epoch, rewriting every wide
    /// word's handle through the remap (values, tags, prefixes unchanged).
    /// Typed [`Denial`] when the remap is not this container's arena/epoch.
    ///
    /// **Coexisting-arena boundary:** owned column + owned remap; mint-checked
    /// [`Admission::prove_shared`].
    pub fn gather(self, remap: &EpochRemap) -> Result<WordColumn, Denial> {
        Admission::prove_shared(
            self.domain.arena,
            self.domain.epoch,
            remap.arena_id(),
            remap.source_epoch(),
        )?;
        let mut extent = 0u32;
        let mut words = Vec::with_capacity(self.words.len());
        for w in self.words {
            let g = w.gathered(remap)?;
            if let Some(code) = g.code() {
                let next = code
                    .raw()
                    .checked_add(1)
                    .ok_or(Denial::CodeRemapOverflow)?;
                extent = extent.max(next);
            }
            words.push(g);
        }
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
/// domain. Physical word identity goes through [`Admission`].
///
/// **Coexisting-arena boundary:** same as [`AdmittedCodes`] — unbranded
/// durable [`Admission`], not a nest brand that cannot escape admission.
pub struct AdmittedWords<'a, O: BulkObserver> {
    words: &'a [Value],
    /// Admission authority spent into this pass at [`WordColumn::admit`].
    pass: BulkPass<'a, O>,
    ctx: Admission,
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
    pub fn ctx(&self) -> &Admission {
        &self.ctx
    }

    /// The canonical bytes of the word at `i` (inline: rebuilt; wide:
    /// resolved through the observer).
    ///
    /// A non-inline word without a code is corrupt residency — typed
    /// [`Denial::BookkeepingBroken`], never a panic.
    pub fn canonical(&self, i: usize) -> Result<Vec<u8>, Denial> {
        let w = &self.words[i];
        match w.inline_canonical() {
            Some(bytes) => Ok(bytes),
            None => {
                let code = w.code().ok_or(Denial::BookkeepingBroken)?;
                Ok(self
                    .pass
                    .resolve(crate::value::convert::usize_from_u32(code.raw()))?
                    .to_vec())
            }
        }
    }

    /// Storage-order comparison: physical word identity under
    /// [`Admission`] first, then the word's local knowledge (tags, inline
    /// bytes, prefixes), the observer only on a remaining tie.
    pub fn cmp_at(&self, i: usize, j: usize) -> Result<Ordering, Denial> {
        let (a, b) = (&self.words[i], &self.words[j]);
        if a.same_word(b, &self.ctx) {
            return Ok(Ordering::Equal);
        }
        match a.try_cmp_storage(b) {
            Some(o) => Ok(o),
            None => Ok(self.canonical(i)?.cmp(&self.canonical(j)?)),
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
    use miette::{IntoDiagnostic, Result, miette};

    use super::super::admission::Denial;
    use super::super::arena::Arena;
    use super::super::canonical::{Datum, encode};
    use super::super::number::Num;
    use super::*;

    fn intern_num(arena: &mut Arena, n: i64) -> Result<StampedCode> {
        let cb = encode(Datum::Num(Num::int(n)));
        Ok(match Value::mint(&cb, arena).into_diagnostic()?.stamp() {
            None => arena_intern_direct(arena, &cb)?,
            Some(stamp) => stamp,
        })
    }

    /// Numbers are always inline as words, but columns of CODES intern
    /// everything — the fixpoint currency is codes. Plane-internal intern
    /// is the door for that.
    fn arena_intern_direct(
        arena: &mut Arena,
        cb: &super::super::canonical::CanonicalBytes,
    ) -> Result<StampedCode> {
        Ok(arena.intern(cb.as_bytes()).into_diagnostic()?)
    }

    fn str_datum_stamp(arena: &mut Arena, s: &str) -> Result<StampedCode> {
        Ok(arena
            .intern(encode(Datum::Str(s)).as_bytes())
            .into_diagnostic()?)
    }

    // ------------------------------------------------------------------
    // Write door: only domain-lawful stamps enter (typed refusal).
    // ------------------------------------------------------------------

    #[test]
    fn write_door_refuses_stale_stamps() -> Result<()> {
        let mut arena = Arena::new();
        let sc = intern_num(&mut arena, 1)?;
        arena.seal().into_diagnostic()?;
        let f = arena.frame();
        let mut col = CodeColumn::new_in(&f);
        assert!(
            matches!(col.push(sc), Err(Denial::EpochMismatch { .. })),
            "epoch 0 stamp into an epoch 1 column must refuse typed"
        );
        Ok(())
    }

    #[test]
    fn write_door_refuses_foreign_stamps() -> Result<()> {
        let mut a = Arena::new();
        let mut b = Arena::new();
        let sa = intern_num(&mut a, 1)?;
        b.intern(b"x").into_diagnostic()?;
        let fb = b.frame();
        let mut col = CodeColumn::new_in(&fb);
        assert!(
            matches!(col.push(sa), Err(Denial::ArenaMismatch { .. })),
            "foreign-arena stamp must refuse typed"
        );
        Ok(())
    }

    // ------------------------------------------------------------------
    // Admission: arena + epoch + VISIBILITY. The snapshot-cut case is the
    // one the theorem names.
    // ------------------------------------------------------------------

    #[test]
    fn admission_refuses_stale_containers() -> Result<()> {
        let mut arena = Arena::new();
        let sc = intern_num(&mut arena, 1)?;
        let col = {
            let f = arena.frame();
            let mut c = CodeColumn::new_in(&f);
            c.push(sc).into_diagnostic()?;
            c
        };
        arena.seal().into_diagnostic()?;
        let f = arena.frame();
        assert!(
            matches!(col.admit(&f), Err(Denial::EpochMismatch { .. })),
            "stale container must refuse typed — gather first"
        );
        Ok(())
    }

    #[test]
    fn admission_refuses_contents_beyond_a_snapshot_cut() -> Result<()> {
        let mut arena = Arena::new();
        intern_num(&mut arena, 1)?;
        let early = arena.snapshot();
        // Same epoch, but this code was minted after the cut.
        let late = intern_num(&mut arena, 2)?;
        let mut col = CodeColumn::new_in(&arena.frame());
        col.push(late).into_diagnostic()?;
        assert!(
            matches!(col.admit(&early), Err(Denial::VisibilityOverflow { .. })),
            "contents beyond the observer cut must refuse typed"
        );
        Ok(())
    }

    #[test]
    fn admission_is_one_check_then_free_spends() -> Result<()> {
        let mut arena = Arena::new();
        let mut stamps = Vec::with_capacity(100);
        for i in 0..100 {
            stamps.push(intern_num(&mut arena, i)?);
        }
        let f = arena.frame();
        let mut col = CodeColumn::new_in(&f);
        for sc in stamps {
            col.push(sc).into_diagnostic()?;
        }
        let adm = col.admit(&f).into_diagnostic()?;
        for i in 0..adm.len() {
            assert!(!adm.resolve(i).into_diagnostic()?.is_empty());
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // The fast lane and its boundary: sealed-only columns compare
    // numerically; tail-bearing columns must not claim the lane.
    // ------------------------------------------------------------------

    #[test]
    fn sealed_fast_lane_agrees_with_byte_order() -> Result<()> {
        let mut arena = Arena::new();
        for i in [5i64, -3, 99, 0, 42] {
            intern_num(&mut arena, i)?;
        }
        let remap = arena.seal().into_diagnostic()?;
        match remap { value => core::mem::drop(value) };
        // Re-intern: all sealed hits now.
        let mut stamps = Vec::new();
        for &i in &[5i64, -3, 99, 0, 42, -3] {
            stamps.push(intern_num(&mut arena, i)?);
        }
        let f = arena.frame();
        let mut col = CodeColumn::new_in(&f);
        for sc in stamps {
            col.push(sc).into_diagnostic()?;
        }
        let adm = col.admit(&f).into_diagnostic()?;
        assert!(adm.all_sealed());
        assert!(adm.raw_sealed().is_some());
        let perm = adm.sort_permutation().into_diagnostic()?;
        // The permutation must equal the byte-order sort, exactly.
        let len_u32 = u32::try_from(adm.len()).map_err(|_| miette!("column len"))?;
        let mut expect: Vec<u32> = (0..len_u32).collect();
        let resolved: Vec<&[u8]> = {
            let mut out = Vec::with_capacity(adm.len());
            for i in 0..adm.len() {
                out.push(adm.resolve(i).into_diagnostic()?);
            }
            out
        };
        expect.sort_by(|&i, &j| {
            let ii = crate::value::convert::usize_from_u32(i);
            let jj = crate::value::convert::usize_from_u32(j);
            resolved[ii].cmp(resolved[jj])
        });
        // Stable comparison: resolve-order and permutation order agree
        // pairwise (indices with equal values may swap; compare values).
        let by_perm: Vec<&[u8]> = {
            let mut out = Vec::with_capacity(perm.len());
            for &i in &perm {
                out.push(resolved[crate::value::convert::usize_from_u32(i)]);
            }
            out
        };
        let by_expect: Vec<&[u8]> = {
            let mut out = Vec::with_capacity(expect.len());
            for &i in &expect {
                out.push(resolved[crate::value::convert::usize_from_u32(i)]);
            }
            out
        };
        assert_eq!(by_perm, by_expect);
        Ok(())
    }

    #[test]
    fn tail_bearing_columns_leave_the_fast_lane_and_still_order_correctly() -> Result<()> {
        let mut arena = Arena::new();
        intern_num(&mut arena, 10)?;
        arena.seal().into_diagnostic()?;
        // Mixed: sealed hit + fresh tail codes, deliberately out of
        // arrival order vs value order.
        let stamps = vec![
            intern_num(&mut arena, 10)?,         // sealed
            str_datum_stamp(&mut arena, "zzz")?, // tail
            intern_num(&mut arena, -7)?,         // tail — numerically before 10
        ];
        let f = arena.frame();
        let mut col = CodeColumn::new_in(&f);
        for sc in stamps {
            col.push(sc).into_diagnostic()?;
        }
        let adm = col.admit(&f).into_diagnostic()?;
        assert!(!adm.all_sealed());
        assert!(adm.raw_sealed().is_none());
        let perm = adm.sort_permutation().into_diagnostic()?;
        let mut sorted = Vec::with_capacity(perm.len());
        for &i in &perm {
            sorted.push(
                adm.resolve(crate::value::convert::usize_from_u32(i))
                    .into_diagnostic()?,
            );
        }
        let mut expect = sorted.clone();
        expect.sort();
        assert_eq!(sorted, expect, "tail path diverged from byte order");
        // -7 (Num) sorts before 10 (Num) sorts before "zzz" (Str tag).
        assert_eq!(perm[0], 2);
        assert_eq!(perm[1], 0);
        assert_eq!(perm[2], 1);
        Ok(())
    }

    // ------------------------------------------------------------------
    // The gather law: consuming, value-preserving, order-preserving,
    // and the only path to a new-epoch container.
    // ------------------------------------------------------------------

    #[test]
    fn gather_preserves_values_and_sortedness_and_readmits() -> Result<()> {
        let mut arena = Arena::new();
        let mut stamps = Vec::with_capacity(50);
        for i in 0..50 {
            stamps.push(intern_num(&mut arena, i * 3 % 17)?);
        }
        let mut col = CodeColumn::new_in(&arena.frame());
        for sc in stamps {
            col.push(sc).into_diagnostic()?;
        }
        // Record values + a sorted-by-value permutation before the seal.
        let before: Vec<Vec<u8>> = {
            let f = arena.frame();
            let adm = col.admit(&f).into_diagnostic()?;
            let mut out = Vec::with_capacity(adm.len());
            for i in 0..adm.len() {
                out.push(adm.resolve(i).into_diagnostic()?.to_vec());
            }
            out
        };
        let remap = arena.seal().into_diagnostic()?;
        let col = col.gather(&remap).into_diagnostic()?;
        let f = arena.frame();
        let adm = col.admit(&f).into_diagnostic()?; // readmits in the new epoch
        for (i, b) in before.iter().enumerate() {
            assert_eq!(
                adm.resolve(i).into_diagnostic()?,
                b.as_slice(),
                "gather moved a value"
            );
        }
        // Monotone remap: a sorted sealed container stays sorted.
        let mut arena2 = Arena::new();
        let mut sorted_col = CodeColumn::new_in(&arena2.frame());
        for i in 0..20 {
            let sc = intern_num(&mut arena2, i)?;
            sorted_col.push(sc).into_diagnostic()?;
        }
        let r1 = arena2.seal().into_diagnostic()?;
        let sorted_col = sorted_col.gather(&r1).into_diagnostic()?;
        // Everything was tail (arrival = insertion order 0..20 which is
        // also value order); after seal, codes are sealed ranks.
        intern_num(&mut arena2, -1000)?; // will re-rank on next seal
        let r2 = arena2.seal().into_diagnostic()?;
        let sorted_col = sorted_col.gather(&r2).into_diagnostic()?;
        let f2 = arena2.frame();
        let adm2 = sorted_col.admit(&f2).into_diagnostic()?;
        let raw = adm2.raw_sealed().ok_or_else(|| miette!("raw_sealed"))?;
        assert!(
            raw.windows(2).all(|w| w[0] < w[1]),
            "monotone gather broke sortedness"
        );
        Ok(())
    }

    #[test]
    fn gather_refuses_the_wrong_remap() -> Result<()> {
        let mut arena = Arena::new();
        let sc = intern_num(&mut arena, 1)?;
        let mut col = CodeColumn::new_in(&arena.frame());
        col.push(sc).into_diagnostic()?;
        let r1 = arena.seal().into_diagnostic()?;
        let col = col.gather(&r1).into_diagnostic()?;
        let r2_skipped = arena.seal().into_diagnostic()?;
        let skipped_advanced = r2_skipped.target_epoch() != r2_skipped.source_epoch();
        let r3 = arena.seal().into_diagnostic()?;
        assert!(skipped_advanced || r3.target_epoch() != r3.source_epoch());
        // col is at epoch 1; r3 reads epoch 2.
        assert!(
            matches!(col.gather(&r3), Err(Denial::EpochMismatch { .. })),
            "wrong-epoch remap must refuse typed"
        );
        Ok(())
    }

    // ------------------------------------------------------------------
    // Word columns: inline words free, wide words stamped, gather
    // rewrites handles only.
    // ------------------------------------------------------------------

    #[test]
    fn word_column_holds_mixed_residency_and_gathers() -> Result<()> {
        let mut arena = Arena::new();
        let mut col = WordColumn::new_in(&arena.frame());
        let small = encode(Datum::Str("hi"));
        let big = encode(Datum::Str("a string well past the inline max"));
        col.push(Value::mint(&small, &mut arena).into_diagnostic()?)
            .into_diagnostic()?;
        col.push(Value::mint(&big, &mut arena).into_diagnostic()?)
            .into_diagnostic()?;
        {
            let f = arena.frame();
            let adm = col.admit(&f).into_diagnostic()?;
            assert_eq!(adm.canonical(0).into_diagnostic()?, small.as_bytes());
            assert_eq!(adm.canonical(1).into_diagnostic()?, big.as_bytes());
            assert_eq!(
                adm.cmp_at(0, 1).into_diagnostic()?,
                small.as_bytes().cmp(big.as_bytes())
            );
        }
        let remap = arena.seal().into_diagnostic()?;
        let col = col.gather(&remap).into_diagnostic()?;
        let f = arena.frame();
        let adm = col.admit(&f).into_diagnostic()?;
        assert_eq!(adm.canonical(0).into_diagnostic()?, small.as_bytes());
        assert_eq!(
            adm.canonical(1).into_diagnostic()?,
            big.as_bytes(),
            "gather moved a word's value"
        );
        Ok(())
    }

    #[test]
    fn word_column_write_door_refuses_stale_wide_words() -> Result<()> {
        let mut arena = Arena::new();
        let big = encode(Datum::Str("a string well past the inline max"));
        let minted = Value::mint(&big, &mut arena).into_diagnostic()?;
        arena.seal().into_diagnostic()?;
        let mut col = WordColumn::new_in(&arena.frame());
        assert!(
            matches!(col.push(minted), Err(Denial::EpochMismatch { .. })),
            "stale wide word must refuse typed"
        );
        Ok(())
    }
}
