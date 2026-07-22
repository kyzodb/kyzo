/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `Vector`: **content-addressed identity** from exact float components —
//! dimensionality + the canonical element sequence, hashed deterministically
//! — so the same content yields the same identity regardless of row or
//! storage position. Every component passes through Num's float law
//! (`-0.0 → +0.0`, one canonical NaN); exact float is the authority.
//! Quantized codes are a rebuildable projection of those floats
//! (engine-side, [OPEN] cross-story dep #308 under epic #353) — never a
//! second identity. This value-plane identity seat is complete; quantized
//! traversal/rerank wiring is not inventable here.
//!
//! Similarity metrics are operator/query context, never part of identity.
//! Storage order (dimension first, then elementwise float order) is
//! deterministic, NOT a semantic "less than" for vectors — expression
//! comparability is a separate refusable authority.
//!
//! The canonical payload is a u32 dimension count followed by each
//! component as Num's order-preserving float key (see
//! [`super::super::canonical`]). Content identity is FNV-1a 64 over
//! dimension bytes + exact float bits of that payload's semantic content
//! — derived, not row-positional.
//!
//! After the admit door, dimension is a proven newtype and the store is
//! [`Vec<VectorComponent>`] — bare `Vec<f64>` is not a post-door store.

use super::super::number::Num;
use super::json::fnv1a64;

/// One vector component after Num's float law: `-0.0 → +0.0`, one
/// canonical NaN. Private field; the only public mint is [`Self::admit`].
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct VectorComponent(f64);

const _: () = assert!(std::mem::size_of::<VectorComponent>() == std::mem::size_of::<f64>());
const _: () = assert!(std::mem::align_of::<VectorComponent>() == std::mem::align_of::<f64>());

impl VectorComponent {
    /// Admit door: apply Num's float law, then brand the result.
    /// `Num::float` always stores a float (never Int), so `to_f64` is the
    /// canonical magnitude — no `Option`/`expect` on the product path.
    pub fn admit(raw: f64) -> VectorComponent {
        VectorComponent(Num::float(raw).to_f64())
    }

    /// Post-proof mint: the float is already in Num's canonical form.
    pub(crate) fn from_canonical(canonical: f64) -> VectorComponent {
        VectorComponent(canonical)
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

impl PartialEq for VectorComponent {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}

impl Eq for VectorComponent {}

impl std::hash::Hash for VectorComponent {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.0.to_bits());
    }
}

/// Vector dimensionality as stored: a `u32` count proven at the admit
/// door. Private field; mint only through [`try_from_len`] /
/// [`from_len_unchecked`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct VectorDimension(u32);

const _: () = assert!(std::mem::size_of::<VectorDimension>() == std::mem::size_of::<u32>());
const _: () = assert!(std::mem::align_of::<VectorDimension>() == std::mem::align_of::<u32>());

impl VectorDimension {
    /// Prove a component length fits the wire dimension (`u32`).
    pub fn try_from_len(len: usize) -> Option<VectorDimension> {
        (match u32::try_from(len) { Ok(v) => Some(VectorDimension(v)), Err(_overflow) => None })
    }

    /// Post-proof mint: length already proven at [`Vector::try_new`].
    pub(crate) fn from_len_unchecked(len: u32) -> VectorDimension {
        VectorDimension(len)
    }

    pub fn get(self) -> u32 {
        self.0
    }

    pub fn as_usize(self) -> usize {
        self.0 as usize
    }
}

/// Content-addressed identity of a [`Vector`]: FNV-1a 64 over the
/// dimension and exact float bits after Num's law. Same content → same
/// id regardless of row position. Exact floats remain the equality and
/// storage authority; this id is the address derived from them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub struct VectorContentId(u64);

const _: () = assert!(std::mem::size_of::<VectorContentId>() == std::mem::size_of::<u64>());
const _: () = assert!(std::mem::align_of::<VectorContentId>() == std::mem::align_of::<u64>());

impl VectorContentId {
    pub fn get(self) -> u64 {
        self.0
    }
}

/// A vector value: proven [`VectorComponent`]s held after [`Vector::try_new`]
/// admits every raw `f64`. Identity is content-addressed
/// ([`VectorContentId`]) from those exact components — not row-positional.
#[derive(Clone, Debug)]
pub struct Vector {
    dim: VectorDimension,
    /// Proven components only — never a bare `Vec<f64>` after the door.
    components: Vec<VectorComponent>,
    /// Derived at admit from exact float bits; same content → same id.
    content_id: VectorContentId,
}

impl Vector {
    /// Admit door: brand every raw float, prove the dimension fits `u32`,
    /// mint the content-addressed identity from the proven floats.
    /// `None` when the component count exceeds the wire dimension.
    pub fn try_new(components: Vec<f64>) -> Option<Vector> {
        let components: Vec<VectorComponent> =
            components.into_iter().map(VectorComponent::admit).collect();
        let dim = VectorDimension::try_from_len(components.len())?;
        let content_id = content_id_from_parts(dim, &components);
        Some(Vector {
            dim,
            components,
            content_id,
        })
    }

    /// Content-addressed identity: deterministic hash of exact float
    /// content. Independent of row or storage position.
    pub fn content_id(&self) -> VectorContentId {
        self.content_id
    }

    /// Proven components.
    pub fn components(&self) -> impl Iterator<Item = VectorComponent> + '_ {
        self.components.iter().copied()
    }

    /// Proven dimensionality (wire `u32` count).
    pub fn dimension(&self) -> VectorDimension {
        self.dim
    }

    /// Proven component store after the admit door (read-only; not a mint).
    pub fn as_slice(&self) -> &[VectorComponent] {
        &self.components
    }

    /// Canonical float magnitudes copied out of the proven store.
    pub fn to_f64s(&self) -> Vec<f64> {
        self.components.iter().map(|c| c.get()).collect()
    }

    pub fn len(&self) -> usize {
        self.components.len()
    }

    pub fn is_empty(&self) -> bool {
        self.components.is_empty()
    }
}

/// FNV-1a 64 over dimension (big-endian) then each component's exact
/// float bits — the one content-hash door. Exact floats are the input;
/// the id is the address.
fn content_id_from_parts(dim: VectorDimension, components: &[VectorComponent]) -> VectorContentId {
    let mut bytes = Vec::with_capacity(4 + components.len() * 8);
    bytes.extend_from_slice(&dim.get().to_be_bytes());
    for c in components {
        bytes.extend_from_slice(&c.get().to_bits().to_be_bytes());
    }
    VectorContentId(fnv1a64(&bytes))
}

impl PartialEq for Vector {
    fn eq(&self, other: &Self) -> bool {
        // Exact float is authority — never equality-by-hash alone.
        self.dim == other.dim && self.components == other.components
    }
}

impl Eq for Vector {}

impl std::hash::Hash for Vector {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // Content-addressed: hash the content id (Eq-consistent: equal
        // floats → equal content_id → equal Hash).
        self.content_id.hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::canonical::{Datum, decode, encode};
    use super::super::super::number::Num;
    use super::{Vector, VectorComponent, VectorDimension};
    use crate::value::DataValue;

    #[test]
    fn component_identity_follows_num_law() {
        let a = encode(Datum::Vector(&Vector::try_new(vec![0.0, 1.0]).unwrap()));
        let b = encode(Datum::Vector(&Vector::try_new(vec![-0.0, 1.0]).unwrap()));
        assert_eq!(a, b);
        let v = Vector::try_new(vec![-0.0, 1.0]).unwrap();
        assert_eq!(v.dimension(), VectorDimension::try_from_len(2).unwrap());
        assert_eq!(
            v.components().map(|c| c.get()).collect::<Vec<_>>(),
            vec![0.0, 1.0]
        );
        assert_eq!(
            VectorComponent::admit(f64::NAN).get().to_bits(),
            Num::float(f64::NAN).as_float().unwrap().to_bits()
        );
    }

    #[test]
    fn same_content_same_identity() {
        let a = Vector::try_new(vec![1.0, 2.0, 3.0]).unwrap();
        let b = Vector::try_new(vec![1.0, 2.0, 3.0]).unwrap();
        assert_eq!(a.content_id(), b.content_id());
        assert_eq!(a, b);
        // Num law: −0 and +0 are one content → one identity.
        let pos = Vector::try_new(vec![0.0, 1.0]).unwrap();
        let neg = Vector::try_new(vec![-0.0, 1.0]).unwrap();
        assert_eq!(pos.content_id(), neg.content_id());
        assert_eq!(pos, neg);
        // Same content constructed twice is not row-positional.
        let again = Vector::try_new(vec![0.0, 1.0]).unwrap();
        assert_eq!(pos.content_id(), again.content_id());
    }

    #[test]
    fn one_bit_change_different_identity() {
        let a = Vector::try_new(vec![1.0, 2.0]).unwrap();
        let mut bits = 2.0f64.to_bits();
        bits ^= 1;
        let b = Vector::try_new(vec![1.0, f64::from_bits(bits)]).unwrap();
        assert_ne!(a.content_id(), b.content_id());
        assert_ne!(a, b);
        // Dimension change is also different content.
        let c = Vector::try_new(vec![1.0, 2.0, 0.0]).unwrap();
        assert_ne!(a.content_id(), c.content_id());
    }

    #[test]
    fn content_hash_determinism() {
        let v = Vector::try_new(vec![f64::NAN, -1.5, 0.0]).unwrap();
        let id = v.content_id();
        assert_eq!(id, v.content_id());
        assert_eq!(
            id,
            Vector::try_new(vec![f64::NAN, -1.5, -0.0])
                .unwrap()
                .content_id()
        );
        // Independent recomputation from the same exact bits matches.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&v.dimension().get().to_be_bytes());
        for c in v.components() {
            bytes.extend_from_slice(&c.get().to_bits().to_be_bytes());
        }
        assert_eq!(id.get(), super::super::json::fnv1a64(&bytes));
    }

    #[test]
    fn exact_float_round_trip() {
        let original = Vector::try_new(vec![-0.0, 1.5, f64::NAN, f64::INFINITY]).unwrap();
        let enc = encode(Datum::Vector(&original));
        let back = match decode(enc.as_bytes()).expect("decode own encoding") {
            DataValue::Vector(v) => v,
            other => panic!("expected Vector, got {other:?}"),
        };
        // Exact float is authority through the codec — bit-exact meter
        // (NaN has bits; bare f64 PartialEq treats NaN ≠ NaN).
        assert_eq!(
            back.as_slice()
                .iter()
                .map(|c| c.get().to_bits())
                .collect::<Vec<_>>(),
            original
                .as_slice()
                .iter()
                .map(|c| c.get().to_bits())
                .collect::<Vec<_>>(),
        );
        assert_eq!(back, original);
        assert_eq!(back.content_id(), original.content_id());
        // −0 admitted to +0 before encode; round-trip keeps that form.
        assert_eq!(back.to_f64s()[0].to_bits(), 0.0f64.to_bits());
    }
}
