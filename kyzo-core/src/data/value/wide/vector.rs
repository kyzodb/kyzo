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

#[cfg(test)]
mod tests {
    use super::super::super::canonical::{Datum, encode};

    #[test]
    fn component_identity_follows_num_law() {
        // -0.0 and +0.0 components are one vector identity; all NaN bit
        // patterns are one.
        let a = encode(Datum::Vector(&[0.0, 1.0]));
        let b = encode(Datum::Vector(&[-0.0, 1.0]));
        assert_eq!(a, b);
        let n1 = encode(Datum::Vector(&[f64::NAN]));
        let n2 = encode(Datum::Vector(&[f64::from_bits(0xFFF8_0000_0000_0001)]));
        assert_eq!(n1, n2);
    }
}
