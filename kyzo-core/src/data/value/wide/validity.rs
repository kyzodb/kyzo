/*
 * Copyright 2026, The KyzoDB Authors.
 * KyzoDB is a fork of CozoDB (Copyright 2022, The Cozo Project Authors).
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `Validity`: the time-axis value kind, its law **imported** from the
//! sealed bitemporal guardrail (`data/bitemporal.rs` + the
//! validity-in-key storage contract), never invented here:
//!
//! - A validity is a pure value: (timestamp in microseconds, assert
//!   flag). No ambient clock, no query-time normalization.
//! - Total order is **descending timestamp** (`Reverse<i64>` in the
//!   contract: as-of seeks find the latest instant first), with assert
//!   sorting before retract at the same instant (`Reverse<bool>`).
//! - Claim polarity beyond the flag (assert/retract/erase) rides in row
//!   VALUES per the guardrail, not in this kind.
//!
//! `Ord` here is lawful as a trait: the value is fully inline-comparable
//! — no deref, no context — and the impl is the imported law.

use std::cmp::Ordering;

/// The validity value: a valid-time instant and its assert flag.
///
/// Deliberately NO `Ord`: the imported total order is *as-of seek order*
/// (descending time), and a trait impl would leak `later < earlier` into
/// any generic comparison. The order authority is the named
/// [`Validity::cmp_as_of_order`], and the canonical encoding embeds it.
///
/// Canonical payload (format v1): the 8-byte DESCENDING timestamp key
/// (ascending sign-flipped big-endian, complemented), then one polarity
/// byte (`0x00` assert, `0x01` retract; anything else refuses). Note:
/// this kind treats every `i64` — including `i64::MAX` — as an ordinary
/// instant; open-end range sentinels are the temporal tuple layer's
/// vocabulary, not a value of this kind.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Validity {
    ts_micros: i64,
    is_assert: bool,
}

impl Validity {
    pub fn new(ts_micros: i64, is_assert: bool) -> Validity {
        Validity {
            ts_micros,
            is_assert,
        }
    }

    pub fn ts_micros(self) -> i64 {
        self.ts_micros
    }

    pub fn is_assert(self) -> bool {
        self.is_assert
    }
}

impl Validity {
    /// The imported as-of order: descending timestamp, assert before
    /// retract. NAMED, not a trait — see the type docs for why.
    pub fn cmp_as_of_order(self, other: Validity) -> Ordering {
        other
            .ts_micros
            .cmp(&self.ts_micros)
            .then(other.is_assert.cmp(&self.is_assert))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_law_descending_ts_assert_first() {
        // Later instants sort first (descending), assert before retract.
        let lt = |a: Validity, b: Validity| a.cmp_as_of_order(b) == Ordering::Less;
        assert!(lt(Validity::new(10, true), Validity::new(5, true)));
        assert!(lt(Validity::new(5, true), Validity::new(5, false)));
        assert!(lt(
            Validity::new(i64::MAX, false),
            Validity::new(i64::MIN, true)
        ));
        assert_eq!(Validity::new(7, true), Validity::new(7, true));
    }
}
