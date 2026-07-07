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
//!   [`Value::same_word`] (physical 16-byte identity, which is value
//!   identity only within one stamped context).

// #119 execution-currency foundation / naive oracle: exercised by its own tests (and, for
// laws, by runtime/verify.rs); #120 wires the foundation into the RA engine. dead_code is
// target-split (used in one target, dead in another), so #[expect] cannot be satisfied uniformly.
#![allow(dead_code)]

use std::cmp::Ordering;

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
    /// the [`Minted::Wide`] arm carries the context stamp the caller's
    /// container must hold — the word itself cannot.
    pub fn mint(cb: &CanonicalBytes, arena: &mut Arena) -> Minted {
        let canonical = cb.as_bytes();
        debug_assert!(
            !canonical.is_empty(),
            "CanonicalBytes witness is never empty"
        );
        let payload = &canonical[1..];
        let mut bytes = [0u8; 16];
        bytes[0] = canonical[0];
        if payload.len() <= INLINE_MAX {
            bytes[1] = payload.len() as u8;
            bytes[2..2 + payload.len()].copy_from_slice(payload);
            Minted {
                value: Value { bytes },
                stamp: None,
            }
        } else {
            let sc = arena.intern(canonical);
            bytes[1] = LEN_OUT_OF_LINE;
            bytes[2..6].copy_from_slice(&cb.prefix4());
            bytes[6..10].copy_from_slice(&sc.code().raw().to_be_bytes());
            Minted {
                value: Value { bytes },
                stamp: Some(sc),
            }
        }
    }

    pub fn tag(self) -> Tag {
        Tag::from_byte(self.bytes[0]).expect("a Value carries a lawful tag by construction")
    }

    pub fn is_inline(self) -> bool {
        self.bytes[1] != LEN_OUT_OF_LINE
    }

    /// The inline canonical payload (the bytes after the tag), if inline.
    pub fn inline_payload(&self) -> Option<&[u8]> {
        if self.is_inline() {
            Some(&self.bytes[2..2 + self.bytes[1] as usize])
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
            Some(Code(u32::from_be_bytes(
                self.bytes[6..10].try_into().expect("4 bytes"),
            )))
        }
    }

    /// The shared 4-byte prefix of the value's canonical encoding — the
    /// ONE prefix doctrine: identical whether the value is inline
    /// (computed) or out of line (stored at mint from the same function).
    pub fn prefix4(self) -> [u8; 4] {
        if self.is_inline() {
            let mut p = [0u8; 4];
            let n = (1 + self.bytes[1] as usize).min(4);
            p[..1].copy_from_slice(&self.bytes[..1]);
            if n > 1 {
                p[1..n].copy_from_slice(&self.bytes[2..2 + n - 1]);
            }
            p
        } else {
            self.bytes[2..6].try_into().expect("4 bytes")
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
    pub(super) fn gathered(self, remap: &super::arena::EpochRemap) -> Value {
        match self.code() {
            None => self,
            Some(code) => {
                let mut bytes = self.bytes;
                bytes[6..10].copy_from_slice(&remap.apply_raw(code).raw().to_be_bytes());
                Value { bytes }
            }
        }
    }

    /// Physical 16-byte identity: value identity ONLY within one stamped
    /// context (one arena, one epoch) — the container's law, not the
    /// word's.
    pub fn same_word(&self, other: &Value) -> bool {
        self.bytes == other.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::super::canonical::{Datum, decode, encode};
    use super::super::number::Num;
    use super::*;

    fn strd(s: &str) -> CanonicalBytes {
        encode(Datum::Str(s))
    }

    #[test]
    fn residency_law_is_a_pure_function_of_canonical_length() {
        let mut arena = Arena::new();
        // Canonical payload for a clean string of n chars is n + 2
        // (terminator), so 12 chars = 14 payload bytes = last inline size.
        let inline_edge = Value::mint(&strd("abcdefghijkl"), &mut arena);
        assert!(inline_edge.is_inline());
        let outline_edge = Value::mint(&strd("abcdefghijklm"), &mut arena);
        assert!(!outline_edge.is_inline());
        // Minting twice produces the identical word (deterministic
        // residency AND deterministic code via arena dedup).
        let again = Value::mint(&strd("abcdefghijklm"), &mut arena);
        assert!(outline_edge.value().same_word(&again.value()));
    }

    /// The per-kind residency table, pinned: residency is
    /// format-significant, so which kinds can never / always / sometimes
    /// inline is a law, not an accident.
    #[test]
    fn residency_table_by_kind_is_pinned() {
        use super::super::wide::interval::{Bound, Interval};
        use super::super::wide::json::Json;
        use super::super::wide::validity::Validity;
        let mut arena = Arena::new();
        let inline =
            |cb: &CanonicalBytes, arena: &mut Arena| -> bool { Value::mint(cb, arena).is_inline() };
        // Always inline: Null, Bool, every Num (payload <= 13), Validity
        // (payload 9), empty/half-bounded intervals (payload <= 11).
        assert!(inline(&encode(Datum::Null), &mut arena));
        assert!(inline(&encode(Datum::Bool(true)), &mut arena));
        assert!(inline(&encode(Datum::Num(Num::int(i64::MIN))), &mut arena));
        assert!(inline(
            &encode(Datum::Num(Num::float(f64::MAX))),
            &mut arena
        ));
        assert!(inline(
            &encode(Datum::Validity(Validity::new(i64::MAX, false))),
            &mut arena
        ));
        assert!(inline(
            &encode(Datum::Interval(Interval::EMPTY)),
            &mut arena
        ));
        assert!(inline(
            &encode(Datum::Interval(Interval::new(
                Bound::Closed(7),
                Bound::Unbounded
            ))),
            &mut arena
        ));
        // Never inline: Uuid (payload 16) and two-sided finite intervals
        // (payload 19).
        assert!(!inline(&encode(Datum::Uuid([0u8; 16])), &mut arena));
        assert!(!inline(
            &encode(Datum::Interval(Interval::new(
                Bound::Closed(1),
                Bound::Closed(2)
            ))),
            &mut arena
        ));
        // Length-dependent: empty collections inline; Json's tiniest value
        // (1 value byte + 8 hash bytes = 9 payload) still inlines.
        assert!(inline(&encode(Datum::List(&[])), &mut arena));
        assert!(inline(&encode(Datum::Set(&[])), &mut arena));
        assert!(inline(&encode(Datum::Json(&Json::Null)), &mut arena));
    }

    #[test]
    fn inline_round_trip_reconstructs_canonical_bytes() {
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
            let m = Value::mint(&cb, &mut arena);
            let v = m.value();
            assert!(v.is_inline(), "small value went out of line");
            assert!(m.stamp().is_none());
            assert_eq!(v.inline_canonical().expect("inline"), cb.as_bytes());
            assert_eq!(v.tag().byte(), cb.as_bytes()[0]);
        }
    }

    #[test]
    fn outline_resolves_through_an_observer_to_the_same_canonical_bytes() {
        let mut arena = Arena::new();
        let big: Vec<Datum> = (0..40).map(|_| Datum::Num(Num::int(7))).collect();
        let cb = encode(Datum::List(&big));
        let m = Value::mint(&cb, &mut arena);
        let v = m.value();
        assert!(!v.is_inline());
        let sc = m.stamp().expect("outline mints a stamp");
        assert_eq!(v.code(), Some(sc.code()));
        let f = arena.frame();
        let resolved = f.resolve(sc);
        assert_eq!(
            resolved,
            cb.as_bytes(),
            "arena holds the full canonical bytes"
        );
        assert!(decode(resolved).is_ok());
        // The ONE prefix doctrine: word prefix == canonical prefix.
        assert_eq!(v.prefix4(), cb.prefix4());
    }

    #[test]
    fn storage_cmp_decides_exactly_what_the_word_knows() {
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
        let words: Vec<(Value, CanonicalBytes)> = corpus
            .into_iter()
            .map(|cb| (Value::mint(&cb, &mut arena).value(), cb))
            .collect();
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
    }

    #[test]
    fn same_word_is_physical_not_semantic() {
        let mut arena_a = Arena::new();
        let mut arena_b = Arena::new();
        let big_x = encode(Datum::Str("xxxxxxxxxxxxxxxxxxxx"));
        let big_y = encode(Datum::Str("xxxxxxxxxxxxxxxxxxxy"));
        // Same prefix, different values, DIFFERENT arenas: both get code
        // 0, producing identical words — exactly why same_word is only
        // value identity under one stamped context.
        let va = Value::mint(&big_x, &mut arena_a).value();
        let vb = Value::mint(&big_y, &mut arena_b).value();
        assert!(va.same_word(&vb), "the trap this API's name warns about");
        assert_eq!(va.try_cmp_storage(&vb), None, "and storage cmp refuses it");
    }
}
