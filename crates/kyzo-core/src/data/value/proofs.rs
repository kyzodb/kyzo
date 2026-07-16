/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Compile-time ABSENCE proofs for the value plane's authority model.
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

use super::DataValue;
use super::Tuple;
use super::arena::BulkSpendAuthority;
use super::canonical::CanonicalBytes;
use super::cell::{Minted, Value};
use super::code::{Code, StampedCode};
use super::row::RelationId;
use super::string::MintedStr;

/// Assert that `$t` does NOT implement all of `$($tr)+`. Compiles iff the
/// type lacks the trait: two blanket impls stay unambiguous only while the
/// type is missing the trait; add the impl and the associated-item lookup
/// becomes ambiguous, which is a hard compile error.
macro_rules! assert_not_impl {
    ($t:ty: $($tr:path),+ $(,)?) => {
        const _: fn() = || {
            trait AmbiguousIfImpl<A> {
                fn __proof() {}
            }
            impl<T: ?Sized> AmbiguousIfImpl<()> for T {}
            // Marker exists only to give the second blanket impl below a
            // distinct type parameter; the ambiguity trick never
            // constructs it, so a plain dead_code lint fires by design.
            #[allow(dead_code)]
            struct Marker;
            impl<T: ?Sized $(+ $tr)+> AmbiguousIfImpl<Marker> for T {}
            // Unresolvable (ambiguous) the moment `$t` implements the
            // traits; resolvable — this line compiles — only while it does
            // not.
            let _ = <$t as AmbiguousIfImpl<_>>::__proof;
        };
    };
}

// The compile-time absence proof is a general utility (a build-time witness
// that a type lacks a capability), not value-plane-specific. It is the
// mechanism the staged-construction idiom uses to prove `build()` is absent
// on an incomplete typestate — exported crate-wide so those proofs live
// beside the builders they guard, never re-spelled.
pub(crate) use assert_not_impl;

// Code is identity ONLY: no order. It cannot be compared, so no read path
// can sneak an ordering out of a bare handle — order is the observer's
// through resolved bytes, never the code's.
assert_not_impl!(Code: PartialOrd);
assert_not_impl!(Code: Ord);

// The 16-byte cell exposes no semantic equality or order TRAIT: comparison
// is `try_cmp_storage` (locality-only). Physical word identity under a
// proven context is rebuilt under DomainCtx (#304 cut `same_word`).
// A derived `Ord`/`Eq` would silently deref or misjudge; it must not exist.
assert_not_impl!(Value: PartialOrd);
assert_not_impl!(Value: Ord);
assert_not_impl!(Value: PartialEq);
assert_not_impl!(Value: Eq);

// A stamped code cannot be conjured: no `Default`. Its only mints are
// `Arena::intern` and `EpochRemap::apply`, both demanding the arena's
// private mint token.
assert_not_impl!(StampedCode: Default);

// The bulk-spend authority is one-per-admission and non-duplicable: no
// `Clone`, no `Copy`, no `Default`. Its only mint is
// `Domain::admit_to` (plane-internal). By-ref spend was cut (#304);
// T5 rebuilds consume-on-spend.
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
    //! The absence proofs above are compile-time. This runtime test just
    //! re-states, executably, that the LAWFUL mints exist and the checked
    //! door refuses — a positive companion to the negatives.
    use super::*;

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
        let a = super::super::encode_owned(&super::super::DataValue::from(1i64));
        let b = super::super::encode_owned(&super::super::DataValue::from(1i64));
        assert_eq!(a, b);
    }
}
