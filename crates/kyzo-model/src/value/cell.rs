/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `Value`: the 16-byte tagged cell — tag+length header, then inline canonical payload or a 4-byte prefix + arena handle; prefix-first compare, deref only on tie.
//!
//! ## The physical word (format-internal layout, not on-disk)
//!
//! ```text
//! byte 0      : tag byte (the canonical encoding's first byte)
//! byte 1      : payload length 0..=14, or 0xFF = out of line
//! bytes 2..16 : inline — the canonical payload (the bytes after the tag)
//!               out of line — prefix4 of the FULL canonical encoding
//!               (bytes 2..6), the Code big-endian (bytes 6..10), zeros
//!               (bytes 10..16)
//! ```
//!
//! **The residency law**: a value is inline iff its canonical payload fits
//! (≤ 14 bytes) — a pure function of the canonical bytes, so one value can
//! never appear both ways. Residency is never identity.
//!
//! ## The authority discipline (this cell is not a hidden oracle)
//!
//! - Inline cells may use **local byte identity and storage-order
//!   comparison** where the tag law permits — and that is *storage*
//!   order; expression-level `<` stays a separate refusable authority
//!   even for inline values.
//! - Out-of-line cells **cannot** deref or compare-by-value locally: the
//!   16-byte word cannot hold an (arena, epoch) stamp, so out-of-line
//!   identity is only lawful under the stamp carried by the observer or
//!   container the cell sits in. [`Value::mint`] therefore returns the
//!   stamp *beside* the word, for the container to carry.
//! - Accordingly there is **no** `PartialEq`/`Eq`/`Ord`/`Hash` on
//!   `Value`: any of them would either lie across contexts or secretly
//!   consult one. What exists is exact and named:
//!   [`Value::try_cmp_storage`] (decides only what local information can
//!   lawfully decide, `None` otherwise — never a deref) and
//!   [`Value::same_word`] (physical 16-byte identity under a proven
//!   [`Admission`](super::admission::Admission) — without the token the call
//!   does not compile; the token is mint-checked / unbranded because
//!   words from coexisting arenas must share one compare API — nest
//!   brands live on [`Frame::with_nested_ctx`](super::arena::Frame::with_nested_ctx);
//!   refusal is typed [`Denial`](super::admission::Denial), never a bare bool).

use std::cmp::Ordering;

use super::admission::{Admission, Denial};
use super::arena::Arena;
use super::canonical::CanonicalBytes;
use super::code::{Code, StampedCode};
use super::tag::Tag;

/// Maximum inline canonical-payload bytes (after the tag byte).
pub const INLINE_MAX: usize = 14;

const LEN_OUT_OF_LINE: u8 = 0xFF;

/// The 16-byte cell. See the module docs for layout and the authority
/// discipline. `Copy` and POD by design: this is the execution currency's
/// word, moved by the million.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct Value {
    bytes: [u8; 16],
}

// The representation contract, not just the size: `repr(transparent)`
// over `[u8; 16]` pins layout and alignment for raw copies, mmap, and the
// FFI boundaries the bindings will cross. POD means "a minted word is
// raw-copyable", NEVER "any 16 bytes are a Value": no `from_bytes`
// exists, and the only mints are `Value::mint` and the plane-internal
// `gathered` — arbitrary bytes need a validation door that deliberately
// does not exist yet.
const _: () = assert!(std::mem::size_of::<Value>() == 16);
const _: () = assert!(std::mem::align_of::<Value>() == 1);

/// The result of minting a word: the stamp is structurally inseparable
/// from an out-of-line value — private fields, minted ONLY by
/// [`Value::mint`], so a wide word can never be paired with a dropped or
/// unrelated stamp (the coherence is by construction, not by check).
/// `#[must_use]` refuses silent drops; container write doors consume it.
#[must_use = "an out-of-line word without its stamp is unspendable; carry the stamp"]
pub struct Minted {
    value: Value,
    stamp: Option<StampedCode>,
}

impl Minted {
    pub fn value(&self) -> Value {
        self.value
    }

    /// `Some` exactly for out-of-line words: the context stamp their
    /// container must carry.
    pub fn stamp(&self) -> Option<StampedCode> {
        self.stamp
    }

    pub fn is_inline(&self) -> bool {
        self.stamp.is_none()
    }
}

impl Value {
    /// Mint the physical word for a canonical value. Inline values are
    /// self-contained; out-of-line values intern through the arena, and
    /// the [`Minted`] stamp arm carries the context stamp the caller's
    /// container must hold — the word itself cannot.
    ///
    /// Wide path refuses with [`Denial::ExtentOverflow`] when the arena
    /// cannot admit another distinct value or the canonical bytes exceed
    /// `u32` span space. An empty [`CanonicalBytes`] witness (corrupt —
    /// encode never mints one) refuses with
    /// [`Denial::BookkeepingBroken`] — never a process abort.
    pub fn mint(cb: &CanonicalBytes, arena: &mut Arena) -> Result<Minted, Denial> {
        let canonical = cb.as_bytes();
        // CanonicalBytes is mint-only via encode; an empty witness is corrupt
        // bookkeeping — typed refuse, never a slice-index panic.
        let Some((&tag, payload)) = canonical.split_first() else {
            return Err(Denial::BookkeepingBroken);
        };
        let mut bytes = [0u8; 16];
        bytes[0] = tag;
        if payload.len() <= INLINE_MAX {
            bytes[1] = match u8::try_from(payload.len()) {
                Ok(n) => n,
                // INLINE_MAX is 14; length here always fits u8.
                Err(_overflow) => {
                    return Err(Denial::BookkeepingBroken);
                }
            };
            bytes[2..2 + payload.len()].copy_from_slice(payload);
            Ok(Minted {
                value: Value { bytes },
                stamp: None,
            })
        } else {
            let sc = arena.intern(canonical)?;
            bytes[1] = LEN_OUT_OF_LINE;
            bytes[2..6].copy_from_slice(&cb.prefix4());
            bytes[6..10].copy_from_slice(&sc.code().raw().to_be_bytes());
            Ok(Minted {
                value: Value { bytes },
                stamp: Some(sc),
            })
        }
    }

    /// Tag byte of a minted word. Lawful by construction: only
    /// [`Value::mint`] / [`Value::gathered`] write the word, and they
    /// copy the tag from a [`CanonicalBytes`] witness (itself mint-only
    /// via encode). An illegal tag is unrepresentable without forging the
    /// private `[u8; 16]` (no `from_bytes` door) — forged bytes order as
    /// [`Tag::Null`] rather than aborting the process.
    pub fn tag(self) -> Tag {
        match Tag::from_byte(self.bytes[0]) {
            Some(tag) => tag,
            None => Tag::Null,
        }
    }

    pub fn is_inline(self) -> bool {
        self.bytes[1] != LEN_OUT_OF_LINE
    }

    /// The inline canonical payload (the bytes after the tag), if inline.
    pub fn inline_payload(&self) -> Option<&[u8]> {
        if self.is_inline() {
            Some(&self.bytes[2..2 + usize::from(self.bytes[1])])
        } else {
            None
        }
    }

    /// Reconstruct the full canonical bytes of an inline value.
    pub fn inline_canonical(&self) -> Option<Vec<u8>> {
        self.inline_payload().map(|p| {
            let mut v = Vec::with_capacity(1 + p.len());
            v.push(self.bytes[0]);
            v.extend_from_slice(p);
            v
        })
    }

    /// The raw arena handle of an out-of-line value — identity only,
    /// spendable solely under the context's stamp.
    pub fn code(self) -> Option<Code> {
        if self.is_inline() {
            None
        } else {
            let mut raw = [0u8; 4];
            raw.copy_from_slice(&self.bytes[6..10]);
            Some(Code(u32::from_be_bytes(raw)))
        }
    }

    /// The shared 4-byte prefix of the value's canonical encoding — the
    /// ONE prefix doctrine: identical whether the value is inline
    /// (computed) or out of line (stored at mint from the same function).
    pub fn prefix4(self) -> [u8; 4] {
        if self.is_inline() {
            let mut p = [0u8; 4];
            let n = (1 + usize::from(self.bytes[1])).min(4);
            p[..1].copy_from_slice(&self.bytes[..1]);
            if n > 1 {
                p[1..n].copy_from_slice(&self.bytes[2..2 + n - 1]);
            }
            p
        } else {
            let mut p = [0u8; 4];
            p.copy_from_slice(&self.bytes[2..6]);
            p
        }
    }

    /// Storage-order comparison using only what the word lawfully knows:
    ///
    /// - different tags: decided (tag byte order is the cross-type law);
    /// - both inline: decided (full canonical bytes are present);
    /// - differing prefixes: decided (first four canonical bytes differ);
    /// - otherwise `None`: deciding would need a deref, and a deref needs
    ///   an observer. This function NEVER dereferences — same-code
    ///   equality included, because equal words in different epochs may
    ///   denote different values.
    pub fn try_cmp_storage(&self, other: &Value) -> Option<Ordering> {
        if self.bytes[0] != other.bytes[0] {
            return Some(self.bytes[0].cmp(&other.bytes[0]));
        }
        if let (Some(pa), Some(pb)) = (self.inline_payload(), other.inline_payload()) {
            return Some(pa.cmp(pb));
        }
        let (pa, pb) = (self.prefix4(), other.prefix4());
        if pa != pb { Some(pa.cmp(&pb)) } else { None }
    }

    /// Plane-internal: the gather door's per-word step — rewrite an
    /// out-of-line word's code through the epoch remap (the value, its
    /// tag, and its prefix are unchanged; only the handle moves). Inline
    /// words pass through untouched.
    ///
    /// CONTAINMENT: lawful ONLY inside a container gather that has
    /// already verified arena + epoch against the remap — called bare,
    /// this is the raw-code remap smuggle at word level. Its only caller
    /// is `WordColumn::gather`; keep it that way.
    pub(super) fn gathered(self, remap: &super::arena::EpochRemap) -> Result<Value, Denial> {
        match self.code() {
            None => Ok(self),
            Some(code) => {
                let mapped = remap.apply_raw(code)?;
                let mut bytes = self.bytes;
                bytes[6..10].copy_from_slice(&mapped.raw().to_be_bytes());
                Ok(Value { bytes })
            }
        }
    }

    /// Physical 16-byte word identity under a proven shared [`Admission`].
    /// Value identity only when both words were minted in that context —
    /// without the token this call does not compile, so cross-context
    /// comparison cannot affirmatively lie.
    ///
    /// **Coexisting-arena boundary:** takes the unbranded durable
    /// [`Admission`] (mint via [`Admission::prove_shared`] /
    /// [`Admission::from_observer`]). Nest-branded compare under one live
    /// frame uses [`NestedDomainCtx`](super::admission::NestedDomainCtx) /
    /// [`Frame::with_nested_ctx`](super::arena::Frame::with_nested_ctx)
    /// and projects `.ctx()` when calling here.
    #[inline]
    pub fn same_word(&self, other: &Value, _ctx: &Admission) -> bool {
        self.bytes == other.bytes
    }
}

#[cfg(test)]
mod tests {
    use miette::{IntoDiagnostic, Result, miette};

    use super::super::arena::BulkObserver;
    use super::super::canonical::{Datum, decode, encode};
    use super::super::number::Num;
    use super::*;

    fn strd(s: &str) -> CanonicalBytes {
        encode(Datum::Str(s))
    }

    #[test]
    fn residency_law_is_a_pure_function_of_canonical_length() -> Result<()> {
        let mut arena = Arena::new();
        // Canonical payload for a clean string of n chars is n + 2
        // (terminator), so 12 chars = 14 payload bytes = last inline size.
        let inline_edge = Value::mint(&strd("abcdefghijkl"), &mut arena).into_diagnostic()?;
        assert!(inline_edge.is_inline());
        let outline_edge = Value::mint(&strd("abcdefghijklm"), &mut arena).into_diagnostic()?;
        assert!(!outline_edge.is_inline());
        // Minting twice produces the identical word (deterministic
        // residency AND deterministic code via arena dedup).
        let again = Value::mint(&strd("abcdefghijklm"), &mut arena).into_diagnostic()?;
        // Nest brand under one live frame; project durable Admission for
        // same_word (coexisting-arena API).
        let ok = arena
            .frame()
            .with_nested_ctx(|nest| outline_edge.value().same_word(&again.value(), &nest.ctx()));
        assert!(ok);
        Ok(())
    }

    /// The per-kind residency table, pinned: residency is
    /// format-significant, so which kinds can never / always / sometimes
    /// inline is a law, not an accident.
    #[test]
    fn residency_table_by_kind_is_pinned() -> Result<()> {
        use super::super::kind::interval::{Bound, Interval};
        use super::super::kind::json::Json;
        use super::super::kind::validity::{Validity, ValidityTs};
        let mut arena = Arena::new();
        let inline = |cb: &CanonicalBytes, arena: &mut Arena| -> Result<bool> {
            Ok(Value::mint(cb, arena).into_diagnostic()?.is_inline())
        };
        // Always inline: Null, Bool, every Num (payload <= 13), Validity
        // (payload 9), empty/half-bounded intervals (payload <= 11).
        assert!(inline(&encode(Datum::Null), &mut arena)?);
        assert!(inline(&encode(Datum::Bool(true)), &mut arena)?);
        assert!(inline(&encode(Datum::Num(Num::int(i64::MIN))), &mut arena)?);
        assert!(inline(
            &encode(Datum::Num(Num::float(f64::MAX))),
            &mut arena
        )?);
        assert!(inline(
            &encode(Datum::Validity(
                Validity::new(ValidityTs::of_micros(i64::MAX), false)
                    .ok_or_else(|| miette!("retract admits every tick"))?,
            )),
            &mut arena
        )?);
        assert!(inline(
            &encode(Datum::Interval(Interval::EMPTY)),
            &mut arena
        )?);
        assert!(inline(
            &encode(Datum::Interval(Interval::new(
                Bound::Closed(7),
                Bound::Unbounded
            ))),
            &mut arena
        )?);
        // Never inline: Uuid (payload 16) and two-sided finite intervals
        // (payload 19).
        assert!(!inline(&encode(Datum::Uuid([0u8; 16])), &mut arena)?);
        assert!(!inline(
            &encode(Datum::Interval(Interval::new(
                Bound::Closed(1),
                Bound::Closed(2)
            ))),
            &mut arena
        )?);
        // Length-dependent: empty collections inline; Json's tiniest value
        // (1 value byte + 8 hash bytes = 9 payload) still inlines.
        assert!(inline(&encode(Datum::List(&[])), &mut arena)?);
        assert!(inline(&encode(Datum::Set(&[])), &mut arena)?);
        assert!(inline(&encode(Datum::Json(&Json::Null)), &mut arena)?);
        Ok(())
    }

    #[test]
    fn inline_round_trip_reconstructs_canonical_bytes() -> Result<()> {
        let mut arena = Arena::new();
        for d in [
            Datum::Null,
            Datum::Bool(true),
            Datum::Num(Num::int(42)),
            Datum::Num(Num::float(-1.5)),
            Datum::Str(""),
            Datum::Str("a\u{0}b"),
        ] {
            let cb = encode(d);
            let m = Value::mint(&cb, &mut arena).into_diagnostic()?;
            let v = m.value();
            assert!(v.is_inline(), "small value went out of line");
            assert!(m.stamp().is_none());
            assert_eq!(v.inline_canonical().ok_or_else(|| miette!("inline"))?, cb.as_bytes());
            assert_eq!(v.tag().byte(), cb.as_bytes()[0]);
        }
        Ok(())
    }

    #[test]
    fn outline_resolves_through_an_observer_to_the_same_canonical_bytes() -> Result<()> {
        let mut arena = Arena::new();
        let big: Vec<Datum> = (0..40).map(|_| Datum::Num(Num::int(7))).collect();
        let cb = encode(Datum::List(&big));
        let m = Value::mint(&cb, &mut arena).into_diagnostic()?;
        let v = m.value();
        assert!(!v.is_inline());
        let sc = m.stamp().ok_or_else(|| miette!("outline mints a stamp"))?;
        assert_eq!(v.code().map(Code::raw), Some(sc.code().raw()));
        let f = arena.frame();
        let resolved = f.resolve(sc).into_diagnostic()?;
        assert_eq!(
            resolved,
            cb.as_bytes(),
            "arena holds the full canonical bytes"
        );
        assert!(decode(resolved).is_ok());
        // The ONE prefix doctrine: word prefix == canonical prefix.
        assert_eq!(v.prefix4(), cb.prefix4());
        Ok(())
    }

    #[test]
    fn storage_cmp_decides_exactly_what_the_word_knows() -> Result<()> {
        let mut arena = Arena::new();
        let corpus: Vec<CanonicalBytes> = vec![
            encode(Datum::Null),
            encode(Datum::Bool(false)),
            encode(Datum::Num(Num::int(-3))),
            encode(Datum::Num(Num::float(2.5))),
            strd(""),
            strd("a"),
            strd("abcdefghijkl"),      // inline edge
            strd("abcdefghijklm"),     // outline edge
            strd("abcdefghijklmnopq"), // outline, shares prefix with above
            strd("zzzzzzzzzzzzzzzzz"), // outline, distinct prefix
        ];
        let mut words: Vec<(Value, CanonicalBytes)> = Vec::with_capacity(corpus.len());
        for cb in corpus {
            words.push((Value::mint(&cb, &mut arena).into_diagnostic()?.value(), cb));
        }
        for (va, ca) in &words {
            for (vb, cb) in &words {
                let truth = ca.as_bytes().cmp(cb.as_bytes());
                match va.try_cmp_storage(vb) {
                    // Whatever the word decides must be the canonical
                    // storage order.
                    Some(o) => assert_eq!(o, truth, "word lied: {ca:?} vs {cb:?}"),
                    // Refusal is lawful only when locality genuinely
                    // cannot decide: same tag, equal prefixes, not both
                    // inline.
                    None => {
                        assert_eq!(va.tag(), vb.tag());
                        assert_eq!(va.prefix4(), vb.prefix4());
                        assert!(!(va.is_inline() && vb.is_inline()));
                    }
                }
            }
        }
        Ok(())
    }

    /// Cross-arena mint cannot obtain a shared [`Admission`], so the old
    /// trap — calling `same_word` across contexts and getting a lying
    /// `true` — cannot be written. Physical identity remains only under a
    /// proven token; storage cmp still refuses the unresolved prefix tie.
    #[test]
    fn same_word_requires_shared_admission() -> Result<()> {
        use super::super::admission::Denial;
        let mut arena_a = Arena::new();
        let mut arena_b = Arena::new();
        let big_x = encode(Datum::Str("xxxxxxxxxxxxxxxxxxxx"));
        let big_y = encode(Datum::Str("xxxxxxxxxxxxxxxxxxxy"));
        // Same prefix, different values, DIFFERENT arenas: both get code
        // 0, producing identical words — the trap the unproven API told.
        let va = Value::mint(&big_x, &mut arena_a).into_diagnostic()?.value();
        let vb = Value::mint(&big_y, &mut arena_b).into_diagnostic()?.value();
        assert_eq!(va.try_cmp_storage(&vb), None, "storage cmp refuses it");
        let fa = arena_a.frame();
        let fb = arena_b.frame();
        assert!(
            matches!(
                Admission::prove_shared(
                    fa.bulk_arena(),
                    fa.bulk_epoch(),
                    fb.bulk_arena(),
                    fb.bulk_epoch(),
                ),
                Err(Denial::ArenaMismatch { .. })
            ),
            "cross-arena prove_shared must refuse — no token, no same_word"
        );
        // Under one arena, identical minting yields same_word.
        let again = Value::mint(&big_x, &mut arena_a).into_diagnostic()?.value();
        let ctx = Admission::from_observer(&arena_a.frame());
        assert!(va.same_word(&again, &ctx));
        Ok(())
    }
}
