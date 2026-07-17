/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `Vector`: identity is **dimensionality + the canonical element
//! sequence**, with every component passing through Num's float law
//! (`-0.0 → +0.0`, one canonical NaN) — a vector containing `-0.0` and
//! one containing `+0.0` are one value, or dedup would split equal
//! things. Similarity metrics are operator/query context, never part of
//! identity. Storage order (dimension first, then elementwise float
//! order) is deterministic, NOT a semantic "less than" for vectors —
//! expression comparability is a separate refusable authority.
//!
//! The canonical payload is a u32 dimension count followed by each
//! component as Num's order-preserving float key (see
//! [`super::super::canonical`]).
//!
//! After the admit door, dimension is a proven newtype and components
//! are private canonical floats — bare `Vec<f64>` is not a public mint.

use super::super::number::Num;

/// One vector component after Num's float law: `-0.0 → +0.0`, one
/// canonical NaN. Private field; the only public mint is [`Self::admit`].
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct VectorComponent(f64);

const _: () = assert!(std::mem::size_of::<VectorComponent>() == std::mem::size_of::<f64>());
const _: () = assert!(std::mem::align_of::<VectorComponent>() == std::mem::align_of::<f64>());

impl VectorComponent {
    /// Admit door: apply Num's float law, then brand the result.
    pub fn admit(raw: f64) -> VectorComponent {
        VectorComponent(Num::float(raw).as_float().expect("float stays float"))
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
        u32::try_from(len).ok().map(VectorDimension)
    }

    /// Post-proof mint: length already proven at [`Vector::new`].
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

/// A vector value: private canonical floats held after [`Vector::new`]
/// admits every raw `f64` through [`VectorComponent::admit`]. Identity is
/// dimensionality + exact component bits.
#[derive(Clone, Debug)]
pub struct Vector {
    dim: VectorDimension,
    /// Canonical component magnitudes; private — not a pub mint surface.
    components: Vec<f64>,
}

impl Vector {
    /// Admit door: brand every raw float, prove the dimension fits `u32`.
    pub fn new(components: Vec<f64>) -> Vector {
        let components: Vec<f64> = components
            .into_iter()
            .map(|raw| VectorComponent::admit(raw).get())
            .collect();
        let dim = VectorDimension::try_from_len(components.len())
            .expect("vector dimension exceeds u32");
        Vector { dim, components }
    }

    /// Proven components as [`VectorComponent`] (re-brand of private store).
    pub fn components(&self) -> impl Iterator<Item = VectorComponent> + '_ {
        self.components
            .iter()
            .copied()
            .map(VectorComponent::from_canonical)
    }

    /// Proven dimensionality (wire `u32` count).
    pub fn dimension(&self) -> VectorDimension {
        self.dim
    }

    /// Canonical float view after the admit door (read-only; not a mint).
    pub fn as_slice(&self) -> &[f64] {
        &self.components
    }

    pub fn len(&self) -> usize {
        self.components.len()
    }

    pub fn is_empty(&self) -> bool {
        self.components.is_empty()
    }
}

impl PartialEq for Vector {
    fn eq(&self, other: &Self) -> bool {
        self.dim == other.dim
            && self
                .components
                .iter()
                .zip(other.components.iter())
                .all(|(a, b)| a.to_bits() == b.to_bits())
    }
}

impl Eq for Vector {}

impl std::hash::Hash for Vector {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.dim.hash(state);
        for c in &self.components {
            state.write_u64(c.to_bits());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::canonical::{Datum, encode};
    use super::super::super::number::Num;
    use super::{Vector, VectorComponent, VectorDimension};

    #[test]
    fn component_identity_follows_num_law() {
        let a = encode(Datum::Vector(&[0.0, 1.0]));
        let b = encode(Datum::Vector(&[-0.0, 1.0]));
        assert_eq!(a, b);
        let v = Vector::new(vec![-0.0, 1.0]);
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
}
