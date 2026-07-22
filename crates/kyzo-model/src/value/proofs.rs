/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Compile-time ABSENCE proofs for the value plane's authority model,
//! plus near-exhaustive runtime campaigns for the one-law encode /
//! comparator layer (Tag prefixes, sentinel-free unbounded encodings,
//! cross-type total order).
//!
//! The authority types are only sound if the forbidden operations are
//! *unrepresentable*, not merely un-called. Every `const _` below is a
//! compile-time assertion that a type does NOT implement a trait; if a
//! future change ever added that impl, this module would stop compiling.
//! They run in every build (not just tests), so the negatives are locked
//! for good.
//!
//! What construction-from-nothing these close: a type with no public
//! constructor and a private field cannot be built outside its own module
//! (the compiler enforces field privacy). The proofs here add the
//! trait-shaped forge vectors that field privacy alone does not cover —
//! `Default` (conjure one), `From<RawShape>` (launder raw bytes into a
//! witness), and `Clone`/`Copy` (duplicate a one-per-admission or
//! one-per-mint token).
//!
//! ## Near-exhaustive one-law campaigns (`#[cfg(test)]`)
//!
//! `canonical.rs`'s order-embedding tests are semi-circular: they agree
//! `encode` with an in-file semantic mirror that tracks the same grammar.
//! A consistent-wrong change that updates both stays green. The campaigns
//! below pin the format-v1 literals (tag bytes, unbounded markers) as an
//! independent oracle over a small enumerable universe — the zone where
//! DST's random edge-bouncing is the wrong tool and exhaustive checking
//! is tractable.

use super::DataValue;
use super::Tuple;
use super::admission::{
    Admission, BulkSpendAuthority, Denial, DomainCtx, NestId, NestedAdmission, NestedDomainCtx,
    SpendAdmission,
};
use super::canonical::CanonicalBytes;
use super::cell::{Minted, Value};
use super::code::{Code, StampedCode};
use super::row::RelationId;
use super::string::MintedStr;

/// Assert that `$t` does NOT implement all of `$($tr)+`. Compiles iff the
/// type lacks the trait: two blanket impls stay unambiguous only while the
/// type is missing the trait; add the impl and the associated-item lookup
/// becomes ambiguous, which is a hard compile error.
#[macro_export]
macro_rules! assert_not_impl {

    ($t:ty: $($tr:path),+ $(,)?) => {
        const _: fn() = || {
            trait AmbiguousIfImpl<A> {
                fn __proof() {}
            }
            impl<T: ?Sized> AmbiguousIfImpl<()> for T {}
            // Underscored marker: type-parameter only for the ambiguity
            // trick; never constructed as a value.
            struct _Marker;
            impl<T: ?Sized $(+ $tr)+> AmbiguousIfImpl<_Marker> for T {}
            // Unresolvable (ambiguous) the moment `$t` implements the
            // traits; resolvable — this line compiles — only while it does
            // not.
            {
                let proof_fn = <$t as AmbiguousIfImpl<_>>::__proof;
                proof_fn();
            }
        };
    };
}

// The compile-time absence proof is a general utility (a build-time witness
// that a type lacks a capability), not value-plane-specific. It is the
// mechanism the staged-construction idiom uses to prove `build()` is absent
// on an incomplete typestate — exported crate-wide so those proofs live
// beside the builders they guard, never re-spelled.

// Code is identity ONLY: no order and no inherent equality/hash. Value
// order is the observer's through resolved bytes. Handle identity /
// identity-order under a proven context are `Admission::same_handle` /
// `Admission::cmp_identity` — `code_a == code_b` must not compile.
assert_not_impl!(Code: PartialOrd);
assert_not_impl!(Code: Ord);
assert_not_impl!(Code: PartialEq);
assert_not_impl!(Code: Eq);
assert_not_impl!(Code: std::hash::Hash);

// The 16-byte cell exposes no semantic equality or order TRAIT: comparison
// is `try_cmp_storage` (locality-only) and `same_word` under `&Admission`.
// A derived `Ord`/`Eq` would silently deref or misjudge; it must not exist.
assert_not_impl!(Value: PartialOrd);
assert_not_impl!(Value: Ord);
assert_not_impl!(Value: PartialEq);
assert_not_impl!(Value: Eq);

// Admission/Denial vocabulary: one discipline, opposite directions.
// Thin aliases DomainCtx / DomainCtxRefusal name the same types.
const _: fn() = || {
    fn admission_is_domain_ctx(a: Admission) -> DomainCtx {
        a
    }
    fn denial_is_refusal(d: Denial) -> super::admission::DomainCtxRefusal {
        d
    }
    fn nested_alias(n: NestedAdmission<'static>) -> NestedDomainCtx<'static> {
        n
    }
    fn spend_alias(s: SpendAdmission) -> BulkSpendAuthority {
        s
    }
    match (
        admission_is_domain_ctx,
        denial_is_refusal,
        nested_alias,
        spend_alias,
    ) {
        proof => core::mem::drop(proof),
    }
};

// An admission token cannot be conjured empty — only from_observer /
// prove_shared / plane-internal `at`. Durable re-checkable fact: Copy.
// Coexisting-arena form: deliberately unbranded (see Admission docs /
// code.rs measurement); nest brands are NestedDomainCtx under
// Frame/Snapshot::with_nested_ctx.
assert_not_impl!(Admission: Default);
assert_not_impl!(DomainCtx: Default);
assert_not_impl!(Denial: Default);

// Nest-branded admission cannot be conjured empty either — only the
// with_nested_ctx doors mint one (and HRTB keeps `'id` from escaping).
assert_not_impl!(NestedDomainCtx<'static>: Default);
assert_not_impl!(NestedAdmission<'static>: Default);

// Durable fact tokens are Copy (re-checkable, not consumable permission).
const _: fn() = || {
    fn needs_copy<T: Copy>() {}
    needs_copy::<Admission>();
    needs_copy::<DomainCtx>();
    needs_copy::<NestId<'static>>();
    needs_copy::<NestedDomainCtx<'static>>();
    needs_copy::<Denial>();
};

// A stamped code cannot be conjured: no `Default`. Its only mints are
// `Arena::intern` and `EpochRemap::apply`, both demanding the arena's
// private mint token.
assert_not_impl!(StampedCode: Default);

// Consumable permission: one-per-admission, move-only consume-on-spend.
// No `Clone`/`Copy`/`Default` — duplication would forge a second spend.
// Reuse after `resolve_raw`/`cmp_raw`/`open_pass` is a move error (E0382);
// these absence proofs are the static half of that refusal.
assert_not_impl!(BulkSpendAuthority: Clone);
assert_not_impl!(BulkSpendAuthority: Copy);
assert_not_impl!(BulkSpendAuthority: Default);

// A minted out-of-line word is one coherent (value, stamp) product: it
// cannot be duplicated to double-spend, nor conjured. Its only mint is
// `Value::mint`.
assert_not_impl!(Minted: Clone);
assert_not_impl!(Minted: Copy);
assert_not_impl!(Minted: Default);
assert_not_impl!(MintedStr: Clone);
assert_not_impl!(MintedStr: Copy);
assert_not_impl!(MintedStr: Default);

// Canonical bytes cannot be forged from arbitrary bytes: no
// `From<Vec<u8>>`, no `Default`. The only mint is `encode`/`encode_owned`,
// which establish the format law. (It DOES have `Ord` — that is the
// storage byte order, intended.)
assert_not_impl!(CanonicalBytes: From<Vec<u8>>);
assert_not_impl!(CanonicalBytes: From<&'static [u8]>);
assert_not_impl!(CanonicalBytes: Default);

// A relation id cannot bypass its allocation ceiling: no `From<u64>`, no
// `Default`. The only mints are `RelationId::new` (checked against `CAP`),
// `raw_decode` (same refusal), `SYSTEM`, and `next` (routed through
// decode). The tuple field is private, so `RelationId(x)` does not compile
// outside this crate either.
assert_not_impl!(RelationId: From<u64>);
assert_not_impl!(RelationId: Default);

// The decoded row is unforgeable: no blanket conversion from a bare value
// vector, no Deref into one. The only door is `Tuple::from_vec` (explicit).
// #300 T5 — if either impl ever appears, row authority dissolves.
assert_not_impl!(Tuple: From<Vec<DataValue>>);
assert_not_impl!(Tuple: std::ops::Deref);
assert_not_impl!(Tuple: std::ops::DerefMut);

#[cfg(test)]
mod tests {
    //! Compile-time absences above; runtime positives + near-exhaustive
    //! one-law campaigns below. Oracles are pinned format-v1 literals —
    //! not `DataValue::Ord` and not `canonical`'s in-file semantic mirror.
    use std::collections::BTreeSet;

    use miette::{IntoDiagnostic, Result, miette};

    use super::*;
    use crate::value::kind::interval::{Bound, Hi, Interval, Lo};
    use crate::value::kind::json::Json;
    use crate::value::kind::regex::{RegexFlags, RegexSource};
    use crate::value::kind::validity::{Validity, ValidityTs};
    use crate::value::number::Num;
    use crate::value::tag::{STRUCT_SEQ_END, STRUCT_STRING, Tag};
    use crate::value::{DataValue, Geometry, UuidWrapper, Vector, decode, encode_owned};

    /// Format v1 tag table — the independent oracle. A consistent-wrong
    /// retag of `Tag` + `encode` still fails here.
    const FORMAT_V1_TAGS: [(Tag, u8); 14] = [
        (Tag::Null, 0x05),
        (Tag::Bool, 0x08),
        (Tag::Num, 0x10),
        (Tag::Str, 0x18),
        (Tag::Bytes, 0x20),
        (Tag::Uuid, 0x28),
        (Tag::Regex, 0x30),
        (Tag::Json, 0x38),
        (Tag::Vector, 0x40),
        (Tag::List, 0x48),
        (Tag::Set, 0x50),
        (Tag::Validity, 0x58),
        (Tag::Interval, 0x60),
        (Tag::Geometry, 0x68),
    ];

    /// Interval body markers (format v1): empty / range form, and
    /// unbounded vs finite ends. Never `i64::MIN`/`i64::MAX` sentinels.
    const IV_EMPTY: u8 = 0x01;
    const IV_RANGE: u8 = 0x02;
    const END_UNBOUNDED: u8 = 0x01;
    const END_FINITE: u8 = 0x02;

    /// Ascending order-preserving i64 key (format v1): sign-bit flip,
    /// big-endian. Pinned here so the unbounded campaign can prove
    /// unbounded ends are NOT this encoding of `i64::MIN`/`MAX`.
    fn asc_ts_key(ts: i64) -> [u8; 8] {
        (ts ^ i64::MIN).cast_unsigned().to_be_bytes()
    }

    fn pinned_tag_byte(tag: Tag) -> Result<u8> {
        FORMAT_V1_TAGS
            .iter()
            .find(|(t, _)| *t == tag)
            .map(|(_, b)| *b)
            .ok_or_else(|| miette!("every Tag is in FORMAT_V1_TAGS"))
    }

    /// One representative per kind — the enumerable cross-type universe.
    fn one_per_kind() -> Result<[(Tag, DataValue); 14]> {
        let u = UuidWrapper::new(uuid::Uuid::from_bytes([0x11; 16]));
        Ok([
            (Tag::Null, DataValue::Null),
            (Tag::Bool, DataValue::Bool(false)),
            (Tag::Num, DataValue::Num(Num::int(0))),
            (Tag::Str, DataValue::Str(String::new())),
            (Tag::Bytes, DataValue::Bytes(vec![])),
            (Tag::Uuid, DataValue::Uuid(u)),
            (
                Tag::Regex,
                DataValue::Regex(
                    RegexSource::validated(RegexFlags::NONE, "a".into()).into_diagnostic()?,
                ),
            ),
            (Tag::Json, DataValue::Json(Json::Null)),
            (
                Tag::Vector,
                DataValue::Vector(Vector::try_new(vec![]).ok_or_else(|| miette!("empty vec"))?),
            ),
            (Tag::List, DataValue::List(vec![])),
            (Tag::Set, DataValue::Set(BTreeSet::new())),
            (
                Tag::Validity,
                DataValue::Validity(
                    Validity::new(ValidityTs::of_micros(0), true)
                        .ok_or_else(|| miette!("non-reserved"))?
                        .into(),
                ),
            ),
            (Tag::Interval, DataValue::Interval(Interval::EMPTY)),
            (
                Tag::Geometry,
                DataValue::Geometry(Geometry::from_cells(0, 0)),
            ),
        ])
    }

    /// Near-exhaustive interval universe: every start/end from a small
    /// bound set, plus EMPTY. Canonicalization collapses empties.
    fn interval_universe() -> Vec<Interval> {
        let starts = [
            Bound::Unbounded,
            Bound::Closed(i64::MIN),
            Bound::Closed(-1),
            Bound::Closed(0),
            Bound::Closed(1),
            Bound::Closed(i64::MAX),
            Bound::Open(i64::MIN),
            Bound::Open(-1),
            Bound::Open(0),
        ];
        let ends = [
            Bound::Unbounded,
            Bound::Closed(i64::MIN),
            Bound::Closed(-1),
            Bound::Closed(0),
            Bound::Closed(1),
            Bound::Closed(i64::MAX),
            Bound::Open(1),
            Bound::Open(i64::MAX),
        ];
        let mut out = vec![Interval::EMPTY];
        for &s in &starts {
            for &e in &ends {
                let iv = Interval::new(s, e);
                if !out.contains(&iv) {
                    out.push(iv);
                }
            }
        }
        // Explicit range forms that pin Unbounded vs finite extremes.
        for iv in [
            Interval::range(Lo::NegUnbounded, Hi::PosUnbounded),
            Interval::range(Lo::NegUnbounded, Hi::At(i64::MAX)),
            Interval::range(Lo::At(i64::MIN), Hi::PosUnbounded),
            Interval::range(Lo::At(i64::MIN), Hi::At(i64::MAX)),
            Interval::range(Lo::At(i64::MAX), Hi::At(i64::MAX)),
            Interval::range(Lo::At(i64::MIN), Hi::At(i64::MIN)),
        ] {
            if !out.contains(&iv) {
                out.push(iv);
            }
        }
        out
    }

    #[test]
    fn relation_id_lawful_mint_and_refusal() {
        assert!(RelationId::new(7).is_some());
        assert!(RelationId::new(RelationId::CAP).is_none());
        assert!(RelationId::new(u64::MAX).is_none());
        // SYSTEM is the one const id.
        assert_eq!(RelationId::SYSTEM.raw(), 0);
    }

    #[test]
    fn canonical_bytes_only_mint_is_encode() {
        // The ONLY way to obtain CanonicalBytes is to encode a value; the
        // bytes then carry the format law by construction.
        let a = encode_owned(&DataValue::from(1i64));
        let b = encode_owned(&DataValue::from(1i64));
        assert_eq!(a, b);
    }

    /// Tag prefixes: every kind's encoding opens with the pinned format
    /// v1 tag byte — not structural `0x00`/`0x01`, not a reserved gap.
    #[test]
    fn near_exhaustive_tag_prefix_matches_format_v1() -> Result<()> {
        for (tag, pinned) in FORMAT_V1_TAGS {
            assert_eq!(tag.byte(), pinned, "Tag::{tag:?} drifted from format v1");
            assert_eq!(Tag::from_byte(pinned), Some(tag));
        }
        assert_eq!(Tag::from_byte(STRUCT_STRING), None);
        assert_eq!(Tag::from_byte(STRUCT_SEQ_END), None);

        for (tag, value) in one_per_kind()? {
            let enc = encode_owned(&value);
            let bytes = enc.as_bytes();
            assert!(!bytes.is_empty(), "empty encoding for {tag:?}");
            let expected = pinned_tag_byte(tag)?;
            assert_eq!(
                bytes[0], expected,
                "encode({tag:?}) prefix {:#04x} != format v1 {expected:#04x}",
                bytes[0]
            );
            assert_eq!(value.tag(), tag);
            assert!(
                bytes[0] >= 0x05,
                "tag prefix below structural floor: {:#04x}",
                bytes[0]
            );
        }

        // Exhaustive over every byte: only the 14 pinned tags decode.
        let mut seen = 0u32;
        for b in 0u8..=255 {
            match Tag::from_byte(b) {
                Some(t) => {
                    assert_eq!(pinned_tag_byte(t)?, b);
                    seen += 1;
                }
                None => {
                    assert!(
                        FORMAT_V1_TAGS.iter().all(|(_, p)| *p != b),
                        "pinned tag {b:#04x} failed from_byte"
                    );
                }
            }
        }
        assert_eq!(seen, 14);
        Ok(())
    }

    /// Cross-type total order over the 14-kind universe: byte order of
    /// encodings equals pinned tag-byte order when kinds differ, and
    /// equals `DataValue::Ord` on every pair (totality, no holes).
    #[test]
    fn near_exhaustive_cross_type_total_order() -> Result<()> {
        let universe = one_per_kind()?;
        let encoded: Vec<_> = universe
            .iter()
            .map(|(tag, v)| -> Result<_> {
                let enc = encode_owned(v);
                assert_eq!(enc.as_bytes()[0], pinned_tag_byte(*tag)?);
                Ok(enc)
            })
            .collect::<Result<Vec<_>>>()?;

        for i in 0..universe.len() {
            for j in 0..universe.len() {
                let (ti, vi) = &universe[i];
                let (tj, vj) = &universe[j];
                let byte = encoded[i].as_bytes().cmp(encoded[j].as_bytes());
                let structural = vi.cmp(vj);
                let pinned = pinned_tag_byte(*ti)?.cmp(&pinned_tag_byte(*tj)?);

                assert_eq!(byte, structural, "byte != Ord: {ti:?} vs {tj:?}");
                if ti != tj {
                    assert_eq!(
                        byte, pinned,
                        "cross-type byte order left format v1: {ti:?} vs {tj:?}"
                    );
                    assert_eq!(
                        structural, pinned,
                        "cross-type Ord left format v1: {ti:?} vs {tj:?}"
                    );
                }
                assert!(
                    vi.partial_cmp(vj).is_some(),
                    "PartialOrd hole: {ti:?} vs {tj:?}"
                );
            }
        }

        // Transitivity over the sorted-by-pinned-tag universe.
        let pinned_bytes: Vec<u8> = universe
            .iter()
            .map(|(t, _)| pinned_tag_byte(*t))
            .collect::<Result<Vec<_>>>()?;
        let mut order: Vec<usize> = (0..universe.len()).collect();
        order.sort_by(|&a, &b| pinned_bytes[a].cmp(&pinned_bytes[b]));
        for w in order.windows(2) {
            assert!(
                encoded[w[0]].as_bytes() < encoded[w[1]].as_bytes(),
                "tag order not embedded: {:?} !< {:?}",
                universe[w[0]].0,
                universe[w[1]].0
            );
        }
        Ok(())
    }

    /// Sentinel-free unbounded: every enumerable interval encodes
    /// unbounded ends as the `0x01` marker, never as the finite key of
    /// `i64::MIN`/`i64::MAX`, and those forms stay byte-distinct.
    #[test]
    fn near_exhaustive_sentinel_free_unbounded_encodings() -> Result<()> {
        let universe = interval_universe();
        assert!(
            universe.len() >= 20,
            "interval universe too small: {}",
            universe.len()
        );

        let mut encoded = Vec::with_capacity(universe.len());
        for iv in &universe {
            let v = DataValue::Interval(*iv);
            let enc = encode_owned(&v);
            assert_eq!(
                enc.as_bytes()[0],
                pinned_tag_byte(Tag::Interval)?,
                "Interval tag prefix"
            );
            let body = &enc.as_bytes()[1..];
            match iv.ends() {
                None => {
                    assert_eq!(body, &[IV_EMPTY], "EMPTY must be single 0x01 marker");
                }
                Some((lo, hi)) => {
                    assert_eq!(body[0], IV_RANGE, "non-empty form marker");
                    let mut at = 1usize;
                    match lo {
                        Lo::NegUnbounded => {
                            assert_eq!(body[at], END_UNBOUNDED, "lo unbounded marker");
                            at += 1;
                        }
                        Lo::At(t) => {
                            assert_eq!(body[at], END_FINITE, "lo finite marker");
                            at += 1;
                            assert_eq!(&body[at..at + 8], &asc_ts_key(t));
                            at += 8;
                        }
                    }
                    match hi {
                        Hi::PosUnbounded => {
                            assert_eq!(body[at], END_UNBOUNDED, "hi unbounded marker");
                            at += 1;
                        }
                        Hi::At(t) => {
                            assert_eq!(body[at], END_FINITE, "hi finite marker");
                            at += 1;
                            assert_eq!(&body[at..at + 8], &asc_ts_key(t));
                            at += 8;
                        }
                    }
                    assert_eq!(at, body.len(), "trailing junk in interval body");
                }
            }
            let back = decode(enc.as_bytes()).into_diagnostic()?;
            assert_eq!(back, v, "round-trip changed {iv:?}");
            encoded.push(enc);
        }

        // Unbounded ≠ finite extreme, bytewise and as values.
        let neg_unb = DataValue::Interval(Interval::range(Lo::NegUnbounded, Hi::At(0)));
        let min_lo = DataValue::Interval(Interval::range(Lo::At(i64::MIN), Hi::At(0)));
        let pos_unb = DataValue::Interval(Interval::range(Lo::At(0), Hi::PosUnbounded));
        let max_hi = DataValue::Interval(Interval::range(Lo::At(0), Hi::At(i64::MAX)));
        assert_ne!(encode_owned(&neg_unb), encode_owned(&min_lo));
        assert_ne!(encode_owned(&pos_unb), encode_owned(&max_hi));
        assert_ne!(neg_unb, min_lo);
        assert_ne!(pos_unb, max_hi);
        // Marker 0x01 sorts before any finite end (0x02 + key).
        assert!(encode_owned(&neg_unb) < encode_owned(&min_lo));
        assert!(encode_owned(&pos_unb) < encode_owned(&max_hi));

        // Pairwise: byte order == Ord over the enumerable universe.
        for i in 0..universe.len() {
            for j in 0..universe.len() {
                let a = DataValue::Interval(universe[i]);
                let b = DataValue::Interval(universe[j]);
                assert_eq!(
                    encoded[i].as_bytes().cmp(encoded[j].as_bytes()),
                    a.cmp(&b),
                    "interval order embedding: {:?} vs {:?}",
                    universe[i],
                    universe[j]
                );
            }
        }
        Ok(())
    }

    /// Small within-kind domains (bool × bool, tiny nums) — enumerable
    /// order embedding against pinned tag prefixes.
    #[test]
    fn near_exhaustive_small_scalar_domains() -> Result<()> {
        let bools = [DataValue::Bool(false), DataValue::Bool(true)];
        for a in &bools {
            for b in &bools {
                let ea = encode_owned(a);
                let eb = encode_owned(b);
                assert_eq!(ea.as_bytes()[0], pinned_tag_byte(Tag::Bool)?);
                assert_eq!(
                    ea.as_bytes().cmp(eb.as_bytes()),
                    a.cmp(b),
                    "bool order embedding"
                );
            }
        }
        // Bool payload is 0x00/0x01 — total over the two-value domain.
        assert_eq!(encode_owned(&bools[0]).as_bytes(), &[0x08, 0x00]);
        assert_eq!(encode_owned(&bools[1]).as_bytes(), &[0x08, 0x01]);
        assert!(encode_owned(&bools[0]) < encode_owned(&bools[1]));

        let nums = [
            DataValue::Num(Num::int(i64::MIN)),
            DataValue::Num(Num::int(-1)),
            DataValue::Num(Num::int(0)),
            DataValue::Num(Num::int(1)),
            DataValue::Num(Num::int(i64::MAX)),
            DataValue::Num(Num::float(-1.0)),
            DataValue::Num(Num::float(0.0)),
            DataValue::Num(Num::float(1.0)),
        ];
        let enc: Vec<_> = nums.iter().map(encode_owned).collect();
        for i in 0..nums.len() {
            assert_eq!(enc[i].as_bytes()[0], pinned_tag_byte(Tag::Num)?);
            for j in 0..nums.len() {
                assert_eq!(
                    enc[i].as_bytes().cmp(enc[j].as_bytes()),
                    nums[i].cmp(&nums[j]),
                    "num order embedding: {:?} vs {:?}",
                    nums[i],
                    nums[j]
                );
            }
        }

        // List terminator is structural 0x01 — sorts below any tag prefix
        // continuation (prefix law), pinned without going through Ord.
        let empty = encode_owned(&DataValue::List(vec![]));
        let with_null = encode_owned(&DataValue::List(vec![DataValue::Null]));
        assert_eq!(
            empty.as_bytes(),
            &[pinned_tag_byte(Tag::List)?, STRUCT_SEQ_END]
        );
        assert_eq!(
            with_null.as_bytes()[1],
            pinned_tag_byte(Tag::Null)?,
            "list element must open with a tag, not the terminator"
        );
        assert!(empty.as_bytes() < with_null.as_bytes());
        Ok(())
    }
}
