/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The one prefix-first comparison doctrine, minted once and consumed by
//! both the 16-byte cell and the arena's run entries — two lookalike
//! implementations whose divergence would surface as an undetectable
//! ordering anomaly are structurally impossible because there is exactly
//! one.
//!
//! A prefix is the first four payload bytes, zero-padded. Comparison
//! decides on the prefix alone wherever the prefix is conclusive and
//! reports [`PrefixCmp::NeedPayload`] only where it is not: equal prefixes
//! with *both* lengths past the prefix width. That single fallback site is
//! where the deref counter lives — "dereferences only on a tie" is proven
//! by counting this path, not asserted.

use std::cmp::Ordering;

/// Prefix width in bytes: what fits beside a 4-byte handle in the 16-byte
/// cell, and beside a span in a run entry.
pub const PREFIX_LEN: usize = 4;

/// The first four bytes of a payload, zero-padded. Bytewise order of
/// prefixes never contradicts bytewise order of payloads (zero-padding is
/// sound because a shorter string is a strict prefix of any extension).
#[inline]
pub fn prefix4(bytes: &[u8]) -> [u8; PREFIX_LEN] {
    let mut p = [0u8; PREFIX_LEN];
    let n = bytes.len().min(PREFIX_LEN);
    p[..n].copy_from_slice(&bytes[..n]);
    p
}

/// Outcome of a prefix-first comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrefixCmp {
    /// The prefix (plus lengths) was conclusive.
    Decided(Ordering),
    /// Equal prefixes, both payloads longer than the prefix: only the
    /// payload bytes can decide. The one tie path.
    NeedPayload,
}

/// Compare two values by `(prefix, len)` alone.
///
/// Decides everything the prefix can possibly decide:
/// - unequal prefixes decide bytewise;
/// - equal prefixes where *either* length is within the prefix width
///   decide by length, because that value's entire payload is inside its
///   prefix, making it a strict prefix (or equal) of the other.
///
/// Only equal prefixes with both lengths past the width need payload
/// bytes.
#[inline]
pub fn cmp_prefixed(pa: [u8; 4], la: u32, pb: [u8; 4], lb: u32) -> PrefixCmp {
    match pa.cmp(&pb) {
        Ordering::Equal => {
            if super::convert::usize_from_u32(la) <= PREFIX_LEN
                || super::convert::usize_from_u32(lb) <= PREFIX_LEN
            {
                PrefixCmp::Decided(la.cmp(&lb))
            } else {
                PrefixCmp::NeedPayload
            }
        }
        decided @ Ordering::Less | decided @ Ordering::Greater => PrefixCmp::Decided(decided),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The soundness law: whatever the prefix decides must equal the full
    /// bytewise comparison, and NeedPayload may occur only on equal
    /// prefixes with both payloads longer than the prefix.
    fn assert_sound(a: &[u8], b: &[u8]) {
        match cmp_prefixed(
            prefix4(a),
            u32::try_from(a.len()).expect("INVARIANT(len_fits_u32): slice len fits u32"),
            prefix4(b),
            u32::try_from(b.len()).expect("INVARIANT(len_fits_u32): slice len fits u32"),
        ) {
            PrefixCmp::Decided(o) => {
                assert_eq!(
                    o,
                    a.cmp(b),
                    "prefix decided {:?} wrongly for {a:?} vs {b:?}",
                    o
                );
            }
            PrefixCmp::NeedPayload => {
                assert_eq!(prefix4(a), prefix4(b));
                assert!(a.len() > PREFIX_LEN && b.len() > PREFIX_LEN);
            }
        }
    }

    #[test]
    fn law_soundness_exhaustive_small() {
        // All strings over {0x00, 0x61, 0xff} of length <= 5: every pair.
        let alpha = [0x00u8, 0x61, 0xff];
        let mut universe: Vec<Vec<u8>> = vec![vec![]];
        let mut layer: Vec<Vec<u8>> = vec![vec![]];
        for _ in 0..5 {
            let mut next = Vec::new();
            for v in &layer {
                for &c in &alpha {
                    let mut w = v.clone();
                    w.push(c);
                    next.push(w);
                }
            }
            universe.extend(next.iter().cloned());
            layer = next;
        }
        for a in &universe {
            for b in &universe {
                assert_sound(a, b);
            }
        }
    }

    #[test]
    fn law_soundness_random_long() {
        // Seeded xorshift64*; long strings with shared prefixes.
        let mut s = 0x9E37_79B9u64;
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            // INVARIANT(xorshift_finalizer): xorshift* final mul is defined wrapping on u64.
            (std::num::Wrapping(s) * std::num::Wrapping(0x2545_F491_4F6C_DD1D)).0
        };
        for _ in 0..20_000 {
            let la = match usize::try_from(next() % 12) {
                Ok(n) => n,
                Err(_) => 0,
            };
            let lb = match usize::try_from(next() % 12) {
                Ok(n) => n,
                Err(_) => 0,
            };
            // Tiny alphabet forces prefix collisions constantly.
            let a: Vec<u8> = (0..la)
                .map(|_| match u8::try_from(next() % 2) {
                    Ok(b) => b,
                    Err(_) => 0,
                })
                .collect();
            let b: Vec<u8> = (0..lb)
                .map(|_| match u8::try_from(next() % 2) {
                    Ok(b) => b,
                    Err(_) => 0,
                })
                .collect();
            assert_sound(&a, &b);
        }
    }

    #[test]
    fn equal_short_values_decide_equal_without_payload() {
        let a = b"ab";
        assert_eq!(
            cmp_prefixed(prefix4(a), 2, prefix4(a), 2),
            PrefixCmp::Decided(Ordering::Equal)
        );
    }
}
